//! Contract 1.63.0 shapes shared by every provider's tests: the `api stop` result keys and
//! the `api observe` `conversation` block keys.
#![allow(dead_code)]
use serde_json::Value;

pub const STOP_KEYS: [&str; 17] = [
    "id",
    "kind",
    "session_id",
    "source_session_id",
    "provider",
    "phase",
    "detail",
    "already_stopped",
    "pid",
    "process_start",
    "signal",
    "pane_closed",
    "pane_close_reason",
    "interrupted_turn",
    "auto_start",
    "provider_conversation_id",
    "source_account_id",
];

pub const CONVERSATION_KEYS: [&str; 16] = [
    "compacted_at",
    "compacted_at_reason",
    "compacted_at_source",
    "history_turns",
    "last_instruction",
    "last_instruction_reason",
    "last_instruction_source",
    "last_reply",
    "last_reply_reason",
    "last_reply_source",
    "reason",
    "recent_turns",
    "recent_turns_reason",
    "recent_turns_source",
    "source",
    "supported",
];

/// Every shared stop key is present, both session ids name `session`, and the provider is set.
pub fn assert_stop_shape(v: &Value, session: &str, provider: &str) {
    for k in STOP_KEYS {
        assert!(v.get(k).is_some(), "stop result lacks {k}: {v}");
    }
    assert_eq!(v["session_id"], session, "{v}");
    assert_eq!(v["source_session_id"], session, "{v}");
    assert_eq!(v["provider"], provider, "{v}");
    assert!(v["already_stopped"].is_boolean(), "{v}");
}

/// The observe `conversation` block has exactly the shared keys; every field has a source
/// when known and a reason when not.
pub fn assert_conversation_shape(observed: &Value) {
    let c = &observed["conversation"];
    let mut keys: Vec<&str> = c
        .as_object()
        .unwrap_or_else(|| panic!("no conversation block: {observed}"))
        .keys()
        .map(String::as_str)
        .collect();
    keys.sort_unstable();
    assert_eq!(keys, CONVERSATION_KEYS, "{c}");
    for f in [
        "last_reply",
        "last_instruction",
        "compacted_at",
        "recent_turns",
    ] {
        let known = !c[f].is_null();
        assert_eq!(c[format!("{f}_source")].is_string(), known, "{f}: {c}");
        assert_eq!(c[format!("{f}_reason")].is_string(), !known, "{f}: {c}");
    }
}

/// Contract 1.64.0 (SM1): the one live `api send` request every provider's tests send. It
/// carries a Codex-only option (`if_busy`) and a terminal/peer worker option
/// (`timeout_seconds`), so each provider ignores exactly one of them.
pub fn unified_send(session: &Value, mode: &str, text: &str, expires_at: i64) -> Value {
    serde_json::json!({"api_version":1,"message_id":uuid::Uuid::new_v4().to_string(),"mode":mode,
        "session_id":session["id"],"expected_generation":session["generation"],"text":text,
        "expires_at":expires_at,"origin":"operator","delivery":"auto","if_busy":"queue","timeout_seconds":120})
}

pub const DELIVERY_KEYS: [&str; 7] = [
    "fallback_reason",
    "framing",
    "held",
    "intent",
    "mode_requested",
    "mode_used",
    "queued",
];

/// The uniform delivery block (and `ignored_options`) of a result or receipt.
pub fn assert_delivery(v: &Value, requested: &str, used: &str, framing: &str, ignored: &[&str]) {
    let d = &v["delivery"];
    let mut keys: Vec<&str> = d
        .as_object()
        .unwrap_or_else(|| panic!("no delivery block: {v}"))
        .keys()
        .map(String::as_str)
        .collect();
    keys.sort_unstable();
    assert_eq!(keys, DELIVERY_KEYS, "{d}");
    assert_eq!(d["mode_requested"], requested, "{v}");
    assert_eq!(d["mode_used"], used, "{v}");
    assert_eq!(d["framing"], framing, "{v}");
    assert!(d["queued"].is_boolean() && d["held"].is_boolean(), "{d}");
    assert_eq!(v["ignored_options"], serde_json::json!(ignored), "{v}");
    assert_eq!(v["ignored_reason"].is_string(), !ignored.is_empty(), "{v}");
}
