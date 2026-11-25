use super::*;
use crate::messaging::tests::{attachment_fixture, bind, fixture, scripted_connections};
use std::sync::{Arc, Mutex};

#[derive(Clone, Copy, PartialEq)]
enum Behavior {
    /// The add is consumed at once: a turn records the input.
    Consumes,
    /// The add is acknowledged but the session never reads it.
    NeverReads,
    /// Acknowledged, then neither queued nor recorded anywhere.
    Vanishes,
}
#[derive(Default)]
struct Fake {
    reads: usize,
    adds: usize,
    deletes: usize,
    queued: bool,
    consumed: bool,
}

fn quick() -> Timing {
    Timing {
        ready: Duration::from_secs(5),
        settle: Duration::ZERO,
        verify: Duration::from_millis(600),
        poll: Duration::from_millis(30),
    }
}

/// Run one public send against a scripted app-server. `unready_reads` thread
/// reads fail first, like a session whose input loop is still starting;
/// `last_status`/`error` describe the previous turn.
async fn run(
    behavior: Behavior,
    unready_reads: usize,
    thread_kind: &'static str,
    last_status: &'static str,
    error: Option<&'static str>,
) -> (Value, Arc<Mutex<Fake>>) {
    let (_dir, paths, mut input, _) = fixture();
    input.expires_at = crate::daemon::now() + 120;
    let mut a = attachment_fixture(&paths, &input);
    bind(&paths, &mut a);
    TEST_TIMING
        .lock()
        .unwrap()
        .push((paths.root.clone(), quick()));
    let state = Arc::new(Mutex::new(Fake::default()));
    let shared = state.clone();
    let id = a.id.clone();
    let message = input.message_id.clone();
    let server = scripted_connections(
        &a.endpoint(),
        move |r| {
            let mut s = shared.lock().unwrap();
            match r["method"].as_str().unwrap() {
                "thread/read" => {
                    s.reads += 1;
                    if s.reads <= unready_reads {
                        return None;
                    }
                    Some(json!({"thread":{"id":id,"status":{"type":thread_kind}}}))
                }
                "thread/turns/list" => Some(if s.consumed {
                    json!({"data":[{"id":"turn-new","status":"inProgress","itemsView":"summary",
                        "items":[{"type":"userMessage","id":"item-1","clientId":message}]}],"nextCursor":null})
                } else {
                    json!({"data":[{"id":"previous","status":last_status,"itemsView":"summary",
                        "items":[],"error":error.map(|m| json!({"message":m}))}],"nextCursor":null})
                }),
                "thread/queue/add" => {
                    s.adds += 1;
                    s.queued = behavior == Behavior::NeverReads;
                    s.consumed = behavior == Behavior::Consumes;
                    Some(json!({"queuedSubmission":{"id":"q1","clientUserMessageId":r["params"]["clientUserMessageId"]}}))
                }
                "thread/queue/list" => Some(if s.queued {
                    json!({"data":[{"id":"q1","clientUserMessageId":message}],"nextCursor":null})
                } else {
                    json!({"data":[],"nextCursor":null})
                }),
                "thread/queue/delete" => {
                    s.deletes += 1;
                    let was = s.queued;
                    s.queued = false;
                    Some(json!({"deleted":was}))
                }
                other => panic!("unexpected {other}"),
            }
        },
        400,
    )
    .await;
    let instance: external::Instance = serde_json::from_value(json!({"pane":"%1","tmux_session":"fixture","pid":a.pid,"start_time":a.start,"cwd":"/tmp","thread_id":a.id,"rollout":"/tmp/unused","home":a.home,"binary":"fixture","resume_options":[],"account":"selected"})).unwrap();
    let session = crate::catalog::reconcile(&paths, &[instance])
        .unwrap()
        .remove(0);
    let result = PublicRequest {
        origin: None,
        delivery: None,
        await_reply: false,
        if_busy: None,
        steering: None,
        recovery: None,
        api_version: 1,
        message_id: input.message_id,
        session_id: session.id.clone(),
        expected_generation: session.generation(),
        mode: PublicMode::Submit,
        // A long message, like the 2.3 KB one that was lost on a fresh session.
        text: "long prompt ".repeat(200),
        expires_at: input.expires_at,
    };
    let out = send_public(&paths, result).await.unwrap();
    server.abort();
    (out, state)
}
use crate::messaging::public::{PublicRequest, send_public};

#[tokio::test]
async fn long_send_is_accepted_only_after_the_input_is_seen_consumed() {
    let (receipt, fake) = run(Behavior::Consumes, 0, "idle", "completed", None).await;
    assert_eq!(receipt["state"], "execution_started", "{receipt}");
    assert_eq!(receipt["facts"]["input_consumed"], true);
    assert_eq!(fake.lock().unwrap().adds, 1);
}

#[tokio::test]
async fn input_the_session_never_reads_is_removed_and_reported_failed() {
    let (receipt, fake) = run(Behavior::NeverReads, 0, "idle", "completed", None).await;
    assert_eq!(receipt["state"], "failed", "{receipt}");
    assert_eq!(receipt["reason"], "input_not_consumed");
    assert_eq!(receipt["facts"]["input_consumed"], false);
    let fake = fake.lock().unwrap();
    // Exactly one add and one removal: nothing is replayed, nothing is left queued.
    assert_eq!((fake.adds, fake.deletes, fake.queued), (1, 1, false));
}

#[tokio::test]
async fn input_that_cannot_be_traced_is_uncertain_never_accepted() {
    let (receipt, fake) = run(Behavior::Vanishes, 0, "idle", "completed", None).await;
    assert_eq!(receipt["state"], "delivery_unknown", "{receipt}");
    assert_eq!(receipt["reason"], "consumption_unverified");
    assert_eq!(receipt["facts"]["input_consumed"], "unknown");
    assert_eq!(fake.lock().unwrap().adds, 1);
}

#[tokio::test]
async fn a_session_that_is_not_ready_yet_is_waited_for_before_any_input() {
    let (receipt, fake) = run(Behavior::Consumes, 6, "idle", "completed", None).await;
    assert_eq!(receipt["state"], "execution_started", "{receipt}");
    let fake = fake.lock().unwrap();
    assert!(fake.reads > 6, "readiness was probed until it answered");
    assert_eq!(fake.adds, 1);
}

#[tokio::test]
async fn a_failed_capacity_turn_leaves_a_live_session_that_takes_input() {
    for kind in ["idle", "systemError"] {
        let (receipt, fake) = run(
            Behavior::Consumes,
            0,
            kind,
            "failed",
            Some("Selected model is at capacity"),
        )
        .await;
        assert_eq!(receipt["state"], "execution_started", "{kind}: {receipt}");
        assert_ne!(receipt["reason"], "explicit_resume_required");
        assert_eq!(fake.lock().unwrap().adds, 1);
    }
}

#[test]
fn process_age_is_measured_from_the_boot_clock() {
    let stat = fs::read_to_string("/proc/self/stat").unwrap();
    let start: u64 = stat
        .rsplit_once(") ")
        .unwrap()
        .1
        .split_whitespace()
        .nth(19)
        .unwrap()
        .parse()
        .unwrap();
    let age = process_age(start).unwrap();
    assert!(age < Duration::from_secs(86400 * 365));
    assert!(process_age(u64::MAX / 4).is_none());
}

fn request(
    session: &crate::catalog::Session,
    id: &str,
    expires_at: i64,
) -> crate::messaging::public::PublicRequest {
    crate::messaging::public::PublicRequest {
        origin: None,
        delivery: None,
        await_reply: false,
        if_busy: None,
        steering: None,
        recovery: None,
        api_version: 1,
        message_id: id.into(),
        session_id: session.id.clone(),
        expected_generation: session.generation(),
        mode: crate::messaging::public::PublicMode::Submit,
        text: "hello".into(),
        expires_at,
    }
}

/// Contract 1.58.0: a refused connection is a definite, retryable failure with the native
/// error and a health counter; a later success resets the counter.
#[tokio::test]
async fn refused_connection_is_not_delivered_with_native_error_and_counts_in_health() {
    use crate::messaging::input_health;
    let (_dir, paths, input, _) = fixture();
    let expires = crate::daemon::now() + 120;
    let mut a = attachment_fixture(&paths, &input);
    bind(&paths, &mut a);
    let instance: external::Instance = serde_json::from_value(json!({"pane":"%1","tmux_session":"fixture","pid":a.pid,"start_time":a.start,"cwd":"/tmp","thread_id":a.id,"rollout":"/tmp/unused","home":a.home,"binary":"fixture","resume_options":[],"account":"selected"})).unwrap();
    let session = crate::catalog::reconcile(&paths, &[instance])
        .unwrap()
        .remove(0);
    let one = "11111111-1111-4111-8111-111111111111";
    let two = "22222222-2222-4222-8222-222222222222";
    let three = "33333333-3333-4333-8333-333333333333";
    let out = send_public(&paths, request(&session, one, expires))
        .await
        .unwrap();
    assert_eq!(out["state"], "rejected", "{out}");
    assert_eq!(out["reason"], "endpoint_unavailable");
    assert_eq!(out["facts"]["native_error"]["kind"], "endpoint_unavailable");
    assert_eq!(out["facts"]["retryable"], true);
    assert_eq!(out["facts"]["accepted_native"], false);
    assert!(!out.to_string().contains("hello"));
    let h = input_health::public(&paths, &a.id);
    assert_eq!(h["consecutive_failures"], 1);
    assert_eq!(h["last_outcome"], "not_delivered");
    assert_eq!(h["last_native_error"]["kind"], "endpoint_unavailable");
    assert!(h["last_success_at"].is_null());
    let out = send_public(&paths, request(&session, two, expires))
        .await
        .unwrap();
    assert_eq!(out["state"], "rejected");
    assert_eq!(
        input_health::public(&paths, &a.id)["consecutive_failures"],
        2
    );
    // The app-server comes back: the next send is accepted and the counter resets.
    let server = scripted_connections(
        &a.endpoint(),
        move |r| match r["method"].as_str().unwrap() {
            "thread/read" => {
                Some(json!({"thread":{"id":r["params"]["threadId"],"status":{"type":"idle"}}}))
            }
            "thread/queue/add" => Some(
                json!({"queuedSubmission":{"id":"q1","clientUserMessageId":r["params"]["clientUserMessageId"]}}),
            ),
            "thread/turns/list" => Some(json!({"data":[],"nextCursor":null})),
            other => panic!("unexpected {other}"),
        },
        4,
    )
    .await;
    let out = send_public(&paths, request(&session, three, expires))
        .await
        .unwrap();
    server.abort();
    assert_eq!(out["state"], "native_accepted", "{out}");
    let h = input_health::public(&paths, &a.id);
    assert_eq!(h["consecutive_failures"], 0);
    assert_eq!(h["last_outcome"], "accepted");
    assert!(h["last_success_at"].is_i64());
    // The earlier error stays as the last diagnostic.
    assert_eq!(h["last_native_error"]["kind"], "endpoint_unavailable");
}

pub(super) fn timing(paths: &Paths) -> Timing {
    TEST_TIMING
        .lock()
        .unwrap()
        .iter()
        .find(|(root, _)| *root == paths.root) // raw-path: in-process registry keyed by Paths.root
        .map_or(Timing::DISABLED, |(_, t)| *t)
}
