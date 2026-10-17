//! `execute explain FILE --json`: a per-step plan of native effects.
//!
//! The plan is derived from the parsed program and the resolved manifests. It
//! does not run anything. Effects come only from applet contracts that declare
//! them. A step whose manifest has no contract, or that runs a native command
//! directly, reports `effects: null` ("undeclared"), never an empty guess.
//! Argv is shown only when every parameter is a literal value.

use crate::ast::{Invocation, LoopKind, Program, StepKind, Value};
use crate::manifest::Registry;
use anyhow::Result;
use serde_json::{json, Value as Json};
use std::collections::BTreeSet;

/// Protocol identifier of the `--json` object.
pub const PROTOCOL: &str = "execute.explain/1";

/// Build the explain object for a parsed program. `program_path` is shown as
/// given. The caller validates the program first.
pub fn explain_json(program: &Program, registry: &Registry, program_path: &str) -> Result<Json> {
    let mut steps = Vec::new();
    for step in &program.steps {
        let invocations = invocations(&step.kind);
        if invocations.is_empty() {
            let row = match &step.kind {
                StepKind::Save { .. } => json!({
                    "line": step.line, "kind": "save", "direct": false,
                    "applet": null, "binary": null, "argv": null, "effects": null,
                    "capabilities": null, "writes": true, "network": false
                }),
                _ => json!({
                    "line": step.line, "kind": "other", "direct": false,
                    "applet": null, "binary": null, "argv": null, "effects": null,
                    "capabilities": null, "writes": false, "network": false
                }),
            };
            steps.push(row);
            continue;
        }
        for (kind, call) in invocations {
            steps.push(invocation_row(step.line, kind, call, registry));
        }
    }
    Ok(summary_object(program_path, steps))
}

/// Each invocation a step performs, tagged with its step kind.
fn invocations(kind: &StepKind) -> Vec<(&'static str, &Invocation)> {
    match kind {
        StepKind::TaskGroup { tasks, .. } => {
            tasks.iter().map(|call| ("task_group", call)).collect()
        }
        StepKind::SpawnJob { stages, .. } => stages
            .iter()
            .map(|stage| ("spawn_job", &stage.call))
            .collect(),
        StepKind::IfCommand { call, .. } => vec![("if_command", call)],
        StepKind::CallProgram { call, .. } => vec![("call_program", call)],
        StepKind::Invoke { call, .. } => vec![("invoke", call)],
        StepKind::Loop {
            kind: LoopKind::WhileCommand { call, .. },
            ..
        } => vec![("while_command", call)],
        _ => Vec::new(),
    }
}

fn invocation_row(line: usize, kind: &str, call: &Invocation, registry: &Registry) -> Json {
    // A reusable program is a local call, not an applet or native command.
    if kind == "call_program" {
        return json!({
            "line": line, "kind": kind, "direct": false,
            "applet": null, "binary": null, "argv": null, "effects": null,
            "capabilities": null, "writes": null, "network": null
        });
    }
    // A quoted executable bypasses the catalog, so its effects are unknown.
    if call.direct {
        return json!({
            "line": line, "kind": kind, "direct": true,
            "applet": null, "binary": call.operation, "argv": null, "effects": null,
            "capabilities": null, "writes": null, "network": null
        });
    }
    let Ok(manifest) = registry.resolve_call(call) else {
        return json!({
            "line": line, "kind": kind, "direct": false,
            "applet": call.operation, "binary": null, "argv": null, "effects": null,
            "capabilities": null, "writes": null, "network": null, "resolved": false
        });
    };
    let argv = if call.parameters.values().any(has_reference) {
        None
    } else {
        manifest.build_arguments(&call.parameters).ok()
    };
    let (effects, capabilities) = match manifest.execution_contract() {
        Some(contract) => (
            Some(contract.effects.clone()),
            Some(contract.capabilities.clone()),
        ),
        None => (None, None),
    };
    let writes = effects
        .as_ref()
        .map(|effects| effects.iter().any(|effect| is_write_effect(effect)));
    let network = effects
        .as_ref()
        .map(|effects| effects.iter().any(|effect| effect.starts_with("network.")));
    json!({
        "line": line,
        "kind": kind,
        "direct": false,
        "applet": manifest.name_for(call),
        "binary": manifest.binary_for(call),
        "argv": argv,
        "effects": effects,
        "capabilities": capabilities,
        "writes": writes,
        "network": network,
    })
}

fn has_reference(value: &Value) -> bool {
    match value {
        Value::Reference(_) => true,
        Value::List(items) => items.iter().any(has_reference),
        Value::Record(fields) => fields.values().any(has_reference),
        _ => false,
    }
}

fn is_write_effect(effect: &str) -> bool {
    effect.starts_with("fs.write") || effect.starts_with("file.write") || effect.contains(".write")
}

fn summary_object(program_path: &str, steps: Vec<Json>) -> Json {
    let mut executables = BTreeSet::new();
    let mut write_effects = BTreeSet::new();
    let mut network = false;
    let mut saves = false;
    let mut undeclared = 0usize;
    let mut invocations = 0usize;
    for row in &steps {
        let kind = row["kind"].as_str().unwrap_or("");
        if kind == "save" {
            saves = true;
        }
        if matches!(kind, "other" | "save" | "call_program") {
            continue;
        }
        invocations += 1;
        if let Some(binary) = row["binary"].as_str() {
            executables.insert(binary.to_owned());
        }
        if row["effects"].is_null() {
            undeclared += 1;
        }
        if let Some(effects) = row["effects"].as_array() {
            for effect in effects.iter().filter_map(Json::as_str) {
                if is_write_effect(effect) {
                    write_effects.insert(effect.to_owned());
                }
                if effect.starts_with("network.") {
                    network = true;
                }
            }
        }
    }
    json!({
        "protocol": PROTOCOL,
        "program": program_path,
        "steps": steps,
        "summary": {
            "invocations": invocations,
            "executables": executables.into_iter().collect::<Vec<_>>(),
            "writes": saves || !write_effects.is_empty(),
            "write_effects": write_effects.into_iter().collect::<Vec<_>>(),
            "save_steps": saves,
            "network": network,
            "undeclared_invocations": undeclared,
            "effects_declared": undeclared == 0,
            "privileged": "not_classified_see_policy_marking"
        }
    })
}
