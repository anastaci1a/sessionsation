use crate::{paths::Paths, rpc::Rpc};
use anyhow::{Context, Result};
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use std::{path::Path, time::Duration};
use tokio::process::Command;

#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Outcome {
    Passed,
    Failed,
    NotTested,
}
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct Check {
    pub name: String,
    pub outcome: Outcome,
    pub detail: String,
}
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct Report {
    pub schema_version: u32,
    pub codex_version: String,
    pub automatic_switching_ready: bool,
    pub checks: Vec<Check>,
}
impl Report {
    pub fn unverified(version: String) -> Self {
        Self { schema_version: 1, codex_version: version, automatic_switching_ready: false, checks: vec![
            Check { name: "cross_account_continuation".into(), outcome: Outcome::NotTested, detail: "Requires two enrolled accounts and an isolated live conversation.".into() },
            Check { name: "native_terminal_draft_preservation".into(), outcome: Outcome::NotTested, detail: "Requires a native terminal handoff with an unsent draft and accepted pending input.".into() },
            Check { name: "subagent_coordination_restoration".into(), outcome: Outcome::NotTested, detail: "Requires a live parent/child tree across worker restart, including pending messages.".into() },
            Check { name: "positive_tool_quiescence".into(), outcome: Outcome::NotTested, detail: "Thread idle status alone does not prove that detached processes or remote tool effects have stopped.".into() },
        ] }
    }
}
pub async fn version(binary: &str) -> Result<String> {
    let output = tokio::time::timeout(
        Duration::from_secs(10),
        Command::new(binary).arg("--version").output(),
    )
    .await??;
    anyhow::ensure!(output.status.success(), "Codex version command failed");
    Ok(String::from_utf8(output.stdout)?.trim().into())
}

/// This probe never inherits the active CODEX_HOME or invokes model inference.
/// A protocol handshake is deliberately NOT a production certification.
pub async fn probe(binary: &str, _paths: &Paths) -> Result<Report> {
    let mut report = Report::unverified(version(binary).await?);
    let temp = tempfile::tempdir()?;
    let home = temp.path().join("home");
    match Rpc::spawn(binary, &home, temp.path()).await {
        Ok(rpc) => {
            let result = rpc
                .request("account/read", json!({"refreshToken":false}))
                .await;
            let isolated = result.as_ref().is_ok_and(|v| v["account"].is_null());
            report.checks.push(Check {
                name: "isolated_stdio_handshake".into(),
                outcome: if isolated {
                    Outcome::Passed
                } else {
                    Outcome::Failed
                },
                detail: if isolated {
                    "Initialized an isolated worker without account credentials."
                } else {
                    "Could not confirm isolated unauthenticated worker."
                }
                .into(),
            });
            let stopped = rpc.stop_idle().await.is_ok();
            report.checks.push(Check {
                name: "idle_worker_graceful_exit".into(),
                outcome: if stopped {
                    Outcome::Passed
                } else {
                    Outcome::Failed
                },
                detail: "Closed stdin and waited for worker exit without a forced signal.".into(),
            });
        }
        Err(_) => report.checks.push(Check {
            name: "isolated_stdio_handshake".into(),
            outcome: Outcome::Failed,
            detail: "Could not initialize isolated app-server.".into(),
        }),
    }
    let schema_dir = temp.path().join("schema");
    let output = tokio::time::timeout(
        Duration::from_secs(30),
        Command::new(binary)
            .env("CODEX_HOME", &home)
            .args([
                "app-server",
                "generate-json-schema",
                "--experimental",
                "--out",
            ])
            .arg(&schema_dir)
            .output(),
    )
    .await??;
    if output.status.success() {
        let required = [
            "TurnSteerParams",
            "ThreadReadResponse",
            "ThreadListParams",
            "GetAccountRateLimitsResponse",
            "ThreadResumeParams",
        ];
        let missing: Vec<_> = required
            .iter()
            .filter(|n| !schema_dir.join("v2").join(format!("{n}.json")).exists())
            .collect();
        report.checks.push(Check {
            name: "required_protocol_schemas".into(),
            outcome: if missing.is_empty() {
                Outcome::Passed
            } else {
                Outcome::Failed
            },
            detail: if missing.is_empty() {
                "Usage, steering, tree enumeration, and resume schemas exist.".into()
            } else {
                format!("Missing schemas: {missing:?}")
            },
        });
        let login_path = schema_dir.join("v2/LoginAccountParams.json");
        if login_path.exists() {
            let login: Value = serde_json::from_slice(&std::fs::read(login_path)?)?;
            let internal = login["oneOf"].as_array().is_some_and(|items| {
                items.iter().any(|v| {
                    v["properties"]["type"]["enum"][0] == "chatgptAuthTokens"
                        && v["description"]
                            .as_str()
                            .unwrap_or("")
                            .contains("INTERNAL USE ONLY")
                })
            });
            report.checks.push(Check { name: "external_token_contract".into(), outcome: if internal {Outcome::Failed} else {Outcome::NotTested}, detail: if internal {"Installed schema marks chatgptAuthTokens INTERNAL USE ONLY / DO NOT USE. The planned credential broker needs a supported replacement or an explicitly supported Codex version."} else {"External-token contract requires live compatibility verification."}.into() });
        }
    } else {
        report.checks.push(Check {
            name: "required_protocol_schemas".into(),
            outcome: Outcome::Failed,
            detail: "Schema generation failed.".into(),
        });
    }
    let websocket = websocket_probe(binary, &temp.path().join("socket-home"), temp.path()).await;
    report.checks.push(Check { name:"isolated_unix_websocket".into(), outcome:if websocket.is_ok() {Outcome::Passed} else {Outcome::Failed}, detail:if websocket.is_ok() {"Initialized a WebSocket connection over a private Unix socket without authentication or inference.".into()} else {"Could not verify the native Unix WebSocket transport.".into()} });
    Ok(report)
}

async fn websocket_probe(binary: &str, home: &Path, cwd: &Path) -> Result<()> {
    use futures_util::{SinkExt, StreamExt};
    use std::process::Stdio;
    crate::paths::private_dir(home)?;
    let socket = home.join("rpc.sock");
    let mut child = Command::new(binary)
        .args(["app-server", "--listen"])
        .arg(format!("unix://{}", socket.display()))
        .env("CODEX_HOME", home)
        .env_remove("OPENAI_API_KEY")
        .env_remove("CODEX_API_KEY")
        .env_remove("CODEX_ACCESS_TOKEN")
        .current_dir(cwd)
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .process_group(0)
        .spawn()?;
    let result = tokio::time::timeout(Duration::from_secs(10),async {
        let stream = loop {
            if let Ok(stream) = tokio::net::UnixStream::connect(&socket).await {break stream;}
            tokio::time::sleep(Duration::from_millis(25)).await;
        };
        let (mut websocket,_) = tokio_tungstenite::client_async("ws://localhost/",stream).await?;
        websocket.send(tokio_tungstenite::tungstenite::Message::Text(json!({"id":1,"method":"initialize","params":{"clientInfo":{"name":"codex_switcher_probe","version":"0.1.0"}}}).to_string().into())).await?;
        loop {
            let frame = websocket.next().await.context("socket closed before initialization")??;
            if frame.is_text() {
                let response: Value = serde_json::from_str(frame.to_text()?)?;
                if response["id"] == 1 {anyhow::ensure!(response.get("result").is_some(),"initialization rejected");break;}
            }
        }
        websocket.close(None).await?;
        Ok::<_,anyhow::Error>(())
    }).await;
    // This disposable worker has never received a thread or model request.
    if let Some(pid) = child.id() {
        unsafe {
            libc::kill(pid as i32, libc::SIGTERM);
        }
    }
    let _ = tokio::time::timeout(Duration::from_secs(5), child.wait()).await;
    result.context("Unix WebSocket probe timed out")?
}

pub fn save_report(path: &Path, report: &Report) -> Result<()> {
    crate::paths::atomic_write(path, &serde_json::to_vec_pretty(report)?)
        .context("could not save compatibility report")
}

/// Fail closed. No boolean in user configuration can certify a runtime.
pub fn require_native_handoff() -> Result<()> {
    anyhow::bail!(
        "NATIVE_HANDOFF_UNVERIFIED: automatic switching is unavailable until cross-account, terminal draft, subagent restoration, and tool-quiescence gates pass. Run `sessionsation doctor --probe`; see docs/operations.md. Account monitoring and simulation are available."
    )
}
