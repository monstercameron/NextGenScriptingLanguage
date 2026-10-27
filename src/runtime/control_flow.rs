//! Runtime state for lexical loop/call exits and `try` cleanup.
use super::ProgramExit;
use anyhow::{anyhow, bail, Context, Error, Result};
use std::time::{Duration, Instant};

const CLEANUP_STEP_BUDGET: usize = 16_384;
const CLEANUP_TIME_BUDGET: Duration = Duration::from_secs(5);

/// Cleanup ran out of its step budget. `after_success` is set when nothing was
/// pending as it ran: the cleanup followed statements that all completed, so the
/// run's output is still worth delivering with the failure.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct CleanupBudgetExceeded {
    pub(crate) after_success: bool,
}

impl std::fmt::Display for CleanupBudgetExceeded {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(
            formatter,
            "cleanup step budget of {CLEANUP_STEP_BUDGET} exceeded in a finally, on exit or on interrupt block"
        )
    }
}

impl std::error::Error for CleanupBudgetExceeded {}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum TransferKind {
    Break,
    Continue,
    Return,
    /// Ends a reusable call because its caller captures native status, without
    /// cancelling a run exit pending in an enclosing cleanup block.
    CallStatus,
    /// Unwinds to the end of the finite run. The pending exit record supplies
    /// the status. Unlike an error, it is never caught.
    Exit,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct UnwindTo {
    pub call_depth: usize,
    pub loop_depth: usize,
    pub drop_bindings: Vec<String>,
}

impl UnwindTo {
    pub(crate) fn new(call_depth: usize, loop_depth: usize) -> Self {
        Self {
            call_depth,
            loop_depth,
            drop_bindings: Vec::new(),
        }
    }
}

pub(crate) enum ControlAction {
    Continue {
        drop_bindings: Vec<String>,
    },
    Jump {
        target: usize,
        transfer: Option<TransferKind>,
        unwind: UnwindTo,
    },
    Catch {
        target: usize,
        binding: String,
        error: Error,
        job_scope: usize,
        unwind: UnwindTo,
    },
    Rethrow(Error),
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct LoopTarget {
    pub header: usize,
    pub finish: usize,
    /// Loop depth after the target loop has been entered.
    pub depth: usize,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct CallTarget {
    pub end_marker: usize,
    pub call_depth: usize,
    pub loop_depth: usize,
}

#[derive(Debug)]
struct LoopFrame {
    header: usize,
    finish: usize,
}

#[derive(Debug)]
struct CallFrame {
    end_marker: usize,
    loop_depth: usize,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum TryPhase {
    Body,
    Catch,
    Finally,
}

enum PendingExit {
    Error {
        error: Error,
        cancellation: bool,
    },
    Transfer {
        kind: TransferKind,
        target: usize,
        unwind: UnwindTo,
    },
}

fn preserve_cancellation(primary: Error, pending: Option<PendingExit>) -> Error {
    match pending {
        None => primary,
        Some(PendingExit::Error {
            error: prior,
            cancellation: true,
        }) => prior.context(format!(
            "cleanup also failed during cancellation: {primary:#}"
        )),
        Some(PendingExit::Error { error, .. }) => primary.context(format!(
            "earlier failure also pending during cancellation: {error:#}"
        )),
        Some(PendingExit::Transfer { kind, .. }) => primary.context(format!(
            "pending {kind:?} transfer was superseded by cancellation"
        )),
    }
}

struct TryFrame {
    catch: Option<(usize, String)>,
    finally: Option<usize>,
    end_marker: usize,
    call_depth: usize,
    loop_depth: usize,
    job_scope: usize,
    phase: TryPhase,
    pending: Option<PendingExit>,
    catch_binding_active: bool,
}

#[derive(Default)]
pub(crate) struct RuntimeState {
    calls: Vec<CallFrame>,
    loops: Vec<LoopFrame>,
    tries: Vec<TryFrame>,
    cleanup_deadline: Option<Instant>,
    cleanup_steps: usize,
    /// The exit that ends the run once its unwinding reaches the end. Errors
    /// and non-exit transfers that replace it clear this record.
    exit: Option<ProgramExit>,
}

impl RuntimeState {
    pub(crate) fn set_exit(&mut self, exit: ProgramExit) {
        self.exit = Some(exit);
    }

    pub(crate) fn exit_pending(&self) -> bool {
        self.exit.is_some()
    }

    pub(crate) fn take_exit(&mut self) -> Option<ProgramExit> {
        self.exit.take()
    }

    pub(crate) fn enter_call(&mut self, end_marker: usize) {
        self.calls.push(CallFrame {
            end_marker,
            loop_depth: self.loops.len(),
        });
    }

    pub(crate) fn leave_call(&mut self, end_marker: usize) -> Result<()> {
        let frame = self
            .calls
            .pop()
            .ok_or_else(|| anyhow!("program end without call"))?;
        if frame.end_marker != end_marker {
            bail!("program end does not match active call");
        }
        if self.loops.len() != frame.loop_depth {
            bail!("program returned with active loops");
        }
        Ok(())
    }

    pub(crate) fn enter_loop(&mut self, header: usize, finish: usize) -> Result<()> {
        if let Some(frame) = self.loops.last() {
            if frame.header == header {
                if frame.finish != finish {
                    bail!("loop boundary changed while active");
                }
                return Ok(());
            }
            if finish >= frame.finish {
                bail!("loop blocks are not properly nested");
            }
        }
        self.loops.push(LoopFrame { header, finish });
        Ok(())
    }

    pub(crate) fn leave_loop(&mut self, header: usize) -> Result<()> {
        let frame = self
            .loops
            .pop()
            .ok_or_else(|| anyhow!("loop end without active loop"))?;
        if frame.header != header {
            bail!("loop end does not match active loop");
        }
        Ok(())
    }

    pub(crate) fn current_loop(&self) -> Result<LoopTarget> {
        let minimum_depth = self.calls.last().map_or(0, |frame| frame.loop_depth);
        let (depth, frame) = self
            .loops
            .iter()
            .enumerate()
            .rev()
            .find(|(index, _)| *index >= minimum_depth)
            .ok_or_else(|| anyhow!("loop transfer outside a loop"))?;
        Ok(LoopTarget {
            header: frame.header,
            finish: frame.finish,
            depth: depth + 1,
        })
    }

    pub(crate) fn current_call(&self) -> Result<CallTarget> {
        let frame = self
            .calls
            .last()
            .ok_or_else(|| anyhow!("return outside a reusable program"))?;
        Ok(CallTarget {
            // Calls store the exclusive end offset so ordinary execution and
            // EndCall validation share the same half-open boundary. A return
            // transfer targets the EndCall step itself so that its usual
            // frame restoration and output binding still execute.
            end_marker: frame
                .end_marker
                .checked_sub(1)
                .context("invalid reusable-program end offset")?,
            call_depth: self.calls.len(),
            loop_depth: frame.loop_depth,
        })
    }

    pub(crate) fn call_depth(&self) -> usize {
        self.calls.len()
    }

    pub(crate) fn enter_try(
        &mut self,
        header: usize,
        catch: Option<(usize, String)>,
        finally: Option<usize>,
        end_marker: usize,
    ) -> Result<()> {
        self.enter_try_with_job_scope(header, catch, finally, end_marker, 0)
    }

    pub(crate) fn enter_try_with_job_scope(
        &mut self,
        header: usize,
        catch: Option<(usize, String)>,
        finally: Option<usize>,
        end_marker: usize,
        job_scope: usize,
    ) -> Result<()> {
        if end_marker <= header
            || catch
                .as_ref()
                .is_some_and(|(marker, _)| *marker <= header || *marker >= end_marker)
            || finally.is_some_and(|marker| marker <= header || marker >= end_marker)
        {
            bail!("invalid try block offsets");
        }
        if let Some(parent) = self.tries.last() {
            if end_marker >= parent.end_marker {
                bail!("try blocks are not properly nested");
            }
        }
        self.tries.push(TryFrame {
            catch,
            finally,
            end_marker,
            call_depth: self.calls.len(),
            loop_depth: self.loops.len(),
            job_scope,
            phase: TryPhase::Body,
            pending: None,
            catch_binding_active: false,
        });
        Ok(())
    }

    pub(crate) fn active_job_scope(&self, call_depth: usize) -> Option<usize> {
        self.tries
            .last()
            .filter(|frame| frame.call_depth == call_depth)
            .map(|frame| frame.job_scope)
    }

    pub(crate) fn job_scopes(&self, call_depth: usize) -> Vec<usize> {
        self.tries
            .iter()
            .rev()
            .filter(|frame| frame.call_depth == call_depth)
            .map(|frame| frame.job_scope)
            .collect()
    }

    pub(crate) fn job_scope_at_end(&self, end_marker: usize) -> Result<usize> {
        self.tries
            .last()
            .filter(|frame| frame.end_marker == end_marker)
            .map(|frame| frame.job_scope)
            .ok_or_else(|| anyhow!("try end does not match active job scope"))
    }

    /// Normal execution reaching Catch skips its body. An error transition
    /// jumps directly to the first catcher statement after binding the record.
    pub(crate) fn catch_marker(&mut self, marker: usize) -> Result<ControlAction> {
        let frame = self.try_at_marker_mut(marker, false)?;
        if frame.phase == TryPhase::Body {
            // Catch is a marker before its handler body. On ordinary success,
            // skip the handler but still execute EndTry to pop this frame.
            let target = frame.finally.unwrap_or(frame.end_marker - 1);
            Ok(ControlAction::Jump {
                target,
                transfer: None,
                unwind: UnwindTo::new(frame.call_depth, frame.loop_depth),
            })
        } else if frame.phase == TryPhase::Catch {
            bail!("catch marker was entered twice");
        } else {
            bail!("catch marker appears after finally");
        }
    }

    pub(crate) fn finally_marker(&mut self, marker: usize) -> Result<ControlAction> {
        let frame = self.try_at_marker_mut(marker, true)?;
        if frame.phase == TryPhase::Finally {
            return Ok(ControlAction::Continue {
                drop_bindings: Vec::new(),
            });
        }
        let drop_bindings = Self::take_catch_binding(frame);
        frame.phase = TryPhase::Finally;
        self.start_cleanup();
        Ok(ControlAction::Continue { drop_bindings })
    }

    pub(crate) fn end_try(&mut self, marker: usize) -> Result<ControlAction> {
        self.end_try_before_call(marker, None)
    }

    /// Complete a try inside a call that captures native status. If its pending
    /// error came from a failed native command, leave caller-level catches active
    /// so the call can convert that failure into its completion status.
    pub(crate) fn end_try_for_call(
        &mut self,
        marker: usize,
        call_depth: usize,
    ) -> Result<ControlAction> {
        self.end_try_before_call(marker, Some(call_depth))
    }

    fn end_try_before_call(
        &mut self,
        marker: usize,
        minimum_call_depth: Option<usize>,
    ) -> Result<ControlAction> {
        let frame = self
            .tries
            .pop()
            .ok_or_else(|| anyhow!("try end without active try"))?;
        if frame.end_marker != marker {
            bail!("try end does not match active try");
        }
        let drops = Self::active_catch_binding(&frame)
            .into_iter()
            .collect::<Vec<_>>();
        let pending = frame.pending;
        let action = match pending {
            Some(PendingExit::Error {
                error,
                cancellation,
            }) => {
                let action = if minimum_call_depth.is_some_and(|_| {
                    error
                        .downcast_ref::<super::CommandFailed>()
                        .and_then(|failure| failure.status.code())
                        .is_some()
                }) {
                    self.fail_before_call(
                        error,
                        cancellation,
                        minimum_call_depth.expect("checked above"),
                        true,
                    )?
                } else {
                    self.fail(error, cancellation)?
                };
                Ok(Self::with_drops(action, drops))
            }
            Some(PendingExit::Transfer {
                kind,
                target,
                mut unwind,
            }) => {
                unwind.drop_bindings.extend(drops);
                self.transfer(kind, target, unwind)
            }
            None => Ok(ControlAction::Continue {
                drop_bindings: drops,
            }),
        }?;
        self.reset_cleanup_if_idle();
        Ok(action)
    }

    pub(crate) fn transfer(
        &mut self,
        kind: TransferKind,
        target: usize,
        mut unwind: UnwindTo,
    ) -> Result<ControlAction> {
        // A break, continue or return that replaces a pending exit (for example
        // from finally) cancels it; the exit transfer itself keeps its record.
        if !matches!(kind, TransferKind::Exit | TransferKind::CallStatus) {
            self.exit = None;
        }
        loop {
            let Some(frame) = self.tries.last_mut() else {
                unwind.call_depth = unwind.call_depth.min(self.calls.len());
                unwind.loop_depth = unwind.loop_depth.min(self.loops.len());
                return Ok(ControlAction::Jump {
                    target,
                    transfer: Some(kind),
                    unwind,
                });
            };
            if frame.end_marker > target {
                unwind.drop_bindings.extend(Self::take_catch_binding(frame));
                unwind.call_depth = unwind.call_depth.min(self.calls.len());
                unwind.loop_depth = unwind.loop_depth.min(self.loops.len());
                return Ok(ControlAction::Jump {
                    target,
                    transfer: Some(kind),
                    unwind,
                });
            }
            if frame.phase == TryPhase::Finally {
                if let Some(PendingExit::Error {
                    error,
                    cancellation: true,
                }) = &mut frame.pending
                {
                    let prior = std::mem::replace(error, anyhow!("cancellation pending"));
                    *error = prior.context(format!(
                        "ignored {kind:?} transfer while cancellation cleanup was active"
                    ));
                    return Ok(ControlAction::Continue {
                        drop_bindings: Vec::new(),
                    });
                }
                // A control transfer from finally overrides an earlier pending
                // exit, including an error, and continues through outer cleanup.
                let mut frame = self.tries.pop().expect("active try");
                unwind
                    .drop_bindings
                    .extend(Self::take_catch_binding(&mut frame));
                continue;
            }
            if let Some(finally) = frame.finally {
                let (call_depth, loop_depth, drop_bindings) = {
                    let frame = self.tries.last_mut().expect("active try");
                    frame.pending = Some(PendingExit::Transfer {
                        kind,
                        target,
                        unwind,
                    });
                    (
                        frame.call_depth,
                        frame.loop_depth,
                        Self::take_catch_binding(frame),
                    )
                };
                self.start_cleanup();
                return Ok(ControlAction::Jump {
                    target: finally,
                    transfer: None,
                    unwind: UnwindTo {
                        call_depth,
                        loop_depth,
                        drop_bindings,
                    },
                });
            }
            let mut frame = self.tries.pop().expect("active try");
            unwind
                .drop_bindings
                .extend(Self::take_catch_binding(&mut frame));
        }
    }

    pub(crate) fn fail(&mut self, error: Error, cancellation: bool) -> Result<ControlAction> {
        self.fail_before_call(error, cancellation, 0, false)
    }

    /// Route a native error through tries created within the status-capturing
    /// call, but do not expose it to a caller's catch block. The call captures
    /// the native status after its own cleanup has completed.
    pub(crate) fn fail_for_call(
        &mut self,
        error: Error,
        cancellation: bool,
        call_depth: usize,
    ) -> Result<ControlAction> {
        self.fail_before_call(error, cancellation, call_depth, true)
    }

    fn fail_before_call(
        &mut self,
        error: Error,
        mut cancellation: bool,
        minimum_call_depth: usize,
        preserve_exit: bool,
    ) -> Result<ControlAction> {
        // An error or cancellation supersedes a pending explicit exit.
        if !preserve_exit {
            self.exit = None;
        }
        let mut error = error;
        let mut drop_bindings = Vec::new();
        loop {
            let Some(frame) = self.tries.last_mut() else {
                self.reset_cleanup_if_idle();
                return Ok(ControlAction::Rethrow(error));
            };
            if frame.call_depth < minimum_call_depth {
                // This try belongs to the caller, outside the reusable call that
                // captures status. Keep its scope active while returning normally.
                self.reset_cleanup_if_idle();
                return Ok(ControlAction::Rethrow(error));
            }
            match frame.phase {
                TryPhase::Body if !cancellation => {
                    if let Some((marker, binding)) = &frame.catch {
                        let target = marker + 1;
                        let binding = binding.clone();
                        frame.phase = TryPhase::Catch;
                        frame.catch_binding_active = true;
                        let mut unwind = UnwindTo::new(frame.call_depth, frame.loop_depth);
                        unwind.drop_bindings.append(&mut drop_bindings);
                        return Ok(ControlAction::Catch {
                            target,
                            binding,
                            error,
                            job_scope: frame.job_scope,
                            unwind,
                        });
                    }
                    if let Some(finally) = frame.finally {
                        drop_bindings.extend(Self::take_catch_binding(frame));
                        frame.phase = TryPhase::Finally;
                        frame.pending = Some(PendingExit::Error {
                            error,
                            cancellation,
                        });
                        let unwind = UnwindTo {
                            call_depth: frame.call_depth,
                            loop_depth: frame.loop_depth,
                            drop_bindings: std::mem::take(&mut drop_bindings),
                        };
                        self.start_cleanup();
                        return Ok(ControlAction::Jump {
                            target: finally,
                            transfer: None,
                            unwind,
                        });
                    }
                    drop_bindings.extend(Self::take_catch_binding(frame));
                    self.tries.pop();
                }
                TryPhase::Body => {
                    if let Some(finally) = frame.finally {
                        drop_bindings.extend(Self::take_catch_binding(frame));
                        frame.phase = TryPhase::Finally;
                        frame.pending = Some(PendingExit::Error {
                            error,
                            cancellation,
                        });
                        let unwind = UnwindTo {
                            call_depth: frame.call_depth,
                            loop_depth: frame.loop_depth,
                            drop_bindings: std::mem::take(&mut drop_bindings),
                        };
                        self.start_cleanup();
                        return Ok(ControlAction::Jump {
                            target: finally,
                            transfer: None,
                            unwind,
                        });
                    }
                    drop_bindings.extend(Self::take_catch_binding(frame));
                    self.tries.pop();
                }
                TryPhase::Catch => {
                    if let Some(finally) = frame.finally {
                        drop_bindings.extend(Self::take_catch_binding(frame));
                        frame.phase = TryPhase::Finally;
                        frame.pending = Some(PendingExit::Error {
                            error,
                            cancellation,
                        });
                        let unwind = UnwindTo {
                            call_depth: frame.call_depth,
                            loop_depth: frame.loop_depth,
                            drop_bindings: std::mem::take(&mut drop_bindings),
                        };
                        self.start_cleanup();
                        return Ok(ControlAction::Jump {
                            target: finally,
                            transfer: None,
                            unwind,
                        });
                    }
                    error = error.context("catch block failed");
                    drop_bindings.extend(Self::take_catch_binding(frame));
                    self.tries.pop();
                }
                TryPhase::Finally => {
                    if cancellation {
                        let frame = self.tries.last_mut().expect("active try");
                        frame.pending = Some(PendingExit::Error {
                            error: preserve_cancellation(error, frame.pending.take()),
                            cancellation: true,
                        });
                        drop_bindings.extend(Self::take_catch_binding(frame));
                        return Ok(ControlAction::Continue { drop_bindings });
                    }
                    let mut frame = self.tries.pop().expect("active try");
                    drop_bindings.extend(Self::take_catch_binding(&mut frame));
                    match frame.pending.take() {
                        Some(PendingExit::Error {
                            error: prior,
                            cancellation: true,
                        }) => {
                            error = preserve_cancellation(
                                error,
                                Some(PendingExit::Error {
                                    error: prior,
                                    cancellation: true,
                                }),
                            );
                            // The original cancellation remains uncaught even
                            // when cleanup itself fails.
                            cancellation = true;
                        }
                        Some(PendingExit::Error { error: prior, .. }) => {
                            error = error.context(format!(
                                "finally block failed while unwinding earlier error: {prior:#}"
                            ));
                        }
                        Some(PendingExit::Transfer { kind, .. }) => {
                            error = error.context(format!(
                                "finally block failed while unwinding pending {kind:?} transfer"
                            ));
                        }
                        None => {}
                    }
                }
            }
        }
    }

    pub(crate) fn apply_unwind(&mut self, unwind: &UnwindTo) {
        self.calls.truncate(unwind.call_depth);
        self.loops.truncate(unwind.loop_depth);
    }

    pub(crate) fn cleanup_active(&self) -> bool {
        self.tries.iter().any(|frame| {
            frame.phase == TryPhase::Finally || (frame.pending.is_some() && frame.finally.is_some())
        })
    }

    pub(crate) fn cleanup_deadline(&self) -> Option<Instant> {
        self.cleanup_deadline
    }

    pub(crate) fn charge_cleanup_step(&mut self) -> Result<()> {
        if !self.cleanup_active() {
            return Ok(());
        }
        if self
            .cleanup_deadline
            .is_some_and(|deadline| Instant::now() >= deadline)
        {
            bail!("cleanup budget of {:?} exceeded", CLEANUP_TIME_BUDGET);
        }
        if self.cleanup_steps >= CLEANUP_STEP_BUDGET {
            return Err(CleanupBudgetExceeded {
                after_success: self.tries.iter().all(|frame| frame.pending.is_none()),
            }
            .into());
        }
        self.cleanup_steps += 1;
        Ok(())
    }

    fn try_at_marker_mut(&mut self, marker: usize, finally: bool) -> Result<&mut TryFrame> {
        let frame = self.tries.last_mut().ok_or_else(|| {
            anyhow!(
                "{} marker without active try",
                if finally { "finally" } else { "catch" }
            )
        })?;
        let expected = if finally {
            frame.finally
        } else {
            frame.catch.as_ref().map(|(index, _)| *index)
        };
        if expected != Some(marker) {
            bail!(
                "{} marker does not match active try",
                if finally { "finally" } else { "catch" }
            );
        }
        Ok(frame)
    }

    fn start_cleanup(&mut self) {
        if self.cleanup_deadline.is_none() {
            self.cleanup_deadline = Instant::now().checked_add(CLEANUP_TIME_BUDGET);
            self.cleanup_steps = 0;
        }
    }

    fn reset_cleanup_if_idle(&mut self) {
        if !self.cleanup_active() {
            self.cleanup_deadline = None;
            self.cleanup_steps = 0;
        }
    }

    fn take_catch_binding(frame: &mut TryFrame) -> Vec<String> {
        if frame.catch_binding_active {
            frame.catch_binding_active = false;
            frame
                .catch
                .as_ref()
                .map(|(_, binding)| vec![binding.clone()])
                .unwrap_or_default()
        } else {
            Vec::new()
        }
    }

    fn active_catch_binding(frame: &TryFrame) -> Option<String> {
        frame
            .catch_binding_active
            .then(|| frame.catch.as_ref().map(|(_, binding)| binding.clone()))
            .flatten()
    }

    fn with_drops(action: ControlAction, drops: Vec<String>) -> ControlAction {
        match action {
            ControlAction::Continue { mut drop_bindings } => {
                drop_bindings.extend(drops);
                ControlAction::Continue { drop_bindings }
            }
            ControlAction::Jump {
                target,
                transfer,
                mut unwind,
            } => {
                unwind.drop_bindings.extend(drops);
                ControlAction::Jump {
                    target,
                    transfer,
                    unwind,
                }
            }
            ControlAction::Catch {
                target,
                binding,
                error,
                job_scope,
                mut unwind,
            } => {
                unwind.drop_bindings.extend(drops);
                ControlAction::Catch {
                    target,
                    binding,
                    error,
                    job_scope,
                    unwind,
                }
            }
            ControlAction::Rethrow(error) => ControlAction::Rethrow(error),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn jump(action: ControlAction) -> (usize, Option<TransferKind>, UnwindTo) {
        let ControlAction::Jump {
            target,
            transfer,
            unwind,
        } = action
        else {
            panic!("expected jump action")
        };
        (target, transfer, unwind)
    }

    #[test]
    fn loop_continue_runs_finally_before_reaching_end_loop() {
        let mut state = RuntimeState::default();
        state.enter_loop(0, 20).unwrap();
        state.enter_try(2, None, Some(4), 6).unwrap();

        let action = state
            .transfer(TransferKind::Continue, 19, UnwindTo::new(0, 1))
            .unwrap();
        assert_eq!(jump(action).0, 4);
        assert!(state.cleanup_active());

        let action = state.end_try(6).unwrap();
        let (target, transfer, unwind) = jump(action);
        assert_eq!(target, 19);
        assert_eq!(transfer, Some(TransferKind::Continue));
        assert_eq!(unwind.loop_depth, 1);
        assert!(!state.cleanup_active());
    }

    #[test]
    fn nested_finalizers_share_one_cleanup_budget() {
        let mut state = RuntimeState::default();
        state.enter_try(0, None, Some(7), 11).unwrap();
        state.enter_try(1, None, Some(4), 6).unwrap();
        let action = state
            .transfer(TransferKind::Break, 12, UnwindTo::new(0, 0))
            .unwrap();
        assert_eq!(jump(action).0, 4);
        let deadline = state.cleanup_deadline().unwrap();
        state.charge_cleanup_step().unwrap();

        let action = state.end_try(6).unwrap();
        assert_eq!(jump(action).0, 7);
        assert_eq!(state.cleanup_deadline(), Some(deadline));
        state.charge_cleanup_step().unwrap();

        let action = state.end_try(11).unwrap();
        let (target, transfer, _) = jump(action);
        assert_eq!(target, 12);
        assert_eq!(transfer, Some(TransferKind::Break));
        assert!(!state.cleanup_active());
        assert_eq!(state.cleanup_deadline(), None);
    }

    #[test]
    fn cleanup_budget_allows_a_sixty_four_item_loop() {
        let mut state = RuntimeState::default();
        state.enter_try(0, None, Some(2), 4).unwrap();
        state
            .transfer(TransferKind::Exit, 5, UnwindTo::new(0, 0))
            .unwrap();
        for _ in 0..(64 * 3) {
            state.charge_cleanup_step().unwrap();
        }
    }

    #[test]
    fn cleanup_budget_overrun_says_whether_anything_was_pending() {
        let mut state = RuntimeState::default();
        state.enter_try(0, None, Some(2), 4).unwrap();
        // Ordinary completion reaches finally with nothing pending.
        state.finally_marker(2).unwrap();
        for _ in 0..CLEANUP_STEP_BUDGET {
            state.charge_cleanup_step().unwrap();
        }
        let error = state.charge_cleanup_step().unwrap_err();
        let overrun = error.downcast_ref::<CleanupBudgetExceeded>().unwrap();
        assert!(overrun.after_success);

        let mut state = RuntimeState::default();
        state.enter_try(0, None, Some(2), 4).unwrap();
        state.fail(anyhow!("body failed"), false).unwrap();
        for _ in 0..CLEANUP_STEP_BUDGET {
            state.charge_cleanup_step().unwrap();
        }
        let error = state.charge_cleanup_step().unwrap_err();
        let overrun = error.downcast_ref::<CleanupBudgetExceeded>().unwrap();
        assert!(!overrun.after_success);
    }

    #[test]
    fn cancellation_stays_primary_when_nested_cleanup_fails() {
        let mut state = RuntimeState::default();
        state.enter_try(0, None, Some(7), 10).unwrap();
        state.enter_try(1, None, Some(4), 6).unwrap();

        let action = state
            .fail(anyhow!("workflow cancelled by signal"), true)
            .unwrap();
        assert_eq!(jump(action).0, 4);
        let action = state.fail(anyhow!("inner cleanup failed"), false).unwrap();
        assert_eq!(jump(action).0, 7);
        let ControlAction::Rethrow(error) = state.end_try(10).unwrap() else {
            panic!("cancellation must escape after outer cleanup")
        };
        let chain = format!("{error:#}");
        assert!(chain.contains("workflow cancelled by signal"), "{chain}");
        assert!(chain.contains("inner cleanup failed"), "{chain}");
    }

    #[test]
    fn cancellation_suppresses_loop_transfer_inside_its_finalizer() {
        let mut state = RuntimeState::default();
        state.enter_loop(0, 20).unwrap();
        state.enter_try(2, None, Some(4), 6).unwrap();
        let action = state
            .fail(anyhow!("workflow cancelled by signal"), true)
            .unwrap();
        assert_eq!(jump(action).0, 4);

        assert!(matches!(
            state.transfer(TransferKind::Break, 20, UnwindTo::new(0, 0)),
            Ok(ControlAction::Continue { .. })
        ));
        let ControlAction::Rethrow(error) = state.end_try(6).unwrap() else {
            panic!("cancellation must remain the pending exit")
        };
        assert!(format!("{error:#}").contains("workflow cancelled by signal"));
    }
}
