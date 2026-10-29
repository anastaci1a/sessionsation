//! Attach-only RPC. One reader routes responses independently of notifications.
use super::{Fault, Result};
use futures_util::{SinkExt, StreamExt};
use serde::Deserialize;
use serde_json::{Value, json, value::RawValue};
use std::{collections::HashMap, path::Path, sync::Arc, time::Duration};
use tokio::{
    net::UnixStream,
    sync::{Mutex, oneshot},
};
use tokio_tungstenite::{
    WebSocketStream,
    tungstenite::{Message, Utf8Bytes, protocol::WebSocketConfig},
};

type Rejections = Arc<Mutex<HashMap<u64, (i64, Option<&'static str>)>>>;
// Waiters receive the whole response frame; the typed result is parsed once from
// it, so a large frame is never copied into a second owned buffer.
type Pending = Arc<Mutex<HashMap<u64, oneshot::Sender<Result<Utf8Bytes>>>>>;
/// Ordinary messaging connections: one native frame or message.
pub(super) const FRAME_LIMIT: usize = 1024 * 1024;
/// Checkpoint projection connections.
pub(super) const CHECKPOINT_FRAME_LIMIT: usize = 8 * 1024 * 1024;
/// Steer anchor and reply observation: one full native turn of a long session.
/// Peak memory stays a small multiple of this bound; larger frames still fail
/// closed with `native_frame_limit` before any native write.
pub(super) const STEER_FRAME_LIMIT: usize = 64 * 1024 * 1024;
/// Slowest local frame throughput budgeted for: a read that may carry a whole
/// bounded frame gets its base budget plus the time to move that frame, so a
/// large legitimate turn is not cut off by a budget sized for 1 MiB.
const FRAME_BYTES_PER_SECOND: u64 = 16 * 1024 * 1024;
/// `base` plus the transfer time of one `max_bytes` frame.
pub(super) fn frame_budget(base: Duration, max_bytes: usize) -> Duration {
    base + Duration::from_millis(max_bytes as u64 * 1000 / FRAME_BYTES_PER_SECOND)
}
#[derive(Deserialize)]
struct Frame<'a> {
    method: Option<serde::de::IgnoredAny>,
    id: Option<Value>,
    #[serde(borrow)]
    result: Option<&'a RawValue>,
    error: Option<NativeError>,
    // Payloads of unsolicited events (including reasoning) are never deserialized.
}

#[derive(Deserialize)]
struct NativeError {
    code: i64,
    data: Option<ErrorData>,
}
#[derive(Deserialize)]
#[serde(untagged)]
enum ErrorData {
    Typed {
        #[serde(rename = "codexErrorInfo")]
        info: ErrorInfo,
    },
    Other(serde::de::IgnoredAny),
}
#[derive(Deserialize)]
#[serde(untagged)]
enum ErrorInfo {
    NotSteerable {
        #[serde(rename = "activeTurnNotSteerable")]
        kind: NotSteerable,
    },
    Other(serde::de::IgnoredAny),
}
#[derive(Deserialize)]
struct NotSteerable {
    #[serde(rename = "turnKind")]
    kind: String,
}
impl NativeError {
    fn non_steerable(&self) -> Option<&'static str> {
        if self.code != -32600 {
            return None;
        }
        match &self.data {
            Some(ErrorData::Typed {
                info: ErrorInfo::NotSteerable { kind },
            }) => match kind.kind.as_str() {
                "compact" => Some("compact_turn_not_steerable"),
                "review" => Some("review_turn_not_steerable"),
                _ => None,
            },
            _ => None,
        }
    }
}
#[derive(Clone, Debug, serde::Serialize)]
pub struct NativeReadFailure {
    pub method: String,
    pub code: i64,
}
impl std::fmt::Display for NativeReadFailure {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(
            f,
            "Native read rejected (method={}, code={})",
            self.method, self.code
        )
    }
}
impl std::error::Error for NativeReadFailure {}

pub(super) struct Transport {
    writer: futures_util::stream::SplitSink<WebSocketStream<UnixStream>, Message>,
    pending: Pending,
    reader: tokio::task::JoinHandle<()>,
    sequence: u64,
    rejections: Rejections,
    pub(super) last_rejection: Option<NativeReadFailure>,
    last_non_steerable: Option<&'static str>,
    verified_profile: bool,
    pub(super) provider_version: Option<String>,
    max_bytes: usize,
}
impl Drop for Transport {
    fn drop(&mut self) {
        self.reader.abort();
    }
}
impl Transport {
    pub(super) async fn connect(path: &Path, max_bytes: usize) -> Result<Self> {
        tokio::time::timeout(Duration::from_secs(3), Self::open(path, max_bytes))
            .await
            .map_err(|_| Fault("endpoint_unavailable"))?
    }
    async fn open(path: &Path, max_bytes: usize) -> Result<Self> {
        let stream = UnixStream::connect(path)
            .await
            .map_err(|_| Fault("endpoint_unavailable"))?;
        let config = WebSocketConfig::default()
            .max_message_size(Some(max_bytes))
            .max_frame_size(Some(max_bytes));
        let (socket, _) =
            tokio_tungstenite::client_async_with_config("ws://localhost/", stream, Some(config))
                .await
                .map_err(|_| Fault("endpoint_unavailable"))?;
        let (writer, mut reader) = socket.split();
        let pending: Pending = Arc::new(Mutex::new(HashMap::new()));
        let waiting = pending.clone();
        let rejections = Arc::new(Mutex::new(HashMap::new()));
        let rejected = rejections.clone();
        let task = tokio::spawn(async move {
            let mut failure = Fault("disconnected");
            while let Some(incoming) = reader.next().await {
                let message = match incoming {
                    Ok(message) => message,
                    Err(tokio_tungstenite::tungstenite::Error::Capacity(_)) => {
                        failure = Fault("native_frame_limit");
                        break;
                    }
                    Err(_) => break,
                };
                match message {
                    Message::Text(text) => {
                        let Ok(frame) = serde_json::from_str::<Frame>(text.as_str()) else {
                            break;
                        };
                        // Server requests have their own ID namespace; never consume an RPC waiter.
                        if frame.method.is_some() {
                            continue;
                        }
                        if let Some(id) = frame.id.and_then(|id| id.as_u64()) {
                            if let Some(tx) = waiting.lock().await.remove(&id) {
                                let result = if let Some(error) = frame.error {
                                    rejected
                                        .lock()
                                        .await
                                        .insert(id, (error.code, error.non_steerable()));
                                    Err(Fault("native_rejected"))
                                } else {
                                    frame
                                        .result
                                        .map(|_| text.clone())
                                        .ok_or(Fault("malformed_response"))
                                };
                                let _ = tx.send(result);
                            }
                        }
                    }
                    Message::Close(_) | Message::Binary(_) => break,
                    _ => {}
                }
            }
            for (_, tx) in waiting.lock().await.drain() {
                let _ = tx.send(Err(failure));
            }
        });
        Ok(Self {
            writer,
            pending,
            reader: task,
            sequence: 0,
            rejections,
            last_rejection: None,
            last_non_steerable: None,
            verified_profile: false,
            provider_version: None,
            max_bytes,
        })
    }
    pub(super) async fn initialize(&mut self) -> Result<()> {
        #[derive(Deserialize)]
        struct Initialization {
            #[serde(rename = "userAgent")]
            user_agent: String,
        }
        let response: Initialization = self.request("initialize",json!({"clientInfo":{"name":"codex_switcher_messaging","version":env!("CARGO_PKG_VERSION")},"capabilities":{"experimentalApi":true}})).await?;
        // A specific inspected protocol profile, never a substring version match.
        self.provider_version = response
            .user_agent
            .split_whitespace()
            .next()
            .and_then(|s| s.rsplit_once('/'))
            .filter(|(name, version)| {
                !name.is_empty()
                    && !version.is_empty()
                    && version.len() <= 64
                    && version
                        .bytes()
                        .all(|b| b.is_ascii_alphanumeric() || matches!(b, b'.' | b'-' | b'+'))
            })
            .map(|(_, version)| version.to_owned());
        if !self
            .provider_version
            .as_deref()
            .is_some_and(|v| super::NATIVE_PROFILES.contains(&v))
        {
            return Err(Fault("unsupported_native_version"));
        }
        self.writer
            .send(Message::Text(
                json!({"method":"initialized","params":{}})
                    .to_string()
                    .into(),
            ))
            .await
            .map_err(|_| Fault("disconnected"))?;
        self.verified_profile = true;
        Ok(())
    }
    /// Stock Enter-equivalent after the adapter's idle check. The native operation
    /// may steer if a concurrent caller starts work; it has no atomic idle guard.
    pub(super) async fn start_or_steer(
        &mut self,
        session: &str,
        client: &str,
        text: &str,
    ) -> Result<String> {
        super::uuid(session)?;
        super::uuid(client)?;
        if !self.verified_profile {
            return Err(Fault("unsupported"));
        }
        if text.is_empty() || text.len() > 16384 {
            return Err(Fault("invalid_request"));
        }
        #[derive(Deserialize)]
        struct Ack {
            turn: Turn,
        }
        #[derive(Deserialize)]
        struct Turn {
            id: String,
            status: String,
        }
        let ack:Ack=self.rpc("turn/start",json!({"threadId":session,"clientUserMessageId":client,"input":[{"type":"text","text":text,"text_elements":[]}]})).await?;
        if ack.turn.id.is_empty()
            || ack.turn.id.len() > 256
            || ack.turn.id.chars().any(char::is_control)
            || ack.turn.status != "inProgress"
        {
            return Err(Fault("malformed_response"));
        }
        Ok(ack.turn.id)
    }
    /// Explicit recovery only. Stock queue/start atomically requires idle and
    /// consumes this exact queued item; it never steers or selects the queue head.
    pub(super) async fn start_queued(&mut self, session: &str, queued: &str) -> Result<String> {
        super::uuid(session)?;
        if !self.verified_profile || queued.is_empty() || queued.len() > 256 {
            return Err(Fault("unsupported"));
        }
        #[derive(Deserialize)]
        struct Ack {
            turn: Turn,
        }
        #[derive(Deserialize)]
        struct Turn {
            id: String,
            status: String,
        }
        let ack: Ack = self
            .rpc(
                "thread/queue/start",
                json!({"threadId":session,"queuedSubmissionId":queued}),
            )
            .await?;
        if ack.turn.id.is_empty()
            || ack.turn.id.len() > 256
            || ack.turn.id.chars().any(char::is_control)
            || ack.turn.status != "inProgress"
        {
            return Err(Fault("malformed_response"));
        }
        Ok(ack.turn.id)
    }
    /// Typed explicit in-turn steering; the generic operation allowlist remains non-steering.
    pub(super) async fn steer(
        &mut self,
        session: &str,
        client: &str,
        turn: &str,
        text: &str,
    ) -> Result<String> {
        super::uuid(session)?;
        super::uuid(client)?;
        if !self.verified_profile {
            return Err(Fault("unsupported"));
        }
        if turn.is_empty()
            || turn.len() > 256
            || turn.chars().any(char::is_control)
            || text.is_empty()
            || text.len() > 16384
        {
            return Err(Fault("invalid_request"));
        }
        #[derive(Deserialize)]
        #[serde(rename_all = "camelCase")]
        struct Ack {
            turn_id: String,
        }
        let ack: Ack = self.rpc("turn/steer", json!({"threadId":session,"clientUserMessageId":client,"expectedTurnId":turn,"input":[{"type":"text","text":text,"text_elements":[]}]})).await.map_err(|e| if e.0 == "native_rejected" { self.last_non_steerable.map(Fault).unwrap_or(e) } else { e })?;
        if ack.turn_id != turn {
            return Err(Fault("correlation_changed"));
        }
        Ok(ack.turn_id)
    }
    pub(super) async fn request<T: serde::de::DeserializeOwned>(
        &mut self,
        method: &str,
        params: Value,
    ) -> Result<T> {
        // Generic RPC cannot launch or steer; busy steering uses the bounded typed method.
        if !matches!(
            method,
            "initialize"
                | "thread/read"
                | "thread/queue/add"
                | "thread/queue/list"
                | "thread/queue/delete"
                | "thread/turns/list"
        ) {
            return Err(Fault("unsupported"));
        }
        if method == "thread/turns/list" && params["itemsView"] != "summary" {
            return Err(Fault("unsupported"));
        }
        self.rpc(method, params).await
    }
    async fn rpc<T: serde::de::DeserializeOwned>(
        &mut self,
        method: &str,
        params: Value,
    ) -> Result<T> {
        self.rpc_budget(method, params, Duration::from_secs(3))
            .await
    }
    /// Passive summary reconstruction shares the caller's existing five-second
    /// observation deadline instead of imposing an earlier per-RPC cutoff.
    pub(super) async fn passive_summary<T: serde::de::DeserializeOwned>(
        &mut self,
        id: &str,
    ) -> Result<T> {
        super::uuid(id)?;
        self.rpc_budget(
            "thread/turns/list",
            json!({"threadId":id,"itemsView":"summary","sortDirection":"desc","limit":1}),
            Duration::from_secs(5),
        )
        .await
    }
    /// One summary page under the public receipt observation budget. Never
    /// permits full-item reads, writes, launch or steering.
    pub(super) async fn passive_summary_page<T: serde::de::DeserializeOwned>(
        &mut self,
        id: &str,
        cursor: Option<&str>,
        limit: u32,
    ) -> Result<T> {
        super::uuid(id)?;
        if !(1..=50).contains(&limit) || cursor.is_some_and(|s| s.len() > 4096) {
            return Err(Fault("unsupported"));
        }
        self.rpc_budget("thread/turns/list",json!({"threadId":id,"itemsView":"summary","sortDirection":"desc","limit":limit,"cursor":cursor}),Duration::from_secs(5)).await
    }
    /// Bounded exact-turn item read, projected to public assistant text by checkpoints.
    pub(super) async fn checkpoint_items<T: serde::de::DeserializeOwned>(
        &mut self,
        id: &str,
        turn: &str,
        cursor: Option<&str>,
    ) -> Result<T> {
        super::uuid(id)?;
        if !self.verified_profile
            || turn.is_empty()
            || turn.len() > 256
            || cursor.is_some_and(|s| s.len() > 4096)
        {
            return Err(Fault("unsupported"));
        }
        self.rpc_budget(
            "thread/items/list",
            json!({"threadId":id,"turnId":turn,"cursor":cursor,"limit":50,"sortDirection":"asc"}),
            frame_budget(Duration::from_secs(3), self.max_bytes),
        )
        .await
    }
    /// Steer reply attribution: one full turn per page, newest first. The caller
    /// verifies the exact turn ID and materializes only item identifiers, client
    /// IDs and assistant text; never writes, launches or steers.
    pub(super) async fn steer_turn_page<T: serde::de::DeserializeOwned>(
        &mut self,
        id: &str,
        cursor: Option<&str>,
    ) -> Result<T> {
        super::uuid(id)?;
        if !self.verified_profile || cursor.is_some_and(|s| s.is_empty() || s.len() > 4096) {
            return Err(Fault("unsupported"));
        }
        self.rpc_budget(
            "thread/turns/list",
            json!({"threadId":id,"itemsView":"full","sortDirection":"desc","limit":1,"cursor":cursor}),
            frame_budget(Duration::from_secs(3), self.max_bytes),
        )
        .await
    }
    /// Only the checkpoint projection may request one full latest legacy turn.
    /// The caller verifies its exact turn ID and discards non-public items.
    pub(super) async fn checkpoint_legacy_turn<T: serde::de::DeserializeOwned>(
        &mut self,
        id: &str,
    ) -> Result<T> {
        super::uuid(id)?;
        if !self.verified_profile {
            return Err(Fault("unsupported"));
        }
        self.rpc_budget(
            "thread/turns/list",
            json!({"threadId":id,"itemsView":"full","sortDirection":"desc","limit":1}),
            frame_budget(Duration::from_secs(3), self.max_bytes),
        )
        .await
    }
    async fn rpc_budget<T: serde::de::DeserializeOwned>(
        &mut self,
        method: &str,
        params: Value,
        budget: Duration,
    ) -> Result<T> {
        self.last_rejection = None;
        self.last_non_steerable = None;
        self.sequence += 1;
        let id = self.sequence;
        let (tx, rx) = oneshot::channel();
        self.pending.lock().await.insert(id, tx);
        let request = async {
            self.writer
                .send(Message::Text(
                    json!({"id":id,"method":method,"params":params})
                        .to_string()
                        .into(),
                ))
                .await
                .map_err(|_| Fault("disconnected"))?;
            #[derive(Deserialize)]
            struct Envelope<T> {
                result: T,
            }
            let frame = rx.await.map_err(|_| Fault("disconnected"))??;
            serde_json::from_str::<Envelope<T>>(frame.as_str())
                .map(|e| e.result)
                .map_err(|_| Fault("malformed_response"))
        };
        let result = tokio::time::timeout(budget, request)
            .await
            .unwrap_or(Err(Fault("timeout")));
        self.pending.lock().await.remove(&id);
        if let Some((code, kind)) = self.rejections.lock().await.remove(&id) {
            self.last_non_steerable = kind;
            self.last_rejection = Some(NativeReadFailure {
                method: method.to_owned(),
                code,
            });
        }
        result
    }
}

#[cfg(test)]
mod budget_tests {
    use super::*;
    #[test]
    fn full_read_budget_scales_with_the_frame_bound() {
        let base = Duration::from_secs(3);
        // An ordinary 1 MiB connection keeps essentially its base budget.
        assert!(frame_budget(base, FRAME_LIMIT) < base + Duration::from_millis(100));
        // A 64 MiB steer frame gets the time to move it at the budgeted floor.
        assert_eq!(
            frame_budget(base, STEER_FRAME_LIMIT),
            base + Duration::from_secs(4)
        );
        assert!(frame_budget(base, CHECKPOINT_FRAME_LIMIT) > base);
    }
}
