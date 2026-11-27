//! `api account limits` / `api account reset-credit` for a Claude account (contract 1.60.0)
//! with the metadata-only fake Claude and a fake curl. Fixture credentials only; no network.
#![allow(
    clippy::disallowed_methods,
    reason = "integration tests are exempt from owned roles (src/guard.rs)"
)]
use serde_json::{Value, json};
use sessionsation::{daemon, paths::Paths};
use std::{
    fs,
    os::unix::fs::PermissionsExt,
    path::Path,
    process::{Child, Command, Stdio},
    time::{Duration, Instant},
};

const ACCESS: &str = "sk-ant-oat01-FIXTURE-CLI-ACCESS";
const ORG: &str = "6f1f3d5e-8a52-4b7e-9c1d-2e3f4a5b6c7d";

fn binary() -> &'static str {
    env!("CARGO_BIN_EXE_sessionsation")
}
struct Process(Child);
impl Drop for Process {
    fn drop(&mut self) {
        if self.0.try_wait().ok().flatten().is_none() {
            unsafe {
                libc::kill(self.0.id() as i32, libc::SIGKILL);
            }
        }
        let _ = self.0.wait();
    }
}
fn api(root: &Path, args: &[&str]) -> (bool, Value) {
    let out = Command::new(binary())
        .arg("--data-dir")
        .arg(root)
        .arg("api")
        .args(args)
        .env_remove("CLAUDE_CONFIG_DIR")
        .stdin(Stdio::null())
        .output()
        .unwrap();
    let all = format!(
        "{}{}",
        String::from_utf8_lossy(&out.stdout),
        String::from_utf8_lossy(&out.stderr)
    );
    assert!(
        !all.contains("FIXTURE"),
        "a token reached the output: {all}"
    );
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
        let v: Value = serde_json::from_str(line).unwrap_or_else(|_| panic!("{text}"));
        (false, v["error"].clone())
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn claude_limits_and_reset_credit_through_the_cli() {
    let tmp = tempfile::tempdir().unwrap();
    let install = |name: &str, body: &str| {
        let path = tmp.path().join(name);
        fs::write(&path, body).unwrap();
        fs::set_permissions(&path, fs::Permissions::from_mode(0o700)).unwrap();
        path
    };
    let claude = install("fake-claude", include_str!("support/fake-claude.sh"));
    let codex = install("fake-codex", include_str!("support/fake-codex.sh"));
    let fake = tmp.path().join("curl-data");
    fs::create_dir(&fake).unwrap();
    let curl = install(
        "fake-curl",
        &format!(
            r#"#!/bin/sh
d='{}'
printf '%s\n' "$*" >> "$d/argv"
req="$d/req-$$"
cat > "$req"
case "$(sed -n 's/^url = "\(.*\)"$/\1/p' "$req")" in
  *reset_rate_limits*) k=claim ;;
  *oauth/usage*) k=usage ;;
  *) k=other ;;
esac
echo "$k" >> "$d/calls"
printf '%s\n%s' "$(cat "$d/$k-body" 2>/dev/null)" "$(cat "$d/$k-status" 2>/dev/null || echo 500)"
"#,
            fake.display()
        ),
    );
    let paths = Paths::new(Some(tmp.path().join("state"))).unwrap();
    sessionsation::store::save_config(
        &paths,
        &sessionsation::config::Config {
            codex_binary: codex.to_str().unwrap().into(),
            claude_binary: claude.to_str().unwrap().into(),
            curl_binary: curl.to_str().unwrap().into(),
            ..Default::default()
        },
    )
    .unwrap();
    let _daemon = Process(
        Command::new(binary())
            .arg("--data-dir")
            .arg(&paths.root)
            .args(["daemon", "serve"])
            // Contract 1.62.0: no background meter read between the counted requests.
            .env("SESSIONSATION_CLAUDE_METER_SECONDS", "0")
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .spawn()
            .unwrap(),
    );
    let deadline = Instant::now() + Duration::from_secs(10);
    while daemon::call(&paths, "status", json!({})).await.is_err() {
        assert!(Instant::now() < deadline, "supervisor did not start");
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
    let root = paths.root.clone();
    let (ok, e) = api(&root, &["account", "add", "claude:personal"]);
    assert!(ok, "{e}");
    let home = root.join("accounts/claude/personal");
    let creds = json!({"claudeAiOauth":{"accessToken":ACCESS,"refreshToken":"sk-ant-ort01-FIXTURE-CLI",
        "expiresAt":(sessionsation::daemon::now() + 3600) * 1000,"scopes":["user:inference","user:profile"]}});
    fs::write(home.join(".credentials.json"), creds.to_string()).unwrap();
    let mut cfg: Value =
        serde_json::from_slice(&fs::read(home.join(".claude.json")).unwrap_or(b"{}".to_vec()))
            .unwrap_or(json!({}));
    cfg["oauthAccount"] = json!({"organizationUuid":ORG});
    fs::write(home.join(".claude.json"), cfg.to_string()).unwrap();

    // Capability names Claude.
    let (_, caps) = api(&root, &["capabilities"]);
    assert_eq!(
        caps["account_reset_credit"]["providers"],
        json!(["codex", "claude"])
    );
    assert_eq!(caps["contract_version"], "1.73.0");

    // Limits: an on-demand status read.
    fs::write(fake.join("usage-status"), "200").unwrap();
    fs::write(
        fake.join("usage-body"),
        json!({"cedar_ember":{"eligible":true,"grants":[{"id":"g5h","resets_left":2,"usable_now":true,
            "type":"session","ends_at":"2036-12-01T00:00:00Z"}],"next_grant_id":"g5h"}})
        .to_string(),
    )
    .unwrap();
    let (ok, v) = api(&root, &["account", "limits", "claude:personal"]);
    assert!(ok, "{v}");
    assert_eq!(v["reset_credits_read"]["ok"], true);
    assert_eq!(v["usage"]["reset_credits"]["available"], 2);
    assert_eq!(v["usage"]["reset_credits"]["credits"][0]["kind"], "session");
    // The account list shows the stored read without another request.
    let (ok, list) = api(&root, &["account", "list"]);
    assert!(ok);
    let row = list
        .as_array()
        .unwrap()
        .iter()
        .find(|a| a["provider"] == "claude")
        .unwrap()
        .clone();
    assert_eq!(row["usage"]["reset_credits"]["available"], 2, "{row}");

    // Redeem with --kind.
    fs::write(fake.join("claim-status"), "200").unwrap();
    fs::write(
        fake.join("claim-body"),
        r#"{"result":"reset","resets_left":1}"#,
    )
    .unwrap();
    let id = uuid::Uuid::new_v4().to_string();
    let (ok, v) = api(
        &root,
        &[
            "account",
            "reset-credit",
            "claude:personal",
            "--kind",
            "5-hour",
            "--request-id",
            &id,
        ],
    );
    assert!(ok, "{v}");
    assert_eq!(v["outcome"], "reset");
    assert_eq!(v["credit_id"], "g5h");
    assert_eq!(v["request_id"], id);
    let (_, ops) = api(&root, &["operations"]);
    assert!(ops.to_string().contains(&id), "{ops}");
    // --kind is Claude-only; an unknown kind is refused.
    let (ok, e) = api(
        &root,
        &[
            "account",
            "reset-credit",
            "claude:personal",
            "--kind",
            "monthly",
        ],
    );
    assert!(!ok);
    assert_eq!(e["code"], "INVALID_ARGUMENT");

    // An expired token with Claude Code's refresh lock held: AUTH_STALE, retryable.
    let mut expired = creds.clone();
    expired["claudeAiOauth"]["expiresAt"] = json!(1000);
    fs::write(home.join(".credentials.json"), expired.to_string()).unwrap();
    fs::create_dir(home.join(".oauth_refresh.lock")).unwrap();
    let claims = fs::read_to_string(fake.join("calls")).unwrap();
    let (ok, e) = api(
        &root,
        &[
            "account",
            "reset-credit",
            "claude:personal",
            "--credit-id",
            "g5h",
        ],
    );
    assert!(!ok);
    assert_eq!(e["code"], "AUTH_STALE", "{e}");
    assert_eq!(e["retryable"], true);
    assert!(
        e["message"]
            .as_str()
            .unwrap()
            .contains("A running Claude session on the account refreshes it")
    );
    assert_eq!(
        fs::read_to_string(fake.join("calls")).unwrap(),
        claims,
        "nothing sent"
    );
    for line in fs::read_to_string(fake.join("argv")).unwrap().lines() {
        assert_eq!(line, "-q -K -");
    }
}
