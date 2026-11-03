use flow::manifest::Registry;
use flow::runtime::{run, RunOptions};
use std::process::{Command, Stdio};
use std::thread;
use std::time::{Duration, Instant};

#[test]
fn interrupt_runtime_child() -> anyhow::Result<()> {
    let Ok(marker) = std::env::var("EXECUTE_CONTROL_INTERRUPT_CHILD") else {
        return Ok(());
    };
    let directory = std::path::Path::new(&marker)
        .parent()
        .expect("marker has parent")
        .to_path_buf();
    let program = flow::parser::parse(&format!(
        "on interrupt\n  use value \"handled\"\n  then save to {:?}\nuse value \"loop\" as marker\nwhile marker is \"loop\"\n  use value \"loop\" as marker\n",
        marker
    ))?;
    let registry = Registry::from_sources(&[] as &[&str])?;
    let result = run(
        &program,
        &registry,
        &RunOptions {
            working_directory: directory,
            ..RunOptions::new(std::env::temp_dir())
        },
    );
    let result = result?;
    assert_eq!(result.exit.map(|exit| exit.status), Some(130));
    Ok(())
}

#[cfg(unix)]
fn send_interrupt(child: &std::process::Child) -> anyhow::Result<()> {
    let result = unsafe { libc::kill(child.id() as libc::pid_t, libc::SIGINT) };
    anyhow::ensure!(result == 0, "failed to send SIGINT");
    Ok(())
}

#[cfg(windows)]
fn send_interrupt(child: &std::process::Child) -> anyhow::Result<()> {
    const CTRL_BREAK_EVENT: u32 = 1;
    #[link(name = "Kernel32")]
    unsafe extern "system" {
        fn GenerateConsoleCtrlEvent(event: u32, process_group_id: u32) -> i32;
    }
    let result = unsafe { GenerateConsoleCtrlEvent(CTRL_BREAK_EVENT, child.id()) };
    anyhow::ensure!(result != 0, "failed to send Ctrl-Break to child process group");
    Ok(())
}

#[test]
fn on_interrupt_runs_body_and_reports_interrupt() -> anyhow::Result<()> {
    let directory = tempfile::tempdir()?;
    let marker = directory.path().join("interrupt-marker");
    let mut command = Command::new(std::env::current_exe()?);
    command
        .arg("--exact")
        .arg("interrupt_runtime_child")
        .arg("--nocapture")
        .env("EXECUTE_CONTROL_INTERRUPT_CHILD", &marker)
        .current_dir(directory.path())
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped());
    #[cfg(windows)]
    {
        use std::os::windows::process::CommandExt;
        command.creation_flags(0x0000_0200); // CREATE_NEW_PROCESS_GROUP
    }
    let mut child = command.spawn()?;
    thread::sleep(Duration::from_millis(250));
    send_interrupt(&child)?;

    let deadline = Instant::now() + Duration::from_secs(5);
    while child.try_wait()?.is_none() && Instant::now() < deadline {
        thread::sleep(Duration::from_millis(20));
    }
    if child.try_wait()?.is_none() {
        let _ = child.kill();
        anyhow::bail!("interrupted runtime child did not finish draining");
    }
    let output = child.wait_with_output()?;
    assert!(
        output.status.success(),
        "child failed: {}{}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );
    assert_eq!(std::fs::read(marker)?, b"handled");
    Ok(())
}
