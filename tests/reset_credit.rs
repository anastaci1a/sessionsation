//! `api account reset-credit` (contract 1.53.0) against the fake Codex app-server
//! only (tests/support/fake-codex.sh). No real account or app-server is involved.
use serde_json::{Value, json};
use std::{
    fs,
    os::unix::fs::PermissionsExt,
    path::Path,
    process::{Command, Stdio},
    time::Duration,
};

fn binary() -> &'static str {
    env!("CARGO_BIN_EXE_sessionsation")
}
struct Process(std::process::Child);
impl Drop for Process {
    fn drop(&mut self) {
        if self.0.try_wait().ok().flatten().is_none() {
            unsafe {
                libc::kill(self.0.id() as i32, libc::SIGTERM);
            }
        }
        let _ = self.0.wait();
    }
}
/// (ok, result-or-error) of one `api` call.
fn api(root: &Path, args: &[&str]) -> (bool, Value) {
    let out = Command::new(binary())
        .arg("--data-dir")
        .arg(root)
        .arg("api")
        .args(args)
        .output()
        .unwrap();
    if out.status.success() {
        let v: Value = serde_json::from_slice(&out.stdout).unwrap();
        (true, v["result"].clone())
    } else {
        let text = String::from_utf8_lossy(&out.stderr);
        let line = text
            .lines()
            .rev()
            .find(|l| l.starts_with('{'))
            .unwrap_or("");
        let v: Value = serde_json::from_str(line)
            .unwrap_or_else(|_| panic!("no error envelope in stderr: {text}"));
        (false, v["error"].clone())
    }
}
fn lines(path: &Path) -> Vec<String> {
    fs::read_to_string(path)
        .unwrap_or_default()
        .lines()
        .map(str::to_owned)
        .collect()
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn reset_credit_outcomes_idempotency_and_refreshed_usage() {
    use sessionsation::{daemon, paths::Paths, provider::ProviderId};
    let tmp = tempfile::tempdir().unwrap();
    let fake = tmp.path().join("fake-codex");
    fs::write(&fake, include_str!("support/fake-codex.sh")).unwrap();
    fs::set_permissions(&fake, fs::Permissions::from_mode(0o700)).unwrap();
    let paths = Paths::new(Some(tmp.path().join("state"))).unwrap();
    // Default idle cadence (120 s): after the first poll, only the redemption
    // itself can refresh the cached usage during this test.
    sessionsation::store::save_config(
        &paths,
        &sessionsation::config::Config {
            codex_binary: fake.to_str().unwrap().into(),
            ..Default::default()
        },
    )
    .unwrap();
    let mut server = Process(
        Command::new(binary())
            .arg("--data-dir")
            .arg(&paths.root)
            .args(["daemon", "serve"])
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .spawn()
            .unwrap(),
    );
    let deadline = std::time::Instant::now() + Duration::from_secs(10);
    while daemon::call(&paths, "status", json!({})).await.is_err() {
        assert!(server.0.try_wait().unwrap().is_none(), "supervisor exited");
        assert!(std::time::Instant::now() < deadline, "supervisor not ready");
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
    daemon::call(&paths, "account/add", json!({"name":"first"}))
        .await
        .unwrap();
    let mut observed = Value::Null;
    for _ in 0..200 {
        let v = daemon::call(&paths, "status", json!({})).await.unwrap();
        if v["accounts"][0]["usage_fresh"] == true {
            observed = v["accounts"][0]["usage"].clone();
            break;
        }
        tokio::time::sleep(Duration::from_millis(30)).await;
    }
    assert_eq!(observed["short"]["used_percent"], 23.);
    // Not reported by the app-server: null, never invented.
    assert!(observed.get("reset_credits").is_some_and(Value::is_null));
    let home = paths.account("first").unwrap();
    let root = paths.root.clone();
    let keys = home.join("fake-reset-keys");
    let journaled = home.join("fake-reset-journaled");

    // reset: journaled before the request, then the cached usage is refreshed.
    fs::write(home.join("fake-reset-outcome"), "reset").unwrap();
    fs::write(home.join("fake-reset-available"), "3").unwrap();
    let id = uuid::Uuid::new_v4().to_string();
    let (ok, r) = api(
        &root,
        &[
            "account",
            "reset-credit",
            "codex:first",
            "--request-id",
            &id,
        ],
    );
    assert!(ok, "{r}");
    assert_eq!(r["account_id"], "codex:first");
    assert_eq!(r["request_id"], id.as_str());
    assert_eq!(r["outcome"], "reset");
    assert_eq!(r["attempts"], 1);
    assert_eq!(r["rate_limits_after"]["short"]["used_percent"], 0.);
    assert_eq!(r["rate_limits_after"]["weekly"]["used_percent"], 0.);
    assert_eq!(r["rate_limits_after"]["reset_credits"]["available"], 2);
    assert_eq!(lines(&keys), vec![id.clone()]);
    assert_eq!(lines(&journaled), vec![id.clone()], "journal before RPC");
    let record: Value = serde_json::from_slice(
        &fs::read(root.join(format!("operations/account-reset-credits/{id}.json"))).unwrap(),
    )
    .unwrap();
    assert_eq!(record["phase"], "complete");
    assert_eq!(record["outcome"], "reset");
    assert_eq!(record["idempotency_key"], id.as_str());
    // The supervisor's cached usage shows the reset at once (no poll ran).
    let status = daemon::call(&paths, "status", json!({})).await.unwrap();
    let usage = &status["accounts"][0]["usage"];
    assert_eq!(usage["short"]["used_percent"], 0.);
    assert_eq!(usage["reset_credits"]["available"], 2);
    assert!(usage["reset_credits"]["observed_at"].as_i64().unwrap() > 0);
    // ... and every public view of it.
    let (ok, limits) = api(&root, &["account", "limits", "codex:first"]);
    assert!(ok, "{limits}");
    assert_eq!(limits["usage"]["reset_credits"]["available"], 2);
    let (ok, list) = api(&root, &["account", "list"]);
    assert!(ok, "{list}");
    assert_eq!(list[0]["usage"]["reset_credits"]["available"], 2);
    assert_eq!(list[0]["usage"]["short"]["used_percent"], 0.);
    let start = (daemon::now() / 1800) * 1800 - 1800;
    let (ok, history) = api(
        &root,
        &[
            "account",
            "usage",
            "codex:first",
            "--start",
            &start.to_string(),
            "--end",
            &(start + 3600).to_string(),
        ],
    );
    assert!(ok, "{history}");
    assert_eq!(history["reset_credits"]["available"], 2);
    let (ok, state) = api(&root, &["state", "--cached"]);
    assert!(ok, "{state}");
    let row = state["accounts"]
        .as_array()
        .unwrap()
        .iter()
        .find(|a| a["name"] == "first")
        .unwrap()
        .clone();
    assert_eq!(row["usage"]["reset_credits"]["available"], 2);

    // A retry with the same request id sends the same key: alreadyRedeemed.
    let (ok, again) = api(
        &root,
        &[
            "account",
            "reset-credit",
            "--account",
            "codex:first",
            "--request-id",
            &id,
        ],
    );
    assert!(ok, "{again}");
    assert_eq!(again["outcome"], "alreadyRedeemed");
    assert_eq!(again["attempts"], 2);
    assert_eq!(lines(&keys), vec![id.clone(), id.clone()]);
    assert_eq!(lines(&journaled), vec![id.clone(), id.clone()]);
    // The same id for another credit is a different attempt.
    let (ok, e) = api(
        &root,
        &[
            "account",
            "reset-credit",
            "codex:first",
            "--credit-id",
            "c-1",
            "--request-id",
            &id,
        ],
    );
    assert!(!ok);
    assert_eq!(e["code"], "REQUEST_ID_CONFLICT", "{e}");
    assert_eq!(lines(&keys).len(), 2, "nothing sent");

    // The other outcomes; a request id is generated and journaled when omitted.
    for outcome in ["nothingToReset", "noCredit"] {
        fs::write(home.join("fake-reset-outcome"), outcome).unwrap();
        let (ok, r) = api(&root, &["account", "reset-credit", "codex:first"]);
        assert!(ok, "{r}");
        assert_eq!(r["outcome"], outcome);
        assert!(r["rate_limits_after"].is_null());
        let generated = r["request_id"].as_str().unwrap().to_owned();
        uuid::Uuid::parse_str(&generated).unwrap();
        assert_ne!(generated, id);
        assert_eq!(lines(&keys).last(), Some(&generated));
        assert_eq!(lines(&journaled).last(), Some(&generated));
        assert_eq!(r["operation"]["outcome"], outcome);
    }
    let (ok, ops) = api(&root, &["operations"]);
    assert!(ok, "{ops}");
    let ours: Vec<_> = ops
        .as_array()
        .unwrap()
        .iter()
        .filter(|o| o["kind"] == "account_reset_credit")
        .collect();
    assert_eq!(ours.len(), 3);

    // An account operation holds the lock: refused before anything is sent.
    {
        let _lock =
            sessionsation::accounts::operation_lock(&paths, ProviderId::Codex, "first").unwrap();
        let (ok, e) = api(&root, &["account", "reset-credit", "codex:first"]);
        assert!(!ok);
        assert_eq!(e["code"], "ACCOUNT_BUSY", "{e}");
        assert_eq!(lines(&keys).len(), 4);
    }

    // A codex without the method: a clear error naming the version.
    fs::write(home.join("fake-reset-unsupported"), "").unwrap();
    let unsupported = uuid::Uuid::new_v4().to_string();
    let (ok, e) = api(
        &root,
        &[
            "account",
            "reset-credit",
            "codex:first",
            "--request-id",
            &unsupported,
        ],
    );
    assert!(!ok);
    assert_eq!(e["code"], "UNSUPPORTED_PROVIDER_OPERATION", "{e}");
    let message = e["message"].as_str().unwrap();
    assert!(message.contains("codex-cli test-fixture"), "{message}");
    assert!(
        message.contains("account/rateLimitResetCredit/consume"),
        "{message}"
    );
    assert!(!message.contains("unknown variant"), "{message}");
    assert_eq!(lines(&keys).len(), 5, "sent once, never retried");
    let record: Value = serde_json::from_slice(
        &fs::read(root.join(format!(
            "operations/account-reset-credits/{unsupported}.json"
        )))
        .unwrap(),
    )
    .unwrap();
    assert_eq!(record["phase"], "failed");
    assert!(record["outcome"].is_null());

    // Invalid request ids never reach the supervisor.
    let (ok, e) = api(
        &root,
        &[
            "account",
            "reset-credit",
            "codex:first",
            "--request-id",
            "x",
        ],
    );
    assert!(!ok);
    assert_eq!(e["code"], "INVALID_REQUEST_ID", "{e}");
    assert_eq!(lines(&keys).len(), 5);

    // Claude has its own adapter (contract 1.60.0): an unknown account is not found.
    let (ok, e) = api(&root, &["account", "reset-credit", "claude:nobody"]);
    assert!(!ok);
    assert_eq!(e["code"], "ACCOUNT_NOT_FOUND", "{e}");
    // Other providers are refused.
    for other in ["gemini:nobody", "antigravity:nobody"] {
        let (ok, e) = api(&root, &["account", "reset-credit", other]);
        assert!(!ok);
        assert_eq!(e["code"], "UNSUPPORTED_PROVIDER_OPERATION", "{other}: {e}");
    }
    assert_eq!(lines(&keys).len(), 5);
}
