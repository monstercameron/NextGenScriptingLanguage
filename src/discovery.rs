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
use std::cmp::Reverse;
use std::collections::{BTreeMap, BTreeSet, VecDeque};
use std::env;
use std::fs;
use std::path::{Path, PathBuf};
use std::sync::Arc;

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
    /// Bounded multi-stage routes made only from declared typed converter
    /// contracts. Direct one-stage suggestions remain in `suggested_converters`.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub suggested_conversion_plans: Vec<ConversionPlan>,
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
    /// Existing discovery rank hint; this is not a runtime cost estimate.
    pub score: u32,
}

/// An ordered, contract-compatible multi-stage route between declared ports.
/// This is a type-level plan; it does not claim that every runtime value is
/// accepted by each applet.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ConversionPlan {
    pub steps: Vec<ConverterSuggestion>,
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
/// nested mismatch paths, one-stage converter suggestions, and bounded
/// multi-stage plans made only from declared contracts.
pub fn rank_port_matches(
    registry: &Registry,
    request: &PortMatchRequest,
) -> anyhow::Result<Vec<PortMatch>> {
    let manifests = registry.manifests()?;
    Ok(rank_port_matches_in(&manifests, request))
}

fn rank_port_matches_in(manifests: &[&Manifest], request: &PortMatchRequest) -> Vec<PortMatch> {
    let sources = manifests
        .iter()
        .flat_map(|manifest| endpoints(manifest, true))
        .filter(|endpoint| endpoint_matches_query(endpoint, request.source.as_ref()))
        .collect::<Vec<_>>();
    let targets = manifests
        .iter()
        .flat_map(|manifest| endpoints(manifest, false))
        .filter(|endpoint| endpoint_matches_query(endpoint, request.target.as_ref()))
        .collect::<Vec<_>>();

    let source_metadata = endpoint_match_metadata(&sources, request.source.as_ref(), true);
    let target_metadata = endpoint_match_metadata(&targets, request.target.as_ref(), false);
    let result_limit = request.limit.unwrap_or(100).clamp(1, 1000);
    // The endpoint cross product still needs ranking, but retain only the
    // requested top K. This keeps pair storage and explanation allocations
    // independent of catalog cross-product size.
    let mut ranked_pairs = BTreeSet::new();
    for (source_index, source) in sources.iter().enumerate() {
        for (target_index, target) in targets.iter().enumerate() {
            if request.same_applet && source.applet != target.applet {
                continue;
            }
            let source_meta = &source_metadata[source_index];
            let target_meta = &target_metadata[target_index];
            let compatible = schemas_compatible(&source.schema, &target.schema)
                && source.delivery == target.delivery;
            let pair_schema_score = schema_similarity(&source.schema, &target.schema) * 30 / 100;
            let pair_delivery_score = if source.delivery == target.delivery {
                10
            } else {
                0
            };
            let score = (if compatible { 100 } else { 0 })
                + source_meta.request_score
                + target_meta.request_score
                + pair_schema_score
                + pair_delivery_score
                + if source_meta.matches_request && request.source.is_some() {
                    10
                } else {
                    0
                }
                + if target_meta.matches_request && request.target.is_some() {
                    10
                } else {
                    0
                }
                + if source.applet == target.applet { 3 } else { 0 };
            let candidate = RankedEndpointPair {
                rank: (
                    Reverse(score),
                    source_meta.applet_rank,
                    source_meta.port_rank,
                    target_meta.applet_rank,
                    target_meta.port_rank,
                    source_index,
                    target_index,
                ),
                compatible,
                source_matches_request: source_meta.matches_request,
                target_matches_request: target_meta.matches_request,
            };
            if ranked_pairs.len() < result_limit {
                ranked_pairs.insert(candidate);
            } else if ranked_pairs.last().is_some_and(|worst| candidate < *worst) {
                ranked_pairs.pop_last();
                ranked_pairs.insert(candidate);
            }
        }
    }

    let mut matches = ranked_pairs
        .into_iter()
        .map(|ranked| {
            let (_, _, _, _, _, source_index, target_index) = ranked.rank;
            let source = &sources[source_index];
            let target = &targets[target_index];
            let mut issues = schema_mismatch_reasons(
                &source.schema,
                &target.schema,
                &format!("{}.{}", source.applet, source.port),
                &format!("{}.{}", target.applet, target.port),
            );
            if source.delivery != target.delivery {
                issues.push(format!(
                    "delivery is {:?}, expected {:?}",
                    source.delivery, target.delivery
                ));
            }
            debug_assert_eq!(ranked.compatible, issues.is_empty());
            (
                PortMatch {
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
                    compatible: ranked.compatible,
                    score: ranked.rank.0 .0,
                    source_matches_request: ranked.source_matches_request,
                    target_matches_request: ranked.target_matches_request,
                    issues,
                    suggested_converters: Vec::new(),
                    suggested_conversion_plans: Vec::new(),
                },
                source_index,
                target_index,
            )
        })
        .collect::<Vec<_>>();

    if matches.is_empty() || matches.iter().all(|(matched, _, _)| matched.compatible) {
        return matches.into_iter().map(|(matched, _, _)| matched).collect();
    }

    let mut converter_graph = ConversionGraph::new(conversion_edges(manifests));
    for (matched, source_index, target_index) in &mut matches {
        if matched.compatible {
            continue;
        }
        matched.suggested_converters =
            converter_graph.recommend_direct(&sources[*source_index], &targets[*target_index]);
        matched.suggested_conversion_plans =
            converter_graph.recommend_multi_stage(&sources[*source_index], &targets[*target_index]);
    }
    matches.into_iter().map(|(matched, _, _)| matched).collect()
}

#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord)]
struct RankedEndpointPair {
    // Reverse(score) makes BTreeSet iteration match score-descending order.
    // The endpoint lexical ranks reproduce the public stable tie ordering;
    // original indices preserve it for duplicate endpoint names.
    rank: (Reverse<u32>, usize, usize, usize, usize, usize, usize),
    compatible: bool,
    source_matches_request: bool,
    target_matches_request: bool,
}

#[derive(Debug)]
struct EndpointMatchMetadata {
    applet_rank: usize,
    port_rank: usize,
    request_score: u32,
    matches_request: bool,
}

fn endpoint_match_metadata(
    endpoints: &[Endpoint],
    query: Option<&EndpointQuery>,
    source: bool,
) -> Vec<EndpointMatchMetadata> {
    let applet_ranks = lexical_ranks(endpoints.iter().map(|endpoint| endpoint.applet.as_str()));
    let port_ranks = lexical_ranks(endpoints.iter().map(|endpoint| endpoint.port.as_str()));
    endpoints
        .iter()
        .map(|endpoint| EndpointMatchMetadata {
            applet_rank: applet_ranks[&endpoint.applet],
            port_rank: port_ranks[&endpoint.port],
            request_score: endpoint_request_score(endpoint, query),
            matches_request: endpoint_query_match(endpoint, query, source),
        })
        .collect()
}

fn lexical_ranks<'a>(values: impl Iterator<Item = &'a str>) -> BTreeMap<String, usize> {
    values
        .map(str::to_owned)
        .collect::<BTreeSet<_>>()
        .into_iter()
        .enumerate()
        .map(|(rank, value)| (value, rank))
        .collect()
}

const MAX_CONVERSION_STAGES: usize = 3;
const MAX_CONVERSION_PLANS: usize = 5;
const MAX_CONVERSION_EXPANDED_STATES: usize = 512;
const MAX_CONVERSION_QUEUED_STATES: usize = 2048;
const MAX_CONVERSION_EDGE_CHECKS: usize = 8192;
const MAX_PATHS_PER_TYPED_STATE: usize = 4;
const MAX_TOTAL_CONVERSION_PLANS: usize = 256;
const MAX_TOTAL_CONVERSION_EXPANDED_STATES: usize = 8192;
const MAX_TOTAL_CONVERSION_EDGE_CHECKS: usize = 65536;
const MAX_TOTAL_CONVERSION_SCHEMA_CHECKS: usize = 1_000_000;

#[derive(Debug, Clone, PartialEq, Eq)]
struct ConversionNode {
    schema: ValueSchema,
    delivery: PortDelivery,
}

#[derive(Debug, Clone)]
struct ConversionEdge {
    suggestion: ConverterSuggestion,
    input_schema: ValueSchema,
    input_delivery: PortDelivery,
    output_schema: ValueSchema,
    output_delivery: PortDelivery,
}

#[derive(Debug)]
struct ConversionGraph {
    edges: Vec<ConversionEdge>,
    /// Schemas in the catalog tend to recur across many applets. Cache the
    /// directional schema matches so each distinct shape scans edges once.
    compatible_inputs: BTreeMap<String, Arc<[usize]>>,
    expanded_states: usize,
    edge_checks: usize,
    schema_checks: usize,
    plans_returned: usize,
}

#[derive(Debug, Clone)]
struct ConversionPath {
    steps: Vec<usize>,
    visited: Vec<ConversionNode>,
}

impl ConversionGraph {
    fn new(edges: Vec<ConversionEdge>) -> Self {
        Self {
            edges,
            compatible_inputs: BTreeMap::new(),
            expanded_states: 0,
            edge_checks: 0,
            schema_checks: 0,
            plans_returned: 0,
        }
    }

    fn recommend_direct(&self, source: &Endpoint, target: &Endpoint) -> Vec<ConverterSuggestion> {
        let mut suggestions = self
            .edges
            .iter()
            .filter(|edge| edge_accepts(edge, &source.schema, source.delivery))
            .filter(|edge| edge_produces(edge, &target.schema, target.delivery))
            .map(|edge| edge.suggestion.clone())
            .collect::<Vec<_>>();
        sort_converter_suggestions(&mut suggestions);
        suggestions.dedup_by(|left, right| {
            left.applet == right.applet
                && left.input_port == right.input_port
                && left.output_port == right.output_port
        });
        suggestions
    }

    fn recommend_multi_stage(
        &mut self,
        source: &Endpoint,
        target: &Endpoint,
    ) -> Vec<ConversionPlan> {
        if self.plans_returned >= MAX_TOTAL_CONVERSION_PLANS
            || self.expanded_states >= MAX_TOTAL_CONVERSION_EXPANDED_STATES
            || self.edge_checks >= MAX_TOTAL_CONVERSION_EDGE_CHECKS
            || self.schema_checks >= MAX_TOTAL_CONVERSION_SCHEMA_CHECKS
        {
            return Vec::new();
        }
        let source_node = ConversionNode {
            schema: source.schema.clone(),
            delivery: source.delivery,
        };
        let mut queue = VecDeque::new();
        let mut path_counts = BTreeMap::<(String, u8, usize), usize>::new();

        let compatible_edges = self.compatible_edge_indices(&source.schema);
        let mut edge_checks = 0usize;
        for edge_index in compatible_edges.iter().copied() {
            if queue.len() >= MAX_CONVERSION_QUEUED_STATES {
                break;
            }
            if edge_checks >= MAX_CONVERSION_EDGE_CHECKS
                || self.edge_checks >= MAX_TOTAL_CONVERSION_EDGE_CHECKS
            {
                break;
            }
            edge_checks += 1;
            self.edge_checks += 1;
            let edge = &self.edges[edge_index];
            if edge.input_delivery != source.delivery {
                continue;
            }
            let node = ConversionNode {
                schema: edge.output_schema.clone(),
                delivery: edge.output_delivery,
            };
            // A type-preserving self-loop cannot make a typed route progress.
            if node == source_node {
                continue;
            }
            if !reserve_path(&mut path_counts, &node, 1) {
                continue;
            }
            queue.push_back(ConversionPath {
                steps: vec![edge_index],
                visited: vec![source_node.clone(), node],
            });
        }

        let mut plans = Vec::new();
        let mut expanded_states = 0usize;
        let mut shortest_plan_stages = None;

        while let Some(path) = queue.pop_front() {
            if shortest_plan_stages.is_some_and(|stages| path.steps.len() > stages) {
                break;
            }
            if expanded_states >= MAX_CONVERSION_EXPANDED_STATES
                || self.expanded_states >= MAX_TOTAL_CONVERSION_EXPANDED_STATES
            {
                break;
            }
            expanded_states += 1;
            self.expanded_states += 1;

            let final_edge = &self.edges[*path.steps.last().expect("paths contain one edge")];
            if edge_produces(final_edge, &target.schema, target.delivery) {
                if path.steps.len() >= 2 {
                    if plans.len() >= MAX_CONVERSION_PLANS
                        || self.plans_returned >= MAX_TOTAL_CONVERSION_PLANS
                    {
                        break;
                    }
                    shortest_plan_stages.get_or_insert(path.steps.len());
                    plans.push(ConversionPlan {
                        steps: path
                            .steps
                            .iter()
                            .map(|index| self.edges[*index].suggestion.clone())
                            .collect(),
                    });
                    self.plans_returned += 1;
                }
                // Once a typed state already satisfies the target, adding
                // more applets cannot improve the declared compatibility.
                continue;
            }
            if path.steps.len() >= MAX_CONVERSION_STAGES {
                continue;
            }

            let output_node = path.visited.last().expect("paths contain one node");
            let compatible_edges = self.compatible_edge_indices(&output_node.schema);
            for edge_index in compatible_edges.iter().copied() {
                if edge_checks >= MAX_CONVERSION_EDGE_CHECKS
                    || self.edge_checks >= MAX_TOTAL_CONVERSION_EDGE_CHECKS
                {
                    break;
                }
                edge_checks += 1;
                self.edge_checks += 1;
                let edge = &self.edges[edge_index];
                if edge.input_delivery != output_node.delivery || path.steps.contains(&edge_index) {
                    continue;
                }
                let next_node = ConversionNode {
                    schema: edge.output_schema.clone(),
                    delivery: edge.output_delivery,
                };
                // A path visits each typed state at most once, so cycles and
                // self-loops cannot crowd the bounded search.
                if path.visited.contains(&next_node) {
                    continue;
                }
                let next_stages = path.steps.len() + 1;
                if !reserve_path(&mut path_counts, &next_node, next_stages) {
                    continue;
                }
                if queue.len() >= MAX_CONVERSION_QUEUED_STATES {
                    break;
                }
                let mut steps = path.steps.clone();
                steps.push(edge_index);
                let mut visited = path.visited.clone();
                visited.push(next_node);
                queue.push_back(ConversionPath { steps, visited });
            }
            if edge_checks >= MAX_CONVERSION_EDGE_CHECKS
                || self.edge_checks >= MAX_TOTAL_CONVERSION_EDGE_CHECKS
            {
                break;
            }
        }

        plans.sort_by(|left, right| {
            left.steps
                .len()
                .cmp(&right.steps.len())
                .then_with(|| compare_plan_steps(&left.steps, &right.steps))
        });
        plans.dedup();
        plans.truncate(MAX_CONVERSION_PLANS);
        plans
    }

    fn compatible_edge_indices(&mut self, schema: &ValueSchema) -> Arc<[usize]> {
        let key = schema.json_schema().to_string();
        if !self.compatible_inputs.contains_key(&key) {
            let mut compatible = Vec::new();
            for index in 0..self.edges.len() {
                if self.schema_checks >= MAX_TOTAL_CONVERSION_SCHEMA_CHECKS {
                    break;
                }
                self.schema_checks += 1;
                if schemas_compatible(schema, &self.edges[index].input_schema) {
                    compatible.push(index);
                }
            }
            self.compatible_inputs
                .insert(key.clone(), Arc::from(compatible));
        }
        self.compatible_inputs
            .get(&key)
            .cloned()
            .unwrap_or_else(|| Arc::from([]))
    }
}

fn reserve_path(
    path_counts: &mut BTreeMap<(String, u8, usize), usize>,
    node: &ConversionNode,
    stages: usize,
) -> bool {
    let delivery = match node.delivery {
        PortDelivery::Single => 0,
        PortDelivery::Records => 1,
        PortDelivery::Events => 2,
    };
    let count = path_counts
        .entry((node.schema.json_schema().to_string(), delivery, stages))
        .or_default();
    if *count >= MAX_PATHS_PER_TYPED_STATE {
        return false;
    }
    *count += 1;
    true
}

fn compare_plan_steps(
    left: &[ConverterSuggestion],
    right: &[ConverterSuggestion],
) -> std::cmp::Ordering {
    left.iter()
        .map(|step| (&step.applet, &step.input_port, &step.output_port))
        .cmp(
            right
                .iter()
                .map(|step| (&step.applet, &step.input_port, &step.output_port)),
        )
}

fn sort_converter_suggestions(suggestions: &mut [ConverterSuggestion]) {
    suggestions.sort_by(|left, right| {
        right
            .score
            .cmp(&left.score)
            .then_with(|| left.applet.cmp(&right.applet))
            .then_with(|| left.input_port.cmp(&right.input_port))
            .then_with(|| left.output_port.cmp(&right.output_port))
    });
}

fn edge_accepts(edge: &ConversionEdge, schema: &ValueSchema, delivery: PortDelivery) -> bool {
    schemas_compatible(schema, &edge.input_schema) && delivery == edge.input_delivery
}

fn edge_produces(edge: &ConversionEdge, schema: &ValueSchema, delivery: PortDelivery) -> bool {
    schemas_compatible(&edge.output_schema, schema) && delivery == edge.output_delivery
}

fn conversion_edges(manifests: &[&Manifest]) -> Vec<ConversionEdge> {
    let mut edges = Vec::new();
    for manifest in manifests {
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
            let Some((output_name, output)) = ports.outputs.iter().next() else {
                continue;
            };
            edges.push(ConversionEdge {
                suggestion: ConverterSuggestion {
                    applet: manifest.name.clone(),
                    input_port: input_name.clone(),
                    output_port: output_name.clone(),
                    outcome: ConverterOutcome::ContractsMatch,
                    score: 100,
                },
                input_schema: input.schema.clone(),
                input_delivery: input.delivery,
                output_schema: output.schema.clone(),
                output_delivery: output.delivery,
            });
        }
    }
    edges.sort_by(|left, right| {
        left.suggestion
            .applet
            .cmp(&right.suggestion.applet)
            .then_with(|| left.suggestion.input_port.cmp(&right.suggestion.input_port))
            .then_with(|| {
                left.suggestion
                    .output_port
                    .cmp(&right.suggestion.output_port)
            })
    });
    edges.dedup_by(|left, right| left.suggestion == right.suggestion);
    edges
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
    let same_kind = left.kind == right.kind;
    if !same_kind && !left.is_assignable_to(right) && !right.is_assignable_to(left) {
        return 0;
    }
    let mut score = if same_kind { 60 } else { 35 };
    if left.nullable == right.nullable {
        score += 10;
    }
    match (left.kind, right.kind) {
        (crate::contracts::OutputType::Text, crate::contracts::OutputType::Text) => {
            score += if left.enum_values == right.enum_values {
                30
            } else if schemas_compatible(left, right) || schemas_compatible(right, left) {
                20
            } else {
                0
            };
        }
        (crate::contracts::OutputType::List, crate::contracts::OutputType::List) => {
            match (&left.items, &right.items) {
                (Some(left), Some(right)) => score += schema_similarity(left, right) * 30 / 100,
                (None, None) => score += 30,
                _ => {}
            }
        }
        (crate::contracts::OutputType::Record, crate::contracts::OutputType::Record) => {
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
        (left_kind, right_kind) if left_kind == right_kind => score += 30,
        _ => {}
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
        scalar_only: false,
        additional_properties: None,
        min_properties: None,
        max_properties: None,
        nonempty: false,
        kind: output.kind,
        nullable: false,
        items: output.items.clone(),
        fields: output.fields.clone(),
        required: output.required.clone(),
        enum_values: output.enum_values.clone(),
        single_line: false,
    }
}

#[cfg(test)]
mod tests {
    fn ranked_matches(registry: &Registry, request: &PortMatchRequest) -> Vec<PortMatch> {
        rank_port_matches(registry, request).expect("bundled or fixture manifests parse")
    }

    use super::*;
    use crate::contracts::OutputType;
    #[cfg(unix)]
    use std::fs;
    #[cfg(unix)]
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

        let matches = ranked_matches(&registry, &request);
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
            scalar_only: false,
            additional_properties: None,
            min_properties: None,
            max_properties: None,
            nonempty: false,
            kind: OutputType::Text,
            nullable: false,
            items: None,
            fields: Default::default(),
            required: Vec::new(),
            enum_values: None,
            single_line: false,
        };
        let integer_items = ValueSchema {
            scalar_only: false,
            additional_properties: None,
            min_properties: None,
            max_properties: None,
            nonempty: false,
            kind: OutputType::Integer,
            ..text_items.clone()
        };
        let text_list = ValueSchema {
            scalar_only: false,
            additional_properties: None,
            min_properties: None,
            max_properties: None,
            nonempty: false,
            kind: OutputType::List,
            items: Some(Box::new(text_items)),
            ..integer_items.clone()
        };
        let integer_list = ValueSchema {
            scalar_only: false,
            additional_properties: None,
            min_properties: None,
            max_properties: None,
            nonempty: false,
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
        let matches = ranked_matches(&registry, &request);
        assert_eq!(matches.len(), 1);
        assert!(!matches[0].compatible);
        assert_eq!(matches[0].suggested_converters.len(), 1);
        let suggestion = &matches[0].suggested_converters[0];
        assert_eq!(suggestion.applet, "fixture.text_to_integer");
        assert_eq!(suggestion.input_port, "text");
        assert_eq!(suggestion.output_port, "value");
        assert_eq!(suggestion.outcome, ConverterOutcome::ContractsMatch);
    }

    #[test]
    fn number_to_text_manifest_is_suggested_for_number_to_text_ports() {
        let target = serde_json::json!({
            "name": "fixture.text_target",
            "binary": "target",
            "accepts_stdin": true,
            "parameters": {},
            "arguments": [],
            "contract": {
                "version": 1,
                "ports": { "inputs": {
                    "stdin": {
                        "schema": { "type": "text" },
                        "delivery": "single",
                        "encoding": "raw"
                    }
                } }
            }
        });
        let registry = Registry::from_sources(&[
            include_str!("../manifests/core.number_add.json").to_owned(),
            include_str!("../manifests/core.number_to_text.json").to_owned(),
            target.to_string(),
        ])
        .unwrap();
        let request = PortMatchRequest {
            source: Some(EndpointQuery {
                applet: Some("number.add".into()),
                port: Some("value".into()),
                ..EndpointQuery::default()
            }),
            target: Some(EndpointQuery {
                applet: Some("fixture.text_target".into()),
                port: Some("stdin".into()),
                ..EndpointQuery::default()
            }),
            ..PortMatchRequest::default()
        };

        let matches = ranked_matches(&registry, &request);
        assert_eq!(matches.len(), 1);
        assert!(!matches[0].compatible);
        assert_eq!(matches[0].suggested_converters.len(), 1);
        let suggestion = &matches[0].suggested_converters[0];
        assert_eq!(suggestion.applet, "number.to_text");
        assert_eq!(suggestion.input_port, "value");
        assert_eq!(suggestion.output_port, "value");
        assert_eq!(suggestion.outcome, ConverterOutcome::ContractsMatch);
    }

    #[test]
    fn plans_a_multistage_chain_from_real_declared_converter_manifests() {
        let target = serde_json::json!({
            "name": "fixture.number_target",
            "binary": "target",
            "accepts_stdin": true,
            "parameters": {},
            "arguments": [],
            "contract": {
                "version": 1,
                "ports": { "inputs": {
                    "stdin": {
                        "schema": { "type": "number" },
                        "delivery": "single",
                        "encoding": "json"
                    }
                } }
            }
        });
        let registry = Registry::from_sources(&[
            serde_json::json!({
                "name": "fixture.path_source",
                "binary": "source",
                "parameters": {},
                "arguments": [],
                "contract": { "version": 1, "output": { "type": "path", "encoding": "raw" } }
            })
            .to_string(),
            include_str!("../manifests/core.path_basename.json").to_owned(),
            include_str!("../manifests/core.number_parse_integer.json").to_owned(),
            target.to_string(),
        ])
        .unwrap();
        let request = PortMatchRequest {
            source: Some(EndpointQuery {
                applet: Some("fixture.path_source".into()),
                port: Some("stdout".into()),
                ..EndpointQuery::default()
            }),
            target: Some(EndpointQuery {
                applet: Some("fixture.number_target".into()),
                port: Some("stdin".into()),
                ..EndpointQuery::default()
            }),
            ..PortMatchRequest::default()
        };

        let matches = ranked_matches(&registry, &request);
        assert_eq!(matches.len(), 1);
        let matched = &matches[0];
        assert!(!matched.compatible);
        assert_eq!(matched.suggested_converters.len(), 0);
        assert!(matched.suggested_conversion_plans.iter().any(|plan| {
            plan.steps
                .iter()
                .map(|step| step.applet.as_str())
                .eq(["path.basename", "number.parse_integer"])
        }));
        let plan_json = serde_json::to_value(&matched.suggested_conversion_plans[0]).unwrap();
        assert!(plan_json.get("cost").is_none());
        assert!(plan_json.get("lossless").is_none());
    }

    #[test]
    fn conversion_plans_reject_structural_similarity_without_assignability() {
        let source = serde_json::json!({
            "name": "fixture.source",
            "binary": "source",
            "parameters": {},
            "arguments": [],
            "contract": {
                "version": 1,
                "output": {
                    "type": "record",
                    "encoding": "json",
                    "fields": { "left": { "type": "text" } },
                    "required": ["left"]
                }
            }
        });
        let target = serde_json::json!({
            "name": "fixture.target",
            "binary": "target",
            "accepts_stdin": true,
            "parameters": {},
            "arguments": [],
            "contract": {
                "version": 1,
                "ports": { "inputs": {
                    "stdin": { "schema": { "type": "integer" }, "delivery": "single", "encoding": "json" }
                } }
            }
        });
        let incompatible_record_converter = serde_json::json!({
            "name": "fixture.wrong_record_shape",
            "binary": "@execute-applet",
            "parameters": { "record": { "type": "json", "required": true } },
            "arguments": [{ "literal": "fixture.wrong_record_shape" }, { "literal": "--args" }, { "json_arguments": true }],
            "contract": {
                "version": 1,
                "backend": "in_process",
                "output": { "type": "text", "encoding": "json" },
                "effects": [],
                "ports": {
                    "inputs": {
                        "record": {
                            "schema": {
                                "type": "record",
                                "fields": { "right": { "type": "text" } },
                                "required": ["right"]
                            },
                            "delivery": "single"
                        }
                    },
                    "outputs": {
                        "value": { "schema": { "type": "text" }, "delivery": "single" }
                    }
                }
            }
        });
        let text_to_integer =
            fixture_converter_manifest("fixture.text_to_integer", "text", "integer");
        let registry = Registry::from_sources(&[
            source.to_string(),
            target.to_string(),
            incompatible_record_converter.to_string(),
            text_to_integer,
        ])
        .unwrap();
        let source_schema = ValueSchema {
            scalar_only: false,
            additional_properties: None,
            min_properties: None,
            max_properties: None,
            nonempty: false,
            kind: OutputType::Record,
            nullable: false,
            items: None,
            fields: [("left".to_owned(), text_schema())].into(),
            required: vec!["left".into()],
            enum_values: None,
            single_line: false,
        };
        let wrong_input_schema = ValueSchema {
            fields: [("right".to_owned(), text_schema())].into(),
            required: vec!["right".into()],
            ..source_schema.clone()
        };
        assert!(!schemas_compatible(&source_schema, &wrong_input_schema));
        assert!(schema_similarity(&source_schema, &wrong_input_schema) > 0);

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

        let matches = ranked_matches(&registry, &request);
        assert_eq!(matches.len(), 1);
        assert!(matches[0].suggested_converters.is_empty());
        assert!(matches[0].suggested_conversion_plans.is_empty());
    }

    #[test]
    fn conversion_planning_respects_nested_nullability_enums_and_delivery() {
        let target = serde_json::json!({
            "name": "fixture.boolean_target",
            "binary": "target",
            "accepts_stdin": true,
            "parameters": {},
            "arguments": [],
            "contract": {
                "version": 1,
                "ports": { "inputs": {
                    "stdin": { "schema": { "type": "boolean" }, "delivery": "single", "encoding": "json" }
                } }
            }
        })
        .to_string();
        let integer_to_boolean =
            fixture_converter_manifest("fixture.integer_to_boolean", "integer", "boolean");
        let rank = |source: String, list_converter: String| {
            let registry = Registry::from_sources(&[
                source,
                target.clone(),
                list_converter,
                integer_to_boolean.clone(),
            ])
            .unwrap();
            ranked_matches(
                &registry,
                &PortMatchRequest {
                    source: Some(EndpointQuery {
                        applet: Some("fixture.list_source".into()),
                        port: Some("stdout".into()),
                        ..EndpointQuery::default()
                    }),
                    target: Some(EndpointQuery {
                        applet: Some("fixture.boolean_target".into()),
                        port: Some("stdin".into()),
                        ..EndpointQuery::default()
                    }),
                    ..PortMatchRequest::default()
                },
            )
        };

        let safely_widened = rank(
            fixture_list_source_manifest(false, &["safe"]),
            fixture_list_converter_manifest(
                "fixture.list_to_integer",
                true,
                &["safe", "other"],
                "single",
            ),
        );
        assert!(safely_widened[0]
            .suggested_conversion_plans
            .iter()
            .any(|plan| {
                plan.steps
                    .iter()
                    .map(|step| step.applet.as_str())
                    .eq(["fixture.list_to_integer", "fixture.integer_to_boolean"])
            }));

        let nested_enum_and_nullability_narrowing = rank(
            fixture_list_source_manifest(true, &["safe", "other"]),
            fixture_list_converter_manifest("fixture.list_to_integer", false, &["safe"], "single"),
        );
        assert!(nested_enum_and_nullability_narrowing[0]
            .suggested_conversion_plans
            .is_empty());

        let delivery_mismatch = rank(
            fixture_list_source_manifest(false, &["safe"]),
            fixture_list_converter_manifest(
                "fixture.list_to_integer",
                true,
                &["safe", "other"],
                "records",
            ),
        );
        assert!(delivery_mismatch[0].suggested_conversion_plans.is_empty());
    }

    #[test]
    fn conversion_plans_do_not_repeat_typed_states_or_converter_edges() {
        let source = serde_json::json!({
            "name": "fixture.source",
            "binary": "source",
            "parameters": {},
            "arguments": [],
            "contract": { "version": 1, "output": { "type": "text", "encoding": "raw" } }
        });
        let target = serde_json::json!({
            "name": "fixture.target",
            "binary": "target",
            "accepts_stdin": true,
            "parameters": {},
            "arguments": [],
            "contract": {
                "version": 1,
                "ports": { "inputs": {
                    "stdin": { "schema": { "type": "boolean" }, "delivery": "single", "encoding": "json" }
                } }
            }
        });
        let text_self_loop = fixture_converter_manifest("fixture.text_self_loop", "text", "text");
        let text_to_integer =
            fixture_converter_manifest("fixture.text_to_integer", "text", "integer");
        let integer_to_text =
            fixture_converter_manifest("fixture.integer_to_text", "integer", "text");
        let integer_to_boolean =
            fixture_converter_manifest("fixture.integer_to_boolean", "integer", "boolean");
        let registry = Registry::from_sources(&[
            source.to_string(),
            target.to_string(),
            text_self_loop,
            text_to_integer,
            integer_to_text,
            integer_to_boolean,
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

        let matches = ranked_matches(&registry, &request);
        assert_eq!(matches.len(), 1);
        let plans = &matches[0].suggested_conversion_plans;
        assert_eq!(plans.len(), 1);
        assert_eq!(
            plans[0]
                .steps
                .iter()
                .map(|step| step.applet.as_str())
                .collect::<Vec<_>>(),
            ["fixture.text_to_integer", "fixture.integer_to_boolean"]
        );
    }

    #[test]
    fn bounded_endpoint_ranking_matches_full_sort_oracle() {
        let kinds = ["text", "integer", "number", "boolean", "bytes", "path"];
        let mut sources = kinds
            .iter()
            .map(|kind| {
                serde_json::json!({
                    "name": format!("fixture.source.{kind}"),
                    "binary": "source",
                    "parameters": {},
                    "arguments": [],
                    "contract": {
                        "version": 1,
                        "output": {
                            "type": kind,
                            "encoding": if *kind == "bytes" { "raw" } else { "json" }
                        }
                    }
                })
                .to_string()
            })
            .collect::<Vec<_>>();
        sources.extend(kinds.iter().map(|kind| {
            serde_json::json!({
                "name": format!("fixture.target.{kind}"),
                "binary": "target",
                "accepts_stdin": true,
                "parameters": {},
                "arguments": [],
                "contract": {
                    "version": 1,
                    "ports": { "inputs": {
                        "stdin": {
                            "schema": { "type": kind },
                            "delivery": "single",
                            "encoding": if *kind == "bytes" { "raw" } else { "json" }
                        }
                    } }
                }
            })
            .to_string()
        }));
        let registry = Registry::from_sources(&sources).unwrap();
        let limited = ranked_matches(
            &registry,
            &PortMatchRequest {
                limit: Some(5),
                ..PortMatchRequest::default()
            },
        );
        let full_sort_oracle = rank_port_matches_reference(
            &registry,
            &PortMatchRequest {
                limit: Some(5),
                ..PortMatchRequest::default()
            },
        );
        let reversed_sources = sources.iter().rev().cloned().collect::<Vec<_>>();
        let reversed_registry = Registry::from_sources(&reversed_sources).unwrap();
        let reversed_matches = ranked_matches(
            &reversed_registry,
            &PortMatchRequest {
                limit: Some(5),
                ..PortMatchRequest::default()
            },
        );

        assert_eq!(limited.len(), 5);
        assert_eq!(limited, full_sort_oracle);
        assert_eq!(limited, reversed_matches);
    }

    #[test]
    #[ignore = "manual catalog-scaling probe; run by itself outside builds and tests"]
    fn measures_conversion_planning_for_large_catalogs() {
        use std::time::Instant;

        for converter_count in [100, 500, 1_000, 2_000] {
            let mut sources = vec![
                serde_json::json!({
                    "name": "fixture.source",
                    "binary": "source",
                    "parameters": {},
                    "arguments": [],
                    "contract": { "version": 1, "output": { "type": "path", "encoding": "raw" } }
                })
                .to_string(),
                serde_json::json!({
                    "name": "fixture.target",
                    "binary": "target",
                    "accepts_stdin": true,
                    "parameters": {},
                    "arguments": [],
                    "contract": {
                        "version": 1,
                        "ports": { "inputs": {
                            "stdin": { "schema": { "type": "boolean" }, "delivery": "single", "encoding": "json" }
                        } }
                    }
                })
                .to_string(),
            ];
            sources.extend((0..converter_count).map(|index| {
                fixture_converter_manifest(
                    &format!("fixture.path_to_text_{index:04}"),
                    "path",
                    "text",
                )
            }));
            sources.push(fixture_converter_manifest(
                "fixture.text_to_integer",
                "text",
                "integer",
            ));
            sources.push(fixture_converter_manifest(
                "fixture.integer_to_boolean",
                "integer",
                "boolean",
            ));
            let registry = Registry::from_sources(&sources).unwrap();
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

            let started = Instant::now();
            let matches = ranked_matches(&registry, &request);
            let elapsed = started.elapsed();
            assert_eq!(
                matches[0].suggested_conversion_plans.len(),
                MAX_PATHS_PER_TYPED_STATE
            );
            eprintln!("{converter_count} converters: {elapsed:?}");
        }
    }

    #[test]
    #[ignore = "manual broad-catalog probe; run alone outside builds and tests"]
    fn measures_ranking_for_a_thousand_by_a_thousand_endpoint_catalog() {
        use std::time::Instant;

        let mut sources = Vec::with_capacity(2_000);
        sources.extend((0..1_000).map(|index| {
            serde_json::json!({
                "name": format!("fixture.source_{index:04}"),
                "binary": "source",
                "parameters": {},
                "arguments": [],
                "contract": {
                    "version": 1,
                    "output": { "type": "path", "encoding": "raw" }
                }
            })
            .to_string()
        }));
        sources.extend((0..1_000).map(|index| {
            serde_json::json!({
                "name": format!("fixture.target_{index:04}"),
                "binary": "target",
                "accepts_stdin": true,
                "parameters": {},
                "arguments": [],
                "contract": {
                    "version": 1,
                    "ports": { "inputs": {
                        "stdin": {
                            "schema": { "type": "boolean" },
                            "delivery": "single",
                            "encoding": "json"
                        }
                    } }
                }
            })
            .to_string()
        }));
        let registry = Registry::from_sources(&sources).unwrap();
        let request = PortMatchRequest {
            limit: Some(1_000),
            ..PortMatchRequest::default()
        };

        let started = Instant::now();
        let matches = ranked_matches(&registry, &request);
        let elapsed = started.elapsed();

        assert_eq!(matches.len(), 1_000);
        assert!(matches.iter().all(|matched| {
            !matched.compatible && matched.source.applet == "fixture.source_0000"
        }));
        eprintln!("1,000 source × 1,000 target endpoints: {elapsed:?}");
    }

    fn fixture_converter_manifest(name: &str, input_type: &str, output_type: &str) -> String {
        let parameter_type = match input_type {
            "boolean" | "integer" | "number" | "path" => input_type,
            "text" => "string",
            _ => "json",
        };
        serde_json::json!({
            "name": name,
            "binary": "@execute-applet",
            "parameters": { "input": { "type": parameter_type, "required": true } },
            "arguments": [{ "literal": name }, { "literal": "--args" }, { "json_arguments": true }],
            "contract": {
                "version": 1,
                "backend": "in_process",
                "output": { "type": output_type, "encoding": "json" },
                "effects": [],
                "ports": {
                    "inputs": {
                        "input": { "schema": { "type": input_type }, "delivery": "single" }
                    },
                    "outputs": {
                        "value": { "schema": { "type": output_type }, "delivery": "single" }
                    }
                }
            }
        })
        .to_string()
    }

    /// Full-materialization oracle retaining the original comparator. This is
    /// intentionally independent of the production bounded BTreeSet selector.
    fn rank_port_matches_reference(
        registry: &Registry,
        request: &PortMatchRequest,
    ) -> Vec<PortMatch> {
        let manifests = registry
            .manifests()
            .expect("bundled or fixture manifests parse");
        let sources = manifests
            .iter()
            .flat_map(|manifest| endpoints(manifest, true))
            .filter(|endpoint| endpoint_matches_query(endpoint, request.source.as_ref()))
            .collect::<Vec<_>>();
        let targets = manifests
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
                let mut issues = schema_mismatch_reasons(
                    &source.schema,
                    &target.schema,
                    &format!("{}.{}", source.applet, source.port),
                    &format!("{}.{}", target.applet, target.port),
                );
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
                let score = (if compatible { 100 } else { 0 })
                    + endpoint_request_score(source, request.source.as_ref())
                    + endpoint_request_score(target, request.target.as_ref())
                    + schema_similarity(&source.schema, &target.schema) * 30 / 100
                    + if source.delivery == target.delivery {
                        10
                    } else {
                        0
                    }
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
                    suggested_converters: Vec::new(),
                    suggested_conversion_plans: Vec::new(),
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

    fn fixture_list_source_manifest(nullable_item: bool, values: &[&str]) -> String {
        serde_json::json!({
            "name": "fixture.list_source",
            "binary": "source",
            "parameters": {},
            "arguments": [],
            "contract": {
                "version": 1,
                "output": {
                    "type": "list",
                    "encoding": "json",
                    "items": {
                        "type": "text",
                        "nullable": nullable_item,
                        "enum": values
                    }
                }
            }
        })
        .to_string()
    }

    fn fixture_list_converter_manifest(
        name: &str,
        nullable_item: bool,
        values: &[&str],
        delivery: &str,
    ) -> String {
        serde_json::json!({
            "name": name,
            "binary": "@execute-applet",
            "parameters": { "input": { "type": "json", "required": true } },
            "arguments": [{ "literal": name }, { "literal": "--args" }, { "json_arguments": true }],
            "contract": {
                "version": 1,
                "backend": "in_process",
                "output": { "type": "integer", "encoding": "json" },
                "effects": [],
                "ports": {
                    "inputs": {
                        "input": {
                            "schema": {
                                "type": "list",
                                "items": {
                                    "type": "text",
                                    "nullable": nullable_item,
                                    "enum": values
                                }
                            },
                            "delivery": delivery
                        }
                    },
                    "outputs": {
                        "value": { "schema": { "type": "integer" }, "delivery": "single" }
                    }
                }
            }
        })
        .to_string()
    }

    fn text_schema() -> ValueSchema {
        ValueSchema {
            scalar_only: false,
            additional_properties: None,
            min_properties: None,
            max_properties: None,
            nonempty: false,
            kind: OutputType::Text,
            nullable: false,
            items: None,
            fields: Default::default(),
            required: Vec::new(),
            enum_values: None,
            single_line: false,
        }
    }
}
