//! Contract 1.62.0 (SM8): one usage meter per account, readable with no session running.
//! Codex from the supervisor's app-server rate-limit read; Claude from the OAuth usage
//! endpoint through the 1.60.0 owner (fake curl) and from the statusline recorder, the
//! fresher reading winning without ever lowering a reset cycle. Fixture credentials only.
#![allow(
    clippy::disallowed_methods,
    reason = "integration tests are exempt from owned roles (src/guard.rs)"
)]
use serde_json::{Value, json};
use sessionsation::{daemon, paths::Paths};
use std::{
    fs,
    io::Write,
    os::unix::fs::PermissionsExt,
    path::{Path, PathBuf},
    process::{Child, Command, Stdio},
    time::{Duration, Instant},
};

const ACCESS: &str = "sk-ant-oat01-FIXTURE-METER-ACCESS";

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
fn ok(root: &Path, args: &[&str]) -> Value {
    let (ok, v) = api(root, args);
    assert!(ok, "{args:?}: {v}");
    v
}
/// RFC 3339 of Unix seconds, with a fraction (as the endpoint answers).
fn iso(t: i64) -> String {
    let (days, secs) = (t.div_euclid(86_400), t.rem_euclid(86_400));
    let z = days + 719_468;
    let era = z.div_euclid(146_097);
    let doe = z - era * 146_097;
    let yoe = (doe - doe / 1460 + doe / 36_524 - doe / 146_096) / 365;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let d = doy - (153 * mp + 2) / 5 + 1;
    let m = if mp < 10 { mp + 3 } else { mp - 9 };
    let y = yoe + era * 400 + i64::from(m <= 2);
    format!(
        "{y:04}-{m:02}-{d:02}T{:02}:{:02}:{:02}.271828+00:00",
        secs / 3600,
        secs % 3600 / 60,
        secs % 60
    )
}

struct Fixture {
    _tmp: tempfile::TempDir,
    paths: Paths,
    fake: PathBuf,
    _daemon: Process,
}
impl Fixture {
    async fn new(meter_seconds: &str) -> Self {
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
url=$(sed -n 's/^url = "\(.*\)"$/\1/p' "$req")
printf '%s\n' "$url" >> "$d/urls"
case "$url" in
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
        let daemon = Process(
            Command::new(binary())
                .arg("--data-dir")
                .arg(&paths.root)
                .args(["daemon", "serve"])
                .env("SESSIONSATION_CLAUDE_METER_SECONDS", meter_seconds)
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
        Self {
            _tmp: tmp,
            paths,
            fake,
            _daemon: daemon,
        }
    }
    fn root(&self) -> &Path {
        &self.paths.root
    }
    /// A Claude account with a fixture login and no session at all.
    fn add_claude(&self, name: &str, expires_at_ms: i64) -> PathBuf {
        ok(self.root(), &["account", "add", &format!("claude:{name}")]);
        let home = self.root().join(format!("accounts/claude/{name}"));
        self.login(&home, expires_at_ms);
        home
    }
    fn login(&self, home: &Path, expires_at_ms: i64) {
        let creds = json!({"claudeAiOauth":{"accessToken":ACCESS,"refreshToken":"sk-ant-ort01-FIXTURE-METER",
            "expiresAt":expires_at_ms,"scopes":["user:inference","user:profile"]}});
        fs::write(home.join(".credentials.json"), creds.to_string()).unwrap();
    }
    fn answer(&self, five: f64, week: f64, r5: i64, r7: i64) {
        fs::write(self.fake.join("usage-status"), "200").unwrap();
        fs::write(
            self.fake.join("usage-body"),
            json!({"five_hour":{"utilization":five,"resets_at":iso(r5)},
                "seven_day":{"utilization":week,"resets_at":iso(r7)},
                "seven_day_opus":null,"seven_day_sonnet":{"utilization":3.0,"resets_at":iso(r7)},
                "extra_usage":{"is_enabled":false,"monthly_limit":null,"used_credits":null,"utilization":null}})
            .to_string(),
        )
        .unwrap();
    }
    fn calls(&self) -> usize {
        fs::read_to_string(self.fake.join("calls"))
            .unwrap_or_default()
            .lines()
            .count()
    }
    fn statusline(&self, home: &Path, five: f64, week: f64, r5: i64, r7: i64) {
        let mut child = Command::new(binary())
            .args(["provider-hook", "claude-statusline", "--record-only"])
            .env("CLAUDE_CONFIG_DIR", home)
            .stdin(Stdio::piped())
            .stdout(Stdio::null())
            .spawn()
            .unwrap();
        let payload = json!({"session_id":"5d6e7f80-1111-4222-8333-944455556666","version":"2.1.314",
            "model":{"id":"claude-fable-5-2"},
            "rate_limits":{"five_hour":{"used_percentage":five,"resets_at":r5},
                "seven_day":{"used_percentage":week,"resets_at":r7}}});
        child
            .stdin
            .take()
            .unwrap()
            .write_all(payload.to_string().as_bytes())
            .unwrap();
        assert!(child.wait().unwrap().success());
    }
}
fn row<'a>(usage: &'a Value, id: &str) -> &'a Value {
    usage["accounts"]
        .as_array()
        .unwrap()
        .iter()
        .find(|r| r["account_id"] == id)
        .unwrap_or_else(|| panic!("no {id} in {usage}"))
}
fn keys(v: &Value) -> Vec<String> {
    let mut k: Vec<String> = v.as_object().unwrap().keys().cloned().collect();
    k.sort();
    k
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn idle_codex_and_claude_accounts_have_one_meter_shape_without_a_session() {
    let f = Fixture::new("0").await;
    let root = f.root().to_path_buf();
    // Codex: the supervisor reads every enabled account over its app-server.
    daemon::call(&f.paths, "account/add", json!({"name":"first"}))
        .await
        .unwrap();
    let deadline = Instant::now() + Duration::from_secs(10);
    loop {
        let v = daemon::call(&f.paths, "status", json!({})).await.unwrap();
        if v["accounts"][0]["usage_fresh"] == true {
            break;
        }
        assert!(Instant::now() < deadline, "no Codex usage read");
        tokio::time::sleep(Duration::from_millis(30)).await;
    }
    let usage = ok(&root, &["usage", "--account", "codex:first"]);
    let codex = row(&usage, "codex:first");
    let meter = &codex["meter"];
    assert_eq!(codex["supported"], true);
    assert_eq!(codex["requires_session"], false);
    assert_eq!(meter["source"], "app_server");
    assert_eq!(meter["fresh"], true);
    assert_eq!(meter["windows"]["five_hour"]["used_percent"], 23.0);
    assert_eq!(meter["windows"]["seven_day"]["used_percent"], 42.0);
    assert!(meter["observed_at"].as_i64().is_some());
    assert!(codex["read"].is_null(), "Codex needs no on-demand read");

    // Claude: no session ever ran; the OAuth usage read fills the meter.
    let now = daemon::now();
    let (r5, r7) = (now + 3_600, now + 5 * 86_400);
    let home = f.add_claude("personal", (now + 3_600) * 1000);
    f.answer(31.0, 55.0, r5, r7);
    assert_eq!(f.calls(), 0);
    let usage = ok(&root, &["usage", "--account", "claude:personal"]);
    let claude = row(&usage, "claude:personal");
    assert_eq!(claude["read"]["ok"], true, "{claude}");
    assert_eq!(claude["read"]["attempted"], true);
    assert_eq!(claude["supported"], true);
    assert_eq!(claude["requires_session"], false);
    let meter = &claude["meter"];
    assert_eq!(meter["source"], "oauth");
    assert_eq!(meter["fresh"], true);
    assert_eq!(meter["windows"]["five_hour"]["used_percent"], 31.0);
    assert_eq!(meter["windows"]["five_hour"]["resets_at"], r5);
    assert_eq!(meter["windows"]["five_hour"]["source"], "oauth");
    assert_eq!(meter["windows"]["seven_day"]["used_percent"], 55.0);
    assert_eq!(claude["short"]["used_percent"], 31.0, "legacy keys agree");
    assert_eq!(claude["state"], "fresh");
    assert_eq!(f.calls(), 1);
    // Claude Code's usage read with its cedar_ember status, the bearer only on curl's stdin.
    assert_eq!(
        fs::read_to_string(f.fake.join("urls")).unwrap().trim(),
        "https://api.anthropic.com/api/oauth/usage?cedar_ember=1&skip_spend=1",
        "contract 1.65.0: one read refreshes the meter and the reset credits"
    );
    for line in fs::read_to_string(f.fake.join("argv")).unwrap().lines() {
        assert_eq!(line, "-q -K -");
    }
    // A second read within the cache sends nothing.
    let again = ok(&root, &["usage", "--account", "claude:personal"]);
    assert!(row(&again, "claude:personal")["read"].is_null());
    assert_eq!(f.calls(), 1);

    // The same meter is on the account rows (list and limits).
    let list = ok(&root, &["account", "list"]);
    for a in list.as_array().unwrap() {
        assert_eq!(a["usage"]["meter"]["requires_session"], false, "{a}");
    }
    let limits = ok(&root, &["account", "limits", "claude:personal"]);
    assert_eq!(limits["usage"]["meter"]["source"], "oauth", "{limits}");
    assert_eq!(
        limits["usage"]["meter"]["windows"]["seven_day"]["used_percent"],
        55.0
    );

    // Every provider has a row of the same shape; Gemini says why it has no meter.
    ok(&root, &["account", "add", "antigravity:g"]);
    let sent = f.calls();
    let all = ok(&root, &["usage", "--cached"]);
    let gemini = row(&all, "antigravity:g");
    assert_eq!(gemini["supported"], false);
    assert!(gemini["meter"]["reason"].is_string());
    let shape = keys(row(&all, "codex:first"));
    for r in all["accounts"].as_array().unwrap() {
        assert_eq!(keys(r), shape, "{r}");
        assert_eq!(keys(&r["meter"]), keys(&row(&all, "codex:first")["meter"]));
    }
    assert_eq!(f.calls(), sent, "--cached never contacts a provider");

    // Statusline and OAuth readings meet in one owner: a lower statusline replay of the
    // same cycle never lowers the meter, a higher one wins.
    f.statusline(&home, 20.0, 50.0, r5, r7);
    let v = ok(
        &root,
        &["usage", "--account", "claude:personal", "--cached"],
    );
    let m = &row(&v, "claude:personal")["meter"];
    assert_eq!(m["windows"]["five_hour"]["used_percent"], 31.0);
    assert_eq!(m["windows"]["five_hour"]["source"], "oauth");
    assert_eq!(m["windows"]["seven_day"]["used_percent"], 55.0);
    f.statusline(&home, 33.5, 50.0, r5, r7);
    let v = ok(
        &root,
        &["usage", "--account", "claude:personal", "--cached"],
    );
    let m = &row(&v, "claude:personal")["meter"];
    assert_eq!(m["windows"]["five_hour"]["used_percent"], 33.5);
    assert_eq!(m["windows"]["five_hour"]["source"], "statusline");
    assert_eq!(m["windows"]["seven_day"]["used_percent"], 55.0);
    assert_eq!(m["windows"]["seven_day"]["source"], "oauth");
    // `source` names the newest window's source (both were learned this second here).
    assert!(m["source"] == "statusline" || m["source"] == "oauth", "{m}");
    // An unknown account is refused; nothing is invented.
    let (okay, e) = api(&root, &["usage", "--account", "claude:nobody"]);
    assert!(!okay);
    assert!(e["code"].as_str().unwrap().starts_with("ACCOUNT_"), "{e}");
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn the_supervisor_reads_idle_claude_meters_and_never_renews_a_token() {
    let f = Fixture::new("1").await;
    let root = f.root().to_path_buf();
    let now = daemon::now();
    let (r5, r7) = (now + 3_600, now + 5 * 86_400);
    f.answer(12.0, 64.0, r5, r7);
    let home = f.add_claude("idle", (now + 3_600) * 1000);
    // Nobody asks: the supervisor reads the idle account by itself.
    let deadline = Instant::now() + Duration::from_secs(20);
    while f.calls() == 0 {
        assert!(
            Instant::now() < deadline,
            "the supervisor never read the meter"
        );
        tokio::time::sleep(Duration::from_millis(50)).await;
    }
    let deadline = Instant::now() + Duration::from_secs(10);
    let meter = loop {
        let v = ok(&root, &["usage", "--account", "claude:idle", "--cached"]);
        let m = row(&v, "claude:idle")["meter"].clone();
        if m["source"] == "oauth" {
            break m;
        }
        assert!(Instant::now() < deadline, "{v}");
        tokio::time::sleep(Duration::from_millis(50)).await;
    };
    assert_eq!(meter["windows"]["seven_day"]["used_percent"], 64.0);
    assert_eq!(meter["requires_session"], false);

    // A token due for renewal is left alone: no request, no token endpoint, no write.
    f.login(&home, 1000);
    let before = fs::read(home.join(".credentials.json")).unwrap();
    // Let the current period pass, then give the supervisor several ticks.
    tokio::time::sleep(Duration::from_millis(1500)).await;
    let calls = f.calls();
    tokio::time::sleep(Duration::from_millis(2500)).await;
    assert_eq!(f.calls(), calls, "a background read never renews");
    assert_eq!(fs::read(home.join(".credentials.json")).unwrap(), before);
    let attempt: Value =
        serde_json::from_slice(&fs::read(home.join(".sessionsation/usage-read.json")).unwrap())
            .unwrap();
    assert_eq!(attempt["ok"], false, "{attempt}");
    assert_eq!(attempt["code"], "AUTH_STALE");
    assert!(!attempt.to_string().contains("FIXTURE"));
    // The last reading stays (its age says how old it is).
    let v = ok(&root, &["usage", "--account", "claude:idle", "--cached"]);
    assert_eq!(
        row(&v, "claude:idle")["meter"]["windows"]["seven_day"]["used_percent"],
        64.0
    );
}
