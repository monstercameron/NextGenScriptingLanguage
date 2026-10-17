//! Versioned, typed boundary between program-owned applets and a UI renderer.
//!
//! The renderer owns pixels and input-device delivery. Applet identity,
//! workflow behavior, property schemas, signal schemas, cancellation and
//! recovery remain owned by the program/runtime. Quickshell's CLI IPC only
//! accepts scalar arguments, so this boundary sends a bounded JSON frame as a
//! single string and validates the structured values before and after IPC.

use crate::capabilities;
use crate::contracts::{OutputEncoding, OutputSchema, OutputType, ValueSchema};
use crate::graph::{
    GraphContext, GraphExecutor, GraphNode, GraphNodeKind, GraphPlan, GraphRunOptions,
    GraphRunResult, GraphSession, NodeIo, PortDelivery, PortSchema, PortValue,
    RuntimeGraphExecutor,
};
use crate::structured;
use anyhow::{anyhow, bail, Context, Result};
use serde::{Deserialize, Serialize};
use serde_json::Value;
use std::collections::{BTreeMap, VecDeque};
use std::ffi::OsString;
#[cfg(feature = "tui")]
use std::io::Write;
use std::io::{self, IsTerminal, Read};
use std::path::{Path, PathBuf};
use std::process::{Child, Command, Stdio};
use std::thread;
use std::time::{Duration, Instant};

pub const RENDERER_PROTOCOL: &str = "execute.renderer";
pub const RENDERER_PROTOCOL_VERSION: u32 = 1;
pub const DEFAULT_MAX_FRAME_BYTES: usize = 1024 * 1024;
pub const DEFAULT_MAX_WIDGETS: usize = 256;
pub const DEFAULT_MAX_QUEUED_EVENTS: usize = 256;
const DEFAULT_COMMAND_TIMEOUT: Duration = Duration::from_secs(3);
const DEFAULT_STARTUP_TIMEOUT: Duration = Duration::from_secs(8);
const IPC_FUNCTION: &str = "request";
const IPC_TARGET_PREFIX: &str = "executeDesktop-";
const IPC_TARGET_PLACEHOLDER: &str =
    "property string ipcTarget: \"__EXECUTE_RENDERER_IPC_TARGET__\"";
const POLICY_APPLET: &str = "desktop.quickshell";
const WIDGET_ID: &str = "main";
const EVENT_POLL_INTERVAL: Duration = Duration::from_millis(50);
const EVENT_POLL_LIMIT: usize = 64;
const QUICKSHELL_SOURCE: &str = include_str!("../desktop/quickshell/shell.qml");

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ProtocolRange {
    pub minimum: u32,
    pub maximum: u32,
}

impl ProtocolRange {
    pub fn new(minimum: u32, maximum: u32) -> Result<Self> {
        let range = Self { minimum, maximum };
        range.validate()?;
        Ok(range)
    }

    pub fn validate(&self) -> Result<()> {
        if self.minimum == 0 || self.maximum < self.minimum {
            bail!("renderer protocol version range must be nonzero and ordered");
        }
        Ok(())
    }

    pub fn negotiate(self, peer: Self) -> Result<u32> {
        self.validate()?;
        peer.validate()?;
        let selected = self.maximum.min(peer.maximum);
        if selected < self.minimum.max(peer.minimum) {
            bail!(
                "renderer protocol ranges do not overlap ({}..={} vs {}..={})",
                self.minimum,
                self.maximum,
                peer.minimum,
                peer.maximum
            );
        }
        Ok(selected)
    }
}

/// A widget declares property and signal payloads using the same graph port
/// schema that connects applets. Property values use a `single` record port;
/// signals use named `events` ports. The renderer cannot invent application
/// fields or translate between data types.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct WidgetContract {
    pub component: String,
    pub properties: PortSchema,
    #[serde(default)]
    pub signals: BTreeMap<String, PortSchema>,
}

impl WidgetContract {
    pub fn validate(&self) -> Result<()> {
        if !matches!(self.component.as_str(), "text" | "button") {
            bail!(
                "unsupported generic renderer component {:?}",
                self.component
            );
        }
        if self.properties.delivery != PortDelivery::Single
            || self.properties.schema.kind != OutputType::Record
        {
            bail!("widget properties must use a single record port");
        }
        schema_for_record(&self.properties.schema, None)?.validate()?;
        let allowed_properties: &[&str] = if self.component == "button" {
            &["text", "enabled"]
        } else {
            &["text"]
        };
        if self
            .properties
            .schema
            .fields
            .keys()
            .any(|field| !allowed_properties.contains(&field.as_str()))
        {
            bail!("widget contract declares a property the generic component does not render");
        }
        for (name, signal) in &self.signals {
            validate_identifier(name, "signal name")?;
            if self.component != "button" || name != "clicked" {
                bail!(
                    "generic {:?} widget does not emit signal {name:?}",
                    self.component
                );
            }
            if signal.delivery != PortDelivery::Events || signal.schema.kind != OutputType::Record {
                bail!("widget signal {name:?} must use an events record port");
            }
            schema_for_record(&signal.schema, None)?.validate()?;
        }
        if self.component == "button" {
            let clicked = self
                .signals
                .get("clicked")
                .ok_or_else(|| anyhow!("button widget must declare a `clicked` event port"))?;
            if !clicked.schema.required.is_empty() || !clicked.schema.fields.is_empty() {
                bail!("generic button `clicked` event payload must be an empty record");
            }
            for property in ["text", "enabled"] {
                if !self.properties.schema.fields.contains_key(property) {
                    bail!("generic button widget must declare its {property:?} property");
                }
                if !self
                    .properties
                    .schema
                    .required
                    .iter()
                    .any(|required| required == property)
                {
                    bail!("generic button widget must require its {property:?} property");
                }
            }
            if self.properties.schema.fields["text"].kind != OutputType::Text
                || self.properties.schema.fields["enabled"].kind != OutputType::Boolean
                || self.properties.schema.fields["text"].nullable
                || self.properties.schema.fields["enabled"].nullable
            {
                bail!("generic button `text` and `enabled` properties must be text and boolean");
            }
        } else if !self.properties.schema.fields.contains_key("text") {
            bail!("generic text widget must declare its `text` property");
        } else if self.properties.schema.fields["text"].kind != OutputType::Text
            || self.properties.schema.fields["text"].nullable
            || !self
                .properties
                .schema
                .required
                .iter()
                .any(|required| required == "text")
        {
            bail!("generic text widget `text` property must be required, non-null text");
        }
        Ok(())
    }
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct RendererRequest {
    pub protocol: String,
    /// Zero is reserved for the initial negotiation request.
    pub version: u32,
    pub request_id: u64,
    pub session_id: String,
    pub generation: u64,
    pub revision: u64,
    pub body: RendererCommand,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case", deny_unknown_fields)]
pub enum RendererCommand {
    Negotiate {
        supported: ProtocolRange,
        max_frame_bytes: usize,
        max_widgets: usize,
        max_queued_events: usize,
    },
    Instantiate {
        /// The stable graph node/applet identity assigned by the program.
        applet_id: String,
        widget_id: String,
        contract: WidgetContract,
        properties: Value,
    },
    Patch {
        applet_id: String,
        widget_id: String,
        properties: Value,
    },
    Destroy {
        applet_id: String,
        widget_id: String,
    },
    PollEvents {
        after_cursor: u64,
        limit: usize,
    },
    AckEvents {
        through_cursor: u64,
    },
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct RendererResponse {
    pub protocol: String,
    pub version: u32,
    pub request_id: u64,
    pub session_id: String,
    pub generation: u64,
    pub revision: u64,
    pub body: RendererReply,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case", deny_unknown_fields)]
pub enum RendererReply {
    Negotiated {
        renderer: String,
        supported: ProtocolRange,
        max_frame_bytes: usize,
        max_widgets: usize,
        max_queued_events: usize,
    },
    Applied,
    EventBatch {
        events: Vec<RendererEvent>,
        next_cursor: u64,
        oldest_cursor: u64,
        overflow: bool,
    },
    Error {
        code: String,
        message: String,
    },
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct RendererEvent {
    pub cursor: u64,
    pub applet_id: String,
    pub widget_id: String,
    pub signal: String,
    pub generation: u64,
    /// Revision of the widget properties visible when the user acted.
    pub source_revision: u64,
    pub payload: Value,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum EventDisposition {
    Accepted,
    StaleGeneration,
    StaleRevision,
}

#[derive(Debug, Clone)]
struct ActiveWidget {
    contract: WidgetContract,
    properties: Value,
    revision: u64,
}

/// Host-owned protocol and widget state. This is the source of truth across a
/// renderer restart; QML receives a replay from this map and never owns applet
/// state or application decisions.
#[derive(Debug)]
pub struct RendererProtocol {
    session_id: String,
    supported: ProtocolRange,
    negotiated: Option<u32>,
    generation: u64,
    revision: u64,
    last_issued_revision: u64,
    next_request_id: u64,
    event_cursor: u64,
    last_polled_cursor: u64,
    max_frame_bytes: usize,
    max_widgets: usize,
    max_queued_events: usize,
    widgets: BTreeMap<(String, String), ActiveWidget>,
    pending_mutations: VecDeque<(u64, u64)>,
}

impl RendererProtocol {
    pub fn new(session_id: impl Into<String>, supported: ProtocolRange) -> Result<Self> {
        supported.validate()?;
        let session_id = session_id.into();
        validate_identifier(&session_id, "renderer session id")?;
        Ok(Self {
            session_id,
            supported,
            negotiated: None,
            generation: 1,
            revision: 0,
            last_issued_revision: 0,
            next_request_id: 1,
            event_cursor: 0,
            last_polled_cursor: 0,
            max_frame_bytes: DEFAULT_MAX_FRAME_BYTES,
            max_widgets: DEFAULT_MAX_WIDGETS,
            max_queued_events: DEFAULT_MAX_QUEUED_EVENTS,
            widgets: BTreeMap::new(),
            pending_mutations: VecDeque::new(),
        })
    }

    pub fn generation(&self) -> u64 {
        self.generation
    }

    pub fn revision(&self) -> u64 {
        self.revision
    }

    pub fn widget_count(&self) -> usize {
        self.widgets.len()
    }

    pub fn negotiation_request(&mut self) -> Result<RendererRequest> {
        self.request(
            0,
            0,
            RendererCommand::Negotiate {
                supported: self.supported,
                max_frame_bytes: self.max_frame_bytes,
                max_widgets: self.max_widgets,
                max_queued_events: self.max_queued_events,
            },
        )
    }

    pub fn accept_negotiation(
        &mut self,
        request: &RendererRequest,
        response: &RendererResponse,
    ) -> Result<()> {
        self.validate_response(request, response)?;
        if request.version != 0 || !matches!(&request.body, RendererCommand::Negotiate { .. }) {
            bail!("renderer negotiation response did not match a negotiation request");
        }
        let RendererReply::Negotiated {
            supported,
            max_frame_bytes,
            max_widgets,
            max_queued_events,
            ..
        } = &response.body
        else {
            return response_error(&response.body);
        };
        let selected = self.supported.negotiate(*supported)?;
        if response.version != selected {
            bail!(
                "renderer negotiated version {} but replied with {}",
                selected,
                response.version
            );
        }
        if *max_frame_bytes == 0 || *max_frame_bytes > self.max_frame_bytes {
            bail!("renderer advertised an invalid frame limit");
        }
        if *max_widgets == 0 || *max_widgets > self.max_widgets {
            bail!("renderer advertised an invalid widget limit");
        }
        if *max_queued_events == 0 || *max_queued_events > self.max_queued_events {
            bail!("renderer advertised an invalid event queue limit");
        }
        self.negotiated = Some(selected);
        self.max_frame_bytes = *max_frame_bytes;
        self.max_widgets = *max_widgets;
        self.max_queued_events = *max_queued_events;
        Ok(())
    }

    pub fn instantiate_request(
        &mut self,
        applet_id: impl Into<String>,
        widget_id: impl Into<String>,
        contract: WidgetContract,
        properties: Value,
    ) -> Result<RendererRequest> {
        self.require_negotiated()?;
        let applet_id = applet_id.into();
        let widget_id = widget_id.into();
        validate_identifier(&applet_id, "applet identity")?;
        validate_identifier(&widget_id, "widget identity")?;
        contract.validate()?;
        validate_properties(&contract, &properties, true)?;
        let key = (applet_id.clone(), widget_id.clone());
        if self.widgets.contains_key(&key) {
            bail!("widget {applet_id:?}/{widget_id:?} is already active");
        }
        if self.widgets.len() >= self.max_widgets {
            bail!(
                "renderer widget limit {} has been reached",
                self.max_widgets
            );
        }
        self.mutation_request(RendererCommand::Instantiate {
            applet_id,
            widget_id,
            contract,
            properties,
        })
    }

    pub fn patch_request(
        &mut self,
        applet_id: &str,
        widget_id: &str,
        properties: Value,
    ) -> Result<RendererRequest> {
        self.require_negotiated()?;
        let key = (applet_id.to_owned(), widget_id.to_owned());
        let active = self
            .widgets
            .get(&key)
            .ok_or_else(|| anyhow!("unknown renderer widget {applet_id:?}/{widget_id:?}"))?;
        validate_properties(&active.contract, &properties, false)?;
        self.mutation_request(RendererCommand::Patch {
            applet_id: applet_id.into(),
            widget_id: widget_id.into(),
            properties,
        })
    }

    pub fn destroy_request(&mut self, applet_id: &str, widget_id: &str) -> Result<RendererRequest> {
        self.require_negotiated()?;
        if !self
            .widgets
            .contains_key(&(applet_id.into(), widget_id.into()))
        {
            bail!("unknown renderer widget {applet_id:?}/{widget_id:?}");
        }
        self.mutation_request(RendererCommand::Destroy {
            applet_id: applet_id.into(),
            widget_id: widget_id.into(),
        })
    }

    pub fn poll_request(&mut self, limit: usize) -> Result<RendererRequest> {
        self.require_negotiated()?;
        if !self.pending_mutations.is_empty() {
            bail!("renderer events cannot be polled while a widget mutation is unacknowledged");
        }
        if limit == 0 || limit > self.max_queued_events {
            bail!(
                "renderer event poll limit must be between 1 and {}",
                self.max_queued_events
            );
        }
        self.request(
            self.negotiated.unwrap(),
            self.revision,
            RendererCommand::PollEvents {
                after_cursor: self.event_cursor,
                limit,
            },
        )
    }

    pub fn ack_events_request(&mut self, through_cursor: u64) -> Result<RendererRequest> {
        self.require_negotiated()?;
        if !self.pending_mutations.is_empty() {
            bail!(
                "renderer events cannot be acknowledged while a widget mutation is unacknowledged"
            );
        }
        if through_cursor < self.event_cursor {
            bail!("renderer event cursor cannot move backwards");
        }
        if through_cursor > self.last_polled_cursor {
            bail!("renderer event cursor cannot acknowledge events that were not polled");
        }
        self.request(
            self.negotiated.unwrap(),
            self.revision,
            RendererCommand::AckEvents { through_cursor },
        )
    }

    /// Verify and commit an acknowledged mutation. A non-ack or mismatched
    /// envelope never mutates host-owned state.
    pub fn accept_applied(
        &mut self,
        request: &RendererRequest,
        response: &RendererResponse,
    ) -> Result<()> {
        self.validate_response(request, response)?;
        if !matches!(&response.body, RendererReply::Applied) {
            return response_error(&response.body);
        }
        let mutation = matches!(
            &request.body,
            RendererCommand::Instantiate { .. }
                | RendererCommand::Patch { .. }
                | RendererCommand::Destroy { .. }
        );
        if mutation
            && self.pending_mutations.front() != Some(&(request.request_id, request.revision))
        {
            bail!("renderer mutation acknowledgement is out of order or was not issued");
        }
        if !mutation
            && matches!(&request.body, RendererCommand::AckEvents { .. })
            && !self.pending_mutations.is_empty()
        {
            bail!("renderer event acknowledgement arrived during a widget mutation");
        }
        match &request.body {
            RendererCommand::Instantiate {
                applet_id,
                widget_id,
                contract,
                properties,
            } => {
                if request.revision <= self.revision {
                    bail!("renderer create revision is not monotonic");
                }
                self.widgets.insert(
                    (applet_id.clone(), widget_id.clone()),
                    ActiveWidget {
                        contract: contract.clone(),
                        properties: properties.clone(),
                        revision: request.revision,
                    },
                );
            }
            RendererCommand::Patch {
                applet_id,
                widget_id,
                properties,
            } => {
                if request.revision <= self.revision {
                    bail!("renderer patch revision is not monotonic");
                }
                let active = self
                    .widgets
                    .get_mut(&(applet_id.clone(), widget_id.clone()))
                    .ok_or_else(|| {
                        anyhow!("renderer acknowledged a patch for an unknown widget")
                    })?;
                merge_object(&mut active.properties, properties)?;
                active.revision = request.revision;
            }
            RendererCommand::Destroy {
                applet_id,
                widget_id,
            } => {
                if request.revision <= self.revision {
                    bail!("renderer destroy revision is not monotonic");
                }
                if self
                    .widgets
                    .remove(&(applet_id.clone(), widget_id.clone()))
                    .is_none()
                {
                    bail!("renderer acknowledged a destroy for an unknown widget");
                }
            }
            RendererCommand::AckEvents { through_cursor } => {
                if *through_cursor < self.event_cursor {
                    bail!("renderer event acknowledgement moved backwards");
                }
                self.event_cursor = *through_cursor;
            }
            RendererCommand::Negotiate { .. } | RendererCommand::PollEvents { .. } => {
                bail!("renderer command does not accept an applied acknowledgement");
            }
        }
        self.revision = request.revision;
        if mutation {
            self.pending_mutations.pop_front();
        }
        Ok(())
    }

    pub fn accept_event_batch(
        &mut self,
        request: &RendererRequest,
        response: &RendererResponse,
    ) -> Result<(Vec<RendererEvent>, u64)> {
        self.validate_response(request, response)?;
        let RendererCommand::PollEvents {
            after_cursor,
            limit,
        } = &request.body
        else {
            bail!("event batch response did not match an event poll request");
        };
        let RendererReply::EventBatch {
            events,
            next_cursor,
            oldest_cursor,
            overflow,
        } = &response.body
        else {
            return response_error(&response.body).map(|_| (Vec::new(), self.event_cursor));
        };
        if *overflow {
            bail!("renderer event queue overflowed; event delivery is no longer complete");
        }
        if *oldest_cursor > after_cursor.saturating_add(1) {
            bail!(
                "renderer event cursor has a gap (requested after {after_cursor}, oldest retained is {oldest_cursor})"
            );
        }
        if events.len() > *limit || *next_cursor < *after_cursor {
            bail!("renderer returned an invalid event batch cursor or size");
        }
        let mut last_cursor = *after_cursor;
        let mut accepted = Vec::with_capacity(events.len());
        for event in events {
            if event.cursor <= last_cursor || event.cursor > *next_cursor {
                bail!("renderer event cursors are not strictly increasing");
            }
            last_cursor = event.cursor;
            if self.validate_event(event)? {
                accepted.push(event.clone());
            }
        }
        if events.is_empty() && *next_cursor != *after_cursor {
            bail!("renderer advanced its event cursor without returning events");
        }
        if !events.is_empty() && *next_cursor != last_cursor {
            bail!("renderer event batch cursor does not match its last event");
        }
        self.last_polled_cursor = *next_cursor;
        Ok((accepted, *next_cursor))
    }

    pub fn accept_event(
        &self,
        event: &RendererEvent,
    ) -> Result<(EventDisposition, Option<RendererEvent>)> {
        let disposition = if event.generation != self.generation {
            EventDisposition::StaleGeneration
        } else if self
            .widgets
            .get(&(event.applet_id.clone(), event.widget_id.clone()))
            .is_some_and(|widget| event.source_revision < widget.revision)
        {
            EventDisposition::StaleRevision
        } else {
            EventDisposition::Accepted
        };
        self.validate_event(event)
            .map(|accepted| (disposition, accepted.then(|| event.clone())))
    }

    /// Start a new renderer generation while retaining the applet-owned widget
    /// snapshots. Callers negotiate the new process and replay these snapshots.
    pub fn begin_restart(&mut self) -> Result<()> {
        self.generation = self
            .generation
            .checked_add(1)
            .ok_or_else(|| anyhow!("renderer generation space exhausted"))?;
        self.negotiated = None;
        self.event_cursor = 0;
        self.last_polled_cursor = 0;
        self.last_issued_revision = self.revision;
        self.pending_mutations.clear();
        Ok(())
    }

    pub fn replay_requests(&mut self) -> Result<Vec<RendererRequest>> {
        self.require_negotiated()?;
        let snapshots = self
            .widgets
            .iter()
            .map(|((applet_id, widget_id), state)| {
                (
                    applet_id.clone(),
                    widget_id.clone(),
                    state.contract.clone(),
                    state.properties.clone(),
                )
            })
            .collect::<Vec<_>>();
        let mut requests = Vec::with_capacity(snapshots.len());
        for (applet_id, widget_id, contract, properties) in snapshots {
            let revision = self.next_revision()?;
            let request = self.request(
                self.negotiated.unwrap(),
                revision,
                RendererCommand::Instantiate {
                    applet_id,
                    widget_id,
                    contract,
                    properties,
                },
            )?;
            self.pending_mutations
                .push_back((request.request_id, request.revision));
            requests.push(request);
        }
        Ok(requests)
    }

    fn validate_event(&self, event: &RendererEvent) -> Result<bool> {
        if event.generation != self.generation {
            return Ok(false);
        }
        let key = (event.applet_id.clone(), event.widget_id.clone());
        let Some(widget) = self.widgets.get(&key) else {
            bail!("renderer emitted an event for an unknown widget");
        };
        if event.source_revision < widget.revision {
            return Ok(false);
        }
        if event.source_revision > widget.revision {
            bail!("renderer event references a future widget revision");
        }
        let signal = widget
            .contract
            .signals
            .get(&event.signal)
            .ok_or_else(|| anyhow!("renderer emitted undeclared signal {:?}", event.signal))?;
        let schema = schema_for_record(&signal.schema, None)?;
        schema
            .decode(&serde_json::to_vec(&event.payload)?)
            .context("validating renderer signal payload")?;
        Ok(true)
    }

    fn request(
        &mut self,
        version: u32,
        revision: u64,
        body: RendererCommand,
    ) -> Result<RendererRequest> {
        let request_id = self.next_request_id;
        self.next_request_id = self
            .next_request_id
            .checked_add(1)
            .ok_or_else(|| anyhow!("renderer request id space exhausted"))?;
        Ok(RendererRequest {
            protocol: RENDERER_PROTOCOL.into(),
            version,
            request_id,
            session_id: self.session_id.clone(),
            generation: self.generation,
            revision,
            body,
        })
    }

    fn next_revision(&mut self) -> Result<u64> {
        let revision = self
            .last_issued_revision
            .checked_add(1)
            .ok_or_else(|| anyhow!("renderer revision space exhausted"))?;
        self.last_issued_revision = revision;
        Ok(revision)
    }

    fn mutation_request(&mut self, body: RendererCommand) -> Result<RendererRequest> {
        if !self.pending_mutations.is_empty() {
            bail!(
                "renderer mutations must be acknowledged in order before another mutation is issued"
            );
        }
        let revision = self.next_revision()?;
        let request = self.request(self.negotiated.unwrap(), revision, body)?;
        self.pending_mutations
            .push_back((request.request_id, request.revision));
        Ok(request)
    }

    fn require_negotiated(&self) -> Result<()> {
        if self.negotiated.is_none() {
            bail!("renderer protocol has not been negotiated for this generation");
        }
        Ok(())
    }

    fn validate_response(
        &self,
        request: &RendererRequest,
        response: &RendererResponse,
    ) -> Result<()> {
        if response.protocol != RENDERER_PROTOCOL
            || response.request_id != request.request_id
            || response.session_id != self.session_id
            || response.generation != self.generation
        {
            bail!("renderer response identity does not match the active request");
        }
        let expected_version = if matches!(&request.body, RendererCommand::Negotiate { .. }) {
            response.version
        } else {
            self.negotiated
                .ok_or_else(|| anyhow!("renderer is not negotiated"))?
        };
        if response.version != expected_version {
            bail!("renderer response protocol version changed within a session");
        }
        if response.revision != request.revision {
            bail!("renderer response revision does not match its request");
        }
        Ok(())
    }
}

/// A real Quickshell process adapter. It launches only the explicitly supplied
/// QML config and uses argv-based `qs ipc call`; it never invokes a shell.
/// Dropping this value kills and reaps the renderer process it owns.
pub struct QuickShellRenderer {
    binary: PathBuf,
    config: PathBuf,
    _private_assets: Option<PrivateRendererAssets>,
    registry: crate::manifest::Registry,
    ipc_target: String,
    protocol: RendererProtocol,
    child: Option<Child>,
    timeout: Duration,
}

struct PrivateRendererAssets {
    directory: PathBuf,
    config: PathBuf,
}

impl PrivateRendererAssets {
    fn preflight(
        runtime_dir: &Path,
        registry: &crate::manifest::Registry,
        cwd: &Path,
    ) -> Result<()> {
        verify_private_runtime_directory(runtime_dir)?;
        capabilities::authorize_host_path(registry, runtime_dir, true, cwd)
            .context("authorizing host write for private Quickshell assets")?;
        capabilities::authorize_applet_host_path(registry, POLICY_APPLET, runtime_dir, false, cwd)
            .context("authorizing Quickshell child read for private assets")?;
        Ok(())
    }

    fn preflight_from_environment(
        registry: &crate::manifest::Registry,
        cwd: &Path,
    ) -> Result<PathBuf> {
        let runtime_dir = verified_xdg_runtime_directory()?;
        Self::preflight(&runtime_dir, registry, cwd)?;
        Ok(runtime_dir)
    }

    fn create(
        runtime_dir: &Path,
        session_id: &str,
        registry: &crate::manifest::Registry,
    ) -> Result<Self> {
        validate_identifier(session_id, "renderer session id")?;
        let cwd = std::env::current_dir().context("resolving renderer asset working directory")?;
        Self::preflight(runtime_dir, registry, &cwd)?;

        let mut random = [0_u8; 16];
        getrandom::fill(&mut random).context("generating private renderer asset identity")?;
        let suffix = random
            .iter()
            .map(|byte| format!("{byte:02x}"))
            .collect::<String>();
        let directory =
            runtime_dir.join(format!("execute-renderer-{}-{suffix}", std::process::id()));
        create_private_directory(&directory)?;
        verify_private_owned_directory(&directory)?;

        let assets = Self {
            config: directory.join("shell.qml"),
            directory,
        };
        let target = format!("{IPC_TARGET_PREFIX}{session_id}");
        let encoded_target = serde_json::to_string(&target)?;
        if QUICKSHELL_SOURCE.matches(IPC_TARGET_PLACEHOLDER).count() != 1 {
            bail!("embedded Quickshell source must contain exactly one IPC target placeholder");
        }
        let source = QUICKSHELL_SOURCE.replace(
            IPC_TARGET_PLACEHOLDER,
            &format!("property string ipcTarget: {encoded_target}"),
        );
        write_private_file(&assets.config, source.as_bytes())?;
        Ok(assets)
    }
}

impl Drop for PrivateRendererAssets {
    fn drop(&mut self) {
        if std::fs::symlink_metadata(&self.directory)
            .is_ok_and(|metadata| metadata.is_dir() && !metadata.file_type().is_symlink())
        {
            let _ = std::fs::remove_dir_all(&self.directory);
        }
    }
}

#[cfg(unix)]
fn current_uid() -> u32 {
    // SAFETY: geteuid has no pointer arguments or other memory preconditions.
    crate::platform::unix_ffi::effective_uid()
}

#[cfg(unix)]
fn verified_xdg_runtime_directory() -> Result<PathBuf> {
    let configured = std::env::var_os("XDG_RUNTIME_DIR")
        .ok_or_else(|| anyhow!("Quickshell requires XDG_RUNTIME_DIR"))?;
    let path = PathBuf::from(configured);
    if !path.is_absolute() {
        bail!("XDG_RUNTIME_DIR must be an absolute path");
    }
    verify_private_runtime_directory(&path)?;
    Ok(path)
}

#[cfg(not(unix))]
fn verified_xdg_runtime_directory() -> Result<PathBuf> {
    bail!("private Quickshell runtime assets are unsupported on this platform")
}

#[cfg(unix)]
fn verify_private_runtime_directory(path: &Path) -> Result<()> {
    use std::os::unix::fs::MetadataExt;

    let metadata = std::fs::symlink_metadata(path)
        .context("inspecting XDG_RUNTIME_DIR without following a symlink")?;
    if metadata.file_type().is_symlink() || !metadata.is_dir() {
        bail!("XDG_RUNTIME_DIR must be a real directory, not a symlink");
    }
    if metadata.uid() != current_uid() || metadata.mode() & 0o077 != 0 {
        bail!("XDG_RUNTIME_DIR must be user-owned and inaccessible to group/other");
    }
    if path.canonicalize().as_deref().ok() != Some(path) {
        bail!("XDG_RUNTIME_DIR must not contain symlink or noncanonical path components");
    }
    Ok(())
}

#[cfg(not(unix))]
fn verify_private_runtime_directory(_path: &Path) -> Result<()> {
    bail!("private Quickshell runtime assets are unsupported on this platform")
}

#[cfg(unix)]
fn create_private_directory(path: &Path) -> Result<()> {
    use std::os::unix::fs::DirBuilderExt;

    let mut builder = std::fs::DirBuilder::new();
    builder.mode(0o700);
    builder
        .create(path)
        .context("creating private Quickshell runtime directory")
}

#[cfg(not(unix))]
fn create_private_directory(_path: &Path) -> Result<()> {
    bail!("private Quickshell runtime assets are unsupported on this platform")
}

#[cfg(unix)]
fn verify_private_owned_directory(path: &Path) -> Result<()> {
    use std::os::unix::fs::MetadataExt;

    let metadata = std::fs::symlink_metadata(path)
        .context("inspecting private Quickshell runtime directory")?;
    if metadata.file_type().is_symlink()
        || !metadata.is_dir()
        || metadata.uid() != current_uid()
        || metadata.mode() & 0o077 != 0
    {
        bail!("private Quickshell directory has unexpected type, owner, or permissions");
    }
    Ok(())
}

#[cfg(not(unix))]
fn verify_private_owned_directory(_path: &Path) -> Result<()> {
    bail!("private Quickshell runtime assets are unsupported on this platform")
}

#[cfg(unix)]
fn write_private_file(path: &Path, bytes: &[u8]) -> Result<()> {
    use std::os::unix::fs::{MetadataExt, OpenOptionsExt};

    let mut options = std::fs::OpenOptions::new();
    options.write(true).create_new(true).mode(0o600);
    let mut file = options
        .open(path)
        .context("creating private embedded Quickshell config")?;
    file.write_all(bytes)
        .context("writing private embedded Quickshell config")?;
    file.sync_all()
        .context("syncing private embedded Quickshell config")?;
    let metadata =
        std::fs::symlink_metadata(path).context("verifying private embedded Quickshell config")?;
    if metadata.file_type().is_symlink()
        || !metadata.is_file()
        || metadata.uid() != current_uid()
        || metadata.mode() & 0o077 != 0
    {
        bail!("embedded Quickshell config has unexpected type, owner, or permissions");
    }
    Ok(())
}

#[cfg(not(unix))]
fn write_private_file(_path: &Path, _bytes: &[u8]) -> Result<()> {
    bail!("private Quickshell runtime assets are unsupported on this platform")
}

impl QuickShellRenderer {
    pub fn start(
        binary: impl AsRef<Path>,
        config: impl AsRef<Path>,
        session_id: impl Into<String>,
        supported: ProtocolRange,
    ) -> Result<Self> {
        Self::start_with_registry(
            binary,
            config,
            session_id,
            supported,
            crate::manifest::Registry::default(),
        )
    }

    pub fn start_with_registry(
        binary: impl AsRef<Path>,
        config: impl AsRef<Path>,
        session_id: impl Into<String>,
        supported: ProtocolRange,
        registry: crate::manifest::Registry,
    ) -> Result<Self> {
        Self::start_resolved(
            binary.as_ref().to_owned(),
            config.as_ref().to_owned(),
            session_id.into(),
            supported,
            registry,
            None,
        )
    }

    /// Start the embedded, packaged QML renderer from a private runtime copy.
    /// The asset owner remains attached to this value until its child is
    /// reaped, so the renderer never depends on a source checkout or package
    /// resource directory after launch.
    pub fn start_embedded_with_registry(
        binary: impl AsRef<Path>,
        session_id: impl Into<String>,
        supported: ProtocolRange,
        registry: crate::manifest::Registry,
    ) -> Result<Self> {
        let session_id = session_id.into();
        let runtime_dir = verified_xdg_runtime_directory()?;
        let assets = PrivateRendererAssets::create(&runtime_dir, &session_id, &registry)?;
        Self::start_resolved(
            binary.as_ref().to_owned(),
            assets.config.clone(),
            session_id,
            supported,
            registry,
            Some(assets),
        )
    }

    fn start_resolved(
        binary: PathBuf,
        config: PathBuf,
        session_id: String,
        supported: ProtocolRange,
        registry: crate::manifest::Registry,
        private_assets: Option<PrivateRendererAssets>,
    ) -> Result<Self> {
        if !config.is_file() {
            bail!("Quickshell config does not exist: {}", config.display());
        }
        validate_identifier(&session_id, "renderer session id")?;
        let ipc_target = format!("{IPC_TARGET_PREFIX}{session_id}");
        let mut renderer = Self {
            binary,
            config,
            _private_assets: private_assets,
            registry,
            ipc_target,
            protocol: RendererProtocol::new(session_id, supported)?,
            child: None,
            timeout: DEFAULT_COMMAND_TIMEOUT,
        };
        renderer.spawn_child()?;
        if let Err(error) = renderer.negotiate_until_ready() {
            renderer.terminate_owned_child();
            return Err(error).context("starting Quickshell renderer");
        }
        Ok(renderer)
    }

    pub fn start_default(session_id: impl Into<String>) -> Result<Self> {
        Self::start_embedded_with_registry(
            "qs",
            session_id,
            ProtocolRange::new(RENDERER_PROTOCOL_VERSION, RENDERER_PROTOCOL_VERSION)?,
            crate::manifest::Registry::default(),
        )
    }

    pub fn generation(&self) -> u64 {
        self.protocol.generation()
    }

    pub fn instantiate(
        &mut self,
        applet_id: &str,
        widget_id: &str,
        contract: WidgetContract,
        properties: Value,
    ) -> Result<()> {
        let request = self
            .protocol
            .instantiate_request(applet_id, widget_id, contract, properties)?;
        self.send_applied(request)
    }

    pub fn patch(&mut self, applet_id: &str, widget_id: &str, properties: Value) -> Result<()> {
        let request = self
            .protocol
            .patch_request(applet_id, widget_id, properties)?;
        self.send_applied(request)
    }

    pub fn destroy(&mut self, applet_id: &str, widget_id: &str) -> Result<()> {
        let request = self.protocol.destroy_request(applet_id, widget_id)?;
        self.send_applied(request)
    }

    pub fn poll_events(&mut self, limit: usize) -> Result<Vec<RendererEvent>> {
        let (events, next_cursor) = self.poll_events_unacked(limit)?;
        if next_cursor > self.protocol.event_cursor {
            self.acknowledge_events(next_cursor)?;
        }
        Ok(events)
    }

    /// Read a validated event batch without acknowledging its renderer-local
    /// cursor. Graph adapters should emit the typed signal through NodeIo
    /// before calling [`Self::acknowledge_events`].
    pub fn poll_events_unacked(&mut self, limit: usize) -> Result<(Vec<RendererEvent>, u64)> {
        let request = self.protocol.poll_request(limit)?;
        let response = self.send(&request)?;
        self.protocol.accept_event_batch(&request, &response)
    }

    pub fn acknowledge_events(&mut self, through_cursor: u64) -> Result<()> {
        let request = self.protocol.ack_events_request(through_cursor)?;
        self.send_applied(request)
    }

    /// Restart this renderer and reconstruct active widgets from the last
    /// acknowledged Rust-owned state. A failed request is not retried; the
    /// caller must decide whether to restart after a reported uncertain result.
    pub fn restart(&mut self) -> Result<()> {
        self.terminate_owned_child();
        self.protocol.begin_restart()?;
        self.spawn_child()?;
        if let Err(error) = self.negotiate_until_ready() {
            self.terminate_owned_child();
            return Err(error).context("renegotiating restarted Quickshell renderer");
        }
        for request in self.protocol.replay_requests()? {
            if let Err(error) = self.send_applied(request) {
                self.terminate_owned_child();
                return Err(error).context("replaying renderer widget after restart");
            }
        }
        Ok(())
    }

    /// Stop and reap the owned renderer. A parent applet should call destroy
    /// for its widgets before this on normal cancellation; process teardown is
    /// still guaranteed if the parent or graph is cancelled unexpectedly.
    pub fn shutdown(&mut self) {
        self.terminate_owned_child();
    }

    pub fn is_running(&mut self) -> Result<bool> {
        Ok(!self.child_exited()?)
    }

    fn negotiate_until_ready(&mut self) -> Result<()> {
        let deadline = Instant::now() + DEFAULT_STARTUP_TIMEOUT;
        let mut last_error = None;
        while Instant::now() < deadline {
            if self.child_exited()? {
                bail!("Quickshell exited before registering its IPC handler");
            }
            let request = self.protocol.negotiation_request()?;
            match self.send_raw(&request) {
                Ok(response) => match self.protocol.accept_negotiation(&request, &response) {
                    Ok(()) => return Ok(()),
                    Err(error) => return Err(error),
                },
                Err(error) => {
                    last_error = Some(error);
                    thread::sleep(Duration::from_millis(80));
                }
            }
        }
        Err(last_error.unwrap_or_else(|| anyhow!("timed out waiting for Quickshell IPC readiness")))
    }

    fn spawn_child(&mut self) -> Result<()> {
        if self.child.is_some() {
            bail!("Quickshell renderer child is already running");
        }
        let mut command =
            policy_command(&self.binary, launch_arguments(&self.config), &self.registry)?;
        let child = command
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .spawn()
            .with_context(|| {
                format!(
                    "launching {} --path {}",
                    self.binary.display(),
                    self.config.display()
                )
            })?;
        self.child = Some(child);
        Ok(())
    }

    fn send_applied(&mut self, request: RendererRequest) -> Result<()> {
        let response = match self.send_raw(&request) {
            Ok(response) => response,
            Err(error) => {
                // A failed IPC call may have been applied before its reply was
                // lost. Retire this process to prevent local/renderer state
                // from diverging; the caller can explicitly restart and replay.
                self.terminate_owned_child();
                return Err(error)
                    .context("renderer mutation outcome is uncertain; owned renderer stopped");
            }
        };
        if let Err(error) = self.protocol.accept_applied(&request, &response) {
            if matches!(&response.body, RendererReply::Error { .. }) {
                return Err(error);
            }
            self.terminate_owned_child();
            return Err(error)
                .context("renderer mutation acknowledgement was invalid; owned renderer stopped");
        }
        Ok(())
    }

    fn send(&mut self, request: &RendererRequest) -> Result<RendererResponse> {
        match self.send_raw(request) {
            Ok(response) => Ok(response),
            Err(error) => {
                self.terminate_owned_child();
                Err(error).context("renderer request outcome is uncertain; owned renderer stopped")
            }
        }
    }

    fn send_raw(&mut self, request: &RendererRequest) -> Result<RendererResponse> {
        let bytes = serde_json::to_vec(request).context("encoding renderer request")?;
        if bytes.len() > self.protocol.max_frame_bytes {
            bail!(
                "renderer request exceeds {} byte frame limit",
                self.protocol.max_frame_bytes
            );
        }
        if self.child.is_none() || self.child_exited()? {
            bail!("owned Quickshell renderer is not running");
        }
        let frame = String::from_utf8(bytes).expect("serde_json emits UTF-8");
        let mut command = policy_command(
            &self.binary,
            ipc_arguments(&self.ipc_target, &frame),
            &self.registry,
        )?;
        let output = run_bounded(&mut command, self.protocol.max_frame_bytes, self.timeout)?;
        serde_json::from_slice::<RendererResponse>(&output)
            .context("decoding Quickshell renderer response frame")
    }

    fn child_exited(&mut self) -> Result<bool> {
        let Some(child) = &mut self.child else {
            return Ok(true);
        };
        Ok(child.try_wait()?.is_some())
    }

    fn terminate_owned_child(&mut self) {
        if let Some(mut child) = self.child.take() {
            let _ = child.kill();
            let _ = child.wait();
        }
    }
}

impl Drop for QuickShellRenderer {
    fn drop(&mut self) {
        self.terminate_owned_child();
    }
}

/// Graph executor that routes ordinary graph nodes through the shared runtime
/// backend and owns one Quickshell process for each persistent renderer node.
/// The renderer node id is the stable applet identity; its single `main`
/// widget is a generic `text` or `button` component selected by the backend.
pub struct DesktopGraphExecutor {
    runtime: RuntimeGraphExecutor,
    registry: crate::manifest::Registry,
    quickshell_binary: PathBuf,
    quickshell_config: PathBuf,
    embedded_quickshell: bool,
    #[cfg(feature = "tui")]
    tui: std::sync::Arc<std::sync::Mutex<TuiRendererState>>,
}

impl DesktopGraphExecutor {
    pub fn new(
        registry: crate::manifest::Registry,
        options: crate::runtime::RunOptions,
        quickshell_binary: impl Into<PathBuf>,
        quickshell_config: impl Into<PathBuf>,
    ) -> Self {
        Self::from_runtime(
            RuntimeGraphExecutor::new(registry.clone(), options),
            registry,
            quickshell_binary,
            quickshell_config,
            false,
        )
    }

    pub fn with_module_session(
        registry: crate::manifest::Registry,
        options: crate::runtime::RunOptions,
        quickshell_binary: impl Into<PathBuf>,
        quickshell_config: impl Into<PathBuf>,
        module_session: std::sync::Arc<crate::modules::ProgramModuleSession>,
    ) -> Self {
        Self::from_runtime(
            RuntimeGraphExecutor::with_module_session(registry.clone(), options, module_session),
            registry,
            quickshell_binary,
            quickshell_config,
            false,
        )
    }

    fn from_runtime(
        runtime: RuntimeGraphExecutor,
        registry: crate::manifest::Registry,
        quickshell_binary: impl Into<PathBuf>,
        quickshell_config: impl Into<PathBuf>,
        embedded_quickshell: bool,
    ) -> Self {
        Self {
            runtime,
            registry,
            quickshell_binary: quickshell_binary.into(),
            quickshell_config: quickshell_config.into(),
            embedded_quickshell,
            #[cfg(feature = "tui")]
            tui: std::sync::Arc::new(std::sync::Mutex::new(TuiRendererState::default())),
        }
    }

    fn with_embedded_quickshell_module_session(
        registry: crate::manifest::Registry,
        options: crate::runtime::RunOptions,
        quickshell_binary: impl Into<PathBuf>,
        module_session: std::sync::Arc<crate::modules::ProgramModuleSession>,
    ) -> Self {
        Self::from_runtime(
            RuntimeGraphExecutor::with_module_session(registry.clone(), options, module_session),
            registry,
            quickshell_binary,
            PathBuf::new(),
            true,
        )
    }

    pub fn with_default_quickshell(
        registry: crate::manifest::Registry,
        options: crate::runtime::RunOptions,
    ) -> Self {
        Self::from_runtime(
            RuntimeGraphExecutor::new(registry.clone(), options),
            registry,
            "qs",
            PathBuf::new(),
            true,
        )
    }

    fn execute_quickshell(
        &self,
        node: &GraphNode,
        io: &mut NodeIo,
        context: &GraphContext,
        backend: &str,
    ) -> Result<()> {
        let component = backend
            .strip_prefix("quickshell:")
            .ok_or_else(|| anyhow!("unsupported desktop renderer backend {backend:?}"))?;
        if !matches!(component, "text" | "button") {
            bail!("unsupported Quickshell component {component:?}");
        }
        let contract = validate_renderer_node(node, component)?;
        let session_id = renderer_session_id(node, context);
        let mut renderer: Option<QuickShellRenderer> = None;
        if let Some(initial) = io.recv("properties")? {
            ensure_current_graph_event(&initial, context)?;
            let initial = graph_record_json(initial.payload, "Quickshell initial properties")?;
            renderer =
                Some(self.create_quickshell_widget(node, &session_id, &contract, initial)?);
        }

        let outcome = 'renderer: loop {
            if io.is_cancelled() {
                break Ok(());
            }
            if let Some(active) = renderer.as_mut() {
                let (events, next_cursor) = match active.poll_events_unacked(EVENT_POLL_LIMIT) {
                    Ok(batch) => batch,
                    Err(_error) if !active.is_running()? => {
                        active
                            .restart()
                            .context("recovering failed Quickshell renderer")?;
                        active
                            .poll_events_unacked(EVENT_POLL_LIMIT)
                            .context("polling restarted Quickshell renderer")?
                    }
                    Err(error) => break Err(error).context("polling Quickshell renderer events"),
                };
                for event in events {
                    if event.applet_id != node.id || event.widget_id != WIDGET_ID {
                        break 'renderer Err(anyhow!(
                            "Quickshell emitted a signal for an unowned widget"
                        ));
                    }
                    io.emit(
                        &event.signal,
                        PortValue::Value(structured::from_json(event.payload)),
                    )?;
                }
                if next_cursor > active.protocol.event_cursor {
                    active.acknowledge_events(next_cursor)?;
                }
            }

            let update = match io.recv_timeout("property_updates", EVENT_POLL_INTERVAL) {
                Ok(update) => update,
                Err(_) if io.is_cancelled() => break 'renderer Ok(()),
                Err(error) => break Err(error).context("receiving Quickshell property update"),
            };
            let Some(update) = update else {
                // recv_timeout returns immediately when an optional port has
                // no bound producers or every producer has closed.
                thread::sleep(EVENT_POLL_INTERVAL);
                continue;
            };
            if update.generation != context.generation || update.revision != context.revision {
                // Old graph messages must never roll back a newer renderer state.
                continue;
            }
            let properties = match graph_record_json(update.payload, "Quickshell property update") {
                Ok(properties) => properties,
                Err(error) => break 'renderer Err(error),
            };
            if let Some(active) = renderer.as_mut() {
                if let Err(error) = active.patch(&node.id, WIDGET_ID, properties.clone()) {
                    if active.is_running()? {
                        break 'renderer Err(error)
                            .context("updating Quickshell widget properties");
                    }
                    if let Err(restart_error) = active.restart() {
                        break 'renderer Err(restart_error)
                            .context("recovering failed Quickshell property update");
                    }
                    if let Err(retry_error) = active.patch(&node.id, WIDGET_ID, properties) {
                        break 'renderer Err(retry_error)
                            .context("reapplying Quickshell property update after restart");
                    }
                }
            } else {
                match self.create_quickshell_widget(node, &session_id, &contract, properties) {
                    Ok(created) => renderer = Some(created),
                    Err(error) => break 'renderer Err(error),
                }
            }
        };

        if io.is_cancelled() {
            if let Some(active) = renderer.as_mut() {
                if active.is_running()? {
                    let _ = active.destroy(&node.id, WIDGET_ID);
                }
                active.shutdown();
            }
            return Ok(());
        }
        outcome?;
        if let Some(active) = renderer.as_mut() {
            if active.is_running()? {
                active.destroy(&node.id, WIDGET_ID)?;
            }
            active.shutdown();
        }
        Ok(())
    }

    fn create_quickshell_widget(
        &self,
        node: &GraphNode,
        session_id: &str,
        contract: &WidgetContract,
        properties: Value,
    ) -> Result<QuickShellRenderer> {
        let mut renderer = if self.embedded_quickshell {
            QuickShellRenderer::start_embedded_with_registry(
                &self.quickshell_binary,
                session_id,
                ProtocolRange::new(RENDERER_PROTOCOL_VERSION, RENDERER_PROTOCOL_VERSION)?,
                self.registry.clone(),
            )?
        } else {
            QuickShellRenderer::start_with_registry(
                &self.quickshell_binary,
                &self.quickshell_config,
                session_id,
                ProtocolRange::new(RENDERER_PROTOCOL_VERSION, RENDERER_PROTOCOL_VERSION)?,
                self.registry.clone(),
            )?
        };
        if let Err(error) =
            renderer.instantiate(&node.id, WIDGET_ID, contract.clone(), properties.clone())
        {
            if !renderer.is_running()? {
                renderer
                    .restart()
                    .context("recovering uncertain renderer widget creation")?;
                renderer.instantiate(&node.id, WIDGET_ID, contract.clone(), properties)?;
            } else {
                return Err(error).context("creating Quickshell widget");
            }
        }
        Ok(renderer)
    }

    #[cfg(not(feature = "tui"))]
    fn execute_tui(
        &self,
        _node: &GraphNode,
        _io: &mut NodeIo,
        _context: &GraphContext,
        backend: &str,
    ) -> Result<()> {
        bail!("desktop renderer backend {backend:?} requires the tui feature")
    }

    #[cfg(feature = "tui")]
    fn execute_tui(
        &self,
        node: &GraphNode,
        io: &mut NodeIo,
        context: &GraphContext,
        backend: &str,
    ) -> Result<()> {
        let component = backend
            .strip_prefix("tui:")
            .ok_or_else(|| anyhow!("unsupported desktop renderer backend {backend:?}"))?;
        if !matches!(component, "text" | "button") {
            bail!("unsupported TUI component {component:?}");
        }
        let _contract = validate_renderer_node(node, component)?;
        let key = format!("{}::{WIDGET_ID}", node.id);
        let mut widget_active = false;
        if let Some(initial) = io.recv("properties")? {
            ensure_current_graph_event(&initial, context)?;
            let initial = graph_record_json(initial.payload, "TUI initial properties")?;
            if let Err(error) = self.tui_lock()?.add(key.clone(), component, &initial) {
                let _ = self.tui_lock()?.remove(&key);
                return Err(error);
            }
            widget_active = true;
        }

        let outcome = (|| -> Result<()> {
            loop {
                if io.is_cancelled() {
                    return Ok(());
                }
                let clicked = {
                    let mut tui = self.tui_lock()?;
                    if tui.poll_input(Duration::ZERO)? {
                        io.cancellation_flag()
                            .store(true, std::sync::atomic::Ordering::Release);
                        return Ok(());
                    }
                    if widget_active && component == "button" {
                        tui.take_click(&key)?
                    } else {
                        false
                    }
                };
                if clicked {
                    io.emit(
                        "clicked",
                        PortValue::Value(structured::from_json(serde_json::json!({}))),
                    )?;
                }
                let update = io.recv_timeout("property_updates", EVENT_POLL_INTERVAL)?;
                let Some(update) = update else {
                    thread::sleep(EVENT_POLL_INTERVAL);
                    continue;
                };
                if update.generation != context.generation || update.revision != context.revision {
                    continue;
                }
                let properties = graph_record_json(update.payload, "TUI property update")?;
                if widget_active {
                    self.tui_lock()?.patch(&key, &properties)?;
                } else {
                    self.tui_lock()?.add(key.clone(), component, &properties)?;
                    widget_active = true;
                }
            }
        })();
        let cleanup = self.tui_lock()?.remove(&key);
        outcome?;
        cleanup?;
        Ok(())
    }

    #[cfg(feature = "tui")]
    fn tui_lock(&self) -> Result<std::sync::MutexGuard<'_, TuiRendererState>> {
        self.tui
            .lock()
            .map_err(|_| anyhow!("TUI renderer state lock was poisoned"))
    }
}

/// Apply one canonical node contract to both renderer implementations. The
/// initial single-value port is optional; the first record on the update stream
/// may instantiate the widget, with later records applied as patches.
fn validate_renderer_node(node: &GraphNode, component: &str) -> Result<WidgetContract> {
    if !node.outputs.is_empty() {
        bail!("renderer nodes expose typed signals through event ports, not outputs");
    }
    if node
        .inputs
        .keys()
        .any(|name| name != "properties" && name != "property_updates")
    {
        bail!("renderer node has an unsupported input port");
    }
    let properties = node
        .inputs
        .get("properties")
        .ok_or_else(|| anyhow!("renderer node needs a `properties` input"))?;
    if properties.delivery != PortDelivery::Single || properties.schema.kind != OutputType::Record {
        bail!("renderer `properties` input must be a single record port");
    }
    properties.schema.validate()?;
    let updates = node
        .inputs
        .get("property_updates")
        .ok_or_else(|| anyhow!("renderer node needs a `property_updates` events port"))?;
    if updates.delivery != PortDelivery::Records
        || updates.schema.kind != OutputType::Record
        || updates.required
    {
        bail!(
            "renderer `property_updates` must be an optional records port with a partial record schema"
        );
    }
    updates.schema.validate()?;
    if updates
        .schema
        .fields
        .iter()
        .any(|(name, schema)| properties.schema.fields.get(name) != Some(schema))
    {
        bail!("renderer property update schema must be a subset of the properties schema");
    }
    if updates
        .schema
        .required
        .iter()
        .any(|field| !updates.schema.fields.contains_key(field))
    {
        bail!("renderer property update required fields must be declared in its schema");
    }
    let contract = WidgetContract {
        component: component.to_owned(),
        properties: properties.clone(),
        signals: node.events.clone(),
    };
    contract.validate()?;
    Ok(contract)
}

impl GraphExecutor for DesktopGraphExecutor {
    fn execute(&self, node: &GraphNode, io: &mut NodeIo, context: &GraphContext) -> Result<()> {
        match &node.kind {
            GraphNodeKind::Renderer { backend } if backend.starts_with("quickshell:") => {
                self.execute_quickshell(node, io, context, backend)
            }
            GraphNodeKind::Renderer { backend } if backend.starts_with("tui:") => {
                self.execute_tui(node, io, context, backend)
            }
            GraphNodeKind::Renderer { backend } => {
                bail!("desktop renderer backend {backend:?} is not installed")
            }
            _ => self.runtime.execute(node, io, context),
        }
    }
}

#[derive(Debug, Clone)]
#[cfg(feature = "tui")]
struct TuiWidget {
    key: String,
    component: String,
    text: String,
    enabled: bool,
}

#[derive(Debug, Default)]
#[cfg(feature = "tui")]
struct TuiRendererState {
    widgets: Vec<TuiWidget>,
    buttons: Vec<String>,
    focused: Option<usize>,
    clicks: BTreeMap<String, VecDeque<()>>,
    active: bool,
    lease_held: bool,
}

#[cfg(feature = "tui")]
static TUI_TERMINAL_LEASE: std::sync::OnceLock<std::sync::Mutex<bool>> = std::sync::OnceLock::new();

#[cfg(feature = "tui")]
impl TuiRendererState {
    fn add(&mut self, key: String, component: &str, properties: &Value) -> Result<()> {
        if self.widgets.len() >= DEFAULT_MAX_WIDGETS {
            bail!("TUI renderer widget limit has been reached");
        }
        if self.widgets.iter().any(|widget| widget.key == key) {
            bail!("TUI widget {key:?} is already active");
        }
        let text = properties
            .get("text")
            .and_then(Value::as_str)
            .ok_or_else(|| anyhow!("TUI component requires a text property"))?
            .to_owned();
        let enabled = match component {
            "text" => true,
            "button" => properties
                .get("enabled")
                .and_then(Value::as_bool)
                .ok_or_else(|| anyhow!("TUI button requires an enabled boolean property"))?,
            _ => bail!("unsupported TUI component {component:?}"),
        };
        if !self.active {
            self.enter_terminal()?;
        }
        if component == "button" {
            self.buttons.push(key.clone());
            self.clicks.insert(key.clone(), VecDeque::new());
            if self.focused.is_none() {
                self.focused = Some(0);
            }
        }
        self.widgets.push(TuiWidget {
            key,
            component: component.to_owned(),
            text,
            enabled,
        });
        self.render()
    }

    fn patch(&mut self, key: &str, properties: &Value) -> Result<()> {
        let widget = self
            .widgets
            .iter_mut()
            .find(|widget| widget.key == key)
            .ok_or_else(|| anyhow!("cannot patch unknown TUI widget {key:?}"))?;
        if let Some(text) = properties.get("text") {
            widget.text = text
                .as_str()
                .ok_or_else(|| anyhow!("TUI widget text property must be text"))?
                .to_owned();
        }
        if let Some(enabled) = properties.get("enabled") {
            if widget.component != "button" {
                bail!("TUI text component has no enabled property");
            }
            widget.enabled = enabled
                .as_bool()
                .ok_or_else(|| anyhow!("TUI button enabled property must be boolean"))?;
        }
        self.render()
    }

    fn remove(&mut self, key: &str) -> Result<()> {
        let Some(index) = self.widgets.iter().position(|widget| widget.key == key) else {
            return Ok(());
        };
        self.widgets.remove(index);
        if let Some(button_index) = self.buttons.iter().position(|button| button == key) {
            self.buttons.remove(button_index);
            self.clicks.remove(key);
            self.focused = match (self.focused, self.buttons.len()) {
                (_, 0) => None,
                (Some(focused), len) => Some(focused.min(len - 1)),
                (None, _) => Some(0),
            };
        }
        if self.widgets.is_empty() {
            self.leave_terminal();
            Ok(())
        } else {
            self.render()
        }
    }

    fn poll_input(&mut self, timeout: Duration) -> Result<bool> {
        if !self.active || !crossterm::event::poll(timeout)? {
            return Ok(false);
        }
        let crossterm::event::Event::Key(key) = crossterm::event::read()? else {
            return Ok(false);
        };
        if key.kind != crossterm::event::KeyEventKind::Press {
            return Ok(false);
        }
        if key.code == crossterm::event::KeyCode::Esc
            || (key.code == crossterm::event::KeyCode::Char('c')
                && key
                    .modifiers
                    .contains(crossterm::event::KeyModifiers::CONTROL))
        {
            return Ok(true);
        }
        if self.buttons.is_empty() {
            return Ok(false);
        }
        match key.code {
            crossterm::event::KeyCode::Tab | crossterm::event::KeyCode::Down => {
                let next = self.focused.unwrap_or(0).saturating_add(1) % self.buttons.len();
                self.focused = Some(next);
                self.render()?;
            }
            crossterm::event::KeyCode::BackTab | crossterm::event::KeyCode::Up => {
                let current = self.focused.unwrap_or(0) % self.buttons.len();
                self.focused = Some((current + self.buttons.len() - 1) % self.buttons.len());
                self.render()?;
            }
            crossterm::event::KeyCode::Enter | crossterm::event::KeyCode::Char(' ') => {
                let Some(index) = self.focused else {
                    return Ok(false);
                };
                let key = &self.buttons[index.min(self.buttons.len() - 1)];
                let widget = self
                    .widgets
                    .iter()
                    .find(|widget| &widget.key == key)
                    .ok_or_else(|| anyhow!("focused TUI button has no active widget"))?;
                if !widget.enabled {
                    return Ok(false);
                }
                let queue = self
                    .clicks
                    .get_mut(key)
                    .ok_or_else(|| anyhow!("focused TUI button has no event queue"))?;
                if queue.len() >= DEFAULT_MAX_QUEUED_EVENTS {
                    bail!("TUI button event queue overflowed");
                }
                queue.push_back(());
            }
            _ => {}
        }
        Ok(false)
    }

    fn take_click(&mut self, key: &str) -> Result<bool> {
        Ok(self
            .clicks
            .get_mut(key)
            .ok_or_else(|| anyhow!("TUI button {key:?} has no event queue"))?
            .pop_front()
            .is_some())
    }

    fn enter_terminal(&mut self) -> Result<()> {
        if !io::stdin().is_terminal() || !io::stdout().is_terminal() {
            bail!("TUI renderer requires an interactive stdin and stdout terminal");
        }
        acquire_tui_terminal_lease()?;
        if let Err(error) = crossterm::terminal::enable_raw_mode() {
            release_tui_terminal_lease();
            return Err(error).context("enabling TUI raw mode");
        }
        let mut stdout = io::stdout();
        if let Err(error) = crossterm::execute!(
            stdout,
            crossterm::terminal::EnterAlternateScreen,
            crossterm::cursor::Hide,
            crossterm::terminal::Clear(crossterm::terminal::ClearType::All)
        ) {
            let _ = crossterm::terminal::disable_raw_mode();
            release_tui_terminal_lease();
            return Err(error).context("entering TUI alternate screen");
        }
        self.active = true;
        self.lease_held = true;
        Ok(())
    }

    fn render(&mut self) -> Result<()> {
        if !self.active {
            return Ok(());
        }
        let mut stdout = io::stdout();
        crossterm::execute!(
            stdout,
            crossterm::cursor::MoveTo(0, 0),
            crossterm::terminal::Clear(crossterm::terminal::ClearType::All)
        )?;
        writeln!(
            stdout,
            "Execute renderer  |  Tab/↑/↓ focus  Enter/Space activate"
        )?;
        for widget in &self.widgets {
            let safe_text = sanitize_terminal_text(&widget.text);
            match widget.component.as_str() {
                "text" => writeln!(stdout, "  {safe_text}")?,
                "button" => {
                    let selected = self
                        .focused
                        .and_then(|index| self.buttons.get(index))
                        .is_some_and(|key| key == &widget.key);
                    let marker = if selected { ">" } else { " " };
                    let state = if widget.enabled { "" } else { " (disabled)" };
                    writeln!(stdout, "{marker} [{safe_text}]{state}")?;
                }
                _ => {}
            }
        }
        stdout.flush()?;
        Ok(())
    }

    fn leave_terminal(&mut self) {
        if !self.active && !self.lease_held {
            return;
        }
        let mut stdout = io::stdout();
        let _ = crossterm::execute!(stdout, crossterm::cursor::Show);
        let _ = crossterm::execute!(stdout, crossterm::terminal::LeaveAlternateScreen);
        let _ = crossterm::terminal::disable_raw_mode();
        self.active = false;
        if self.lease_held {
            self.lease_held = false;
            release_tui_terminal_lease();
        }
    }
}

#[cfg(feature = "tui")]
fn acquire_tui_terminal_lease() -> Result<()> {
    let lease = TUI_TERMINAL_LEASE.get_or_init(|| std::sync::Mutex::new(false));
    let mut occupied = match lease.lock() {
        Ok(occupied) => occupied,
        Err(poisoned) => poisoned.into_inner(),
    };
    if *occupied {
        bail!("another graph currently owns the interactive TUI terminal");
    }
    *occupied = true;
    Ok(())
}

#[cfg(feature = "tui")]
fn release_tui_terminal_lease() {
    let lease = TUI_TERMINAL_LEASE.get_or_init(|| std::sync::Mutex::new(false));
    let mut occupied = match lease.lock() {
        Ok(occupied) => occupied,
        Err(poisoned) => poisoned.into_inner(),
    };
    *occupied = false;
}

/// Replace terminal control characters with visible Unicode markers before
/// rendering applet-provided text, so untrusted data cannot issue ANSI, OSC,
/// carriage-return, or backspace terminal commands.
#[cfg(feature = "tui")]
fn sanitize_terminal_text(value: &str) -> String {
    let mut sanitized = String::with_capacity(value.len());
    for character in value.chars() {
        match character as u32 {
            0x00..=0x1f => sanitized.push(char::from_u32(0x2400 + character as u32).unwrap()),
            0x7f => sanitized.push('\u{2421}'),
            0x80..=0x9f => {
                sanitized.push_str(&format!("‹U+{:04X}›", character as u32));
            }
            _ => sanitized.push(character),
        }
    }
    sanitized
}

#[cfg(feature = "tui")]
impl Drop for TuiRendererState {
    fn drop(&mut self) {
        self.leave_terminal();
    }
}

/// Select the renderer-aware executor only when the graph contains renderer
/// nodes. Renderer commands, configuration and capability policy are checked
/// before [`GraphSession::start`] can launch any applet node.
pub fn graph_executor(
    plan: &GraphPlan,
    registry: crate::manifest::Registry,
    options: crate::runtime::RunOptions,
) -> Result<std::sync::Arc<dyn GraphExecutor>> {
    let module_session = std::sync::Arc::new(
        crate::modules::ProgramModuleSession::preload_invocations(&registry, plan.invocations())?,
    );
    graph_executor_with_module_session(plan, registry, options, module_session)
}

/// Variant for graph drivers that already preloaded the plan-wide module
/// snapshot. This keeps desktop and ordinary graph applet nodes on one source
/// snapshot without resolving package contents twice.
pub fn graph_executor_with_module_session(
    plan: &GraphPlan,
    registry: crate::manifest::Registry,
    options: crate::runtime::RunOptions,
    module_session: std::sync::Arc<crate::modules::ProgramModuleSession>,
) -> Result<std::sync::Arc<dyn GraphExecutor>> {
    let renderer_nodes = plan
        .nodes()
        .iter()
        .filter(|node| matches!(node.kind, GraphNodeKind::Renderer { .. }))
        .collect::<Vec<_>>();
    if renderer_nodes.is_empty() {
        return Ok(std::sync::Arc::new(
            RuntimeGraphExecutor::with_module_session(registry, options, module_session),
        ));
    }

    let mut needs_quickshell = false;
    for node in &renderer_nodes {
        let GraphNodeKind::Renderer { backend } = &node.kind else {
            unreachable!()
        };
        let component = if let Some(component) = backend.strip_prefix("quickshell:") {
            needs_quickshell = true;
            component
        } else if let Some(component) = backend.strip_prefix("tui:") {
            if !io::stdin().is_terminal() || !io::stdout().is_terminal() {
                bail!("TUI renderer requires an interactive stdin and stdout terminal");
            }
            component
        } else {
            bail!("desktop renderer backend {backend:?} is not installed");
        };
        if !matches!(component, "text" | "button") {
            bail!("unsupported Quickshell component {component:?}");
        }
        validate_renderer_node(node, component)
            .with_context(|| format!("preflighting renderer node {:?}", node.id))?;
    }

    let mut quickshell_config = PathBuf::new();
    let quickshell_binary = if needs_quickshell {
        let cwd = std::env::current_dir().context("resolving renderer launch working directory")?;
        let runtime_dir = PrivateRendererAssets::preflight_from_environment(&registry, &cwd)?;
        // This non-existent path is only used to validate the exact argv and
        // policy shape. Check mode never writes renderer assets.
        quickshell_config = runtime_dir.join("execute-renderer-preflight.qml");
        let binary = resolve_executable(Path::new("qs"))
            .context("preflighting Quickshell renderer executable")?;
        preflight_renderer_commands(&binary, &quickshell_config, &registry)?;
        binary
    } else {
        PathBuf::from("qs")
    };
    let executor = if needs_quickshell {
        DesktopGraphExecutor::with_embedded_quickshell_module_session(
            registry,
            options,
            quickshell_binary,
            module_session,
        )
    } else {
        DesktopGraphExecutor::with_module_session(
            registry,
            options,
            quickshell_binary,
            quickshell_config,
            module_session,
        )
    };
    Ok(std::sync::Arc::new(executor))
}

/// Run a graph through the common scheduler and typed transport, selecting a
/// desktop adapter only for declared renderer nodes. Calling the helper keeps
/// renderer policy preflight ahead of all graph effects.
pub fn run(
    plan: GraphPlan,
    registry: crate::manifest::Registry,
    options: crate::runtime::RunOptions,
    graph_options: GraphRunOptions,
) -> Result<GraphRunResult> {
    plan.validate_with_registry(&registry)?;
    let cwd = std::fs::canonicalize(&options.working_directory)
        .context("resolving graph run working directory")?;
    capabilities::preflight_invocations(plan.invocations(), &registry, &cwd)?;
    let module_session = std::sync::Arc::new(
        crate::modules::ProgramModuleSession::preload_invocations(&registry, plan.invocations())?,
    );
    let scoped_calls = module_session
        .scoped_body_invocations()
        .iter()
        .map(|(scope_registry, invocation)| (scope_registry, invocation))
        .collect::<Vec<_>>();
    for (scope_registry, invocation) in scoped_calls {
        capabilities::preflight_invocations(std::iter::once(invocation), scope_registry, &cwd)
            .context("preflighting program module body capabilities")?;
    }
    let executor = graph_executor_with_module_session(&plan, registry, options, module_session)?;
    GraphSession::start(plan, executor, graph_options)?.run_to_completion()
}

fn preflight_renderer_commands(
    binary: &Path,
    config: &Path,
    registry: &crate::manifest::Registry,
) -> Result<()> {
    let cwd = std::env::current_dir().context("resolving renderer launch working directory")?;
    // command_for validates the dedicated capability identity, executable
    // pin, environment grant and required OS sandbox before any graph task is
    // started. These are the only argv shapes this adapter can launch.
    drop(capabilities::command_for(
        binary,
        launch_arguments(config),
        registry,
        POLICY_APPLET,
        &crate::working_dir::WorkingDirectory::plain(cwd.clone()),
    )?);
    drop(capabilities::command_for(
        binary,
        ipc_arguments("executeDesktop-preflight", "{}"),
        registry,
        POLICY_APPLET,
        &crate::working_dir::WorkingDirectory::plain(cwd.clone()),
    )?);
    Ok(())
}

fn policy_command(
    binary: &Path,
    arguments: Vec<OsString>,
    registry: &crate::manifest::Registry,
) -> Result<Command> {
    let cwd = std::env::current_dir().context("resolving renderer launch working directory")?;
    capabilities::command_for(
        binary,
        arguments,
        registry,
        POLICY_APPLET,
        &crate::working_dir::WorkingDirectory::plain(cwd.clone()),
    )
}

fn launch_arguments(config: &Path) -> Vec<OsString> {
    vec![OsString::from("--path"), config.as_os_str().to_owned()]
}

fn ipc_arguments(target: &str, frame: &str) -> Vec<OsString> {
    vec![
        OsString::from("ipc"),
        OsString::from("call"),
        OsString::from(target),
        OsString::from(IPC_FUNCTION),
        OsString::from(frame),
    ]
}

fn resolve_executable(binary: &Path) -> Result<PathBuf> {
    let candidate = if binary.components().count() > 1 || binary.is_absolute() {
        Some(binary.to_owned())
    } else {
        std::env::split_paths(&std::env::var_os("PATH").unwrap_or_default())
            .map(|directory| directory.join(binary))
            .find(|candidate| executable_file(candidate))
    }
    .ok_or_else(|| anyhow!("executable {:?} was not found in PATH", binary))?;
    let canonical = candidate
        .canonicalize()
        .with_context(|| format!("resolving renderer executable {}", candidate.display()))?;
    if !executable_file(&canonical) {
        bail!(
            "renderer executable is not an executable file: {}",
            canonical.display()
        );
    }
    Ok(canonical)
}

#[cfg(unix)]
fn executable_file(path: &Path) -> bool {
    use std::os::unix::fs::PermissionsExt;
    std::fs::metadata(path)
        .is_ok_and(|metadata| metadata.is_file() && metadata.permissions().mode() & 0o111 != 0)
}

#[cfg(not(unix))]
fn executable_file(path: &Path) -> bool {
    path.is_file()
}

fn ensure_current_graph_event(
    event: &crate::graph::EventEnvelope,
    context: &GraphContext,
) -> Result<()> {
    if event.generation != context.generation || event.revision != context.revision {
        bail!("Quickshell renderer received a stale graph property envelope");
    }
    Ok(())
}

fn graph_record_json(payload: PortValue, label: &str) -> Result<Value> {
    let PortValue::Value(value) = payload else {
        bail!("{label} must be a typed record value");
    };
    let json = structured::to_json(&value).with_context(|| format!("encoding {label}"))?;
    if !json.is_object() {
        bail!("{label} must encode to a JSON object");
    }
    Ok(json)
}

fn renderer_session_id(node: &GraphNode, context: &GraphContext) -> String {
    use std::sync::atomic::{AtomicU64, Ordering};
    static NEXT_SESSION: AtomicU64 = AtomicU64::new(1);
    let sequence = NEXT_SESSION.fetch_add(1, Ordering::Relaxed);
    let node_prefix = node.id.chars().take(64).collect::<String>();
    format!(
        "desktop-{}-{sequence}-{}-{}",
        std::process::id(),
        context.generation,
        node_prefix
    )
}

fn response_error(reply: &RendererReply) -> Result<()> {
    if let RendererReply::Error { code, message } = reply {
        bail!("renderer rejected request ({code}): {message}");
    }
    bail!("renderer returned a reply kind that does not match the request")
}

fn validate_properties(
    contract: &WidgetContract,
    properties: &Value,
    complete: bool,
) -> Result<()> {
    let mut schema = contract.properties.schema.clone();
    if !complete {
        schema.required.clear();
    }
    let output = schema_for_record(&schema, None)?;
    output.validate()?;
    output
        .decode(&serde_json::to_vec(properties)?)
        .context("validating renderer property record")?;
    Ok(())
}

fn schema_for_record(schema: &ValueSchema, required: Option<Vec<String>>) -> Result<OutputSchema> {
    if schema.kind != OutputType::Record {
        bail!("renderer property and signal schemas must be records");
    }
    let output = OutputSchema {
        kind: OutputType::Record,
        encoding: OutputEncoding::Json,
        items: None,
        fields: schema.fields.clone(),
        required: required.unwrap_or_else(|| schema.required.clone()),
        enum_values: schema.enum_values.clone(),
        trim_final_lf: false,
    };
    output.validate()?;
    Ok(output)
}

fn merge_object(target: &mut Value, patch: &Value) -> Result<()> {
    let target = target
        .as_object_mut()
        .ok_or_else(|| anyhow!("active widget properties are not an object"))?;
    let patch = patch
        .as_object()
        .ok_or_else(|| anyhow!("renderer property patch must be an object"))?;
    for (key, value) in patch {
        target.insert(key.clone(), value.clone());
    }
    Ok(())
}

fn validate_identifier(value: &str, label: &str) -> Result<()> {
    if value.is_empty()
        || value.len() > 128
        || !value
            .bytes()
            .all(|byte| byte.is_ascii_alphanumeric() || b"._-".contains(&byte))
    {
        bail!("{label} must be 1-128 ASCII letters, digits, dot, underscore, or hyphen");
    }
    Ok(())
}

fn run_bounded(command: &mut Command, max_bytes: usize, timeout: Duration) -> Result<Vec<u8>> {
    let mut child = command
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        .spawn()
        .context("launching Quickshell IPC command")?;
    let mut stdout = child
        .stdout
        .take()
        .ok_or_else(|| anyhow!("IPC stdout pipe was not created"))?;
    let reader = thread::spawn(move || -> io::Result<Vec<u8>> {
        let mut bytes = Vec::with_capacity(max_bytes.min(64 * 1024));
        stdout
            .by_ref()
            .take(max_bytes.saturating_add(1) as u64)
            .read_to_end(&mut bytes)?;
        Ok(bytes)
    });
    let deadline = Instant::now() + timeout;
    let status = loop {
        if let Some(status) = child
            .try_wait()
            .context("checking Quickshell IPC process")?
        {
            break status;
        }
        if Instant::now() >= deadline {
            let _ = child.kill();
            let _ = child.wait();
            let _ = reader.join();
            bail!(
                "Quickshell IPC request timed out after {} ms",
                timeout.as_millis()
            );
        }
        thread::sleep(Duration::from_millis(10));
    };
    let bytes = reader
        .join()
        .map_err(|_| anyhow!("Quickshell IPC output reader panicked"))??;
    if bytes.len() > max_bytes {
        bail!("Quickshell IPC response exceeds {max_bytes} byte frame limit");
    }
    if !status.success() {
        bail!("Quickshell IPC command exited with status {status}");
    }
    Ok(bytes)
}

#[cfg(test)]
#[cfg(feature = "tui")]
mod terminal_tests {
    use super::{acquire_tui_terminal_lease, release_tui_terminal_lease, sanitize_terminal_text};

    #[test]
    fn tui_text_displays_ansi_osc_and_control_bytes_as_visible_glyphs() {
        let input = "\x1b]52;c;payload\x07\x1b[31mred\x1b[0m\rback\x08space\tline\nnext\u{009b}text\u{009d}";
        let sanitized = sanitize_terminal_text(input);
        assert_eq!(
            sanitized,
            "␛]52;c;payload␇␛[31mred␛[0m␍back␈space␉line␊next‹U+009B›text‹U+009D›"
        );
        assert!(!sanitized.chars().any(char::is_control));
    }

    #[test]
    fn tui_terminal_has_one_graph_owner_at_a_time() {
        acquire_tui_terminal_lease().unwrap();
        assert!(acquire_tui_terminal_lease().is_err());
        release_tui_terminal_lease();
        acquire_tui_terminal_lease().unwrap();
        release_tui_terminal_lease();
    }
}

#[cfg(all(test, unix))]
mod private_renderer_asset_tests {
    use super::{PrivateRendererAssets, QUICKSHELL_SOURCE};
    use std::os::unix::fs::{symlink, PermissionsExt};
    use std::process::Command;

    #[test]
    fn embedded_qml_is_readable_by_a_moved_binary_mock_and_cleaned_after_reap() {
        let temporary = tempfile::tempdir().unwrap();
        let owned_root = temporary.path().canonicalize().unwrap();
        let runtime = owned_root.join("runtime");
        std::fs::create_dir(&runtime).unwrap();
        std::fs::set_permissions(&runtime, std::fs::Permissions::from_mode(0o700)).unwrap();
        let registry = crate::manifest::Registry::default();
        let assets =
            PrivateRendererAssets::create(&runtime, "moved-binary-test", &registry).unwrap();
        let config = assets.config.clone();
        let contents = std::fs::read_to_string(&config).unwrap();
        assert!(contents.contains("executeDesktop-moved-binary-test"));
        assert!(!contents.contains("__EXECUTE_RENDERER_IPC_TARGET__"));
        assert!(!contents.contains(env!("CARGO_MANIFEST_DIR")));
        assert_eq!(
            std::fs::metadata(&config).unwrap().permissions().mode() & 0o777,
            0o600
        );
        assert_eq!(
            std::fs::metadata(&assets.directory)
                .unwrap()
                .permissions()
                .mode()
                & 0o777,
            0o700
        );

        // Simulate a packaged binary launched away from the checkout. The
        // offline mock reads exactly the private generated path passed as
        // `qs --path <config>`; no source-tree asset lookup is available.
        let captured = temporary.path().join("captured.qml");
        let mock = temporary.path().join("mock-qs");
        std::fs::write(
            &mock,
            "#!/bin/sh\n[ \"$1\" = \"--path\" ] || exit 2\ncat -- \"$2\" > \"$CAPTURED_QML\"\n",
        )
        .unwrap();
        std::fs::set_permissions(&mock, std::fs::Permissions::from_mode(0o700)).unwrap();
        let status = Command::new(&mock)
            .arg("--path")
            .arg(&config)
            .env_clear()
            .env("CAPTURED_QML", &captured)
            .current_dir(&owned_root)
            .status()
            .unwrap();
        assert!(status.success());
        assert_eq!(std::fs::read_to_string(&captured).unwrap(), contents);
        drop(assets);
        assert!(!config.exists());

        let linked_runtime = temporary.path().join("runtime-link");
        symlink(&runtime, &linked_runtime).unwrap();
        assert!(super::verify_private_runtime_directory(&linked_runtime).is_err());
        assert!(!QUICKSHELL_SOURCE.contains(env!("CARGO_MANIFEST_DIR")));
    }
}
