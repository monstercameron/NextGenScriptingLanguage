#![cfg(target_os = "linux")]
//! Opt-in Lane V2 Linux measurements. Production choices require CI timings.
use std::os::fd::AsRawFd;
use std::os::unix::process::CommandExt;
use std::process::{Command, Stdio};
use std::time::{Duration, Instant};

fn median(mut samples: Vec<Duration>) -> Duration {
    samples.sort_unstable();
    samples[samples.len() / 2]
}

#[test]
#[ignore = "Linux launch profile; run with strace -f -c and perf stat"]
fn lane_v2_linux_launch_setup() {
    for mode in ["ordinary", "group", "directory_handle"] {
        let mut samples = Vec::new();
        let directory =
            flow::working_dir::WorkingDirectory::open(std::path::Path::new("/")).unwrap();
        for _ in 0..9 {
            let start = Instant::now();
            for _ in 0..100 {
                let mut command = Command::new("/bin/true");
                command
                    .stdin(Stdio::null())
                    .stdout(Stdio::null())
                    .stderr(Stdio::null());
                if mode != "ordinary" {
                    command.process_group(0);
                }
                if mode == "directory_handle" {
                    directory.apply(&mut command);
                }
                assert!(command.spawn().unwrap().wait().unwrap().success());
            }
            samples.push(start.elapsed());
        }
        eprintln!("Linux 100 launches mode={mode}: {:?}", median(samples));
    }
}

fn pipe_trial(bytes: usize, stages: usize, capacity: Option<i32>) -> (Duration, usize) {
    let start = Instant::now();
    let mut source = Command::new("head")
        .args(["-c", &bytes.to_string(), "/dev/zero"])
        .stdout(Stdio::piped())
        .spawn()
        .unwrap();
    let mut previous = source.stdout.take().unwrap();
    let mut children = vec![source];
    let mut resized = 0;
    for _ in 1..stages {
        if let Some(capacity) = capacity {
            // SAFETY: previous owns this live pipe descriptor. F_SETPIPE_SZ
            // accepts an integer capacity and changes no descriptor ownership.
            if unsafe { libc::fcntl(previous.as_raw_fd(), libc::F_SETPIPE_SZ, capacity) } >= 0 {
                resized += 1;
            }
        }
        let mut child = Command::new("cat")
            .stdin(previous)
            .stdout(Stdio::piped())
            .spawn()
            .unwrap();
        previous = child.stdout.take().unwrap();
        children.push(child);
    }
    let counter = Command::new("wc")
        .arg("-c")
        .stdin(previous)
        .stdout(Stdio::piped())
        .spawn()
        .unwrap()
        .wait_with_output()
        .unwrap();
    for child in &mut children {
        assert!(child.wait().unwrap().success());
    }
    assert!(counter.status.success());
    assert_eq!(
        String::from_utf8(counter.stdout)
            .unwrap()
            .trim()
            .parse::<usize>()
            .unwrap(),
        bytes
    );
    (start.elapsed(), resized)
}

#[test]
#[ignore = "Linux pipe capacity sweep; retain a production setting only after a CI win"]
fn lane_v2_native_pipe_capacity_sweep() {
    for (bytes, stages) in [(100 * 1024 * 1024, 2), (16, 5)] {
        let mut samples = [Vec::new(), Vec::new(), Vec::new()];
        let capacities = [None, Some(256 * 1024), Some(1024 * 1024)];
        let mut resized = [0; 3];
        for batch in 0..9 {
            for mode in if batch % 2 == 0 { [0, 1, 2] } else { [2, 1, 0] } {
                let (elapsed, changes) = pipe_trial(bytes, stages, capacities[mode]);
                samples[mode].push(elapsed);
                resized[mode] += changes;
            }
        }
        for (mode, sample) in samples.into_iter().enumerate() {
            eprintln!(
                "pipe bytes={bytes} stages={stages} capacity={:?}: {:?}; successful resizes={}",
                capacities[mode],
                median(sample),
                resized[mode]
            );
        }
    }
}

#[test]
#[ignore = "1 GiB save baseline; use /usr/bin/time -v to record RSS"]
fn lane_v2_one_gib_save() {
    use std::io::{Read, Seek, SeekFrom};
    let root = tempfile::tempdir().unwrap();
    let script = root.path().join("save.program");
    std::fs::write(&script, "run \"head\" with arguments [\"-c\", \"1073741824\", \"/dev/zero\"]\nthen save to file \"large.bin\"\n").unwrap();
    let start = Instant::now();
    let status = Command::new(env!("CARGO_BIN_EXE_execute"))
        .arg("run")
        .arg(&script)
        .args(["--max-output-bytes", "1073741824"])
        .current_dir(root.path())
        .stdout(Stdio::null())
        .status()
        .unwrap();
    eprintln!("1 GiB save elapsed {:?}", start.elapsed());
    assert!(status.success());
    let mut saved = std::fs::File::open(root.path().join("large.bin")).unwrap();
    assert_eq!(saved.metadata().unwrap().len(), 1024 * 1024 * 1024);
    let mut bytes = [1; 128];
    saved.read_exact(&mut bytes).unwrap();
    assert_eq!(bytes, [0; 128]);
    saved.seek(SeekFrom::End(-128)).unwrap();
    saved.read_exact(&mut bytes).unwrap();
    assert_eq!(bytes, [0; 128]);
}
