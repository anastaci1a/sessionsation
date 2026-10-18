//! Terminal hosting only. Provider authentication and conversation preparation stay
//! in the account-aware launcher and movement worker.
use super::*;
use sha2::{Digest, Sha256};

#[derive(Serialize, Deserialize)]
struct HerdrBinding {
    target: String,
    binding: Binding,
}

pub use crate::terminal::herdr_target;
use crate::terminal::{call, target};
pub(super) use crate::terminal::{relaunch, retain, state};

pub(super) fn bind(paths: &Paths, pane: &str, binding: Binding) -> Result<()> {
    target(pane)?;
    let dir = paths.root.join("herdr-panes");
    private_dir(&dir)?;
    let key = format!("{:x}", Sha256::digest(pane.as_bytes()));
    atomic_write(
        &dir.join(format!("{key}.json")),
        &serde_json::to_vec(&HerdrBinding {
            target: pane.into(),
            binding,
        })?,
    )
}

pub(super) fn instances(paths: &Paths) -> Result<Vec<Instance>> {
    let dir = paths.root.join("herdr-panes");
    if !dir.exists() {
        return Ok(vec![]);
    }
    let mut result = Vec::new();
    for entry in fs::read_dir(dir)? {
        let entry = entry?;
        if entry.path().extension().is_none_or(|e| e != "json") {
            continue;
        }
        ensure!(!entry.file_type()?.is_symlink(), "unsafe HERDR binding");
        let record: HerdrBinding = serde_json::from_slice(&fs::read(entry.path())?)?;
        let b = record.binding;
        // Read-only discovery never starts a server and never infers ownership
        // from the UI's selected pane or an account's display name.
        let Ok((false, pid)) = state(&record.target) else {
            continue;
        };
        let Ok(info) = process_info(pid) else {
            continue;
        };
        if pid != b.pid
            || info.start_time != b.start_time
            || process_home(pid).ok().as_ref() != Some(&b.home)
            || resumed_thread(pid)?.as_deref() != Some(&b.thread)
        {
            continue;
        }
        let Some(rollout) = rollout_by_thread(&b.home, &b.thread)? else {
            continue;
        };
        let launch = parse_launch(&process_args(pid)?);
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
            detail: activity_detail(activity).into(),
            warnings: vec![],
            shared_server: true,
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
            home: b.home,
            binary: launch.binary,
            resume_options: launch.options,
        });
    }
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
    launch_inner(paths, session, account, thread, options, None).await
}
pub async fn launch_checked(
    paths: &Paths,
    session: &str,
    account: &str,
    thread: Option<&str>,
    options: &[String],
    expected: (&str, &str),
) -> Result<Value> {
    launch_inner(paths, session, account, thread, options, Some(expected)).await
}
async fn launch_inner(
    paths: &Paths,
    session: &str,
    account: &str,
    thread: Option<&str>,
    options: &[String],
    expected: Option<(&str, &str)>,
) -> Result<Value> {
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
                && a["enabled"] == true
                && a["auth_required"] == false)),
        "ACCOUNT_UNAVAILABLE"
    );
    let cwd = std::env::current_dir()?;
    // Probe before creating topology. An absent server is an explicit error;
    // starting/stopping headless hosting is an owner's separate operation.
    call(session, &["api", "snapshot"])?;
    let created = call(
        session,
        &[
            "workspace",
            "create",
            "--cwd",
            cwd.to_str().context("working directory is not UTF-8")?,
            "--label",
            "sessionsation",
            "--env",
            "SESSIONSATION_LAUNCHER_BYPASS=1",
            "--no-focus",
        ],
    )?;
    let pane = created["root_pane"]["pane_id"]
        .as_str()
        .context("HERDR pane ID missing")?;
    let key = herdr_target(session, pane)?;
    let mut ready = false;
    for _ in 0..50 {
        if state(&key).is_ok_and(|(idle, _)| idle) {
            ready = true;
            break;
        }
        tokio::time::sleep(Duration::from_millis(100)).await;
    }
    ensure!(ready, "HERDR_SHELL_UNAVAILABLE: {key}");
    let exe = std::env::current_exe()?;
    let mut args = vec![
        exe.to_string_lossy().into_owned(),
        "--data-dir".into(),
        paths.root.to_string_lossy().into_owned(),
        "run".into(),
        "--account".into(),
        account.into(),
    ];
    if let Some((id, generation)) = expected {
        args.truncate(3);
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
    let command = format!(
        "cd -- {} && env -u TMUX -u TMUX_PANE SESSIONSATION_HERDR_SESSION={} {}",
        shell_quote(&cwd.to_string_lossy()),
        shell_quote(session),
        args.iter()
            .map(|s| shell_quote(s))
            .collect::<Vec<_>>()
            .join(" ")
    );
    call(session, &["pane", "run", pane, &command])?;
    for _ in 0..150 {
        if let Some(i) = instances(paths)?.into_iter().find(|i| i.pane == key) {
            return Ok(
                json!({"schema_version":1,"state":"running","host":"herdr", "target":key,"account":account,"thread_id":i.thread_id,"viewer_attach":["herdr","session","attach",session]}),
            );
        }
        tokio::time::sleep(Duration::from_millis(100)).await;
    }
    Ok(
        json!({"schema_version":1,"state":"launch_unconfirmed","host":"herdr",
        "target":key,"account":account,"thread_id":thread,
        "viewer_attach":["herdr","session","attach",session]}),
    )
}
