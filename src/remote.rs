//! Execute's versioned remote-applet wire protocol.
//!
//! `execute.applet/1` is a small HTTPS request/response protocol. It is not
//! MCP. Discovery is an explicit network operation; static contract checks
//! use only the in-memory descriptor cached by an earlier explicit discovery.
//! Invocation is never automatically retried because a transport failure may
//! happen after the remote service has performed its effect.

use crate::ast;
use crate::contracts::{
    AppletContract, Outcome, OutcomeKind, OutputType, PortDelivery, RemoteAuthSource,
    RemoteEndpointConfig, ValueSchema,
};
use crate::ssrf;
pub use crate::contracts::{EffectCertainty, RemoteError, RemoteErrorKind};
mod transport;
use serde::{Deserialize, Serialize};
use serde_json::Value;
use sha2::{Digest, Sha256};
use std::collections::{BTreeMap, BTreeSet};
use std::fmt;
use std::fs::OpenOptions;
use std::io::{self, BufReader, Read};
use std::path::{Path, PathBuf};
use std::process::{Child, Command, Stdio};
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};
use url::{Host, Url};

use ureq::unversioned::resolver::{ResolvedSocketAddrs, Resolver};
use ureq::unversioned::transport::Connector as _;

pub const PROTOCOL_VERSION: u32 = 1;
pub const DISCOVERY_PATH: &str = "/.well-known/execute-applet/v1";
pub const INVOKE_PATH: &str = "/invoke";
pub const DEFAULT_TIMEOUT_MS: u64 = 30_000;
pub const DEFAULT_MAX_REQUEST_BYTES: usize = 1_048_576;
pub const DEFAULT_MAX_RESPONSE_BYTES: usize = 1_048_576;
pub const MAX_TIMEOUT_MS: u64 = 300_000;
pub const MAX_FRAME_BYTES: usize = 16 * 1_048_576;
const MAX_CREDENTIAL_FILE_BYTES: usize = 64 * 1024;
const MAX_TOKEN_BYTES: usize = 8192;
const MAX_TEXT_FIELD_BYTES: usize = 16 * 1024;

/// Construct transport settings with secure HTTPS defaults.
impl RemoteEndpointConfig {
    pub fn new(endpoint: impl Into<String>) -> Self {
        Self {
            endpoint: endpoint.into(),
            auth: None,
            allow_loopback_http_fixture: false,
            allow_discovery: false,
            descriptor_cache: None,
            descriptor_sha256: None,
            timeout_ms: DEFAULT_TIMEOUT_MS,
            max_request_bytes: DEFAULT_MAX_REQUEST_BYTES,
            max_response_bytes: DEFAULT_MAX_RESPONSE_BYTES,
        }
    }
}

struct SecretToken(String);

impl SecretToken {
    fn as_str(&self) -> &str {
        &self.0
    }
}

impl Drop for SecretToken {
    fn drop(&mut self) {
        // Best-effort clearing for this owned copy. The HTTP library may keep
        // its own header copy until the request is dropped.
        // SAFETY: `as_mut_vec` lets the caller break the String's UTF-8 invariant, so the loop
        // must write only bytes that keep it valid: 0 is ASCII, and the length is not changed.
        // `self` is `&mut`, so the buffer has no other reference, and each `byte` comes from
        // iterating the Vec's initialized elements, so each write is in bounds.
        unsafe {
            let bytes = self.0.as_mut_vec();
            for byte in bytes {
                std::ptr::write_volatile(byte, 0);
            }
        }
        std::sync::atomic::compiler_fence(Ordering::SeqCst);
    }
}

struct SecretHeader(String);

impl SecretHeader {
    fn as_str(&self) -> &str {
        &self.0
    }
}

impl Drop for SecretHeader {
    fn drop(&mut self) {
        // SAFETY: `as_mut_vec` lets the caller break the String's UTF-8 invariant, so the loop
        // must write only bytes that keep it valid: 0 is ASCII, and the length is not changed.
        // `self` is `&mut`, so the buffer has no other reference, and each `byte` comes from
        // iterating the Vec's initialized elements, so each write is in bounds.
        unsafe {
            let bytes = self.0.as_mut_vec();
            for byte in bytes {
                std::ptr::write_volatile(byte, 0);
            }
        }
        std::sync::atomic::compiler_fence(Ordering::SeqCst);
    }
}

fn read_auth_token(
    auth: &RemoteAuthSource,
    credential_base: &Path,
) -> Result<SecretToken, RemoteError> {
    let mut bytes = match auth {
        RemoteAuthSource::BearerEnv { name } => std::env::var(name)
            .map_err(|_| {
                RemoteError::new(
                    RemoteErrorKind::CredentialUnavailable,
                    EffectCertainty::NotStarted,
                )
            })?
            .into_bytes(),
        RemoteAuthSource::BearerFile { path } => {
            #[cfg(windows)]
            {
                let resolved = if path.is_absolute() {
                    path.clone()
                } else {
                    credential_base.join(path)
                };
                let mut file = crate::security::open_private_file(&resolved).map_err(|_| {
                    RemoteError::new(
                        RemoteErrorKind::CredentialInvalid,
                        EffectCertainty::NotStarted,
                    )
                })?;
                let mut bytes = Vec::new();
                Read::by_ref(&mut file)
                    .take((MAX_CREDENTIAL_FILE_BYTES + 1) as u64)
                    .read_to_end(&mut bytes)
                    .map_err(|_| {
                        RemoteError::new(
                            RemoteErrorKind::CredentialUnavailable,
                            EffectCertainty::NotStarted,
                        )
                    })?;
                if bytes.len() > MAX_CREDENTIAL_FILE_BYTES {
                    return Err(RemoteError::new(
                        RemoteErrorKind::CredentialInvalid,
                        EffectCertainty::NotStarted,
                    ));
                }
                bytes
            }
            #[cfg(not(windows))]
            {
                let resolved = if path.is_absolute() {
                    path.clone()
                } else {
                    credential_base.join(path)
                };
                let mut options = OpenOptions::new();
                options.read(true);
                #[cfg(unix)]
                {
                    use std::os::unix::fs::OpenOptionsExt;
                    options.custom_flags(libc::O_NOFOLLOW | libc::O_NONBLOCK | libc::O_CLOEXEC);
                }
                #[cfg(windows)]
                {
                    use std::os::windows::fs::OpenOptionsExt;
                    const FILE_FLAG_OPEN_REPARSE_POINT: u32 = 0x0020_0000;
                    options.custom_flags(FILE_FLAG_OPEN_REPARSE_POINT);
                }
                let mut file = options.open(&resolved).map_err(|_| {
                    RemoteError::new(
                        RemoteErrorKind::CredentialUnavailable,
                        EffectCertainty::NotStarted,
                    )
                })?;
                let metadata = file.metadata().map_err(|_| {
                    RemoteError::new(
                        RemoteErrorKind::CredentialUnavailable,
                        EffectCertainty::NotStarted,
                    )
                })?;
                if !metadata.is_file() {
                    return Err(RemoteError::new(
                        RemoteErrorKind::CredentialInvalid,
                        EffectCertainty::NotStarted,
                    ));
                }
                #[cfg(unix)]
                {
                    use std::os::unix::fs::MetadataExt;
                    // SAFETY: geteuid(2) takes no arguments, cannot fail (POSIX), and has no
                    // memory preconditions. Its result is compared with the owner uid in
                    // `metadata`.
                    if metadata.uid() != crate::platform::unix_ffi::effective_uid()
                        || metadata.mode() & 0o077 != 0
                    {
                        return Err(RemoteError::new(
                            RemoteErrorKind::CredentialInvalid,
                            EffectCertainty::NotStarted,
                        ));
                    }
                }
                #[cfg(windows)]
                {
                    use std::os::windows::fs::MetadataExt;
                    if metadata.file_attributes() & 0x400 != 0 {
                        return Err(RemoteError::new(
                            RemoteErrorKind::CredentialInvalid,
                            EffectCertainty::NotStarted,
                        ));
                    }
                }
                let mut bytes = Vec::new();
                Read::by_ref(&mut file)
                    .take((MAX_CREDENTIAL_FILE_BYTES + 1) as u64)
                    .read_to_end(&mut bytes)
                    .map_err(|_| {
                        RemoteError::new(
                            RemoteErrorKind::CredentialUnavailable,
                            EffectCertainty::NotStarted,
                        )
                    })?;
                if bytes.len() > MAX_CREDENTIAL_FILE_BYTES {
                    return Err(RemoteError::new(
                        RemoteErrorKind::CredentialInvalid,
                        EffectCertainty::NotStarted,
                    ));
                }
                bytes
            }
        }
    };
    if bytes.ends_with(b"\r\n") {
        bytes.truncate(bytes.len() - 2);
    } else if bytes.ends_with(b"\n") {
        bytes.truncate(bytes.len() - 1);
    }
    if bytes.is_empty()
        || bytes.len() > MAX_TOKEN_BYTES
        || !bytes.iter().all(|byte| (0x21..=0x7e).contains(byte))
    {
        return Err(RemoteError::new(
            RemoteErrorKind::CredentialInvalid,
            EffectCertainty::NotStarted,
        ));
    }
    String::from_utf8(bytes).map(SecretToken).map_err(|_| {
        RemoteError::new(
            RemoteErrorKind::CredentialInvalid,
            EffectCertainty::NotStarted,
        )
    })
}

/// A backend-neutral identity advertised by a version-1 endpoint.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct RemoteAppletIdentity {
    pub name: String,
    pub version: String,
}

/// A typed discovery document. The `contract` value is the same canonical
/// contract type used for local applets; remote transport adds identity,
/// event adapters and explicit idempotency claims around it.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct RemoteAppletDescriptor {
    pub protocol_version: u32,
    pub identity: RemoteAppletIdentity,
    pub contract: AppletContract,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub events: Vec<RemoteEventPort>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub idempotency: Option<RemoteIdempotencyContract>,
}

impl RemoteAppletDescriptor {
    pub fn validate(&self) -> Result<(), RemoteError> {
        if self.protocol_version != PROTOCOL_VERSION {
            return Err(RemoteError::new(
                RemoteErrorKind::UnsupportedProtocolVersion(self.protocol_version),
                EffectCertainty::NotStarted,
            ));
        }
        if !valid_applet_name(&self.identity.name) || !valid_version_label(&self.identity.version) {
            return Err(RemoteError::new(
                RemoteErrorKind::InvalidDescriptor,
                EffectCertainty::NotStarted,
            ));
        }
        self.contract.validate_interface().map_err(|_| {
            RemoteError::new(
                RemoteErrorKind::InvalidDescriptor,
                EffectCertainty::NotStarted,
            )
        })?;
        let Some(ports) = &self.contract.ports else {
            return Err(RemoteError::new(
                RemoteErrorKind::InvalidDescriptor,
                EffectCertainty::NotStarted,
            ));
        };
        let mut names = BTreeSet::new();
        for event in &self.events {
            event.validate()?;
            if !names.insert(event.port.as_str()) {
                return Err(RemoteError::new(
                    RemoteErrorKind::InvalidDescriptor,
                    EffectCertainty::NotStarted,
                ));
            }
            if !ports
                .events
                .get(&event.port)
                .is_some_and(|port| port.delivery == PortDelivery::Events)
            {
                return Err(RemoteError::new(
                    RemoteErrorKind::InvalidDescriptor,
                    EffectCertainty::NotStarted,
                ));
            }
        }
        if ports
            .events
            .keys()
            .any(|name| !names.contains(name.as_str()))
        {
            return Err(RemoteError::new(
                RemoteErrorKind::InvalidDescriptor,
                EffectCertainty::NotStarted,
            ));
        }
        if ports
            .inputs
            .values()
            .chain(ports.outputs.values())
            .chain(ports.events.values())
            .any(|port| port.schema.validate().is_err())
        {
            return Err(RemoteError::new(
                RemoteErrorKind::InvalidDescriptor,
                EffectCertainty::NotStarted,
            ));
        }
        if ports
            .inputs
            .keys()
            .any(|name| !valid_argument_name(name) && name != "stdin")
            || ports.outputs.keys().any(|name| !valid_port_name(name))
            || ports.events.keys().any(|name| !valid_port_name(name))
        {
            return Err(RemoteError::new(
                RemoteErrorKind::InvalidDescriptor,
                EffectCertainty::NotStarted,
            ));
        }
        if let Some(idempotency) = &self.idempotency {
            idempotency.validate()?;
        }
        Ok(())
    }
}

/// The event framing and cursor behavior for one named event output.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct RemoteEventPort {
    pub port: String,
    pub format: RemoteEventFormat,
    #[serde(default = "default_event_bytes")]
    pub max_event_bytes: usize,
    #[serde(default)]
    pub resumable: bool,
    #[serde(default)]
    pub reconnect: bool,
    /// Maximum consecutive reconnect attempts without a newly advanced cursor.
    #[serde(default)]
    pub max_reconnects: u8,
    /// This protocol adapter is pull-based. A consumer requests the next
    /// record before another record is read from the transport.
    #[serde(default = "pull_backpressure")]
    pub backpressure: RemoteBackpressure,
}

impl RemoteEventPort {
    fn validate(&self) -> Result<(), RemoteError> {
        if !valid_port_name(&self.port)
            || !(1..=MAX_FRAME_BYTES).contains(&self.max_event_bytes)
            || self.backpressure != RemoteBackpressure::Pull
            || self.max_reconnects > 10
            || (self.reconnect && (!self.resumable || self.max_reconnects == 0))
            || (!self.reconnect && self.max_reconnects != 0)
        {
            return Err(RemoteError::new(
                RemoteErrorKind::InvalidDescriptor,
                EffectCertainty::NotStarted,
            ));
        }
        Ok(())
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum RemoteEventFormat {
    Sse,
    JsonLines,
}

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum RemoteBackpressure {
    #[default]
    Pull,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct RemoteIdempotencyContract {
    /// A server-defined, stable scope such as `account:42/operation:sync`.
    pub scope: String,
    /// Only these operations promise deduplication for a supplied key.
    pub operations: BTreeSet<String>,
    /// The server's minimum deduplication retention, in seconds.
    pub minimum_retention_seconds: u32,
}

impl RemoteIdempotencyContract {
    fn validate(&self) -> Result<(), RemoteError> {
        if !valid_scope(&self.scope)
            || self.operations.is_empty()
            || self
                .operations
                .iter()
                .any(|operation| !valid_operation(operation))
            || self.minimum_retention_seconds == 0
        {
            return Err(RemoteError::new(
                RemoteErrorKind::InvalidDescriptor,
                EffectCertainty::NotStarted,
            ));
        }
        Ok(())
    }

    fn permits(&self, operation: &str) -> bool {
        self.operations.contains(operation)
    }
}

/// Protocol payloads preserve JSON values as JSON and bytes using canonical
/// base64. Decoded byte values remain bounded by the configured frame limit.
#[derive(Clone, PartialEq, Serialize, Deserialize)]
#[serde(
    tag = "encoding",
    content = "data",
    rename_all = "snake_case",
    deny_unknown_fields
)]
pub enum RemoteValue {
    Json(Value),
    Base64(String),
}

impl fmt::Debug for RemoteValue {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Json(value) => formatter
                .debug_tuple("Json")
                .field(&format_args!(
                    "<redacted:{}-bytes>",
                    serde_json::to_vec(value).map_or(0, |bytes| bytes.len())
                ))
                .finish(),
            Self::Base64(value) => formatter
                .debug_tuple("Base64")
                .field(&format_args!("<redacted:{}-encoded-bytes>", value.len()))
                .finish(),
        }
    }
}

impl RemoteValue {
    pub fn decode_bytes(&self, limit: usize) -> Result<Option<Vec<u8>>, RemoteError> {
        match self {
            Self::Json(_) => Ok(None),
            Self::Base64(encoded) => decode_base64(encoded, limit)
                .map(Some)
                .map_err(|kind| RemoteError::new(kind, EffectCertainty::KnownResponse)),
        }
    }

    pub fn bytes(bytes: &[u8]) -> Self {
        Self::Base64(encode_base64(bytes))
    }
}

#[derive(Clone, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct RemoteRequest {
    pub protocol_version: u32,
    pub request_id: String,
    pub operation: String,
    #[serde(default)]
    pub arguments: BTreeMap<String, Value>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub input: Option<RemoteValue>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub idempotency_key: Option<String>,
}

impl fmt::Debug for RemoteRequest {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("RemoteRequest")
            .field("protocol_version", &self.protocol_version)
            .field("operation", &self.operation)
            .field("argument_names", &self.arguments.keys().collect::<Vec<_>>())
            .field("input", &self.input)
            .field(
                "idempotency_key",
                &self.idempotency_key.as_ref().map(|_| "<redacted>"),
            )
            .finish()
    }
}

impl RemoteRequest {
    pub fn new(operation: impl Into<String>) -> Self {
        Self {
            protocol_version: PROTOCOL_VERSION,
            request_id: next_request_id(),
            operation: operation.into(),
            arguments: BTreeMap::new(),
            input: None,
            idempotency_key: None,
        }
    }

    /// Encode stdin according to its canonical port framing. Raw bytes remain
    /// base64 on the wire; typed JSON is parsed and validated before encoding.
    pub fn set_input_bytes(
        &mut self,
        contract: &AppletContract,
        bytes: &[u8],
    ) -> Result<(), RemoteError> {
        let port = contract
            .ports
            .as_ref()
            .and_then(|ports| ports.inputs.get("stdin"))
            .ok_or_else(|| {
                RemoteError::new(RemoteErrorKind::InvalidRequest, EffectCertainty::NotStarted)
            })?;
        let value = match port.encoding {
            Some(crate::contracts::OutputEncoding::Raw) => RemoteValue::bytes(bytes),
            Some(crate::contracts::OutputEncoding::Json) => {
                let value: Value = parse_strict_json(bytes).map_err(|_| {
                    RemoteError::new(RemoteErrorKind::InvalidRequest, EffectCertainty::NotStarted)
                })?;
                RemoteValue::Json(value)
            }
            None => {
                return Err(RemoteError::new(
                    RemoteErrorKind::InvalidDescriptor,
                    EffectCertainty::NotStarted,
                ));
            }
        };
        if !remote_value_matches_input(&value, port) {
            return Err(RemoteError::new(
                RemoteErrorKind::InvalidRequest,
                EffectCertainty::NotStarted,
            ));
        }
        self.input = Some(value);
        Ok(())
    }

    pub fn validate(&self, max_bytes: usize) -> Result<Vec<u8>, RemoteError> {
        if self.protocol_version != PROTOCOL_VERSION
            || !valid_request_id(&self.request_id)
            || !valid_operation(&self.operation)
            || self
                .arguments
                .keys()
                .any(|key| key == "stdin" || !valid_argument_name(key))
            || self
                .idempotency_key
                .as_ref()
                .is_some_and(|key| !valid_idempotency_key(key))
        {
            return Err(RemoteError::new(
                RemoteErrorKind::InvalidRequest,
                EffectCertainty::NotStarted,
            ));
        }
        let encoded = serde_json::to_vec(self).map_err(|_| {
            RemoteError::new(RemoteErrorKind::InvalidRequest, EffectCertainty::NotStarted)
        })?;
        if encoded.len() > max_bytes {
            return Err(RemoteError::new(
                RemoteErrorKind::RequestTooLarge,
                EffectCertainty::NotStarted,
            ));
        }
        Ok(encoded)
    }

    fn validate_against_contract(&self, contract: &AppletContract) -> Result<(), RemoteError> {
        let ports = contract.ports.as_ref().ok_or_else(|| {
            RemoteError::new(
                RemoteErrorKind::InvalidDescriptor,
                EffectCertainty::NotStarted,
            )
        })?;
        for (name, value) in &self.arguments {
            let schema = &ports
                .inputs
                .get(name)
                .ok_or_else(|| {
                    RemoteError::new(RemoteErrorKind::InvalidRequest, EffectCertainty::NotStarted)
                })?
                .schema;
            if !json_matches_schema(schema, value) {
                return Err(RemoteError::new(
                    RemoteErrorKind::InvalidRequest,
                    EffectCertainty::NotStarted,
                ));
            }
        }
        if let Some(input) = &self.input {
            let port = ports.inputs.get("stdin").ok_or_else(|| {
                RemoteError::new(RemoteErrorKind::InvalidRequest, EffectCertainty::NotStarted)
            })?;
            if !remote_value_matches_input(input, port) {
                return Err(RemoteError::new(
                    RemoteErrorKind::InvalidRequest,
                    EffectCertainty::NotStarted,
                ));
            }
        }
        Ok(())
    }
}

#[derive(Clone, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct RemoteApplicationError {
    pub code: String,
    pub message: String,
    #[serde(default)]
    pub retryable: bool,
}

impl fmt::Debug for RemoteApplicationError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("RemoteApplicationError")
            .field("code", &self.code)
            .field("message", &"<redacted>")
            .field("retryable", &self.retryable)
            .finish()
    }
}

#[derive(Clone, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct RemoteResponse {
    pub protocol_version: u32,
    pub request_id: String,
    /// The remote applet's declared status code. The contract maps it to a
    /// named `OutcomeKind`; transport failures are represented separately.
    pub status: i32,
    #[serde(default)]
    pub outputs: BTreeMap<String, RemoteValue>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub error: Option<RemoteApplicationError>,
}

impl fmt::Debug for RemoteResponse {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("RemoteResponse")
            .field("protocol_version", &self.protocol_version)
            .field("status", &self.status)
            .field("output_names", &self.outputs.keys().collect::<Vec<_>>())
            .field("outputs", &self.outputs)
            .field("error_code", &self.error.as_ref().map(|error| &error.code))
            .finish()
    }
}

impl RemoteResponse {
    fn validate(
        &self,
        descriptor: &RemoteAppletDescriptor,
        request: &RemoteRequest,
        max_output_bytes: usize,
    ) -> Result<(), RemoteError> {
        if self.protocol_version != PROTOCOL_VERSION {
            return Err(RemoteError::new(
                RemoteErrorKind::UnsupportedProtocolVersion(self.protocol_version),
                EffectCertainty::Uncertain,
            ));
        }
        if self.request_id != request.request_id {
            return Err(RemoteError::new(
                RemoteErrorKind::RequestIdMismatch,
                EffectCertainty::Uncertain,
            ));
        }
        if self.outputs.keys().any(|name| !valid_port_name(name)) {
            return Err(RemoteError::new(
                RemoteErrorKind::InvalidResponse,
                EffectCertainty::Uncertain,
            ));
        }
        if let Some(error) = &self.error {
            if !valid_error_code(&error.code)
                || error.message.len() > MAX_TEXT_FIELD_BYTES
                || error.message.chars().any(|ch| ch == '\0')
            {
                return Err(RemoteError::new(
                    RemoteErrorKind::InvalidResponse,
                    EffectCertainty::Uncertain,
                ));
            }
        }
        let ports = descriptor.contract.ports.as_ref().ok_or_else(|| {
            RemoteError::new(
                RemoteErrorKind::InvalidDescriptor,
                EffectCertainty::Uncertain,
            )
        })?;
        for (name, value) in &self.outputs {
            let schema = &ports
                .outputs
                .get(name)
                .ok_or_else(|| {
                    RemoteError::new(RemoteErrorKind::InvalidResponse, EffectCertainty::Uncertain)
                })?
                .schema;
            if !remote_value_matches_schema(value, schema)
                || matches!(value, RemoteValue::Base64(encoded) if decode_base64(encoded, max_output_bytes).is_err())
            {
                return Err(RemoteError::new(
                    RemoteErrorKind::InvalidResponse,
                    EffectCertainty::Uncertain,
                ));
            }
        }
        let outcome = descriptor.contract.outcome_for(self.status);
        if (outcome.kind == OutcomeKind::Success) != self.error.is_none() {
            return Err(RemoteError::new(
                RemoteErrorKind::InvalidResponse,
                EffectCertainty::Uncertain,
            ));
        }
        Ok(())
    }

    pub fn outcome(&self, contract: &AppletContract) -> Outcome {
        contract.outcome_for(self.status)
    }
}

/// Authenticated, bounded transport client. Constructing it and checking its
/// in-memory descriptor cache do not perform network I/O.
#[derive(Clone)]
pub struct RemoteClient {
    config: Arc<RemoteEndpointConfig>,
    base_url: Url,
    credential_base: Arc<PathBuf>,
    cached_descriptor: Arc<Mutex<Option<RemoteAppletDescriptor>>>,
    secret_redactor: Option<Arc<crate::security::SecretRedactor>>,
    resolver_helper: Option<Arc<PathBuf>>,
    process_observer: Option<Arc<dyn Fn(usize) + Send + Sync>>,
    /// True only when the capability policy explicitly granted this endpoint's
    /// origin. Preserves intentional private-destination grants; the default
    /// denies non-public destinations even when the hostname looks benign.
    ssrf_granted_origin: bool,
}

impl fmt::Debug for RemoteClient {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("RemoteClient")
            .field("endpoint", &"<redacted>")
            .field("config", &self.config)
            .finish_non_exhaustive()
    }
}

impl RemoteClient {
    pub fn new(config: RemoteEndpointConfig) -> Result<Self, RemoteError> {
        let credential_base = std::env::current_dir().map_err(|_| {
            RemoteError::new(
                RemoteErrorKind::InvalidConfiguration,
                EffectCertainty::NotStarted,
            )
        })?;
        Self::new_with_credential_base(config, credential_base)
    }

    /// Resolve relative bearer-file paths against a stable, caller-selected
    /// directory (normally the manifest directory or invocation cwd).
    pub fn new_with_credential_base(
        config: RemoteEndpointConfig,
        credential_base: impl Into<PathBuf>,
    ) -> Result<Self, RemoteError> {
        config.validate_declaration().map_err(|_| {
            RemoteError::new(
                RemoteErrorKind::InvalidConfiguration,
                EffectCertainty::NotStarted,
            )
        })?;
        validate_endpoint(&config.endpoint, config.allow_loopback_http_fixture)?;
        let base_url = Url::parse(&config.endpoint).map_err(|_| {
            RemoteError::new(
                RemoteErrorKind::InvalidConfiguration,
                EffectCertainty::NotStarted,
            )
        })?;
        let credential_base = credential_base.into();
        let credential_base = if credential_base.is_absolute() {
            credential_base
        } else {
            std::env::current_dir()
                .map_err(|_| {
                    RemoteError::new(
                        RemoteErrorKind::InvalidConfiguration,
                        EffectCertainty::NotStarted,
                    )
                })?
                .join(credential_base)
        };
        Ok(Self {
            config: Arc::new(config),
            base_url,
            credential_base: Arc::new(credential_base),
            cached_descriptor: Arc::new(Mutex::new(None)),
            secret_redactor: None,
            resolver_helper: None,
            process_observer: None,
            ssrf_granted_origin: false,
        })
    }

    /// Attach the registry-wide secret redactor used for host diagnostics.
    /// Direct transport users may omit it; runtime dispatch supplies it.
    pub fn with_secret_redactor(mut self, redactor: Arc<crate::security::SecretRedactor>) -> Self {
        self.secret_redactor = Some(redactor);
        self
    }

    /// Enable cancellation-supervised DNS via the execute CLI's hidden helper.
    /// Library embeddings that omit this can still use literal-IP endpoints.
    pub fn with_resolver_helper(mut self, executable: impl Into<PathBuf>) -> Self {
        self.resolver_helper = Some(Arc::new(executable.into()));
        self
    }

    /// Observe successful resolver-helper process spawns. The callback runs
    /// immediately after spawn, before resolution can later fail or cancel.
    pub fn with_process_observer(mut self, observer: Arc<dyn Fn(usize) + Send + Sync>) -> Self {
        self.process_observer = Some(observer);
        self
    }

    /// Record that the capability policy explicitly granted this endpoint's
    /// origin. Only an explicit grant preserves non-public destinations; the
    /// default denies them at both the by-name check and the resolver.
    pub fn with_ssrf_grant(mut self, granted: bool) -> Self {
        self.ssrf_granted_origin = granted;
        self
    }

    /// Refuse a non-public endpoint host by name, before any resolution or
    /// socket use. Hostnames that merely look benign pass here and are
    /// pinned again at the resolver, which stops DNS rebinding.
    fn check_ssrf(&self) -> Result<(), RemoteError> {
        let host = match self.base_url.host() {
            Some(Host::Ipv4(address)) => address.to_string(),
            Some(Host::Ipv6(address)) => address.to_string(),
            Some(Host::Domain(name)) => name.to_string(),
            None => {
                return Err(RemoteError::new(
                    RemoteErrorKind::InvalidConfiguration,
                    EffectCertainty::NotStarted,
                ));
            }
        };
        let origin =
            crate::capabilities::canonical_http_origin(&self.config.endpoint).map_err(|_| {
                RemoteError::new(
                    RemoteErrorKind::InvalidConfiguration,
                    EffectCertainty::NotStarted,
                )
            })?;
        let granted = if self.ssrf_granted_origin {
            BTreeSet::from([origin.clone()])
        } else {
            BTreeSet::new()
        };
        ssrf::check_endpoint_host(
            &host,
            &origin,
            self.config.allow_loopback_http_fixture,
            &granted,
        )
        .map_err(|_| {
            RemoteError::new(
                RemoteErrorKind::InvalidConfiguration,
                EffectCertainty::NotStarted,
            )
        })
    }

    /// Fetch and validate one typed discovery descriptor. This is explicit;
    /// neither `new` nor a compatibility check performs network effects.
    pub fn discover(&self) -> Result<RemoteAppletDescriptor, RemoteError> {
        self.discover_cancellable(&Arc::new(AtomicBool::new(false)))
    }

    pub fn discover_cancellable(
        &self,
        cancelled: &Arc<AtomicBool>,
    ) -> Result<RemoteAppletDescriptor, RemoteError> {
        check_cancel(cancelled, EffectCertainty::NotStarted)?;
        self.check_ssrf()?;
        let url = self.route(DISCOVERY_PATH)?;
        let response = self.send_get(
            &url,
            cancelled,
            None,
            self.config.timeout_ms,
            "application/vnd.execute.applet+json;version=1",
        )?;
        let mut body = response.into_body();
        let bytes = read_bounded(
            body.as_reader(),
            self.config.max_response_bytes,
            cancelled,
            EffectCertainty::NotStarted,
        )?;
        let descriptor: RemoteAppletDescriptor = parse_strict_json(&bytes).map_err(|_| {
            RemoteError::new(
                RemoteErrorKind::InvalidDescriptor,
                EffectCertainty::NotStarted,
            )
        })?;
        descriptor.validate()?;
        *self.cached_descriptor.lock().map_err(|_| {
            RemoteError::new(
                RemoteErrorKind::InvalidDescriptor,
                EffectCertainty::NotStarted,
            )
        })? = Some(descriptor.clone());
        Ok(descriptor)
    }

    /// Check a manifest's static contract against the last explicitly fetched
    /// discovery descriptor. This method is guaranteed to be network-free.
    pub fn check_compatible(
        &self,
        expected_name: &str,
        expected: &AppletContract,
    ) -> Result<RemoteAppletDescriptor, RemoteError> {
        let descriptor = self
            .cached_descriptor
            .lock()
            .map_err(|_| {
                RemoteError::new(
                    RemoteErrorKind::DiscoveryNotCached,
                    EffectCertainty::NotStarted,
                )
            })?
            .clone()
            .ok_or_else(|| {
                RemoteError::new(
                    RemoteErrorKind::DiscoveryNotCached,
                    EffectCertainty::NotStarted,
                )
            })?;
        if descriptor.identity.name != expected_name
            || !contracts_have_same_interface(&descriptor.contract, expected)
        {
            return Err(RemoteError::new(
                RemoteErrorKind::IncompatibleContract,
                EffectCertainty::NotStarted,
            ));
        }
        Ok(descriptor)
    }

    pub fn cached_descriptor(&self) -> Option<RemoteAppletDescriptor> {
        self.cached_descriptor
            .lock()
            .ok()
            .and_then(|cache| cache.clone())
    }

    /// Absolute cache path to authorize before calling
    /// `load_configured_descriptor_cache`.
    pub fn descriptor_cache_path(&self) -> Option<PathBuf> {
        self.config.descriptor_cache.as_ref().map(|path| {
            if path.is_absolute() {
                path.clone()
            } else {
                self.credential_base.join(path)
            }
        })
    }

    /// Absolute bearer-file path to authorize before discovery, invocation,
    /// or stream setup. This does not read the file.
    pub fn bearer_file_path(&self) -> Option<PathBuf> {
        match self.config.auth.as_ref() {
            Some(RemoteAuthSource::BearerFile { path }) => Some(if path.is_absolute() {
                path.clone()
            } else {
                self.credential_base.join(path)
            }),
            _ => None,
        }
    }

    /// Load the explicitly configured, SHA-256-pinned descriptor cache without
    /// network I/O. The caller must authorize this host-file read under its
    /// filesystem policy before calling this method.
    pub fn load_configured_descriptor_cache(
        &self,
    ) -> Result<Option<RemoteAppletDescriptor>, RemoteError> {
        let (Some(path), Some(expected_digest)) = (
            self.config.descriptor_cache.as_ref(),
            self.config.descriptor_sha256.as_ref(),
        ) else {
            return Ok(None);
        };
        let path = if path.is_absolute() {
            path.clone()
        } else {
            self.credential_base.join(path)
        };
        let bytes = read_regular_file(&path, self.config.max_response_bytes)?;
        let actual_digest = format!("{:x}", Sha256::digest(&bytes));
        if &actual_digest != expected_digest {
            return Err(RemoteError::new(
                RemoteErrorKind::DescriptorCacheHashMismatch,
                EffectCertainty::NotStarted,
            ));
        }
        let descriptor: RemoteAppletDescriptor = parse_strict_json(&bytes).map_err(|_| {
            RemoteError::new(
                RemoteErrorKind::InvalidDescriptor,
                EffectCertainty::NotStarted,
            )
        })?;
        descriptor.validate()?;
        self.use_cached_descriptor(descriptor.clone())?;
        Ok(Some(descriptor))
    }

    /// Install a previously persisted discovery descriptor. This is
    /// filesystem-free: callers own any cache storage and permissions.
    pub fn use_cached_descriptor(
        &self,
        descriptor: RemoteAppletDescriptor,
    ) -> Result<(), RemoteError> {
        descriptor.validate()?;
        *self.cached_descriptor.lock().map_err(|_| {
            RemoteError::new(
                RemoteErrorKind::InvalidDescriptor,
                EffectCertainty::NotStarted,
            )
        })? = Some(descriptor);
        Ok(())
    }

    /// Open one explicitly declared event port. The stream is demand-driven,
    /// uses at-least-once cursor semantics, and never buffers an unbounded
    /// queue. Passing `resume_after` sends `Last-Event-ID` only when the port
    /// declares resumability. Reconnection is automatic only when that port
    /// explicitly enables it and declares a finite consecutive retry limit.
    /// A valid event with a new cursor resets that budget; duplicates and
    /// heartbeats do not count as recovery.
    pub fn open_events(
        &self,
        descriptor: &RemoteAppletDescriptor,
        port_name: &str,
        resume_after: Option<&str>,
        cancelled: Arc<AtomicBool>,
    ) -> Result<RemoteEventStream, RemoteError> {
        descriptor.validate()?;
        self.check_ssrf()?;
        let port = descriptor
            .events
            .iter()
            .find(|event| event.port == port_name)
            .cloned()
            .ok_or_else(|| {
                RemoteError::new(RemoteErrorKind::InvalidStream, EffectCertainty::NotStarted)
            })?;
        if resume_after.is_some() && !port.resumable {
            return Err(RemoteError::new(
                RemoteErrorKind::InvalidRequest,
                EffectCertainty::NotStarted,
            ));
        }
        if resume_after.is_some_and(|cursor| !valid_cursor(cursor)) {
            return Err(RemoteError::new(
                RemoteErrorKind::InvalidRequest,
                EffectCertainty::NotStarted,
            ));
        }
        let schema = descriptor
            .contract
            .ports
            .as_ref()
            .and_then(|ports| ports.events.get(port_name))
            .map(|port| port.schema.clone())
            .ok_or_else(|| {
                RemoteError::new(
                    RemoteErrorKind::InvalidDescriptor,
                    EffectCertainty::NotStarted,
                )
            })?;
        let reader =
            self.open_event_reader(&port, resume_after, &cancelled, self.config.timeout_ms)?;
        let reconnects_left = if port.reconnect {
            port.max_reconnects
        } else {
            0
        };
        Ok(RemoteEventStream {
            client: self.clone(),
            port,
            schema,
            reader,
            cancelled,
            cursor: resume_after.map(str::to_owned),
            reconnects_left,
        })
    }

    fn open_event_reader(
        &self,
        port: &RemoteEventPort,
        cursor: Option<&str>,
        cancelled: &Arc<AtomicBool>,
        timeout_ms: u64,
    ) -> Result<BufReader<ureq::BodyReader<'static>>, RemoteError> {
        let mut url = self.base_url.clone();
        url.set_path(&format!("/events/{}", port.port));
        url.set_query(None);
        url.set_fragment(None);
        let accept = match port.format {
            RemoteEventFormat::Sse => "text/event-stream",
            RemoteEventFormat::JsonLines => "application/x-ndjson",
        };
        let idle_enabled = Arc::new(AtomicBool::new(false));
        let response = self.send_get_stream(
            &url,
            cancelled,
            cursor,
            timeout_ms,
            accept,
            Arc::clone(&idle_enabled),
        )?;
        idle_enabled.store(true, Ordering::Relaxed);
        validate_content_type(
            response.headers().get("content-type"),
            accept,
            false,
            EffectCertainty::NotStarted,
        )?;
        let capacity = port.max_event_bytes.clamp(1, 8192);
        Ok(BufReader::with_capacity(
            capacity,
            response.into_body().into_reader(),
        ))
    }

    /// Invoke exactly once. Any I/O failure after entering the HTTP request
    /// path is marked uncertain; this client never retries a POST.
    pub fn invoke(
        &self,
        descriptor: &RemoteAppletDescriptor,
        request: &RemoteRequest,
    ) -> Result<RemoteResponse, RemoteError> {
        self.invoke_cancellable(descriptor, request, &Arc::new(AtomicBool::new(false)))
    }

    pub fn invoke_cancellable(
        &self,
        descriptor: &RemoteAppletDescriptor,
        request: &RemoteRequest,
        cancelled: &Arc<AtomicBool>,
    ) -> Result<RemoteResponse, RemoteError> {
        descriptor.validate()?;
        check_cancel(cancelled, EffectCertainty::NotStarted)?;
        self.check_ssrf()?;
        if self.cached_descriptor().as_ref() != Some(descriptor) {
            return Err(RemoteError::new(
                RemoteErrorKind::IncompatibleContract,
                EffectCertainty::NotStarted,
            ));
        }
        let request_bytes = request.validate(self.config.max_request_bytes)?;
        request.validate_against_contract(&descriptor.contract)?;
        if request.idempotency_key.is_some()
            && !descriptor
                .idempotency
                .as_ref()
                .is_some_and(|contract| contract.permits(&request.operation))
        {
            return Err(RemoteError::new(
                RemoteErrorKind::IdempotencyNotDeclared,
                EffectCertainty::NotStarted,
            ));
        }
        let url = self.route(INVOKE_PATH)?;
        let token = self.read_token()?;
        check_cancel(cancelled, EffectCertainty::NotStarted)?;
        let mut remote_request = self
            .agent(self.config.timeout_ms, Arc::clone(cancelled))
            .post(url.as_str())
            .header("Accept", "application/vnd.execute.applet+json;version=1")
            .header(
                "Content-Type",
                "application/vnd.execute.applet+json;version=1",
            );
        let authorization = token.map(|token| SecretHeader(format!("Bearer {}", token.as_str())));
        if let Some(authorization) = &authorization {
            remote_request = remote_request.header("Authorization", authorization.as_str());
        }
        let response = remote_request
            .send(&request_bytes)
            .map_err(|error| map_ureq_error(error, EffectCertainty::Uncertain, cancelled))?;
        if !response.status().is_success() {
            return Err(RemoteError::new(
                RemoteErrorKind::HttpStatus(response.status().as_u16()),
                EffectCertainty::Uncertain,
            ));
        }
        validate_content_type(
            response.headers().get("content-type"),
            "application/vnd.execute.applet+json",
            true,
            EffectCertainty::Uncertain,
        )?;
        let mut body = response.into_body();
        let bytes = read_bounded(
            body.as_reader(),
            self.config.max_response_bytes,
            cancelled,
            EffectCertainty::Uncertain,
        )?;
        let response: RemoteResponse = parse_strict_json(&bytes).map_err(|_| {
            RemoteError::new(RemoteErrorKind::InvalidResponse, EffectCertainty::Uncertain)
        })?;
        response.validate(descriptor, request, self.config.max_response_bytes)?;
        Ok(response)
    }

    fn read_token(&self) -> Result<Option<SecretToken>, RemoteError> {
        let token = self
            .config
            .auth
            .as_ref()
            .map(|auth| read_auth_token(auth, &self.credential_base))
            .transpose()?;
        if let (Some(token), Some(redactor)) = (&token, &self.secret_redactor) {
            redactor.register(token.as_str().as_bytes()).map_err(|_| {
                RemoteError::new(
                    RemoteErrorKind::CredentialInvalid,
                    EffectCertainty::NotStarted,
                )
            })?;
        }
        Ok(token)
    }

    fn route(&self, path: &str) -> Result<Url, RemoteError> {
        let mut url = self.base_url.clone();
        url.set_path(path);
        url.set_query(None);
        url.set_fragment(None);
        Ok(url)
    }

    fn agent(&self, timeout_ms: u64, cancelled: Arc<AtomicBool>) -> ureq::Agent {
        let config = ureq::Agent::config_builder()
            .timeout_global(Some(Duration::from_millis(timeout_ms)))
            .http_status_as_error(false)
            .max_redirects(0)
            .proxy(None);
        let config = config.build();
        let connector = ().chain(transport::InterruptibleTcpConnector::new(Arc::clone(
            &cancelled,
        )));
        #[cfg(windows)]
        let connector =
            connector.chain(ureq::unversioned::transport::NativeTlsConnector::default());
        #[cfg(unix)]
        let connector = connector.chain(ureq::unversioned::transport::RustlsConnector::default());
        ureq::Agent::with_parts(
            config,
            connector,
            InterruptibleSystemResolver {
                helper: self.resolver_helper.as_deref().cloned(),
                cancelled,
                process_observer: self.process_observer.clone(),
                allow_loopback: self.config.allow_loopback_http_fixture,
                granted_origin: self.ssrf_granted_origin,
            },
        )
    }

    fn send_get_stream(
        &self,
        url: &Url,
        cancelled: &Arc<AtomicBool>,
        cursor: Option<&str>,
        timeout_ms: u64,
        accept: &str,
        idle_enabled: Arc<AtomicBool>,
    ) -> Result<ureq::http::Response<ureq::Body>, RemoteError> {
        let timeout = Some(Duration::from_millis(timeout_ms));
        let config = ureq::Agent::config_builder()
            .timeout_resolve(timeout)
            .timeout_connect(timeout)
            .timeout_send_request(timeout)
            .timeout_recv_response(timeout)
            .http_status_as_error(false)
            .max_redirects(0)
            .proxy(None)
            .build();
        let connector = ().chain(transport::InterruptibleTcpConnector::with_stream_idle(
            Arc::clone(cancelled),
            idle_enabled,
            Duration::from_millis(timeout_ms),
        ));
        #[cfg(windows)]
        let connector =
            connector.chain(ureq::unversioned::transport::NativeTlsConnector::default());
        #[cfg(unix)]
        let connector = connector.chain(ureq::unversioned::transport::RustlsConnector::default());
        let agent = ureq::Agent::with_parts(
            config,
            connector,
            InterruptibleSystemResolver {
                helper: self.resolver_helper.as_deref().cloned(),
                cancelled: Arc::clone(cancelled),
                process_observer: self.process_observer.clone(),
                allow_loopback: self.config.allow_loopback_http_fixture,
                granted_origin: self.ssrf_granted_origin,
            },
        );
        let mut request = agent.get(url.as_str()).header("Accept", accept);
        let token = self.read_token()?;
        let authorization = token.map(|token| SecretHeader(format!("Bearer {}", token.as_str())));
        if let Some(authorization) = &authorization {
            request = request.header("Authorization", authorization.as_str());
        }
        if let Some(cursor) = cursor {
            if !valid_cursor(cursor) {
                return Err(RemoteError::new(
                    RemoteErrorKind::InvalidRequest,
                    EffectCertainty::NotStarted,
                ));
            }
            request = request.header("Last-Event-ID", cursor);
        }
        check_cancel(cancelled, EffectCertainty::NotStarted)?;
        let response = request
            .call()
            .map_err(|error| map_ureq_error(error, EffectCertainty::NotStarted, cancelled))?;
        if !response.status().is_success() {
            return Err(RemoteError::new(
                RemoteErrorKind::HttpStatus(response.status().as_u16()),
                EffectCertainty::NotStarted,
            ));
        }
        Ok(response)
    }

    fn send_get(
        &self,
        url: &Url,
        cancelled: &Arc<AtomicBool>,
        last_event_id: Option<&str>,
        timeout_ms: u64,
        accept: &str,
    ) -> Result<ureq::http::Response<ureq::Body>, RemoteError> {
        let mut request = self
            .agent(timeout_ms, Arc::clone(cancelled))
            .get(url.as_str())
            .header("Accept", accept);
        let token = self.read_token()?;
        let authorization = token.map(|token| SecretHeader(format!("Bearer {}", token.as_str())));
        if let Some(authorization) = &authorization {
            request = request.header("Authorization", authorization.as_str());
        }
        if let Some(cursor) = last_event_id {
            if !valid_cursor(cursor) {
                return Err(RemoteError::new(
                    RemoteErrorKind::InvalidRequest,
                    EffectCertainty::NotStarted,
                ));
            }
            request = request.header("Last-Event-ID", cursor);
        }
        check_cancel(cancelled, EffectCertainty::NotStarted)?;
        let response = request
            .call()
            .map_err(|error| map_ureq_error(error, EffectCertainty::NotStarted, cancelled))?;
        if !response.status().is_success() {
            return Err(RemoteError::new(
                RemoteErrorKind::HttpStatus(response.status().as_u16()),
                EffectCertainty::NotStarted,
            ));
        }
        if accept == "application/vnd.execute.applet+json;version=1" {
            validate_content_type(
                response.headers().get("content-type"),
                "application/vnd.execute.applet+json",
                true,
                EffectCertainty::NotStarted,
            )?;
        }
        Ok(response)
    }
}

/// One decoded event from a remote event port. Delivery is at-least-once:
/// reconnects resume using the last cursor, and an equal cursor is surfaced
/// with `duplicate_cursor = true` so callers can apply their own idempotent
/// handling without silently dropping events.
#[derive(Clone, PartialEq)]
pub struct RemoteStreamEvent {
    pub event: String,
    pub cursor: Option<String>,
    pub data: Value,
    pub duplicate_cursor: bool,
}

impl fmt::Debug for RemoteStreamEvent {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("RemoteStreamEvent")
            .field("event", &self.event)
            .field("cursor", &self.cursor.as_ref().map(|_| "<redacted>"))
            .field(
                "data_bytes",
                &serde_json::to_vec(&self.data).map_or(0, |bytes| bytes.len()),
            )
            .field("duplicate_cursor", &self.duplicate_cursor)
            .finish()
    }
}

/// Pull-based event reader with bounded frames, an idle timeout and
/// opt-in reconnection with a finite consecutive retry budget. It reads only when
/// `next_event` is called.
pub struct RemoteEventStream {
    client: RemoteClient,
    port: RemoteEventPort,
    schema: ValueSchema,
    reader: BufReader<ureq::BodyReader<'static>>,
    cancelled: Arc<AtomicBool>,
    cursor: Option<String>,
    reconnects_left: u8,
}

impl fmt::Debug for RemoteEventStream {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("RemoteEventStream")
            .field("port", &self.port.port)
            .field("format", &self.port.format)
            .field("cursor", &self.cursor.as_ref().map(|_| "<redacted>"))
            .field("reconnects_left", &self.reconnects_left)
            .finish_non_exhaustive()
    }
}

impl RemoteEventStream {
    /// Read one event. Equal consecutive cursors are surfaced and marked as
    /// duplicates; no event is silently discarded by this transport layer.
    pub fn next_event(&mut self) -> Result<Option<RemoteStreamEvent>, RemoteError> {
        loop {
            check_cancel(&self.cancelled, EffectCertainty::NotStarted)?;
            let result = match self.port.format {
                RemoteEventFormat::Sse => self.read_sse_event(),
                RemoteEventFormat::JsonLines => self.read_json_line_event(),
            };
            match result {
                Ok(Some(mut event)) => {
                    if !json_matches_schema(&self.schema, &event.data) {
                        return Err(RemoteError::new(
                            RemoteErrorKind::InvalidStream,
                            EffectCertainty::NotStarted,
                        ));
                    }
                    event.duplicate_cursor = event
                        .cursor
                        .as_ref()
                        .is_some_and(|cursor| self.cursor.as_ref() == Some(cursor));
                    if let Some(cursor) = &event.cursor {
                        if !event.duplicate_cursor {
                            self.cursor = Some(cursor.clone());
                            // A new validated cursor demonstrates forward progress;
                            // duplicate events or heartbeats must not reset the budget.
                            if self.port.reconnect {
                                self.reconnects_left = self.port.max_reconnects;
                            }
                        }
                    }
                    return Ok(Some(event));
                }
                Ok(None) => {
                    if self.try_reconnect()? {
                        continue;
                    }
                    return Ok(None);
                }
                Err(error) => {
                    if self.try_reconnect()? {
                        continue;
                    }
                    return Err(error);
                }
            }
        }
    }

    pub fn last_cursor(&self) -> Option<&str> {
        self.cursor.as_deref()
    }

    fn try_reconnect(&mut self) -> Result<bool, RemoteError> {
        if !self.port.reconnect || self.reconnects_left == 0 || self.cursor.is_none() {
            return Ok(false);
        }
        check_cancel(&self.cancelled, EffectCertainty::NotStarted)?;
        let mut last_error = None;
        while self.reconnects_left > 0 {
            let attempt = self.port.max_reconnects - self.reconnects_left;
            self.reconnects_left -= 1;
            let delay = Duration::from_millis((50_u64 << attempt.min(4)).min(800));
            let start = Instant::now();
            while start.elapsed() < delay {
                check_cancel(&self.cancelled, EffectCertainty::NotStarted)?;
                std::thread::sleep(
                    delay
                        .saturating_sub(start.elapsed())
                        .min(Duration::from_millis(20)),
                );
            }
            match self.client.open_event_reader(
                &self.port,
                self.cursor.as_deref(),
                &self.cancelled,
                self.client.config.timeout_ms,
            ) {
                Ok(reader) => {
                    self.reader = reader;
                    return Ok(true);
                }
                Err(error) if error.kind == RemoteErrorKind::Cancelled => return Err(error),
                Err(error) => last_error = Some(error),
            }
        }
        Err(last_error.expect("at least one reconnect was attempted"))
    }

    fn read_sse_event(&mut self) -> Result<Option<RemoteStreamEvent>, RemoteError> {
        let mut cursor = None;
        let mut event = "message".to_owned();
        let mut data = Vec::new();
        let mut frame_bytes = 0usize;
        loop {
            let Some(mut line) =
                read_limited_line(&mut self.reader, self.port.max_event_bytes, &self.cancelled)?
            else {
                if data.is_empty() {
                    return Ok(None);
                }
                if data.last() == Some(&b'\n') {
                    data.pop();
                }
                return decode_sse_event(event, cursor, data).map(Some);
            };
            frame_bytes = frame_bytes.saturating_add(line.len());
            if frame_bytes > self.port.max_event_bytes {
                return Err(RemoteError::new(
                    RemoteErrorKind::StreamTooLarge,
                    EffectCertainty::NotStarted,
                ));
            }
            if line.last() == Some(&b'\n') {
                line.pop();
            }
            if line.last() == Some(&b'\r') {
                line.pop();
            }
            if line.is_empty() {
                if data.is_empty() {
                    cursor = None;
                    event = "message".to_owned();
                    frame_bytes = 0;
                    continue;
                }
                if data.last() == Some(&b'\n') {
                    data.pop();
                }
                return decode_sse_event(event, cursor, data).map(Some);
            }
            if line.first() == Some(&b':') {
                continue;
            }
            let line = std::str::from_utf8(&line).map_err(|_| {
                RemoteError::new(RemoteErrorKind::InvalidStream, EffectCertainty::NotStarted)
            })?;
            let (field, value) = line.split_once(':').unwrap_or((line, ""));
            let value = value.strip_prefix(' ').unwrap_or(value);
            match field {
                "data" => {
                    if data.len().saturating_add(value.len() + 1) > self.port.max_event_bytes {
                        return Err(RemoteError::new(
                            RemoteErrorKind::StreamTooLarge,
                            EffectCertainty::NotStarted,
                        ));
                    }
                    data.extend_from_slice(value.as_bytes());
                    data.push(b'\n');
                }
                "event" => {
                    if value.is_empty()
                        || value.len() > 128
                        || !value.bytes().all(|byte| {
                            byte.is_ascii_alphanumeric() || matches!(byte, b'_' | b'.' | b'-')
                        })
                    {
                        return Err(RemoteError::new(
                            RemoteErrorKind::InvalidStream,
                            EffectCertainty::NotStarted,
                        ));
                    }
                    event = value.to_owned();
                }
                "id" => {
                    if value.is_empty() {
                        cursor = None;
                    } else if valid_cursor(value) {
                        cursor = Some(value.to_owned());
                    } else {
                        return Err(RemoteError::new(
                            RemoteErrorKind::InvalidStream,
                            EffectCertainty::NotStarted,
                        ));
                    }
                }
                "retry" | "" => {}
                _ => {}
            }
        }
    }

    fn read_json_line_event(&mut self) -> Result<Option<RemoteStreamEvent>, RemoteError> {
        let Some(mut line) =
            read_limited_line(&mut self.reader, self.port.max_event_bytes, &self.cancelled)?
        else {
            return Ok(None);
        };
        if line.last() == Some(&b'\n') {
            line.pop();
        }
        if line.last() == Some(&b'\r') {
            line.pop();
        }
        if line.is_empty() {
            return Err(RemoteError::new(
                RemoteErrorKind::InvalidStream,
                EffectCertainty::NotStarted,
            ));
        }
        let frame: RemoteJsonLineEvent = parse_strict_json(&line).map_err(|_| {
            RemoteError::new(RemoteErrorKind::InvalidStream, EffectCertainty::NotStarted)
        })?;
        if frame
            .cursor
            .as_ref()
            .is_some_and(|cursor| !valid_cursor(cursor))
        {
            return Err(RemoteError::new(
                RemoteErrorKind::InvalidStream,
                EffectCertainty::NotStarted,
            ));
        }
        let event = frame.event.unwrap_or_else(|| "message".to_owned());
        if event.is_empty()
            || event.len() > 128
            || !event
                .bytes()
                .all(|byte| byte.is_ascii_alphanumeric() || matches!(byte, b'_' | b'.' | b'-'))
        {
            return Err(RemoteError::new(
                RemoteErrorKind::InvalidStream,
                EffectCertainty::NotStarted,
            ));
        }
        Ok(Some(RemoteStreamEvent {
            event,
            cursor: frame.cursor,
            data: frame.data,
            duplicate_cursor: false,
        }))
    }
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct RemoteJsonLineEvent {
    #[serde(default)]
    cursor: Option<String>,
    data: Value,
    #[serde(default)]
    event: Option<String>,
}

fn decode_sse_event(
    event: String,
    cursor: Option<String>,
    bytes: Vec<u8>,
) -> Result<RemoteStreamEvent, RemoteError> {
    let data = parse_strict_json(&bytes).map_err(|_| {
        RemoteError::new(RemoteErrorKind::InvalidStream, EffectCertainty::NotStarted)
    })?;
    Ok(RemoteStreamEvent {
        event,
        cursor,
        data,
        duplicate_cursor: false,
    })
}

fn read_limited_line<R: Read>(
    reader: &mut R,
    max_bytes: usize,
    cancelled: &AtomicBool,
) -> Result<Option<Vec<u8>>, RemoteError> {
    let mut line = Vec::new();
    let mut byte = [0_u8; 1];
    loop {
        check_cancel(cancelled, EffectCertainty::NotStarted)?;
        match reader.read(&mut byte) {
            Ok(0) if line.is_empty() => return Ok(None),
            Ok(0) => return Ok(Some(line)),
            Ok(_) => {
                line.push(byte[0]);
                if line.len() > max_bytes {
                    return Err(RemoteError::new(
                        RemoteErrorKind::StreamTooLarge,
                        EffectCertainty::NotStarted,
                    ));
                }
                if byte[0] == b'\n' {
                    return Ok(Some(line));
                }
            }
            Err(error) => {
                check_cancel(cancelled, EffectCertainty::NotStarted)?;
                return Err(RemoteError::new(
                    if is_timeout(&error) {
                        RemoteErrorKind::Timeout
                    } else {
                        RemoteErrorKind::Transport
                    },
                    EffectCertainty::NotStarted,
                ));
            }
        }
    }
}

fn default_event_bytes() -> usize {
    64 * 1024
}

fn pull_backpressure() -> RemoteBackpressure {
    RemoteBackpressure::Pull
}

fn validate_endpoint(endpoint: &str, allow_loopback_http: bool) -> Result<(), RemoteError> {
    let authority = endpoint
        .split_once("://")
        .map(|(_, remainder)| remainder.split(['/', '?', '#']).next().unwrap_or(""));
    if authority.is_some_and(|authority| authority.contains('@')) {
        return Err(RemoteError::new(
            RemoteErrorKind::InvalidConfiguration,
            EffectCertainty::NotStarted,
        ));
    }
    let parsed = Url::parse(endpoint).map_err(|_| {
        RemoteError::new(
            RemoteErrorKind::InvalidConfiguration,
            EffectCertainty::NotStarted,
        )
    })?;
    if !parsed.username().is_empty()
        || parsed.password().is_some()
        || parsed.query().is_some()
        || parsed.fragment().is_some()
        || parsed.path() != "/"
        || parsed.host().is_none()
    {
        return Err(RemoteError::new(
            RemoteErrorKind::InvalidConfiguration,
            EffectCertainty::NotStarted,
        ));
    }
    match parsed.scheme() {
        "https" => Ok(()),
        "http" if allow_loopback_http && is_loopback(parsed.host().expect("host checked")) => {
            Ok(())
        }
        _ => Err(RemoteError::new(
            RemoteErrorKind::InvalidConfiguration,
            EffectCertainty::NotStarted,
        )),
    }
}

fn is_loopback(host: Host<&str>) -> bool {
    match host {
        Host::Ipv4(address) => address.is_loopback(),
        Host::Ipv6(address) => address.is_loopback(),
        Host::Domain(name) => name.eq_ignore_ascii_case("localhost"),
    }
}

fn valid_applet_name(name: &str) -> bool {
    valid_namespaced(name, true)
}

fn valid_operation(name: &str) -> bool {
    valid_namespaced(name, false)
}

fn valid_port_name(name: &str) -> bool {
    valid_namespaced(name, false)
}

fn valid_argument_name(name: &str) -> bool {
    !name.is_empty()
        && name.len() <= 128
        && name
            .bytes()
            .all(|byte| byte.is_ascii_alphanumeric() || matches!(byte, b'_' | b'-'))
}

fn valid_namespaced(value: &str, first_lower: bool) -> bool {
    if value.is_empty() || value.len() > 128 {
        return false;
    }
    let mut previous_dot = true;
    for (index, byte) in value.bytes().enumerate() {
        if byte == b'.' {
            if previous_dot || index + 1 == value.len() {
                return false;
            }
            previous_dot = true;
            continue;
        }
        if !(byte.is_ascii_lowercase() || byte.is_ascii_digit() || matches!(byte, b'_' | b'-'))
            || first_lower && index == 0 && !byte.is_ascii_lowercase()
        {
            return false;
        }
        previous_dot = false;
    }
    true
}

fn valid_version_label(value: &str) -> bool {
    !value.is_empty()
        && value.len() <= 128
        && value
            .bytes()
            .all(|byte| byte.is_ascii_alphanumeric() || matches!(byte, b'.' | b'+' | b'-'))
}

fn valid_request_id(value: &str) -> bool {
    !value.is_empty()
        && value.len() <= 128
        && value
            .bytes()
            .all(|byte| byte.is_ascii_alphanumeric() || matches!(byte, b'.' | b'_' | b':' | b'-'))
}

fn valid_idempotency_key(value: &str) -> bool {
    valid_request_id(value) && value.len() <= 256
}

fn valid_cursor(value: &str) -> bool {
    !value.is_empty()
        && value.len() <= 1024
        && !value
            .chars()
            .any(|ch| ch == '\0' || ch == '\r' || ch == '\n')
}

fn valid_scope(value: &str) -> bool {
    !value.is_empty()
        && value.len() <= 256
        && value
            .chars()
            .all(|ch| ch.is_ascii_alphanumeric() || matches!(ch, ':' | '/' | '.' | '_' | '-'))
}

fn valid_error_code(value: &str) -> bool {
    !value.is_empty()
        && value.len() <= 128
        && value.bytes().all(|byte| {
            byte.is_ascii_lowercase() || byte.is_ascii_digit() || matches!(byte, b'_' | b'.' | b'-')
        })
}

fn contracts_have_same_interface(left: &AppletContract, right: &AppletContract) -> bool {
    if left.version != right.version
        || left.output != right.output
        || left.outcomes != right.outcomes
        || left.result_envelope != right.result_envelope
        || left.ports != right.ports
    {
        return false;
    }

    // Transport permissions belong to the selected backend. A remote
    // descriptor may therefore include network.connect where an equivalent
    // local implementation does not. All other advertised capabilities and
    // every advertised effect must stay within the manifest's declared
    // allowance; discovery never expands that allowance.
    let advertised_capabilities: BTreeSet<_> = left
        .capabilities
        .iter()
        .filter(|capability| capability.as_str() != "network.connect")
        .collect();
    let allowed_capabilities: BTreeSet<_> = right
        .capabilities
        .iter()
        .filter(|capability| capability.as_str() != "network.connect")
        .collect();
    let advertised_effects: BTreeSet<_> = left.effects.iter().collect();
    let allowed_effects: BTreeSet<_> = right.effects.iter().collect();
    advertised_capabilities.is_subset(&allowed_capabilities)
        && advertised_effects.is_subset(&allowed_effects)
}

fn next_request_id() -> String {
    static NEXT: AtomicU64 = AtomicU64::new(1);
    format!(
        "{}-{}",
        std::process::id(),
        NEXT.fetch_add(1, Ordering::Relaxed)
    )
}

fn read_regular_file(path: &Path, limit: usize) -> Result<Vec<u8>, RemoteError> {
    let mut options = OpenOptions::new();
    options.read(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        options.custom_flags(libc::O_NOFOLLOW | libc::O_NONBLOCK | libc::O_CLOEXEC);
    }
    #[cfg(windows)]
    {
        use std::os::windows::fs::OpenOptionsExt;
        const FILE_FLAG_OPEN_REPARSE_POINT: u32 = 0x0020_0000;
        options.custom_flags(FILE_FLAG_OPEN_REPARSE_POINT);
    }
    let mut file = options.open(path).map_err(|_| {
        RemoteError::new(
            RemoteErrorKind::DescriptorCacheUnavailable,
            EffectCertainty::NotStarted,
        )
    })?;
    let metadata = file.metadata().map_err(|_| {
        RemoteError::new(
            RemoteErrorKind::DescriptorCacheUnavailable,
            EffectCertainty::NotStarted,
        )
    })?;
    if !metadata.is_file() || metadata.len() > limit as u64 {
        return Err(RemoteError::new(
            RemoteErrorKind::ResponseTooLarge,
            EffectCertainty::NotStarted,
        ));
    }
    #[cfg(windows)]
    {
        use std::os::windows::fs::MetadataExt;
        if metadata.file_attributes() & 0x400 != 0 {
            return Err(RemoteError::new(
                RemoteErrorKind::DescriptorCacheUnavailable,
                EffectCertainty::NotStarted,
            ));
        }
    }
    let mut bytes = Vec::with_capacity(metadata.len() as usize);
    Read::by_ref(&mut file)
        .take((limit + 1) as u64)
        .read_to_end(&mut bytes)
        .map_err(|_| {
            RemoteError::new(
                RemoteErrorKind::DescriptorCacheUnavailable,
                EffectCertainty::NotStarted,
            )
        })?;
    if bytes.len() > limit {
        return Err(RemoteError::new(
            RemoteErrorKind::ResponseTooLarge,
            EffectCertainty::NotStarted,
        ));
    }
    Ok(bytes)
}

fn read_bounded<R: Read>(
    mut reader: R,
    max_bytes: usize,
    cancelled: &AtomicBool,
    certainty: EffectCertainty,
) -> Result<Vec<u8>, RemoteError> {
    let mut output = Vec::new();
    let mut buffer = [0_u8; 8192];
    loop {
        check_cancel(cancelled, certainty)?;
        let count = reader.read(&mut buffer).map_err(|error| {
            if cancelled.load(Ordering::Relaxed) {
                RemoteError::new(RemoteErrorKind::Cancelled, certainty)
            } else if is_timeout(&error) {
                RemoteError::new(RemoteErrorKind::Timeout, certainty)
            } else {
                RemoteError::new(RemoteErrorKind::Transport, certainty)
            }
        })?;
        if count == 0 {
            return Ok(output);
        }
        if output.len().saturating_add(count) > max_bytes {
            return Err(RemoteError::new(
                RemoteErrorKind::ResponseTooLarge,
                certainty,
            ));
        }
        output.extend_from_slice(&buffer[..count]);
    }
}

fn check_cancel(cancelled: &AtomicBool, certainty: EffectCertainty) -> Result<(), RemoteError> {
    if cancelled.load(Ordering::Relaxed) {
        Err(RemoteError::new(RemoteErrorKind::Cancelled, certainty))
    } else {
        Ok(())
    }
}

fn is_timeout(error: &io::Error) -> bool {
    error.kind() == io::ErrorKind::TimedOut
}

fn validate_content_type(
    value: Option<&ureq::http::HeaderValue>,
    expected: &str,
    require_version: bool,
    certainty: EffectCertainty,
) -> Result<(), RemoteError> {
    let Some(value) = value.and_then(|value| value.to_str().ok()) else {
        return Err(RemoteError::new(
            RemoteErrorKind::InvalidResponse,
            certainty,
        ));
    };
    let mut parts = value.split(';').map(str::trim);
    if !parts
        .next()
        .is_some_and(|media_type| media_type.eq_ignore_ascii_case(expected))
    {
        return Err(RemoteError::new(
            RemoteErrorKind::InvalidResponse,
            certainty,
        ));
    }
    if require_version
        && !parts.any(|parameter| {
            parameter.eq_ignore_ascii_case("version=1")
                || parameter.eq_ignore_ascii_case("version=\"1\"")
        })
    {
        return Err(RemoteError::new(
            RemoteErrorKind::InvalidResponse,
            certainty,
        ));
    }
    Ok(())
}

fn map_ureq_error(
    error: ureq::Error,
    certainty: EffectCertainty,
    cancelled: &AtomicBool,
) -> RemoteError {
    if cancelled.load(Ordering::Relaxed) {
        return RemoteError::new(RemoteErrorKind::Cancelled, certainty);
    }
    match error {
        ureq::Error::StatusCode(status) => {
            RemoteError::new(RemoteErrorKind::HttpStatus(status), certainty)
        }
        ureq::Error::Timeout(_) => RemoteError::new(RemoteErrorKind::Timeout, certainty),
        _ => RemoteError::new(RemoteErrorKind::Transport, certainty),
    }
}

/// DNS resolver supervised by the execute CLI process. A child is killed and
/// reaped when the caller cancels or its total request budget expires, so DNS
/// cannot leave a detached worker behind or block cancellation indefinitely.
///
/// The resolver also pins the endpoint against DNS rebinding: every resolved
/// address is checked against the SSRF guard, and a set containing any
/// non-public address is refused as a whole unless the endpoint's origin was
/// explicitly granted. Hostname checks alone cannot stop rebinding, so this
/// connect-time filter is the actual barrier; the by-name check only fails
/// fast for literals.
#[derive(Clone)]
struct InterruptibleSystemResolver {
    helper: Option<PathBuf>,
    cancelled: Arc<AtomicBool>,
    process_observer: Option<Arc<dyn Fn(usize) + Send + Sync>>,
    allow_loopback: bool,
    granted_origin: bool,
}

impl std::fmt::Debug for InterruptibleSystemResolver {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter
            .debug_struct("InterruptibleSystemResolver")
            .field("helper", &self.helper)
            .finish_non_exhaustive()
    }
}

impl Resolver for InterruptibleSystemResolver {
    fn resolve(
        &self,
        uri: &ureq::http::Uri,
        _config: &ureq::config::Config,
        timeout: ureq::unversioned::transport::NextTimeout,
    ) -> Result<ResolvedSocketAddrs, ureq::Error> {
        if !matches!(uri.scheme_str(), Some("http" | "https")) {
            return Err(ureq::Error::BadUri("unsupported URL scheme".to_owned()));
        }
        let authority = uri
            .authority()
            .ok_or_else(|| ureq::Error::BadUri("missing authority".to_owned()))?;
        let port = authority
            .port_u16()
            .or_else(|| match uri.scheme_str() {
                Some("http") => Some(80),
                Some("https") => Some(443),
                _ => None,
            })
            .ok_or_else(|| ureq::Error::BadUri("missing port".to_owned()))?;
        let host = authority.host();
        let mut addrs = self.empty();
        if let Ok(ip) = host.parse::<std::net::IpAddr>() {
            addrs.push(std::net::SocketAddr::new(ip, port));
        } else {
            let helper = self.helper.as_deref().ok_or(ureq::Error::HostNotFound)?;
            addrs = resolve_with_helper(
                helper,
                host,
                port,
                timeout,
                &self.cancelled,
                self.process_observer.as_deref(),
            )?;
        }
        // Pin the resolved set before the transport connects: any blocked
        // address refuses the whole set, which stops DNS rebinding even when
        // the hostname looked benign at check time.
        ssrf::check_resolved_addrs(&addrs, self.allow_loopback, self.granted_origin)
            .map_err(|_| {
                ureq::Error::Io(io::Error::new(
                    io::ErrorKind::PermissionDenied,
                    "blocked non-public network destination",
                ))
            })?;
        if addrs.is_empty() {
            Err(ureq::Error::HostNotFound)
        } else {
            Ok(addrs)
        }
    }
}

struct ResolverChild(Child);

impl Drop for ResolverChild {
    fn drop(&mut self) {
        // Every early exit kills and reaps the helper, including cancellation
        // and malformed or oversized helper output.
        let _ = self.0.kill();
        let _ = self.0.wait();
    }
}

fn resolve_with_helper(
    helper: &Path,
    host: &str,
    port: u16,
    timeout: ureq::unversioned::transport::NextTimeout,
    cancelled: &AtomicBool,
    process_observer: Option<&(dyn Fn(usize) + Send + Sync)>,
) -> Result<ResolvedSocketAddrs, ureq::Error> {
    let started = Instant::now();
    let mut child = ResolverChild(
        Command::new(helper)
            .arg("__remote_resolve")
            .arg(host)
            .arg(port.to_string())
            .env_clear()
            .stdin(Stdio::null())
            .stdout(Stdio::piped())
            .stderr(Stdio::null())
            .spawn()
            .map_err(ureq::Error::Io)?,
    );
    if let Some(observer) = process_observer {
        observer(1);
    }

    loop {
        if cancelled.load(Ordering::Relaxed) {
            return Err(ureq::Error::Io(io::Error::new(
                io::ErrorKind::Interrupted,
                "request cancelled",
            )));
        }
        if let Some(limit) = timeout.not_zero() {
            if started.elapsed() >= *limit {
                return Err(ureq::Error::Timeout(timeout.reason));
            }
        }
        if child.0.try_wait().map_err(ureq::Error::Io)?.is_some() {
            break;
        }
        std::thread::sleep(transport::CANCEL_POLL_INTERVAL);
    }

    let status = child.0.wait().map_err(ureq::Error::Io)?;
    if !status.success() {
        return Err(ureq::Error::HostNotFound);
    }
    let mut output = Vec::new();
    child
        .0
        .stdout
        .take()
        .ok_or(ureq::Error::HostNotFound)?
        .take(4097)
        .read_to_end(&mut output)
        .map_err(ureq::Error::Io)?;
    if output.is_empty() || output.len() > 4096 || !output.ends_with(b"\n") {
        return Err(ureq::Error::HostNotFound);
    }
    let text = std::str::from_utf8(&output).map_err(|_| ureq::Error::HostNotFound)?;
    let mut addrs = ResolvedSocketAddrs::from_fn(|_| std::net::SocketAddr::from(([0, 0, 0, 0], 0)));
    for line in text.lines() {
        let address = line
            .parse::<std::net::SocketAddr>()
            .map_err(|_| ureq::Error::HostNotFound)?;
        if addrs.len() == 16 {
            return Err(ureq::Error::HostNotFound);
        }
        addrs.push(address);
    }
    if addrs.is_empty() {
        Err(ureq::Error::HostNotFound)
    } else {
        Ok(addrs)
    }
}

fn parse_strict_json<T: for<'de> Deserialize<'de>>(bytes: &[u8]) -> Result<T, serde_json::Error> {
    let mut deserializer = serde_json::Deserializer::from_slice(bytes);
    let value = StrictJsonValue::deserialize(&mut deserializer)?;
    deserializer.end()?;
    let json = value.into_json();
    serde_json::from_value(json)
        .map_err(|error| <serde_json::Error as serde::de::Error>::custom(error.to_string()))
}

enum StrictJsonValue {
    Null,
    Boolean(bool),
    Number(serde_json::Number),
    String(String),
    Array(Vec<Self>),
    Object(BTreeMap<String, Self>),
}

impl StrictJsonValue {
    fn into_json(self) -> Value {
        match self {
            Self::Null => Value::Null,
            Self::Boolean(value) => Value::Bool(value),
            Self::Number(value) => Value::Number(value),
            Self::String(value) => Value::String(value),
            Self::Array(values) => Value::Array(values.into_iter().map(Self::into_json).collect()),
            Self::Object(values) => Value::Object(
                values
                    .into_iter()
                    .map(|(key, value)| (key, value.into_json()))
                    .collect(),
            ),
        }
    }
}

impl<'de> Deserialize<'de> for StrictJsonValue {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: serde::Deserializer<'de>,
    {
        struct StrictVisitor;
        impl<'de> serde::de::Visitor<'de> for StrictVisitor {
            type Value = StrictJsonValue;

            fn expecting(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
                formatter.write_str("a JSON value with unique object keys")
            }

            fn visit_unit<E>(self) -> Result<Self::Value, E>
            where
                E: serde::de::Error,
            {
                Ok(StrictJsonValue::Null)
            }

            fn visit_none<E>(self) -> Result<Self::Value, E>
            where
                E: serde::de::Error,
            {
                Ok(StrictJsonValue::Null)
            }

            fn visit_bool<E>(self, value: bool) -> Result<Self::Value, E>
            where
                E: serde::de::Error,
            {
                Ok(StrictJsonValue::Boolean(value))
            }

            fn visit_i64<E>(self, value: i64) -> Result<Self::Value, E>
            where
                E: serde::de::Error,
            {
                Ok(StrictJsonValue::Number(value.into()))
            }

            fn visit_u64<E>(self, value: u64) -> Result<Self::Value, E>
            where
                E: serde::de::Error,
            {
                Ok(StrictJsonValue::Number(value.into()))
            }

            fn visit_f64<E>(self, value: f64) -> Result<Self::Value, E>
            where
                E: serde::de::Error,
            {
                serde_json::Number::from_f64(value)
                    .map(StrictJsonValue::Number)
                    .ok_or_else(|| serde::de::Error::custom("non-finite JSON number"))
            }

            fn visit_str<E>(self, value: &str) -> Result<Self::Value, E>
            where
                E: serde::de::Error,
            {
                Ok(StrictJsonValue::String(value.to_owned()))
            }

            fn visit_string<E>(self, value: String) -> Result<Self::Value, E>
            where
                E: serde::de::Error,
            {
                Ok(StrictJsonValue::String(value))
            }

            fn visit_seq<A>(self, mut sequence: A) -> Result<Self::Value, A::Error>
            where
                A: serde::de::SeqAccess<'de>,
            {
                let mut values = Vec::new();
                while let Some(value) = sequence.next_element()? {
                    values.push(value);
                }
                Ok(StrictJsonValue::Array(values))
            }

            fn visit_map<A>(self, mut map: A) -> Result<Self::Value, A::Error>
            where
                A: serde::de::MapAccess<'de>,
            {
                let mut values = BTreeMap::new();
                while let Some((key, value)) = map.next_entry::<String, StrictJsonValue>()? {
                    if values.insert(key.clone(), value).is_some() {
                        return Err(serde::de::Error::custom(format!(
                            "duplicate JSON object key {key:?}"
                        )));
                    }
                }
                Ok(StrictJsonValue::Object(values))
            }
        }
        deserializer.deserialize_any(StrictVisitor)
    }
}

fn encode_base64(bytes: &[u8]) -> String {
    const TABLE: &[u8; 64] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789+/";
    let mut output = String::with_capacity(bytes.len().div_ceil(3) * 4);
    for chunk in bytes.chunks(3) {
        let first = chunk[0];
        let second = chunk.get(1).copied().unwrap_or(0);
        let third = chunk.get(2).copied().unwrap_or(0);
        output.push(TABLE[(first >> 2) as usize] as char);
        output.push(TABLE[(((first & 0x03) << 4) | (second >> 4)) as usize] as char);
        output.push(if chunk.len() > 1 {
            TABLE[(((second & 0x0f) << 2) | (third >> 6)) as usize] as char
        } else {
            '='
        });
        output.push(if chunk.len() > 2 {
            TABLE[(third & 0x3f) as usize] as char
        } else {
            '='
        });
    }
    output
}

fn decode_base64(encoded: &str, max_bytes: usize) -> Result<Vec<u8>, RemoteErrorKind> {
    fn digit(byte: u8) -> Option<u8> {
        match byte {
            b'A'..=b'Z' => Some(byte - b'A'),
            b'a'..=b'z' => Some(byte - b'a' + 26),
            b'0'..=b'9' => Some(byte - b'0' + 52),
            b'+' => Some(62),
            b'/' => Some(63),
            _ => None,
        }
    }
    if !encoded.len().is_multiple_of(4) {
        return Err(RemoteErrorKind::InvalidResponse);
    }
    let decoded_len = (encoded.len() / 4)
        .checked_mul(3)
        .and_then(|length| {
            if encoded.ends_with("==") {
                length.checked_sub(2)
            } else if encoded.ends_with('=') {
                length.checked_sub(1)
            } else {
                Some(length)
            }
        })
        .ok_or(RemoteErrorKind::InvalidResponse)?;
    if decoded_len > max_bytes {
        return Err(RemoteErrorKind::ResponseTooLarge);
    }
    let bytes = encoded.as_bytes();
    let mut output = Vec::with_capacity(decoded_len);
    let (chunks, _) = bytes.as_chunks::<4>();
    for (index, chunk) in chunks.iter().enumerate() {
        let last = index + 1 == bytes.len() / 4;
        let a = digit(chunk[0]).ok_or(RemoteErrorKind::InvalidResponse)?;
        let b = digit(chunk[1]).ok_or(RemoteErrorKind::InvalidResponse)?;
        let c = if chunk[2] == b'=' {
            if !last || chunk[3] != b'=' || b & 0x0f != 0 {
                return Err(RemoteErrorKind::InvalidResponse);
            }
            0
        } else {
            digit(chunk[2]).ok_or(RemoteErrorKind::InvalidResponse)?
        };
        let d = if chunk[3] == b'=' {
            if !last || chunk[2] != b'=' && c & 0x03 != 0 {
                return Err(RemoteErrorKind::InvalidResponse);
            }
            0
        } else {
            digit(chunk[3]).ok_or(RemoteErrorKind::InvalidResponse)?
        };
        output.push((a << 2) | (b >> 4));
        if chunk[2] != b'=' {
            output.push((b << 4) | (c >> 2));
        }
        if chunk[3] != b'=' {
            output.push((c << 6) | d);
        }
    }
    if output.len() != decoded_len {
        return Err(RemoteErrorKind::InvalidResponse);
    }
    Ok(output)
}

fn remote_value_matches_schema(value: &RemoteValue, schema: &ValueSchema) -> bool {
    match value {
        RemoteValue::Json(json) => json_matches_schema(schema, json),
        RemoteValue::Base64(encoded) => {
            schema.kind == OutputType::Bytes && decode_base64(encoded, MAX_FRAME_BYTES).is_ok()
        }
    }
}

fn remote_value_matches_input(value: &RemoteValue, port: &crate::contracts::AppletPort) -> bool {
    match (port.encoding, value) {
        (Some(crate::contracts::OutputEncoding::Json), RemoteValue::Json(json)) => {
            json_matches_schema(&port.schema, json)
        }
        (Some(crate::contracts::OutputEncoding::Raw), RemoteValue::Base64(encoded)) => {
            let Ok(bytes) = decode_base64(encoded, MAX_FRAME_BYTES) else {
                return false;
            };
            match port.schema.kind {
                OutputType::Bytes => true,
                OutputType::Text | OutputType::Path => {
                    std::str::from_utf8(&bytes).ok().is_some_and(|text| {
                        port.schema
                            .validate_value(&ast::Value::String(text.to_owned()))
                            .is_ok()
                    })
                }
                _ => false,
            }
        }
        _ => false,
    }
}

fn json_matches_schema(schema: &ValueSchema, value: &Value) -> bool {
    schema.validate_value(&json_to_ast(value)).is_ok()
}

fn json_to_ast(value: &Value) -> ast::Value {
    match value {
        Value::Null => ast::Value::Null,
        Value::Bool(value) => ast::Value::Bool(*value),
        Value::Number(value) => value
            .as_i64()
            .map(ast::Value::Integer)
            .unwrap_or_else(|| ast::Value::Number(value.clone())),
        Value::String(value) => ast::Value::String(value.clone()),
        Value::Array(values) => ast::Value::List(values.iter().map(json_to_ast).collect()),
        Value::Object(values) => ast::Value::Record(
            values
                .iter()
                .map(|(name, value)| (name.clone(), json_to_ast(value)))
                .collect(),
        ),
    }
}
