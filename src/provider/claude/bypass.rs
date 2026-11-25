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
mod tests;
