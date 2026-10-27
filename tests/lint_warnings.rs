use flow::diagnostics::{analyze_source, code_info, DiagnosticSeverity};
use flow::manifest::Registry;

#[test]
fn shell_expansion_in_literal_arguments_warns_without_becoming_an_error() {
    let diagnostics = analyze_source(
        "run \"echo\" with arguments [\"$HOME/x\", \"${name}\\n\"]\n",
        &Registry::default(),
    );
    let warnings = diagnostics
        .iter()
        .filter(|diagnostic| diagnostic.code == "lint.shell_expansion_literal")
        .collect::<Vec<_>>();
    assert_eq!(warnings.len(), 2, "{diagnostics:#?}");
    assert!(warnings
        .iter()
        .all(|diagnostic| diagnostic.severity == DiagnosticSeverity::Warning));
}

#[test]
fn shell_free_literal_arguments_do_not_warn() {
    let diagnostics = analyze_source(
        "run \"echo\" with arguments [\"hello\", \"world\"]\n",
        &Registry::default(),
    );
    assert!(!diagnostics
        .iter()
        .any(|diagnostic| diagnostic.code == "lint.shell_expansion_literal"));
}

#[test]
fn executable_name_with_glob_syntax_warns_with_a_repair() {
    let diagnostics = analyze_source("run \"ls *.txt\"\n", &Registry::default());
    let warning = diagnostics
        .iter()
        .find(|diagnostic| diagnostic.code == "lint.executable_name_whitespace")
        .expect("lint diagnostic");
    assert_eq!(warning.severity, DiagnosticSeverity::Warning);
    assert_eq!(warning.fixes[0].edits[0].replacement, "\"ls\" with arguments [\"*.txt\"]");
}

#[test]
fn zero_repeat_loop_warns() {
    let diagnostics = analyze_source("repeat 0 times\n  run \"true\"\n", &Registry::default());
    assert!(diagnostics
        .iter()
        .any(|diagnostic| diagnostic.code == "lint.empty_loop"));
}

#[test]
fn warning_codes_are_published_with_warning_severity() {
    for code in ["lint.shell_expansion_literal", "lint.executable_name_whitespace"] {
        assert_eq!(code_info(code).unwrap().severity, DiagnosticSeverity::Warning);
    }
}
