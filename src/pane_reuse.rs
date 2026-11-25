//! Idle-pane reuse for HERDR starts and resumes (contract 1.52.0).
//!
//! Before a start or resume opens a new tab in an existing Space it takes, in order:
//! 1. the session's own recorded pane (resumes only) while it still exists in that
//!    Space and an idle shell is all that runs in it;
//! 2. else the first tab, in HERDR order, whose label equals the new tab's name exactly,
//!    that has exactly one pane, whose pane is an idle shell and is not the attachment
//!    of another catalog session;
//! 3. else a new tab, as before.
//!
//! The idle test is the one auto-recover has always used ([`pane_idle`]). Callers
//! re-check it right before typing their command and open a new tab when it no longer
//! holds. Every uncertainty (an older HERDR without `tab list`, an unreadable catalog,
//! a failed claim) means "no reuse". The `reuse_idle_tabs` setting turns it off.
use crate::{
    catalog, daemon,
    paths::{Paths, atomic_write, lock_file, private_dir},
    terminal,
};
use anyhow::Result;
use fs2::FileExt;
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use std::{collections::BTreeMap, path::Path};

pub const RECORDED_DETAIL: &str = "Reused the session's previous pane (an idle shell).";
pub const SAME_NAME_DETAIL: &str = "Reused an idle tab with the same name.";
/// The selection order, as advertised in `capabilities.pane_reuse.order`.
pub const ORDER: [&str; 3] = [
    "recorded_pane_if_idle_shell",
    "same_name_single_pane_idle_tab_in_space",
    "new_tab",
];
/// A claim keeps a picked pane from a concurrent start this long (seconds).
const CLAIM_SECONDS: i64 = 120;

pub fn capability() -> Value {
    json!({"since_contract":"1.52.0","version":1,"order":ORDER,"setting":"reuse_idle_tabs",
        "built_in_default":crate::config::Config::default().reuse_idle_tabs,
        "applies_to":["launch start","resume-launch start","resume --space","auto_start"],
        "applies_when":"existing_space","new_space":"new_workspace_shell",
        "label_match":"exact_case_sensitive","tab_panes":1,
        "idle_test":"foreground_group_is_shell_and_single_idle_shell_process",
        "excludes":"attachment_of_another_catalog_session",
        "rechecked_before_typing":true,"recheck_failed":"new_tab",
        "keys_sent":"command_only","receipt_field":"pane_reused",
        "details":{"recorded_pane":RECORDED_DETAIL,"same_name_tab":SAME_NAME_DETAIL}})
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Kind {
    RecordedPane,
    SameNameTab,
}

/// A pane picked for reuse (HERDR ids, without the `herdr/<session>/` prefix).
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Reuse {
    pub pane: String,
    pub tab: Option<String>,
    pub workspace: String,
    pub kind: Kind,
}
impl Reuse {
    pub fn detail(&self) -> &'static str {
        match self.kind {
            Kind::RecordedPane => RECORDED_DETAIL,
            Kind::SameNameTab => SAME_NAME_DETAIL,
        }
    }
}

/// The HERDR calls a selection makes (read-only, plus a best-effort tab rename).
pub trait Host {
    fn call(&self, args: &[&str]) -> Result<Value>;
}
/// One HERDR server: by session name, or by an exact socket (launch requests).
pub struct Herdr<'a> {
    pub session: &'a str,
    pub socket: Option<&'a Path>,
}
impl Host for Herdr<'_> {
    fn call(&self, args: &[&str]) -> Result<Value> {
        terminal::call_at(self.session, args, self.socket)
    }
}

/// A pane may be reused only while it exists and an idle shell is all that runs in
/// it: the foreground process group is the shell, and a single shell process.
pub fn pane_idle(host: &impl Host, pane: &str) -> bool {
    let Ok(info) = host.call(&["pane", "process-info", "--pane", pane]) else {
        return false;
    };
    let info = &info["process_info"];
    if let Some(group) = info["foreground_process_group_id"].as_u64()
        && info["shell_pid"].as_u64() != Some(group)
    {
        return false;
    }
    terminal::process_state(info).is_ok_and(|(idle, _)| idle)
}
/// [`pane_idle`] for a `herdr/<session>/<pane>` target on the default socket.
pub fn reusable(target: &str) -> bool {
    let Ok((session, pane)) = terminal::target(target) else {
        return false;
    };
    pane_idle(
        &Herdr {
            session,
            socket: None,
        },
        pane,
    )
}

/// The operator switch (`reuse_idle_tabs`, default on). An unreadable config is "off".
pub fn enabled(paths: &Paths) -> bool {
    crate::store::load_config(paths).is_ok_and(|c| c.reuse_idle_tabs)
}

/// What to look for in one Space.
pub struct Want<'a> {
    /// The destination Space (HERDR workspace id).
    pub workspace: &'a str,
    /// The label a new tab would get.
    pub name: &'a str,
    /// The session's recorded pane id (resumes only).
    pub recorded: Option<&'a str>,
    /// Pane ids that are the attachment of another catalog session.
    pub excluded: &'a [String],
}

fn in_workspace(v: &Value, id_field: &str, workspace: &str) -> bool {
    match v["workspace_id"].as_str() {
        Some(w) => w == workspace,
        None => v[id_field]
            .as_str()
            .and_then(|id| id.split_once(':'))
            .is_some_and(|(w, _)| w == workspace),
    }
}

/// Steps 1 and 2 of the order (read-only). `None` means "open a new tab".
pub fn select(host: &impl Host, want: &Want) -> Option<Reuse> {
    let listed = host
        .call(&["pane", "list", "--workspace", want.workspace])
        .ok()?;
    let panes: Vec<&Value> = listed["panes"]
        .as_array()?
        .iter()
        .filter(|p| in_workspace(p, "pane_id", want.workspace))
        .collect();
    let usable = |pane: &str| !want.excluded.iter().any(|e| e == pane) && pane_idle(host, pane);
    let reuse = |pane: &str, tab: Option<&str>, kind| Reuse {
        pane: pane.to_owned(),
        tab: tab.map(str::to_owned),
        workspace: want.workspace.to_owned(),
        kind,
    };
    if let Some(recorded) = want.recorded
        && let Some(p) = panes.iter().find(|p| p["pane_id"] == recorded)
        && usable(recorded)
    {
        return Some(reuse(recorded, p["tab_id"].as_str(), Kind::RecordedPane));
    }
    let tabs = host
        .call(&["tab", "list", "--workspace", want.workspace])
        .ok()?;
    for tab in tabs["tabs"].as_array()? {
        if !in_workspace(tab, "tab_id", want.workspace)
            || tab["label"].as_str() != Some(want.name)
            || tab["pane_count"].as_u64().is_some_and(|n| n != 1)
        {
            continue;
        }
        let Some(id) = tab["tab_id"].as_str() else {
            continue;
        };
        let inside: Vec<&&Value> = panes.iter().filter(|p| p["tab_id"] == id).collect();
        if let [only] = inside[..]
            && let Some(pane) = only["pane_id"].as_str()
            && usable(pane)
        {
            return Some(reuse(pane, Some(id), Kind::SameNameTab));
        }
    }
    None
}

/// Pane ids in HERDR session `herdr_session` recorded as the attachment of a catalog
/// session other than `except`.
pub fn attached_panes(
    paths: &Paths,
    herdr_session: &str,
    except: Option<&str>,
) -> Result<Vec<String>> {
    Ok(catalog::list(paths)?
        .into_iter()
        .filter(|s| except != Some(s.id.as_str()))
        .filter_map(|s| s.attachment)
        .filter_map(|a| {
            terminal::target(&a.target)
                .ok()
                .filter(|(session, _)| *session == herdr_session)
                .map(|(_, pane)| pane.to_owned())
        })
        .collect())
}

/// The session's recorded pane in `herdr_session`: its catalog attachment, else the
/// pane auto-start last saw it in.
pub fn recorded_pane(paths: &Paths, session_id: &str, herdr_session: &str) -> Option<String> {
    let attached = catalog::find(paths, session_id)
        .ok()
        .and_then(|s| s.attachment.map(|a| a.target));
    let target = attached.or_else(|| {
        crate::auto_recover::all(paths)
            .get(session_id)
            .and_then(|s| s.last_pane.clone())
    })?;
    let (session, pane) = terminal::target(&target).ok()?;
    (session == herdr_session).then(|| pane.to_owned())
}

#[derive(Default, Serialize, Deserialize)]
struct Claims {
    #[serde(default)]
    panes: BTreeMap<String, Claim>,
}
#[derive(Serialize, Deserialize)]
struct Claim {
    owner: String,
    at: i64,
}
/// Claim `target` for `owner` (an operation or request id) so a concurrent start does
/// not pick the same pane. The same owner may claim again (a retry). False on any doubt.
pub fn claim(paths: &Paths, target: &str, owner: &str) -> bool {
    let attempt = || -> Result<bool> {
        private_dir(&paths.root.join(crate::layout::REGISTRY))?;
        let lock = lock_file(&paths.root.join(crate::layout::PANE_CLAIMS_LOCK))?;
        lock.lock_exclusive()?;
        let path = paths.root.join(crate::layout::PANE_CLAIMS);
        let mut claims: Claims = std::fs::read(&path)
            .ok()
            .and_then(|b| serde_json::from_slice(&b).ok())
            .unwrap_or_default();
        let now = daemon::now();
        claims
            .panes
            .retain(|_, c| (0..CLAIM_SECONDS).contains(&(now - c.at)));
        if claims.panes.get(target).is_some_and(|c| c.owner != owner) {
            return Ok(false);
        }
        claims.panes.insert(
            target.to_owned(),
            Claim {
                owner: owner.to_owned(),
                at: now,
            },
        );
        atomic_write(&path, &serde_json::to_vec(&claims)?)?;
        Ok(true)
    };
    attempt().unwrap_or(false)
}

/// The full choice for a manual start or resume into existing Space `workspace`:
/// `session` is the resumed session (None for a new one), `owner` the request id.
/// The pane is claimed for `owner`; `None` means "open a new tab".
pub fn find(
    paths: &Paths,
    herdr: &Herdr,
    workspace: &str,
    name: &str,
    session: Option<&str>,
    owner: &str,
) -> Option<Reuse> {
    if !enabled(paths) {
        return None;
    }
    let excluded = attached_panes(paths, herdr.session, session).ok()?;
    let recorded = session.and_then(|id| recorded_pane(paths, id, herdr.session));
    let reuse = select(
        herdr,
        &Want {
            workspace,
            name,
            recorded: recorded.as_deref(),
            excluded: &excluded,
        },
    )?;
    let target = terminal::herdr_target(herdr.session, &reuse.pane).ok()?;
    claim(paths, &target, owner).then_some(reuse)
}

/// Give a reused recorded pane's tab the name a new tab would get (presentation only;
/// a same-named tab already has it).
pub fn label(host: &impl Host, reuse: &Reuse, name: &str) {
    if reuse.kind == Kind::RecordedPane
        && let Some(tab) = &reuse.tab
    {
        let _ = host.call(&["tab", "rename", tab, name]);
    }
}

#[cfg(test)]
mod tests;
