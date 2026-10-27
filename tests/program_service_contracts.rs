use flow::contracts::{
    AppletContract, AppletPort, AppletPorts, OutputEncoding, OutputSchema, OutputType,
    PortDelivery, ValueSchema,
};
use flow::manifest::Registry;
use flow::modules::{
    load_program_module, load_program_module_package, ModuleLifecycle, ProgramModule,
    ProgramModuleExport, ProgramModuleInput, ProgramModuleInputType, ProgramModulePackageManifest,
    ProgramModuleSession, PROGRAM_MODULE_INTERFACE_VERSION, PROGRAM_MODULE_PACKAGE_FORMAT,
    PROGRAM_MODULE_SERVICE_INTERFACE_VERSION, PROGRAM_MODULE_SERVICE_ROOT_ENTRY,
};
use serde_json::{json, Value as JsonValue};
use sha2::{Digest, Sha256};
use std::collections::BTreeMap;
use std::fs;
use std::path::{Path, PathBuf};
use tempfile::tempdir;

fn digest(bytes: &[u8]) -> String {
    format!("{:x}", Sha256::digest(bytes))
}

fn integer_schema() -> ValueSchema {
    ValueSchema {
        scalar_only: false,
        additional_properties: None,
        min_properties: None,
        max_properties: None,
        nonempty: false,
        kind: OutputType::Integer,
        nullable: false,
        items: None,
        fields: BTreeMap::new(),
        required: Vec::new(),
        enum_values: None,
        single_line: false,
    }
}

fn output_schema() -> OutputSchema {
    OutputSchema {
        kind: OutputType::Record,
        encoding: OutputEncoding::Json,
        enum_values: None,
        same_as_parameter: None,
        items: None,
        fields: BTreeMap::from([("count".into(), integer_schema())]),
        required: vec!["count".into()],
        trim_final_lf: false,
    }
}

fn decoded_output_port_schema() -> ValueSchema {
    ValueSchema {
        scalar_only: false,
        additional_properties: None,
        min_properties: None,
        max_properties: None,
        nonempty: false,
        kind: OutputType::Record,
        nullable: false,
        items: None,
        fields: output_schema().fields,
        required: vec!["count".into()],
        enum_values: None,
        single_line: false,
    }
}

fn result_envelope_port_schema() -> ValueSchema {
    let text = text_schema();
    let integer = integer_schema();
    let value = ValueSchema {
        nullable: true,
        ..decoded_output_port_schema()
    };
    ValueSchema {
        scalar_only: false,
        additional_properties: None,
        min_properties: None,
        max_properties: None,
        nonempty: false,
        kind: OutputType::Record,
        nullable: false,
        items: None,
        fields: BTreeMap::from([
            ("value".into(), value),
            ("outcome".into(), text.clone()),
            ("kind".into(), text.clone()),
            ("effect_certainty".into(), text),
            ("status".into(), integer),
        ]),
        required: vec![
            "value".into(),
            "outcome".into(),
            "kind".into(),
            "effect_certainty".into(),
            "status".into(),
        ],
        enum_values: None,
        single_line: false,
    }
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
        fields: BTreeMap::new(),
        required: Vec::new(),
        enum_values: None,
        single_line: false,
    }
}

fn port(schema: ValueSchema, delivery: PortDelivery) -> AppletPort {
    AppletPort {
        schema,
        delivery,
        sensitivity: Default::default(),
        pointer: None,
        encoding: None,
    }
}

fn service_ports() -> AppletPorts {
    AppletPorts {
        inputs: BTreeMap::from([(
            "threshold".into(),
            port(integer_schema(), PortDelivery::Single),
        )]),
        outputs: BTreeMap::new(),
        events: BTreeMap::from([(
            "updated".into(),
            port(decoded_output_port_schema(), PortDelivery::Events),
        )]),
    }
}

fn service_source() -> &'static str {
    "accept threshold as integer\nevery 1 second\n  run \"printf\" with arguments [\"{\\\"count\\\":1}\"]\n"
}

fn service_package(source: &str) -> ProgramModulePackageManifest {
    ProgramModulePackageManifest {
        format_version: PROGRAM_MODULE_PACKAGE_FORMAT,
        name: "example.poller".into(),
        version: "1.0.0".into(),
        interface_version: PROGRAM_MODULE_SERVICE_INTERFACE_VERSION,
        source: PathBuf::from("src/service.program"),
        source_sha256: digest(source.as_bytes()),
        exports: BTreeMap::from([(
            "poll".into(),
            ProgramModuleExport {
                entry: PROGRAM_MODULE_SERVICE_ROOT_ENTRY.into(),
                interface_version: PROGRAM_MODULE_SERVICE_INTERFACE_VERSION,
                inputs: vec![ProgramModuleInput {
                    name: "threshold".into(),
                    kind: ProgramModuleInputType::Integer,
                    required: true,
                }],
                output: output_schema(),
                outcomes: Vec::new(),
                result_envelope: false,
                lifecycle: ModuleLifecycle::Schedule,
                capabilities: Vec::new(),
                effects: Vec::new(),
                ports: Some(service_ports()),
            },
        )]),
        dependencies: BTreeMap::new(),
    }
}

fn write_package(root: &Path, source: &str, manifest: &ProgramModulePackageManifest) -> PathBuf {
    let directory = root.join("package");
    fs::create_dir_all(directory.join("src")).unwrap();
    fs::write(directory.join("src/service.program"), source).unwrap();
    let path = directory.join("program.modules.json");
    fs::write(&path, serde_json::to_vec_pretty(manifest).unwrap()).unwrap();
    path
}

fn load_service_package(root: &Path, source: &str) -> (PathBuf, ProgramModulePackageManifest) {
    let manifest = service_package(source);
    let path = write_package(root, source, &manifest);
    (path, manifest)
}

fn source_direct_applet(root: &Path, source: &str) -> (ProgramModule, PathBuf) {
    let applet_dir = root.join("applets");
    let module_dir = root.join("modules");
    fs::create_dir_all(&applet_dir).unwrap();
    fs::create_dir_all(&module_dir).unwrap();
    fs::write(module_dir.join("service.program"), source).unwrap();
    let manifest = applet_dir.join("service.json");
    fs::write(&manifest, "{}").unwrap();
    (
        ProgramModule {
            source: PathBuf::from("../modules/service.program"),
            entry: PROGRAM_MODULE_SERVICE_ROOT_ENTRY.into(),
            interface_version: PROGRAM_MODULE_SERVICE_INTERFACE_VERSION,
            package_manifest: None,
            package_lock: None,
        },
        manifest,
    )
}

fn contract_json(reference: &ProgramModule, export: &ProgramModuleExport) -> JsonValue {
    json!({
        "version": 1,
        "backend": "program_module",
        "program": reference,
        "lifecycle": {"kind": "program_service"},
        "output": export.output,
        "outcomes": export.outcomes,
        "result_envelope": export.result_envelope,
        "capabilities": export.capabilities,
        "effects": export.effects,
        "ports": export.ports
    })
}

#[test]
fn v1_finite_package_metadata_serialization_and_loading_remain_compatible() {
    let source = "define a program named summarize\n  accept threshold as integer\n  then encode threshold as json\n";
    let mut manifest = service_package(source);
    manifest.name = "example.finite".into();
    manifest.interface_version = PROGRAM_MODULE_INTERFACE_VERSION;
    let export = manifest.exports.get_mut("poll").unwrap();
    export.interface_version = PROGRAM_MODULE_INTERFACE_VERSION;
    export.entry = "summarize".into();
    export.lifecycle = ModuleLifecycle::OneShot;
    export.ports = None;
    let root = tempdir().unwrap();
    let path = write_package(root.path(), source, &manifest);

    let encoded = serde_json::to_value(&manifest).unwrap();
    assert_eq!(PROGRAM_MODULE_INTERFACE_VERSION, 1);
    assert!(encoded["exports"]["poll"].get("ports").is_none());
    let loaded = load_program_module_package(&path, root.path(), None).unwrap();
    let exported = loaded.export("poll").unwrap();
    assert!(exported.finite_definition().is_some());
    assert!(!matches!(
        &exported.entry,
        flow::modules::LoadedProgramEntry::ProgramService
    ));
}

#[test]
fn v2_schedule_package_and_direct_source_select_the_real_program_root() {
    let root = tempdir().unwrap();
    let (path, _) = load_service_package(root.path(), service_source());
    let package = load_program_module_package(&path, root.path(), None).unwrap();
    let exported = package.export("poll").unwrap();
    assert!(matches!(
        &exported.entry,
        flow::modules::LoadedProgramEntry::ProgramService
    ));
    assert!(exported.finite_definition().is_none());
    assert_eq!(exported.reference.entry, "poll");
    assert!(matches!(
        &exported.program.trigger,
        Some(flow::ast::Trigger::Every { .. })
    ));
    assert_eq!(exported.program.inputs[0].name, "threshold");

    let (reference, applet_manifest) = source_direct_applet(root.path(), service_source());
    let direct = load_program_module(&reference, &applet_manifest, root.path()).unwrap();
    assert!(direct.is_program_service());
    assert_eq!(direct.input_declarations()[0].name, "threshold");
    assert!(direct
        .body_steps()
        .iter()
        .any(|step| matches!(&step.kind, flow::ast::StepKind::Invoke { .. })));
}

#[test]
fn v2_listener_export_uses_the_persistent_service_root_and_typed_contract() {
    let source = "accept threshold as integer\nlisten to test.producer with count threshold\nwhenever a line arrives as message\n  then encode threshold as json\n";
    let root = tempdir().unwrap();
    let mut manifest = service_package(source);
    manifest.name = "example.listener".into();
    manifest.exports.get_mut("poll").unwrap().lifecycle = ModuleLifecycle::Listener;
    let path = write_package(root.path(), source, &manifest);

    let loaded_package = load_program_module_package(&path, root.path(), None).unwrap();
    let exported = loaded_package.export("poll").unwrap();
    assert!(matches!(
        &exported.entry,
        flow::modules::LoadedProgramEntry::ProgramService
    ));
    assert!(matches!(
        &exported.program.trigger,
        Some(flow::ast::Trigger::Listen {
            source: flow::ast::ListenSource::Applet(_),
            ..
        })
    ));
    assert_eq!(exported.reference.entry, "poll");

    let reference = ProgramModule {
        source: PathBuf::from("../package/src/service.program"),
        entry: "poll".into(),
        interface_version: PROGRAM_MODULE_SERVICE_INTERFACE_VERSION,
        package_manifest: Some(PathBuf::from("../package/program.modules.json")),
        package_lock: Some(PathBuf::from("../package/program.modules.lock.json")),
    };
    fs::write(
        root.path().join("package/program.modules.lock.json"),
        serde_json::to_vec_pretty(&loaded_package.lock_snapshot(root.path()).unwrap()).unwrap(),
    )
    .unwrap();
    let applet_manifest = root.path().join("applets/listener.json");
    fs::create_dir_all(applet_manifest.parent().unwrap()).unwrap();
    fs::write(&applet_manifest, "{}").unwrap();
    let loaded_source = load_program_module(&reference, &applet_manifest, root.path()).unwrap();
    let contract: AppletContract =
        serde_json::from_value(contract_json(&reference, &exported.export)).unwrap();
    assert_eq!(
        contract.lifecycle,
        flow::contracts::Lifecycle::ProgramService
    );
    loaded_source.validate_contract(&contract).unwrap();
}

#[test]
fn listener_trigger_applet_is_retained_in_the_verified_package_scope() {
    let root = tempdir().unwrap();
    let catalog = root.path().join("catalog");
    fs::create_dir_all(&catalog).unwrap();
    let source = "accept threshold as integer\nlisten to test.producer with count threshold\nwhenever a line arrives as message\n  then encode threshold as json\n";
    let mut package_manifest = service_package(source);
    package_manifest.name = "example.listener".into();
    package_manifest.exports.get_mut("poll").unwrap().lifecycle = ModuleLifecycle::Listener;
    let package_path = write_package(&catalog, source, &package_manifest);
    let package = load_program_module_package(&package_path, &catalog, None).unwrap();
    fs::write(
        catalog.join("package/program.modules.lock.json"),
        serde_json::to_vec_pretty(&package.lock_snapshot(&catalog).unwrap()).unwrap(),
    )
    .unwrap();

    let reference = ProgramModule {
        source: PathBuf::from("package/src/service.program"),
        entry: "poll".into(),
        interface_version: PROGRAM_MODULE_SERVICE_INTERFACE_VERSION,
        package_manifest: Some(PathBuf::from("package/program.modules.json")),
        package_lock: Some(PathBuf::from("package/program.modules.lock.json")),
    };
    let export = package.export("poll").unwrap();
    let applet = json!({
        "name": "test.listener_module",
        "binary": "@program-module",
        "accepts_stdin": false,
        "parameters": {"threshold": {"type": "integer", "required": true}},
        "arguments": [],
        "contract": contract_json(&reference, &export.export)
    });
    fs::write(
        catalog.join("listener_module.json"),
        serde_json::to_vec_pretty(&applet).unwrap(),
    )
    .unwrap();
    let producer = json!({
        "name": "test.producer",
        "binary": "/bin/true",
        "parameters": {"count": {"type": "integer", "required": true}},
        "arguments": [{"parameter": "count"}],
        "contract": {"version": 1, "output": {"type": "bytes", "encoding": "raw"}}
    });
    fs::write(
        catalog.join("producer.json"),
        serde_json::to_vec_pretty(&producer).unwrap(),
    )
    .unwrap();

    let registry = Registry::load(&catalog).unwrap();
    let caller = flow::parser::parse("run test.listener_module with threshold 7\n").unwrap();
    let flow::ast::StepKind::Invoke { call, .. } = &caller.steps[0].kind else {
        panic!("expected a graph service invocation");
    };
    let session =
        ProgramModuleSession::preload_invocations(&registry, std::iter::once(call)).unwrap();
    let trigger = session
        .scoped_body_invocations()
        .iter()
        .find(|(_, invocation)| invocation.operation == "test.producer")
        .expect("listener trigger call is part of scoped module preflight");
    assert_eq!(
        trigger.0.resolve_call(&trigger.1).unwrap().name,
        "test.producer"
    );

    let manifest_path = catalog.join("listener_module.json");
    let mut applet: JsonValue = serde_json::from_slice(&fs::read(&manifest_path).unwrap()).unwrap();
    applet["accepts_stdin"] = json!(true);
    fs::write(&manifest_path, serde_json::to_vec_pretty(&applet).unwrap()).unwrap();
    let stdin_registry = Registry::load(&catalog).unwrap();
    let error = ProgramModuleSession::preload_invocations(&stdin_registry, std::iter::once(call))
        .unwrap_err();
    assert!(format!("{error:#}").contains("cannot accept ambient stdin"));
}

#[test]
fn scheduled_source_rejects_missing_or_listener_trigger_and_wrong_root_selector() {
    let root = tempdir().unwrap();
    let missing = "accept threshold as integer\nrun \"true\"\n";
    let (path, _) = load_service_package(root.path(), missing);
    let error = load_program_module_package(&path, root.path(), None).unwrap_err();
    assert!(format!("{error:#}").contains("requires a schedule or applet-listener trigger"));

    let listening = "accept threshold as integer\nlisten to stdin\nwhenever a line arrives as message\n  then tr with from \"a\" and to \"b\"\n";
    let (path, _) = load_service_package(root.path(), listening);
    let error = load_program_module_package(&path, root.path(), None).unwrap_err();
    assert!(format!("{error:#}").contains("cannot read ambient stdin"));

    let listening = "accept threshold as integer\nlisten to test.producer with count threshold\nwhenever a line arrives as message\n  then encode threshold as json\n";
    let (path, _) = load_service_package(root.path(), listening);
    let error = load_program_module_package(&path, root.path(), None).unwrap_err();
    assert!(format!("{error:#}").contains("scheduled program module export"));

    let (reference, applet_manifest) = source_direct_applet(root.path(), service_source());
    let mut reference = reference;
    reference.entry = "poll".into();
    let error = load_program_module(&reference, &applet_manifest, root.path()).unwrap_err();
    assert!(format!("{error:#}").contains("reserved root entry \"main\""));
}

#[test]
fn schedule_and_finite_exports_cannot_cross_interface_versions() {
    let root = tempdir().unwrap();
    let finite = "define a program named finite\n  run \"true\"\n";
    let mut manifest = service_package(finite);
    let path = write_package(root.path(), finite, &manifest);
    let error = load_program_module_package(&path, root.path(), None).unwrap_err();
    assert!(format!("{error:#}").contains("requires a schedule or applet-listener trigger"));

    manifest.interface_version = PROGRAM_MODULE_INTERFACE_VERSION;
    let export = manifest.exports.get_mut("poll").unwrap();
    export.interface_version = PROGRAM_MODULE_INTERFACE_VERSION;
    export.lifecycle = ModuleLifecycle::Schedule;
    let path = write_package(root.path(), finite, &manifest);
    let error = load_program_module_package(&path, root.path(), None).unwrap_err();
    assert!(format!("{error:#}")
        .contains("unsupported Schedule lifecycle for finite interface version 1"));

    manifest.interface_version = PROGRAM_MODULE_SERVICE_INTERFACE_VERSION;
    let export = manifest.exports.get_mut("poll").unwrap();
    export.interface_version = PROGRAM_MODULE_SERVICE_INTERFACE_VERSION;
    export.lifecycle = ModuleLifecycle::OneShot;
    manifest.source_sha256 = digest(service_source().as_bytes());
    let path = write_package(root.path(), service_source(), &manifest);
    let error = load_program_module_package(&path, root.path(), None).unwrap_err();
    assert!(format!("{error:#}")
        .contains("unsupported OneShot lifecycle for service interface version 2"));

    manifest.interface_version = 3;
    let path = write_package(root.path(), service_source(), &manifest);
    let error = load_program_module_package(&path, root.path(), None).unwrap_err();
    assert!(format!("{error:#}").contains("unsupported program module interface version 3"));
}

#[test]
fn service_export_preflight_checks_inputs_event_shape_and_unambiguous_ports() {
    let source = service_source();
    let root = tempdir().unwrap();
    let mut manifest = service_package(source);

    let input = manifest.exports.get_mut("poll").unwrap();
    input.inputs[0].name = "other".into();
    let path = write_package(root.path(), source, &manifest);
    let error = load_program_module_package(&path, root.path(), None).unwrap_err();
    assert!(format!("{error:#}").contains("declares input \"other\" missing from source entry"));

    let mut manifest = service_package(source);
    let input = manifest.exports.get_mut("poll").unwrap();
    input
        .ports
        .as_mut()
        .unwrap()
        .inputs
        .get_mut("threshold")
        .unwrap()
        .delivery = PortDelivery::Records;
    let path = write_package(root.path(), source, &manifest);
    let error = load_program_module_package(&path, root.path(), None).unwrap_err();
    assert!(format!("{error:#}").contains("must use single delivery"));

    let mut manifest = service_package(source);
    manifest
        .exports
        .get_mut("poll")
        .unwrap()
        .ports
        .as_mut()
        .unwrap()
        .outputs
        .insert(
            "stdout_value".into(),
            port(integer_schema(), PortDelivery::Single),
        );
    let path = write_package(root.path(), source, &manifest);
    let error = load_program_module_package(&path, root.path(), None).unwrap_err();
    assert!(format!("{error:#}").contains("does not support stdout output ports"));

    let mut manifest = service_package(source);
    manifest
        .exports
        .get_mut("poll")
        .unwrap()
        .ports
        .as_mut()
        .unwrap()
        .inputs
        .get_mut("threshold")
        .unwrap()
        .schema = text_schema();
    let path = write_package(root.path(), source, &manifest);
    let error = load_program_module_package(&path, root.path(), None).unwrap_err();
    assert!(format!("{error:#}")
        .contains("input port \"threshold\" is incompatible with its source declaration"));

    let mut manifest = service_package(source);
    let input = manifest.exports.get_mut("poll").unwrap();
    input.ports.as_mut().unwrap().events.insert(
        "duplicate".into(),
        port(decoded_output_port_schema(), PortDelivery::Events),
    );
    let path = write_package(root.path(), source, &manifest);
    let error = load_program_module_package(&path, root.path(), None).unwrap_err();
    assert!(format!("{error:#}").contains("must declare exactly one event port"));

    let mut manifest = service_package(source);
    let input = manifest.exports.get_mut("poll").unwrap();
    input
        .ports
        .as_mut()
        .unwrap()
        .events
        .get_mut("updated")
        .unwrap()
        .schema = text_schema();
    let path = write_package(root.path(), source, &manifest);
    let error = load_program_module_package(&path, root.path(), None).unwrap_err();
    assert!(format!("{error:#}").contains("schema must match its decoded handler output"));

    let mut manifest = service_package(source);
    manifest.exports.get_mut("poll").unwrap().ports = None;
    let path = write_package(root.path(), source, &manifest);
    let error = load_program_module_package(&path, root.path(), None).unwrap_err();
    assert!(format!("{error:#}").contains("requires typed input and event ports"));

    let argv_source = "accept threshold as integer from argument 1\nevery 1 second\n  run \"printf\" with arguments [\"{\\\"count\\\":1}\"]\n";
    let (path, _) = load_service_package(root.path(), argv_source);
    let error = load_program_module_package(&path, root.path(), None).unwrap_err();
    assert!(format!("{error:#}").contains("must be supplied as a startup port value"));
}

#[test]
fn service_event_uses_the_canonical_decoded_result_envelope_schema() {
    let source = service_source();
    let root = tempdir().unwrap();
    let mut manifest = service_package(source);
    let export = manifest.exports.get_mut("poll").unwrap();
    export.result_envelope = true;
    export
        .ports
        .as_mut()
        .unwrap()
        .events
        .get_mut("updated")
        .unwrap()
        .schema = result_envelope_port_schema();
    let path = write_package(root.path(), source, &manifest);

    let package = load_program_module_package(&path, root.path(), None).unwrap();
    let export = package.export("poll").unwrap().export;
    let reference = ProgramModule {
        source: PathBuf::from("../package/src/service.program"),
        entry: "poll".into(),
        interface_version: PROGRAM_MODULE_SERVICE_INTERFACE_VERSION,
        package_manifest: None,
        package_lock: None,
    };
    let contract: AppletContract =
        serde_json::from_value(contract_json(&reference, &export)).unwrap();
    assert_eq!(
        contract.result_envelope_schema(),
        Some(result_envelope_port_schema())
    );
}

#[test]
fn interface_lifecycle_pairing_is_explicit_and_package_ports_match_contract_exactly() {
    let valid_v2 = json!({
        "name":"service",
        "binary":"@program-module",
        "parameters":{"threshold":{"type":"integer","required":true}},
        "arguments":[{"parameter":"threshold"}],
        "contract":{
            "version":1,
            "backend":"program_module",
            "program":{"source":"service.program","entry":"main","interface_version":2},
            "lifecycle":{"kind":"program_service"},
            "output":{"type":"record","encoding":"json","fields":{"count":{"type":"integer"}},"required":["count"]},
            "ports":{
                "inputs":{"threshold":{"schema":{"type":"integer"},"delivery":"single"}},
                "events":{"updated":{"schema":{"type":"record","fields":{"count":{"type":"integer"}},"required":["count"]},"delivery":"events"}}
            }
        }
    });
    let catalog = tempdir().unwrap();
    let manifest_path = catalog.path().join("service.json");
    fs::write(
        &manifest_path,
        serde_json::to_vec_pretty(&valid_v2).unwrap(),
    )
    .unwrap();
    if let Err(error) = Registry::load(catalog.path()) {
        panic!("{error:#}");
    }

    let mut v2_finite = valid_v2.clone();
    v2_finite["contract"]["lifecycle"] = json!({"kind":"oneshot"});
    fs::write(
        &manifest_path,
        serde_json::to_vec_pretty(&v2_finite).unwrap(),
    )
    .unwrap();
    assert!(Registry::load(catalog.path()).is_err());
    let mut v1_service = valid_v2.clone();
    v1_service["contract"]["program"]["interface_version"] = json!(1);
    fs::write(
        &manifest_path,
        serde_json::to_vec_pretty(&v1_service).unwrap(),
    )
    .unwrap();
    assert!(Registry::load(catalog.path()).is_err());

    let root = tempdir().unwrap();
    let (manifest_path, package_manifest) = load_service_package(root.path(), service_source());
    let package = load_program_module_package(&manifest_path, root.path(), None).unwrap();
    let applet_manifest = root.path().join("applets/applet.json");
    fs::create_dir_all(applet_manifest.parent().unwrap()).unwrap();
    fs::write(&applet_manifest, "{}").unwrap();
    let reference = ProgramModule {
        source: PathBuf::from("../package/src/service.program"),
        entry: "poll".into(),
        interface_version: PROGRAM_MODULE_SERVICE_INTERFACE_VERSION,
        package_manifest: Some(PathBuf::from("../package/program.modules.json")),
        package_lock: None,
    };
    // Package descriptors require a manifest and lock together, so write the
    // exact verified lock snapshot before loading the descriptor.
    let lock = package.lock_snapshot(root.path()).unwrap();
    fs::write(
        root.path().join("package/program.modules.lock.json"),
        serde_json::to_vec_pretty(&lock).unwrap(),
    )
    .unwrap();
    let mut reference = reference;
    reference.package_lock = Some(PathBuf::from("../package/program.modules.lock.json"));
    let loaded = load_program_module(&reference, &applet_manifest, root.path()).unwrap();
    let export = loaded.export.as_ref().unwrap();
    let contract: AppletContract =
        serde_json::from_value(contract_json(&loaded.reference, export)).unwrap();
    loaded.validate_contract(&contract).unwrap();

    let mut changed = contract.clone();
    changed
        .ports
        .as_mut()
        .unwrap()
        .events
        .get_mut("updated")
        .unwrap()
        .schema = text_schema();
    assert!(loaded.validate_contract(&changed).is_err());

    // Missing v1 export ports remain compatible with older contracts that
    // independently declared graph ports.
    let finite_source = "define a program named summarize\n  accept threshold as integer\n  then encode threshold as json\n";
    let mut finite_package = service_package(finite_source);
    finite_package.name = "example.finite".into();
    finite_package.interface_version = PROGRAM_MODULE_INTERFACE_VERSION;
    let finite_export = finite_package.exports.get_mut("poll").unwrap();
    finite_export.interface_version = PROGRAM_MODULE_INTERFACE_VERSION;
    finite_export.entry = "summarize".into();
    finite_export.lifecycle = ModuleLifecycle::OneShot;
    finite_export.ports = None;
    let finite_path = write_package(root.path(), finite_source, &finite_package);
    let _ = package_manifest;
    let finite_loaded_package =
        load_program_module_package(&finite_path, root.path(), None).unwrap();
    let finite_lock = finite_loaded_package.lock_snapshot(root.path()).unwrap();
    fs::write(
        root.path().join("package/program.modules.lock.json"),
        serde_json::to_vec_pretty(&finite_lock).unwrap(),
    )
    .unwrap();
    let finite_manifest = root.path().join("applets/finite.json");
    fs::write(&finite_manifest, "{}").unwrap();
    let finite_reference = ProgramModule {
        source: PathBuf::from("../package/src/service.program"),
        entry: "poll".into(),
        interface_version: PROGRAM_MODULE_INTERFACE_VERSION,
        package_manifest: Some(PathBuf::from("../package/program.modules.json")),
        package_lock: Some(PathBuf::from("../package/program.modules.lock.json")),
    };
    let finite = load_program_module(&finite_reference, &finite_manifest, root.path()).unwrap();
    let finite_export = finite.export.as_ref().unwrap();
    let mut legacy_contract_json = contract_json(&finite.reference, finite_export);
    legacy_contract_json["lifecycle"] = json!({"kind":"oneshot"});
    legacy_contract_json["ports"] = json!({
        "inputs":{"threshold":{"schema":{"type":"integer"},"delivery":"single"}}
    });
    let legacy_contract: AppletContract = serde_json::from_value(legacy_contract_json).unwrap();
    finite.validate_contract(&legacy_contract).unwrap();
}

#[test]
fn scheduled_modules_are_rejected_from_finite_program_calls_during_preflight() {
    let root = tempdir().unwrap();
    let source = service_source();
    fs::write(root.path().join("service.program"), source).unwrap();
    let reference = ProgramModule {
        source: PathBuf::from("service.program"),
        entry: PROGRAM_MODULE_SERVICE_ROOT_ENTRY.into(),
        interface_version: PROGRAM_MODULE_SERVICE_INTERFACE_VERSION,
        package_manifest: None,
        package_lock: None,
    };
    let export = service_package(source).exports.get("poll").unwrap().clone();
    let applet_manifest = json!({
        "name": "test.poller",
        "binary": "@program-module",
        "parameters": {"threshold": {"type": "integer", "required": true}},
        "arguments": [{"parameter": "threshold"}],
        "contract": contract_json(&reference, &export)
    });
    fs::write(
        root.path().join("poller.json"),
        serde_json::to_vec_pretty(&applet_manifest).unwrap(),
    )
    .unwrap();
    let registry = Registry::load(root.path()).unwrap();
    let finite_program = flow::parser::parse("run test.poller with threshold 10\n").unwrap();

    let error = ProgramModuleSession::preload_program(&finite_program, &registry).unwrap_err();
    assert!(format!("{error:#}")
        .contains("can run only as a graph service node, not as a finite program call"));
}
