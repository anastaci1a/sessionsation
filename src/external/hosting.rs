//! Terminal hosting only. Provider authentication and conversation preparation stay
//! in the account-aware launcher and movement worker.
use super::*;
use sha2::{Digest, Sha256};

#[derive(Serialize, Deserialize)]
struct HerdrBinding {
    target: String,
    binding: Binding,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    socket: Option<PathBuf>,
}

pub use crate::terminal::herdr_target;
pub(super) use crate::terminal::retain;
use crate::terminal::{call, target};

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
    paths.root.join("herdr-panes").join(format!("{key}.json"))
}
fn receipt_socket(paths: &Paths, pane: &str, thread: Option<&str>) -> Result<Option<PathBuf>> {
    let (session, pane_id) = target(pane)?;
    let mut latest: Option<(i64, PathBuf)> = None;
    for directory in ["launches", "resumptions"] {
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
pub(super) fn relaunch(paths: &Paths, pane: &str, cwd: &str, command: &str) -> Result<()> {
    if let Some(socket) = socket_for(paths, pane)? {
        crate::terminal::relaunch_at(pane, cwd, command, Some(&socket))
    } else {
        crate::terminal::relaunch(pane, cwd, command)
    }
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
        // A dead or replaced bound process cannot own this pane. Reject it before
        // contacting HERDR: stopped servers otherwise add a connection timeout
        // per historical binding on every menu/discovery request. Retain the
        // binding/history, and keep the full host/ownership checks for live PIDs.
        let Ok(bound) = process_info(b.pid) else {
            continue;
        };
        if bound.start_time != b.start_time || matches!(bound.state, 'Z' | 'X') {
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
        if state(paths, &key).is_ok_and(|(idle, _)| idle) {
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
        // Providers without a Codex binding record their exact process in the
        // catalog before exec; require that PID to run the expected session.
        if let Some(s) = crate::catalog::list(paths)?.into_iter().find(|s| {
            s.provider == crate::provider::ProviderId::Claude.as_str()
                && s.account == account
                && s.attachment.as_ref().is_some_and(|a| a.target == key)
                && crate::provider::claude::launch_observed(s)
        }) {
            return Ok(
                json!({"schema_version":1,"state":"running","host":"herdr","target":key,"account":account,"thread_id":s.conversation_id,"provider":s.provider,"viewer_attach":["herdr","session","attach",session]}),
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
mod discovery_tests {
    use super::*;
    #[test]
    fn stale_bindings_never_contact_a_host_and_remain_recoverable() {
        let dir = tempfile::tempdir().unwrap();
        let paths = Paths {
            root: dir.path().join("state"),
            runtime: dir.path().join("runtime"),
        };
        let pid = std::process::id();
        let start = process_info(pid).unwrap().start_time;
        for (index, (pid, start_time)) in [(u32::MAX, 0), (pid, start + 1)].into_iter().enumerate()
        {
            bind(
                &paths,
                &format!("herdr/stopped/w1:p{}", index + 1),
                Binding {
                    pid,
                    start_time,
                    home: dir.path().join("home"),
                    thread: "11111111-1111-4111-8111-111111111111".into(),
                },
            )
            .unwrap();
        }
        let files: Vec<_> = fs::read_dir(paths.root.join("herdr-panes"))
            .unwrap()
            .map(|e| {
                let p = e.unwrap().path();
                let bytes = fs::read(&p).unwrap();
                (p, bytes)
            })
            .collect();
        let found = instances_with_state(&paths, |_, _| {
            panic!("stale process must not trigger HERDR socket I/O")
        })
        .unwrap();
        assert!(found.is_empty());
        for (path, bytes) in files {
            assert_eq!(fs::read(path).unwrap(), bytes);
        }
    }
    #[test]
    fn bindings_retain_socket_across_reader_environments_and_process_exit() {
        let tmp = tempfile::tempdir().unwrap();
        let paths = Paths::new(Some(tmp.path().join("state"))).unwrap();
        let socket = PathBuf::from("/isolated/herdr.sock");
        let mut child = Command::new("sleep")
            .arg("30")
            .env("HERDR_SOCKET_PATH", &socket)
            .spawn()
            .unwrap();
        let pane = "herdr/named/w1:p1";
        bind(
            &paths,
            pane,
            Binding {
                pid: child.id(),
                start_time: process_info(child.id()).unwrap().start_time,
                home: tmp.path().join("home"),
                thread: "11111111-1111-4111-8111-111111111111".into(),
            },
        )
        .unwrap();
        assert_eq!(socket_for(&paths, pane).unwrap(), Some(socket.clone()));
        let mut calls = 0;
        instances_with_state(&paths, |key, locator| {
            assert_eq!(key, pane);
            assert_eq!(locator, Some(socket.as_path()));
            calls += 1;
            Ok((true, child.id()))
        })
        .unwrap();
        assert_eq!(calls, 1);
        let bytes = fs::read(binding_file(&paths, pane)).unwrap();
        let mut legacy: Value = serde_json::from_slice(&bytes).unwrap();
        legacy.as_object_mut().unwrap().remove("socket");
        fs::write(
            binding_file(&paths, pane),
            serde_json::to_vec(&legacy).unwrap(),
        )
        .unwrap();
        assert_eq!(
            socket_for(&paths, pane).unwrap(),
            Some(socket.clone()),
            "legacy live bindings derive their own process socket, not the reader's"
        );
        assert_eq!(
            serde_json::from_slice::<Value>(&fs::read(binding_file(&paths, pane)).unwrap())
                .unwrap(),
            legacy,
            "observation does not migrate legacy binding files"
        );
        child.kill().unwrap();
        child.wait().unwrap();
        private_dir(&paths.root.join("resumptions")).unwrap();
        fs::write(paths.root.join("resumptions/fixture.json"), serde_json::to_vec(&json!({"request":{"host":{"session":"named","socket":socket}},"pane_id":"w1:p1","conversation_id":"11111111-1111-4111-8111-111111111111","created_at":1})).unwrap()).unwrap();
        assert_eq!(
            socket_for(&paths, pane).unwrap(),
            Some(socket.clone()),
            "legacy stopped bindings use their own durable launch locator"
        );
        fs::write(binding_file(&paths, pane), bytes).unwrap();
        assert_eq!(
            socket_for(&paths, pane).unwrap(),
            Some(socket),
            "retained socket is available for safe relaunch after process exit"
        );
    }
    #[test]
    fn live_pid_still_requires_current_host_confirmation() {
        let dir = tempfile::tempdir().unwrap();
        let paths = Paths {
            root: dir.path().join("state"),
            runtime: dir.path().join("runtime"),
        };
        let pid = std::process::id();
        bind(
            &paths,
            "herdr/named/w1:p1",
            Binding {
                pid,
                start_time: process_info(pid).unwrap().start_time,
                home: dir.path().join("home"),
                thread: "11111111-1111-4111-8111-111111111111".into(),
            },
        )
        .unwrap();
        let mut calls = 0;
        let found = instances_with_state(&paths, |target, _| {
            assert_eq!(target, "herdr/named/w1:p1");
            calls += 1;
            Ok((true, pid))
        })
        .unwrap();
        assert_eq!(calls, 1);
        assert!(found.is_empty());
    }
}

#[cfg(test)]
mod space_binding_tests {
    use super::*;
    #[test]
    fn messaging_inventory_includes_new_herdr_bindings_without_adopting_deleted_history() {
        let d = tempfile::tempdir().unwrap();
        let p = Paths::new(Some(d.path().join("state"))).unwrap();
        let thread = uuid::Uuid::new_v4().to_string();
        let home = p.root.join("synthetic-home");
        private_dir(&p.root.join("herdr-panes")).unwrap();
        atomic_write(
            &binding_file(&p, "herdr/fixture/w1:p1"),
            &serde_json::to_vec(&HerdrBinding {
                target: "herdr/fixture/w1:p1".into(),
                socket: None,
                binding: Binding {
                    pid: 123,
                    start_time: 42,
                    home: home.clone(),
                    thread: thread.clone(),
                },
            })
            .unwrap(),
        )
        .unwrap();
        assert_eq!(
            super::super::messaging_inventory(&p).unwrap(),
            vec![thread.clone()]
        );
        private_dir(&p.root.join("conversations")).unwrap();
        let record = p.root.join(format!("conversations/{thread}.json"));
        atomic_write(
            &record,
            &serde_json::to_vec(&json!({"owner":home,"deleted":true})).unwrap(),
        )
        .unwrap();
        assert!(super::super::messaging_inventory(&p).unwrap().is_empty());
        atomic_write(
            &record,
            &serde_json::to_vec(&json!({"owner":p.root.join("other-home")})).unwrap(),
        )
        .unwrap();
        // The authoritative saved owner remains a known conversation, but the
        // stale HERDR home cannot supply its live messaging attachment.
        assert!(!catalog::messaging_owner(&p, &thread, &home).unwrap());
        assert_eq!(super::super::messaging_inventory(&p).unwrap(), vec![thread]);
    }
    #[test]
    fn binding_write_rename_gap_recovers_and_cannot_replace_another_process() {
        let d = tempfile::tempdir().unwrap();
        let p = Paths::new(Some(d.path().join("state"))).unwrap();
        private_dir(&p.root.join("herdr-panes")).unwrap();
        let old = "herdr/fixture/w1:p1";
        let new = "herdr/fixture/w2:p2";
        let socket = p.root.join("owned.sock");
        let binding = Binding {
            pid: 11,
            start_time: 22,
            home: p.root.join("home"),
            thread: uuid::Uuid::new_v4().to_string(),
        };
        // Simulate interruption after publishing the new target in the old file.
        atomic_write(
            &binding_file(&p, old),
            &serde_json::to_vec(&HerdrBinding {
                target: new.into(),
                binding: binding.clone(),
                socket: Some(socket.clone()),
            })
            .unwrap(),
        )
        .unwrap();
        rebind(&p, old, new, &socket, &binding).unwrap();
        assert!(!binding_file(&p, old).exists());
        rebind(&p, old, new, &socket, &binding).unwrap();
        let changed = Binding { pid: 99, ..binding };
        assert!(rebind(&p, old, new, &socket, &changed).is_err());
    }
}
