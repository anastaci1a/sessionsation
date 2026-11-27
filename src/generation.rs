//! Generation hashing (guard role `generation`): the sha256 hex that clients hold
//! as a session `generation`, an account rename `expected_generation`, a messaging
//! attachment generation or an account `revision`, and pass back to prove that what
//! they saw is still current. The golden vectors in `tests.rs` were recorded on main
//! before this module owned any hasher (design S3): they pin the exact bytes.

use serde::Serialize;
use sha2::{Digest, Sha256};

/// Lowercase sha256 hex of the compact JSON of `value` (`serde_json::to_vec`, serde
/// field order). The only error is a value serde cannot serialize.
pub fn of(value: &impl Serialize) -> serde_json::Result<String> {
    Ok(hex(serde_json::to_vec(value)?))
}
/// Lowercase sha256 hex of raw bytes (contract 1.73.6 `meter.upstream_identity`).
pub fn hex(bytes: impl AsRef<[u8]>) -> String {
    format!("{:x}", Sha256::digest(bytes))
}

#[cfg(test)]
mod tests;
