//! Caller-authorized recovery. Journals launch and delivery separately; reads never dispatch.
use super::*;
use anyhow::{Context, ensure};
use std::{os::unix::fs::PermissionsExt, path::Path, process::Stdio};
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case", deny_unknown_fields)]
pub enum Host {
    Herdr {
        session: String,
        socket: PathBuf,
        workspace_id: String,
    },
    Tmux {
        socket: PathBuf,
        session: String,
    },
}
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Plan {
    pub session_id: String,
    pub expected_generation: String,
    pub provider_conversation_id: String,
    pub account_id: String,
    pub directory: PathBuf,
    pub executable: PathBuf,
    pub executable_sha256: String,
    pub launcher_executable: PathBuf,
    pub launcher_sha256: String,
    pub configuration_sha256: String,
    pub history_sha256: String,
    pub directory_identity: String,
    pub host_identity: String,
    pub host: Host,
    pub native_endpoint_generation: String,
    pub previous_turn_id: Option<String>,
    pub previous_turn_status: Option<String>,
    pub stop_intent_id: Option<String>,
}
#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
pub struct PlanRequest {
    pub session_id: String,
    pub expected_generation: String,
    pub host: Host,
}
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Intent {
    pub expected_state: String,
    #[serde(deserialize_with = "explicit_nullable")]
    pub expected_previous_turn_id: Option<String>,
    #[serde(deserialize_with = "explicit_nullable")]
    pub expected_stop_id: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub expected_runtime_generation: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub launch: Option<Plan>,
}
fn explicit_nullable<'de, D: serde::Deserializer<'de>>(
    d: D,
) -> std::result::Result<Option<String>, D::Error> {
    Option::<String>::deserialize(d)
}

fn hash(value: impl AsRef<[u8]>) -> String {
    format!("{:x}", Sha256::digest(value.as_ref()))
}
fn identity(path: &Path) -> anyhow::Result<String> {
    let m = fs::metadata(path)?;
    Ok(hash(format!("{}:{}", m.dev(), m.ino())))
}
fn executable(name: &str) -> anyhow::Result<PathBuf> {
    let p = PathBuf::from(name);
    let p = if p.is_absolute() {
        p
    } else {
        ensure!(
            p.components().count() == 1,
            "RECOVERY_EXECUTABLE_REQUIRES_ABSOLUTE_PATH"
        );
        std::env::split_paths(&std::env::var_os("PATH").unwrap_or_default())
            .map(|d| d.join(name))
            .find(|p| p.is_file())
            .context("RECOVERY_EXECUTABLE_NOT_FOUND")?
    };
    let p = fs::canonicalize(p).context("RECOVERY_EXECUTABLE_NOT_FOUND")?;
    let m = fs::metadata(&p)?;
    ensure!(
        m.is_file() && m.permissions().mode() & 0o111 != 0,
        "RECOVERY_EXECUTABLE_NOT_EXECUTABLE"
    );
    Ok(p)
}
fn tmux(socket: &Path, args: &[&str]) -> anyhow::Result<String> {
    let output = std::process::Command::new("tmux")
        .arg("-S")
        .arg(socket)
        .args(args)
        .output()
        .context("RECOVERY_TMUX_UNAVAILABLE")?;
    ensure!(output.status.success(), "RECOVERY_TMUX_REJECTED");
    Ok(String::from_utf8(output.stdout)?.trim().into())
}
async fn validate_host(host: &Host) -> anyhow::Result<String> {
    let socket = match host {
        Host::Herdr { socket, .. } | Host::Tmux { socket, .. } => socket,
    };
    ensure!(
        socket.is_absolute(),
        "RECOVERY_HOST_SOCKET_REQUIRES_ABSOLUTE_PATH"
    );
    let meta = fs::metadata(socket).context("RECOVERY_HOST_SOCKET_MISSING")?;
    ensure!(
        meta.file_type().is_socket() && meta.uid() == unsafe { libc::geteuid() },
        "RECOVERY_HOST_SOCKET_UNSAFE"
    );
    match host {
        Host::Herdr {
            session,
            socket,
            workspace_id,
        } => {
            crate::paths::validate_name(session)?;
            let spaces = crate::launch::host_call(
                &crate::launch::Host {
                    session: session.clone(),
                    socket: socket.clone(),
                },
                &["workspace", "list"],
            )
            .await?;
            ensure!(
                spaces["workspaces"]
                    .as_array()
                    .is_some_and(|ss| ss.iter().any(|s| s["workspace_id"] == *workspace_id)),
                "RECOVERY_WORKSPACE_NOT_FOUND"
            );
        }
        Host::Tmux { socket, session } => {
            crate::paths::validate_name(session)?;
            let names = tmux(socket, &["list-sessions", "-F", "#{session_name}"])?;
            ensure!(
                !names.lines().any(|n| n == session),
                "RECOVERY_TMUX_DESTINATION_EXISTS_CHOOSE_NEW_NAME"
            );
        }
    }
    Ok(hash(format!("{}:{}", meta.dev(), meta.ino())))
}
pub async fn plan(paths: &Paths, r: PlanRequest) -> anyhow::Result<Value> {
    plan_inner(paths, r, None).await
}
async fn plan_inner(paths: &Paths, r: PlanRequest, own: Option<&str>) -> anyhow::Result<Value> {
    let (s, source) = crate::api::selected(paths, &r.session_id, &r.expected_generation).await?;
    ensure!(
        s.attachment.is_none() && source.inactive,
        "RECOVERY_PROCESS_STILL_RUNNING"
    );
    crate::lifecycle::ensure_available(paths, &s.conversation_id, own)?;
    crate::lifecycle::ensure_no_handoff(paths, &s.conversation_id, own)?;
    ensure!(
        source.account == s.account && source.home == paths.account(&s.account)?,
        "RECOVERY_ACCOUNT_POLICY_AMBIGUOUS"
    );
    ensure!(!source.archived, "RECOVERY_RESTORE_ARCHIVE_FIRST");
    ensure!(
        matches!(source.block_reason.as_deref(), None | Some("unfinished")),
        "RECOVERY_HISTORY_UNVERIFIED"
    );
    ensure!(
        !external::rollout_still_open(&source.rollout)?,
        "RECOVERY_HISTORY_HAS_ACTIVE_WRITER"
    );
    let permissions = external::recovery_permissions(&source.rollout)
        .context("RECOVERY_PERMISSIONS_MISSING_RECORD_EXPLICIT_LAUNCH_POLICY_FIRST")?;
    let directory = fs::canonicalize(&source.cwd).context("RECOVERY_SAVED_DIRECTORY_MISSING")?;
    ensure!(directory.is_dir(), "RECOVERY_SAVED_DIRECTORY_MISSING");
    let configured = crate::store::load_config(paths)?.codex_binary;
    let exe = executable(&configured)?;
    let configuration = hash(serde_json::to_vec(&json!([
        configured,
        source.home,
        source.cwd,
        source.resume_options,
        permissions
    ]))?);
    validate_owner(paths, &s.account).await?;
    let mut native = home_attachment(paths, &s)?.connect().await?;
    let summary = native_preflight(&mut native, &s.conversation_id, None, true).await?;
    let launcher = fs::canonicalize(std::env::current_exe()?)
        .context("RECOVERY_BACKEND_ARTIFACT_MISSING_USE_IMMUTABLE_INSTALL")?;
    let plan = Plan {
        launcher_sha256: hash(fs::read(&launcher)?),
        launcher_executable: launcher,
        native_endpoint_generation: home_attachment(paths, &s)?.generation(),
        previous_turn_id: summary["previous_turn_id"].as_str().map(str::to_owned),
        previous_turn_status: summary["previous_turn_status"].as_str().map(str::to_owned),
        stop_intent_id: external::messaging_stop_intent(paths, &s.conversation_id)?,
        session_id: s.id.clone(),
        expected_generation: s.generation(),
        provider_conversation_id: s.conversation_id.clone(),
        account_id: format!("codex:{}", s.account),
        directory_identity: identity(&directory)?,
        directory,
        executable_sha256: hash(fs::read(&exe)?),
        executable: exe,
        configuration_sha256: configuration,
        history_sha256: hash(fs::read(&source.rollout)?),
        host_identity: validate_host(&r.host).await?,
        host: r.host,
    };
    background_quiescent(paths, &s).await?;
    Ok(json!(plan))
}
async fn validate_owner(paths: &Paths, account: &str) -> anyhow::Result<()> {
    crate::accounts::ensure_available(paths, account)?;
    let status = tokio::time::timeout(
        std::time::Duration::from_secs(5),
        crate::daemon::call(paths, "status", json!({})),
    )
    .await
    .context("RECOVERY_SUPERVISOR_STATUS_TIMEOUT")?
    .context("RECOVERY_EXISTING_SUPERVISOR_REQUIRED")?;
    ensure!(
        status["daemon"]["lifecycle_reservations"]
            .as_array()
            .is_some_and(|rs| rs.iter().any(|r| r == "message-recoveries")),
        "RECOVERY_SUPERVISOR_UPGRADE_REQUIRED_COORDINATE_ALL_ROOT_WRITERS"
    );
    ensure!(
        status["accounts"].as_array().is_some_and(|aa| aa
            .iter()
            .any(|a| a["name"] == account && a["enabled"] == true && a["auth_required"] == false)),
        "RECOVERY_ACCOUNT_NOT_READY_ENABLE_OR_LOGIN_FIRST"
    );
    Ok(())
}
async fn background_quiescent(paths: &Paths, s: &crate::catalog::Session) -> anyhow::Result<()> {
    let mut control = crate::control::Control::connect(&paths.account(&s.account)?).await?;
    ensure!(
        control.terminals(&s.conversation_id).await?.is_empty(),
        "RECOVERY_BACKGROUND_WORK_ACTIVE_STOP_IT_EXPLICITLY_FIRST"
    );
    Ok(())
}

fn home_attachment(paths: &Paths, s: &crate::catalog::Session) -> anyhow::Result<Attachment> {
    Ok(Attachment {
        id: s.conversation_id.clone(),
        home: paths.account(&s.account)?,
        pid: 0,
        start: 0,
    })
}
async fn native_preflight(
    control: &mut Transport,
    id: &str,
    intent: Option<&Intent>,
    exited: bool,
) -> anyhow::Result<Value> {
    tokio::time::timeout(
        std::time::Duration::from_secs(5),
        native_preflight_inner(control, id, intent, exited),
    )
    .await
    .context("RECOVERY_OBSERVATION_TIMEOUT_NO_INPUT_SENT")?
}
async fn native_preflight_inner(
    control: &mut Transport,
    id: &str,
    intent: Option<&Intent>,
    exited: bool,
) -> anyhow::Result<Value> {
    let value: Value = control
        .request("thread/read", json!({"threadId":id,"includeTurns":false}))
        .await?;
    ensure!(
        value["thread"]["id"] == id,
        "RECOVERY_CONVERSATION_NOT_FOUND_CHECK_SAVED_TARGET"
    );
    let status = &value["thread"]["status"];
    if !exited {
        ensure!(
            status["type"] != "notLoaded",
            "RECOVERY_LIVE_THREAD_UNLOADED_RECONNECT_OR_STOP_FIRST"
        );
    }
    ensure!(
        matches!(
            status["type"].as_str(),
            Some("idle" | "notLoaded" | "systemError")
        ),
        "RECOVERY_SESSION_BUSY_WAIT_FOR_IDLE"
    );
    ensure!(
        status["activeFlags"]
            .as_array()
            .is_none_or(|a| a.is_empty()),
        "RECOVERY_APPROVAL_OR_INPUT_REQUIRED"
    );
    let turns: Value = control.passive_summary(id).await?;
    let turns = turns["data"]
        .as_array()
        .context("RECOVERY_TURN_SUMMARY_UNAVAILABLE")?;
    ensure!(turns.len() <= 1, "RECOVERY_TURN_SUMMARY_AMBIGUOUS");
    if let Some(t) = turns.first() {
        ensure!(
            t["id"].as_str().is_some_and(|id| !id.is_empty()
                && id.len() <= 256
                && !id.chars().any(char::is_control)),
            "RECOVERY_TURN_SUMMARY_AMBIGUOUS"
        );
        ensure!(
            matches!(
                t["status"].as_str(),
                Some("completed" | "interrupted" | "failed")
            ),
            "RECOVERY_SAVED_TURN_NOT_TERMINAL_INSPECT_PROVIDER_HISTORY"
        );
    }
    if status["type"] == "systemError" {
        ensure!(
            turns.first().is_some_and(|t| t["status"] == "failed"),
            "RECOVERY_PROVIDER_ERROR_WITHOUT_TERMINAL_EVIDENCE"
        );
    }
    if let Some(intent) = intent {
        let previous = turns.first().and_then(|t| t["id"].as_str());
        ensure!(
            previous == intent.expected_previous_turn_id.as_deref(),
            "RECOVERY_PREVIOUS_TURN_CHANGED_OBSERVE_AGAIN"
        );
        match intent.expected_state.as_str() {
            "interrupted" => ensure!(
                turns.first().is_some_and(|t| t["status"] == "interrupted"),
                "RECOVERY_STATE_CHANGED_OBSERVE_AGAIN"
            ),
            "errored" => ensure!(
                status["type"] == "systemError"
                    || turns.first().is_some_and(|t| t["status"] == "failed"),
                "RECOVERY_STATE_CHANGED_OBSERVE_AGAIN"
            ),
            "stopped" => ensure!(
                exited || intent.expected_stop_id.is_some(),
                "RECOVERY_EXPLICIT_STOP_EVIDENCE_REQUIRED"
            ),
            _ => anyhow::bail!("RECOVERY_EXPECTED_STATE_MUST_BE_STOPPED_INTERRUPTED_OR_ERRORED"),
        }
    }
    let queue: Value = control
        .request("thread/queue/list", json!({"threadId":id}))
        .await?;
    ensure!(
        queue["data"].as_array().is_some_and(|q| q.is_empty())
            && queue.get("nextCursor").is_some_and(Value::is_null),
        "RECOVERY_EXISTING_QUEUED_WORK_RESOLVE_BEFORE_RESUME"
    );
    let recheck: Value = control
        .request("thread/read", json!({"threadId":id,"includeTurns":false}))
        .await?;
    ensure!(
        recheck["thread"]["id"] == id && recheck["thread"]["status"] == *status,
        "RECOVERY_STATE_CHANGED_OBSERVE_AGAIN"
    );
    Ok(
        json!({"previous_turn_id":turns.first().map(|t|t["id"].clone()),"previous_turn_status":turns.first().map(|t|t["status"].clone())}),
    )
}
#[derive(Clone, Serialize, Deserialize)]
struct Record {
    schema_version: u32,
    request: PublicRequest,
    session: crate::catalog::Session,
    phase: String,
    detail: Option<String>,
    target: Option<String>,
    launched_generation: Option<String>,
    created_at: i64,
    #[serde(default)]
    updated_at: i64,
    #[serde(default)]
    launch_confirmed: bool,
}
fn file(paths: &Paths, id: &str) -> anyhow::Result<PathBuf> {
    uuid(id)?;
    Ok(paths
        .root
        .join("message-recoveries")
        .join(format!("{id}.json")))
}
fn read(paths: &Paths, id: &str) -> anyhow::Result<Record> {
    let r: Record = serde_json::from_slice(&fs::read(file(paths, id)?)?)?;
    ensure!(
        r.schema_version == 1,
        "RECOVERY_JOURNAL_VERSION_UNSUPPORTED"
    );
    Ok(r)
}
fn save(paths: &Paths, r: &Record) -> anyhow::Result<()> {
    private_dir(&paths.root.join("message-recoveries"))?;
    crate::paths::atomic_write(
        &file(paths, &r.request.message_id)?,
        &serde_json::to_vec(&{
            let mut saved = r.clone();
            saved.updated_at = crate::daemon::now();
            saved
        })?,
    )
}
pub(super) fn exists(paths: &Paths, id: &str) -> bool {
    file(paths, id).is_ok_and(|p| p.is_file())
}
fn receipt_record(r: &Record) -> Value {
    json!({"message_id":r.request.message_id,"session_id":r.session.id,"provider":"codex",
        "provider_conversation_id":r.session.conversation_id,"accepted_generation":r.request.expected_generation,"mode":"resume",
        "state":if r.phase=="failed" {"rejected"}else if r.phase=="uncertain" {"launch_unknown"}else{"pending_local"},
        "reason":r.detail,"accepted_runtime_generation":null,"native_queue_id":null,"correlation":null,"reply":null,
        "observation":{"state":"not_checked","checked_at":crate::daemon::now(),"native_attempted_at":null,"evidence_observed_at":null,"freshness":"unknown","error_code":null},"facts":{"accepted_local":true,"accepted_native":false,"input_recorded":false,"input_consumed":"unknown","reply_observed":false},
        "launch":{"required":r.request.recovery.as_ref().is_some_and(|i|i.launch.is_some()),"phase":r.phase,
            "confirmed":r.launch_confirmed,"detail":r.detail,"generation":r.launched_generation,"target":r.target,"created_at":r.created_at,"updated_at":r.updated_at,"reconcile_command":["api","recovery-reconcile",r.request.message_id]},
        "delivery_semantics":"non_steering_native_queue"})
}
pub(super) fn decorate(paths: &Paths, id: &str, mut value: Value) -> anyhow::Result<Value> {
    if exists(paths, id) {
        let r = read(paths, id)?;
        value["launch"] = receipt_record(&r)["launch"].clone();
    }
    Ok(value)
}
pub(super) fn pending_receipt(paths: &Paths, id: &str) -> anyhow::Result<Value> {
    Ok(receipt_record(&read(paths, id)?))
}
fn intent(request: &PublicRequest) -> anyhow::Result<&Intent> {
    request
        .recovery
        .as_ref()
        .context("RECOVERY_PRECONDITIONS_REQUIRED")
}
async fn validate(
    paths: &Paths,
    request: &PublicRequest,
    own: Option<&str>,
) -> anyhow::Result<crate::catalog::Session> {
    ensure!(
        request.expires_at > crate::daemon::now(),
        "RECOVERY_REQUEST_EXPIRED_NO_LAUNCH"
    );
    let s = crate::catalog::resolve(paths, &request.session_id, &request.expected_generation)?;
    ensure!(s.provider == "codex", "UNSUPPORTED_PROVIDER_OPERATION");
    ensure!(
        !matches!(s.account.as_str(), "external" | "unknown"),
        "RECOVERY_ACCOUNT_POLICY_AMBIGUOUS"
    );
    let i = intent(request)?;
    ensure!(
        matches!(
            i.expected_state.as_str(),
            "stopped" | "interrupted" | "errored"
        ),
        "RECOVERY_EXPECTED_STATE_MUST_BE_STOPPED_INTERRUPTED_OR_ERRORED"
    );
    crate::lifecycle::ensure_available(paths, &s.conversation_id, own)?;
    crate::lifecycle::ensure_no_handoff(paths, &s.conversation_id, own)?;
    ensure!(
        modes::queue::sending_enabled(paths)?,
        "RECOVERY_NATIVE_SENDING_DISABLED"
    );
    ensure!(
        external::messaging_stop_intent(paths, &s.conversation_id)? == i.expected_stop_id,
        "RECOVERY_STOP_INTENT_CHANGED_OBSERVE_AGAIN"
    );
    ensure!(
        !external::archiving::metadata(paths)?
            .get(&s.id)
            .is_some_and(|v| v["archived"] == true),
        "RECOVERY_RESTORE_ARCHIVE_FIRST"
    );
    validate_owner(paths, &s.account).await?;
    let live = readiness::recorded(paths, &s)?;
    let mut native = if let Some(a) = live {
        ensure!(
            i.launch.is_none(),
            "RECOVERY_LIVE_ATTACHMENT_DOES_NOT_NEED_LAUNCH"
        );
        ensure!(
            i.expected_runtime_generation.as_deref() == Some(&a.generation()),
            "RECOVERY_RUNTIME_CHANGED_OBSERVE_AGAIN"
        );
        a.connect().await?
    } else {
        let p = i
            .launch
            .as_ref()
            .context("RECOVERY_EXITED_PROCESS_REQUIRES_VALIDATED_LAUNCH_PLAN")?;
        let current = plan_inner(
            paths,
            PlanRequest {
                session_id: s.id.clone(),
                expected_generation: s.generation(),
                host: p.host.clone(),
            },
            own,
        )
        .await?;
        let current: Plan = serde_json::from_value(current)?;
        ensure!(
            current.session_id == p.session_id
                && current.provider_conversation_id == p.provider_conversation_id,
            "RECOVERY_TARGET_CHANGED_REVALIDATE"
        );
        ensure!(
            current.launcher_executable == p.launcher_executable
                && current.launcher_sha256 == p.launcher_sha256,
            "RECOVERY_BACKEND_ARTIFACT_CHANGED_REVALIDATE"
        );
        ensure!(
            current.account_id == p.account_id,
            "RECOVERY_ACCOUNT_POLICY_CHANGED_REVALIDATE"
        );
        ensure!(
            current.executable == p.executable && current.executable_sha256 == p.executable_sha256,
            "RECOVERY_EXECUTABLE_CHANGED_REVALIDATE"
        );
        ensure!(
            current.directory == p.directory && current.directory_identity == p.directory_identity,
            "RECOVERY_DIRECTORY_CHANGED_REVALIDATE"
        );
        ensure!(
            current.configuration_sha256 == p.configuration_sha256,
            "RECOVERY_CONFIGURATION_CHANGED_REVALIDATE"
        );
        ensure!(
            current.history_sha256 == p.history_sha256,
            "RECOVERY_HISTORY_CHANGED_REVALIDATE"
        );
        ensure!(
            current.host_identity == p.host_identity,
            "RECOVERY_HOST_RESTARTED_REVALIDATE"
        );
        ensure!(
            current.native_endpoint_generation == p.native_endpoint_generation,
            "RECOVERY_NATIVE_OWNER_CHANGED_REVALIDATE"
        );
        ensure!(current == *p, "RECOVERY_LAUNCH_PLAN_CHANGED_REVALIDATE");
        home_attachment(paths, &s)?.connect().await?
    };
    native_preflight(&mut native, &s.conversation_id, Some(i), i.launch.is_some()).await?;
    background_quiescent(paths, &s).await?;
    Ok(s)
}
pub(super) async fn send(paths: &Paths, request: PublicRequest) -> anyhow::Result<Value> {
    uuid(&request.message_id)?;
    uuid(&request.session_id)?;
    ensure!(
        request.api_version == crate::api::VERSION,
        "UNSUPPORTED_API_VERSION"
    );
    let now = crate::daemon::now();
    ensure!(
        !request.text.is_empty() && request.text.len() <= 16384,
        "INVALID_REQUEST"
    );
    let ledger = Ledger::open(paths)?;
    public::initialize(&ledger)?;
    if exists(paths, &request.message_id) {
        let old = read(paths, &request.message_id)?;
        ensure!(old.request == request, "MESSAGE_ID_CONFLICT");
        drop(ledger);
        return public::receipt_public(paths, &request.message_id, false).await;
    }
    ensure!(
        request.expires_at > now && request.expires_at <= now + 86400,
        "INVALID_EXPIRY"
    );
    ensure!(
        public::get(&ledger, &request.message_id)?.is_none()
            && ledger.get(&request.message_id)?.is_none(),
        "MESSAGE_ID_CONFLICT"
    );
    let selected =
        crate::catalog::resolve(paths, &request.session_id, &request.expected_generation)?;
    let _admission =
        crate::lifecycle::admit(paths, &selected.conversation_id, Some(&request.message_id))?;
    let session = validate(paths, &request, Some(&request.message_id)).await?;
    let count = fs::read_dir(paths.root.join("message-recoveries"))
        .map(|es| {
            es.filter_map(std::result::Result::ok)
                .filter(|e| e.path().extension().is_some_and(|s| s == "json"))
                .count()
        })
        .unwrap_or(0);
    ensure!(count < 10000, "RECEIPT_CAPACITY_REACHED");
    let mut record = Record {
        schema_version: 1,
        request,
        session,
        phase: "admitted".into(),
        detail: None,
        target: None,
        launched_generation: None,
        created_at: now,
        updated_at: now,
        launch_confirmed: false,
    };
    save(paths, &record)?;
    let child = crate::background::detach(&mut std::process::Command::new(
        crate::self_executable::path()?,
    ))
    .arg("--data-dir")
    .arg(&paths.root)
    .args(["message-recovery-worker", &record.request.message_id])
    .stdin(Stdio::null())
    .stdout(Stdio::null())
    .stderr(Stdio::null())
    .spawn();
    if child.is_err() {
        record.phase = "failed".into();
        record.detail = Some("RECOVERY_WORKER_COULD_NOT_START_NO_PROVIDER_ACTION".into());
        save(paths, &record)?;
    }
    Ok(receipt_record(&record))
}
mod launching;
pub(crate) use launching::{attached, launch_material};
pub use launching::{run, worker};
pub(super) async fn deliver(
    paths: &Paths,
    ledger: &mut Ledger,
    receipt: &mut Receipt,
    input: &Enqueue,
    request: &PublicRequest,
) -> anyhow::Result<()> {
    let _guard = mutation_lock(paths, &receipt.session_id)?;
    crate::lifecycle::ensure_available(paths, &receipt.session_id, Some(&request.message_id))?;
    crate::lifecycle::ensure_no_handoff(paths, &receipt.session_id, Some(&request.message_id))?;
    let a = attachment(paths, &receipt.session_id)?;
    ensure!(a.generation() == receipt.generation, "STALE_TARGET");
    let mut control = a.connect().await?;
    if receipt.state == "unknown" {
        reconcile(&mut control, ledger, receipt).await?;
        return Ok(());
    }
    if receipt.state != "pending_local" {
        return Ok(());
    }
    let i = intent(request)?;
    ensure!(
        modes::queue::sending_enabled(paths)?,
        "native_sending_disabled"
    );
    ensure!(
        external::messaging_stop_intent(paths, &receipt.session_id)? == i.expected_stop_id,
        "RECOVERY_STOP_INTENT_CHANGED_OBSERVE_AGAIN"
    );
    // Process recovery may load the saved thread, but cannot change its last turn.
    native_preflight(
        &mut control,
        &receipt.session_id,
        Some(i),
        i.launch.is_some(),
    )
    .await?;
    ensure!(
        attachment(paths, &receipt.session_id)?.generation() == receipt.generation,
        "STALE_TARGET"
    );
    submit(&mut control, ledger, receipt, input).await?;
    if receipt.state == "native_accepted" {
        start_accepted(&mut control, ledger, receipt).await?;
    }
    if receipt
        .recovery_start
        .as_ref()
        .is_some_and(|s| s.state == "confirmed")
    {
        if let Some(stop) = i.expected_stop_id.as_deref() {
            external::reconcile_recovery_stop_intent(paths, &receipt.session_id, stop)?;
        }
    }
    Ok(())
}

// Only the original explicit recovery dispatch calls this. Persist before RPC;
// retries, receipt reads and reconciliation never repeat a possible start.
async fn start_accepted(
    control: &mut Transport,
    ledger: &mut Ledger,
    receipt: &mut Receipt,
) -> Result<()> {
    if receipt.recovery_start.is_some() || receipt.state != "native_accepted" {
        return Ok(());
    }
    let queued = receipt
        .native_queue_id
        .clone()
        .ok_or(Fault("malformed_response"))?;
    receipt.recovery_start = Some(RecoveryStart {
        state: "unknown".into(),
        turn_id: None,
    });
    receipt.reason = Some("recovery_start_uncertain".into());
    ledger.save(receipt)?;
    match control.start_queued(&receipt.session_id, &queued).await {
        Ok(turn_id) => {
            receipt.recovery_start = Some(RecoveryStart {
                state: "confirmed".into(),
                turn_id: Some(turn_id),
            });
            receipt.reason = None;
        }
        Err(Fault("native_rejected")) => {
            // The item may already have auto-started. Do not infer cancellation,
            // consumption or readiness from rejection, and never enqueue again.
            receipt.recovery_start = Some(RecoveryStart {
                state: "rejected".into(),
                turn_id: None,
            });
            receipt.reason = Some("recovery_start_rejected_observe_original_input".into());
        }
        Err(_) => {}
    }
    ledger.save(receipt)
}

/// Explicit local-journal reconciliation. Never launches, resumes, queues or cancels.
pub async fn reconcile_recovery(paths: &Paths, id: &str) -> anyhow::Result<Value> {
    let lease = lock_file(&file(paths, id)?.with_extension("worker.lock"))?;
    lease
        .try_lock_exclusive()
        .context("RECOVERY_WORKER_STILL_RUNNING_WAIT_AND_OBSERVE")?;
    let mut r = read(paths, id)?;
    if matches!(r.phase.as_str(), "failed" | "complete") {
        return public::receipt_public(paths, id, false).await;
    }
    if matches!(
        r.phase.as_str(),
        "admitted" | "preparing" | "creating_terminal"
    ) {
        // A durable dispatching phase is always written before a terminal receives
        // the provider command. A possibly created plain shell is harmless.
        r.phase = "failed".into();
        r.detail=Some("Recovery worker stopped before provider dispatch; message was not attempted. A created plain terminal may remain. This ID will never launch or send.".into());
        save(paths, &r)?;
        return Ok(receipt_record(&r));
    }
    let generation = r
        .launched_generation
        .as_deref()
        .context("RECOVERY_LAUNCH_UNCERTAIN_INSPECT_DESTINATION_DO_NOT_RETRY")?;
    let session = crate::catalog::resolve(paths, &r.session.id, generation)
        .context("RECOVERY_TARGET_CHANGED_INSPECT_ORIGINAL_DESTINATION")?;
    ensure!(
        session.account == r.session.account
            && session.conversation_id == r.session.conversation_id,
        "RECOVERY_IDENTITY_MISMATCH"
    );
    let confirmed = if intent(&r.request)?.launch.is_some() {
        external::new_launch_observed(paths, &session)?
    } else {
        readiness::recorded(paths, &session)?.is_some()
    };
    ensure!(
        confirmed,
        "RECOVERY_LAUNCH_UNCERTAIN_INSPECT_DESTINATION_DO_NOT_RETRY"
    );
    let ledger = Ledger::open(paths)?;
    public::initialize(&ledger)?;
    let bound = public::get(&ledger, id)?.is_some();
    r.launch_confirmed = true;
    r.phase = if bound { "complete" } else { "failed" }.into();
    r.detail=Some(if bound {"Launch confirmed; inspect the original delivery receipt. Reconciliation sent no input."}
        else {"Launch confirmed; worker stopped before message dispatch. This ID will never send; inspect the session before requesting new input."}.into());
    save(paths, &r)?;
    drop(ledger);
    public::receipt_public(paths, id, false).await
}

/// Human guidance alongside stable codes; never provider payloads or credentials.
pub fn diagnostic(code: &str) -> Option<&'static str> {
    match code {
        "RECOVERY_EXITED_PROCESS_REQUIRES_VALIDATED_LAUNCH_PLAN" => Some(
            "The agent has exited. Obtain a recovery-plan for an explicit destination, then send Resume with that plan.",
        ),
        "RECOVERY_RUNTIME_CHANGED_OBSERVE_AGAIN" => Some(
            "The runtime changed. Observe the session again and review its current runtime generation.",
        ),
        "RECOVERY_PREVIOUS_TURN_CHANGED_OBSERVE_AGAIN"
        | "RECOVERY_STOP_INTENT_CHANGED_OBSERVE_AGAIN"
        | "RECOVERY_STATE_CHANGED_OBSERVE_AGAIN" => Some(
            "The conversation changed since inspection. Observe it again and review the current turn and stop intent.",
        ),
        "RECOVERY_EXISTING_QUEUED_WORK_RESOLVE_BEFORE_RESUME" => Some(
            "Queued input already exists. Inspect and resolve it before recovery; loading the conversation could otherwise run it.",
        ),
        "RECOVERY_PERMISSIONS_MISSING_RECORD_EXPLICIT_LAUNCH_POLICY_FIRST" => Some(
            "Saved history has no complete approval and sandbox policy. Recovery cannot guess permissions; use an explicitly configured launch.",
        ),
        "RECOVERY_SUPERVISOR_UPGRADE_REQUIRED_COORDINATE_ALL_ROOT_WRITERS" => Some(
            "The current supervisor does not support recovery reservations. Coordinate a compatible owner/client upgrade; do not start a second supervisor.",
        ),
        "RECOVERY_LAUNCH_UNCERTAIN_INSPECT_DESTINATION_DO_NOT_RETRY"
        | "RECOVERY_LAUNCH_ACK_UNCERTAIN_DO_NOT_RETRY"
        | "RECOVERY_PROVIDER_EXEC_NOT_CONFIRMED_DO_NOT_RETRY" => Some(
            "Launch is uncertain. Keep the original request ID, inspect its receipt and destination, and use recovery-reconcile. No automatic retry is safe.",
        ),
        "RECOVERY_WORKER_STILL_RUNNING_WAIT_AND_OBSERVE" => Some(
            "The original recovery worker is still running. Observe its receipt and wait before reconciling.",
        ),
        "RECOVERY_LIVE_THREAD_UNLOADED_RECONNECT_OR_STOP_FIRST" => Some(
            "The agent process exists but its conversation is unloaded. Reconnect it, or explicitly stop it before preparing exited-process recovery.",
        ),
        "RECOVERY_BACKGROUND_WORK_ACTIVE_STOP_IT_EXPLICITLY_FIRST" => Some(
            "Background work is still active. Inspect and explicitly stop that work before requesting recovery.",
        ),
        "RECOVERY_BACKEND_ARTIFACT_MISSING_USE_IMMUTABLE_INSTALL" => Some(
            "The backend executable was replaced or removed. Select an immutable installed artifact and generate a new plan.",
        ),
        _ if code.starts_with("RECOVERY_") && code.ends_with("REVALIDATE") => Some(
            "The launch plan no longer matches the target. Generate a new plan and review the changed values before retrying.",
        ),
        _ => None,
    }
}

#[cfg(test)]
mod tests {
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn slow_legacy_summary_shares_the_bounded_recovery_preflight_budget() {
        use crate::messaging::tests::{attachment_fixture, fixture, scripted_server};
        let (_dir, paths, input, _) = fixture();
        let a = attachment_fixture(&paths, &input);
        let id = a.id.clone();
        let server = scripted_server(&a.endpoint(), move |r| {
            match r["method"].as_str().unwrap() {
                "thread/read" => Some(json!({"thread":{"id":id,"status":{"type":"idle"}}})),
                "thread/turns/list" => {
                    std::thread::sleep(std::time::Duration::from_millis(3250));
                    Some(json!({"data":[{"id":"previous","status":"interrupted"}]}))
                }
                "thread/queue/list" => Some(json!({"data":[],"nextCursor":null})),
                _ => panic!("Preflight must not write"),
            }
        })
        .await;
        let mut control = a.connect().await.unwrap();
        let result = native_preflight(&mut control, &a.id, None, false)
            .await
            .unwrap();
        assert_eq!(result["previous_turn_id"], "previous");
        server.abort();
    }
    use super::*;
    #[tokio::test]
    async fn interrupted_worker_reconciliation_never_reclaims_a_launch_or_expired_message() {
        let (_dir, paths, input, _) = super::super::tests::fixture();
        let session = crate::catalog::Session {
            id: uuid::Uuid::new_v4().to_string(),
            name: "recovery".into(),
            provider: "codex".into(),
            account: "selected".into(),
            conversation_id: input.session_id,
            revision: 1,
            attachment: None,
        };
        let request = PublicRequest {
            steering: None,
            api_version: 1,
            message_id: input.message_id,
            session_id: session.id.clone(),
            expected_generation: session.generation(),
            mode: PublicMode::Resume,
            text: "fixture".into(),
            expires_at: 1,
            recovery: Some(Intent {
                expected_state: "stopped".into(),
                expected_previous_turn_id: None,
                expected_stop_id: None,
                expected_runtime_generation: None,
                launch: None,
            }),
        };
        let mut record = Record {
            schema_version: 1,
            request: request.clone(),
            session,
            phase: "preparing".into(),
            detail: None,
            target: None,
            launched_generation: None,
            created_at: 1,
            updated_at: 1,
            launch_confirmed: false,
        };
        save(&paths, &record).unwrap();
        let r = reconcile_recovery(&paths, &request.message_id)
            .await
            .unwrap();
        assert_eq!(r["state"], "rejected");
        assert_eq!(r["facts"]["accepted_native"], false);
        let retry = send(&paths, request.clone()).await.unwrap();
        assert_eq!(retry["state"], "rejected");
        record.phase = "dispatching".into();
        save(&paths, &record).unwrap();
        assert!(
            reconcile_recovery(&paths, &request.message_id)
                .await
                .unwrap_err()
                .to_string()
                .contains("RECOVERY_LAUNCH_UNCERTAIN")
        );
        assert_eq!(
            read(&paths, &request.message_id).unwrap().phase,
            "dispatching"
        );
        assert!(
            crate::lifecycle::pending(&paths, &record.session.conversation_id, None)
                .unwrap()
                .is_some()
        );
        let mut changed = request;
        changed.text = "changed".into();
        assert!(
            send(&paths, changed)
                .await
                .unwrap_err()
                .to_string()
                .contains("MESSAGE_ID_CONFLICT")
        );
    }
}

#[cfg(test)]
mod dispatch_tests {
    use super::*;
    use crate::messaging::tests::{attachment_fixture, fixture, scripted_server};
    #[tokio::test]
    async fn explicit_start_is_exact_once_and_lost_ack_is_never_replayed() {
        for lose_ack in [false, true] {
            let (_dir, paths, input, now) = fixture();
            let a = attachment_fixture(&paths, &input);
            let calls = std::sync::Arc::new(std::sync::atomic::AtomicUsize::new(0));
            let seen = calls.clone();
            let task = scripted_server(&a.endpoint(), move |request| {
                assert_eq!(request["method"], "thread/queue/start");
                assert_eq!(request["params"]["queuedSubmissionId"], "only-this-item");
                seen.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
                (!lose_ack).then(|| json!({"turn":{"id":"started-turn","status":"inProgress"}}))
            })
            .await;
            let mut control = a.connect().await.unwrap();
            let mut ledger = Ledger::open(&paths).unwrap();
            let mut receipt = ledger.accept(&input, now).unwrap();
            receipt.state = "native_accepted".into();
            receipt.native_queue_id = Some("only-this-item".into());
            ledger.save(&receipt).unwrap();
            start_accepted(&mut control, &mut ledger, &mut receipt)
                .await
                .unwrap();
            assert_eq!(
                receipt.recovery_start.as_ref().unwrap().state,
                if lose_ack { "unknown" } else { "confirmed" }
            );
            assert!(receipt.correlation.is_none());
            drop(ledger);
            let mut ledger = Ledger::open(&paths).unwrap();
            let mut receipt = ledger.get(&input.message_id).unwrap().unwrap();
            start_accepted(&mut control, &mut ledger, &mut receipt)
                .await
                .unwrap();
            assert_eq!(calls.load(std::sync::atomic::Ordering::SeqCst), 1);
            task.abort();
        }
    }
}
