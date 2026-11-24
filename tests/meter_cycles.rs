//! Contract 1.65.0: every usage window names its reset cycle (`cycle`, `cycle_started_at`,
//! `cycle_reason`), the marker changes on any reset (a new `resets_at`, a redeem through
//! `api account reset-credit`, a reset made elsewhere), within one cycle usage never goes down,
//! and the supervisor refreshes `usage.reset_credits` with the meter. Fake curl and fake Codex
//! app-server only; disposable homes, fixture credentials, no network.
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

const ACCESS: &str = "sk-ant-oat01-FIXTURE-CYCLE-ACCESS";
const ORG: &str = "6f1f3d5e-8a52-4b7e-9c1d-2e3f4a5b6c7d";
const WEEK: i64 = 7 * 86_400;
const FIVE: i64 = 5 * 3_600;

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
/// A usage answer with its `cedar_ember` block; `five: None` is a cleared 5-hour window.
fn body(five: Option<(f64, i64)>, week: (f64, i64), grants: Value) -> Value {
    let five = match five {
        Some((u, r)) => json!({"utilization":u,"resets_at":iso(r)}),
        None => json!({"utilization":null,"resets_at":null}),
    };
    json!({"five_hour":five,"seven_day":{"utilization":week.0,"resets_at":iso(week.1)},
        "seven_day_opus":null,"extra_usage":{"is_enabled":false},
        "cedar_ember":{"eligible":true,"grants":grants}})
}
/// Claude Code 2.1.314's launch grant: clears the 5-hour window and gives back weekly usage.
fn fable(left: u64) -> Value {
    json!({"id":"launch-fable","resets_left":left,"resets_total":1,"usable_now":left > 0,
        "clears":["five_hour","seven_day_overage_included"],"gives_back_weekly":true})
}
fn full(left: u64) -> Value {
    json!({"id":"full-reset","resets_left":left,"resets_total":1,"usable_now":left > 0,"type":"full",
        "clears":["five_hour","seven_day","seven_day_overage_included"]})
}
fn row<'a>(v: &'a Value, id: &str) -> &'a Value {
    v["accounts"]
        .as_array()
        .unwrap()
        .iter()
        .find(|r| r["account_id"] == id)
        .unwrap_or_else(|| panic!("no {id} in {v}"))
}
async fn until<F: FnMut() -> Option<Value>>(what: &str, mut f: F) -> Value {
    let deadline = Instant::now() + Duration::from_secs(30);
    loop {
        if let Some(v) = f() {
            return v;
        }
        assert!(Instant::now() < deadline, "timed out waiting for {what}");
        tokio::time::sleep(Duration::from_millis(50)).await;
    }
}

struct Claude {
    _tmp: tempfile::TempDir,
    paths: Paths,
    fake: PathBuf,
    _daemon: Process,
}
impl Claude {
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
        // After a claim the usage endpoint answers `usage-after-body` (when present).
        let curl = install(
            "fake-curl",
            &format!(
                r#"#!/bin/sh
d='{}'
req="$d/req-$$"
cat > "$req"
url=$(sed -n 's/^url = "\(.*\)"$/\1/p' "$req")
printf '%s\n' "$url" >> "$d/urls"
case "$url" in
  *reset_rate_limits*) k=claim; : > "$d/claimed" ;;
  *oauth/usage*) k=usage; if [ -f "$d/claimed" ] && [ -f "$d/usage-after-body" ]; then k=usage-after; fi ;;
  *) k=other ;;
esac
echo "$k" >> "$d/calls"
printf '%s\n%s' "$(cat "$d/$k-body" 2>/dev/null)" "$(cat "$d/$k-status" 2>/dev/null || echo 200)"
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
                .env("SESSIONSATION_TEST_TIME_SCALE", "20")
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
    /// A Claude account with a fixture login (and organization) and no session.
    fn add(&self, name: &str) -> PathBuf {
        ok(self.root(), &["account", "add", &format!("claude:{name}")]);
        let home = self.root().join(format!("accounts/claude/{name}"));
        let creds = json!({"claudeAiOauth":{"accessToken":ACCESS,"refreshToken":"sk-ant-ort01-FIXTURE-CYCLE",
            "expiresAt":(daemon::now() + 3600) * 1000,"scopes":["user:inference","user:profile"]}});
        fs::write(home.join(".credentials.json"), creds.to_string()).unwrap();
        let mut cfg: Value = fs::read(home.join(".claude.json"))
            .ok()
            .and_then(|b| serde_json::from_slice(&b).ok())
            .unwrap_or(json!({}));
        cfg["oauthAccount"] = json!({"organizationUuid":ORG});
        fs::write(home.join(".claude.json"), cfg.to_string()).unwrap();
        home
    }
    fn answer(&self, body: &Value) {
        fs::write(self.fake.join("usage-body"), body.to_string()).unwrap();
    }
    fn answer_after_claim(&self, claim: &Value, body: &Value) {
        fs::write(self.fake.join("claim-body"), claim.to_string()).unwrap();
        fs::write(self.fake.join("usage-after-body"), body.to_string()).unwrap();
    }
    fn statusline(&self, home: &Path, five: (f64, i64), week: (f64, i64)) {
        let mut child = Command::new(binary())
            .args(["provider-hook", "claude-statusline", "--record-only"])
            .env("CLAUDE_CONFIG_DIR", home)
            .stdin(Stdio::piped())
            .stdout(Stdio::null())
            .spawn()
            .unwrap();
        let payload = json!({"session_id":"5d6e7f80-1111-4222-8333-944455556666","version":"2.1.314",
            "rate_limits":{"five_hour":{"used_percentage":five.0,"resets_at":five.1},
                "seven_day":{"used_percentage":week.0,"resets_at":week.1}}});
        child
            .stdin
            .take()
            .unwrap()
            .write_all(payload.to_string().as_bytes())
            .unwrap();
        assert!(child.wait().unwrap().success());
    }
    /// The cached meter of an account (never contacts the provider).
    fn meter(&self, id: &str) -> Value {
        let v = ok(self.root(), &["usage", "--account", id, "--cached"]);
        row(&v, id)["meter"].clone()
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_claude_redeem_starts_a_new_cycle_and_the_lower_reading_is_accepted() {
    let f = Claude::new("0").await;
    let root = f.root().to_path_buf();
    let home = f.add("personal");
    let now = daemon::now();
    let (r5, r7) = (now + 3_600, now + 5 * 86_400);
    f.answer(&body(Some((40.0, r5)), (20.0, r7), json!([fable(1)])));
    let limits = ok(&root, &["account", "limits", "claude:personal"]);
    let week = &limits["usage"]["meter"]["windows"]["seven_day"];
    assert_eq!(week["used_percent"], 20.0, "{limits}");
    let natural = week["cycle"].as_str().expect("a cycle id").to_owned();
    assert_eq!(week["cycle_reason"], "natural");
    assert_eq!(week["cycle_started_at"], r7 - WEEK);
    // The legacy usage window carries the same marker.
    assert_eq!(limits["usage"]["weekly"]["cycle"], natural.as_str());
    assert_eq!(limits["usage"]["weekly"]["cycle_started_at"], r7 - WEEK);

    // Without a redeem, a lower reading of the same cycle is kept out (as since 1.56.0).
    f.statusline(&home, (40.0, r5), (10.0, r7));
    let m = f.meter("claude:personal");
    assert_eq!(m["windows"]["seven_day"]["used_percent"], 20.0, "{m}");
    assert_eq!(m["windows"]["seven_day"]["cycle"], natural.as_str());

    // The redeem: Claude clears the 5-hour window and gives back weekly usage, with the same
    // weekly resets_at (the 11-24 incident's shape).
    f.answer_after_claim(
        &json!({"result":"reset","resets_left":0,"cleared":["five_hour","seven_day_overage_included"]}),
        &body(None, (10.0, r7), json!([fable(0)])),
    );
    let id = uuid::Uuid::new_v4().to_string();
    let before = daemon::now();
    let v = ok(
        &root,
        &[
            "account",
            "reset-credit",
            "claude:personal",
            "--kind",
            "fable",
            "--request-id",
            &id,
        ],
    );
    assert_eq!(v["outcome"], "reset", "{v}");
    assert_eq!(v["cycles_started"], json!(["five_hour", "seven_day"]));
    let after = &v["rate_limits_after"]["weekly"];
    assert_eq!(
        after["used_percent"], 10.0,
        "the lower reading is accepted: {v}"
    );
    assert_eq!(after["resets_at"], r7, "same resets_at");
    assert_eq!(after["cycle_reason"], "redeem");
    let redeemed = after["cycle"].as_str().unwrap().to_owned();
    assert_ne!(redeemed, natural, "a redeem is a new cycle");
    assert!(after["cycle_started_at"].as_i64().unwrap() >= before);
    // The cleared 5-hour window has not started again: 0% until first use.
    let short = &v["rate_limits_after"]["short"];
    assert_eq!(short["starts_on_first_use"], true, "{v}");
    assert_eq!(short["used_percent"], 0.0);
    assert!(short["resets_at"].is_null());
    let m = f.meter("claude:personal");
    assert_eq!(m["windows"]["seven_day"]["used_percent"], 10.0);
    assert_eq!(m["windows"]["seven_day"]["cycle"], redeemed.as_str());
    assert_eq!(m["windows"]["seven_day"]["source"], "oauth");

    // An idle session re-rendering the pre-redeem response is kept out; within the new
    // cycle the rule holds again, and real growth counts.
    f.statusline(&home, (40.0, r5), (20.0, r7));
    let m = f.meter("claude:personal");
    assert_eq!(m["windows"]["seven_day"]["used_percent"], 10.0, "{m}");
    assert_eq!(m["windows"]["five_hour"]["used_percent"], 0.0, "{m}");
    assert_eq!(m["windows"]["five_hour"]["starts_on_first_use"], true);
    f.statusline(&home, (2.0, now + 60 + FIVE), (11.0, r7));
    let m = f.meter("claude:personal");
    assert_eq!(m["windows"]["seven_day"]["used_percent"], 11.0);
    assert_eq!(m["windows"]["seven_day"]["cycle"], redeemed.as_str());
    assert_eq!(m["windows"]["five_hour"]["used_percent"], 2.0);
    assert_eq!(m["windows"]["five_hour"]["cycle_reason"], "redeem");
    f.statusline(&home, (2.0, now + 60 + FIVE), (9.0, r7));
    let m = f.meter("claude:personal");
    assert_eq!(m["windows"]["seven_day"]["used_percent"], 11.0);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_stale_lower_reading_in_the_same_cycle_is_still_kept_out() {
    let f = Claude::new("0").await;
    let home = f.add("personal");
    let now = daemon::now();
    let (r5, r7) = (now + 3_600, now + 5 * 86_400);
    f.statusline(&home, (30.0, r5), (60.0, r7));
    let m = f.meter("claude:personal");
    let cycle = m["windows"]["seven_day"]["cycle"]
        .as_str()
        .expect("a cycle id")
        .to_owned();
    f.statusline(&home, (25.0, r5), (50.0, r7));
    let m = f.meter("claude:personal");
    assert_eq!(m["windows"]["seven_day"]["used_percent"], 60.0);
    assert_eq!(m["windows"]["five_hour"]["used_percent"], 30.0);
    assert_eq!(m["windows"]["seven_day"]["cycle"], cycle.as_str());
    // A Claude session started before the upgrade keeps running its release's statusline
    // recorder; one older than 1.56.0 rewrites usage.json with whatever it was given. That is
    // one more reading, under the same rule.
    let at = daemon::now();
    fs::write(
        home.join(".sessionsation/usage.json"),
        json!({"source":"claude-code-statusline","observed_at":at,"claude_version":"2.1.314",
            "session_id":"5d6e7f80-1111-4222-8333-944455556666",
            "five_hour":{"used_percentage":25.0,"resets_at":r5,"observed_at":at},
            "seven_day":{"used_percentage":50.0,"resets_at":r7,"observed_at":at}})
        .to_string(),
    )
    .unwrap();
    let m = f.meter("claude:personal");
    assert_eq!(m["windows"]["seven_day"]["used_percent"], 60.0, "{m}");
    assert_eq!(m["windows"]["five_hour"]["used_percent"], 30.0);
    assert_eq!(m["windows"]["seven_day"]["cycle"], cycle.as_str());
    // The next write repairs the record, cycle included.
    f.statusline(&home, (31.0, r5), (60.0, r7));
    let stored: Value =
        serde_json::from_slice(&fs::read(home.join(".sessionsation/usage.json")).unwrap()).unwrap();
    assert_eq!(stored["seven_day"]["used_percentage"], 60.0);
    assert_eq!(stored["seven_day"]["cycle"], cycle.as_str());
    assert_eq!(stored["five_hour"]["used_percentage"], 31.0);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_claude_redeem_made_elsewhere_is_seen_as_resets_left_falling() {
    let f = Claude::new("1").await;
    let now = daemon::now();
    let (r5, r7) = (now + 3_600, now + 5 * 86_400);
    f.answer(&body(Some((50.0, r5)), (70.0, r7), json!([full(1)])));
    f.add("personal");
    // The supervisor reads the idle meter and its reset credits by itself.
    let first = until("the first background read", || {
        let m = f.meter("claude:personal");
        (m["windows"]["seven_day"]["used_percent"] == 70.0).then_some(m)
    })
    .await;
    let natural = first["windows"]["seven_day"]["cycle"]
        .as_str()
        .unwrap()
        .to_owned();
    // Claude Code's own /limit-reset on this account: the grant is spent, both windows
    // clear, their resets_at stay.
    f.answer(&body(Some((2.0, r5)), (5.0, r7), json!([full(0)])));
    let m = until("the external reset", || {
        let m = f.meter("claude:personal");
        (m["windows"]["seven_day"]["used_percent"] == 5.0).then_some(m)
    })
    .await;
    let week = &m["windows"]["seven_day"];
    assert_eq!(week["cycle_reason"], "external_redeem", "{m}");
    assert_ne!(week["cycle"], natural.as_str());
    assert_eq!(week["resets_at"], r7);
    assert_eq!(m["windows"]["five_hour"]["used_percent"], 2.0);
    assert_eq!(m["windows"]["five_hour"]["cycle_reason"], "external_redeem");
    // The new cycle holds the rule again.
    let list = ok(f.root(), &["account", "list"]);
    let claude = list
        .as_array()
        .unwrap()
        .iter()
        .find(|a| a["provider"] == "claude")
        .unwrap();
    assert_eq!(claude["usage"]["reset_credits"]["available"], 0);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_natural_claude_reset_gives_a_new_cycle() {
    let f = Claude::new("0").await;
    let home = f.add("personal");
    let now = daemon::now();
    let (r5, r7) = (now + 3_600, now + 5 * 86_400);
    f.statusline(&home, (10.0, r5), (30.0, r7));
    let m = f.meter("claude:personal");
    let (five, week) = (
        m["windows"]["five_hour"]["cycle"].clone(),
        m["windows"]["seven_day"]["cycle"].clone(),
    );
    assert!(five.is_string() && week.is_string(), "{m}");
    assert_eq!(m["windows"]["five_hour"]["cycle_started_at"], r5 - FIVE);
    // Header seconds versus an ISO time a few seconds apart are the same cycle.
    f.statusline(&home, (11.0, r5 + 2), (30.0, r7));
    assert_eq!(
        f.meter("claude:personal")["windows"]["five_hour"]["cycle"],
        five
    );
    f.statusline(&home, (1.0, r5 + FIVE), (31.0, r7));
    let m = f.meter("claude:personal");
    let w = &m["windows"]["five_hour"];
    assert_eq!(w["used_percent"], 1.0);
    assert_ne!(w["cycle"], five);
    assert_eq!(w["cycle_reason"], "natural");
    assert_eq!(w["cycle_started_at"], r5);
    assert_eq!(m["windows"]["seven_day"]["cycle"], week, "{m}");
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn the_monitor_refreshes_claude_reset_credits_with_fresh_and_age() {
    // The default period: a meter a running session keeps fresh is not re-read, but the
    // reset credits are refreshed with it.
    let f = Claude::new("600").await;
    let now = daemon::now();
    let (r5, r7) = (now + 3_600, now + 5 * 86_400);
    f.answer(&body(Some((12.0, r5)), (40.0, r7), json!([fable(1)])));
    let home = f.add("personal");
    f.statusline(&home, (12.0, r5), (40.0, r7));
    let credits = until("reset credits from the monitor", || {
        let list = ok(f.root(), &["account", "list"]);
        let c = list
            .as_array()
            .unwrap()
            .iter()
            .find(|a| a["provider"] == "claude")
            .unwrap()["usage"]["reset_credits"]
            .clone();
        (!c.is_null()).then_some(c)
    })
    .await;
    assert_eq!(credits["available"], 1, "{credits}");
    assert_eq!(credits["fresh"], true);
    assert!(credits["age_seconds"].as_i64().is_some_and(|a| a >= 0));
    // One request, with the cedar_ember status, through the stored token only.
    let urls = fs::read_to_string(f.fake.join("urls")).unwrap();
    assert!(
        urls.lines()
            .all(|u| u == "https://api.anthropic.com/api/oauth/usage?cedar_ember=1&skip_spend=1"),
        "{urls}"
    );
    // The same field is on every view of the account.
    let m = ok(
        f.root(),
        &["usage", "--account", "claude:personal", "--cached"],
    );
    assert_eq!(row(&m, "claude:personal")["meter"]["fresh"], true);
}

// ------------------------------------------------------------------- Codex

struct Codex {
    _tmp: tempfile::TempDir,
    paths: Paths,
    _daemon: Process,
}
impl Codex {
    async fn new(idle_poll: u64) -> Self {
        let tmp = tempfile::tempdir().unwrap();
        let codex = tmp.path().join("fake-codex");
        fs::write(&codex, include_str!("support/fake-codex.sh")).unwrap();
        fs::set_permissions(&codex, fs::Permissions::from_mode(0o700)).unwrap();
        let paths = Paths::new(Some(tmp.path().join("state"))).unwrap();
        sessionsation::store::save_config(
            &paths,
            &sessionsation::config::Config {
                codex_binary: codex.to_str().unwrap().into(),
                idle_poll_seconds: idle_poll,
                active_poll_seconds: idle_poll.min(30),
                near_limit_poll_seconds: idle_poll.min(10),
                stale_seconds: idle_poll + 90,
                ..Default::default()
            },
        )
        .unwrap();
        let daemon = Process(
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
        let deadline = Instant::now() + Duration::from_secs(10);
        while daemon::call(&paths, "status", json!({})).await.is_err() {
            assert!(Instant::now() < deadline, "supervisor did not start");
            tokio::time::sleep(Duration::from_millis(20)).await;
        }
        daemon::call(&paths, "account/add", json!({"name":"first"}))
            .await
            .unwrap();
        Self {
            _tmp: tmp,
            paths,
            _daemon: daemon,
        }
    }
    fn home(&self) -> PathBuf {
        self.paths.root.join("accounts/codex/first")
    }
    async fn usage(&self) -> Value {
        let v = daemon::call(&self.paths, "status", json!({}))
            .await
            .unwrap();
        v["accounts"][0]["usage"].clone()
    }
    async fn until(&self, what: &str, f: impl Fn(&Value) -> bool) -> Value {
        let deadline = Instant::now() + Duration::from_secs(20);
        loop {
            let u = self.usage().await;
            if f(&u) {
                return u;
            }
            assert!(
                Instant::now() < deadline,
                "timed out waiting for {what}: {u}"
            );
            tokio::time::sleep(Duration::from_millis(50)).await;
        }
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_codex_redeem_starts_a_new_cycle() {
    // Default idle cadence: after the first poll only the redeem refreshes the usage.
    let f = Codex::new(120).await;
    let u = f
        .until("the first reading", |u| u["short"]["used_percent"] == 23.0)
        .await;
    let natural = u["short"]["cycle"].as_str().expect("a cycle id").to_owned();
    assert_eq!(u["short"]["cycle_reason"], "natural");
    assert_eq!(
        u["meter"]["windows"]["five_hour"]["cycle"],
        natural.as_str()
    );
    fs::write(f.home().join("fake-reset-outcome"), "reset").unwrap();
    fs::write(f.home().join("fake-reset-available"), "3").unwrap();
    let before = daemon::now();
    let r = ok(&f.paths.root, &["account", "reset-credit", "codex:first"]);
    assert_eq!(r["outcome"], "reset", "{r}");
    let after = &r["rate_limits_after"];
    assert_eq!(after["short"]["used_percent"], 0.0);
    assert_eq!(after["short"]["cycle_reason"], "redeem");
    assert_ne!(after["short"]["cycle"], natural.as_str());
    assert!(after["short"]["cycle_started_at"].as_i64().unwrap() >= before);
    assert_eq!(after["weekly"]["cycle_reason"], "redeem");
    let u = f.usage().await;
    assert_eq!(u["meter"]["windows"]["seven_day"]["cycle_reason"], "redeem");
    assert_eq!(
        u["meter"]["windows"]["five_hour"]["cycle"],
        after["short"]["cycle"]
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn codex_cycles_follow_natural_external_and_lowered_resets() {
    let f = Codex::new(1).await;
    fs::write(f.home().join("fake-reset-listed"), "").unwrap();
    fs::write(f.home().join("fake-reset-available"), "3").unwrap();
    let u = f
        .until("credits", |u| u["reset_credits"]["available"] == 3)
        .await;
    let (five, week) = (u["short"]["cycle"].clone(), u["weekly"]["cycle"].clone());
    assert!(five.is_string(), "{u}");
    // Contract 1.65.0: the reset credits carry fresh and age_seconds like the meter.
    assert_eq!(u["reset_credits"]["fresh"], true, "{u}");
    assert!(u["reset_credits"]["age_seconds"].as_i64().is_some());
    // A natural reset of the 5-hour window: a later resetsAt.
    fs::write(f.home().join("fake-reset-shift"), FIVE.to_string()).unwrap();
    let u = f
        .until("a natural reset", |u| u["short"]["cycle"] != five)
        .await;
    assert_eq!(u["short"]["cycle_reason"], "natural");
    assert_eq!(
        u["short"]["cycle_started_at"].as_i64().unwrap(),
        u["short"]["resets_at"].as_i64().unwrap() - FIVE
    );
    assert_eq!(
        u["weekly"]["cycle"], week,
        "the weekly window did not reset"
    );
    let five = u["short"]["cycle"].clone();
    // A credit spent by another Codex client: fewer available, no consume from here.
    fs::write(f.home().join("fake-reset-available"), "2").unwrap();
    let u = f
        .until("an external redeem", |u| {
            u["reset_credits"]["available"] == 2
        })
        .await;
    assert_eq!(u["short"]["cycle_reason"], "external_redeem", "{u}");
    assert_eq!(u["weekly"]["cycle_reason"], "external_redeem");
    assert_ne!(u["short"]["cycle"], five);
    let five = u["short"]["cycle"].clone();
    // Less usage within one resetsAt with no redeem seen: only a reset does that.
    fs::write(f.home().join("fake-short"), "5").unwrap();
    let u = f
        .until("a lowered reading", |u| u["short"]["used_percent"] == 5.0)
        .await;
    assert_eq!(u["short"]["cycle_reason"], "provider_lowered");
    assert_ne!(u["short"]["cycle"], five);
    assert_eq!(
        u["meter"]["windows"]["five_hour"]["cycle"],
        u["short"]["cycle"]
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn codex_credits_without_a_list_that_fall_are_not_an_external_redeem() {
    // Without the earlier credit rows an expiry cannot be told from a redeem elsewhere.
    let f = Codex::new(1).await;
    fs::write(f.home().join("fake-reset-available"), "3").unwrap();
    let u = f
        .until("credits", |u| u["reset_credits"]["available"] == 3)
        .await;
    let five = u["short"]["cycle"].clone();
    fs::write(f.home().join("fake-reset-available"), "2").unwrap();
    let u = f
        .until("fewer credits", |u| u["reset_credits"]["available"] == 2)
        .await;
    assert_eq!(u["short"]["cycle"], five, "{u}");
    assert_eq!(u["short"]["cycle_reason"], "natural");
}

/// A redeem through `api account reset-credit` whose post-claim read fails (`usage-after`
/// answers 500): the cycle stays open on the pre-redeem value. Returns (r5, r7, cycle id).
async fn open_redeem(f: &Claude, home: &Path) -> (i64, i64, String) {
    let root = f.root().to_path_buf();
    let now = daemon::now();
    let (r5, r7) = (now + 3_600, now + 5 * 86_400);
    f.answer(&body(Some((40.0, r5)), (20.0, r7), json!([fable(1)])));
    ok(&root, &["account", "limits", "claude:personal"]);
    f.statusline(home, (40.0, r5), (20.0, r7));
    f.answer_after_claim(
        &json!({"result":"reset","resets_left":0}),
        &body(Some((40.0, r5)), (10.0, r7), json!([fable(0)])),
    );
    fs::write(f.fake.join("usage-after-status"), "500").unwrap();
    let v = ok(
        &root,
        &[
            "account",
            "reset-credit",
            "claude:personal",
            "--kind",
            "fable",
        ],
    );
    assert_eq!(v["outcome"], "reset", "{v}");
    assert!(v["rate_limits_after_error"].is_string(), "{v}");
    let m = f.meter("claude:personal");
    let w = &m["windows"]["seven_day"];
    assert_eq!(w["used_percent"], 20.0, "{m}");
    assert_eq!(w["cycle_reason"], "redeem");
    (r5, r7, w["cycle"].as_str().unwrap().to_owned())
}
/// Let the post-claim usage answer succeed and wait for the monitor to read it.
async fn until_week(f: &Claude, used: f64) -> Value {
    until("the monitor's OAuth read", || {
        let m = f.meter("claude:personal");
        (m["windows"]["seven_day"]["used_percent"] == used).then_some(m)
    })
    .await
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_failed_read_after_a_redeem_then_a_replay_then_the_real_reading() {
    let f = Claude::new("1").await;
    let home = f.add("personal");
    let (r5, r7, cycle) = open_redeem(&f, &home).await;
    // An idle session re-renders its pre-redeem response: kept out, the cycle stays open.
    f.statusline(&home, (40.0, r5), (20.0, r7));
    let m = f.meter("claude:personal");
    assert_eq!(m["windows"]["seven_day"]["used_percent"], 20.0);
    // The real reading: the next OAuth answer (the supervisor's read) closes the cycle lower.
    fs::write(f.fake.join("usage-after-status"), "200").unwrap();
    let m = until_week(&f, 10.0).await;
    let w = &m["windows"]["seven_day"];
    assert_eq!(w["cycle"], cycle.as_str());
    assert_eq!(w["source"], "oauth");
    // And the replay stays out afterwards.
    f.statusline(&home, (40.0, r5), (20.0, r7));
    assert_eq!(
        f.meter("claude:personal")["windows"]["seven_day"]["used_percent"],
        10.0
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_stale_lower_statusline_reading_while_a_redeem_cycle_is_open_is_kept_out() {
    let f = Claude::new("1").await;
    let home = f.add("personal");
    let (r5, r7, cycle) = open_redeem(&f, &home).await;
    // Another session's last response, older than the redeem and lower than both values: a
    // statusline reading never closes an open cycle lower.
    f.statusline(&home, (40.0, r5), (5.0, r7));
    let m = f.meter("claude:personal");
    assert_eq!(m["windows"]["seven_day"]["used_percent"], 20.0, "{m}");
    assert_eq!(m["windows"]["seven_day"]["cycle"], cycle.as_str());
    fs::write(f.fake.join("usage-after-status"), "200").unwrap();
    let m = until_week(&f, 10.0).await;
    assert_eq!(m["windows"]["seven_day"]["cycle"], cycle.as_str());
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn an_older_recorder_writing_the_pre_redeem_value_after_the_cycle_closed_is_kept_out() {
    let f = Claude::new("0").await;
    let root = f.root().to_path_buf();
    let home = f.add("personal");
    let now = daemon::now();
    let (r5, r7) = (now + 3_600, now + 5 * 86_400);
    f.answer(&body(Some((40.0, r5)), (20.0, r7), json!([fable(1)])));
    ok(&root, &["account", "limits", "claude:personal"]);
    f.answer_after_claim(
        &json!({"result":"reset","resets_left":0}),
        &body(Some((40.0, r5)), (10.0, r7), json!([fable(0)])),
    );
    let v = ok(
        &root,
        &[
            "account",
            "reset-credit",
            "claude:personal",
            "--kind",
            "fable",
        ],
    );
    assert_eq!(
        v["rate_limits_after"]["weekly"]["used_percent"], 10.0,
        "{v}"
    );
    // A 1.56-1.64 recorder in a session started before the upgrade: it keeps its own rule
    // against usage.json (10), so a replay of its pre-redeem response (20) is "higher" to it
    // and it writes that, in its format, without the cycle.
    let at = daemon::now();
    fs::write(
        home.join(".sessionsation/usage.json"),
        json!({"source":"claude-code-statusline","observed_at":at,"claude_version":"2.1.314",
            "session_id":"5d6e7f80-1111-4222-8333-944455556666",
            "five_hour":{"used_percentage":40.0,"resets_at":r5,"observed_at":at,"source":"statusline"},
            "seven_day":{"used_percentage":20.0,"resets_at":r7,"observed_at":at,"source":"statusline"}})
        .to_string(),
    )
    .unwrap();
    let m = f.meter("claude:personal");
    assert_eq!(m["windows"]["seven_day"]["used_percent"], 10.0, "{m}");
    assert_eq!(m["windows"]["seven_day"]["cycle_reason"], "redeem");
    // The next write repairs the file.
    f.statusline(&home, (41.0, r5), (11.0, r7));
    let stored: Value =
        serde_json::from_slice(&fs::read(home.join(".sessionsation/usage.json")).unwrap()).unwrap();
    assert_eq!(stored["seven_day"]["used_percentage"], 11.0);
    assert_eq!(stored["seven_day"]["cycle_reason"], "redeem");
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_grant_that_vanishes_or_loses_resets_left_after_its_last_use_is_an_external_redeem() {
    let f = Claude::new("1").await;
    let now = daemon::now();
    let (r5, r7) = (now + 3_600, now + 5 * 86_400);
    f.answer(&body(
        Some((50.0, r5)),
        (70.0, r7),
        json!([full(1), fable(1)]),
    ));
    f.add("personal");
    until("the first read", || {
        let m = f.meter("claude:personal");
        (m["windows"]["seven_day"]["used_percent"] == 70.0).then_some(m)
    })
    .await;
    // The fable grant is used up elsewhere and no longer listed.
    f.answer(&body(Some((50.0, r5)), (60.0, r7), json!([full(1)])));
    let m = until_week(&f, 60.0).await;
    assert_eq!(m["windows"]["seven_day"]["cycle_reason"], "external_redeem");
    let first = m["windows"]["seven_day"]["cycle"].clone();
    // The full grant is used up and listed without resets_left (malformed, so skipped).
    let mut b = body(Some((2.0, r5)), (40.0, r7), json!([full(1)]));
    b["cedar_ember"]["grants"][0]["resets_left"] = Value::Null;
    f.answer(&b);
    let m = until_week(&f, 40.0).await;
    let w = &m["windows"]["seven_day"];
    assert_eq!(w["cycle_reason"], "external_redeem", "{m}");
    assert_ne!(w["cycle"], first);
    assert_eq!(m["windows"]["five_hour"]["used_percent"], 2.0);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_usage_json_out_of_step_with_the_cycle_file_with_an_oauth_source_never_lowers() {
    let f = Claude::new("0").await;
    let home = f.add("personal");
    let now = daemon::now();
    let (r5, r7) = (now + 3_600, now + 5 * 86_400);
    f.statusline(&home, (30.0, r5), (60.0, r7));
    let cycle = f.meter("claude:personal")["windows"]["seven_day"]["cycle"].clone();
    // usage.json alone says a lower OAuth value (an older release's OAuth read, or a write the
    // cycle file did not see): one OAuth answer, which can be stale.
    let at = daemon::now();
    fs::write(
        home.join(".sessionsation/usage.json"),
        json!({"source":"claude-code-statusline","observed_at":at,"claude_version":"2.1.314","session_id":null,
            "five_hour":{"used_percentage":30.0,"resets_at":r5,"observed_at":at,"source":"oauth"},
            "seven_day":{"used_percentage":50.0,"resets_at":r7,"observed_at":at,"source":"oauth"}})
        .to_string(),
    )
    .unwrap();
    let m = f.meter("claude:personal");
    assert_eq!(m["windows"]["seven_day"]["used_percent"], 60.0, "{m}");
    assert_eq!(m["windows"]["seven_day"]["cycle"], cycle);
    // The cycle file without usage.json's newer, higher window: that window counts.
    let at = daemon::now();
    fs::write(
        home.join(".sessionsation/usage.json"),
        json!({"source":"claude-code-statusline","observed_at":at,"claude_version":"2.1.314","session_id":null,
            "five_hour":{"used_percentage":30.0,"resets_at":r5,"observed_at":at,"source":"oauth"},
            "seven_day":{"used_percentage":64.0,"resets_at":r7,"observed_at":at,"source":"oauth"}})
        .to_string(),
    )
    .unwrap();
    let m = f.meter("claude:personal");
    assert_eq!(m["windows"]["seven_day"]["used_percent"], 64.0, "{m}");
    assert_eq!(m["windows"]["seven_day"]["cycle"], cycle);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_malformed_cedar_ember_block_costs_at_most_one_request_per_period() {
    let f = Claude::new("2").await;
    let now = daemon::now();
    let (r5, r7) = (now + 3_600, now + 5 * 86_400);
    let mut b = body(Some((12.0, r5)), (40.0, r7), json!([]));
    b["cedar_ember"] = json!({"eligible":"yes"});
    f.answer(&b);
    f.add("personal");
    let m = until("the meter despite the malformed block", || {
        let m = f.meter("claude:personal");
        (m["windows"]["seven_day"]["used_percent"] == 40.0).then_some(m)
    })
    .await;
    assert_eq!(m["fresh"], true);
    let calls = || {
        fs::read_to_string(f.fake.join("calls"))
            .unwrap_or_default()
            .lines()
            .count()
    };
    let start = calls();
    tokio::time::sleep(Duration::from_secs(6)).await;
    let spent = calls() - start;
    assert!(spent <= 4, "{spent} requests in 6 s with a 2 s period");
}

// ------------------------------------------------- after a window's own reset (SM11)

/// The number of usage requests the fake curl answered.
fn usage_calls(f: &Claude) -> usize {
    fs::read_to_string(f.fake.join("calls"))
        .unwrap_or_default()
        .lines()
        .filter(|l| l.starts_with("usage"))
        .count()
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn an_idle_claude_five_hour_window_reports_zero_and_starts_on_first_use_after_its_reset() {
    // The default period: only the reset itself makes the supervisor read again.
    let f = Claude::new("600").await;
    let now = daemon::now();
    let (r5, r7) = (now + 8, now + 5 * 86_400);
    f.answer(&body(Some((30.0, r5)), (40.0, r7), json!([])));
    f.add("personal");
    until("the first read", || {
        let m = f.meter("claude:personal");
        (m["windows"]["five_hour"]["used_percent"] == 30.0).then_some(m)
    })
    .await;
    // After the reset an idle account's 5-hour window has not started: no resets_at.
    f.answer(&json!({"five_hour":{"utilization":0.0,"resets_at":null},
        "seven_day":{"utilization":40.0,"resets_at":iso(r7)},"cedar_ember":{"eligible":true,"grants":[]}}));
    let m = until("the read after the reset", || {
        let m = f.meter("claude:personal");
        (m["windows"]["five_hour"]["starts_on_first_use"] == true).then_some(m)
    })
    .await;
    let w = &m["windows"]["five_hour"];
    assert_eq!(w["used_percent"], 0.0, "{m}");
    assert!(w["resets_at"].is_null());
    assert_eq!(w["cycle_reason"], "natural");
    assert_eq!(m["fresh"], true, "{m}");
    assert!(m["window_reasons"]["five_hour"].is_null());
    assert_eq!(m["windows"]["seven_day"]["starts_on_first_use"], false);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn an_idle_claude_weekly_window_shows_its_new_reset_after_one_triggered_read() {
    let f = Claude::new("600").await;
    let now = daemon::now();
    let (r5, r7) = (now + 3_600, now + 8);
    f.answer(&body(Some((10.0, r5)), (40.0, r7), json!([])));
    f.add("personal");
    until("the first read", || {
        let m = f.meter("claude:personal");
        (m["windows"]["seven_day"]["used_percent"] == 40.0).then_some(m)
    })
    .await;
    let next = now + 7 * 86_400;
    f.answer(&body(Some((10.0, r5)), (1.0, next), json!([])));
    let m = until("the triggered read", || {
        let m = f.meter("claude:personal");
        (m["windows"]["seven_day"]["resets_at"] == next).then_some(m)
    })
    .await;
    assert_eq!(m["fresh"], true, "{m}");
    assert_eq!(m["windows"]["seven_day"]["used_percent"], 1.0);
    assert_eq!(m["windows"]["seven_day"]["cycle_reason"], "natural");
    // One read per reset: nothing more within the period.
    let calls = usage_calls(&f);
    tokio::time::sleep(Duration::from_secs(5)).await;
    assert_eq!(usage_calls(&f), calls, "at most one read per window reset");
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn between_a_claude_reset_and_the_next_read_the_meter_is_not_fresh_and_says_why() {
    let f = Claude::new("0").await;
    let home = f.add("personal");
    let now = daemon::now();
    let (r5, r7) = (now + 2, now + 5 * 86_400);
    f.statusline(&home, (55.0, r5), (40.0, r7));
    assert_eq!(f.meter("claude:personal")["fresh"], true);
    tokio::time::sleep(Duration::from_millis(3_500)).await;
    let m = f.meter("claude:personal");
    assert_eq!(m["fresh"], false, "a past resets_at is never fresh: {m}");
    let why = format!("window reset at {r5}; awaiting a post-reset read");
    assert_eq!(m["reason"], why.as_str());
    assert_eq!(m["window_reasons"]["five_hour"], why.as_str());
    assert!(m["window_reasons"]["seven_day"].is_null());
    // Claude Code drops the window after its reset: null, still with the reason.
    let mut child = Command::new(binary())
        .args(["provider-hook", "claude-statusline", "--record-only"])
        .env("CLAUDE_CONFIG_DIR", &home)
        .stdin(Stdio::piped())
        .stdout(Stdio::null())
        .spawn()
        .unwrap();
    child
        .stdin
        .take()
        .unwrap()
        .write_all(
            json!({"session_id":"s","version":"2.1.314","rate_limits":{
                "seven_day":{"used_percentage":40.0,"resets_at":r7}}})
            .to_string()
            .as_bytes(),
        )
        .unwrap();
    assert!(child.wait().unwrap().success());
    let m = f.meter("claude:personal");
    assert!(m["windows"]["five_hour"].is_null(), "{m}");
    assert_eq!(m["window_reasons"]["five_hour"], why.as_str());
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_codex_window_past_its_reset_is_stale_with_a_reason_until_a_read_shows_the_new_one() {
    let f = Codex::new(2).await;
    let r5 = daemon::now() + 3;
    fs::write(f.home().join("fake-short-resets-at"), r5.to_string()).unwrap();
    f.until("the reading", |u| u["short"]["resets_at"] == r5)
        .await;
    // The provider has not rolled the window yet: the reading still names the past reset.
    let why = format!("window reset at {r5}; awaiting a post-reset read");
    let u = f
        .until("the reset to pass", |u| {
            u["meter"]["window_reasons"]["five_hour"] == why.as_str()
        })
        .await;
    assert_eq!(u["meter"]["fresh"], false, "{u}");
    assert!(u["meter"]["reason"].as_str().unwrap().contains(&why), "{u}");
    // The next read shows the new window.
    fs::remove_file(f.home().join("fake-short-resets-at")).unwrap();
    let u = f
        .until("the new window", |u| {
            u["short"]["resets_at"]
                .as_i64()
                .is_some_and(|r| r > daemon::now())
        })
        .await;
    let m = &u["meter"];
    assert_eq!(m["fresh"], true, "{m}");
    assert!(m["window_reasons"]["five_hour"].is_null());
    assert_eq!(m["windows"]["five_hour"]["starts_on_first_use"], false);
}
