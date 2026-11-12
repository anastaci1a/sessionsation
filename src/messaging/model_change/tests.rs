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
    let d = dialog(&bare).unwrap();
    assert_eq!((d.kind, d.pointer), (Kind::Effort, Pointer::No));
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
const M1: &str = "11111111-1111-4111-8111-111111111111";

#[tokio::test]
async fn codex_change_waits_for_idle_applies_model_then_effort_and_verifies() {
    let w = world().await;
    w.fake.lock().unwrap().busy = true;
    let op = w.set_model("gpt-6-astra", Some("high")).unwrap();
    let id = op["id"].as_str().unwrap().to_owned();
    assert_eq!(op["phase"], "waiting");
    assert_eq!(op["kind"], "model_change");
    tick(&w.paths).await;
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
    tick(&w.paths).await;
    let r = w.record(&id);
    assert_eq!(r.phase, "complete", "{r:?}");
    assert_eq!(r.applied.as_ref().unwrap().model, "gpt-6-astra");
    assert_eq!(r.applied.as_ref().unwrap().effort.as_deref(), Some("high"));
    assert_eq!(r.before.as_ref().unwrap().model, "gpt-6");
    // Model first, then effort (a model switch may reset it), then the held input on the new model.
    tick(&w.paths).await;
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
    tick(&w.paths).await;
    let r = w.record(&id);
    assert_eq!(r.phase, "complete");
    assert_eq!(r.effort_source, "preserved");
    // The effort in force before ("medium") is re-applied after the model reset it.
    assert_eq!(r.applied.unwrap().effort.as_deref(), Some("medium"));
    let again = w.set_model("gpt-6-astra", Some("medium")).unwrap();
    let id2 = again["id"].as_str().unwrap().to_owned();
    tick(&w.paths).await;
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
    tick(&w.paths).await;
    let r = w.record(op["id"].as_str().unwrap());
    assert_eq!(
        (r.phase.as_str(), r.reason.as_deref()),
        ("failed", Some("model_not_available"))
    );
    // Effort the model does not offer.
    let op = w.set_model("gpt-6-astra", Some("low")).unwrap();
    tick(&w.paths).await;
    assert_eq!(
        w.record(op["id"].as_str().unwrap()).reason.as_deref(),
        Some("effort_unsupported")
    );
    // The app-server refuses the update.
    w.fake.lock().unwrap().reject_updates = true;
    let op = w.set_model("gpt-6-astra", Some("high")).unwrap();
    tick(&w.paths).await;
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
        tick(&w.paths).await;
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
    tick(&w.paths).await;
    assert_eq!(w.updates(), [format!("add {M1} on gpt-6")]);
    assert!(
        operations(&w.paths)
            .unwrap()
            .iter()
            .any(|o| o["id"] == id.as_str() && o["kind"] == "model_change")
    );
}
