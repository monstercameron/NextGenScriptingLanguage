//! Issue #463, round-trip criterion: for every example program, the control tree
//! rebuilt from the structure plan (each row's `id`, `parent` and `depth`) matches
//! the nesting of the parser's steps. The expected placement is computed from
//! `program.steps` alone, so the explain walk and this check are separate code.
//! Rows are matched by step id, not by line: the parser gives implicit `EndIf` and
//! `EndLoop` steps the line of the statement that follows, so lines repeat.

use flow::agent::explain;
use flow::ast::{step_ids, Step, StepKind};
use flow::manifest::Registry;
use flow::parser;
use serde_json::{json, Value};
use std::collections::{BTreeMap, BTreeSet};
use std::path::{Path, PathBuf};

/// One step (from the parser) or one plan row, reduced to what the tree needs.
#[derive(Debug, Clone, PartialEq)]
struct Node {
    id: String,
    line: u64,
    depth: u64,
    parent: Option<String>,
}

/// The placement the explain walk should report for each top-level step, in step
/// order. A called program's body is inlined after its `CallProgram`; the walk
/// reports that body through the call, so the inlined copy is skipped here.
fn expected_nodes(steps: &[Step]) -> Vec<Node> {
    let ids = step_ids(steps);
    let make = |index: usize, open: &[usize]| Node {
        id: ids[index].clone(),
        line: steps[index].line as u64,
        depth: open.len() as u64,
        parent: open.last().map(|block| ids[*block].clone()),
    };
    let mut out = Vec::new();
    let mut open: Vec<usize> = Vec::new();
    let mut index = 0;
    while index < steps.len() {
        let step = &steps[index];
        if let StepKind::CallProgram { end, .. } = &step.kind {
            if let Some(target) = index
                .checked_add(*end)
                .filter(|target| *target > index && *target <= steps.len())
            {
                out.push(make(index, open.as_slice()));
                index = target;
                continue;
            }
        }
        match &step.kind {
            // Clause markers sit at the level of the block they belong to.
            StepKind::Otherwise { .. } | StepKind::Catch { .. } | StepKind::Finally => {
                let block = open.pop();
                out.push(make(index, open.as_slice()));
                open.extend(block);
            }
            // Closers take the level outside the block they end.
            StepKind::EndIf
            | StepKind::EndLoop { .. }
            | StepKind::EndTry
            | StepKind::EndTempScope { .. } => {
                open.pop();
                out.push(make(index, open.as_slice()));
            }
            StepKind::If { .. }
            | StepKind::IfCommand { .. }
            | StepKind::Loop { .. }
            | StepKind::Try { .. }
            | StepKind::TempScope { .. } => {
                out.push(make(index, open.as_slice()));
                open.push(index);
            }
            _ => out.push(make(index, open.as_slice())),
        }
        index += 1;
    }
    out
}

/// The plan's rows, one node per step id (rows of one step share its id), read from
/// `id`, `parent`, `depth` and `line`. Rows of called bodies are included too.
fn plan_nodes(plan: &Value) -> Result<Vec<Node>, String> {
    let rows = plan["steps"].as_array().ok_or("no steps array")?;
    let mut out: Vec<Node> = Vec::new();
    for row in rows {
        let Some(id) = row["id"].as_str().map(str::to_string) else {
            // The listen trigger row is walked before the steps, outside any block, and is
            // not a step, so the parser has no node for it. Any other id-less row is an error.
            if row["kind"] == json!("listen_source") {
                continue;
            }
            return Err("row without id".to_string());
        };
        if out.iter().any(|node| node.id == id) {
            continue;
        }
        out.push(Node {
            line: row["line"].as_u64().ok_or("row without line")?,
            depth: row["depth"].as_u64().ok_or("row without depth")?,
            parent: row
                .get("parent")
                .map(|parent| {
                    parent
                        .as_str()
                        .map(str::to_string)
                        .ok_or("parent is not an id")
                })
                .transpose()?,
            id,
        });
    }
    Ok(out)
}

/// Nested tree rebuilt from parent links alone.
fn rebuild(nodes: &[Node], parent: Option<&str>) -> Value {
    Value::Array(
        nodes
            .iter()
            .filter(|node| node.parent.as_deref() == parent)
            .map(|node| json!({"line": node.line, "children": rebuild(nodes, Some(node.id.as_str()))}))
            .collect(),
    )
}

fn example_programs(dir: &Path, out: &mut Vec<PathBuf>) {
    let Ok(entries) = std::fs::read_dir(dir) else {
        return;
    };
    for entry in entries.flatten() {
        let path = entry.path();
        if path.is_dir() {
            example_programs(&path, out);
        } else if path.extension().and_then(|ext| ext.to_str()) == Some("program") {
            out.push(path);
        }
    }
}

/// Compare one parsed program with its structure plan. Returns the first mismatch.
fn check_program(source: &str, label: &str) -> Result<(), String> {
    let program = parser::parse(source).map_err(|e| format!("{label}: parse: {e}"))?;
    let plan = explain::explain_json_with_structure(
        &program,
        &Registry::default(),
        label,
        &BTreeMap::new(),
    )
    .map_err(|e| format!("{label}: explain: {e}"))?;

    let expected = expected_nodes(&program.steps);
    let top_ids: BTreeSet<&str> = expected.iter().map(|node| node.id.as_str()).collect();
    let rows: Vec<Node> = plan_nodes(&plan)
        .map_err(|e| format!("{label}: {e}"))?
        .into_iter()
        .filter(|node| top_ids.contains(node.id.as_str()))
        .collect();

    for node in &expected {
        match rows.iter().find(|row| row.id == node.id) {
            None => {
                return Err(format!(
                    "{label}: no plan row for the step at line {}",
                    node.line
                ))
            }
            Some(row) if row.depth != node.depth || row.parent != node.parent => {
                return Err(format!(
                    "{label}: line {} expected depth {} parent {:?}, plan has depth {} parent {:?}",
                    node.line, node.depth, node.parent, row.depth, row.parent
                ));
            }
            Some(_) => {}
        }
    }

    if rebuild(&expected, None) != rebuild(&rows, None) {
        return Err(format!(
            "{label}: rebuilt control tree differs from the parser's"
        ));
    }
    Ok(())
}

#[test]
fn every_example_round_trips_its_control_tree() {
    let root = Path::new(env!("CARGO_MANIFEST_DIR")).join("examples");
    let mut programs = Vec::new();
    example_programs(&root, &mut programs);
    programs.sort();
    assert!(!programs.is_empty(), "no example programs found");

    // Files that are not standalone programs (graph fragments, modules, and older
    // syntax) do not parse and have no control tree to compare. They are listed, not
    // hidden, so the coordinator can see them.
    let mut mismatches = Vec::new();
    let mut skipped = Vec::new();
    let mut checked = 0;
    for path in &programs {
        let label = path.display().to_string();
        let source = match std::fs::read_to_string(path) {
            Ok(source) => source,
            Err(e) => {
                mismatches.push(format!("{label}: read: {e}"));
                continue;
            }
        };
        if let Err(e) = parser::parse(&source) {
            skipped.push(format!("{label}: {e}"));
            continue;
        }
        checked += 1;
        if let Err(message) = check_program(&source, &label) {
            mismatches.push(message);
        }
    }
    let shown: Vec<String> = mismatches
        .iter()
        .map(|message| {
            message
                .chars()
                .take(400)
                .collect::<String>()
                .replace('\n', " | ")
        })
        .collect();
    eprintln!(
        "round trip checked {checked} of {} examples; skipped as non-parsing:\n{}\n{}",
        programs.len(),
        skipped.join("\n"),
        shown.join("\n")
    );
    assert!(checked >= 40, "only {checked} examples parsed");
    assert!(
        mismatches.is_empty(),
        "{} of {checked} examples differ from the parser's tree: {}",
        mismatches.len(),
        shown.join(" || ")
    );
}

#[test]
fn round_trip_helper_rebuilds_the_sample_nesting() {
    let source = "accept mode as text with default \"x\"
use value [\"a\", \"b\"] as items
for each item in items
  if mode is \"x\"
    run \"rm\" with arguments [\"-f\", item]
  otherwise
    run \"touch\" with arguments [item]
run \"echo\" with arguments [\"done\"]
";
    if let Err(message) = check_program(source, "sample.program") {
        eprintln!("sample.program: {}", message.replace('\n', " | "));
        panic!("sample round-trips");
    }

    let program = parser::parse(source).expect("sample parses");
    let tree = rebuild(&expected_nodes(&program.steps), None);
    // Line 3 is the loop, line 4 the if inside it, line 5 the rm inside the if.
    let loop_node = tree
        .as_array()
        .and_then(|nodes| nodes.iter().find(|node| node["line"] == json!(3)))
        .expect("loop node at top level");
    let if_node = loop_node["children"]
        .as_array()
        .and_then(|nodes| nodes.iter().find(|node| node["line"] == json!(4)))
        .expect("if node inside the loop");
    let rm_under_if = if_node["children"]
        .as_array()
        .is_some_and(|nodes| nodes.iter().any(|node| node["line"] == json!(5)));
    assert!(rm_under_if, "the rm on line 5 sits under the if on line 4");
}
