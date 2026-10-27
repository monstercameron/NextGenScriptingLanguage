use super::state::*;
use crate::ast::{ListenSource, Program, Trigger};
use crate::runtime::{self, RunOptions};
use anyhow::{bail, Context, Result};
use chrono::Local;
use std::sync::{
    atomic::{AtomicBool, Ordering},
    Arc,
};
use std::thread::{self, JoinHandle};
use std::time::{Duration, Instant};

struct Producer {
    epoch: u64,
    cancel: Arc<AtomicBool>,
    handle: Option<JoinHandle<()>>,
}
impl Drop for Producer {
    fn drop(&mut self) {
        self.cancel.store(true, Ordering::Relaxed);
        if let Some(handle) = self.handle.take() {
            let _ = handle.join();
        }
    }
}
fn join_producer(producer: Option<Producer>) {
    drop(producer);
}
fn start_producer(shared: &Arc<Shared>, options: &RunOptions, epoch: u64) -> Result<Producer> {
    let mut state = shared.state.lock().unwrap();
    let Some(Trigger::Listen {
        source: ListenSource::Applet(invocation),
        ..
    }) = &state.plan.program.trigger
    else {
        bail!("no native listener producer");
    };
    let invocation = invocation.clone();
    let cancel = Arc::new(AtomicBool::new(false));
    state.producer_cancel = cancel.clone();
    state.producer_done = false;
    state.producer_error = None;
    let bindings = state.doc.node_state().bindings;
    let options = RunOptions {
        cache_directory: options.cache_directory.clone(),
        trace: options.trace,
        max_output_bytes: options.max_output_bytes,
        max_loop_iterations: options.max_loop_iterations,
        loop_budget_scope: options.loop_budget_scope,
        command_timeout: options.command_timeout,
        total_timeout: options.total_timeout,
        ..RunOptions::new(state.doc.cwd.clone())
    };
    drop(state);
    let control = shared.clone();
    let token = cancel.clone();
    let handle = thread::spawn(move || {
        let result = runtime::live::producer(
            &invocation,
            &control.registry,
            &options,
            &bindings,
            token.clone(),
            control.max_bytes,
            |bytes, _, _| {
                let mut state = control.state.lock().unwrap();
                loop {
                    if token.load(Ordering::Relaxed)
                        || control.stop.load(Ordering::Relaxed)
                        || state.producer_epoch != epoch
                    {
                        bail!("producer cancelled");
                    }
                    if state.doc.inbox.len() < MAX_QUEUE {
                        let record = Data::from_bytes(bytes.to_vec());
                        if state.doc.enqueue(record)? {
                            state.changed("producer_record");
                            control.wake.notify_all();
                            return Ok(());
                        }
                    }
                    state = control
                        .wake
                        .wait_timeout(state, Duration::from_millis(20))
                        .unwrap()
                        .0;
                }
            },
        );
        let mut state = control.state.lock().unwrap();
        if state.producer_epoch == epoch {
            state.producer_done = true;
            if let Ok(launched) = result.as_ref() {
                state.launches += launched;
            }
            if let Err(error) = result {
                if !token.load(Ordering::Relaxed) {
                    state.producer_error = Some(format!("{error:#}").chars().take(4096).collect());
                }
            }
            state.changed("producer_ended");
            control.wake.notify_all();
        }
    });
    Ok(Producer {
        epoch,
        cancel,
        handle: Some(handle),
    })
}

pub(super) fn run(shared: Arc<Shared>, options: RunOptions) {
    let mut producer: Option<Producer> = None;
    let mut source = String::new();
    let mut next_tick = None;
    let mut last_minute = Local::now().timestamp().div_euclid(60);
    loop {
        let mut state = shared.state.lock().unwrap();
        if shared.stop.load(Ordering::Relaxed) {
            state.active_cancel.store(true, Ordering::Relaxed);
            state.producer_cancel.store(true, Ordering::Relaxed);
            state.running = false;
            state.status = "stopped".into();
            state.changed("stopped");
            shared.wake.notify_all();
            drop(state);
            break;
        }
        if producer
            .as_ref()
            .is_some_and(|p| p.epoch != state.producer_epoch)
        {
            drop(state);
            join_producer(producer.take());
            continue;
        }
        if source != state.doc.source {
            source = state.doc.source.clone();
            next_tick = None;
            last_minute = Local::now().timestamp().div_euclid(60);
        }
        if state.plan.screen.is_some() {
            drop(state);
            if let Err(error) = crate::ui::run_live(shared.clone(), &options) {
                let mut state = shared.state.lock().unwrap();
                state.running = false;
                state.in_flight = false;
                state.error = Some(format!("{error:#}").chars().take(4096).collect());
                state.uncertain = true;
                state.status = "failed".into();
                let epoch = state.ui_epoch;
                state.changed("ui_failed");
                shared.wake.notify_all();
                while !shared.stop.load(Ordering::Relaxed)
                    && state.plan.screen.is_some()
                    && !state.running
                    && state.ui_epoch == epoch
                {
                    state = shared
                        .wake
                        .wait_timeout(state, Duration::from_millis(20))
                        .unwrap()
                        .0;
                }
            }
            continue;
        }
        if state.plan.stream.is_none()
            && state.doc.cursor >= state.plan.program.steps.len()
            && state.status != "completed"
            && (state.plan.program.trigger.is_none() || state.cycle)
        {
            let trigger = state.plan.program.trigger.clone();
            let is_service = trigger.is_some();
            state.last_output = state.doc.input.clone();
            state.runs += 1;
            if is_service {
                state.cycle = false;
                state.doc.cursor = state.plan.body_start;
                if let Some(Trigger::Every { interval_ms, .. }) = trigger {
                    // Skip missed ticks; never overlap or replay a backlog.
                    let interval = Duration::from_millis(interval_ms);
                    let due = next_tick.unwrap_or_else(Instant::now);
                    match runtime::next_interval(due, Instant::now(), interval) {
                        Ok(deadline) => next_tick = Some(deadline),
                        Err(error) => {
                            state.running = false;
                            state.error = Some(error.to_string());
                            state.status = "failed".into();
                            state.changed("schedule_failed");
                            shared.wake.notify_all();
                            continue;
                        }
                    }
                }
                state.status = if state.running { "waiting" } else { "paused" }.into();
            } else {
                state.running = false;
                state.status = "completed".into();
            }
            state.changed("run_completed");
            shared.wake.notify_all();
            continue;
        }
        if !state.running {
            if state.status != "failed" && state.status != "completed" && state.status != "paused" {
                state.status = "paused".into();
                state.changed("paused");
                shared.wake.notify_all();
            }
            drop(
                shared
                    .wake
                    .wait_timeout(state, Duration::from_millis(20))
                    .unwrap(),
            );
            continue;
        }
        if let Some(stream) = state.plan.stream.clone() {
            if state.doc.cursor == state.plan.program.steps.len() {
                state.running = false;
                state.status = "completed".into();
                state.changed("completed");
                shared.wake.notify_all();
                continue;
            }
            let token = Arc::new(AtomicBool::new(false));
            state.active_cancel = token.clone();
            state.in_flight = true;
            state.active = Some((0, state.plan.program.steps.len()));
            state.status = "running".into();
            state.changed("stream_started");
            shared.wake.notify_all();
            let context = state.doc.node_state();
            drop(state);
            let result = run_stream(&shared, &options, &stream, context, token);
            let mut state = shared.state.lock().unwrap();
            state.in_flight = false;
            state.active = None;
            let result = result.and_then(|launched| {
                let previous = state.doc.cursor;
                state.doc.cursor = state.plan.program.steps.len();
                let fits = within_state_limit(&state.doc);
                if !matches!(fits, Ok(true)) {
                    state.doc.cursor = previous;
                }
                if !fits? {
                    bail!("state exceeds 8 MiB at stream completion; effects may have completed");
                }
                Ok(launched)
            });
            match result {
                Ok(launched) => {
                    state.runs += 1;
                    state.launches += launched;
                    state.nodes += 1;
                    state.running = false;
                    state.status = "completed".into();
                }
                Err(error) => {
                    state.error = Some(format!("{error:#}").chars().take(4096).collect());
                    state.uncertain = true;
                    state.running = false;
                    state.status = "failed".into();
                }
            }
            state.changed("stream_ended");
            shared.wake.notify_all();
            continue;
        }
        let trigger = state.plan.program.trigger.clone();
        let is_service = trigger.is_some();
        if is_service && state.doc.cursor >= state.plan.body_start && !state.cycle {
            let fire = match trigger.as_ref().unwrap() {
                Trigger::Every { interval_ms, .. } => {
                    let due = *next_tick.get_or_insert_with(|| {
                        Instant::now() + Duration::from_millis(*interval_ms)
                    });
                    state.step_budget.is_some() || Instant::now() >= due
                }
                Trigger::Cron { expression, .. } => {
                    let now = Local::now();
                    let minute = now.timestamp().div_euclid(60);
                    let due = minute > last_minute
                        && crate::schedule::CronSchedule::parse(expression)
                            .is_ok_and(|s| s.matches(&now));
                    last_minute = minute;
                    state.step_budget.is_some() || due
                }
                Trigger::Listen { .. } => !state.doc.inbox.is_empty(),
            };
            let native = matches!(
                trigger,
                Some(Trigger::Listen {
                    source: ListenSource::Applet(_),
                    ..
                })
            );
            if native && producer.is_none() {
                let epoch = state.producer_epoch;
                drop(state);
                match start_producer(&shared, &options, epoch) {
                    Ok(started) => producer = Some(started),
                    Err(error) => {
                        let mut s = shared.state.lock().unwrap();
                        s.error = Some(format!("{error:#}").chars().take(4096).collect());
                        s.running = false;
                        s.status = "failed".into();
                        s.changed("producer_failed");
                        shared.wake.notify_all();
                    }
                }
                continue;
            }
            if !fire {
                if native && state.producer_done {
                    state.running = false;
                    if let Some(error) = state.producer_error.clone() {
                        state.error = Some(error);
                        state.status = "failed".into();
                    } else {
                        state.status = "completed".into();
                    }
                    state.changed("source_exhausted");
                    shared.wake.notify_all();
                    continue;
                }
                if state.status != "waiting" {
                    state.status = "waiting".into();
                    state.changed("waiting");
                    shared.wake.notify_all();
                }
                let wait = trigger_wait(trigger.as_ref().unwrap(), next_tick, Instant::now());
                drop(shared.wake.wait_timeout(state, wait).unwrap());
                continue;
            }
            if let Some(Trigger::Listen { binding, .. }) = &trigger {
                if let Err(error) = state.doc.begin_record(binding) {
                    state.error = Some(format!("{error:#}"));
                    state.status = "failed".into();
                    state.running = false;
                    state.changed("record_rejected");
                    shared.wake.notify_all();
                    continue;
                }
            }
            state.cycle = true;
            state.doc.cursor = state.plan.body_start;
            shared.wake.notify_all();
        }
        let start = state.doc.cursor;
        let end = state.plan.end(start);
        let node = Program {
            definitions: state.plan.program.definitions.clone(),
            inputs: state.plan.program.inputs.clone(),
            steps: state.plan.program.steps[start..end].to_vec(),
            trigger: None,
        };
        let context = state.doc.node_state();
        let job_session = Arc::clone(&state.job_session);
        let source_last_uses = runtime::live::job_last_uses(&state.plan.program.steps);
        let job_last_uses = source_last_uses
            .into_iter()
            .filter_map(|(name, source_index)| {
                (start <= source_index && source_index < end)
                    .then_some((name, source_index - start))
            })
            .collect();
        let final_node = end == state.plan.program.steps.len();
        let token = Arc::new(AtomicBool::new(false));
        state.active_cancel = token.clone();
        state.in_flight = true;
        state.active = Some((start, end));
        state.status = "running".into();
        state.changed("node_started");
        shared.wake.notify_all();
        drop(state);
        // A run-mode node streams uncaptured stdout to this terminal, as plain run does.
        // Breakpoints pause the node at statement boundaries through the runtime hook.
        let hook: Arc<dyn runtime::statement_hook::StatementHook> =
            Arc::new(super::debug::LiveHook {
                shared: shared.clone(),
                cancel: token.clone(),
            });
        let result = runtime::statement_hook::with_statement_hook(hook, || {
            let execute = || {
                runtime::live::execute_node_with_session(
                    &node,
                    &shared.registry,
                    &options,
                    context,
                    token,
                    runtime::live::JobNode {
                        session: job_session,
                        last_uses: job_last_uses,
                        final_node,
                    },
                )
            };
            if shared.run_mode {
                runtime::with_streamed_stdout(execute)
            } else {
                execute()
            }
        });
        let mut state = shared.state.lock().unwrap();
        state.in_flight = false;
        state.active = None;
        match result {
            Ok((result, context)) => {
                let mut candidate = state.doc.clone();
                candidate.apply_node(context);
                // `exit with` or `fail with` ends the program: no later node may run.
                let exit = result.exit.clone();
                candidate.cursor = if exit.is_some() {
                    state.plan.program.steps.len()
                } else {
                    end
                };
                if within_state_limit(&candidate).unwrap_or(false) {
                    state.doc = candidate;
                    state.nodes += 1;
                    state.launches += result.launched_processes;
                    state.cache_hits += result.cache_hits;
                    if let Some(budget) = &mut state.step_budget {
                        *budget -= 1;
                        if *budget == 0 {
                            state.running = false;
                        }
                    }
                    if let Some(exit) = exit {
                        state.exit = Some(ExitRecord {
                            status: i32::from(exit.status),
                            message: exit.message,
                        });
                    }
                    state.status = if state.running { "ready" } else { "paused" }.into();
                    state.changed("node_completed");
                } else {
                    state.error =
                        Some("result exceeds live state budget; effects may have completed".into());
                    state.uncertain = true;
                    state.running = false;
                    state.status = "failed".into();
                    state.changed("state_capacity_exceeded");
                }
            }
            Err(error) => {
                state.exit = Some(ExitRecord {
                    status: command_exit_status(&error),
                    message: None,
                });
                state.error = Some(format!("{error:#}").chars().take(4096).collect());
                state.uncertain = true;
                state.running = false;
                state.status = "failed".into();
                state.changed("node_failed");
            }
        }
        shared.wake.notify_all();
    }
    join_producer(producer);
    let job_session = Arc::clone(&shared.state.lock().unwrap().job_session);
    job_session.cancel();
}

/// The process status a failed run reports. It mirrors the CLI's mapping for a
/// failed native command: its own status (or 128 plus its signal), else 1.
fn command_exit_status(error: &anyhow::Error) -> i32 {
    let Some(failure) = error.downcast_ref::<runtime::CommandFailed>() else {
        return 1;
    };
    #[cfg(unix)]
    let signalled = {
        use std::os::unix::process::ExitStatusExt;
        failure.status.signal().map_or(1, |signal| 128 + signal)
    };
    #[cfg(not(unix))]
    let signalled = 1;
    let code = failure.status.code().unwrap_or(signalled);
    let valid = if cfg!(windows) {
        code != 0
    } else {
        (1..=255).contains(&code)
    };
    if valid {
        code
    } else {
        1
    }
}

fn trigger_wait(trigger: &Trigger, next_tick: Option<Instant>, now: Instant) -> Duration {
    // Control/source edits notify this condvar, so a long interval need not poll
    // at 50 Hz. Keep a bounded fallback while never sleeping past a due tick.
    if matches!(trigger, Trigger::Every { .. }) {
        next_tick.map_or(Duration::from_millis(20), |due| {
            due.saturating_duration_since(now)
                .min(Duration::from_secs(1))
        })
    } else {
        Duration::from_millis(20)
    }
}

fn run_stream(
    shared: &Arc<Shared>,
    options: &RunOptions,
    stream: &crate::stream::StreamProgram,
    context: runtime::live::NodeState,
    cancel: Arc<AtomicBool>,
) -> Result<usize> {
    let options = RunOptions {
        cache_directory: options.cache_directory.clone(),
        trace: options.trace,
        max_output_bytes: options.max_output_bytes,
        max_loop_iterations: options.max_loop_iterations,
        loop_budget_scope: options.loop_budget_scope,
        command_timeout: options.command_timeout,
        total_timeout: options.total_timeout,
        ..RunOptions::new(context.cwd)
    };
    // Persistent streams retain their original native argv until cancellation/restart.
    runtime::live::stream(
        stream,
        &shared.registry,
        &options,
        &context.bindings,
        cancel.clone(),
        shared.max_bytes,
        |bytes, _, _| {
            let mut state = shared.state.lock().unwrap();
            while !state.running {
                if shared.stop.load(Ordering::Relaxed) || cancel.load(Ordering::Relaxed) {
                    bail!("stream cancelled");
                }
                if state.status != "paused" {
                    state.status = "paused".into();
                    state.changed("paused");
                    shared.wake.notify_all();
                }
                state = shared
                    .wake
                    .wait_timeout(state, Duration::from_millis(20))
                    .unwrap()
                    .0;
            }
            let data = Data::from_bytes(bytes.to_vec());
            state.doc.replace_input(data.clone())?;
            state.last_output = data;
            if let Some(budget) = &mut state.step_budget {
                *budget -= 1;
                if *budget == 0 {
                    state.running = false;
                }
            }
            state.changed("stream_record");
            shared.wake.notify_all();
            Ok(())
        },
    )
    .context("controlled stream")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn interval_wait_never_delays_a_future_tick_or_spins_before_its_deadline() {
        let now = Instant::now();
        let trigger = Trigger::Every {
            interval_ms: 1,
            line: 1,
        };
        for remaining in [
            Duration::from_micros(100),
            Duration::from_millis(1),
            Duration::from_millis(5),
            Duration::from_millis(500),
            Duration::from_secs(30),
        ] {
            let wait = trigger_wait(&trigger, Some(now + remaining), now);
            assert!(!wait.is_zero());
            assert!(wait <= remaining, "a wait must not postpone a due interval");
            assert!(
                wait <= Duration::from_secs(1),
                "retain bounded fallback polling"
            );
        }
        assert_eq!(trigger_wait(&trigger, Some(now), now), Duration::ZERO);
    }
}
