use crate::ast::{InputDeclaration, InputType, Invocation, Value};
use crate::contracts::{
    AppletPort, OutputEncoding, OutputSchema, OutputType, PortDelivery, Sensitivity, ValueSchema,
};
use crate::manifest::Registry;
use crate::security::SensitivitySet;
use anyhow::{anyhow, bail, Result};
use serde::{Deserialize, Serialize};
use std::collections::{BTreeMap, BTreeSet, HashSet, VecDeque};

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
        minimum: None,
        pattern: None,
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
    /// A finite applet invocation whose canonical result is published as one
    /// graph event. This is an explicit graph projection, not a backend event
    /// lifecycle; invocation and cancellation still use the ordinary finite
    /// applet path.
    AppletResultEvent {
        call: Invocation,
        event_port: String,
    },
    /// A persistent interface-2 `.program` module whose lifetime belongs to this graph.
    ProgramService {
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
            GraphNodeKind::Applet { call } | GraphNodeKind::AppletResultEvent { call, .. } => {
                vec![call]
            }
            GraphNodeKind::ProgramService { call } => vec![call],
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
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct GraphPlan {
    pub name: String,
    nodes: Vec<GraphNode>,
    edges: Vec<EdgeSpec>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    inputs: Vec<InputDeclaration>,
    #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
    initial_inputs: BTreeMap<String, BTreeMap<String, Value>>,
    #[serde(skip)]
    node_ids: HashSet<String>,
}

impl PartialEq for GraphPlan {
    fn eq(&self, other: &Self) -> bool {
        self.name == other.name
            && self.nodes == other.nodes
            && self.edges == other.edges
            && self.inputs == other.inputs
            && self.initial_inputs == other.initial_inputs
    }
}

impl GraphPlan {
    pub fn new(name: impl Into<String>) -> Self {
        Self {
            name: name.into(),
            nodes: Vec::new(),
            edges: Vec::new(),
            inputs: Vec::new(),
            initial_inputs: BTreeMap::new(),
            node_ids: HashSet::new(),
        }
    }

    pub fn nodes(&self) -> &[GraphNode] {
        &self.nodes
    }

    pub fn edges(&self) -> &[EdgeSpec] {
        &self.edges
    }

    /// The graph's ordinary top-level input interface.
    pub fn input_declarations(&self) -> &[InputDeclaration] {
        &self.inputs
    }

    /// Set the graph's top-level input interface after validating its schema.
    pub fn set_input_declarations(&mut self, inputs: Vec<InputDeclaration>) -> Result<()> {
        crate::inputs::validate(&inputs)?;
        self.inputs = inputs;
        Ok(())
    }

    /// Opt a finite applet node into emitting its single canonical result as
    /// one typed graph event. Registry-backed validation checks the selected
    /// event against the applet contract before execution.
    pub(super) fn project_result_event(
        &mut self,
        node_id: &str,
        event_port: &str,
        line: usize,
    ) -> Result<()> {
        let node = self
            .nodes
            .iter_mut()
            .find(|node| node.id == node_id)
            .ok_or_else(|| {
                anyhow!("line {line}: result projection names unknown node {node_id:?}")
            })?;
        let call = match &node.kind {
            GraphNodeKind::Applet { call } => call.clone(),
            GraphNodeKind::AppletResultEvent { .. } => {
                bail!("line {line}: node {node_id:?} already projects its result")
            }
            _ => bail!("line {line}: result projection requires a finite `run` applet node"),
        };
        if node.events.len() != 1
            || node
                .events
                .get(event_port)
                .is_none_or(|port| port.delivery != PortDelivery::Events)
        {
            bail!(
                "line {line}: node {node_id:?} must declare exactly one Events port named {event_port:?}"
            );
        }
        // `run` normally exposes a generated status output. Projection mode
        // suppresses that generated port, then rejects every remaining
        // ordinary output so no result or status path is ambiguous.
        node.outputs.remove("status");
        if !node.outputs.is_empty() {
            bail!("line {line}: result projection on {node_id:?} cannot mix ordinary output ports");
        }
        node.kind = GraphNodeKind::AppletResultEvent {
            call,
            event_port: event_port.to_owned(),
        };
        Ok(())
    }

    /// Return a plan with supplied input references replaced by typed values.
    /// Defaults are available in both modes. Partial mode leaves required
    /// inputs unresolved so static inspection can proceed; complete mode also
    /// reads declared environment inputs and requires every value.
    pub fn with_input_values(
        &self,
        supplied: &BTreeMap<String, Value>,
        require_complete: bool,
    ) -> Result<Self> {
        let values = if require_complete {
            crate::inputs::resolve(&self.inputs, supplied, false)?
        } else {
            crate::inputs::known_values(&self.inputs, supplied)?
        };
        let mut plan = self.clone();
        for node in &mut plan.nodes {
            match &mut node.kind {
                GraphNodeKind::Applet { call }
                | GraphNodeKind::AppletResultEvent { call, .. }
                | GraphNodeKind::ProgramService { call } => resolve_call_inputs(call, &values),
                GraphNodeKind::Listener {
                    source: crate::ast::ListenSource::Applet(call),
                    ..
                } => resolve_call_inputs(call, &values),
                GraphNodeKind::NativeStream { calls, .. }
                    if calls
                        .iter()
                        .any(|call| call.parameters.values().any(contains_reference)) =>
                {
                    bail!(
                        "graph inputs can be used only in `run` applet parameters; native stream parameters must be literal"
                    );
                }
                _ => {}
            }
        }
        Ok(plan)
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
        // Rebuild once for plans created by deserialization; subsequent inserts
        // use the index instead of scanning every node (#167).
        if self.node_ids.len() != self.nodes.len() {
            self.node_ids = self
                .nodes
                .iter()
                .map(|existing| existing.id.clone())
                .collect();
            if self.node_ids.len() != self.nodes.len() {
                bail!("graph plan contains duplicate node ids");
            }
        }
        if !self.node_ids.insert(node.id.clone()) {
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
        if let Some(node) = self
            .nodes
            .iter()
            .find(|node| node.invocations().iter().any(|call| call.discard_stderr))
        {
            bail!(
                "graph node {:?} does not support `and discard stderr`",
                node.id
            );
        }
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
    ///
    /// The default run limits are applied first, before any per-node work, so `check`
    /// refuses a graph over `max_nodes` or `max_edges` the way `run` does (#167).
    /// Embedders that build a plan in code and admit it with larger
    /// `GraphRunOptions` validate it with [`GraphPlan::validate`] instead.
    pub fn validate_with_registry(&self, registry: &Registry) -> Result<()> {
        let limits = super::transport::GraphRunOptions::default();
        if self.nodes.len() > limits.max_nodes {
            bail!(
                "graph node count {} exceeds max_nodes {}",
                self.nodes.len(),
                limits.max_nodes
            );
        }
        if self.edges.len() > limits.max_edges {
            bail!(
                "graph edge count {} exceeds max_edges {}",
                self.edges.len(),
                limits.max_edges
            );
        }
        self.validate()?;
        crate::inputs::validate(&self.inputs)?;
        let input_types = self
            .inputs
            .iter()
            .map(|input| (input.name.clone(), input.kind))
            .collect::<BTreeMap<_, _>>();
        let sensitivity_labels = self.effective_sensitivity_labels(registry)?;
        for node in &self.nodes {
            if matches!(&node.kind, GraphNodeKind::NativeStream { .. })
                && node
                    .invocations()
                    .iter()
                    .any(|call| call.parameters.values().any(contains_reference))
            {
                bail!(
                    "graph inputs can be used only in `run` applet parameters; native stream parameters must be literal"
                );
            }
            for source_call in node.invocations() {
                let mut call = source_call.clone();
                if call.direct {
                    bail!(
                        "graph node {:?} uses a direct executable; graph nodes require named applets",
                        node.id
                    );
                }
                validate_input_references(&call, &input_types, &node.id)?;
                if matches!(
                    &node.kind,
                    GraphNodeKind::Applet { .. }
                        | GraphNodeKind::AppletResultEvent { .. }
                        | GraphNodeKind::ProgramService { .. }
                ) {
                    let is_program_service =
                        matches!(&node.kind, GraphNodeKind::ProgramService { .. });
                    let projects_result =
                        matches!(&node.kind, GraphNodeKind::AppletResultEvent { .. });
                    if !is_program_service && !projects_result && !node.events.is_empty() {
                        bail!(
                            "graph applet node {:?} declares event outputs, but generic one-shot applets cannot publish backend events",
                            node.id
                        );
                    }
                    if projects_result {
                        validate_result_event_node(node, &call, registry)?;
                    }
                    if is_program_service {
                        if !node.outputs.is_empty() {
                            bail!(
                                "graph program service node {:?} cannot declare finite output ports",
                                node.id
                            );
                        }
                        if node.events.len() != 1
                            || node
                                .events
                                .values()
                                .any(|port| port.delivery != PortDelivery::Events)
                        {
                            bail!(
                                "graph program service node {:?} must declare exactly one events output port",
                                node.id
                            );
                        }
                        if node
                            .inputs
                            .values()
                            .any(|port| port.delivery != PortDelivery::Single)
                        {
                            bail!(
                                "graph program service node {:?} accepts only static single startup inputs",
                                node.id
                            );
                        }
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
                    .validate_with_types(&call, &input_types)
                    .map_err(|error| anyhow!("node {:?}: {error:#}", node.id))?;

                match &node.kind {
                    GraphNodeKind::ProgramService { .. } => {
                        let contract = manifest.execution_contract().ok_or_else(|| {
                            anyhow!(
                                "graph program service node {:?} requires a versioned applet contract",
                                node.id
                            )
                        })?;
                        if manifest.execution_backend()
                            != crate::contracts::AppletBackend::ProgramModule
                            || contract.lifecycle != crate::contracts::Lifecycle::ProgramService
                        {
                            bail!(
                                "graph node {:?} is declared as a program service, but its applet contract is not an interface-2 `.program` service",
                                node.id
                            );
                        }
                        if !node.outputs.is_empty() {
                            bail!(
                                "graph program service node {:?} cannot declare finite output ports",
                                node.id
                            );
                        }
                        let ports = contract.ports.as_ref().ok_or_else(|| {
                            anyhow!(
                                "graph program service node {:?} has no declared typed event port",
                                node.id
                            )
                        })?;
                        let expected_events = ports
                            .events
                            .iter()
                            .map(|(name, port)| {
                                (name.clone(), PortSchema::from_applet(port, false))
                            })
                            .collect::<BTreeMap<_, _>>();
                        if expected_events.len() != 1 || node.events != expected_events {
                            bail!(
                                "graph program service node {:?} event ports must match its single typed event contract",
                                node.id
                            );
                        }
                        let manifest_contract = manifest.contract();
                        let required_inputs = manifest_contract["parameters_schema"]["required"]
                            .as_array()
                            .into_iter()
                            .flatten()
                            .filter_map(serde_json::Value::as_str)
                            .map(str::to_owned)
                            .collect::<BTreeSet<_>>();
                        let expected_inputs = ports
                            .inputs
                            .iter()
                            .map(|(name, port)| {
                                (
                                    name.clone(),
                                    PortSchema::from_applet(port, required_inputs.contains(name)),
                                )
                            })
                            .collect::<BTreeMap<_, _>>();
                        if node.inputs != expected_inputs
                            || node
                                .inputs
                                .values()
                                .any(|port| port.delivery != PortDelivery::Single)
                        {
                            bail!(
                                "graph program service node {:?} inputs must match its static single startup input contract",
                                node.id
                            );
                        }
                    }
                    GraphNodeKind::Applet { .. } | GraphNodeKind::AppletResultEvent { .. }
                        if manifest.execution_contract().is_some_and(|contract| {
                            contract.lifecycle == crate::contracts::Lifecycle::ProgramService
                        }) =>
                    {
                        bail!(
                            "persistent `.program` service {:?} must run as a graph program service node",
                            manifest.name
                        );
                    }
                    _ => {}
                }

                if let GraphNodeKind::Listener { source, .. } = &node.kind {
                    if !node.inputs.is_empty() || !node.outputs.is_empty() {
                        bail!("listener node {:?} may declare only event outputs", node.id);
                    }
                    let expected_events = match source {
                        crate::ast::ListenSource::Stdin => BTreeMap::from([(
                            "line".to_owned(),
                            PortSchema::events(ValueSchema {
                                scalar_only: false,
                                additional_properties: None,
                                min_properties: None,
                                max_properties: None,
                                nonempty: false,
                                kind: OutputType::Bytes,
                                nullable: false,
                                items: None,
                                fields: BTreeMap::new(),
                                required: Vec::new(),
                                enum_values: None,
                                single_line: false,
                                minimum: None,
                                pattern: None,
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
                                scalar_only: false,
                                additional_properties: None,
                                min_properties: None,
                                max_properties: None,
                                nonempty: false,
                                kind: OutputType::Bytes,
                                nullable: false,
                                items: None,
                                fields: BTreeMap::new(),
                                required: Vec::new(),
                                enum_values: None,
                                single_line: false,
                                minimum: None,
                                pattern: None,
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
        let nodes = self
            .nodes
            .iter()
            .map(|node| (node.id.clone(), node))
            .collect::<BTreeMap<_, _>>();
        let mut indegree = nodes
            .keys()
            .cloned()
            .map(|id| (id, 0usize))
            .collect::<BTreeMap<_, _>>();
        let mut adjacency = BTreeMap::<String, Vec<String>>::new();
        let mut incoming_edges = BTreeMap::<String, Vec<&EdgeSpec>>::new();
        let mut incoming_ports = BTreeMap::<String, BTreeSet<String>>::new();
        for edge in &self.edges {
            if indegree.contains_key(&edge.from.node) && indegree.contains_key(&edge.to.node) {
                *indegree.entry(edge.to.node.clone()).or_default() += 1;
                adjacency
                    .entry(edge.from.node.clone())
                    .or_default()
                    .push(edge.to.node.clone());
                incoming_edges
                    .entry(edge.to.node.clone())
                    .or_default()
                    .push(edge);
                incoming_ports
                    .entry(edge.to.node.clone())
                    .or_default()
                    .insert(edge.to.port.clone());
            }
        }
        let mut ready = indegree
            .iter()
            .filter_map(|(id, count)| (*count == 0).then_some(id.clone()))
            .collect::<VecDeque<_>>();
        let mut labels = BTreeMap::new();
        while let Some(id) = ready.pop_front() {
            let Some(node) = nodes.get(&id).copied() else {
                continue;
            };
            let mut incoming = None;
            for edge in incoming_edges.get(&id).into_iter().flatten() {
                if let Some(source_labels) =
                    labels.get(&(edge.from.node.clone(), edge.from.port.clone()))
                {
                    extend_sensitivity_set(&mut incoming, source_labels);
                }
                if let Some(port) = node.inputs.get(&edge.to.port) {
                    insert_sensitivity(&mut incoming, port.sensitivity);
                }
            }
            if let GraphNodeKind::Applet { call }
            | GraphNodeKind::AppletResultEvent { call, .. }
            | GraphNodeKind::ProgramService { call } = &node.kind
            {
                for name in call.parameters.keys() {
                    if !incoming_ports
                        .get(&id)
                        .is_some_and(|ports| ports.contains(name))
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

    /// Why this graph cannot be checkpointed, if it cannot. Phase 1 covers finite
    /// applet graphs whose nodes have at most one stream input each. A program
    /// module node must be preloaded with a complete checkpoint identity.
    pub fn checkpoint_refusal(
        &self,
        registry: &Registry,
        modules: &crate::modules::ProgramModuleSession,
    ) -> Result<()> {
        for node in &self.nodes {
            if let Some(call) = node.invocation() {
                let manifest = registry.resolve_call(call)?;
                if manifest.execution_backend() == crate::contracts::AppletBackend::ProgramModule {
                    if modules.get_for_manifest(registry, manifest).is_none() {
                        bail!(
                            "graph node {:?} calls program module {:?}, which is not preloaded",
                            node.id,
                            manifest.name
                        );
                    }
                    modules.validate_checkpoint_identity().map_err(|error| {
                        anyhow!(
                            "graph node {:?} calls program module {:?}, whose identity cannot be fully computed: {error:#}",
                            node.id,
                            manifest.name
                        )
                    })?;
                }
            }
            let source = match &node.kind {
                GraphNodeKind::Applet { .. } | GraphNodeKind::AppletResultEvent { .. } => None,
                GraphNodeKind::ProgramService { .. } => Some("a persistent program service"),
                GraphNodeKind::Timer { .. } => Some("a timer"),
                GraphNodeKind::Listener { .. } => Some("a listener"),
                GraphNodeKind::NativeStream { .. } => Some("a native stream"),
                GraphNodeKind::Renderer { .. } => Some("a renderer"),
            };
            if let Some(source) = source {
                bail!(
                    "graph node {:?} is {source}; checkpointed graphs support only finite applet nodes",
                    node.id
                );
            }
            let streams = node
                .inputs
                .values()
                .filter(|schema| schema.delivery != PortDelivery::Single)
                .count();
            if streams > 1 {
                bail!(
                    "graph node {:?} has {streams} stream inputs; checkpointed graphs allow at most one per node",
                    node.id
                );
            }
        }
        Ok(())
    }

    /// A fingerprint per node. It covers the node's definition without source
    /// lines, its incoming edges, the identity of the invocation it runs, its
    /// upstream fingerprints, and every preloaded program module's fingerprint. A
    /// completed record is reused only while it matches.
    pub fn checkpoint_fingerprints(
        &self,
        registry: &Registry,
        cwd: &std::path::Path,
        module_fingerprints: &BTreeMap<String, String>,
    ) -> Result<BTreeMap<String, String>> {
        let mut fingerprints = BTreeMap::<String, String>::new();
        while fingerprints.len() < self.nodes.len() {
            let before = fingerprints.len();
            for node in &self.nodes {
                if fingerprints.contains_key(&node.id) {
                    continue;
                }
                let incoming: Vec<&EdgeSpec> = self
                    .edges
                    .iter()
                    .filter(|edge| edge.to.node == node.id)
                    .collect();
                if incoming
                    .iter()
                    .any(|edge| !fingerprints.contains_key(&edge.from.node))
                {
                    continue;
                }
                let fingerprint = self.node_fingerprint(
                    node,
                    &incoming,
                    &fingerprints,
                    registry,
                    cwd,
                    module_fingerprints,
                )?;
                fingerprints.insert(node.id.clone(), fingerprint);
            }
            if fingerprints.len() == before {
                bail!("checkpointed graphs must be acyclic");
            }
        }
        Ok(fingerprints)
    }

    fn node_fingerprint(
        &self,
        node: &GraphNode,
        incoming: &[&EdgeSpec],
        fingerprints: &BTreeMap<String, String>,
        registry: &Registry,
        cwd: &std::path::Path,
        module_fingerprints: &BTreeMap<String, String>,
    ) -> Result<String> {
        let mut definition = node.clone();
        definition.line = None;
        let mut edges: Vec<serde_json::Value> = incoming
            .iter()
            .map(|edge| {
                serde_json::json!({
                    "from": edge.from,
                    "to": edge.to,
                    "capacity": edge.capacity,
                    "policy": edge.policy,
                    "upstream": fingerprints.get(&edge.from.node),
                })
            })
            .collect();
        edges.sort_by_key(|edge| edge.to_string());
        let identity = serde_json::to_vec(&serde_json::json!({
            "node": definition,
            "incoming": edges,
        }))?;
        crate::runtime::resume::graph_node_fingerprint(
            &identity,
            node.invocation(),
            registry,
            cwd,
            module_fingerprints,
        )
    }

    /// Whether an interrupted run of `node` may run again without a manual retry.
    /// The applet must be builtin or cacheable with declared read-only effects.
    pub fn checkpoint_retry_safe(&self, node: &GraphNode, registry: &Registry) -> Result<bool> {
        let Some(call) = node.invocation() else {
            return Ok(false);
        };
        let manifest = registry.resolve_call(call)?;
        Ok(manifest.builtin.is_some() || manifest.cacheable_with_read_only_effects())
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

fn validate_result_event_node(
    node: &GraphNode,
    call: &Invocation,
    registry: &Registry,
) -> Result<()> {
    let GraphNodeKind::AppletResultEvent { event_port, .. } = &node.kind else {
        unreachable!("result-event validation is called only for result projection nodes");
    };
    if call.cache || call.status_binding.is_some() || call.result_binding.is_some() {
        bail!(
            "graph result-event node {:?} cannot use cache, status, or result bindings",
            node.id
        );
    }
    if !node.outputs.is_empty() {
        bail!(
            "graph result-event node {:?} cannot declare ordinary output ports",
            node.id
        );
    }
    let manifest = registry
        .resolve_call(call)
        .map_err(|error| anyhow!("node {:?}: {error:#}", node.id))?;
    let contract = manifest.execution_contract().ok_or_else(|| {
        anyhow!(
            "graph result-event node {:?} requires an explicit applet port contract",
            node.id
        )
    })?;
    if contract.lifecycle == crate::contracts::Lifecycle::ProgramService {
        bail!(
            "persistent `.program` service {:?} must use its service event lifecycle",
            manifest.name
        );
    }
    let ports = contract.ports.as_ref().ok_or_else(|| {
        anyhow!(
            "graph result-event node {:?} requires a declared Events output port",
            node.id
        )
    })?;
    if !ports.outputs.is_empty() {
        bail!(
            "graph result-event node {:?} cannot mix declared ordinary outputs with a projected result",
            node.id
        );
    }
    let expected_events = ports
        .events
        .iter()
        .map(|(name, port)| (name.clone(), PortSchema::from_applet(port, false)))
        .collect::<BTreeMap<_, _>>();
    if expected_events.len() != 1
        || expected_events
            .get(event_port)
            .is_none_or(|port| port.delivery != PortDelivery::Events)
        || node.events != expected_events
    {
        bail!(
            "graph result-event node {:?} must select its single declared Events port",
            node.id
        );
    }
    let canonical_result_schema = if let Some(envelope) = contract.result_envelope_schema() {
        envelope
    } else if let Some(output) = contract
        .output
        .as_ref()
        .filter(|output| output.kind != OutputType::Bytes)
    {
        value_schema_from_output(output)
    } else {
        ValueSchema {
            scalar_only: false,
            additional_properties: None,
            min_properties: None,
            max_properties: None,
            nonempty: false,
            kind: OutputType::Bytes,
            nullable: false,
            items: None,
            fields: BTreeMap::new(),
            required: Vec::new(),
            enum_values: None,
            single_line: false,
            minimum: None,
            pattern: None,
        }
    };
    if node.events[event_port].schema != canonical_result_schema {
        bail!(
            "graph result-event port {}.{} schema must exactly match the applet's canonical result schema",
            node.id,
            event_port
        );
    }
    Ok(())
}

fn dot_escape(text: &str) -> String {
    text.replace('\\', "\\\\")
        .replace('"', "\\\"")
        .replace('\n', "\\n")
}

fn node_kind_name(kind: &GraphNodeKind) -> &'static str {
    match kind {
        GraphNodeKind::Applet { .. } => "applet/oneshot",
        GraphNodeKind::AppletResultEvent { .. } => "applet/result-event",
        GraphNodeKind::ProgramService { .. } => "program-service/persistent",
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
    source.is_assignable_to(target)
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
        GraphNodeKind::Applet { call }
        | GraphNodeKind::AppletResultEvent { call, .. }
        | GraphNodeKind::ProgramService { call } => call.parameters.contains_key(name),
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

fn resolve_call_inputs(call: &mut Invocation, values: &BTreeMap<String, Value>) {
    for value in call.parameters.values_mut() {
        *value = resolve_known_references(value, values);
    }
}

fn resolve_known_references(value: &Value, values: &BTreeMap<String, Value>) -> Value {
    match value {
        Value::Reference(name) => values.get(name).cloned().unwrap_or_else(|| value.clone()),
        Value::List(items) => Value::List(
            items
                .iter()
                .map(|item| resolve_known_references(item, values))
                .collect(),
        ),
        Value::Record(fields) => Value::Record(
            fields
                .iter()
                .map(|(name, value)| (name.clone(), resolve_known_references(value, values)))
                .collect(),
        ),
        _ => value.clone(),
    }
}

fn validate_input_references(
    call: &Invocation,
    input_types: &BTreeMap<String, InputType>,
    node: &str,
) -> Result<()> {
    fn visit(
        value: &Value,
        parameter: &str,
        input_types: &BTreeMap<String, InputType>,
        node: &str,
    ) -> Result<()> {
        match value {
            Value::Reference(name) if !input_types.contains_key(name) => bail!(
                "graph node {node:?} parameter {parameter:?} references undeclared graph input {name:?}"
            ),
            Value::List(items) => {
                for item in items {
                    visit(item, parameter, input_types, node)?;
                }
            }
            Value::Record(fields) => {
                for value in fields.values() {
                    visit(value, parameter, input_types, node)?;
                }
            }
            _ => {}
        }
        Ok(())
    }

    for (parameter, value) in &call.parameters {
        visit(value, parameter, input_types, node)?;
    }
    Ok(())
}

fn placeholder_value(schema: &ValueSchema) -> Result<Value> {
    Ok(match schema.kind {
        OutputType::Bytes => bail!("bytes cannot supply a named applet parameter"),
        OutputType::Text => Value::String(
            schema
                .enum_values
                .as_ref()
                .and_then(|values| values.first())
                .cloned()
                .unwrap_or_else(|| "__graph_preflight__".into()),
        ),
        OutputType::Path => Value::String("__graph_preflight__".into()),
        OutputType::Integer => Value::Integer(0),
        OutputType::Number => Value::Integer(0),
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
    match (schema.kind, payload) {
        (OutputType::Bytes, PortValue::Bytes(_)) => Ok(()),
        (_, PortValue::Value(value)) => schema.validate_value(value),
        (expected, actual) => bail!(
            "expected {expected:?} payload, received {}",
            actual.kind_name()
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
