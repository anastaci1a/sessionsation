//! Session model and reasoning effort (contract 1.49.0).
//!
//! Exact model ids only: a Claude Code alias (`fable`, `opus`, ...) can resolve to an older
//! release, so nothing here accepts or trusts one. The model and effort a session runs with are
//! observed, never assumed from a request:
//!
//! - Claude: Sessionsation's own status-line hook (`provider-hook claude-statusline`, routed
//!   through every managed start) receives Claude Code's status-line payload, whose
//!   `model.id` is the session's runtime main-loop model and `effort.level` its effort. The hook
//!   records both per conversation in `<account home>/.sessionsation/model/<session_id>.json`.
//!   The rendered footer is the account's own script output and is never parsed.
//! - Codex: `thread/start`, `thread/resume` and `thread/read` results carry `model` and
//!   `reasoningEffort`; observed values go to the sidecar `registry/session-catalog/models.json`.
//!
//! Neither file is part of the catalog row, so a model change never changes a session
//! generation, and an older release ignores both files.
use crate::{
    paths::{Paths, atomic_write, lock_file, private_dir},
    provider::ProviderId,
};
use anyhow::{Result, ensure};
use fs2::FileExt;
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use std::{
    collections::BTreeMap,
    fs,
    path::{Path, PathBuf},
};

/// Claude Code's own `--effort` / `/effort` choices.
pub const CLAUDE_EFFORTS: [&str; 5] = ["low", "medium", "high", "xhigh", "max"];
/// Sidecar of observed values, next to the catalog (never inside it).
pub const MODELS_FILE: &str = "models.json";
/// The hook rewrites an unchanged record at most this often (streaming sessions run it a lot).
const HOOK_REFRESH_MS: i64 = 60_000;
/// Hook records of conversations not seen for this long are removed by the hook's sweep.
const HOOK_RECORD_DAYS: i64 = 30;

/// A Claude model id: `claude-` then lower-case letters, digits, dots and dashes, optionally
/// with the `[1m]` context suffix. Aliases never match.
pub fn validate_claude_model(model: &str) -> Result<()> {
    let base = model.strip_suffix("[1m]").unwrap_or(model);
    let rest = base.strip_prefix("claude-").unwrap_or_default();
    ensure!(
        (2..=81).contains(&rest.len())
            && rest
                .chars()
                .next()
                .is_some_and(|c| c.is_ascii_lowercase() || c.is_ascii_digit())
            && rest
                .chars()
                .all(|c| c.is_ascii_lowercase() || c.is_ascii_digit() || matches!(c, '.' | '-')),
        "INVALID_MODEL: Claude needs an exact model id such as claude-fable-5-2 (aliases like fable or opus are refused)"
    );
    Ok(())
}

/// Model and effort grammar for `provider`. Codex keeps the launch request rules.
pub fn validate(provider: ProviderId, model: &str, effort: Option<&str>) -> Result<()> {
    match provider {
        ProviderId::Claude => {
            validate_claude_model(model)?;
            if let Some(e) = effort {
                ensure!(
                    CLAUDE_EFFORTS.contains(&e),
                    "INVALID_REASONING_EFFORT: Claude efforts are low, medium, high, xhigh, max"
                );
            }
        }
        ProviderId::Codex => {
            crate::launch::validate_model(model)?;
            if let Some(e) = effort {
                ensure!(
                    crate::launch::REASONING_EFFORTS.contains(&e),
                    "INVALID_REASONING_EFFORT"
                );
            }
        }
        _ => anyhow::bail!(
            "UNSUPPORTED_PROVIDER_OPERATION: model selection supports codex and claude sessions"
        ),
    }
    Ok(())
}

/// Observed values compare after ASCII lower-casing, nothing else.
pub fn same(a: &str, b: &str) -> bool {
    a.eq_ignore_ascii_case(b)
}
/// Model ids compare like [`same`], ignoring a trailing `[1m]` context suffix on either side:
/// Claude Code reports its main-loop model setting in the status line, and whether that keeps
/// the suffix is not part of any documented contract.
pub fn same_model(a: &str, b: &str) -> bool {
    let base = |s: &str| s.strip_suffix("[1m]").unwrap_or(s).to_ascii_lowercase();
    base(a) == base(b)
}

// ---------------------------------------------------------------- Claude hook record

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct HookRecord {
    pub schema_version: u32,
    pub session_id: String,
    pub model_id: Option<String>,
    #[serde(default)]
    pub model_display_name: Option<String>,
    #[serde(default)]
    pub effort_level: Option<String>,
    #[serde(default)]
    pub thinking_enabled: Option<bool>,
    #[serde(default)]
    pub fast_mode: Option<bool>,
    #[serde(default)]
    pub claude_version: Option<String>,
    pub observed_at_ms: i64,
    pub changed_at_ms: i64,
}

fn hook_dir(home: &Path) -> PathBuf {
    home.join(".sessionsation/model")
}
/// The record of one conversation; None for anything that is not a UUID.
pub fn hook_file(home: &Path, session_id: &str) -> Option<PathBuf> {
    uuid::Uuid::parse_str(session_id)
        .ok()
        .filter(|u| u.to_string() == session_id)
        .map(|_| hook_dir(home).join(format!("{session_id}.json")))
}
pub fn hook_record(home: &Path, session_id: &str) -> Option<HookRecord> {
    let bytes = fs::read(hook_file(home, session_id)?).ok()?;
    serde_json::from_slice(&bytes).ok()
}

/// Record the model facts of one status-line payload. Returns whether the file was written.
/// Never called with anything but the payload Claude Code passed to the status line.
pub fn record_hook(home: &Path, input: &Value, now_ms: i64) -> Result<bool> {
    let Some(session_id) = input["session_id"].as_str() else {
        return Ok(false);
    };
    let Some(file) = hook_file(home, session_id) else {
        return Ok(false);
    };
    let Some(model) = input["model"]["id"].as_str().filter(|m| !m.is_empty()) else {
        // A payload without a model keeps the previous record.
        return Ok(false);
    };
    let text = |v: &Value| v.as_str().map(|s| s.chars().take(200).collect::<String>());
    let mut fresh = HookRecord {
        schema_version: 1,
        session_id: session_id.to_owned(),
        model_id: Some(model.chars().take(200).collect()),
        model_display_name: text(&input["model"]["display_name"]),
        effort_level: text(&input["effort"]["level"]),
        thinking_enabled: input["thinking"]["enabled"].as_bool(),
        fast_mode: input["fast_mode"].as_bool(),
        claude_version: text(&input["version"]),
        observed_at_ms: now_ms,
        changed_at_ms: now_ms,
    };
    if let Some(old) = fs::read(&file)
        .ok()
        .and_then(|b| serde_json::from_slice::<HookRecord>(&b).ok())
    {
        let unchanged = old.model_id == fresh.model_id
            && old.effort_level == fresh.effort_level
            && old.thinking_enabled == fresh.thinking_enabled
            && old.fast_mode == fresh.fast_mode;
        if unchanged {
            if now_ms - old.observed_at_ms < HOOK_REFRESH_MS && now_ms >= old.observed_at_ms {
                return Ok(false);
            }
            fresh.changed_at_ms = old.changed_at_ms;
        }
    }
    private_dir(&hook_dir(home))?;
    sweep_hook_records(&hook_dir(home), now_ms / 1000);
    atomic_write(&file, &serde_json::to_vec(&fresh)?)?;
    Ok(true)
}
fn sweep_hook_records(dir: &Path, now: i64) {
    crate::provider::claude::sweep_stale_tmp(dir, now);
    let Ok(entries) = fs::read_dir(dir) else {
        return;
    };
    for entry in entries.flatten() {
        let old = entry
            .metadata()
            .ok()
            .filter(|m| m.is_file())
            .and_then(|m| m.modified().ok())
            .and_then(|t| t.duration_since(std::time::UNIX_EPOCH).ok())
            .is_some_and(|t| now - t.as_secs() as i64 > HOOK_RECORD_DAYS * 86400);
        if old && entry.file_name().to_string_lossy().ends_with(".json") {
            let _ = fs::remove_file(entry.path());
        }
    }
}

// ---------------------------------------------------------------- observed state

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct Observed {
    pub model: String,
    pub effort: Option<String>,
    /// Unix seconds.
    pub observed_at: i64,
    /// `claude_statusline` | `codex_thread` | `set_model`
    pub source: String,
}
impl Observed {
    pub fn public(&self) -> Value {
        json!({"id":self.model,"effort":self.effort,"observed_at":self.observed_at,"source":self.source})
    }
    pub fn from_hook(r: &HookRecord) -> Option<Self> {
        Some(Self {
            model: r.model_id.clone()?,
            effort: r.effort_level.clone(),
            observed_at: r.observed_at_ms / 1000,
            source: "claude_statusline".into(),
        })
    }
}

#[derive(Default, Serialize, Deserialize)]
struct Sidecar {
    schema_version: u32,
    /// Keyed by provider conversation id.
    sessions: BTreeMap<String, Observed>,
}
fn sidecar_file(paths: &Paths) -> PathBuf {
    paths
        .root
        .join(crate::layout::SESSION_CATALOG)
        .join(MODELS_FILE)
}
fn load(paths: &Paths) -> Sidecar {
    fs::read(sidecar_file(paths))
        .ok()
        .and_then(|b| serde_json::from_slice(&b).ok())
        .unwrap_or_default()
}
/// Store an observed value of one conversation; written only when it changed.
pub fn store(paths: &Paths, conversation: &str, observed: Observed) -> Result<()> {
    let dir = paths.root.join(crate::layout::SESSION_CATALOG);
    private_dir(&dir)?;
    let lock = lock_file(&dir.join("models.lock"))?;
    lock.lock_exclusive()?;
    let mut all = load(paths);
    let changed = all.sessions.get(conversation).is_none_or(|old| {
        old.model != observed.model
            || old.effort != observed.effort
            || old.source != observed.source
            || observed.observed_at - old.observed_at > 600
    });
    if !changed {
        return Ok(());
    }
    all.schema_version = 1;
    all.sessions.insert(conversation.to_owned(), observed);
    if all.sessions.len() > 4096 {
        // Bounded: the oldest observations go first.
        let mut by_age: Vec<(i64, String)> = all
            .sessions
            .iter()
            .map(|(k, v)| (v.observed_at, k.clone()))
            .collect();
        by_age.sort();
        for (_, k) in by_age.into_iter().take(all.sessions.len() - 4096) {
            all.sessions.remove(&k);
        }
    }
    atomic_write(&sidecar_file(paths), &serde_json::to_vec_pretty(&all)?)
}
/// Codex threads read by the inventory (`thread/read` values): keep their model facts.
pub fn record_codex_threads(paths: &Paths, threads: &[Value]) {
    let now = crate::daemon::now();
    for t in threads {
        if let (Some(id), Some(model)) = (t["id"].as_str(), t["model"].as_str()) {
            let _ = store(
                paths,
                id,
                Observed {
                    model: model.to_owned(),
                    effort: t["reasoningEffort"].as_str().map(str::to_owned),
                    observed_at: now,
                    source: "codex_thread".into(),
                },
            );
        }
    }
}

/// The current value of a conversation: the newer of the Claude hook record (when `home` is
/// given) and the sidecar entry. File reads only.
pub fn current(paths: &Paths, conversation: &str, home: Option<&Path>) -> Option<Observed> {
    current_in(&load(paths), conversation, home)
}
fn current_in(all: &Sidecar, conversation: &str, home: Option<&Path>) -> Option<Observed> {
    let hook = home
        .and_then(|h| hook_record(h, conversation))
        .and_then(|r| Observed::from_hook(&r));
    let side = all.sessions.get(conversation).cloned();
    match (hook, side) {
        (Some(h), Some(s)) if s.observed_at > h.observed_at => Some(s),
        (Some(h), _) => Some(h),
        (None, s) => s,
    }
}

/// Fill `model` in public session rows (`state`, `session`, `resolve`). Rows of providers
/// without model reporting get null.
pub fn annotate(paths: &Paths, rows: &mut [Value]) {
    let all = load(paths);
    let mut homes: BTreeMap<String, Option<PathBuf>> = BTreeMap::new();
    for row in rows.iter_mut() {
        let conversation = row["conversation_id"]
            .as_str()
            .unwrap_or_default()
            .to_owned();
        let provider = row["provider"].as_str().unwrap_or_default();
        let home = if provider == "claude" {
            let account = row["account"].as_str().unwrap_or_default().to_owned();
            homes
                .entry(account.clone())
                .or_insert_with(|| {
                    crate::provider::claude::account(paths, &account)
                        .ok()
                        .map(|a| a.home)
                })
                .clone()
        } else {
            None
        };
        row["model"] = match provider {
            "claude" | "codex" => {
                current_in(&all, &conversation, home.as_deref()).map_or(Value::Null, |o| o.public())
            }
            _ => Value::Null,
        };
    }
}

// ---------------------------------------------------------------- errors

/// A `--wait` that ended without a confirmed result: the error envelope carries the record.
#[derive(Debug)]
pub struct NotConfirmed {
    pub code: &'static str,
    pub message: String,
    pub operation: Value,
}
impl std::fmt::Display for NotConfirmed {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{}: {}", self.code, self.message)
    }
}
impl std::error::Error for NotConfirmed {}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn claude_ids_are_exact_and_aliases_are_refused() {
        for ok in [
            "claude-fable-5-2",
            "claude-opus-5-5",
            "claude-opus-5-5[1m]",
            "claude-haiku-4.5",
        ] {
            validate_claude_model(ok).unwrap();
        }
        for bad in [
            "fable",
            "opus",
            "sonnet",
            "haiku",
            "default",
            "opusplan",
            "best",
            "Claude-Opus-5-5",
            "claude-",
            "claude--x",
            "claude-opus 5",
            "-x",
            "claude-opus-5-5[2m]",
        ] {
            assert!(validate_claude_model(bad).is_err(), "{bad} must be refused");
        }
        validate(ProviderId::Claude, "claude-opus-5-5", Some("xhigh")).unwrap();
        assert!(validate(ProviderId::Claude, "claude-opus-5-5", Some("minimal")).is_err());
        validate(ProviderId::Codex, "gpt-6-astra", Some("minimal")).unwrap();
        assert!(validate(ProviderId::Codex, "gpt-6-astra", Some("max")).is_err());
        assert!(
            validate(ProviderId::Gemini, "x", None)
                .unwrap_err()
                .to_string()
                .starts_with("UNSUPPORTED_PROVIDER_OPERATION")
        );
    }

    #[test]
    fn hook_records_model_and_effort_and_bounds_rewrites() {
        let dir = tempfile::tempdir().unwrap();
        let home = dir.path();
        let id = "11111111-1111-4111-8111-111111111111";
        let payload = json!({"session_id":id,"version":"2.1.314","model":{"id":"claude-fable-5-2","display_name":"Fable 5.2"},
            "effort":{"level":"high"},"thinking":{"enabled":true},"fast_mode":false});
        assert!(record_hook(home, &payload, 1_000_000).unwrap());
        let r = hook_record(home, id).unwrap();
        assert_eq!(r.model_id.as_deref(), Some("claude-fable-5-2"));
        assert_eq!(r.effort_level.as_deref(), Some("high"));
        assert_eq!(r.claude_version.as_deref(), Some("2.1.314"));
        // Unchanged within a minute: not rewritten.
        assert!(!record_hook(home, &payload, 1_010_000).unwrap());
        // Unchanged after a minute: rewritten, changed_at kept.
        assert!(record_hook(home, &payload, 1_070_000).unwrap());
        let r = hook_record(home, id).unwrap();
        assert_eq!((r.observed_at_ms, r.changed_at_ms), (1_070_000, 1_000_000));
        // No effort (a model without effort support) is null.
        let mut no_effort = payload.clone();
        no_effort.as_object_mut().unwrap().remove("effort");
        assert!(record_hook(home, &no_effort, 1_080_000).unwrap());
        assert_eq!(hook_record(home, id).unwrap().effort_level, None);
        // No model: the previous record stays; a non-UUID id writes nothing.
        assert!(!record_hook(home, &json!({"session_id":id}), 1_090_000).unwrap());
        assert!(hook_record(home, id).is_some());
        assert!(
            !record_hook(
                home,
                &json!({"session_id":"../x","model":{"id":"claude-a-b"}}),
                1
            )
            .unwrap()
        );
        assert!(hook_file(home, "../x").is_none());
    }

    #[test]
    fn current_prefers_the_newer_observation_and_rows_get_the_field() {
        let dir = tempfile::tempdir().unwrap();
        let paths = Paths::new(Some(dir.path().join("root"))).unwrap();
        let home = dir.path().join("home");
        let id = "22222222-2222-4222-8222-222222222222";
        assert!(current(&paths, id, Some(&home)).is_none());
        store(
            &paths,
            id,
            Observed {
                model: "claude-opus-5-5".into(),
                effort: Some("medium".into()),
                observed_at: 100,
                source: "set_model".into(),
            },
        )
        .unwrap();
        assert_eq!(current(&paths, id, None).unwrap().model, "claude-opus-5-5");
        record_hook(
            &home,
            &json!({"session_id":id,"model":{"id":"claude-fable-5-2"},"effort":{"level":"high"}}),
            200_000,
        )
        .unwrap();
        let now = current(&paths, id, Some(&home)).unwrap();
        assert_eq!(
            (now.model.as_str(), now.source.as_str()),
            ("claude-fable-5-2", "claude_statusline")
        );
        let mut rows = vec![
            json!({"provider":"codex","conversation_id":id,"account":"a"}),
            json!({"provider":"gemini","conversation_id":id,"account":"a"}),
        ];
        annotate(&paths, &mut rows);
        assert_eq!(rows[0]["model"]["id"], "claude-opus-5-5");
        assert_eq!(rows[0]["model"]["source"], "set_model");
        assert!(rows[1]["model"].is_null());
        record_codex_threads(
            &paths,
            &[json!({"id":id,"model":"gpt-6-astra","reasoningEffort":"high"})],
        );
        let c = current(&paths, id, None).unwrap();
        assert_eq!(
            (c.model.as_str(), c.effort.as_deref()),
            ("gpt-6-astra", Some("high"))
        );
    }
}
