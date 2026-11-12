//! Issue #416: the cloud-init example program parses and validates through the library,
//! and the cloud-init guide runs it with `--record` and keeps the same program text.

use flow::{manifest::Registry, parser, runtime};
use std::{collections::BTreeMap, fs, path::Path};

fn project_file(relative: &str) -> String {
    let path = Path::new(env!("CARGO_MANIFEST_DIR")).join(relative);
    fs::read_to_string(&path).unwrap_or_else(|error| panic!("read {}: {error}", path.display()))
}

#[test]
fn cloud_init_user_data_program_parses_and_validates() {
    let source = project_file("examples/cloud-init/user-data.program");
    let program = parser::parse(&source).expect("cloud-init example parses");
    runtime::validate_with_arguments(&program, &Registry::default(), &BTreeMap::new())
        .expect("cloud-init example validates");
}

#[test]
fn cloud_init_guide_runs_the_program_with_a_run_record() {
    let guide = project_file("docs/cloud-init.md");
    let run_line = guide
        .lines()
        .find(|line| line.contains("/bin/execute run "))
        .expect("guide has an execute run line");
    assert!(
        run_line.contains("/srv/execute/user-data.program"),
        "run line names the program: {run_line}"
    );
    assert!(
        run_line.contains("--record "),
        "run line keeps a run record: {run_line}"
    );
}

#[test]
fn cloud_init_guide_embeds_the_example_program_text() {
    let guide = project_file("docs/cloud-init.md");
    let source = project_file("examples/cloud-init/user-data.program");
    for line in source.lines().filter(|line| !line.trim().is_empty()) {
        assert!(
            guide.contains(line.trim()),
            "guide write_files is missing example line: {line}"
        );
    }
}
