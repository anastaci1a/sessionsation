//! Deliberate in-turn steering. No queue fallback, launch or reply attribution.
use super::*;
use std::time::Duration;

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Intent {
    pub version: u32,
    pub expected_turn_id: String,
    pub expected_runtime_generation: String,
}
impl Intent {
    pub(super) fn validate(&self) -> anyhow::Result<()> {
        anyhow::ensure!(self.version == 1, "UNSUPPORTED_STEERING_VERSION");
        anyhow::ensure!(
            !self.expected_turn_id.is_empty()
                && self.expected_turn_id.len() <= 256
                && !self.expected_turn_id.chars().any(char::is_control),
            "INVALID_ACTIVE_TURN"
        );
        anyhow::ensure!(
            self.expected_runtime_generation.len() == 64
                && self
                    .expected_runtime_generation
                    .bytes()
                    .all(|b| b.is_ascii_digit() || (b'a'..=b'f').contains(&b)),
            "INVALID_RUNTIME_GENERATION"
        );
        Ok(())
    }
}
#[derive(Deserialize)]
struct Read {
    thread: Thread,
}
#[derive(Deserialize)]
struct Thread {
    id: String,
    status: Status,
}
#[derive(Deserialize, PartialEq)]
#[serde(rename_all = "camelCase")]
struct Status {
    r#type: String,
    #[serde(default)]
    active_flags: Vec<String>,
}
#[derive(Deserialize)]
struct Page {
    data: Vec<Turn>,
}
#[derive(Deserialize)]
struct Turn {
    id: String,
    status: String,
}
async fn status(c: &mut Transport, id: &str) -> Result<Status> {
    let r: Read = c
        .request("thread/read", json!({"threadId":id,"includeTurns":false}))
        .await?;
    if r.thread.id != id {
        return Err(Fault("correlation_changed"));
    }
    if !r.thread.status.active_flags.is_empty() {
        return Err(Fault("approval_or_user_input_required"));
    }
    if r.thread.status.r#type != "active" {
        return Err(Fault("steer_requires_active_turn"));
    }
    Ok(r.thread.status)
}
async fn active_turn(c: &mut Transport, id: &str) -> Result<String> {
    let before = status(c, id).await?;
    let page: Page = c.passive_summary(id).await?;
    let turn = page
        .data
        .first()
        .filter(|t| {
            page.data.len() == 1
                && t.status == "inProgress"
                && !t.id.is_empty()
                && t.id.len() <= 256
                && !t.id.chars().any(char::is_control)
        })
        .ok_or(Fault("active_turn_unverified"))?;
    if status(c, id).await? != before {
        return Err(Fault("state_changed"));
    }
    Ok(turn.id.clone())
}
async fn connect_active(a: &Attachment) -> Result<(Transport, String)> {
    tokio::time::timeout(Duration::from_secs(5), async {
        let mut c = a.connect().await?;
        let turn = active_turn(&mut c, &a.id).await?;
        Ok((c, turn))
    })
    .await
    .map_err(|_| Fault("observation_timeout"))?
}
fn permitted(paths: &Paths, id: &str) -> Result<()> {
    if !modes::queue::sending_enabled(paths)? {
        return Err(Fault("native_sending_disabled"));
    }
    if external::messaging_stop_intent(paths, id)
        .map_err(|_| Fault("discovery_unavailable"))?
        .is_some()
    {
        return Err(Fault("explicit_resume_required"));
    }
    Ok(())
}
/// Explicit, bounded read-only lookup. Ordinary polling does not reconstruct busy history.
pub async fn target(paths: &Paths, id: &str, generation: &str) -> anyhow::Result<Value> {
    let s = crate::catalog::resolve(paths, id, generation)?;
    anyhow::ensure!(s.provider == "codex", "UNSUPPORTED_PROVIDER_OPERATION");
    anyhow::ensure!(
        !matches!(s.account.as_str(), "external" | "unknown")
            && !crate::accounts::removed(paths, &s.account)?,
        "ACCOUNT_UNAVAILABLE"
    );
    let a = readiness::recorded(paths, &s)?.ok_or(Fault("no_live_attachment"))?;
    let runtime = a.generation();
    let checked = async {
        let _guard = mutation_lock(paths, &a.id)?;
        permitted(paths, &a.id)?;
        let (_, turn) = connect_active(&a).await?;
        Ok::<_, Fault>(turn)
    }
    .await;
    crate::catalog::resolve(paths, id, generation)?;
    anyhow::ensure!(
        readiness::recorded(paths, &s)?
            .as_ref()
            .map(Attachment::generation)
            == Some(runtime.clone()),
        "STALE_TARGET"
    );
    let now = crate::daemon::now();
    Ok(
        json!({"version":1,"session_id":id,"generation":generation,"runtime_generation":runtime,"active_turn_id":checked.as_ref().ok(),"available":checked.is_ok(),"reason":checked.as_ref().err().map(|e|e.0),"checked_at":now,"observed_at":if checked.is_ok(){Some(now)}else{None},"valid_until":if checked.is_ok(){Some(now+5)}else{None},"sends_input":false,"starts_process":false,"acceptance_only":true,"reply_correlation":"unsupported"}),
    )
}

pub(super) fn accept(db: &mut Ledger, b: &Binding) -> Result<Receipt> {
    let hash = format!(
        "{:x}",
        Sha256::digest(serde_json::to_vec(&b.request).map_err(|_| Fault("invalid_request"))?)
    );
    db.accept_bound(
        &b.native,
        crate::daemon::now(),
        Some((
            &hash,
            "public_steer",
            Some(SubmitEvidence {
                expected_turn_id: b
                    .request
                    .steering
                    .as_ref()
                    .ok_or(Fault("invalid_request"))?
                    .expected_turn_id
                    .clone(),
                acknowledged_turn_id: None,
                action: Some("public_steer".into()),
                resume: false,
            }),
        )),
    )
}
pub(super) async fn deliver(
    paths: &Paths,
    b: &Binding,
    db: &mut Ledger,
    r: &mut Receipt,
) -> anyhow::Result<()> {
    // Even a lost acknowledgement or a process restart must not repeat steering.
    if r.state != "pending_local" {
        return Ok(());
    }
    let intent = b
        .request
        .steering
        .as_ref()
        .context("STEERING_INTENT_REQUIRED")?;
    let _guard = mutation_lock(paths, &b.native.session_id)?;
    let session =
        crate::catalog::resolve(paths, &b.request.session_id, &b.request.expected_generation)?;
    anyhow::ensure!(
        !matches!(session.account.as_str(), "external" | "unknown")
            && !crate::accounts::removed(paths, &session.account)?,
        "ACCOUNT_UNAVAILABLE"
    );
    let a = attachment(paths, &b.native.session_id)?;
    anyhow::ensure!(
        a.generation() == b.native.expected_generation
            && a.generation() == intent.expected_runtime_generation,
        "RUNTIME_GENERATION_MISMATCH"
    );
    permitted(paths, &a.id)?;
    let (mut c, turn) = connect_active(&a).await?;
    anyhow::ensure!(turn == intent.expected_turn_id, "ACTIVE_TURN_CHANGED");
    crate::catalog::resolve(paths, &b.request.session_id, &b.request.expected_generation)?;
    anyhow::ensure!(
        attachment(paths, &a.id)?.generation() == intent.expected_runtime_generation,
        "RUNTIME_GENERATION_MISMATCH"
    );
    db.expire(r, crate::daemon::now())?;
    if r.state != "pending_local" {
        return Ok(());
    }
    r.state = "unknown".into();
    r.reason = Some("steer_dispatch_intent".into());
    r.submit = Some(SubmitEvidence {
        expected_turn_id: turn.clone(),
        acknowledged_turn_id: None,
        action: Some("public_steer".into()),
        resume: false,
    });
    db.save(r)?; // Durable uncertainty precedes the first possible native write.
    match c
        .steer(&a.id, &b.request.message_id, &turn, &b.request.text)
        .await
    {
        Ok(ack) => {
            r.state = "native_accepted".into();
            r.reason = None;
            r.submit.as_mut().unwrap().acknowledged_turn_id = Some(ack);
        }
        Err(e) => {
            r.reason = Some(e.0.into());
        } // A generic rejection cannot prove non-consumption.
    }
    db.save(r)?;
    Ok(())
}

pub(super) fn unsubmitted(b: &Binding, state: &str) -> Value {
    project(b, state, None, None)
}
pub(super) fn receipt(b: &Binding, r: &Receipt) -> Value {
    project(b, &r.state, r.reason.as_deref(), r.submit.as_ref())
}
fn project(
    b: &Binding,
    state: &str,
    reason: Option<&str>,
    evidence: Option<&SubmitEvidence>,
) -> Value {
    let ack = evidence.and_then(|e| e.acknowledged_turn_id.as_ref());
    json!({"message_id":b.request.message_id,"session_id":b.request.session_id,"accepted_generation":b.request.expected_generation,"accepted_runtime_generation":b.native.expected_generation,"provider":"codex","provider_conversation_id":b.native.session_id,"mode":"steer","delivery_semantics":"deliberate_in_turn_steer","state":if state=="unknown" {"delivery_unknown"} else {state},"reason":reason,"expected_turn_id":b.request.steering.as_ref().map(|i|&i.expected_turn_id),"acknowledged_turn_id":ack,"acceptance_only":true,"reply_correlation":"unsupported","facts":{"accepted_local":true,"accepted_native":ack.is_some(),"native_acceptance":if ack.is_some(){"accepted"}else if state=="unknown"{"unknown"}else{"not_attempted"},"input_recorded":false,"input_consumed":"unknown","turn_completed":"unknown","reply_observed":false},"native_queue_id":null,"correlation":null,"reply":null,"observation":{"state":"acceptance_only","checked_at":crate::daemon::now(),"native_attempted_at":null,"error_code":null,"freshness":"unknown","evidence_observed_at":null}})
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::messaging::tests::{attachment_fixture, bind, fixture};
    use futures_util::{SinkExt, StreamExt};
    use std::sync::{Arc, Mutex};
    use tokio::net::UnixListener;
    use tokio_tungstenite::tungstenite::Message;
    async fn scripted_server(
        path: &std::path::Path,
        handler: impl Fn(&Value) -> Option<Value> + Send + 'static,
    ) -> tokio::task::JoinHandle<()> {
        let listener = UnixListener::bind(path).unwrap();
        tokio::spawn(async move {
            loop {
                let (stream, _) = listener.accept().await.unwrap();
                let mut socket = tokio_tungstenite::accept_async(stream).await.unwrap();
                while let Some(Ok(Message::Text(text))) = socket.next().await {
                    let req: Value = serde_json::from_str(&text).unwrap();
                    if req["method"] == "initialized" {
                        continue;
                    }
                    let result = if req["method"] == "initialize" {
                        Some(json!({"userAgent":"test-originator/0.163.0 (Linux)"}))
                    } else {
                        handler(&req)
                    };
                    let Some(result) = result else {
                        break;
                    };
                    if socket
                        .send(Message::Text(
                            json!({"id":req["id"],"result":result}).to_string().into(),
                        ))
                        .await
                        .is_err()
                    {
                        break;
                    }
                }
            }
        })
    }
    #[tokio::test]
    async fn explicit_steering_is_exact_turn_acceptance_only_and_never_replayed() {
        for host in ["%1", "herdr/fixture/w1:p1"] {
            for scenario in [
                "success",
                "idle",
                "turn_changed",
                "runtime_changed",
                "catalog_changed",
                "lost_ack",
                "wrong_ack",
                "stopped",
                "migration",
                "restart_after_intent",
                "restart_before_intent",
                "runtime_race",
            ] {
                let (_tmp, paths, mut input, _) = fixture();
                input.expires_at = crate::daemon::now() + 120;
                let mut a = attachment_fixture(&paths, &input);
                bind(&paths, &mut a);
                let calls = Arc::new(Mutex::new(Vec::new()));
                let seen = calls.clone();
                let native = a.id.clone();
                let client = input.message_id.clone();
                let binding_path = paths.root.join("managed-panes/1.json");
                let server=scripted_server(&a.endpoint(),move |req| {
                    let method=req["method"].as_str().unwrap(); seen.lock().unwrap().push(method.to_owned());
                    if method == "thread/turns/list" && scenario == "runtime_race" { let _ = fs::remove_file(&binding_path); }
                    match method {
                        "thread/read"=>Some(json!({"thread":{"id":native,"status":{"type":if scenario=="idle"{"idle"}else{"active"}}}})),
                        "thread/turns/list"=>Some(json!({"data":[{"id":if scenario=="turn_changed"{"turn-other"}else{"turn-active"},"status":"inProgress"}]})),
                        "turn/steer"=>{
                            assert_eq!(req["params"]["expectedTurnId"],"turn-active");
                            assert_eq!(req["params"]["clientUserMessageId"],client);
                            assert_eq!(req["params"]["threadId"],native);
                            if scenario=="lost_ack"{None}else{Some(json!({"turnId":if scenario=="wrong_ack"{"turn-other"}else{"turn-active"}}))}
                        },
                        _=>panic!("Forbidden RPC {method}"),
                    }
                }).await;
                let source:external::Instance=serde_json::from_value(json!({"pane":host,"tmux_session":"fixture","pid":a.pid,"start_time":a.start,"cwd":"/tmp","thread_id":a.id,"rollout":"/tmp/unused","home":a.home,"binary":"fixture","resume_options":[],"account":"selected"})).unwrap();
                let s = crate::catalog::reconcile(&paths, &[source])
                    .unwrap()
                    .remove(0);
                let request = PublicRequest {
                    api_version: 1,
                    message_id: input.message_id.clone(),
                    session_id: s.id.clone(),
                    expected_generation: s.generation(),
                    mode: PublicMode::Steer,
                    text: input.text.clone(),
                    expires_at: input.expires_at,
                    recovery: None,
                    steering: Some(Intent {
                        version: 1,
                        expected_turn_id: "turn-active".into(),
                        expected_runtime_generation: if scenario == "runtime_changed" {
                            "0".repeat(64)
                        } else {
                            a.generation()
                        },
                    }),
                };
                if scenario == "stopped" {
                    fs::create_dir_all(paths.root.join("conversations")).unwrap();
                    fs::write(
                        paths
                            .root
                            .join("conversations")
                            .join(format!("{}.json", a.id)),
                        json!({"stop_intent":"stop-1"}).to_string(),
                    )
                    .unwrap();
                }
                let migration = if scenario == "migration" {
                    Some(mutation_lock(&paths, &a.id).unwrap())
                } else {
                    None
                };
                if scenario == "catalog_changed" {
                    crate::catalog::rename(&paths, &s.id, &s.generation(), "changed").unwrap();
                }
                if matches!(scenario, "restart_after_intent" | "restart_before_intent") {
                    let mut db = Ledger::open(&paths).unwrap();
                    initialize(&db).unwrap();
                    let b = Binding {
                        cancelled: false,
                        evidence_observed_at: None,
                        request: request.clone(),
                        native: Enqueue {
                            expected_generation: a.generation(),
                            ..input.clone()
                        },
                    };
                    db.db
                        .execute(
                            "INSERT INTO public_messages VALUES (?,?)",
                            params![request.message_id, serde_json::to_string(&b).unwrap()],
                        )
                        .unwrap();
                    assert_eq!(
                        db.accept(&b.native, crate::daemon::now()).unwrap_err().0,
                        "message_id_conflict",
                        "legacy must not adopt a bound public intent"
                    );
                    if scenario == "restart_after_intent" {
                        let mut r = accept(&mut db, &b).unwrap();
                        r.state = "unknown".into();
                        r.submit = Some(SubmitEvidence {
                            expected_turn_id: "turn-active".into(),
                            acknowledged_turn_id: None,
                            action: Some("public_steer".into()),
                            resume: false,
                        });
                        db.save(&r).unwrap();
                    }
                }
                if matches!(scenario, "success" | "idle") {
                    let hint = target(&paths, &s.id, &s.generation()).await.unwrap();
                    assert_eq!(
                        hint["available"],
                        scenario == "success",
                        "{host} {scenario}: {hint}"
                    );
                    assert_eq!(hint["sends_input"], false);
                    if scenario == "success" {
                        assert_eq!(hint["active_turn_id"], "turn-active");
                    }
                }
                let first = send_public(&paths, request.clone()).await;
                if scenario == "catalog_changed" {
                    assert!(first.unwrap_err().to_string().contains("STALE_TARGET"));
                    server.abort();
                    continue;
                }
                let first = first.unwrap();
                assert_eq!(first["delivery_semantics"], "deliberate_in_turn_steer");
                assert_eq!(
                    first["facts"]["accepted_native"],
                    matches!(scenario, "success" | "restart_before_intent"),
                    "{host} {scenario}: {first}"
                );
                assert_eq!(first["facts"]["input_recorded"], false);
                assert_eq!(first["reply_correlation"], "unsupported");
                assert!(first["reply"].is_null() && first["correlation"].is_null());
                assert_eq!(
                    first["state"],
                    match scenario {
                        "success" | "restart_before_intent" => "native_accepted",
                        "lost_ack" | "wrong_ack" | "restart_after_intent" => "delivery_unknown",
                        _ => "rejected",
                    }
                );
                let count = calls.lock().unwrap().len();
                let read = receipt_public(&paths, &request.message_id, false)
                    .await
                    .unwrap();
                assert_eq!(read["state"], first["state"]);
                assert_eq!(
                    calls.lock().unwrap().len(),
                    count,
                    "receipt must not borrow a turn reply"
                );
                send_public(&paths, request.clone()).await.unwrap();
                let mut conflict = request.clone();
                conflict.steering.as_mut().unwrap().expected_turn_id = "changed".into();
                assert!(
                    send_public(&paths, conflict)
                        .await
                        .unwrap_err()
                        .to_string()
                        .contains("MESSAGE_ID_CONFLICT")
                );
                assert_eq!(
                    calls
                        .lock()
                        .unwrap()
                        .iter()
                        .filter(|m| m.as_str() == "turn/steer")
                        .count(),
                    usize::from(matches!(
                        scenario,
                        "success" | "restart_before_intent" | "lost_ack" | "wrong_ack"
                    ))
                );
                if matches!(
                    scenario,
                    "success" | "lost_ack" | "wrong_ack" | "restart_after_intent"
                ) {
                    assert!(
                        receipt_public(&paths, &request.message_id, true)
                            .await
                            .unwrap_err()
                            .to_string()
                            .contains("STEERING_CANCELLATION_UNSUPPORTED")
                    );
                }
                drop(migration);
                server.abort();
            }
        }
    }
}
