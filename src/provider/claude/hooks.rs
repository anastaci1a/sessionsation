//! Claude Code hooks Sessionsation installs and verifies (contract 1.64.0, SM5).
//!
//! One async command hook per needed event, in the account's own settings file under
//! `hooks.<Event>`: `{"matcher"?, "hooks":[{"type":"command","command":"<exe> provider-hook
//! claude-event","async":true,"timeout":10}]}`. An entry is Sessionsation's when its command
//! contains [`MARKER`]; every other entry (the user's hooks, other keys) is kept as it is.
//! Installing is idempotent: a file that already has exactly the wanted entries is not
//! written (`already_installed`); a stale entry of ours (another executable path) is replaced
//! in place. A symlinked or unparseable settings file is refused and reported, never edited.
//!
//! The hook itself ([`event_hook`]) reads the payload from stdin, keeps only content-free
//! fields ([`crate::provider::events::hook_line`]) and appends one line to
//! `<home>/.sessionsation/hook-events.jsonl` under a short lock. It prints nothing (some hook
//! output becomes model context), never fails the event, and never touches a data root.
use super::peer::{read_settings, replace, settings_file};
use anyhow::{Result, bail};
use serde_json::{Map, Value, json};
use std::{
    io::{Read, Write},
    path::{Path, PathBuf},
};

pub const MARKER: &str = "provider-hook claude-event";
pub const TIMEOUT_SECONDS: u64 = 10;
/// The events Sessionsation needs, with the matcher each entry carries.
pub const EVENTS: [(&str, Option<&str>); 10] = [
    ("SessionStart", None),
    ("SessionEnd", None),
    ("UserPromptSubmit", None),
    ("Stop", None),
    ("StopFailure", None),
    ("SubagentStart", None),
    ("SubagentStop", None),
    ("PostToolUse", Some("Bash")),
    ("PostCompact", None),
    ("Notification", Some("idle_prompt")),
];

/// The executable the hooks name: never `/proc/self/exe`, without the `(deleted)` suffix the
/// kernel adds after the installation was replaced, and mapped to the package's stable
/// `current` path ([`stable`]) so a release swap or a pruned release never breaks them.
pub fn executable() -> Result<PathBuf> {
    let exe = std::env::current_exe()?;
    let text = exe.to_string_lossy();
    let exe = match text.strip_suffix(" (deleted)") {
        Some(path) => PathBuf::from(path),
        None => exe,
    };
    Ok(stable(&exe))
}
/// `<pkg>/releases/<x>/<bin>` becomes `<pkg>/current/<bin>` when that exists and is the same
/// file; anything else stays as it is.
pub fn stable(exe: &Path) -> PathBuf {
    let mapped = (|| {
        let name = exe.file_name()?;
        let release_dir = exe.parent()?;
        let releases = release_dir.parent()?;
        if releases.file_name()? != "releases" {
            return None;
        }
        let current = releases.parent()?.join("current").join(name);
        let same = crate::alias::same_path(
            &crate::alias::canonical(&current).ok()?,
            &crate::alias::canonical(exe).ok()?,
        );
        same.then_some(current)
    })();
    mapped.unwrap_or_else(|| exe.to_path_buf())
}
/// The hook command. A missing binary (a removed installation) fails silently and at once:
/// the shell's error goes to /dev/null and `|| true` makes the hook exit 0, so Claude shows
/// nothing in the session.
pub fn command(executable: &Path) -> String {
    format!(
        "{} {MARKER} 2>/dev/null || true",
        crate::terminal::shell_quote(&executable.to_string_lossy())
    )
}
fn ours(hook: &Value) -> bool {
    hook["command"].as_str().is_some_and(|c| c.contains(MARKER))
}
fn group(command: &str, matcher: Option<&str>) -> Value {
    let mut g = json!({"hooks":[{"type":"command","command":command,"async":true,"timeout":TIMEOUT_SECONDS}]});
    if let Some(m) = matcher {
        g["matcher"] = json!(m);
    }
    g
}

/// What the settings hold for Sessionsation's hooks.
#[derive(Clone, Debug, Default, PartialEq)]
pub struct Health {
    pub missing: Vec<&'static str>,
    /// Ours, but not the wanted entry (another executable, matcher or options).
    pub stale: Vec<&'static str>,
    /// Hook entries that are not Sessionsation's (kept untouched).
    pub user_hooks: usize,
}
impl Health {
    pub fn installed(&self) -> bool {
        self.missing.is_empty() && self.stale.is_empty()
    }
}

fn hooks_of(settings: &Map<String, Value>) -> Result<Option<&Map<String, Value>>> {
    match settings.get("hooks") {
        None => Ok(None),
        Some(Value::Object(h)) => Ok(Some(h)),
        Some(_) => bail!("SETTINGS_INVALID: hooks is not an object"),
    }
}
/// Check a settings map against the wanted entries. Pure.
pub fn check(settings: &Map<String, Value>, command: &str) -> Result<Health> {
    let mut health = Health::default();
    let hooks = hooks_of(settings)?;
    if let Some(hooks) = hooks {
        for groups in hooks.values() {
            for g in groups.as_array().into_iter().flatten() {
                health.user_hooks += g["hooks"]
                    .as_array()
                    .map_or(0, |h| h.iter().filter(|h| !ours(h)).count());
            }
        }
    }
    for (event, matcher) in EVENTS {
        let groups = hooks
            .and_then(|h| h.get(event))
            .and_then(Value::as_array)
            .cloned()
            .unwrap_or_default();
        let mine: Vec<&Value> = groups
            .iter()
            .filter(|g| g["hooks"].as_array().is_some_and(|h| h.iter().any(ours)))
            .collect();
        match mine.as_slice() {
            [] => health.missing.push(event),
            [one] if **one == group(command, matcher) => {}
            _ => health.stale.push(event),
        }
    }
    Ok(health)
}
/// Make the settings map hold exactly the wanted entries; user entries are kept. Pure.
/// Returns whether anything changed.
pub fn merge(settings: &mut Map<String, Value>, command: &str) -> Result<bool> {
    let health = check(settings, command)?;
    if health.installed() {
        return Ok(false);
    }
    let hooks = settings
        .entry("hooks")
        .or_insert_with(|| json!({}))
        .as_object_mut()
        .expect("checked to be an object");
    for (event, matcher) in EVENTS {
        if !health.missing.contains(&event) && !health.stale.contains(&event) {
            continue;
        }
        let entry = hooks.entry(event).or_insert_with(|| json!([]));
        let Some(groups) = entry.as_array_mut() else {
            bail!("SETTINGS_INVALID: hooks.{event} is not an array");
        };
        // Drop only our own hook entries (and groups left empty by that).
        for g in groups.iter_mut() {
            if let Some(list) = g["hooks"].as_array_mut()
                && list.iter().any(ours)
            {
                list.retain(|h| !ours(h));
            }
        }
        groups.retain(|g| g["hooks"].as_array().is_none_or(|h| !h.is_empty()));
        groups.push(group(command, matcher));
    }
    Ok(true)
}

/// Install (or repair) the hooks in one account home. `Ok(false)`: already installed.
pub fn install(home: &Path, command: &str) -> Result<bool> {
    anyhow::ensure!(home.is_dir(), "ACCOUNT_HOME_UNAVAILABLE");
    let mut settings = read_settings(home)?.unwrap_or_default();
    if !merge(&mut settings, command)? {
        return Ok(false);
    }
    let mut bytes = serde_json::to_vec_pretty(&Value::Object(settings))?;
    bytes.push(b'\n');
    replace(&settings_file(home), &bytes)?;
    Ok(true)
}
/// The account's hook health with this executable (read-only).
pub fn status(home: &Path, command: &str) -> Result<Health> {
    check(&read_settings(home)?.unwrap_or_default(), command)
}
/// Best effort at launch: install with the running executable; a refusal is reported by
/// `api hooks`, never a failed launch.
pub fn ensure(home: &Path) {
    if std::env::var("SESSIONSATION_CLAUDE_HOOKS").is_ok_and(|v| v == "0") {
        return;
    }
    if let Ok(exe) = executable() {
        let _ = install(home, &command(&exe));
    }
}

/// `provider-hook claude-event`: record one hook payload. Always exits 0 and prints nothing.
pub fn event_hook() -> i32 {
    let mut input = vec![];
    let _ = std::io::stdin().take(1024 * 1024).read_to_end(&mut input);
    let Some(home) = std::env::var_os("CLAUDE_CONFIG_DIR")
        .map(PathBuf::from)
        .filter(|h| h.is_absolute())
    else {
        return 0;
    };
    let Ok(value) = serde_json::from_slice::<Value>(&input) else {
        return 0;
    };
    let now_ms = crate::messaging::claude_steer::terminal::now_ms();
    if let Some(line) = crate::provider::events::hook_line(&value, now_ms) {
        let _ = append(&home, &line);
    }
    0
}
fn append(home: &Path, line: &Value) -> Result<()> {
    use fs2::FileExt;
    let file = crate::provider::events::hook_file(home);
    let dir = file.parent().expect("hook file has a parent");
    crate::paths::private_dir(dir)?;
    let lock = crate::paths::lock_file(&crate::provider::events::hook_lock(home))?;
    lock.lock_exclusive()?;
    let mut text = line.to_string();
    text.push('\n');
    let mut f = std::fs::OpenOptions::new()
        .create(true)
        .append(true)
        .open(&file)?;
    f.write_all(text.as_bytes())?;
    Ok(())
}

#[cfg(test)]
mod tests {
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
        let v: Value =
            serde_json::from_slice(&fs::read(settings_file(fresh.path())).unwrap()).unwrap();
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
}
