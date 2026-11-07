//! Opt-in sender registration (`native_sender_registration`, contract 1.45.2).
//!
//! Claude Code shows a native peer message as a collapsed `@<name> ❯` entry only when the
//! sending process is a registered session (`<home>/sessions/<pid>.json`, the same-user
//! registry any process may write). The peer worker therefore writes one short-lived entry
//! for its own pid before the frame and removes it on every exit path. No Claude process
//! runs and no API call is made: only this file and the reply socket.
//!
//! Ownership marker: `entrypoint` = [`ENTRYPOINT`] together with `registeredBy` =
//! `sessionsation`. Only entries carrying both are ever swept or removed; nothing is matched
//! by name. Fields follow what Claude Code 2.1.314 reads (`pid`, `procStart` as the
//! `/proc/<pid>/stat` start-tick string, `pidDomain`, `messagingSocketPath`, `name`,
//! `nameSource`, `nameSince`, `peerProtocol`, `startedAt`, `updatedAt`, `cwd`, `version`).
//! Deliberately omitted so Claude is not misled: `sessionId` (no identity claim, no
//! duplicate-identity warning), `kind` (no claim to be an interactive/bg session),
//! `peerFeatures` (we handle no control frames from others), `status`, `jobId`, `tmux`.
use super::proc_start;
use anyhow::{Context, Result};
use serde_json::{Value, json};
use std::{
    fs,
    path::{Path, PathBuf},
};

pub const ENTRYPOINT: &str = "sessionsation-peer-worker";
pub const MARKER_FIELD: &str = "registeredBy";
pub const MARKER_VALUE: &str = "sessionsation";

fn ms() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map_or(0, |d| d.as_millis() as u64)
}
fn alive(pid: u32, start: &str) -> bool {
    proc_start(pid).is_some_and(|(state, s)| !matches!(state, 'Z' | 'X') && s.to_string() == start)
}
fn owned(v: &Value) -> bool {
    v["entrypoint"] == ENTRYPOINT && v[MARKER_FIELD] == MARKER_VALUE
}
/// Claude Code's `pidDomain`: `linux:<machine-id>:<pid namespace link>`.
fn pid_domain() -> String {
    let id = fs::read_to_string("/etc/machine-id").unwrap_or_default();
    let ns = fs::read_link("/proc/self/ns/pid")
        .map(|p| p.display().to_string())
        .unwrap_or_default();
    format!("linux:{}:{ns}", id.trim())
}

/// A written registry entry; removed on drop (and by [`Registration::remove`]).
pub struct Registration {
    path: PathBuf,
    pid: u32,
}
impl Registration {
    pub fn path(&self) -> &Path {
        &self.path
    }
    /// Remove the entry if it is still the one this process wrote.
    pub fn remove(&self) {
        remove_if_ours(&self.path, self.pid);
    }
}
impl Drop for Registration {
    fn drop(&mut self) {
        self.remove();
    }
}
fn remove_if_ours(path: &Path, pid: u32) {
    let ours = fs::read(path)
        .ok()
        .and_then(|b| serde_json::from_slice::<Value>(&b).ok())
        .is_some_and(|v| owned(&v) && v["pid"] == pid);
    if ours {
        let _ = fs::remove_file(path);
    }
}

/// Why registration was skipped (recorded on the receipt; delivery goes ahead unregistered).
pub fn skip_reason(home: &Path, name: &str) -> Option<String> {
    if name.is_empty() || name.len() > 64 || name.chars().any(char::is_control) {
        return Some("SENDER_NAME_INVALID".into());
    }
    let dir = home.join("sessions");
    if !dir.is_dir() {
        return Some("SENDER_REGISTRY_UNAVAILABLE".into());
    }
    for e in fs::read_dir(&dir).ok()?.flatten() {
        let Some(v) = fs::read(e.path())
            .ok()
            .and_then(|b| serde_json::from_slice::<Value>(&b).ok())
        else {
            continue;
        };
        if owned(&v) || v["name"].as_str() != Some(name) {
            continue;
        }
        let live = v["pid"]
            .as_u64()
            .zip(v["procStart"].as_str())
            .is_some_and(|(p, s)| alive(p as u32, s));
        if live {
            return Some("SENDER_NAME_COLLIDES_WITH_LIVE_SESSION".into());
        }
    }
    None
}

/// Register this process under `name`. `Err` means "deliver unregistered".
pub fn register(home: &Path, name: &str, socket: &Path) -> Result<Registration> {
    if let Some(reason) = skip_reason(home, name) {
        anyhow::bail!(reason);
    }
    let pid = std::process::id();
    let (_, start) = proc_start(pid).context("SENDER_PROC_START_UNAVAILABLE")?;
    let path = home.join("sessions").join(format!("{pid}.json"));
    // A file for this pid we do not own is a real session's: never replace it.
    if let Ok(bytes) = fs::read(&path)
        && !serde_json::from_slice::<Value>(&bytes).is_ok_and(|v| owned(&v))
    {
        anyhow::bail!("SENDER_PID_FILE_FOREIGN");
    }
    let now = ms();
    let entry = json!({"pid":pid,"cwd":std::env::current_dir().map(|p| p.display().to_string()).unwrap_or_else(|_| "?".into()),
        "startedAt":now,"procStart":start.to_string(),"version":format!("sessionsation-{}",env!("CARGO_PKG_VERSION")),
        "peerProtocol":super::peer::PROTOCOL,"entrypoint":ENTRYPOINT,MARKER_FIELD:MARKER_VALUE,
        "pidDomain":pid_domain(),"messagingSocketPath":socket.display().to_string(),
        "name":name,"nameSource":"peer","nameSince":now,"updatedAt":now});
    let mut bytes = serde_json::to_vec(&entry)?;
    bytes.push(b'\n');
    super::peer::replace(&path, &bytes)?;
    Ok(Registration { path, pid })
}

/// Remove entries this crate wrote whose process is gone (or whose pid was reused: the
/// recorded `procStart` no longer matches). Entries without both markers are never touched.
pub fn sweep(home: &Path) -> usize {
    let Ok(dir) = fs::read_dir(home.join("sessions")) else {
        return 0;
    };
    let mut removed = 0;
    for e in dir.flatten() {
        let Some(v) = fs::read(e.path())
            .ok()
            .and_then(|b| serde_json::from_slice::<Value>(&b).ok())
        else {
            continue;
        };
        if !owned(&v) {
            continue;
        }
        let live = v["pid"]
            .as_u64()
            .zip(v["procStart"].as_str())
            .is_some_and(|(p, s)| alive(p as u32, s));
        if !live && fs::remove_file(e.path()).is_ok() {
            removed += 1;
        }
    }
    removed
}

#[cfg(test)]
mod tests {
    use super::*;
    fn home() -> tempfile::TempDir {
        let t = tempfile::tempdir().unwrap();
        fs::create_dir(t.path().join("sessions")).unwrap();
        t
    }
    fn files(h: &Path) -> Vec<String> {
        let mut v: Vec<_> = fs::read_dir(h.join("sessions"))
            .unwrap()
            .map(|e| e.unwrap().file_name().to_string_lossy().into_owned())
            .collect();
        v.sort();
        v
    }
    #[test]
    fn registers_with_the_expected_shape_and_drop_removes_it() {
        let h = home();
        let g = register(h.path(), "localica-agent-11", Path::new("/tmp/x/1-ab.sock")).unwrap();
        let v: Value = serde_json::from_slice(&fs::read(g.path()).unwrap()).unwrap();
        assert_eq!(v["name"], "localica-agent-11");
        assert_eq!(v["pid"], std::process::id());
        assert_eq!(v["peerProtocol"], 1);
        assert_eq!(v["messagingSocketPath"], "/tmp/x/1-ab.sock");
        assert!(v.get("sessionId").is_none() && v.get("kind").is_none());
        assert!(owned(&v));
        drop(g);
        assert!(files(h.path()).is_empty());
    }
    #[test]
    fn entry_is_gone_after_an_error_path_unwinds() {
        let h = home();
        let r: Result<()> = (|| {
            let _g = register(h.path(), "s", Path::new("/tmp/s.sock"))?;
            anyhow::bail!("delivery failed");
        })();
        assert!(r.is_err());
        assert!(files(h.path()).is_empty());
    }
    #[test]
    fn a_live_name_collision_or_foreign_pid_file_is_not_touched() {
        let h = home();
        let me = std::process::id();
        let (_, start) = proc_start(me).unwrap();
        // A "real" session with the same name that is alive (this test process).
        let real =
            json!({"pid":1u32.max(me),"procStart":start.to_string(),"name":"dup","sessionId":"s"});
        fs::write(h.path().join("sessions/99999999.json"), real.to_string()).unwrap();
        let mut real2 = real.clone();
        real2["pid"] = json!(me);
        fs::write(h.path().join("sessions/other.json"), real2.to_string()).unwrap();
        let err = register(h.path(), "dup", Path::new("/tmp/s.sock"))
            .err()
            .unwrap();
        assert!(err.to_string().contains("COLLIDES"), "{err}");
        // Foreign file under our own pid is never replaced.
        fs::write(
            h.path().join(format!("sessions/{me}.json")),
            real.to_string(),
        )
        .unwrap();
        let err = register(h.path(), "fresh", Path::new("/tmp/s.sock"))
            .err()
            .unwrap();
        assert!(err.to_string().contains("FOREIGN"), "{err}");
        assert_eq!(files(h.path()).len(), 3);
        assert_eq!(sweep(h.path()), 0);
        assert_eq!(files(h.path()).len(), 3);
    }
    #[test]
    fn sweep_removes_only_owned_dead_entries_after_sigkill() {
        let h = home();
        // A child registers itself then is SIGKILLed: no drop runs.
        let mut child = std::process::Command::new("sleep")
            .arg("30")
            .spawn()
            .unwrap();
        let cpid = child.id();
        let (_, cstart) = proc_start(cpid).unwrap();
        let entry = |pid: u32, start: String, owned_marker: bool| {
            let mut v = json!({"pid":pid,"procStart":start,"name":"n","entrypoint":if owned_marker {ENTRYPOINT} else {"cli"}});
            if owned_marker {
                v[MARKER_FIELD] = json!(MARKER_VALUE);
            }
            v.to_string()
        };
        let sess = h.path().join("sessions");
        fs::write(
            sess.join(format!("{cpid}.json")),
            entry(cpid, cstart.to_string(), true),
        )
        .unwrap();
        // Live owned entry stays; reused pid (procStart mismatch) goes; foreign dead stays.
        let me = std::process::id();
        let (_, mystart) = proc_start(me).unwrap();
        fs::write(
            sess.join(format!("{me}.json")),
            entry(me, mystart.to_string(), true),
        )
        .unwrap();
        fs::write(sess.join("4000000.json"), entry(4000000, "1".into(), false)).unwrap();
        fs::write(sess.join("4000001.json"), entry(4000001, "1".into(), true)).unwrap();
        assert_eq!(sweep(h.path()), 1); // only the dead owned one (4000001)
        child.kill().unwrap();
        child.wait().unwrap();
        assert_eq!(sweep(h.path()), 1); // the SIGKILLed worker's entry
        let mut expected = vec![format!("{me}.json"), "4000000.json".to_owned()];
        expected.sort();
        assert_eq!(files(h.path()), expected);
    }
}
