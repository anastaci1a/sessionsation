//! Per-account control of Claude Code's `skipDangerousModePermissionPrompt`
//! (contract revision 21): the warning Claude shows when it starts in
//! bypass-permissions (YOLO) mode. Edits exactly one key of the account's
//! `settings.json`, preserving every other key; the file is replaced atomically
//! with its mode kept and a symlinked file is refused. Credentials are never read.
use super::peer::{read_settings, replace, settings_file};
use anyhow::Result;
use anyhow::ensure;
use serde_json::Value;
use std::path::Path;

pub const SETTING: &str = "skipDangerousModePermissionPrompt";

/// The stored value (None when absent, unreadable or not a boolean).
pub fn skip(home: &Path) -> Option<bool> {
    read_settings(home)
        .ok()
        .flatten()
        .and_then(|m| m.get(SETTING).and_then(Value::as_bool))
}

#[derive(Debug, PartialEq)]
pub struct Toggle {
    pub changed: bool,
    pub previous: Option<bool>,
}

/// `skip=true` writes `true` (warning skipped); `skip=false` writes an explicit
/// `false` (warning shown), so a later default never overrides the choice.
pub fn set(home: &Path, skip_warning: bool) -> Result<Toggle> {
    ensure!(home.is_dir(), "ACCOUNT_HOME_UNAVAILABLE");
    let mut map = read_settings(home)?.unwrap_or_default();
    let previous = map.get(SETTING).and_then(Value::as_bool);
    if previous == Some(skip_warning) {
        return Ok(Toggle {
            changed: false,
            previous,
        });
    }
    map.insert(SETTING.into(), Value::Bool(skip_warning));
    let mut bytes = serde_json::to_vec_pretty(&Value::Object(map))?;
    bytes.push(b'\n');
    replace(&settings_file(home), &bytes)?;
    Ok(Toggle {
        changed: true,
        previous,
    })
}

/// Default for a NEW sessionsation-created account: skip, unless a value is
/// already stored. Returns whether it wrote.
pub fn default_new(home: &Path) -> Result<bool> {
    if read_settings(home)?.is_some_and(|m| m.contains_key(SETTING)) {
        return Ok(false);
    }
    Ok(set(home, true)?.changed)
}

/// Row fields for account listings.
pub fn row(home: &Path) -> Value {
    let v = skip(home);
    serde_json::json!({"bypass_warning":match v {Some(true)=>"skip",_=>"show"},"bypass_warning_value":v,"bypass_warning_setting":SETTING})
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::{fs, os::unix::fs::PermissionsExt};

    #[test]
    fn read_write_preserve_mode_and_symlink() {
        let d = tempfile::tempdir().unwrap();
        let h = d.path();
        assert_eq!(skip(h), None);
        assert_eq!(row(h)["bypass_warning"], "show");
        let f = settings_file(h);
        fs::write(&f, r#"{"model":"opus","crossSessionInbound":"accept"}"#).unwrap();
        fs::set_permissions(&f, fs::Permissions::from_mode(0o640)).unwrap();
        let t = set(h, true).unwrap();
        assert!(t.changed && t.previous.is_none());
        assert_eq!(skip(h), Some(true));
        assert_eq!(row(h)["bypass_warning"], "skip");
        let v: Value = serde_json::from_slice(&fs::read(&f).unwrap()).unwrap();
        assert_eq!(v["model"], "opus");
        assert_eq!(v["crossSessionInbound"], "accept");
        assert_eq!(
            fs::metadata(&f).unwrap().permissions().mode() & 0o777,
            0o640
        );
        assert!(!set(h, true).unwrap().changed);
        let t = set(h, false).unwrap();
        assert!(t.changed && t.previous == Some(true));
        assert_eq!(skip(h), Some(false));
        assert_eq!(row(h)["bypass_warning"], "show");
        let v: Value = serde_json::from_slice(&fs::read(&f).unwrap()).unwrap();
        assert_eq!(v["model"], "opus");
    }

    #[test]
    fn symlink_refused() {
        let d = tempfile::tempdir().unwrap();
        let real = d.path().join("real.json");
        fs::write(&real, "{}").unwrap();
        std::os::unix::fs::symlink(&real, d.path().join("settings.json")).unwrap();
        let e = set(d.path(), true).unwrap_err().to_string();
        assert!(e.contains("SETTINGS_IS_SYMLINK"), "{e}");
        assert_eq!(fs::read_to_string(&real).unwrap(), "{}");
    }

    #[test]
    fn new_account_default_respects_existing() {
        let d = tempfile::tempdir().unwrap();
        assert!(default_new(d.path()).unwrap());
        assert_eq!(skip(d.path()), Some(true));
        let e = tempfile::tempdir().unwrap();
        set(e.path(), false).unwrap();
        assert!(!default_new(e.path()).unwrap());
        assert_eq!(skip(e.path()), Some(false));
    }
}
