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
mod tests {
    use super::*;
    use std::cell::RefCell;

    /// An in-memory HERDR: tabs in order, panes with their tab, and per-pane state.
    #[derive(Default)]
    struct Fake {
        tabs: Vec<Value>,
        panes: Vec<Value>,
        /// pane -> process-info answers, served in order (the last one repeats).
        info: BTreeMap<String, Vec<Value>>,
        no_tab_list: bool,
        calls: RefCell<Vec<String>>,
    }
    fn idle() -> Value {
        json!({"process_info":{"shell_pid":7,"foreground_process_group_id":7,
            "foreground_processes":[{"pid":7,"name":"bash"}]}})
    }
    fn busy_group() -> Value {
        // The shell is the only listed process but another group owns the terminal.
        json!({"process_info":{"shell_pid":7,"foreground_process_group_id":9,
            "foreground_processes":[{"pid":7,"name":"bash"}]}})
    }
    fn running() -> Value {
        json!({"process_info":{"shell_pid":7,"foreground_process_group_id":9,
            "foreground_processes":[{"pid":9,"name":"vim"}]}})
    }
    impl Fake {
        fn tab(mut self, id: &str, label: &str, panes: &[&str]) -> Self {
            let ws = id.split_once(':').unwrap().0;
            self.tabs.push(
                json!({"tab_id":id,"workspace_id":ws,"label":label,"pane_count":panes.len()}),
            );
            for p in panes {
                self.panes
                    .push(json!({"pane_id":p,"tab_id":id,"workspace_id":ws}));
                self.info.insert((*p).into(), vec![idle()]);
            }
            self
        }
        fn state(mut self, pane: &str, answers: Vec<Value>) -> Self {
            self.info.insert(pane.into(), answers);
            self
        }
    }
    impl Host for Fake {
        fn call(&self, args: &[&str]) -> Result<Value> {
            self.calls.borrow_mut().push(args.join(" "));
            match args {
                ["pane", "list", "--workspace", ws] => Ok(json!({"panes":self.panes.iter()
                    .filter(|p| p["workspace_id"] == *ws).collect::<Vec<_>>()})),
                ["tab", "list", ..] if self.no_tab_list => anyhow::bail!("unsupported"),
                ["tab", "list", "--workspace", ws] => Ok(json!({"tabs":self.tabs.iter()
                    .filter(|t| t["workspace_id"] == *ws).collect::<Vec<_>>()})),
                ["pane", "process-info", "--pane", pane] => {
                    let seen = self
                        .calls
                        .borrow()
                        .iter()
                        .filter(|c| **c == format!("pane process-info --pane {pane}"))
                        .count();
                    let answers = self.info.get(*pane).context_missing()?;
                    Ok(answers[(seen - 1).min(answers.len() - 1)].clone())
                }
                ["tab", "rename", ..] => Ok(json!({})),
                _ => anyhow::bail!("unexpected {args:?}"),
            }
        }
    }
    trait Missing<T> {
        fn context_missing(self) -> Result<T>;
    }
    impl<T> Missing<T> for Option<T> {
        fn context_missing(self) -> Result<T> {
            self.ok_or_else(|| anyhow::anyhow!("pane_not_found"))
        }
    }
    fn want<'a>(name: &'a str, recorded: Option<&'a str>, excluded: &'a [String]) -> Want<'a> {
        Want {
            workspace: "w1",
            name,
            recorded,
            excluded,
        }
    }

    #[test]
    fn the_recorded_idle_pane_comes_first() {
        let host = Fake::default().tab("w1:t1", "Work", &["w1:p1"]).tab(
            "w1:t2",
            "Old",
            &["w1:p2", "w1:p3"],
        );
        let got = select(&host, &want("Work", Some("w1:p3"), &[])).unwrap();
        assert_eq!(got.pane, "w1:p3");
        assert_eq!(got.tab.as_deref(), Some("w1:t2"));
        assert_eq!(got.kind, Kind::RecordedPane);
        assert_eq!(got.detail(), RECORDED_DETAIL);
    }

    #[test]
    fn a_recorded_pane_elsewhere_busy_or_gone_falls_through_to_the_same_name_tab() {
        let base = || {
            Fake::default()
                .tab("w1:t1", "Work", &["w1:p1"])
                .tab("w2:t5", "Work", &["w2:p5"])
        };
        // In another Space.
        let got = select(&base(), &want("Work", Some("w2:p5"), &[])).unwrap();
        assert_eq!((got.pane.as_str(), got.kind), ("w1:p1", Kind::SameNameTab));
        assert_eq!(got.detail(), SAME_NAME_DETAIL);
        // Gone.
        let got = select(&base(), &want("Work", Some("w1:p9"), &[])).unwrap();
        assert_eq!(got.pane, "w1:p1");
        // Busy.
        let host = base()
            .tab("w1:t3", "Other", &["w1:p3"])
            .state("w1:p3", vec![running()]);
        let got = select(&host, &want("Work", Some("w1:p3"), &[])).unwrap();
        assert_eq!(got.pane, "w1:p1");
    }

    #[test]
    fn a_same_name_single_pane_idle_tab_is_reused_first_by_herdr_order() {
        let host = Fake::default()
            .tab("w1:t1", "work", &["w1:p1"])
            .tab("w1:t2", "Work", &["w1:p2"])
            .tab("w1:t3", "Work", &["w1:p3"]);
        let got = select(&host, &want("Work", None, &[])).unwrap();
        assert_eq!(
            got.pane, "w1:p2",
            "exact, case-sensitive label; first qualifying"
        );
        assert_eq!(got.tab.as_deref(), Some("w1:t2"));
        assert_eq!(got.workspace, "w1");
    }

    #[test]
    fn nothing_is_reused_when_no_tab_qualifies() {
        // A busy pane: the foreground group is not the shell.
        let host = Fake::default()
            .tab("w1:t1", "Work", &["w1:p1"])
            .state("w1:p1", vec![busy_group()]);
        assert_eq!(select(&host, &want("Work", None, &[])), None);
        // Something else runs in it.
        let host = Fake::default()
            .tab("w1:t1", "Work", &["w1:p1"])
            .state("w1:p1", vec![running()]);
        assert_eq!(select(&host, &want("Work", None, &[])), None);
        // Two panes in the tab.
        let host = Fake::default().tab("w1:t1", "Work", &["w1:p1", "w1:p2"]);
        assert_eq!(select(&host, &want("Work", None, &[])), None);
        // Another label.
        let host = Fake::default().tab("w1:t1", "Work 2", &["w1:p1"]);
        assert_eq!(select(&host, &want("Work", None, &[])), None);
        // Another running session's attachment.
        let host = Fake::default().tab("w1:t1", "Work", &["w1:p1"]);
        assert_eq!(
            select(&host, &want("Work", None, &["w1:p1".to_owned()])),
            None
        );
        // ...even as the recorded pane.
        assert_eq!(
            select(&host, &want("Work", Some("w1:p1"), &["w1:p1".to_owned()])),
            None
        );
        // Only in another Space.
        let host = Fake::default().tab("w2:t1", "Work", &["w2:p1"]);
        assert_eq!(select(&host, &want("Work", None, &[])), None);
        // An older HERDR without `tab list`.
        let mut host = Fake::default().tab("w1:t1", "Work", &["w1:p1"]);
        host.no_tab_list = true;
        assert_eq!(select(&host, &want("Work", None, &[])), None);
    }

    #[test]
    fn a_pane_that_turns_busy_after_selection_fails_the_recheck() {
        let host = Fake::default()
            .tab("w1:t1", "Work", &["w1:p1"])
            .state("w1:p1", vec![idle(), running()]);
        let got = select(&host, &want("Work", None, &[])).unwrap();
        assert!(!pane_idle(&host, &got.pane));
    }

    #[test]
    fn only_a_reused_recorded_pane_is_renamed() {
        let host = Fake::default().tab("w1:t1", "Old", &["w1:p1"]);
        let recorded = select(&host, &want("New", Some("w1:p1"), &[])).unwrap();
        label(&host, &recorded, "New");
        assert!(
            host.calls
                .borrow()
                .contains(&"tab rename w1:t1 New".to_owned())
        );
        let host = Fake::default().tab("w1:t1", "New", &["w1:p1"]);
        let same = select(&host, &want("New", None, &[])).unwrap();
        label(&host, &same, "New");
        assert!(
            !host
                .calls
                .borrow()
                .iter()
                .any(|c| c.starts_with("tab rename"))
        );
    }

    #[test]
    fn a_claim_excludes_other_owners_until_it_expires() {
        let tmp = tempfile::tempdir().unwrap();
        let paths = Paths::new(Some(tmp.path().join("data"))).unwrap();
        assert!(claim(&paths, "herdr/s/w1:p1", "a"));
        assert!(claim(&paths, "herdr/s/w1:p1", "a"), "the same owner again");
        assert!(!claim(&paths, "herdr/s/w1:p1", "b"));
        assert!(claim(&paths, "herdr/s/w1:p2", "b"));
        let path = paths.root.join(crate::layout::PANE_CLAIMS);
        let mut stored: Value = serde_json::from_slice(&std::fs::read(&path).unwrap()).unwrap();
        stored["panes"]["herdr/s/w1:p1"]["at"] = json!(daemon::now() - CLAIM_SECONDS);
        std::fs::write(&path, stored.to_string()).unwrap();
        assert!(claim(&paths, "herdr/s/w1:p1", "b"), "an expired claim");
    }

    #[test]
    fn the_setting_turns_reuse_off() {
        let tmp = tempfile::tempdir().unwrap();
        let paths = Paths::new(Some(tmp.path().join("data"))).unwrap();
        assert!(enabled(&paths));
        let config = crate::config::Config {
            reuse_idle_tabs: false,
            ..Default::default()
        };
        crate::store::save_config(&paths, &config).unwrap();
        assert!(!enabled(&paths));
        let herdr = Herdr {
            session: "fixture",
            socket: None,
        };
        // Off: no HERDR call is made at all.
        assert_eq!(find(&paths, &herdr, "w1", "Work", None, "r"), None);
    }
}
