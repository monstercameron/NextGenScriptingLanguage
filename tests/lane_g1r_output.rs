use std::{fs, path::Path, process::Command};

fn execute(
    directory: &Path,
    source: &str,
    command: &str,
    checkpoint: bool,
) -> std::process::Output {
    let path = directory.join("output.program");
    fs::write(&path, source).unwrap();
    let mut process = Command::new(env!("CARGO_BIN_EXE_execute"));
    process.current_dir(directory).arg(command).arg(path);
    let manifests = directory.join("manifests");
    fs::create_dir_all(&manifests).unwrap();
    fs::copy(
        Path::new(env!("CARGO_MANIFEST_DIR")).join("manifests/core.text_concat.json"),
        manifests.join("core.text_concat.json"),
    )
    .unwrap();
    let fixture = Path::new(env!("CARGO_BIN_EXE_native-fixture"));
    fs::write(
        manifests.join("emit.json"),
        serde_json::json!({
            "name": "emit", "binary": fixture,
            "parameters": {"message": {"type": "string", "required": true}},
            "arguments": [{"literal": "write"}, {"parameter": "message"}]
        })
        .to_string(),
    )
    .unwrap();
    process.arg("--manifest-dir").arg(manifests);
    if checkpoint {
        process
            .arg("--checkpoint")
            .arg(directory.join("checkpoint"));
    }
    let output = process.output().unwrap();
    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    output
}

#[test]
fn try_success_keeps_output_for_the_consumer_after_catch() {
    let directory = tempfile::tempdir().unwrap();
    let source = "try\n  run text.concat with parts [\"body\"]\ncatch failure\n  run text.concat with parts [\"caught\"]\nthen save to \"result.txt\"\n";
    let output = execute(directory.path(), source, "run", false);
    assert_eq!(
        fs::read(directory.path().join("result.txt")).unwrap(),
        b"body"
    );
    assert_eq!(output.stdout, b"body");
}

#[test]
fn checkpointed_try_success_keeps_output_for_its_consumer() {
    let directory = tempfile::tempdir().unwrap();
    let source = "try\n  run text.concat with parts [\"body\"]\ncatch failure\n  run text.concat with parts [\"caught\"]\nthen save to \"result.txt\"\n";
    assert_eq!(
        execute(directory.path(), source, "run", true).stdout,
        b"body"
    );
    assert_eq!(
        fs::read(directory.path().join("result.txt")).unwrap(),
        b"body"
    );
    assert!(execute(directory.path(), source, "resume", true)
        .stdout
        .is_empty());
}

#[test]
fn completed_checkpoint_does_not_repeat_a_final_explicit_value() {
    let directory = tempfile::tempdir().unwrap();
    let source = "use value \"visible\"\n";
    assert_eq!(
        execute(directory.path(), source, "run", true).stdout,
        b"visible"
    );
    assert!(execute(directory.path(), source, "resume", true)
        .stdout
        .is_empty());
}

#[test]
fn unbound_await_keeps_its_output_and_bound_await_withholds_it() {
    let directory = tempfile::tempdir().unwrap();
    for capture in ["", " and keep status as code", " and keep result as result"] {
        let source =
            format!("spawn as work\n  run emit with message \"done\\n\"\nawait work{capture}\n");
        let output = execute(directory.path(), &source, "run", false);
        assert_eq!(
            output.stdout,
            if capture.is_empty() {
                b"done\n".as_slice()
            } else {
                b""
            }
        );
    }
}

#[test]
fn await_replaces_prior_pipeline_data_and_streams_before_later_steps() {
    let directory = tempfile::tempdir().unwrap();
    let source = "run emit with message \"before\\n\"\nspawn as work\n  run emit with message \"job\\n\"\nawait work\nthen capture bytes as joined\nuse bytes joined\n";
    assert_eq!(
        execute(directory.path(), source, "run", false).stdout,
        b"before\njob\n"
    );
    let source = "spawn as work\n  run emit with message \"job\\n\"\nawait work\nrun emit with message \"after\\n\"\n";
    assert_eq!(
        execute(directory.path(), source, "run", false).stdout,
        b"job\nafter\n"
    );
}

#[test]
fn loop_transfers_do_not_consume_native_stdout() {
    for source in [
        "repeat 2 times\n  run emit with message \"tick\\n\"\n  continue\nrun emit with message \"end\\n\"\n",
        "repeat 2 times\n  run emit with message \"tick\\n\"\n  break\nrun emit with message \"tick\\nend\\n\"\n",
    ] {
        let directory = tempfile::tempdir().unwrap();
        assert_eq!(execute(directory.path(), source, "run", false).stdout, b"tick\ntick\nend\n");
    }
}

#[test]
fn every_control_body_emits_unconsumed_output_in_order() {
    control_bodies(false);
}

#[test]
fn checkpointed_control_bodies_match_plain_output_without_replay() {
    control_bodies(true);
}

fn control_bodies(checkpoint: bool) {
    for source in [
        "repeat 2 times\n  run emit with message \"tick\\n\"\nrun emit with message \"end\\n\"\n",
        "for each item in [\"tick\\n\", \"tick\\n\"]\n  run emit with message item\nrun emit with message \"end\\n\"\n",
        "use value true as again\nwhile again is true\n  run emit with message \"tick\\n\"\n  use value false as again\nif again is false\n  run emit with message \"tick\\n\"\nrun emit with message \"end\\n\"\n",
        "try\n  run emit with message \"tick\\n\"\nfinally\n  run emit with message \"tick\\n\"\nrun emit with message \"end\\n\"\n",
        "run emit with message \"tick\\n\"\non exit\n  run emit with message \"end\\n\"\nrun emit with message \"tick\\n\"\n",
    ] {
            let directory = tempfile::tempdir().unwrap();
            assert_eq!(execute(directory.path(), source, "run", checkpoint).stdout, b"tick\ntick\nend\n", "{source}");
            if checkpoint {
                assert!(execute(directory.path(), source, "resume", true).stdout.is_empty(), "{source}");
            }
    }
}
