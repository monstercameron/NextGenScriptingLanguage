use flow::diagnostics::{analyze_source, code_info, DiagnosticSeverity, SuggestedFix};
use flow::manifest::Registry;

fn apply_fix(source: &str, fix: &SuggestedFix) -> String {
    let mut edits = fix.edits.clone();
    edits.sort_by_key(|edit| std::cmp::Reverse(edit.span.start_byte));
    let mut output = source.to_owned();
    for edit in edits {
        output.replace_range(edit.span.start_byte..edit.span.end_byte, &edit.replacement);
    }
    output
}

fn format_registry() -> Registry {
    Registry::from_sources(&[include_str!("../manifests/core.text_format.json").to_owned()])
        .unwrap()
}

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
fn explicit_shell_command_script_is_not_mistaken_for_a_literal_argument() {
    for source in [
        "run \"sh\" with arguments [\"-c\", \"echo $HOME\"]\n",
        "run \"bash\" with arguments [\"-o\", \"pipefail\", \"-c\", \"echo $HOME\"]\n",
        "run \"bash\" with arguments [\"-O\", \"extglob\", \"-c\", \"echo $HOME\"]\n",
        "run \"bash\" with arguments [\"-xc\", \"echo $HOME\"]\n",
        "run \"bash\" with arguments [\"-lc\", \"echo $HOME\"]\n",
        "run \"zsh\" with arguments [\"-lc\", \"echo $HOME\"]\n",
        "run \"sh\" with arguments [\"-ac\", \"echo $HOME\"]\n",
        "run \"fish\" with arguments [\"-c\", \"echo $HOME\"]\n",
        "run \"env\" with arguments [\"bash\", \"-c\", \"echo $HOME\"]\n",
        "run \"busybox\" with arguments [\"sh\", \"-c\", \"echo $HOME\"]\n",
    ] {
        let diagnostics = analyze_source(source, &Registry::default());
        assert!(
            !diagnostics
                .iter()
                .any(|diagnostic| diagnostic.code == "lint.shell_expansion_literal"),
            "{source}: {diagnostics:#?}"
        );
    }
}

#[test]
fn shell_command_arguments_after_the_script_remain_literal() {
    let diagnostics = analyze_source(
        "run \"sh\" with arguments [\"-c\", \"echo $HOME\", \"$OTHER\"]\n",
        &Registry::default(),
    );
    let warnings = diagnostics
        .iter()
        .filter(|diagnostic| diagnostic.code == "lint.shell_expansion_literal")
        .collect::<Vec<_>>();
    assert_eq!(warnings.len(), 1, "{diagnostics:#?}");
    assert_eq!(
        &"run \"sh\" with arguments [\"-c\", \"echo $HOME\", \"$OTHER\"]\n"
            [warnings[0].span.start_byte..warnings[0].span.end_byte],
        "\"$OTHER\""
    );
}

#[test]
fn awk_and_jq_program_arguments_are_not_treated_as_shell_expansions() {
    for source in [
        "run \"awk\" with arguments [\"{ print $i, $(i + 1) }\"]\n",
        "run \"jq\" with arguments [\"-r\", \". as $draft | select(.name == $workflow)\"]\n",
    ] {
        let diagnostics = analyze_source(source, &Registry::default());
        assert!(
            !diagnostics
                .iter()
                .any(|diagnostic| diagnostic.code == "lint.shell_expansion_literal"),
            "{source}: {diagnostics:#?}"
        );
    }
}

#[test]
fn awk_and_jq_data_arguments_remain_subject_to_expansion_lint() {
    for source in [
        "run \"awk\" with arguments [\"-v\", \"name\", \"$HOME\", \"{ print $name }\"]\n",
        "run \"jq\" with arguments [\"--arg\", \"name\", \"$HOME\", \".name == $name\"]\n",
    ] {
        let diagnostics = analyze_source(source, &Registry::default());
        let warnings = diagnostics
            .iter()
            .filter(|diagnostic| diagnostic.code == "lint.shell_expansion_literal")
            .collect::<Vec<_>>();
        assert_eq!(warnings.len(), 1, "{source}: {diagnostics:#?}");
    }
}

#[test]
fn jq_long_flags_and_repeated_awk_sources_skip_only_script_text() {
    for source in [
        "run \"jq\" with arguments [\"--stream\", \"--seq\", \"--tab\", \"--unbuffered\", \"--raw-output0\", \"--arg\", \"draft\", \"draft\", \". as $draft | select(.name == $workflow)\"]\n",
        "run \"awk\" with arguments [\"-e\", \"{ print $i }\", \"--source\", \"{ print $(i + 1) }\"]\n",
    ] {
        let diagnostics = analyze_source(source, &Registry::default());
        assert!(
            !diagnostics
                .iter()
                .any(|diagnostic| diagnostic.code == "lint.shell_expansion_literal"),
            "{source}: {diagnostics:#?}"
        );
    }
}

#[test]
fn shell_looking_later_arguments_are_not_treated_as_a_command_string() {
    for source in [
        "run \"sh\" with arguments [\"script.sh\", \"-c\", \"echo $HOME\"]\n",
        "run \"env\" with arguments [\"echo\", \"bash\", \"-c\", \"echo $HOME\"]\n",
        "run \"busybox\" with arguments [\"echo\", \"sh\", \"-c\", \"echo $HOME\"]\n",
        "run \"env\" with arguments [\"-S\", \"echo bash -c\", \"bash\", \"-c\", \"echo $HOME\"]\n",
    ] {
        let diagnostics = analyze_source(source, &Registry::default());
        assert!(
            diagnostics
                .iter()
                .any(|diagnostic| diagnostic.code == "lint.shell_expansion_literal"),
            "{source}: {diagnostics:#?}"
        );
    }
}

#[test]
fn safe_first_command_expansion_fix_declares_environment_and_formats_value() {
    let source = "run \"echo\" with arguments [\"$HOME/x\"]\n";
    let registry = format_registry();
    let diagnostics = analyze_source(source, &registry);
    let warning = diagnostics
        .iter()
        .find(|diagnostic| diagnostic.code == "lint.shell_expansion_literal")
        .expect("shell expansion warning");
    let fix = warning.fixes.first().expect("safe fix for first run step");
    let repaired = apply_fix(source, fix);
    assert!(repaired.contains("from environment \"HOME\" with default \"\""));
    assert!(repaired.contains("text.format with template \"{{v0}}/x\""));
    assert!(!analyze_source(&repaired, &registry)
        .iter()
        .any(|diagnostic| diagnostic.code == "lint.shell_expansion_literal"));
}

#[test]
fn braced_expansion_fix_reuses_a_declared_input() {
    let source = "accept name as text\nrun \"printf\" with arguments [\"hello ${name}\\n\"]\n";
    let registry = format_registry();
    let diagnostics = analyze_source(source, &registry);
    let warning = diagnostics
        .iter()
        .find(|diagnostic| diagnostic.code == "lint.shell_expansion_literal")
        .expect("braced shell-like expansion warning");
    let fix = warning
        .fixes
        .first()
        .expect("declared input can be formatted safely");
    let repaired = apply_fix(source, fix);
    assert!(repaired.contains("accept name as text\n"));
    assert!(!repaired.contains("from environment \"name\""));
    assert!(repaired.contains("values {\"v0\": name}"));
    assert!(repaired.contains("template \"hello {{v0}}\\n\""));
    assert!(!analyze_source(&repaired, &registry)
        .iter()
        .any(|diagnostic| diagnostic.code == "lint.shell_expansion_literal"));
}

#[test]
fn braced_expansion_fix_is_withheld_for_non_scalar_declared_inputs() {
    for source in [
        "accept name as json\nrun \"printf\" with arguments [\"${name}\"]\n",
        "accept name as list of text\nrun \"printf\" with arguments [\"${name}\"]\n",
    ] {
        let diagnostics = analyze_source(source, &format_registry());
        let warning = diagnostics
            .iter()
            .find(|diagnostic| diagnostic.code == "lint.shell_expansion_literal")
            .expect("literal expansion warning remains");
        assert!(warning.fixes.is_empty(), "{source}: {warning:#?}");
    }
}

#[test]
fn shell_expansion_fix_is_withheld_when_insertion_would_change_pipeline_input() {
    let source = "read \"input.txt\"\nthen \"echo\" with arguments [\"$HOME/x\"]\n";
    let diagnostics = analyze_source(source, &format_registry());
    let warning = diagnostics
        .iter()
        .find(|diagnostic| diagnostic.code == "lint.shell_expansion_literal")
        .expect("shell expansion warning");
    assert!(warning.fixes.is_empty());
}

#[test]
fn braced_environment_expansion_gets_an_explicit_formatting_fix() {
    let source = "run \"echo\" with arguments [\"${name}\\n\"]\n";
    let registry = format_registry();
    let diagnostics = analyze_source(source, &registry);
    let fix = diagnostics
        .iter()
        .find(|diagnostic| diagnostic.code == "lint.shell_expansion_literal")
        .and_then(|diagnostic| diagnostic.fixes.first())
        .expect("environment expansion fix");
    let repaired = apply_fix(source, fix);
    assert!(repaired.contains("from environment \"name\""));
    assert!(repaired.contains("template \"{{v0}}\\n\""));
    assert!(!analyze_source(&repaired, &registry)
        .iter()
        .any(|diagnostic| diagnostic.code == "lint.shell_expansion_literal"));
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
fn backslash_escaped_shell_marker_does_not_warn() {
    let diagnostics = analyze_source(
        "run \"echo\" with arguments [\"\\\\$HOME\"]\n",
        &Registry::default(),
    );
    assert!(!diagnostics
        .iter()
        .any(|diagnostic| diagnostic.code == "lint.shell_expansion_literal"));
}

#[test]
fn shell_expansion_in_path_is_also_reported() {
    let diagnostics = analyze_source(
        "run fs.read with path \"$HOME/input.txt\"\n",
        &Registry::default(),
    );
    assert!(diagnostics
        .iter()
        .any(|diagnostic| diagnostic.code == "lint.shell_expansion_literal"));
}

#[test]
fn safe_first_path_expansion_gets_explicit_environment_fix() {
    let source = "run fs.read with path \"$HOME/input.txt\"\n";
    let registry = format_registry();
    let diagnostics = analyze_source(source, &registry);
    let warning = diagnostics
        .iter()
        .find(|diagnostic| diagnostic.code == "lint.shell_expansion_literal")
        .expect("path expansion warning");
    let fix = warning.fixes.first().expect("safe first-step path fix");
    let repaired = apply_fix(source, fix);
    assert!(repaired.contains("from environment \"HOME\""));
    assert!(repaired.contains("text.format with template \"{{v0}}/input.txt\""));
    assert!(!analyze_source(&repaired, &registry)
        .iter()
        .any(|diagnostic| diagnostic.code == "lint.shell_expansion_literal"));
}

#[test]
fn shell_expansion_fixes_require_the_format_applet_in_the_active_registry() {
    let arguments_source = "run \"echo\" with arguments [\"$HOME/x\"]\n";
    let empty_registry = Registry::from_sources(&[] as &[&str]).unwrap();
    let arguments_diagnostics = analyze_source(arguments_source, &empty_registry);
    let arguments_warning = arguments_diagnostics
        .iter()
        .find(|diagnostic| diagnostic.code == "lint.shell_expansion_literal")
        .expect("literal argument warning remains available");
    assert!(arguments_warning.fixes.is_empty());

    let command_format = include_str!("../manifests/core.text_format.json")
        .replace("\"backend\": \"in_process\"", "\"backend\": \"command\"");
    let command_registry = Registry::from_sources(&[command_format]).unwrap();
    let command_diagnostics = analyze_source(arguments_source, &command_registry);
    let command_warning = command_diagnostics
        .iter()
        .find(|diagnostic| diagnostic.code == "lint.shell_expansion_literal")
        .expect("literal argument warning remains with a command backend");
    assert!(command_warning.fixes.is_empty());

    let path_source = "run fs.read with path \"$HOME/input.txt\"\n";
    let path_registry =
        Registry::from_sources(&[include_str!("../manifests/core.fs_read.json")]).unwrap();
    let path_diagnostics = analyze_source(path_source, &path_registry);
    let path_warning = path_diagnostics
        .iter()
        .find(|diagnostic| diagnostic.code == "lint.shell_expansion_literal")
        .expect("literal path warning remains available");
    assert!(path_warning.fixes.is_empty());
}

#[test]
fn path_expansion_fix_is_withheld_after_a_prior_pipeline_step() {
    let source = "read \"input.txt\"\nthen fs.read with path \"$HOME/input.txt\"\n";
    let diagnostics = analyze_source(source, &format_registry());
    let warning = diagnostics
        .iter()
        .find(|diagnostic| diagnostic.code == "lint.shell_expansion_literal")
        .expect("path expansion warning");
    assert!(warning.fixes.is_empty());
}

#[test]
fn matching_per_line_suppression_hides_only_the_selected_warning() {
    let source =
        "run \"echo\" with arguments [\"$HOME\"] # execute-allow: lint.shell_expansion_literal\n";
    let diagnostics = analyze_source(source, &Registry::default());
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
    assert_eq!(
        warning.fixes[0].edits[0].replacement,
        "\"ls\" with arguments [\"*.txt\"]"
    );
}

#[test]
fn zero_repeat_loop_warns() {
    let diagnostics = analyze_source("repeat 0 times\n  run \"true\"\n", &Registry::default());
    assert!(diagnostics
        .iter()
        .any(|diagnostic| diagnostic.code == "lint.empty_loop"));
}

#[test]
fn unused_bindings_and_inputs_warn_but_references_are_counted() {
    let source = "accept unused_input as text\naccept used_input as text\nrun \"printf\" with arguments [used_input] as unused_result\n";
    let diagnostics = analyze_source(source, &Registry::default());
    assert!(diagnostics
        .iter()
        .any(|diagnostic| diagnostic.code == "lint.unused_input"
            && diagnostic.message.contains("input \"unused_input\"")));
    assert!(!diagnostics
        .iter()
        .any(|diagnostic| diagnostic.code == "lint.unused_input"
            && diagnostic.message.contains("input \"used_input\"")));
    assert!(diagnostics
        .iter()
        .any(|diagnostic| diagnostic.code == "lint.unused_binding"
            && diagnostic.message.contains("unused_result")));
}

#[test]
fn unused_name_quick_fixes_only_add_behavior_neutral_suppression_comments() {
    let source =
        "accept unused_input as text\nrun \"printf\" with arguments [\"x\"] as unused_result\n";
    let diagnostics = analyze_source(source, &Registry::default());
    for (code, line) in [
        ("lint.unused_input", "accept unused_input as text # execute-allow: lint.unused_input\n"),
        (
            "lint.unused_binding",
            "run \"printf\" with arguments [\"x\"] as unused_result # execute-allow: lint.unused_binding\n",
        ),
    ] {
        let warning = diagnostics
            .iter()
            .find(|diagnostic| diagnostic.code == code)
            .expect("unused-name warning");
        let fix = warning.fixes.first().expect("safe suppression fix");
        let repaired = apply_fix(source, fix);
        assert!(repaired.contains(line), "{repaired:?}");
        assert!(!analyze_source(&repaired, &Registry::default())
            .iter()
            .any(|diagnostic| diagnostic.code == code));
    }
}

#[test]
fn unused_intermediate_pipeline_binding_can_be_removed_without_changing_the_pipeline() {
    let source = "run \"printf\" with arguments [\"x\"] as intermediate\nthen \"cat\"\n";
    let diagnostics = analyze_source(source, &Registry::default());
    let warning = diagnostics
        .iter()
        .find(|diagnostic| {
            diagnostic.code == "lint.unused_binding" && diagnostic.message.contains("intermediate")
        })
        .expect("unused pipeline binding warning");
    let fix = warning
        .fixes
        .first()
        .expect("safe removal fix for an intermediate pipeline binding");
    assert!(fix.message.contains("literal printf pipeline"));
    let repaired = apply_fix(source, fix);
    assert_eq!(
        repaired,
        "run \"printf\" with arguments [\"x\"]\nthen \"cat\"\n"
    );
    assert!(!analyze_source(&repaired, &Registry::default())
        .iter()
        .any(|diagnostic| diagnostic.code == "lint.unused_binding"));
}

#[test]
fn unused_sensitive_pipeline_binding_is_not_removed_by_quick_fix() {
    let source = "run \"printf\" with arguments [\"x\"] as secret and keep secret\nthen \"cat\"\n";
    let diagnostics = analyze_source(source, &Registry::default());
    let warning = diagnostics
        .iter()
        .find(|diagnostic| {
            diagnostic.code == "lint.unused_binding" && diagnostic.message.contains("secret")
        })
        .expect("unused sensitive binding warning");
    assert!(
        warning
            .fixes
            .iter()
            .all(|fix| !fix.message.contains("intermediate pipeline")),
        "{warning:#?}"
    );
}

#[test]
fn listener_applet_inputs_are_counted_as_reads() {
    let source = "accept path as text\nlisten to \"tail\" with arguments [\"-F\", path]\nwhenever a line arrives as event\n  then \"cat\"\n";
    let diagnostics = analyze_source(source, &Registry::default());
    assert!(
        !diagnostics
            .iter()
            .any(|diagnostic| diagnostic.code == "lint.unused_input"),
        "{diagnostics:#?}"
    );
}

#[test]
fn task_output_bindings_are_used_by_a_consumed_group_result() {
    let source = "parallel at most 2 as results\n  run number.add with left 20 and right 22 as sum\n  run number.multiply with left 6 and right 7 as product\nthen encode results as json\n";
    let diagnostics = analyze_source(source, &Registry::default());
    assert!(
        !diagnostics
            .iter()
            .any(|diagnostic| diagnostic.code == "lint.unused_binding"),
        "{diagnostics:#?}"
    );
}

#[test]
fn unused_pipeline_binding_fix_is_withheld_for_reference_derived_output() {
    let source = "accept token as text from environment \"TOKEN\"\nrun \"printf\" with arguments [token] as intermediate\nthen \"cat\"\n";
    let diagnostics = analyze_source(source, &Registry::default());
    let warning = diagnostics
        .iter()
        .find(|diagnostic| {
            diagnostic.code == "lint.unused_binding" && diagnostic.message.contains("intermediate")
        })
        .expect("unused binding warning");
    assert!(
        warning
            .fixes
            .iter()
            .all(|fix| !fix.message.contains("intermediate pipeline")),
        "{warning:#?}"
    );
}

#[test]
fn unused_pipeline_binding_fix_is_withheld_after_secret_pipeline_input() {
    let source = "accept token as text from secret prompt \"Token?\"\nuse value token\nthen \"cat\" as intermediate\nthen \"cat\"\n";
    let diagnostics = analyze_source(source, &Registry::default());
    let warning = diagnostics
        .iter()
        .find(|diagnostic| {
            diagnostic.code == "lint.unused_binding" && diagnostic.message.contains("intermediate")
        })
        .expect("unused binding warning");
    assert!(
        warning
            .fixes
            .iter()
            .all(|fix| !fix.message.contains("literal printf pipeline")),
        "{warning:#?}"
    );
}

#[test]
fn use_of_result_suppresses_unused_binding_warning() {
    let source = "run \"printf\" with arguments [\"x\"] as result\nthen \"printf\" with arguments [result]\n";
    let diagnostics = analyze_source(source, &Registry::default());
    assert!(!diagnostics
        .iter()
        .any(|diagnostic| diagnostic.code == "lint.unused_binding"
            && diagnostic.message.contains("result")));
}

#[test]
fn references_in_a_different_reusable_program_do_not_count_as_uses() {
    let source = "define a program named first\n  run \"printf\" with arguments [\"x\"] as local_value\ndefine a program named second\n  then \"printf\" with arguments [local_value]\n";
    let diagnostics = analyze_source(source, &Registry::default());
    assert!(diagnostics
        .iter()
        .any(|diagnostic| diagnostic.code == "lint.unused_binding"
            && diagnostic.message.contains("local_value")));
}

#[test]
fn unused_binding_inside_a_nested_body_is_checked_in_that_body() {
    let source = "accept ready as boolean\nif ready is true\n  run \"printf\" with arguments [\"x\"] as nested_result\n  run \"true\"\n";
    let diagnostics = analyze_source(source, &Registry::default());
    assert!(diagnostics
        .iter()
        .any(|diagnostic| diagnostic.code == "lint.unused_binding"
            && diagnostic.message.contains("nested_result")));
}

#[test]
fn binding_used_after_a_conditional_is_not_reported_unused() {
    let source = "accept ready as boolean\nif ready is true\n  run \"printf\" with arguments [\"x\"] as conditional_result\nthen \"printf\" with arguments [conditional_result]\n";
    let diagnostics = analyze_source(source, &Registry::default());
    assert!(!diagnostics
        .iter()
        .any(|diagnostic| diagnostic.code == "lint.unused_binding"
            && diagnostic.message.contains("conditional_result")));
}

#[test]
fn model_result_use_without_acceptance_and_missing_threshold_warn() {
    let source = "run jev.classify with question \"classify\" and labels [\"yes\", \"no\"] as classification\ntake text at \"/label\" from classification as label\nrun \"printf\" with arguments [label]\n";
    let diagnostics = analyze_source(source, &Registry::default());
    assert!(diagnostics
        .iter()
        .any(|diagnostic| diagnostic.code == "model.result_unchecked"));
    assert!(diagnostics
        .iter()
        .any(|diagnostic| diagnostic.code == "model.threshold_missing"));
}

#[test]
fn model_acceptance_guard_and_threshold_suppress_lints() {
    let source = "run jev.classify with question \"classify\" and labels [\"yes\", \"no\"] and minimum 80 as classification\ntake boolean at \"/accepted\" from classification as accepted\nif accepted is true\n  run \"printf\" with arguments [\"approved\"]\n";
    let diagnostics = analyze_source(source, &Registry::default());
    assert!(!diagnostics.iter().any(|diagnostic| matches!(
        diagnostic.code.as_str(),
        "model.result_unchecked" | "model.threshold_missing"
    )));
}

#[test]
fn model_acceptance_guard_after_json_parse_suppresses_result_lint() {
    let source = "run jev.classify with question \"classify\" and labels [\"yes\", \"no\"] and minimum 80 as raw\nthen parse json as classification\ntake boolean at \"/accepted\" from classification as accepted\ntake text at \"/label\" from classification as label\nif accepted is true\n  run \"printf\" with arguments [label]\n";
    let diagnostics = analyze_source(source, &Registry::default());
    assert!(!diagnostics
        .iter()
        .any(|diagnostic| diagnostic.code == "model.result_unchecked"));
}

#[test]
fn unrelated_acceptance_guard_does_not_suppress_unchecked_result_lint() {
    let source = "run jev.classify with question \"classify\" and labels [\"yes\", \"no\"] and minimum 80 as raw\nthen parse json as classification\ntake boolean at \"/accepted\" from classification as accepted\ntake text at \"/label\" from classification as label\nif accepted is true\n  run \"printf\" with arguments [\"accepted\"]\nrun \"printf\" with arguments [label]\n";
    let diagnostics = analyze_source(source, &Registry::default());
    assert!(diagnostics.iter().any(|diagnostic| {
        diagnostic.code == "model.result_unchecked" && diagnostic.message.contains("raw")
    }));
}

#[test]
fn rejected_acceptance_branch_does_not_guard_model_result_use() {
    let source = "run jev.classify with question \"classify\" and labels [\"yes\", \"no\"] and minimum 80 as classification\ntake boolean at \"/accepted\" from classification as accepted\ntake text at \"/label\" from classification as label\nif accepted is false\n  run \"printf\" with arguments [label]\n";
    let diagnostics = analyze_source(source, &Registry::default());
    assert!(diagnostics
        .iter()
        .any(|diagnostic| diagnostic.code == "model.result_unchecked"));
}

#[test]
fn unread_keep_status_does_not_warn_as_an_unused_binding() {
    let diagnostics = analyze_source(
        "run \"false\" and keep status as exit_code\n",
        &Registry::default(),
    );
    assert!(!diagnostics.iter().any(|diagnostic| {
        diagnostic.code == "lint.unused_binding" && diagnostic.message.contains("exit_code")
    }));
}

#[test]
fn acceptance_from_another_model_call_does_not_count_as_a_gate() {
    let source = "run jev.classify with question \"one\" and labels [\"yes\"] and minimum 80 as first\nrun jev.classify with question \"two\" and labels [\"yes\"] and minimum 80 as second\ntake text at \"/label\" from first as label\ntake boolean at \"/accepted\" from second as accepted\nif accepted is true\n  run \"printf\" with arguments [label]\n";
    let diagnostics = analyze_source(source, &Registry::default());
    assert!(diagnostics
        .iter()
        .any(|diagnostic| diagnostic.code == "model.result_unchecked"
            && diagnostic.message.contains("first")));
}

#[test]
fn earlier_condition_with_reused_name_does_not_gate_later_model_result() {
    let source = "accept accepted as boolean\nif accepted is true\n  run \"true\"\nrun jev.classify with question \"classify\" and labels [\"yes\"] and minimum 80 as raw\ntake boolean at \"/accepted\" from raw as accepted\ntake text at \"/label\" from raw as label\nrun \"printf\" with arguments [label]\n";
    let diagnostics = analyze_source(source, &Registry::default());
    assert!(diagnostics
        .iter()
        .any(|diagnostic| diagnostic.code == "model.result_unchecked"));
}

#[test]
fn forwarding_model_result_to_unverified_helper_keeps_the_warning() {
    let source = "define a program named review\n  accept label as text\n  if label is \"yes\"\n    run \"printf\" with arguments [label]\nrun jev.classify with question \"classify\" and labels [\"yes\", \"no\"] and minimum 80 as classification\ntake text at \"/label\" from classification as label\nrun program review with label label\n";
    let diagnostics = analyze_source(source, &Registry::default());
    assert!(diagnostics
        .iter()
        .any(|diagnostic| diagnostic.code == "model.result_unchecked"));
}

#[test]
fn model_text_passed_to_shell_c_is_flagged_but_data_arguments_are_allowed() {
    for bad in [
        "run llm.ask with prompt \"write something\" as generated\nrun \"sh\" with arguments [\"-c\", generated]\n",
        "run llm.ask with prompt \"write something\" as generated\nrun \"bash\" with arguments [\"-xc\", generated]\n",
        "run llm.ask with prompt \"write something\" as generated\nrun \"bash\" with arguments [\"-lc\", generated]\n",
        "run llm.ask with prompt \"write something\" as generated\nrun \"zsh\" with arguments [\"-lc\", generated]\n",
        "run llm.ask with prompt \"write something\" as generated\nrun \"sh\" with arguments [\"-ac\", generated]\n",
    ] {
        let bad_diagnostics = analyze_source(bad, &Registry::default());
        assert!(
            bad_diagnostics
                .iter()
                .any(|diagnostic| diagnostic.code == "model.text_to_command"),
            "{bad}: {bad_diagnostics:#?}"
        );
    }

    let safe = "run llm.ask with prompt \"write something\" as generated\nrun \"printf\" with arguments [\"%s\", generated]\n";
    let safe_diagnostics = analyze_source(safe, &Registry::default());
    assert!(!safe_diagnostics
        .iter()
        .any(|diagnostic| diagnostic.code == "model.text_to_command"));
}

#[test]
fn model_text_from_extract_and_env_shell_invocation_are_flagged() {
    let source = "run llm.extract with model model_name and instruction \"draft\" and schema schema as raw\nthen parse json as response\ntake text at \"/value\" from response as generated\nrun \"env\" with arguments [\"bash\", \"-c\", generated]\n";
    let diagnostics = analyze_source(source, &Registry::default());
    assert!(diagnostics
        .iter()
        .any(|diagnostic| diagnostic.code == "model.text_to_command"));
}

#[test]
fn shell_looking_later_env_arguments_do_not_mark_model_text_as_shell_code() {
    let source = "run llm.ask with prompt \"write something\" as generated\nrun \"env\" with arguments [\"echo\", \"bash\", \"-c\", generated]\n";
    let diagnostics = analyze_source(source, &Registry::default());
    assert!(!diagnostics
        .iter()
        .any(|diagnostic| diagnostic.code == "model.text_to_command"));
}

#[test]
fn count_option_is_not_mistaken_for_a_shell_command_flag() {
    let source = "run llm.ask with prompt \"write something\" as generated\nrun \"grep\" with arguments [\"-c\", generated]\n";
    let diagnostics = analyze_source(source, &Registry::default());
    assert!(!diagnostics
        .iter()
        .any(|diagnostic| diagnostic.code == "model.text_to_command"));
}

#[test]
fn unreachable_lint_respects_if_branch_boundaries() {
    let source = "accept ready as boolean\nif ready is true\n  exit with status 0\n  run \"true\"\notherwise\n  run \"true\"\nrun \"true\"\n";
    let diagnostics = analyze_source(source, &Registry::default());
    let unreachable = diagnostics
        .iter()
        .filter(|diagnostic| diagnostic.code == "lint.unreachable")
        .collect::<Vec<_>>();
    assert_eq!(unreachable.len(), 1, "{diagnostics:#?}");
    assert_eq!(unreachable[0].span.line, 4);
}

#[test]
fn warning_codes_are_published_with_warning_severity() {
    for code in [
        "lint.shell_expansion_literal",
        "lint.executable_name_whitespace",
        "lint.empty_loop",
        "lint.unused_binding",
        "lint.unused_input",
        "lint.unreachable",
        "model.result_unchecked",
        "model.threshold_missing",
        "model.text_to_command",
    ] {
        assert_eq!(
            code_info(code).unwrap().severity,
            DiagnosticSeverity::Warning
        );
    }
}
