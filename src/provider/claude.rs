//! Claude Code provider. Each account is an isolated `CLAUDE_CONFIG_DIR`, either
//! created at `<root>/claude-accounts/<name>` or adopted in place. Sessionsation
//! never reads Claude credentials (`.credentials.json`, `sessions/*.key`); it uses
//! only `claude auth status --json`, session registry metadata, transcript header
//! fields (sessionId/cwd/timestamp) and statusline rate-limit fields.
use super::{Capability, ProviderCapabilities, ProviderId, no, yes};
use crate::{
    catalog,
    config::Config,
    model::State,
    paths::{Paths, atomic_write, private_dir, validate_name},
};
use anyhow::{Context, Result, bail, ensure};
use serde::Serialize;
use serde_json::{Value, json};
pub mod attach;
pub mod launch;

use std::{
    fs,
    io::{BufRead, BufReader, Read, Write},
    path::{Path, PathBuf},
    process::Stdio,
    time::Duration,
};

/// Ambient variables that would silently switch Claude to API-key billing or
/// another backend. Removed from every Claude child (ANTHROPIC_MODEL is kept).
pub const STRIPPED_ENV: &[&str] = &[
    "ANTHROPIC_API_KEY",
    "ANTHROPIC_AUTH_TOKEN",
    "ANTHROPIC_BASE_URL",
    "CLAUDE_CODE_OAUTH_TOKEN",
    "CLAUDE_CODE_USE_BEDROCK",
    "CLAUDE_CODE_USE_VERTEX",
    "CLAUDE_CODE_USE_FOUNDRY",
    "ANTHROPIC_BEDROCK_BASE_URL",
    "ANTHROPIC_VERTEX_PROJECT_ID",
];
pub const USAGE_SOURCE: &str = "claude-code-statusline";
pub const USAGE_SEMANTICS: &str = "used percentage of the Claude subscription 5-hour and 7-day rate-limit windows as last reported to the Claude Code statusline; observed only while Claude Code is active";
/// Statusline observations older than this are stale.
pub const USAGE_STALE_SECONDS: i64 = 1800;
pub const PROBE_TIMEOUT: Duration = Duration::from_secs(10);
const HOOK_MARKER: &str = "provider-hook claude-statusline";
pub const NATIVE_HOME_BOUND: &str = "native session is bound to its account config home";
pub const NO_INPUT_API: &str = "no supported native input API for a live Claude session";
pub const LABEL_ONLY: &str =
    "label applied as --name on next launch/resume; live native title unchanged";
const NO_LIFECYCLE: &str = "no verified Claude process lifecycle control; stop it in its terminal";

pub fn capabilities() -> ProviderCapabilities {
    ProviderCapabilities {
        enrollment: yes(),
        adopt_config_dir: yes(),
        login: yes(),
        auth_status: yes(),
        usage: yes(),
        new: yes(),
        resume: yes(),
        rename: yes(),
        native_rename: no(LABEL_ONLY),
        stop: no(NO_LIFECYCLE),
        restart: no(NO_LIFECYCLE),
        account_switch: no(NATIVE_HOME_BOUND),
        relocate: no(NATIVE_HOME_BOUND),
        move_space: no("HERDR space moves are verified only for Codex"),
        archive: no("Claude transcripts are owned by the account config home"),
        delete_session: no("Claude transcripts are owned by the account config home"),
        submit: no(NO_INPUT_API),
        queue: no(NO_INPUT_API),
        steer: no(NO_INPUT_API),
        account_removal: yes(),
        account_deletion: Capability {
            supported: true,
            reason: Some(
                "managed homes: conversation history preserved, home and credentials deleted; adopted config directories are left in place and only the account record is removed",
            ),
        },
        account_rename: yes(),
        headless_delivery: Capability {
            supported: true,
            reason: Some(
                "api send mode resume (stopped session) and mode new run claude -p headlessly; live sessions are refused",
            ),
        },
        account_limits: yes(),
        cross_provider_conversion: no("conversations are bound to their provider"),
    }
}

/// The managed (non-adopted) home for a Claude account.
pub fn default_home(paths: &Paths, name: &str) -> Result<PathBuf> {
    validate_name(name)?;
    Ok(paths.root.join("claude-accounts").join(name))
}
pub fn home(paths: &Paths, name: &str, config_home: Option<&Path>) -> Result<PathBuf> {
    match config_home {
        Some(home) => Ok(home.to_path_buf()),
        None => default_home(paths, name),
    }
}
/// Create the managed home `<root>/claude-accounts/<name>` (0700), or register
/// one an operator already placed there. An existing home is never wiped,
/// re-initialised or re-permissioned: it must already be a private (0700)
/// directory owned by this user and not a symlink. Returns (home, existed).
pub fn create_home(paths: &Paths, name: &str) -> Result<(PathBuf, bool)> {
    use std::os::unix::fs::MetadataExt;
    private_dir(&paths.root.join("claude-accounts"))?;
    let home = default_home(paths, name)?;
    match fs::symlink_metadata(&home) {
        Ok(m) => {
            ensure!(
                m.file_type().is_dir()
                    && m.uid() == unsafe { libc::geteuid() }
                    && m.mode() & 0o777 == 0o700,
                "UNSAFE_ACCOUNT_HOME: existing {} must be a 0700 directory owned by this user and not a symlink; it was left unchanged",
                home.display()
            );
            Ok((home, true))
        }
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
            private_dir(&home)?;
            Ok((home, false))
        }
        Err(e) => Err(e.into()),
    }
}
/// The managed home must already be an operator-populated private directory.
/// Never created, copied, moved or re-permissioned here.
pub fn validate_managed_dir(home: &Path) -> Result<()> {
    use std::os::unix::fs::MetadataExt;
    let m = match fs::symlink_metadata(home) {
        Ok(m) => m,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => bail!(
            "ACCOUNT_HOME_UNAVAILABLE: managed home {} does not exist; copy the config directory there first",
            home.display()
        ),
        Err(e) => bail!("ACCOUNT_HOME_UNAVAILABLE: managed home is not accessible: {e}"),
    };
    ensure!(
        m.file_type().is_dir()
            && m.uid() == unsafe { libc::geteuid() }
            && m.mode() & 0o777 == 0o700,
        "UNSAFE_ACCOUNT_HOME: {} must be a 0700 directory owned by this user and not a symlink; it was left unchanged",
        home.display()
    );
    Ok(())
}
/// Any other process (readable environ) whose CLAUDE_CONFIG_DIR is one of `homes`.
/// A Claude Code process: its executable is a native Claude build (`.../claude/versions/<v>`
/// or a file named `claude`), or it was started as the configured Claude command. Other
/// processes that merely inherited CLAUDE_CONFIG_DIR do not use the config home.
fn is_claude_process(pid: u32, claude_binary: &str) -> bool {
    let exe = fs::read_link(format!("/proc/{pid}/exe")).ok();
    if exe.as_deref().is_some_and(|e| {
        e.file_name().is_some_and(|n| n == "claude")
            || e.components().any(|c| c.as_os_str() == "claude")
                && e.components().any(|c| c.as_os_str() == "versions")
    }) {
        return true;
    }
    let Ok(cmdline) = fs::read(format!("/proc/{pid}/cmdline")) else {
        return false;
    };
    let configured = Path::new(claude_binary);
    cmdline
        .split(|b| *b == 0)
        .take(2)
        .filter_map(|a| std::str::from_utf8(a).ok())
        .any(|a| {
            Path::new(a) == configured
                || Path::new(a).is_absolute()
                    && fs::canonicalize(a)
                        .is_ok_and(|c| fs::canonicalize(configured).is_ok_and(|d| c == d))
        })
}
pub(crate) fn env_in_use(homes: &[&Path], claude_binary: &str) -> bool {
    let canon: Vec<_> = homes
        .iter()
        .filter_map(|h| fs::canonicalize(h).ok())
        .collect();
    let Ok(entries) = fs::read_dir("/proc") else {
        return false;
    };
    let me = std::process::id();
    entries.flatten().any(|entry| {
        let Some(pid) = entry
            .file_name()
            .to_str()
            .and_then(|n| n.parse::<u32>().ok())
        else {
            return false;
        };
        if pid == me || !is_claude_process(pid, claude_binary) {
            return false;
        }
        let Some(env) = environ(pid) else {
            return false;
        };
        env.iter()
            .find(|(k, _)| k == "CLAUDE_CONFIG_DIR")
            .is_some_and(|(_, v)| {
                let v = PathBuf::from(v);
                homes.iter().any(|h| **h == *v)
                    || fs::canonicalize(&v).is_ok_and(|c| canon.contains(&c))
            })
    })
}
/// Preconditions for switching an adopted Claude account to its managed home.
/// Returns the previous adopted home, or None when already managed. Read-only;
/// the supervisor persists the change. Fails closed before any state change.
pub async fn check_managed_home(
    paths: &Paths,
    config: &Config,
    name: &str,
    adopted: Option<&Path>,
) -> Result<Option<PathBuf>> {
    let managed = default_home(paths, name)?;
    validate_managed_dir(&managed)?;
    let Some(old) = adopted else {
        return Ok(None);
    };
    ensure!(
        fs::metadata(old).is_ok_and(|m| m.is_dir()),
        "ACCOUNT_HOME_UNAVAILABLE: adopted home {} is not an accessible directory",
        old.display()
    );
    ensure!(
        fs::canonicalize(old)? != fs::canonicalize(&managed)?,
        "UNSAFE_ACCOUNT_HOME: adopted and managed homes are the same directory"
    );
    for home in [old, managed.as_path()] {
        let live = live_sessions(home)
            .map_err(|e| anyhow::anyhow!("ACCOUNT_IN_USE: cannot verify live sessions: {e}"))?;
        ensure!(
            live.is_empty(),
            "ACCOUNT_IN_USE: a live Claude session uses {}; stop it first",
            home.display()
        );
    }
    ensure!(
        !env_in_use(&[old, managed.as_path()], &config.claude_binary),
        "ACCOUNT_IN_USE: a running Claude process uses CLAUDE_CONFIG_DIR for the old or managed home; stop it first"
    );
    let new_auth = auth_status(config, &managed).await?;
    new_auth.gate()?;
    let old_auth = auth_status(config, old).await?;
    ensure!(
        old_auth.identity == new_auth.identity,
        "AUTH_IDENTITY_MISMATCH: the managed home is not logged in as the same Claude identity as the adopted home; nothing changed"
    );
    Ok(Some(old.to_path_buf()))
}
/// Non-identifying auth summary for enrollment output (never email/org).
pub async fn auth_report(config: &Config, home: &Path) -> Value {
    match auth_status(config, home).await {
        Ok(s) => {
            json!({"logged_in":s.logged_in,"auth_method":s.auth_method,"api_provider":s.api_provider,
            "subscription_type":s.subscription_type,"problem":s.problem()})
        }
        Err(e) => {
            json!({"problem":e.to_string().split(':').next().unwrap_or("AUTH_STATUS_UNAVAILABLE")})
        }
    }
}

/// Validate an existing CLAUDE_CONFIG_DIR for in-place adoption. It is never
/// copied or moved; it must not overlap sessionsation-managed homes or another
/// enrolled account's home.
pub fn validate_adopt(paths: &Paths, state: &State, dir: &Path) -> Result<PathBuf> {
    ensure!(
        dir.is_absolute(),
        "INVALID_CONFIG_DIR: --config-dir must be an absolute path"
    );
    let metadata = fs::metadata(dir).context("INVALID_CONFIG_DIR: --config-dir does not exist")?;
    ensure!(
        metadata.is_dir(),
        "INVALID_CONFIG_DIR: --config-dir is not a directory"
    );
    let dir = fs::canonicalize(dir)?;
    ensure!(
        !dir.starts_with(&paths.root) && !paths.root.starts_with(&dir),
        "INVALID_CONFIG_DIR: --config-dir overlaps the sessionsation data directory (including its accounts)"
    );
    for account in state.accounts.values() {
        let Ok(other) = super::account_home(paths, account) else {
            continue;
        };
        let other = fs::canonicalize(&other).unwrap_or(other);
        ensure!(
            !dir.starts_with(&other) && !other.starts_with(&dir),
            "CONFIG_DIR_IN_USE: --config-dir overlaps the home of {}:{}",
            account.provider,
            account.name
        );
    }
    Ok(dir)
}

/// The single builder for every Claude child process: binary, isolated config
/// home, stripped billing/backend overrides, no autoupdate, optional HOME.
pub fn command(config: &Config, home: &Path) -> std::process::Command {
    let mut command = std::process::Command::new(&config.claude_binary);
    for name in STRIPPED_ENV {
        command.env_remove(name);
    }
    command
        .env("CLAUDE_CONFIG_DIR", home)
        .env("DISABLE_AUTOUPDATER", "1");
    if let Some(session_home) = &config.claude_session_home {
        command.env("HOME", session_home);
    }
    command
}

#[derive(Clone, Debug, PartialEq, Serialize)]
pub struct AuthStatus {
    pub logged_in: bool,
    pub auth_method: Option<String>,
    pub api_provider: Option<String>,
    pub subscription_type: Option<String>,
    /// sha256 over (loggedIn, authMethod, apiProvider, orgId, email); the raw
    /// identity fields are never retained, logged or serialized.
    #[serde(skip)]
    pub identity: String,
}
impl AuthStatus {
    /// Reads only non-identifying fields; email/org are never retained.
    pub fn parse(bytes: &[u8]) -> Result<Self> {
        let v: Value = serde_json::from_slice(bytes).context("AUTH_STATUS_UNAVAILABLE")?;
        ensure!(v.is_object(), "AUTH_STATUS_UNAVAILABLE");
        let text = |k: &str| v[k].as_str().map(str::to_owned);
        use sha2::{Digest, Sha256};
        let mut hasher = Sha256::new();
        hasher.update(v["loggedIn"].as_bool().unwrap_or(false).to_string());
        for key in ["authMethod", "apiProvider", "orgId", "email"] {
            hasher.update([0]);
            hasher.update(v[key].as_str().unwrap_or("").as_bytes());
        }
        Ok(Self {
            identity: format!("{:x}", hasher.finalize()),
            logged_in: v["loggedIn"].as_bool().unwrap_or(false),
            auth_method: text("authMethod"),
            api_provider: text("apiProvider"),
            subscription_type: text("subscriptionType"),
        })
    }
    /// None when a subscription login on the first-party API is confirmed.
    pub fn problem(&self) -> Option<&'static str> {
        if !self.logged_in {
            Some("AUTH_REQUIRED")
        } else if self.auth_method.as_deref() != Some("claude.ai")
            || self.api_provider.as_deref() != Some("firstParty")
        {
            Some("WRONG_AUTH_MODE")
        } else {
            None
        }
    }
    pub fn gate(&self) -> Result<()> {
        match self.problem() {
            None => Ok(()),
            Some("AUTH_REQUIRED") => bail!(
                "AUTH_REQUIRED: this Claude account is not logged in; run `sessionsation account login claude:<name>`"
            ),
            Some(_) => bail!(
                "WRONG_AUTH_MODE: this Claude account must use a claude.ai subscription login on the first-party API (found auth method {}, provider {})",
                self.auth_method.as_deref().unwrap_or("none"),
                self.api_provider.as_deref().unwrap_or("none")
            ),
        }
    }
}

/// Bounded `claude auth status --json` with the account environment.
pub async fn auth_status(config: &Config, home: &Path) -> Result<AuthStatus> {
    let mut command = tokio::process::Command::from(self::command(config, home));
    command
        .args(["auth", "status", "--json"])
        .current_dir(home)
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        .kill_on_drop(true);
    let output = tokio::time::timeout(PROBE_TIMEOUT, command.output())
        .await
        .context("AUTH_STATUS_TIMEOUT: claude auth status did not answer")?
        .context("AUTH_STATUS_UNAVAILABLE: claude binary could not start")?;
    ensure!(output.stdout.len() <= 64 * 1024, "AUTH_STATUS_UNAVAILABLE");
    AuthStatus::parse(&output.stdout)
}
pub async fn prelaunch_gate(config: &Config, home: &Path) -> Result<()> {
    auth_status(config, home).await?.gate()
}

/// Settings merged at launch: route the statusline through the usage recorder,
/// which then chains to the account's own statusline command.
pub fn launch_settings(executable: &Path) -> String {
    json!({"statusLine":{"type":"command","command":format!(
        "{} {HOOK_MARKER}",
        crate::terminal::shell_quote(&executable.to_string_lossy())
    )}})
    .to_string()
}
pub fn new_argv(config: &Config, session_id: &str, name: &str, settings: &str) -> Vec<String> {
    [
        config.claude_binary.as_str(),
        "--session-id",
        session_id,
        "--name",
        name,
        "--settings",
        settings,
    ]
    .map(str::to_owned)
    .to_vec()
}
pub fn resume_argv(config: &Config, session_id: &str, name: &str, settings: &str) -> Vec<String> {
    [
        config.claude_binary.as_str(),
        "--resume",
        session_id,
        "--name",
        name,
        "--settings",
        settings,
    ]
    .map(str::to_owned)
    .to_vec()
}
pub fn login_argv(config: &Config) -> Vec<String> {
    vec![
        config.claude_binary.clone(),
        "auth".into(),
        "login".into(),
        "--claudeai".into(),
    ]
}

// ---------------------------------------------------------------- discovery

#[derive(Clone, Debug, Serialize)]
pub struct Transcript {
    pub session_id: String,
    pub cwd: Option<String>,
    pub started_at: Option<String>,
    pub path: PathBuf,
    pub bytes: u64,
}
pub(crate) fn regular_file(path: &Path) -> bool {
    fs::symlink_metadata(path).is_ok_and(|m| m.file_type().is_file())
}
fn project_dirs(home: &Path) -> Vec<PathBuf> {
    let Ok(entries) = fs::read_dir(home.join("projects")) else {
        return vec![];
    };
    let mut dirs: Vec<_> = entries
        .flatten()
        .filter(|e| e.file_type().is_ok_and(|t| t.is_dir()))
        .map(|e| e.path())
        .collect();
    dirs.sort();
    dirs
}
/// Read only metadata fields from the first lines; message text is never kept.
fn transcript_metadata(path: &Path) -> (Option<String>, Option<String>, Option<String>) {
    let (mut id, mut cwd, mut at) = (None, None, None);
    let Ok(file) = fs::File::open(path) else {
        return (id, cwd, at);
    };
    let mut reader = BufReader::new(file.take(1024 * 1024));
    let mut line = String::new();
    for _ in 0..64 {
        line.clear();
        if reader.read_line(&mut line).unwrap_or(0) == 0 {
            break;
        }
        let Ok(v) = serde_json::from_str::<Value>(&line) else {
            continue;
        };
        let field = |k: &str| v[k].as_str().filter(|s| !s.is_empty()).map(str::to_owned);
        id = id.or_else(|| field("sessionId"));
        cwd = cwd.or_else(|| field("cwd"));
        at = at.or_else(|| field("timestamp"));
        if id.is_some() && cwd.is_some() && at.is_some() {
            break;
        }
    }
    (id, cwd, at)
}
/// `forkedFrom.sessionId` on the leading records: Claude copies a branched
/// conversation into a new session and stamps each copied record with it.
fn forked_from(path: &Path) -> Option<String> {
    let stem = path.file_stem()?.to_str()?;
    let file = fs::File::open(path).ok()?;
    let mut reader = BufReader::new(file.take(1024 * 1024));
    let mut line = String::new();
    for _ in 0..64 {
        line.clear();
        if reader.read_line(&mut line).unwrap_or(0) == 0 {
            break;
        }
        #[derive(serde::Deserialize)]
        struct Fork {
            #[serde(rename = "forkedFrom")]
            forked_from: Option<ForkSource>,
        }
        #[derive(serde::Deserialize)]
        struct ForkSource {
            #[serde(rename = "sessionId")]
            session_id: Option<String>,
        }
        if let Ok(Fork {
            forked_from: Some(ForkSource {
                session_id: Some(id),
            }),
        }) = serde_json::from_str::<Fork>(&line)
            && id != stem
            && uuid::Uuid::parse_str(&id).is_ok()
        {
            return Some(id);
        }
    }
    None
}
/// Bounded tail read for the continuation marker.
const CONTINUATION_TAIL: u64 = 1024 * 1024;
/// The session this transcript continued in, from Claude Code's own
/// `{"type":"continued-in","sessionId":OLD,"continuedInSessionId":NEW}` record
/// (written when a conversation is sent to the background daemon under a new
/// session id). Only the record `type` and these ids are read. The marker counts
/// only while no user/assistant record follows it, i.e. the old session was not
/// continued again after the hand-off.
pub fn continued_in(path: &Path) -> Option<String> {
    use std::io::{Seek, SeekFrom};
    let stem = path.file_stem()?.to_str()?;
    let mut file = fs::File::open(path).ok()?;
    let len = file.metadata().ok()?.len();
    let start = len.saturating_sub(CONTINUATION_TAIL);
    file.seek(SeekFrom::Start(start)).ok()?;
    let mut bytes = vec![];
    file.take(CONTINUATION_TAIL).read_to_end(&mut bytes).ok()?;
    let mut lines: Vec<&[u8]> = bytes.split(|b| *b == b'\n').collect();
    if start > 0 && !lines.is_empty() {
        lines.remove(0);
    }
    #[derive(serde::Deserialize)]
    struct Record {
        #[serde(rename = "type")]
        kind: Option<String>,
        #[serde(rename = "sessionId")]
        session_id: Option<String>,
        #[serde(rename = "continuedInSessionId")]
        continued_in: Option<String>,
    }
    for line in lines.into_iter().rev() {
        if line.iter().all(u8::is_ascii_whitespace) {
            continue;
        }
        let Ok(r) = serde_json::from_slice::<Record>(line) else {
            continue;
        };
        match r.kind.as_deref() {
            Some("user" | "assistant") => return None,
            Some("continued-in") => {
                let next = r.continued_in?;
                return (r.session_id.as_deref() == Some(stem)
                    && next != stem
                    && uuid::Uuid::parse_str(&next).is_ok())
                .then_some(next);
            }
            _ => {}
        }
    }
    None
}
/// Transcripts `<home>/projects/*/<uuid>.jsonl` with metadata only.
pub fn transcripts(home: &Path) -> Vec<Transcript> {
    let mut result = vec![];
    for dir in project_dirs(home) {
        let Ok(entries) = fs::read_dir(&dir) else {
            continue;
        };
        for entry in entries.flatten() {
            let path = entry.path();
            let Some(stem) = path
                .file_name()
                .and_then(|n| n.to_str())
                .and_then(|n| n.strip_suffix(".jsonl"))
            else {
                continue;
            };
            if uuid::Uuid::parse_str(stem).is_err() || !regular_file(&path) {
                continue;
            }
            let (id, cwd, started_at) = transcript_metadata(&path);
            if id.as_deref().is_some_and(|id| id != stem) {
                continue;
            }
            result.push(Transcript {
                session_id: stem.into(),
                cwd,
                started_at,
                bytes: fs::symlink_metadata(&path).map(|m| m.len()).unwrap_or(0),
                path,
            });
        }
    }
    result.sort_by(|a, b| a.session_id.cmp(&b.session_id));
    result
}
pub fn find_transcript(home: &Path, session_id: &str) -> Option<PathBuf> {
    uuid::Uuid::parse_str(session_id).ok()?;
    project_dirs(home)
        .into_iter()
        .map(|d| d.join(format!("{session_id}.jsonl")))
        .find(|p| regular_file(p))
}

// ------------------------------------------------------------ live identity

#[derive(Clone, Debug, PartialEq, Serialize)]
pub struct Live {
    pub pid: u32,
    pub process_start: u64,
    pub session_id: String,
    pub cwd: Option<String>,
    pub status: Option<String>,
    pub name: Option<String>,
    /// Registry `kind` (`interactive`, `bg` for background-daemon sessions, ...).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub kind: Option<String>,
    /// Registry `jobId` of a background session: the handle `claude attach` takes.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub job_id: Option<String>,
}
impl Live {
    /// A background-daemon session that `claude attach <job>` can open.
    pub fn attachable(&self) -> bool {
        self.kind.as_deref() == Some("bg") && self.job_id.is_some()
    }
}
/// (state, starttime) from /proc/<pid>/stat field 22.
pub fn proc_start(pid: u32) -> Option<(char, u64)> {
    let stat = fs::read_to_string(format!("/proc/{pid}/stat")).ok()?;
    let fields: Vec<_> = stat.rsplit_once(") ")?.1.split_whitespace().collect();
    Some((
        fields.first()?.chars().next()?,
        fields.get(19)?.parse().ok()?,
    ))
}
pub(crate) fn environ(pid: u32) -> Option<Vec<(String, String)>> {
    let bytes = fs::read(format!("/proc/{pid}/environ")).ok()?;
    Some(
        bytes
            .split(|b| *b == 0)
            .filter_map(|kv| {
                let kv = std::str::from_utf8(kv).ok()?;
                let (k, v) = kv.split_once('=')?;
                Some((k.to_owned(), v.to_owned()))
            })
            .collect(),
    )
}
/// When readable, the process's effective config dir must be this account home.
fn environ_matches(pid: u32, home: &Path) -> bool {
    let Some(env) = environ(pid) else {
        return true;
    };
    let get = |k: &str| env.iter().find(|(n, _)| n == k).map(|(_, v)| v.clone());
    let effective = get("CLAUDE_CONFIG_DIR")
        .map(PathBuf::from)
        .or_else(|| get("HOME").map(|h| PathBuf::from(h).join(".claude")));
    let canonical = |p: &Path| fs::canonicalize(p).ok();
    effective.is_some_and(|e| canonical(&e).is_some() && canonical(&e) == canonical(home))
}
/// Verified live sessions of one home. A registry entry counts only when its
/// PID exists with the recorded start time (PID-reuse guard) and, when
/// readable, the process uses this config home. `*.key` files are never read.
pub fn live_sessions(home: &Path) -> Result<Vec<Live>> {
    let dir = home.join("sessions");
    let entries = match fs::read_dir(&dir) {
        Ok(entries) => entries,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(vec![]),
        Err(e) => return Err(e.into()),
    };
    let mut result = vec![];
    for entry in entries {
        let Ok(entry) = entry else {
            continue;
        };
        let name = entry.file_name();
        let Some(pid) = name
            .to_str()
            .and_then(|n| n.strip_suffix(".json"))
            .and_then(|n| n.parse::<u32>().ok())
        else {
            continue;
        };
        // A registry file left by a killed process (or removed while listing)
        // never makes the whole probe fail; an unreadable entry of a still-live
        // process does, because that process may hold the session.
        if !entry.file_type().is_ok_and(|t| t.is_file()) {
            continue;
        }
        let mut bytes = vec![];
        let read =
            fs::File::open(entry.path()).and_then(|f| f.take(64 * 1024).read_to_end(&mut bytes));
        match read {
            Ok(_) => {}
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => continue,
            Err(_) if proc_start(pid).is_none_or(|(s, _)| matches!(s, 'Z' | 'X')) => continue,
            Err(e) => return Err(e.into()),
        }
        let Ok(v) = serde_json::from_slice::<Value>(&bytes) else {
            continue;
        };
        if let Some(live) = verify_entry(pid, &v, home) {
            result.push(live);
        }
    }
    result.sort_by_key(|l| l.pid);
    Ok(result)
}
fn verify_entry(pid: u32, v: &Value, home: &Path) -> Option<Live> {
    if v["pid"].as_u64() != Some(pid as u64) {
        return None;
    }
    let recorded = v["procStart"]
        .as_str()
        .and_then(|s| s.trim().parse::<u64>().ok())
        .or_else(|| v["procStart"].as_u64())?;
    let session_id = v["sessionId"].as_str()?.to_owned();
    let (state, start) = proc_start(pid)?;
    if start != recorded || matches!(state, 'Z' | 'X') || !environ_matches(pid, home) {
        return None;
    }
    let text = |k: &str| v[k].as_str().map(str::to_owned);
    Some(Live {
        pid,
        process_start: start,
        session_id,
        cwd: text("cwd"),
        status: text("status").filter(|s| matches!(s.as_str(), "busy" | "idle")),
        name: text("name"),
        kind: text("kind").filter(|k| k.len() <= 32),
        job_id: text("jobId").filter(|j| {
            !j.is_empty()
                && j.len() <= 64
                && j.bytes().all(|b| b.is_ascii_alphanumeric() || b == b'-')
        }),
    })
}

/// Map a verified PID to a host. A launch-recorded attachment with the same
/// PID/start is exact; otherwise ask HERDR whether this PID is the foreground
/// process of the pane named in its environment. Never infer from titles.
pub fn host_attachment(
    live: &Live,
    recorded: Option<&catalog::Attachment>,
) -> Option<catalog::Attachment> {
    if let Some(a) = recorded
        && a.pid == live.pid
        && a.process_start == live.process_start
    {
        return Some(a.clone());
    }
    let env = environ(live.pid)?;
    let get = |k: &str| env.iter().find(|(n, _)| n == k).map(|(_, v)| v.clone());
    let pane = get("HERDR_PANE_ID")?;
    let session = crate::terminal::herdr_session_from(get)?;
    let socket = get("HERDR_SOCKET_PATH").map(PathBuf::from);
    let target = crate::terminal::herdr_target(&session, &pane).ok()?;
    let (idle, pid) = crate::terminal::state_at(&target, socket.as_deref()).ok()?;
    (!idle && pid == live.pid).then(|| catalog::Attachment {
        host: "herdr".into(),
        target,
        pid: live.pid,
        process_start: live.process_start,
    })
}

#[derive(Clone, Debug, Serialize)]
pub struct Observation {
    /// running | stopped | stale | unknown
    pub availability: &'static str,
    pub native_status: Option<String>,
    pub native_name: Option<String>,
    pub pid: Option<u32>,
    pub process_start: Option<u64>,
    pub directory: Option<String>,
    pub transcript: Option<PathBuf>,
    pub transcript_bytes: Option<u64>,
    pub started_at: Option<String>,
    pub attachment: Option<catalog::Attachment>,
    pub detail: Option<String>,
    /// Registry kind of the live process (`bg` = background-daemon session).
    pub native_kind: Option<String>,
    /// Background job handle accepted by `claude attach`.
    pub job_id: Option<String>,
    /// The recorded attachment's exact process now runs another native session
    /// (conversation switched in the same terminal); it no longer holds this one.
    pub attachment_switched: bool,
    /// `forkedFrom.sessionId` of the copied leading records (a branch/fork; the
    /// parent conversation remains separate).
    pub forked_from: Option<String>,
}
/// Live registry entries of one home, probed once and shared by observations.
pub type LiveProbe = std::result::Result<Vec<Live>, String>;
pub fn probe(home: &Path) -> LiveProbe {
    live_sessions(home).map_err(|e| e.to_string())
}
pub fn observe(
    home: &Path,
    session_id: &str,
    recorded: Option<&catalog::Attachment>,
) -> Observation {
    observe_in(home, session_id, recorded, &probe(home))
}
pub fn observe_in(
    home: &Path,
    session_id: &str,
    recorded: Option<&catalog::Attachment>,
    live: &LiveProbe,
) -> Observation {
    let transcript = find_transcript(home, session_id);
    let (_, cwd, started_at) = transcript
        .as_deref()
        .map(transcript_metadata)
        .unwrap_or_default();
    let mut o = Observation {
        availability: "unknown",
        native_status: None,
        native_name: None,
        pid: None,
        process_start: None,
        directory: cwd,
        transcript_bytes: transcript
            .as_deref()
            .and_then(|p| fs::symlink_metadata(p).ok())
            .map(|m| m.len()),
        forked_from: transcript.as_deref().and_then(forked_from),
        transcript,
        started_at,
        attachment: None,
        detail: None,
        native_kind: None,
        job_id: None,
        attachment_switched: false,
    };
    match live {
        Err(e) => o.detail = Some(format!("PROBE_FAILED: {e}")),
        Ok(live) => {
            o.attachment_switched = recorded.is_some_and(|a| {
                live.iter().any(|l| {
                    l.pid == a.pid
                        && l.process_start == a.process_start
                        && l.session_id != session_id
                })
            });
            match live.iter().find(|l| l.session_id == session_id) {
                Some(l) => {
                    o.availability = "running";
                    o.attachment = match recorded {
                        Some(a) if attach_client_observed(a, l) => Some(a.clone()),
                        _ => host_attachment(l, recorded),
                    };
                    o.native_status = l.status.clone();
                    o.native_name = l.name.clone();
                    o.native_kind = l.kind.clone();
                    o.job_id = l.job_id.clone();
                    o.pid = Some(l.pid);
                    o.process_start = Some(l.process_start);
                    if l.cwd.is_some() {
                        o.directory = l.cwd.clone();
                    }
                }
                None if o.transcript.is_some() => o.availability = "stopped",
                None => o.availability = "stale",
            }
        }
    }
    o
}
fn cmdline(pid: u32) -> Vec<String> {
    fs::read(format!("/proc/{pid}/cmdline"))
        .map(|c| {
            c.split(|b| *b == 0)
                .map(|a| String::from_utf8_lossy(a).into_owned())
                .collect()
        })
        .unwrap_or_default()
}
/// A recorded attachment that is a `claude attach` client of this background
/// session (or sessionsation's own `claude-attach` step just before it execs
/// that client). Exact PID/start identity; the client is not the agent process.
pub fn attach_client_observed(a: &catalog::Attachment, live: &Live) -> bool {
    if !live.attachable()
        || proc_start(a.pid)
            .is_none_or(|(state, start)| start != a.process_start || matches!(state, 'Z' | 'X'))
    {
        return false;
    }
    let args = cmdline(a.pid);
    let job = live.job_id.as_deref().unwrap_or_default();
    args.windows(2)
        .any(|w| w[0] == "attach" && (w[1] == job || w[1] == live.session_id))
        || args
            .windows(2)
            .any(|w| w[0] == "claude-attach" && args.iter().any(|x| x == "--expected-generation"))
}
/// True when the exact process recorded for a new/resumed session runs Claude
/// for that native session (argv check, PID-reuse guarded).
pub fn launch_observed(session: &catalog::Session) -> bool {
    let Some(a) = &session.attachment else {
        return false;
    };
    if proc_start(a.pid)
        .is_none_or(|(state, start)| start != a.process_start || matches!(state, 'Z' | 'X'))
    {
        return false;
    }
    let Ok(cmdline) = fs::read(format!("/proc/{}/cmdline", a.pid)) else {
        return false;
    };
    let args: Vec<_> = cmdline
        .split(|b| *b == 0)
        .map(|a| String::from_utf8_lossy(a).into_owned())
        .collect();
    args.windows(2).any(|w| {
        matches!(w[0].as_str(), "--session-id" | "--resume") && w[1] == session.conversation_id
    })
}

// ----------------------------------------------------------------- accounts

#[derive(Clone, Debug)]
pub struct Enrolled {
    pub name: String,
    pub home: PathBuf,
    pub enabled: bool,
    pub auth_required: bool,
}
/// Claude accounts from a read-only registry snapshot (daemon need not run).
pub fn enrolled(paths: &Paths) -> Result<Vec<Enrolled>> {
    if !paths.root.join("registry.db").exists() {
        return Ok(vec![]);
    }
    let state = crate::store::load_state_snapshot(paths)?;
    state
        .accounts
        .values()
        .filter(|a| a.provider == ProviderId::Claude)
        .map(|a| {
            Ok(Enrolled {
                name: a.name.clone(),
                home: home(paths, &a.name, a.config_home.as_deref())?,
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
        "ACCOUNT_DELETED: claude:{name} was deleted locally; its conversation history is preserved but it cannot be resumed through this account"
    );
    bail!("ACCOUNT_NOT_FOUND: no enrolled Claude account with this name")
}
/// True when a completed local deletion removed this (no longer enrolled) Claude account.
pub fn deleted(paths: &Paths, name: &str) -> Result<bool> {
    let id = format!("claude:{name}");
    Ok(crate::account_deletion::list(paths)?
        .iter()
        .any(|r| r.account_id == id && r.phase == "complete"))
}

/// Continuation markers keyed by transcript path and (size, mtime), so the
/// bounded tail of an unchanged transcript is read once, not on every poll.
struct LineageCache {
    file: PathBuf,
    entries: std::collections::BTreeMap<String, (u64, i128, Option<String>)>,
    changed: bool,
}
impl LineageCache {
    fn load(paths: &Paths) -> Self {
        let file = paths.root.join("session-catalog/claude-lineage.json");
        let entries = fs::read(&file)
            .ok()
            .and_then(|b| serde_json::from_slice(&b).ok())
            .unwrap_or_default();
        Self {
            file,
            entries,
            changed: false,
        }
    }
    fn continued_in(&mut self, path: &Path) -> Option<String> {
        let meta = fs::symlink_metadata(path).ok()?;
        let modified = meta
            .modified()
            .ok()
            .and_then(|m| m.duration_since(std::time::UNIX_EPOCH).ok())
            .map_or(0, |d| d.as_nanos() as i128);
        let key = path.to_string_lossy().into_owned();
        if let Some((len, at, next)) = self.entries.get(&key)
            && *len == meta.len()
            && *at == modified
        {
            return next.clone();
        }
        let next = continued_in(path);
        self.entries
            .insert(key, (meta.len(), modified, next.clone()));
        self.changed = true;
        next
    }
    fn save(self, live: &std::collections::BTreeSet<String>) {
        if !self.changed && self.entries.keys().all(|k| live.contains(k)) {
            return;
        }
        let entries: std::collections::BTreeMap<_, _> = self
            .entries
            .into_iter()
            .filter(|(k, _)| live.contains(k))
            .collect();
        if let Some(dir) = self.file.parent()
            && private_dir(dir).is_ok()
            && let Ok(bytes) = serde_json::to_vec(&entries)
        {
            let _ = atomic_write(&self.file, &bytes);
        }
    }
}

/// Register discovered transcripts, link verified continuations and refresh live
/// attachments for every Claude account. Returns observations keyed by catalog
/// session id.
///
/// Identity rules: a live process is attached to the one catalog entry whose
/// conversation its registry entry (`sessions/<pid>.json` `sessionId`) currently
/// names. When the same process switched conversations in its terminal, the
/// entry it left is released to stopped and the entry of the new conversation
/// takes over the exact attachment, so no process is ever listed twice.
pub fn reconcile(
    paths: &Paths,
) -> Result<std::collections::BTreeMap<String, (Observation, PathBuf)>> {
    let accounts = enrolled(paths)?;
    let mut discovered = vec![];
    let mut links = vec![];
    let mut cache = LineageCache::load(paths);
    let mut seen = std::collections::BTreeSet::new();
    for a in &accounts {
        let found = transcripts(&a.home);
        let ids: std::collections::BTreeSet<_> =
            found.iter().map(|t| t.session_id.clone()).collect();
        for t in found {
            seen.insert(t.path.to_string_lossy().into_owned());
            if let Some(next) = cache.continued_in(&t.path)
                && ids.contains(&next)
            {
                // Continued under a new id in this same account home: the old
                // conversation is never registered as a separate entry.
                links.push(catalog::Continuation {
                    account: a.name.clone(),
                    from: t.session_id,
                    to: next,
                });
                continue;
            }
            discovered.push(catalog::Discovered {
                account: a.name.clone(),
                conversation_id: t.session_id,
                label: t
                    .cwd
                    .as_deref()
                    .and_then(|c| Path::new(c).file_name())
                    .and_then(|n| n.to_str())
                    .unwrap_or("claude")
                    .to_owned(),
                directory: t.cwd,
            });
        }
    }
    cache.save(&seen);
    catalog::link_continuations(paths, ProviderId::Claude, &links)?;
    let sessions = catalog::register_discovered(paths, ProviderId::Claude, &discovered)?;
    let probes: std::collections::BTreeMap<_, _> = accounts
        .iter()
        .map(|a| (a.name.clone(), probe(&a.home)))
        .collect();
    let mut observations = std::collections::BTreeMap::new();
    let mut attachments = vec![];
    for s in &sessions {
        let Some(a) = accounts.iter().find(|a| a.name == s.account) else {
            continue;
        };
        let live = &probes[&a.name];
        // The exact attachment of the process now running this conversation,
        // even when it was recorded on the entry that process switched away from.
        let current = live
            .as_ref()
            .ok()
            .and_then(|l| l.iter().find(|l| l.session_id == s.conversation_id));
        let recorded = match current {
            Some(l)
                if s.attachment
                    .as_ref()
                    .is_none_or(|r| r.pid != l.pid || r.process_start != l.process_start)
                    && !s
                        .attachment
                        .as_ref()
                        .is_some_and(|r| attach_client_observed(r, l)) =>
            {
                sessions
                    .iter()
                    .filter_map(|o| o.attachment.as_ref())
                    .find(|r| r.pid == l.pid && r.process_start == l.process_start)
                    .or(s.attachment.as_ref())
            }
            _ => s.attachment.as_ref(),
        };
        let o = observe_in(&a.home, &s.conversation_id, recorded, live);
        if o.availability != "unknown" {
            let next = if o.availability == "running" {
                o.attachment.clone()
            } else if o.attachment_switched {
                None
            } else {
                s.attachment.clone().filter(|r| {
                    !crate::external::attachment_definitely_gone(r.pid, r.process_start)
                })
            };
            attachments.push((s.clone(), next, o.directory.clone()));
        }
        observations.insert(s.id.clone(), (o, a.home.clone()));
    }
    catalog::observe_provider_sessions(paths, attachments)?;
    Ok(observations)
}

// -------------------------------------------------------------------- usage

pub fn usage_file(home: &Path) -> PathBuf {
    home.join(".sessionsation/usage.json")
}
fn statusline_window(v: &Value) -> Value {
    let used = v["used_percentage"]
        .as_f64()
        .filter(|p| p.is_finite() && (0. ..=100.).contains(p));
    let resets = v["resets_at"]
        .as_i64()
        .or_else(|| v["resets_at"].as_f64().map(|f| f as i64))
        .filter(|r| *r > 0);
    match (used, resets) {
        (Some(used), Some(resets)) => json!({"used_percentage":used,"resets_at":resets}),
        _ => Value::Null,
    }
}
/// Persist only rate-limit fields from one statusline payload. Returns false
/// when the payload carries no rate-limit information (nothing observed).
pub fn record_statusline(home: &Path, input: &Value, now: i64) -> Result<bool> {
    let limits = &input["rate_limits"];
    if !limits.is_object() {
        return Ok(false);
    }
    let record = json!({
        "source": USAGE_SOURCE,
        "observed_at": now,
        "claude_version": input["version"].as_str(),
        "session_id": input["session_id"].as_str(),
        "five_hour": statusline_window(&limits["five_hour"]),
        "seven_day": statusline_window(&limits["seven_day"]),
    });
    private_dir(&home.join(".sessionsation"))?;
    atomic_write(&usage_file(home), &serde_json::to_vec(&record)?)?;
    Ok(true)
}
/// The hidden statusline hook. Never fails the statusline on recorder errors.
pub fn statusline_hook(record_only: bool) -> i32 {
    let mut input = vec![];
    let _ = std::io::stdin().take(1024 * 1024).read_to_end(&mut input);
    let home = std::env::var_os("CLAUDE_CONFIG_DIR").map(PathBuf::from);
    if let (Some(home), Ok(value)) = (&home, serde_json::from_slice::<Value>(&input))
        && home.is_absolute()
    {
        let _ = record_statusline(home, &value, crate::daemon::now());
    }
    if record_only {
        return 0;
    }
    let Some(command) = home.as_deref().and_then(chained_command) else {
        return 0;
    };
    let Ok(mut child) = std::process::Command::new("/bin/sh")
        .arg("-c")
        .arg(&command)
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        .spawn()
    else {
        return 0;
    };
    if let Some(mut stdin) = child.stdin.take() {
        let _ = stdin.write_all(&input);
    }
    if let Ok(output) = child.wait_with_output() {
        let _ = std::io::stdout().write_all(&output.stdout);
    }
    0
}
/// The account's own statusline command, unless it is this recorder.
fn chained_command(home: &Path) -> Option<String> {
    let bytes = fs::read(home.join("settings.json")).ok()?;
    let v: Value = serde_json::from_slice(&bytes).ok()?;
    let command = v["statusLine"]["command"].as_str()?.trim();
    (!command.is_empty() && !command.contains(HOOK_MARKER)).then(|| command.to_owned())
}

/// Project recorded statusline usage into the shared account usage shape.
/// Missing windows stay null (never 0%). Returns (usage, fresh, state).
pub fn project_usage(home: &Path, now: i64) -> (Value, bool, &'static str) {
    let base = |state: &'static str| {
        json!({"short":null,"weekly":null,"observed_at":0,"exhausted":false,
            "source":USAGE_SOURCE,"semantics":USAGE_SEMANTICS,"state":state})
    };
    let bytes = match fs::read(usage_file(home)) {
        Ok(b) => b,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
            return (base("missing"), false, "missing");
        }
        Err(_) => return (base("error"), false, "error"),
    };
    let Ok(record) = serde_json::from_slice::<Value>(&bytes) else {
        return (base("error"), false, "error");
    };
    let Some(observed) = record["observed_at"].as_i64().filter(|t| *t > 0) else {
        return (base("error"), false, "error");
    };
    let window = |key: &str, minutes: u64| {
        let w = statusline_window(&record[key]);
        if w.is_null() {
            Value::Null
        } else {
            json!({"duration_minutes":minutes,"used_percent":w["used_percentage"],"resets_at":w["resets_at"]})
        }
    };
    let short = window("five_hour", 300);
    let weekly = window("seven_day", 10080);
    let present: Vec<&Value> = [&short, &weekly]
        .into_iter()
        .filter(|w| !w.is_null())
        .collect();
    let state = if observed > now + 60 {
        "error"
    } else if now - observed > USAGE_STALE_SECONDS
        || present
            .iter()
            .any(|w| w["resets_at"].as_i64().is_none_or(|r| r <= now))
    {
        "stale"
    } else if present.is_empty() {
        "no_windows"
    } else if present.len() == 1 {
        "partial"
    } else {
        "fresh"
    };
    let mut usage = base(state);
    usage["short"] = short;
    usage["weekly"] = weekly;
    usage["observed_at"] = json!(observed);
    usage["claude_version"] = record["claude_version"].clone();
    (usage, state == "fresh", state)
}

#[cfg(test)]
mod tests {
    use super::*;
    fn paths() -> (tempfile::TempDir, Paths) {
        let t = tempfile::tempdir().unwrap();
        let p = Paths::new(Some(t.path().join("root"))).unwrap();
        (t, p)
    }
    #[test]
    fn child_environment_strips_billing_overrides_and_isolates_config() {
        let config = Config {
            claude_binary: "/opt/claude".into(),
            claude_session_home: Some("/srv/claude-home".into()),
            ..Default::default()
        };
        let c = command(&config, Path::new("/tmp/account-home"));
        assert_eq!(c.get_program(), "/opt/claude");
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
            assert!(
                envs.contains(&(name.to_string(), None)),
                "{name} not removed"
            );
        }
        assert!(!envs.iter().any(|(k, _)| k == "ANTHROPIC_MODEL"));
        assert!(envs.contains(&("CLAUDE_CONFIG_DIR".into(), Some("/tmp/account-home".into()))));
        assert!(envs.contains(&("DISABLE_AUTOUPDATER".into(), Some("1".into()))));
        assert!(envs.contains(&("HOME".into(), Some("/srv/claude-home".into()))));
        let inherited = command(&Config::default(), Path::new("/tmp/h"));
        assert!(!inherited.get_envs().any(|(k, _)| k == "HOME"));
    }
    #[test]
    fn auth_gate_requires_subscription_on_first_party() {
        let ok = AuthStatus::parse(br#"{"loggedIn":true,"authMethod":"claude.ai","apiProvider":"firstParty","email":"x@example.invalid","orgId":"o"}"#).unwrap();
        assert!(ok.gate().is_ok());
        assert!(!format!("{ok:?}").contains("example.invalid"));
        let out = AuthStatus::parse(br#"{"loggedIn":false}"#).unwrap();
        assert!(
            out.gate()
                .unwrap_err()
                .to_string()
                .starts_with("AUTH_REQUIRED")
        );
        for body in [
            r#"{"loggedIn":true,"authMethod":"apiKey","apiProvider":"firstParty"}"#,
            r#"{"loggedIn":true,"authMethod":"claude.ai","apiProvider":"bedrock"}"#,
        ] {
            let s = AuthStatus::parse(body.as_bytes()).unwrap();
            assert!(
                s.gate()
                    .unwrap_err()
                    .to_string()
                    .starts_with("WRONG_AUTH_MODE")
            );
        }
        assert!(AuthStatus::parse(b"not json").is_err());
    }
    #[test]
    fn adoption_requires_an_existing_unshared_directory_outside_the_root() {
        let (t, p) = paths();
        let state = State::default();
        assert!(validate_adopt(&p, &state, Path::new("relative")).is_err());
        assert!(validate_adopt(&p, &state, &t.path().join("missing")).is_err());
        let file = t.path().join("file");
        fs::write(&file, b"").unwrap();
        assert!(validate_adopt(&p, &state, &file).is_err());
        let inside = p.root.join("accounts/x");
        fs::create_dir_all(&inside).unwrap();
        assert!(
            validate_adopt(&p, &state, &inside)
                .unwrap_err()
                .to_string()
                .contains("INVALID_CONFIG_DIR")
        );
        let external = t.path().join("claude-config");
        fs::create_dir(&external).unwrap();
        assert_eq!(
            validate_adopt(&p, &state, &external).unwrap(),
            fs::canonicalize(&external).unwrap()
        );
        let mut state = State::default();
        let mut a = crate::model::Account::new("other".into(), String::new());
        a.provider = ProviderId::Claude;
        a.config_home = Some(external.clone());
        state.accounts.insert("other".into(), a);
        let nested = external.join("nested");
        fs::create_dir(&nested).unwrap();
        for dir in [&external, &nested] {
            let e = validate_adopt(&p, &state, dir).unwrap_err().to_string();
            assert!(
                e.contains("CONFIG_DIR_IN_USE") && e.contains("claude:other"),
                "{e}"
            );
        }
    }
    #[test]
    fn registry_entries_require_matching_process_start_and_never_read_keys() {
        let t = tempfile::tempdir().unwrap();
        let home = t.path();
        let pid = std::process::id();
        let (_, start) = proc_start(pid).unwrap();
        fs::create_dir_all(home.join("sessions")).unwrap();
        let id = "11111111-1111-4111-8111-111111111111";
        fs::write(
            home.join("sessions").join(format!("{pid}.json")),
            json!({"pid":pid,"sessionId":id,"procStart":(start + 1).to_string(),"status":"busy"})
                .to_string(),
        )
        .unwrap();
        // An unreadable *.key file must never be opened.
        let key = home.join("sessions").join(format!("{pid}.abc.key"));
        fs::write(&key, b"secret").unwrap();
        use std::os::unix::fs::PermissionsExt;
        fs::set_permissions(&key, fs::Permissions::from_mode(0o000)).unwrap();
        assert!(
            live_sessions(home).unwrap().is_empty(),
            "PID reuse must not count as live"
        );
        let o = observe(home, id, None);
        assert_eq!(o.availability, "stale");
        let dir = home.join("projects/-work");
        fs::create_dir_all(&dir).unwrap();
        fs::write(
            dir.join(format!("{id}.jsonl")),
            format!("{}\n", json!({"sessionId":id,"cwd":"/work","timestamp":"2026-01-01T00:00:00Z","message":{"content":"private text"}})),
        )
        .unwrap();
        let o = observe(home, id, None);
        assert_eq!(o.availability, "stopped");
        assert_eq!(o.directory.as_deref(), Some("/work"));
        assert!(!serde_json::to_string(&o).unwrap().contains("private text"));
        let listed = transcripts(home);
        assert_eq!(listed.len(), 1);
        assert!(
            !serde_json::to_string(&listed)
                .unwrap()
                .contains("private text")
        );
        // Matching start but a different config home (this test process has no
        // CLAUDE_CONFIG_DIR) is still not a live session of this home.
        fs::write(
            home.join("sessions").join(format!("{pid}.json")),
            json!({"pid":pid,"sessionId":id,"procStart":start.to_string()}).to_string(),
        )
        .unwrap();
        let expected = std::env::var_os("CLAUDE_CONFIG_DIR").is_none();
        if expected {
            assert!(live_sessions(home).unwrap().is_empty());
        }
    }
    #[test]
    fn continuation_and_fork_markers_use_only_claude_metadata_records() {
        let t = tempfile::tempdir().unwrap();
        let old = "11111111-1111-4111-8111-111111111111";
        let new = "22222222-2222-4222-8222-222222222222";
        let path = t.path().join(format!("{old}.jsonl"));
        let user = json!({"type":"user","sessionId":old,"message":{"content":format!("\"type\":\"continued-in\" {new}")}});
        let marker = json!({"type":"continued-in","timestamp":"2026-10-28T00:00:00Z","sessionId":old,"continuedInSessionId":new});
        fs::write(&path, format!("{user}\n")).unwrap();
        assert_eq!(continued_in(&path), None, "message text is not a marker");
        fs::write(
            &path,
            format!(
                "{user}\n{marker}\n{}\n",
                json!({"type":"last-prompt","sessionId":old})
            ),
        )
        .unwrap();
        assert_eq!(continued_in(&path).as_deref(), Some(new));
        fs::write(&path, format!("{user}\n{marker}\n{user}\n")).unwrap();
        assert_eq!(
            continued_in(&path),
            None,
            "continued again after the hand-off"
        );
        let foreign = json!({"type":"continued-in","sessionId":new,"continuedInSessionId":old});
        fs::write(&path, format!("{foreign}\n")).unwrap();
        assert_eq!(continued_in(&path), None, "marker must name this session");
        let forked = t.path().join(format!("{new}.jsonl"));
        fs::write(&forked, format!("{}\n", json!({"type":"user","sessionId":new,"forkedFrom":{"sessionId":old,"messageUuid":"m"}}))).unwrap();
        assert_eq!(forked_from(&forked).as_deref(), Some(old));
        assert_eq!(forked_from(&path), None);
    }
    #[test]
    fn leftover_registry_files_of_killed_processes_never_fail_the_probe() {
        let t = tempfile::tempdir().unwrap();
        let home = t.path();
        fs::create_dir_all(home.join("sessions")).unwrap();
        let mut child = std::process::Command::new("sleep")
            .arg("30")
            .spawn()
            .unwrap();
        let dead = child.id();
        child.kill().unwrap();
        child.wait().unwrap();
        let file = home.join("sessions").join(format!("{dead}.json"));
        fs::write(&file, b"{not json").unwrap();
        use std::os::unix::fs::PermissionsExt;
        fs::set_permissions(&file, fs::Permissions::from_mode(0o000)).unwrap();
        fs::write(home.join("sessions/123.json"), b"{\"pid\":124}").unwrap();
        // Root may still read 0o000 files; either way the probe must succeed.
        assert!(live_sessions(home).unwrap().is_empty());
        assert_ne!(
            observe(home, "11111111-1111-4111-8111-111111111111", None).availability,
            "unknown"
        );
    }
    #[test]
    fn statusline_records_only_limits_and_projection_never_invents_zero() {
        let t = tempfile::tempdir().unwrap();
        let home = t.path();
        let now = 1_000_000;
        assert_eq!(project_usage(home, now).2, "missing");
        assert!(project_usage(home, now).0["short"].is_null());
        assert!(!record_statusline(home, &json!({"session_id":"s","version":"2"}), now).unwrap());
        let input = json!({"session_id":"s","version":"2.1.312","model":{"display_name":"secret-model"},
            "workspace":{"current_dir":"/private"},"transcript_path":"/private/t.jsonl",
            "rate_limits":{"five_hour":{"used_percentage":12.5,"resets_at":now+3600},"seven_day":{"used_percentage":40,"resets_at":now+86400}}});
        assert!(record_statusline(home, &input, now).unwrap());
        let file = usage_file(home);
        use std::os::unix::fs::PermissionsExt;
        assert_eq!(
            fs::metadata(&file).unwrap().permissions().mode() & 0o777,
            0o600
        );
        assert_eq!(
            fs::metadata(file.parent().unwrap())
                .unwrap()
                .permissions()
                .mode()
                & 0o777,
            0o700
        );
        let stored: Value = serde_json::from_slice(&fs::read(&file).unwrap()).unwrap();
        let mut keys: Vec<_> = stored.as_object().unwrap().keys().cloned().collect();
        keys.sort();
        assert_eq!(
            keys,
            [
                "claude_version",
                "five_hour",
                "observed_at",
                "session_id",
                "seven_day",
                "source"
            ]
        );
        assert!(!stored.to_string().contains("private"));
        let (usage, fresh, state) = project_usage(home, now + 10);
        assert!(fresh);
        assert_eq!(state, "fresh");
        assert_eq!(usage["short"]["duration_minutes"], 300);
        assert_eq!(usage["short"]["used_percent"], 12.5);
        assert_eq!(usage["weekly"]["duration_minutes"], 10080);
        assert_eq!(usage["source"], USAGE_SOURCE);
        assert!(usage["semantics"].is_string());
        let (_, fresh, state) = project_usage(home, now + USAGE_STALE_SECONDS + 1);
        assert!(!fresh);
        assert_eq!(state, "stale");
        let (_, _, state) = project_usage(home, now + 3601);
        assert_eq!(state, "stale", "a passed reset is stale");
        let partial =
            json!({"rate_limits":{"five_hour":{"used_percentage":5,"resets_at":now+3600}}});
        record_statusline(home, &partial, now).unwrap();
        let (usage, fresh, state) = project_usage(home, now);
        assert_eq!(state, "partial");
        assert!(!fresh);
        assert!(usage["weekly"].is_null(), "missing window is null, never 0");
        record_statusline(home, &json!({"rate_limits":{}}), now).unwrap();
        let (usage, _, state) = project_usage(home, now);
        assert_eq!(state, "no_windows");
        assert!(usage["short"].is_null() && usage["weekly"].is_null());
        fs::write(usage_file(home), b"{").unwrap();
        assert_eq!(project_usage(home, now).2, "error");
    }
    #[test]
    fn chained_statusline_skips_the_recorder_itself() {
        let t = tempfile::tempdir().unwrap();
        assert_eq!(chained_command(t.path()), None);
        fs::write(
            t.path().join("settings.json"),
            json!({"statusLine":{"command":"echo mine"}}).to_string(),
        )
        .unwrap();
        assert_eq!(chained_command(t.path()).as_deref(), Some("echo mine"));
        fs::write(
            t.path().join("settings.json"),
            json!({"statusLine":{"command":format!("/x/sessionsation {HOOK_MARKER}")}}).to_string(),
        )
        .unwrap();
        assert_eq!(chained_command(t.path()), None);
    }
    #[test]
    fn launch_plans_set_native_identity_name_and_settings() {
        let config = Config::default();
        let settings = launch_settings(Path::new("/opt/session sation"));
        let v: Value = serde_json::from_str(&settings).unwrap();
        assert!(
            v["statusLine"]["command"]
                .as_str()
                .unwrap()
                .ends_with(HOOK_MARKER)
        );
        assert!(
            v["statusLine"]["command"]
                .as_str()
                .unwrap()
                .starts_with('\'')
        );
        let id = "11111111-1111-4111-8111-111111111111";
        assert_eq!(
            new_argv(&config, id, "label", &settings),
            [
                "claude",
                "--session-id",
                id,
                "--name",
                "label",
                "--settings",
                settings.as_str()
            ]
        );
        assert_eq!(resume_argv(&config, id, "label", &settings)[1], "--resume");
    }
}
