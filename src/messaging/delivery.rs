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
    /// Compressed by `SESSIONSATION_TEST_TIME_SCALE` (test-only; identity otherwise).
    #[cfg_attr(test, allow(dead_code))]
    pub fn scaled(self) -> Timing {
        use crate::timing::scaled;
        Timing {
            ready: scaled(self.ready),
            settle: scaled(self.settle),
            verify: scaled(self.verify),
            poll: scaled(self.poll),
        }
    }
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
    let timing = tests::timing(paths);
    #[cfg(not(test))]
    let timing = {
        let _ = paths;
        if std::env::var("SESSIONSATION_NATIVE_SEND_CHECKS").as_deref() == Ok("off") {
            Timing::DISABLED
        } else {
            Timing::DEFAULT.scaled()
        }
    };
    timing
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
    let mut ledger = ledger::open_write_blocking(paths, Duration::from_secs(3))?;
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
mod tests;
