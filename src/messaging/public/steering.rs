//! Deliberate in-turn steering. No queue fallback, launch or automatic replay.
//! Replies are attributed after the steer's own native input item (revision 15).
use super::*;
use std::time::Duration;
mod reply;

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
        let mut c = a.connect_steer().await?;
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
        json!({"version":1,"session_id":id,"generation":generation,"runtime_generation":runtime,"active_turn_id":checked.as_ref().ok(),"available":checked.is_ok(),"reason":checked.as_ref().err().map(|e|e.0),"checked_at":now,"observed_at":if checked.is_ok(){Some(now)}else{None},"valid_until":if checked.is_ok(){Some(now+5)}else{None},"sends_input":false,"starts_process":false,"acceptance_only":false,"reply_correlation":"after_steer_item"}),
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
    // Item positions before the steer; a failed read is still provably unsent.
    let anchor = reply::anchor(&mut c, &a.id, &turn, &b.request.message_id).await?;
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
    r.steer = Some(SteerEvidence {
        anchor: Some(anchor),
        ..Default::default()
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
            if matches!(
                e.0,
                "compact_turn_not_steerable" | "review_turn_not_steerable"
            ) {
                // Verified native pre-admission rejection, not an uncertain injection.
                r.state = "rejected".into();
            }
            r.reason = Some(e.0.into());
        } // A generic rejection cannot prove non-consumption.
    }
    db.save(r)?;
    Ok(())
}

pub(super) fn unsubmitted(b: &Binding, state: &str) -> Value {
    let r = Receipt {
        recovery_start: None,
        message_id: b.request.message_id.clone(),
        session_id: b.native.session_id.clone(),
        generation: b.native.expected_generation.clone(),
        state: state.into(),
        native_queue_id: None,
        reason: None,
        correlation: None,
        reply: None,
        submit: None,
        steer: None,
    };
    receipt(b, &r)
}
pub(super) fn receipt(b: &Binding, r: &Receipt) -> Value {
    project(b, r, json!({"state":"acceptance_only"}))
}
/// Whether a receipt can still gain native item evidence by observation.
pub(super) fn observable(r: &Receipt) -> bool {
    r.submit.is_some()
        && matches!(
            r.state.as_str(),
            "unknown" | "native_accepted" | "execution_started"
        )
}
pub(super) use reply::snapshot;
/// Terminal durable evidence is never regressed by a later observation.
pub(super) fn commit(db: &mut Ledger, r: &mut Receipt, observed: Receipt) -> Result<()> {
    if reply::terminal(&r.state) {
        return Ok(());
    }
    if *r != observed {
        db.save(&observed)?;
        *r = observed;
    }
    Ok(())
}
pub(super) fn observed(b: &Binding, r: &Receipt, observation: Value) -> Value {
    project(b, r, observation)
}
fn project(b: &Binding, r: &Receipt, observation: Value) -> Value {
    let state = r.state.as_str();
    let ack = r
        .submit
        .as_ref()
        .and_then(|e| e.acknowledged_turn_id.as_ref());
    let steer = r.steer.clone().unwrap_or_default();
    let recorded = steer.input_item_id.is_some();
    let not_consumed = r.reason.as_deref() == Some("input_not_consumed");
    let final_reply = state == "completed_with_reply";
    let now = crate::daemon::now();
    let freshness = match b.evidence_observed_at {
        Some(at) if at <= now && now < at + 5 => "fresh",
        Some(_) => "stale",
        None => "unknown",
    };
    let obs = observation["state"].as_str().unwrap_or("acceptance_only");
    json!({"message_id":b.request.message_id,"session_id":b.request.session_id,
        "accepted_generation":b.request.expected_generation,"accepted_runtime_generation":b.native.expected_generation,
        "provider":"codex","provider_conversation_id":b.native.session_id,"mode":"steer",
        "delivery_semantics":"deliberate_in_turn_steer","state":if state=="unknown" {"delivery_unknown"} else {state},
        "reason":r.reason,"expected_turn_id":b.request.steering.as_ref().map(|i|&i.expected_turn_id),
        "acknowledged_turn_id":ack,"acceptance_only":false,"reply_correlation":"after_steer_item",
        "facts":{"accepted_local":true,"accepted_native":ack.is_some()||recorded,
            "native_acceptance":if ack.is_some(){"accepted"}else if recorded{"input_observed"}else if state=="unknown"{"unknown"}else{"not_attempted"},
            "input_recorded":recorded,
            "input_consumed":if not_consumed {json!(false)} else if recorded && (steer.agent_items_after>0 || steer.turn_status.as_deref()==Some("completed")) {json!(true)} else {json!("unknown")},
            "turn_completed":match steer.turn_status.as_deref() {Some("completed")=>json!(true),Some(_)=>json!(false),None=>json!("unknown")},
            "turn_status":steer.turn_status,"reply_observed":r.reply.is_some(),"reply_final":final_reply,
            "shared_with_turn_tail":steer.shared_with_turn_tail},
        "native_queue_id":null,
        "correlation":if recorded {json!({"native_turn_id":r.submit.as_ref().map(|s|&s.expected_turn_id),
            "steer_input_item_id":steer.input_item_id,"completion_input_item_id":steer.completed_input_item_id,"next_input_item_id":steer.next_input_item_id,
            "reply_item_id":r.reply.as_ref().map(|x|&x.item_id),"observed_runtime_generation":steer.observed_generation,
            "anchor":steer.anchor.as_ref().map(|a|json!({"user_items_before":a.user_items_before,"items_before":a.items_before,"last_agent_item_id":a.last_agent_item_id}))})} else {Value::Null},
        "reply":r.reply.as_ref().map(|x|json!({"item_id":x.item_id,"text":x.text,"phase":x.phase,"final":final_reply,"shared_with_turn_tail":steer.shared_with_turn_tail})),
        "observation":{"state":obs,"checked_at":now,
            "native_attempted_at":if matches!(obs,"observed"|"uncorrelated"|"unavailable") {Some(now)} else {None},
            "error_code":observation["code"],"freshness":if obs=="acceptance_only" {"unknown"} else {freshness},
            "evidence_observed_at":b.evidence_observed_at}})
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
                        .send(Message::Text(if result["fixture_error"].is_object() {
                            json!({"id":req["id"],"error":result["fixture_error"]})
                                .to_string()
                                .into()
                        } else {
                            json!({"id":req["id"],"result":result}).to_string().into()
                        }))
                        .await
                        .is_err()
                    {
                        break;
                    }
                }
            }
        })
    }
    /// Full-view turn in the live-verified 0.165 shape (synthetic text).
    fn full_turn(id: &str, status: &str, steer: Option<&str>) -> Value {
        let mut items = vec![
            json!({"type":"userMessage","id":"u-0","clientId":null,"content":[{"type":"text","text":"PRIVATE prompt","text_elements":[]}]}),
            json!({"type":"reasoning","id":"r-0","summary":[],"content":["PRIVATE reasoning"]}),
            json!({"type":"agentMessage","id":"msg_before","text":"working","phase":"commentary","memoryCitation":null,"delivery":null,"questions":null}),
        ];
        if let Some(client) = steer {
            items.push(json!({"type":"userMessage","id":"u-steer","clientId":client,"content":[{"type":"text","text":"PRIVATE steer","text_elements":[]}]}));
            items.push(json!({"type":"agentMessage","id":"msg_after","text":"done STEERED","phase":"final_answer","memoryCitation":null,"delivery":null,"questions":null}));
        }
        json!({"id":id,"status":status,"itemsView":"full","items":items,"error":null,"startedAt":1,"completedAt":null,"durationMs":null,"rootTurnId":null})
    }
    #[tokio::test]
    async fn explicit_steering_is_exact_turn_correlated_and_never_replayed() {
        for host in ["%1", "herdr/fixture/w1:p1"] {
            for scenario in [
                "success",
                "compact",
                "review",
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
                let binding_path = paths.root.join(crate::layout::MANAGED_PANES).join("1.json");
                let steered = Arc::new(Mutex::new(false));
                let server=scripted_server(&a.endpoint(),move |req| {
                    let method=req["method"].as_str().unwrap(); seen.lock().unwrap().push(if req["params"]["itemsView"]=="full" {"full".to_owned()} else {method.to_owned()});
                    if method == "thread/turns/list" && req["params"]["itemsView"]=="full" {
                        // Live 0.165 item shapes with synthetic text.
                        let recorded = *steered.lock().unwrap() && scenario != "wrong_ack";
                        return Some(json!({"data":[full_turn(if scenario=="turn_changed"{"turn-other"}else{"turn-active"}, if recorded {"completed"} else {"inProgress"}, recorded.then_some(client.as_str()))],"nextCursor":null}));
                    }
                    if method == "thread/turns/list" && scenario == "runtime_race" { let _ = fs::remove_file(&binding_path); }
                    match method {
                        "thread/read"=>Some(json!({"thread":{"id":native,"status":{"type":if scenario=="idle"{"idle"}else{"active"}}}})),
                        "thread/turns/list"=>Some(json!({"data":[{"id":if scenario=="turn_changed"{"turn-other"}else{"turn-active"},"status":"inProgress"}]})),
                        "turn/steer"=>{
                            assert_eq!(req["params"]["expectedTurnId"],"turn-active");
                            assert_eq!(req["params"]["clientUserMessageId"],client);
                            assert_eq!(req["params"]["threadId"],native);
                            *steered.lock().unwrap() = !matches!(scenario, "compact" | "review");
                            if matches!(scenario,"compact"|"review"){Some(json!({"fixture_error":{"code":-32600,"message":"PRIVATE native diagnostic","data":{"codexErrorInfo":{"activeTurnNotSteerable":{"turnKind":scenario}}}}}))}else if scenario=="lost_ack"{None}else{Some(json!({"turnId":if scenario=="wrong_ack"{"turn-other"}else{"turn-active"}}))}
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
                    fs::create_dir_all(paths.root.join(crate::layout::CONVERSATIONS)).unwrap();
                    fs::write(
                        paths
                            .root
                            .join(crate::layout::CONVERSATIONS)
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
                assert_eq!(first["reply_correlation"], "after_steer_item");
                assert_eq!(first["acceptance_only"], false);
                assert!(first["reply"].is_null() && first["correlation"].is_null());
                assert_eq!(
                    first["state"],
                    match scenario {
                        "success" | "restart_before_intent" => "native_accepted",
                        "lost_ack" | "wrong_ack" | "restart_after_intent" => "delivery_unknown",
                        _ => "rejected",
                    }
                );
                if matches!(scenario, "compact" | "review") {
                    assert_eq!(first["reason"], format!("{scenario}_turn_not_steerable"));
                    assert_eq!(first["facts"]["native_acceptance"], "not_attempted");
                    assert!(!first.to_string().contains("PRIVATE"));
                }
                let count = calls.lock().unwrap().len();
                let read = receipt_public(&paths, &request.message_id, false)
                    .await
                    .unwrap();
                let correlated =
                    matches!(scenario, "success" | "restart_before_intent" | "lost_ack");
                if correlated {
                    // The steer's own input item anchors the reply, even after a lost ack.
                    assert_eq!(read["state"], "completed_with_reply", "{scenario}: {read}");
                    assert_eq!(read["facts"]["input_recorded"], true);
                    assert_eq!(read["facts"]["accepted_native"], true);
                    assert_eq!(
                        read["facts"]["native_acceptance"],
                        if scenario == "lost_ack" {
                            "input_observed"
                        } else {
                            "accepted"
                        }
                    );
                    assert_eq!(read["facts"]["turn_completed"], true);
                    assert_eq!(read["facts"]["turn_status"], "completed");
                    assert_eq!(read["facts"]["shared_with_turn_tail"], true);
                    assert_eq!(read["reply"]["item_id"], "msg_after");
                    assert_eq!(read["reply"]["text"], "done STEERED");
                    assert_eq!(read["reply"]["final"], true);
                    assert_eq!(read["correlation"]["steer_input_item_id"], "u-steer");
                    assert_eq!(read["observation"]["state"], "observed");
                    assert!(!read.to_string().contains("PRIVATE"));
                } else {
                    assert_eq!(read["state"], first["state"]);
                    assert!(read["reply"].is_null());
                }
                let reads: Vec<String> = calls.lock().unwrap()[count..].to_vec();
                let observable =
                    correlated || matches!(scenario, "wrong_ack" | "restart_after_intent");
                assert!(
                    reads.iter().all(|m| m == "full") && reads.is_empty() != observable,
                    "{scenario}: receipt reads only the steered turn: {reads:?}"
                );
                let count = calls.lock().unwrap().len();
                let cached = receipt_public(&paths, &request.message_id, false)
                    .await
                    .unwrap();
                assert_eq!(cached["state"], read["state"]);
                if correlated {
                    assert_eq!(cached["observation"]["state"], "cached");
                    assert_eq!(
                        calls.lock().unwrap().len(),
                        count,
                        "terminal evidence is cached"
                    );
                }
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
                        "success"
                            | "restart_before_intent"
                            | "lost_ack"
                            | "wrong_ack"
                            | "compact"
                            | "review"
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
    fn user(id: &str, client: Option<&str>) -> Value {
        json!({"type":"userMessage","id":id,"clientId":client,"content":[{"type":"text","text":"PRIVATE","text_elements":[]}]})
    }
    fn agent(id: &str, text: &str, phase: &str) -> Value {
        json!({"type":"agentMessage","id":id,"text":text,"phase":phase,"memoryCitation":null,"delivery":null,"questions":null})
    }
    fn turn(id: &str, status: &str, items: Vec<Value>) -> Value {
        json!({"id":id,"status":status,"itemsView":"full","items":items})
    }
    /// Full-view responses per observation phase: 0 = before steer (anchor),
    /// 1 = first receipt read, 2 = second receipt read.
    fn script(case: &str, phase: u32, client: &str, cursor: &Value) -> Value {
        let base = || {
            vec![
                user("u-0", None),
                agent("msg_before", "working", "commentary"),
            ]
        };
        let with = |mut extra: Vec<Value>| {
            let mut items = base();
            items.append(&mut extra);
            items
        };
        let page = |t: Value| json!({"data":[t],"nextCursor":null});
        match (case, phase) {
            ("anchor_mismatch", _) => page(turn("turn-next", "inProgress", base())),
            (_, 0) => page(turn("turn-active", "inProgress", base())),
            ("interrupted", n) => page(turn(
                "turn-active",
                if n == 1 { "inProgress" } else { "interrupted" },
                with(vec![
                    user("u-steer", Some(client)),
                    agent("msg_p", "partial", "commentary"),
                ]),
            )),
            ("not_consumed", n) => page(turn(
                "turn-active",
                if n == 1 { "inProgress" } else { "completed" },
                base(),
            )),
            ("two_steers", _) => page(turn(
                "turn-active",
                "completed",
                with(vec![
                    user("u-steer", Some(client)),
                    agent("msg_mine", "mine", "final_answer"),
                    user("u-later", Some("33333333-3333-4333-8333-333333333333")),
                    agent("msg_tail", "tail", "final_answer"),
                ]),
            )),
            ("other_turn", _) if cursor.is_null() => {
                json!({"data":[turn("turn-new","inProgress",vec![user("u-steer", Some(client))])],"nextCursor":"c1"})
            }
            ("other_turn", _) => page(turn("turn-active", "completed", base())),
            _ => unreachable!(),
        }
    }
    #[tokio::test]
    async fn steer_reply_progresses_and_classifies_terminal_outcomes() {
        for case in [
            "interrupted",
            "not_consumed",
            "two_steers",
            "anchor_mismatch",
            "other_turn",
        ] {
            let (_tmp, paths, mut input, _) = fixture();
            input.expires_at = crate::daemon::now() + 120;
            let mut a = attachment_fixture(&paths, &input);
            bind(&paths, &mut a);
            let native = a.id.clone();
            let client = input.message_id.clone();
            let phase = Arc::new(Mutex::new(0u32));
            let steers = Arc::new(Mutex::new(0u32));
            let (p, n) = (phase.clone(), steers.clone());
            let server = scripted_server(&a.endpoint(), move |req| {
                let full = req["params"]["itemsView"] == "full";
                match req["method"].as_str().unwrap() {
                    "thread/read" => {
                        Some(json!({"thread":{"id":native,"status":{"type":"active"}}}))
                    }
                    "thread/turns/list" if full => Some(script(
                        case,
                        *p.lock().unwrap(),
                        &client,
                        &req["params"]["cursor"],
                    )),
                    "thread/turns/list" => {
                        Some(json!({"data":[{"id":"turn-active","status":"inProgress"}]}))
                    }
                    "turn/steer" => {
                        *n.lock().unwrap() += 1;
                        Some(json!({"turnId":"turn-active"}))
                    }
                    m => panic!("Forbidden RPC {m}"),
                }
            })
            .await;
            let source:external::Instance=serde_json::from_value(json!({"pane":"%1","tmux_session":"fixture","pid":a.pid,"start_time":a.start,"cwd":"/tmp","thread_id":a.id,"rollout":"/tmp/unused","home":a.home,"binary":"fixture","resume_options":[],"account":"selected"})).unwrap();
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
                    expected_runtime_generation: a.generation(),
                }),
            };
            let first = send_public(&paths, request.clone()).await.unwrap();
            if case == "anchor_mismatch" {
                assert_eq!(first["state"], "rejected", "{first}");
                assert_eq!(first["reason"], "active_turn_changed");
                assert_eq!(*steers.lock().unwrap(), 0, "no steer without an anchor");
                server.abort();
                continue;
            }
            assert_eq!(first["state"], "native_accepted");
            *phase.lock().unwrap() = 1;
            let id = &request.message_id;
            let r1 = receipt_public(&paths, id, false).await.unwrap();
            *phase.lock().unwrap() = 2;
            let r2 = receipt_public(&paths, id, false).await.unwrap();
            assert!(!r2.to_string().contains("PRIVATE"));
            match case {
                "interrupted" => {
                    assert_eq!(r1["state"], "execution_started", "{r1}");
                    assert_eq!(r1["reply"]["text"], "partial");
                    assert_eq!(r1["reply"]["final"], false);
                    assert_eq!(r1["facts"]["input_consumed"], true);
                    assert_eq!(r1["facts"]["turn_completed"], false);
                    assert_eq!(r1["facts"]["turn_status"], "inProgress");
                    assert_eq!(
                        r1["correlation"]["anchor"]["last_agent_item_id"],
                        "msg_before"
                    );
                    assert_eq!(r1["correlation"]["anchor"]["user_items_before"], 1);
                    assert_eq!(r2["state"], "failed");
                    assert_eq!(r2["reason"], "steer_turn_interrupted");
                    assert_eq!(r2["reply"]["text"], "partial");
                    assert_eq!(r2["reply"]["final"], false);
                    assert_eq!(r2["facts"]["turn_status"], "interrupted");
                }
                "not_consumed" => {
                    assert_eq!(r1["state"], "native_accepted");
                    assert_eq!(r1["observation"]["state"], "uncorrelated");
                    assert_eq!(r2["state"], "failed", "{r2}");
                    assert_eq!(r2["reason"], "input_not_consumed");
                    assert_eq!(r2["facts"]["input_recorded"], false);
                    assert_eq!(r2["facts"]["input_consumed"], false);
                    assert!(r2["reply"].is_null() && r2["correlation"].is_null());
                }
                "two_steers" => {
                    assert_eq!(r1["state"], "completed_with_reply", "{r1}");
                    assert_eq!(r1["reply"]["item_id"], "msg_mine");
                    assert_eq!(r1["reply"]["shared_with_turn_tail"], false);
                    assert_eq!(r1["correlation"]["next_input_item_id"], "u-later");
                    assert_eq!(r2["observation"]["state"], "cached");
                }
                "other_turn" => {
                    assert_eq!(r1["state"], "native_accepted", "{r1}");
                    assert_eq!(r1["observation"]["error_code"], "correlation_changed");
                    assert!(r1["reply"].is_null());
                }
                _ => unreachable!(),
            }
            assert_eq!(*steers.lock().unwrap(), 1, "observation never resends");
            server.abort();
        }
    }
}
