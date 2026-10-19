//! One explicitly admitted launch; status and retries never run this worker.
use super::*;
pub async fn worker(paths: &Paths, id: &str) -> anyhow::Result<()> {
    let lock = lock_file(&file(paths, id)?.with_extension("worker.lock"))?;
    lock.try_lock_exclusive()?;
    let mut r = read(paths, id)?;
    ensure!(r.phase == "admitted", "RECOVERY_ALREADY_DISPATCHED");
    r.phase = "preparing".into();
    save(paths, &r)?;
    let result = execute(paths, &mut r).await;
    if let Err(e) = result {
        r.phase = if matches!(r.phase.as_str(), "preparing" | "launch_confirmed") {
            "failed"
        } else {
            "uncertain"
        }
        .into();
        r.detail = Some(format!("{e:#}"));
        save(paths, &r)?;
    }
    Ok(())
}
async fn execute(paths: &Paths, r: &mut Record) -> anyhow::Result<()> {
    validate(paths, &r.request, Some(&r.request.message_id)).await?;
    if let Some(p) = intent(&r.request)?.launch.clone() {
        r.phase = "creating_terminal".into();
        save(paths, r)?;
        let q = crate::terminal::shell_quote;
        let command = format!(
            "cd -- {} && SESSIONSATION_LAUNCHER_BYPASS=1 {} --data-dir {} message-recovery-run {}",
            q(p.directory.to_str().context("RECOVERY_INVALID_DIRECTORY")?),
            q(std::env::current_exe()?
                .to_str()
                .context("RECOVERY_INVALID_EXECUTABLE")?),
            q(paths.root.to_str().context("RECOVERY_INVALID_DATA_ROOT")?),
            q(&r.request.message_id)
        );
        match &p.host {
            Host::Herdr {
                session,
                socket,
                workspace_id,
            } => {
                let h = crate::launch::Host {
                    session: session.clone(),
                    socket: socket.clone(),
                };
                let created = crate::launch::create_terminal(
                    &h,
                    &crate::launch::Space::Existing {
                        id: workspace_id.clone(),
                    },
                    &p.directory,
                    &r.session.name,
                )
                .await?;
                let pane = created["root_pane"]["pane_id"]
                    .as_str()
                    .context("RECOVERY_HOST_PANE_MISSING")?;
                ensure!(
                    created["tab"]["workspace_id"] == *workspace_id,
                    "RECOVERY_HOST_WRONG_SPACE"
                );
                let target = crate::terminal::herdr_target(session, pane)?;
                r.target = Some(target.clone());
                r.phase = "dispatching".into();
                save(paths, r)?;
                let mut ready = false;
                for _ in 0..50 {
                    if crate::terminal::state_at(&target, Some(socket)).is_ok_and(|v| v.0) {
                        ready = true;
                        break;
                    }
                    tokio::time::sleep(std::time::Duration::from_millis(100)).await;
                }
                ensure!(ready, "RECOVERY_NEW_TERMINAL_NOT_READY");
                let command=command.replace(" SESSIONSATION_LAUNCHER_BYPASS=1 ",&format!(" env -u TMUX -u TMUX_PANE SESSIONSATION_HERDR_SESSION={} HERDR_SOCKET_PATH={} SESSIONSATION_LAUNCHER_BYPASS=1 ",q(session),q(socket.to_str().context("RECOVERY_INVALID_SOCKET")?)));
                crate::launch::host_call(&h, &["pane", "run", pane, &command]).await?;
            }
            Host::Tmux { socket, session } => {
                let pane = tmux(
                    socket,
                    &[
                        "new-session",
                        "-d",
                        "-s",
                        session,
                        "-c",
                        p.directory.to_str().context("RECOVERY_INVALID_DIRECTORY")?,
                        "-P",
                        "-F",
                        "#{pane_id}",
                        "bash --noprofile --norc",
                    ],
                )?;
                ensure!(
                    pane.starts_with('%') && pane[1..].bytes().all(|b| b.is_ascii_digit()),
                    "RECOVERY_HOST_PANE_MISSING"
                );
                tmux(
                    socket,
                    &["set-option", "-p", "-t", &pane, "remain-on-exit", "on"],
                )?;
                r.target = Some(pane.clone());
                r.phase = "dispatching".into();
                save(paths, r)?;
                tmux(socket, &["send-keys", "-t", &pane, "-l", &command])?;
                tmux(socket, &["send-keys", "-t", &pane, "Enter"])?;
            }
        }
        for _ in 0..1300 {
            let current = read(paths, &r.request.message_id)?;
            if current.launched_generation.is_some() {
                *r = current;
                break;
            }
            tokio::time::sleep(std::time::Duration::from_millis(100)).await;
        }
        ensure!(
            r.launched_generation.is_some(),
            "RECOVERY_LAUNCH_ACK_UNCERTAIN_DO_NOT_RETRY"
        );
        let mut confirmed = false;
        for _ in 0..50 {
            if external::new_launch_observed(
                paths,
                &crate::catalog::resolve(
                    paths,
                    &r.session.id,
                    r.launched_generation.as_deref().unwrap(),
                )?,
            )
            .unwrap_or(false)
            {
                confirmed = true;
                break;
            }
            tokio::time::sleep(std::time::Duration::from_millis(100)).await;
        }
        ensure!(
            confirmed,
            "RECOVERY_PROVIDER_EXEC_NOT_CONFIRMED_DO_NOT_RETRY"
        );
    } else {
        r.launched_generation = Some(r.session.generation());
    }
    r.launch_confirmed = true;
    r.phase = "launch_confirmed".into();
    save(paths, r)?;
    public::deliver_recovered(
        paths,
        r.request.clone(),
        r.launched_generation.as_deref().unwrap(),
    )
    .await?;
    r.phase = "complete".into();
    save(paths, r)?;
    Ok(())
}
pub async fn run(paths: &Paths, id: &str) -> anyhow::Result<()> {
    let r = read(paths, id)?;
    ensure!(
        r.request.expires_at > crate::daemon::now(),
        "RECOVERY_REQUEST_EXPIRED_NO_LAUNCH"
    );
    ensure!(r.phase == "dispatching", "RECOVERY_ALREADY_DISPATCHED");
    let p = intent(&r.request)?
        .launch
        .as_ref()
        .context("RECOVERY_PLAN_MISSING")?;
    let host_socket = match &p.host {
        Host::Herdr { socket, .. } | Host::Tmux { socket, .. } => socket,
    };
    ensure!(
        identity(host_socket)? == p.host_identity,
        "RECOVERY_HOST_RESTARTED_REVALIDATE"
    );
    match &p.host {
        Host::Herdr {
            session, socket, ..
        } => {
            let pane = std::env::var("HERDR_PANE_ID").context("RECOVERY_WRONG_HOST")?;
            ensure!(
                r.target.as_ref() == Some(&crate::terminal::herdr_target(session, &pane)?),
                "RECOVERY_WRONG_PANE"
            );
            ensure!(
                std::env::var_os("HERDR_SOCKET_PATH").as_deref() == Some(socket.as_os_str()),
                "RECOVERY_WRONG_HOST_SOCKET"
            );
        }
        Host::Tmux { socket, .. } => {
            ensure!(
                std::env::var("TMUX_PANE").ok() == r.target,
                "RECOVERY_WRONG_PANE"
            );
            let current = tmux(
                socket,
                &[
                    "display-message",
                    "-p",
                    "-t",
                    r.target.as_deref().context("RECOVERY_WRONG_PANE")?,
                    "#{pane_pid}",
                ],
            )?;
            ensure!(
                current.parse::<i32>()? == unsafe { libc::getppid() },
                "RECOVERY_WRONG_HOST_SOCKET"
            );
        }
    }
    ensure!(
        identity(&p.directory)? == p.directory_identity
            && hash(fs::read(&p.executable)?) == p.executable_sha256,
        "RECOVERY_LAUNCH_MATERIAL_CHANGED"
    );
    external::run_account_message_recovery(
        paths,
        &r.session.account,
        &r.session.conversation_id,
        (&r.session.id, &r.request.expected_generation),
        id,
    )
    .await
}
pub(crate) fn launch_material(paths: &Paths, id: &str) -> anyhow::Result<(PathBuf, Vec<String>)> {
    let r = read(paths, id)?;
    ensure!(r.phase == "dispatching", "RECOVERY_ALREADY_DISPATCHED");
    let p = intent(&r.request)?
        .launch
        .as_ref()
        .context("RECOVERY_PLAN_MISSING")?;
    ensure!(
        home_attachment(paths, &r.session)?.generation() == p.native_endpoint_generation,
        "RECOVERY_NATIVE_OWNER_CHANGED_REVALIDATE"
    );
    ensure!(
        fs::canonicalize(std::env::current_exe()?)? == p.launcher_executable
            && hash(fs::read(&p.launcher_executable)?) == p.launcher_sha256,
        "RECOVERY_BACKEND_ARTIFACT_CHANGED_REVALIDATE"
    );
    let configured = crate::store::load_config(paths)?.codex_binary;
    ensure!(
        executable(&configured)? == p.executable
            && hash(fs::read(&p.executable)?) == p.executable_sha256,
        "RECOVERY_EXECUTABLE_CHANGED_REVALIDATE"
    );
    ensure!(
        identity(&p.directory)? == p.directory_identity && std::env::current_dir()? == p.directory,
        "RECOVERY_DIRECTORY_CHANGED_REVALIDATE"
    );
    let home = paths.account(&r.session.account)?;
    let rollout = external::recovery_rollout(&home, &r.session.conversation_id)?
        .context("RECOVERY_HISTORY_MISSING")?;
    ensure!(
        hash(fs::read(&rollout)?) == p.history_sha256,
        "RECOVERY_HISTORY_CHANGED_REVALIDATE"
    );
    let (approval, sandbox) =
        external::recovery_permissions(&rollout).context("RECOVERY_PERMISSIONS_MISSING")?;
    Ok((
        p.executable.clone(),
        vec![
            "-C".into(),
            p.directory.to_string_lossy().into_owned(),
            "-a".into(),
            approval,
            "-s".into(),
            sandbox,
        ],
    ))
}
pub(crate) fn attached(paths: &Paths, id: &str, s: &crate::catalog::Session) -> anyhow::Result<()> {
    let mut r = read(paths, id)?;
    ensure!(
        r.phase == "dispatching"
            && s.id == r.session.id
            && s.account == r.session.account
            && s.conversation_id == r.session.conversation_id,
        "RECOVERY_IDENTITY_MISMATCH"
    );
    ensure!(
        s.attachment
            .as_ref()
            .is_some_and(|a| Some(&a.target) == r.target.as_ref()),
        "RECOVERY_ATTACHMENT_MISMATCH"
    );
    r.launched_generation = Some(s.generation());
    save(paths, &r)
}
