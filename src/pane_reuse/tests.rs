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
        self.tabs
            .push(json!({"tab_id":id,"workspace_id":ws,"label":label,"pane_count":panes.len()}));
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
    let host =
        Fake::default()
            .tab("w1:t1", "Work", &["w1:p1"])
            .tab("w1:t2", "Old", &["w1:p2", "w1:p3"]);
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
