//! Optional per-agent authorization for the local live-control protocol.
//!
//! The live socket remains a trusted same-user interface when no policy is
//! configured. With a policy installed, each request must carry a named agent
//! credential and the agent must hold the permission required by that
//! operation. Policy files contain token hashes, never bearer tokens. This is
//! cooperative-client authorization: it does not isolate credentials from a
//! hostile process running as the same OS user. Store token files in an
//! owner-private OS location that applet sandboxes do not expose to workloads.

use serde::de::{MapAccess, Visitor};
use serde::{Deserialize, Deserializer, Serialize};
use sha2::{Digest, Sha256};
use std::collections::{BTreeMap, BTreeSet};
use std::fmt;
use std::fs::File;
use std::io::{self, Read, Write};
use std::path::Path;

const MAX_POLICY_BYTES: usize = 64 * 1024;
const MAX_TOKEN_FILE_BYTES: usize = 8 * 1024;
const MIN_TOKEN_BYTES: usize = 32;

/// Credential carried only in a live-control request. Its debug form never
/// includes the token, and callers should remove this field before persisting
/// the request or constructing a response.
#[derive(Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct RequestAuth {
    pub agent: String,
    pub token: String,
}

impl fmt::Debug for RequestAuth {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("RequestAuth")
            .field("agent", &self.agent)
            .field("token", &"<redacted>")
            .finish()
    }
}

/// Client-side credential loaded from an explicitly named private token file.
#[derive(Clone, PartialEq, Eq)]
pub struct AgentCredential {
    agent: String,
    token: String,
}

impl fmt::Debug for AgentCredential {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("AgentCredential")
            .field("agent", &self.agent)
            .field("token", &"<redacted>")
            .finish()
    }
}

impl AgentCredential {
    /// Create a high-entropy bearer token in a new owner-private file. The
    /// file is never overwritten. Choose a path outside applet sandbox read
    /// grants. Only its hash should be copied into the server's agent policy;
    /// the token itself is returned in memory for the caller to save through
    /// an explicitly authorized client setup path.
    pub fn create_token_file(agent: impl Into<String>, path: &Path) -> Result<Self, AuthError> {
        let agent = agent.into();
        if !valid_agent_id(&agent) {
            return Err(AuthError::InvalidCredential);
        }
        let mut random = [0u8; 32];
        getrandom::fill(&mut random).map_err(|_| AuthError::CredentialUnavailable)?;
        let token = hex(&random);

        let mut options = std::fs::OpenOptions::new();
        options.write(true).create_new(true);
        #[cfg(unix)]
        {
            use std::os::unix::fs::OpenOptionsExt;
            options
                .mode(0o600)
                .custom_flags(libc::O_NOFOLLOW | libc::O_CLOEXEC);
        }
        #[cfg(windows)]
        {
            use std::os::windows::fs::OpenOptionsExt;
            options.custom_flags(
                windows_sys::Win32::Storage::FileSystem::FILE_FLAG_OPEN_REPARSE_POINT,
            );
        }
        let mut file = options
            .open(path)
            .map_err(|_| AuthError::CredentialUnavailable)?;
        #[cfg(windows)]
        if crate::platform::windows::restrict_file(&file).is_err() {
            return Err(AuthError::CredentialUnavailable);
        }
        if file
            .write_all(token.as_bytes())
            .and_then(|()| file.write_all(b"\n"))
            .and_then(|()| file.sync_all())
            .is_err()
        {
            return Err(AuthError::CredentialUnavailable);
        }
        Ok(Self { agent, token })
    }

    /// Read one explicitly named token. The token file must be a regular,
    /// private file owned by the current user on Unix. A single final LF or
    /// CRLF is ignored; other whitespace is rejected.
    pub fn from_token_file(agent: impl Into<String>, path: &Path) -> Result<Self, AuthError> {
        let agent = agent.into();
        if !valid_agent_id(&agent) {
            return Err(AuthError::InvalidCredential);
        }
        let mut file = open_private_file(path).map_err(|_| AuthError::CredentialUnavailable)?;
        let mut bytes = Vec::new();
        Read::by_ref(&mut file)
            .take((MAX_TOKEN_FILE_BYTES + 1) as u64)
            .read_to_end(&mut bytes)
            .map_err(|_| AuthError::CredentialUnavailable)?;
        if bytes.len() > MAX_TOKEN_FILE_BYTES {
            return Err(AuthError::InvalidCredential);
        }
        if bytes.ends_with(b"\r\n") {
            bytes.truncate(bytes.len() - 2);
        } else if bytes.ends_with(b"\n") {
            bytes.truncate(bytes.len() - 1);
        }
        if !valid_token(&bytes) {
            return Err(AuthError::InvalidCredential);
        }
        let token = String::from_utf8(bytes).map_err(|_| AuthError::InvalidCredential)?;
        Ok(Self { agent, token })
    }

    /// Build the request field. The caller owns the resulting in-memory token
    /// and is responsible for omitting it from stored state and responses.
    pub fn request_auth(&self) -> RequestAuth {
        RequestAuth {
            agent: self.agent.clone(),
            token: self.token.clone(),
        }
    }

    /// Return the lowercase SHA-256 token hash for a policy entry.
    pub fn token_sha256(&self) -> String {
        sha256_hex(self.token.as_bytes())
    }
}

/// Permissions that can be granted to a named live-control agent.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum AgentPermission {
    Read,
    Edit,
    Control,
    Input,
    Stop,
}

#[derive(Clone, PartialEq, Eq, Deserialize)]
#[serde(deny_unknown_fields)]
struct AgentGrant {
    token_sha256: String,
    permissions: BTreeSet<AgentPermission>,
}

impl fmt::Debug for AgentGrant {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("AgentGrant")
            .field("token_sha256", &"<redacted>")
            .field("permissions", &self.permissions)
            .finish()
    }
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct PolicyDocument {
    version: u32,
    agents: UniqueMap<AgentGrant>,
}

/// Validated per-agent live-control policy. A policy can be installed at
/// server startup; absent policy preserves the existing same-user socket mode.
#[derive(Clone, PartialEq, Eq)]
pub struct AgentAuthPolicy {
    agents: BTreeMap<String, AgentGrant>,
}

impl fmt::Debug for AgentAuthPolicy {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("AgentAuthPolicy")
            .field("agents", &self.agents.keys().collect::<Vec<_>>())
            .finish()
    }
}

impl AgentAuthPolicy {
    /// Load a bounded policy file. The file must be regular and non-symlink;
    /// Unix files must be owned by the current user and have no group/other
    /// permissions. The policy stores token hashes only.
    pub fn load(path: &Path) -> Result<Self, AuthError> {
        let mut file = open_private_file(path).map_err(|_| AuthError::PolicyUnavailable)?;
        let mut bytes = Vec::new();
        Read::by_ref(&mut file)
            .take((MAX_POLICY_BYTES + 1) as u64)
            .read_to_end(&mut bytes)
            .map_err(|_| AuthError::PolicyUnavailable)?;
        if bytes.len() > MAX_POLICY_BYTES {
            return Err(AuthError::InvalidPolicy);
        }
        Self::from_json(&bytes)
    }

    /// Parse and validate the version-1 JSON policy.
    pub fn from_json(bytes: &[u8]) -> Result<Self, AuthError> {
        if bytes.len() > MAX_POLICY_BYTES {
            return Err(AuthError::InvalidPolicy);
        }
        let document: PolicyDocument =
            serde_json::from_slice(bytes).map_err(|_| AuthError::InvalidPolicy)?;
        if document.version != 1 || document.agents.0.is_empty() || document.agents.0.len() > 256 {
            return Err(AuthError::InvalidPolicy);
        }
        for (name, grant) in &document.agents.0 {
            if !valid_agent_id(name)
                || !valid_digest(&grant.token_sha256)
                || grant.permissions.is_empty()
            {
                return Err(AuthError::InvalidPolicy);
            }
        }
        Ok(Self {
            agents: document.agents.0,
        })
    }

    /// Authorize the exact operation before live-state dispatch. Unknown
    /// operations and unsupported control actions fail closed.
    pub fn authorize(
        &self,
        auth: Option<&RequestAuth>,
        operation: &str,
        action: Option<&str>,
    ) -> Result<(), AuthError> {
        let auth = auth.ok_or(AuthError::Unauthorized)?;
        if !valid_agent_id(&auth.agent) || !valid_token(auth.token.as_bytes()) {
            return Err(AuthError::Unauthorized);
        }
        let grant = self
            .agents
            .get(&auth.agent)
            .ok_or(AuthError::Unauthorized)?;
        let supplied = sha256_hex(auth.token.as_bytes());
        if !constant_time_eq(supplied.as_bytes(), grant.token_sha256.as_bytes()) {
            return Err(AuthError::Unauthorized);
        }
        let required = required_permission(operation, action)?;
        if grant.permissions.contains(&required) {
            Ok(())
        } else {
            Err(AuthError::Unauthorized)
        }
    }
}

/// Determine which grant protects one validated live request. Exposed so the
/// request parser can authorize before any mutation is attempted.
pub fn required_permission(
    operation: &str,
    action: Option<&str>,
) -> Result<AgentPermission, AuthError> {
    match operation {
        "read" | "wait" => Ok(AgentPermission::Read),
        "create" | "update" | "delete" | "patch" => Ok(AgentPermission::Edit),
        "send" | "event" => Ok(AgentPermission::Input),
        "control" => match action {
            Some("stop") => Ok(AgentPermission::Stop),
            Some(
                "pause" | "resume" | "step" | "continue" | "breakpoint" | "cancel" | "retry"
                | "reset" | "restart" | "restart-source",
            ) => Ok(AgentPermission::Control),
            _ => Err(AuthError::InvalidRequest),
        },
        _ => Err(AuthError::InvalidRequest),
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum AuthError {
    PolicyUnavailable,
    InvalidPolicy,
    CredentialUnavailable,
    InvalidCredential,
    InvalidRequest,
    Unauthorized,
}

impl fmt::Display for AuthError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str(match self {
            Self::PolicyUnavailable => "agent authorization policy is unavailable",
            Self::InvalidPolicy => "agent authorization policy is invalid",
            Self::CredentialUnavailable => "agent credential is unavailable",
            Self::InvalidCredential => "agent credential is invalid",
            Self::InvalidRequest => "live request has no authorization mapping",
            Self::Unauthorized => "live request is not authorized",
        })
    }
}

impl std::error::Error for AuthError {}

struct UniqueMap<T>(BTreeMap<String, T>);

impl<'de, T: Deserialize<'de>> Deserialize<'de> for UniqueMap<T> {
    fn deserialize<D: Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        struct UniqueMapVisitor<T>(std::marker::PhantomData<T>);
        impl<'de, T: Deserialize<'de>> Visitor<'de> for UniqueMapVisitor<T> {
            type Value = UniqueMap<T>;

            fn expecting(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
                formatter.write_str("an object with unique keys")
            }

            fn visit_map<A: MapAccess<'de>>(self, mut access: A) -> Result<Self::Value, A::Error> {
                let mut values = BTreeMap::new();
                while let Some((key, value)) = access.next_entry::<String, T>()? {
                    if values.insert(key, value).is_some() {
                        return Err(serde::de::Error::custom("duplicate agent id"));
                    }
                }
                Ok(UniqueMap(values))
            }
        }
        deserializer.deserialize_map(UniqueMapVisitor(std::marker::PhantomData))
    }
}

fn open_private_file(path: &Path) -> io::Result<File> {
    use std::fs::OpenOptions;
    let mut options = OpenOptions::new();
    options.read(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        options.custom_flags(libc::O_NOFOLLOW | libc::O_CLOEXEC | libc::O_NONBLOCK);
    }
    #[cfg(windows)]
    {
        use std::os::windows::fs::OpenOptionsExt;
        options.custom_flags(windows_sys::Win32::Storage::FileSystem::FILE_FLAG_OPEN_REPARSE_POINT);
    }
    let file = options.open(path)?;
    let metadata = file.metadata()?;
    if !metadata.is_file() || metadata.file_type().is_symlink() {
        return Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            "not a regular file",
        ));
    }
    #[cfg(unix)]
    {
        use std::os::unix::fs::MetadataExt;
        if metadata.uid() != unsafe { libc::geteuid() } || metadata.mode() & 0o077 != 0 {
            return Err(io::Error::new(
                io::ErrorKind::PermissionDenied,
                "file is not private",
            ));
        }
    }
    #[cfg(windows)]
    if crate::platform::windows::is_reparse(&metadata) {
        return Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            "file is a reparse point",
        ));
    }
    Ok(file)
}

fn valid_agent_id(value: &str) -> bool {
    !value.is_empty()
        && value.len() <= 64
        && value
            .bytes()
            .all(|byte| byte.is_ascii_alphanumeric() || matches!(byte, b'_' | b'-' | b'.'))
}

fn valid_token(bytes: &[u8]) -> bool {
    (MIN_TOKEN_BYTES..=MAX_TOKEN_FILE_BYTES).contains(&bytes.len())
        && bytes.iter().all(|byte| (0x21..=0x7e).contains(byte))
}

fn valid_digest(value: &str) -> bool {
    value.len() == 64
        && value
            .bytes()
            .all(|byte| byte.is_ascii_digit() || (b'a'..=b'f').contains(&byte))
}

fn sha256_hex(bytes: &[u8]) -> String {
    let digest = Sha256::digest(bytes);
    let mut output = String::with_capacity(64);
    for byte in digest {
        use std::fmt::Write as _;
        write!(&mut output, "{byte:02x}").expect("writing to String cannot fail");
    }
    output
}

fn hex(bytes: &[u8]) -> String {
    let mut output = String::with_capacity(bytes.len() * 2);
    for byte in bytes {
        use std::fmt::Write as _;
        write!(&mut output, "{byte:02x}").expect("writing to String cannot fail");
    }
    output
}

fn constant_time_eq(left: &[u8], right: &[u8]) -> bool {
    if left.len() != right.len() {
        return false;
    }
    left.iter()
        .zip(right)
        .fold(0u8, |difference, (left, right)| difference | (left ^ right))
        == 0
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn policy(permissions: &[&str]) -> AgentAuthPolicy {
        let token_hash = sha256_hex(b"0123456789abcdef0123456789abcdef");
        AgentAuthPolicy::from_json(
            json!({
                "version": 1,
                "agents": {
                    "writer": {
                        "token_sha256": token_hash,
                        "permissions": permissions,
                    }
                }
            })
            .to_string()
            .as_bytes(),
        )
        .unwrap()
    }

    fn auth() -> RequestAuth {
        RequestAuth {
            agent: "writer".into(),
            token: "0123456789abcdef0123456789abcdef".into(),
        }
    }

    #[test]
    fn authorization_is_bound_to_agent_token_operation_and_action() {
        let policy = policy(&["read", "edit", "control"]);
        assert!(policy.authorize(Some(&auth()), "update", None).is_ok());
        assert!(policy.authorize(Some(&auth()), "read", None).is_ok());
        assert!(policy.authorize(Some(&auth()), "send", None).is_err());
        assert!(policy
            .authorize(Some(&auth()), "control", Some("stop"))
            .is_err());
        assert!(policy
            .authorize(Some(&auth()), "control", Some("pause"))
            .is_ok());
        assert!(policy.authorize(None, "read", None).is_err());
        let mut wrong = auth();
        wrong.token = "abcdefghijklmnopqrstuvwxyz012345".into();
        assert!(policy.authorize(Some(&wrong), "read", None).is_err());
        let mut unknown_agent = auth();
        unknown_agent.agent = "other".into();
        assert!(policy
            .authorize(Some(&unknown_agent), "read", None)
            .is_err());
    }

    #[test]
    fn policy_parse_fails_closed_for_ambiguous_or_unknown_fields() {
        let hash = sha256_hex(b"0123456789abcdef0123456789abcdef");
        let valid_grant = format!("{{\"token_sha256\":\"{hash}\",\"permissions\":[\"read\"]}}");
        for policy in [
            format!("{{\"version\":1,\"agents\":{{\"a\":{valid_grant},\"a\":{valid_grant}}}}}"),
            format!(
                "{{\"version\":1,\"agents\":{{\"a\":{{\"token_sha256\":\"{hash}\",\"permissions\":[\"read\"],\"extra\":true}}}}}}"
            ),
            format!("{{\"version\":2,\"agents\":{{\"a\":{valid_grant}}}}}"),
        ] {
            assert!(AgentAuthPolicy::from_json(policy.as_bytes()).is_err());
        }
    }

    #[test]
    fn debug_output_never_contains_bearer_tokens_or_hashes() {
        let token = "0123456789abcdef0123456789abcdef";
        let auth = RequestAuth {
            agent: "writer".into(),
            token: token.into(),
        };
        assert!(!format!("{auth:?}").contains(token));
        let policy = policy(&["read"]);
        assert!(!format!("{policy:?}").contains(&sha256_hex(token.as_bytes())));
    }

    #[test]
    fn control_actions_have_separate_stop_permission() {
        assert_eq!(
            required_permission("control", Some("stop")),
            Ok(AgentPermission::Stop)
        );
        assert_eq!(
            required_permission("control", Some("resume")),
            Ok(AgentPermission::Control)
        );
        assert!(required_permission("control", Some("unknown")).is_err());
        assert!(required_permission("other", None).is_err());
    }

    #[cfg(unix)]
    #[test]
    fn credential_file_is_bounded_private_and_redacted() {
        use std::os::unix::fs::PermissionsExt;
        let temp = tempfile::tempdir().unwrap();
        let path = temp.path().join("token");
        std::fs::write(&path, b"0123456789abcdef0123456789abcdef\n").unwrap();
        std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o600)).unwrap();
        let credential = AgentCredential::from_token_file("writer", &path).unwrap();
        let auth = credential.request_auth();
        assert_eq!(auth.token, "0123456789abcdef0123456789abcdef");
        assert!(!format!("{credential:?}").contains(&auth.token));
        std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o644)).unwrap();
        assert!(AgentCredential::from_token_file("writer", &path).is_err());
    }

    #[cfg(unix)]
    #[test]
    fn generated_token_is_high_entropy_private_and_never_overwrites() {
        use std::os::unix::fs::PermissionsExt;
        let temp = tempfile::tempdir().unwrap();
        let path = temp.path().join("agent.token");
        let credential = AgentCredential::create_token_file("editor", &path).unwrap();
        assert_eq!(credential.token.len(), 64);
        assert!(credential
            .token
            .bytes()
            .all(|byte| byte.is_ascii_hexdigit()));
        assert_ne!(credential.token_sha256(), credential.token);
        assert_eq!(
            std::fs::metadata(&path).unwrap().permissions().mode() & 0o777,
            0o600
        );
        let loaded = AgentCredential::from_token_file("editor", &path).unwrap();
        assert_eq!(loaded.token_sha256(), credential.token_sha256());
        assert!(AgentCredential::create_token_file("editor", &path).is_err());
        assert_eq!(
            AgentCredential::from_token_file("editor", &path)
                .unwrap()
                .token_sha256(),
            credential.token_sha256()
        );
    }
}
