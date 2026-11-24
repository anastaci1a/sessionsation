//! How each provider compacts (contract 1.64.0, SM6), and whether there is anything to
//! compact. The journal and the procedures live in [`crate::messaging::compaction`].
//!
//! - Codex: `thread/compact/start` on the account app-server at the next idle point.
//! - Claude (terminal sessions): `/compact` typed into the verified HERDR pane at the next
//!   idle point with an empty prompt; done when Claude reports the compaction (its
//!   `PostCompact` hook event or the transcript's `compact_boundary`).
//! - Antigravity and Gemini: unsupported, with the reason.
//!
//! Before anything is started, the provider's own history file answers whether there is
//! anything to compact: a conversation with no instruction or reply yet is
//! `nothing_to_compact`; one whose last compaction is newer than its last instruction and
//! reply is `already_compacted`. Both complete at once without touching the session.
use super::{ProviderId, conversation};
use serde_json::Value;
use std::path::Path;

/// The refusal code of an unsupported provider (the error envelope adds `supported:false`).
pub const UNSUPPORTED_CODE: &str = "UNSUPPORTED_PROVIDER_OPERATION";
pub const ANTIGRAVITY_UNSUPPORTED: &str = "the Antigravity CLI has no compaction command Sessionsation can start or verify (agy records no compaction step)";

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Method {
    /// `thread/compact/start` on the account app-server.
    AppServer,
    /// `/compact` typed into the session's terminal.
    Terminal,
    Unsupported(&'static str),
}
impl Method {
    pub fn name(self) -> Option<&'static str> {
        match self {
            Self::AppServer => Some("app_server"),
            Self::Terminal => Some("terminal"),
            Self::Unsupported(_) => None,
        }
    }
    pub fn native(self) -> Option<&'static str> {
        match self {
            Self::AppServer => Some("thread/compact/start"),
            Self::Terminal => Some("/compact"),
            Self::Unsupported(_) => None,
        }
    }
    /// Whether new native input is held while the compaction is pending (Codex only: a
    /// terminal session queues typed input itself).
    pub fn holds_input(self) -> bool {
        self == Self::AppServer
    }
}

pub fn method(provider: ProviderId) -> Method {
    match provider {
        ProviderId::Codex => Method::AppServer,
        ProviderId::Claude => Method::Terminal,
        ProviderId::Antigravity => Method::Unsupported(ANTIGRAVITY_UNSUPPORTED),
        ProviderId::Gemini => Method::Unsupported(conversation::GEMINI_UNSUPPORTED),
    }
}
/// [`method`] by stored provider name (records written before 1.64.0 are Codex).
pub fn method_of(provider: &str) -> Method {
    ProviderId::parse(provider).map_or(Method::AppServer, method)
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Already {
    NothingToCompact,
    AlreadyCompacted(i64),
}
impl Already {
    pub fn reason(self) -> &'static str {
        match self {
            Self::NothingToCompact => "nothing_to_compact",
            Self::AlreadyCompacted(_) => "already_compacted",
        }
    }
}

/// What the conversation block says about compacting now (None: compact).
pub fn already_from(block: &Value) -> Option<Already> {
    if block["supported"] != true || block["source"].is_null() {
        // No history file (or an unreadable one): the provider decides.
        return None;
    }
    let at = |k: &str| block[k]["at"].as_i64();
    let instruction = block["last_instruction"].is_object();
    let reply = block["last_reply"].is_object();
    if !instruction && !reply {
        return Some(Already::NothingToCompact);
    }
    let compacted = block["compacted_at"].as_i64()?;
    let newest = at("last_instruction").max(at("last_reply"))?;
    (compacted >= newest).then_some(Already::AlreadyCompacted(compacted))
}
/// [`already_from`] for one conversation, from the provider's own history file.
pub fn already(provider: ProviderId, home: &Path, conversation_id: &str) -> Option<Already> {
    let file = conversation::history_file(provider, home, conversation_id)?;
    already_from(&conversation::observe(provider, Some(&file), None))
}
/// Unix seconds of the conversation's newest compaction in its history file.
pub fn compacted_at(provider: ProviderId, home: &Path, conversation_id: &str) -> Option<i64> {
    let file = conversation::history_file(provider, home, conversation_id)?;
    conversation::observe(provider, Some(&file), None)["compacted_at"].as_i64()
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;
    use std::fs;

    #[test]
    fn methods_and_nothing_or_already_compacted() {
        assert_eq!(method(ProviderId::Codex).name(), Some("app_server"));
        assert_eq!(method(ProviderId::Claude).native(), Some("/compact"));
        assert!(matches!(
            method(ProviderId::Gemini),
            Method::Unsupported(r) if r == conversation::GEMINI_UNSUPPORTED
        ));
        assert!(matches!(
            method(ProviderId::Antigravity),
            Method::Unsupported(_)
        ));
        assert_eq!(method_of("no-such"), Method::AppServer);
        let d = tempfile::tempdir().unwrap();
        let write = |lines: &[Value]| {
            let p = d.path().join("t.jsonl");
            fs::write(
                &p,
                lines
                    .iter()
                    .map(|l| l.to_string() + "\n")
                    .collect::<String>(),
            )
            .unwrap();
            conversation::observe(ProviderId::Claude, Some(&p), None)
        };
        let user =
            |t: &str| json!({"type":"user","timestamp":t,"message":{"role":"user","content":"hi"}});
        let reply = |t: &str| json!({"type":"assistant","timestamp":t,"message":{"role":"assistant","content":[{"type":"text","text":"ok"}]}});
        let boundary =
            |t: &str| json!({"type":"system","subtype":"compact_boundary","timestamp":t});
        assert_eq!(
            already_from(&write(&[json!({"type":"summary"})])),
            Some(Already::NothingToCompact)
        );
        assert_eq!(
            already_from(&write(&[
                user("2026-11-23T10:00:00Z"),
                reply("2026-11-23T10:00:05Z")
            ])),
            None
        );
        assert!(matches!(
            already_from(&write(&[
                user("2026-11-23T10:00:00Z"),
                reply("2026-11-23T10:00:05Z"),
                boundary("2026-11-23T10:01:00Z")
            ])),
            Some(Already::AlreadyCompacted(_))
        ));
        assert_eq!(
            already_from(&write(&[
                boundary("2026-11-23T10:01:00Z"),
                user("2026-11-23T10:02:00Z")
            ])),
            None
        );
        // No history file: nothing is decided here.
        assert_eq!(
            already_from(&conversation::observe(ProviderId::Claude, None, None)),
            None
        );
    }
}
