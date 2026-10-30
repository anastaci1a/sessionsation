//! Google Antigravity CLI (`agy`) provider (contract revision 24). Each account
//! is an isolated `HOME` created at `<root>/accounts/antigravity/<name>`; agy
//! keeps everything under `<home>/.gemini/antigravity-cli/` (plus
//! `.gemini/config` and `.config/antigravity`). Children get the account home
//! as both `AGY_ACCOUNT_HOME` (the host wrapper's required variable) and `HOME`.
//!
//! Sessionsation never reads agy credentials: an account is logged in when
//! `antigravity-oauth-token` is a non-empty regular file (stat only).
//! Conversations come from agy's own index `conversation_summaries.db`
//! (read-only: id, workspace URI, step count, times) and exist while
//! `conversations/<id>.db` does; message text is read only from
//! `brain/<id>/.system_generated/logs/transcript.jsonl` to anchor typed input.
//! Liveness comes from `/proc`: an `agy` process whose `HOME` is this home and
//! that holds `presence/<id>.lock` (or `conversations/<id>.db`) open, or names
//! `--conversation <id>`. agy cannot pre-assign an id, so a new session first
//! creates an empty conversation with a bounded headless run that sends no
//! prompt (`--input-format stream-json` with empty input) and then opens it.
use super::{
    Capability, ProviderCapabilities, ProviderId,
    claude::{Enrolled, Live, Observation, Transcript, environ, proc_start, regular_file},
    no, yes,
};
use crate::{
    catalog,
    config::{Config, LaunchPermissions},
    paths::{Paths, private_dir, validate_name},
};
use anyhow::{Context, Result, bail, ensure};
use serde_json::{Value, json};
use std::{
    collections::BTreeMap,
    fs,
    io::Read,
    path::{Path, PathBuf},
    time::Duration,
};
pub mod index;
pub mod launch;
pub mod trust;

/// Ambient variables that would switch agy's auth, billing, endpoint or model.
/// The same list the host wrapper clears, plus `XDG_RUNTIME_DIR` (agy's peer
/// sockets must not land in a RAM runtime directory).
pub const STRIPPED_ENV: &[&str] = &[
    "GEMINI_API_KEY",
    "GOOGLE_API_KEY",
    "GOOGLE_GENAI_USE_VERTEXAI",
    "GOOGLE_GENAI_USE_GCA",
    "GOOGLE_CLOUD_PROJECT",
    "GOOGLE_CLOUD_LOCATION",
    "GOOGLE_CLOUD_ACCESS_TOKEN",
    "GOOGLE_APPLICATION_CREDENTIALS",
    "GOOGLE_GEMINI_BASE_URL",
    "GOOGLE_VERTEX_BASE_URL",
    "CLOUD_SHELL",
    "GEMINI_CLI_USE_COMPUTE_ADC",
    "GEMINI_MODEL",
    "ANTIGRAVITY_API_KEY",
    "AGY_LLM_GATEWAY_URL",
    "AGY_LLM_GATEWAY_PROXY_URL",
    "AGY_ADC_AUTH",
    "XDG_RUNTIME_DIR",
];
/// The host wrapper's account variable.
pub const HOME_ENV: &str = "AGY_ACCOUNT_HOME";
/// The credential file (stat only, never opened).
pub const TOKEN_FILE: &str = "antigravity-oauth-token";
pub const YOLO_FLAG: &str = "--dangerously-skip-permissions";
pub const USAGE_SOURCE: &str = "antigravity-cli-unsupported";
pub const USAGE_SEMANTICS: &str = "Antigravity CLI shows quota only in its interactive /usage view; headless JSON reports token counts, not quota, so no window is ever reported and none is shown as 0%";
pub const USAGE_UNSUPPORTED: &str = "Antigravity CLI exposes quota only in its interactive /usage view; no headless output or file reports it";
pub const NATIVE_HOME_BOUND: &str = "native session is bound to its account home";
pub const NO_HERDR_ATTACH: &str = "NO_NATIVE_ATTACH: Antigravity CLI has no supported attach to a running session; stop it in its terminal, then use Start Session";
pub const NO_INPUT_API: &str = "no supported native input API for a live Antigravity session";
pub const INPUT_DELIVERY: &str = "live sessions in a verified HERDR pane take typed input (terminal_input): Enter or alt+m by the session's queuedMessages setting; see each session's capabilities";
pub const LABEL_ONLY: &str =
    "label is a Sessionsation catalog label; Antigravity CLI renames only inside its own /rename";
pub const ARCHIVE: &str = "stopped sessions: the conversation's exact-id agy files (conversations/<id>.db, brain/<id>/, annotations/<id>.pbtxt, presence/<id>.lock) move to sessionsation-managed Archived Sessions and its conversation_summaries.db row is saved and removed in one SQLite transaction after a backup; restore puts both back; refused while running or hosted";
pub const DELETE: &str = "stopped or archived sessions: the same exact-id files and index row are staged and removed, and the catalog entry is deleted; agy's prompt history (history.jsonl) is unchanged, as with agy's own picker delete";
pub const STOP: &str = "running sessions: SIGTERM to the exact verified agy process (pid, start time, command line, home and conversation re-checked through a pidfd); agy exits like /exit and flushes its database; never SIGKILL, a process that does not exit is reported";
pub const NO_LOGOUT: &str = "UNSUPPORTED_PROVIDER_OPERATION: Antigravity CLI signs out only with /logout inside agy; run agy in the account (account login antigravity:NAME) and use /logout there";
const NO_RESTART: &str =
    "restart is not supported for Antigravity CLI: stop the session, then resume it";
/// Bound for the empty run that creates a new conversation (no prompt is sent).
pub const ALLOCATE_SECONDS: u64 = 30;

fn input() -> Capability {
    Capability {
        supported: true,
        reason: Some(INPUT_DELIVERY),
    }
}

pub fn capabilities() -> ProviderCapabilities {
    ProviderCapabilities {
        enrollment: yes(),
        adopt_config_dir: no("Antigravity homes are created and owned by sessionsation"),
        login: yes(),
        auth_status: Capability {
            supported: true,
            reason: Some(
                "file-based: the agy OAuth token file is present (stat only); validity is not checked without a network call",
            ),
        },
        usage: no(USAGE_UNSUPPORTED),
        new: yes(),
        resume: yes(),
        rename: yes(),
        native_rename: no(LABEL_ONLY),
        stop: Capability {
            supported: true,
            reason: Some(STOP),
        },
        restart: no(NO_RESTART),
        account_switch: no(NATIVE_HOME_BOUND),
        relocate: no(NATIVE_HOME_BOUND),
        move_space: no("HERDR space moves are verified only for Codex"),
        archive: Capability {
            supported: true,
            reason: Some(ARCHIVE),
        },
        delete_session: Capability {
            supported: true,
            reason: Some(DELETE),
        },
        submit: input(),
        queue: input(),
        steer: input(),
        account_removal: yes(),
        account_deletion: Capability {
            supported: true,
            reason: Some(
                "managed homes: conversation history preserved, home and credentials deleted",
            ),
        },
        account_rename: yes(),
        account_logout: no(NO_LOGOUT),
        headless_delivery: Capability {
            supported: true,
            reason: Some(
                "api send mode resume (stopped session) and mode new run agy -p with stream-json input and --print-timeout; live sessions are refused",
            ),
        },
        account_limits: no(USAGE_UNSUPPORTED),
        cross_provider_conversion: no("conversations are bound to their provider"),
    }
}

// ------------------------------------------------------------------- homes

pub fn home(paths: &Paths, name: &str) -> Result<PathBuf> {
    validate_name(name)?;
    Ok(crate::layout::account_home(
        &paths.root,
        ProviderId::Antigravity,
        name,
    ))
}
/// Create the managed home (0700), or register one an operator already placed
/// there; an existing home must be a private (0700) directory owned by this
/// user and not a symlink, and is never wiped. Returns (home, existed).
pub fn create_home(paths: &Paths, name: &str) -> Result<(PathBuf, bool)> {
    use std::os::unix::fs::MetadataExt;
    private_dir(home(paths, name)?.parent().context("missing parent")?)?;
    let home = home(paths, name)?;
    let existed = match fs::symlink_metadata(&home) {
        Ok(m) => {
            ensure!(
                m.file_type().is_dir()
                    && m.uid() == unsafe { libc::geteuid() }
                    && m.mode() & 0o777 == 0o700,
                "UNSAFE_ACCOUNT_HOME: existing {} must be a 0700 directory owned by this user and not a symlink; it was left unchanged",
                home.display()
            );
            true
        }
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
            private_dir(&home)?;
            false
        }
        Err(e) => return Err(e.into()),
    };
    Ok((home, existed))
}
/// `<home>/.gemini/antigravity-cli`
pub fn state_dir(home: &Path) -> PathBuf {
    home.join(".gemini/antigravity-cli")
}

// --------------------------------------------------------------- environment

/// The single builder for every agy child: binary, `AGY_ACCOUNT_HOME` and
/// `HOME` set to the account home, `XDG_CACHE_HOME` inside it, the self-updater
/// off and ambient auth/endpoint overrides removed.
pub fn command(config: &Config, home: &Path) -> std::process::Command {
    let mut command = std::process::Command::new(&config.antigravity_binary);
    for name in STRIPPED_ENV {
        command.env_remove(name);
    }
    command
        .env(HOME_ENV, home)
        .env("HOME", home)
        .env("XDG_CACHE_HOME", home.join(".cache"))
        .env("AGY_CLI_DISABLE_AUTO_UPDATE", "1");
    command
}

// --------------------------------------------------------------------- auth

#[derive(Clone, Debug, PartialEq)]
pub struct AuthStatus {
    pub logged_in: bool,
}
impl AuthStatus {
    pub fn problem(&self) -> Option<&'static str> {
        (!self.logged_in).then_some("AUTH_REQUIRED")
    }
    pub fn gate(&self) -> Result<()> {
        ensure!(
            self.logged_in,
            "AUTH_REQUIRED: this Antigravity account is not logged in; run `sessionsation account login antigravity:<name>`"
        );
        Ok(())
    }
    pub fn mode(&self) -> Value {
        json!({"logged_in":self.logged_in,"auth_method":self.logged_in.then_some("google-oauth"),
            "api_provider":null,"subscription_type":null})
    }
}
fn token_path(home: &Path) -> PathBuf {
    state_dir(home).join(TOKEN_FILE)
}
/// Presence of the OAuth token file: stat only, never opened.
pub fn auth_status(home: &Path) -> AuthStatus {
    AuthStatus {
        logged_in: fs::symlink_metadata(token_path(home))
            .is_ok_and(|m| m.file_type().is_file() && m.len() > 0),
    }
}
/// Identity of the token file (modified time and size), stat only.
pub fn credentials_stamp(home: &Path) -> Option<(std::time::SystemTime, u64)> {
    let m = fs::symlink_metadata(token_path(home)).ok()?;
    Some((m.modified().ok()?, m.len()))
}
/// True once a login that started with `before` has produced a token.
pub fn login_completed(home: &Path, before: Option<(std::time::SystemTime, u64)>) -> bool {
    auth_status(home).logged_in && credentials_stamp(home) != before
}
/// Non-identifying auth summary (never the email).
pub fn auth_report(home: &Path) -> Value {
    let status = auth_status(home);
    let mut v = status.mode();
    v["problem"] = json!(status.problem());
    v
}
pub fn prelaunch_gate(home: &Path) -> Result<()> {
    auth_status(home).gate()
}
/// Interactive login: agy itself in the account environment. A signed-out agy
/// offers "Google OAuth", prints the Google URL and reads the pasted code.
pub fn login_command(config: &Config, home: &Path) -> std::process::Command {
    command(config, home)
}
/// The working directory for the login: the private parent of the home, so
/// the home itself is not opened (and trusted) as a workspace.
pub fn login_directory(home: &Path) -> PathBuf {
    home.parent().unwrap_or(home).to_path_buf()
}

// ---------------------------------------------------------------- discovery

pub fn conversation_db(home: &Path, id: &str) -> PathBuf {
    state_dir(home)
        .join("conversations")
        .join(format!("{id}.db"))
}
/// agy's per-conversation transcript log (one JSON step per line).
pub fn transcript_log(home: &Path, id: &str) -> PathBuf {
    state_dir(home)
        .join("brain")
        .join(id)
        .join(".system_generated/logs/transcript.jsonl")
}
/// One row of agy's conversation index.
#[derive(Clone, Debug, PartialEq)]
pub struct Summary {
    pub id: String,
    pub cwd: Option<String>,
    pub steps: i64,
    pub modified: Option<String>,
}
/// `file:///a%20b` -> `/a b` (absolute paths only).
fn uri_path(uri: &str) -> Option<String> {
    let raw = uri.strip_prefix("file://")?;
    let bytes = raw.as_bytes();
    let mut out = Vec::with_capacity(bytes.len());
    let mut i = 0;
    while i < bytes.len() {
        if bytes[i] == b'%' && i + 2 < bytes.len() {
            let hex = std::str::from_utf8(&bytes[i + 1..i + 3]).ok()?;
            out.push(u8::from_str_radix(hex, 16).ok()?);
            i += 3;
        } else {
            out.push(bytes[i]);
            i += 1;
        }
    }
    let path = String::from_utf8(out).ok()?;
    Path::new(&path).is_absolute().then_some(path)
}
/// The conversation index, read-only. None when it cannot be read.
pub fn summaries(home: &Path) -> Option<Vec<Summary>> {
    use rusqlite::OpenFlags;
    let path = state_dir(home).join("conversation_summaries.db");
    if !regular_file(&path) {
        return Some(vec![]);
    }
    let db = rusqlite::Connection::open_with_flags(
        &path,
        OpenFlags::SQLITE_OPEN_READ_ONLY | OpenFlags::SQLITE_OPEN_NO_MUTEX,
    )
    .ok()?;
    db.busy_timeout(Duration::from_millis(500)).ok()?;
    let mut statement = db
        .prepare("SELECT conversation_id, workspace_uris, step_count, CAST(last_modified_time AS TEXT) FROM conversation_summaries")
        .ok()?;
    let rows = statement
        .query_map([], |r| {
            Ok((
                r.get::<_, String>(0)?,
                r.get::<_, Option<String>>(1)?,
                r.get::<_, Option<i64>>(2)?,
                r.get::<_, Option<String>>(3)?,
            ))
        })
        .ok()?;
    let mut out = vec![];
    for (id, uris, steps, modified) in rows.flatten() {
        if uuid::Uuid::parse_str(&id).is_err() {
            continue;
        }
        let cwd = uris
            .and_then(|u| serde_json::from_str::<Vec<String>>(&u).ok())
            .and_then(|u| u.first().and_then(|u| uri_path(u)));
        out.push(Summary {
            id,
            cwd,
            steps: steps.unwrap_or(0),
            modified,
        });
    }
    Some(out)
}
fn transcript_of(home: &Path, s: &Summary) -> Option<Transcript> {
    let db = conversation_db(home, &s.id);
    if !regular_file(&db) {
        return None;
    }
    let bytes = fs::symlink_metadata(transcript_log(home, &s.id))
        .map(|m| m.len())
        .unwrap_or(0);
    Some(Transcript {
        session_id: s.id.clone(),
        cwd: s.cwd.clone(),
        started_at: s.modified.clone(),
        path: db,
        bytes,
    })
}
/// Conversations with at least one step (an empty conversation is agy's own
/// scratch, or one Sessionsation created and already records).
pub fn transcripts(home: &Path) -> Vec<Transcript> {
    summaries(home)
        .unwrap_or_default()
        .iter()
        .filter(|s| s.steps > 0)
        .filter_map(|s| transcript_of(home, s))
        .collect()
}
/// The conversation's database file when it exists (any step count).
pub fn find_transcript(home: &Path, id: &str) -> Option<PathBuf> {
    uuid::Uuid::parse_str(id).ok()?;
    let db = conversation_db(home, id);
    regular_file(&db).then_some(db)
}
fn summary(home: &Path, id: &str) -> Option<Summary> {
    summaries(home)?.into_iter().find(|s| s.id == id)
}

// ------------------------------------------------------------ live identity

fn arguments(pid: u32) -> Option<Vec<String>> {
    let mut bytes = vec![];
    fs::File::open(format!("/proc/{pid}/cmdline"))
        .ok()?
        .take(64 * 1024)
        .read_to_end(&mut bytes)
        .ok()?;
    Some(
        bytes
            .split(|b| *b == 0)
            .filter(|a| !a.is_empty())
            .map(|a| String::from_utf8_lossy(a).into_owned())
            .collect(),
    )
}
/// An agy process: its executable (first argument) is named `agy`.
pub(crate) fn is_agy_arguments(args: &[String]) -> bool {
    let name = |i: usize| {
        args.get(i)
            .and_then(|a| Path::new(a).file_name())
            .and_then(|n| n.to_str())
    };
    // The Go binary itself, or a shell running an `agy` script (test fakes).
    name(0) == Some("agy")
        || (matches!(name(0), Some("sh" | "bash" | "dash")) && name(1) == Some("agy"))
}
/// Print mode (`-p`, `--print`, `--prompt`, with or without `=value`).
pub(crate) fn is_print(args: &[String]) -> bool {
    args.iter().skip(1).any(|a| {
        let flag = a.split('=').next().unwrap_or(a);
        matches!(flag, "-p" | "--print" | "-print" | "--prompt" | "-prompt")
    })
}
/// The conversation named on the command line (`--conversation ID` or `=ID`).
pub(crate) fn conversation_argument(args: &[String]) -> Option<String> {
    let mut it = args.iter().skip(1);
    while let Some(a) = it.next() {
        let value = match a.split_once('=') {
            Some(("--conversation" | "-conversation", v)) => Some(v.to_owned()),
            None if matches!(a.as_str(), "--conversation" | "-conversation") => it.next().cloned(),
            _ => None,
        };
        if let Some(v) = value.filter(|v| uuid::Uuid::parse_str(v).is_ok()) {
            return Some(v);
        }
    }
    None
}
fn same_dir(a: &Path, b: &Path) -> bool {
    a == b
        || fs::canonicalize(a)
            .ok()
            .is_some_and(|c| fs::canonicalize(b).is_ok_and(|h| c == h))
}
fn home_of(pid: u32) -> Option<PathBuf> {
    let env = environ(pid)?;
    let get = |k: &str| env.iter().find(|(n, _)| n == k).map(|(_, v)| v.clone());
    get(HOME_ENV).or_else(|| get("HOME")).map(PathBuf::from)
}
fn home_matches(pid: u32, home: &Path) -> bool {
    home_of(pid).is_some_and(|h| same_dir(&h, home))
}
/// The conversation whose presence lock (or database) the process holds open.
fn open_conversation(pid: u32, home: &Path) -> Option<String> {
    let state = fs::canonicalize(state_dir(home)).ok()?;
    let presence = state.join("presence");
    let conversations = state.join("conversations");
    let mut db = None;
    for fd in fs::read_dir(format!("/proc/{pid}/fd")).ok()?.flatten() {
        let Ok(target) = fs::read_link(fd.path()) else {
            continue;
        };
        let Some(name) = target.file_name().and_then(|n| n.to_str()) else {
            continue;
        };
        let parent = target.parent();
        if parent == Some(presence.as_path())
            && let Some(id) = name.strip_suffix(".lock")
            && uuid::Uuid::parse_str(id).is_ok()
        {
            return Some(id.to_owned());
        }
        if parent == Some(conversations.as_path())
            && let Some(id) = name.strip_suffix(".db")
            && uuid::Uuid::parse_str(id).is_ok()
        {
            db = Some(id.to_owned());
        }
    }
    db
}
fn pids() -> Vec<u32> {
    let me = std::process::id();
    fs::read_dir("/proc")
        .into_iter()
        .flatten()
        .flatten()
        .filter_map(|e| e.file_name().to_str().and_then(|n| n.parse::<u32>().ok()))
        .filter(|pid| *pid != me)
        .collect()
}
/// One /proc pass: every live interactive agy process (pid, start, args).
pub struct ProcessSnapshot(Vec<(u32, u64, Vec<String>)>);
impl ProcessSnapshot {
    pub fn take() -> Self {
        let mut found = vec![];
        for pid in pids() {
            let Some(args) = arguments(pid) else { continue };
            if !is_agy_arguments(&args) || is_print(&args) {
                continue;
            }
            let Some((state, start)) = proc_start(pid) else {
                continue;
            };
            if matches!(state, 'Z' | 'X') {
                continue;
            }
            found.push((pid, start, args));
        }
        Self(found)
    }
}
pub fn live_sessions(home: &Path) -> Result<Vec<Live>> {
    live_sessions_in(&ProcessSnapshot::take(), home)
}
/// Verified live sessions of one home: an interactive agy process of this home
/// holding a conversation open (or naming one before it opened it).
pub fn live_sessions_in(snapshot: &ProcessSnapshot, home: &Path) -> Result<Vec<Live>> {
    let mut result = vec![];
    for (pid, start, args) in &snapshot.0 {
        if !home_matches(*pid, home) {
            continue;
        }
        let Some(session_id) =
            open_conversation(*pid, home).or_else(|| conversation_argument(args))
        else {
            continue;
        };
        result.push(Live {
            pid: *pid,
            process_start: *start,
            session_id,
            cwd: fs::read_link(format!("/proc/{pid}/cwd"))
                .ok()
                .and_then(|p| p.to_str().map(str::to_owned)),
            status: None,
            name: None,
            kind: None,
            job_id: None,
            registry_started_ms: None,
            spare: false,
            parked_job_id: None,
            name_source: None,
        });
    }
    result.sort_by_key(|l| l.pid);
    Ok(result)
}
/// Any agy process (interactive or headless) using one of `homes`.
pub(crate) fn env_in_use(homes: &[&Path]) -> bool {
    pids().into_iter().any(|pid| {
        arguments(pid).is_some_and(|a| is_agy_arguments(&a))
            && proc_start(pid).is_some_and(|(s, _)| !matches!(s, 'Z' | 'X'))
            && homes.iter().any(|h| home_matches(pid, h))
    })
}
/// Every file agy keeps for exactly one conversation id (those that exist):
/// the conversation database and its WAL/shm sidecars, the `brain/<id>/`
/// transcript tree, the annotations file and the presence lock. agy's own
/// picker delete removes the first three groups and the index row; the empty
/// presence lock is also moved so that nothing named by the id is left behind.
pub fn conversation_files(home: &Path, id: &str) -> Vec<PathBuf> {
    let state = state_dir(home);
    let db = format!("{id}.db");
    vec![
        state.join("conversations").join(&db),
        state.join("conversations").join(format!("{db}-wal")),
        state.join("conversations").join(format!("{db}-shm")),
        state.join("brain").join(id),
        state.join("annotations").join(format!("{id}.pbtxt")),
        state.join("presence").join(format!("{id}.lock")),
    ]
}
/// The pid of any agy process (interactive or headless) of this home that
/// holds the conversation open or was started on it.
pub fn conversation_in_use(home: &Path, id: &str) -> Option<u32> {
    pids().into_iter().find(|pid| {
        arguments(*pid).is_some_and(|args| {
            is_agy_arguments(&args)
                && proc_start(*pid).is_some_and(|(s, _)| !matches!(s, 'Z' | 'X'))
                && home_matches(*pid, home)
                && (open_conversation(*pid, home).as_deref() == Some(id)
                    || conversation_argument(&args).as_deref() == Some(id))
        })
    })
}

/// What a stop did.
#[derive(Debug, PartialEq, Eq)]
pub struct Stopped {
    pub pid: u32,
    pub process_start: u64,
    pub conversation_id: String,
}
/// Seconds agy gets to exit after SIGTERM before the stop reports a timeout.
pub const STOP_SECONDS: u64 = 20;

fn exited(pid: u32, start: u64) -> bool {
    proc_start(pid).is_none_or(|(state, s)| s != start || matches!(state, 'Z' | 'X'))
}
/// Gracefully stop the one verified interactive agy process that runs
/// `conversation` in `home`: SIGTERM, delivered through a pidfd opened before
/// the identity is re-verified (exact pid, start time, agy command line, home
/// and conversation), then a bounded wait. agy handles SIGTERM like /exit: it
/// flushes and checkpoints its conversation database and prints its resume
/// line. Never SIGKILL: a process that does not exit in time is reported, not
/// killed.
pub fn stop(home: &Path, conversation: &str, wait: Duration) -> Result<Stopped> {
    uuid::Uuid::parse_str(conversation)?;
    let live: Vec<Live> = live_sessions(home)?
        .into_iter()
        .filter(|l| l.session_id == conversation)
        .collect();
    let target = match live.as_slice() {
        [] => bail!("SESSION_NOT_RUNNING: no Antigravity process runs this conversation"),
        [one] => one,
        _ => bail!("AMBIGUOUS_SESSION: several Antigravity processes run this conversation"),
    };
    let (pid, start) = (target.pid, target.process_start);
    // A pidfd names this process object even if the pid number is reused.
    let fd = unsafe { libc::syscall(libc::SYS_pidfd_open, pid as libc::c_int, 0) };
    ensure!(
        fd >= 0,
        "STOP_UNAVAILABLE: could not open a stable handle on process {pid}"
    );
    let fd = fd as libc::c_int;
    let verified = (|| {
        if exited(pid, start) {
            return false;
        }
        let Some(args) = arguments(pid) else {
            return false;
        };
        is_agy_arguments(&args)
            && !is_print(&args)
            && home_matches(pid, home)
            && (open_conversation(pid, home).as_deref() == Some(conversation)
                || conversation_argument(&args).as_deref() == Some(conversation))
    })();
    if !verified {
        unsafe { libc::close(fd) };
        bail!(
            "SESSION_STATE_UNVERIFIED: the Antigravity process changed before it could be stopped; nothing was signalled"
        );
    }
    let sent = unsafe {
        libc::syscall(
            libc::SYS_pidfd_send_signal,
            fd,
            libc::SIGTERM,
            std::ptr::null::<libc::siginfo_t>(),
            0,
        )
    };
    unsafe { libc::close(fd) };
    ensure!(
        sent == 0,
        "STOP_FAILED: could not signal process {pid}: {}",
        std::io::Error::last_os_error()
    );
    let deadline = std::time::Instant::now() + wait;
    while !exited(pid, start) {
        ensure!(
            std::time::Instant::now() < deadline,
            "STOP_TIMEOUT: Antigravity process {pid} did not exit within {}s of SIGTERM; it was not killed",
            wait.as_secs()
        );
        std::thread::sleep(Duration::from_millis(100));
    }
    Ok(Stopped {
        pid,
        process_start: start,
        conversation_id: conversation.to_owned(),
    })
}
/// True when the exact recorded process runs agy for this conversation
/// (PID reuse guarded by start time).
pub fn launch_observed(session: &catalog::Session) -> bool {
    let Some(a) = &session.attachment else {
        return false;
    };
    if proc_start(a.pid)
        .is_none_or(|(state, start)| start != a.process_start || matches!(state, 'Z' | 'X'))
    {
        return false;
    }
    let Some(args) = arguments(a.pid) else {
        return false;
    };
    if !is_agy_arguments(&args) {
        return false;
    }
    let held = home_of(a.pid).and_then(|h| open_conversation(a.pid, &h));
    held.or_else(|| conversation_argument(&args)).as_deref() == Some(&session.conversation_id)
}

fn observation(
    recorded: Option<&catalog::Attachment>,
    session_id: &str,
    found: Option<(PathBuf, Option<String>, Option<String>, u64)>,
    live: &std::result::Result<Vec<Live>, String>,
) -> Observation {
    let (transcript, directory, started_at, bytes) = match found {
        Some((p, cwd, started, bytes)) => (Some(p), cwd, started, Some(bytes)),
        None => (None, None, None, None),
    };
    let mut o = Observation {
        availability: "unknown",
        native_status: None,
        native_name: None,
        pid: None,
        process_start: None,
        directory,
        transcript,
        transcript_bytes: bytes,
        started_at,
        attachment: None,
        detail: None,
        native_kind: None,
        job_id: None,
        attachment_switched: false,
        forked_from: None,
        active_since: None,
    };
    match live {
        Err(e) => o.detail = Some(format!("PROBE_FAILED: {e}")),
        Ok(live) => match live.iter().find(|l| l.session_id == session_id) {
            Some(l) => {
                o.availability = "running";
                o.attachment = super::claude::host_attachment(l, recorded);
                o.pid = Some(l.pid);
                o.process_start = Some(l.process_start);
                o.active_since = super::claude::process_started_at(l.process_start);
                if l.cwd.is_some() {
                    o.directory = l.cwd.clone();
                }
            }
            None => {
                // The recorded process is alive but now holds another conversation.
                o.attachment_switched = recorded.is_some_and(|r| {
                    live.iter().any(|l| {
                        l.pid == r.pid
                            && l.process_start == r.process_start
                            && l.session_id != session_id
                    })
                });
                o.availability = if o.transcript.is_some() {
                    "stopped"
                } else {
                    "stale"
                };
            }
        },
    }
    o
}
pub fn observe(
    home: &Path,
    session_id: &str,
    recorded: Option<&catalog::Attachment>,
) -> Observation {
    let found = find_transcript(home, session_id).map(|db| {
        let s = summary(home, session_id);
        let bytes = fs::symlink_metadata(transcript_log(home, session_id))
            .map(|m| m.len())
            .unwrap_or(0);
        (
            db,
            s.as_ref().and_then(|s| s.cwd.clone()),
            s.and_then(|s| s.modified),
            bytes,
        )
    });
    let live = live_sessions(home).map_err(|e| e.to_string());
    observation(recorded, session_id, found, &live)
}

// ----------------------------------------------------------------- launch args

/// agy flags for a start: `yolo` auto-approves every tool request; `default`
/// passes nothing (the account's own settings decide).
pub fn approval_args(mode: LaunchPermissions) -> Vec<String> {
    match mode {
        LaunchPermissions::Yolo => vec![YOLO_FLAG.into()],
        LaunchPermissions::Default => vec![],
    }
}
/// Interactive open of an existing conversation (new or resumed).
pub fn open_argv(config: &Config, id: &str, mode: LaunchPermissions) -> Vec<String> {
    let mut argv = vec![
        config.antigravity_binary.clone(),
        "--conversation".into(),
        id.into(),
    ];
    argv.extend(approval_args(mode));
    argv
}
/// The empty headless run that only creates a conversation: stream-json input
/// with nothing on stdin runs no turn and sends no prompt.
pub fn allocate_args() -> Vec<String> {
    [
        "--input-format",
        "stream-json",
        "--output-format",
        "stream-json",
        "--print-timeout",
        "20s",
        "--disable-slash-commands",
        "-p=",
    ]
    .map(str::to_owned)
    .to_vec()
}
/// The conversation id of the `init` event in stream-json output.
pub fn allocated_id(stdout: &[u8]) -> Option<String> {
    stdout.split(|b| *b == b'\n').find_map(|line| {
        let v: Value = serde_json::from_slice(line).ok()?;
        (v["event"] == "init")
            .then(|| v["conversation_id"].as_str().map(str::to_owned))
            .flatten()
            .filter(|id| uuid::Uuid::parse_str(id).is_ok())
    })
}
/// Create an empty conversation in `cwd` (no prompt, no model call). Callers
/// gate on authentication first: agy would otherwise wait for a browser login.
pub fn allocate(config: &Config, home: &Path, cwd: &Path) -> Result<String> {
    use std::os::unix::process::CommandExt;
    prelaunch_gate(home)?;
    let mut command = command(config, home);
    command
        .args(allocate_args())
        .current_dir(cwd)
        .stdin(std::process::Stdio::null())
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::null())
        .process_group(0);
    let mut child = command
        .spawn()
        .context("PROVIDER_COULD_NOT_START: could not start agy")?;
    let pid = child.id();
    let mut stdout = child.stdout.take().context("PROVIDER_COULD_NOT_START")?;
    let reader = std::thread::spawn(move || {
        let mut bytes = vec![];
        let _ = (&mut stdout).take(1024 * 1024).read_to_end(&mut bytes);
        bytes
    });
    let deadline = std::time::Instant::now() + Duration::from_secs(ALLOCATE_SECONDS);
    let status = loop {
        if let Some(s) = child.try_wait()? {
            break Some(s);
        }
        if std::time::Instant::now() >= deadline {
            unsafe {
                libc::kill(-(pid as i32), libc::SIGKILL);
            }
            let _ = child.wait();
            break None;
        }
        std::thread::sleep(Duration::from_millis(50));
    };
    let bytes = reader.join().unwrap_or_default();
    let Some(_) = status else {
        bail!(
            "CONVERSATION_CREATE_TIMEOUT: agy did not create a conversation within {ALLOCATE_SECONDS}s"
        );
    };
    let id = allocated_id(&bytes)
        .context("CONVERSATION_CREATE_FAILED: agy reported no new conversation id")?;
    ensure!(
        find_transcript(home, &id).is_some(),
        "CONVERSATION_CREATE_FAILED: agy reported a conversation that does not exist in this account"
    );
    Ok(id)
}

// --------------------------------------------------------------- settings

/// agy's queuedMessages setting: what Enter does while a turn runs.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum QueueMode {
    /// Enter queues; alt+m sends immediately (agy's default).
    Queue,
    /// Enter sends immediately; alt+m queues.
    SendImmediately,
}
impl QueueMode {
    pub fn as_str(self) -> &'static str {
        match self {
            QueueMode::Queue => "queue",
            QueueMode::SendImmediately => "send-immediately",
        }
    }
    fn parse(v: &Value) -> Option<Self> {
        match v.as_str()? {
            "queue" => Some(QueueMode::Queue),
            "send-immediately" => Some(QueueMode::SendImmediately),
            _ => None,
        }
    }
}
/// The effective queuedMessages of a session: (mode, source). An inline
/// `--settings` JSON on the command line wins over the account's settings.json;
/// unknown (None) when it cannot be determined (a settings file argument, an
/// unreadable settings.json or an unknown value).
pub fn queue_mode(home: &Path, args: Option<&[String]>) -> (Option<QueueMode>, &'static str) {
    if let Some(args) = args {
        let mut it = args.iter().skip(1);
        while let Some(a) = it.next() {
            let value = match a.split_once('=') {
                Some(("--settings" | "-settings", v)) => Some(v.to_owned()),
                None if matches!(a.as_str(), "--settings" | "-settings") => it.next().cloned(),
                _ => None,
            };
            if let Some(v) = value {
                let Ok(parsed) = serde_json::from_str::<Value>(&v) else {
                    return (None, "settings_argument_unreadable");
                };
                if let Some(q) = parsed.get("queuedMessages") {
                    return (QueueMode::parse(q), "settings_argument");
                }
            }
        }
    }
    let path = state_dir(home).join("settings.json");
    let bytes = match fs::symlink_metadata(&path) {
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
            return (Some(QueueMode::Queue), "default");
        }
        Ok(m) if m.file_type().is_file() && m.len() <= 1024 * 1024 => match fs::read(&path) {
            Ok(b) => b,
            Err(_) => return (None, "settings_unreadable"),
        },
        _ => return (None, "settings_unreadable"),
    };
    let Ok(v) = serde_json::from_slice::<Value>(&bytes) else {
        return (None, "settings_unreadable");
    };
    match v.get("queuedMessages") {
        None | Some(Value::Null) => (Some(QueueMode::Queue), "default"),
        Some(q) => (QueueMode::parse(q), "account_settings"),
    }
}
/// The command line of a live process (for its `--settings`).
pub fn process_arguments(pid: u32) -> Option<Vec<String>> {
    arguments(pid)
}

// ----------------------------------------------------------------- accounts

pub fn enrolled(paths: &Paths) -> Result<Vec<Enrolled>> {
    if !paths.root.join(crate::layout::REGISTRY_DB).exists() {
        return Ok(vec![]);
    }
    let state = crate::store::load_state_snapshot(paths)?;
    state
        .accounts
        .values()
        .filter(|a| a.provider == ProviderId::Antigravity)
        .map(|a| {
            Ok(Enrolled {
                name: a.name.clone(),
                home: home(paths, &a.name)?,
                enabled: a.enabled,
                auth_required: a.auth_required,
            })
        })
        .collect()
}
pub fn account(paths: &Paths, name: &str) -> Result<Enrolled> {
    if let Some(a) = enrolled(paths)?.into_iter().find(|a| a.name == name) {
        return Ok(a);
    }
    ensure!(
        !deleted(paths, name)?,
        "ACCOUNT_DELETED: antigravity:{name} was deleted locally; its conversation history is preserved but it cannot be resumed through this account"
    );
    bail!("ACCOUNT_NOT_FOUND: no enrolled Antigravity account with this name")
}
pub fn deleted(paths: &Paths, name: &str) -> Result<bool> {
    let id = format!("antigravity:{name}");
    Ok(crate::account_deletion::list(paths)?
        .iter()
        .any(|r| r.account_id == id && r.phase == "complete"))
}

/// Register discovered conversations and refresh live attachments for every
/// Antigravity account. Returns observations keyed by catalog session id.
pub fn reconcile(paths: &Paths) -> Result<BTreeMap<String, (Observation, PathBuf)>> {
    let accounts = enrolled(paths)?;
    let mut discovered = vec![];
    let snapshot = ProcessSnapshot::take();
    let mut scans = BTreeMap::new();
    for a in &accounts {
        let index = summaries(&a.home).unwrap_or_default();
        let found: Vec<Transcript> = index
            .iter()
            .filter(|s| s.steps > 0)
            .filter_map(|s| transcript_of(&a.home, s))
            .collect();
        for t in &found {
            discovered.push(catalog::Discovered {
                account: a.name.clone(),
                conversation_id: t.session_id.clone(),
                label: t
                    .cwd
                    .as_deref()
                    .and_then(|c| Path::new(c).file_name())
                    .and_then(|n| n.to_str())
                    .unwrap_or("antigravity")
                    .to_owned(),
                directory: t.cwd.clone(),
            });
        }
        let live = live_sessions_in(&snapshot, &a.home).map_err(|e| e.to_string());
        scans.insert(a.name.clone(), (index, live));
    }
    let sessions = catalog::register_discovered(paths, ProviderId::Antigravity, &discovered)?;
    let mut observations = BTreeMap::new();
    let mut attachments = vec![];
    for s in sessions {
        let Some(a) = accounts.iter().find(|a| a.name == s.account) else {
            continue;
        };
        let (index, live) = &scans[&a.name];
        let found = find_transcript(&a.home, &s.conversation_id).map(|db| {
            let row = index.iter().find(|r| r.id == s.conversation_id);
            let bytes = fs::symlink_metadata(transcript_log(&a.home, &s.conversation_id))
                .map(|m| m.len())
                .unwrap_or(0);
            (
                db,
                row.and_then(|r| r.cwd.clone()),
                row.and_then(|r| r.modified.clone()),
                bytes,
            )
        });
        let o = observation(s.attachment.as_ref(), &s.conversation_id, found, live);
        if o.availability != "unknown" {
            let next = if o.availability == "running" {
                o.attachment.clone()
            } else {
                s.attachment.clone().filter(|r| {
                    !o.attachment_switched
                        && !crate::external::attachment_definitely_gone(r.pid, r.process_start)
                })
            };
            attachments.push((s.clone(), next, o.directory.clone()));
        }
        observations.insert(s.id.clone(), (o, a.home.clone()));
    }
    catalog::observe_provider_sessions(paths, attachments)?;
    Ok(observations)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::os::unix::fs::PermissionsExt;

    fn args(v: &[&str]) -> Vec<String> {
        v.iter().map(|s| s.to_string()).collect()
    }

    #[test]
    fn child_environment_isolates_home_and_strips_overrides() {
        let config = Config {
            antigravity_binary: "/opt/agy".into(),
            ..Default::default()
        };
        let c = command(&config, Path::new("/srv/accounts/a"));
        assert_eq!(c.get_program(), "/opt/agy");
        let envs: Vec<_> = c
            .get_envs()
            .map(|(k, v)| {
                (
                    k.to_string_lossy().into_owned(),
                    v.map(|v| v.to_string_lossy().into_owned()),
                )
            })
            .collect();
        for name in STRIPPED_ENV {
            assert!(envs.contains(&(name.to_string(), None)), "{name}");
        }
        for (k, v) in [
            (HOME_ENV, "/srv/accounts/a"),
            ("HOME", "/srv/accounts/a"),
            ("XDG_CACHE_HOME", "/srv/accounts/a/.cache"),
            ("AGY_CLI_DISABLE_AUTO_UPDATE", "1"),
        ] {
            assert!(envs.contains(&(k.into(), Some(v.into()))), "{k} {envs:?}");
        }
    }

    #[test]
    fn auth_is_token_file_presence_only() {
        let d = tempfile::tempdir().unwrap();
        let home = d.path();
        assert_eq!(auth_status(home).problem(), Some("AUTH_REQUIRED"));
        assert!(
            auth_status(home)
                .gate()
                .unwrap_err()
                .to_string()
                .contains("account login antigravity")
        );
        let dir = state_dir(home);
        fs::create_dir_all(&dir).unwrap();
        let token = dir.join(TOKEN_FILE);
        let before = credentials_stamp(home);
        fs::write(&token, "synthetic").unwrap();
        // Only stat'ed: an unreadable token still counts.
        fs::set_permissions(&token, fs::Permissions::from_mode(0o000)).unwrap();
        assert!(auth_status(home).logged_in);
        assert!(login_completed(home, before));
        assert!(!login_completed(home, credentials_stamp(home)));
        assert_eq!(auth_report(home)["auth_method"], "google-oauth");
        fs::set_permissions(&token, fs::Permissions::from_mode(0o600)).unwrap();
        fs::write(&token, "").unwrap();
        assert!(!auth_status(home).logged_in, "empty token file");
    }

    #[test]
    fn command_lines_identify_agy_sessions() {
        let id = "11111111-1111-4111-8111-111111111111";
        let tui = args(&["/opt/antigravity/install/current/agy", "--conversation", id]);
        assert!(is_agy_arguments(&tui) && !is_print(&tui));
        assert_eq!(conversation_argument(&tui).as_deref(), Some(id));
        let eq = args(&["agy", &format!("--conversation={id}"), YOLO_FLAG]);
        assert_eq!(conversation_argument(&eq).as_deref(), Some(id));
        assert!(is_print(&args(&["agy", "-p=", "--conversation", id])));
        assert!(is_print(&args(&["agy", "--print", "x"])));
        assert_eq!(
            conversation_argument(&args(&["agy", "--conversation", "latest"])),
            None
        );
        assert!(!is_agy_arguments(&args(&["vim", "agy"])));
        assert!(is_agy_arguments(&args(&[
            "/bin/sh",
            "/opt/fake/agy",
            "--conversation",
            id
        ])));
        assert!(!is_agy_arguments(&args(&["/bin/sh", "-c", "agy"])));
    }

    #[test]
    fn launch_arguments_open_the_conversation_and_yolo_skips_permissions() {
        let config = Config::default();
        let id = "11111111-1111-4111-8111-111111111111";
        assert_eq!(
            open_argv(&config, id, LaunchPermissions::Yolo),
            ["agy", "--conversation", id, YOLO_FLAG]
        );
        assert_eq!(
            open_argv(&config, id, LaunchPermissions::Default),
            ["agy", "--conversation", id]
        );
        let a = allocate_args();
        assert!(a.contains(&"--print-timeout".to_string()) && a.last().unwrap() == "-p=");
        let out = b"{\"event\":\"init\",\"conversation_id\":\"11111111-1111-4111-8111-111111111111\",\"init\":{}}\n";
        assert_eq!(allocated_id(out).as_deref(), Some(id));
        assert_eq!(
            allocated_id(b"{\"event\":\"init\",\"conversation_id\":\"\"}"),
            None
        );
    }

    #[test]
    fn queue_mode_follows_settings_argument_then_account_settings() {
        let d = tempfile::tempdir().unwrap();
        let home = d.path();
        assert_eq!(queue_mode(home, None), (Some(QueueMode::Queue), "default"));
        fs::create_dir_all(state_dir(home)).unwrap();
        let settings = state_dir(home).join("settings.json");
        fs::write(&settings, r#"{"queuedMessages":"send-immediately"}"#).unwrap();
        assert_eq!(
            queue_mode(home, None),
            (Some(QueueMode::SendImmediately), "account_settings")
        );
        let inline = args(&["agy", "--settings", r#"{"queuedMessages":"queue"}"#]);
        assert_eq!(
            queue_mode(home, Some(&inline)),
            (Some(QueueMode::Queue), "settings_argument")
        );
        let file = args(&["agy", "--settings=/srv/s.json"]);
        assert_eq!(
            queue_mode(home, Some(&file)).0,
            None,
            "a settings file is not read"
        );
        fs::write(&settings, r#"{"queuedMessages":"later"}"#).unwrap();
        assert_eq!(queue_mode(home, None).0, None);
        fs::write(&settings, "{not json").unwrap();
        assert_eq!(queue_mode(home, None), (None, "settings_unreadable"));
    }

    #[test]
    fn the_conversation_index_gives_identity_directory_and_steps() {
        let d = tempfile::tempdir().unwrap();
        let home = d.path();
        let dir = state_dir(home);
        fs::create_dir_all(dir.join("conversations")).unwrap();
        let db = rusqlite::Connection::open(dir.join("conversation_summaries.db")).unwrap();
        db.execute_batch("CREATE TABLE conversation_summaries (conversation_id text, title text NOT NULL DEFAULT '', step_count integer NOT NULL DEFAULT 0, last_modified_time datetime NOT NULL, workspace_uris text NOT NULL, PRIMARY KEY (conversation_id));").unwrap();
        let a = "11111111-1111-4111-8111-111111111111";
        let empty = "22222222-2222-4222-8222-222222222222";
        let gone = "33333333-3333-4333-8333-333333333333";
        for (id, steps, uri) in [
            (a, 4, "file:///srv/my%20work"),
            (empty, 0, "file:///srv/x"),
            (gone, 2, "file:///srv/y"),
            ("not-a-uuid", 2, "file:///srv/z"),
        ] {
            db.execute(
                "INSERT INTO conversation_summaries (conversation_id, step_count, last_modified_time, workspace_uris) VALUES (?1, ?2, '2026-01-01 00:00:00+00:00', ?3)",
                rusqlite::params![id, steps, format!("[\"{uri}\"]")],
            )
            .unwrap();
        }
        drop(db);
        fs::write(conversation_db(home, a), "x").unwrap();
        fs::write(conversation_db(home, empty), "x").unwrap();
        let listed = transcripts(home);
        assert_eq!(listed.len(), 1, "{listed:?}");
        assert_eq!(listed[0].session_id, a);
        assert_eq!(listed[0].cwd.as_deref(), Some("/srv/my work"));
        assert!(
            find_transcript(home, empty).is_some(),
            "an empty conversation still exists"
        );
        assert!(find_transcript(home, gone).is_none());
        assert_eq!(observe(home, a, None).availability, "stopped");
        assert_eq!(
            observe(home, a, None).directory.as_deref(),
            Some("/srv/my work")
        );
        assert_eq!(observe(home, gone, None).availability, "stale");
        assert_eq!(uri_path("file:///a%2Fb"), Some("/a/b".into()));
        assert_eq!(uri_path("file://relative"), None);
        assert_eq!(uri_path("https://x"), None);
    }
}
