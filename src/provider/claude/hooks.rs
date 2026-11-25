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
mod tests;
