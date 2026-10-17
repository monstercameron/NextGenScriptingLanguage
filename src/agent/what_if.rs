//! `execute run FILE --what-if` and `--confirm FINGERPRINT`: confirm the plan you
//! reviewed. `--what-if` prints the explain plan with a fingerprint and runs
//! nothing. `--confirm` runs only when the current plan's fingerprint equals the
//! one given, so a changed script or manifest is refused before any effect.

use anyhow::Result;
use serde_json::{json, Value};
use sha2::{Digest, Sha256};

/// Protocol identifier of the `--what-if` object.
pub const PROTOCOL: &str = "execute.what_if/1";

/// SHA-256 of the plan's canonical JSON. The plan's object keys are sorted, so the
/// same plan always gives the same fingerprint.
pub fn fingerprint(plan: &Value) -> Result<String> {
    let bytes = serde_json::to_vec(plan)?;
    Ok(format!("{:x}", Sha256::digest(&bytes)))
}

/// The `--what-if` report: the fingerprint to confirm, and the plan it covers.
pub fn report(program_path: &str, fingerprint: &str, plan: Value) -> Value {
    json!({
        "protocol": PROTOCOL,
        "program": program_path,
        "fingerprint": fingerprint,
        "plan": plan,
    })
}
