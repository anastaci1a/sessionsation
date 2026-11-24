//! Switcher-owned, transport-independent messaging boundary. Never launches Codex.
mod batch;
pub mod checkpoints;
pub mod claude_steer;
pub mod compaction;
mod delivery;
pub mod headless;
pub mod input_health;
mod ledger;
pub mod model_change;
mod modes;
mod public;
mod readiness;
pub mod recovery;
mod retention;
pub use batch::{BatchRequest, observations};
pub use public::{
    PublicMode, PublicRequest, SteeringIntent, receipt_public, send_public, steering_target,
};
pub use readiness::{observe_command, observe_public};
pub(crate) use retention::DELIVERED_JOURNAL_STATES;
mod observe;
mod transport;
use crate::{
    external,
    paths::{Paths, lock_file, private_dir},
};
use fs2::FileExt;
pub use input_health::NativeError;
use ledger::{Ledger, LedgerRead, Reads};
use rusqlite::{Connection, OptionalExtension, params};
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use sha2::{Digest, Sha256};
use std::{
    fs,
    io::Read,
    os::unix::fs::{FileTypeExt, MetadataExt},
    path::PathBuf,
};
pub use transport::NativeReadFailure;
use transport::Transport;

/// Exact upstream profiles inspected for queue, cancellation and summary semantics.
pub const NATIVE_PROFILES: &[&str] = &["0.163.0", "0.164.0", "0.165.0", "0.166.0"];
pub const VERSION: &str = "1.2.1";
type Result<T> = std::result::Result<T, Fault>;
#[derive(Debug, Clone, Copy)]
pub struct Fault(pub &'static str);
impl std::fmt::Display for Fault {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(self.0)
    }
}
impl std::error::Error for Fault {}
impl From<rusqlite::Error> for Fault {
    fn from(_: rusqlite::Error) -> Self {
        Self("storage_error")
    }
}
impl From<std::io::Error> for Fault {
    fn from(_: std::io::Error) -> Self {
        Self("local_io_error")
    }
}

#[derive(clap::Subcommand)]
pub enum Command {
    /// Versioned message modes: native Queue and read-only Submit preparation.
    Modes {
        #[command(subcommand)]
        command: modes::Command,
    },
    Capabilities,
    Sessions,
    Resolve {
        #[arg(long)]
        session: String,
    },
    /// Read the immutable enqueue request as JSON from stdin.
    Enqueue,
    Status {
        #[arg(long)]
        message: String,
    },
    /// Refresh correlated public terminal replies without sending input.
    Observe {
        #[arg(long)]
        message: String,
    },
    Events {
        #[arg(long)]
        cursor: Option<String>,
    },
    Cancel {
        #[arg(long)]
        message: String,
    },
}
#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Enqueue {
    pub message_id: String,
    pub session_id: String,
    pub text: String,
    pub expires_at: i64,
    pub expected_generation: String,
}
#[derive(Clone, Debug, Serialize, Deserialize, PartialEq)]
pub struct Receipt {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub recovery_start: Option<RecoveryStart>,
    pub message_id: String,
    pub session_id: String,
    pub generation: String,
    pub state: String,
    pub native_queue_id: Option<String>,
    pub reason: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub correlation: Option<observe::Correlation>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub reply: Option<observe::Reply>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub submit: Option<SubmitEvidence>,
    /// Steer-only reply attribution evidence (contract revision 15).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub steer: Option<SteerEvidence>,
    /// Contract 1.58.0: the newest native transport error of a submit (bounded, no message text).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub native_error: Option<NativeError>,
}
/// Native item positions recorded immediately before a steer and re-verified on
/// every observation. Contains item identifiers only, never input text.
#[derive(Clone, Debug, Default, Serialize, Deserialize, PartialEq)]
pub struct SteerAnchor {
    pub turn_id: String,
    pub user_items_before: u32,
    pub items_before: u32,
    pub last_agent_item_id: Option<String>,
}
#[derive(Clone, Debug, Default, Serialize, Deserialize, PartialEq)]
pub struct SteerEvidence {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub anchor: Option<SteerAnchor>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub input_item_id: Option<String>,
    /// Completion-projection id of the same input when Codex renumbered legacy
    /// `item-N` ids at turn end; `input_item_id` keeps the in-progress id.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub completed_input_item_id: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub next_input_item_id: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub turn_status: Option<String>,
    #[serde(default)]
    pub agent_items_after: u32,
    #[serde(default)]
    pub shared_with_turn_tail: bool,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub observed_generation: Option<String>,
}
#[derive(Clone, Debug, Serialize, Deserialize, PartialEq)]
pub struct RecoveryStart {
    pub state: String,
    pub turn_id: Option<String>,
}
#[derive(Clone, Debug, Serialize, Deserialize, PartialEq)]
pub struct SubmitEvidence {
    pub expected_turn_id: String,
    pub acknowledged_turn_id: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub action: Option<String>,
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub resume: bool,
}
#[derive(Clone)]
struct Attachment {
    id: String,
    home: PathBuf,
    pid: u32,
    start: u64,
}
impl Attachment {
    fn endpoint(&self) -> PathBuf {
        self.home.join("app-server-control/app-server-control.sock")
    }
    fn generation(&self) -> String {
        let endpoint = fs::metadata(self.endpoint()).ok();
        let boot = crate::proc::identity::boot_id_raw();
        let value = json!([
            self.id,
            self.home,
            self.pid,
            self.start,
            boot,
            endpoint
                .as_ref()
                .map(|m| (m.dev(), m.ino(), m.ctime(), m.ctime_nsec()))
        ]);
        crate::generation::of(&value).expect("serializable attachment")
    }
    async fn connect(&self) -> Result<Transport> {
        self.connect_with_profile(&mut None).await
    }
    async fn connect_with_profile(&self, version: &mut Option<String>) -> Result<Transport> {
        self.connect_profile(version, transport::FRAME_LIMIT).await
    }
    async fn connect_checkpoints(&self) -> Result<Transport> {
        self.connect_profile(&mut None, transport::CHECKPOINT_FRAME_LIMIT)
            .await
    }
    /// Steer anchor and reply observation read one full native turn.
    async fn connect_steer(&self) -> Result<Transport> {
        self.connect_profile(&mut None, transport::STEER_FRAME_LIMIT)
            .await
    }
    async fn connect_profile(
        &self,
        version: &mut Option<String>,
        max_bytes: usize,
    ) -> Result<Transport> {
        let path = self.endpoint();
        let meta = fs::symlink_metadata(&path).map_err(|_| Fault("endpoint_unavailable"))?;
        let parent = fs::metadata(path.parent().ok_or(Fault("unsafe_endpoint"))?)?;
        if !(meta.file_type().is_socket() || meta.file_type().is_symlink())
            || meta.uid() != unsafe { libc::geteuid() }
            || parent.uid() != unsafe { libc::geteuid() }
            || parent.mode() & 0o022 != 0
        {
            return Err(Fault("unsafe_endpoint"));
        }
        // Native Codex publishes a symlink to its private short Unix socket path.
        // Validate both sides and connect to the resolved path, never an unchecked link.
        // raw-path: I/O target of a socket link, validated below; never compared to a path
        let resolved = fs::canonicalize(&path).map_err(|_| Fault("endpoint_unavailable"))?;
        let target = fs::symlink_metadata(&resolved).map_err(|_| Fault("endpoint_unavailable"))?;
        let directory = fs::metadata(resolved.parent().ok_or(Fault("unsafe_endpoint"))?)?;
        if !target.file_type().is_socket()
            || target.uid() != unsafe { libc::geteuid() }
            || directory.uid() != unsafe { libc::geteuid() }
            || directory.mode() & 0o022 != 0
        {
            return Err(Fault("unsafe_endpoint"));
        }
        let mut transport = Transport::connect(&resolved, max_bytes).await?;
        let initialized = transport.initialize().await;
        *version = transport.provider_version.clone();
        initialized?;
        Ok(transport)
    }
}
fn uuid(value: &str) -> Result<()> {
    if uuid::Uuid::parse_str(value).is_ok_and(|u| u.to_string() == value) {
        Ok(())
    } else {
        Err(Fault("invalid_id"))
    }
}
fn attachments(paths: &Paths) -> Result<Vec<Attachment>> {
    external::messaging_bindings(paths)
        .map(|list| {
            list.into_iter()
                .map(|(id, home, pid, start)| Attachment {
                    id,
                    home,
                    pid,
                    start,
                })
                .collect()
        })
        .map_err(|_| Fault("discovery_unavailable"))
}
fn select(list: Vec<Attachment>, id: &str) -> Result<Attachment> {
    let mut matches = list.into_iter().filter(|a| a.id == id);
    let first = matches.next().ok_or(Fault("unavailable"))?;
    if matches.next().is_some() {
        return Err(Fault("ambiguous_session"));
    }
    Ok(first)
}
fn attachment(paths: &Paths, id: &str) -> Result<Attachment> {
    uuid(id)?;
    select(attachments(paths)?, id)
}
fn mutation_lock(paths: &Paths, id: &str) -> Result<(fs::File, fs::File)> {
    external::messaging_lock(paths, id).map_err(Fault)
}

#[derive(Deserialize)]
struct ThreadRead {
    thread: ThreadState,
}
#[derive(Deserialize)]
struct ThreadState {
    id: String,
    status: ThreadStatus,
}
#[derive(Deserialize)]
struct ThreadStatus {
    #[serde(rename = "type")]
    kind: String,
}
async fn activity(control: &mut Transport, id: &str) -> Result<&'static str> {
    let result: ThreadRead = control
        .request("thread/read", json!({"threadId":id,"includeTurns":false}))
        .await?;
    if result.thread.id != id {
        return Err(Fault("malformed_response"));
    }
    match result.thread.status.kind.as_str() {
        "idle" => Ok("idle"),
        "active" => Ok("busy"),
        "notLoaded" => Err(Fault("unavailable")),
        _ => Ok("active"),
    }
}
fn labels(
    paths: &Paths,
    id: &str,
    home: Option<&std::path::Path>,
) -> Result<(Option<String>, Option<String>)> {
    let (name, account) =
        external::messaging_labels(paths, id, home).map_err(|_| Fault("discovery_unavailable"))?;
    let name = name.and_then(|s| {
        let mut bounded = String::new();
        for c in s.chars().filter(|c| !c.is_control()) {
            if bounded.len() + c.len_utf8() > 128 {
                break;
            }
            bounded.push(c);
        }
        (!bounded.trim().is_empty()).then_some(bounded)
    });
    Ok((name, account))
}
async fn resolve(paths: &Paths, id: &str) -> Result<Value> {
    uuid(id)?;
    let selected = match attachment(paths, id) {
        Ok(a) => a,
        Err(Fault("unavailable")) => {
            let (name, account) = labels(paths, id, None)?;
            return Ok(
                json!({"session_id":id,"display_name":name,"account_name":account,"generation":null,"availability":"unavailable"}),
            );
        }
        Err(e) => return Err(e),
    };
    let generation = selected.generation();
    let availability = match mutation_lock(paths, id) {
        Err(Fault("migrating")) => "migrating",
        Err(error) => return Err(error),
        Ok(_guard) => match selected.connect().await {
            Ok(mut control) => activity(&mut control, id).await.unwrap_or("unavailable"),
            Err(Fault("unsupported" | "unsupported_native_version")) => "unsupported",
            Err(_) => "unavailable",
        },
    };
    let (name, account) = labels(paths, id, Some(&selected.home))?;
    Ok(
        json!({"session_id":id,"display_name":name,"account_name":account,"generation":generation,"availability":availability}),
    )
}
fn capabilities() -> Value {
    json!({"protocol_version":VERSION,"schema":"urn:codex-switcher:messaging:1.2.1","experimental":true,"live_validated":false,"native_profiles":NATIVE_PROFILES,"operations":{"sessions":true,"resolve":true,"enqueue":true,"status":true,"observe":true,"events":true,"cancel":true},"native_queue":{"method":"thread/queue/add","delivery":"non_steering_idle_dispatch","deduplication":"adapter_only","expiry":"submission_deadline_only","cancel":"delete_true_under_native_dispatch_lock"},"events":{"scope":"adapter_lifecycle_and_terminal_replies","retained":1000,"page_size":100,"public_replies":"correlated_terminal_summary","execution_correlation":"public_client_id","native_replay":"unsupported"},"receipts":{"capacity":10000,"automatic_eviction":true,"eviction":"certain_terminal_after_retention_days_with_tombstone","retired_lookup":"receipt_retired","uncertain_submission":"never_replayed"},"reply_observation":{"mode":"poll_status_or_observe","native_method":"thread/turns/list","items_view":"summary","max_pages":20,"max_text_bytes":65536},"unsupported_reason":"native_push_subscription_and_token_streaming_not_supported"})
}

/// How long a public path waits for the root-wide ledger lease before it reports
/// `adapter_busy`. Sends may wait longer than reads.
#[derive(Clone, Copy)]
enum Wait {
    Send,
    Read,
}
#[cfg(test)]
pub(super) static TEST_WAIT: std::sync::Mutex<Vec<(PathBuf, std::time::Duration)>> =
    std::sync::Mutex::new(Vec::new());
fn wait_budget(paths: &Paths, wait: Wait) -> std::time::Duration {
    #[cfg(test)]
    {
        // Unit tests keep the fail-fast behaviour unless a test registers its root.
        let _ = wait;
        TEST_WAIT
            .lock()
            .unwrap()
            .iter()
            .find(|(root, _)| *root == paths.root) // raw-path: in-process registry keyed by Paths.root
            .map_or(std::time::Duration::ZERO, |(_, d)| *d)
    }
    #[cfg(not(test))]
    {
        let _ = paths;
        std::time::Duration::from_secs(match wait {
            Wait::Send => 20,
            Wait::Read => 10,
        })
    }
}
/// What is left of a wait budget started earlier (a shared read that falls back to a
/// writer shares one budget).
fn remaining(deadline: tokio::time::Instant) -> std::time::Duration {
    deadline.saturating_duration_since(tokio::time::Instant::now())
}
/// Like `mutation_lock`, but waits (bounded, async) for a concurrent holder.
async fn mutation_lock_waiting(
    paths: &Paths,
    id: &str,
    budget: std::time::Duration,
) -> Result<(fs::File, fs::File)> {
    let deadline = tokio::time::Instant::now() + budget;
    loop {
        match mutation_lock(paths, id) {
            Err(Fault("operation_busy")) if tokio::time::Instant::now() < deadline => {
                tokio::time::sleep(std::time::Duration::from_millis(25)).await;
            }
            other => return other,
        }
    }
}
/// Unthrottled receipt-retention pass (part of the data-root retention sweep);
/// returns the number of retired message ids.
pub fn run_receipt_retention(paths: &Paths, now: i64) -> anyhow::Result<usize> {
    let retired = (|| {
        let mut ledger = ledger::open_write_blocking(paths, std::time::Duration::from_secs(5))?;
        retention::run(&mut ledger, now, retention::Policy::load(paths))
    })();
    // However the pass ended (a busy ledger included): the stats file is bounded
    // independently, best effort, and never a reason to fail the sweep.
    let _ = ledger::trim_stats(paths);
    Ok(retired?)
}
/// Bring the receipts schema to the current version once, at supervisor start, so
/// that after a cutover reads never have to migrate. A busy ledger is left to the
/// first open.
pub fn prepare_ledger(paths: &Paths) -> anyhow::Result<()> {
    drop(ledger::open_write_blocking(
        paths,
        std::time::Duration::from_secs(2),
    )?);
    Ok(())
}
#[derive(Deserialize)]
struct Queued {
    id: String,
    #[serde(rename = "clientUserMessageId")]
    client_id: String,
}
#[derive(Deserialize)]
struct Added {
    #[serde(rename = "queuedSubmission")]
    item: Queued,
}
#[derive(Deserialize)]
struct QueuePage {
    data: Vec<Queued>,
    #[serde(rename = "nextCursor")]
    next: Option<String>,
}

async fn reconcile(
    control: &mut Transport,
    ledger: &mut Ledger,
    receipt: &mut Receipt,
) -> Result<()> {
    // Do not hold movement ownership across an unbounded paginated read, nor the
    // root-wide ledger lease across any native I/O.
    let budget = wait_budget(&ledger.paths, Wait::Send);
    let found = ledger
        .during(budget, async {
            tokio::time::timeout(
                std::time::Duration::from_secs(3),
                reconcile_pages(control, receipt),
            )
            .await
            .map_err(|_| Fault("reconciliation_incomplete"))?
        })
        .await??;
    if let Some(id) = found {
        // Another process may have recorded a later state meanwhile: never regress it.
        if ledger.unchanged(receipt)? {
            receipt.state = "native_accepted".into();
            receipt.native_queue_id = Some(id);
            receipt.reason = None;
            ledger.save(receipt)?;
        }
    }
    Ok(())
}
async fn reconcile_pages(control: &mut Transport, receipt: &Receipt) -> Result<Option<String>> {
    let mut cursor: Option<String> = None;
    let mut match_id: Option<String> = None;
    // Bounded, complete search. A client ID is not a native uniqueness constraint.
    for _ in 0..100 {
        let page: QueuePage = control
            .request(
                "thread/queue/list",
                json!({"threadId":receipt.session_id,"cursor":cursor,"limit":100}),
            )
            .await?;
        for item in page
            .data
            .into_iter()
            .filter(|q| q.client_id == receipt.message_id)
        {
            if item.id.is_empty() || item.id.len() > 256 {
                return Err(Fault("malformed_response"));
            }
            if match_id.is_some() {
                return Err(Fault("ambiguous_native_correlation"));
            }
            match_id = Some(item.id);
        }
        match page.next {
            None => return Ok(match_id),
            Some(next) => {
                if cursor.as_ref() == Some(&next) {
                    return Err(Fault("malformed_response"));
                }
                cursor = Some(next);
            }
        }
    }
    Err(Fault("reconciliation_incomplete"))
}
async fn submit(
    control: &mut Transport,
    ledger: &mut Ledger,
    receipt: &mut Receipt,
    input: &Enqueue,
) -> Result<()> {
    // Persist uncertainty before touching the socket, under the lease. No automatic
    // replay, even on crash. Every other process now sees `unknown` and can only
    // reconcile, so exactly one process ever reaches the add below for an id.
    receipt.state = "unknown".into();
    receipt.reason = Some("submission_uncertain".into());
    ledger.save(receipt)?;
    let budget = wait_budget(&ledger.paths, Wait::Send);
    let result:Result<Added>=ledger.during(budget, control.request("thread/queue/add",json!({"threadId":receipt.session_id,"clientUserMessageId":receipt.message_id,"input":[{"type":"text","text":input.text,"text_elements":[]}]}))).await?;
    let outcome = match result {
        Ok(response) => {
            if response.item.client_id == receipt.message_id
                && !response.item.id.is_empty()
                && response.item.id.len() <= 256
            {
                // A concurrent reconcile/observe may already have advanced it.
                if ledger.unchanged(receipt)? {
                    receipt.state = "native_accepted".into();
                    receipt.native_queue_id = Some(response.item.id);
                    receipt.reason = None;
                    ledger.save(receipt)?;
                }
                "accepted"
            } else {
                "uncertain"
            }
        }
        // A late acknowledgement (a session still starting) leaves the entry in the
        // native queue: learn that without ever resending. The request may have reached the
        // server, so the receipt stays uncertain unless that proves it was accepted; the
        // transport error is kept on the receipt (contract 1.58.0).
        Err(fault) => {
            let at = crate::daemon::now();
            let error = input_health::native_error(fault, at);
            if let Some(error) = error.clone()
                && ledger.unchanged(receipt)?
            {
                receipt.native_error = Some(error);
                ledger.save(receipt)?;
            }
            let _ = reconcile(control, ledger, receipt).await;
            let learned = receipt.state == "native_accepted";
            input_health::record(
                &ledger.paths,
                &receipt.session_id,
                if learned { "accepted" } else { "uncertain" },
                error,
                at,
            );
            return Ok(());
        }
    };
    input_health::record(
        &ledger.paths,
        &receipt.session_id,
        outcome,
        None,
        crate::daemon::now(),
    );
    Ok(())
}
async fn deliver(
    paths: &Paths,
    ledger: &mut Ledger,
    receipt: &mut Receipt,
    input: Option<&Enqueue>,
) -> Result<()> {
    deliver_with_policy(paths, ledger, receipt, input, None, false)
        .await
        .map(|_| ())
}
async fn deliver_with_policy(
    paths: &Paths,
    ledger: &mut Ledger,
    receipt: &mut Receipt,
    input: Option<&Enqueue>,
    public_mode: Option<PublicMode>,
    // Delivering input a finished compaction held: the hold no longer applies to it.
    released: bool,
) -> Result<bool> {
    // All provider I/O below runs without the root-wide ledger lease so concurrent
    // sends and receipt polls are not blocked behind a busy target. Lock order is
    // always session ownership first, ledger second.
    let budget = wait_budget(paths, Wait::Send);
    let stored: &Receipt = receipt;
    let (_guard, mut control, current) = ledger
        .during(budget, async {
            let guard = mutation_lock_waiting(paths, &stored.session_id, budget).await?;
            // A queued compaction holds new input for its session (contract 1.47.0). Checked
            // under session ownership, which the compaction start also takes, so an input is
            // either in the native queue before the start (the start then waits) or held.
            // Only the public send path is held (it has a ledger binding the release can find);
            // steer, resume, the legacy enqueue and modes never are.
            if input.is_some()
                && public_mode.is_some()
                && !released
                && stored.state == "pending_local"
            {
                if compaction::hold_active(paths, &stored.session_id) {
                    return Err(Fault("compaction_hold"));
                }
                // Contract 1.49.0: a pending model change holds new input the same way, so
                // input sent after the request runs on the new model.
                if model_change::hold_active(paths, &stored.session_id) {
                    return Err(Fault("model_change_hold"));
                }
            }
            let selected = attachment(paths, &stored.session_id)?;
            if selected.generation() != stored.generation {
                return Err(Fault("generation_mismatch"));
            }
            let mut control = selected.connect().await?;
            let current = if public_mode.is_some() && stored.state == "pending_local" {
                let o =
                    modes::session_status::observe_fresh_public(&mut control, &stored.session_id)
                        .await?;
                if let Some(reason) = o.reason {
                    return Err(Fault(reason));
                }
                match o.state.as_str() {
                    // A failed turn (capacity, quota) leaves a live idle session; only an
                    // interrupted turn needs explicit Resume.
                    "idle" | "errored" => "idle",
                    "working" => "busy",
                    "interrupted" => return Err(Fault("explicit_resume_required")),
                    "disconnected" => return Err(Fault("unavailable")),
                    _ => return Err(Fault("state_unverified")),
                }
            } else {
                activity(&mut control, &stored.session_id).await?
            };
            Ok((guard, control, current))
        })
        .await??;
    // Another process may have delivered, cancelled or observed this id while the
    // lease was released. Its record wins; this call never resubmits over it.
    if !ledger.unchanged(receipt)? {
        return Ok(false);
    }
    if public_mode == Some(PublicMode::Submit)
        && current != "idle"
        && receipt.state == "pending_local"
    {
        return Err(Fault("target_busy"));
    }
    // Re-resolve at the mutation boundary while holding switcher's ownership locks.
    if attachment(paths, &receipt.session_id)?.generation() != receipt.generation {
        return Err(Fault("generation_mismatch"));
    }
    ledger.expire(receipt, crate::daemon::now())?;
    let mut accepted_from_idle = false;
    if receipt.state == "pending_local" {
        if let Some(input) = input {
            if !modes::queue::sending_enabled(paths)? {
                return Err(Fault("native_sending_disabled"));
            }
            if external::messaging_stop_intent(paths, &receipt.session_id)
                .map_err(|_| Fault("discovery_unavailable"))?
                .is_some()
            {
                return Err(Fault("explicit_resume_required"));
            }
            submit(&mut control, ledger, receipt, input).await?;
            accepted_from_idle = current == "idle" && receipt.state == "native_accepted";
        }
    } else if receipt.state == "unknown" {
        reconcile(&mut control, ledger, receipt).await?;
    }
    Ok(accepted_from_idle)
}
async fn cancel(paths: &Paths, ledger: &mut Ledger, receipt: &mut Receipt) -> Result<()> {
    ledger.expire(receipt, crate::daemon::now())?;
    match receipt.state.as_str() {
        "cancelled" => return Ok(()),
        "pending_local" => {
            receipt.state = "cancelled".into();
            receipt.reason = None;
            return ledger.save(receipt);
        }
        "unknown" => return Err(Fault("unknown")),
        "execution_started" | "completed_with_reply" => return Err(Fault("already_started")),
        "native_accepted" => {}
        _ => return Err(Fault("already_submitted")),
    }
    // Native I/O runs without the root-wide lease; session ownership is awaited
    // (bounded) instead of rejecting a cancel that merely overlaps another send.
    let budget = wait_budget(paths, Wait::Send);
    let stored: &Receipt = receipt;
    let (_guard, mut control) = ledger
        .during(budget, async {
            let guard = mutation_lock_waiting(paths, &stored.session_id, budget).await?;
            let selected = attachment(paths, &stored.session_id)?;
            if selected.generation() != stored.generation {
                return Err(Fault("generation_mismatch"));
            }
            let control = selected.connect().await?;
            if attachment(paths, &stored.session_id)?.generation() != stored.generation {
                return Err(Fault("generation_mismatch"));
            }
            Ok((guard, control))
        })
        .await??;
    if !ledger.unchanged(receipt)? {
        return changed_during_cancel(receipt);
    }
    #[derive(Deserialize)]
    struct Deleted {
        deleted: bool,
    }
    receipt.state = "unknown".into();
    receipt.reason = Some("cancellation_uncertain".into());
    ledger.save(receipt)?;
    let result: Result<Deleted> = ledger
        .during(
            budget,
            control.request(
                "thread/queue/delete",
                json!({"threadId":receipt.session_id,"queuedSubmissionId":receipt.native_queue_id}),
            ),
        )
        .await?;
    // Another process may have recorded the outcome meanwhile: its record wins.
    if !ledger.unchanged(receipt)? {
        return changed_during_cancel(receipt);
    }
    match result {
        Ok(result) if result.deleted => {
            receipt.state = "cancelled".into();
            receipt.reason = None;
            ledger.save(receipt)
        }
        Ok(_) => {
            receipt.state = "native_accepted".into();
            receipt.reason = Some("already_submitted".into());
            ledger.save(receipt)?;
            Err(Fault("already_submitted"))
        }
        Err(_) => Err(Fault("unknown")),
    }
}
/// A concurrent writer settled the receipt while a cancel was in flight.
fn changed_during_cancel(receipt: &Receipt) -> Result<()> {
    match receipt.state.as_str() {
        "cancelled" => Ok(()),
        "execution_started" | "completed_with_reply" => Err(Fault("already_started")),
        "unknown" => Err(Fault("unknown")),
        _ => Err(Fault("already_submitted")),
    }
}
async fn execute(paths: &Paths, command: Command) -> Result<Value> {
    match command {
        Command::Modes { .. } => Err(Fault("unsupported")),
        Command::Capabilities => Ok(capabilities()),
        Command::Sessions => {
            let mut ids: Vec<_> = attachments(paths)?.into_iter().map(|a| a.id).collect();
            ids.sort();
            ids.dedup();
            let mut sessions = Vec::new();
            for id in ids {
                sessions.push(resolve(paths, &id).await?);
            }
            Ok(json!({"sessions":sessions}))
        }
        Command::Resolve { session } => resolve(paths, &session).await,
        Command::Enqueue => {
            let mut bytes = Vec::new();
            std::io::stdin().take(32769).read_to_end(&mut bytes)?;
            if bytes.len() > 32768 {
                return Err(Fault("input_too_large"));
            }
            let input: Enqueue =
                serde_json::from_slice(&bytes).map_err(|_| Fault("invalid_request"))?;
            let mut ledger = ledger::open_write(paths, wait_budget(paths, Wait::Send)).await?;
            let mut receipt = ledger.accept(&input, crate::daemon::now())?;
            ledger.expire(&mut receipt, crate::daemon::now())?;
            if matches!(receipt.state.as_str(), "pending_local" | "unknown") {
                if let Err(error) = deliver(paths, &mut ledger, &mut receipt, Some(&input)).await {
                    if receipt.reason.as_deref() != Some(error.0) {
                        ledger.amend(&mut receipt, |r| r.reason = Some(error.0.into()))?;
                    }
                }
            }
            Ok(json!(receipt))
        }
        Command::Status { message } | Command::Observe { message } => {
            uuid(&message)?;
            let deadline = tokio::time::Instant::now() + wait_budget(paths, Wait::Read);
            {
                // Shared first: a settled receipt needs no write and no provider read.
                let reader = ledger::open_read(paths, remaining(deadline)).await?;
                let receipt = reader.lookup(&message)?;
                if receipt.submit.is_some() {
                    return Err(Fault("mode_receipt_requires_modes_status"));
                }
                if let Some(settled) = observe::settled(&reader, &receipt, crate::daemon::now())? {
                    return Ok(settled);
                }
            }
            status_write(paths, &message, remaining(deadline)).await
        }
        Command::Events { cursor } => {
            let mut page = ledger::open_read(paths, wait_budget(paths, Wait::Read))
                .await?
                .events(cursor.as_deref())?;
            page["events"]
                .as_array_mut()
                .ok_or(Fault("storage_error"))?
                .retain(|e| e["receipt"]["submit"].is_null());
            Ok(page)
        }
        Command::Cancel { message } => {
            uuid(&message)?;
            let mut ledger = ledger::open_write(paths, wait_budget(paths, Wait::Read)).await?;
            let mut receipt = ledger.lookup(&message)?;
            if receipt.submit.is_some() {
                return Err(Fault("mode_receipt_requires_modes_cancel"));
            }
            cancel(paths, &mut ledger, &mut receipt).await?;
            Ok(json!(receipt))
        }
    }
}
/// The writer path of legacy `status`/`observe`: may expire, re-deliver an `unknown`
/// receipt and observe it natively.
async fn status_write(paths: &Paths, message: &str, budget: std::time::Duration) -> Result<Value> {
    let mut ledger = ledger::open_write(paths, budget).await?;
    let mut receipt = ledger.lookup(message)?;
    if receipt.submit.is_some() {
        return Err(Fault("mode_receipt_requires_modes_status"));
    }
    ledger.expire(&mut receipt, crate::daemon::now())?;
    if receipt.state == "unknown" {
        let _ = deliver(paths, &mut ledger, &mut receipt, None).await;
    }
    observe::result(paths, &mut ledger, &mut receipt).await
}
/// JSON-only errors deliberately omit arbitrary native messages and local paths.
pub async fn run(paths: &Paths, command: Command) -> bool {
    if let Command::Modes { command } = command {
        return modes::run(paths, command).await;
    }
    match execute(paths, command).await {
        Ok(result) => {
            println!(
                "{}",
                json!({"contract_version":VERSION,"ok":true,"result":result})
            );
            true
        }
        Err(error) => {
            println!(
                "{}",
                json!({"contract_version":VERSION,"ok":false,"error":{"code":error.0}})
            );
            false
        }
    }
}
#[cfg(test)]
mod tests;
