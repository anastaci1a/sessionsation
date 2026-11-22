//! Contract 1.58.0: per-session Codex input-delivery health and the bounded native transport
//! error a failed submit leaves on its receipt.
//!
//! One tiny file per conversation (`messaging/input-health/<conversation>.json`), written by the
//! submit path under the session's mutation lock and read by `api state` without any provider
//! call or ledger lock. Holds no message text, only outcomes, counters and the bounded error.
use super::{Fault, Paths};
use crate::paths::atomic_write;
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use std::fs;

/// The newest error of a native transport, as shown on a receipt (`facts.native_error`) and in the
/// session health. `message` is a fixed bounded sentence per kind (never provider text).
#[derive(Clone, Debug, Serialize, Deserialize, PartialEq)]
pub struct NativeError {
    pub kind: String,
    pub message: String,
    pub at: i64,
}

/// Transport-level failure kinds (a native-protocol or local-policy refusal is not one).
pub(super) fn describe(kind: &str) -> Option<&'static str> {
    Some(match kind {
        "endpoint_unavailable" => {
            "the app-server socket is missing, refused the connection or failed the handshake; nothing was sent"
        }
        "disconnected" => "the app-server connection closed before it answered the request",
        "timeout" => "the app-server did not answer within the send budget",
        "native_rejected" => "the app-server answered the request with a JSON-RPC error",
        "malformed_response" => "the app-server answered with a frame that could not be understood",
        "native_frame_limit" => "the app-server frame exceeded the size limit",
        "unsafe_endpoint" => "the app-server socket failed the ownership and permission checks",
        _ => return None,
    })
}

pub(super) fn native_error(fault: Fault, at: i64) -> Option<NativeError> {
    describe(fault.0).map(|m| NativeError {
        kind: fault.0.into(),
        message: m.into(),
        at,
    })
}

#[derive(Clone, Debug, Default, Serialize, Deserialize, PartialEq)]
pub struct Health {
    /// `accepted` | `not_delivered` | `uncertain`
    #[serde(default)]
    pub last_outcome: Option<String>,
    #[serde(default)]
    pub last_attempt_at: Option<i64>,
    #[serde(default)]
    pub last_success_at: Option<i64>,
    #[serde(default)]
    pub consecutive_failures: u32,
    #[serde(default)]
    pub last_native_error: Option<NativeError>,
}

fn file(paths: &Paths, conversation: &str) -> Option<std::path::PathBuf> {
    (!conversation.is_empty()
        && conversation.len() <= 128
        && conversation
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || matches!(b, b'-' | b'_')))
    .then(|| {
        paths
            .root
            .join(crate::layout::INPUT_HEALTH)
            .join(format!("{conversation}.json"))
    })
}

pub fn read(paths: &Paths, conversation: &str) -> Health {
    file(paths, conversation)
        .and_then(|f| fs::read(f).ok())
        .and_then(|b| serde_json::from_slice(&b).ok())
        .unwrap_or_default()
}

/// Record one submit outcome. Best effort: a health write never changes a delivery result.
pub(super) fn record(
    paths: &Paths,
    conversation: &str,
    outcome: &str,
    error: Option<NativeError>,
    now: i64,
) {
    let Some(path) = file(paths, conversation) else {
        return;
    };
    let mut h = read(paths, conversation);
    h.last_outcome = Some(outcome.into());
    h.last_attempt_at = Some(now);
    if outcome == "accepted" {
        h.consecutive_failures = 0;
        h.last_success_at = Some(now);
    } else {
        h.consecutive_failures = h.consecutive_failures.saturating_add(1);
        if error.is_some() {
            h.last_native_error = error;
        }
    }
    if let Ok(bytes) = serde_json::to_vec(&h) {
        let _ = atomic_write(&path, &bytes);
    }
}

/// `input_delivery.health` of a Codex session row.
pub fn public(paths: &Paths, conversation: &str) -> Value {
    let h = read(paths, conversation);
    json!({"since_revision":58,"last_outcome":h.last_outcome,"last_attempt_at":h.last_attempt_at,
        "last_success_at":h.last_success_at,"consecutive_failures":h.consecutive_failures,
        "last_native_error":h.last_native_error})
}
