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
