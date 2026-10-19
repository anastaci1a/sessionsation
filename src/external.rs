//! Account-aware Codex lifecycle, independent of terminal placement.
pub(crate) use crate::terminal::{shell_quote, tmux};
use crate::{
    daemon,
    paths::{Paths, atomic_write, private_dir, validate_name},
};
use anyhow::{Context, Result, bail, ensure};
use fs2::FileExt;
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use std::{
    fs::{self, File, OpenOptions},
    io::{BufRead, BufReader, Read, Seek, SeekFrom},
    os::fd::{AsRawFd, FromRawFd, OwnedFd},
    os::unix::{ffi::OsStrExt, fs::OpenOptionsExt, process::CommandExt},
    path::{Path, PathBuf},
    process::{Command, Stdio},
    time::Duration,
};

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct Instance {
    /// Presentation-only tmux row; never accepted by Codex lifecycle workers.
    #[serde(default)]
    pub tmux_only: bool,
    #[serde(default)]
    pub tmux_dead: bool,
    #[serde(default)]
    pub display_name: String,
    /// Unix milliseconds; explicit opens, with tmux attachment time as migration fallback.
    #[serde(default)]
    pub last_accessed: Option<i64>,
    #[serde(default)]
    pub archived: bool,
    #[serde(default)]
    pub inactive: bool,
    #[serde(default)]
    pub account: String,
    /// Signed-in enrolled nickname, independent of CODEX_HOME provenance.
    #[serde(default)]
    pub display_account: Option<String>,
    #[serde(default)]
    pub block_reason: Option<String>,
    #[serde(default)]
    pub activity: Activity,
    #[serde(default)]
    pub detail: String,
    #[serde(default)]
    pub warnings: Vec<String>,
    #[serde(default)]
    pub shared_server: bool,
    #[serde(default)]
    pub move_id: Option<String>,
    pub pane: String,
    pub tmux_session: String,
    pub pid: u32,
    #[serde(default)]
    pub start_time: u64,
    pub cwd: String,
    pub thread_id: String,
    pub rollout: PathBuf,
    pub home: PathBuf,
    pub binary: String,
    pub resume_options: Vec<String>,
}
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Activity {
    Idle,
    Busy,
    #[default]
    Unknown,
    Inactive,
}
impl Instance {
    pub fn needs_prefix(&self) -> bool {
        !self.tmux_only
            && !self.inactive
            && !self.pane.starts_with("herdr/")
            && !self.tmux_session.starts_with("codex-")
    }
    pub fn key(&self) -> String {
        if self.tmux_only {
            return format!("tmux:{}", self.pane);
        }
        if self.archived {
            use sha2::{Digest, Sha256};
            let home = format!(
                "{:x}",
                Sha256::digest(self.home.as_os_str().as_encoded_bytes())
            );
            return format!("archive:{}:{}", &home[..16], self.thread_id);
        }
        if self.inactive {
            format!("thread:{}", self.thread_id)
        } else {
            format!("pane:{}", self.pane)
        }
    }
}
pub fn activity_detail(activity: Activity) -> &'static str {
    match activity {
        Activity::Idle => "Codex is idle. Select an account to move this conversation.",
        Activity::Busy => {
            "Codex is working. Moving requires sending a pause request, then waits for the turn and background work to stop."
        }
        Activity::Unknown => {
            "Checking activity. You can queue a move; the source stays running until its state is verified."
        }
        Activity::Inactive => "Saved conversation. Moving opens it in a new tmux session.",
    }
}
pub fn reason_detail(reason: Option<&str>) -> &'static str {
    match reason {
        Some("unknown") => {
            "The saved working directory is missing. Restore that directory before resuming this conversation."
        }
        Some("busy") => {
            "Another process still has this history open. The move waits for that writer to release it."
        }
        Some("unfinished") => {
            "The saved turn did not finish normally. You can resume its recorded history after acknowledging that pending work may need review."
        }
        Some("diverged") => {
            "The accounts contain different histories for this conversation. The switcher will preserve both copies rather than overwrite newer work."
        }
        Some("moving") => {
            "A move is already queued for this conversation. Select it to see its progress."
        }
        _ => "Saved conversation. Moving opens it in a new tmux session.",
    }
}
fn rollout_activity(path: &Path) -> Activity {
    let Ok(file) = File::open(path) else {
        return Activity::Unknown;
    };
    activity_from_tail(file)
}
const ACTIVITY_TAIL_BYTES: u64 = 64 * 1024;
fn activity_from_tail(mut reader: impl Read + Seek) -> Activity {
    let Ok(length) = reader.seek(SeekFrom::End(0)) else {
        return Activity::Unknown;
    };
    let start = length.saturating_sub(ACTIVITY_TAIL_BYTES);
    if reader.seek(SeekFrom::Start(start)).is_err() {
        return Activity::Unknown;
    }
    let mut bytes = Vec::new();
    if reader
        .take(ACTIVITY_TAIL_BYTES)
        .read_to_end(&mut bytes)
        .is_err()
    {
        return Activity::Unknown;
    }
    // The window may start inside JSON or a UTF-8 sequence. Discard that first
    // partial record. If no activity event remains, stay unknown rather than
    // trusting an old idle event outside the bounded window.
    let tail = if start > 0 {
        let Some(end) = bytes.iter().position(|b| *b == b'\n') else {
            return Activity::Unknown;
        };
        &bytes[end + 1..]
    } else {
        &bytes[..]
    };
    let mut activity = Activity::Unknown;
    for line in BufReader::new(tail).lines() {
        let Ok(value) = line
            .map_err(anyhow::Error::from)
            .and_then(|line| Ok(serde_json::from_str::<Value>(&line)?))
        else {
            activity = Activity::Unknown;
            continue;
        };
        if value["type"] == "event_msg" {
            match value["payload"]["type"].as_str() {
                Some("task_started") => activity = Activity::Busy,
                Some("task_complete" | "turn_aborted") => activity = Activity::Idle,
                _ => {}
            }
        }
    }
    activity
}

/// Add live server state without starting another app-server or loading threads.
pub async fn discover(paths: &Paths) -> Result<Vec<Instance>> {
    discover_filtered(paths, true).await
}
pub async fn live_sessions(paths: &Paths) -> Result<Vec<Instance>> {
    discover_filtered(paths, false).await
}
async fn discover_filtered(paths: &Paths, include_inactive: bool) -> Result<Vec<Instance>> {
    let mut result = match inventory(paths, include_inactive) {
        Ok(items) => items,
        Err(error) => {
            let mut live = instances()?;
            let state = crate::store::load_state_snapshot(paths).ok();
            for i in &mut live {
                i.account = account_for_home(paths, &i.home);
                i.display_account =
                    display_account_for_home(paths, &i.home, &i.account, state.as_ref());
                i.warnings
                    .push(format!("Saved-history discovery is incomplete: {error}"));
            }
            live
        }
    };
    let homes: std::collections::BTreeSet<_> = result
        .iter()
        .filter(|i| !i.inactive)
        .map(|i| i.home.clone())
        .collect();
    for home in homes {
        let Ok(mut control) = crate::control::Control::connect(&home).await else {
            continue;
        };
        let Ok(threads) = control.loaded().await else {
            continue;
        };
        for index in 0..result.len() {
            if result[index].inactive || result[index].home != home {
                continue;
            }
            let source = &result[index];
            let exact = threads
                .iter()
                .find(|t| t["id"].as_str() == Some(source.thread_id.as_str()));
            let matched = if exact.is_some() {
                exact
            } else if source.thread_id.is_empty() {
                let same_panes = result
                    .iter()
                    .filter(|p| !p.inactive && p.home == home && p.cwd == source.cwd)
                    .count();
                let matches: Vec<_> = threads
                    .iter()
                    .filter(|t| {
                        t["cwd"].as_str() == Some(source.cwd.as_str())
                            && t["parentThreadId"].is_null()
                    })
                    .collect();
                if same_panes == 1 && matches.len() == 1 {
                    Some(matches[0])
                } else {
                    None
                }
            } else {
                None
            };
            let Some(thread) = matched else { continue };
            let Some(id) = thread["id"]
                .as_str()
                .filter(|id| uuid::Uuid::parse_str(id).is_ok())
            else {
                continue;
            };
            let path = thread["path"]
                .as_str()
                .map(PathBuf::from)
                .or_else(|| rollout_by_thread(&home, id).ok().flatten());
            let Some(path) = path.filter(|p| p.starts_with(home.join("sessions"))) else {
                continue;
            };
            let source = &mut result[index];
            if source.thread_id.is_empty() {
                source.warnings.push("The server conversation was matched by its unique working directory. Check the conversation ID and directory before confirming the move.".into());
            }
            source.thread_id = id.into();
            source.rollout = path;
            source.shared_server = true;
            if let Some(cwd) = thread["cwd"].as_str() {
                source.cwd = cwd.into();
            }
            let state = if thread["status"]["type"] == "systemError" {
                control
                    .thread(id, true)
                    .await
                    .unwrap_or_else(|_| thread.clone())
            } else {
                thread.clone()
            };
            source.activity = match state["status"]["type"].as_str() {
                Some("idle") => Activity::Idle,
                Some("active") => Activity::Busy,
                Some("systemError") if crate::control::confirmed_quiescent(&state, false) => {
                    Activity::Idle
                }
                _ => Activity::Unknown,
            };
            if source.move_id.is_none() {
                source.detail = if state["status"]["type"] == "systemError"
                    && source.activity == Activity::Idle
                {
                    "Codex's last turn failed and ended. Moving is available; no more work will be sent to this account."
                } else {
                    activity_detail(source.activity)
                }
                .into();
            }
        }
    }
    let live: std::collections::BTreeSet<_> = result
        .iter()
        .filter(|i| !i.inactive)
        .map(|i| i.thread_id.clone())
        .collect();
    result.retain(|i| !i.inactive || !live.contains(&i.thread_id));
    catalog::decorate(paths, &mut result)?;
    if include_inactive {
        for pending in deletion::pending(paths)? {
            if !result.iter().any(|s| s.thread_id == pending.thread_id) {
                result.push(pending);
            }
        }
    }
    access::sort(paths, &mut result)?;
    Ok(result)
}

/// Versioned menu-facing snapshot. Discovery never starts a Codex turn.
pub async fn sessions_snapshot(paths: &Paths, include_inactive: bool) -> Result<Value> {
    Ok(snapshot(discover_filtered(paths, include_inactive).await?))
}
fn snapshot(items: Vec<Instance>) -> Value {
    let sessions = items
        .into_iter()
        .map(|i| {
            let mut value = serde_json::to_value(&i).expect("serializable instance");
            value["id"] = json!(i.key());
            value["kind"] = json!(if i.tmux_only { "tmux" } else { "codex" });
            value["needs_prefix"] = json!(i.needs_prefix());
            value["name"] = json!(catalog::label(&i));
            value["lifecycle"] = json!(if i.tmux_dead {
                "exited"
            } else if i.inactive {
                "inactive"
            } else {
                "live"
            });
            value["selectable"] = json!(true);
            value
        })
        .collect::<Vec<_>>();
    json!({"schema_version":1,"observed_at":daemon::now(),"sessions":sessions})
}
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct Job {
    /// Explicit new HERDR terminal for a saved-session transfer. Older jobs have none.
    #[serde(default)]
    pub resume_destination: Option<String>,
    #[serde(default)]
    pub non_steering: bool,
    pub id: String,
    #[serde(default = "default_job_kind")]
    pub kind: String,
    pub source: Instance,
    pub target: String,
    pub phase: String,
    pub detail: String,
    #[serde(default)]
    pub source_lease_released: bool,
    #[serde(default)]
    pub pause_approved: bool,
    #[serde(default)]
    pub risks_approved: bool,
    #[serde(default)]
    pub unfinished_history_confirmed: bool,
    #[serde(default)]
    pub pause_delivery: String,
    #[serde(default)]
    pub resume_after_move: bool,
    /// Written before delivery; an uncertain request must never be replayed.
    #[serde(default)]
    pub continuation_delivery: String,
    #[serde(default)]
    pub continuation_turn_id: Option<String>,
    /// Public summary evidence only; acceptance is not execution.
    #[serde(default)]
    pub continuation_observation: String,
    #[serde(default)]
    pub source_archived: bool,
    #[serde(default)]
    pub created_at: i64,
    #[serde(default)]
    pub confirmation_seq: u64,
    #[serde(default)]
    pub acknowledged_risks: Vec<String>,
    #[serde(default)]
    pub target_loaded: bool,
    #[serde(default)]
    pub steered_turns: std::collections::BTreeMap<String, String>,
    #[serde(default)]
    pub relocation: Option<relocation::Relocation>,
    /// Saved before unloading, so recovery can unarchive every member without reopening it.
    #[serde(default)]
    pub termination_threads: Vec<String>,
    /// Content hashes only; private conversation text never enters the job journal.
    #[serde(default)]
    pub history_checks: Vec<history::HistoryCheck>,
    #[serde(default)]
    pub prepared_threads: Vec<String>,
}
fn default_job_kind() -> String {
    "account".into()
}

#[derive(Clone, Copy, Default)]
pub struct MoveOptions {
    pub pause: bool,
    pub accept_risks: bool,
}

#[derive(Serialize, Deserialize)]
struct Binding {
    pid: u32,
    start_time: u64,
    home: PathBuf,
    thread: String,
}
fn binding_path(paths: &Paths, pane: &str) -> Result<PathBuf> {
    let name = pane.strip_prefix('%').context("invalid tmux pane ID")?;
    ensure!(
        name.bytes().all(|b| b.is_ascii_digit()) && !name.is_empty(),
        "invalid tmux pane ID"
    );
    let dir = paths.root.join("managed-panes");
    private_dir(&dir)?;
    Ok(dir.join(format!("{name}.json")))
}
fn bind_pane(paths: &Paths, pane: &str, pid: u32, home: &Path, thread: &str) -> Result<()> {
    let binding = Binding {
        pid,
        start_time: process_info(pid)?.start_time,
        home: home.to_path_buf(),
        thread: thread.into(),
    };
    if pane.starts_with("herdr/") {
        return hosting::bind(paths, pane, binding);
    }
    atomic_write(&binding_path(paths, pane)?, &serde_json::to_vec(&binding)?)?;
    let name = tmux(&["display-message", "-p", "-t", pane, "#{session_name}"])?;
    catalog::remember_name(paths, thread, &name)
}

fn rollout_for(pid: u32) -> Result<Option<(String, PathBuf, PathBuf)>> {
    let mut found = Vec::new();
    for entry in fs::read_dir(format!("/proc/{pid}/fd"))? {
        let entry = entry?;
        let Ok(path) = fs::read_link(entry.path()) else {
            continue;
        };
        let Some(parent) = path.parent() else {
            continue;
        };
        if !path
            .file_name()
            .and_then(|n| n.to_str())
            .is_some_and(|n| n.starts_with("rollout-") && n.ends_with(".jsonl"))
        {
            continue;
        }
        let Some(sessions) = path
            .ancestors()
            .find(|p| p.file_name().is_some_and(|n| n == "sessions"))
        else {
            continue;
        };
        ensure!(parent.starts_with(sessions), "rollout outside sessions");
        let home = sessions
            .parent()
            .context("invalid Codex home")?
            .to_path_buf();
        let meta = session_meta(&path)?;
        let id = meta["id"]
            .as_str()
            .context("rollout has no thread ID")?
            .to_owned();
        if !meta["source"].is_object() {
            found.push((id, path, home));
        }
    }
    found.sort();
    found.dedup();
    Ok((found.len() == 1).then(|| found.remove(0)))
}
fn process_home(pid: u32) -> Result<PathBuf> {
    let raw = fs::read(format!("/proc/{pid}/environ"))?;
    let vars = raw.split(|b| *b == 0);
    let home = vars.clone().find_map(|v| v.strip_prefix(b"CODEX_HOME="));
    let path = match home {
        Some(value) => PathBuf::from(std::ffi::OsStr::from_bytes(value)),
        None => {
            let user_home = vars
                .clone()
                .find_map(|v| v.strip_prefix(b"HOME="))
                .context("Codex process has no home")?;
            PathBuf::from(std::ffi::OsStr::from_bytes(user_home)).join(".codex")
        }
    };
    ensure!(path.is_absolute(), "Codex home is not absolute");
    Ok(fs::canonicalize(path)?)
}
fn resumed_thread(pid: u32) -> Result<Option<String>> {
    Ok(parse_launch(&process_args(pid)?).thread)
}
fn rollout_by_thread(home: &Path, thread: &str) -> Result<Option<PathBuf>> {
    let mut files = Vec::new();
    collect_rollouts(&home.join("sessions"), &mut files)?;
    let mut matches = files.into_iter().filter(|path| {
        path.file_name()
            .is_some_and(|name| name.to_string_lossy().contains(thread))
            && session_meta(path).is_ok_and(|m| m["id"] == thread)
    });
    let first = matches.next();
    ensure!(
        matches.next().is_none(),
        "ambiguous rollout for resumed thread"
    );
    Ok(first)
}
fn process_args(pid: u32) -> Result<Vec<String>> {
    let raw = fs::read(format!("/proc/{pid}/cmdline"))?;
    let args: Vec<String> = raw
        .split(|b| *b == 0)
        .filter(|b| !b.is_empty())
        .map(|b| String::from_utf8(b.to_vec()))
        .collect::<std::result::Result<_, _>>()?;
    Ok(args)
}
#[derive(Default)]
struct Launch {
    binary: String,
    options: Vec<String>,
    thread: Option<String>,
    omitted: bool,
    cwd: Option<String>,
    prompt: Vec<String>,
}
fn parse_launch(args: &[String]) -> Launch {
    let mut launch = Launch {
        binary: args.first().cloned().unwrap_or_else(|| "codex".into()),
        ..Launch::default()
    };
    let mut options = Vec::new();
    let mut i = 1;
    let mut resume = false;
    while i < args.len() {
        let (flag, inline) = args[i]
            .split_once('=')
            .map_or((args[i].as_str(), None), |(a, b)| (a, Some(b)));
        if flag == "resume" {
            resume = true;
            i += 1;
            continue;
        }
        if resume && uuid::Uuid::parse_str(flag).is_ok() {
            launch.thread = Some(flag.into());
            i += 1;
            continue;
        }
        if matches!(flag, "--last" | "--all" | "--include-non-interactive") {
            i += 1;
            continue;
        }
        if flag == "--remote" {
            i += if inline.is_some() { 1 } else { 2 };
            continue;
        }
        if [
            "--dangerously-bypass-approvals-and-sandbox",
            "--dangerously-bypass-hook-trust",
            "--oss",
            "--full-auto",
            "--approve-for-me",
            "--no-alt-screen",
            "--no-daemon",
            "--search",
        ]
        .contains(&flag)
        {
            options.push(flag.to_owned());
            i += 1;
        } else if [
            "-m",
            "--model",
            "-s",
            "--sandbox",
            "-a",
            "--ask-for-approval",
            "-p",
            "--profile",
            "-c",
            "--config",
            "--enable",
            "--disable",
            "--add-dir",
            "--local-provider",
            "-C",
            "--cd",
        ]
        .contains(&flag)
        {
            let value = inline.or_else(|| args.get(i + 1).map(String::as_str));
            if let Some(value) = value {
                if matches!(flag, "-C" | "--cd") {
                    launch.cwd = Some(value.to_owned());
                } else {
                    options.extend([flag.to_owned(), value.to_owned()]);
                }
            } else {
                launch.omitted = true;
            }
            i += if inline.is_some() { 1 } else { 2 };
        } else {
            // Initial prompts are already in the history. Unknown switches need
            // acknowledgement, but must not hide a live session from the chooser.
            launch.omitted |= flag.starts_with('-');
            if !flag.starts_with('-') {
                launch.prompt.push(args[i].clone());
            }
            i += 1;
        }
    }
    launch.options = options;
    launch
}
/// Read-only native file facts; never substitute mtime for conversation creation.
pub(crate) fn history_file_details(path: &Path) -> Value {
    let meta = session_meta(path).ok();
    json!({"conversation_created_at":meta.as_ref().and_then(|m|m["timestamp"].as_str()),
        "history_file_bytes":fs::metadata(path).ok().filter(|m|m.is_file()).map(|m|m.len())})
}
fn session_meta(path: &Path) -> Result<Value> {
    let file = File::open(path)?;
    let mut line = String::new();
    BufReader::new(file).read_line(&mut line)?;
    let value: Value = serde_json::from_str(&line)?;
    ensure!(value["type"] == "session_meta", "rollout metadata missing");
    Ok(value["payload"].clone())
}
pub fn instances() -> Result<Vec<Instance>> {
    let text = tmux(&[
        "list-panes",
        "-a",
        "-F",
        "#{session_name}\t#{pane_id}\t#{pane_pid}\t#{pane_current_command}\t#{pane_current_path}\t#{pane_dead}",
    ])?;
    let mut result = Vec::new();
    for row in text.lines() {
        let fields: Vec<_> = row.split('\t').collect();
        if fields.len() != 6 || fields[3] != "codex" || fields[5] != "0" {
            continue;
        }
        let Ok(pane_pid) = fields[2].parse::<u32>() else {
            continue;
        };
        let is_codex = |pid| {
            process_args(pid).is_ok_and(|args| {
                args.first()
                    .is_some_and(|a| Path::new(a).file_name().is_some_and(|n| n == "codex"))
                    && !args.iter().any(|a| a == "app-server")
            })
        };
        let pid = if is_codex(pane_pid) {
            pane_pid
        } else {
            child_processes(pane_pid)
                .unwrap_or_default()
                .into_iter()
                .find(|p| is_codex(p.pid))
                .map_or(pane_pid, |p| p.pid)
        };
        let Ok(info) = process_info(pid) else {
            continue;
        };
        let mut reason: Option<String> = None;
        let home = process_home(pid).unwrap_or_default();
        let discovered = rollout_for(pid).ok().flatten().or_else(|| {
            let thread = resumed_thread(pid).ok().flatten()?;
            let rollout = rollout_by_thread(&home, &thread).ok().flatten()?;
            Some((thread, rollout, home.clone()))
        });
        let (mut thread_id, rollout, home) = match discovered {
            Some(found) => found,
            None => {
                reason = Some("unknown".into());
                (String::new(), PathBuf::new(), home)
            }
        };
        let launch = process_args(pid)
            .map(|args| parse_launch(&args))
            .unwrap_or_default();
        if thread_id.is_empty() {
            thread_id = launch.thread.clone().unwrap_or_default();
        }
        let mut warnings = Vec::new();
        if pid != pane_pid {
            warnings.push("Codex runs inside a shell wrapper. After Codex stops, that wrapper will be replaced to resume the conversation in the same pane.".into());
        }
        if launch.omitted {
            warnings.push("Some original launch switches are not supported on resume and will be omitted. Recorded permissions and conversation history are retained.".into());
        }
        let activity = rollout_activity(&rollout);
        let mut cwd = fs::read_link(format!("/proc/{pid}/cwd"))
            .map(|p| p.to_string_lossy().into_owned())
            .unwrap_or_else(|_| fields[4].to_owned());
        if let Some(dir) = &launch.cwd {
            cwd = fs::canonicalize(Path::new(&cwd).join(dir))
                .map(|p| p.to_string_lossy().into_owned())
                .unwrap_or_else(|_| dir.clone());
        }
        result.push(Instance {
            tmux_only: false,
            tmux_dead: false,
            display_name: String::new(),
            last_accessed: None,
            archived: false,
            inactive: false,
            account: String::new(),
            display_account: None,
            block_reason: None,
            activity,
            detail: if reason.is_some() { "Identifying the conversation in this pane. You can queue a move while identification completes.".into() } else { activity_detail(activity).into() },
            warnings,
            shared_server: false,
            move_id: None,
            pane: fields[1].into(),
            tmux_session: fields[0].into(),
            pid,
            start_time: info.start_time,
            cwd,
            thread_id,
            rollout,
            home,
            binary: launch.binary,
            resume_options: launch.options,
        });
    }
    result.sort_by(|a, b| {
        a.tmux_session
            .cmp(&b.tmux_session)
            .then(a.pane.cmp(&b.pane))
    });
    Ok(result)
}
pub fn host_instances(paths: &Paths) -> Result<Vec<Instance>> {
    let mut result = instances()?;
    result.extend(hosting::instances(paths)?);
    Ok(result)
}
pub(crate) fn home_has_processes(home: &Path, clients_only: bool) -> Result<bool> {
    for e in fs::read_dir("/proc")? {
        let e = e?;
        let Ok(pid) = e.file_name().to_string_lossy().parse::<u32>() else {
            continue;
        };
        if clients_only
            && process_args(pid).is_ok_and(|args| args.iter().any(|a| a == "app-server"))
        {
            continue;
        }
        if process_info(pid).is_ok_and(|i| !matches!(i.state, 'Z' | 'X'))
            && process_home(pid).is_ok_and(|h| h == home)
        {
            return Ok(true);
        }
    }
    Ok(false)
}
pub fn all_instances(paths: &Paths) -> Result<Vec<Instance>> {
    inventory(paths, true)
}
fn inventory(paths: &Paths, include_inactive: bool) -> Result<Vec<Instance>> {
    inventory_kind(paths, include_inactive, false)
}
fn inventory_kind(paths: &Paths, include_inactive: bool, archived: bool) -> Result<Vec<Instance>> {
    let mut result = host_instances(paths)?;
    let state = crate::store::load_state_snapshot(paths).ok();
    for instance in &mut result {
        if instance.thread_id.is_empty() {
            if let Ok(binding) = binding_path(paths, &instance.pane)
                .and_then(|p| Ok(serde_json::from_slice::<Binding>(&fs::read(p)?)?))
            {
                if binding.pid == instance.pid
                    && binding.start_time == instance.start_time
                    && binding.home == instance.home
                {
                    instance.thread_id = binding.thread.clone();
                    if let Some(path) = rollout_by_thread(&instance.home, &binding.thread)? {
                        instance.rollout = path;
                    }
                }
            }
        }
        instance.account = account_for_home(paths, &instance.home);
        instance.display_account =
            display_account_for_home(paths, &instance.home, &instance.account, state.as_ref());
    }
    if !include_inactive {
        let queued = jobs(paths)?;
        for instance in &mut result {
            if let Some(job) = queued
                .iter()
                .find(|j| job_conflicts_with_source(paths, j, instance))
            {
                instance.move_id = Some(job.id.clone());
                instance.detail = job.detail.clone();
            }
        }
        return Ok(result);
    }
    let open_histories = open_rollouts()?;
    let live: std::collections::BTreeSet<_> = result.iter().map(|i| i.thread_id.clone()).collect();
    let config = crate::store::load_config(paths)?;
    let accounts = paths.root.join("accounts");
    let mut homes: std::collections::BTreeSet<PathBuf> =
        result.iter().map(|i| i.home.clone()).collect();
    if let Some(home) = std::env::var_os("CODEX_HOME")
        .map(PathBuf::from)
        .or_else(|| std::env::var_os("HOME").map(|h| PathBuf::from(h).join(".codex")))
    {
        if let Ok(home) = fs::canonicalize(home) {
            homes.insert(home);
        }
    }
    for job in jobs(paths)? {
        homes.insert(job.source.home);
    }
    if accounts.exists() {
        for entry in fs::read_dir(accounts)? {
            let entry = entry?;
            if entry.file_type()?.is_dir() && !entry.file_type()?.is_symlink() {
                homes.insert(fs::canonicalize(entry.path())?);
            }
        }
    }
    let preserved = preserved_homes(paths)?;
    homes.extend(preserved.iter().map(|(_, h)| h.clone()));
    if archived {
        result.clear();
    }
    let live_count = result.len();
    for home in homes {
        let account = account_for_home(paths, &home);
        let display_account = display_account_for_home(paths, &home, &account, state.as_ref());
        let mut files = Vec::new();
        collect_rollouts(
            &home.join(if archived {
                "archived_sessions"
            } else {
                "sessions"
            }),
            &mut files,
        )?;
        if !archived && preserved.iter().any(|(_, h)| *h == home) {
            collect_rollouts(&home.join("archived_sessions"), &mut files)?;
        }
        for rollout in files {
            let Ok(meta) = session_meta(&rollout) else {
                continue;
            };
            let Some(thread_id) = meta["id"].as_str() else {
                continue;
            };
            if uuid::Uuid::parse_str(thread_id).is_err() {
                continue;
            }
            if meta["source"].is_object() || (!archived && live.contains(thread_id)) {
                continue;
            }
            let relocated = relocation::saved_location(paths, thread_id);
            let cwd = relocated
                .as_deref()
                .unwrap_or_else(|| meta["cwd"].as_str().unwrap_or_default());
            let block_reason = if !Path::new(cwd).is_dir() {
                Some("unknown".into())
            } else if open_histories.contains(&rollout) {
                Some("busy".into())
            } else if !complete_rollout(&rollout).unwrap_or(false) {
                Some("unfinished".into())
            } else {
                None
            };
            result.push(Instance {
                tmux_only: false,
                tmux_dead: false,
                display_name: String::new(),
                last_accessed: None,
                archived,
                inactive: true,
                account: account.clone(),
                display_account: display_account.clone(),
                activity: Activity::Inactive,
                detail: reason_detail(block_reason.as_deref()).into(),
                warnings: Vec::new(),
                shared_server: false,
                move_id: None,
                block_reason,
                pane: String::new(),
                tmux_session: String::new(),
                pid: 0,
                start_time: 0,
                cwd: cwd.into(),
                thread_id: thread_id.into(),
                rollout,
                home: home.clone(),
                binary: config.codex_binary.clone(),
                resume_options: Vec::new(),
            });
        }
    }
    let inactive = result.split_off(live_count);
    let mut latest: std::collections::BTreeMap<String, Instance> =
        std::collections::BTreeMap::new();
    for candidate in inactive {
        if !catalog::current_copy(paths, &candidate)? {
            continue;
        }
        let key = if archived {
            candidate.key()
        } else {
            candidate.thread_id.clone()
        };
        match latest.entry(key) {
            std::collections::btree_map::Entry::Vacant(entry) => {
                entry.insert(candidate);
            }
            std::collections::btree_map::Entry::Occupied(mut entry) => {
                let previous = entry.get();
                let old = fs::read(&previous.rollout)?;
                let new = fs::read(&candidate.rollout)?;
                let newer_copy = new == old
                    && fs::metadata(&candidate.rollout)?.modified()?
                        > fs::metadata(&previous.rollout)?.modified()?;
                if (new.starts_with(&old) && new.len() > old.len()) || newer_copy {
                    let mut replacement = candidate;
                    if previous.block_reason.as_deref() == Some("diverged") {
                        replacement.block_reason = Some("diverged".into());
                    }
                    entry.insert(replacement);
                } else if !old.starts_with(&new) {
                    entry.get_mut().block_reason = Some("diverged".into());
                }
            }
        }
    }
    result.extend(latest.into_values());
    result.sort_by(|a, b| {
        a.inactive
            .cmp(&b.inactive)
            .then(a.tmux_session.cmp(&b.tmux_session))
            .then(a.thread_id.cmp(&b.thread_id))
    });
    let queued = jobs(paths)?;
    for instance in &mut result {
        if let Some(job) = queued
            .iter()
            .find(|job| job_conflicts_with_source(paths, job, instance))
        {
            instance.move_id = Some(job.id.clone());
            instance.detail = job.detail.clone();
            if instance.inactive {
                instance.block_reason = Some("moving".into());
            }
        } else if instance.inactive {
            instance.detail = if archived {
                "Archived conversation retained on its original home. Open restores it without sending a prompt. If a newer copy is live, open that copy instead.".into()
            } else {
                reason_detail(instance.block_reason.as_deref()).into()
            };
        }
    }
    Ok(result)
}
fn preserved_homes(paths: &Paths) -> Result<Vec<(String, PathBuf)>> {
    let mut homes = crate::account_deletion::preserved_homes(paths)?;
    homes.extend(archiving::restored_homes(paths)?);
    Ok(homes)
}
fn account_for_home(paths: &Paths, home: &Path) -> String {
    let Ok(home) = fs::canonicalize(home) else {
        return "unknown".into();
    };
    if let Ok(homes) = preserved_homes(paths)
        && let Some((name, _)) = homes.into_iter().find(|(_, h)| *h == home)
    {
        return name;
    }
    let Ok(accounts) = fs::read_dir(paths.root.join("accounts")) else {
        return "external".into();
    };
    for entry in accounts.flatten() {
        if fs::canonicalize(entry.path()).is_ok_and(|candidate| candidate == home) {
            return entry.file_name().to_string_lossy().into_owned();
        }
    }
    "external".into()
}
fn display_account_for_home(
    paths: &Paths,
    home: &Path,
    account: &str,
    state: Option<&crate::model::State>,
) -> Option<String> {
    if account != "external" && account != "unknown" {
        return Some(account.to_owned());
    }
    if account != "external" {
        return None;
    }
    let identity = crate::identity::read_identity(home).ok()?;
    let mut matches = state?.accounts.values().filter(|candidate| {
        !candidate.identity.is_empty()
            && candidate.identity == identity
            && paths
                .account(&candidate.name)
                .ok()
                .and_then(|enrolled_home| crate::identity::read_identity(&enrolled_home).ok())
                .is_some_and(|current| current == identity)
    });
    let nickname = matches.next()?.name.clone();
    matches.next().is_none().then_some(nickname)
}
fn collect_rollouts(dir: &Path, files: &mut Vec<PathBuf>) -> Result<()> {
    if !dir.exists() {
        return Ok(());
    }
    for entry in fs::read_dir(dir)? {
        let entry = entry?;
        let ty = entry.file_type()?;
        ensure!(!ty.is_symlink(), "symlink in rollout tree");
        if ty.is_dir() {
            collect_rollouts(&entry.path(), files)?;
        } else if entry.path().extension().is_some_and(|x| x == "jsonl") {
            files.push(entry.path());
        }
    }
    Ok(())
}
pub async fn run_account(paths: &Paths, account: &str, options: &[String]) -> Result<()> {
    run_account_thread(paths, account, None, options).await
}
pub async fn account_control(paths: &Paths, account: &str) -> Result<crate::control::Control> {
    crate::accounts::ensure_available(paths, account)?;
    for _ in 0..50 {
        match daemon::call(paths, "account/prepare", json!({"name":account})).await {
            Ok(_) => return crate::control::Control::connect(&paths.account(account)?).await,
            Err(error) if error.to_string().contains("poll in progress") => {
                tokio::time::sleep(Duration::from_millis(200)).await
            }
            Err(error) => return Err(error),
        }
    }
    bail!("account metadata poll is still finishing; retry shortly")
}
pub async fn run_account_thread(
    paths: &Paths,
    account: &str,
    thread: Option<&str>,
    options: &[String],
) -> Result<()> {
    run_account_thread_guarded(paths, account, thread, options, None).await
}
pub async fn run_account_thread_guarded(
    paths: &Paths,
    account: &str,
    thread: Option<&str>,
    options: &[String],
    expected: Option<(&str, &str)>,
) -> Result<()> {
    run_account_thread_inner(paths, account, thread, options, expected, None, None).await
}
pub async fn run_account_new_named(
    paths: &Paths,
    account: &str,
    request: &str,
    name: &str,
) -> Result<()> {
    run_account_thread_inner(paths, account, None, &[], None, Some((request, name)), None).await
}
pub(crate) async fn run_account_resume_requested(
    paths: &Paths,
    account: &str,
    thread: &str,
    expected: (&str, &str),
    request: &str,
) -> Result<()> {
    run_account_thread_inner(
        paths,
        account,
        Some(thread),
        &[],
        Some(expected),
        None,
        Some(request),
    )
    .await
}
async fn run_account_thread_inner(
    paths: &Paths,
    account: &str,
    thread: Option<&str>,
    options: &[String],
    expected: Option<(&str, &str)>,
    creation: Option<(&str, &str)>,
    resumption: Option<&str>,
) -> Result<()> {
    validate_name(account)?;
    let _account_operation = crate::accounts::launch_lock(paths, account)?;
    crate::accounts::ensure_available(paths, account)?;
    daemon::ensure_started(paths).await?;
    let status = daemon::call(paths, "status", json!({})).await?;
    ensure!(
        status["accounts"].as_array().is_some_and(|a| a
            .iter()
            .any(|a| a["name"] == account && a["enabled"] == true)),
        "account is not enrolled or is disabled"
    );
    let home = paths.account(account)?;
    let config = crate::store::load_config(paths)?;
    let mut cwd = std::env::current_dir()?;
    let mut args = vec![config.codex_binary.clone()];
    args.extend_from_slice(options);
    let launch = parse_launch(&args);
    ensure!(
        !launch.omitted,
        "unsupported launch option; use run --account NAME -- [Codex options] or --thread UUID for an exact resume"
    );
    if let Some(dir) = &launch.cwd {
        cwd = fs::canonicalize(cwd.join(dir)).context("working directory is unavailable")?;
    }
    ensure!(cwd.is_dir(), "working directory is not a directory");
    let requested = thread.or(launch.thread.as_deref());
    let _conversation_lock = if let Some(id) = requested {
        let lock = catalog::operation_lock(paths, id)?;
        catalog::ensure_available(paths, id)?;
        Some(lock)
    } else {
        None
    };
    if let Some(id) = requested {
        ensure!(
            uuid::Uuid::parse_str(id).is_ok(),
            "--thread requires an exact conversation UUID"
        );
    }
    if let Some((id, generation)) = expected {
        crate::catalog::reconcile(paths, &discover(paths).await?)?;
        let selected = crate::catalog::resolve(paths, id, generation)?;
        ensure!(
            selected.account == account
                && Some(selected.conversation_id.as_str()) == requested
                && selected.attachment.is_none(),
            "STALE_TARGET"
        );
    }
    let mut control = account_control(paths, account).await?;
    let mut params = json!({"cwd":cwd});
    if let Some(id) = requested {
        let saved = control.thread_for_state(id).await?;
        ensure!(
            crate::control::confirmed_quiescent(&saved, true),
            "the conversation is still working; open its existing terminal instead"
        );
        if launch.cwd.is_none() {
            cwd = PathBuf::from(
                relocation::saved_location(paths, id)
                    .or_else(|| saved["cwd"].as_str().map(str::to_owned))
                    .context("saved working directory is unavailable")?,
            );
            ensure!(cwd.is_dir(), "saved working directory is unavailable");
            params["cwd"] = json!(cwd);
        }
        let permissions = rollout_by_thread(&home, id)?
            .as_deref()
            .and_then(recorded_permissions)
            .unwrap_or_else(|| ("never".into(), "danger-full-access".into()));
        params["approvalPolicy"] = json!(permissions.0);
        params["sandbox"] = json!(permissions.1);
    }
    if requested.is_none() {
        params["historyMode"] = json!("legacy");
    }
    if launch
        .options
        .iter()
        .any(|s| matches!(s.as_str(), "--full-auto" | "--approve-for-me"))
    {
        params["approvalPolicy"] = json!("on-request");
        params["sandbox"] = json!("workspace-write");
        if launch.options.iter().any(|s| s == "--approve-for-me") {
            params["approvalsReviewer"] = json!("auto_review");
        }
    }
    if let Some((approval, sandbox)) = launch_permissions(&launch.options) {
        params["approvalPolicy"] = json!(approval);
        params["sandbox"] = json!(sandbox);
    }
    if let Some(id) = requested {
        params["threadId"] = json!(id);
        params["excludeTurns"] = json!(true);
    }
    for pair in launch.options.windows(2) {
        if matches!(pair[0].as_str(), "-m" | "--model") {
            params["model"] = json!(pair[1]);
        }
        if matches!(pair[0].as_str(), "-a" | "--ask-for-approval") {
            params["approvalPolicy"] = json!(pair[1]);
        }
        if matches!(pair[0].as_str(), "-s" | "--sandbox") {
            params["sandbox"] = json!(pair[1]);
        }
        if matches!(pair[0].as_str(), "-c" | "--config") {
            if let Some((key, value)) = pair[1].split_once('=') {
                match key.trim() {
                    "approval_policy" => {
                        params["approvalPolicy"] = json!(value.trim().trim_matches('"'))
                    }
                    "sandbox_mode" => params["sandbox"] = json!(value.trim().trim_matches('"')),
                    _ => {}
                }
            }
        }
    }
    let response = if requested.is_some() {
        control.resume_with_permissions(params.clone()).await?
    } else {
        // A loaded thread keeps its original permission profile on resume.
        // Select the native profile at creation, before name/set persists it.
        let mut start_params = params.clone();
        if start_params["sandbox"] == "danger-full-access" {
            start_params.as_object_mut().unwrap().remove("sandbox");
            start_params["permissions"] = json!(":danger-full-access");
        }
        control
            .request_timeout("thread/start", start_params, Duration::from_secs(120))
            .await?
    };
    let id = response["thread"]["id"]
        .as_str()
        .context("Codex returned no conversation identity")?;
    if requested.is_none() {
        let name = if let Ok(pane) = std::env::var("TMUX_PANE") {
            let name = tmux(&["display-message", "-p", "-t", &pane, "#{session_name}"])?;
            name.strip_prefix("codex-").unwrap_or(&name).to_owned()
        } else {
            cwd.file_name()
                .and_then(|name| name.to_str())
                .unwrap_or("Codex session")
                .to_owned()
        };
        let name = if let Some((_, selected_name)) = creation {
            selected_name
        } else if name.is_empty() {
            "Codex session"
        } else {
            &name
        };
        // thread/start only allocates an in-memory thread. Native name/set
        // materializes its rollout, making a zero-turn conversation resumable.
        // Never submit a dummy turn or manufacture history to force persistence.
        control
            .request_timeout(
                "thread/name/set",
                json!({"threadId":id,"name":name}),
                Duration::from_secs(120),
            )
            .await
            .context("could not save the new conversation before opening Codex")?;
        params.as_object_mut().unwrap().remove("historyMode");
        params["threadId"] = json!(id);
        params["excludeTurns"] = json!(true);
        control
            .resume_with_permissions(params)
            .await
            .context("the new conversation was saved but Codex could not reopen it")?;
    }
    access::record(paths, id)?;
    let mut attachment = None;
    if let Ok(pane) = std::env::var("TMUX_PANE") {
        bind_pane(paths, &pane, std::process::id(), &home, id)?;
        attachment = Some(crate::catalog::Attachment {
            host: "tmux".into(),
            target: pane,
            pid: std::process::id(),
            process_start: process_info(std::process::id())?.start_time,
        });
    } else if std::env::var("HERDR_ENV").as_deref() == Ok("1") {
        let session = std::env::var("SESSIONSATION_HERDR_SESSION")
            .or_else(|_| std::env::var("HERDR_SESSION"))?;
        let pane = std::env::var("HERDR_PANE_ID")?;
        let key = hosting::herdr_target(&session, &pane)?;
        bind_pane(paths, &key, std::process::id(), &home, id)?;
        attachment = Some(crate::catalog::Attachment {
            host: "herdr".into(),
            target: key,
            pid: std::process::id(),
            process_start: process_info(std::process::id())?.start_time,
        });
    }
    if requested.is_some() {
        catalog::set_stop_intent(paths, id, None)?;
    }
    if let Some((request, _)) = creation {
        crate::launch::attached(paths, request, id, attachment)?;
    } else {
        let recorded = crate::catalog::record_launch(
            paths,
            account,
            id,
            cwd.file_name()
                .and_then(|n| n.to_str())
                .unwrap_or("session"),
            attachment,
        )?;
        if let Some(request) = resumption {
            crate::resume::attached(paths, request, &recorded)?;
        }
    }
    let socket = home.join("app-server-control/app-server-control.sock");
    let error = Command::new(&config.codex_binary)
        .args(["resume", id, "--remote"])
        .arg(format!("unix://{}", socket.display()))
        .args(remote_tui_options(&launch.options))
        .args(&launch.prompt)
        .current_dir(&cwd)
        .env("CODEX_HOME", home)
        .env_remove("OPENAI_API_KEY")
        .env_remove("CODEX_API_KEY")
        .env_remove("CODEX_ACCESS_TOKEN")
        .exec();
    Err(error).context("could not launch Codex")
}
fn jobs_dir(paths: &Paths) -> Result<PathBuf> {
    let dir = paths.root.join("handoff-jobs");
    private_dir(&dir)?;
    Ok(dir)
}
fn job_path(paths: &Paths, id: &str) -> Result<PathBuf> {
    validate_name(id)?;
    Ok(jobs_dir(paths)?.join(format!("{id}.json")))
}
fn save_job(paths: &Paths, job: &Job) -> Result<()> {
    atomic_write(&job_path(paths, &job.id)?, &serde_json::to_vec_pretty(job)?)
}
pub fn jobs(paths: &Paths) -> Result<Vec<Job>> {
    let mut result = Vec::new();
    for entry in fs::read_dir(jobs_dir(paths)?)? {
        let entry = entry?;
        if entry.path().extension().is_some_and(|x| x == "json") {
            if let Ok(mut job) = serde_json::from_slice::<Job>(&fs::read(entry.path())?) {
                if !matches!(
                    job.phase.as_str(),
                    "complete" | "failed" | "cancelled" | "recovered"
                ) && daemon::now() - job.created_at > 10
                    && !handoff_worker_alive(&job.id)
                {
                    job.phase = "failed".into();
                    job.detail = "The move worker stopped. The original history is preserved; requeue a running session or use recover for a stopped pane.".into();
                }
                result.push(job);
            }
        }
    }
    result.sort_by(|a, b| {
        b.created_at
            .cmp(&a.created_at)
            .then_with(|| b.id.cmp(&a.id))
    });
    Ok(result)
}
pub fn destination_tui_may_be_active(paths: &Paths, account: &str) -> bool {
    jobs(paths).map_or(true, |jobs| {
        jobs.iter().any(|job| {
            job.target == account
                && matches!(job.phase.as_str(), "launching_target" | "running_on_target")
                && pane_state(&job.source.pane).map_or(true, |(dead, _)| !dead)
        })
    })
}
pub fn inactive_source_only_lease(paths: &Paths, account: &str) -> bool {
    let Ok(home) = paths
        .account(account)
        .and_then(|p| Ok(fs::canonicalize(p)?))
    else {
        return false;
    };
    let Ok(live) = instances() else {
        return false;
    };
    jobs(paths).is_ok_and(|jobs| {
        jobs.iter().any(|job| {
            if !job.source.inactive
                || job.source_lease_released
                || job.phase != "running_on_target"
                || job.target == account
                || fs::canonicalize(&job.source.home).ok().as_ref() != Some(&home)
            {
                return false;
            }
            let Ok(target) = paths
                .account(&job.target)
                .and_then(|p| Ok(fs::canonicalize(p)?))
            else {
                return false;
            };
            handoff_worker_alive(&job.id)
                && live.iter().any(|instance| {
                    instance.pane == job.source.pane
                        && fs::canonicalize(&instance.home).ok().as_ref() == Some(&target)
                })
        })
    })
}
pub(crate) fn handoff_worker_alive(id: &str) -> bool {
    let Ok(entries) = fs::read_dir("/proc") else {
        return false;
    };
    entries
        .flatten()
        .filter(|entry| {
            entry
                .file_name()
                .to_string_lossy()
                .bytes()
                .all(|b| b.is_ascii_digit())
        })
        .any(|entry| {
            let Ok(raw) = fs::read(entry.path().join("cmdline")) else {
                return false;
            };
            let args: Vec<_> = raw.split(|b| *b == 0).collect();
            args.iter().any(|arg| *arg == b"handoff-worker")
                && args
                    .windows(2)
                    .any(|pair| pair[0] == b"--job" && pair[1] == id.as_bytes())
        })
}
pub fn is_current_account(paths: &Paths, source: &Instance, name: &str) -> bool {
    match (
        fs::canonicalize(&source.home),
        paths
            .account(name)
            .and_then(|path| Ok(fs::canonicalize(path)?)),
    ) {
        (Ok(source), Ok(account)) => source == account,
        _ => false,
    }
}
fn job_conflicts_with_source(paths: &Paths, job: &Job, source: &Instance) -> bool {
    let same = (!source.inactive && !job.source.inactive && job.source.pane == source.pane)
        || (!source.thread_id.is_empty() && job.source.thread_id == source.thread_id);
    if !same
        || matches!(
            job.phase.as_str(),
            "failed" | "complete" | "recovered" | "cancelled"
        )
    {
        return false;
    }
    if job.phase == "running_on_target" && !source.inactive && job.source.pane == source.pane {
        if let (Ok(target), Ok(current)) = (
            paths
                .account(&job.target)
                .and_then(|p| Ok(fs::canonicalize(p)?)),
            fs::canonicalize(&source.home),
        ) {
            if target == current {
                return false;
            }
        }
    }
    true
}
pub async fn queue(paths: &Paths, pane: &str, target: &str) -> Result<Job> {
    queue_selected(paths, Some(pane), None, target).await
}
pub async fn queue_selected(
    paths: &Paths,
    pane: Option<&str>,
    thread: Option<&str>,
    target: &str,
) -> Result<Job> {
    queue_move(paths, pane, thread, target, MoveOptions::default()).await
}
pub async fn queue_move(
    paths: &Paths,
    pane: Option<&str>,
    thread: Option<&str>,
    target: &str,
    options: MoveOptions,
) -> Result<Job> {
    validate_name(target)?;
    ensure!(
        pane.is_some() != thread.is_some(),
        "specify exactly one of --pane or --thread"
    );
    let source = discover_filtered(paths, thread.is_some())
        .await?
        .into_iter()
        .find(|i| {
            pane.is_some_and(|p| !i.inactive && i.pane == p)
                || thread.is_some_and(|t| i.inactive && i.thread_id == t)
        })
        .context("selected Codex conversation is no longer available")?;
    ensure!(
        fs::canonicalize(&source.home)? != fs::canonicalize(paths.account(target)?)?,
        "conversation is already using that account home"
    );
    let status = daemon::call(paths, "status", json!({})).await?;
    let account = status["accounts"]
        .as_array()
        .and_then(|a| a.iter().find(|a| a["name"] == target))
        .context("unknown destination account")?;
    let _ = account;
    enqueue(paths, source, target, options, None, "account")
}

/// Retry the intent against fresh discovery; old consent and process IDs never carry over.
pub async fn retry(paths: &Paths, id: &str) -> Result<Job> {
    let job = jobs(paths)?
        .into_iter()
        .find(|j| j.id == id)
        .context("job not found")?;
    ensure!(
        matches!(job.phase.as_str(), "failed" | "cancelled" | "recovered"),
        "only stopped jobs can be retried"
    );
    ensure!(
        !handoff_worker_alive(id),
        "the job is still finishing recovery; retry shortly"
    );
    if job.source_archived
        || job.target_loaded
        || !job.prepared_threads.is_empty()
        || job
            .relocation
            .as_ref()
            .is_some_and(|r| r.state != "prepared" && job.phase != "recovered")
    {
        recover(paths, id).await?;
    }
    let source = discover(paths).await?.into_iter().find(|s| s.thread_id == job.source.thread_id && s.home == job.source.home).context("The original session is no longer in its original home. Open Sessions to choose its current copy.")?;
    let pane = (!source.inactive).then_some(source.pane.as_str());
    let thread = source.inactive.then_some(source.thread_id.as_str());
    let options = MoveOptions::default();
    match job.kind.as_str() {
        "account" => queue_move(paths, pane, thread, &job.target, options).await,
        "directory" => {
            let r = job.relocation.context("missing directory change details")?;
            relocation::queue(paths, pane, thread, &r.to, r.move_files, options).await
        }
        "restart" => {
            relocation::queue_restart(
                paths,
                pane.context("session is saved; open it from Sessions first")?,
                options,
            )
            .await
        }
        "pause" => {
            session_control::queue_pause(
                paths,
                pane.context("session is already stopped")?,
                options,
            )
            .await
        }
        "terminate" => {
            session_control::queue_termination(
                paths,
                pane.context("session is already stopped")?,
                options,
            )
            .await
        }
        _ => bail!("unknown job type; choose an action from Sessions"),
    }
}
fn enqueue(
    paths: &Paths,
    source: Instance,
    target: &str,
    options: MoveOptions,
    relocation: Option<relocation::Relocation>,
    kind: &str,
) -> Result<Job> {
    enqueue_with_id(
        paths,
        source,
        target,
        options,
        relocation,
        kind,
        EnqueuePolicy::default(),
    )
}

pub async fn queue_automatic(
    paths: &Paths,
    pane: &str,
    thread: &str,
    from: &str,
    target: &str,
    id: &str,
) -> Result<Job> {
    uuid::Uuid::parse_str(id)?;
    validate_name(target)?;
    let source = discover_filtered(paths, false)
        .await?
        .into_iter()
        .find(|s| {
            !s.inactive
                && s.pane == pane
                && s.thread_id == thread
                && s.account == from
                && s.shared_server
        })
        .context("automatic move source changed; review before retrying")?;
    ensure!(
        !jobs(paths)?
            .iter()
            .any(|job| job_conflicts_with_source(paths, job, &source)),
        "another session operation is already in progress"
    );
    enqueue_with_id(
        paths,
        source,
        target,
        MoveOptions {
            pause: true,
            accept_risks: false,
        },
        None,
        if target == from { "pause" } else { "account" },
        EnqueuePolicy {
            automatic_id: Some(id),
            non_steering: false,
            ..Default::default()
        },
    )
}

#[derive(Default)]
struct EnqueuePolicy<'a> {
    automatic_id: Option<&'a str>,
    non_steering: bool,
    resume_destination: Option<&'a str>,
    expected: Option<(&'a str, &'a str)>,
    unfinished_history_confirmed: bool,
}

fn enqueue_with_id(
    paths: &Paths,
    source: Instance,
    target: &str,
    options: MoveOptions,
    relocation: Option<relocation::Relocation>,
    kind: &str,
    policy: EnqueuePolicy<'_>,
) -> Result<Job> {
    let automatic_id = policy.automatic_id;
    let queue_lock = OpenOptions::new()
        .read(true)
        .write(true)
        .create(true)
        .truncate(false)
        .mode(0o600)
        .custom_flags(libc::O_NOFOLLOW)
        .open(jobs_dir(paths)?.join("queue.lock"))?;
    queue_lock.lock_exclusive()?;
    if !matches!(kind, "terminate" | "pause") {
        crate::accounts::ensure_available(paths, target)?;
    }
    let _conversation_lock = if !source.thread_id.is_empty() {
        let lock = catalog::operation_lock(paths, &source.thread_id)?;
        catalog::ensure_available(paths, &source.thread_id)?;
        Some(lock)
    } else {
        None
    };
    if let Some((id, generation)) = policy.expected {
        let selected = crate::catalog::resolve(paths, id, generation)?;
        ensure!(
            selected.conversation_id == source.thread_id
                && selected.account == source.account
                && selected.attachment.is_none(),
            "STALE_TARGET"
        );
    }
    if let Some(existing) = jobs(paths)?
        .into_iter()
        .find(|j| job_conflicts_with_source(paths, j, &source))
    {
        ensure!(
            automatic_id.is_none(),
            "another session operation is already in progress"
        );
        ensure!(
            !policy.non_steering || existing.non_steering,
            "OPERATION_POLICY_CONFLICT"
        );
        let same_relocation = match (&existing.relocation, &relocation) {
            (None, None) => true,
            (Some(a), Some(b)) => {
                a.from == b.from
                    && a.to == b.to
                    && a.move_files == b.move_files
                    && a.restart == b.restart
            }
            _ => false,
        };
        ensure!(
            existing.kind == kind && existing.target == target && same_relocation,
            "a different move is already queued for this conversation ({}); cancel it before choosing another destination",
            existing.id
        );
        return Ok(existing);
    }
    if !source.inactive && !matches!(kind, "terminate" | "pause") {
        hosting::retain(&source.pane, true)?;
    }
    if let Some(id) = automatic_id {
        ensure!(
            !job_path(paths, id)?.exists(),
            "automatic job already journaled; do not replay"
        );
    }
    if matches!(kind, "pause" | "terminate")
        && automatic_id.is_none()
        && !source.thread_id.is_empty()
    {
        catalog::set_stop_intent(
            paths,
            &source.thread_id,
            Some(uuid::Uuid::new_v4().to_string()),
        )?;
    }
    let phase = "queued";
    let job = Job {
        resume_destination: policy.resume_destination.map(str::to_owned),
        non_steering: policy.non_steering,
        id: automatic_id
            .map(str::to_owned)
            .unwrap_or_else(|| uuid::Uuid::new_v4().to_string()),
        kind: kind.into(),
        resume_after_move: !policy.non_steering
            && kind == "account"
            && !source.inactive
            && source.activity == Activity::Busy,
        continuation_delivery: String::new(),
        continuation_turn_id: None,
        continuation_observation: String::new(),
        source,
        target: target.into(),
        phase: phase.into(),
        detail: "Verifying the selected conversation before handoff.".into(),
        source_lease_released: false,
        pause_approved: options.pause,
        risks_approved: options.accept_risks,
        unfinished_history_confirmed: policy.unfinished_history_confirmed,
        pause_delivery: String::new(),
        source_archived: false,
        created_at: daemon::now(),
        confirmation_seq: 0,
        acknowledged_risks: Vec::new(),
        target_loaded: false,
        steered_turns: Default::default(),
        relocation,
        termination_threads: Vec::new(),
        history_checks: Vec::new(),
        prepared_threads: Vec::new(),
    };
    save_job(paths, &job)?;
    let log = OpenOptions::new()
        .append(true)
        .create(true)
        .truncate(false)
        .mode(0o600)
        .open(paths.root.join("handoff.log"))?;
    let child = Command::new(crate::self_executable::path()?)
        .arg("--data-dir")
        .arg(&paths.root)
        .args(["handoff-worker", "--job", &job.id])
        .current_dir(&paths.root)
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(log)
        .process_group(0)
        .spawn();
    if let Err(error) = child {
        let mut failed = job.clone();
        failed.phase = "failed".into();
        failed.detail = "handoff worker could not start".into();
        save_job(paths, &failed)?;
        return Err(error)
            .context("could not launch handoff worker from the running switcher executable");
    }
    Ok(job)
}
fn pane_state(pane: &str) -> Result<(bool, u32)> {
    if pane.starts_with("herdr/") {
        return hosting::state(pane);
    }
    let output = tmux(&[
        "display-message",
        "-p",
        "-t",
        pane,
        "#{pane_dead}\t#{pane_pid}",
    ])?;
    let parts: Vec<_> = output.split('\t').collect();
    ensure!(parts.len() == 2, "invalid tmux pane state");
    Ok((parts[0] == "1", parts[1].parse()?))
}
#[derive(Clone, Copy)]
struct ProcessInfo {
    pid: u32,
    parent: u32,
    state: char,
    start_time: u64,
}
fn process_info(pid: u32) -> Result<ProcessInfo> {
    let stat = fs::read_to_string(format!("/proc/{pid}/stat"))?;
    let fields: Vec<_> = stat
        .rsplit_once(") ")
        .context("invalid process stat")?
        .1
        .split_whitespace()
        .collect();
    ensure!(fields.len() > 19, "short process stat");
    Ok(ProcessInfo {
        pid,
        state: fields[0].chars().next().context("missing process state")?,
        parent: fields[1].parse()?,
        start_time: fields[19].parse()?,
    })
}
fn child_processes(pid: u32) -> Result<Vec<ProcessInfo>> {
    let mut processes = Vec::new();
    for entry in fs::read_dir("/proc")? {
        let entry = entry?;
        let Some(number) = entry
            .file_name()
            .to_str()
            .and_then(|s| s.parse::<u32>().ok())
        else {
            continue;
        };
        if let Ok(info) = process_info(number) {
            if info.state != 'Z' {
                processes.push(info);
            }
        }
    }
    let mut found = Vec::new();
    let mut frontier = vec![pid];
    while let Some(parent) = frontier.pop() {
        for info in processes.iter().filter(|info| info.parent == parent) {
            if !found
                .iter()
                .any(|child: &ProcessInfo| child.pid == info.pid)
            {
                found.push(*info);
                frontier.push(info.pid);
            }
        }
    }
    Ok(found)
}
fn stop_exact_process(info: ProcessInfo) -> Result<()> {
    if process_info(info.pid).is_ok_and(|current| current.state == 'Z') {
        return Ok(());
    }
    ensure!(
        process_info(info.pid)?.start_time == info.start_time,
        "process identity changed before stop"
    );
    // pidfd binds the signal to the verified process, even if its numeric PID is reused.
    let fd = unsafe { libc::syscall(libc::SYS_pidfd_open, info.pid, 0) };
    ensure!(
        fd >= 0,
        "cannot open process handle: {}",
        std::io::Error::last_os_error()
    );
    let fd = unsafe { OwnedFd::from_raw_fd(fd as i32) };
    ensure!(
        process_info(info.pid)?.start_time == info.start_time,
        "process identity changed before signal"
    );
    let result = unsafe {
        libc::syscall(
            libc::SYS_pidfd_send_signal,
            fd.as_raw_fd(),
            libc::SIGTERM,
            std::ptr::null::<libc::siginfo_t>(),
            0,
        )
    };
    if result < 0 {
        let error = std::io::Error::last_os_error();
        ensure!(
            error.raw_os_error() == Some(libc::ESRCH),
            "cannot stop process: {error}"
        );
    }
    Ok(())
}
async fn wait_stopped(info: ProcessInfo) -> Result<()> {
    for _ in 0..50 {
        if process_info(info.pid).map_or(true, |current| {
            current.start_time != info.start_time || current.state == 'Z'
        }) {
            return Ok(());
        }
        tokio::time::sleep(Duration::from_millis(100)).await;
    }
    bail!("process did not stop after SIGTERM; no forced kill was sent")
}
pub fn respond_move(paths: &Paths, id: &str, approve: bool) -> Result<()> {
    respond_move_sequence(paths, id, approve, None)
}
pub fn respond_move_sequence(
    paths: &Paths,
    id: &str,
    approve: bool,
    sequence: Option<u64>,
) -> Result<()> {
    let job: Job = serde_json::from_slice(&fs::read(job_path(paths, id)?)?)?;
    ensure!(
        job.phase.starts_with("awaiting_"),
        "this move is not waiting for confirmation"
    );
    ensure!(
        sequence.is_none_or(|s| s == job.confirmation_seq),
        "the confirmation changed; refresh the move before answering"
    );
    atomic_write(
        &jobs_dir(paths)?.join(format!("{id}.response")),
        &serde_json::to_vec(
            &json!({"phase":job.phase,"sequence":job.confirmation_seq,"approve":approve}),
        )?,
    )
}
pub fn cancel_move(paths: &Paths, id: &str) -> Result<()> {
    validate_name(id)?;
    let lock = crate::paths::lock_file(&jobs_dir(paths)?.join(format!("{id}.transition-lock")))?;
    lock.lock_exclusive()?;
    let job: Job = serde_json::from_slice(&fs::read(job_path(paths, id)?)?)?;
    ensure!(
        can_cancel_move(&job),
        "the move has already crossed the restart boundary; wait for completion or use recover if it fails"
    );
    atomic_write(&jobs_dir(paths)?.join(format!("{id}.cancel")), b"cancel")
}
pub(crate) fn can_cancel_move(job: &Job) -> bool {
    !matches!(
        job.phase.as_str(),
        "stopping_source"
            | "verifying"
            | "verified"
            | "launching_target"
            | "relocating_files"
            | "running_on_target"
            | "complete"
            | "failed"
            | "cancelled"
            | "recovered"
    )
}
pub fn respond_background(paths: &Paths, id: &str, approve: bool) -> Result<()> {
    let job: Job = serde_json::from_slice(&fs::read(job_path(paths, id)?)?)?;
    ensure!(
        job.phase == "awaiting_background_approval",
        "move is not awaiting background-process approval"
    );
    respond_move(paths, id, approve)
}
fn complete_rollout(path: &Path) -> Result<bool> {
    let file = File::open(path)?;
    let mut terminal = false;
    let mut seen = false;
    for line in BufReader::new(file).lines() {
        let line = line?;
        let Ok(value) = serde_json::from_str::<Value>(&line) else {
            // Damage in an old turn must not hide newer positive completion.
            // Damage after the last completion still prevents stopping.
            terminal = false;
            seen = false;
            continue;
        };
        if value["type"] != "event_msg" {
            continue;
        }
        match value["payload"]["type"].as_str() {
            Some("task_started") => {
                terminal = false;
                seen = true
            }
            Some("task_complete") => {
                terminal = true;
                seen = true
            }
            Some("turn_aborted") => {
                terminal = false;
                seen = true
            }
            _ => {}
        }
    }
    Ok(seen && terminal)
}
fn rollout_still_open(path: &Path) -> Result<bool> {
    Ok(open_rollouts()?.contains(path))
}
fn open_rollouts() -> Result<std::collections::BTreeSet<PathBuf>> {
    let mut found = std::collections::BTreeSet::new();
    for process in fs::read_dir("/proc")? {
        let process = process?;
        if !process
            .file_name()
            .as_encoded_bytes()
            .iter()
            .all(u8::is_ascii_digit)
        {
            continue;
        }
        let Ok(entries) = fs::read_dir(process.path().join("fd")) else {
            continue;
        };
        for entry in entries.flatten() {
            if let Ok(link) = fs::read_link(entry.path()) {
                if link.extension().is_some_and(|ext| ext == "jsonl") {
                    // Discovery and previews read the same histories concurrently.
                    // A reader is not evidence of an active conversation writer.
                    // Unknown access flags remain conservative.
                    let info = process.path().join("fdinfo").join(entry.file_name());
                    if fs::read_to_string(info)
                        .ok()
                        .and_then(|s| descriptor_writable(&s))
                        == Some(false)
                    {
                        continue;
                    }
                    found.insert(link);
                }
            }
        }
    }
    Ok(found)
}
fn descriptor_writable(info: &str) -> Option<bool> {
    let flags = info.lines().find_map(|line| line.strip_prefix("flags:"))?;
    let flags = u32::from_str_radix(flags.trim(), 8).ok()?;
    Some(flags & 3 != 0)
}
fn descendants(home: &Path, root: &str) -> Result<Vec<PathBuf>> {
    let mut files = Vec::new();
    fn walk(dir: &Path, files: &mut Vec<PathBuf>) -> Result<()> {
        if !dir.exists() {
            return Ok(());
        }
        for entry in fs::read_dir(dir)? {
            let entry = entry?;
            let ty = entry.file_type()?;
            ensure!(!ty.is_symlink(), "symlink in rollout tree");
            if ty.is_dir() {
                walk(&entry.path(), files)?
            } else if entry.path().extension().is_some_and(|x| x == "jsonl") {
                files.push(entry.path())
            }
        }
        Ok(())
    }
    walk(&home.join("sessions"), &mut files)?;
    // A deleted account's private vault retains archived conversations too.
    // Its fingerprints are revalidated before transfer; no provider owner or
    // credentials are recreated in the vault.
    if home
        .parent()
        .is_some_and(|p| p.join("history-checks.json").is_file())
    {
        walk(&home.join("archived_sessions"), &mut files)?;
    }
    let mut ids = std::collections::BTreeSet::from([root.to_owned()]);
    let mut selected = Vec::new();
    loop {
        let before = ids.len();
        for path in &files {
            let Ok(meta) = session_meta(path) else {
                continue;
            };
            let Some(id) = meta["id"].as_str() else {
                continue;
            };
            let source = &meta["source"];
            if ids.contains(id)
                || (source.is_object()
                    && ids.iter().any(|parent| source.to_string().contains(parent)))
            {
                ids.insert(id.into());
                if !selected.contains(path) {
                    selected.push(path.clone());
                }
            }
        }
        if ids.len() == before {
            break;
        }
    }
    ensure!(
        selected
            .iter()
            .any(|p| session_meta(p).is_ok_and(|m| m["id"] == root)),
        "root rollout not found"
    );
    Ok(selected)
}

pub async fn queue_snapshot(
    paths: &Paths,
    source: Instance,
    target: &str,
    options: MoveOptions,
    kind: &str,
) -> Result<Job> {
    ensure!(
        !source.inactive && !source.thread_id.is_empty(),
        "SESSION_NOT_RUNNING"
    );
    let owner = source.account.clone();
    let target = if kind == "account" { target } else { &owner };
    validate_name(target)?;
    if kind == "terminate" {
        session_control::disable_autostart(paths, &source).await?;
    } else {
        let status = daemon::call(paths, "status", json!({})).await?;
        ensure!(
            status["accounts"]
                .as_array()
                .is_some_and(|accounts| accounts.iter().any(|a| a["name"] == target
                    && a["enabled"] == true
                    && a["auth_required"] == false)),
            "ACCOUNT_UNAVAILABLE"
        );
    }
    if kind == "account" {
        ensure!(source.account != target, "ACCOUNT_ALREADY_SELECTED");
    }
    let plan = if kind == "restart" {
        Some(relocation::restart_plan(paths, &source)?)
    } else {
        None
    };
    ensure!(
        matches!(kind, "account" | "restart" | "terminate"),
        "UNSUPPORTED_OPERATION"
    );
    enqueue_with_id(
        paths,
        source,
        target,
        options,
        plan,
        kind,
        EnqueuePolicy {
            non_steering: true,
            ..Default::default()
        },
    )
}

/// Transfer saved history using the existing non-steering worker into a newly
/// created, explicitly owned HERDR pane. The journal precedes all provider writes.
pub(crate) async fn queue_saved_resume(
    paths: &Paths,
    source: Instance,
    target: &str,
    pane: &str,
    expected: (&str, &str),
    request: &str,
    unfinished_history_confirmed: bool,
) -> Result<Job> {
    ensure!(
        source.inactive && !source.thread_id.is_empty(),
        "SESSION_ALREADY_RUNNING"
    );
    crate::terminal::target(pane)?;
    ensure!(hosting::state(pane)?.0, "NEW_TERMINAL_NOT_READY");
    enqueue_with_id(
        paths,
        source,
        target,
        MoveOptions::default(),
        None,
        "account",
        EnqueuePolicy {
            automatic_id: Some(request),
            non_steering: true,
            resume_destination: Some(pane),
            expected: Some(expected),
            unfinished_history_confirmed,
        },
    )
}

/// Positive post-exec observation, supporting native binaries and packaged wrappers.
pub(crate) fn new_launch_observed(
    paths: &Paths,
    session: &crate::catalog::Session,
) -> Result<bool> {
    let Some(a) = &session.attachment else {
        return Ok(false);
    };
    let process = process_info(a.pid)?;
    if process.start_time != a.process_start || matches!(process.state, 'Z' | 'X') {
        return Ok(false);
    }
    let args = fs::read(format!("/proc/{}/cmdline", a.pid))?
        .split(|b| *b == 0)
        .filter(|a| !a.is_empty())
        .map(|a| String::from_utf8_lossy(a).into_owned())
        .collect::<Vec<_>>();
    let socket = format!(
        "unix://{}",
        paths
            .account(&session.account)?
            .join("app-server-control/app-server-control.sock")
            .display()
    );
    Ok(args.windows(4).any(|a| {
        a == [
            "resume",
            session.conversation_id.as_str(),
            "--remote",
            socket.as_str(),
        ]
    }))
}

#[cfg(test)]
fn copy_rollout(source: &Path, home: &Path, target_home: &Path) -> Result<()> {
    let relative = source
        .strip_prefix(home)
        .context("rollout outside source home")?;
    ensure!(relative.starts_with("sessions"), "invalid rollout location");
    let target = target_home.join(relative);
    private_dir(target.parent().context("missing rollout parent")?)?;
    if target.exists() {
        ensure!(
            !fs::symlink_metadata(&target)?.file_type().is_symlink(),
            "destination rollout is a symlink"
        );
        let old = fs::read(&target)?;
        let current = fs::read(source)?;
        ensure!(
            current.starts_with(&old),
            "destination rollout diverged; refusing overwrite"
        );
        if current.len() > old.len() {
            atomic_write(&target, &current)?;
        }
        return Ok(());
    }
    let mut temp = tempfile::NamedTempFile::new_in(target.parent().unwrap())?;
    std::io::copy(&mut File::open(source)?, &mut temp)?;
    temp.as_file().sync_all()?;
    temp.persist_noclobber(&target)?;
    File::open(target.parent().unwrap())?.sync_all()?;
    Ok(())
}

pub mod access;
pub mod autostart;
pub mod catalog;
pub mod deletion;
mod history;
pub(crate) use history::preserve_history_checks;
pub(crate) use history::verify_preserved_history;
pub mod archiving;
pub mod hosting;
mod movement;
pub mod opening;
pub mod relocation;
pub mod session_control;
pub mod tmux_inventory;
pub use movement::{recover, worker};

fn recorded_permissions(rollout: &Path) -> Option<(String, String)> {
    let file = File::open(rollout).ok()?;
    let mut latest = None;
    for line in BufReader::new(file).lines() {
        let Ok(line) = line else { break };
        let Ok(event) = serde_json::from_str::<Value>(&line) else {
            continue;
        };
        if event["type"] != "turn_context" {
            continue;
        }
        let payload = &event["payload"];
        let approval = payload["approval_policy"].as_str();
        let sandbox = payload["sandbox_policy"]["type"].as_str();
        latest = match (approval, sandbox) {
            (
                Some(a @ ("never" | "on-request" | "untrusted")),
                Some(s @ ("read-only" | "workspace-write" | "danger-full-access")),
            ) => Some((a.to_owned(), s.to_owned())),
            _ => None,
        };
    }
    latest
}

fn launch_permissions(options: &[String]) -> Option<(String, String)> {
    if options
        .iter()
        .any(|s| s == "--dangerously-bypass-approvals-and-sandbox")
    {
        return Some(("never".into(), "danger-full-access".into()));
    }
    let mut approval = None;
    let mut sandbox = None;
    for pair in options.windows(2) {
        match pair[0].as_str() {
            "-a" | "--ask-for-approval" => approval = Some(pair[1].clone()),
            "-s" | "--sandbox" => sandbox = Some(pair[1].clone()),
            "-c" | "--config" if pair[1].starts_with("approval_policy=") => {
                approval = Some(pair[1]["approval_policy=".len()..].trim_matches('"').into());
            }
            "-c" | "--config" if pair[1].starts_with("sandbox_mode=") => {
                sandbox = Some(pair[1]["sandbox_mode=".len()..].trim_matches('"').into());
            }
            _ => {}
        }
    }
    match (approval.as_deref(), sandbox.as_deref()) {
        (
            Some("never" | "on-request" | "untrusted"),
            Some("read-only" | "workspace-write" | "danger-full-access"),
        ) => Some((approval.unwrap(), sandbox.unwrap())),
        _ => None,
    }
}

fn without_permission_options(options: &[String]) -> Vec<String> {
    let mut result = Vec::new();
    let mut i = 0;
    while i < options.len() {
        let flag = options[i].as_str();
        if matches!(
            flag,
            "--dangerously-bypass-approvals-and-sandbox" | "--full-auto" | "--approve-for-me"
        ) {
            i += 1;
        } else if matches!(flag, "-a" | "--ask-for-approval" | "-s" | "--sandbox")
            || (matches!(flag, "-c" | "--config")
                && options.get(i + 1).is_some_and(|value| {
                    value.split_once('=').is_some_and(|(key, _)| {
                        matches!(key.trim(), "approval_policy" | "sandbox_mode")
                    })
                }))
        {
            i += 2;
        } else {
            result.push(options[i].clone());
            i += 1;
        }
    }
    result
}

fn remote_tui_options(options: &[String]) -> Vec<String> {
    without_permission_options(options)
        .into_iter()
        .filter(|s| s != "--no-daemon")
        .collect()
}

fn resume_shell(
    home: &Path,
    binary: &str,
    options: &[String],
    thread: &str,
    rollout: &Path,
) -> String {
    let (approval, sandbox) = recorded_permissions(rollout)
        .or_else(|| launch_permissions(options))
        .unwrap_or_else(|| ("never".into(), "danger-full-access".into()));
    let mut options = without_permission_options(options);
    options.extend([
        "--ask-for-approval".into(),
        approval,
        "--sandbox".into(),
        sandbox,
    ]);
    let options = options
        .iter()
        .map(|s| shell_quote(s))
        .collect::<Vec<_>>()
        .join(" ");
    format!(
        "exec env -u OPENAI_API_KEY -u CODEX_API_KEY -u CODEX_ACCESS_TOKEN CODEX_HOME={} {} resume {} {}",
        shell_quote(&home.to_string_lossy()),
        shell_quote(binary),
        options,
        shell_quote(thread)
    )
}

/// Minimal switcher-owned messaging inventory. No account credentials or rollout reads.
/// Legacy unmanaged panes must be adopted by normal switcher workflows first.
pub(crate) fn messaging_bindings(paths: &Paths) -> Result<Vec<(String, PathBuf, u32, u64)>> {
    let dir = paths.root.join("managed-panes");
    let mut result = Vec::new();
    for i in hosting::instances(paths)? {
        if catalog::messaging_owner(paths, &i.thread_id, &i.home)? {
            result.push((i.thread_id, i.home, i.pid, i.start_time));
        }
    }
    if !dir.exists() {
        return Ok(result);
    }
    for entry in fs::read_dir(dir)? {
        let entry = entry?;
        if !entry.file_type()?.is_file() || entry.path().extension().is_none_or(|s| s != "json") {
            continue;
        }
        let binding: Binding = serde_json::from_slice(&fs::read(entry.path())?)?;
        if uuid::Uuid::parse_str(&binding.thread).is_err() {
            continue;
        }
        if !process_info(binding.pid)
            .is_ok_and(|p| p.start_time == binding.start_time && !matches!(p.state, 'Z' | 'X'))
        {
            continue;
        }
        if !catalog::messaging_owner(paths, &binding.thread, &binding.home)? {
            continue;
        }
        result.push((
            binding.thread,
            binding.home,
            binding.pid,
            binding.start_time,
        ));
    }
    result.sort();
    result.dedup();
    Ok(result)
}

/// Same lock order as enqueue_with_id. Hold both across the actual native mutation.
pub(crate) fn messaging_lock(
    paths: &Paths,
    id: &str,
) -> std::result::Result<(File, File), &'static str> {
    let dir = jobs_dir(paths).map_err(|_| "local_io_error")?;
    let queue = crate::paths::lock_file(&dir.join("queue.lock")).map_err(|_| "local_io_error")?;
    queue.try_lock_exclusive().map_err(|_| "operation_busy")?;
    let conversation = catalog::operation_lock(paths, id).map_err(|_| "operation_busy")?;
    catalog::ensure_available(paths, id).map_err(|_| "unavailable")?;
    // Read persisted phases, not jobs()'s inferred worker failure: uncertainty blocks input.
    for entry in fs::read_dir(dir).map_err(|_| "local_io_error")? {
        let entry = entry.map_err(|_| "local_io_error")?;
        if entry.path().extension().is_some_and(|s| s == "json") {
            let bytes = fs::read(entry.path()).map_err(|_| "local_io_error")?;
            let job: Job = serde_json::from_slice(&bytes).map_err(|_| "migration_state_unknown")?;
            if job.source.thread_id == id
                && !matches!(
                    job.phase.as_str(),
                    "complete" | "failed" | "cancelled" | "recovered"
                )
            {
                return Err("migrating");
            }
        }
    }
    Ok((queue, conversation))
}

/// Messaging labels use only the existing catalog and enrolled home mapping.
/// Never use credential-derived identity matching for external homes.
pub(crate) fn messaging_labels(
    paths: &Paths,
    id: &str,
    home: Option<&Path>,
) -> Result<(Option<String>, Option<String>)> {
    let name = catalog::saved_name(paths, id)?;
    let account = home
        .map(|h| account_for_home(paths, h))
        .filter(|a| a != "external" && a != "unknown" && crate::paths::validate_name(a).is_ok());
    Ok((name, account))
}

pub(crate) fn messaging_stop_intent(paths: &Paths, id: &str) -> Result<Option<String>> {
    catalog::stop_intent(paths, id)
}
pub(crate) fn messaging_clear_stop_intent(paths: &Paths, id: &str) -> Result<()> {
    catalog::set_stop_intent(paths, id, None)
}
pub(crate) fn messaging_inventory(paths: &Paths) -> Result<Vec<String>> {
    let mut ids = catalog::messaging_inventory(paths)?;
    let dir = paths.root.join("managed-panes");
    if dir.exists() {
        for entry in fs::read_dir(dir)? {
            let entry = entry?;
            if !entry.file_type()?.is_file() || entry.path().extension().is_none_or(|s| s != "json")
            {
                continue;
            }
            ensure!(entry.metadata()?.len() <= 65536, "invalid binding");
            let binding: Binding = serde_json::from_slice(&fs::read(entry.path())?)?;
            if uuid::Uuid::parse_str(&binding.thread).is_ok()
                && catalog::messaging_owner(paths, &binding.thread, &binding.home)?
            {
                ids.push(binding.thread);
            }
            ensure!(ids.len() <= 1024, "discovery limit");
        }
    }

    ids.sort();
    ids.dedup();
    ensure!(ids.len() <= 256, "discovery limit");
    Ok(ids)
}

#[cfg(test)]
mod tests {
    #[test]
    fn history_readers_do_not_block_resume_but_writers_do() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("rollout.jsonl");
        std::fs::write(&path, b"fixture").unwrap();
        let reader = std::fs::File::open(&path).unwrap();
        assert!(!super::open_rollouts().unwrap().contains(&path));
        let writer = std::fs::OpenOptions::new()
            .append(true)
            .open(&path)
            .unwrap();
        assert!(super::open_rollouts().unwrap().contains(&path));
        drop(writer);
        assert!(!super::open_rollouts().unwrap().contains(&path));
        drop(reader);
        assert_eq!(super::descriptor_writable("flags:\t0100000\n"), Some(false));
        assert_eq!(super::descriptor_writable("flags:\t0100002\n"), Some(true));
        assert_eq!(super::descriptor_writable("flags: invalid"), None);
    }
    use super::*;
    #[test]
    fn preserved_archive_is_a_resume_source_without_recreating_account_credentials() {
        let d = tempfile::tempdir().unwrap();
        let home = d.path().join("vault/home");
        fs::create_dir_all(home.join("archived_sessions")).unwrap();
        let rollout = home.join("archived_sessions/saved.jsonl");
        fs::write(&rollout, format!("{}\n",json!({"type":"session_meta","payload":{"id":"saved","cwd":d.path(),"source":"cli"}}))).unwrap();
        assert!(descendants(&home, "saved").is_err());
        fs::write(home.parent().unwrap().join("history-checks.json"), "[]").unwrap();
        assert_eq!(descendants(&home, "saved").unwrap(), vec![rollout]);
        assert!(!home.join("auth.json").exists());
    }
    #[test]
    fn activity_recovers_after_old_damage_but_never_trusts_a_damaged_tail() {
        let tmp = tempfile::tempdir().unwrap();
        let file = tmp.path().join("rollout.jsonl");
        let event = |kind| format!("{}\n", json!({"type":"event_msg","payload":{"type":kind}}));
        fs::write(
            &file,
            format!("broken old record\n{}", event("task_started")),
        )
        .unwrap();
        assert_eq!(rollout_activity(&file), Activity::Busy);
        assert!(!complete_rollout(&file).unwrap());
        fs::write(
            &file,
            format!(
                "broken old record\n{}{}",
                event("task_started"),
                event("task_complete")
            ),
        )
        .unwrap();
        assert_eq!(rollout_activity(&file), Activity::Idle);
        assert!(complete_rollout(&file).unwrap());
        fs::write(&file, format!("{}broken tail", event("task_complete"))).unwrap();
        assert_eq!(rollout_activity(&file), Activity::Unknown);
        assert!(!complete_rollout(&file).unwrap());
        fs::write(
            &file,
            format!("{}{}", event("task_started"), event("turn_aborted")),
        )
        .unwrap();
        assert_eq!(rollout_activity(&file), Activity::Idle);
    }
    #[test]
    fn activity_reads_a_bounded_tail_and_never_infers_idle_from_an_old_prefix() {
        use std::{cell::Cell, io::Cursor, rc::Rc};
        struct Counted {
            inner: Cursor<Vec<u8>>,
            count: Rc<Cell<usize>>,
        }
        impl Read for Counted {
            fn read(&mut self, buf: &mut [u8]) -> std::io::Result<usize> {
                let n = self.inner.read(buf)?;
                self.count.set(self.count.get() + n);
                Ok(n)
            }
        }
        impl Seek for Counted {
            fn seek(&mut self, pos: SeekFrom) -> std::io::Result<u64> {
                self.inner.seek(pos)
            }
        }
        let event = |kind| format!("{}\n", json!({"type":"event_msg","payload":{"type":kind}}));
        let mut data = event("task_complete").into_bytes();
        data.extend(vec![b'x'; ACTIVITY_TAIL_BYTES as usize * 8]);
        data.extend_from_slice(b"\n");
        let count = Rc::new(Cell::new(0));
        assert_eq!(
            activity_from_tail(Counted {
                inner: Cursor::new(data.clone()),
                count: count.clone()
            }),
            Activity::Unknown
        );
        assert!(count.get() <= ACTIVITY_TAIL_BYTES as usize);
        data.extend_from_slice(event("task_started").as_bytes());
        assert_eq!(
            activity_from_tail(Cursor::new(data.clone())),
            Activity::Busy
        );
        data.extend_from_slice(event("task_complete").as_bytes());
        assert_eq!(
            activity_from_tail(Cursor::new(data.clone())),
            Activity::Idle
        );
        data.extend_from_slice(b"broken tail");
        assert_eq!(activity_from_tail(Cursor::new(data)), Activity::Unknown);
    }
    use crate::model::{Account, State};

    #[test]
    fn moved_session_uses_latest_recorded_permissions() {
        let tmp = tempfile::tempdir().unwrap();
        let rollout = tmp.path().join("rollout.jsonl");
        fs::write(&rollout, concat!(
            "{\"type\":\"turn_context\",\"payload\":{\"approval_policy\":\"never\",\"sandbox_policy\":{\"type\":\"danger-full-access\"}}}\n",
            "{\"type\":\"turn_context\",\"payload\":{\"approval_policy\":\"on-request\",\"sandbox_policy\":{\"type\":\"workspace-write\"}}}\n"
        )).unwrap();
        let command = resume_shell(
            tmp.path(),
            "codex",
            &[
                "--dangerously-bypass-approvals-and-sandbox".into(),
                "--model".into(),
                "test-model".into(),
            ],
            "thread",
            &rollout,
        );
        assert!(
            command.contains("'--ask-for-approval' 'on-request' '--sandbox' 'workspace-write'")
        );
        assert!(command.contains("'--model' 'test-model'"));
        assert!(!command.contains("'--dangerously-bypass-approvals-and-sandbox'"));
    }

    #[test]
    fn moved_session_uses_explicit_flags_then_yolo_fallback() {
        let tmp = tempfile::tempdir().unwrap();
        let rollout = tmp.path().join("rollout.jsonl");
        fs::write(&rollout, "{\"type\":\"session_meta\",\"payload\":{}}\n").unwrap();
        let known = resume_shell(
            tmp.path(),
            "codex",
            &[
                "--sandbox".into(),
                "read-only".into(),
                "--ask-for-approval".into(),
                "on-request".into(),
            ],
            "thread",
            &rollout,
        );
        assert!(known.contains("'--ask-for-approval' 'on-request' '--sandbox' 'read-only'"));
        let unknown = resume_shell(tmp.path(), "codex", &[], "thread", &rollout);
        assert!(unknown.contains("'--ask-for-approval' 'never' '--sandbox' 'danger-full-access'"));
    }

    #[test]
    fn external_home_display_requires_unique_current_enrolled_identity() {
        let tmp = tempfile::tempdir().unwrap();
        let paths = Paths::new(Some(tmp.path().join("state"))).unwrap();
        let external = tmp.path().join("default-codex");
        private_dir(&external).unwrap();
        let personal = paths.account("personal").unwrap();
        let work = paths.account("work").unwrap();
        private_dir(&personal).unwrap();
        private_dir(&work).unwrap();
        fs::write(
            external.join("auth.json"),
            r#"{"tokens":{"account_id":"id-personal"}}"#,
        )
        .unwrap();
        fs::write(
            personal.join("auth.json"),
            r#"{"tokens":{"account_id":"id-personal"}}"#,
        )
        .unwrap();
        fs::write(
            work.join("auth.json"),
            r#"{"tokens":{"account_id":"id-work"}}"#,
        )
        .unwrap();
        let mut state = State::default();
        state.accounts.insert(
            "personal".into(),
            Account::new("personal".into(), "id-personal".into()),
        );
        state
            .accounts
            .insert("work".into(), Account::new("work".into(), "id-work".into()));
        assert_eq!(account_for_home(&paths, &external), "external");
        assert_eq!(
            display_account_for_home(&paths, &external, "external", Some(&state)).as_deref(),
            Some("personal")
        );
        assert_eq!(
            display_account_for_home(&paths, &work, "work", Some(&state)).as_deref(),
            Some("work")
        );
        fs::write(
            external.join("auth.json"),
            r#"{"tokens":{"account_id":"unregistered"}}"#,
        )
        .unwrap();
        assert_eq!(
            display_account_for_home(&paths, &external, "external", Some(&state)),
            None
        );
        fs::write(
            external.join("auth.json"),
            r#"{"tokens":{"account_id":"id-personal"}}"#,
        )
        .unwrap();
        fs::write(
            personal.join("auth.json"),
            r#"{"tokens":{"account_id":"changed"}}"#,
        )
        .unwrap();
        assert_eq!(
            display_account_for_home(&paths, &external, "external", Some(&state)),
            None
        );
        fs::write(
            personal.join("auth.json"),
            r#"{"tokens":{"account_id":"id-personal"}}"#,
        )
        .unwrap();
        state.accounts.get_mut("work").unwrap().identity = "id-personal".into();
        fs::write(
            work.join("auth.json"),
            r#"{"tokens":{"account_id":"id-personal"}}"#,
        )
        .unwrap();
        assert_eq!(
            display_account_for_home(&paths, &external, "external", Some(&state)),
            None
        );
        assert_eq!(
            display_account_for_home(&paths, &external, "external", None),
            None
        );
    }
    #[test]
    fn inactive_discovery_prefers_longest_consistent_copy() {
        let tmp = tempfile::tempdir().unwrap();
        let paths = Paths::new(Some(tmp.path().join("state"))).unwrap();
        crate::store::save_config(&paths, &crate::config::Config::default()).unwrap();
        let id = "33333333-3333-4333-8333-333333333333";
        let base = format!(
            "{{\"type\":\"session_meta\",\"payload\":{{\"id\":\"{id}\",\"source\":\"cli\",\"cwd\":\"{}\"}}}}\n{{\"type\":\"event_msg\",\"payload\":{{\"type\":\"task_started\"}}}}\n{{\"type\":\"event_msg\",\"payload\":{{\"type\":\"task_complete\"}}}}\n",
            tmp.path().display()
        );
        for (account, content) in [
            ("first", base.clone()),
            (
                "second",
                format!(
                    "{base}{{\"type\":\"event_msg\",\"payload\":{{\"type\":\"task_started\"}}}}\n{{\"type\":\"event_msg\",\"payload\":{{\"type\":\"task_complete\"}}}}\n"
                ),
            ),
        ] {
            let dir = paths
                .root
                .join("accounts")
                .join(account)
                .join("sessions/2026/10/09");
            private_dir(&dir).unwrap();
            fs::write(dir.join(format!("rollout-{id}.jsonl")), content).unwrap();
        }
        let unfinished = "55555555-5555-4555-8555-555555555555";
        fs::write(
            paths.root.join(format!("accounts/first/sessions/2026/10/09/rollout-{unfinished}.jsonl")),
            format!("{{\"type\":\"session_meta\",\"payload\":{{\"id\":\"{unfinished}\",\"source\":\"cli\",\"cwd\":\"{}\"}}}}\n{{\"type\":\"event_msg\",\"payload\":{{\"type\":\"task_started\"}}}}\n", tmp.path().display()),
        ).unwrap();
        let found = all_instances(&paths).unwrap();
        assert_eq!(found.len(), 2);
        let latest = found.iter().find(|item| item.thread_id == id).unwrap();
        assert!(latest.inactive);
        assert_eq!(latest.home, paths.account("second").unwrap());
        assert_eq!(
            found
                .iter()
                .find(|item| item.thread_id == unfinished)
                .unwrap()
                .block_reason
                .as_deref(),
            Some("unfinished")
        );
        let mut prior = Job {
            resume_destination: None,
            non_steering: false,
            id: "prior".into(),
            kind: "account".into(),
            source: latest.clone(),
            target: "second".into(),
            phase: "running_on_target".into(),
            detail: String::new(),
            source_lease_released: false,
            pause_approved: false,
            risks_approved: false,
            unfinished_history_confirmed: false,
            pause_delivery: String::new(),
            resume_after_move: false,
            continuation_delivery: String::new(),
            continuation_turn_id: None,
            continuation_observation: String::new(),
            source_archived: false,
            created_at: 0,
            confirmation_seq: 0,
            acknowledged_risks: Vec::new(),
            target_loaded: false,
            steered_turns: Default::default(),
            relocation: None,
            termination_threads: Vec::new(),
            history_checks: Vec::new(),
            prepared_threads: Vec::new(),
        };
        prior.source.pane = "%29".into();
        let mut live = latest.clone();
        live.inactive = false;
        live.pane = "%29".into();
        assert!(!job_conflicts_with_source(&paths, &prior, &live));
        prior.phase = "waiting_for_idle".into();
        assert!(job_conflicts_with_source(&paths, &prior, &live));
    }
    #[test]
    fn repeated_handoff_advances_only_from_an_older_prefix() {
        let tmp = tempfile::tempdir().unwrap();
        let source = tmp.path().join("source");
        let dest = tmp.path().join("dest");
        let relative = Path::new("sessions/2026/10/09/rollout-test.jsonl");
        private_dir(source.join("sessions/2026/10/09").as_path()).unwrap();
        private_dir(dest.join("sessions/2026/10/09").as_path()).unwrap();
        let old = b"first\n";
        let newer = b"first\nsecond\n";
        fs::write(source.join(relative), newer).unwrap();
        fs::write(dest.join(relative), old).unwrap();
        copy_rollout(&source.join(relative), &source, &dest).unwrap();
        assert_eq!(fs::read(dest.join(relative)).unwrap(), newer);
        fs::write(source.join(relative), b"diverged\n").unwrap();
        assert!(copy_rollout(&source.join(relative), &source, &dest).is_err());
        assert_eq!(fs::read(dest.join(relative)).unwrap(), newer);
    }
}
