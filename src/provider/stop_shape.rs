//! One `api stop` result shape for every provider (contract 1.63.0, SM2 / P1).
//!
//! Every result (Codex terminate job, Claude and Antigravity SIGTERM, the idempotent
//! `already_stopped` answer) carries [`KEYS`]. A key the provider path did not report is
//! `null`; nothing is invented. `session_id` and `source_session_id` both name the catalog
//! session for one minor version (the 1.55.0 `already_stopped` answer used `session_id`, every
//! other stop result `source_session_id`). Existing keys keep their meaning.
use serde_json::{Value, json};

/// The shared keys, in contract order.
pub const KEYS: [&str; 17] = [
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

/// Fill the shared keys of one stop result for catalog session `s`.
pub fn shape(mut v: Value, s: &crate::catalog::Session) -> Value {
    if !v.is_object() {
        return v;
    }
    v["session_id"] = json!(s.id);
    v["source_session_id"] = json!(s.id);
    v["provider"] = json!(s.provider);
    if v.get("kind").is_none_or(Value::is_null) {
        v["kind"] = json!("terminate");
    }
    if v.get("provider_conversation_id").is_none_or(Value::is_null) {
        v["provider_conversation_id"] = json!(s.conversation_id);
    }
    if v.get("source_account_id").is_none_or(Value::is_null) {
        v["source_account_id"] = json!(format!("{}:{}", s.provider, s.account));
    }
    let already = v["already_stopped"] == true;
    v["already_stopped"] = json!(already);
    if already {
        // Nothing was signalled and nothing was interrupted.
        if v.get("interrupted_turn").is_none_or(Value::is_null) {
            v["interrupted_turn"] = json!(false);
        }
        if v.get("pane_closed").is_none_or(Value::is_null) {
            v["pane_closed"] = json!(false);
        }
        if v.get("detail").is_none_or(Value::is_null) {
            v["detail"] = json!("The session has no running process; nothing was stopped.");
        }
    }
    for k in KEYS {
        if v.get(k).is_none() {
            v[k] = Value::Null;
        }
    }
    v
}

/// Why a provider has no stop at all (Gemini, contract 1.63.0), if so.
pub fn unsupported(provider: &str) -> Option<&'static str> {
    match super::ProviderId::parse(provider) {
        Ok(super::ProviderId::Gemini) => Some(super::conversation::GEMINI_UNSUPPORTED),
        _ => None,
    }
}

/// A refusal for a provider that cannot perform the operation at all: the error envelope gets
/// `supported:false` and `reason` (contract 1.63.0).
#[derive(Debug)]
pub struct Unsupported(pub &'static str);
impl std::fmt::Display for Unsupported {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "UNSUPPORTED_PROVIDER_OPERATION: {}", self.0)
    }
}
impl std::error::Error for Unsupported {}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn every_result_gets_the_shared_keys_and_keeps_its_own() {
        let s = crate::catalog::Session {
            id: "s1".into(),
            name: "n".into(),
            provider: "claude".into(),
            account: "a".into(),
            conversation_id: "c1".into(),
            revision: 1,
            attachment: None,
            directory: None,
            previous_conversation_ids: vec![],
            space: None,
        };
        let already = shape(
            json!({"session_id":"s1","already_stopped":true,"phase":"complete"}),
            &s,
        );
        let done = shape(
            json!({"id":"x","source_session_id":"s1","kind":"terminate","phase":"complete","pid":7,"signal":"SIGTERM","extra":1}),
            &s,
        );
        for v in [&already, &done] {
            for k in KEYS {
                assert!(v.get(k).is_some(), "{k} missing in {v}");
            }
            assert_eq!(v["session_id"], "s1");
            assert_eq!(v["source_session_id"], "s1");
            assert_eq!(v["provider"], "claude");
            assert_eq!(v["source_account_id"], "claude:a");
        }
        assert_eq!(already["already_stopped"], true);
        assert_eq!(already["interrupted_turn"], false);
        assert!(already["pid"].is_null());
        assert_eq!(done["already_stopped"], false);
        assert_eq!(done["pid"], 7);
        assert_eq!(done["extra"], 1);
        assert!(done["auto_start"].is_null());
    }
}
