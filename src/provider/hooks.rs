//! Hook installation and health per account and session (contract 1.64.0, SM5).
//!
//! - Claude: Sessionsation's async command hooks in the account's settings (see
//!   [`super::claude::hooks`]), installed at every Claude launch and by `api hooks install`.
//! - Codex: nothing is installed. Its events come from the supervisor's turn observations;
//!   Codex has one `notify` command slot, which belongs to the user, and its own hooks need
//!   per-hook trust, so Sessionsation writes neither.
//! - Antigravity: nothing to install; events come from the supervisor's observations.
//! - Gemini: unsupported (its CLI no longer signs in individual accounts).
use super::ProviderId;
use anyhow::{Result, bail};
use serde_json::{Value, json};

pub const CODEX_METHOD: &str = "no hook is installed: the supervisor observes Codex turns (rollout and app-server), and the Codex notify slot and hooks stay the user's";
pub const ANTIGRAVITY_METHOD: &str =
    "no hook is installed: the supervisor observes the agy step log of running sessions";

/// `(method, required)` of a provider's events.
fn method(provider: ProviderId) -> Option<(&'static str, bool)> {
    match provider {
        ProviderId::Claude => Some(("settings_hooks", true)),
        ProviderId::Codex | ProviderId::Antigravity => Some(("supervisor", false)),
        ProviderId::Gemini => None,
    }
}

fn claude_row(paths: &crate::paths::Paths, name: &str, install: bool) -> Value {
    let base = json!({"account_id":format!("claude:{name}"),"provider":"claude","supported":true,
        "method":"settings_hooks","required":true,"events":super::claude::hooks::EVENTS.map(|(e, _)| e)});
    let attempt = || -> Result<Value> {
        let home = super::claude::account(paths, name)?.home;
        let command = super::claude::hooks::command(&super::claude::hooks::executable()?);
        let changed = if install {
            Some(super::claude::hooks::install(&home, &command)?)
        } else {
            None
        };
        let h = super::claude::hooks::status(&home, &command)?;
        let mut row = base.clone();
        row["installed"] = json!(h.installed());
        row["missing_events"] = json!(h.missing);
        row["stale_events"] = json!(h.stale);
        row["user_hooks"] = json!(h.user_hooks);
        row["last_event_at"] = json!(super::events::last_hook_at(paths, &home));
        row["reason"] = if h.installed() {
            Value::Null
        } else {
            json!(
                "not installed yet: the next launch on this account or api hooks install adds them"
            )
        };
        if let Some(changed) = changed {
            row["changed"] = json!(changed);
            row["already_installed"] = json!(!changed);
        }
        Ok(row)
    };
    attempt().unwrap_or_else(|e| {
        let mut row = base.clone();
        row["installed"] = json!(false);
        row["missing_events"] = Value::Null;
        row["stale_events"] = Value::Null;
        row["user_hooks"] = Value::Null;
        row["last_event_at"] = Value::Null;
        row["reason"] = json!(format!("{e:#}").chars().take(300).collect::<String>());
        if install {
            row["changed"] = json!(false);
            row["already_installed"] = json!(false);
        }
        row
    })
}

fn row(paths: &crate::paths::Paths, provider: ProviderId, name: &str, install: bool) -> Value {
    let id = format!("{}:{name}", provider.as_str());
    match (provider, method(provider)) {
        (ProviderId::Claude, _) => claude_row(paths, name, install),
        (_, Some((method, required))) => {
            let mut r = json!({"account_id":id,"provider":provider.as_str(),"supported":true,
                "method":method,"required":required,"events":Value::Null,"installed":true,
                "missing_events":[],"stale_events":[],"user_hooks":Value::Null,"last_event_at":Value::Null,
                "reason":if provider == ProviderId::Codex {CODEX_METHOD} else {ANTIGRAVITY_METHOD}});
            if install {
                r["changed"] = json!(false);
                r["already_installed"] = json!(true);
            }
            r
        }
        (_, None) => {
            let mut r = json!({"account_id":id,"provider":provider.as_str(),"supported":false,
                "method":Value::Null,"required":false,"events":Value::Null,"installed":false,
                "missing_events":Value::Null,"stale_events":Value::Null,"user_hooks":Value::Null,
                "last_event_at":Value::Null,"reason":super::conversation::GEMINI_UNSUPPORTED});
            if install {
                r["changed"] = json!(false);
                r["already_installed"] = json!(false);
            }
            r
        }
    }
}

/// Per-session hook health: what the hooks reported for each Claude conversation.
fn sessions(paths: &crate::paths::Paths) -> Vec<Value> {
    super::events::tasks(paths)
        .into_iter()
        .map(|(conversation, t)| {
            json!({"session_id":crate::catalog::id_for_conversation(paths, ProviderId::Claude, &conversation),
                "provider":"claude","provider_conversation_id":conversation,
                "process_started_at":t.process_started_at,"last_event_at":t.last_event_at,"ended":t.ended,
                "subagents_running":t.subagents.len()})
        })
        .collect()
}

/// `api hooks` (`install` false) or `api hooks install`: one row per account, plus sessions.
pub fn report(paths: &crate::paths::Paths, account: Option<&str>, install: bool) -> Result<Value> {
    let _ = super::events::ingest(paths);
    let enrolled: Vec<(ProviderId, String)> =
        if paths.root.join(crate::layout::REGISTRY_DB).exists() {
            crate::store::load_state_snapshot(paths)?
                .accounts
                .values()
                .map(|a| (a.provider, a.name.clone()))
                .collect()
        } else {
            vec![]
        };
    let selected: Vec<&(ProviderId, String)> = match account {
        None => enrolled.iter().collect(),
        Some(id) => {
            let r = super::resolve(enrolled.iter().map(|(p, n)| (n.as_str(), *p)), id)?;
            let found: Vec<_> = enrolled
                .iter()
                .filter(|(p, n)| *p == r.provider && *n == r.name)
                .collect();
            if found.is_empty() {
                bail!("ACCOUNT_NOT_FOUND: {id}");
            }
            found
        }
    };
    let accounts: Vec<Value> = selected
        .iter()
        .map(|(p, n)| row(paths, *p, n, install))
        .collect();
    Ok(json!({"accounts":accounts,"sessions":sessions(paths),"installed":install}))
}

pub fn capability() -> Value {
    json!({"since_contract":"1.64.0","command":"hooks","install":"hooks install","args":["--account"],
        "account_keys":["account_id","provider","supported","method","required","events","installed","missing_events","stale_events","user_hooks","last_event_at","reason"],
        "install_keys":["changed","already_installed"],
        "session_keys":["session_id","provider","provider_conversation_id","process_started_at","last_event_at","ended","subagents_running"],
        "providers":{
            "claude":{"supported":true,"method":"settings_hooks","events":super::claude::hooks::EVENTS.map(|(e, _)| e),
                "entry":{"type":"command","async":true,"timeout":super::claude::hooks::TIMEOUT_SECONDS,"command_marker":super::claude::hooks::MARKER},
                "installed_at":["every Claude launch (best effort; SESSIONSATION_CLAUDE_HOOKS=0 skips)","api hooks install"],
                "user_hooks":"kept unchanged; only entries whose command carries the marker are Sessionsation's",
                "idempotent":"an installed file is not rewritten (already_installed:true)",
                "refused":["SETTINGS_IS_SYMLINK","SETTINGS_INVALID","SETTINGS_TOO_LARGE"],
                "applies_to":"Claude processes started after the install"},
            "codex":{"supported":true,"method":"supervisor","installs":false,"reason":CODEX_METHOD},
            "antigravity":{"supported":true,"method":"supervisor","installs":false,"reason":ANTIGRAVITY_METHOD},
            "gemini":{"supported":false,"reason":super::conversation::GEMINI_UNSUPPORTED}}})
}
