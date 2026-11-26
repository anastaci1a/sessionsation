//! Terminal hosting only. Provider authentication and conversation preparation stay
//! in the account-aware launcher and movement worker.
use super::*;
use sha2::{Digest, Sha256};

#[derive(Clone, Serialize, Deserialize)]
struct HerdrBinding {
    target: String,
    binding: Binding,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    socket: Option<PathBuf>,
}

/// Pane binding records by file version (see [`crate::file_cache`]).
static BINDINGS: crate::file_cache::FileCache<HerdrBinding> = crate::file_cache::FileCache::new();
/// Bound processes (pid, start time) proven gone. A process identity never comes back: the
/// pid can be reused only with another start time, so a gone binding is skipped without
/// reading `/proc` again (there are hundreds of historical bindings for a handful of live ones).
static GONE: crate::proc::identity::GoneSet = crate::proc::identity::GoneSet::new();

pub use crate::terminal::herdr_target;
pub(super) use crate::terminal::retain;
use crate::terminal::{call, target};

/// HERDR socket of a still-identical process (same start time), read from the
/// HERDR_SOCKET_PATH it was started with; for sessions sessionsation did not launch.
pub(crate) fn live_process_socket(pid: u32, start_time: u64) -> Option<PathBuf> {
    process_info(pid)
        .ok()
        .filter(|p| p.start_time == start_time && !matches!(p.state, 'Z' | 'X'))
        .and_then(|p| process_socket(p.pid))
}
fn process_socket(pid: u32) -> Option<PathBuf> {
    let bytes = fs::read(format!("/proc/{pid}/environ")).ok()?;
    bytes
        .split(|b| *b == 0)
        .find_map(|v| v.strip_prefix(b"HERDR_SOCKET_PATH="))
        .and_then(|v| std::str::from_utf8(v).ok())
        .map(PathBuf::from)
        .filter(|p| p.is_absolute())
}
fn binding_file(paths: &Paths, pane: &str) -> PathBuf {
    let key = format!("{:x}", Sha256::digest(pane.as_bytes()));
    paths
        .root
        .join(crate::layout::HERDR_PANES)
        .join(format!("{key}.json"))
}
fn receipt_socket(paths: &Paths, pane: &str, thread: Option<&str>) -> Result<Option<PathBuf>> {
    let (session, pane_id) = target(pane)?;
    let mut latest: Option<(i64, PathBuf)> = None;
    for directory in [crate::layout::LAUNCHES, crate::layout::RESUMPTIONS] {
        let dir = paths.root.join(directory);
        if !dir.exists() {
            continue;
        }
        for entry in fs::read_dir(dir)? {
            let entry = entry?;
            if !entry.file_type()?.is_file() || entry.path().extension().is_none_or(|e| e != "json")
            {
                continue;
            }
            let value: Value = serde_json::from_slice(&fs::read(entry.path())?)?;
            if value["request"]["host"]["session"] != session
                || value["pane_id"] != pane_id
                || thread.is_some_and(|id| value["conversation_id"] != id)
            {
                continue;
            }
            let Some(socket) = value["request"]["host"]["socket"]
                .as_str()
                .map(PathBuf::from)
                .filter(|p| p.is_absolute())
            else {
                continue;
            };
            let time = value["created_at"]
                .as_i64()
                .or_else(|| value["updated_at"].as_i64())
                .unwrap_or(0);
            if latest.as_ref().is_none_or(|(t, _)| time > *t) {
                latest = Some((time, socket));
            }
        }
    }
    Ok(latest.map(|(_, socket)| socket))
}
pub(crate) fn socket_for(paths: &Paths, pane: &str) -> Result<Option<PathBuf>> {
    let file = binding_file(paths, pane);
    if !file.exists() {
        return if pane.starts_with("herdr/") {
            receipt_socket(paths, pane, None)
        } else {
            Ok(None)
        };
    }
    ensure!(
        !fs::symlink_metadata(&file)?.file_type().is_symlink(),
        "unsafe HERDR binding"
    );
    let record: HerdrBinding = serde_json::from_slice(&fs::read(file)?)?;
    ensure!(record.target == pane, "HERDR_BINDING_MISMATCH");
    let socket = record.socket.or_else(|| {
        process_info(record.binding.pid)
            .ok()
            .filter(|p| p.start_time == record.binding.start_time && !matches!(p.state, 'Z' | 'X'))
            .and_then(|p| process_socket(p.pid))
    });
    if socket.is_some() {
        Ok(socket)
    } else {
        receipt_socket(paths, pane, Some(&record.binding.thread))
    }
}
pub(super) fn state(paths: &Paths, pane: &str) -> Result<(bool, u32)> {
    if let Some(socket) = socket_for(paths, pane)? {
        crate::terminal::state_at(pane, Some(&socket))
    } else {
        crate::terminal::state(pane)
    }
}
/// Close the session's recorded pane after its agent has verifiably stopped.
/// HERDR panes only, and only while nothing but the shell runs in them. tmux
/// panes are never closed (documented: skipped with a reason).
pub fn close_pane(paths: &Paths, pane: &str) -> (bool, Option<String>) {
    if !pane.starts_with("herdr/") {
        return (
            false,
            Some("PANE_CLOSE_UNSUPPORTED: tmux panes are left open".into()),
        );
    }
    match socket_for(paths, pane) {
        Ok(socket) => crate::terminal::close_idle_pane(pane, socket.as_deref()),
        Err(_) => (false, Some("PANE_STATE_UNVERIFIED".into())),
    }
}
/// Seconds a stop waits for the pane to return to its idle shell before reporting `PANE_BUSY`.
pub const PANE_SETTLE_SECONDS: u64 = 3;
/// Contract 1.66.2: [`close_pane`] right after a verified stop. The stopped provider's
/// wrapper or shutdown children (a resume wrapper, an async SessionEnd hook) can still be
/// the pane's foreground for a moment, so a busy pane is re-checked for up to
/// [`PANE_SETTLE_SECONDS`]. Nothing is signalled; the close rules are unchanged.
pub fn close_pane_after_stop(paths: &Paths, pane: &str) -> (bool, Option<String>) {
    let deadline =
        std::time::Instant::now() + crate::timing::budget(Duration::from_secs(PANE_SETTLE_SECONDS));
    loop {
        let result = close_pane(paths, pane);
        let busy = result
            .1
            .as_deref()
            .is_some_and(|r| r.starts_with("PANE_BUSY"));
        if !busy || std::time::Instant::now() >= deadline {
            return result;
        }
        std::thread::sleep(crate::timing::scaled(Duration::from_millis(100)));
    }
}
pub(super) fn relaunch(paths: &Paths, pane: &str, cwd: &str, command: &str) -> Result<()> {
    if let Some(socket) = socket_for(paths, pane)? {
        crate::terminal::relaunch_at(pane, cwd, command, Some(&socket))
    } else {
        crate::terminal::relaunch(pane, cwd, command)
    }
}

pub(super) fn bind(paths: &Paths, pane: &str, binding: Binding) -> Result<()> {
    target(pane)?;
    let dir = paths.root.join(crate::layout::HERDR_PANES);
    private_dir(&dir)?;
    let key = format!("{:x}", Sha256::digest(pane.as_bytes()));
    atomic_write(
        &dir.join(format!("{key}.json")),
        &serde_json::to_vec(&HerdrBinding {
            target: pane.into(),
            socket: process_socket(binding.pid),
            binding,
        })?,
    )
}

pub(super) fn instances(paths: &Paths) -> Result<Vec<Instance>> {
    instances_with_state(paths, crate::terminal::state_at)
}

fn instances_with_state(
    paths: &Paths,
    mut state: impl FnMut(&str, Option<&Path>) -> Result<(bool, u32)>,
) -> Result<Vec<Instance>> {
    let dir = paths.root.join(crate::layout::HERDR_PANES);
    if !dir.exists() {
        return Ok(vec![]);
    }
    let mut result = Vec::new();
    let mut present = Vec::new();
    for entry in fs::read_dir(&dir)? {
        let entry = entry?;
        if entry.path().extension().is_none_or(|e| e != "json") {
            continue;
        }
        ensure!(!entry.file_type()?.is_symlink(), "unsafe HERDR binding");
        present.push(entry.path());
        let record =
            match BINDINGS.load(&entry.path(), |b| serde_json::from_slice::<HerdrBinding>(b)) {
                Ok(record) => (*record).clone(),
                Err(crate::file_cache::LoadError::Io(e)) => return Err(e.into()),
                Err(crate::file_cache::LoadError::Parse(e)) => return Err(e.into()),
            };
        let b = record.binding;
        if GONE.contains(b.pid, b.start_time) {
            continue;
        }
        // A dead or replaced bound process cannot own this pane. Reject it before
        // contacting HERDR: stopped servers otherwise add a connection timeout
        // per historical binding on every menu/discovery request. Retain the
        // binding/history, and keep the full host/ownership checks for live PIDs.
        let bound = match process_info(b.pid) {
            Ok(bound) => bound,
            Err(error) => {
                if error
                    .downcast_ref::<std::io::Error>()
                    .is_some_and(|e| e.kind() == std::io::ErrorKind::NotFound)
                {
                    GONE.insert(b.pid, b.start_time);
                }
                continue;
            }
        };
        if bound.start_time != b.start_time || matches!(bound.state, 'Z' | 'X') {
            GONE.insert(b.pid, b.start_time);
            continue;
        }
        // Read-only discovery never starts a server and never infers ownership
        // from the UI's selected pane or an account's display name.
        let socket = record.socket.or_else(|| process_socket(b.pid));
        let socket = if socket.is_some() {
            socket
        } else {
            receipt_socket(paths, &record.target, Some(&b.thread))?
        };
        let Ok((false, pid)) = state(&record.target, socket.as_deref()) else {
            continue;
        };
        let Ok(info) = process_info(pid) else {
            continue;
        };
        if pid != b.pid
            || info.start_time != b.start_time
            || !process_home(pid).is_ok_and(|actual| crate::layout::same_home(&actual, &b.home))
            || resumed_thread(pid)?.as_deref() != Some(&b.thread)
        {
            continue;
        }
        let Some(rollout) = rollout_by_thread(&b.home, &b.thread)? else {
            continue;
        };
        let launch = parse_launch(&process_args(pid)?);
        let turn_facts = rollout_turn_facts(&rollout);
        let activity = rollout_activity(&rollout);
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
            turn_facts,
            detail: activity_detail(activity).into(),
            warnings: vec![],
            shared_server: true,
            server: None,
            move_id: None,
            pane: record.target.clone(),
            tmux_session: target(&record.target)?.0.into(),
            pid,
            start_time: info.start_time,
            cwd: fs::read_link(format!("/proc/{pid}/cwd"))?
                .to_string_lossy()
                .into(),
            thread_id: b.thread,
            rollout,
            home: process_home(pid)?,
            binary: launch.binary,
            resume_options: launch.options,
        });
    }
    BINDINGS.retain_dir(&dir, &present.iter().map(|p| p.as_path()).collect());
    Ok(result)
}

/// Admit a launch to a named existing headless server. This receipt does not
/// assert that Codex started; discovery confirms the resulting process identity.
pub async fn launch(
    paths: &Paths,
    session: &str,
    account: &str,
    thread: Option<&str>,
    options: &[String],
) -> Result<Value> {
    launch_inner(
        paths,
        session,
        account,
        thread,
        options,
        None,
        Placement::default(),
    )
    .await
}
pub async fn launch_checked(
    paths: &Paths,
    session: &str,
    account: &str,
    thread: Option<&str>,
    options: &[String],
    expected: (&str, &str),
) -> Result<Value> {
    launch_inner(
        paths,
        session,
        account,
        thread,
        options,
        Some(expected),
        Placement::default(),
    )
    .await
}
/// Resolve a requested Space against HERDR's list: the HERDR workspace id, else
/// the visible name (exact, then case-insensitive). Stored ids do not survive a
/// HERDR or array restart; the name does.
pub(crate) fn resolve_space(list: &[Value], wanted: &str) -> Option<String> {
    list.iter()
        .find(|w| w["workspace_id"] == wanted)
        .or_else(|| list.iter().find(|w| w["label"] == wanted))
        .or_else(|| {
            list.iter().find(|w| {
                w["label"]
                    .as_str()
                    .is_some_and(|l| l.eq_ignore_ascii_case(wanted.trim()))
            })
        })
        .and_then(|w| w["workspace_id"].as_str().map(str::to_owned))
}
/// Where Start Session opens: a new tab in an existing Space, or (default) a new
/// Space; `label` names the new tab.
#[derive(Clone, Copy, Debug, Default)]
pub struct Placement<'a> {
    pub space: Option<&'a str>,
    pub label: Option<&'a str>,
    /// Create the new Space with this label (refused if one already exists).
    pub new_space: Option<&'a str>,
    /// Resume callers: an unconfirmed launch, or a command that visibly exits
    /// again, is an error rather than `ok`.
    pub strict: bool,
    /// Contract 1.66.0: the `api resume-launch` request that types this command; the typed
    /// resume does not count that receipt as a resume in progress.
    pub owner: Option<&'a str>,
}

/// Outcome of one launch poll tick for a pane that was handed a command.
#[derive(Debug, PartialEq, Eq)]
pub(crate) enum PaneWatch {
    Waiting,
    /// The command ran (pane was busy) and the shell is idle again.
    Exited,
}

/// `busy_seen`: the pane was non-idle earlier in this poll; `idle`: it is idle now.
pub(crate) fn watch_pane(busy_seen: bool, idle: bool) -> PaneWatch {
    if busy_seen && idle {
        PaneWatch::Exited
    } else {
        PaneWatch::Waiting
    }
}

/// Strict-mode error for a launch that never confirmed.
pub(crate) fn unconfirmed_error(target: &str, exited: bool) -> String {
    if exited {
        format!("RESUME_LAUNCH_FAILED: the command exited before the session appeared in {target}")
    } else {
        format!("RESUME_LAUNCH_UNCONFIRMED: no running session was observed in {target}")
    }
}
pub async fn launch_placed(
    paths: &Paths,
    session: &str,
    account: &str,
    thread: Option<&str>,
    expected: (&str, &str),
    placement: Placement<'_>,
) -> Result<Value> {
    launch_inner(
        paths,
        session,
        account,
        thread,
        &[],
        Some(expected),
        placement,
    )
    .await
}
async fn launch_inner(
    paths: &Paths,
    session: &str,
    account: &str,
    thread: Option<&str>,
    options: &[String],
    expected: Option<(&str, &str)>,
    placement: Placement<'_>,
) -> Result<Value> {
    // `account` is a bare Codex name or a qualified `<provider>:<name>` (names are
    // unique per provider only).
    let account_arg = account;
    let (provider, account) = match account.split_once(':') {
        Some((provider, name)) => (crate::provider::ProviderId::parse(provider)?, name),
        None => (crate::provider::ProviderId::Codex, account),
    };
    validate_name(account)?;
    validate_name(session)?;
    if let Some(id) = thread {
        uuid::Uuid::parse_str(id)?;
    }
    let status = daemon::call(paths, "status", json!({})).await?;
    ensure!(
        status["accounts"]
            .as_array()
            .is_some_and(|accounts| accounts.iter().any(|a| a["name"] == account
                && a["provider"].as_str().unwrap_or("codex") == provider.as_str()
                && a["enabled"] == true
                && a["auth_required"] == false)),
        "ACCOUNT_UNAVAILABLE"
    );
    let cwd = std::env::current_dir()?;
    // Probe before creating topology. An absent server is an explicit error;
    // starting/stopping headless hosting is an owner's separate operation.
    call(session, &["api", "snapshot"])?;
    let dir = cwd.to_str().context("working directory is not UTF-8")?;
    // An existing Space is found by its id, else by its visible name; a requested
    // new Space whose name exists is that Space (never an error).
    let existing = match (placement.space, placement.new_space) {
        (None, None) => None,
        (wanted, label) => {
            let spaces = call(session, &["workspace", "list"])?;
            let list = spaces["workspaces"]
                .as_array()
                .context("INVALID_HOST_SPACES")?;
            match (wanted, label) {
                (Some(w), _) => Some(resolve_space(list, w).context("SPACE_NOT_FOUND")?),
                (None, Some(l)) => resolve_space(list, l),
                _ => None,
            }
        }
    };
    let tab_label = placement.label.unwrap_or("sessionsation");
    let new_tab = |space: &str| -> Result<String> {
        let created = call(
            session,
            &[
                "tab",
                "create",
                "--workspace",
                space,
                "--cwd",
                dir,
                "--label",
                tab_label,
                "--env",
                "SESSIONSATION_LAUNCHER_BYPASS=1",
                "--no-focus",
            ],
        )?;
        if let Some(actual) = created["tab"]["workspace_id"].as_str() {
            ensure!(actual == space, "HOST_WRONG_WORKSPACE");
        }
        Ok(created["root_pane"]["pane_id"]
            .as_str()
            .context("HERDR pane ID missing")?
            .to_owned())
    };
    // Contract 1.52.0: an idle pane in the destination Space before a new tab.
    let herdr = crate::pane_reuse::Herdr {
        session,
        socket: None,
    };
    let owner = uuid::Uuid::new_v4().to_string();
    let mut reused = existing.as_deref().and_then(|space| {
        crate::pane_reuse::find(
            paths,
            &herdr,
            space,
            tab_label,
            expected.map(|(id, _)| id),
            &owner,
        )
    });
    let pane = match (existing.as_deref(), &reused) {
        (_, Some(r)) => {
            crate::pane_reuse::label(&herdr, r, tab_label);
            r.pane.clone()
        }
        (Some(space), None) => new_tab(space)?,
        (None, None) => {
            if let Some(label) = placement.new_space {
                crate::paths::validate_space_label(label)?;
            }
            // The new Space's own initial shell is the pane that runs the
            // session; nothing extra is created or closed.
            let created = call(
                session,
                &[
                    "workspace",
                    "create",
                    "--cwd",
                    dir,
                    "--label",
                    placement.new_space.unwrap_or("sessionsation"),
                    "--env",
                    "SESSIONSATION_LAUNCHER_BYPASS=1",
                    "--no-focus",
                ],
            )?;
            if let (Some(label), Some(tab)) = (placement.label, created["tab"]["tab_id"].as_str()) {
                // Naming the tab is presentation only; the launch does not depend on it.
                let _ = call(session, &["tab", "rename", tab, label]);
            }
            created["root_pane"]["pane_id"]
                .as_str()
                .context("HERDR pane ID missing")?
                .to_owned()
        }
    };
    let ready = |key: String| async move {
        for _ in 0..50 {
            if state(paths, &key).is_ok_and(|(idle, _)| idle) {
                return Ok(key);
            }
            tokio::time::sleep(Duration::from_millis(100)).await;
        }
        bail!("HERDR_SHELL_UNAVAILABLE: {key}")
    };
    // A reused pane was just seen idle; the re-check below is right before typing.
    let mut key = herdr_target(session, &pane)?;
    if reused.is_none() {
        key = ready(key).await?;
    }
    let exe = crate::provider::claude::hooks::executable()?;
    let mut args = vec![
        exe.to_string_lossy().into_owned(),
        "--data-dir".into(),
        paths.root.to_string_lossy().into_owned(),
    ];
    // The pane's process starts the provider: carry an explicit choice there.
    if let Some(mode) = crate::config::launch_permissions_override() {
        args.extend(["--permissions".into(), mode.as_str().into()]);
    }
    let prefix = args.len();
    args.extend(["run".into(), "--account".into(), account_arg.into()]);
    if let Some((id, generation)) = expected {
        args.truncate(prefix);
        args.extend([
            "resume".into(),
            id.into(),
            "--expected-generation".into(),
            generation.into(),
        ]);
    } else {
        if let Some(id) = thread {
            args.extend(["--thread".into(), id.into()]);
        }
        args.push("--".into());
        args.extend_from_slice(options);
    }
    // The launcher bypass is the new tab's own environment; set here too so a
    // reused shell runs the command exactly as a new tab would.
    let command = format!(
        "cd -- {} && env -u TMUX -u TMUX_PANE SESSIONSATION_LAUNCHER_BYPASS=1 SESSIONSATION_HERDR_SESSION={} {}{}",
        shell_quote(&cwd.to_string_lossy()),
        shell_quote(session),
        placement
            .owner
            .map(|o| format!("{}={} ", crate::inline::RESUME_OWNER_ENV, shell_quote(o)))
            .unwrap_or_default(),
        args.iter()
            .map(|s| shell_quote(s))
            .collect::<Vec<_>>()
            .join(" ")
    );
    // Re-check a reused pane right before typing; a pane that stopped being an idle
    // shell gets a new tab instead.
    if let Some(r) = &reused
        && !crate::pane_reuse::pane_idle(&herdr, &r.pane)
    {
        let space = r.workspace.clone();
        reused = None;
        key = ready(herdr_target(session, &new_tab(&space)?)?).await?;
    }
    let (_, pane) = crate::terminal::target(&key)?;
    call(session, &["pane", "run", pane, &command])?;
    let mark = |mut v: Value| {
        if let Some(r) = &reused {
            v["pane_reused"] = json!(true);
            v["detail"] = json!(r.detail());
        }
        v
    };
    let mut busy_seen = false;
    for _ in 0..150 {
        if let Some(i) = instances(paths)?.into_iter().find(|i| i.pane == key) {
            return Ok(mark(
                json!({"schema_version":1,"state":"running","host":"herdr", "target":key,"account":account,"thread_id":i.thread_id,"viewer_attach":["herdr","session","attach",session]}),
            ));
        }
        // Providers without a Codex binding record their exact process in the
        // catalog before exec; require that PID to run the expected session.
        if let Some(s) = crate::catalog::list(paths)?.into_iter().find(|s| {
            crate::provider::native::of(&s.provider).is_some()
                && s.provider == provider.as_str()
                && s.account == account
                && s.attachment.as_ref().is_some_and(|a| a.target == key)
                && crate::provider::native::launch_observed(s)
        }) {
            return Ok(mark(
                json!({"schema_version":1,"state":"running","host":"herdr","target":key,"account":account,"thread_id":s.conversation_id,"provider":s.provider,"viewer_attach":["herdr","session","attach",session]}),
            ));
        }
        if placement.strict {
            if let Ok((idle, _)) = state(paths, &key) {
                if watch_pane(busy_seen, idle) == PaneWatch::Exited {
                    bail!("{}", unconfirmed_error(&key, true));
                }
                busy_seen |= !idle;
            }
        }
        tokio::time::sleep(Duration::from_millis(100)).await;
    }
    if placement.strict {
        bail!("{}", unconfirmed_error(&key, false));
    }
    Ok(mark(
        json!({"schema_version":1,"state":"launch_unconfirmed","host":"herdr",
        "target":key,"account":account,"thread_id":thread,
        "viewer_attach":["herdr","session","attach",session]}),
    ))
}

/// Publish one verified binding even if a previous caller died between write/rename.
pub(super) fn rebind(
    paths: &Paths,
    old: &str,
    new: &str,
    socket: &Path,
    expected: &Binding,
) -> Result<()> {
    target(old)?;
    target(new)?;
    let from = binding_file(paths, old);
    let to = binding_file(paths, new);
    if !from.exists() {
        let r: HerdrBinding = serde_json::from_slice(&fs::read(&to)?)?;
        ensure!(
            r.target == new && r.binding == *expected && r.socket.as_deref() == Some(socket),
            "HERDR_BINDING_MISMATCH"
        );
        return Ok(());
    }
    let mut r: HerdrBinding = serde_json::from_slice(&fs::read(&from)?)?;
    ensure!(
        (r.target == old || r.target == new) && r.binding == *expected,
        "HERDR_BINDING_MISMATCH"
    );
    if to.exists() && from != to {
        let other: HerdrBinding = serde_json::from_slice(&fs::read(&to)?)?;
        ensure!(
            other.target == new
                && other.binding == *expected
                && other.socket.as_deref() == Some(socket),
            "HERDR_BINDING_MISMATCH"
        );
    }
    r.target = new.into();
    r.socket = Some(socket.into());
    atomic_write(&from, &serde_json::to_vec(&r)?)?;
    if from != to {
        fs::rename(&from, &to)?;
        fs::File::open(to.parent().unwrap())?.sync_all()?;
    }
    Ok(())
}

#[cfg(test)]
mod tests;
