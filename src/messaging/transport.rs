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
    tungstenite::{Message, protocol::WebSocketConfig},
};

type Pending = Arc<Mutex<HashMap<u64, oneshot::Sender<Result<Box<RawValue>>>>>>;
#[derive(Deserialize)]
struct Frame {
    method: Option<serde::de::IgnoredAny>,
    id: Option<Value>,
    result: Option<Box<RawValue>>,
    error: Option<serde::de::IgnoredAny>,
    // Payloads of unsolicited events (including reasoning) are never deserialized.
}

pub(super) struct Transport {
    writer: futures_util::stream::SplitSink<WebSocketStream<UnixStream>, Message>,
    pending: Pending,
    reader: tokio::task::JoinHandle<()>,
    sequence: u64,
    verified_profile: bool,
}
impl Drop for Transport {
    fn drop(&mut self) {
        self.reader.abort();
    }
}
impl Transport {
    pub(super) async fn connect(path: &Path) -> Result<Self> {
        tokio::time::timeout(Duration::from_secs(3), Self::open(path))
            .await
            .map_err(|_| Fault("endpoint_unavailable"))?
    }
    async fn open(path: &Path) -> Result<Self> {
        let stream = UnixStream::connect(path)
            .await
            .map_err(|_| Fault("endpoint_unavailable"))?;
        let config = WebSocketConfig::default()
            .max_message_size(Some(1024 * 1024))
            .max_frame_size(Some(1024 * 1024));
        let (socket, _) =
            tokio_tungstenite::client_async_with_config("ws://localhost/", stream, Some(config))
                .await
                .map_err(|_| Fault("endpoint_unavailable"))?;
        let (writer, mut reader) = socket.split();
        let pending: Pending = Arc::new(Mutex::new(HashMap::new()));
        let waiting = pending.clone();
        let task = tokio::spawn(async move {
            while let Some(Ok(message)) = reader.next().await {
                match message {
                    Message::Text(text) => {
                        let Ok(frame) = serde_json::from_str::<Frame>(&text) else {
                            break;
                        };
                        // Server requests have their own ID namespace; never consume an RPC waiter.
                        if frame.method.is_some() {
                            continue;
                        }
                        if let Some(id) = frame.id.and_then(|id| id.as_u64()) {
                            if let Some(tx) = waiting.lock().await.remove(&id) {
                                let result = if frame.error.is_some() {
                                    Err(Fault("native_rejected"))
                                } else {
                                    frame.result.ok_or(Fault("malformed_response"))
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
                let _ = tx.send(Err(Fault("disconnected")));
            }
        });
        Ok(Self {
            writer,
            pending,
            reader: task,
            sequence: 0,
            verified_profile: false,
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
        if !response
            .user_agent
            .split_whitespace()
            .next()
            .and_then(|s| s.rsplit_once('/'))
            .is_some_and(|(name, version)| !name.is_empty() && version == "0.163.0")
        {
            return Err(Fault("unsupported"));
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
    /// Typed busy Submit only; the generic operation allowlist remains non-steering.
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
        let ack: Ack = self.rpc("turn/steer", json!({"threadId":session,"clientUserMessageId":client,"expectedTurnId":turn,"input":[{"type":"text","text":text,"text_elements":[]}]})).await?;
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
    async fn rpc_budget<T: serde::de::DeserializeOwned>(
        &mut self,
        method: &str,
        params: Value,
        budget: Duration,
    ) -> Result<T> {
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
            let raw = rx.await.map_err(|_| Fault("disconnected"))??;
            serde_json::from_str(raw.get()).map_err(|_| Fault("malformed_response"))
        };
        let result = tokio::time::timeout(budget, request)
            .await
            .unwrap_or(Err(Fault("timeout")));
        self.pending.lock().await.remove(&id);
        result
    }
}
