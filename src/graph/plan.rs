use crate::ast::{Invocation, Value};
use crate::contracts::{
    AppletPort, OutputEncoding, OutputSchema, OutputType, PortDelivery, Sensitivity, ValueSchema,
};
use crate::manifest::Registry;
use crate::security::SensitivitySet;
use anyhow::{anyhow, bail, Result};
use serde::{Deserialize, Serialize};
use std::collections::{BTreeMap, BTreeSet, VecDeque};

/// The message contract of one named graph port. The payload schema is the
/// repository-wide contract schema; this module adds only graph delivery rules.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct PortSchema {
    pub schema: ValueSchema,
    pub delivery: PortDelivery,
    /// Declared information sensitivity carried by this boundary.
    #[serde(default, skip_serializing_if = "is_unknown_sensitivity")]
    pub sensitivity: Sensitivity,
    /// Required incoming ports must be bound or supplied as a literal parameter.
    #[serde(default)]
    pub required: bool,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub pointer: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub encoding: Option<OutputEncoding>,
}

impl PortSchema {
    pub fn single(schema: ValueSchema) -> Self {
        Self {
            schema,
            delivery: PortDelivery::Single,
            sensitivity: Sensitivity::Unknown,
            required: false,
            pointer: None,
            encoding: None,
        }
    }

    pub fn records(schema: ValueSchema) -> Self {
        Self {
            schema,
            delivery: PortDelivery::Records,
            sensitivity: Sensitivity::Unknown,
            required: false,
            pointer: None,
            encoding: None,
        }
    }

    pub fn events(schema: ValueSchema) -> Self {
        Self {
            schema,
            delivery: PortDelivery::Events,
            sensitivity: Sensitivity::Unknown,
            required: false,
            pointer: None,
            encoding: None,
        }
    }
}

pub(crate) fn value_schema_from_output(output: &OutputSchema) -> ValueSchema {
    ValueSchema {
        kind: output.kind,
        nullable: false,
        items: output.items.clone(),
        fields: output.fields.clone(),
        required: output.required.clone(),
    }
}

/// `records` is a typed multiport; `events` is a revisioned event stream.
/// A single value can feed a records port as its first item, but data and events
/// never cross implicitly.
impl PortSchema {
    pub fn from_applet(port: &AppletPort, required: bool) -> Self {
        Self {
            schema: port.schema.clone(),
            delivery: port.delivery,
            sensitivity: port.sensitivity,
            required,
            pointer: port.pointer.clone(),
            encoding: port.encoding,
        }
    }
}

fn is_unknown_sensitivity(value: &Sensitivity) -> bool {
    *value == Sensitivity::Unknown
}

/// Source and destination port namespaces. Events are published through the
/// `events` namespace and consumed through an input declared with `Events`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PortDirection {
    Input,
    Output,
    Event,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct PortRef {
    pub node: String,
    pub port: String,
}

impl PortRef {
    pub fn new(node: impl Into<String>, port: impl Into<String>) -> Self {
        Self {
            node: node.into(),
            port: port.into(),
        }
    }
}

/// How an edge behaves when its bounded queue reaches capacity.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum BufferPolicy {
    /// Block the producer until the consumer makes room. No messages are lost.
    Lossless,
    /// Keep queued messages and discard a new message when full.
    DropNewest,
    /// Discard the oldest queued message to make room for the new one.
    DropOldest,
    /// Keep only the newest queued message, coalescing the backlog.
    Latest,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct EdgeSpec {
    pub from: PortRef,
    pub to: PortRef,
    pub capacity: usize,
    pub policy: BufferPolicy,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub line: Option<usize>,
}

impl EdgeSpec {
    pub fn lossless(from: PortRef, to: PortRef, capacity: usize) -> Self {
        Self {
            from,
            to,
            capacity,
            policy: BufferPolicy::Lossless,
            line: None,
        }
    }
}

/// A node's runtime kind. The generic executor supplies the implementation;
/// the graph owns topology, typed transport, cancellation and event revisions.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum GraphNodeKind {
    Applet {
        call: Invocation,
    },
    Timer {
        interval_ms: u64,
    },
    Listener {
        source: crate::ast::ListenSource,
        binding: String,
    },
    NativeStream {
        source: String,
        calls: Vec<Invocation>,
    },
    Renderer {
        backend: String,
    },
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct GraphNode {
    pub id: String,
    pub kind: GraphNodeKind,
    pub inputs: BTreeMap<String, PortSchema>,
    pub outputs: BTreeMap<String, PortSchema>,
    pub events: BTreeMap<String, PortSchema>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub line: Option<usize>,
}

impl GraphNode {
    pub fn applet(
        id: impl Into<String>,
        call: Invocation,
        inputs: BTreeMap<String, PortSchema>,
        outputs: BTreeMap<String, PortSchema>,
    ) -> Self {
        Self {
            id: id.into(),
            kind: GraphNodeKind::Applet { call },
            inputs,
            outputs,
            events: BTreeMap::new(),
            line: None,
        }
    }

    pub fn invocation(&self) -> Option<&Invocation> {
        self.invocations().into_iter().next()
    }

    pub fn invocations(&self) -> Vec<&Invocation> {
        match &self.kind {
            GraphNodeKind::Applet { call } => vec![call],
            GraphNodeKind::Listener {
                source: crate::ast::ListenSource::Applet(call),
                ..
            } => vec![call],
            GraphNodeKind::NativeStream { calls, .. } => calls.iter().collect(),
            _ => Vec::new(),
        }
    }

    pub fn port(&self, direction: PortDirection, name: &str) -> Option<&PortSchema> {
        match direction {
            PortDirection::Input => self.inputs.get(name),
            PortDirection::Output => self.outputs.get(name),
            PortDirection::Event => self.events.get(name),
        }
    }

    pub fn source_port(&self, name: &str) -> Option<(PortDirection, &PortSchema)> {
        self.outputs
            .get(name)
            .map(|port| (PortDirection::Output, port))
            .or_else(|| {
                self.events
                    .get(name)
                    .map(|port| (PortDirection::Event, port))
            })
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct GraphDiagnostic {
    pub code: &'static str,
    pub message: String,
    pub line: Option<usize>,
}

impl std::fmt::Display for GraphDiagnostic {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        if let Some(line) = self.line {
            write!(f, "line {line}: ")?;
        }
        write!(f, "{}: {}", self.code, self.message)
    }
}

/// A fully parsed graph. `validate` is a pure preflight operation: it checks
/// every port and edge before any executor can start a node.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct GraphPlan {
    pub name: String,
    nodes: Vec<GraphNode>,
    edges: Vec<EdgeSpec>,
    #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
    initial_inputs: BTreeMap<String, BTreeMap<String, Value>>,
}

impl GraphPlan {
    pub fn new(name: impl Into<String>) -> Self {
        Self {
            name: name.into(),
            nodes: Vec::new(),
            edges: Vec::new(),
            initial_inputs: BTreeMap::new(),
        }
    }

    pub fn nodes(&self) -> &[GraphNode] {
        &self.nodes
    }

    pub fn edges(&self) -> &[EdgeSpec] {
        &self.edges
    }

    pub fn initial_inputs(&self) -> &BTreeMap<String, BTreeMap<String, Value>> {
        &self.initial_inputs
    }

    /// Stable machine-readable topology inspection. Nodes, ports, and edges
    /// serialize in source/declaration order; named port maps are ordered.
    pub fn to_json(&self) -> Result<String> {
        Ok(serde_json::to_string_pretty(self)?)
    }

    /// Render an inspectable dependency/event graph without executing nodes.
    /// Labels include the node lifecycle, typed ports, and edge queue policy.
    pub fn to_dot(&self) -> String {
        let mut output = format!("digraph \"{}\" {{\n  rankdir=LR;\n", dot_escape(&self.name));
        for node in &self.nodes {
            let mut label = vec![format!("{}\n{}", node.id, node_kind_name(&node.kind))];
            if let Some(line) = node.line {
                label.push(format!("source line {line}"));
            }
            for (section, ports) in [
                ("in", &node.inputs),
                ("out", &node.outputs),
                ("event", &node.events),
            ] {
                for (name, port) in ports {
                    let schema = serde_json::to_string(&port.schema)
                        .unwrap_or_else(|_| format!("{:?}", port.schema));
                    label.push(format!(
                        "{section} {name}: {schema} {:?} [{}]{}",
                        port.delivery,
                        sensitivity_name(port.sensitivity),
                        if port.required { " required" } else { "" }
                    ));
                }
            }
            output.push_str(&format!(
                "  \"{}\" [label=\"{}\"];\n",
                dot_escape(&node.id),
                dot_escape(&label.join("\n"))
            ));
        }
        for edge in &self.edges {
            let source_line = edge
                .line
                .map_or(String::new(), |line| format!(" line={line}"));
            let edge_label = dot_escape(&format!(
                "{}.{} → {}.{}\n{:?} capacity={}{}",
                edge.from.node,
                edge.from.port,
                edge.to.node,
                edge.to.port,
                edge.policy,
                edge.capacity,
                source_line
            ));
            output.push_str(&format!(
                "  \"{}\" -> \"{}\" [label=\"{}\"];\n",
                dot_escape(&edge.from.node),
                dot_escape(&edge.to.node),
                edge_label
            ));
        }
        output.push_str("}\n");
        output
    }

    pub fn add_node(&mut self, node: GraphNode) -> Result<()> {
        if self.nodes.iter().any(|existing| existing.id == node.id) {
            bail!("duplicate graph node {:?}", node.id);
        }
        self.nodes.push(node);
        Ok(())
    }

    pub fn add_edge(&mut self, edge: EdgeSpec) {
        self.edges.push(edge);
    }

    /// Seed one named input port with a typed program literal. This uses the
    /// same canonical schema validation as an edge payload.
    pub fn add_initial_input(
        &mut self,
        node: impl Into<String>,
        port: impl Into<String>,
        value: Value,
    ) -> Result<()> {
        let node = node.into();
        let port = port.into();
        let target = self
            .nodes
            .iter()
            .find(|candidate| candidate.id == node)
            .ok_or_else(|| anyhow!("initial input names unknown node {node:?}"))?;
        let schema = target
            .inputs
            .get(&port)
            .ok_or_else(|| anyhow!("initial input names unknown port {node}.{port}"))?;
        schema.schema.validate_value(&value)?;
        if self
            .initial_inputs
            .entry(node.clone())
            .or_default()
            .insert(port.clone(), value)
            .is_some()
        {
            bail!("initial input {node}.{port} was declared more than once");
        }
        Ok(())
    }

    pub fn validate(&self) -> Result<()> {
        let diagnostics = self.diagnostics();
        if diagnostics.is_empty() {
            Ok(())
        } else {
            Err(anyhow!(
                "graph {:?} is invalid: {}",
                self.name,
                diagnostics
                    .iter()
                    .map(ToString::to_string)
                    .collect::<Vec<_>>()
                    .join("; ")
            ))
        }
    }

    /// Validate node applet calls against their manifests after topology has
    /// supplied any required dynamic parameter ports.
    pub fn validate_with_registry(&self, registry: &Registry) -> Result<()> {
        self.validate()?;
        let sensitivity_labels = self.effective_sensitivity_labels(registry)?;
        for node in &self.nodes {
            for source_call in node.invocations() {
                let mut call = source_call.clone();
                if call.direct {
                    bail!(
                        "graph node {:?} uses a direct executable; graph nodes require named applets",
                        node.id
                    );
                }
                if call.parameters.values().any(contains_reference) {
                    bail!(
                        "graph node {:?} has an unresolved reference in `with`; use `bind` for graph inputs",
                        node.id
                    );
                }
                if matches!(&node.kind, GraphNodeKind::Applet { .. }) {
                    if !node.events.is_empty() {
                        bail!(
                            "graph applet node {:?} declares event outputs, but generic one-shot applets cannot publish backend events",
                            node.id
                        );
                    }
                    let stream_inputs = node
                        .inputs
                        .iter()
                        .filter(|(name, port)| {
                            port.delivery != PortDelivery::Single
                                && self.edges.iter().any(|edge| {
                                    edge.to.node == node.id
                                        && edge.to.port.as_str() == name.as_str()
                                })
                        })
                        .map(|(name, _)| name.as_str())
                        .collect::<Vec<_>>();
                    if stream_inputs.len() > 1 {
                        bail!(
                            "graph node {:?} has multiple streaming inputs {stream_inputs:?}; one applet invocation is triggered by one merged records/events port",
                            node.id
                        );
                    }
                    if !stream_inputs.is_empty() {
                        let single_outputs = node
                            .outputs
                            .iter()
                            .filter(|(_, port)| port.delivery == PortDelivery::Single)
                            .map(|(name, _)| name.as_str())
                            .collect::<Vec<_>>();
                        if !single_outputs.is_empty() {
                            bail!(
                                "graph applet node {:?} is triggered by streaming input {:?} but declares single-valued outputs {:?}; use records outputs for repeated invocations",
                                node.id,
                                stream_inputs,
                                single_outputs
                            );
                        }
                    }
                    for (name, port) in &node.inputs {
                        if name == "stdin" || call.parameters.contains_key(name) {
                            continue;
                        }
                        let bound = self
                            .edges
                            .iter()
                            .any(|edge| edge.to.node == node.id && edge.to.port == *name);
                        if bound {
                            call.parameters
                                .insert(name.clone(), placeholder_value(&port.schema)?);
                        }
                    }
                }
                let manifest = registry
                    .resolve_call(&call)
                    .map_err(|error| anyhow!("node {:?}: {error:#}", node.id))?;
                manifest
                    .validate(&call)
                    .map_err(|error| anyhow!("node {:?}: {error:#}", node.id))?;

                if let GraphNodeKind::Listener { source, .. } = &node.kind {
                    if !node.inputs.is_empty() || !node.outputs.is_empty() {
                        bail!("listener node {:?} may declare only event outputs", node.id);
                    }
                    let expected_events = match source {
                        crate::ast::ListenSource::Stdin => BTreeMap::from([(
                            "line".to_owned(),
                            PortSchema::events(ValueSchema {
                                kind: OutputType::Bytes,
                                nullable: false,
                                items: None,
                                fields: BTreeMap::new(),
                                required: Vec::new(),
                            }),
                        )]),
                        crate::ast::ListenSource::Applet(_)
                            if manifest.execution_backend()
                                == crate::contracts::AppletBackend::Remote =>
                        {
                            if !call.parameters.is_empty() {
                                bail!(
                                    "remote event listener {:?} cannot pass parameters through the GET event protocol",
                                    node.id
                                );
                            }
                            manifest
                                .ports()
                                .map(|ports| {
                                    ports
                                        .events
                                        .iter()
                                        .map(|(name, port)| {
                                            (name.clone(), PortSchema::from_applet(port, false))
                                        })
                                        .collect()
                                })
                                .unwrap_or_default()
                        }
                        crate::ast::ListenSource::Applet(_) => BTreeMap::from([(
                            "line".to_owned(),
                            PortSchema::events(ValueSchema {
                                kind: OutputType::Bytes,
                                nullable: false,
                                items: None,
                                fields: BTreeMap::new(),
                                required: Vec::new(),
                            }),
                        )]),
                    };
                    if node.events != expected_events {
                        bail!(
                            "listener node {:?} event ports do not match its source contract",
                            node.id
                        );
                    }
                }
            }

            for call in node.invocations() {
                let manifest = registry
                    .resolve_call(call)
                    .map_err(|error| anyhow!("node {:?}: {error:#}", node.id))?;
                let is_remote_event_source = matches!(
                    &node.kind,
                    GraphNodeKind::Listener {
                        source: crate::ast::ListenSource::Applet(_),
                        ..
                    }
                ) && manifest.execution_backend()
                    == crate::contracts::AppletBackend::Remote;
                if let Some(remote) = manifest
                    .execution_contract()
                    .and_then(|contract| contract.remote.as_ref())
                {
                    if is_remote_event_source {
                        // Event GETs carry no caller parameters in this graph
                        // protocol, but a configured bearer credential is
                        // still secret data sent to the remote origin.
                        if remote.auth.is_some() {
                            let sensitivities = SensitivitySet::from_label(Sensitivity::Secret);
                            crate::security::authorize_data_flow_set(
                                registry,
                                &manifest.name,
                                &remote.endpoint,
                                &sensitivities,
                            )
                            .map_err(|error| {
                                anyhow!(
                                    "graph authentication data flow into remote listener {:?} was denied: {error}",
                                    node.id
                                )
                            })?;
                        }
                    } else {
                        let mut incoming = None;
                        for edge in self.edges.iter().filter(|edge| edge.to.node == node.id) {
                            if let Some(labels) = sensitivity_labels
                                .get(&(edge.from.node.clone(), edge.from.port.clone()))
                            {
                                extend_sensitivity_set(&mut incoming, labels);
                            }
                            if let Some(port) = node.inputs.get(&edge.to.port) {
                                insert_sensitivity(&mut incoming, port.sensitivity);
                            }
                        }
                        // Literal parameters can carry contract labels too. They
                        // are data entering the remote invocation just as an edge
                        // is; omitting them would let a public edge mask an
                        // unknown or secret literal parameter.
                        for name in call.parameters.keys() {
                            if let Some(port) = node.inputs.get(name) {
                                insert_sensitivity(&mut incoming, port.sensitivity);
                            }
                        }
                        for name in self
                            .initial_inputs
                            .get(&node.id)
                            .into_iter()
                            .flat_map(|values| values.keys())
                        {
                            if let Some(port) = node.inputs.get(name) {
                                insert_sensitivity(&mut incoming, port.sensitivity);
                            }
                        }
                        if crate::security::applet_has_declared_secret_environment(
                            registry,
                            &manifest.name,
                        ) {
                            // A remote applet may itself read a declared secret
                            // environment variable and include it in the request,
                            // even when its graph ports are public.
                            insert_sensitivity(&mut incoming, Sensitivity::Secret);
                        }
                        let sensitivities = incoming.unwrap_or_default();
                        crate::security::authorize_data_flow_set(
                            registry,
                            &manifest.name,
                            &remote.endpoint,
                            &sensitivities,
                        )
                        .map_err(|error| {
                            anyhow!(
                                "graph data flow into remote node {:?} was denied: {error}",
                                node.id
                            )
                        })?;
                    }
                }
            }

            if let GraphNodeKind::NativeStream { source, calls } = &node.kind {
                let stream = crate::stream::parse(source).map_err(|error| {
                    anyhow!("node {:?}: invalid native stream: {error:#}", node.id)
                })?;
                crate::stream::validate(&stream, registry).map_err(|error| {
                    anyhow!("node {:?}: invalid native stream: {error:#}", node.id)
                })?;
                let parsed_calls = stream
                    .pipeline
                    .steps
                    .iter()
                    .filter_map(|step| match &step.kind {
                        crate::ast::StepKind::Invoke { call, .. } => Some(call.clone()),
                        _ => None,
                    })
                    .collect::<Vec<_>>();
                if &parsed_calls != calls {
                    bail!(
                        "node {:?}: native stream source and declared invocations disagree",
                        node.id
                    );
                }
            }
        }
        Ok(())
    }

    /// Conservative label sets after declared output sensitivity and every
    /// upstream value have been unioned. No label can mask another.
    pub fn effective_sensitivity_labels(
        &self,
        registry: &Registry,
    ) -> Result<BTreeMap<(String, String), SensitivitySet>> {
        let mut indegree = self
            .nodes
            .iter()
            .map(|node| (node.id.clone(), 0usize))
            .collect::<BTreeMap<_, _>>();
        let mut adjacency = BTreeMap::<String, Vec<String>>::new();
        for edge in &self.edges {
            if indegree.contains_key(&edge.from.node) && indegree.contains_key(&edge.to.node) {
                *indegree.entry(edge.to.node.clone()).or_default() += 1;
                adjacency
                    .entry(edge.from.node.clone())
                    .or_default()
                    .push(edge.to.node.clone());
            }
        }
        let mut ready = indegree
            .iter()
            .filter_map(|(id, count)| (*count == 0).then_some(id.clone()))
            .collect::<VecDeque<_>>();
        let mut labels = BTreeMap::new();
        while let Some(id) = ready.pop_front() {
            let Some(node) = self.nodes.iter().find(|node| node.id == id) else {
                continue;
            };
            let mut incoming = None;
            for edge in self.edges.iter().filter(|edge| edge.to.node == id) {
                if let Some(source_labels) =
                    labels.get(&(edge.from.node.clone(), edge.from.port.clone()))
                {
                    extend_sensitivity_set(&mut incoming, source_labels);
                }
                if let Some(port) = node.inputs.get(&edge.to.port) {
                    insert_sensitivity(&mut incoming, port.sensitivity);
                }
            }
            if let GraphNodeKind::Applet { call } = &node.kind {
                for name in call.parameters.keys() {
                    if !self
                        .edges
                        .iter()
                        .any(|edge| edge.to.node == id && edge.to.port == *name)
                    {
                        if let Some(port) = node.inputs.get(name) {
                            insert_sensitivity(&mut incoming, port.sensitivity);
                        }
                    }
                }
            }
            if let Some(values) = self.initial_inputs.get(&id) {
                for name in values.keys() {
                    if let Some(port) = node.inputs.get(name) {
                        insert_sensitivity(&mut incoming, port.sensitivity);
                    }
                }
            }
            for call in node.invocations() {
                let manifest = registry.resolve_call(call)?;
                if crate::security::applet_has_declared_secret_environment(registry, &manifest.name)
                {
                    insert_sensitivity(&mut incoming, Sensitivity::Secret);
                }
            }
            for (name, port) in node.outputs.iter().chain(node.events.iter()) {
                let mut sensitivity = incoming
                    .clone()
                    .unwrap_or_else(|| SensitivitySet::from_label(port.sensitivity));
                sensitivity.insert(port.sensitivity);
                labels.insert((id.clone(), name.clone()), sensitivity);
            }
            for target in adjacency.get(&id).into_iter().flatten() {
                if let Some(count) = indegree.get_mut(target) {
                    *count = count.saturating_sub(1);
                    if *count == 0 {
                        ready.push_back(target.clone());
                    }
                }
            }
        }
        Ok(labels)
    }

    pub fn diagnostics(&self) -> Vec<GraphDiagnostic> {
        let mut issues = Vec::new();
        if !is_identifier(&self.name) {
            issues.push(GraphDiagnostic {
                code: "invalid_graph_name",
                message: format!("invalid graph name {:?}", self.name),
                line: None,
            });
        }
        let mut node_map = BTreeMap::new();
        for (index, node) in self.nodes.iter().enumerate() {
            if !is_identifier(&node.id) {
                issues.push(GraphDiagnostic {
                    code: "invalid_node_name",
                    message: format!("invalid node name {:?}", node.id),
                    line: node.line,
                });
            }
            if node_map.insert(node.id.as_str(), index).is_some() {
                issues.push(GraphDiagnostic {
                    code: "duplicate_node",
                    message: format!("duplicate node {:?}", node.id),
                    line: node.line,
                });
            }
            validate_port_names(node, &mut issues);
            if let GraphNodeKind::Timer { interval_ms } = &node.kind {
                if *interval_ms == 0 {
                    issues.push(GraphDiagnostic {
                        code: "invalid_timer_interval",
                        message: format!("timer node {:?} interval must be positive", node.id),
                        line: node.line,
                    });
                }
            }
        }

        let mut incoming: BTreeMap<(String, String), usize> = BTreeMap::new();
        let mut seen_edges = BTreeSet::new();
        for edge in &self.edges {
            if edge.capacity == 0 {
                issues.push(GraphDiagnostic {
                    code: "invalid_edge_capacity",
                    message: "edge capacity must be positive".into(),
                    line: edge.line,
                });
                continue;
            }
            let key = (
                edge.from.node.clone(),
                edge.from.port.clone(),
                edge.to.node.clone(),
                edge.to.port.clone(),
            );
            if !seen_edges.insert(key) {
                issues.push(GraphDiagnostic {
                    code: "duplicate_edge",
                    message: format!(
                        "duplicate edge {}.{} to {}.{}",
                        edge.from.node, edge.from.port, edge.to.node, edge.to.port
                    ),
                    line: edge.line,
                });
            }
            let Some(&source_index) = node_map.get(edge.from.node.as_str()) else {
                issues.push(GraphDiagnostic {
                    code: "unknown_source_node",
                    message: format!("unknown source node {:?}", edge.from.node),
                    line: edge.line,
                });
                continue;
            };
            let Some(&target_index) = node_map.get(edge.to.node.as_str()) else {
                issues.push(GraphDiagnostic {
                    code: "unknown_target_node",
                    message: format!("unknown target node {:?}", edge.to.node),
                    line: edge.line,
                });
                continue;
            };
            let source = &self.nodes[source_index];
            let target = &self.nodes[target_index];
            let Some((source_direction, source_port)) = source.source_port(&edge.from.port) else {
                issues.push(GraphDiagnostic {
                    code: "unknown_source_port",
                    message: format!(
                        "node {:?} has no output or event port {:?}",
                        source.id, edge.from.port
                    ),
                    line: edge.line,
                });
                continue;
            };
            let Some(target_port) = target.inputs.get(&edge.to.port) else {
                issues.push(GraphDiagnostic {
                    code: "unknown_target_port",
                    message: format!("node {:?} has no input port {:?}", target.id, edge.to.port),
                    line: edge.line,
                });
                continue;
            };
            *incoming
                .entry((edge.to.node.clone(), edge.to.port.clone()))
                .or_default() += 1;
            if !schemas_assignable(&source_port.schema, &target_port.schema) {
                issues.push(GraphDiagnostic {
                    code: "port_schema_mismatch",
                    message: format!(
                        "{}.{} schema {:?} is incompatible with {}.{} schema {:?}",
                        source.id,
                        edge.from.port,
                        source_port.schema,
                        target.id,
                        edge.to.port,
                        target_port.schema
                    ),
                    line: edge.line,
                });
            }
            if !delivery_compatible(source_direction, source_port.delivery, target_port.delivery) {
                issues.push(GraphDiagnostic {
                    code: "port_delivery_mismatch",
                    message: format!(
                        "{}.{} delivery {:?} cannot connect to {}.{} delivery {:?}",
                        source.id,
                        edge.from.port,
                        source_port.delivery,
                        target.id,
                        edge.to.port,
                        target_port.delivery
                    ),
                    line: edge.line,
                });
            }
        }

        // There is not yet a source-level seed operation for feedback edges.
        // Reject cycles during preflight instead of starting workers that can
        // all wait forever on empty lossless queues.
        let mut indegree = node_map
            .keys()
            .map(|name| (*name, 0usize))
            .collect::<BTreeMap<_, _>>();
        let mut adjacency = BTreeMap::<&str, Vec<&str>>::new();
        for edge in &self.edges {
            if node_map.contains_key(edge.from.node.as_str())
                && node_map.contains_key(edge.to.node.as_str())
            {
                adjacency
                    .entry(edge.from.node.as_str())
                    .or_default()
                    .push(edge.to.node.as_str());
                *indegree.entry(edge.to.node.as_str()).or_default() += 1;
            }
        }
        let mut ready = indegree
            .iter()
            .filter_map(|(node, degree)| (*degree == 0).then_some(*node))
            .collect::<VecDeque<_>>();
        let mut visited = 0usize;
        while let Some(node) = ready.pop_front() {
            visited += 1;
            for target in adjacency.get(node).into_iter().flatten() {
                if let Some(degree) = indegree.get_mut(target) {
                    *degree -= 1;
                    if *degree == 0 {
                        ready.push_back(target);
                    }
                }
            }
        }
        if visited < node_map.len() {
            issues.push(GraphDiagnostic {
                code: "graph_cycle_requires_seed",
                message: "feedback cycles require an explicit seed operation, which this graph syntax does not yet provide".into(),
                line: None,
            });
        }

        for node in &self.nodes {
            for (name, port) in &node.inputs {
                let bound = incoming
                    .get(&(node.id.clone(), name.clone()))
                    .copied()
                    .unwrap_or(0);
                if port.required && bound == 0 && !node_has_literal_input(node, name) {
                    let has_initial = self
                        .initial_inputs
                        .get(&node.id)
                        .is_some_and(|values| values.contains_key(name));
                    if has_initial {
                        continue;
                    }
                    issues.push(GraphDiagnostic {
                        code: "required_port_unbound",
                        message: format!("required input {}.{} is not bound", node.id, name),
                        line: node.line,
                    });
                }
                if port.delivery == PortDelivery::Single && bound > 1 {
                    issues.push(GraphDiagnostic {
                        code: "single_port_multiple_producers",
                        message: format!(
                            "single input {}.{} has {bound} incoming edges; declare it as records to merge",
                            node.id, name
                        ),
                        line: node.line,
                    });
                }
                let has_initial = self
                    .initial_inputs
                    .get(&node.id)
                    .is_some_and(|values| values.contains_key(name));
                if bound > 0 && (node_has_literal_input(node, name) || has_initial) {
                    issues.push(GraphDiagnostic {
                        code: "input_has_literal_and_edge",
                        message: format!(
                            "input {}.{} is supplied both by `with` and by a graph edge",
                            node.id, name
                        ),
                        line: node.line,
                    });
                }
            }
        }

        for (node_id, values) in &self.initial_inputs {
            let Some(node) = node_map
                .get(node_id.as_str())
                .map(|index| &self.nodes[*index])
            else {
                issues.push(GraphDiagnostic {
                    code: "initial_input_unknown_node",
                    message: format!("initial input names unknown node {node_id:?}"),
                    line: None,
                });
                continue;
            };
            for (name, value) in values {
                match node.inputs.get(name) {
                    Some(port) => {
                        if let Err(error) = port.schema.validate_value(value) {
                            issues.push(GraphDiagnostic {
                                code: "initial_input_schema_mismatch",
                                message: format!("initial input {}.{}: {error}", node.id, name),
                                line: node.line,
                            });
                        }
                    }
                    None => issues.push(GraphDiagnostic {
                        code: "initial_input_unknown_port",
                        message: format!("initial input names unknown port {}.{}", node.id, name),
                        line: node.line,
                    }),
                }
            }
        }

        issues
    }

    pub fn output_terminals(&self) -> Vec<PortRef> {
        let bound = self
            .edges
            .iter()
            .map(|edge| (edge.from.node.as_str(), edge.from.port.as_str()))
            .collect::<BTreeSet<_>>();
        let mut terminals = Vec::new();
        for node in &self.nodes {
            for port in node.outputs.keys().chain(node.events.keys()) {
                if !bound.contains(&(node.id.as_str(), port.as_str())) {
                    terminals.push(PortRef::new(node.id.clone(), port.clone()));
                }
            }
        }
        terminals
    }
}

fn dot_escape(text: &str) -> String {
    text.replace('\\', "\\\\")
        .replace('"', "\\\"")
        .replace('\n', "\\n")
}

fn node_kind_name(kind: &GraphNodeKind) -> &'static str {
    match kind {
        GraphNodeKind::Applet { .. } => "applet/oneshot",
        GraphNodeKind::Timer { .. } => "timer/persistent",
        GraphNodeKind::Listener { .. } => "listener/persistent",
        GraphNodeKind::NativeStream { .. } => "native-stream/persistent",
        GraphNodeKind::Renderer { .. } => "renderer/persistent",
    }
}

fn insert_sensitivity(set: &mut Option<SensitivitySet>, label: Sensitivity) {
    match set {
        Some(set) => set.insert(label),
        None => *set = Some(SensitivitySet::from_label(label)),
    }
}

fn extend_sensitivity_set(target: &mut Option<SensitivitySet>, labels: &SensitivitySet) {
    match target {
        Some(target) => target.extend(labels),
        None => *target = Some(labels.clone()),
    }
}

fn sensitivity_name(sensitivity: Sensitivity) -> &'static str {
    match sensitivity {
        Sensitivity::Public => "public",
        Sensitivity::Internal => "internal",
        Sensitivity::Confidential => "confidential",
        Sensitivity::Secret => "secret",
        Sensitivity::Unknown => "unknown",
    }
}

fn validate_port_names(node: &GraphNode, issues: &mut Vec<GraphDiagnostic>) {
    for (direction, ports) in [
        ("input", &node.inputs),
        ("output", &node.outputs),
        ("event", &node.events),
    ] {
        for (name, port) in ports {
            if !is_identifier(name) {
                issues.push(GraphDiagnostic {
                    code: "invalid_port_name",
                    message: format!("invalid {direction} port name {:?}", name),
                    line: node.line,
                });
            }
            if let Err(error) = validate_schema(&port.schema, false) {
                issues.push(GraphDiagnostic {
                    code: "invalid_port_schema",
                    message: format!("{}.{}: {error}", node.id, name),
                    line: node.line,
                });
            }
            match (direction, port.delivery) {
                ("event", PortDelivery::Events) => {}
                ("event", _) => issues.push(GraphDiagnostic {
                    code: "invalid_event_delivery",
                    message: format!("event port {}.{} must use events delivery", node.id, name),
                    line: node.line,
                }),
                ("input", PortDelivery::Events) => {}
                ("input", _) => {}
                ("output", PortDelivery::Events) => issues.push(GraphDiagnostic {
                    code: "invalid_output_delivery",
                    message: format!(
                        "{} has event delivery; declare it in the events namespace",
                        name
                    ),
                    line: node.line,
                }),
                _ => {}
            }
        }
    }
    for name in node.outputs.keys() {
        if node.events.contains_key(name) {
            issues.push(GraphDiagnostic {
                code: "ambiguous_port_name",
                message: format!(
                    "node {:?} uses {:?} for both an output and event port",
                    node.id, name
                ),
                line: node.line,
            });
        }
    }
}

fn validate_schema(schema: &ValueSchema, nested: bool) -> Result<()> {
    schema.validate()?;
    if schema.kind == OutputType::Bytes && nested {
        bail!("bytes cannot be nested in records or lists");
    }
    if schema.kind == OutputType::Record {
        if schema.items.is_some() {
            bail!("record schema cannot declare list items");
        }
    } else if schema.kind == OutputType::List {
        if !schema.fields.is_empty() || !schema.required.is_empty() {
            bail!("list schema cannot declare record fields");
        }
        let item_schema = schema
            .items
            .as_deref()
            .ok_or_else(|| anyhow!("list schema must declare item type"))?;
        validate_schema(item_schema, true)?;
    } else if schema.items.is_some() || !schema.fields.is_empty() || !schema.required.is_empty() {
        bail!("items apply only to lists; fields and required apply only to records");
    }
    let mut required = BTreeSet::new();
    for name in &schema.required {
        if !schema.fields.contains_key(name) || !required.insert(name) {
            bail!("record required fields must be declared once in fields");
        }
    }
    for (name, field) in &schema.fields {
        if name.is_empty() {
            bail!("record field names must be nonempty");
        }
        validate_schema(field, true)?;
    }
    Ok(())
}

fn schemas_assignable(source: &ValueSchema, target: &ValueSchema) -> bool {
    if source.kind != target.kind
        || (source.kind != OutputType::Json && source.nullable && !target.nullable)
    {
        return false;
    }
    match source.kind {
        OutputType::Record => {
            if target
                .required
                .iter()
                .any(|name| !source.required.contains(name))
            {
                return false;
            }
            source.fields.iter().all(|(name, source_field)| {
                target
                    .fields
                    .get(name)
                    .is_some_and(|target_field| schemas_assignable(source_field, target_field))
            })
        }
        OutputType::List => match (source.items.as_deref(), target.items.as_deref()) {
            (Some(source), Some(target)) => schemas_assignable(source, target),
            _ => false,
        },
        _ => true,
    }
}

fn delivery_compatible(
    source_direction: PortDirection,
    source: PortDelivery,
    target: PortDelivery,
) -> bool {
    match source_direction {
        PortDirection::Event => source == PortDelivery::Events && target == PortDelivery::Events,
        PortDirection::Output => matches!(
            (source, target),
            (
                PortDelivery::Single,
                PortDelivery::Single | PortDelivery::Records
            ) | (PortDelivery::Records, PortDelivery::Records)
        ),
        PortDirection::Input => false,
    }
}

fn node_has_literal_input(node: &GraphNode, name: &str) -> bool {
    match &node.kind {
        GraphNodeKind::Applet { call } => call.parameters.contains_key(name),
        _ => false,
    }
}

fn contains_reference(value: &Value) -> bool {
    match value {
        Value::Reference(_) => true,
        Value::List(values) => values.iter().any(contains_reference),
        Value::Record(values) => values.values().any(contains_reference),
        _ => false,
    }
}

fn placeholder_value(schema: &ValueSchema) -> Result<Value> {
    Ok(match schema.kind {
        OutputType::Bytes => bail!("bytes cannot supply a named applet parameter"),
        OutputType::Text | OutputType::Path => Value::String("__graph_preflight__".into()),
        OutputType::Integer => Value::Integer(0),
        OutputType::Boolean => Value::Bool(false),
        OutputType::Json => Value::Null,
        OutputType::Record => {
            let mut fields = BTreeMap::new();
            for name in &schema.required {
                let field = schema
                    .fields
                    .get(name)
                    .ok_or_else(|| anyhow!("required record field {name:?} is not declared"))?;
                fields.insert(name.clone(), placeholder_value(field)?);
            }
            Value::Record(fields)
        }
        OutputType::List => Value::List(Vec::new()),
    })
}

fn is_identifier(name: &str) -> bool {
    let mut chars = name.chars();
    matches!(chars.next(), Some(c) if c == '_' || c.is_ascii_alphabetic())
        && chars.all(|c| c == '_' || c.is_ascii_alphanumeric() || c == '-')
}

/// Validate a payload against the exact declared graph type before transport.
pub(crate) fn validate_payload(schema: &ValueSchema, payload: &PortValue) -> Result<()> {
    if schema.nullable && matches!(payload, PortValue::Value(Value::Null)) {
        return Ok(());
    }
    match (schema.kind, payload) {
        (OutputType::Bytes, PortValue::Bytes(_)) => Ok(()),
        (OutputType::Text | OutputType::Path, PortValue::Value(Value::String(value))) => {
            if schema.kind == OutputType::Path && (value.is_empty() || value.contains('\0')) {
                bail!("path payload must be nonempty and contain no NUL byte");
            }
            Ok(())
        }
        (OutputType::Integer, PortValue::Value(Value::Integer(_)))
        | (OutputType::Boolean, PortValue::Value(Value::Bool(_)))
        | (OutputType::Json, PortValue::Value(_)) => Ok(()),
        (OutputType::Record, PortValue::Value(Value::Record(fields))) => {
            validate_record(&schema.fields, &schema.required, fields)
        }
        (OutputType::List, PortValue::Value(Value::List(values))) => {
            let item_schema = schema
                .items
                .as_deref()
                .ok_or_else(|| anyhow!("list schema must declare item type"))?;
            for (index, value) in values.iter().enumerate() {
                validate_value(item_schema, value)
                    .map_err(|error| anyhow!("list item {index}: {error}"))?;
            }
            Ok(())
        }
        (expected, actual) => bail!(
            "expected {expected:?} payload, received {}",
            actual.kind_name()
        ),
    }
}

fn validate_record(
    schema: &BTreeMap<String, ValueSchema>,
    required: &[String],
    fields: &BTreeMap<String, Value>,
) -> Result<()> {
    for name in required {
        if !fields.contains_key(name) {
            bail!("record is missing required field {name:?}");
        }
    }
    for (name, value) in fields {
        let Some(field_schema) = schema.get(name) else {
            bail!("record contains undeclared field {name:?}");
        };
        validate_value(field_schema, value).map_err(|error| anyhow!("field {name:?}: {error}"))?;
    }
    Ok(())
}

fn validate_value(schema: &ValueSchema, value: &Value) -> Result<()> {
    if schema.nullable && matches!(value, Value::Null) {
        return Ok(());
    }
    match (schema.kind, value) {
        (OutputType::Text | OutputType::Path, Value::String(_))
        | (OutputType::Integer, Value::Integer(_))
        | (OutputType::Boolean, Value::Bool(_))
        | (OutputType::Json, _) => Ok(()),
        (OutputType::Record, Value::Record(fields)) => {
            validate_record(&schema.fields, &schema.required, fields)
        }
        (OutputType::List, Value::List(values)) => {
            let item_schema = schema
                .items
                .as_deref()
                .ok_or_else(|| anyhow!("list schema must declare item type"))?;
            for (index, value) in values.iter().enumerate() {
                validate_value(item_schema, value)
                    .map_err(|error| anyhow!("list item {index}: {error}"))?;
            }
            Ok(())
        }
        (expected, actual) => bail!(
            "expected {expected:?}, received {}",
            value_kind_name(actual)
        ),
    }
}

fn value_kind_name(value: &Value) -> &'static str {
    match value {
        Value::String(_) => "string",
        Value::Integer(_) => "integer",
        Value::Bool(_) => "boolean",
        Value::List(_) => "list",
        Value::Record(_) => "record",
        Value::Null => "null",
        Value::Number(_) => "number",
        Value::Reference(_) => "unresolved reference",
    }
}

/// Typed runtime payload. Bytes remain byte vectors; all other values use the
/// existing AST value model without JSON/string coercion.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(tag = "kind", content = "value", rename_all = "snake_case")]
pub enum PortValue {
    Bytes(Vec<u8>),
    Value(Value),
}

impl PortValue {
    fn kind_name(&self) -> &'static str {
        match self {
            PortValue::Bytes(_) => "bytes",
            PortValue::Value(value) => value_kind_name(value),
        }
    }
}
