//! Switcher-owned, transport-independent messaging boundary. Never launches Codex.
mod batch;
pub mod checkpoints;
pub mod claude_steer;
pub mod headless;
mod modes;
mod public;
mod readiness;
pub mod recovery;
pub use batch::{BatchRequest, observations};
pub use public::{
    PublicMode, PublicRequest, SteeringIntent, receipt_public, send_public, steering_target,
};
pub use readiness::observe_public;
mod observe;
mod transport;
use crate::{
    external,
    paths::{Paths, lock_file, private_dir},
};
use fs2::FileExt;
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
pub const NATIVE_PROFILES: &[&str] = &["0.163.0", "0.164.0", "0.165.0"];
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
        let boot = fs::read_to_string("/proc/sys/kernel/random/boot_id").unwrap_or_default();
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
        format!("{:x}", Sha256::digest(value.to_string().as_bytes()))
    }
    async fn connect(&self) -> Result<Transport> {
        self.connect_with_profile(&mut None).await
    }
    async fn connect_with_profile(&self, version: &mut Option<String>) -> Result<Transport> {
        self.connect_profile(version, false).await
    }
    async fn connect_checkpoints(&self) -> Result<Transport> {
        self.connect_profile(&mut None, true).await
    }
    async fn connect_profile(
        &self,
        version: &mut Option<String>,
        checkpoints: bool,
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
        let mut transport = if checkpoints {
            Transport::connect_checkpoints(&resolved).await?
        } else {
            Transport::connect(&resolved).await?
        };
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
    json!({"protocol_version":VERSION,"schema":"urn:codex-switcher:messaging:1.2.1","experimental":true,"live_validated":false,"native_profiles":NATIVE_PROFILES,"operations":{"sessions":true,"resolve":true,"enqueue":true,"status":true,"observe":true,"events":true,"cancel":true},"native_queue":{"method":"thread/queue/add","delivery":"non_steering_idle_dispatch","deduplication":"adapter_only","expiry":"submission_deadline_only","cancel":"delete_true_under_native_dispatch_lock"},"events":{"scope":"adapter_lifecycle_and_terminal_replies","retained":1000,"page_size":100,"public_replies":"correlated_terminal_summary","execution_correlation":"public_client_id","native_replay":"unsupported"},"receipts":{"capacity":10000,"automatic_eviction":false,"uncertain_submission":"never_replayed"},"reply_observation":{"mode":"poll_status_or_observe","native_method":"thread/turns/list","items_view":"summary","max_pages":20,"max_text_bytes":65536},"unsupported_reason":"native_push_subscription_and_token_streaming_not_supported"})
}

struct Ledger {
    db: Connection,
    _lock: fs::File,
}
impl Ledger {
    fn open(paths: &Paths) -> Result<Self> {
        let dir = paths.root.join("messaging");
        private_dir(&dir).map_err(|_| Fault("storage_error"))?;
        let lock = lock_file(&dir.join("adapter.lock")).map_err(|_| Fault("storage_error"))?;
        lock.try_lock_exclusive()
            .map_err(|_| Fault("adapter_busy"))?;
        let path = dir.join("receipts.db");
        let file = lock_file(&path).map_err(|_| Fault("storage_error"))?;
        if file.metadata()?.uid() != unsafe { libc::geteuid() } {
            return Err(Fault("storage_error"));
        }
        let db = Connection::open(path)?;
        db.execute_batch("PRAGMA journal_mode=DELETE; PRAGMA synchronous=FULL; CREATE TABLE IF NOT EXISTS metadata(epoch TEXT NOT NULL); CREATE TABLE IF NOT EXISTS receipts(id TEXT PRIMARY KEY, hash TEXT NOT NULL, expiry INTEGER NOT NULL, receipt TEXT NOT NULL); CREATE TABLE IF NOT EXISTS events(seq INTEGER PRIMARY KEY AUTOINCREMENT, receipt TEXT NOT NULL); CREATE TABLE IF NOT EXISTS mode_requests(id TEXT PRIMARY KEY, mode TEXT NOT NULL);")?;
        if db.query_row("SELECT count(*) FROM metadata", [], |r| r.get::<_, i64>(0))? == 0 {
            db.execute(
                "INSERT INTO metadata VALUES (?)",
                [uuid::Uuid::new_v4().to_string()],
            )?;
        }
        Ok(Self { db, _lock: lock })
    }
    fn get(&self, id: &str) -> Result<Option<Receipt>> {
        let text: Option<String> = self
            .db
            .query_row("SELECT receipt FROM receipts WHERE id=?", [id], |r| {
                r.get(0)
            })
            .optional()?;
        text.map(|s| serde_json::from_str(&s).map_err(|_| Fault("storage_error")))
            .transpose()
    }
    fn save(&mut self, receipt: &Receipt) -> Result<()> {
        let data = serde_json::to_string(receipt).map_err(|_| Fault("storage_error"))?;
        let tx = self.db.transaction()?;
        tx.execute(
            "UPDATE receipts SET receipt=? WHERE id=?",
            params![data, receipt.message_id],
        )?;
        tx.execute("INSERT INTO events(receipt) VALUES (?)", [data])?;
        tx.execute(
            "DELETE FROM events WHERE seq <= (SELECT coalesce(max(seq),0)-1000 FROM events)",
            [],
        )?;
        tx.commit()?;
        Ok(())
    }
    fn accept(&mut self, input: &Enqueue, now: i64) -> Result<Receipt> {
        self.accept_with_mode(input, now, None)
    }
    fn accept_with_mode(
        &mut self,
        input: &Enqueue,
        now: i64,
        mode_hash: Option<&str>,
    ) -> Result<Receipt> {
        self.accept_bound(input, now, mode_hash.map(|hash| (hash, "queue", None)))
    }
    fn accept_bound(
        &mut self,
        input: &Enqueue,
        now: i64,
        binding: Option<(&str, &str, Option<SubmitEvidence>)>,
    ) -> Result<Receipt> {
        // A public steering intent reserves its UUID even before receipt creation.
        let has_public: bool = self.db.query_row("SELECT EXISTS(SELECT 1 FROM sqlite_master WHERE type='table' AND name='public_messages')", [], |r|r.get(0))?;
        if has_public {
            let public_mode: Option<String> = self
                .db
                .query_row(
                    "SELECT json_extract(binding,'$.request.mode') FROM public_messages WHERE id=?",
                    [&input.message_id],
                    |r| r.get(0),
                )
                .optional()?;
            if public_mode.as_deref() == Some("steer")
                && binding.as_ref().is_none_or(|b| b.1 != "public_steer")
            {
                return Err(Fault("message_id_conflict"));
            }
        }
        let steer = binding.as_ref().is_some_and(|b| b.1 == "public_steer");
        let mode_hash = binding.as_ref().map(|b| b.0);
        uuid(&input.message_id)?;
        uuid(&input.session_id)?;
        if input.text.is_empty()
            || input.text.len() > 16384
            || input.expected_generation.len() != 64
            || !input
                .expected_generation
                .bytes()
                .all(|b| b.is_ascii_digit() || (b'a'..=b'f').contains(&b))
        {
            return Err(Fault("invalid_request"));
        }
        let hash = format!(
            "{:x}",
            Sha256::digest(serde_json::to_vec(input).map_err(|_| Fault("invalid_request"))?)
        );
        let hash = mode_hash.unwrap_or(&hash).to_owned();
        if let Some(receipt) = self.get(&input.message_id)? {
            let old: String = self.db.query_row(
                "SELECT hash FROM receipts WHERE id=?",
                [&input.message_id],
                |r| r.get(0),
            )?;
            if old != hash {
                return Err(Fault("message_id_conflict"));
            }
            return Ok(receipt);
        }
        if input.expires_at <= now || input.expires_at > now + 86400 {
            return Err(Fault("invalid_expiry"));
        }
        if self
            .db
            .query_row("SELECT count(*) FROM receipts", [], |r| r.get::<_, i64>(0))?
            >= 10000
        {
            return Err(Fault("receipt_capacity"));
        }
        let receipt = Receipt {
            recovery_start: None,
            message_id: input.message_id.clone(),
            session_id: input.session_id.clone(),
            generation: input.expected_generation.clone(),
            state: if mode_hash.is_some() && !steer {
                "unknown"
            } else {
                "pending_local"
            }
            .into(),
            native_queue_id: None,
            reason: mode_hash
                .filter(|_| !steer)
                .map(|_| "dispatch_intent".into()),
            correlation: None,
            reply: None,
            submit: binding.as_ref().and_then(|b| b.2.clone()),
            steer: None,
        };
        let data = serde_json::to_string(&receipt).map_err(|_| Fault("storage_error"))?;
        let tx = self.db.transaction()?;
        tx.execute(
            "INSERT INTO receipts VALUES (?,?,?,?)",
            params![input.message_id, hash, input.expires_at, data],
        )?;
        if let Some((_, mode, _)) = binding {
            tx.execute(
                "INSERT INTO mode_requests(id,mode) VALUES (?,?)",
                params![input.message_id, mode],
            )?;
        }
        tx.execute("INSERT INTO events(receipt) VALUES (?)", [data])?;
        tx.execute(
            "DELETE FROM events WHERE seq <= (SELECT coalesce(max(seq),0)-1000 FROM events)",
            [],
        )?;
        tx.commit()?;
        Ok(receipt)
    }
    fn expire(&mut self, receipt: &mut Receipt, now: i64) -> Result<()> {
        let expiry: i64 = self.db.query_row(
            "SELECT expiry FROM receipts WHERE id=?",
            [&receipt.message_id],
            |r| r.get(0),
        )?;
        if receipt.state == "pending_local" && expiry <= now {
            receipt.state = "expired".into();
            receipt.reason = None;
            self.save(receipt)?;
        }
        Ok(())
    }
    fn events(&self, cursor: Option<&str>) -> Result<Value> {
        let epoch: String = self
            .db
            .query_row("SELECT epoch FROM metadata", [], |r| r.get(0))?;
        let (min, max): (i64, i64) = self.db.query_row(
            "SELECT coalesce(min(seq),1),coalesce(max(seq),0) FROM events",
            [],
            |r| Ok((r.get(0)?, r.get(1)?)),
        )?;
        let after = if let Some(cursor) = cursor {
            let (given, seq) = cursor.split_once(':').ok_or(Fault("invalid_cursor"))?;
            let seq: i64 = seq.parse().map_err(|_| Fault("invalid_cursor"))?;
            if given != epoch || seq < min - 1 || seq > max {
                return Err(Fault("cursor_gap"));
            }
            seq
        } else {
            min - 1
        };
        let mut statement = self
            .db
            .prepare("SELECT seq,receipt FROM events WHERE seq>? ORDER BY seq LIMIT 100")?;
        let rows = statement.query_map([after], |r| {
            Ok((r.get::<_, i64>(0)?, r.get::<_, String>(1)?))
        })?;
        let mut events = Vec::new();
        let mut last = after;
        for row in rows {
            let (seq, text) = row?;
            last = seq;
            let receipt: Receipt =
                serde_json::from_str(&text).map_err(|_| Fault("storage_error"))?;
            events.push(json!({"cursor":format!("{epoch}:{seq}"),"receipt":receipt}));
        }
        Ok(
            json!({"events":events,"cursor":format!("{epoch}:{last}"),"has_more":last<max,"public_replies":"correlated_terminal_summary"}),
        )
    }
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
    // Do not hold movement ownership across an unbounded paginated read.
    tokio::time::timeout(
        std::time::Duration::from_secs(3),
        reconcile_pages(control, ledger, receipt),
    )
    .await
    .map_err(|_| Fault("reconciliation_incomplete"))?
}
async fn reconcile_pages(
    control: &mut Transport,
    ledger: &mut Ledger,
    receipt: &mut Receipt,
) -> Result<()> {
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
            None => {
                if let Some(id) = match_id {
                    receipt.state = "native_accepted".into();
                    receipt.native_queue_id = Some(id);
                    receipt.reason = None;
                    ledger.save(receipt)?;
                }
                return Ok(());
            }
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
    // Persist uncertainty before touching the socket. No automatic replay, even on crash.
    receipt.state = "unknown".into();
    receipt.reason = Some("submission_uncertain".into());
    ledger.save(receipt)?;
    let result:Result<Added>=control.request("thread/queue/add",json!({"threadId":receipt.session_id,"clientUserMessageId":receipt.message_id,"input":[{"type":"text","text":input.text,"text_elements":[]}]})).await;
    if let Ok(response) = result {
        if response.item.client_id == receipt.message_id
            && !response.item.id.is_empty()
            && response.item.id.len() <= 256
        {
            receipt.state = "native_accepted".into();
            receipt.native_queue_id = Some(response.item.id);
            receipt.reason = None;
            ledger.save(receipt)?;
        }
    }
    Ok(())
}
async fn deliver(
    paths: &Paths,
    ledger: &mut Ledger,
    receipt: &mut Receipt,
    input: Option<&Enqueue>,
) -> Result<()> {
    deliver_with_policy(paths, ledger, receipt, input, None).await
}
async fn deliver_with_policy(
    paths: &Paths,
    ledger: &mut Ledger,
    receipt: &mut Receipt,
    input: Option<&Enqueue>,
    public_mode: Option<PublicMode>,
) -> Result<()> {
    let _guard = mutation_lock(paths, &receipt.session_id)?;
    let selected = attachment(paths, &receipt.session_id)?;
    if selected.generation() != receipt.generation {
        return Err(Fault("generation_mismatch"));
    }
    let mut control = selected.connect().await?;
    let current = if public_mode.is_some() && receipt.state == "pending_local" {
        let o =
            modes::session_status::observe_fresh_public(&mut control, &receipt.session_id).await?;
        if let Some(reason) = o.reason {
            return Err(Fault(reason));
        }
        match o.state.as_str() {
            "idle" => "idle",
            "working" => "busy",
            "interrupted" | "errored" => return Err(Fault("explicit_resume_required")),
            "disconnected" => return Err(Fault("unavailable")),
            _ => return Err(Fault("state_unverified")),
        }
    } else {
        activity(&mut control, &receipt.session_id).await?
    };
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
        }
    } else if receipt.state == "unknown" {
        reconcile(&mut control, ledger, receipt).await?;
    }
    Ok(())
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
    let _guard = mutation_lock(paths, &receipt.session_id)?;
    let selected = attachment(paths, &receipt.session_id)?;
    if selected.generation() != receipt.generation {
        return Err(Fault("generation_mismatch"));
    }
    let mut control = selected.connect().await?;
    if attachment(paths, &receipt.session_id)?.generation() != receipt.generation {
        return Err(Fault("generation_mismatch"));
    }
    #[derive(Deserialize)]
    struct Deleted {
        deleted: bool,
    }
    receipt.state = "unknown".into();
    receipt.reason = Some("cancellation_uncertain".into());
    ledger.save(receipt)?;
    let result: Result<Deleted> = control
        .request(
            "thread/queue/delete",
            json!({"threadId":receipt.session_id,"queuedSubmissionId":receipt.native_queue_id}),
        )
        .await;
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
            let mut ledger = Ledger::open(paths)?;
            let mut receipt = ledger.accept(&input, crate::daemon::now())?;
            ledger.expire(&mut receipt, crate::daemon::now())?;
            if matches!(receipt.state.as_str(), "pending_local" | "unknown") {
                if let Err(error) = deliver(paths, &mut ledger, &mut receipt, Some(&input)).await {
                    if receipt.reason.as_deref() != Some(error.0) {
                        receipt.reason = Some(error.0.into());
                        ledger.save(&receipt)?;
                    }
                }
            }
            Ok(json!(receipt))
        }
        Command::Status { message } | Command::Observe { message } => {
            uuid(&message)?;
            let mut ledger = Ledger::open(paths)?;
            let mut receipt = ledger.get(&message)?.ok_or(Fault("receipt_not_found"))?;
            if receipt.submit.is_some() {
                return Err(Fault("mode_receipt_requires_modes_status"));
            }
            ledger.expire(&mut receipt, crate::daemon::now())?;
            if receipt.state == "unknown" {
                let _ = deliver(paths, &mut ledger, &mut receipt, None).await;
            }
            observe::result(paths, &mut ledger, &mut receipt).await
        }
        Command::Events { cursor } => {
            let mut page = Ledger::open(paths)?.events(cursor.as_deref())?;
            page["events"]
                .as_array_mut()
                .ok_or(Fault("storage_error"))?
                .retain(|e| e["receipt"]["submit"].is_null());
            Ok(page)
        }
        Command::Cancel { message } => {
            uuid(&message)?;
            let mut ledger = Ledger::open(paths)?;
            let mut receipt = ledger.get(&message)?.ok_or(Fault("receipt_not_found"))?;
            if receipt.submit.is_some() {
                return Err(Fault("mode_receipt_requires_modes_cancel"));
            }
            cancel(paths, &mut ledger, &mut receipt).await?;
            Ok(json!(receipt))
        }
    }
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
