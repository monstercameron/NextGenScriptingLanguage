use std::fs;

fn doc(path: &str) -> String {
    let root = std::env::var_os("LANE_D1_DOC_ROOT")
        .map(std::path::PathBuf::from)
        .unwrap_or_else(|| std::path::PathBuf::from(env!("CARGO_MANIFEST_DIR")));
    fs::read_to_string(root.join(path)).unwrap_or_else(|error| panic!("reading {path}: {error}"))
}

#[test]
fn grammar_and_language_reference_cover_current_forms() {
    let grammar = doc("docs/grammar.md");
    for form in [
        "otherwise if",
        "then capture bytes as NAME",
        "triple-quoted raw block",
        "use bytes NAME",
        "and without environment",
        "and retry",
        "with temporary file",
        "on interrupt",
        "launch detached",
        "millisecond",
        "stderr",
    ] {
        assert!(grammar.contains(form), "grammar is missing {form:?}");
    }

    let language = doc("docs/language.md");
    assert!(language.contains("One logical statement per line"));
    assert!(!language.contains("One statement per line"));
    assert!(language.contains("and environment {\"NAME\": \"value\"}"));
    assert!(!language.contains("`with environment` on one command"));
    assert!(language.contains("then capture bytes as name"));
    assert!(language.contains("use bytes name"));
    assert!(language.contains("set -o pipefail"));
    assert!(language.contains("status 70"));
    assert!(language.contains("reader closed"));
    assert!(language.contains("SIGPIPE"));
    assert!(language.contains("preserves the existing file mode"));
    assert!(language.contains("16,384 VM-step budget"));
    let runtime_reference = doc("docs/runtime-reference.md");
    assert!(runtime_reference.contains("divides the available output allowance"));
    let guide = doc("docs/guide.md");
    assert!(guide.contains("One logical statement per line"));
    assert!(!guide.contains("One statement per line"));
}

#[test]
fn security_and_applet_docs_record_current_parser_and_file_contracts() {
    let security = doc("SECURITY.md");
    for detail in [
        "host_environment",
        "allow_wrappers",
        "python3.12",
        "Replay refuses detached launches and handovers",
    ] {
        assert!(security.contains(detail), "SECURITY.md is missing {detail}");
    }

    let threat = doc("docs/threat-model.md");
    assert!(threat.contains("one total 2 second deadline"));
    assert!(threat.contains("Review, record and replay"));

    let applets = doc("docs/applet-contracts.md");
    assert!(applets.contains("Duplicate JSON object keys are\nrefused everywhere"));
    assert!(applets.contains("nonnumeric index is"));
    assert!(applets.contains("fs.ensure_line"));
    assert!(applets.contains("`path.dirname` returns `.`"));
    assert!(applets.contains("`path.basename` returns an empty string"));

    let csv = doc("docs/csv.md");
    assert!(csv.contains("One unquoted, physically blank trailing record"));
    assert!(csv.contains("after another record is ignored, for LF and CRLF alike"));
    assert!(csv.contains("A quoted empty record"));
    assert!(csv.contains("TAB is ordinary cell content"));
    assert!(csv.contains("written as `\"\"`"));
}

#[test]
fn cosign_verification_uses_real_shell_line_continuations() {
    let release = doc("docs/releases.md");
    assert!(release.contains(r#"cosign verify-blob --bundle SHA256SUMS.sigstore.json \"#));
    assert!(!release.contains("\\n  --certificate-identity"));
}

#[test]
fn benchmark_tables_report_the_json_fixture_size() {
    let readme = doc("README.md");
    let readiness = doc("docs/production-readiness.md");
    let index = doc("docs/index.html");
    for content in [&readme, &readiness, &index] {
        assert!(content.contains("about 240 KB"));
        assert!(!content.contains("400 KB"));
    }
}

#[test]
fn reproducibility_docs_scope_claims_and_version_files() {
    let release = doc("docs/releases.md");
    assert!(release.contains("`SOURCE_DATE_EPOCH`"));
    assert!(release.contains("fuzz/Cargo.lock"));
    assert!(release.contains("x86_64 Linux musl binaries"));
}

#[test]
fn man_page_lists_cli_options_subcommands_and_graph_checkpoint_shape() {
    let man = doc("docs/man/execute.1");
    let normalized_man = man.replace("\\-", "-");
    for option in [
        "--answers",
        "--record",
        "--what-if",
        "--confirm",
        "--allow",
        "--policy-header",
        "--format",
        "--single-instance",
        "--control",
        "--break-at",
        "--daemon",
        "--json-errors",
        "--version",
        "--headless",
        "--once",
        "--init",
        "--max-runs",
        "--missed-ticks",
        "--max-catch-up",
        "--calendar-dedup",
        "--max-record-bytes",
        "--total-timeout-ms",
    ] {
        assert!(
            normalized_man.contains(option),
            "man page is missing {option}"
        );
    }
    for command in [
        "test",
        "run-parts",
        "show-run",
        "replay",
        "repl",
        "repair",
        "compile",
        "checkpoint",
        "attach",
        "dap",
        "daemon",
    ] {
        assert!(man.contains(command), "man page is missing {command}");
    }
    assert!(normalized_man.contains("--option=value"));

    let schema = doc("docs/schemas/checkpoint-status.v1.schema.json");
    assert!(schema.contains("Finite graph checkpoint status"));
    assert!(schema.contains("\"kind\": { \"const\": \"graph\" }"));
    assert!(schema.contains("\"completed_nodes\": { \"type\": \"integer\""));
    assert!(schema.contains("\"in_flight\": {\n              \"type\": \"array\""));
}
