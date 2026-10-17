use flow::contracts::{OutputEncoding, OutputSchema, OutputType, ValueSchema};
use flow::modules::{
    load_program_module, load_program_module_package, ModuleLifecycle, ProgramModule,
    ProgramModuleDependency, ProgramModuleExport, ProgramModuleInput, ProgramModuleInputType,
    ProgramModulePackageManifest, ProgramModuleSession, PROGRAM_MODULE_INTERFACE_VERSION,
    PROGRAM_MODULE_PACKAGE_FORMAT,
};
use sha2::{Digest, Sha256};
use std::collections::BTreeMap;
use std::fs;
use std::path::{Path, PathBuf};
use tempfile::tempdir;

fn digest(bytes: &[u8]) -> String {
    format!("{:x}", Sha256::digest(bytes))
}

fn write_direct_applet(dir: &Path, name: &str, source: &str) {
    fs::create_dir_all(dir).unwrap();
    let source_name = format!("{}.program", name.replace('.', "_"));
    fs::write(dir.join(&source_name), source).unwrap();
    let manifest = serde_json::json!({
        "name": name,
        "binary": "@program-module",
        "accepts_stdin": false,
        "parameters": {},
        "arguments": [],
        "contract": {
            "version": 1,
            "backend": "program_module",
            "program": {
                "source": source_name,
                "entry": "entry",
                "interface_version": 1
            },
            "lifecycle": { "kind": "oneshot" },
            "output": { "type": "text", "encoding": "json" }
        }
    });
    fs::write(
        dir.join(format!("{name}.json")),
        serde_json::to_vec_pretty(&manifest).unwrap(),
    )
    .unwrap();
}

fn record_output() -> OutputSchema {
    OutputSchema {
        kind: OutputType::Record,
        encoding: OutputEncoding::Json,
        enum_values: None,
        items: None,
        fields: BTreeMap::from([
            (
                "name".into(),
                ValueSchema {
                    kind: OutputType::Text,
                    nullable: false,
                    enum_values: None,
                    items: None,
                    fields: BTreeMap::new(),
                    required: Vec::new(),
                },
            ),
            (
                "count".into(),
                ValueSchema {
                    kind: OutputType::Integer,
                    nullable: false,
                    enum_values: None,
                    items: None,
                    fields: BTreeMap::new(),
                    required: Vec::new(),
                },
            ),
        ]),
        required: vec!["name".into(), "count".into()],
        trim_final_lf: false,
    }
}

fn package(source: &str, name: &str, version: &str) -> ProgramModulePackageManifest {
    ProgramModulePackageManifest {
        format_version: PROGRAM_MODULE_PACKAGE_FORMAT,
        name: name.into(),
        version: version.into(),
        interface_version: PROGRAM_MODULE_INTERFACE_VERSION,
        source: PathBuf::from("src/library.program"),
        source_sha256: digest(source.as_bytes()),
        exports: BTreeMap::from([(
            "describe".into(),
            ProgramModuleExport {
                entry: "describe".into(),
                interface_version: PROGRAM_MODULE_INTERFACE_VERSION,
                inputs: vec![ProgramModuleInput {
                    name: "payload".into(),
                    kind: ProgramModuleInputType::Json,
                    required: true,
                }],
                output: record_output(),
                outcomes: Vec::new(),
                result_envelope: false,
                lifecycle: ModuleLifecycle::OneShot,
                capabilities: Vec::new(),
                effects: Vec::new(),
                ports: None,
            },
        )]),
        dependencies: BTreeMap::new(),
    }
}

fn write_package(dir: &Path, source: &str, name: &str, version: &str) -> PathBuf {
    fs::create_dir_all(dir.join("src")).unwrap();
    fs::write(dir.join("src/library.program"), source).unwrap();
    let path = dir.join("program.modules.json");
    fs::write(
        &path,
        serde_json::to_vec_pretty(&package(source, name, version)).unwrap(),
    )
    .unwrap();
    path
}

#[test]
fn versioned_module_exports_typed_record_and_resolves_source_from_manifest_origin() {
    let temp = tempdir().unwrap();
    let root = temp.path().join("package-root");
    let source = "define a program named describe\n  accept payload as json\n  then encode payload as json\n";
    let package_path = write_package(&root.join("libs/report"), source, "example.report", "1.2.3");
    let loaded = load_program_module_package(&package_path, &root, None).unwrap();
    let export = loaded.export("describe").unwrap();
    assert_eq!(export.package_name, "example.report");
    assert_eq!(export.package_version, "1.2.3");
    assert_eq!(
        export.finite_definition().unwrap().inputs[0].kind,
        flow::ast::InputType::Json
    );
    assert_eq!(export.export.output.kind, OutputType::Record);
    assert_eq!(export.export.output.required, ["name", "count"]);

    fs::create_dir_all(root.join("applets")).unwrap();
    let applet_manifest = root.join("applets/report.json");
    fs::write(&applet_manifest, "{}").unwrap();
    let direct = load_program_module(
        &ProgramModule {
            source: PathBuf::from("../libs/report/src/library.program"),
            entry: "describe".into(),
            interface_version: 1,
            package_manifest: None,
            package_lock: None,
        },
        &applet_manifest,
        &root,
    )
    .unwrap();
    assert_eq!(direct.source_sha256, digest(source.as_bytes()));
    assert_eq!(
        direct.finite_definition().unwrap().inputs[0].name,
        "payload"
    );
}

#[test]
fn tampered_source_and_mismatched_export_contract_are_rejected() {
    let temp = tempdir().unwrap();
    let root = temp.path().join("modules");
    let source = "define a program named describe\n  accept payload as json\n  then encode payload as json\n";
    let manifest_path = write_package(&root.join("report"), source, "example.report", "1.2.3");
    let loaded = load_program_module_package(&manifest_path, &root, None).unwrap();
    assert!(loaded.export("missing").is_err());

    let source_path = root.join("report/src/library.program");
    fs::write(&source_path, format!("{source}\n")).unwrap();
    let error = load_program_module_package(&manifest_path, &root, None).unwrap_err();
    assert!(format!("{error:#}").contains("source SHA-256 mismatch"));
}

#[test]
fn dependency_semver_interface_and_cycle_checks_are_local_and_bounded() {
    let temp = tempdir().unwrap();
    let root = temp.path().join("modules");
    let source = "define a program named describe\n  accept payload as json\n  then encode payload as json\n";
    let first_path = write_package(&root.join("first"), source, "example.first", "1.2.3");
    let second_path = write_package(&root.join("second"), source, "example.second", "1.2.4");
    let mut second: ProgramModulePackageManifest =
        serde_json::from_slice(&fs::read(&second_path).unwrap()).unwrap();
    second.dependencies.insert(
        "first".into(),
        ProgramModuleDependency {
            manifest: PathBuf::from("../first/program.modules.json"),
            source_sha256: digest(source.as_bytes()),
            manifest_sha256: digest(&fs::read(&first_path).unwrap()),
            version: "^1.2.0".into(),
            interface_min: 1,
            interface_max_exclusive: 2,
        },
    );
    fs::write(&second_path, serde_json::to_vec(&second).unwrap()).unwrap();
    let loaded = load_program_module_package(&second_path, &root, None).unwrap();
    assert!(loaded.dependencies.contains_key("first"));
    assert_eq!(
        loaded.dependencies["first"].manifest_path,
        fs::canonicalize(&first_path).unwrap()
    );
    assert_eq!(
        loaded
            .export("describe")
            .unwrap()
            .transitive_fingerprints
            .len(),
        2
    );

    second.dependencies.get_mut("first").unwrap().interface_min = 2;
    second
        .dependencies
        .get_mut("first")
        .unwrap()
        .interface_max_exclusive = 3;
    fs::write(&second_path, serde_json::to_vec(&second).unwrap()).unwrap();
    let error = load_program_module_package(&second_path, &root, None).unwrap_err();
    assert!(format!("{error:#}").contains("interface version in 2..3"));

    second.dependencies.get_mut("first").unwrap().interface_min = 1;
    second
        .dependencies
        .get_mut("first")
        .unwrap()
        .interface_max_exclusive = 2;
    let mut first: ProgramModulePackageManifest =
        serde_json::from_slice(&fs::read(&first_path).unwrap()).unwrap();
    first.dependencies.insert(
        "second".into(),
        ProgramModuleDependency {
            manifest: PathBuf::from("../second/program.modules.json"),
            source_sha256: digest(source.as_bytes()),
            manifest_sha256: "0".repeat(64),
            version: "^1.2.0".into(),
            interface_min: 1,
            interface_max_exclusive: 2,
        },
    );
    fs::write(&first_path, serde_json::to_vec(&first).unwrap()).unwrap();
    fs::write(&second_path, serde_json::to_vec(&second).unwrap()).unwrap();
    let error = load_program_module_package(&second_path, &root, None).unwrap_err();
    assert!(format!("{error:#}").contains("program module dependency cycle"));
}

#[test]
fn module_lock_rejects_source_changes_and_unsupported_lifecycle_before_execution() {
    let temp = tempdir().unwrap();
    let root = temp.path().join("modules");
    let source = "define a program named describe\n  accept payload as json\n  then encode payload as json\n";
    let path = write_package(&root.join("report"), source, "example.report", "1.2.3");
    let loaded = load_program_module_package(&path, &root, None).unwrap();
    let lock = root.join("program.modules.lock.json");
    fs::write(
        &lock,
        serde_json::to_vec_pretty(&loaded.lock_snapshot(&root).unwrap()).unwrap(),
    )
    .unwrap();
    assert!(load_program_module_package(&path, &root, Some(&lock)).is_ok());

    let mut changed: ProgramModulePackageManifest =
        serde_json::from_slice(&fs::read(&path).unwrap()).unwrap();
    changed.exports.get_mut("describe").unwrap().lifecycle = ModuleLifecycle::Listener;
    fs::write(&path, serde_json::to_vec(&changed).unwrap()).unwrap();
    let error = load_program_module_package(&path, &root, Some(&lock)).unwrap_err();
    assert!(format!("{error:#}").contains("unsupported Listener lifecycle"));
}

#[test]
fn checked_in_module_example_has_a_valid_catalog_and_lock() {
    let root = Path::new(env!("CARGO_MANIFEST_DIR")).join("examples/modules");
    let package_manifest = root.join("package/program.modules.json");
    let lock_path = root.join("package/program.modules.lock.json");
    let package = load_program_module_package(&package_manifest, &root, Some(&lock_path)).unwrap();
    assert!(package.export("report").is_ok());

    let registry = flow::manifest::Registry::load(&root).unwrap();
    let caller =
        flow::parser::parse(&fs::read_to_string(root.join("caller.program")).unwrap()).unwrap();
    let session = ProgramModuleSession::preload_program(&caller, &registry).unwrap();
    let applet = registry.resolve("examples.records.report").unwrap();
    assert!(session.get_for_manifest(&registry, applet).is_some());
}

#[test]
fn package_descriptor_uses_manifest_origin_lock_and_transitive_checkpoint_identity() {
    let temp = tempdir().unwrap();
    let root = temp.path().join("modules");
    let applet_dir = root.join("applets");
    let main_dir = root.join("main");
    let dependency_dir = root.join("dependency");
    fs::create_dir_all(&applet_dir).unwrap();

    let main_source = "define a program named describe\n  accept payload as json\n  then encode payload as json\n";
    let dependency_source = "define a program named describe\n  accept payload as json\n  then encode payload as json\n";
    let main_manifest_path = write_package(&main_dir, main_source, "example.main", "1.4.0");
    let dependency_manifest_path = write_package(
        &dependency_dir,
        dependency_source,
        "example.dependency",
        "1.2.3",
    );

    let mut main: ProgramModulePackageManifest =
        serde_json::from_slice(&fs::read(&main_manifest_path).unwrap()).unwrap();
    main.dependencies.insert(
        "dependency".into(),
        ProgramModuleDependency {
            manifest: PathBuf::from("../dependency/program.modules.json"),
            source_sha256: digest(dependency_source.as_bytes()),
            manifest_sha256: digest(&fs::read(&dependency_manifest_path).unwrap()),
            version: "^1.2.0".into(),
            interface_min: 1,
            interface_max_exclusive: 2,
        },
    );
    fs::write(
        &main_manifest_path,
        serde_json::to_vec_pretty(&main).unwrap(),
    )
    .unwrap();

    let lock_path = main_dir.join("program.modules.lock.json");
    let loaded_package = load_program_module_package(&main_manifest_path, &root, None).unwrap();
    fs::write(
        &lock_path,
        serde_json::to_vec_pretty(&loaded_package.lock_snapshot(&root).unwrap()).unwrap(),
    )
    .unwrap();

    let applet_manifest_path = applet_dir.join("report.json");
    fs::write(&applet_manifest_path, "{}").unwrap();
    let reference = ProgramModule {
        source: PathBuf::from("../main/src/library.program"),
        entry: "describe".into(),
        interface_version: 1,
        package_manifest: Some(PathBuf::from("../main/program.modules.json")),
        package_lock: Some(PathBuf::from("../main/program.modules.lock.json")),
    };
    let first = load_program_module(&reference, &applet_manifest_path, &root).unwrap();
    assert_eq!(first.transitive_fingerprints.len(), 2);

    let changed_dependency_source = format!("{dependency_source}\n");
    fs::write(
        dependency_dir.join("src/library.program"),
        &changed_dependency_source,
    )
    .unwrap();
    let mut dependency: ProgramModulePackageManifest =
        serde_json::from_slice(&fs::read(&dependency_manifest_path).unwrap()).unwrap();
    dependency.source_sha256 = digest(changed_dependency_source.as_bytes());
    fs::write(
        &dependency_manifest_path,
        serde_json::to_vec_pretty(&dependency).unwrap(),
    )
    .unwrap();
    main.dependencies
        .get_mut("dependency")
        .unwrap()
        .source_sha256 = dependency.source_sha256.clone();
    main.dependencies
        .get_mut("dependency")
        .unwrap()
        .manifest_sha256 = digest(&fs::read(&dependency_manifest_path).unwrap());
    fs::write(
        &main_manifest_path,
        serde_json::to_vec_pretty(&main).unwrap(),
    )
    .unwrap();

    let stale = load_program_module(&reference, &applet_manifest_path, &root).unwrap_err();
    assert!(format!("{stale:#}").contains("lock does not match"));
    let refreshed = load_program_module_package(&main_manifest_path, &root, None).unwrap();
    fs::write(
        &lock_path,
        serde_json::to_vec_pretty(&refreshed.lock_snapshot(&root).unwrap()).unwrap(),
    )
    .unwrap();
    let second = load_program_module(&reference, &applet_manifest_path, &root).unwrap();
    assert_ne!(first.checkpoint_fingerprint, second.checkpoint_fingerprint);
}

#[cfg(unix)]
#[test]
fn changed_transitive_module_source_rejects_resume_before_replaying_completed_effects() {
    let temp = tempdir().unwrap();
    let catalog = temp.path().join("catalog");
    let root_dir = catalog.join("packages/root");
    let dependency_dir = catalog.join("packages/dependency");
    fs::create_dir_all(&catalog).unwrap();

    let root_source = "define a program named describe\n  accept payload as json\n  then encode payload as json\n";
    let dependency_source = "define a program named describe\n  accept payload as json\n  then encode payload as json\n";
    let root_manifest_path = write_package(&root_dir, root_source, "example.root", "1.0.0");
    let dependency_manifest_path = write_package(
        &dependency_dir,
        dependency_source,
        "example.dependency",
        "1.2.3",
    );
    let mut root_package: ProgramModulePackageManifest =
        serde_json::from_slice(&fs::read(&root_manifest_path).unwrap()).unwrap();
    root_package.dependencies.insert(
        "lib".into(),
        ProgramModuleDependency {
            manifest: PathBuf::from("../dependency/program.modules.json"),
            source_sha256: digest(dependency_source.as_bytes()),
            manifest_sha256: digest(&fs::read(&dependency_manifest_path).unwrap()),
            version: "^1.2.0".into(),
            interface_min: 1,
            interface_max_exclusive: 2,
        },
    );
    fs::write(
        &root_manifest_path,
        serde_json::to_vec_pretty(&root_package).unwrap(),
    )
    .unwrap();
    let lock_path = root_dir.join("program.modules.lock.json");
    let loaded = load_program_module_package(&root_manifest_path, &catalog, None).unwrap();
    fs::write(
        &lock_path,
        serde_json::to_vec_pretty(&loaded.lock_snapshot(&catalog).unwrap()).unwrap(),
    )
    .unwrap();

    let applet_manifest = serde_json::json!({
        "name": "test.root",
        "binary": "@program-module",
        "accepts_stdin": false,
        "parameters": {"payload": {"type": "json", "required": true}},
        "arguments": [],
        "contract": {
            "version": 1,
            "backend": "program_module",
            "program": {
                "source": "packages/root/src/library.program",
                "entry": "describe",
                "interface_version": 1,
                "package_manifest": "packages/root/program.modules.json",
                "package_lock": "packages/root/program.modules.lock.json"
            },
            "lifecycle": {"kind": "oneshot"},
            "output": serde_json::to_value(record_output()).unwrap()
        }
    });
    fs::write(
        catalog.join("test.root.json"),
        serde_json::to_vec_pretty(&applet_manifest).unwrap(),
    )
    .unwrap();
    let registry = flow::manifest::Registry::load(&catalog).unwrap();
    let cwd = fs::canonicalize(&temp).unwrap();
    let program_source = "accept payload as json\nrun \"sh\" with arguments [\"-c\", \"printf x >> replay.log\"]\nrun test.root with payload payload as result\nthen encode result as json\n";
    let program_path = cwd.join("main.program");
    fs::write(&program_path, program_source).unwrap();
    let program = flow::parser::parse(program_source).unwrap();
    let options = flow::runtime::RunOptions {
        inputs: BTreeMap::from([(
            "payload".into(),
            flow::ast::Value::Record(BTreeMap::from([
                ("count".into(), flow::ast::Value::Integer(5)),
                ("name".into(), flow::ast::Value::String("stable".into())),
            ])),
        )]),
        working_directory: cwd.clone(),
        cache_directory: None,
        trace: false,
        max_output_bytes: 1024 * 1024,
        command_timeout: None,
        total_timeout: None,
    };
    let checkpoint = flow::runtime::CheckpointOptions {
        directory: cwd.join("journal"),
        program_path,
        resume: false,
        retry_uncertain: false,
    };
    flow::runtime::run_checkpointed(&program, &registry, &options, &checkpoint).unwrap();
    assert_eq!(fs::read(cwd.join("replay.log")).unwrap(), b"x");

    let changed_dependency_source = format!("{dependency_source}\n");
    fs::write(
        dependency_dir.join("src/library.program"),
        &changed_dependency_source,
    )
    .unwrap();
    let mut dependency_package: ProgramModulePackageManifest =
        serde_json::from_slice(&fs::read(&dependency_manifest_path).unwrap()).unwrap();
    dependency_package.source_sha256 = digest(changed_dependency_source.as_bytes());
    fs::write(
        &dependency_manifest_path,
        serde_json::to_vec_pretty(&dependency_package).unwrap(),
    )
    .unwrap();
    root_package
        .dependencies
        .get_mut("lib")
        .unwrap()
        .source_sha256 = dependency_package.source_sha256.clone();
    root_package
        .dependencies
        .get_mut("lib")
        .unwrap()
        .manifest_sha256 = digest(&fs::read(&dependency_manifest_path).unwrap());
    fs::write(
        &root_manifest_path,
        serde_json::to_vec_pretty(&root_package).unwrap(),
    )
    .unwrap();
    let refreshed = load_program_module_package(&root_manifest_path, &catalog, None).unwrap();
    fs::write(
        &lock_path,
        serde_json::to_vec_pretty(&refreshed.lock_snapshot(&catalog).unwrap()).unwrap(),
    )
    .unwrap();

    let resume = flow::runtime::CheckpointOptions {
        resume: true,
        ..checkpoint
    };
    let error = match flow::runtime::run_checkpointed(&program, &registry, &options, &resume) {
        Ok(_) => panic!("changed transitive module source unexpectedly resumed"),
        Err(error) => error,
    };
    assert!(format!("{error:#}").contains("completed checkpoint node"));
    assert_eq!(fs::read(cwd.join("replay.log")).unwrap(), b"x");
}

#[test]
fn execution_session_preloads_manifest_relative_modules_and_exposes_checkpoint_identity() {
    let temp = tempdir().unwrap();
    let catalog = temp.path().join("applets");
    write_direct_applet(
        &catalog,
        "test.module",
        "define a program named entry\n  then encode \"ready\" as json\n",
    );
    let registry = flow::manifest::Registry::load(&catalog).unwrap();
    let caller = flow::parser::parse("run test.module\n").unwrap();

    let session = ProgramModuleSession::preload_program(&caller, &registry).unwrap();
    let manifest = registry.resolve("test.module").unwrap();
    let loaded = session.get_for_manifest(&registry, manifest).unwrap();
    assert_eq!(
        loaded.source_path,
        fs::canonicalize(catalog.join("test_module.program")).unwrap()
    );
    assert_eq!(session.fingerprints().len(), 1);
    assert_eq!(
        session.fingerprints().values().next().unwrap(),
        &loaded.checkpoint_fingerprint
    );
    assert!(session.body_invocations().is_empty());
}

#[cfg(unix)]
#[test]
fn module_private_bindings_and_folder_changes_do_not_escape_to_caller() {
    let temp = tempdir().unwrap();
    let catalog = temp.path().join("catalog");
    let working_dir = temp.path().join("caller");
    fs::create_dir_all(working_dir.join("sub")).unwrap();
    let cwd = fs::canonicalize(working_dir).unwrap();
    write_direct_applet(
        &catalog,
        "test.scope",
        "define a program named entry\n  use folder \"sub\"\n  run \"pwd\" as path\n  then encode path as json\n",
    );
    let registry = flow::manifest::Registry::load(&catalog).unwrap();
    let caller = flow::parser::parse(
        "accept path as text with default \"caller-local\"\nrun \"pwd\" as before\nrun test.scope as result\nrun \"pwd\" as caller_directory\nthen \"printf\" with arguments [\"%s|%s|%s|%s\", before, result, path, caller_directory]\n",
    )
    .unwrap();
    let options = flow::runtime::RunOptions {
        inputs: BTreeMap::new(),
        working_directory: cwd.clone(),
        cache_directory: None,
        trace: false,
        max_output_bytes: 1024 * 1024,
        command_timeout: None,
        total_timeout: None,
    };
    let result = flow::runtime::run(&caller, &registry, &options).unwrap();
    let expected = format!(
        "{}\n|{}\n|caller-local|{}\n",
        cwd.display(),
        cwd.join("sub").display(),
        cwd.display()
    );
    assert_eq!(result.output, expected.as_bytes());
}

#[test]
fn checkpoint_rejects_cwd_changing_module_child_calls_before_effects() {
    let temp = tempdir().unwrap();
    let catalog = temp.path().join("catalog");
    let cwd = temp.path().join("caller");
    fs::create_dir_all(cwd.join("sub")).unwrap();
    write_direct_applet(
        &catalog,
        "test.cwd",
        "define a program named entry\n  use folder \"sub\"\n  run \"pwd\" as location\n  then encode location as json\n",
    );
    let registry = flow::manifest::Registry::load(&catalog).unwrap();
    let source = "run \"sh\" with arguments [\"-c\", \"printf x >> replay.log\"]\nrun test.cwd\n";
    let program_path = cwd.join("checkpoint.program");
    fs::write(&program_path, source).unwrap();
    let program = flow::parser::parse(source).unwrap();
    let options = flow::runtime::RunOptions {
        inputs: BTreeMap::new(),
        working_directory: fs::canonicalize(&cwd).unwrap(),
        cache_directory: None,
        trace: false,
        max_output_bytes: 1024 * 1024,
        command_timeout: None,
        total_timeout: None,
    };
    let checkpoint = flow::runtime::CheckpointOptions {
        directory: cwd.join("journal"),
        program_path,
        resume: false,
        retry_uncertain: false,
    };

    let error = flow::runtime::run_checkpointed(&program, &registry, &options, &checkpoint)
        .err()
        .expect("checkpointing must reject cwd-dependent child identity");
    assert!(format!("{error:#}").contains("unpinned body applets"));
    assert!(
        !cwd.join("replay.log").exists(),
        "identity rejection must happen before the earlier effect"
    );
}

#[test]
fn module_cache_key_tracks_resolved_typed_parameters() {
    let temp = tempdir().unwrap();
    let catalog = temp.path().join("catalog");
    fs::create_dir_all(&catalog).unwrap();
    fs::write(
        catalog.join("cache.program"),
        "define a program named entry\n  accept payload as text\n  then encode payload as json\n",
    )
    .unwrap();
    let manifest = serde_json::json!({
        "name": "test.cache",
        "binary": "@program-module",
        "cacheable": true,
        "accepts_stdin": false,
        "parameters": {"payload": {"type": "string", "required": true}},
        "arguments": [],
        "contract": {
            "version": 1,
            "backend": "program_module",
            "program": {
                "source": "cache.program",
                "entry": "entry",
                "interface_version": 1
            },
            "lifecycle": {"kind": "oneshot"},
            "output": {"type": "text", "encoding": "json"}
        }
    });
    fs::write(
        catalog.join("test.cache.json"),
        serde_json::to_vec_pretty(&manifest).unwrap(),
    )
    .unwrap();

    let registry = flow::manifest::Registry::load(&catalog).unwrap();
    let caller = flow::parser::parse(
        "run test.cache with payload \"A\" as first and cache\nrun test.cache with payload \"B\" as second and cache\nrun test.cache with payload \"B\" as third and cache\nthen encode second as json\n",
    )
    .unwrap();
    let cwd = fs::canonicalize(&temp).unwrap();
    let options = flow::runtime::RunOptions {
        inputs: BTreeMap::new(),
        working_directory: cwd.clone(),
        cache_directory: Some(cwd.join("cache")),
        trace: false,
        max_output_bytes: 1024 * 1024,
        command_timeout: None,
        total_timeout: None,
    };

    let result = flow::runtime::run(&caller, &registry, &options).unwrap();
    assert_eq!(result.output, br#""B""#);
    assert_eq!(
        result.cache_hits, 1,
        "A then B must miss; the repeated B call should hit"
    );
}

#[cfg(unix)]
#[test]
fn changed_body_applet_invalidates_module_cache_and_checkpoint_resume() {
    use std::os::unix::fs::PermissionsExt;

    let temp = tempdir().unwrap();
    let catalog = temp.path().join("catalog");
    fs::create_dir_all(&catalog).unwrap();
    let helper = catalog.join("helper.sh");
    let write_helper = |contents: &str| {
        fs::write(&helper, format!("#!/bin/sh\nprintf {contents}\n")).unwrap();
        fs::set_permissions(&helper, fs::Permissions::from_mode(0o755)).unwrap();
    };
    write_helper("first");
    fs::write(
        catalog.join("test.helper.json"),
        serde_json::to_vec_pretty(&serde_json::json!({
            "name": "test.helper",
            "binary": helper,
            "parameters": {},
            "arguments": []
        }))
        .unwrap(),
    )
    .unwrap();
    write_direct_applet(
        &catalog,
        "test.body",
        "define a program named entry\n  run test.helper as answer\n  then encode answer as json\n",
    );
    let mut module_manifest: serde_json::Value =
        serde_json::from_slice(&fs::read(catalog.join("test.body.json")).unwrap()).unwrap();
    module_manifest["cacheable"] = true.into();
    fs::write(
        catalog.join("test.body.json"),
        serde_json::to_vec_pretty(&module_manifest).unwrap(),
    )
    .unwrap();

    let registry = flow::manifest::Registry::load(&catalog).unwrap();
    let cwd = fs::canonicalize(&temp).unwrap();
    let cached_caller = flow::parser::parse("run test.body as output and cache\n").unwrap();
    let options = flow::runtime::RunOptions {
        inputs: BTreeMap::new(),
        working_directory: cwd.clone(),
        cache_directory: Some(cwd.join("cache")),
        trace: false,
        max_output_bytes: 1024 * 1024,
        command_timeout: None,
        total_timeout: None,
    };
    let first = flow::runtime::run(&cached_caller, &registry, &options).unwrap();
    assert_eq!(first.output, br#""first""#);
    assert_eq!(first.cache_hits, 0);

    write_helper("second");
    let changed = flow::runtime::run(&cached_caller, &registry, &options).unwrap();
    assert_eq!(changed.output, br#""second""#);
    assert_eq!(
        changed.cache_hits, 0,
        "a changed helper must never return the prior module output"
    );

    let checkpoint_source = "run \"sh\" with arguments [\"-c\", \"printf x >> replay.log\"]\nrun test.body as output\nrun \"false\"\n";
    let program_path = cwd.join("checkpoint.program");
    fs::write(&program_path, checkpoint_source).unwrap();
    let checkpoint_program = flow::parser::parse(checkpoint_source).unwrap();
    let checkpoint = flow::runtime::CheckpointOptions {
        directory: cwd.join("journal"),
        program_path,
        resume: false,
        retry_uncertain: false,
    };
    assert!(
        flow::runtime::run_checkpointed(&checkpoint_program, &registry, &options, &checkpoint)
            .is_err()
    );
    assert_eq!(fs::read(cwd.join("replay.log")).unwrap(), b"x");

    write_helper("third");
    let resume = flow::runtime::CheckpointOptions {
        resume: true,
        ..checkpoint
    };
    let error = flow::runtime::run_checkpointed(&checkpoint_program, &registry, &options, &resume)
        .err()
        .expect("changed helper must invalidate the completed checkpoint node");
    assert!(format!("{error:#}").contains("completed checkpoint node"));
    assert_eq!(
        fs::read(cwd.join("replay.log")).unwrap(),
        b"x",
        "resume must reject the changed helper before replaying the completed effect"
    );
}

#[test]
fn nested_module_calls_are_rejected_during_preflight() {
    let temp = tempdir().unwrap();
    let catalog = temp.path().join("applets");
    write_direct_applet(
        &catalog,
        "test.inner",
        "define a program named entry\n  then encode \"inner\" as json\n",
    );
    write_direct_applet(
        &catalog,
        "test.outer",
        "define a program named entry\n  run test.inner\n",
    );
    let registry = flow::manifest::Registry::load(&catalog).unwrap();
    let caller = flow::parser::parse("run test.outer\n").unwrap();

    let error = ProgramModuleSession::preload_program(&caller, &registry).unwrap_err();
    assert!(format!("{error:#}").contains("ambient program module"));
}

#[test]
fn module_interface_mismatch_fails_before_an_earlier_save() {
    let temp = tempdir().unwrap();
    let catalog = temp.path().join("catalog");
    write_direct_applet(
        &catalog,
        "test.invalid",
        "define a program named entry\n  accept required as text\n  then encode required as json\n",
    );
    let registry = flow::manifest::Registry::load(&catalog).unwrap();
    let caller = flow::parser::parse(
        "run \"printf\" with arguments [\"must-not-run\"]\nthen save to \"forbidden.txt\"\nrun test.invalid\n",
    )
    .unwrap();
    let options = flow::runtime::RunOptions {
        inputs: BTreeMap::new(),
        working_directory: temp.path().to_path_buf(),
        cache_directory: None,
        trace: false,
        max_output_bytes: 1024 * 1024,
        command_timeout: None,
        total_timeout: None,
    };
    let error = match flow::runtime::run(&caller, &registry, &options) {
        Ok(_) => panic!("module interface mismatch unexpectedly executed"),
        Err(error) => error,
    };
    assert!(format!("{error:#}").contains("program module declares 1 inputs"));
    assert!(!temp.path().join("forbidden.txt").exists());
}

#[test]
fn package_alias_invokes_pinned_transitive_export_without_catalog_duplicate() {
    let temp = tempdir().unwrap();
    let catalog = temp.path().join("catalog");
    let root_dir = catalog.join("packages/root");
    let dependency_dir = catalog.join("packages/dependency");
    let shared_dir = catalog.join("packages/shared");
    fs::create_dir_all(&root_dir).unwrap();

    let shared_source =
        "define a program named pass\n  accept payload as json\n  then encode payload as json\n";
    let shared_manifest = write_package(&shared_dir, shared_source, "example.shared", "1.2.3");
    let mut shared_package: ProgramModulePackageManifest =
        serde_json::from_slice(&fs::read(&shared_manifest).unwrap()).unwrap();
    shared_package.exports.get_mut("describe").unwrap().entry = "pass".into();
    shared_package.exports.get_mut("describe").unwrap().output = record_output();
    fs::write(
        &shared_manifest,
        serde_json::to_vec_pretty(&shared_package).unwrap(),
    )
    .unwrap();

    let dependency_source = "define a program named relay\n  accept payload as json\n  parallel at most 2 as joined\n    run shared.describe with payload payload as left\n    run shared.describe with payload payload as right\n  then encode payload as json\n";
    let dependency_manifest = write_package(
        &dependency_dir,
        dependency_source,
        "example.dependency",
        "1.2.3",
    );
    let mut dependency_package: ProgramModulePackageManifest =
        serde_json::from_slice(&fs::read(&dependency_manifest).unwrap()).unwrap();
    dependency_package
        .exports
        .get_mut("describe")
        .unwrap()
        .entry = "relay".into();
    dependency_package
        .exports
        .get_mut("describe")
        .unwrap()
        .output = record_output();
    dependency_package.dependencies.insert(
        "shared".into(),
        ProgramModuleDependency {
            manifest: PathBuf::from("../shared/program.modules.json"),
            source_sha256: digest(shared_source.as_bytes()),
            manifest_sha256: digest(&fs::read(&shared_manifest).unwrap()),
            version: "^1.2.0".into(),
            interface_min: 1,
            interface_max_exclusive: 2,
        },
    );
    fs::write(
        &dependency_manifest,
        serde_json::to_vec_pretty(&dependency_package).unwrap(),
    )
    .unwrap();

    let root_source = "define a program named compose\n  accept payload as json\n  run lib.describe with payload payload as imported\n  then encode imported as json\n";
    let root_manifest = write_package(&root_dir, root_source, "example.root", "1.0.0");
    let mut root_package: ProgramModulePackageManifest =
        serde_json::from_slice(&fs::read(&root_manifest).unwrap()).unwrap();
    root_package.exports.get_mut("describe").unwrap().entry = "compose".into();
    root_package.exports.get_mut("describe").unwrap().output = record_output();
    root_package.dependencies.insert(
        "lib".into(),
        ProgramModuleDependency {
            manifest: PathBuf::from("../dependency/program.modules.json"),
            source_sha256: digest(dependency_source.as_bytes()),
            manifest_sha256: digest(&fs::read(&dependency_manifest).unwrap()),
            version: "^1.2.0".into(),
            interface_min: 1,
            interface_max_exclusive: 2,
        },
    );
    fs::write(
        &root_manifest,
        serde_json::to_vec_pretty(&root_package).unwrap(),
    )
    .unwrap();

    let loaded = load_program_module_package(&root_manifest, &catalog, None).unwrap();
    let lock_path = root_dir.join("program.modules.lock.json");
    fs::write(
        &lock_path,
        serde_json::to_vec_pretty(&loaded.lock_snapshot(&catalog).unwrap()).unwrap(),
    )
    .unwrap();
    let applet_manifest = serde_json::json!({
        "name": "test.root",
        "binary": "@program-module",
        "accepts_stdin": false,
        "parameters": {"payload": {"type": "json", "required": true}},
        "arguments": [],
        "contract": {
            "version": 1,
            "backend": "program_module",
            "program": {
                "source": "packages/root/src/library.program",
                "entry": "describe",
                "interface_version": 1,
                "package_manifest": "packages/root/program.modules.json",
                "package_lock": "packages/root/program.modules.lock.json"
            },
            "lifecycle": {"kind": "oneshot"},
            "output": serde_json::to_value(record_output()).unwrap()
        }
    });
    fs::write(
        catalog.join("test.root.json"),
        serde_json::to_vec_pretty(&applet_manifest).unwrap(),
    )
    .unwrap();

    // An ambient applet with the same canonical operation must not hijack the
    // package-local alias. The dependency export has no duplicate manifest.
    let ambient = serde_json::json!({
        "name": "lib.describe",
        "binary": "/definitely/not/run",
        "parameters": {},
        "arguments": []
    });
    fs::write(
        catalog.join("ambient.json"),
        serde_json::to_vec_pretty(&ambient).unwrap(),
    )
    .unwrap();

    let registry = flow::manifest::Registry::load(&catalog).unwrap();
    let caller = flow::parser::parse(
        "accept payload as json\nrun test.root with payload payload as result\nthen encode result as json\n",
    )
    .unwrap();
    let options = flow::runtime::RunOptions {
        inputs: BTreeMap::from([(
            "payload".into(),
            flow::ast::Value::Record(BTreeMap::from([
                ("count".into(), flow::ast::Value::Integer(5)),
                ("name".into(), flow::ast::Value::String("pinned".into())),
            ])),
        )]),
        working_directory: temp.path().to_path_buf(),
        cache_directory: None,
        trace: false,
        max_output_bytes: 1024 * 1024,
        command_timeout: None,
        total_timeout: None,
    };
    let result = flow::runtime::run(&caller, &registry, &options).unwrap();
    assert_eq!(result.output, br#"{"count":5,"name":"pinned"}"#);
}
