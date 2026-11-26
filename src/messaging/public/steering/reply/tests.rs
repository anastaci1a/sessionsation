use super::*;
use crate::messaging::tests::{attachment_fixture, fixture, scripted_server};
use std::sync::{Arc as SArc, Mutex as SMutex};
const TURN: &str = "turn-big";
/// A live-shaped full in-progress turn whose tool output pushes one native
/// frame past `bytes`; only the small user and agent items matter.
fn big_turn(bytes: usize) -> Value {
    json!({"data":[{"id":TURN,"status":"inProgress","itemsView":"full","items":[
        {"type":"userMessage","id":"u0","clientId":null,"content":[{"type":"text","text":"start"}]},
        {"type":"commandExecution","id":"c0","aggregatedOutput":"x".repeat(bytes),"command":"cat big"},
        {"type":"plan","id":"p0","text":"plan"},
        {"type":"agentMessage","id":"a0","text":"working","phase":"commentary"}
    ]}],"nextCursor":null})
}
async fn steer_against(
    page: Value,
    steer: bool,
) -> (Result<SteerAnchor>, Result<String>, Vec<String>) {
    let (_dir, paths, input, _) = fixture();
    let a = attachment_fixture(&paths, &input);
    let id = a.id.clone();
    let calls = SArc::new(SMutex::new(Vec::new()));
    let seen = calls.clone();
    let server = scripted_server(&a.endpoint(), move |req| {
        let method = req["method"].as_str().unwrap().to_owned();
        seen.lock().unwrap().push(method.clone());
        match method.as_str() {
            "thread/turns/list" => {
                assert_eq!(req["params"]["itemsView"], "full");
                Some(page.clone())
            }
            "turn/steer" => Some(json!({"turnId":TURN})),
            _ => None,
        }
    })
    .await;
    let mut c = a.connect_steer().await.unwrap();
    let anchored = anchor(&mut c, &id, TURN, ME).await;
    let steered = if steer && anchored.is_ok() {
        c.steer(&id, ME, TURN, "hello").await
    } else {
        Err(Fault("not_sent"))
    };
    drop(c);
    server.abort();
    let calls = calls.lock().unwrap().clone();
    (anchored, steered, calls)
}
#[tokio::test]
async fn steer_to_a_turn_larger_than_one_mib_is_anchored_and_sent() {
    // localica-lead: the latest full turn was 1.86 MB on the 1 MiB connection.
    let (anchored, steered, calls) = steer_against(big_turn(3 * 1024 * 1024), true).await;
    let anchored = anchored.unwrap();
    assert_eq!((anchored.user_items_before, anchored.items_before), (1, 4));
    assert_eq!(anchored.last_agent_item_id.as_deref(), Some("a0"));
    assert_eq!(steered.unwrap(), TURN);
    assert_eq!(calls, ["thread/turns/list", "turn/steer"]);
}
#[tokio::test]
async fn ordinary_connection_keeps_its_one_mib_limit() {
    let (_dir, paths, input, _) = fixture();
    let a = attachment_fixture(&paths, &input);
    let id = a.id.clone();
    let page = big_turn(2 * 1024 * 1024);
    let server = scripted_server(&a.endpoint(), move |_| Some(page.clone())).await;
    let mut c = a.connect().await.unwrap();
    let e = anchor(&mut c, &id, TURN, ME).await.unwrap_err();
    assert_eq!(e.0, "native_frame_limit");
    server.abort();
}
#[tokio::test]
async fn frame_over_the_steer_bound_fails_closed_without_sending() {
    let over = crate::messaging::transport::STEER_FRAME_LIMIT + 1024;
    let (anchored, steered, calls) = steer_against(big_turn(over), true).await;
    assert_eq!(anchored.unwrap_err().0, "native_frame_limit");
    assert_eq!(steered.unwrap_err().0, "not_sent");
    assert_eq!(calls, ["thread/turns/list"]);
}
#[test]
fn projection_retains_only_identifiers_and_bounded_item_count() {
    let page: Page = serde_json::from_value(big_turn(1024 * 1024)).unwrap();
    let items = &page.data[0].items.items;
    assert!(!page.data[0].items.overflow);
    // Tool output and user content are skipped, never retained.
    let retained: usize = items
        .iter()
        .map(|i| {
            [&i.id, &i.client, &i.text, &i.phase]
                .iter()
                .map(|f| f.as_ref().map_or(0, |r| r.get().len()))
                .sum::<usize>()
        })
        .sum();
    assert!(retained < 256, "{retained}");
    let many = json!({"data":[{"id":TURN,"status":"inProgress","itemsView":"full",
        "items":vec![json!({"type":"sleep","id":"s"}); MAX_ITEMS + 5]}],"nextCursor":null});
    let page: Page = serde_json::from_value(many).unwrap();
    let t = page.data.into_iter().next().unwrap();
    assert!(t.items.overflow && t.items.items.len() == MAX_ITEMS);
    assert_eq!(project(t.items).err().unwrap().0, "steer_turn_too_large");
}
fn user(id: &str, client: Option<&str>) -> Item {
    Item::User(User {
        id: id.into(),
        client: client.map(Into::into),
    })
}
fn agent(id: &str, text: &str, phase: &str) -> Item {
    Item::Agent(Agent {
        id: id.into(),
        text: text.into(),
        phase: Some(phase.into()),
    })
}
fn receipt(state: &str, anchor: Option<SteerAnchor>) -> Receipt {
    Receipt {
        recovery_start: None,
        message_id: "11111111-1111-4111-8111-111111111111".into(),
        session_id: "22222222-2222-4222-8222-222222222222".into(),
        generation: "g".into(),
        state: state.into(),
        native_queue_id: None,
        reason: None,
        correlation: None,
        reply: None,
        native_error: None,
        timing: None,
        submit: Some(SubmitEvidence {
            expected_turn_id: "turn-1".into(),
            acknowledged_turn_id: Some("turn-1".into()),
            action: Some("public_steer".into()),
            resume: false,
        }),
        steer: Some(SteerEvidence {
            anchor,
            ..Default::default()
        }),
    }
}
const ME: &str = "11111111-1111-4111-8111-111111111111";
fn run(status: &str, items: Vec<Item>, r: &Receipt) -> Result<Option<Receipt>> {
    attribute(
        Turn {
            status: status.into(),
            items,
        },
        r,
        "gen",
    )
}
#[test]
fn profile_0_166_flat_item_timings_and_new_fields_project_unchanged() {
    // 0.166.0 adds optional flat `startedAtMs`/`completedAtMs` to every
    // ThreadItem and `generationId` to imageGeneration (generated schema diff);
    // the identifiers steer attribution reads are unchanged.
    let page: Page = serde_json::from_value(json!({"data":[{"id":"turn-1","status":"completed","itemsView":"full","startedAt":1,"completedAt":2,"durationMs":1000,"items":[
        {"type":"userMessage","id":"u0","clientId":null,"content":[{"type":"text","text":"start"}],"startedAtMs":1793305461849_i64,"completedAtMs":null},
        {"type":"imageGeneration","id":"ig0","status":"completed","revisedPrompt":null,"result":"","generationId":"gen-1","startedAtMs":null,"completedAtMs":null},
        {"type":"userMessage","id":"u1","clientId":ME,"content":[{"type":"text","text":"steer"}],"startedAtMs":1793305461900_i64,"completedAtMs":1793305461900_i64},
        {"type":"agentMessage","id":"msg_a","text":"done","phase":"final_answer","memoryCitation":null,"startedAtMs":1793305462000_i64,"completedAtMs":1793305463000_i64}
    ]}],"nextCursor":null})).unwrap();
    let t = page.data.into_iter().next().unwrap();
    let items = project(t.items).unwrap();
    let anchor = SteerAnchor {
        turn_id: "turn-1".into(),
        user_items_before: 1,
        items_before: 2,
        last_agent_item_id: None,
    };
    let o = run(
        "completed",
        items,
        &receipt("native_accepted", Some(anchor)),
    )
    .unwrap()
    .unwrap();
    assert_eq!(o.state, "completed_with_reply");
    assert_eq!(o.steer.unwrap().input_item_id.as_deref(), Some("u1"));
    assert_eq!(o.reply.unwrap().item_id, "msg_a");
}
#[test]
fn live_shape_single_steer_reply_is_shared_with_turn_tail() {
    // Live 0.165: [user(initial), user(steer clientId), agent(final_answer)];
    // the agent item streaming at steer time was discarded natively.
    let anchor = SteerAnchor {
        turn_id: "turn-1".into(),
        user_items_before: 1,
        items_before: 1,
        last_agent_item_id: None,
    };
    let r = receipt("native_accepted", Some(anchor));
    let items = || {
        vec![
            user("u0", Some("other")),
            user("u1", Some(ME)),
            agent("msg_a", "one two STEERED", "final_answer"),
        ]
    };
    let o = run("completed", items(), &r).unwrap().unwrap();
    assert_eq!(o.state, "completed_with_reply");
    let s = o.steer.unwrap();
    assert_eq!(s.input_item_id.as_deref(), Some("u1"));
    assert!(s.shared_with_turn_tail);
    assert_eq!(o.reply.unwrap().item_id, "msg_a");
    let o = run("inProgress", items(), &r).unwrap().unwrap();
    assert_eq!(o.state, "execution_started");
    let mut early = items();
    early.pop();
    let o = run("inProgress", early, &r).unwrap().unwrap();
    assert_eq!(o.state, "execution_started");
    assert!(o.reply.is_none() && !o.steer.unwrap().shared_with_turn_tail);
}
/// Live shape (steer 5e998050): anchored and first observed in progress
/// with legacy ids, then Codex rebuilt the finished turn with new counters.
fn renumbered_receipt() -> Receipt {
    let r: Receipt = serde_json::from_value(json!({
        "message_id":ME,"session_id":"22222222-2222-4222-8222-222222222222",
        "generation":"g","state":"execution_started","native_queue_id":null,"reason":null,
        "submit":{"expected_turn_id":"turn-1","acknowledged_turn_id":"turn-1","action":"public_steer"},
        "steer":{"anchor":{"turn_id":"turn-1","user_items_before":1,"items_before":4,"last_agent_item_id":"item-2"},
            "input_item_id":"item-3","turn_status":"inProgress","agent_items_after":0,
            "shared_with_turn_tail":false,"observed_generation":"gen"}
    }))
    .unwrap();
    r
}
fn renumbered_turn() -> Vec<Item> {
    vec![
        user("item-1", None),
        user("item-4", Some(ME)),
        // The old anchor counter now names the reply after the steer.
        agent("item-2", "done STEERED", "final_answer"),
    ]
}
#[test]
fn completion_renumbering_of_legacy_ids_reaches_a_terminal_reply() {
    let stuck = renumbered_receipt();
    let o = run("completed", renumbered_turn(), &stuck)
        .unwrap()
        .unwrap();
    assert_eq!(o.state, "completed_with_reply");
    let s = o.steer.clone().unwrap();
    assert_eq!(s.input_item_id.as_deref(), Some("item-3"));
    assert_eq!(s.completed_input_item_id.as_deref(), Some("item-4"));
    assert_eq!(s.anchor, stuck.steer.as_ref().unwrap().anchor);
    assert_eq!(o.reply.as_ref().unwrap().text, "done STEERED");
    // Re-observing the reconciled evidence is stable, never a new change.
    let again = run("completed", renumbered_turn(), &o).unwrap().unwrap();
    assert_eq!(again, o);
    // Interrupted turns reconcile the same way and stay an honest failure.
    let o = run("interrupted", renumbered_turn(), &stuck)
        .unwrap()
        .unwrap();
    assert_eq!(
        (o.state.as_str(), o.reason.as_deref()),
        ("failed", Some("steer_turn_interrupted"))
    );
}
#[test]
fn only_the_verified_completion_transition_reconciles_an_input_id() {
    let stuck = renumbered_receipt();
    // Still in progress: the same projection must keep the same id.
    assert_eq!(
        run("inProgress", renumbered_turn(), &stuck).unwrap_err().0,
        "correlation_changed"
    );
    // Non-legacy ids are stable identities; a change is never reconciled.
    let mut stable = stuck.clone();
    stable.steer.as_mut().unwrap().input_item_id = Some("u-3".into());
    let mut items = renumbered_turn();
    items[1] = user("u-4", Some(ME));
    assert_eq!(
        run("completed", items, &stable).unwrap_err().0,
        "correlation_changed"
    );
    let mut items = renumbered_turn();
    items[1] = user("u-4", Some(ME));
    assert_eq!(
        run("completed", items, &stuck).unwrap_err().0,
        "correlation_changed"
    );
    // A second renumbering after reconciliation is not accepted.
    let done = run("completed", renumbered_turn(), &stuck)
        .unwrap()
        .unwrap();
    let mut items = renumbered_turn();
    items[1] = user("item-7", Some(ME));
    assert_eq!(
        run("completed", items, &done).unwrap_err().0,
        "correlation_changed"
    );
    // The earlier-input count is still enforced on a renumbered turn.
    assert_eq!(
        run(
            "completed",
            vec![
                user("item-4", Some(ME)),
                agent("item-2", "x", "final_answer")
            ],
            &stuck
        )
        .unwrap_err()
        .0,
        "correlation_changed"
    );
}
#[test]
fn multiple_steers_are_bounded_by_the_next_input() {
    let r = receipt("native_accepted", None);
    let o = run(
        "completed",
        vec![
            user("u0", None),
            agent("a0", "before", "commentary"),
            user("u1", Some(ME)),
            Item::Other,
            agent("a1", "mine-commentary", "commentary"),
            agent("a2", "mine", "final_answer"),
            user("u2", Some("later-steer")),
            agent("a3", "tail", "final_answer"),
        ],
        &r,
    )
    .unwrap()
    .unwrap();
    assert_eq!(o.state, "completed_with_reply");
    assert_eq!(o.reply.as_ref().unwrap().item_id, "a2");
    let s = o.steer.unwrap();
    assert_eq!(s.next_input_item_id.as_deref(), Some("u2"));
    assert_eq!(s.agent_items_after, 2);
    assert!(!s.shared_with_turn_tail);
    // Steer immediately followed by another steer: no reply of its own.
    let o = run(
        "completed",
        vec![
            user("u1", Some(ME)),
            user("u2", Some("x")),
            agent("a", "t", "final_answer"),
        ],
        &r,
    )
    .unwrap()
    .unwrap();
    assert_eq!(o.state, "completed_without_reply");
    assert!(o.reply.is_none());
}
#[test]
fn interrupted_and_unconsumed_and_uncertain_outcomes() {
    let r = receipt("native_accepted", None);
    for status in ["interrupted", "failed"] {
        let o = run(
            status,
            vec![user("u1", Some(ME)), agent("a1", "partial", "commentary")],
            &r,
        )
        .unwrap()
        .unwrap();
        assert_eq!(o.state, "failed");
        assert_eq!(o.reason.as_deref(), Some("steer_turn_interrupted"));
        assert_eq!(o.reply.unwrap().text, "partial");
        let o = run(status, vec![user("u0", None)], &r).unwrap().unwrap();
        assert_eq!(o.reason.as_deref(), Some("input_not_consumed"));
    }
    let o = run("completed", vec![user("u0", None)], &r)
        .unwrap()
        .unwrap();
    assert_eq!(
        (o.state.as_str(), o.reason.as_deref()),
        ("failed", Some("input_not_consumed"))
    );
    // Not yet recorded in an active turn: no claim.
    assert!(
        run("inProgress", vec![user("u0", None)], &r)
            .unwrap()
            .is_none()
    );
    // Uncertain dispatch never becomes a negative claim, only positive evidence.
    let unknown = receipt("unknown", None);
    assert!(
        run("completed", vec![user("u0", None)], &unknown)
            .unwrap()
            .is_none()
    );
    let o = run(
        "completed",
        vec![user("u1", Some(ME)), agent("a", "r", "final_answer")],
        &unknown,
    )
    .unwrap()
    .unwrap();
    assert_eq!(o.state, "completed_with_reply");
}
#[test]
fn reverification_fails_closed() {
    let mut r = receipt("execution_started", None);
    r.steer.as_mut().unwrap().input_item_id = Some("u1".into());
    // Compaction or rewrite that drops a recorded steer is a gap, not a negative.
    assert_eq!(
        run("completed", vec![user("u0", None)], &r).unwrap_err().0,
        "steer_anchor_lost"
    );
    assert_eq!(
        run("completed", vec![user("other-id", Some(ME))], &r)
            .unwrap_err()
            .0,
        "correlation_changed"
    );
    let r = receipt("native_accepted", None);
    assert_eq!(
        run(
            "completed",
            vec![user("a", Some(ME)), user("b", Some(ME))],
            &r
        )
        .unwrap_err()
        .0,
        "ambiguous_native_correlation"
    );
    assert_eq!(
        run("weird", vec![], &r).unwrap_err().0,
        "unsupported_turn_status"
    );
    let anchor = SteerAnchor {
        turn_id: "turn-1".into(),
        user_items_before: 2,
        items_before: 3,
        last_agent_item_id: Some("a0".into()),
    };
    let r = receipt("native_accepted", Some(anchor.clone()));
    // Fewer earlier inputs than at steer time.
    assert_eq!(
        run(
            "completed",
            vec![user("u0", None), user("u1", Some(ME))],
            &r
        )
        .unwrap_err()
        .0,
        "correlation_changed"
    );
    // The pre-steer agent item cannot appear after the steer input.
    assert_eq!(
        run(
            "completed",
            vec![
                user("u0", None),
                user("u0b", None),
                user("u1", Some(ME)),
                agent("a0", "x", "final_answer")
            ],
            &r
        )
        .unwrap_err()
        .0,
        "correlation_changed"
    );
    // A compaction marker or discarded streaming item before it is fine.
    let o = run(
        "completed",
        vec![
            user("u0", None),
            user("u0b", None),
            Item::Other,
            user("u1", Some(ME)),
            agent("a1", "y", "final_answer"),
        ],
        &r,
    )
    .unwrap()
    .unwrap();
    assert_eq!(o.steer.unwrap().anchor, Some(anchor));
    let mut big = receipt("native_accepted", None);
    big.steer = None;
    assert_eq!(
        run(
            "completed",
            vec![
                user("u1", Some(ME)),
                agent("a", &"x".repeat(MAX_REPLY_BYTES + 1), "final_answer")
            ],
            &big
        )
        .unwrap_err()
        .0,
        "reply_too_large"
    );
}
