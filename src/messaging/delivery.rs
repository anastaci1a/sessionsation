//! Input readiness before, and consumption proof after, a native Submit/Queue
//! send. A native acknowledgement only says the app-server stored the input; it
//! is not evidence that the session took it. A session launched moments earlier
//! can acknowledge a queued input while its input loop is not yet running, so a
//! receipt is only left as accepted after the input was seen leaving the queue
//! as a recorded turn input. Otherwise it is `failed` (removed again, safe to
//! resend) or `delivery_unknown`; it is never silently reported as delivered.
use super::*;
use std::time::{Duration, Instant};

/// Bounds for the checks around one send. `Timing::DISABLED` skips them.
#[derive(Clone, Copy, Debug)]
pub(super) struct Timing {
    /// Longest wait for the session to be ready for input before the send.
    pub ready: Duration,
    /// A process younger than this is still starting its input loop.
    pub settle: Duration,
    /// Longest wait for the sent input to be seen as consumed.
    pub verify: Duration,
    pub poll: Duration,
}
impl Timing {
    #[cfg_attr(test, allow(dead_code))]
    pub const DEFAULT: Timing = Timing {
        ready: Duration::from_secs(20),
        settle: Duration::from_secs(4),
        verify: Duration::from_secs(8),
        poll: Duration::from_millis(400),
    };
    pub const DISABLED: Timing = Timing {
        ready: Duration::ZERO,
        settle: Duration::ZERO,
        verify: Duration::ZERO,
        poll: Duration::from_millis(10),
    };
}

#[cfg(test)]
pub(super) static TEST_TIMING: std::sync::Mutex<Vec<(PathBuf, Timing)>> =
    std::sync::Mutex::new(Vec::new());

/// Unit tests run with the checks off unless a test registers its root; the
/// binary runs them unless `SESSIONSATION_NATIVE_SEND_CHECKS=off` (fixtures).
pub(super) fn timing(paths: &Paths) -> Timing {
    #[cfg(test)]
    {
        TEST_TIMING
            .lock()
            .unwrap()
            .iter()
            .find(|(root, _)| *root == paths.root)
            .map_or(Timing::DISABLED, |(_, t)| *t)
    }
    #[cfg(not(test))]
    {
        let _ = paths;
        if std::env::var("SESSIONSATION_NATIVE_SEND_CHECKS").as_deref() == Ok("off") {
            Timing::DISABLED
        } else {
            Timing::DEFAULT
        }
    }
}

fn process_age(start_ticks: u64) -> Option<Duration> {
    let (uptime, hz) = crate::api::boot_clock()?;
    let age = uptime - start_ticks as f64 / hz as f64;
    (age.is_finite() && age >= 0.).then(|| Duration::from_secs_f64(age))
}

/// Faults that waiting cannot cure.
fn permanent(fault: Fault) -> bool {
    matches!(
        fault.0,
        "unsupported_native_version"
            | "generation_mismatch"
            | "ambiguous_session"
            | "correlation_changed"
            | "invalid_request"
            | "unsupported"
    )
}

/// Wait, bounded and without any ledger lock, until the session shows real
/// readiness for input: its process has outlived `settle`, its thread answers a
/// native read and has a settled turn state. Returns when ready, when the state
/// is definitive (busy, blocked, interrupted), or at the deadline; the
/// authoritative gate in delivery runs afterwards either way.
pub(super) async fn await_ready(paths: &Paths, conversation: &str) {
    let t = timing(paths);
    if t.ready.is_zero() {
        return;
    }
    let deadline = Instant::now() + t.ready;
    loop {
        let ready: Result<bool> = async {
            let selected = attachment(paths, conversation)?;
            if process_age(selected.start).is_some_and(|age| age < t.settle) {
                return Ok(false);
            }
            let mut control = selected.connect().await?;
            let o = modes::session_status::observe_fresh_public(&mut control, conversation).await?;
            Ok(match o.reason {
                Some("approval_or_user_input_required") => true,
                Some(_) => false,
                None => matches!(
                    o.state.as_str(),
                    "idle" | "working" | "errored" | "interrupted"
                ),
            })
        }
        .await;
        match ready {
            Ok(true) => return,
            Err(e) if permanent(e) => return,
            _ if Instant::now() >= deadline => return,
            _ => tokio::time::sleep(t.poll).await,
        }
    }
}

/// Native queue entry of this message, if still queued.
async fn queued(control: &mut Transport, receipt: &Receipt) -> Result<Option<String>> {
    let mut cursor: Option<String> = None;
    for _ in 0..5 {
        let page: QueuePage = control
            .request(
                "thread/queue/list",
                json!({"threadId":receipt.session_id,"cursor":cursor,"limit":100}),
            )
            .await?;
        if let Some(item) = page
            .data
            .into_iter()
            .find(|q| q.client_id == receipt.message_id)
        {
            return Ok(Some(item.id));
        }
        match page.next {
            None => return Ok(None),
            Some(next) => cursor = Some(next),
        }
    }
    Err(Fault("reconciliation_incomplete"))
}

pub(super) enum Verdict {
    /// Seen as a recorded turn input; the receipt to commit.
    Consumed(Box<Receipt>),
    /// Still sitting in the native queue at the deadline.
    StillQueued,
    /// Neither consumption nor the queue entry could be established.
    Unverified,
}

/// Poll, without holding any lock, until the input is seen consumed.
pub(super) async fn verify(paths: &Paths, receipt: &Receipt, t: &Timing) -> Verdict {
    let deadline = Instant::now() + t.verify;
    let mut last = Verdict::Unverified;
    loop {
        let round: Result<Verdict> = async {
            let selected = attachment(paths, &receipt.session_id)?;
            let generation = selected.generation();
            if generation != receipt.generation {
                return Err(Fault("generation_mismatch"));
            }
            let mut control = selected.connect().await?;
            if let Some(seen) = observe::read(&mut control, receipt, &generation).await? {
                return Ok(Verdict::Consumed(Box::new(seen)));
            }
            Ok(match queued(&mut control, receipt).await? {
                Some(_) => Verdict::StillQueued,
                None => Verdict::Unverified,
            })
        }
        .await;
        match round {
            Ok(v @ Verdict::Consumed(_)) => return v,
            Ok(v) => last = v,
            // Keep what was last positively seen; a flaky read is not a verdict.
            Err(_) => {}
        }
        if Instant::now() >= deadline {
            return last;
        }
        tokio::time::sleep(t.poll).await;
    }
}

/// Apply the verdict to the receipt of a send this call just accepted.
pub(super) async fn settle(paths: &Paths, receipt: &mut Receipt, verdict: Verdict) -> Result<()> {
    let mut ledger = Ledger::open_waiting(paths, Duration::from_secs(3))?;
    let current = ledger.get(&receipt.message_id)?;
    if current.as_ref() != Some(&*receipt) {
        // Cancelled or observed meanwhile: that record wins.
        if let Some(current) = current {
            *receipt = current;
        }
        return Ok(());
    }
    match verdict {
        Verdict::Consumed(seen) => observe::commit(&mut ledger, receipt, *seen),
        Verdict::StillQueued => match cancel(paths, &mut ledger, receipt).await {
            Ok(()) if receipt.state == "cancelled" => {
                // Removed before the session took it: nothing ran, safe to resend.
                receipt.state = "failed".into();
                receipt.reason = Some("input_not_consumed".into());
                ledger.save(receipt)
            }
            // Consumed in the meantime or removal uncertain: cancel() already
            // recorded the honest state; a later receipt query refreshes it.
            _ => Ok(()),
        },
        Verdict::Unverified => {
            receipt.state = "unknown".into();
            receipt.reason = Some("consumption_unverified".into());
            ledger.save(receipt)
        }
    }
}

#[cfg(test)]
mod tests {
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
}
