//! Claude Code cross-session messaging wire protocol, version-pinned in ONE place
//! (contract 1.40.0). Everything that depends on Claude Code's undocumented frame
//! shapes lives here so a protocol change is contained:
//!
//! - registry: `<home>/sessions/<pid>.json` with `peerProtocol` = [`PEER_PROTOCOL`]
//!   and `messagingSocketPath` (read by [`super::peer::endpoint`]);
//! - inbound frame (one JSON line, newline terminated, then close):
//!   `{"type":"user","message":{"role":"user","content":TEXT},"priority":"next"|"later",
//!   "from":"uds:<our socket>","msgV":1,"msg_id":ID,"uuid":ID,"session_id":ID}`;
//! - control frames, same line framing: `{"type":"control","action":"notify_when_idle",
//!   "from":..,"msg_id":..}` asks for one idle notice;
//! - receipts, written by the receiver to our `from` socket:
//!   `{"type":"control","action":"peer_message_status","orig_msg_id":ID,"status":
//!   "held"|"denied"|"expired"|"delivered"|"refused"|"dropped"}` and
//!   `{"type":"control","action":"peer_idle_notice","orig_msg_id":ID,"state":..,
//!   "finished_at":..,"detail":..}`. Success is silent: no frame means the frame was
//!   accepted by the transport, never that Claude read it;
//! - the `from` socket must live in the receiver's own socket directory and be named
//!   `<pid>.sock` or `<pid>-<8 hex>.sock` with `<pid>` the sender's pid (the receiver
//!   checks it against SO_PEERCRED); otherwise it drops our receipts silently;
//! - on Linux the optional `{"type":"auth","token":..}` first line is not needed, so
//!   Sessionsation never reads the `*.key` token files.
//!
//! Verified against Claude Code 2.1.312 and 2.1.314 (docs: cross-session-messaging).
//! Older frames of this crate's earlier revisions used bare `type` names; the
//! receiver side [`classify`] still accepts both shapes.
use super::peer::PROTOCOL;
use anyhow::{Context, Result, ensure};
use serde_json::{Value, json};
use std::{
    fs,
    os::unix::fs::MetadataExt,
    path::{Path, PathBuf},
    time::Duration,
};

/// Registry `peerProtocol` this crate speaks.
pub const PEER_PROTOCOL: u64 = PROTOCOL;
/// Frame `msgV`.
pub const MSG_V: u64 = 1;
/// Claude Code versions the protocol was verified against.
pub const VERIFIED_VERSIONS: &[&str] = &["2.1.312", "2.1.314"];
/// Largest accepted frame line (Claude refuses about a million characters).
pub const MAX_FRAME_BYTES: usize = 1024 * 1024;
/// A subscription for an idle notice expires after 12 hours.
pub const IDLE_NOTICE_EXPIRY_SECONDS: i64 = 12 * 3600;
/// How long the sender waits for a first receipt before reporting a silent accept.
pub const STATUS_WINDOW: Duration = Duration::from_secs(3);

pub const SETTING: &str = super::peer::SETTING;

/// The inbound message frame.
pub fn user_frame(id: &str, conversation: &str, text: &str, priority: &str, from: &str) -> Value {
    json!({"type":"user","message":{"role":"user","content":text},"priority":priority,
        "from":from,"msgV":MSG_V,"msg_id":id,"uuid":id,"session_id":conversation})
}
/// Ask for one `peer_idle_notice` when the session next goes idle or exits.
pub fn notify_idle_frame(from: &str, msg_id: &str) -> Value {
    json!({"type":"control","action":"notify_when_idle","from":from,"msg_id":msg_id})
}
/// Newline-framed bytes of `frames`; every line must stay under the cap.
pub fn encode(frames: &[Value]) -> Result<Vec<u8>> {
    let mut out = vec![];
    for f in frames {
        let line = f.to_string();
        ensure!(
            line.len() < MAX_FRAME_BYTES,
            "PEER_MESSAGE_TOO_LARGE: the frame exceeds the 1 MiB cross-session cap"
        );
        out.extend_from_slice(line.as_bytes());
        out.push(b'\n');
    }
    Ok(out)
}

/// A frame received on our reply socket.
#[derive(Debug, PartialEq)]
pub enum Inbound {
    Status {
        orig: String,
        status: String,
        detail: Option<String>,
        drop_reason: Option<String>,
    },
    IdleNotice {
        orig: String,
        state: String,
        finished_at: Option<Value>,
        detail: Option<Value>,
    },
    Other,
}
/// Parse one control frame. `{"type":"control","action":..}` is the documented
/// shape; a bare `type` naming the action is accepted as well.
pub fn classify(v: &Value) -> Inbound {
    let kind = if v["type"] == "control" {
        v["action"].as_str()
    } else {
        v["type"].as_str()
    };
    let text = |k: &str| v[k].as_str().map(str::to_owned);
    match kind {
        Some("peer_message_status") => {
            let (Some(orig), Some(status)) = (text("orig_msg_id"), text("status")) else {
                return Inbound::Other;
            };
            // `expired` with detail `refused` is a refusal.
            let detail = text("status_detail");
            let status = if status == "expired" && detail.as_deref() == Some("refused") {
                "refused".to_owned()
            } else {
                status
            };
            Inbound::Status {
                orig,
                status,
                detail,
                drop_reason: text("drop_reason"),
            }
        }
        Some("peer_idle_notice") => match text("orig_msg_id") {
            Some(orig) => Inbound::IdleNotice {
                orig,
                state: text("state").unwrap_or_default(),
                finished_at: Some(v["finished_at"].clone()).filter(|x| !x.is_null()),
                detail: Some(v["detail"].clone()).filter(|x| !x.is_null()),
            },
            None => Inbound::Other,
        },
        _ => Inbound::Other,
    }
}

/// Whether a live process accepts inbound peer messages: the account setting
/// `crossSessionInbound: accept`, or this very process was started with a
/// `--settings` value that says so (what Sessionsation launches pass).
pub fn accepting(home: &Path, pid: u32) -> bool {
    super::peer::enabled(home) || process_accepts(pid)
}
fn process_accepts(pid: u32) -> bool {
    let Ok(bytes) = fs::read(format!("/proc/{pid}/cmdline")) else {
        return false;
    };
    let args: Vec<&[u8]> = bytes.split(|b| *b == 0).collect();
    args.iter().enumerate().any(|(i, a)| {
        let value = if *a == b"--settings" {
            args.get(i + 1).copied()
        } else {
            a.strip_prefix(b"--settings=".as_slice())
        };
        value
            .and_then(|v| serde_json::from_slice::<Value>(v).ok())
            .is_some_and(|v| v[SETTING] == super::peer::ACCEPT)
    })
}
/// Add `crossSessionInbound: accept` to a launch `--settings` JSON unless the
/// account's own setting is an explicit `hold` or `refuse` (the operator's choice).
pub fn with_inbound_accept(settings: &str, home: &Path) -> String {
    if super::peer::inbound(home).is_some_and(|v| v != super::peer::ACCEPT) {
        return settings.to_owned();
    }
    let Ok(mut v) = serde_json::from_str::<Value>(settings) else {
        return settings.to_owned();
    };
    if v.get(SETTING).is_none() {
        v[SETTING] = json!(super::peer::ACCEPT);
    }
    v.to_string()
}

/// Our reply socket, bound where the receiver will accept it.
pub struct Reply {
    pub path: PathBuf,
    /// In the receiver's socket directory under the accepted name.
    pub namespaced: bool,
    pub listener: std::sync::Arc<tokio::net::UnixListener>,
}
impl Reply {
    pub fn address(&self) -> String {
        format!("uds:{}", self.path.display())
    }
}
impl Drop for Reply {
    fn drop(&mut self) {
        let _ = fs::remove_file(&self.path);
    }
}
fn own_real_dir(dir: &Path) -> bool {
    fs::symlink_metadata(dir).is_ok_and(|m| {
        m.file_type().is_dir() && m.uid() == unsafe { libc::geteuid() } && m.mode() & 0o022 == 0
    })
}
/// Bind the reply socket next to the target's inbox socket as
/// `<our pid>-<8 hex>.sock`; when that directory is not usable, bind in
/// `fallback_dir` (receipts may then be dropped by the receiver, which the
/// receipt reports as `reply_socket:"private"`).
pub fn bind_reply(target_socket: &Path, fallback_dir: &Path) -> Result<Reply> {
    let suffix = &uuid::Uuid::new_v4().simple().to_string()[..8];
    let name = format!("{}-{suffix}.sock", std::process::id());
    if let Some(dir) = target_socket.parent().filter(|d| own_real_dir(d))
        && let Ok(listener) = bind_at(&dir.join(&name))
    {
        return Ok(Reply {
            path: dir.join(&name),
            namespaced: true,
            listener: std::sync::Arc::new(listener),
        });
    }
    crate::paths::private_dir(fallback_dir)?;
    let path = fallback_dir.join(&name);
    let listener = bind_at(&path).context("PEER_REPLY_SOCKET_UNAVAILABLE")?;
    Ok(Reply {
        path,
        namespaced: false,
        listener: std::sync::Arc::new(listener),
    })
}
fn bind_at(path: &Path) -> std::io::Result<tokio::net::UnixListener> {
    use std::os::unix::fs::PermissionsExt;
    let listener = tokio::net::UnixListener::bind(path)?;
    fs::set_permissions(path, fs::Permissions::from_mode(0o600))?;
    Ok(listener)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn frames_carry_the_pinned_shape() {
        let f = user_frame("m", "c", "hi", "next", "uds:/tmp/cc-socks/1-aaaaaaaa.sock");
        assert_eq!(f["type"], "user");
        assert_eq!(f["msgV"], 1);
        assert_eq!(f["message"], json!({"role":"user","content":"hi"}));
        assert_eq!(f["msg_id"], "m");
        let n = notify_idle_frame("uds:/x", "n");
        assert_eq!(n["type"], "control");
        assert_eq!(n["action"], "notify_when_idle");
        let bytes = encode(&[f, n]).unwrap();
        assert_eq!(bytes.iter().filter(|b| **b == b'\n').count(), 2);
        let big = user_frame("m", "c", &"x".repeat(MAX_FRAME_BYTES), "next", "uds:/x");
        assert!(
            encode(&[big])
                .unwrap_err()
                .to_string()
                .starts_with("PEER_MESSAGE_TOO_LARGE")
        );
    }

    #[test]
    fn both_control_shapes_classify_and_refusal_is_normalized() {
        for v in [
            json!({"type":"control","action":"peer_message_status","orig_msg_id":"m","status":"held"}),
            json!({"type":"peer_message_status","orig_msg_id":"m","status":"held"}),
        ] {
            assert_eq!(
                classify(&v),
                Inbound::Status {
                    orig: "m".into(),
                    status: "held".into(),
                    detail: None,
                    drop_reason: None
                }
            );
        }
        let v = json!({"type":"control","action":"peer_message_status","orig_msg_id":"m","status":"expired","status_detail":"refused"});
        assert!(matches!(classify(&v), Inbound::Status { status, .. } if status == "refused"));
        let v = json!({"type":"control","action":"peer_message_status","orig_msg_id":"m","status":"dropped","drop_reason":"queue-full"});
        assert!(
            matches!(classify(&v), Inbound::Status { drop_reason: Some(r), .. } if r == "queue-full")
        );
        let v = json!({"type":"control","action":"peer_idle_notice","orig_msg_id":"n","state":"idle","finished_at":5,"detail":"done"});
        assert!(matches!(classify(&v), Inbound::IdleNotice { state, .. } if state == "idle"));
        assert_eq!(classify(&json!({"type":"user"})), Inbound::Other);
        assert_eq!(
            classify(&json!({"type":"control","action":"peer_message_status"})),
            Inbound::Other
        );
    }

    #[test]
    fn launch_settings_accept_unless_the_operator_chose_otherwise() {
        let d = tempfile::tempdir().unwrap();
        let base = r#"{"statusLine":{"type":"command","command":"x"}}"#;
        let v: Value = serde_json::from_str(&with_inbound_accept(base, d.path())).unwrap();
        assert_eq!(v[SETTING], "accept");
        assert_eq!(v["statusLine"]["command"], "x", "other keys are kept");
        for choice in ["hold", "refuse"] {
            fs::write(
                d.path().join("settings.json"),
                format!(r#"{{"{SETTING}":"{choice}"}}"#),
            )
            .unwrap();
            let v: Value = serde_json::from_str(&with_inbound_accept(base, d.path())).unwrap();
            assert!(v.get(SETTING).is_none(), "{choice} is respected");
        }
        fs::write(d.path().join("settings.json"), "{}").unwrap();
        assert_eq!(
            serde_json::from_str::<Value>(&with_inbound_accept(base, d.path())).unwrap()[SETTING],
            "accept"
        );
    }

    #[tokio::test]
    async fn the_reply_socket_is_bound_in_the_receivers_directory() {
        let d = tempfile::tempdir().unwrap();
        let socks = d.path().join("cc-socks");
        std::fs::create_dir(&socks).unwrap();
        let reply = bind_reply(&socks.join("42.sock"), &d.path().join("private")).unwrap();
        assert!(reply.namespaced);
        let name = reply.path.file_name().unwrap().to_str().unwrap().to_owned();
        let (pid, hex) = name.strip_suffix(".sock").unwrap().split_once('-').unwrap();
        assert_eq!(pid, std::process::id().to_string());
        assert_eq!(hex.len(), 8);
        assert!(hex.bytes().all(|b| b.is_ascii_hexdigit()));
        assert_eq!(reply.path.parent().unwrap(), socks);
        let path = reply.path.clone();
        drop(reply);
        assert!(!path.exists(), "removed on drop");
        // A directory another user could write to is not used.
        std::fs::set_permissions(&socks, std::os::unix::fs::PermissionsExt::from_mode(0o777))
            .unwrap();
        let reply = bind_reply(&socks.join("42.sock"), &d.path().join("private")).unwrap();
        assert!(!reply.namespaced);
        assert!(reply.path.starts_with(d.path().join("private")));
    }
}
