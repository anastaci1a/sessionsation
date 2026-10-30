//! Open exact conversations in their existing home. No turn is submitted.
use super::*;

pub fn archives(paths: &Paths) -> Result<Vec<Instance>> {
    let mut items = inventory_kind(paths, true, true)?;
    catalog::decorate(paths, &mut items)?;
    access::sort(paths, &mut items)?;
    Ok(items)
}
pub fn archives_snapshot(paths: &Paths) -> Result<Value> {
    Ok(snapshot(archives(paths)?))
}

fn opened(paths: &Paths, source: &Instance) -> Result<Value> {
    if !source.thread_id.is_empty() {
        access::record(paths, &source.thread_id)?;
    }
    Ok(
        json!({"schema_version":1,"session":{"id":format!("pane:{}",source.pane),"pane":source.pane,"tmux_session":source.tmux_session,"thread_id":source.thread_id,"account":source.account,"display_account":source.display_account}}),
    )
}

pub(super) fn tmux_label(name: &str) -> String {
    let mut safe = String::new();
    for c in name.chars() {
        if safe.len() + c.len_utf8() > 128 {
            break;
        }
        safe.push(if c.is_alphanumeric() || " _+-=~()[]".contains(c) {
            c
        } else {
            '-'
        });
    }
    if safe.starts_with("codex-") {
        safe
    } else {
        format!("codex-{safe}")
    }
}
pub(super) fn session_name(
    paths: &Paths,
    cwd: &str,
    thread: &str,
    name: Option<&str>,
) -> Result<String> {
    let saved = catalog::saved_name(paths, thread)?;
    let mut chosen_name = name.map(tmux_label).unwrap_or_else(|| {
        tmux_label(saved.as_deref().unwrap_or_else(|| {
            Path::new(cwd)
                .file_name()
                .and_then(|s| s.to_str())
                .unwrap_or("saved")
        }))
    });
    ensure!(
        !chosen_name.is_empty()
            && chosen_name.len() <= 160
            && chosen_name
                .chars()
                .all(|c| c.is_alphanumeric() || " _+-=~()[]".contains(c)),
        "tmux name contains unsupported characters"
    );
    let base = chosen_name.clone();
    for suffix in 2..102 {
        if !Command::new("tmux")
            .args(["has-session", "-t", &format!("={chosen_name}")])
            .output()?
            .status
            .success()
        {
            break;
        }
        if tmux_inventory::release_ended_name(&chosen_name).is_ok() {
            break;
        }
        ensure!(
            name.is_none() && suffix < 101,
            "tmux session name is already in use; choose another --name"
        );
        chosen_name = format!("{base}-{suffix}");
    }
    Ok(chosen_name)
}

pub async fn open(paths: &Paths, key: &str, name: Option<&str>) -> Result<Value> {
    if key.starts_with("tmux:") {
        return opened(paths, &tmux_inventory::open_plain(paths, key)?);
    }
    if !key.starts_with("archive:") {
        if let Some(source) = live_sessions(paths).await?.into_iter().find(|s| {
            s.key() == key
                || key
                    .strip_prefix("thread:")
                    .is_some_and(|id| id == s.thread_id)
        }) {
            return opened(paths, &source);
        }
        ensure!(
            !key.starts_with("pane:"),
            "session is no longer available; refresh the inventory"
        );
    }
    let mut source = if key.starts_with("archive:") {
        archives(paths)?
    } else {
        discover(paths).await?
    }
    .into_iter()
    .find(|s| {
        s.key() == key
            || key
                .strip_prefix("thread:")
                .is_some_and(|id| id == s.thread_id)
    })
    .context("session is no longer available; refresh the inventory")?;
    if !source.inactive {
        return opened(paths, &source);
    }
    ensure!(
        source.move_id.is_none(),
        "this conversation has a pending job; finish or cancel it first"
    );
    let _lock = catalog::operation_lock(paths, &source.thread_id)?;
    catalog::ensure_available(paths, &source.thread_id)?;
    let previous_stop = catalog::stop_intent(paths, &source.thread_id)?;
    if let Some(live) = live_sessions(paths)
        .await?
        .into_iter()
        .find(|s| s.thread_id == source.thread_id)
    {
        ensure!(
            !source.archived,
            "a copy of this conversation is live; open that session instead of restoring an older archive"
        );
        return opened(paths, &live);
    }
    ensure!(
        Path::new(&source.cwd).is_dir(),
        "the recorded project directory is missing; restore it or relocate this saved conversation first"
    );
    catalog::remember(paths, &source)?;
    let chosen_name = session_name(paths, &source.cwd, &source.thread_id, name)?;
    if let Some(name) = name {
        catalog::set_name(paths, &source.thread_id, name)?;
    }
    let mut control = if !matches!(source.account.as_str(), "external" | "unknown") {
        daemon::ensure_started(paths).await?;
        Some(account_control(paths, &source.account).await?)
    } else if crate::control::available(&source.home) {
        Some(crate::control::Control::connect(&source.home).await?)
    } else {
        None
    };
    let config = crate::store::load_config(paths)?;
    let mode = config.effective_launch_permissions();
    let permissions = start_permissions(mode, recorded_permissions(&source.rollout));
    if let Some(control) = control.as_mut() {
        let thread = control.thread_for_state(&source.thread_id).await?;
        ensure!(
            crate::control::confirmed_quiescent(&thread, true),
            "the conversation is still working in Codex; wait before opening another terminal"
        );
        if source.archived {
            control
                .request("thread/unarchive", json!({"threadId":source.thread_id}))
                .await?;
            source.rollout = PathBuf::from(
                control.thread(&source.thread_id, false).await?["path"]
                    .as_str()
                    .context("restored history path missing")?,
            );
        }
        let mut params = json!({"threadId":source.thread_id,"cwd":source.cwd,"excludeTurns":true});
        set_permissions(&mut params, permissions);
        control.resume_with_permissions(params).await?;
    } else {
        ensure!(
            !source.archived,
            "archive recovery requires the Codex control server for this home; the archive remains intact"
        );
        ensure!(
            !rollout_still_open(&source.rollout)?,
            "another process owns this conversation; wait before opening it"
        );
    }
    let command = if control.is_some() {
        super::movement::remote_resume_shell(
            &source.home,
            &config.codex_binary,
            &source.resume_options,
            &source.thread_id,
        )
    } else {
        resume_shell(
            &source.home,
            &config.codex_binary,
            &source.resume_options,
            &source.thread_id,
            &source.rollout,
            mode,
        )
    };
    // Keep startup diagnostics, while a normal /exit still removes the pane.
    let command = format!(
        "tmux set-option -p remain-on-exit failed; {command} -C {}",
        shell_quote(&source.cwd)
    );
    source.pane = tmux(&[
        "new-session",
        "-d",
        "-P",
        "-F",
        "#{pane_id}",
        "-s",
        &chosen_name,
        "-c",
        &source.cwd,
        &command,
    ])?;
    source.tmux_session = chosen_name;
    for _ in 0..50 {
        let (dead, pid) = pane_state(paths, &source.pane)?;
        ensure!(
            !dead,
            "Codex exited during startup; its error is retained in tmux and its history is unchanged"
        );
        if process_home(pid).is_ok_and(|h| crate::layout::same_home(&h, &source.home))
            && resumed_thread(pid)?.as_deref() == Some(&source.thread_id)
        {
            bind_pane(paths, &source.pane, pid, &source.home, &source.thread_id)?;
            tokio::time::sleep(Duration::from_millis(500)).await;
            ensure!(
                !pane_state(paths, &source.pane)?.0,
                "Codex exited during startup; inspect the retained tmux pane"
            );
            catalog::clear_stop_intent(paths, &source.thread_id, previous_stop.as_deref())?;
            return opened(paths, &source);
        }
        tokio::time::sleep(Duration::from_millis(100)).await;
    }
    bail!(
        "Codex did not confirm its conversation identity; inspect the new tmux pane before retrying"
    )
}
