use super::*;
use std::{fs, os::unix::fs::PermissionsExt};

const USER: &str = r#"{"model":"opus","hooks":{
    "Stop":[{"hooks":[{"type":"command","command":"notify-send done"}]}],
    "PreToolUse":[{"matcher":"Bash","hooks":[{"type":"command","command":"guard.sh"}]}]}}"#;

#[test]
fn install_is_idempotent_and_keeps_user_hooks() {
    let d = tempfile::tempdir().unwrap();
    let f = settings_file(d.path());
    fs::write(&f, USER).unwrap();
    fs::set_permissions(&f, fs::Permissions::from_mode(0o640)).unwrap();
    let cmd = command(Path::new("/opt/sessionsation"));
    assert!(!status(d.path(), &cmd).unwrap().installed());
    assert!(install(d.path(), &cmd).unwrap());
    let first = fs::read(&f).unwrap();
    // Second install: nothing to do, the file is not rewritten.
    assert!(!install(d.path(), &cmd).unwrap());
    assert_eq!(fs::read(&f).unwrap(), first);
    let health = status(d.path(), &cmd).unwrap();
    assert!(health.installed(), "{health:?}");
    assert_eq!(health.user_hooks, 2);
    let v: Value = serde_json::from_slice(&first).unwrap();
    assert_eq!(v["model"], "opus");
    assert_eq!(
        fs::metadata(&f).unwrap().permissions().mode() & 0o777,
        0o640
    );
    // The user's Stop hook is still first and unchanged; ours is its own group.
    assert_eq!(
        v["hooks"]["Stop"][0]["hooks"][0]["command"],
        "notify-send done"
    );
    assert_eq!(v["hooks"]["Stop"][1]["hooks"][0]["command"], cmd);
    assert_eq!(v["hooks"]["Stop"][1]["hooks"][0]["async"], true);
    assert_eq!(
        v["hooks"]["PreToolUse"][0]["hooks"][0]["command"],
        "guard.sh"
    );
    assert_eq!(v["hooks"]["PostToolUse"][0]["matcher"], "Bash");
    assert_eq!(v["hooks"]["Notification"][0]["matcher"], "idle_prompt");
    for (event, _) in EVENTS {
        let groups = v["hooks"][event].as_array().unwrap();
        let mine = groups
            .iter()
            .flat_map(|g| g["hooks"].as_array().unwrap())
            .filter(|h| ours(h))
            .count();
        assert_eq!(mine, 1, "{event}");
    }
    // A new executable path replaces only our entries, once.
    let moved = command(Path::new("/new/place/sessionsation"));
    assert_eq!(status(d.path(), &moved).unwrap().stale.len(), EVENTS.len());
    assert!(install(d.path(), &moved).unwrap());
    let v: Value = serde_json::from_slice(&fs::read(&f).unwrap()).unwrap();
    assert_eq!(v["hooks"]["Stop"].as_array().unwrap().len(), 2);
    assert_eq!(
        v["hooks"]["Stop"][0]["hooks"][0]["command"],
        "notify-send done"
    );
    assert_eq!(v["hooks"]["Stop"][1]["hooks"][0]["command"], moved);
    assert!(!install(d.path(), &moved).unwrap());
    assert_eq!(status(d.path(), &moved).unwrap().user_hooks, 2);
}

#[test]
fn a_release_path_maps_to_current_and_an_old_release_entry_is_rewritten_once() {
    let d = tempfile::tempdir().unwrap();
    let pkg = d.path().join("pkg");
    let release = pkg.join("releases/abc123");
    fs::create_dir_all(&release).unwrap();
    let exe = release.join("sessionsation");
    fs::write(&exe, "bin").unwrap();
    // No current yet: the path stays.
    assert_eq!(stable(&exe), exe);
    std::os::unix::fs::symlink("releases/abc123", pkg.join("current")).unwrap();
    assert_eq!(stable(&exe), pkg.join("current/sessionsation"));
    // current points at another release: not the same file, the path stays.
    let other = pkg.join("releases/def456/sessionsation");
    fs::create_dir_all(other.parent().unwrap()).unwrap();
    fs::write(&other, "bin2").unwrap();
    assert_eq!(stable(&other), other);
    // Outside a package layout nothing changes.
    assert_eq!(
        stable(Path::new("/usr/bin/sessionsation")),
        Path::new("/usr/bin/sessionsation")
    );
    // Entries naming the release are stale and become the stable path; a later install
    // from any release that current points at changes nothing.
    let home = d.path().join("home");
    fs::create_dir_all(&home).unwrap();
    assert!(install(&home, &command(&exe)).unwrap());
    let stable_cmd = command(&stable(&exe));
    assert_eq!(
        status(&home, &stable_cmd).unwrap().stale.len(),
        EVENTS.len()
    );
    assert!(install(&home, &stable_cmd).unwrap());
    let v: Value = serde_json::from_slice(&fs::read(settings_file(&home)).unwrap()).unwrap();
    assert_eq!(v["hooks"]["Stop"].as_array().unwrap().len(), 1);
    assert_eq!(v["hooks"]["Stop"][0]["hooks"][0]["command"], stable_cmd);
    assert!(stable_cmd.contains("/current/sessionsation"));
    assert!(!install(&home, &command(&stable(&exe))).unwrap());
}

#[test]
fn a_missing_binary_fails_silently_with_exit_zero() {
    let cmd = command(Path::new("/nonexistent/dir/sessionsation"));
    let out = std::process::Command::new("/bin/sh")
        .arg("-c")
        .arg(&cmd)
        .stdin(std::process::Stdio::null())
        .output()
        .unwrap();
    assert!(out.status.success(), "{cmd}");
    assert!(out.stdout.is_empty() && out.stderr.is_empty(), "{out:?}");
}

#[test]
fn a_hook_of_ours_inside_a_user_group_is_moved_out_and_the_user_hook_kept() {
    let mut s: Map<String, Value> = serde_json::from_value(json!({"hooks":{"Stop":[
        {"hooks":[{"type":"command","command":"mine.sh"},{"type":"command","command":"/old provider-hook claude-event"}]}]}}))
    .unwrap();
    let cmd = command(Path::new("/x"));
    assert!(merge(&mut s, &cmd).unwrap());
    assert_eq!(s["hooks"]["Stop"][0]["hooks"].as_array().unwrap().len(), 1);
    assert_eq!(s["hooks"]["Stop"][0]["hooks"][0]["command"], "mine.sh");
    assert_eq!(s["hooks"]["Stop"][1], group(&cmd, None));
    assert!(!merge(&mut s, &cmd).unwrap());
}

#[test]
fn unusable_settings_are_refused_and_not_edited() {
    let d = tempfile::tempdir().unwrap();
    let f = settings_file(d.path());
    fs::write(&f, r#"{"hooks":[]}"#).unwrap();
    assert!(install(d.path(), "x provider-hook claude-event").is_err());
    assert_eq!(fs::read_to_string(&f).unwrap(), r#"{"hooks":[]}"#);
    fs::write(&f, "not json").unwrap();
    assert!(install(d.path(), "x provider-hook claude-event").is_err());
    let real = d.path().join("real.json");
    fs::write(&real, "{}").unwrap();
    fs::remove_file(&f).unwrap();
    std::os::unix::fs::symlink(&real, &f).unwrap();
    let e = install(d.path(), "x provider-hook claude-event").unwrap_err();
    assert!(e.to_string().contains("SETTINGS_IS_SYMLINK"), "{e}");
    assert_eq!(fs::read_to_string(&real).unwrap(), "{}");
    // A missing file is created with only the hooks.
    let fresh = tempfile::tempdir().unwrap();
    assert!(install(fresh.path(), "x provider-hook claude-event").unwrap());
    let v: Value = serde_json::from_slice(&fs::read(settings_file(fresh.path())).unwrap()).unwrap();
    assert_eq!(v.as_object().unwrap().len(), 1);
}

#[test]
fn launched_provider_arguments_and_pane_commands_name_the_stable_path() {
    let d = tempfile::tempdir().unwrap();
    let pkg = d.path().join("pkg");
    let release = pkg.join("releases/abc123");
    fs::create_dir_all(&release).unwrap();
    let exe = release.join("sessionsation");
    fs::write(&exe, "bin").unwrap();
    std::os::unix::fs::symlink("releases/abc123", pkg.join("current")).unwrap();
    // The statusline recorder in `--settings` is the stable path, never the release dir.
    let settings = crate::provider::claude::launch_settings(
        &stable(&exe),
        crate::config::LaunchPermissions::Default,
        0,
    );
    assert!(
        settings.contains("/pkg/current/sessionsation"),
        "{settings}"
    );
    assert!(!settings.contains("releases"), "{settings}");
    // Every site that bakes the running executable into a launched provider's arguments, a
    // pane command or a service definition goes through `executable()` (-> `stable`).
    let root = Path::new(env!("CARGO_MANIFEST_DIR")).join("src");
    for file in [
        "provider/claude/launch.rs",
        "provider/claude/attach.rs",
        "resume.rs",
        "launch.rs",
        "messaging/recovery/launching.rs",
        "external/hosting.rs",
        "claude_switch.rs",
        "auto_recover.rs",
        "main.rs",
    ] {
        let text = fs::read_to_string(root.join(file)).unwrap();
        let bare = text
            .lines()
            .filter(|l| {
                l.contains("env::current_exe()")
                    && !l.contains("alias::canonical")
                    && !l.contains("Command::new")
            })
            .count();
        assert_eq!(
            bare, 0,
            "{file} bakes a release path; use hooks::executable()"
        );
    }
}
