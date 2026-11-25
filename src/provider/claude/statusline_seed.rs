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
mod tests;
