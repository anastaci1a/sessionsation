//! Project relocation shares account-move quiescence, confirmation and restart logic.
use super::movement::{
    identify, launch_target, phase, restore_source, risks, stop_source, tick, wait_idle,
};
use super::*;
use crate::control::Control;
use std::os::unix::fs::MetadataExt;

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct Relocation {
    pub from: PathBuf,
    pub to: PathBuf,
    pub move_files: bool,
    pub cross_filesystem: bool,
    pub state: String,
    pub source_device: u64,
    pub source_inode: u64,
    #[serde(default)]
    pub restart: bool,
    #[serde(default)]
    pub backend_note: Option<String>,
    #[serde(default)]
    pub stage: Option<PathBuf>,
    #[serde(default)]
    pub retained_source: Option<PathBuf>,
    #[serde(default)]
    pub retained_destination: Option<PathBuf>,
}

// Resolve existing symlinks without creating anything during a preview.
fn existing_parent(path: &Path) -> Result<(PathBuf, Vec<std::ffi::OsString>)> {
    let mut ancestor = path;
    let mut missing = Vec::new();
    loop {
        match fs::symlink_metadata(ancestor) {
            Ok(_) => {
                let resolved = fs::canonicalize(ancestor)?;
                ensure!(resolved.is_dir(), "destination parent is not a directory");
                return Ok((resolved, missing));
            }
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
                missing.push(
                    ancestor
                        .file_name()
                        .context("use a path without '..' in missing directories")?
                        .to_owned(),
                );
                ancestor = ancestor.parent().context("destination parent missing")?;
            }
            Err(e) => return Err(e.into()),
        }
    }
}
fn destination(path: &Path) -> Result<PathBuf> {
    ensure!(
        !path.as_os_str().is_empty(),
        "enter a destination directory"
    );
    let path = if path.is_absolute() {
        path.to_path_buf()
    } else {
        std::env::current_dir()?.join(path)
    };
    let name = path
        .file_name()
        .context("destination must name a directory")?;
    let (mut parent, missing) =
        existing_parent(path.parent().context("destination parent missing")?)?;
    for name in missing.iter().rev() {
        parent.push(name);
    }
    let target = parent.join(name);
    ensure!(
        !fs::symlink_metadata(&target).is_ok_and(|m| m.file_type().is_symlink()),
        "destination cannot be a symlink"
    );
    Ok(target)
}
fn plan_for(
    paths: &Paths,
    source: &Instance,
    to: &Path,
    move_files: bool,
    restart: bool,
) -> Result<Relocation> {
    let from = fs::canonicalize(&source.cwd).context("source working directory does not exist")?;
    let to = destination(to)?;
    ensure!(
        restart || from != to,
        "the conversation already uses that directory"
    );
    let metadata = fs::metadata(&from)?;
    ensure!(metadata.is_dir(), "source is not a directory");
    let (ancestor, _) = existing_parent(to.parent().context("destination parent missing")?)?;
    let target_device = fs::metadata(ancestor)?.dev();
    if move_files {
        ensure!(
            !to.try_exists()? && fs::symlink_metadata(&to).is_err(),
            "destination already exists; folder moves never merge or overwrite it"
        );
        ensure!(
            !to.starts_with(&from),
            "destination cannot be inside the source project"
        );
        ensure!(
            from.parent().is_some()
                && std::env::var_os("HOME").is_none_or(|home| home != from.as_os_str()),
            "cannot move the filesystem root or home directory"
        );
        ensure!(
            !paths.root.starts_with(&from) && !source.home.starts_with(&from),
            "project contains switcher state or Codex history; choose a narrower project directory"
        );
        ensure!(
            metadata.dev() == fs::metadata(from.parent().context("source parent missing")?)?.dev(),
            "source is a mount point; move a directory inside that filesystem instead"
        );
        let mounts = fs::read_to_string("/proc/self/mountinfo")?;
        ensure!(
            !mounts
                .lines()
                .filter_map(|line| line.split_whitespace().nth(4))
                .any(|mount| {
                    let mount = mount
                        .replace("\\040", " ")
                        .replace("\\011", "\t")
                        .replace("\\012", "\n")
                        .replace("\\134", "\\");
                    Path::new(&mount).starts_with(&from)
                }),
            "project contains a mount point; unmount it before moving the folder"
        );
        if metadata.dev() != target_device {
            ensure!(
                Command::new("rsync")
                    .arg("--version")
                    .stdout(Stdio::null())
                    .status()
                    .is_ok_and(|s| s.success()),
                "cross-filesystem moves require rsync"
            );
        }
    } else {
        ensure!(
            to.is_dir(),
            "the new working directory must already exist (use --move-files to move a folder)"
        );
    }
    Ok(Relocation {
        from,
        to,
        move_files,
        cross_filesystem: metadata.dev() != target_device,
        state: "prepared".into(),
        source_device: metadata.dev(),
        source_inode: metadata.ino(),
        restart,
        backend_note: None,
        stage: None,
        retained_source: None,
        retained_destination: None,
    })
}
async fn selected(paths: &Paths, pane: Option<&str>, thread: Option<&str>) -> Result<Instance> {
    ensure!(
        pane.is_some() != thread.is_some(),
        "specify exactly one of --pane or --thread"
    );
    discover(paths)
        .await?
        .into_iter()
        .find(|i| {
            pane.is_some_and(|p| !i.inactive && i.pane == p)
                || thread.is_some_and(|t| i.inactive && i.thread_id == t)
        })
        .context("selected conversation is no longer available")
}
pub async fn preview(
    paths: &Paths,
    pane: Option<&str>,
    thread: Option<&str>,
    to: &Path,
    move_files: bool,
) -> Result<Value> {
    let source = selected(paths, pane, thread).await?;
    let plan = plan_for(paths, &source, to, move_files, false)?;
    let affected = other_sessions(paths, &source, &plan).await?;
    Ok(
        json!({"schema_version":1,"source":source,"relocation":plan,"other_sessions":affected,"detail":if move_files {"The folder moves after this conversation is idle. Other live sessions using it must close first. Cross-filesystem moves retain an original backup."} else {"Only this conversation's working directory changes. No project files are moved."}}),
    )
}
/// Public admission keeps the exact catalog generation through the queue lock.
pub async fn queue_public(
    paths: &Paths,
    source: Instance,
    to: &Path,
    move_files: bool,
    expected: (&str, &str),
) -> Result<Job> {
    ensure!(!source.inactive, "SESSION_NOT_RUNNING");
    let plan = plan_for(paths, &source, to, move_files, false)?;
    let account = source.account.clone();
    let status = daemon::call(paths, "status", json!({})).await?;
    ensure!(
        status["accounts"]
            .as_array()
            .is_some_and(|accounts| accounts.iter().any(|a| a["name"] == account
                && a["provider"].as_str().unwrap_or("codex") == "codex"
                && a["enabled"] == true
                && a["auth_required"] == false)),
        "ACCOUNT_UNAVAILABLE"
    );
    enqueue_with_id(
        paths,
        source,
        &account,
        MoveOptions::default(),
        Some(plan),
        "directory",
        EnqueuePolicy {
            non_steering: true,
            expected: Some(expected),
            ..Default::default()
        },
    )
}
pub async fn preview_snapshot(
    paths: &Paths,
    source: &Instance,
    to: &Path,
    move_files: bool,
) -> Result<Value> {
    ensure!(!source.inactive, "SESSION_NOT_RUNNING");
    let plan = plan_for(paths, source, to, move_files, false)?;
    let affected = other_sessions(paths, source, &plan).await?;
    Ok(
        json!({"from":plan.from,"to":plan.to,"move_files":move_files,"cross_filesystem":plan.cross_filesystem,"other_sessions":affected,"detail":if move_files {"Move this project folder and restart this session in the destination. Other sessions using the folder must close first. Existing destinations are never overwritten; cross-filesystem moves retain an original backup."} else {"Restart this session in the selected existing directory. No project files are moved. Conversation identity, history and account are retained."}}),
    )
}
pub async fn queue(
    paths: &Paths,
    pane: Option<&str>,
    thread: Option<&str>,
    to: &Path,
    move_files: bool,
    options: MoveOptions,
) -> Result<Job> {
    let source = selected(paths, pane, thread).await?;
    let plan = plan_for(paths, &source, to, move_files, false)?;
    let account = source.account.clone();
    enqueue(paths, source, &account, options, Some(plan), "directory")
}
pub async fn queue_restart(paths: &Paths, pane: &str, options: MoveOptions) -> Result<Job> {
    let source = selected(paths, Some(pane), None).await?;
    let plan = plan_for(paths, &source, Path::new(&source.cwd), false, true)?;
    let account = source.account.clone();
    enqueue(paths, source, &account, options, Some(plan), "restart")
}
async fn other_sessions(paths: &Paths, source: &Instance, r: &Relocation) -> Result<Vec<String>> {
    if !r.move_files {
        return Ok(Vec::new());
    }
    Ok(live_sessions(paths)
        .await?
        .into_iter()
        .filter(|i| {
            !i.inactive && i.key() != source.key() && Path::new(&i.cwd).starts_with(&r.from)
        })
        .map(|i| format!("{} ({})", i.tmux_session, i.pane))
        .collect())
}
fn same_source(r: &Relocation, path: &Path) -> bool {
    fs::symlink_metadata(path).is_ok_and(|m| {
        m.is_dir()
            && !m.file_type().is_symlink()
            && m.dev() == r.source_device
            && m.ino() == r.source_inode
    })
}
/// Linux atomic no-clobber rename; a destination appearing after preflight is never overwritten.
fn rename_new(from: &Path, to: &Path) -> Result<()> {
    let from_c = std::ffi::CString::new(from.as_os_str().as_bytes())?;
    let to_c = std::ffi::CString::new(to.as_os_str().as_bytes())?;
    let rc = unsafe {
        libc::syscall(
            libc::SYS_renameat2,
            libc::AT_FDCWD,
            from_c.as_ptr(),
            libc::AT_FDCWD,
            to_c.as_ptr(),
            libc::RENAME_NOREPLACE,
        )
    };
    if rc != 0 {
        return Err(std::io::Error::last_os_error())
            .with_context(|| format!("cannot rename {} to {}", from.display(), to.display()));
    }
    File::open(from.parent().context("source parent missing")?)?.sync_all()?;
    File::open(to.parent().context("destination parent missing")?)?.sync_all()?;
    Ok(())
}
fn unsupported_rename(error: &anyhow::Error) -> bool {
    error
        .downcast_ref::<std::io::Error>()
        .and_then(std::io::Error::raw_os_error)
        .is_some_and(|code| matches!(code, libc::EINVAL | libc::ENOSYS | libc::EOPNOTSUPP))
}

// Some FUSE filesystems do not support RENAME_NOREPLACE. Exclusively create the
// destination, copy and verify it instead of falling back to a clobbering rename.
async fn publish_directory(from: &Path, to: &Path) -> Result<bool> {
    match rename_new(from, to) {
        Ok(()) => Ok(false),
        Err(error) if unsupported_rename(&error) => {
            copy_new_directory(from, to).await?;
            Ok(true)
        }
        Err(error) => Err(error),
    }
}
async fn copy_new_directory(from: &Path, to: &Path) -> Result<()> {
    fs::create_dir(to).context("destination appeared; no files overwritten")?;
    rsync(from, to, false).await?;
    rsync(from, to, true).await?;
    Ok(())
}
fn transition(paths: &Paths, job: &mut Job, state: &str) -> Result<()> {
    job.relocation.as_mut().context("missing relocation")?.state = state.into();
    save_job(paths, job)
}
#[derive(PartialEq, Debug)]
enum Service {
    PidUpdater,
    Adb,
    Codex,
}
fn cwd_only_service(args: &[String]) -> Option<Service> {
    let binary = Path::new(args.first()?).file_name()?.to_str()?;
    let args: Vec<_> = args.iter().skip(1).map(String::as_str).collect();
    if binary == "codex" {
        if args == ["app-server", "daemon", "pid-update-loop"] {
            return Some(Service::PidUpdater);
        }
        if args
            == [
                "app-server",
                "--listen",
                "unix://",
                "--analytics-default-enabled",
                "--managed-daemon",
            ]
        {
            return Some(Service::Codex);
        }
    }
    if binary == "adb"
        && args.len() == 6
        && args[..5] == ["-L", "tcp:5037", "fork-server", "server", "--reply-fd"]
        && args[5].parse::<u32>().is_ok()
    {
        return Some(Service::Adb);
    }
    None
}
async fn project_users(paths: &Paths, job: &Job, root: &Path) -> Result<Vec<u32>> {
    // HERDR retains the selected pane's idle shell after stopping Codex. Its
    // cwd alone is not an unrelated project user. Never exempt open files,
    // another pane, an active foreground command, or a reused PID.
    let retained_shell = if job.source.pane.starts_with("herdr/") {
        hosting::state(paths, &job.source.pane)
            .ok()
            .filter(|(idle, _)| *idle)
            .and_then(|(_, pid)| process_info(pid).ok().map(|p| (pid, p.start_time)))
    } else {
        None
    };
    let mut users = Vec::new();
    let mut checked_homes = std::collections::HashMap::new();
    for entry in fs::read_dir("/proc")?.flatten() {
        let Some(pid) = entry
            .file_name()
            .to_str()
            .and_then(|s| s.parse::<u32>().ok())
        else {
            continue;
        };
        if pid == std::process::id() {
            continue;
        }
        let cwd = fs::read_link(entry.path().join("cwd")).is_ok_and(|p| p.starts_with(root));
        let open = fs::read_dir(entry.path().join("fd"))
            .ok()
            .is_some_and(|entries| {
                entries
                    .flatten()
                    .any(|fd| fs::read_link(fd.path()).is_ok_and(|p| p.starts_with(root)))
            });
        if !cwd && !open {
            continue;
        }
        // A service can inherit a project's cwd without using the project. Never
        // exempt open file handles, unknown commands, or unverified Codex threads.
        if !open {
            if retained_shell.is_some_and(|(shell, start)| {
                shell == pid && process_info(pid).is_ok_and(|p| p.start_time == start)
            }) {
                continue;
            }
            match process_args(pid).ok().as_deref().and_then(cwd_only_service) {
                Some(Service::PidUpdater | Service::Adb) => continue,
                Some(Service::Codex) => {
                    if let Ok(home) = process_home(pid) {
                        let unused = if let Some(unused) = checked_homes.get(&home) {
                            *unused
                        } else {
                            let checked = async {
                                let mut control = Control::connect(&home).await?;
                                Ok::<_, anyhow::Error>(control.loaded().await?.iter().all(|t| {
                                    t["cwd"].as_str().is_some_and(|cwd| {
                                        fs::canonicalize(cwd).is_ok_and(|p| !p.starts_with(root))
                                    })
                                }))
                            }
                            .await
                            .unwrap_or(false);
                            checked_homes.insert(home, checked);
                            checked
                        };
                        if unused {
                            continue;
                        }
                    }
                }
                None => {}
            }
        }
        users.push(pid);
    }
    Ok(users)
}
async fn rsync(from: &Path, to: &Path, verify: bool) -> Result<()> {
    let mut command = tokio::process::Command::new("rsync");
    command.args(["-aHAX", "--numeric-ids"]);
    if verify {
        command.args(["--checksum", "--dry-run", "--delete", "--itemize-changes"]);
    }
    let output = command
        .arg("--")
        .arg(format!("{}/", from.display()))
        .arg(format!("{}/", to.display()))
        .output()
        .await?;
    ensure!(
        output.status.success(),
        "rsync {} failed; original project retained",
        if verify { "verification" } else { "copy" }
    );
    ensure!(
        !verify || output.stdout.is_empty(),
        "project changed during copying; original retained, retry after other writers stop"
    );
    Ok(())
}
async fn move_files(paths: &Paths, job: &mut Job) -> Result<()> {
    let mut r = job.relocation.clone().context("missing relocation")?;
    if !r.move_files {
        return Ok(());
    }
    ensure!(
        same_source(&r, &r.from),
        "project directory identity changed; no files moved"
    );
    ensure!(
        !r.to.try_exists()?,
        "destination appeared while waiting; no files overwritten"
    );
    let users = project_users(paths, job, &r.from).await?;
    ensure!(
        users.is_empty(),
        "other processes still use this project (PIDs {users:?}); stop them and retry. No project files moved"
    );
    let parent = r.to.parent().context("destination parent missing")?;
    fs::create_dir_all(parent).context("cannot create destination parent directories")?;
    ensure!(
        fs::canonicalize(parent)? == parent,
        "destination parent changed; retry the move"
    );
    ensure!(
        (fs::metadata(parent)?.dev() != r.source_device) == r.cross_filesystem,
        "destination filesystem changed; retry the move"
    );
    phase(
        paths,
        job,
        "relocating_files",
        "Moving the project; the original is retained until the destination is verified.",
    )?;
    if !r.cross_filesystem {
        transition(paths, job, "renaming")?;
        rename_new(&r.from, &r.to)?;
        transition(paths, job, "moved")?;
        return Ok(());
    }
    let stage =
        r.to.parent()
            .context("destination parent missing")?
            .join(format!(".codex-switcher-copy-{}", job.id));
    let backup = r
        .from
        .parent()
        .context("source parent missing")?
        .join(format!(".codex-switcher-original-{}", job.id));
    r.stage = Some(stage.clone());
    r.retained_source = Some(backup.clone());
    job.relocation = Some(r.clone());
    transition(paths, job, "copying")?;
    fs::create_dir(&stage).context("copy staging path already exists")?;
    rsync(&r.from, &stage, false).await?;
    rsync(&r.from, &stage, true).await?;
    ensure!(
        same_source(&r, &r.from) && project_users(paths, job, &r.from).await?.is_empty(),
        "source changed or became active during copying; original retained"
    );
    transition(paths, job, "publishing")?;
    let copied = publish_directory(&stage, &r.to).await?;
    transition(paths, job, "retiring_source")?;
    rename_new(&r.from, &backup)?;
    transition(paths, job, "moved")?;
    if copied {
        // Only our private staging copy is discarded; the verified destination
        // and original backup are already durable job references.
        let _ = fs::remove_dir_all(&stage);
    }
    Ok(())
}
pub(super) async fn perform(paths: &Paths, job: &mut Job) -> Result<()> {
    identify(paths, job).await?;
    let r = job.relocation.clone().context("missing relocation")?;
    ensure!(
        Path::new(&job.source.cwd) == r.from,
        "session changed working directory; select it again"
    );
    let _ = plan_for(paths, &job.source, &r.to, r.move_files, r.restart)?;
    if !job.source.warnings.is_empty() {
        risks(paths, job, job.source.warnings.join(" ")).await?;
    }
    // Serialize project changes, including ancestor/descendant paths.
    let lock = crate::paths::lock_file(&paths.root.join(crate::layout::PROJECT_RELOCATION_LOCK))?;
    while lock.try_lock_exclusive().is_err() {
        phase(
            paths,
            job,
            "waiting_for_project",
            "Another project relocation is in progress. Waiting without changing this session.",
        )?;
        tick(paths, job).await?;
    }
    loop {
        let others = other_sessions(paths, &job.source, &r).await?;
        if others.is_empty() {
            break;
        }
        phase(
            paths,
            job,
            "waiting_for_project",
            format!(
                "Other live sessions use this folder: {}. Close them, or cancel and change only this conversation's working directory.",
                others.join(", ")
            ),
        )?;
        tick(paths, job).await?;
    }
    let files = wait_idle(paths, job).await?;
    ensure!(
        other_sessions(paths, &job.source, &r).await?.is_empty(),
        "another session started using the project; retry after it closes"
    );
    stop_source(paths, job, files).await?;
    move_files(paths, job).await?;
    let home = job.source.home.clone();
    if r.restart && crate::control::available(&home) {
        let mut current = Control::connect(&home).await?;
        let other_panes = live_sessions(paths).await?.iter().any(|i| i.home == home);
        let other_threads = !current.loaded().await?.is_empty();
        let note = if job.source.pane.starts_with("herdr/") {
            "The HERDR terminal was restarted; the existing account backend remains owned by its supervisor."
        } else if other_panes || other_threads {
            "The shared backend stays running because other sessions use it. This terminal uses the installed Codex binary."
        } else if matches!(job.source.account.as_str(), "external" | "unknown") {
            "The external shared backend stays running; its lifecycle is not owned by this switcher."
        } else {
            for attempt in 0..50 {
                match daemon::call(
                    paths,
                    "account/restart-server",
                    json!({"name":job.source.account}),
                )
                .await
                {
                    Ok(_) => break,
                    Err(error)
                        if error.to_string().contains("poll in progress") && attempt < 49 =>
                    {
                        tokio::time::sleep(Duration::from_millis(200)).await
                    }
                    Err(error) => return Err(error),
                }
            }
            "The unused account backend was restarted too."
        };
        job.relocation.as_mut().unwrap().backend_note = Some(note.into());
        save_job(paths, job)?;
    }
    let mut control = if job.source.shared_server || crate::control::available(&home) {
        Some(Control::connect(&home).await?)
    } else {
        None
    };
    if job.source_archived {
        let c = control.as_mut().context("source control missing")?;
        c.request("thread/unarchive", json!({"threadId":job.source.thread_id}))
            .await?;
        job.source.rollout = PathBuf::from(
            c.thread(&job.source.thread_id, false).await?["path"]
                .as_str()
                .context("unarchived history missing")?,
        );
        job.source_archived = false;
        save_job(paths, job)?;
    }
    let rollout = job.source.rollout.clone();
    launch_target(paths, job, &home, &rollout, control.as_mut()).await
}
pub(super) fn record_location(paths: &Paths, job: &mut Job) -> Result<()> {
    let Some(r) = job.relocation.as_ref() else {
        return Ok(());
    };
    save_location(paths, &job.source.thread_id, &r.to)?;
    transition(paths, job, "complete")
}
pub(crate) fn save_location(paths: &Paths, thread: &str, directory: &Path) -> Result<()> {
    let dir = paths.root.join(crate::layout::THREAD_LOCATIONS);
    private_dir(&dir)?;
    uuid::Uuid::parse_str(thread)?;
    atomic_write(
        &dir.join(format!("{thread}.json")),
        &serde_json::to_vec(&json!({"cwd":directory}))?,
    )
}
pub(super) fn saved_location(paths: &Paths, thread: &str) -> Option<String> {
    uuid::Uuid::parse_str(thread).ok()?;
    let v: Value = serde_json::from_slice(
        &fs::read(
            paths
                .root
                .join(crate::layout::THREAD_LOCATIONS)
                .join(format!("{thread}.json")),
        )
        .ok()?,
    )
    .ok()?;
    v["cwd"].as_str().map(str::to_owned)
}
pub(super) async fn recover(paths: &Paths, mut job: Job) -> Result<Job> {
    ensure!(
        job.phase == "failed"
            || (!handoff_worker_alive(&job.id)
                && !matches!(job.phase.as_str(), "complete" | "cancelled" | "recovered")),
        "only failed or interrupted moves need recovery"
    );
    if !job.source.pane.is_empty() {
        ensure!(
            pane_state(paths, &job.source.pane).map_or(true, |(dead, _)| dead),
            "the conversation is still running; close it before rolling back the folder"
        );
    }
    if job.target_loaded {
        let mut control = Control::connect(&job.source.home).await?;
        let thread = control.thread(&job.source.thread_id, false).await?;
        control
            .verify_unloadable(&job.source.thread_id, true)
            .await?;
        if thread["status"]["type"] != "notLoaded" {
            control
                .request("thread/archive", json!({"threadId":job.source.thread_id}))
                .await?;
            job.source_archived = true;
        }
        job.target_loaded = false;
        save_job(paths, &job)?;
    }
    let r = job.relocation.clone().context("missing relocation")?;
    if r.move_files {
        if !r.cross_filesystem && same_source(&r, &r.to) {
            ensure!(
                project_users(paths, &job, &r.to).await?.is_empty(),
                "processes use the destination; close them before rollback"
            );
            rename_new(&r.to, &r.from)?;
        } else if r.cross_filesystem {
            if let Some(backup) = &r.retained_source {
                if same_source(&r, backup) {
                    rename_new(backup, &r.from)?;
                }
            }
            if r.to.exists()
                && matches!(r.state.as_str(), "publishing" | "retiring_source" | "moved")
            {
                let retained =
                    r.to.parent()
                        .context("destination parent missing")?
                        .join(format!(".codex-switcher-recovery-{}", job.id));
                ensure!(
                    project_users(paths, &job, &r.to).await?.is_empty(),
                    "processes use the destination; close them before rollback"
                );
                job.relocation.as_mut().unwrap().retained_destination =
                    Some(match rename_new(&r.to, &retained) {
                        Ok(()) => retained,
                        // Leave the copy intact when this filesystem cannot rename
                        // without clobbering. Recovery still restores the original.
                        Err(error) if unsupported_rename(&error) => r.to.clone(),
                        Err(error) => return Err(error),
                    });
            }
        }
        ensure!(
            same_source(&r, &r.from),
            "original project identity cannot be verified; recovery needs manual inspection"
        );
    }
    transition(paths, &mut job, "rolled_back")?;
    restore_source(paths, job).await
}

pub(super) fn restart_plan(paths: &Paths, source: &Instance) -> Result<Relocation> {
    plan_for(paths, source, Path::new(&source.cwd), false, true)
}

#[cfg(test)]
mod tests {
    use super::*;
    #[tokio::test]
    async fn portable_publication_preserves_files_and_never_merges() {
        let t = tempfile::tempdir().unwrap();
        let from = t.path().join("staging");
        let to = t.path().join("destination");
        fs::create_dir(&from).unwrap();
        fs::write(from.join("file"), "unsaved changes").unwrap();
        std::os::unix::fs::symlink("file", from.join("link")).unwrap();
        copy_new_directory(&from, &to).await.unwrap();
        assert_eq!(fs::read(to.join("file")).unwrap(), b"unsaved changes");
        assert_eq!(fs::read_link(to.join("link")).unwrap(), Path::new("file"));
        assert!(from.join("file").exists());
        fs::write(from.join("file"), "different").unwrap();
        assert!(copy_new_directory(&from, &to).await.is_err());
        assert_eq!(fs::read(to.join("file")).unwrap(), b"unsaved changes");
    }
    #[test]
    fn missing_parents_resolve_without_writes_and_reject_broken_symlinks() {
        use std::os::unix::fs::symlink;
        let t = tempfile::tempdir().unwrap();
        let target = t.path().join("new/nested/project");
        assert_eq!(destination(&target).unwrap(), target);
        assert!(!target.parent().unwrap().exists());
        symlink(t.path().join("absent"), t.path().join("broken")).unwrap();
        assert!(destination(&t.path().join("broken/project")).is_err());
        assert!(destination(&t.path().join("new/../project")).is_err());
    }
    #[test]
    fn only_known_background_service_commands_can_ignore_inherited_cwd() {
        let service = |argv: &[&str]| {
            cwd_only_service(&argv.iter().map(|s| s.to_string()).collect::<Vec<_>>())
        };
        assert_eq!(
            service(&["/bin/codex", "app-server", "daemon", "pid-update-loop"]),
            Some(Service::PidUpdater)
        );
        assert_eq!(
            service(&[
                "adb",
                "-L",
                "tcp:5037",
                "fork-server",
                "server",
                "--reply-fd",
                "4"
            ]),
            Some(Service::Adb)
        );
        assert_eq!(service(&["adb", "install", "project.apk"]), None);
        assert_eq!(service(&["codex", "resume", "thread"]), None);
        assert_eq!(
            service(&["other", "app-server", "daemon", "pid-update-loop"]),
            None
        );
    }
    #[test]
    fn atomic_rename_never_replaces_existing_destination() {
        let t = tempfile::tempdir().unwrap();
        let from = t.path().join("from");
        let to = t.path().join("to");
        fs::create_dir(&from).unwrap();
        fs::create_dir(&to).unwrap();
        assert!(rename_new(&from, &to).is_err());
        assert!(from.exists() && to.exists());
    }
}
