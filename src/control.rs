//! Connect to an existing Codex server; never start a second credential owner.
use anyhow::{Context, Result, bail, ensure};
use futures_util::{SinkExt, StreamExt};
use serde_json::{Value, json};
use std::{path::Path, time::Duration};
use tokio::net::UnixStream;
use tokio_tungstenite::{WebSocketStream, tungstenite::Message};

pub struct Control {
    socket: WebSocketStream<UnixStream>,
    sequence: u64,
}
pub fn available(home: &Path) -> bool {
    home.join("app-server-control/app-server-control.sock")
        .exists()
}

/// A failed native turn can leave the thread in `systemError` even though no
/// turn is running (for example after an account reaches its usage limit).
/// The error state alone is never sufficient evidence to stop its terminal.
pub fn confirmed_quiescent(thread: &Value, allow_unloaded: bool) -> bool {
    if thread["status"]["activeFlags"]
        .as_array()
        .is_some_and(|flags| !flags.is_empty())
    {
        return false;
    }
    match thread["status"]["type"].as_str() {
        Some("idle") => true,
        Some("notLoaded") => allow_unloaded,
        Some("systemError") => {
            let Some(turns) = thread["turns"].as_array() else {
                return false;
            };
            turns.last().is_some_and(|turn| {
                matches!(
                    turn["status"].as_str(),
                    Some("completed" | "failed" | "interrupted")
                )
            }) && turns.iter().all(|turn| {
                matches!(
                    turn["status"].as_str(),
                    Some("completed" | "failed" | "interrupted")
                )
            })
        }
        _ => false,
    }
}

fn verify_permissions(result: &Value, approval: Option<&str>, sandbox: Option<&str>) -> Result<()> {
    if let Some(expected) = approval {
        ensure!(
            result["approvalPolicy"] == expected,
            "Codex did not restore the requested approval mode"
        );
    }
    if let Some(expected) = sandbox {
        let native = match expected {
            "danger-full-access" => "dangerFullAccess",
            "workspace-write" => "workspaceWrite",
            "read-only" => "readOnly",
            _ => bail!("unsupported sandbox mode for restoration"),
        };
        ensure!(
            result["sandbox"]["type"] == native,
            "Codex did not restore the requested permissions ({expected})"
        );
        if expected == "danger-full-access" {
            ensure!(
                result["activePermissionProfile"]["id"] == ":danger-full-access",
                "Codex did not activate the full-access permission profile"
            );
        }
    }
    Ok(())
}

impl Control {
    pub async fn connect(home: &Path) -> Result<Self> {
        Self::connect_socket(&home.join("app-server-control/app-server-control.sock")).await
    }

    pub async fn connect_socket(path: &Path) -> Result<Self> {
        let mut client = tokio::time::timeout(Duration::from_secs(2), async {
            let stream = UnixStream::connect(path).await?;
            let config = tokio_tungstenite::tungstenite::protocol::WebSocketConfig::default()
                .max_message_size(Some(16 * 1024 * 1024));
            let (socket, _) = tokio_tungstenite::client_async_with_config(
                "ws://localhost/",
                stream,
                Some(config),
            )
            .await?;
            Ok::<_, anyhow::Error>(Self {
                socket,
                sequence: 0,
            })
        })
        .await
        .context("Codex control connection timed out")??;
        client
            .request(
                "initialize",
                json!({
                    "clientInfo":{"name":"codex_switcher", "version":env!("CARGO_PKG_VERSION")},
                    "capabilities":{"experimentalApi":true}
                }),
            )
            .await?;
        client
            .socket
            .send(Message::Text(
                json!({"method":"initialized"}).to_string().into(),
            ))
            .await?;
        Ok(client)
    }

    pub async fn request(&mut self, method: &str, params: Value) -> Result<Value> {
        self.request_timeout(method, params, Duration::from_secs(3))
            .await
    }

    pub async fn request_timeout(
        &mut self,
        method: &str,
        params: Value,
        timeout: Duration,
    ) -> Result<Value> {
        self.sequence += 1;
        let id = self.sequence;
        tokio::time::timeout(timeout, async {
            self.socket
                .send(Message::Text(
                    json!({"id":id,"method":method,"params":params})
                        .to_string()
                        .into(),
                ))
                .await?;
            while let Some(frame) = self.socket.next().await {
                let frame = frame?;
                if !frame.is_text() {
                    continue;
                }
                let value: Value = serde_json::from_str(frame.to_text()?)?;
                if value["id"] != id {
                    continue;
                }
                ensure!(
                    value.get("error").is_none(),
                    "Codex rejected {method} (code {})",
                    value["error"]["code"]
                );
                return Ok(value["result"].clone());
            }
            bail!("Codex disconnected during {method}; outcome may be unknown")
        })
        .await
        .context("Codex control request timed out; outcome may be unknown")?
    }

    pub async fn thread(&mut self, id: &str, turns: bool) -> Result<Value> {
        Ok(self
            .request("thread/read", json!({"threadId":id,"includeTurns":turns}))
            .await?["thread"]
            .clone())
    }

    pub async fn thread_for_state(&mut self, id: &str) -> Result<Value> {
        let mut thread = self.thread(id, false).await?;
        if thread["status"]["type"] == "systemError" {
            // Large histories may time out or exceed the control-frame budget.
            // A bounded latest-turn summary proves termination without fetching items.
            let page = self
                .request(
                    "thread/turns/list",
                    json!({"threadId":id,"itemsView":"summary","sortDirection":"desc","limit":1}),
                )
                .await?;
            let turns = page["data"]
                .as_array()
                .context("Codex returned no turn summary")?;
            ensure!(turns.len() <= 1, "NATIVE_TURN_SUMMARY_INVALID");
            let after = self.thread(id, false).await?;
            ensure!(
                after["id"] == id && thread["id"] == id && after["status"] == thread["status"],
                "NATIVE_THREAD_STATE_CHANGED"
            );
            thread["turns"] = json!(turns);
        }
        Ok(thread)
    }

    /// Recheck accepted input and background work at every unload boundary,
    /// including recovery. An earlier idle observation is not a reservation.
    pub async fn verify_unloadable(&mut self, id: &str, allow_unloaded: bool) -> Result<()> {
        let queue = self
            .request("thread/queue/list", json!({"threadId":id,"limit":1}))
            .await?;
        ensure!(
            queue["data"]
                .as_array()
                .is_some_and(|items| items.is_empty()),
            "conversation has queued input; source retained until it is submitted or removed"
        );
        let goal = self
            .request("thread/goal/get", json!({"threadId":id}))
            .await?;
        ensure!(
            goal["goal"]["status"] != "active",
            "conversation has an active goal; pause it before continuing"
        );
        ensure!(
            self.terminals(id).await?.is_empty(),
            "conversation has running background terminals; source retained"
        );
        ensure!(
            confirmed_quiescent(&self.thread_for_state(id).await?, allow_unloaded),
            "conversation is still working or its state is unconfirmed; source retained"
        );
        Ok(())
    }

    /// Restore the native full-access profile, not just its legacy sandbox view.
    pub async fn resume_with_permissions(&mut self, mut params: Value) -> Result<Value> {
        let approval = params["approvalPolicy"].as_str().map(str::to_owned);
        let sandbox = params["sandbox"].as_str().map(str::to_owned);
        if sandbox.as_deref() == Some("danger-full-access") {
            params
                .as_object_mut()
                .context("invalid resume parameters")?
                .remove("sandbox");
            params["permissions"] = json!(":danger-full-access");
        }
        let result = self
            .request_timeout("thread/resume", params, Duration::from_secs(120))
            .await?;
        verify_permissions(&result, approval.as_deref(), sandbox.as_deref())?;
        Ok(result)
    }

    pub async fn loaded(&mut self) -> Result<Vec<Value>> {
        let mut result = Vec::new();
        let mut cursor = Value::Null;
        loop {
            let page = self
                .request("thread/loaded/list", json!({"cursor":cursor,"limit":100}))
                .await?;
            for id in page["data"]
                .as_array()
                .context("Codex returned no loaded-thread list")?
            {
                if let Some(id) = id.as_str() {
                    result.push(self.thread(id, false).await?);
                }
            }
            cursor = page["nextCursor"].clone();
            if cursor.is_null() {
                break;
            }
        }
        Ok(result)
    }

    pub async fn terminals(&mut self, id: &str) -> Result<Vec<Value>> {
        // Codex 0.163 rejects backgroundTerminals/list for an unloaded thread.
        // Confirm both views, then recheck the thread; an RPC failure or a
        // partially loaded view is never interpreted as an empty terminal list.
        if self.thread_for_state(id).await?["status"]["type"] == "notLoaded" {
            let mut cursor = Value::Null;
            let mut seen = std::collections::BTreeSet::new();
            loop {
                let page = self
                    .request("thread/loaded/list", json!({"cursor":cursor,"limit":100}))
                    .await?;
                let loaded = page["data"]
                    .as_array()
                    .context("Codex returned no loaded-thread list")?;
                ensure!(
                    !loaded.iter().any(|v| v.as_str() == Some(id)),
                    "NATIVE_THREAD_STATE_CHANGED"
                );
                cursor = page["nextCursor"].clone();
                if cursor.is_null() {
                    break;
                }
                ensure!(
                    seen.insert(cursor.to_string()),
                    "NATIVE_THREAD_LIST_INCOMPLETE"
                );
            }
            ensure!(
                self.thread_for_state(id).await?["status"]["type"] == "notLoaded",
                "NATIVE_THREAD_STATE_CHANGED"
            );
            return Ok(vec![]);
        }
        let mut result = Vec::new();
        let mut cursor = Value::Null;
        loop {
            let page = self
                .request(
                    "thread/backgroundTerminals/list",
                    json!({"threadId":id,"cursor":cursor,"limit":100}),
                )
                .await?;
            result.extend(
                page["data"]
                    .as_array()
                    .context("Codex returned no background-terminal list")?
                    .iter()
                    .cloned(),
            );
            cursor = page["nextCursor"].clone();
            if cursor.is_null() {
                return Ok(result);
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn failed_system_error_is_quiescent_only_with_terminal_turn_evidence() {
        let mut thread = json!({"status":{"type":"systemError"},"turns":[
            {"status":"completed"},
            {"status":"failed","error":{"codexErrorInfo":"usageLimitExceeded"}}
        ]});
        assert!(confirmed_quiescent(&thread, false));
        thread["turns"][0]["status"] = json!("inProgress");
        assert!(!confirmed_quiescent(&thread, false));
        thread["turns"][0]["status"] = json!("completed");
        thread["turns"][1]["status"] = json!("inProgress");
        assert!(!confirmed_quiescent(&thread, false));
        thread["turns"] = json!([]);
        assert!(!confirmed_quiescent(&thread, false));
        thread["status"]["type"] = json!("active");
        assert!(!confirmed_quiescent(&thread, false));
        for status in ["completed", "interrupted", "failed"] {
            thread = json!({"status":{"type":"systemError"},"turns":[{"status":status}]});
            assert!(confirmed_quiescent(&thread, false));
            thread["status"]["activeFlags"] = json!(["waitingOnApproval"]);
            assert!(!confirmed_quiescent(&thread, false));
        }
    }

    #[test]
    fn full_access_requires_matching_sandbox_approval_and_named_profile() {
        let correct = json!({"approvalPolicy":"never","sandbox":{"type":"dangerFullAccess"},
            "activePermissionProfile":{"id":":danger-full-access"}});
        assert!(verify_permissions(&correct, Some("never"), Some("danger-full-access")).is_ok());
        for field in [
            "/approvalPolicy",
            "/sandbox/type",
            "/activePermissionProfile/id",
        ] {
            let mut wrong = correct.clone();
            *wrong.pointer_mut(field).unwrap() = Value::Null;
            assert!(verify_permissions(&wrong, Some("never"), Some("danger-full-access")).is_err());
        }
    }
}
