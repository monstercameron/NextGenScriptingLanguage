//! Issue #460: honest version-1 contracts for the shipped native tools.
//!
//! The 29 contract-less manifests defaulted to the `Command` backend with no
//! contract, so `command_for`/`preflight` refused them under any capability
//! policy. Every non-builtin shipped manifest now carries a v1 contract; the
//! `cd` builtin stays exempt because the host manages it without a contract.
//! Child command execution beyond the spawn itself (`awk` `system()`, GNU
//! `sed` `e`, `git.commit` hooks/GPG/editors) is a declared `process.exec`
//! effect, which the gate maps to `undeclared`, so untrusted use needs
//! explicit approval; prose alone would not gate anything. Per-endpoint
//! `network_origins` stay unenforceable for native children (a typed refusal),
//! so model tools keep the documented origin limitation. The bundled
//! `src/bundled_manifests/` artifacts are regenerated separately by the
//! coordinator (`EXECUTE_UPDATE_BUNDLED_INDEX=1 cargo test`).

use flow::{
    capabilities::{AppletGrant, CapabilityPolicy, ContractView, FilesystemGrant},
    contracts::{effect_category, AppletBackend, AppletOutput, OutcomeKind},
    manifest::Registry,
};
use std::collections::{BTreeMap, BTreeSet};
use std::path::Path;

/// (manifest file, expected capabilities, expected effects) for the 31 native
/// and model tools contracted by issue #460. Pure stdin-to-stdout filters need
/// process execution but promise no data effects; readers promise
/// `filesystem.read`; tools whose free-form programs or flag spreads can name
/// files promise reads and writes as their vectors require: sort/uniq operands
/// name input files and sort -o / uniq's second operand names an output file,
/// jq file flags and grep -f/-r read, and the jev model tools read
/// env_file/fixture files (see `src/models.rs`) on top of their model-endpoint
/// connection. `awk`, `sed`, and `git.commit` additionally execute child
/// commands beyond the spawn itself (awk `system()`, GNU sed `e`, git hooks /
/// GPG / editors), which is a declared `process.exec` effect gating as
/// `undeclared`, not prose. jq.ps stays pure: its argv is pinned literals
/// plus one integer and its fixed filter has no $ENV, input, or file
/// constructs. ps/pwd stay effect-free: no file operands, and their host-state
/// output has no placed file effect. tail.follow stays oneshot: oneshot names
/// the spawn contract while follow/streaming is composed with `stream from`.
const NATIVE_TOOL_CONTRACTS: &[(&str, &[&str], &[&str])] = &[
    (
        "awk.json",
        &["process.exec", "filesystem.read", "filesystem.write"],
        &["filesystem.read", "filesystem.write", "process.exec"],
    ),
    ("count.lines.json", &["process.exec"], &[]),
    ("cut.json", &["process.exec"], &[]),
    ("false.json", &["process.exec"], &[]),
    (
        "grep.json",
        &["process.exec", "filesystem.read"],
        &["filesystem.read"],
    ),
    ("head.json", &["process.exec"], &[]),
    (
        "jq.json",
        &["process.exec", "filesystem.read"],
        &["filesystem.read"],
    ),
    ("jq.ps.json", &["process.exec"], &[]),
    ("printf.json", &["process.exec"], &[]),
    ("ps.json", &["process.exec"], &[]),
    ("pwd.json", &["process.exec"], &[]),
    ("tr.json", &["process.exec"], &[]),
    ("upper.json", &["process.exec"], &[]),
    (
        "wc.json",
        &["process.exec", "filesystem.read"],
        &["filesystem.read"],
    ),
    (
        "cat.json",
        &["process.exec", "filesystem.read"],
        &["filesystem.read"],
    ),
    (
        "comm.json",
        &["process.exec", "filesystem.read"],
        &["filesystem.read"],
    ),
    (
        "join.json",
        &["process.exec", "filesystem.read"],
        &["filesystem.read"],
    ),
    (
        "ls.json",
        &["process.exec", "filesystem.read"],
        &["filesystem.read"],
    ),
    (
        "paste.json",
        &["process.exec", "filesystem.read"],
        &["filesystem.read"],
    ),
    (
        "tail.json",
        &["process.exec", "filesystem.read"],
        &["filesystem.read"],
    ),
    (
        "tail.follow.json",
        &["process.exec", "filesystem.read"],
        &["filesystem.read"],
    ),
    (
        "sed.json",
        &["process.exec", "filesystem.read", "filesystem.write"],
        &["filesystem.read", "filesystem.write", "process.exec"],
    ),
    (
        "sort.json",
        &["process.exec", "filesystem.read", "filesystem.write"],
        &["filesystem.read", "filesystem.write"],
    ),
    (
        "uniq.json",
        &["process.exec", "filesystem.read", "filesystem.write"],
        &["filesystem.read", "filesystem.write"],
    ),
    (
        "mkdir.json",
        &["process.exec", "filesystem.write"],
        &["filesystem.write"],
    ),
    (
        "git.commit.json",
        &["process.exec", "filesystem.read", "filesystem.write"],
        &["filesystem.read", "filesystem.write", "process.exec"],
    ),
    (
        "gh.repo.json",
        &["process.exec", "filesystem.read", "network.connect"],
        &["filesystem.read", "network.connect"],
    ),
    (
        "jev.ask.json",
        &["process.exec", "filesystem.read", "network.connect"],
        &["filesystem.read", "network.connect"],
    ),
    (
        "jev.classify.json",
        &["process.exec", "filesystem.read", "network.connect"],
        &["filesystem.read", "network.connect"],
    ),
    (
        "jev.judge.json",
        &["process.exec", "filesystem.read", "network.connect"],
        &["filesystem.read", "network.connect"],
    ),
    (
        "jev.score.json",
        &["process.exec", "filesystem.read", "network.connect"],
        &["filesystem.read", "network.connect"],
    ),
];

/// Effects the Linux/macOS sandbox path can enforce for native children.
/// Anything else fails closed under untrusted execution.
const ENFORCEABLE_NATIVE_EFFECTS: &[&str] = &[
    "filesystem.read",
    "filesystem.write",
    "network.connect",
    "process.exec",
];

fn manifests_dir() -> std::path::PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR")).join("manifests")
}

fn shipped_json_files() -> Vec<std::path::PathBuf> {
    let mut files: Vec<_> = std::fs::read_dir(manifests_dir())
        .unwrap()
        .map(|entry| entry.unwrap().path())
        .filter(|path| path.extension().and_then(|ext| ext.to_str()) == Some("json"))
        .collect();
    files.sort();
    files
}

fn read_json(path: &Path) -> serde_json::Value {
    serde_json::from_slice(&std::fs::read(path).unwrap()).unwrap()
}

fn policy_for(name: &str, capabilities: BTreeSet<String>) -> CapabilityPolicy {
    // `CapabilityPolicy::validate` requires a `network.connect` capability to
    // be paired with the boolean network grant (and vice versa), so a helper
    // that grants the capability must grant network access too.
    let network = capabilities.contains("network.connect");
    CapabilityPolicy {
        version: 1,
        applets: BTreeMap::from([(
            name.to_owned(),
            AppletGrant {
                capabilities,
                binary_sha256: None,
                environment: BTreeSet::new(),
                filesystem: FilesystemGrant::default(),
                network,
                network_origins: BTreeSet::new(),
                sandbox_required: false,
            },
        )]),
        host: None,
        host_environment: BTreeSet::new(),
        deny_privileged: false,
        allow_wrappers: BTreeSet::new(),
    }
}

#[test]
fn every_shipped_non_builtin_manifest_has_a_v1_contract() {
    let mut checked = 0;
    for path in shipped_json_files() {
        let value = read_json(&path);
        let name = value["name"].as_str().unwrap().to_owned();
        if !value["builtin"].is_null() {
            continue;
        }
        let contract = value.get("contract").unwrap_or_else(|| {
            panic!("shipped applet {name} has no version-1 contract");
        });
        assert_eq!(
            contract["version"], 1,
            "shipped applet {name} has an unsupported contract version"
        );
        checked += 1;
    }
    assert!(
        checked > 200,
        "expected to audit the shipped catalog, saw {checked}"
    );
    // The host-managed `cd` builtin stays exempt from the contract check.
    let cd = read_json(&manifests_dir().join("cd.json"));
    assert_eq!(cd["builtin"], "cd");
    assert!(cd.get("contract").is_none());
}

#[test]
fn native_tool_contracts_match_the_honest_taxonomy() {
    assert_eq!(NATIVE_TOOL_CONTRACTS.len(), 31);
    for (file, expected_caps, expected_effects) in NATIVE_TOOL_CONTRACTS {
        let expected_caps: &[&str] = expected_caps;
        let expected_effects: &[&str] = expected_effects;
        let value = read_json(&manifests_dir().join(file));
        let name = value["name"].as_str().unwrap();
        let contract = &value["contract"];
        assert_eq!(contract["version"], 1, "{name}");
        assert_eq!(contract["backend"], "command", "{name}");
        // The three jev model tools keep their pre-existing record/json
        // output schemas; only their capabilities/effects are audited here.
        let is_record_output = matches!(
            *file,
            "jev.classify.json" | "jev.judge.json" | "jev.score.json"
        );
        if is_record_output {
            assert_eq!(contract["output"]["encoding"], "json", "{name}");
        } else {
            // text/raw with no envelope and no trim preserves the legacy byte
            // content as a string value; the representation still changes from
            // raw bytes to a string value (pinned below).
            assert_eq!(
                contract["output"],
                serde_json::json!({"type": "text", "encoding": "raw"}),
                "{name}"
            );
            assert!(contract.get("result_envelope").is_none(), "{name}");
            assert!(contract.get("outcomes").is_none(), "{name}");
        }
        let actual_caps: Vec<&str> = contract["capabilities"]
            .as_array()
            .unwrap()
            .iter()
            .map(|cap| cap.as_str().unwrap())
            .collect();
        let actual_effects: Vec<&str> = contract["effects"]
            .as_array()
            .unwrap()
            .iter()
            .map(|effect| effect.as_str().unwrap())
            .collect();
        assert_eq!(actual_caps, expected_caps, "{name} capabilities");
        assert_eq!(actual_effects, expected_effects, "{name} effects");
        for effect in expected_effects.iter() {
            assert!(
                ENFORCEABLE_NATIVE_EFFECTS.contains(effect),
                "{name} declares effect {effect:?} the native sandbox cannot enforce"
            );
            // `process.exec` as an effect is the conservative marking for child
            // command execution (awk `system()`, GNU sed `e`, git hooks/GPG /
            // editors): it stays `undeclared` so the advisory gate requires
            // explicit approval, while the capability policy (not the gate)
            // enforces it by refusing grants that withhold `process.exec`.
            // The marking over-gates where the vector is absent (e.g. BSD
            // `sed` without `e`): the effect is still declared and approval
            // is still required there. Every other effect must have a placed
            // gate category.
            if *effect == "process.exec" {
                assert_eq!(
                    effect_category(effect),
                    "undeclared",
                    "{name} exec effect must gate as undeclared"
                );
            } else {
                assert_ne!(
                    effect_category(effect),
                    "undeclared",
                    "{name} declares effect {effect:?} with no gate placement"
                );
            }
        }
    }
}

#[test]
fn exec_vectors_require_explicit_process_authority() {
    // Adversarial twin of the grantability test: a policy that grants the
    // file effects but withholds `process.exec` must refuse awk/sed/git.commit,
    // so the child-execution vector cannot ride on file grants alone. The
    // placement assertion pins the second half of the enforcement: the exec
    // effect gates as `undeclared`, requiring explicit gate approval too.
    assert_eq!(effect_category("process.exec"), "undeclared");
    for file in ["awk.json", "sed.json", "git.commit.json"] {
        let source = std::fs::read_to_string(manifests_dir().join(file)).unwrap();
        let registry = Registry::from_sources(std::slice::from_ref(&source)).unwrap();
        let value: serde_json::Value = serde_json::from_str(&source).unwrap();
        let name = value["name"].as_str().unwrap().to_owned();
        let manifest = registry.resolve(&name).unwrap();
        let contract = manifest.execution_contract().unwrap();
        assert!(
            contract
                .effects
                .iter()
                .any(|effect| effect == "process.exec"),
            "{name} must declare its child-execution vector as an effect, not prose"
        );
        let mut granted: BTreeSet<String> = contract.capabilities.iter().cloned().collect();
        assert!(
            granted.remove("process.exec"),
            "{name} must carry process.exec authority to withhold"
        );
        let policy = policy_for(&name, granted);
        let error = policy
            .authorize(
                &name,
                false,
                Path::new("fixture-binary"),
                Some(ContractView {
                    version: contract.version,
                    capabilities: &contract.capabilities,
                    effects: &contract.effects,
                }),
            )
            .unwrap_err();
        assert!(
            format!("{error:#}").contains("process.exec"),
            "{name} refusal must name the withheld process authority: {error:#}"
        );
    }
}

#[test]
fn every_shipped_contract_is_grantable_under_a_matching_policy() {
    for path in shipped_json_files() {
        let source = std::fs::read_to_string(&path).unwrap();
        let registry = Registry::from_sources(std::slice::from_ref(&source)).unwrap();
        let value: serde_json::Value = serde_json::from_str(&source).unwrap();
        let name = value["name"].as_str().unwrap().to_owned();
        if !value["builtin"].is_null() {
            continue;
        }
        let manifest = registry.resolve(&name).unwrap();
        let contract = manifest.execution_contract().unwrap_or_else(|| {
            panic!("shipped applet {name} has no version-1 contract");
        });
        assert_eq!(contract.version, 1, "{name}");
        let mut granted: BTreeSet<String> = contract.capabilities.iter().cloned().collect();
        // The command backends additionally require process execution authority.
        if matches!(
            contract.backend,
            AppletBackend::Command | AppletBackend::JsonLinesWorker
        ) {
            granted.insert("process.exec".into());
        }
        if contract.backend == AppletBackend::Remote {
            granted.insert("network.connect".into());
        }
        let policy = policy_for(&name, granted);
        policy
            .authorize(
                &name,
                false,
                Path::new("fixture-binary"),
                Some(ContractView {
                    version: contract.version,
                    capabilities: &contract.capabilities,
                    effects: &contract.effects,
                }),
            )
            .unwrap_or_else(|error| panic!("shipped applet {name} is not grantable: {error:#}"));
    }
}

#[test]
fn filesystem_read_is_a_placed_reads_effect() {
    assert_eq!(effect_category("filesystem.read"), "reads");
    assert_eq!(effect_category("filesystem.write"), "writes");
    assert_eq!(effect_category("network.connect"), "network");
}

#[test]
fn raw_text_contracts_decode_success_bytes_as_text_values() {
    for path in shipped_json_files() {
        let source = std::fs::read_to_string(&path).unwrap();
        let registry = Registry::from_sources(std::slice::from_ref(&source)).unwrap();
        let value: serde_json::Value = serde_json::from_str(&source).unwrap();
        let name = value["name"].as_str().unwrap().to_owned();
        if !value["builtin"].is_null() {
            continue;
        }
        let manifest = registry.resolve(&name).unwrap();
        let Some(contract) = manifest.execution_contract() else {
            continue;
        };
        let output = contract.output.as_ref();
        let is_plain_raw_text = output.is_some_and(|schema| {
            serde_json::to_value(schema).unwrap()
                == serde_json::json!({"type": "text", "encoding": "raw"})
        });
        if !is_plain_raw_text || contract.result_envelope {
            continue;
        }
        // No trim, no envelope: the success bytes decode to the same text,
        // trailing newline included. The representation differs from the
        // legacy contract-less boundary on purpose: legacy decode returned
        // raw bytes (`Manifest::decode_output` falls back to
        // `AppletOutput::Bytes`), while a text/raw contract returns a string
        // value. Every consumer accepts both: named-result bindings convert
        // either form to the same string (`src/runtime.rs`, `src/browser.rs`),
        // and event ports require the typed form (`src/graph/transport.rs`).
        assert_eq!(
            manifest.decode_output(b"hello\n", 0).unwrap(),
            AppletOutput::Value(flow::ast::Value::String("hello\n".into())),
            "{name} changed its decoded success text"
        );
        assert_eq!(manifest.outcome_for(0).kind, OutcomeKind::Success, "{name}");
    }
}
