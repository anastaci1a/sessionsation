//! Claude Code cross-session messaging wire protocol, version-pinned in ONE place
//! (contract 1.40.0). Everything that depends on Claude Code's undocumented frame
//! shapes lives here so a protocol change is contained:
//!
//! - registry: `<home>/sessions/<pid>.json` with `peerProtocol` = [`PEER_PROTOCOL`]
//!   and `messagingSocketPath` (read by [`super::peer::endpoint`]);
//! - inbound frame (one JSON line, newline terminated, then close):
//!   `{"type":"user","message":{"role":"user","content":ENVELOPE},"priority":"next"|"later",
//!   "from":"uds:<our socket>","msgV":1,"msg_id":ID,"uuid":ID,"session_id":ID}`;
//!   where ENVELOPE is `<cross-session-message from=".." from-name=".." from-mode="bypass|prompting">\nBODY\n</cross-session-message>`
//!   (the only content Claude renders as a collapsed `@<name> ❯` row; the body has any envelope
//!   tag neutralized with U+200B, see [`escape_body`]);
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
/// How long the sender waits for a first receipt before reporting a silent accept.
pub const STATUS_WINDOW: Duration = Duration::from_secs(3);

pub const SETTING: &str = super::peer::SETTING;

/// Who sends a native frame: the `from-name` and `from-mode` of the envelope.
#[derive(Clone, Debug, PartialEq, serde::Serialize, serde::Deserialize)]
pub struct Sender {
    pub name: String,
    /// `bypass` or `prompting`: the SENDER's own permission class, never the receiver's.
    pub mode: String,
    /// `<space>:<name>` of the sending session (contract 1.48.0); omitted when unscoped.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub canonical: Option<String>,
}
impl Sender {
    /// The `from-name` shown to the receiver: the canonical `<space>:<name>` when it
    /// fits the 64-character cap, else the bare name (never a truncated canonical).
    pub fn display_name(&self) -> &str {
        match &self.canonical {
            Some(c) if c.chars().count() <= 64 => c,
            _ => &self.name,
        }
    }
}
impl Default for Sender {
    fn default() -> Self {
        Self {
            name: DEFAULT_SENDER.into(),
            mode: "prompting".into(),
            canonical: None,
        }
    }
}
/// Sender name when the caller is not a managed session.
pub const DEFAULT_SENDER: &str = "sessionsation";
/// Script fallback for `from_component` (contract 1.50.0).
pub const COMPONENT_ENV: &str = "SESSIONSATION_SENDER_COMPONENT";
/// Longest accepted component (`sessionsation:` + 40 = 54, under the 64-character cap).
pub const COMPONENT_MAX: usize = 40;

/// `^[A-Za-z0-9][A-Za-z0-9._-]{0,39}$`.
pub fn valid_component(c: &str) -> bool {
    let b = c.as_bytes();
    !b.is_empty()
        && b.len() <= COMPONENT_MAX
        && b[0].is_ascii_alphanumeric()
        && b[1..]
            .iter()
            .all(|x| x.is_ascii_alphanumeric() || matches!(x, b'.' | b'_' | b'-'))
}

impl Sender {
    /// The default sender named by the component behind it (contract 1.51.0): the bare
    /// `<component>`, `prompting`. The caller must first rule out a collision with a session
    /// name or `sessionsation` (see [`Sender::prefixed_component`]). The name is computed into
    /// `name` (no new stored field), so records stay readable by older binaries and the
    /// worker needs no change.
    pub fn component(component: &str) -> Self {
        Self {
            name: component.into(),
            ..Self::default()
        }
    }

    /// The collision fallback: `sessionsation:<component>` (the 1.50.0 rendering). A
    /// component cannot contain `:`, so neither form ever looks like a canonical
    /// `<space>:<name>` of another session.
    pub fn prefixed_component(component: &str) -> Self {
        Self {
            name: format!("{DEFAULT_SENDER}:{component}"),
            ..Self::default()
        }
    }
}
const CLOSE_TAG: &str = "</cross-session-message";
const OPEN_TAG: &str = "<cross-session-message";

/// `from-name`: strips `"`, `<`, `>`, control characters and newlines, caps at 64 characters,
/// never empty.
pub fn sanitize_sender_name(name: &str) -> String {
    let clean: String = name
        .chars()
        .filter(|c| !c.is_control() && !matches!(c, '"' | '<' | '>'))
        .take(64)
        .collect();
    let clean = clean.trim();
    if clean.is_empty() {
        DEFAULT_SENDER.into()
    } else {
        clean.to_owned()
    }
}
/// Percent-encode everything but `[A-Za-z0-9:_/.-]` (the `from` attribute value).
fn encode_from(from: &str) -> String {
    from.bytes()
        .map(|b| match b {
            b'A'..=b'Z' | b'a'..=b'z' | b'0'..=b'9' | b':' | b'_' | b'/' | b'.' | b'-' => {
                (b as char).to_string()
            }
            _ => format!("%{b:02X}"),
        })
        .collect()
}
/// Neutralize an opening or closing envelope tag inside the body by inserting a zero-width
/// space (U+200B) after the `<`, so the body can neither close nor forge an envelope.
pub fn escape_body(body: &str) -> String {
    body.replace(CLOSE_TAG, "</\u{200B}cross-session-message")
        .replace(OPEN_TAG, "<\u{200B}cross-session-message")
}
/// The message content Claude Code renders as a collapsed `@<name> ❯` row. Fixed attribute
/// order; `from-session`, `hop-chain` and `from-plugin` are omitted.
pub fn envelope(from: &str, sender: &Sender, body: &str) -> String {
    let mode = if sender.mode == "bypass" {
        "bypass"
    } else {
        "prompting"
    };
    format!(
        "<cross-session-message from=\"{}\" from-name=\"{}\" from-mode=\"{mode}\">\n{}\n</cross-session-message>",
        encode_from(from),
        sanitize_sender_name(sender.display_name()),
        escape_body(body)
    )
}

/// The inbound message frame; `content` is the body wrapped in the sender envelope.
pub fn user_frame(
    id: &str,
    conversation: &str,
    text: &str,
    priority: &str,
    from: &str,
    sender: &Sender,
) -> Value {
    json!({"type":"user","message":{"role":"user","content":envelope(from, sender, text)},
        "priority":priority,"from":from,"msgV":MSG_V,"msg_id":id,"uuid":id,"session_id":conversation})
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
mod tests;
