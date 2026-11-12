//! Keeps the in-repo editor and GitHub language metadata for `.program` files in place:
//! the Linguist detectability override, the existing LF rule, and the VS Code language
//! configuration for comments, brackets and the triple-quote raw block.

const GITATTRIBUTES: &str = include_str!("../.gitattributes");
const LANGUAGE_CONFIGURATION: &str = include_str!("../editors/vscode/language-configuration.json");

fn gitattributes_lines() -> Vec<&'static str> {
    GITATTRIBUTES.lines().map(str::trim_end).collect()
}

#[test]
fn program_files_keep_lf_and_skip_linguist_guessing() {
    let lines = gitattributes_lines();
    assert!(
        lines.contains(&"*.program text eol=lf"),
        "the LF rule must stay"
    );
    assert!(
        lines.contains(&"*.program linguist-detectable=false"),
        "*.program must be excluded from the Linguist classifier"
    );
    assert!(
        lines.contains(&"* text=auto"),
        "the global text rule must stay in place"
    );
}

#[test]
fn language_configuration_covers_comments_brackets_and_raw_blocks() {
    let config: serde_json::Value =
        serde_json::from_str(LANGUAGE_CONFIGURATION).expect("language configuration is valid JSON");

    assert_eq!(config["comments"]["lineComment"], "#");

    let brackets = config["brackets"].as_array().expect("brackets is an array");
    for (open, close) in [("[", "]"), ("{", "}")] {
        assert!(
            brackets
                .iter()
                .any(|pair| pair[0] == open && pair[1] == close),
            "bracket pair {open}{close} is missing"
        );
    }

    let markers = &config["folding"]["markers"];
    assert_eq!(
        config["folding"]["offSide"], true,
        "offside folding must stay"
    );
    let start = markers["start"].as_str().expect("raw block start marker");
    let end = markers["end"].as_str().expect("raw block end marker");

    // The opener is a triple quote at the end of a line, preceded by a space and content,
    // and the closer is a line holding only the triple quote.
    assert_eq!(start, r#"\S.*[ \t]"""\s*$"#);
    assert_eq!(end, r#"^\s*"""\s*$"#);
}
