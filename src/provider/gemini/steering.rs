//! Gemini CLI model steering (`experimental.modelSteering`, off by default).
//!
//! With it on, Enter while a turn runs no longer queues the text: Gemini CLI
//! 0.66.0 turns it into a "user steering update" that it adds to the next tool
//! response sent to the model. If the turn ends without another tool call the
//! pending update is submitted on its own as a new prompt once the CLI is idle
//! (interactiveCli: `handleHintSubmit`, `consumeUserHint` and the idle effect
//! around `consumePendingHints`). With it off, Enter while busy uses the native
//! message queue, and every queued message is joined with a blank line into one
//! prompt when the turn ends.
//!
//! The CLI reads the value once when it builds its config, and nothing watches
//! the settings files, so a running session keeps the value it started with:
//! a change takes effect only after that session restarts.
//!
//! `account live-steering gemini:<name> --enable|--disable` edits only this key
//! in `<home>/.gemini/settings.json` (atomic replace, file mode kept, symlinks
//! refused, other keys preserved).
use super::super::claude::peer;
use anyhow::{Context, Result, bail, ensure};
use serde_json::{Map, Value};
use std::{
    fs,
    path::{Path, PathBuf},
};

pub const SETTING: &str = "experimental.modelSteering";
pub const DISABLED: &str = "GEMINI_STEERING_DISABLED: this Gemini account does not have model steering on; enable it with `sessionsation account live-steering gemini:<name> --enable` and restart the session";
pub const RESTART_REQUIRED: &str = "GEMINI_STEERING_RESTART_REQUIRED: a Gemini settings file changed after this session started; Gemini CLI reads experimental.modelSteering only at startup, so restart the session first";
pub const UNKNOWN: &str = "GEMINI_STEERING_UNKNOWN: the Gemini settings that decide experimental.modelSteering for this session could not be read";
pub const QUEUE_WOULD_STEER: &str = "GEMINI_QUEUE_WOULD_STEER: model steering is on for this session, so Enter during a turn becomes a steering update instead of a queued message; use steer, or disable live steering and restart the session";

pub fn settings_file(home: &Path) -> PathBuf {
    home.join(".gemini/settings.json")
}

/// Strict JSON object, or None when the file does not exist. Gemini also
/// accepts comments; such a file is reported unreadable and never rewritten.
fn read(path: &Path) -> Result<Option<Map<String, Value>>> {
    let bytes = match fs::read(path) {
        Ok(b) => b,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(None),
        Err(e) => return Err(e).context("SETTINGS_UNREADABLE"),
    };
    ensure!(bytes.len() <= 1024 * 1024, "SETTINGS_TOO_LARGE");
    if bytes.iter().all(u8::is_ascii_whitespace) {
        return Ok(Some(Map::new()));
    }
    match serde_json::from_slice::<Value>(&bytes)
        .context("SETTINGS_INVALID: settings.json is not plain JSON (comments are not edited)")?
    {
        Value::Object(map) => Ok(Some(map)),
        _ => bail!("SETTINGS_INVALID: settings.json is not a JSON object"),
    }
}
fn value_in(map: &Map<String, Value>) -> Option<&Value> {
    map.get("experimental")?.get("modelSteering")
}

/// The account's own value (`<home>/.gemini/settings.json`): Some(bool) when
/// set, None when absent or unreadable.
pub fn configured(home: &Path) -> Option<bool> {
    read(&settings_file(home))
        .ok()
        .flatten()
        .and_then(|m| value_in(&m).and_then(Value::as_bool))
}

/// Outcome of an explicit enable/disable.
#[derive(Debug, PartialEq)]
pub struct Toggle {
    pub changed: bool,
    pub previous: Option<Value>,
    pub current: Option<bool>,
}

/// Enable sets `experimental.modelSteering: true`; disable removes the key.
/// Nothing else in the file changes; the file is replaced atomically with its
/// mode kept (0600 when new), and a symlinked file is refused.
pub fn set(home: &Path, enable: bool) -> Result<Toggle> {
    ensure!(home.is_dir(), "ACCOUNT_HOME_UNAVAILABLE");
    let dir = home.join(".gemini");
    match fs::symlink_metadata(&dir) {
        Ok(m) => ensure!(
            m.file_type().is_dir(),
            "SETTINGS_IS_SYMLINK: {} is not a directory; edit it manually",
            dir.display()
        ),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => crate::paths::private_dir(&dir)?,
        Err(e) => return Err(e.into()),
    }
    let path = settings_file(home);
    if fs::symlink_metadata(&path).is_ok_and(|m| m.file_type().is_symlink()) {
        bail!("SETTINGS_IS_SYMLINK: settings.json is a symlink; edit it manually");
    }
    let existing = read(&path)?;
    let previous = existing.as_ref().and_then(value_in).cloned();
    let mut map = existing.unwrap_or_default();
    if enable {
        if previous == Some(Value::Bool(true)) {
            return Ok(Toggle {
                changed: false,
                previous,
                current: Some(true),
            });
        }
        let experimental = map
            .entry("experimental")
            .or_insert_with(|| Value::Object(Map::new()));
        if experimental.is_null() {
            *experimental = Value::Object(Map::new());
        }
        let Some(object) = experimental.as_object_mut() else {
            bail!(
                "SETTINGS_INVALID: 'experimental' in settings.json is not an object; it was left unchanged"
            );
        };
        object.insert("modelSteering".into(), Value::Bool(true));
    } else {
        let Some(object) = map.get_mut("experimental").and_then(Value::as_object_mut) else {
            return Ok(Toggle {
                changed: false,
                previous,
                current: None,
            });
        };
        if object.remove("modelSteering").is_none() {
            return Ok(Toggle {
                changed: false,
                previous,
                current: None,
            });
        }
    }
    let mut bytes = serde_json::to_vec_pretty(&Value::Object(map))?;
    bytes.push(b'\n');
    peer::replace(&path, &bytes)?;
    Ok(Toggle {
        changed: true,
        previous,
        current: enable.then_some(true),
    })
}

/// What a running session uses: its settings layers as Gemini CLI 0.66.0
/// merges them (system defaults < user < workspace < system), read now, and
/// whether any layer changed after the process started.
#[derive(Clone, Debug, PartialEq)]
pub struct Effective {
    /// Some(on/off) when known; None when unreadable, workspace-dependent or
    /// changed since the process started (see `reason`).
    pub enabled: Option<bool>,
    pub configured: Option<bool>,
    /// Which layer set the value: `default`, `user`, `workspace`, `system` or
    /// `system_defaults`.
    pub source: &'static str,
    pub restart_required: bool,
    pub reason: Option<&'static str>,
}

/// `env` is the session process's environment (for the system settings path
/// overrides), `directory` its working directory, `started_at` its start (Unix
/// seconds). A file modified in a later second than the start counts as a
/// change the process has not loaded.
pub fn effective(
    home: &Path,
    directory: Option<&Path>,
    env: Option<&[(String, String)]>,
    started_at: Option<i64>,
) -> Effective {
    let get = |k: &str| {
        env.and_then(|e| {
            e.iter()
                .find(|(n, _)| n == k)
                .map(|(_, v)| PathBuf::from(v))
        })
    };
    let layers: Vec<(&'static str, PathBuf)> = [
        (
            "system_defaults",
            get("GEMINI_CLI_SYSTEM_DEFAULTS_PATH")
                .or_else(|| Some("/etc/gemini-cli/system-defaults.json".into())),
        ),
        ("user", Some(settings_file(home))),
        (
            "workspace",
            directory
                .filter(|d| d.is_absolute() && *d != home)
                .map(|d| d.join(".gemini/settings.json")),
        ),
        (
            "system",
            get("GEMINI_CLI_SYSTEM_SETTINGS_PATH")
                .or_else(|| Some("/etc/gemini-cli/settings.json".into())),
        ),
    ]
    .into_iter()
    .filter_map(|(n, p)| Some((n, p?)))
    .collect();
    let mut out = Effective {
        enabled: Some(false),
        configured: configured(home),
        source: "default",
        restart_required: false,
        reason: None,
    };
    let mut workspace_value = None;
    for (name, path) in &layers {
        let map = match read(path) {
            Ok(m) => m,
            Err(_) => {
                out.enabled = None;
                out.reason = Some(UNKNOWN);
                return out;
            }
        };
        let Some(map) = map else { continue };
        if let (Some(start), Ok(modified)) =
            (started_at, fs::metadata(path).and_then(|m| m.modified()))
        {
            let secs = modified
                .duration_since(std::time::UNIX_EPOCH)
                .map(|d| d.as_secs() as i64)
                .unwrap_or(0);
            if secs > start {
                out.restart_required = true;
            }
        }
        match value_in(&map) {
            Some(Value::Bool(b)) => {
                out.enabled = Some(*b);
                out.source = name;
                if *name == "workspace" {
                    workspace_value = Some(*b);
                }
            }
            Some(Value::Null) | None => {}
            Some(_) => {
                out.enabled = None;
                out.reason = Some(UNKNOWN);
                return out;
            }
        }
    }
    // Workspace settings apply only in a trusted folder, which is not checked
    // here: a workspace value is trusted only when it agrees with the rest.
    if let Some(w) = workspace_value {
        let without = layers
            .iter()
            .filter(|(n, _)| *n != "workspace")
            .rev()
            .find_map(|(_, p)| {
                let m = read(p).ok().flatten()?;
                value_in(&m).and_then(Value::as_bool)
            })
            .unwrap_or(false);
        if without != w && out.source == "workspace" {
            out.enabled = None;
            out.reason = Some(UNKNOWN);
            return out;
        }
    }
    if out.restart_required {
        out.enabled = None;
        out.reason = Some(RESTART_REQUIRED);
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::os::unix::fs::PermissionsExt;
    #[test]
    fn toggle_edits_only_the_key_keeps_the_mode_and_refuses_symlinks() {
        let d = tempfile::tempdir().unwrap();
        let home = d.path();
        let path = settings_file(home);
        // Absent file: enable creates it 0600; disable is a no-op.
        assert!(!set(home, false).unwrap().changed);
        let t = set(home, true).unwrap();
        assert!(t.changed && t.previous.is_none());
        assert_eq!(
            fs::metadata(&path).unwrap().permissions().mode() & 0o777,
            0o600
        );
        assert_eq!(configured(home), Some(true));
        // Other keys survive; the mode is kept.
        fs::write(
            &path,
            r#"{"general":{"vimMode":false},"experimental":{"other":1}}"#,
        )
        .unwrap();
        fs::set_permissions(&path, fs::Permissions::from_mode(0o640)).unwrap();
        assert!(set(home, true).unwrap().changed);
        assert!(!set(home, true).unwrap().changed);
        let v: Value = serde_json::from_slice(&fs::read(&path).unwrap()).unwrap();
        assert_eq!(
            v,
            serde_json::json!({"general":{"vimMode":false},"experimental":{"other":1,"modelSteering":true}})
        );
        assert_eq!(
            fs::metadata(&path).unwrap().permissions().mode() & 0o777,
            0o640
        );
        let t = set(home, false).unwrap();
        assert!(t.changed && t.previous == Some(Value::Bool(true)));
        let v: Value = serde_json::from_slice(&fs::read(&path).unwrap()).unwrap();
        assert_eq!(
            v,
            serde_json::json!({"general":{"vimMode":false},"experimental":{"other":1}})
        );
        assert_eq!(configured(home), None);
        // Comments (JSONC), a non-object 'experimental' and symlinks are refused unchanged.
        fs::write(&path, "{ // note\n}").unwrap();
        assert!(set(home, true).is_err());
        fs::write(&path, r#"{"experimental":true}"#).unwrap();
        assert!(set(home, true).is_err());
        assert_eq!(
            fs::read_to_string(&path).unwrap(),
            r#"{"experimental":true}"#
        );
        let real = home.join("real.json");
        fs::write(&real, "{}").unwrap();
        fs::remove_file(&path).unwrap();
        std::os::unix::fs::symlink(&real, &path).unwrap();
        let e = set(home, true).unwrap_err().to_string();
        assert!(e.starts_with("SETTINGS_IS_SYMLINK"), "{e}");
        assert_eq!(fs::read_to_string(&real).unwrap(), "{}");
    }
    #[test]
    fn the_effective_value_follows_layers_and_needs_a_restart_after_a_change() {
        let d = tempfile::tempdir().unwrap();
        let home = d.path().join("home");
        let work = d.path().join("work");
        fs::create_dir_all(&home).unwrap();
        fs::create_dir_all(work.join(".gemini")).unwrap();
        let none: [(String, String); 2] = [
            (
                "GEMINI_CLI_SYSTEM_SETTINGS_PATH".into(),
                d.path().join("absent-system.json").display().to_string(),
            ),
            (
                "GEMINI_CLI_SYSTEM_DEFAULTS_PATH".into(),
                d.path().join("absent-defaults.json").display().to_string(),
            ),
        ];
        let later = crate::daemon::now() + 100;
        let e = effective(&home, Some(&work), Some(&none), Some(later));
        assert_eq!((e.enabled, e.source), (Some(false), "default"));
        set(&home, true).unwrap();
        let e = effective(&home, Some(&work), Some(&none), Some(later));
        assert_eq!((e.enabled, e.source), (Some(true), "user"));
        // Changed after the process started: unknown until a restart.
        let e = effective(&home, Some(&work), Some(&none), Some(1));
        assert!(e.restart_required);
        assert_eq!((e.enabled, e.reason), (None, Some(RESTART_REQUIRED)));
        // A disagreeing workspace value depends on folder trust: unknown.
        fs::write(
            work.join(".gemini/settings.json"),
            r#"{"experimental":{"modelSteering":false}}"#,
        )
        .unwrap();
        let e = effective(&home, Some(&work), Some(&none), Some(later));
        assert_eq!((e.enabled, e.reason), (None, Some(UNKNOWN)));
        // A system file overrides everything below it.
        let system = d.path().join("system.json");
        fs::write(&system, r#"{"experimental":{"modelSteering":true}}"#).unwrap();
        let env = [
            (
                "GEMINI_CLI_SYSTEM_SETTINGS_PATH".to_owned(),
                system.display().to_string(),
            ),
            none[1].clone(),
        ];
        let e = effective(&home, Some(&work), Some(&env), Some(later));
        assert_eq!((e.enabled, e.source), (Some(true), "system"));
    }
}
