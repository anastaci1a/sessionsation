//! Claude Code cross-session messaging ("peer messages") for live interactive
//! sessions, and the per-account opt-in that lets them accept such messages.
//!
//! A running session publishes `messagingSocketPath`/`peerProtocol` in its
//! registry entry (`<home>/sessions/<pid>.json`). Sessionsation reads only those
//! fields (never `*.key` files). Inbound acceptance is the account setting
//! `crossSessionInbound`; Sessionsation sets it to `accept` only when the
//! operator enables live steering for that account and removes it again on
//! disable, preserving every other key.
use super::{Live, proc_start};
use anyhow::{Context, Result, bail, ensure};
use serde_json::{Map, Value};
use std::{
    fs,
    io::Read,
    os::unix::fs::FileTypeExt,
    path::{Path, PathBuf},
};

pub const SETTING: &str = "crossSessionInbound";
pub const ACCEPT: &str = "accept";
pub const PROTOCOL: u64 = 1;
pub const DISABLED: &str = "LIVE_STEERING_DISABLED: inbound peer messages are not accepted by this Claude account; enable with `sessionsation account live-steering claude:<name> --enable`";
pub const NO_ENDPOINT: &str = "PEER_ENDPOINT_UNAVAILABLE: the live Claude session publishes no usable messaging socket (peer protocol 1)";

pub fn settings_file(home: &Path) -> PathBuf {
    home.join("settings.json")
}

fn read_settings(home: &Path) -> Result<Option<Map<String, Value>>> {
    let path = settings_file(home);
    let bytes = match fs::read(&path) {
        Ok(b) => b,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(None),
        Err(e) => return Err(e).context("SETTINGS_UNREADABLE"),
    };
    ensure!(bytes.len() <= 1024 * 1024, "SETTINGS_TOO_LARGE");
    if bytes.iter().all(u8::is_ascii_whitespace) {
        return Ok(Some(Map::new()));
    }
    match serde_json::from_slice::<Value>(&bytes)
        .context("SETTINGS_INVALID: settings.json is not valid JSON")?
    {
        Value::Object(map) => Ok(Some(map)),
        _ => bail!("SETTINGS_INVALID: settings.json is not a JSON object"),
    }
}

/// The current value of `crossSessionInbound` (None when absent/unreadable).
pub fn inbound(home: &Path) -> Option<String> {
    read_settings(home)
        .ok()
        .flatten()
        .and_then(|m| m.get(SETTING).and_then(Value::as_str).map(str::to_owned))
}
pub fn enabled(home: &Path) -> bool {
    inbound(home).as_deref() == Some(ACCEPT)
}

/// Outcome of an explicit enable/disable.
#[derive(Debug, PartialEq)]
pub struct Toggle {
    pub changed: bool,
    pub previous: Option<String>,
    pub current: Option<String>,
}

/// Enable sets `crossSessionInbound: "accept"`; disable removes it only when it
/// is `accept` (an operator's own `hold`/`refuse` is left untouched). Every
/// other key is preserved; the file is replaced atomically with its mode kept.
pub fn set(home: &Path, enable: bool) -> Result<Toggle> {
    ensure!(home.is_dir(), "ACCOUNT_HOME_UNAVAILABLE");
    let existing = read_settings(home)?;
    let previous = existing.as_ref().and_then(|m| m.get(SETTING)).map(|v| {
        v.as_str()
            .map(str::to_owned)
            .unwrap_or_else(|| v.to_string())
    });
    let mut map = existing.clone().unwrap_or_default();
    if enable {
        if previous.as_deref() == Some(ACCEPT) {
            return Ok(Toggle {
                changed: false,
                current: previous.clone(),
                previous,
            });
        }
        map.insert(SETTING.into(), Value::String(ACCEPT.into()));
    } else {
        if previous.as_deref() != Some(ACCEPT) {
            return Ok(Toggle {
                changed: false,
                current: previous.clone(),
                previous,
            });
        }
        map.remove(SETTING);
    }
    let mut bytes = serde_json::to_vec_pretty(&Value::Object(map))?;
    bytes.push(b'\n');
    replace(&settings_file(home), &bytes)?;
    Ok(Toggle {
        changed: true,
        previous,
        current: enable.then(|| ACCEPT.to_owned()),
    })
}

/// Atomic same-directory replacement that keeps the file's mode (0600 when new)
/// and never alters the (possibly adopted) home's own permissions. A symlinked
/// settings file is refused rather than silently replaced by a regular file.
pub(crate) fn replace(path: &Path, bytes: &[u8]) -> Result<()> {
    use std::{io::Write, os::unix::fs::PermissionsExt};
    let mode = match fs::symlink_metadata(path) {
        Ok(m) if m.file_type().is_symlink() => {
            bail!("SETTINGS_IS_SYMLINK: settings.json is a symlink; edit it manually")
        }
        Ok(m) => m.permissions().mode() & 0o777,
        Err(_) => 0o600,
    };
    let parent = path.parent().context("missing parent")?;
    let mut tmp = tempfile::NamedTempFile::new_in(parent)?;
    tmp.as_file()
        .set_permissions(fs::Permissions::from_mode(mode))?;
    tmp.write_all(bytes)?;
    tmp.as_file().sync_all()?;
    tmp.persist(path).map_err(|e| e.error)?;
    fs::File::open(parent)?.sync_all()?;
    Ok(())
}

/// The verified messaging endpoint of one live session.
#[derive(Clone, Debug, PartialEq)]
pub struct Endpoint {
    pub pid: u32,
    pub process_start: u64,
    pub session_id: String,
    pub socket: PathBuf,
}

/// Re-read the live session's registry entry and return its messaging socket.
/// The entry must still describe the same verified process and session.
pub fn endpoint(home: &Path, live: &Live) -> Result<Endpoint> {
    let path = home.join("sessions").join(format!("{}.json", live.pid));
    let mut bytes = vec![];
    fs::File::open(&path)
        .and_then(|f| f.take(64 * 1024).read_to_end(&mut bytes))
        .context("PEER_ENDPOINT_UNAVAILABLE")?;
    let v: Value = serde_json::from_slice(&bytes).context("PEER_ENDPOINT_UNAVAILABLE")?;
    let recorded = v["procStart"]
        .as_str()
        .and_then(|s| s.trim().parse::<u64>().ok())
        .or_else(|| v["procStart"].as_u64());
    ensure!(
        v["pid"].as_u64() == Some(live.pid as u64)
            && v["sessionId"].as_str() == Some(live.session_id.as_str())
            && recorded == Some(live.process_start)
            && proc_start(live.pid)
                .is_some_and(|(s, start)| start == live.process_start && !matches!(s, 'Z' | 'X')),
        "STALE_TARGET: the live Claude process changed"
    );
    ensure!(
        v["peerProtocol"].as_u64() == Some(PROTOCOL),
        "{NO_ENDPOINT}"
    );
    let socket = v["messagingSocketPath"]
        .as_str()
        .map(PathBuf::from)
        .filter(|p| p.is_absolute())
        .context(NO_ENDPOINT)?;
    ensure!(
        fs::symlink_metadata(&socket).is_ok_and(|m| m.file_type().is_socket()),
        "{NO_ENDPOINT}"
    );
    Ok(Endpoint {
        pid: live.pid,
        process_start: live.process_start,
        session_id: live.session_id.clone(),
        socket,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn toggle_preserves_other_keys_and_operator_values() {
        let d = tempfile::tempdir().unwrap();
        let home = d.path();
        // Absent file: disable is a no-op, enable creates it.
        assert!(!set(home, false).unwrap().changed);
        assert!(!settings_file(home).exists());
        let t = set(home, true).unwrap();
        assert!(t.changed && t.previous.is_none());
        assert!(enabled(home));
        fs::write(
            settings_file(home),
            r#"{"theme":"dark","permissions":{"allow":["Bash(ls)"]},"crossSessionInbound":"accept"}"#,
        )
        .unwrap();
        let t = set(home, false).unwrap();
        assert!(t.changed);
        let v: Value = serde_json::from_slice(&fs::read(settings_file(home)).unwrap()).unwrap();
        assert_eq!(v["theme"], "dark");
        assert_eq!(v["permissions"]["allow"][0], "Bash(ls)");
        assert!(v.get(SETTING).is_none());
        assert!(!set(home, false).unwrap().changed, "idempotent");
        // An operator's own hold is never removed by disable.
        fs::write(settings_file(home), r#"{"crossSessionInbound":"hold"}"#).unwrap();
        let t = set(home, false).unwrap();
        assert!(!t.changed);
        assert_eq!(inbound(home).as_deref(), Some("hold"));
        assert!(!enabled(home));
        fs::write(settings_file(home), "[1]").unwrap();
        assert!(
            set(home, true)
                .unwrap_err()
                .to_string()
                .contains("SETTINGS_INVALID")
        );
    }
}
