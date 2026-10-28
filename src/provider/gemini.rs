//! Gemini CLI provider. Each account is an isolated `GEMINI_CLI_HOME` created at
//! `<root>/gemini-accounts/<name>`; everything Gemini keeps lives in
//! `<home>/.gemini/`. Sessionsation never reads Gemini credentials
//! (`oauth_creds.json`): authentication is "present" when that file is a
//! non-empty regular file and `google_accounts.json` names an active account.
//! Sessions come from chat-file headers (`sessionId`, `projectHash`, `startTime`)
//! plus the project registry (`projects.json`, cwd to slug); liveness comes from
//! `/proc` (command line, `GEMINI_CLI_HOME` and start time) because Gemini keeps
//! no live-session registry. Gemini reports no usage, so usage is unsupported
//! (state `unsupported`, windows null, never 0%).
use super::{
    Capability, ProviderCapabilities, ProviderId,
    claude::{Enrolled, Live, Observation, Transcript, environ, proc_start, regular_file},
    no, yes,
};
use crate::{
    catalog,
    config::Config,
    paths::{Paths, atomic_write, private_dir, validate_name},
};
use anyhow::{Context, Result, bail, ensure};
use serde_json::{Value, json};
use sha2::{Digest, Sha256};
use std::{
    collections::BTreeMap,
    fs,
    io::{BufRead, BufReader, Read},
    path::{Path, PathBuf},
};
pub mod launch;

/// Ambient variables that would silently switch Gemini to API-key billing, a
/// Vertex/Cloud project, another endpoint or a different model. Removed from
/// every Gemini child.
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
];
pub const USAGE_SOURCE: &str = "gemini-cli-unsupported";
pub const USAGE_SEMANTICS: &str = "Gemini CLI exposes quota only through its interactive /stats model command; no hook or file reports usage, so no window is ever reported and none is shown as 0%";
pub const USAGE_UNSUPPORTED: &str = "Gemini CLI exposes no usage or quota hook or file; quota is visible only in its /stats model command";
pub const NATIVE_HOME_BOUND: &str = "native session is bound to its account home";
/// Gemini CLI has no supported way to attach a terminal to a running process.
pub const NO_HERDR_ATTACH: &str = "NO_NATIVE_ATTACH: Gemini CLI has no supported attach to a running session; stop it in its terminal, then use Start Session";
pub const NO_INPUT_API: &str = "no supported native input API for a live Gemini session";
pub const LABEL_ONLY: &str =
    "label is a Sessionsation catalog label; Gemini CLI has no native session rename";
const NO_LIFECYCLE: &str = "no verified Gemini process lifecycle control; stop it in its terminal";
/// The auth type recorded in a seeded `settings.json` (Login with Google).
pub const AUTH_TYPE: &str = "oauth-personal";

pub fn capabilities() -> ProviderCapabilities {
    ProviderCapabilities {
        enrollment: yes(),
        adopt_config_dir: no("Gemini homes are created and owned by sessionsation"),
        login: yes(),
        auth_status: Capability {
            supported: true,
            reason: Some(
                "file-based: credential files present and an active Google account recorded; validity is not checked without a network call",
            ),
        },
        usage: no(USAGE_UNSUPPORTED),
        new: yes(),
        resume: yes(),
        rename: yes(),
        native_rename: no(LABEL_ONLY),
        stop: no(NO_LIFECYCLE),
        restart: no(NO_LIFECYCLE),
        account_switch: no(NATIVE_HOME_BOUND),
        relocate: no(NATIVE_HOME_BOUND),
        move_space: no("HERDR space moves are verified only for Codex"),
        archive: no("Gemini chat files are owned by the account home"),
        delete_session: no("Gemini chat files are owned by the account home"),
        submit: no(NO_INPUT_API),
        queue: no(NO_INPUT_API),
        steer: no(NO_INPUT_API),
        account_removal: yes(),
        account_deletion: Capability {
            supported: true,
            reason: Some(
                "managed homes: conversation history preserved, home and credentials deleted",
            ),
        },
        account_rename: yes(),
        headless_delivery: Capability {
            supported: true,
            reason: Some(
                "api send mode resume (stopped session) and mode new run gemini -p headlessly; live sessions are refused",
            ),
        },
        account_limits: no(USAGE_UNSUPPORTED),
        cross_provider_conversion: no("conversations are bound to their provider"),
    }
}

// ------------------------------------------------------------------- homes

pub fn home(paths: &Paths, name: &str) -> Result<PathBuf> {
    validate_name(name)?;
    Ok(paths.root.join("gemini-accounts").join(name))
}
/// Create the managed home `<root>/gemini-accounts/<name>` (0700), or register
/// one an operator already placed there. An existing home is never wiped or
/// re-permissioned: it must already be a private (0700) directory owned by this
/// user and not a symlink. Settings are seeded either way. Returns (home, existed).
pub fn create_home(paths: &Paths, name: &str) -> Result<(PathBuf, bool)> {
    use std::os::unix::fs::MetadataExt;
    private_dir(&paths.root.join("gemini-accounts"))?;
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
    seed_settings(&home)?;
    Ok((home, existed))
}

fn set_path(root: &mut Value, keys: &[&str], value: Value, overwrite: bool) -> Result<bool> {
    let mut node = root;
    for key in &keys[..keys.len() - 1] {
        if node.get(*key).is_none_or(Value::is_null) {
            node[*key] = json!({});
        }
        node = node.get_mut(*key).unwrap();
        ensure!(
            node.is_object(),
            "UNSAFE_SETTINGS: Gemini settings.json has a non-object '{key}'; it was left unchanged"
        );
    }
    let leaf = keys[keys.len() - 1];
    if node.get(leaf) == Some(&value)
        || (!overwrite && node.get(leaf).is_some_and(|v| !v.is_null()))
    {
        return Ok(false);
    }
    node[leaf] = value;
    Ok(true)
}
/// Seed `<home>/.gemini/settings.json`: no auto-update or update notice, no
/// usage statistics, no automatic session deletion, and the Login-with-Google
/// auth type when none is chosen. Only these keys are written; every other key
/// and an operator's own auth selection are preserved. An unreadable or
/// non-object settings file is an error and is left unchanged.
pub fn seed_settings(home: &Path) -> Result<()> {
    let dir = home.join(".gemini");
    private_dir(&dir)?;
    let path = dir.join("settings.json");
    let mut root = match fs::symlink_metadata(&path) {
        Ok(m) => {
            ensure!(
                m.file_type().is_file() && m.len() <= 1024 * 1024,
                "UNSAFE_SETTINGS: Gemini settings.json must be a regular file; it was left unchanged"
            );
            let v: Value = serde_json::from_slice(&fs::read(&path)?).context(
                "UNSAFE_SETTINGS: Gemini settings.json is not valid JSON; it was left unchanged",
            )?;
            ensure!(
                v.is_object(),
                "UNSAFE_SETTINGS: Gemini settings.json is not an object; it was left unchanged"
            );
            v
        }
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => json!({}),
        Err(e) => return Err(e.into()),
    };
    let mut changed = false;
    for (keys, value, overwrite) in [
        (&["general", "enableAutoUpdate"][..], json!(false), true),
        (
            &["general", "enableAutoUpdateNotification"][..],
            json!(false),
            true,
        ),
        (
            &["privacy", "usageStatisticsEnabled"][..],
            json!(false),
            true,
        ),
        (
            &["general", "sessionRetention", "enabled"][..],
            json!(false),
            true,
        ),
        (
            &["security", "auth", "selectedType"][..],
            json!(AUTH_TYPE),
            false,
        ),
    ] {
        changed |= set_path(&mut root, keys, value, overwrite)?;
    }
    if changed || fs::symlink_metadata(&path).is_err() {
        atomic_write(&path, &serde_json::to_vec_pretty(&root)?)?;
    }
    Ok(())
}

// --------------------------------------------------------------- environment

/// The single builder for every Gemini child process: binary, isolated home,
/// no self-relaunch, file-based credential storage, stripped backend/billing
/// overrides. Callers add anything else (for example headless trust).
pub fn command(config: &Config, home: &Path) -> std::process::Command {
    let mut command = std::process::Command::new(&config.gemini_binary);
    for name in STRIPPED_ENV {
        command.env_remove(name);
    }
    command
        .env("GEMINI_CLI_HOME", home)
        .env("GEMINI_CLI_NO_RELAUNCH", "1")
        .env("GEMINI_FORCE_FILE_STORAGE", "true");
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
        match self.problem() {
            None => Ok(()),
            Some(_) => bail!(
                "AUTH_REQUIRED: this Gemini account is not logged in; run `sessionsation account login gemini:<name>`"
            ),
        }
    }
    pub fn mode(&self) -> Value {
        json!({"logged_in":self.logged_in,"auth_method":self.logged_in.then_some(AUTH_TYPE),
            "api_provider":null,"subscription_type":null})
    }
}
/// File-based authentication presence. The credential file is only stat'ed
/// (never opened); `google_accounts.json` holds a display email, not a secret,
/// and only whether an account is active is used.
pub fn auth_status(home: &Path) -> AuthStatus {
    let gemini = home.join(".gemini");
    let credentials = fs::symlink_metadata(gemini.join("oauth_creds.json"))
        .is_ok_and(|m| m.file_type().is_file() && m.len() > 0);
    let active = (|| {
        let mut bytes = vec![];
        let file = fs::File::open(gemini.join("google_accounts.json")).ok()?;
        file.take(64 * 1024).read_to_end(&mut bytes).ok()?;
        let v: Value = serde_json::from_slice(&bytes).ok()?;
        v["active"].as_str().filter(|s| !s.is_empty()).map(|_| ())
    })()
    .is_some();
    AuthStatus {
        logged_in: credentials && active,
    }
}
/// Non-identifying auth summary for enrollment output (never email).
pub fn auth_report(home: &Path) -> Value {
    let status = auth_status(home);
    let mut v = status.mode();
    v["problem"] = json!(status.problem());
    v
}
pub fn prelaunch_gate(home: &Path) -> Result<()> {
    seed_settings(home)?;
    auth_status(home).gate()
}
/// Interactive login: `gemini` itself with `NO_BROWSER=true` so it prints the
/// URL and accepts a pasted code. The user completes it; nothing is automated.
pub fn login_command(config: &Config, home: &Path) -> std::process::Command {
    let mut command = command(config, home);
    command.env("NO_BROWSER", "true");
    command
}
/// The working directory for the login: the private parent of the home, so the
/// home's own `.gemini` is not also read as a project directory.
pub fn login_directory(home: &Path) -> PathBuf {
    home.parent().unwrap_or(home).to_path_buf()
}

// ---------------------------------------------------------------- discovery

/// One parsed chat-file header.
#[derive(Clone, Debug)]
struct Chat {
    path: PathBuf,
    session_id: String,
    project_hash: Option<String>,
    started_at: Option<String>,
    slug: String,
    bytes: u64,
}
fn tmp_dir(home: &Path) -> PathBuf {
    home.join(".gemini/tmp")
}
fn chat_files(home: &Path) -> Vec<(String, PathBuf)> {
    let mut out = vec![];
    let Ok(slugs) = fs::read_dir(tmp_dir(home)) else {
        return out;
    };
    for slug in slugs.flatten() {
        if !slug.file_type().is_ok_and(|t| t.is_dir()) {
            continue;
        }
        let Ok(files) = fs::read_dir(slug.path().join("chats")) else {
            continue;
        };
        for file in files.flatten() {
            let path = file.path();
            let name = file.file_name();
            let Some(name) = name.to_str() else { continue };
            if name.starts_with("session-") && name.ends_with(".jsonl") && regular_file(&path) {
                out.push((slug.file_name().to_string_lossy().into_owned(), path));
            }
        }
    }
    out.sort();
    out
}
/// Header metadata only (first line); message text is never read.
fn header(path: &Path) -> Option<Value> {
    let file = fs::File::open(path).ok()?;
    let mut line = String::new();
    BufReader::new(file.take(64 * 1024))
        .read_line(&mut line)
        .ok()?;
    serde_json::from_str(&line).ok()
}
fn chat(slug: &str, path: &Path) -> Option<Chat> {
    let v = header(path)?;
    let session_id = v["sessionId"].as_str()?.to_owned();
    uuid::Uuid::parse_str(&session_id).ok()?;
    // session-<timestamp>-<first 8 of the id>.jsonl
    let stem = path.file_name()?.to_str()?.strip_suffix(".jsonl")?;
    if !stem.ends_with(&format!("-{}", &session_id[..8])) {
        return None;
    }
    let text = |k: &str| v[k].as_str().filter(|s| !s.is_empty()).map(str::to_owned);
    Some(Chat {
        path: path.to_path_buf(),
        session_id,
        project_hash: text("projectHash"),
        started_at: text("startTime"),
        slug: slug.to_owned(),
        bytes: fs::symlink_metadata(path).map(|m| m.len()).unwrap_or(0),
    })
}
/// `projects.json` maps an absolute working directory to its slug.
fn projects(home: &Path) -> Vec<(String, String)> {
    let mut bytes = vec![];
    let Ok(file) = fs::File::open(home.join(".gemini/projects.json")) else {
        return vec![];
    };
    if file.take(1024 * 1024).read_to_end(&mut bytes).is_err() {
        return vec![];
    }
    let Ok(v) = serde_json::from_slice::<Value>(&bytes) else {
        return vec![];
    };
    v["projects"]
        .as_object()
        .into_iter()
        .flatten()
        .filter_map(|(cwd, slug)| Some((cwd.clone(), slug.as_str()?.to_owned())))
        .filter(|(cwd, _)| Path::new(cwd).is_absolute())
        .collect()
}
fn cwd_of(projects: &[(String, String)], chat: &Chat) -> Option<String> {
    projects
        .iter()
        .find(|(_, slug)| *slug == chat.slug)
        .or_else(|| {
            let hash = chat.project_hash.as_deref()?;
            projects
                .iter()
                .find(|(cwd, _)| format!("{:x}", Sha256::digest(cwd.as_bytes())) == hash)
        })
        .map(|(cwd, _)| cwd.clone())
}
fn transcript(projects: &[(String, String)], chat: Chat) -> Transcript {
    Transcript {
        session_id: chat.session_id.clone(),
        cwd: cwd_of(projects, &chat),
        started_at: chat.started_at.clone(),
        bytes: chat.bytes,
        path: chat.path,
    }
}
/// Chat files `<home>/.gemini/tmp/<slug>/chats/session-*.jsonl` with header
/// metadata only, one entry per session id.
pub fn transcripts(home: &Path) -> Vec<Transcript> {
    let registry = projects(home);
    let mut by_id = BTreeMap::new();
    for (slug, path) in chat_files(home) {
        if let Some(chat) = chat(&slug, &path) {
            by_id.entry(chat.session_id.clone()).or_insert(chat);
        }
    }
    by_id
        .into_values()
        .map(|c| transcript(&registry, c))
        .collect()
}
fn find_chat(home: &Path, session_id: &str) -> Option<Chat> {
    uuid::Uuid::parse_str(session_id).ok()?;
    let suffix = format!("-{}.jsonl", &session_id[..8]);
    chat_files(home)
        .into_iter()
        .filter(|(_, p)| {
            p.file_name()
                .and_then(|n| n.to_str())
                .is_some_and(|n| n.ends_with(&suffix))
        })
        .find_map(|(slug, p)| chat(&slug, &p).filter(|c| c.session_id == session_id))
}
pub fn find_transcript(home: &Path, session_id: &str) -> Option<PathBuf> {
    find_chat(home, session_id).map(|c| c.path)
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
/// A Gemini CLI process: `node .../gemini.js ...` or a `gemini` executable among
/// the leading arguments (interpreter and script).
fn is_gemini_arguments(args: &[String]) -> bool {
    args.iter().take(3).any(|a| {
        Path::new(a)
            .file_name()
            .is_some_and(|n| n == "gemini.js" || n == "gemini")
    })
}
/// The pre-assigned or resumed native session id on the command line.
fn session_argument(args: &[String]) -> Option<String> {
    args.windows(2)
        .find(|w| {
            matches!(w[0].as_str(), "--session-id" | "--resume" | "-r")
                && uuid::Uuid::parse_str(&w[1]).is_ok()
        })
        .map(|w| w[1].clone())
}
fn home_matches(pid: u32, home: &Path) -> bool {
    let Some(env) = environ(pid) else {
        return false;
    };
    env.iter()
        .find(|(k, _)| k == "GEMINI_CLI_HOME")
        .is_some_and(|(_, v)| {
            let v = PathBuf::from(v);
            v == home
                || fs::canonicalize(&v)
                    .ok()
                    .is_some_and(|c| fs::canonicalize(home).is_ok_and(|h| c == h))
        })
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
/// Verified live interactive sessions of one home: a Gemini process whose
/// command line names the session, whose `GEMINI_CLI_HOME` is this home and
/// that is not a zombie. The PID plus /proc start time is the identity.
pub fn live_sessions(home: &Path) -> Result<Vec<Live>> {
    let mut result = vec![];
    for pid in pids() {
        let Some(args) = arguments(pid) else { continue };
        if !is_gemini_arguments(&args) {
            continue;
        }
        let Some(session_id) = session_argument(&args) else {
            continue;
        };
        let Some((state, start)) = proc_start(pid) else {
            continue;
        };
        if matches!(state, 'Z' | 'X') || !home_matches(pid, home) {
            continue;
        }
        result.push(Live {
            pid,
            process_start: start,
            session_id,
            cwd: fs::read_link(format!("/proc/{pid}/cwd"))
                .ok()
                .and_then(|p| p.to_str().map(str::to_owned)),
            status: None,
            name: None,
            kind: None,
            job_id: None,
        });
    }
    result.sort_by_key(|l| l.pid);
    Ok(result)
}
/// Any Gemini process (interactive or headless) using one of `homes`.
pub(crate) fn env_in_use(homes: &[&Path]) -> bool {
    pids().into_iter().any(|pid| {
        arguments(pid).is_some_and(|a| is_gemini_arguments(&a))
            && proc_start(pid).is_some_and(|(s, _)| !matches!(s, 'Z' | 'X'))
            && homes.iter().any(|h| home_matches(pid, h))
    })
}
/// True when the exact process recorded for a new/resumed session runs Gemini
/// for that native session (command line check, PID-reuse guarded by start time).
pub fn launch_observed(session: &catalog::Session) -> bool {
    let Some(a) = &session.attachment else {
        return false;
    };
    if proc_start(a.pid)
        .is_none_or(|(state, start)| start != a.process_start || matches!(state, 'Z' | 'X'))
    {
        return false;
    }
    arguments(a.pid).is_some_and(|args| {
        is_gemini_arguments(&args)
            && session_argument(&args).as_deref() == Some(&session.conversation_id)
    })
}

fn observation(
    recorded: Option<&catalog::Attachment>,
    session_id: &str,
    chat: Option<(Chat, Option<String>)>,
    live: &std::result::Result<Vec<Live>, String>,
) -> Observation {
    let (transcript, cwd, started_at, bytes) = match chat {
        Some((c, cwd)) => (Some(c.path), cwd, c.started_at, Some(c.bytes)),
        None => (None, None, None, None),
    };
    let mut o = Observation {
        availability: "unknown",
        native_status: None,
        native_name: None,
        pid: None,
        process_start: None,
        directory: cwd,
        transcript,
        transcript_bytes: bytes,
        started_at,
        attachment: None,
        detail: None,
        native_kind: None,
        job_id: None,
        attachment_switched: false,
        forked_from: None,
    };
    match live {
        Err(e) => o.detail = Some(format!("PROBE_FAILED: {e}")),
        Ok(live) => match live.iter().find(|l| l.session_id == session_id) {
            Some(l) => {
                o.availability = "running";
                o.attachment = super::claude::host_attachment(l, recorded);
                o.pid = Some(l.pid);
                o.process_start = Some(l.process_start);
                if l.cwd.is_some() {
                    o.directory = l.cwd.clone();
                }
            }
            None if o.transcript.is_some() => o.availability = "stopped",
            None => o.availability = "stale",
        },
    }
    o
}
pub fn observe(
    home: &Path,
    session_id: &str,
    recorded: Option<&catalog::Attachment>,
) -> Observation {
    let registry = projects(home);
    let chat = find_chat(home, session_id).map(|c| {
        let cwd = cwd_of(&registry, &c);
        (c, cwd)
    });
    let live = live_sessions(home).map_err(|e| e.to_string());
    observation(recorded, session_id, chat, &live)
}

// ----------------------------------------------------------------- accounts

/// Gemini accounts from a read-only registry snapshot (daemon need not run).
pub fn enrolled(paths: &Paths) -> Result<Vec<Enrolled>> {
    if !paths.root.join("registry.db").exists() {
        return Ok(vec![]);
    }
    let state = crate::store::load_state_snapshot(paths)?;
    state
        .accounts
        .values()
        .filter(|a| a.provider == ProviderId::Gemini)
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
        "ACCOUNT_DELETED: gemini:{name} was deleted locally; its conversation history is preserved but it cannot be resumed through this account"
    );
    bail!("ACCOUNT_NOT_FOUND: no enrolled Gemini account with this name")
}
/// True when a completed local deletion removed this (no longer enrolled) Gemini account.
pub fn deleted(paths: &Paths, name: &str) -> Result<bool> {
    let id = format!("gemini:{name}");
    Ok(crate::account_deletion::list(paths)?
        .iter()
        .any(|r| r.account_id == id && r.phase == "complete"))
}

/// Register discovered chats and refresh live attachments for every Gemini
/// account. Returns observations keyed by catalog session id.
pub fn reconcile(paths: &Paths) -> Result<BTreeMap<String, (Observation, PathBuf)>> {
    let accounts = enrolled(paths)?;
    let mut discovered = vec![];
    let mut scans = BTreeMap::new();
    for a in &accounts {
        let found = transcripts(&a.home);
        for t in &found {
            discovered.push(catalog::Discovered {
                account: a.name.clone(),
                conversation_id: t.session_id.clone(),
                label: t
                    .cwd
                    .as_deref()
                    .and_then(|c| Path::new(c).file_name())
                    .and_then(|n| n.to_str())
                    .unwrap_or("gemini")
                    .to_owned(),
                directory: t.cwd.clone(),
            });
        }
        // One /proc scan per account, however many sessions it holds.
        let live = live_sessions(&a.home).map_err(|e| e.to_string());
        scans.insert(a.name.clone(), (found, live));
    }
    let sessions = catalog::register_discovered(paths, ProviderId::Gemini, &discovered)?;
    let mut observations = BTreeMap::new();
    let mut attachments = vec![];
    for s in sessions {
        let Some(a) = accounts.iter().find(|a| a.name == s.account) else {
            continue;
        };
        let (found, live) = &scans[&a.name];
        let chat = found
            .iter()
            .find(|t| t.session_id == s.conversation_id)
            .map(|t| {
                (
                    Chat {
                        path: t.path.clone(),
                        session_id: t.session_id.clone(),
                        project_hash: None,
                        started_at: t.started_at.clone(),
                        slug: String::new(),
                        bytes: t.bytes,
                    },
                    t.cwd.clone(),
                )
            });
        let o = observation(s.attachment.as_ref(), &s.conversation_id, chat, live);
        if o.availability != "unknown" {
            let next = if o.availability == "running" {
                o.attachment.clone()
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

#[cfg(test)]
mod tests {
    use super::*;
    use std::os::unix::fs::PermissionsExt;

    #[test]
    fn child_environment_isolates_home_and_strips_backend_overrides() {
        let config = Config {
            gemini_binary: "/opt/gemini".into(),
            ..Default::default()
        };
        let c = command(&config, Path::new("/tmp/account-home"));
        assert_eq!(c.get_program(), "/opt/gemini");
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
        assert!(
            envs.contains(&("GEMINI_CLI_HOME".into(), Some("/tmp/account-home".into()))),
            "{envs:?}"
        );
        assert!(envs.contains(&("GEMINI_CLI_NO_RELAUNCH".into(), Some("1".into()))));
        assert!(envs.contains(&("GEMINI_FORCE_FILE_STORAGE".into(), Some("true".into()))));
        assert!(!envs.iter().any(|(k, _)| k == "NO_BROWSER"));
        let login = login_command(&config, Path::new("/tmp/account-home"));
        assert!(
            login
                .get_envs()
                .any(|(k, v)| k == "NO_BROWSER" && v.is_some_and(|v| v == "true"))
        );
    }

    #[test]
    fn settings_are_seeded_without_clobbering_other_keys() {
        let d = tempfile::tempdir().unwrap();
        let home = d.path();
        seed_settings(home).unwrap();
        let path = home.join(".gemini/settings.json");
        let v: Value = serde_json::from_slice(&fs::read(&path).unwrap()).unwrap();
        assert_eq!(v["general"]["enableAutoUpdate"], false);
        assert_eq!(v["general"]["enableAutoUpdateNotification"], false);
        assert_eq!(v["privacy"]["usageStatisticsEnabled"], false);
        assert_eq!(v["general"]["sessionRetention"]["enabled"], false);
        assert_eq!(v["security"]["auth"]["selectedType"], "oauth-personal");
        assert_eq!(
            fs::metadata(&path).unwrap().permissions().mode() & 0o777,
            0o600
        );
        fs::write(
            &path,
            json!({"theme":"dark","general":{"enableAutoUpdate":true,"vimMode":true},
                "security":{"auth":{"selectedType":"gemini-api-key"}}})
            .to_string(),
        )
        .unwrap();
        seed_settings(home).unwrap();
        let v: Value = serde_json::from_slice(&fs::read(&path).unwrap()).unwrap();
        assert_eq!(v["theme"], "dark");
        assert_eq!(v["general"]["vimMode"], true);
        assert_eq!(
            v["general"]["enableAutoUpdate"], false,
            "auto-update is always disabled"
        );
        assert_eq!(
            v["security"]["auth"]["selectedType"], "gemini-api-key",
            "an operator's own auth selection is kept"
        );
        // Invalid settings are refused and left alone.
        fs::write(&path, "{not json").unwrap();
        assert!(
            seed_settings(home)
                .unwrap_err()
                .to_string()
                .contains("UNSAFE_SETTINGS")
        );
        assert_eq!(fs::read(&path).unwrap(), b"{not json");
    }

    #[test]
    fn auth_needs_credential_file_and_an_active_account_and_never_reads_credentials() {
        let d = tempfile::tempdir().unwrap();
        let home = d.path();
        assert!(!auth_status(home).logged_in);
        assert_eq!(auth_status(home).problem(), Some("AUTH_REQUIRED"));
        let dir = home.join(".gemini");
        fs::create_dir_all(&dir).unwrap();
        let creds = dir.join("oauth_creds.json");
        fs::write(&creds, "synthetic").unwrap();
        // The credential file is only stat'ed: an unreadable one still counts.
        fs::set_permissions(&creds, fs::Permissions::from_mode(0o000)).unwrap();
        assert!(!auth_status(home).logged_in, "no active account recorded");
        fs::write(
            dir.join("google_accounts.json"),
            json!({"active":null,"old":[]}).to_string(),
        )
        .unwrap();
        assert!(!auth_status(home).logged_in);
        fs::write(
            dir.join("google_accounts.json"),
            json!({"active":"fixture@example.invalid","old":[]}).to_string(),
        )
        .unwrap();
        let s = auth_status(home);
        assert!(s.logged_in && s.gate().is_ok());
        assert!(!auth_report(home).to_string().contains("example.invalid"));
        fs::set_permissions(&creds, fs::Permissions::from_mode(0o600)).unwrap();
        fs::write(&creds, "").unwrap();
        assert!(!auth_status(home).logged_in, "empty credential file");
    }

    #[test]
    fn chat_headers_and_project_registry_give_identity_and_directory() {
        let d = tempfile::tempdir().unwrap();
        let home = d.path();
        let id = "11111111-1111-4111-8111-111111111111";
        let slug = home.join(".gemini/tmp/work/chats");
        fs::create_dir_all(&slug).unwrap();
        let file = slug.join("session-2026-01-01T00-00-11111111.jsonl");
        fs::write(
            &file,
            format!("{}\n{}\n", json!({"sessionId":id,"projectHash":"h","startTime":"2026-01-01T00:00:00Z","lastUpdated":"2026-01-01T00:00:00Z"}),
                json!({"type":"user","content":"private text"})),
        )
        .unwrap();
        // Wrong file suffix and non-UUID ids are not sessions.
        fs::write(
            slug.join("session-2026-01-01T00-00-deadbeef.jsonl"),
            format!("{}\n", json!({"sessionId":id,"projectHash":"h"})),
        )
        .unwrap();
        fs::write(
            slug.join("session-2026-01-01T00-00-notauuid.jsonl"),
            format!("{}\n", json!({"sessionId":"notauuid","projectHash":"h"})),
        )
        .unwrap();
        assert_eq!(find_transcript(home, id).as_deref(), Some(file.as_path()));
        let listed = transcripts(home);
        assert_eq!(listed.len(), 1);
        assert_eq!(listed[0].cwd, None, "no registry entry yet");
        fs::write(
            home.join(".gemini/projects.json"),
            json!({"projects":{"/srv/work":"work","relative":"x"}}).to_string(),
        )
        .unwrap();
        let listed = transcripts(home);
        assert_eq!(listed[0].cwd.as_deref(), Some("/srv/work"));
        assert_eq!(
            listed[0].started_at.as_deref(),
            Some("2026-01-01T00:00:00Z")
        );
        assert!(
            !serde_json::to_string(&listed)
                .unwrap()
                .contains("private text")
        );
        let o = observe(home, id, None);
        assert_eq!(o.availability, "stopped");
        assert_eq!(o.directory.as_deref(), Some("/srv/work"));
        assert_eq!(
            observe(home, "22222222-2222-4222-8222-222222222222", None).availability,
            "stale"
        );
        // A symlinked chat file is never followed.
        let outside = d.path().join("outside.jsonl");
        fs::write(&outside, "{}").unwrap();
        std::os::unix::fs::symlink(&outside, slug.join("session-x-33333333.jsonl")).unwrap();
        assert_eq!(transcripts(home).len(), 1);
    }

    #[test]
    fn command_lines_identify_gemini_sessions_only() {
        let id = "11111111-1111-4111-8111-111111111111";
        let args = |v: &[&str]| v.iter().map(|s| s.to_string()).collect::<Vec<_>>();
        let node = args(&["node", "/opt/gemini/bundle/gemini.js", "--session-id", id]);
        assert!(is_gemini_arguments(&node));
        assert_eq!(session_argument(&node).as_deref(), Some(id));
        let resumed = args(&["/usr/bin/gemini", "-r", id]);
        assert!(is_gemini_arguments(&resumed));
        assert_eq!(session_argument(&resumed).as_deref(), Some(id));
        assert_eq!(session_argument(&args(&["gemini", "-r", "latest"])), None);
        assert!(!is_gemini_arguments(&args(&[
            "vim",
            "notes",
            "--session-id",
            id
        ])));
        assert!(!is_gemini_arguments(&args(&["sh", "-c", "x", "gemini.js"])));
    }
    #[test]
    fn live_identity_requires_command_line_home_and_exact_start_time() {
        let d = tempfile::tempdir().unwrap();
        let home = d.path().join("home");
        let other = d.path().join("other");
        fs::create_dir_all(&home).unwrap();
        fs::create_dir_all(&other).unwrap();
        let script = d.path().join("gemini.js");
        fs::write(&script, "#!/bin/sh\nexec sleep 60\n").unwrap();
        fs::set_permissions(&script, fs::Permissions::from_mode(0o700)).unwrap();
        let id = "11111111-1111-4111-8111-111111111111";
        let spawn = |home: &Path, flag: &str, id: &str| {
            std::process::Command::new(&script)
                .args([flag, id])
                .env("GEMINI_CLI_HOME", home)
                .spawn()
                .unwrap()
        };
        let mut child = spawn(&home, "--session-id", id);
        let mut stranger = spawn(&other, "-r", "22222222-2222-4222-8222-222222222222");
        let pid = child.id();
        // `exec sleep` replaces the script: wait for the script's own command line.
        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(5);
        let start = loop {
            if let Some((_, start)) = proc_start(pid)
                && arguments(pid).is_some_and(|a| is_gemini_arguments(&a))
            {
                break start;
            }
            assert!(std::time::Instant::now() < deadline, "script did not start");
            std::thread::sleep(std::time::Duration::from_millis(20));
        };
        let live = live_sessions(&home).unwrap();
        assert_eq!(live.len(), 1, "{live:?}");
        assert_eq!((live[0].pid, live[0].session_id.as_str()), (pid, id));
        assert!(
            live_sessions(&other)
                .unwrap()
                .iter()
                .all(|l| l.session_id != id),
            "a process of another home never holds this account's session"
        );
        assert!(env_in_use(&[home.as_path()]) && !env_in_use(&[d.path()]));
        let attachment = |pid, process_start| catalog::Attachment {
            host: "herdr".into(),
            target: "herdr/test/w1:p1".into(),
            pid,
            process_start,
        };
        let session = |id: &str, a: catalog::Attachment| catalog::Session {
            id: "catalog-id".into(),
            name: "n".into(),
            provider: "gemini".into(),
            account: "a".into(),
            conversation_id: id.into(),
            revision: 1,
            attachment: Some(a),
            directory: None,
            previous_conversation_ids: vec![],
        };
        assert!(launch_observed(&session(id, attachment(pid, start))));
        assert!(
            !launch_observed(&session(id, attachment(pid, start + 1))),
            "a recycled PID with another start time is not the session"
        );
        assert!(
            !launch_observed(&session(
                "33333333-3333-4333-8333-333333333333",
                attachment(pid, start)
            )),
            "the process runs another native session"
        );
        let _ = child.kill();
        let _ = child.wait();
        let _ = stranger.kill();
        let _ = stranger.wait();
        assert!(!launch_observed(&session(id, attachment(pid, start))));
        assert!(live_sessions(&home).unwrap().is_empty());
    }
}
