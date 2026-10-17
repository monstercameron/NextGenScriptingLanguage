//! Property tests (#90) for the parser and the checkpoint journal decoder. Case counts come
//! from `PROPTEST_CASES`; the nightly job raises them. A failing case prints a seed that
//! proptest replays from its regression file.
use flow::program_files::parse_at;
use flow::runtime::read_repair_state;
use proptest::prelude::*;
use std::fs;
use std::path::{Path, PathBuf};
use std::process::Command;
use std::sync::OnceLock;

/// One program statement that the parser accepts, drawn from a small set of forms.
fn statement() -> impl Strategy<Value = String> {
    prop_oneof![
        "[a-z]{1,8}".prop_map(|text| format!("run printf with message \"{text}\"\n")),
        "[a-z ]{0,12}".prop_map(|text| format!("run printf with message \"{text}\"\n")),
        (0u8..=255).prop_map(|status| format!("exit with status {status}\n")),
    ]
}

proptest! {
    /// Every generated program parses, and its encoded AST decodes to the same value. The
    /// parse is deterministic, so the same text yields the same AST on a second parse.
    #[test]
    fn generated_programs_parse_and_round_trip_through_the_ast_encoding(
        lines in prop::collection::vec(statement(), 1..12)
    ) {
        let source: String = lines.concat();
        let program = parse_at(&source, None, None)
            .map_err(|error| TestCaseError::fail(format!("{error:#}")))?;
        let encoded = serde_json::to_string(&program).unwrap();
        let decoded: flow::ast::Program = serde_json::from_str(&encoded).unwrap();
        prop_assert_eq!(&decoded, &program);
        let again = parse_at(&source, None, None).unwrap();
        prop_assert_eq!(&again, &program);
    }

    /// The parser returns a typed result for any text. It never panics.
    #[test]
    fn arbitrary_text_is_either_a_program_or_a_diagnostic(source in ".{0,300}") {
        let _ = parse_at(&source, None, None);
    }
}

/// A finished checkpoint directory, written by the real binary.
fn checkpoint_directory() -> (tempfile::TempDir, PathBuf) {
    let root = tempfile::tempdir().unwrap();
    let program = root.path().join("workflow.program");
    fs::write(
        &program,
        "run printf with message \"a\"\nrun printf with message \"b\"\nrun printf with message \"c\"\n",
    )
    .unwrap();
    let checkpoint = root.path().join("checkpoint");
    let manifests = Path::new(env!("CARGO_MANIFEST_DIR")).join("manifests");
    let output = Command::new(env!("CARGO_BIN_EXE_execute"))
        .arg("run")
        .arg(&program)
        .arg("--checkpoint")
        .arg(&checkpoint)
        .arg("--manifest-dir")
        .arg(&manifests)
        .output()
        .unwrap();
    assert!(output.status.success(), "{:?}", output);
    (root, checkpoint)
}

fn copy_tree(from: &Path, to: &Path) {
    fs::create_dir_all(to).unwrap();
    for entry in fs::read_dir(from).unwrap() {
        let entry = entry.unwrap();
        let destination = to.join(entry.file_name());
        if entry.file_type().unwrap().is_dir() {
            copy_tree(&entry.path(), &destination);
        } else {
            fs::copy(entry.path(), destination).unwrap();
        }
    }
}

fn files_under(directory: &Path) -> Vec<PathBuf> {
    let mut files = Vec::new();
    for entry in fs::read_dir(directory).unwrap() {
        let path = entry.unwrap().path();
        if path.is_dir() {
            files.extend(files_under(&path));
        } else {
            files.push(path);
        }
    }
    files.sort();
    files
}

/// The reference checkpoint, written once; each case mutates a copy of it.
fn reference() -> &'static PathBuf {
    static REFERENCE: OnceLock<(tempfile::TempDir, PathBuf)> = OnceLock::new();
    &REFERENCE.get_or_init(checkpoint_directory).1
}

#[test]
fn an_untouched_checkpoint_decodes() {
    read_repair_state(reference()).expect("a finished checkpoint decodes");
}

proptest! {
    /// Any single-byte change, or a truncation, of any journal file is decoded to a typed
    /// result. It is never a panic and never a silent crash.
    #[test]
    fn mutated_journal_bytes_decode_to_a_typed_result_or_error(
        file in any::<usize>(),
        offset in any::<usize>(),
        value in any::<u8>(),
        truncate in any::<bool>(),
    ) {
        let copy = tempfile::tempdir().unwrap();
        let mutated = copy.path().join("checkpoint");
        copy_tree(reference(), &mutated);
        let files = files_under(&mutated);
        prop_assume!(!files.is_empty());
        let target = &files[file % files.len()];
        let mut bytes = fs::read(target).unwrap();
        if bytes.is_empty() {
            return Ok(());
        }
        if truncate {
            bytes.truncate(offset % bytes.len());
        } else {
            let index = offset % bytes.len();
            bytes[index] = if bytes[index] == value { value.wrapping_add(1) } else { value };
        }
        fs::write(target, bytes).unwrap();
        let _ = read_repair_state(&mutated);
    }
}
