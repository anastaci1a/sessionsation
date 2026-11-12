use super::claude::{Kind, Pointer, command_view, dialog};
use super::*;
use crate::messaging::claude_steer::terminal::cells;
use crate::messaging::public::{PublicMode, PublicRequest, send_public};
use crate::messaging::tests::{attachment_fixture, bind, fixture, scripted_connections};
use std::sync::{Arc, Mutex};

// ---------------------------------------------------------------- Claude screens

fn real(name: &str) -> Vec<Vec<crate::messaging::claude_steer::terminal::Cell>> {
    let path = std::path::Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("tests/fixtures/claude-screens")
        .join(format!("{name}.ansi"));
    cells(&fs::read_to_string(&path).unwrap_or_else(|e| panic!("{path:?}: {e}")))
}
fn plain(lines: &[&str]) -> Vec<Vec<crate::messaging::claude_steer::terminal::Cell>> {
    cells(&lines.join("\n"))
}

#[test]
fn real_typed_commands_show_no_menu_and_are_read_back_exactly() {
    // Claude Code 2.1.314, captured on a throwaway session.
    assert!(command_view(
        &real("model-command-typed"),
        "/model claude-fable-5-2"
    ));
    assert!(command_view(&real("effort-command-typed"), "/effort high"));
    assert!(!command_view(
        &real("model-command-typed"),
        "/model claude-opus-5-5"
    ));
    // A completion menu above the box is never accepted as the typed command.
    assert!(!command_view(&real("slash-menu"), "/model"));
    // After a refused or applied command the prompt is empty again.
    for name in ["model-not-found", "effort-set"] {
        assert!(
            matches!(
                crate::messaging::claude_steer::terminal::classify(&real(name), true),
                Ok(crate::messaging::claude_steer::terminal::View::Main(ref d)) if d.is_empty()
            ),
            "{name}"
        );
    }
}

#[test]
fn switch_dialog_is_recognized_with_its_pointer() {
    let d = dialog(&real("model-switch-dialog")).expect("real dialog");
    assert_eq!(
        (d.kind, d.pointer, d.hook_asked),
        (Kind::Model, Pointer::Yes, false)
    );
    let no = plain(&[
        "   Switch model?",
        "   Your next response will be slower",
        "     1. Yes, switch to Opus 5.5",
        "   ❯ 2. No, go back",
    ]);
    assert_eq!(dialog(&no).unwrap().pointer, Pointer::No);
    // The focus variant without option numbers.
    let bare = plain(&[
        "   Change effort level?",
        "   Yes, switch to high",
        "   ❯ No, go back",
    ]);
    // Options must be numbered as captured; the title alone is still recognized, so the
    // dialog can be closed (Esc) but is never confirmed.
    assert!(dialog(&bare).is_none());
    assert_eq!(
        super::claude::dialog_title(&bare).map(|t| t.1),
        Some(Kind::Effort)
    );
    let loose = plain(&[
        "   Switch model?",
        "   ❯ 1. Yes, switch",
        "     2. Nothing here",
    ]);
    assert!(dialog(&loose).is_none());
    let hook = plain(&[
        "   Switch model?",
        "   A PreModelSwitch hook asked you to confirm",
        "   ❯ 1. Yes, switch to Opus 5.5",
        "     2. No, go back",
    ]);
    assert!(dialog(&hook).unwrap().hook_asked);
    // No pointer at all: recognized but never answered.
    let none = plain(&[
        "   Switch model?",
        "     1. Yes, switch to Opus 5.5",
        "     2. No, go back",
    ]);
    assert_eq!(dialog(&none).unwrap().pointer, Pointer::None);
    // Unrelated overlays and main screens are no dialog.
    for name in ["permission", "trust", "idle-empty", "model-command-typed"] {
        assert!(dialog(&real(name)).is_none(), "{name}");
    }
    let two_titles = plain(&[
        "Switch model?",
        "Switch model?",
        "❯ Yes, switch",
        "No, go back",
    ]);
    assert!(dialog(&two_titles).is_none());
}

// ---------------------------------------------------------------- Codex

#[derive(Default)]
struct Fake {
    busy: bool,
    queue: Vec<String>,
    model: String,
    effort: Option<String>,
    /// `thread/settings/update` calls answered with a JSON-RPC error.
    reject_updates: bool,
    /// Updates are accepted but not applied (a substituted or ignored change).
    ignore_updates: bool,
    log: Vec<String>,
}

struct World {
    _dir: tempfile::TempDir,
    paths: Paths,
    session: crate::catalog::Session,
    fake: Arc<Mutex<Fake>>,
    server: tokio::task::JoinHandle<()>,
}
impl Drop for World {
    fn drop(&mut self) {
        self.server.abort();
    }
}

async fn world() -> World {
    let (dir, paths, input, _) = fixture();
    let mut a = attachment_fixture(&paths, &input);
    bind(&paths, &mut a);
    let fake = Arc::new(Mutex::new(Fake {
        model: "gpt-6".into(),
        effort: Some("medium".into()),
        ..Default::default()
    }));
    let shared = fake.clone();
    let id = a.id.clone();
    let server = scripted_connections(
        &a.endpoint(),
        move |r| {
            let mut s = shared.lock().unwrap();
            let method = r["method"].as_str().unwrap().to_owned();
            match method.as_str() {
                "thread/settings/update" => s.log.push(format!(
                    "update model={} effort={}",
                    r["params"]["model"].as_str().unwrap_or("-"),
                    r["params"]["effort"].as_str().unwrap_or("-")
                )),
                m => s.log.push(m.to_owned()),
            }
            match method.as_str() {
                "thread/read" => Some(json!({"thread":{"id":id,"status":{"type":if s.busy {"active"} else {"idle"}},
                    "model":s.model,"reasoningEffort":s.effort}})),
                "thread/turns/list" => Some(json!({"data":[{"id":"turn-0","status":"completed","itemsView":"summary","items":[]}],"nextCursor":null})),
                "thread/queue/list" => Some(json!({"data":s.queue.iter().map(|q|
                    json!({"id":format!("q-{q}"),"clientUserMessageId":q})).collect::<Vec<_>>(),"nextCursor":null})),
                "thread/queue/add" => {
                    let client = r["params"]["clientUserMessageId"].as_str().unwrap().to_owned();
                    let line = format!("add {client} on {}", s.model);
                    s.log.push(line);
                    Some(json!({"queuedSubmission":{"id":format!("q-{client}"),"clientUserMessageId":client}}))
                }
                "model/list" => Some(json!({"data":[
                    {"id":"gpt-6","model":"gpt-6","supportedReasoningEfforts":[{"reasoningEffort":"low"},{"reasoningEffort":"medium"},{"reasoningEffort":"high"}]},
                    {"id":"gpt-6-astra","model":"gpt-6-astra","supportedReasoningEfforts":[{"reasoningEffort":"medium"},{"reasoningEffort":"high"}]}],
                    "nextCursor":null})),
                "thread/settings/update" => {
                    if s.reject_updates {
                        return Some(json!({"__error":true}));
                    }
                    if !s.ignore_updates {
                        if let Some(m) = r["params"]["model"].as_str() {
                            s.model = m.into();
                            // A model switch resets the effort, as Claude and Codex may.
                            s.effort = Some("low".into());
                        }
                        if let Some(e) = r["params"]["effort"].as_str() {
                            s.effort = Some(e.into());
                        }
                    }
                    Some(json!({}))
                }
                other => panic!("unexpected {other}"),
            }
        },
        400,
    )
    .await;
    let instance: external::Instance = serde_json::from_value(json!({"pane":"%1","tmux_session":"fixture",
        "pid":a.pid,"start_time":a.start,"cwd":"/tmp","thread_id":a.id,"rollout":"/tmp/unused","home":a.home,
        "binary":"fixture","resume_options":[],"account":"selected"}))
    .unwrap();
    let session = crate::catalog::reconcile(&paths, &[instance])
        .unwrap()
        .remove(0);
    World {
        _dir: dir,
        paths,
        session,
        fake,
        server,
    }
}
impl World {
    fn set_model(&self, model: &str, effort: Option<&str>) -> anyhow::Result<Value> {
        request(
            &self.paths,
            Request {
                session: &self.session.id,
                expected_generation: &self.session.generation(),
                model,
                effort,
                when_idle: true,
                expires_at: None,
                operation: None,
            },
        )
    }
    fn record(&self, id: &str) -> Record {
        read(&self.paths, id).unwrap()
    }
    fn updates(&self) -> Vec<String> {
        self.fake
            .lock()
            .unwrap()
            .log
            .iter()
            .filter(|l| l.starts_with("update") || l.starts_with("add"))
            .cloned()
            .collect()
    }
    async fn send(&self, id: &str) -> Value {
        send_public(
            &self.paths,
            PublicRequest {
                origin: None,
                delivery: None,
                await_reply: false,
                if_busy: None,
                steering: None,
                recovery: None,
                api_version: 1,
                message_id: id.into(),
                session_id: self.session.id.clone(),
                expected_generation: self.session.generation(),
                mode: PublicMode::Queue,
                text: format!("message {id}"),
                expires_at: crate::daemon::now() + 300,
            },
        )
        .await
        .unwrap()
    }
}
/// A supervisor pass that ignores the retry back-off of this world's operations.
async fn tick_now(paths: &Paths) {
    let mine: Vec<String> = list(paths).unwrap().into_iter().map(|r| r.id).collect();
    BACKOFF.lock().unwrap().retain(|k, _| !mine.contains(k));
    tick(paths).await;
}
const M1: &str = "11111111-1111-4111-8111-111111111111";

#[tokio::test]
async fn codex_change_waits_for_idle_applies_model_then_effort_and_verifies() {
    let w = world().await;
    w.fake.lock().unwrap().busy = true;
    let op = w.set_model("gpt-6-astra", Some("high")).unwrap();
    let id = op["id"].as_str().unwrap().to_owned();
    assert_eq!(op["phase"], "waiting");
    assert_eq!(op["kind"], "model_change");
    tick_now(&w.paths).await;
    assert_eq!(w.record(&id).reason.as_deref(), Some("turn_active"));
    assert!(w.updates().is_empty());
    // A send while the change is pending is held locally.
    let held = w.send(M1).await;
    assert_eq!(
        (held["state"].as_str(), held["reason"].as_str()),
        (Some("pending_local"), Some("model_change_hold"))
    );
    // Asking again with the same values returns the pending one; other values are refused.
    assert_eq!(
        w.set_model("gpt-6-astra", Some("high")).unwrap()["existing"],
        true
    );
    assert!(
        w.set_model("gpt-6", None)
            .unwrap_err()
            .to_string()
            .starts_with("SESSION_OPERATION_IN_PROGRESS")
    );
    w.fake.lock().unwrap().busy = false;
    tick_now(&w.paths).await;
    let r = w.record(&id);
    assert_eq!(r.phase, "complete", "{r:?}");
    assert_eq!(r.applied.as_ref().unwrap().model, "gpt-6-astra");
    assert_eq!(r.applied.as_ref().unwrap().effort.as_deref(), Some("high"));
    assert_eq!(r.before.as_ref().unwrap().model, "gpt-6");
    // Model first, then effort (a model switch may reset it), then the held input on the new model.
    tick_now(&w.paths).await;
    assert_eq!(
        w.updates(),
        [
            "update model=gpt-6-astra effort=-",
            "update model=- effort=high",
            &format!("add {M1} on gpt-6-astra"),
        ]
    );
    assert!(w.record(&id).hold_released);
    let current =
        crate::session_model::current(&w.paths, &w.session.conversation_id, None).unwrap();
    assert_eq!(
        (current.model.as_str(), current.source.as_str()),
        ("gpt-6-astra", "set_model")
    );
    // The model change did not change the session generation.
    assert_eq!(
        crate::catalog::find(&w.paths, &w.session.id)
            .unwrap()
            .generation(),
        w.session.generation()
    );
}

#[tokio::test]
async fn codex_effort_omitted_is_preserved_and_equal_values_are_already_applied() {
    let w = world().await;
    let op = w.set_model("gpt-6-astra", None).unwrap();
    let id = op["id"].as_str().unwrap().to_owned();
    tick_now(&w.paths).await;
    let r = w.record(&id);
    assert_eq!(r.phase, "complete");
    assert_eq!(r.effort_source, "preserved");
    // The effort in force before ("medium") is re-applied after the model reset it.
    assert_eq!(r.applied.unwrap().effort.as_deref(), Some("medium"));
    let again = w.set_model("gpt-6-astra", Some("medium")).unwrap();
    let id2 = again["id"].as_str().unwrap().to_owned();
    tick_now(&w.paths).await;
    let r2 = w.record(&id2);
    assert_eq!(
        (r2.phase.as_str(), r2.reason.as_deref()),
        ("complete", Some("already_applied"))
    );
    assert_eq!(w.updates().len(), 2, "nothing sent the second time");
}

#[tokio::test]
async fn codex_refusals_and_unconfirmed_changes_fail_closed() {
    let w = world().await;
    // Not in the account's model list.
    let op = w.set_model("gpt-9-nope", None).unwrap();
    tick_now(&w.paths).await;
    let r = w.record(op["id"].as_str().unwrap());
    assert_eq!(
        (r.phase.as_str(), r.reason.as_deref()),
        ("failed", Some("model_not_available"))
    );
    // Effort the model does not offer.
    let op = w.set_model("gpt-6-astra", Some("low")).unwrap();
    tick_now(&w.paths).await;
    assert_eq!(
        w.record(op["id"].as_str().unwrap()).reason.as_deref(),
        Some("effort_unsupported")
    );
    // The app-server refuses the update.
    w.fake.lock().unwrap().reject_updates = true;
    let op = w.set_model("gpt-6-astra", Some("high")).unwrap();
    tick_now(&w.paths).await;
    assert_eq!(
        w.record(op["id"].as_str().unwrap()).reason.as_deref(),
        Some("provider_rejected")
    );
    // Accepted but never visible: retried, then model_not_confirmed with the observed value.
    {
        let mut s = w.fake.lock().unwrap();
        s.reject_updates = false;
        s.ignore_updates = true;
    }
    let op = w.set_model("gpt-6-astra", Some("high")).unwrap();
    let id = op["id"].as_str().unwrap().to_owned();
    for _ in 0..4 {
        tick_now(&w.paths).await;
    }
    let r = w.record(&id);
    assert_eq!(
        (r.phase.as_str(), r.reason.as_deref()),
        ("failed", Some("model_not_confirmed"))
    );
    assert_eq!(r.observed.unwrap().model, "gpt-6");
    assert!(r.applied.is_none());
    // Aliases and unsupported grammar never reach the journal.
    assert!(
        w.set_model("-x", None)
            .unwrap_err()
            .to_string()
            .starts_with("INVALID_MODEL")
    );
}

#[tokio::test]
async fn cancel_releases_held_input_and_wait_reports_not_confirmed() {
    let w = world().await;
    w.fake.lock().unwrap().busy = true;
    let op = w.set_model("gpt-6-astra", Some("high")).unwrap();
    let id = op["id"].as_str().unwrap().to_owned();
    assert_eq!(w.send(M1).await["reason"], "model_change_hold");
    let err = wait(&w.paths, &id, 0).await.unwrap_err();
    let n = err
        .downcast_ref::<crate::session_model::NotConfirmed>()
        .unwrap();
    assert_eq!(n.code, "MODEL_NOT_CONFIRMED");
    assert_eq!(n.operation["phase"], "waiting");
    let c = cancel(&w.paths, &id).await.unwrap();
    assert_eq!(c["phase"], "cancelled");
    assert_eq!(cancel(&w.paths, &id).await.unwrap()["phase"], "cancelled");
    w.fake.lock().unwrap().busy = false;
    tick_now(&w.paths).await;
    assert_eq!(w.updates(), [format!("add {M1} on gpt-6")]);
    assert!(
        operations(&w.paths)
            .unwrap()
            .iter()
            .any(|o| o["id"] == id.as_str() && o["kind"] == "model_change")
    );
}

#[tokio::test]
async fn codex_applying_without_progress_fails_and_releases_held_input() {
    let w = world().await;
    w.fake.lock().unwrap().busy = true;
    let op = w.set_model("gpt-6-astra", Some("high")).unwrap();
    let id = op["id"].as_str().unwrap().to_owned();
    assert_eq!(w.send(M1).await["reason"], "model_change_hold");
    // A supervisor died mid-change and nothing could be observed since.
    let mut r = w.record(&id);
    r.phase = "applying".into();
    r.updated_at = crate::daemon::now() - APPLY_UNOBSERVED_SECONDS - 1;
    save(&w.paths, &r).unwrap();
    w.fake.lock().unwrap().busy = false;
    tick_now(&w.paths).await;
    let r = w.record(&id);
    assert_eq!(
        (r.phase.as_str(), r.reason.as_deref()),
        ("failed", Some("apply_not_observed"))
    );
    tick_now(&w.paths).await;
    assert!(w.record(&id).hold_released);
    assert_eq!(w.updates(), [format!("add {M1} on gpt-6")]);
}

#[test]
fn waiting_changes_back_off_up_to_thirty_seconds() {
    let id = "backoff-test";
    assert!(due(id, 100));
    back_off(id, 100, 4);
    assert!(!due(id, 103) && due(id, 104));
    back_off(id, 104, 4);
    assert!(!due(id, 111) && due(id, 112));
    for _ in 0..6 {
        back_off(id, 200, 4);
    }
    assert!(!due(id, 229) && due(id, 230));
}

#[test]
fn resume_passes_the_last_recorded_model_and_effort() {
    use crate::provider::claude::resume_model_args;
    let o = |m: &str, e: Option<&str>| Observed {
        model: m.into(),
        effort: e.map(str::to_owned),
        observed_at: 1,
        source: "claude_statusline".into(),
    };
    assert_eq!(
        resume_model_args(Some(&o("claude-fable-5-2", Some("high")))),
        ["--model", "claude-fable-5-2", "--effort", "high"]
    );
    assert_eq!(
        resume_model_args(Some(&o("claude-opus-5-5[1m]", None))),
        ["--model", "claude-opus-5-5[1m]"]
    );
    // Nothing recorded, or nothing Sessionsation would pass itself: no flags (as before).
    assert!(resume_model_args(None).is_empty());
    assert!(resume_model_args(Some(&o("fable", Some("high")))).is_empty());
    assert_eq!(
        resume_model_args(Some(&o("claude-fable-5-2", Some("weird")))),
        ["--model", "claude-fable-5-2"]
    );
}

#[test]
fn model_ids_compare_without_the_context_suffix() {
    assert!(crate::session_model::same_model(
        "claude-opus-5-5",
        "claude-opus-5-5[1m]"
    ));
    assert!(crate::session_model::same_model(
        "Claude-Opus-5-5[1m]",
        "claude-opus-5-5"
    ));
    assert!(!crate::session_model::same_model(
        "claude-opus-5-5",
        "claude-opus-5-6"
    ));
}

// ---------------------------------------------------------------- Claude worker, fake pane

const CONV: &str = "44444444-4444-4444-8444-444444444444";

#[derive(Default)]
struct PaneState {
    prompt: String,
    /// (kind title, pointer on Yes, numbered options)
    dialog: Option<(&'static str, Option<bool>, bool)>,
    model: String,
    effort: Option<String>,
    /// Ask before switching models (warm prompt cache).
    ask: bool,
    /// The pointer the dialog opens with (None: no pointer drawn).
    ask_pointer: Option<bool>,
    /// Esc does not close the dialog (a stuck pane).
    stuck: bool,
    statuses: Vec<&'static str>,
    keys: Vec<String>,
    pending_model: Option<String>,
}
struct FakePane {
    home: std::path::PathBuf,
    s: Mutex<PaneState>,
}
impl FakePane {
    fn new(home: &std::path::Path, model: &str, effort: &str) -> Self {
        let p = Self {
            home: home.to_path_buf(),
            s: Mutex::new(PaneState {
                model: model.into(),
                effort: Some(effort.into()),
                ..Default::default()
            }),
        };
        p.hook();
        p
    }
    fn hook(&self) {
        let s = self.s.lock().unwrap();
        let mut payload = json!({"session_id":CONV,"model":{"id":s.model}});
        if let Some(e) = &s.effort {
            payload["effort"] = json!({"level":e});
        }
        let ms = crate::messaging::claude_steer::terminal::now_ms();
        // The status line re-runs on every change: force a rewrite.
        let file = crate::session_model::hook_file(&self.home, CONV).unwrap();
        let _ = fs::remove_file(&file);
        crate::session_model::record_hook(&self.home, &payload, ms).unwrap();
    }
    fn switch(&self, model: String) {
        {
            let mut s = self.s.lock().unwrap();
            s.model = model;
            s.effort = Some("low".into());
        }
        self.hook();
    }
    fn keys_sent(&self) -> Vec<String> {
        self.s.lock().unwrap().keys.clone()
    }
}
fn rule() -> String {
    "─".repeat(80)
}
impl claude::Pane for FakePane {
    fn agent_status(&self) -> anyhow::Result<Option<String>> {
        let mut s = self.s.lock().unwrap();
        let next = if s.statuses.len() > 1 {
            s.statuses.remove(0)
        } else {
            s.statuses.first().copied().unwrap_or("idle")
        };
        Ok(Some(next.into()))
    }
    fn screen(&self) -> anyhow::Result<Vec<Vec<crate::messaging::claude_steer::terminal::Cell>>> {
        let s = self.s.lock().unwrap();
        let mut lines = vec!["  history".to_owned()];
        if let Some((title, pointer, numbered)) = s.dialog {
            let n = |i: &str| {
                if numbered {
                    format!("{i}. ")
                } else {
                    String::new()
                }
            };
            let mark = |yes: bool| match pointer {
                Some(p) if p == yes => "❯ ",
                _ => "  ",
            };
            lines.push(format!("   {title}"));
            lines.push(format!("   {}{}Yes, switch to it", mark(true), n("1")));
            lines.push(format!("   {}{}No, go back", mark(false), n("2")));
        } else {
            lines.extend([rule(), format!("❯ {}", s.prompt), rule(), "  ~/work".into()]);
        }
        Ok(cells(&lines.join("\n")))
    }
    fn keys(&self, keys: &[&str]) -> anyhow::Result<()> {
        for k in keys {
            let mut s = self.s.lock().unwrap();
            s.keys.push((*k).to_owned());
            match (*k, s.dialog) {
                ("up", Some((t, Some(false), n))) => s.dialog = Some((t, Some(true), n)),
                ("down", Some((t, Some(true), n))) => s.dialog = Some((t, Some(false), n)),
                ("esc", Some(_)) if !s.stuck => {
                    s.dialog = None;
                    s.pending_model = None;
                }
                ("enter", Some((_, Some(yes), _))) => {
                    s.dialog = None;
                    let pending = s.pending_model.take();
                    drop(s);
                    if yes && let Some(m) = pending {
                        self.switch(m);
                    }
                }
                ("enter", None) => {
                    let typed = std::mem::take(&mut s.prompt);
                    if let Some(m) = typed.strip_prefix("/model ") {
                        if s.ask {
                            s.pending_model = Some(m.to_owned());
                            s.dialog = Some(("Switch model?", s.ask_pointer, true));
                        } else {
                            drop(s);
                            self.switch(m.to_owned());
                        }
                    } else if let Some(e) = typed.strip_prefix("/effort ") {
                        s.effort = Some(e.to_owned());
                        drop(s);
                        self.hook();
                    }
                }
                ("backspace", None) => {
                    s.prompt.pop();
                }
                _ => {}
            }
        }
        Ok(())
    }
    fn text(&self, t: &str) -> anyhow::Result<()> {
        let mut s = self.s.lock().unwrap();
        if s.dialog.is_none() {
            s.prompt.push_str(t);
        }
        s.keys.push(format!("text:{t}"));
        Ok(())
    }
}

fn claude_record(paths: &Paths, model: &str, effort: Option<&str>) -> String {
    let now = crate::daemon::now();
    let r = Record {
        schema_version: 1,
        id: uuid::Uuid::new_v4().to_string(),
        session_id: "s".into(),
        accepted_generation: "g".into(),
        conversation_id: CONV.into(),
        provider: "claude".into(),
        account: "a".into(),
        when: "now".into(),
        runtime: "1:2:herdr/default/w1:p1".into(),
        requested_model: model.into(),
        requested_effort: effort.map(str::to_owned),
        effort_source: "requested".into(),
        before: None,
        observed: None,
        applied: None,
        phase: "waiting".into(),
        reason: None,
        detail: None,
        step: None,
        steps: vec![],
        attempts: 0,
        created_at: now,
        updated_at: now,
        expires_at: now + 60,
        started_at: None,
        finished_at: None,
        hold_released: true,
        release_claimed_at: None,
        refusal_layout: None,
    };
    save(paths, &r).unwrap();
    r.id
}
fn claude_world() -> (tempfile::TempDir, Paths, std::path::PathBuf) {
    let dir = tempfile::tempdir().unwrap();
    let paths = Paths::new(Some(dir.path().join("root"))).unwrap();
    let home = dir.path().join("home");
    (dir, paths, home)
}
fn never_forbidden(keys: &[String]) {
    for k in keys {
        assert!(
            !matches!(k.as_str(), "ctrl+c" | "ctrl+d" | "ctrl+l")
                && (k.len() != 1 || !k.chars().all(|c| c.is_ascii_digit())),
            "{k} in {keys:?}"
        );
    }
}

#[tokio::test]
async fn claude_change_answers_the_dialog_yes_then_sets_effort() {
    let (_d, paths, home) = claude_world();
    let pane = FakePane::new(&home, "claude-opus-5-5", "high");
    {
        let mut s = pane.s.lock().unwrap();
        s.ask = true;
        s.ask_pointer = Some(false);
    }
    let id = claude_record(&paths, "claude-fable-5-2", Some("high"));
    claude::procedure(&paths, &id, &pane, &home, "herdr/default/w1:p1")
        .await
        .unwrap();
    let r = read(&paths, &id).unwrap();
    assert_eq!(r.phase, "complete", "{r:?}");
    let applied = r.applied.unwrap();
    assert_eq!(
        (applied.model.as_str(), applied.effort.as_deref()),
        ("claude-fable-5-2", Some("high"))
    );
    let keys = pane.keys_sent();
    assert_eq!(
        keys,
        [
            "text:/model claude-fable-5-2",
            "enter",
            "up",
            "enter",
            "text:/effort high",
            "enter"
        ]
    );
    never_forbidden(&keys);
}

#[tokio::test]
async fn a_dialog_that_cannot_be_answered_is_closed_never_left_open() {
    let (_d, paths, home) = claude_world();
    let pane = FakePane::new(&home, "claude-opus-5-5", "high");
    {
        let mut s = pane.s.lock().unwrap();
        s.ask = true;
        s.ask_pointer = None; // no pointer drawn: never confirmed
    }
    let id = claude_record(&paths, "claude-fable-5-2", Some("high"));
    claude::procedure(&paths, &id, &pane, &home, "herdr/default/w1:p1")
        .await
        .unwrap();
    let r = read(&paths, &id).unwrap();
    assert_eq!(
        (r.phase.as_str(), r.reason.as_deref()),
        ("failed", Some("TERMINAL_OVERLAY_UNIDENTIFIED"))
    );
    assert!(pane.s.lock().unwrap().dialog.is_none(), "dialog closed");
    assert_eq!(pane.s.lock().unwrap().model, "claude-opus-5-5");
    assert_eq!(pane.keys_sent().last().unwrap(), "esc");

    // Pointer on Yes but the change must be abandoned (a PreModelSwitch hook asked): No is chosen.
    let (_d2, paths2, home2) = claude_world();
    let pane2 = FakePane::new(&home2, "claude-opus-5-5", "high");
    pane2.s.lock().unwrap().dialog = Some(("Switch model?", Some(true), true));
    assert_eq!(claude::close_dialog(&pane2).await, Ok(()));
    assert_eq!(pane2.keys_sent(), ["down", "enter"]);
    assert_eq!(pane2.s.lock().unwrap().model, "claude-opus-5-5");

    // A dialog that will not close: the failure names the pane.
    let (_d3, paths3, home3) = claude_world();
    let pane3 = FakePane::new(&home3, "claude-opus-5-5", "high");
    {
        let mut s = pane3.s.lock().unwrap();
        s.ask = true;
        s.ask_pointer = None;
        s.stuck = true;
    }
    let id3 = claude_record(&paths3, "claude-fable-5-2", Some("high"));
    claude::procedure(&paths3, &id3, &pane3, &home3, "herdr/default/w1:p1")
        .await
        .unwrap();
    let r3 = read(&paths3, &id3).unwrap();
    assert_eq!(r3.reason.as_deref(), Some("pane_left_in_dialog"));
    assert!(r3.detail.unwrap().contains("herdr/default/w1:p1"));
    let _ = paths2;
}

#[tokio::test]
async fn claude_change_types_only_after_two_idle_reads_and_never_over_a_draft() {
    let (_d, paths, home) = claude_world();
    let pane = FakePane::new(&home, "claude-opus-5-5", "high");
    // Idle once, then a turn starts (a delivery just pressed Enter).
    pane.s.lock().unwrap().statuses = vec!["idle", "working"];
    let id = claude_record(&paths, "claude-fable-5-2", Some("high"));
    claude::procedure(&paths, &id, &pane, &home, "t")
        .await
        .unwrap();
    let r = read(&paths, &id).unwrap();
    assert_eq!(
        (r.phase.as_str(), r.reason.as_deref()),
        ("waiting", Some("turn_active"))
    );
    assert!(pane.keys_sent().is_empty());
    // A user draft: nothing typed, nothing cleared.
    {
        let mut s = pane.s.lock().unwrap();
        s.statuses = vec!["idle"];
        s.prompt = "half a thought".into();
    }
    claude::procedure(&paths, &id, &pane, &home, "t")
        .await
        .unwrap();
    assert_eq!(
        read(&paths, &id).unwrap().reason.as_deref(),
        Some("draft_present")
    );
    assert!(pane.keys_sent().is_empty());
    assert_eq!(pane.s.lock().unwrap().prompt, "half a thought");
    // Withdrawing removes only a prefix of the worker's own command, by length.
    assert_eq!(
        claude::withdraw(&pane, "/model claude-fable-5-2").await,
        Err("prompt_not_owned")
    );
    assert_eq!(pane.s.lock().unwrap().prompt, "half a thought");
    pane.s.lock().unwrap().prompt = "/model claude-fa".into();
    assert_eq!(
        claude::withdraw(&pane, "/model claude-fable-5-2").await,
        Ok(())
    );
    assert!(pane.s.lock().unwrap().prompt.is_empty());
    assert_eq!(pane.keys_sent().len(), "/model claude-fa".len());
    assert!(pane.keys_sent().iter().all(|k| k == "backspace"));
}

#[tokio::test]
async fn a_leftover_command_of_a_dead_worker_is_cleared_exactly() {
    let (_d, paths, home) = claude_world();
    let pane = FakePane::new(&home, "claude-opus-5-5", "high");
    let id = claude_record(&paths, "claude-fable-5-2", Some("high"));
    let mut r = read(&paths, &id).unwrap();
    r.phase = "applying".into();
    r.attempts = 1;
    save(&paths, &r).unwrap();
    pane.s.lock().unwrap().prompt = "/model claude-fable-5-2".into();
    claude::procedure(&paths, &id, &pane, &home, "t")
        .await
        .unwrap();
    let r = read(&paths, &id).unwrap();
    assert_eq!(r.phase, "complete", "{r:?}");
    let keys = pane.keys_sent();
    assert_eq!(
        keys.iter().filter(|k| *k == "backspace").count(),
        "/model claude-fable-5-2".len()
    );
}
