use super::plan::{
    value_schema_from_output, BufferPolicy, EdgeSpec, GraphNode, GraphNodeKind, GraphPlan, PortRef,
    PortSchema,
};
use crate::ast::{Connection, Invocation, StepKind};
use crate::contracts::{
    Lifecycle, OutputEncoding, OutputType, PortDelivery, Sensitivity, ValueSchema,
};
use crate::manifest::{Manifest, Registry};
use crate::parser;
use anyhow::{anyhow, bail, Context, Result};
use std::collections::{BTreeMap, BTreeSet};

/// Return true when the first non-input source statement is a graph declaration.
pub fn is_graph(source: &str) -> bool {
    let Ok(mut lines) = parser::logical_lines(source) else {
        return false;
    };
    if parser::take_language_version(&mut lines).is_err() {
        return false;
    }
    for statement in lines {
        let line = statement.text.trim();
        if line.is_empty() || parser::is_input_header(line) {
            continue;
        }
        return line.starts_with("graph ");
    }
    false
}

/// Parse a readable graph block and resolve applet port contracts without
/// launching applets or opening external resources.
pub fn parse(source: &str, registry: &Registry) -> Result<GraphPlan> {
    let mut graph: Option<GraphPlan> = None;
    let mut saw_end = false;
    let mut pending_edges = Vec::new();
    let mut inputs = Vec::new();
    let mut saw_node_or_edge = false;
    let mut lines = parser::logical_lines(source)?;
    parser::take_language_version(&mut lines)?;
    let mut index = 0usize;

    while index < lines.len() {
        let statement = &lines[index];
        let line_number = statement.number;
        index += 1;
        let line = statement.text.trim();
        if line.is_empty() {
            continue;
        }
        if line.split_ascii_whitespace().next() == Some("execute") {
            bail!("line {line_number}: the `execute N` version marker must be the first statement");
        }
        if graph.is_none() {
            if parser::is_input_header(line) {
                let mut declaration =
                    parser::parse(line).map_err(|error| graph_parse_error(error, line_number))?;
                if !declaration.steps.is_empty() || declaration.inputs.len() != 1 {
                    bail!("line {line_number}: expected one top-level input declaration");
                }
                let mut input = declaration.inputs.remove(0);
                input.line = line_number;
                inputs.push(input);
                continue;
            }
            let words = line.split_whitespace().collect::<Vec<_>>();
            if words.len() != 2 || words[0] != "graph" || !identifier(words[1]) {
                bail!("line {line_number}: expected `graph <name>`");
            }
            let mut plan = GraphPlan::new(words[1]);
            plan.set_input_declarations(std::mem::take(&mut inputs))
                .with_context(|| format!("line {line_number}: invalid graph input interface"))?;
            graph = Some(plan);
            continue;
        }
        if saw_end {
            bail!("line {line_number}: statements cannot follow `end graph`");
        }
        if line == "end graph" {
            saw_end = true;
            continue;
        }
        if line.starts_with("bind ") {
            let mut edge = parse_edge(line, line_number)?;
            edge.line = Some(line_number);
            pending_edges.push(edge);
            saw_node_or_edge = true;
            continue;
        }
        if let Some(body) = line.strip_prefix("emit result from ") {
            let words = body.split_whitespace().collect::<Vec<_>>();
            let [node, "to", event_port] = words.as_slice() else {
                bail!("line {line_number}: expected `emit result from <node> to <event_port>`");
            };
            if !identifier(node) || !identifier(event_port) {
                bail!("line {line_number}: invalid result projection node or event port");
            }
            graph
                .as_mut()
                .expect("graph header was parsed")
                .project_result_event(node, event_port, line_number)?;
            continue;
        }
        if line.starts_with("run ") {
            let mut program =
                parser::parse(line).map_err(|error| graph_parse_error(error, line_number))?;
            if program.steps.len() != 1 {
                bail!("line {line_number}: a graph `run` must declare one applet invocation");
            }
            let step = program.steps.remove(0);
            let StepKind::Invoke {
                mut call,
                connection: Connection::Start,
            } = step.kind
            else {
                bail!("line {line_number}: graph nodes use `run <applet> ... as <node>`");
            };
            let id = call
                .binding
                .take()
                .ok_or_else(|| anyhow!("line {line_number}: graph applet needs `as <node>`"))?;
            if call.retry.is_some() {
                bail!("line {line_number}: graph nodes do not accept `and retry`");
            }
            if call.cache || call.status_binding.is_some() || call.result_binding.is_some() {
                bail!(
                    "line {line_number}: graph nodes do not accept finite cache/status/result suffixes"
                );
            }
            let manifest = registry
                .resolve_call(&call)
                .with_context(|| format!("line {line_number}"))?;
            let node = node_for_call(id, call, manifest, line_number)?;
            graph
                .as_mut()
                .expect("graph header was parsed")
                .add_node(node)
                .map_err(|error| anyhow!("line {line_number}: {error:#}"))?;
            saw_node_or_edge = true;
            continue;
        }
        if let Some(body) = line.strip_prefix("every ") {
            let (interval, id) =
                split_graph_binding(body).with_context(|| format!("line {line_number}"))?;
            if interval.split_whitespace().next().is_some_and(|value| {
                graph.as_ref().is_some_and(|plan| {
                    plan.input_declarations()
                        .iter()
                        .any(|input| input.name == value)
                })
            }) {
                bail!(
                    "line {line_number}: graph inputs can be used only in applet parameters; timer intervals must be literal"
                );
            }
            let interval_ms = parse_interval_ms(&interval, line_number)?;
            let node = timer_node(id, interval_ms, line_number);
            graph
                .as_mut()
                .expect("graph header was parsed")
                .add_node(node)
                .map_err(|error| anyhow!("line {line_number}: {error:#}"))?;
            saw_node_or_edge = true;
            continue;
        }
        if let Some(body) = line.strip_prefix("listen to ") {
            let (source_phrase, id) =
                split_graph_binding(body).with_context(|| format!("line {line_number}"))?;
            let source = if source_phrase == "stdin" {
                crate::ast::ListenSource::Stdin
            } else {
                let mut parsed =
                    parser::parse(&format!("run {source_phrase}")).map_err(|error| {
                        anyhow!("line {line_number}: invalid listener source: {error:#}")
                    })?;
                if parsed.steps.len() != 1 {
                    bail!("line {line_number}: listener source must be one applet invocation");
                }
                let crate::ast::StepKind::Invoke {
                    call,
                    connection: crate::ast::Connection::Start,
                } = parsed.steps.remove(0).kind
                else {
                    bail!("line {line_number}: listener source must be one applet invocation");
                };
                if call.retry.is_some() {
                    bail!("line {line_number}: listener sources do not accept `and retry`");
                }
                if call.binding.is_some()
                    || call.status_binding.is_some()
                    || call.result_binding.is_some()
                    || call.cache
                {
                    bail!(
                        "line {line_number}: listener source cannot bind output/status/result or use cache"
                    );
                }
                registry
                    .resolve_call(&call)
                    .with_context(|| format!("line {line_number}: resolving listener source"))?;
                crate::ast::ListenSource::Applet(call)
            };
            let node = listener_node(id, source, registry, line_number)?;
            graph
                .as_mut()
                .expect("graph header was parsed")
                .add_node(node)
                .map_err(|error| anyhow!("line {line_number}: {error:#}"))?;
            saw_node_or_edge = true;
            continue;
        }
        if let Some(body) = line.strip_prefix("render ") {
            let (render_source, id) =
                split_graph_binding(body).with_context(|| format!("line {line_number}"))?;
            let (backend, initial_properties) = parse_renderer_source(&render_source, line_number)?;
            let node = renderer_node(id.clone(), &backend, line_number)?;
            graph
                .as_mut()
                .expect("graph header was parsed")
                .add_node(node)
                .map_err(|error| anyhow!("line {line_number}: {error:#}"))?;
            if let Some(properties) = initial_properties {
                graph
                    .as_mut()
                    .expect("graph header was parsed")
                    .add_initial_input(id, "properties", properties)
                    .map_err(|error| anyhow!("line {line_number}: {error:#}"))?;
            }
            saw_node_or_edge = true;
            continue;
        }
        if let Some(body) = line.strip_prefix("stream from ") {
            let (source_phrase, id) =
                split_graph_binding(body).with_context(|| format!("line {line_number}"))?;
            let mut stream_source = format!("stream from {source_phrase}\n");
            let mut saw_sink = false;
            while index < lines.len() {
                let directive = lines[index].text.trim();
                if directive.is_empty() {
                    stream_source.push('\n');
                    index += 1;
                    continue;
                }
                if directive == "show each line" {
                    stream_source.push_str("show each line\n");
                    saw_sink = true;
                    index += 1;
                    break;
                }
                if directive.starts_with("pipe to ") || directive.starts_with("take ") {
                    stream_source.push_str(directive);
                    stream_source.push('\n');
                    index += 1;
                    continue;
                }
                break;
            }
            if !saw_sink {
                bail!(
                    "line {line_number}: graph stream requires final `show each line` before the next graph statement"
                );
            }
            let stream = crate::stream::parse(&stream_source)
                .with_context(|| format!("line {line_number}: parsing graph stream"))?;
            if stream.pipeline.steps.iter().any(|step| match &step.kind {
                StepKind::Invoke { call, .. } => call.parameters.values().any(value_has_reference),
                _ => false,
            }) {
                bail!(
                    "line {line_number}: graph inputs can be used only in `run` applet parameters; native stream parameters must be literal"
                );
            }
            crate::stream::validate(&stream, registry)
                .with_context(|| format!("line {line_number}: validating graph stream"))?;
            let calls = stream
                .pipeline
                .steps
                .iter()
                .filter_map(|step| match &step.kind {
                    StepKind::Invoke { call, .. } => Some(call.clone()),
                    _ => None,
                })
                .collect();
            let node = native_stream_node(id, stream_source, calls, registry, line_number)?;
            graph
                .as_mut()
                .expect("graph header was parsed")
                .add_node(node)
                .map_err(|error| anyhow!("line {line_number}: {error:#}"))?;
            saw_node_or_edge = true;
            continue;
        }
        bail!(
            "line {line_number}: expected `run ... as <node>`, `emit result from <node> to <event_port>`, `stream from ... as <node>`, `listen to ... as <node>`, `every ... as <node>`, `render quickshell|tui:<component> as <node>`, `bind <node.port> to <node.port>`, or `end graph`"
        );
    }

    if graph.is_none() {
        bail!("graph source is empty");
    }
    if !saw_end {
        bail!(
            "graph {:?} is missing `end graph`",
            graph.as_ref().unwrap().name
        );
    }
    if !saw_node_or_edge || graph.as_ref().unwrap().nodes().is_empty() {
        bail!("graph must contain at least one `run` node");
    }
    let mut graph = graph.expect("header required");
    for edge in pending_edges {
        graph.add_edge(edge);
    }
    graph.validate_with_registry(registry)?;
    Ok(graph)
}

fn graph_parse_error(error: anyhow::Error, line_number: usize) -> anyhow::Error {
    let message = error.to_string();
    let message = message.strip_prefix("line 1: ").unwrap_or(&message);
    anyhow!("line {line_number}: {message}")
}

fn split_graph_binding(source: &str) -> Result<(String, String)> {
    let tokens = parser::tokenize(source)?;
    let [.., parser::Token::Word("as"), parser::Token::Word(binding)] = tokens.as_slice() else {
        bail!("expected a trailing `as <node>` binding");
    };
    if !identifier(binding) {
        bail!("invalid graph node name {binding:?}");
    }
    let as_offset = match &tokens[tokens.len() - 2] {
        parser::Token::Word(word) => word.as_ptr() as usize - source.as_ptr() as usize,
        _ => unreachable!("matched an `as` word token"),
    };
    let phrase = source[..as_offset].trim_end();
    if phrase.is_empty() {
        bail!("graph node source is empty");
    }
    Ok((phrase.to_owned(), (*binding).to_owned()))
}

fn parse_interval_ms(interval: &str, line: usize) -> Result<u64> {
    let words = interval.split_whitespace().collect::<Vec<_>>();
    if words.len() != 2 {
        bail!(
            "line {line}: expected `every <positive integer> <milliseconds|seconds|minutes|hours> as <node>`"
        );
    }
    let amount = words[0]
        .parse::<u64>()
        .map_err(|_| anyhow!("line {line}: interval must be a positive integer"))?;
    if amount == 0 {
        bail!("line {line}: interval must be greater than zero");
    }
    let factor = match words[1] {
        "millisecond" | "milliseconds" => 1,
        "second" | "seconds" => 1_000,
        "minute" | "minutes" => 60_000,
        "hour" | "hours" => 3_600_000,
        unit => bail!("line {line}: unsupported interval unit {unit:?}"),
    };
    amount
        .checked_mul(factor)
        .ok_or_else(|| anyhow!("line {line}: interval is too large"))
}

fn timer_node(id: String, interval_ms: u64, line: usize) -> GraphNode {
    GraphNode {
        id,
        kind: GraphNodeKind::Timer { interval_ms },
        inputs: BTreeMap::new(),
        outputs: BTreeMap::new(),
        events: BTreeMap::from([(
            "tick".into(),
            PortSchema::events(leaf_schema(OutputType::Integer)),
        )]),
        line: Some(line),
    }
}

fn listener_node(
    id: String,
    source: crate::ast::ListenSource,
    registry: &Registry,
    line: usize,
) -> Result<GraphNode> {
    let events = match &source {
        crate::ast::ListenSource::Stdin => BTreeMap::from([(
            "line".into(),
            PortSchema::events(leaf_schema(OutputType::Bytes)),
        )]),
        crate::ast::ListenSource::Applet(call) => {
            let manifest = registry.resolve_call(call)?;
            if manifest.execution_backend() == crate::contracts::AppletBackend::Remote {
                let ports = manifest
                    .ports()
                    .context("remote listener requires declared event ports")?;
                if ports.events.is_empty() {
                    bail!(
                        "line {line}: remote listener applet {:?} declares no event ports",
                        manifest.name
                    );
                }
                ports
                    .events
                    .iter()
                    .map(|(name, port)| (name.clone(), PortSchema::from_applet(port, false)))
                    .collect()
            } else {
                BTreeMap::from([(
                    "line".into(),
                    PortSchema::events(leaf_schema(OutputType::Bytes)),
                )])
            }
        }
    };
    Ok(GraphNode {
        id,
        kind: GraphNodeKind::Listener {
            source,
            binding: "line".into(),
        },
        inputs: BTreeMap::new(),
        outputs: BTreeMap::new(),
        events,
        line: Some(line),
    })
}

fn native_stream_node(
    id: String,
    source: String,
    calls: Vec<Invocation>,
    registry: &Registry,
    line: usize,
) -> Result<GraphNode> {
    let manifest = calls
        .last()
        .map(|call| registry.resolve_call(call))
        .transpose()?;
    let output = manifest
        .and_then(|manifest| manifest.execution_contract())
        .and_then(|contract| contract.output.as_ref());
    let (schema, encoding) = match output {
        Some(output) => (value_schema_from_output(output), Some(output.encoding)),
        None => (leaf_schema(OutputType::Bytes), None),
    };
    let sensitivity = manifest
        .and_then(|manifest| {
            manifest
                .ports()
                .and_then(|ports| ports.outputs.get("stdout"))
                .map(|port| port.sensitivity)
        })
        .unwrap_or(Sensitivity::Unknown);
    Ok(GraphNode {
        id,
        kind: GraphNodeKind::NativeStream { source, calls },
        inputs: BTreeMap::new(),
        outputs: BTreeMap::from([
            (
                "stdout".into(),
                PortSchema {
                    schema,
                    delivery: PortDelivery::Records,
                    sensitivity,
                    required: false,
                    pointer: None,
                    encoding,
                },
            ),
            (
                "status".into(),
                PortSchema::single(leaf_schema(OutputType::Integer)),
            ),
        ]),
        events: BTreeMap::new(),
        line: Some(line),
    })
}

fn renderer_node(id: String, backend: &str, line: usize) -> Result<GraphNode> {
    let (backend_name, component) = backend
        .split_once(':')
        .ok_or_else(|| anyhow!("line {line}: renderer backend must use `<backend>:<component>`"))?;
    if !matches!(backend_name, "quickshell" | "tui") {
        bail!("line {line}: unsupported renderer backend {backend_name:?}");
    }
    if !matches!(component, "text" | "button") {
        bail!("line {line}: unsupported renderer component {component:?}");
    }
    let mut fields = BTreeMap::from([("text".into(), leaf_schema(OutputType::Text))]);
    let mut required = vec!["text".into()];
    if component == "button" {
        fields.insert("enabled".into(), leaf_schema(OutputType::Boolean));
        required.push("enabled".into());
    }
    let properties = ValueSchema {
        min_items: None,
        scalar_only: false,
        additional_properties: None,
        min_properties: None,
        max_properties: None,
        nonempty: false,
        kind: OutputType::Record,
        nullable: false,
        items: None,
        fields: fields.clone(),
        required: required.clone(),
        enum_values: None,
        single_line: false,
        minimum: None,
        pattern: None,
    };
    let mut inputs = BTreeMap::from([("properties".into(), PortSchema::single(properties))]);
    let updates = ValueSchema {
        min_items: None,
        scalar_only: false,
        additional_properties: None,
        min_properties: None,
        max_properties: None,
        nonempty: false,
        kind: OutputType::Record,
        nullable: false,
        items: None,
        fields,
        required: Vec::new(),
        enum_values: None,
        single_line: false,
        minimum: None,
        pattern: None,
    };
    inputs.insert("property_updates".into(), PortSchema::records(updates));
    let events = if component == "button" {
        BTreeMap::from([(
            "clicked".into(),
            PortSchema::events(ValueSchema {
                min_items: None,
                scalar_only: false,
                additional_properties: None,
                min_properties: None,
                max_properties: None,
                nonempty: false,
                kind: OutputType::Record,
                nullable: false,
                items: None,
                fields: BTreeMap::new(),
                required: Vec::new(),
                enum_values: None,
                single_line: false,
                minimum: None,
                pattern: None,
            }),
        )])
    } else {
        BTreeMap::new()
    };
    Ok(GraphNode {
        id,
        kind: GraphNodeKind::Renderer {
            backend: backend.to_owned(),
        },
        inputs,
        outputs: BTreeMap::new(),
        events,
        line: Some(line),
    })
}

fn parse_renderer_source(source: &str, line: usize) -> Result<(String, Option<crate::ast::Value>)> {
    let Some((backend, value_source)) = source.split_once(" with properties ") else {
        return Ok((source.to_owned(), None));
    };
    let backend = backend.trim();
    let value_source = value_source.trim();
    if backend.is_empty() || value_source.is_empty() {
        bail!("line {line}: renderer seed needs a backend and a properties record");
    }
    let value = parser::parse_value_source(value_source)
        .map_err(|error| anyhow!("line {line}: invalid renderer properties: {error:#}"))?;
    if value_has_reference(&value) {
        bail!(
            "line {line}: graph inputs can be used only in applet parameters; renderer properties must be literal"
        );
    }
    if !matches!(value, crate::ast::Value::Record(_)) {
        bail!("line {line}: renderer properties must be a record literal");
    }
    Ok((backend.to_owned(), Some(value)))
}

fn value_has_reference(value: &crate::ast::Value) -> bool {
    match value {
        crate::ast::Value::Reference(_) => true,
        crate::ast::Value::List(items) => items.iter().any(value_has_reference),
        crate::ast::Value::Record(fields) => fields.values().any(value_has_reference),
        _ => false,
    }
}

fn node_for_call(
    id: String,
    call: Invocation,
    manifest: &Manifest,
    line: usize,
) -> Result<GraphNode> {
    let mut inputs = BTreeMap::new();
    let mut outputs = BTreeMap::new();
    let mut events = BTreeMap::new();

    // Explicit graph ports take precedence. Legacy manifests remain composable
    // through their existing parameter schema, stdin capability and one stdout
    // result schema; no text or JSON conversion is guessed.
    if let Some(ports) = manifest.ports() {
        let parameters = parameter_schemas(manifest)?;
        for (name, port) in &ports.inputs {
            let required = parameters.get(name).is_some_and(|(_, required)| *required);
            inputs.insert(name.clone(), PortSchema::from_applet(port, required));
        }
        for (name, port) in &ports.outputs {
            outputs.insert(name.clone(), PortSchema::from_applet(port, false));
        }
        for (name, port) in &ports.events {
            events.insert(name.clone(), PortSchema::from_applet(port, false));
        }
    } else {
        for (name, (schema, required)) in parameter_schemas(manifest)? {
            inputs.insert(
                name,
                PortSchema {
                    schema,
                    delivery: PortDelivery::Single,
                    sensitivity: Sensitivity::Unknown,
                    required,
                    pointer: None,
                    encoding: None,
                },
            );
        }
        if manifest.accepts_stdin {
            inputs.insert(
                "stdin".into(),
                PortSchema {
                    schema: leaf_schema(OutputType::Bytes),
                    // A finite applet invocation owns one byte-vector stdin.
                    // Repeated stream-triggered invocations are represented
                    // by an explicit `records`/`events` input port instead.
                    delivery: PortDelivery::Single,
                    sensitivity: Sensitivity::Unknown,
                    required: false,
                    pointer: None,
                    encoding: Some(OutputEncoding::Raw),
                },
            );
        }
        let output = manifest
            .execution_contract()
            .and_then(|contract| contract.output.as_ref())
            .map(|schema| ValueSchema {
                min_items: None,
                scalar_only: false,
                additional_properties: None,
                min_properties: None,
                max_properties: None,
                nonempty: false,
                kind: schema.kind,
                nullable: false,
                items: schema.items.clone(),
                fields: schema.fields.clone(),
                required: schema.required.clone(),
                enum_values: schema.enum_values.clone(),
                single_line: false,
                minimum: None,
                pattern: None,
            })
            .unwrap_or_else(|| leaf_schema(OutputType::Bytes));
        outputs.insert(
            "stdout".into(),
            PortSchema {
                schema: output,
                delivery: PortDelivery::Single,
                sensitivity: Sensitivity::Unknown,
                required: false,
                pointer: None,
                encoding: None,
            },
        );
    }

    validate_ports_against_manifest(manifest, &inputs)?;
    if outputs.contains_key("status") || events.contains_key("status") {
        bail!(
            "applet {:?} declares reserved graph port `status`; status is generated by the graph runtime",
            manifest.name
        );
    }
    let status_delivery = if inputs
        .values()
        .any(|port| port.delivery != PortDelivery::Single)
    {
        PortDelivery::Records
    } else {
        PortDelivery::Single
    };
    let is_program_service = manifest
        .execution_contract()
        .is_some_and(|contract| contract.lifecycle == Lifecycle::ProgramService);
    let node = GraphNode {
        id,
        kind: if is_program_service {
            GraphNodeKind::ProgramService { call }
        } else {
            GraphNodeKind::Applet { call }
        },
        inputs,
        outputs,
        events,
        line: Some(line),
    };
    // `status` is a stable result port for all applets, including those with
    // explicit value ports. It is generated from the native invocation result.
    let mut node = node;
    if !is_program_service {
        node.outputs
            .entry("status".into())
            .or_insert_with(|| PortSchema {
                schema: leaf_schema(OutputType::Integer),
                delivery: status_delivery,
                sensitivity: Sensitivity::Unknown,
                required: false,
                pointer: None,
                encoding: None,
            });
    }
    Ok(node)
}

fn validate_ports_against_manifest(
    manifest: &Manifest,
    graph_inputs: &BTreeMap<String, PortSchema>,
) -> Result<()> {
    let parameters = parameter_schemas(manifest)?;
    for name in graph_inputs.keys() {
        if name == "stdin" {
            if !manifest.accepts_stdin {
                bail!("applet {:?} does not accept stdin", manifest.name);
            }
            continue;
        }
        let Some((schema, _)) = parameters.get(name) else {
            bail!("applet {:?} has no input parameter {name:?}", manifest.name);
        };
        if !graph_inputs[name].schema.is_assignable_to(schema) {
            bail!(
                "applet {:?} input port {name:?} schema {:?} does not match parameter schema {:?}",
                manifest.name,
                graph_inputs[name].schema,
                schema
            );
        }
    }
    Ok(())
}

fn parameter_schemas(manifest: &Manifest) -> Result<BTreeMap<String, (ValueSchema, bool)>> {
    let projection = manifest.contract();
    let properties = projection["parameters_schema"]["properties"]
        .as_object()
        .ok_or_else(|| anyhow!("applet {:?} has malformed parameter schema", manifest.name))?;
    let required = projection["parameters_schema"]["required"]
        .as_array()
        .map(|items| {
            items
                .iter()
                .filter_map(serde_json::Value::as_str)
                .map(str::to_owned)
                .collect::<BTreeSet<_>>()
        })
        .unwrap_or_default();
    properties
        .iter()
        .map(|(name, value)| {
            Ok((
                name.clone(),
                (
                    value_schema_from_json_schema(value)?,
                    required.contains(name),
                ),
            ))
        })
        .collect()
}

fn value_schema_from_json_schema(value: &serde_json::Value) -> Result<ValueSchema> {
    let kind = match value.get("type").and_then(serde_json::Value::as_str) {
        Some("string")
            if value.get("format").and_then(serde_json::Value::as_str) == Some("path") =>
        {
            OutputType::Path
        }
        Some("string") => OutputType::Text,
        Some("integer") => OutputType::Integer,
        Some("unsigned_integer") => OutputType::UnsignedInteger,
        Some("usize") => OutputType::Usize,
        Some("number") => OutputType::Number,
        Some("boolean") => OutputType::Boolean,
        Some("array") => OutputType::List,
        Some("object") => OutputType::Record,
        _ => OutputType::Json,
    };
    if kind == OutputType::Record {
        if let Some(properties) = value
            .get("properties")
            .and_then(serde_json::Value::as_object)
        {
            let fields = properties
                .iter()
                .map(|(name, value)| Ok((name.clone(), value_schema_from_json_schema(value)?)))
                .collect::<Result<BTreeMap<_, _>>>()?;
            let required = value
                .get("required")
                .and_then(serde_json::Value::as_array)
                .map(|required| {
                    required
                        .iter()
                        .filter_map(serde_json::Value::as_str)
                        .map(str::to_owned)
                        .collect()
                })
                .unwrap_or_default();
            let enum_values = parse_string_enum(value)?;
            let minimum = parse_numeric_minimum(value)?;
            return Ok(ValueSchema {
                min_items: None,
                scalar_only: false,
                additional_properties: None,
                min_properties: None,
                max_properties: None,
                nonempty: false,
                kind,
                nullable: false,
                fields,
                required,
                items: None,
                enum_values,
                single_line: false,
                minimum,
                pattern: None,
            });
        }
        return Ok(leaf_schema(OutputType::Json));
    }
    let items = if kind == OutputType::List {
        let item_schema = value
            .get("items")
            .ok_or_else(|| anyhow!("list parameter schema must declare item type"))?;
        Some(Box::new(value_schema_from_json_schema(item_schema)?))
    } else {
        None
    };
    let enum_values = parse_string_enum(value)?;
    let minimum = parse_numeric_minimum(value)?;
    Ok(ValueSchema {
        min_items: None,
        scalar_only: false,
        additional_properties: None,
        min_properties: None,
        max_properties: None,
        nonempty: false,
        kind,
        nullable: false,
        fields: BTreeMap::new(),
        required: Vec::new(),
        items,
        enum_values,
        single_line: false,
        minimum,
        pattern: None,
    })
}

fn parse_numeric_minimum(value: &serde_json::Value) -> Result<Option<serde_json::Number>> {
    match value.get("minimum") {
        None => Ok(None),
        Some(serde_json::Value::Number(minimum)) => Ok(Some(minimum.clone())),
        Some(_) => bail!("numeric schema minimum must be a JSON number"),
    }
}

fn leaf_schema(kind: OutputType) -> ValueSchema {
    ValueSchema {
        min_items: None,
        scalar_only: false,
        additional_properties: None,
        min_properties: None,
        max_properties: None,
        nonempty: false,
        kind,
        nullable: false,
        fields: BTreeMap::new(),
        required: Vec::new(),
        items: None,
        enum_values: None,
        single_line: false,
        minimum: None,
        pattern: None,
    }
}

fn parse_string_enum(value: &serde_json::Value) -> Result<Option<Vec<String>>> {
    let Some(values) = value.get("enum") else {
        return Ok(None);
    };
    let values = values
        .as_array()
        .ok_or_else(|| anyhow!("string enum must be an array"))?;
    values
        .iter()
        .map(|value| {
            value
                .as_str()
                .map(str::to_owned)
                .ok_or_else(|| anyhow!("string enum values must all be strings"))
        })
        .collect::<Result<Vec<_>>>()
        .map(Some)
}

fn parse_edge(line: &str, line_number: usize) -> Result<EdgeSpec> {
    let words = line.split_whitespace().collect::<Vec<_>>();
    if words.len() < 4 || words[0] != "bind" || words[2] != "to" {
        bail!("line {line_number}: expected `bind <node.port> to <node.port>`");
    }
    let from = parse_port_ref(words[1]).with_context(|| format!("line {line_number}"))?;
    let to = parse_port_ref(words[3]).with_context(|| format!("line {line_number}"))?;
    let mut capacity = 16usize;
    let mut policy = BufferPolicy::Lossless;
    match words.get(4..) {
        None | Some([]) => {}
        Some(["with", "capacity", number]) => {
            capacity = parse_capacity(number, line_number)?;
        }
        Some(["with", "capacity", number, policy_words @ ..]) => {
            capacity = parse_capacity(number, line_number)?;
            policy = parse_policy(policy_words).with_context(|| format!("line {line_number}"))?;
        }
        _ => bail!(
            "line {line_number}: expected `with capacity N [lossless|drop newest|drop oldest|latest]`"
        ),
    }
    Ok(EdgeSpec {
        from,
        to,
        capacity,
        policy,
        line: Some(line_number),
    })
}

fn parse_policy(words: &[&str]) -> Result<BufferPolicy> {
    match words {
        ["lossless"] => Ok(BufferPolicy::Lossless),
        ["drop", "newest"] => Ok(BufferPolicy::DropNewest),
        ["drop", "oldest"] => Ok(BufferPolicy::DropOldest),
        ["latest"] => Ok(BufferPolicy::Latest),
        _ => bail!("unknown edge policy; use lossless, drop newest, drop oldest, or latest"),
    }
}

fn parse_capacity(value: &str, line: usize) -> Result<usize> {
    let capacity = value
        .parse::<usize>()
        .with_context(|| format!("line {line}: queue capacity must be a positive integer"))?;
    if capacity == 0 {
        bail!("line {line}: queue capacity must be positive");
    }
    Ok(capacity)
}

fn parse_port_ref(value: &str) -> Result<PortRef> {
    let (node, port) = value
        .split_once('.')
        .ok_or_else(|| anyhow!("expected `node.port`, received {value:?}"))?;
    if !identifier(node) || !identifier(port) {
        bail!("invalid graph port reference {value:?}");
    }
    Ok(PortRef::new(node, port))
}

fn identifier(value: &str) -> bool {
    let mut chars = value.chars();
    matches!(chars.next(), Some(c) if c == '_' || c.is_ascii_alphabetic())
        && chars.all(|c| c == '_' || c.is_ascii_alphanumeric() || c == '-')
}

#[cfg(test)]
mod tests {
    use super::*;

    fn registry() -> Registry {
        Registry::from_sources(&[
            r#"{"name":"source","binary":"source","arguments":[],"parameters":{},"contract":{"version":1,"output":{"type":"bytes","encoding":"raw"}}}"#,
            r#"{"name":"sink","binary":"sink","accepts_stdin":true,"arguments":[],"parameters":{},"contract":{"version":1,"output":{"type":"bytes","encoding":"raw"}}}"#,
        ])
        .unwrap()
    }

    fn result_projection_registry(
        event_schema: serde_json::Value,
        ordinary_output: bool,
    ) -> Registry {
        let mut outputs = serde_json::Map::new();
        if ordinary_output {
            outputs.insert(
                "summary".into(),
                serde_json::json!({
                    "schema": { "type": "text" },
                    "delivery": "single",
                    "pointer": "/text"
                }),
            );
        }
        let descriptor = serde_json::json!({
            "name": "result.transform",
            "binary": "transform",
            "arguments": [],
            "parameters": {},
            "contract": {
                "version": 1,
                "output": {
                    "type": "record",
                    "encoding": "json",
                    "fields": { "text": { "type": "text" } },
                    "required": ["text"]
                },
                "ports": {
                    "outputs": outputs,
                    "events": {
                        "result": {
                            "schema": event_schema,
                            "delivery": "events"
                        }
                    }
                }
            }
        });
        let raw = serde_json::to_string(&descriptor).unwrap();
        Registry::from_sources(&[raw]).unwrap()
    }

    #[test]
    fn finite_result_projection_uses_one_declared_typed_event_and_suppresses_status() {
        let schema = serde_json::json!({
            "type": "record",
            "fields": { "text": { "type": "text" } },
            "required": ["text"]
        });
        let registry = result_projection_registry(schema, false);
        let plan = parse(
            "graph result_projection\n  run result.transform as mapped\n  emit result from mapped to result\nend graph\n",
            &registry,
        )
        .unwrap();
        let node = plan
            .nodes()
            .iter()
            .find(|node| node.id == "mapped")
            .unwrap();
        assert!(node.outputs.is_empty());
        assert_eq!(node.events.len(), 1);
        assert!(matches!(
            &node.kind,
            GraphNodeKind::AppletResultEvent { call, event_port }
                if call.operation == "result.transform" && event_port == "result"
        ));
        assert!(plan.to_json().unwrap().contains("applet_result_event"));
    }

    #[test]
    fn finite_result_projection_rejects_ambiguous_or_mismatched_contracts() {
        let record = serde_json::json!({
            "type": "record",
            "fields": { "text": { "type": "text" } },
            "required": ["text"]
        });
        let text = serde_json::json!({ "type": "text" });
        let source = |port: &str| {
            format!(
            "graph result_projection\n  run result.transform as mapped\n  emit result from mapped to {port}\nend graph\n"
        )
        };
        let mixed = result_projection_registry(record.clone(), true);
        assert!(parse(&source("result"), &mixed)
            .unwrap_err()
            .to_string()
            .contains("cannot mix ordinary output ports"));

        let mismatched = result_projection_registry(text, false);
        assert!(parse(&source("result"), &mismatched)
            .unwrap_err()
            .to_string()
            .contains("canonical result schema"));

        let mut multiple = serde_json::json!({
            "name": "result.transform",
            "binary": "transform",
            "arguments": [],
            "parameters": {},
            "contract": {
                "version": 1,
                "output": {
                    "type": "record",
                    "encoding": "json",
                    "fields": { "text": { "type": "text" } },
                    "required": ["text"]
                },
                "ports": {
                    "events": {
                        "result": { "schema": record, "delivery": "events" },
                        "other": { "schema": record, "delivery": "events" }
                    }
                }
            }
        });
        multiple["contract"]["ports"]["events"]["other"] = serde_json::json!({
            "schema": record,
            "delivery": "events"
        });
        let raw = serde_json::to_string(&multiple).unwrap();
        let registry = Registry::from_sources(&[raw]).unwrap();
        assert!(parse(&source("result"), &registry)
            .unwrap_err()
            .to_string()
            .contains("exactly one Events port"));
    }

    #[test]
    fn parses_graph_nodes_bindings_and_explicit_queue_policy() {
        let plan = parse(
            "graph demo\n  run source as left\n  run sink as right\n  bind left.stdout to right.stdin with capacity 3 drop oldest\nend graph\n",
            &registry(),
        )
        .unwrap();
        assert_eq!(plan.name, "demo");
        assert_eq!(plan.nodes().len(), 2);
        assert_eq!(plan.edges()[0].capacity, 3);
        assert_eq!(plan.edges()[0].policy, BufferPolicy::DropOldest);
    }

    #[test]
    fn rejects_missing_end_and_bad_edge_policy() {
        assert!(parse("graph demo\n  run source as s\n", &registry())
            .unwrap_err()
            .to_string()
            .contains("missing `end graph`"));
        let error = parse(
            "graph demo\n  run source as s\n  bind s.stdout to s.stdin with capacity 3 discard\nend graph",
            &registry()
        )
        .unwrap_err();
        let error = format!("{error:#}");
        assert!(error.contains("edge policy"), "{error}");
    }
}
