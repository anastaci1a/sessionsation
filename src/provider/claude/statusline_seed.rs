//! Default `statusLine` for a NEW managed Claude account home (contract 1.27.0).
//! Seeded the same way as the bypass-warning default: only the `statusLine` key is
//! added, every other key is kept, the file is replaced atomically with its mode
//! kept, a symlinked settings.json is refused and an existing `statusLine` is never
//! overwritten. Credentials are never read. Sessionsation's own statusline hook
//! forwards to whatever `statusLine` the account has, so this keeps working.
use super::peer::{read_settings, replace, settings_file};
use anyhow::{Result, ensure};
use serde_json::{Value, json};
use std::path::Path;

pub const KEY: &str = "statusLine";

/// Seed `statusLine = {"type":"command","command":COMMAND}` unless `command` is
/// empty (disabled) or the key already exists. Returns whether it wrote.
pub fn default_new(home: &Path, command: &str) -> Result<bool> {
    if command.trim().is_empty() {
        return Ok(false);
    }
    ensure!(home.is_dir(), "ACCOUNT_HOME_UNAVAILABLE");
    let mut map = read_settings(home)?.unwrap_or_default();
    if map.contains_key(KEY) {
        return Ok(false);
    }
    map.insert(KEY.into(), json!({"type":"command","command":command}));
    let mut bytes = serde_json::to_vec_pretty(&Value::Object(map))?;
    bytes.push(b'\n');
    replace(&settings_file(home), &bytes)?;
    Ok(true)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::{fs, os::unix::fs::PermissionsExt};

    #[test]
    fn seeds_once_keeps_keys_and_mode() {
        let d = tempfile::tempdir().unwrap();
        let f = settings_file(d.path());
        fs::write(&f, r#"{"model":"opus"}"#).unwrap();
        fs::set_permissions(&f, fs::Permissions::from_mode(0o640)).unwrap();
        assert!(default_new(d.path(), "bash x.sh").unwrap());
        let v: Value = serde_json::from_slice(&fs::read(&f).unwrap()).unwrap();
        assert_eq!(
            v,
            json!({"model":"opus","statusLine":{"type":"command","command":"bash x.sh"}})
        );
        assert_eq!(
            fs::metadata(&f).unwrap().permissions().mode() & 0o777,
            0o640
        );
        assert!(!default_new(d.path(), "bash y.sh").unwrap());
    }

    #[test]
    fn empty_disables_and_symlink_refused() {
        let d = tempfile::tempdir().unwrap();
        assert!(!default_new(d.path(), "").unwrap());
        assert!(!settings_file(d.path()).exists());
        let real = d.path().join("real.json");
        fs::write(&real, "{}").unwrap();
        std::os::unix::fs::symlink(&real, settings_file(d.path())).unwrap();
        assert!(default_new(d.path(), "bash x.sh").is_err());
        assert_eq!(fs::read_to_string(&real).unwrap(), "{}");
    }
}
