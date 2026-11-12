#![cfg(feature = "desktop")]
//! Issue #371: many applets multiplex over one renderer protocol session.
//!
//! In-memory protocol test only. It negotiates once, instantiates 50 applets
//! through the same `RendererProtocol`, accepts one event batch whose events
//! belong to distinct applets under a single cursor, and replays every widget
//! after a restart. It spawns nothing and does not certify QML execution.

use flow::contracts::{OutputType, ValueSchema};
use flow::desktop::{
    EventDisposition, ProtocolRange, RendererCommand, RendererEvent, RendererProtocol,
    RendererReply, RendererRequest, RendererResponse, WidgetContract, DEFAULT_MAX_FRAME_BYTES,
    DEFAULT_MAX_QUEUED_EVENTS, DEFAULT_MAX_WIDGETS, RENDERER_PROTOCOL, RENDERER_PROTOCOL_VERSION,
};
use flow::graph::PortSchema;
use serde_json::json;
use std::collections::BTreeMap;

const APPLETS: usize = 50;
const SESSION: &str = "shared-session-test";

fn leaf(kind: OutputType) -> ValueSchema {
    ValueSchema {
        min_items: None,
        scalar_only: false,
        additional_properties: None,
        min_properties: None,
        max_properties: None,
        nonempty: false,
        kind,
        nullable: false,
        enum_values: None,
        single_line: false,
        minimum: None,
        items: None,
        fields: BTreeMap::new(),
        required: Vec::new(),
        pattern: None,
    }
}

fn button_contract() -> WidgetContract {
    let properties = ValueSchema {
        fields: BTreeMap::from([
            ("text".to_owned(), leaf(OutputType::Text)),
            ("enabled".to_owned(), leaf(OutputType::Boolean)),
        ]),
        required: vec!["text".to_owned(), "enabled".to_owned()],
        ..leaf(OutputType::Record)
    };
    WidgetContract {
        component: "button".into(),
        properties: PortSchema::single(properties),
        signals: BTreeMap::from([(
            "clicked".into(),
            PortSchema::events(leaf(OutputType::Record)),
        )]),
    }
}

fn reply(request: &RendererRequest, body: RendererReply) -> RendererResponse {
    RendererResponse {
        protocol: RENDERER_PROTOCOL.into(),
        version: RENDERER_PROTOCOL_VERSION,
        request_id: request.request_id,
        session_id: request.session_id.clone(),
        generation: request.generation,
        revision: request.revision,
        body,
    }
}

fn negotiate(protocol: &mut RendererProtocol) {
    let request = protocol.negotiation_request().unwrap();
    let response = reply(
        &request,
        RendererReply::Negotiated {
            renderer: "synthetic-shared-session-peer".into(),
            supported: ProtocolRange::new(RENDERER_PROTOCOL_VERSION, RENDERER_PROTOCOL_VERSION)
                .unwrap(),
            max_frame_bytes: DEFAULT_MAX_FRAME_BYTES,
            max_widgets: DEFAULT_MAX_WIDGETS,
            max_queued_events: DEFAULT_MAX_QUEUED_EVENTS,
        },
    );
    protocol.accept_negotiation(&request, &response).unwrap();
}

fn applet_id(index: usize) -> String {
    format!("applet_{index:02}")
}

fn new_session() -> RendererProtocol {
    let mut protocol = RendererProtocol::new(
        SESSION,
        ProtocolRange::new(RENDERER_PROTOCOL_VERSION, RENDERER_PROTOCOL_VERSION).unwrap(),
    )
    .unwrap();
    negotiate(&mut protocol);
    protocol
}

fn instantiate_all(protocol: &mut RendererProtocol) {
    for index in 0..APPLETS {
        let create = protocol
            .instantiate_request(
                applet_id(index),
                "main",
                button_contract(),
                json!({"text": format!("item {index}"), "enabled": true}),
            )
            .unwrap();
        assert_eq!(create.session_id, SESSION);
        protocol
            .accept_applied(&create, &reply(&create, RendererReply::Applied))
            .unwrap();
    }
}

#[test]
fn fifty_applets_share_one_negotiated_session() {
    let mut protocol = new_session();
    let generation = protocol.generation();
    instantiate_all(&mut protocol);
    assert_eq!(protocol.widget_count(), APPLETS);
    assert_eq!(protocol.generation(), generation);
}

#[test]
fn one_batch_routes_events_from_many_applets_and_restart_replays_all() {
    let mut protocol = new_session();
    instantiate_all(&mut protocol);

    // One poll, one cursor. Each click is validated against its own applet's
    // widget, so the batch is accepted as a whole and the cursor lands on 50.
    let poll = protocol.poll_request(APPLETS).unwrap();
    let generation = protocol.generation();
    // Applet i was created at revision i + 1, so its clicks cite that revision.
    let events: Vec<RendererEvent> = (0..APPLETS)
        .map(|index| RendererEvent {
            cursor: index as u64 + 1,
            applet_id: applet_id(index),
            widget_id: "main".into(),
            signal: "clicked".into(),
            generation,
            source_revision: index as u64 + 1,
            payload: json!({}),
        })
        .collect();
    let batch = reply(
        &poll,
        RendererReply::EventBatch {
            events: events.clone(),
            next_cursor: APPLETS as u64,
            oldest_cursor: 1,
            overflow: false,
        },
    );
    let (accepted, next_cursor) = protocol.accept_event_batch(&poll, &batch).unwrap();
    assert_eq!(accepted.len(), APPLETS);
    assert_eq!(next_cursor, APPLETS as u64);
    assert_eq!(accepted[7].applet_id, applet_id(7));

    // Restart: a new generation negotiates once and replays every applet in order.
    protocol.begin_restart().unwrap();
    negotiate(&mut protocol);
    let replay = protocol.replay_requests().unwrap();
    assert_eq!(replay.len(), APPLETS);
    for (index, request) in replay.iter().enumerate() {
        let RendererCommand::Instantiate { applet_id: id, .. } = &request.body else {
            panic!("replay must be instantiate requests");
        };
        assert_eq!(id, &applet_id(index));
        assert_eq!(request.session_id, SESSION);
        protocol
            .accept_applied(request, &reply(request, RendererReply::Applied))
            .unwrap();
    }
    assert_eq!(protocol.widget_count(), APPLETS);

    // Events from the previous generation are dropped as stale, not replayed.
    let stale = &events[0];
    assert_eq!(
        protocol.accept_event(stale).unwrap().0,
        EventDisposition::StaleGeneration
    );
}
