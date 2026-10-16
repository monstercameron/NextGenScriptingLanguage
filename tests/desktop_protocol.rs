//! In-memory protocol tests for the Quickshell adapter.
//!
//! These exercise the host's versioned frame/state machine with synthetic
//! replies. They are deliberately named as mock protocol tests and do not
//! certify QML execution, Wayland behavior, or an Omarchy desktop session.

use flow::capabilities::{AppletGrant, CapabilityPolicy, FilesystemGrant};
use flow::contracts::{OutputType, PortDelivery, ValueSchema};
#[cfg(unix)]
use flow::desktop::QuickShellRenderer;
use flow::desktop::{
    EventDisposition, ProtocolRange, RendererCommand, RendererEvent, RendererProtocol,
    RendererReply, RendererRequest, RendererResponse, WidgetContract, DEFAULT_MAX_FRAME_BYTES,
    DEFAULT_MAX_QUEUED_EVENTS, DEFAULT_MAX_WIDGETS, RENDERER_PROTOCOL, RENDERER_PROTOCOL_VERSION,
};
use flow::graph::PortSchema;
use serde_json::json;
use std::collections::{BTreeMap, BTreeSet};

fn leaf(kind: OutputType) -> ValueSchema {
    ValueSchema {
        kind,
        nullable: false,
        enum_values: None,
        items: None,
        fields: BTreeMap::new(),
        required: Vec::new(),
    }
}

fn record(fields: BTreeMap<String, ValueSchema>, required: &[&str]) -> ValueSchema {
    ValueSchema {
        kind: OutputType::Record,
        nullable: false,
        enum_values: None,
        items: None,
        fields,
        required: required.iter().map(|item| (*item).to_owned()).collect(),
    }
}

fn text_contract() -> WidgetContract {
    let properties = record(
        BTreeMap::from([("text".to_owned(), leaf(OutputType::Text))]),
        &["text"],
    );
    WidgetContract {
        component: "text".into(),
        properties: PortSchema::single(properties),
        signals: BTreeMap::new(),
    }
}

fn button_contract() -> WidgetContract {
    let properties = record(
        BTreeMap::from([
            ("text".to_owned(), leaf(OutputType::Text)),
            ("enabled".to_owned(), leaf(OutputType::Boolean)),
        ]),
        &["text", "enabled"],
    );
    let clicked = PortSchema::events(record(BTreeMap::new(), &[]));
    WidgetContract {
        component: "button".into(),
        properties: PortSchema::single(properties),
        signals: BTreeMap::from([("clicked".into(), clicked)]),
    }
}

fn mock_response(request: &RendererRequest, body: RendererReply, version: u32) -> RendererResponse {
    RendererResponse {
        protocol: RENDERER_PROTOCOL.into(),
        version,
        request_id: request.request_id,
        session_id: request.session_id.clone(),
        generation: request.generation,
        revision: request.revision,
        body,
    }
}

fn mock_negotiated(request: &RendererRequest) -> RendererResponse {
    mock_response(
        request,
        RendererReply::Negotiated {
            renderer: "synthetic-test-peer".into(),
            supported: ProtocolRange::new(RENDERER_PROTOCOL_VERSION, RENDERER_PROTOCOL_VERSION)
                .unwrap(),
            max_frame_bytes: DEFAULT_MAX_FRAME_BYTES,
            max_widgets: DEFAULT_MAX_WIDGETS,
            max_queued_events: DEFAULT_MAX_QUEUED_EVENTS,
        },
        RENDERER_PROTOCOL_VERSION,
    )
}

fn negotiate(protocol: &mut RendererProtocol) {
    let request = protocol.negotiation_request().unwrap();
    let response = mock_negotiated(&request);
    protocol.accept_negotiation(&request, &response).unwrap();
}

#[test]
fn protocol_range_requires_a_common_nonzero_version() {
    let client = ProtocolRange::new(1, 3).unwrap();
    assert_eq!(
        client.negotiate(ProtocolRange::new(2, 4).unwrap()).unwrap(),
        3
    );
    assert!(client.negotiate(ProtocolRange::new(4, 5).unwrap()).is_err());
    assert!(ProtocolRange::new(0, 1).is_err());
}

#[test]
fn generic_qml_widget_contracts_match_the_actual_components() {
    assert!(text_contract().validate().is_ok());
    assert!(button_contract().validate().is_ok());

    let mut unsupported_signal = button_contract();
    unsupported_signal.signals.insert(
        "submitted".into(),
        PortSchema::events(record(BTreeMap::new(), &[])),
    );
    assert!(unsupported_signal.validate().is_err());

    let mut incomplete_button = button_contract();
    incomplete_button.properties.schema.required.clear();
    assert!(incomplete_button.validate().is_err());

    let mut nullable_text = text_contract();
    nullable_text
        .properties
        .schema
        .fields
        .get_mut("text")
        .unwrap()
        .nullable = true;
    assert!(nullable_text.validate().is_err());

    let mut unsupported_property = text_contract();
    unsupported_property
        .properties
        .schema
        .fields
        .insert("color".into(), leaf(OutputType::Text));
    assert!(unsupported_property.validate().is_err());
}

#[test]
fn mock_protocol_covers_widget_lifecycle_typed_events_and_restart_replay() {
    // This is intentionally an in-memory protocol peer, not a QuickShell
    // process test or native desktop certification.
    let mut protocol = RendererProtocol::new(
        "desktop-protocol-test",
        ProtocolRange::new(RENDERER_PROTOCOL_VERSION, RENDERER_PROTOCOL_VERSION).unwrap(),
    )
    .unwrap();
    negotiate(&mut protocol);

    let create = protocol
        .instantiate_request(
            "clock_applet",
            "clock_widget",
            text_contract(),
            json!({"text": "09:41"}),
        )
        .unwrap();
    let create_ack = mock_response(&create, RendererReply::Applied, RENDERER_PROTOCOL_VERSION);
    protocol.accept_applied(&create, &create_ack).unwrap();
    assert_eq!(protocol.widget_count(), 1);

    let update = protocol
        .patch_request("clock_applet", "clock_widget", json!({"text": "09:42"}))
        .unwrap();
    let update_ack = mock_response(&update, RendererReply::Applied, RENDERER_PROTOCOL_VERSION);
    protocol.accept_applied(&update, &update_ack).unwrap();

    let button_create = protocol
        .instantiate_request(
            "clock_applet",
            "sync_button",
            button_contract(),
            json!({"text": "Sync", "enabled": true}),
        )
        .unwrap();
    let button_ack = mock_response(
        &button_create,
        RendererReply::Applied,
        RENDERER_PROTOCOL_VERSION,
    );
    protocol
        .accept_applied(&button_create, &button_ack)
        .unwrap();

    let poll = protocol.poll_request(8).unwrap();
    let click = RendererEvent {
        cursor: 1,
        applet_id: "clock_applet".into(),
        widget_id: "sync_button".into(),
        signal: "clicked".into(),
        generation: protocol.generation(),
        source_revision: button_create.revision,
        payload: json!({}),
    };
    let batch = mock_response(
        &poll,
        RendererReply::EventBatch {
            events: vec![click.clone()],
            next_cursor: 1,
            oldest_cursor: 1,
            overflow: false,
        },
        RENDERER_PROTOCOL_VERSION,
    );
    let (accepted, next_cursor) = protocol.accept_event_batch(&poll, &batch).unwrap();
    assert_eq!(accepted, vec![click.clone()]);
    assert_eq!(next_cursor, 1);
    assert_eq!(
        protocol.accept_event(&click).unwrap().0,
        EventDisposition::Accepted
    );
    let acknowledge = protocol.ack_events_request(next_cursor).unwrap();
    let acknowledge_reply = mock_response(
        &acknowledge,
        RendererReply::Applied,
        RENDERER_PROTOCOL_VERSION,
    );
    protocol
        .accept_applied(&acknowledge, &acknowledge_reply)
        .unwrap();
    let next_poll = protocol.poll_request(8).unwrap();
    assert!(matches!(
        next_poll.body,
        RendererCommand::PollEvents {
            after_cursor: 1,
            ..
        }
    ));

    let destroy = protocol
        .destroy_request("clock_applet", "clock_widget")
        .unwrap();
    let destroy_ack = mock_response(&destroy, RendererReply::Applied, RENDERER_PROTOCOL_VERSION);
    protocol.accept_applied(&destroy, &destroy_ack).unwrap();
    assert_eq!(protocol.widget_count(), 1);

    let old_generation = protocol.generation();
    protocol.begin_restart().unwrap();
    assert_eq!(
        protocol.accept_event(&click).unwrap().0,
        EventDisposition::StaleGeneration
    );
    assert!(protocol.accept_event_batch(&poll, &batch).is_err());

    negotiate(&mut protocol);
    let replay = protocol.replay_requests().unwrap();
    assert_eq!(replay.len(), 1);
    let replay_ack = mock_response(
        &replay[0],
        RendererReply::Applied,
        RENDERER_PROTOCOL_VERSION,
    );
    protocol.accept_applied(&replay[0], &replay_ack).unwrap();
    assert!(protocol.generation() > old_generation);

    let destroy_button = protocol
        .destroy_request("clock_applet", "sync_button")
        .unwrap();
    let destroy_button_ack = mock_response(
        &destroy_button,
        RendererReply::Applied,
        RENDERER_PROTOCOL_VERSION,
    );
    protocol
        .accept_applied(&destroy_button, &destroy_button_ack)
        .unwrap();
    assert_eq!(protocol.widget_count(), 0);
}

#[test]
fn mock_protocol_rejects_stale_widget_revisions_and_mismatched_responses() {
    let mut protocol = RendererProtocol::new(
        "desktop-stale-test",
        ProtocolRange::new(RENDERER_PROTOCOL_VERSION, RENDERER_PROTOCOL_VERSION).unwrap(),
    )
    .unwrap();
    negotiate(&mut protocol);
    let create = protocol
        .instantiate_request(
            "clock_applet",
            "sync_button",
            button_contract(),
            json!({"text": "Sync", "enabled": true}),
        )
        .unwrap();
    protocol
        .accept_applied(
            &create,
            &mock_response(&create, RendererReply::Applied, RENDERER_PROTOCOL_VERSION),
        )
        .unwrap();
    let update = protocol
        .patch_request("clock_applet", "sync_button", json!({"text": "Sync now"}))
        .unwrap();
    protocol
        .accept_applied(
            &update,
            &mock_response(&update, RendererReply::Applied, RENDERER_PROTOCOL_VERSION),
        )
        .unwrap();

    let stale = RendererEvent {
        cursor: 1,
        applet_id: "clock_applet".into(),
        widget_id: "sync_button".into(),
        signal: "clicked".into(),
        generation: protocol.generation(),
        source_revision: create.revision,
        payload: json!({}),
    };
    assert_eq!(
        protocol.accept_event(&stale).unwrap().0,
        EventDisposition::StaleRevision
    );

    let poll = protocol.poll_request(8).unwrap();
    let mismatched = mock_response(
        &poll,
        RendererReply::EventBatch {
            events: Vec::new(),
            next_cursor: 0,
            oldest_cursor: 0,
            overflow: false,
        },
        RENDERER_PROTOCOL_VERSION,
    );
    let mut mismatched = mismatched;
    mismatched.request_id += 1;
    assert!(protocol.accept_event_batch(&poll, &mismatched).is_err());
}

#[test]
fn renderer_wire_frames_keep_explicit_request_and_port_contracts() {
    let mut protocol = RendererProtocol::new(
        "wire-test",
        ProtocolRange::new(RENDERER_PROTOCOL_VERSION, RENDERER_PROTOCOL_VERSION).unwrap(),
    )
    .unwrap();
    let request = protocol.negotiation_request().unwrap();
    let encoded = serde_json::to_value(&request).unwrap();
    assert_eq!(encoded["protocol"], RENDERER_PROTOCOL);
    assert_eq!(encoded["version"], 0);
    assert_eq!(encoded["body"]["kind"], "negotiate");
    assert!(matches!(request.body, RendererCommand::Negotiate { .. }));

    let button = button_contract();
    assert_eq!(button.properties.delivery, PortDelivery::Single);
    assert_eq!(button.signals["clicked"].delivery, PortDelivery::Events);
    assert_eq!(
        button.properties.schema.fields["enabled"].kind,
        OutputType::Boolean
    );
}

#[cfg(unix)]
#[test]
fn native_process_mock_renderer_lifecycle_is_not_qml_certification() {
    use std::os::unix::fs::PermissionsExt;

    // This test launches and owns native processes to exercise argv, response
    // bounds, restart replay, and child cleanup. Its fixture is synthetic and
    // deliberately cannot establish that Quickshell parsed or rendered QML.
    let temporary = tempfile::tempdir().unwrap();
    let fixture = std::path::Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("tests/fixtures/quickshell_protocol_mock.py");
    let launcher = temporary.path().join("mock-qs");
    std::fs::write(
        &launcher,
        format!("#!/bin/sh\nexec python3 '{}' \"$@\"\n", fixture.display()),
    )
    .unwrap();
    std::fs::set_permissions(&launcher, std::fs::Permissions::from_mode(0o700)).unwrap();
    let config = temporary.path().join("dummy.qml");
    std::fs::write(&config, "// not evaluated by this process mock\n").unwrap();

    let registry = flow::manifest::Registry::load(
        &std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("manifests"),
    )
    .unwrap();
    let mut renderer = QuickShellRenderer::start_with_registry(
        &launcher,
        &config,
        "native-mock-session",
        ProtocolRange::new(RENDERER_PROTOCOL_VERSION, RENDERER_PROTOCOL_VERSION).unwrap(),
        registry,
    )
    .unwrap();
    renderer
        .instantiate(
            "clock_applet",
            "clock_widget",
            text_contract(),
            json!({"text": "09:41"}),
        )
        .unwrap();
    renderer
        .patch("clock_applet", "clock_widget", json!({"text": "09:42"}))
        .unwrap();
    assert!(renderer.poll_events(4).unwrap().is_empty());
    renderer.restart().unwrap();
    renderer.destroy("clock_applet", "clock_widget").unwrap();
    renderer.shutdown();
    assert!(!renderer.is_running().unwrap());
}

#[test]
fn renderer_launch_policy_uses_the_dedicated_manifest_identity() {
    let registry = flow::manifest::Registry::load(
        &std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("manifests"),
    )
    .unwrap();
    let grant = AppletGrant {
        capabilities: BTreeSet::from(["desktop.render".to_owned(), "process.exec".to_owned()]),
        binary_sha256: None,
        environment: Default::default(),
        filesystem: FilesystemGrant::default(),
        network: false,
        network_origins: Default::default(),
        sandbox_required: false,
    };
    registry.set_host_state(Some(CapabilityPolicy {
        version: 1,
        applets: BTreeMap::from([("desktop.quickshell".to_owned(), grant)]),
    }));
    let cwd = std::env::current_dir().unwrap();
    let binary = std::path::Path::new(if cfg!(target_os = "macos") {
        "/usr/bin/true"
    } else {
        "/bin/true"
    });
    let command = flow::capabilities::command_for(
        binary,
        [
            "ipc",
            "--path",
            "config.qml",
            "call",
            "executeDesktop-test",
            "request",
            "{}",
        ],
        &registry,
        "desktop.quickshell",
        &cwd,
    )
    .unwrap();
    assert_eq!(
        command
            .get_args()
            .map(|argument| argument.to_os_string())
            .collect::<Vec<_>>(),
        [
            "ipc",
            "--path",
            "config.qml",
            "call",
            "executeDesktop-test",
            "request",
            "{}",
        ]
        .map(std::ffi::OsString::from)
    );

    let denied_registry = flow::manifest::Registry::load(
        &std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("manifests"),
    )
    .unwrap();
    denied_registry.set_host_state(Some(CapabilityPolicy {
        version: 1,
        applets: BTreeMap::new(),
    }));
    assert!(flow::capabilities::command_for(
        binary,
        ["--path", "config.qml"],
        &denied_registry,
        "desktop.quickshell",
        &cwd,
    )
    .is_err());
}
