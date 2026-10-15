//! Safe installed-command inventory and typed applet-port matching.
//!
//! Command inventory reads PATH directories and executable metadata only. It
//! never starts a command or treats help text as a semantic contract.

use crate::contracts::{
    AppletBackend, AppletPort, Lifecycle, OutputSchema, PortDelivery, ValueSchema,
};
use crate::diagnostics::{schema_mismatch_reasons, schemas_compatible};
use crate::manifest::{Manifest, Registry};
use serde::{Deserialize, Serialize};
use serde_json::Value as JsonValue;
use std::collections::BTreeSet;
use std::env;
use std::fs;
use std::path::{Path, PathBuf};

/// One executable found in a PATH entry. `path_index` preserves shell-like
/// precedence (zero is the first PATH entry); duplicate basenames are retained.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct InstalledCommand {
    pub name: String,
    pub path: String,
    pub path_index: usize,
    /// There is no semantic contract unless a manifest declares one.
    pub contract_status: String,
}

/// Find executable files on PATH without executing them.
pub fn inventory_commands() -> anyhow::Result<Vec<InstalledCommand>> {
    let paths = env::var_os("PATH")
        .map(|value| env::split_paths(&value).collect::<Vec<_>>())
        .unwrap_or_default();
    Ok(inventory_search_paths(&paths))
}

/// Inventory an explicit ordered search path. Exposed for deterministic tools
/// and tests; each result still describes only filesystem metadata.
pub fn inventory_search_paths(paths: &[PathBuf]) -> Vec<InstalledCommand> {
    let mut commands = Vec::new();
    let mut seen = BTreeSet::new();
    for (path_index, directory) in paths.iter().enumerate() {
        let Ok(entries) = fs::read_dir(directory) else {
            continue;
        };
        for entry in entries.flatten() {
            let command_path = entry.path();
            let Ok(metadata) = fs::metadata(&command_path) else {
                continue;
            };
            if !metadata.is_file() || !is_executable_candidate(&command_path, &metadata) {
                continue;
            }
            let Some(name) = command_name(&command_path) else {
                continue;
            };
            let path_text = command_path.to_string_lossy().into_owned();
            if !seen.insert((name.clone(), path_text.clone())) {
                continue;
            }
            commands.push(InstalledCommand {
                name,
                path: path_text,
                path_index,
                contract_status: "unknown_without_manifest".into(),
            });
        }
    }
    commands.sort_by(|left, right| {
        (&left.name, left.path_index, &left.path).cmp(&(&right.name, right.path_index, &right.path))
    });
    commands
}

#[cfg(unix)]
fn is_executable_candidate(path: &Path, metadata: &fs::Metadata) -> bool {
    use std::os::unix::fs::PermissionsExt;
    let _ = path;
    metadata.permissions().mode() & 0o111 != 0
}

#[cfg(windows)]
fn is_executable_candidate(path: &Path, _metadata: &fs::Metadata) -> bool {
    matches!(
        path.extension().and_then(|extension| extension.to_str()),
        Some(extension) if ["exe", "com", "bat", "cmd"].iter().any(|candidate| extension.eq_ignore_ascii_case(candidate))
    )
}

#[cfg(not(any(unix, windows)))]
fn is_executable_candidate(_path: &Path, metadata: &fs::Metadata) -> bool {
    metadata.is_file()
}

#[cfg(windows)]
fn command_name(path: &Path) -> Option<String> {
    path.file_stem()?.to_str().map(str::to_owned)
}

#[cfg(not(windows))]
fn command_name(path: &Path) -> Option<String> {
    path.file_name()?.to_str().map(str::to_owned)
}

/// Select a candidate source output or target input. Applet and port names are
/// hard filters. Schema and delivery are preferences that rank candidates;
/// mismatching candidates remain visible with their incompatibility reasons.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct EndpointQuery {
    /// Optional exact manifest-name filter.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub applet: Option<String>,
    /// Optional exact named-port filter.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub port: Option<String>,
    /// Desired source output schema or target input schema; mismatch candidates
    /// are retained and ranked below compatible candidates.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub schema: Option<ValueSchema>,
    /// Preferred delivery mode. A delivery mismatch is never compatible.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub delivery: Option<PortDelivery>,
}

/// Rank all declared output-to-input port connections in the loaded registry.
/// Omit either endpoint selector to search all candidates for that side.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct PortMatchRequest {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub source: Option<EndpointQuery>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub target: Option<EndpointQuery>,
    /// Require both endpoints to belong to the same applet.
    #[serde(default, skip_serializing_if = "is_false")]
    pub same_applet: bool,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub limit: Option<usize>,
}

fn is_false(value: &bool) -> bool {
    !value
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct PortEndpoint {
    pub applet: String,
    pub port: String,
    pub schema: ValueSchema,
    pub delivery: PortDelivery,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct PortMatch {
    pub source: PortEndpoint,
    pub target: PortEndpoint,
    pub compatible: bool,
    pub score: u32,
    pub source_matches_request: bool,
    pub target_matches_request: bool,
    pub issues: Vec<String>,
    pub suggested_converters: Vec<ConverterSuggestion>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ConverterOutcome {
    ContractsMatch,
}

/// A declared, one-stage applet route between otherwise incompatible ports.
/// The output describes interface compatibility; the applet may still fail at
/// runtime for ordinary data-dependent reasons.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ConverterSuggestion {
    pub applet: String,
    pub input_port: String,
    pub output_port: String,
    pub outcome: ConverterOutcome,
    pub score: u32,
}

#[derive(Debug)]
struct Endpoint {
    applet: String,
    port: String,
    schema: ValueSchema,
    delivery: PortDelivery,
}

/// Return deterministic, ranked candidates. Recursive schema compatibility
/// requires exact recursive kinds and record/list shapes, with safe nullable
/// widening; other type/path/encoding conversions require an explicit applet.
/// Delivery must match. Incompatible results include actionable
/// nested mismatch paths and compatible one-stage converter suggestions.
pub fn rank_port_matches(registry: &Registry, request: &PortMatchRequest) -> Vec<PortMatch> {
    let sources = registry
        .manifests()
        .iter()
        .flat_map(|manifest| endpoints(manifest, true))
        .filter(|endpoint| endpoint_matches_query(endpoint, request.source.as_ref()))
        .collect::<Vec<_>>();
    let targets = registry
        .manifests()
        .iter()
        .flat_map(|manifest| endpoints(manifest, false))
        .filter(|endpoint| endpoint_matches_query(endpoint, request.target.as_ref()))
        .collect::<Vec<_>>();

    let mut matches = Vec::new();
    for source in &sources {
        for target in &targets {
            if request.same_applet && source.applet != target.applet {
                continue;
            }
            let schema_issues = schema_mismatch_reasons(
                &source.schema,
                &target.schema,
                &format!("{}.{}", source.applet, source.port),
                &format!("{}.{}", target.applet, target.port),
            );
            let mut issues = schema_issues;
            if source.delivery != target.delivery {
                issues.push(format!(
                    "delivery is {:?}, expected {:?}",
                    source.delivery, target.delivery
                ));
            }
            let compatible = issues.is_empty();
            let source_matches_request =
                endpoint_query_match(source, request.source.as_ref(), true);
            let target_matches_request =
                endpoint_query_match(target, request.target.as_ref(), false);
            let source_request_score = endpoint_request_score(source, request.source.as_ref());
            let target_request_score = endpoint_request_score(target, request.target.as_ref());
            let pair_schema_score = schema_similarity(&source.schema, &target.schema) * 30 / 100;
            let pair_delivery_score = if source.delivery == target.delivery {
                10
            } else {
                0
            };
            let score = (if compatible { 100 } else { 0 })
                + source_request_score
                + target_request_score
                + pair_schema_score
                + pair_delivery_score
                + if source_matches_request && request.source.is_some() {
                    10
                } else {
                    0
                }
                + if target_matches_request && request.target.is_some() {
                    10
                } else {
                    0
                }
                + if source.applet == target.applet { 3 } else { 0 };
            matches.push(PortMatch {
                source: PortEndpoint {
                    applet: source.applet.clone(),
                    port: source.port.clone(),
                    schema: source.schema.clone(),
                    delivery: source.delivery,
                },
                target: PortEndpoint {
                    applet: target.applet.clone(),
                    port: target.port.clone(),
                    schema: target.schema.clone(),
                    delivery: target.delivery,
                },
                compatible,
                score,
                source_matches_request,
                target_matches_request,
                issues,
                suggested_converters: if compatible {
                    Vec::new()
                } else {
                    recommend_converters(registry, source, target)
                },
            });
        }
    }
    matches.sort_by(|left, right| {
        right
            .score
            .cmp(&left.score)
            .then_with(|| left.source.applet.cmp(&right.source.applet))
            .then_with(|| left.source.port.cmp(&right.source.port))
            .then_with(|| left.target.applet.cmp(&right.target.applet))
            .then_with(|| left.target.port.cmp(&right.target.port))
    });
    matches.truncate(request.limit.unwrap_or(100).clamp(1, 1000));
    matches
}

fn recommend_converters(
    registry: &Registry,
    source: &Endpoint,
    target: &Endpoint,
) -> Vec<ConverterSuggestion> {
    let mut suggestions = Vec::new();
    for manifest in registry.manifests() {
        let Some(contract) = manifest.execution_contract() else {
            continue;
        };
        if contract.backend != AppletBackend::InProcess
            || contract.lifecycle != Lifecycle::OneShot
            || !contract.effects.is_empty()
        {
            continue;
        }
        let Some(ports) = manifest.ports() else {
            continue;
        };
        if ports.outputs.len() != 1 {
            continue;
        }
        let contract_projection = manifest.contract();
        let required_parameters = contract_projection
            .pointer("/parameters_schema/required")
            .and_then(JsonValue::as_array)
            .into_iter()
            .flatten()
            .filter_map(JsonValue::as_str)
            .map(str::to_owned)
            .collect::<BTreeSet<_>>();

        for (input_name, input) in &ports.inputs {
            // The source connection may satisfy at most the one required
            // parameter represented by this input. Other mandatory inputs
            // cannot be guessed or filled in by an agent-directed suggestion.
            if required_parameters
                .iter()
                .any(|required| required.as_str() != input_name.as_str())
            {
                continue;
            }
            let input_matches = schemas_compatible(&source.schema, &input.schema)
                && input.delivery == source.delivery;
            if !input_matches {
                continue;
            }
            let Some((output_name, output)) = ports.outputs.iter().next() else {
                continue;
            };
            if schemas_compatible(&output.schema, &target.schema)
                && output.delivery == target.delivery
            {
                suggestions.push(ConverterSuggestion {
                    applet: manifest.name.clone(),
                    input_port: input_name.clone(),
                    output_port: output_name.clone(),
                    outcome: ConverterOutcome::ContractsMatch,
                    score: 100,
                });
            }
        }
    }
    suggestions.sort_by(|left, right| {
        right
            .score
            .cmp(&left.score)
            .then_with(|| left.applet.cmp(&right.applet))
            .then_with(|| left.input_port.cmp(&right.input_port))
            .then_with(|| left.output_port.cmp(&right.output_port))
    });
    suggestions.dedup_by(|left, right| {
        left.applet == right.applet
            && left.input_port == right.input_port
            && left.output_port == right.output_port
    });
    suggestions
}

fn endpoint_matches_query(endpoint: &Endpoint, query: Option<&EndpointQuery>) -> bool {
    let Some(query) = query else {
        return true;
    };
    if query
        .applet
        .as_ref()
        .is_some_and(|applet| endpoint.applet != *applet)
        || query
            .port
            .as_ref()
            .is_some_and(|port| endpoint.port != *port)
    {
        return false;
    }
    true
}

fn endpoint_query_match(endpoint: &Endpoint, query: Option<&EndpointQuery>, source: bool) -> bool {
    let Some(query) = query else {
        return true;
    };
    query.schema.as_ref().is_none_or(|schema| {
        if source {
            schemas_compatible(&endpoint.schema, schema)
        } else {
            schemas_compatible(schema, &endpoint.schema)
        }
    }) && query
        .delivery
        .is_none_or(|delivery| delivery == endpoint.delivery)
}

fn endpoint_request_score(endpoint: &Endpoint, query: Option<&EndpointQuery>) -> u32 {
    let Some(query) = query else {
        return 0;
    };
    let schema_score = query
        .schema
        .as_ref()
        .map(|schema| schema_similarity(&endpoint.schema, schema) * 25 / 100)
        .unwrap_or(0);
    let delivery_score = if query
        .delivery
        .is_some_and(|delivery| delivery == endpoint.delivery)
    {
        8
    } else {
        0
    };
    schema_score + delivery_score
}

/// A ranking hint only. Compatibility remains strict and is decided by
/// `schema_mismatch_reasons`; this score prefers candidates whose recursive
/// shapes agree before considering lexical tie breakers.
fn schema_similarity(left: &ValueSchema, right: &ValueSchema) -> u32 {
    if left.kind != right.kind {
        return 0;
    }
    let mut score = 60;
    if left.nullable == right.nullable {
        score += 10;
    }
    match left.kind {
        crate::contracts::OutputType::List => match (&left.items, &right.items) {
            (Some(left), Some(right)) => score += schema_similarity(left, right) * 30 / 100,
            (None, None) => score += 30,
            _ => {}
        },
        crate::contracts::OutputType::Record => {
            let names = left
                .fields
                .keys()
                .chain(right.fields.keys())
                .collect::<BTreeSet<_>>();
            let shared = left
                .fields
                .keys()
                .filter(|name| right.fields.contains_key(*name))
                .collect::<Vec<_>>();
            if names.is_empty() {
                score += 10;
            } else {
                score += (shared.len() as u32 * 10) / names.len() as u32;
            }
            if shared.is_empty() {
                score += if names.is_empty() { 10 } else { 0 };
            } else {
                let nested = shared
                    .iter()
                    .filter_map(|name| {
                        Some(schema_similarity(
                            left.fields.get(*name)?,
                            right.fields.get(*name)?,
                        ))
                    })
                    .sum::<u32>()
                    / shared.len() as u32;
                score += nested * 10 / 100;
            }
            if left.required.iter().collect::<BTreeSet<_>>()
                == right.required.iter().collect::<BTreeSet<_>>()
            {
                score += 10;
            }
        }
        _ => score += 30,
    }
    score.min(100)
}

fn endpoints(manifest: &Manifest, output: bool) -> Vec<Endpoint> {
    let mut endpoints = Vec::new();
    if let Some(ports) = manifest.ports() {
        let declared = if output {
            &ports.outputs
        } else {
            &ports.inputs
        };
        endpoints.extend(
            declared
                .iter()
                .map(|(name, port)| endpoint(manifest, name, port)),
        );
    }
    if output {
        if let Some(schema) = manifest
            .execution_contract()
            .and_then(|contract| contract.output.as_ref())
        {
            endpoints.push(Endpoint {
                applet: manifest.name.clone(),
                port: "stdout".into(),
                schema: schema_from_output(schema),
                delivery: PortDelivery::Single,
            });
        }
    }
    endpoints
}

fn endpoint(manifest: &Manifest, name: &str, port: &AppletPort) -> Endpoint {
    Endpoint {
        applet: manifest.name.clone(),
        port: name.to_owned(),
        schema: port.schema.clone(),
        delivery: port.delivery,
    }
}

fn schema_from_output(output: &OutputSchema) -> ValueSchema {
    ValueSchema {
        kind: output.kind,
        nullable: false,
        items: output.items.clone(),
        fields: output.fields.clone(),
        required: output.required.clone(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::contracts::OutputType;
    use std::fs;
    use tempfile::tempdir;

    #[cfg(unix)]
    #[test]
    fn inventory_reads_executable_metadata_without_running_candidates() {
        use std::os::unix::fs::PermissionsExt;

        let directory = tempdir().unwrap();
        let marker = directory.path().join("ran");
        let command = directory.path().join("would-run");
        fs::write(&command, format!("#!/bin/sh\ntouch {:?}\n", marker)).unwrap();
        let mut permissions = fs::metadata(&command).unwrap().permissions();
        permissions.set_mode(0o755);
        fs::set_permissions(&command, permissions).unwrap();

        let found = inventory_search_paths(&[directory.path().to_path_buf()]);
        assert_eq!(found.len(), 1);
        assert_eq!(found[0].name, "would-run");
        assert_eq!(found[0].contract_status, "unknown_without_manifest");
        assert!(
            !marker.exists(),
            "inventory must not execute discovered commands"
        );
    }

    #[test]
    fn compatible_port_pair_ranks_ahead_of_mismatch_and_reports_reason() {
        let source = serde_json::json!({
            "name": "fixture.source",
            "binary": "source",
            "parameters": {}, "arguments": [],
            "contract": { "version": 1, "output": { "type": "text", "encoding": "raw" } }
        });
        let text_target = serde_json::json!({
            "name": "fixture.text_target",
            "binary": "text-target",
            "accepts_stdin": true,
            "parameters": {}, "arguments": [],
            "contract": { "version": 1, "ports": { "inputs": {
                "stdin": { "schema": { "type": "text" }, "delivery": "single", "encoding": "raw" }
            } } }
        });
        let integer_target = serde_json::json!({
            "name": "fixture.integer_target",
            "binary": "integer-target",
            "accepts_stdin": true,
            "parameters": {}, "arguments": [],
            "contract": { "version": 1, "ports": { "inputs": {
                "stdin": { "schema": { "type": "integer" }, "delivery": "single", "encoding": "json" }
            } } }
        });
        let registry = Registry::from_sources(&[
            source.to_string(),
            text_target.to_string(),
            integer_target.to_string(),
        ])
        .unwrap();
        let request = PortMatchRequest {
            source: Some(EndpointQuery {
                applet: Some("fixture.source".into()),
                port: Some("stdout".into()),
                ..EndpointQuery::default()
            }),
            ..PortMatchRequest::default()
        };

        let matches = rank_port_matches(&registry, &request);
        assert_eq!(matches.len(), 2);
        assert!(matches[0].compatible, "{matches:#?}");
        assert_eq!(matches[0].target.applet, "fixture.text_target");
        let mismatch = matches
            .iter()
            .find(|candidate| candidate.target.applet == "fixture.integer_target")
            .unwrap();
        assert!(!mismatch.compatible);
        assert!(mismatch.issues.iter().any(|issue| issue.contains(
            "fixture.source.stdout → fixture.integer_target.stdin is text, expected integer"
        )));
    }

    #[test]
    fn nested_schema_similarity_ranks_matching_list_item_types() {
        let text_items = ValueSchema {
            kind: OutputType::Text,
            nullable: false,
            items: None,
            fields: Default::default(),
            required: Vec::new(),
        };
        let integer_items = ValueSchema {
            kind: OutputType::Integer,
            ..text_items.clone()
        };
        let text_list = ValueSchema {
            kind: OutputType::List,
            items: Some(Box::new(text_items)),
            ..integer_items.clone()
        };
        let integer_list = ValueSchema {
            kind: OutputType::List,
            items: Some(Box::new(integer_items)),
            ..text_list.clone()
        };
        assert_eq!(schema_similarity(&text_list, &text_list), 100);
        assert!(schema_similarity(&text_list, &integer_list) < 100);
    }

    #[test]
    fn mismatch_suggests_a_declared_pure_single_stage_converter() {
        let source = serde_json::json!({
            "name": "fixture.source",
            "binary": "source",
            "parameters": {}, "arguments": [],
            "contract": { "version": 1, "output": { "type": "text", "encoding": "raw" } }
        });
        let target = serde_json::json!({
            "name": "fixture.target",
            "binary": "target",
            "accepts_stdin": true,
            "parameters": {}, "arguments": [],
            "contract": { "version": 1, "ports": { "inputs": {
                "stdin": { "schema": { "type": "integer" }, "delivery": "single", "encoding": "json" }
            } } }
        });
        let converter = serde_json::json!({
            "name": "fixture.text_to_integer",
            "binary": "@execute-applet",
            "parameters": { "text": { "type": "string", "required": true } },
            "arguments": [{ "parameter": "text" }],
            "contract": {
                "version": 1,
                "backend": "in_process",
                "output": { "type": "integer", "encoding": "json" },
                "effects": [],
                "ports": {
                    "inputs": {
                        "text": { "schema": { "type": "text" }, "delivery": "single" }
                    },
                    "outputs": {
                        "value": { "schema": { "type": "integer" }, "delivery": "single" }
                    }
                }
            }
        });
        let two_input_converter = serde_json::json!({
            "name": "fixture.needs_radix",
            "binary": "@execute-applet",
            "parameters": {
                "text": { "type": "string", "required": true },
                "radix": { "type": "integer", "required": true }
            },
            "arguments": [{ "parameter": "text" }, { "parameter": "radix" }],
            "contract": {
                "version": 1,
                "backend": "in_process",
                "output": { "type": "integer", "encoding": "json" },
                "effects": [],
                "ports": {
                    "inputs": {
                        "text": { "schema": { "type": "text" }, "delivery": "single" },
                        "radix": { "schema": { "type": "integer" }, "delivery": "single" }
                    },
                    "outputs": {
                        "value": { "schema": { "type": "integer" }, "delivery": "single" }
                    }
                }
            }
        });
        let registry = Registry::from_sources(&[
            source.to_string(),
            target.to_string(),
            converter.to_string(),
            two_input_converter.to_string(),
        ])
        .unwrap();
        let request = PortMatchRequest {
            source: Some(EndpointQuery {
                applet: Some("fixture.source".into()),
                port: Some("stdout".into()),
                ..EndpointQuery::default()
            }),
            target: Some(EndpointQuery {
                applet: Some("fixture.target".into()),
                port: Some("stdin".into()),
                ..EndpointQuery::default()
            }),
            ..PortMatchRequest::default()
        };
        let matches = rank_port_matches(&registry, &request);
        assert_eq!(matches.len(), 1);
        assert!(!matches[0].compatible);
        assert_eq!(matches[0].suggested_converters.len(), 1);
        let suggestion = &matches[0].suggested_converters[0];
        assert_eq!(suggestion.applet, "fixture.text_to_integer");
        assert_eq!(suggestion.input_port, "text");
        assert_eq!(suggestion.output_port, "value");
        assert_eq!(suggestion.outcome, ConverterOutcome::ContractsMatch);
    }
}
