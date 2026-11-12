//! `# omarchy:` headers for the ports in `examples/omarchy-ports/`. The lines that
//! `help --format omarchy-metadata` generates from a port's declarations must pass the
//! lint once they are embedded in the first 80 lines. Nothing here runs a port.

use flow::agent::help::{omarchy_metadata_problems, render_omarchy_metadata};
use std::path::{Path, PathBuf};

fn ports_root() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR")).join("examples/omarchy-ports")
}

/// The ten interface ports (`<name>.program`) and the twenty behaviour ports
/// (`<name>/port.program`).
fn port_files() -> Vec<PathBuf> {
    let mut files = Vec::new();
    for entry in std::fs::read_dir(ports_root()).unwrap() {
        let path = entry.unwrap().path();
        if path.is_file() && path.extension().and_then(|e| e.to_str()) == Some("program") {
            files.push(path);
        } else if path.is_dir() && path.join("port.program").is_file() {
            files.push(path.join("port.program"));
        }
    }
    files.sort();
    files
}

#[test]
fn every_port_metadata_is_well_formed_and_lints_clean_when_embedded() {
    let files = port_files();
    assert!(files.len() >= 30, "found {} ports", files.len());
    let mut with_metadata = 0;
    for file in &files {
        let source = std::fs::read_to_string(file).unwrap();
        let program = flow::parser::parse(&source)
            .unwrap_or_else(|error| panic!("{}: {error:#}", file.display()));
        let metadata = render_omarchy_metadata(&program);
        for line in metadata.lines() {
            assert!(
                line.starts_with("# omarchy:summary=") || line.starts_with("# omarchy:args="),
                "{}: unexpected metadata line {line:?}",
                file.display()
            );
        }
        if metadata.is_empty() {
            continue;
        }
        with_metadata += 1;
        // Embed the generated lines directly below the shebang, as a port would carry them.
        let mut embedded = String::new();
        for (index, line) in source.lines().enumerate() {
            embedded.push_str(line);
            embedded.push('\n');
            if index == 0 {
                embedded.push_str(&metadata);
            }
        }
        let problems = omarchy_metadata_problems(&embedded, &program);
        assert!(problems.is_empty(), "{}: {problems:?}", file.display());
    }
    assert!(with_metadata > 0, "no port declares metadata inputs");
}

#[test]
fn a_stale_or_late_header_on_a_real_port_is_reported() {
    let file = ports_root().join("omarchy-notification-dismiss.program");
    let source = std::fs::read_to_string(&file).unwrap();
    let program = flow::parser::parse(&source).unwrap();

    let stale = format!(
        "#!/usr/bin/env -S execute --\n# omarchy:args=<stale>\n{}",
        source.split_once('\n').unwrap().1
    );
    let problems = omarchy_metadata_problems(&stale, &program);
    assert!(
        problems
            .iter()
            .any(|problem| problem.contains("does not match")),
        "{problems:?}"
    );

    let filler = "#\n".repeat(80);
    let late = format!("#!/usr/bin/env -S execute --\n{filler}# omarchy:args=<late>\n");
    let problems = omarchy_metadata_problems(&late, &program);
    assert!(
        problems
            .iter()
            .any(|problem| problem.contains("within the first 80 lines")),
        "{problems:?}"
    );
}
