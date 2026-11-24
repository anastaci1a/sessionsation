use super::*;
use crate::messaging::claude_steer::terminal::cells;
use std::sync::Mutex;

#[derive(Default)]
struct PaneState {
    prompt: String,
    /// Claude draws its command menu above the box while `/compact` is typed.
    menu: bool,
    /// Enter runs the typed command (false: a pane that ignores it).
    enter_runs: bool,
    status: &'static str,
    keys: Vec<String>,
    texts: Vec<String>,
    ran: Vec<String>,
}
struct FakePane(Mutex<PaneState>);
impl FakePane {
    fn new(prompt: &str) -> Self {
        Self(Mutex::new(PaneState {
            prompt: prompt.into(),
            menu: true,
            enter_runs: true,
            status: "idle",
            ..Default::default()
        }))
    }
}
fn rule() -> String {
    "─".repeat(80)
}
impl Pane for FakePane {
    fn agent_status(&self) -> anyhow::Result<Option<String>> {
        Ok(Some(self.0.lock().unwrap().status.into()))
    }
    fn screen(&self) -> anyhow::Result<Vec<Vec<Cell>>> {
        let s = self.0.lock().unwrap();
        let mut lines = vec!["  history".to_owned()];
        if s.menu && s.prompt.starts_with('/') {
            lines.push(
                "  ❯ /compact            Clear conversation history but keep a summary in context"
                    .into(),
            );
            lines.push("    /compact-notes      Another command".into());
        }
        lines.extend([rule(), format!("❯ {}", s.prompt), rule(), "  ~/work".into()]);
        Ok(cells(&lines.join("\n")))
    }
    fn keys(&self, keys: &[&str]) -> anyhow::Result<()> {
        let mut s = self.0.lock().unwrap();
        for k in keys {
            s.keys.push((*k).into());
            match *k {
                "enter" if s.enter_runs => {
                    let typed = std::mem::take(&mut s.prompt);
                    s.ran.push(typed);
                    s.status = "working";
                }
                "backspace" => {
                    s.prompt.pop();
                }
                _ => {}
            }
        }
        Ok(())
    }
    fn text(&self, t: &str) -> anyhow::Result<()> {
        let mut s = self.0.lock().unwrap();
        s.texts.push(t.into());
        s.prompt.push_str(t);
        Ok(())
    }
}

fn paths() -> (tempfile::TempDir, Paths) {
    let dir = tempfile::tempdir().unwrap();
    let paths = Paths {
        root: dir.path().join("state"),
        runtime: dir.path().join("runtime"),
    };
    fs::create_dir_all(&paths.root).unwrap();
    (dir, paths)
}
fn waiting_record(paths: &Paths) -> Record {
    let now = crate::daemon::now();
    let r = Record {
        schema_version: 1,
        id: uuid::Uuid::new_v4().to_string(),
        session_id: "s-claude".into(),
        accepted_generation: "g".into(),
        conversation_id: "44444444-4444-4444-8444-444444444444".into(),
        runtime_generation: "1:2:herdr/s/p".into(),
        phase: "waiting".into(),
        reason: None,
        created_at: now,
        updated_at: now,
        expires_at: now + 600,
        started_at: None,
        finished_at: None,
        attempts: 0,
        baseline: vec![],
        compaction_turn_id: None,
        hold_released: true,
        release_claimed_at: None,
        provider: "claude".into(),
        account: Some("main".into()),
    };
    private_dir(&dir(paths)).unwrap();
    save(paths, &r).unwrap();
    r
}

#[test]
fn the_typed_command_is_read_back_with_or_without_the_menu() {
    let pane = FakePane::new("/compact");
    assert!(compact_view(&pane.screen().unwrap()));
    pane.0.lock().unwrap().menu = false;
    assert!(compact_view(&pane.screen().unwrap()));
    // Another command highlighted, a different prompt, or no prompt at all: refused.
    let other = cells(
        &[
            "  history".to_owned(),
            "  ❯ /compact-notes      Another command".into(),
            "    /compact            Clear".into(),
            rule(),
            "❯ /compact".into(),
            rule(),
        ]
        .join("\n"),
    );
    assert!(!compact_view(&other));
    assert!(!compact_view(&FakePane::new("/compac").screen().unwrap()));
    assert!(!compact_view(&FakePane::new("").screen().unwrap()));
}

#[tokio::test]
async fn claude_compact_types_once_and_completes_when_claude_reports_it() {
    let (_d, paths) = paths();
    let r = waiting_record(&paths);
    let pane = FakePane::new("");
    procedure(&paths, &r.id, &pane).await;
    let running = read(&paths, &r.id).unwrap();
    assert_eq!(running.phase, "running", "{running:?}");
    assert_eq!(running.attempts, 1);
    {
        let s = pane.0.lock().unwrap();
        assert_eq!(s.texts, ["/compact"]);
        assert_eq!(s.keys, ["enter"]);
        assert_eq!(s.ran, ["/compact"]);
    }
    // Not reported yet and still working: keeps running.
    settle(&paths, &running, None, Some(false), None);
    assert_eq!(read(&paths, &r.id).unwrap().phase, "running");
    // A compaction older than the start does not count.
    let since = running.started_at.unwrap();
    settle(&paths, &running, Some(since - 60), Some(false), None);
    assert_eq!(read(&paths, &r.id).unwrap().phase, "running");
    settle(&paths, &running, Some(since), Some(true), None);
    let done = read(&paths, &r.id).unwrap();
    assert_eq!(
        (done.phase.as_str(), done.reason.as_deref()),
        ("complete", Some("compacted"))
    );
    let v = public(&done);
    assert_eq!(v["provider"], "claude");
    assert_eq!(v["method"], "terminal");
    assert_eq!(v["native_method"], "/compact");
    assert_eq!(v["hold"]["active"], false);
    // A finished operation is never typed again.
    procedure(&paths, &r.id, &pane).await;
    assert_eq!(pane.0.lock().unwrap().texts.len(), 1);
}

#[tokio::test]
async fn a_draft_is_never_touched_and_an_ignored_enter_is_withdrawn_not_retyped() {
    let (_d, paths) = paths();
    let r = waiting_record(&paths);
    let pane = FakePane::new("my half-written thought");
    procedure(&paths, &r.id, &pane).await;
    let w = read(&paths, &r.id).unwrap();
    assert_eq!(
        (w.phase.as_str(), w.reason.as_deref()),
        ("waiting", Some("draft_present"))
    );
    assert!(pane.0.lock().unwrap().texts.is_empty());
    assert!(pane.0.lock().unwrap().keys.is_empty());
    // A pane that does not run Enter: the command is cleared with backspaces and the
    // operation fails instead of typing again.
    let pane = FakePane::new("");
    pane.0.lock().unwrap().enter_runs = false;
    procedure(&paths, &r.id, &pane).await;
    let f = read(&paths, &r.id).unwrap();
    assert_eq!(
        (f.phase.as_str(), f.reason.as_deref()),
        ("failed", Some("compaction_not_started"))
    );
    let s = pane.0.lock().unwrap();
    assert_eq!(s.texts, ["/compact"]);
    assert_eq!(s.keys.iter().filter(|k| *k == "enter").count(), 1);
    assert!(s.prompt.is_empty());
}

#[tokio::test]
async fn an_idle_pane_without_a_reported_compaction_fails_after_the_window() {
    let (_d, paths) = paths();
    let r = waiting_record(&paths);
    let _ = update(&paths, &r.id, |x| {
        x.phase = "running".into();
        x.started_at = Some(crate::daemon::now() - IDLE_UNOBSERVED_SECONDS - 5);
        true
    });
    let running = read(&paths, &r.id).unwrap();
    settle(&paths, &running, None, Some(false), None);
    assert_eq!(read(&paths, &r.id).unwrap().phase, "running");
    settle(&paths, &running, None, Some(true), None);
    let f = read(&paths, &r.id).unwrap();
    assert_eq!(f.reason.as_deref(), Some("compaction_not_observed"));
    // A restarted session fails as generation_changed.
    let r2 = waiting_record(&paths);
    let _ = update(&paths, &r2.id, |x| {
        x.phase = "running".into();
        x.started_at = Some(crate::daemon::now());
        true
    });
    settle(
        &paths,
        &read(&paths, &r2.id).unwrap(),
        None,
        None,
        Some("generation_changed".into()),
    );
    assert_eq!(
        read(&paths, &r2.id).unwrap().reason.as_deref(),
        Some("generation_changed")
    );
}
