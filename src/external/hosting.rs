//! Terminal hosting only. Provider authentication and conversation preparation stay
//! in the account-aware launcher and movement worker.
use super::*;
use sha2::{Digest, Sha256};

#[derive(Serialize, Deserialize)]
struct HerdrBinding {
    target: String,
    binding: Binding,
}

pub fn herdr_target(session: &str, pane: &str) -> Result<String> {
    validate_name(session)?;
    ensure!(
        pane.split_once(':').is_some_and(|(w, p)| w.starts_with('w')
            && p.starts_with('p')
            && w[1..].bytes().all(|c| c.is_ascii_digit())
            && p[1..].bytes().all(|c| c.is_ascii_digit())
            && w.len() > 1
            && p.len() > 1),
        "invalid HERDR pane ID"
    );
    Ok(format!("herdr/{session}/{pane}"))
}

fn target(key: &str) -> Result<(&str, &str)> {
    let (session, pane) = key
        .strip_prefix("herdr/")
        .and_then(|s| s.split_once('/'))
        .context("invalid HERDR target")?;
    ensure!(herdr_target(session, pane)? == key, "invalid HERDR target");
    Ok((session, pane))
}

fn call(session: &str, args: &[&str]) -> Result<Value> {
    validate_name(session)?;
    let output = Command::new("herdr")
        .args(["--session", session])
        .args(args)
        .output()
        .context("HERDR is unavailable")?;
    ensure!(
        output.status.success(),
        "HERDR request failed: {}",
        String::from_utf8_lossy(&output.stderr)
    );
    decode_reply(args, &output.stdout)
}

fn decode_reply(args: &[&str], stdout: &[u8]) -> Result<Value> {
    // pane run has no stdout on successful admission in HERDR 0.9.5.
    if stdout.is_empty() && args.starts_with(&["pane", "run"]) {
        return Ok(json!({"admitted":true}));
    }
    let value: Value = serde_json::from_slice(stdout).context("HERDR returned invalid JSON")?;
    ensure!(value.get("error").is_none(), "HERDR rejected request");
    Ok(value["result"].clone())
}

pub(super) fn state(key: &str) -> Result<(bool, u32)> {
    let (session, pane) = target(key)?;
    let result = call(session, &["pane", "process-info", "--pane", pane])?;
    process_state(&result["process_info"])
}

fn process_state(info: &Value) -> Result<(bool, u32)> {
    let shell = info["shell_pid"]
        .as_u64()
        .context("HERDR shell PID missing")? as u32;
    let processes = info["foreground_processes"]
        .as_array()
        .context("HERDR foreground processes missing")?;
    // An idle shell is a retained terminal, not a running agent. Never run a
    // replacement over an editor or an unrelated foreground command.
    if processes.len() == 1
        && processes[0]["pid"].as_u64() == Some(shell as u64)
        && matches!(
            processes[0]["name"].as_str(),
            Some("bash" | "sh" | "zsh" | "fish")
        )
    {
        return Ok((true, shell));
    }
    let codex = processes
        .iter()
        .find(|p| p["name"] == "codex")
        .or_else(|| processes.first())
        .context("HERDR foreground is unknown")?;
    Ok((
        false,
        codex["pid"].as_u64().context("HERDR PID missing")? as u32,
    ))
}

pub(super) fn retain(pane: &str, on: bool) -> Result<()> {
    if pane.starts_with("herdr/") {
        target(pane)?;
        // HERDR's parent shell remains alive after its agent exits.
        return Ok(());
    }
    tmux(&[
        "set-option",
        "-p",
        "-t",
        pane,
        "remain-on-exit",
        if on { "on" } else { "off" },
    ])?;
    Ok(())
}

pub(super) fn relaunch(pane: &str, cwd: &str, command: &str) -> Result<()> {
    if pane.starts_with("herdr/") {
        let (session, id) = target(pane)?;
        ensure!(
            state(pane)?.0,
            "HERDR pane is not an available shell; refresh the target"
        );
        // Keep the host shell for restart and recovery. cwd is enforced here:
        // login shells can reset the directory requested at workspace creation.
        let command = format!(
            "cd -- {} && {}",
            shell_quote(cwd),
            command.strip_prefix("exec ").unwrap_or(command)
        );
        call(session, &["pane", "run", id, &command])?;
    } else {
        tmux(&["respawn-pane", "-k", "-t", pane, "-c", cwd, command])?;
    }
    Ok(())
}

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
    if let Some(id) = thread {
        args.extend(["--thread".into(), id.into()]);
    }
    args.push("--".into());
    args.extend_from_slice(options);
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

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn empty_admission_is_not_a_running_receipt_or_a_valid_inventory() {
        let receipt = decode_reply(&["pane", "run", "w1:p1", "command"], b"").unwrap();
        assert_eq!(receipt["admitted"], true);
        assert!(receipt.get("running").is_none());
        assert!(decode_reply(&["api", "snapshot"], b"").is_err());
        assert!(decode_reply(&["pane", "run"], br#"{"error":{"code":"no_pane"}}"#).is_err());
    }
    #[test]
    fn retained_shell_is_not_an_agent_and_exec_is_not_an_idle_shell() {
        assert_eq!(
            process_state(
                &json!({"shell_pid":42,"foreground_processes":[{"pid":42,"name":"bash"}]})
            )
            .unwrap(),
            (true, 42)
        );
        assert_eq!(
            process_state(
                &json!({"shell_pid":42,"foreground_processes":[{"pid":42,"name":"codex"}]})
            )
            .unwrap(),
            (false, 42)
        );
        assert_eq!(
            process_state(
                &json!({"shell_pid":42,"foreground_processes":[{"pid":43,"name":"vim"}]})
            )
            .unwrap(),
            (false, 43)
        );
        assert!(process_state(&json!({"shell_pid":42,"foreground_processes":[]})).is_err());
    }
    #[test]
    fn herdr_targets_are_scoped_and_validate_each_component() {
        assert_eq!(herdr_target("test", "w12:p3").unwrap(), "herdr/test/w12:p3");
        for (s, p) in [
            ("../default", "w1:p1"),
            ("default", "w:p1"),
            ("default", "w1:p1/extra"),
            ("default", "%1"),
        ] {
            assert!(herdr_target(s, p).is_err());
        }
    }
}
