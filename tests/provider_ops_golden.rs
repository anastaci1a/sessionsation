//! Golden outputs for the provider dispatch owner (`provider::ops`, step S1 of the
//! centralization program, 1.65.0). The move of every per-provider decision behind
//! `provider::ops` is a pure move: `api capabilities`, the per-provider refusal texts and
//! the state rows' `capabilities`/`capability_reasons` must stay byte-identical.
//!
//! The goldens in `tests/fixtures/provider-ops/` were recorded on main before the move
//! (`SESSIONSATION_GOLDEN_RECORD=1 cargo test --test provider_ops_golden`); this test fails on
//! any difference. Disposable homes and metadata-only fakes; no network or model calls.
#![allow(
    clippy::disallowed_methods,
    reason = "integration tests are exempt from owned roles (src/guard.rs)"
)]
use serde_json::{Value, json};
use sessionsation::{daemon, paths::Paths};
use std::{
    os::unix::fs::PermissionsExt,
    path::{Path, PathBuf},
    process::{Child, Command, Stdio},
    time::{Duration, Instant},
};

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

const PROVIDERS: [&str; 4] = ["codex", "claude", "gemini", "antigravity"];
const IDS: [&str; 4] = [
    "0a0a0a0a-0000-4000-8000-00000000c0de",
    "0b0b0b0b-0000-4000-8000-0000000c1a0d",
    "0c0c0c0c-0000-4000-8000-00000000003e",
    "0d0d0d0d-0000-4000-8000-000000000a9e",
];

struct Fixture {
    tmp: tempfile::TempDir,
    paths: Paths,
    project: PathBuf,
    _daemon: Process,
}
impl Fixture {
    fn run(&self, args: &[&str]) -> (bool, Value) {
        let out = Command::new(binary())
            .arg("--data-dir")
            .arg(&self.paths.root)
            .arg("api")
            .args(args)
            .env_remove("TMUX")
            .env_remove("TMUX_PANE")
            .env_remove("HERDR_ENV")
            .env_remove("SESSIONSATION_SESSION_ID")
            .env("XDG_RUNTIME_DIR", "/run/user/fixture")
            .stdin(Stdio::null())
            .output()
            .unwrap();
        let body = if out.status.success() {
            &out.stdout
        } else {
            &out.stderr
        };
        let v = serde_json::from_slice::<Value>(body)
            .unwrap_or_else(|_| json!({"unparsed": String::from_utf8_lossy(body)}));
        (out.status.success(), v)
    }
    fn ok(&self, args: &[&str]) -> Value {
        let (ok, v) = self.run(args);
        assert!(ok, "{args:?}: {v}");
        v["result"].clone()
    }
    fn home(&self, provider: &str) -> PathBuf {
        self.paths.root.join("accounts").join(provider).join("g")
    }
    /// Replace the fixture's paths so the goldens do not depend on the temp dir.
    fn normalize(&self, v: &Value) -> Value {
        let mut text = v.to_string();
        for (from, to) in [
            (self.project.to_string_lossy().into_owned(), "<project>"),
            (self.paths.root.to_string_lossy().into_owned(), "<root>"),
            (self.tmp.path().to_string_lossy().into_owned(), "<tmp>"),
        ] {
            text = text.replace(&from, to);
        }
        serde_json::from_str(&text).unwrap()
    }
}

async fn fixture() -> Fixture {
    let tmp = tempfile::tempdir().unwrap();
    let install = |name: &str, body: &str| {
        let path = tmp.path().join(name);
        std::fs::write(&path, body).unwrap();
        std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o700)).unwrap();
        path
    };
    let codex = install("fake-codex", include_str!("support/fake-codex.sh"));
    let claude = install("fake-claude", include_str!("support/fake-claude.sh"));
    let gemini = install("gemini.js", include_str!("support/fake-gemini.sh"));
    let agy = install("agy", include_str!("support/fake-agy.sh"));
    let paths = Paths::new(Some(tmp.path().join("state"))).unwrap();
    let config = sessionsation::config::Config {
        codex_binary: codex.to_str().unwrap().into(),
        claude_binary: claude.to_str().unwrap().into(),
        gemini_binary: gemini.to_str().unwrap().into(),
        antigravity_binary: agy.to_str().unwrap().into(),
        ..Default::default()
    };
    sessionsation::store::save_config(&paths, &config).unwrap();
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
        assert!(
            Instant::now() < deadline,
            "fixture supervisor did not start"
        );
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
    let project = tmp.path().join("project");
    std::fs::create_dir(&project).unwrap();
    let project = std::fs::canonicalize(project).unwrap();
    Fixture {
        tmp,
        paths,
        project,
        _daemon: daemon,
    }
}

/// One stopped conversation per provider, in each provider's own on-disk format.
fn seed(f: &Fixture) {
    let cwd = &f.project;
    // Codex rollout.
    let p = f
        .home("codex")
        .join(format!("sessions/2026/10/09/rollout-{}.jsonl", IDS[0]));
    std::fs::create_dir_all(p.parent().unwrap()).unwrap();
    std::fs::write(
        &p,
        format!(
            "{}\n{}\n",
            json!({"type":"session_meta","payload":{"id":IDS[0],"cwd":cwd,"source":"cli"}}),
            json!({"type":"event_msg","payload":{"type":"task_complete"}})
        ),
    )
    .unwrap();
    // Claude transcript.
    let dir = f
        .home("claude")
        .join("projects")
        .join(cwd.to_string_lossy().replace('/', "-"));
    std::fs::create_dir_all(&dir).unwrap();
    std::fs::write(
        dir.join(format!("{}.jsonl", IDS[1])),
        format!(
            "{}\n",
            json!({"type":"user","sessionId":IDS[1],"cwd":cwd,"timestamp":"2026-01-01T00:00:00.000Z","message":{"content":"x"}})
        ),
    )
    .unwrap();
    // Gemini chat.
    let slug = cwd
        .to_string_lossy()
        .trim_start_matches('/')
        .replace('/', "-");
    let home = f.home("gemini");
    let dir = home.join(".gemini/tmp").join(&slug).join("chats");
    std::fs::create_dir_all(&dir).unwrap();
    std::fs::write(
        dir.join(format!("session-2026-01-01T00-00-{}.jsonl", &IDS[2][..8])),
        format!(
            "{}\n{}\n",
            json!({"sessionId":IDS[2],"projectHash":"fixture","startTime":"2026-01-01T00:00:00.000Z","lastUpdated":"2026-01-01T00:00:00.000Z"}),
            json!({"type":"user","content":"x"})
        ),
    )
    .unwrap();
    std::fs::write(
        home.join(".gemini/projects.json"),
        json!({"projects":{cwd.to_string_lossy():slug}}).to_string(),
    )
    .unwrap();
    // Antigravity index row + conversation database.
    let state = f.home("antigravity").join(".gemini/antigravity-cli");
    std::fs::create_dir_all(state.join("conversations")).unwrap();
    let db = rusqlite::Connection::open(state.join("conversation_summaries.db")).unwrap();
    db.execute_batch("CREATE TABLE conversation_summaries (conversation_id text, title text NOT NULL DEFAULT '', step_count integer NOT NULL DEFAULT 0, last_modified_time datetime NOT NULL, workspace_uris text NOT NULL, PRIMARY KEY (conversation_id));").unwrap();
    db.execute(
        "INSERT INTO conversation_summaries (conversation_id, step_count, last_modified_time, workspace_uris) VALUES (?1, 3, '2026-01-01 00:00:00+00:00', ?2)",
        rusqlite::params![IDS[3], format!("[\"file://{}\"]", cwd.display())],
    )
    .unwrap();
    drop(db);
    std::fs::write(
        state.join("conversations").join(format!("{}.db", IDS[3])),
        "x",
    )
    .unwrap();
}

/// The per-provider fields of a state row that the dispatch owner computes.
fn row_fields(row: &Value) -> Value {
    let mut out = serde_json::Map::new();
    for k in [
        "provider",
        "capabilities",
        "capability_reasons",
        "resume_block_reason",
        "resume_supported",
    ] {
        if let Some(v) = row.get(k) {
            out.insert(k.into(), v.clone());
        }
    }
    Value::Object(out)
}

fn rows(f: &Fixture, cached: bool) -> Value {
    let args: &[&str] = if cached {
        &["state", "--cached"]
    } else {
        &["state"]
    };
    let state = f.ok(args);
    let mut out = serde_json::Map::new();
    for s in state["sessions"].as_array().unwrap() {
        let conversation = s["conversation_id"].as_str().unwrap_or_default();
        if let Some(i) = IDS.iter().position(|id| *id == conversation) {
            out.insert(PROVIDERS[i].into(), row_fields(s));
        }
    }
    let mut accounts = serde_json::Map::new();
    for a in state["accounts"].as_array().into_iter().flatten() {
        let mut a = a.clone();
        // Volatile: timestamps, usage readings and supervisor progress.
        if let Some(o) = a.as_object_mut() {
            o.retain(|k, _| {
                matches!(
                    k.as_str(),
                    "provider" | "capabilities" | "capability_reasons" | "account_logout"
                )
            });
        }
        accounts.insert(
            format!("{}:{}", a["provider"].as_str().unwrap_or("?"), "g"),
            a,
        );
    }
    json!({"sessions": out, "accounts": accounts})
}

/// Wait until two consecutive state reads agree (the supervisor's first account polls).
fn stable_rows(f: &Fixture) -> Value {
    let deadline = Instant::now() + Duration::from_secs(20);
    let mut last = rows(f, false);
    loop {
        std::thread::sleep(Duration::from_millis(400));
        let next = rows(f, false);
        if next == last && next["sessions"].as_object().is_some_and(|o| o.len() == 4) {
            return next;
        }
        assert!(Instant::now() < deadline, "state did not settle: {next}");
        last = next;
    }
}

/// Refusals that never reach a provider binary or change state.
fn refusals(f: &Fixture, state: &Value) -> Value {
    let mut out = serde_json::Map::new();
    let sessions = state["sessions"].as_array().unwrap();
    let project = f.project.to_string_lossy().into_owned();
    for (i, provider) in PROVIDERS.iter().enumerate() {
        let account = format!("{provider}:g");
        let mut cases: Vec<(String, Vec<String>)> = vec![];
        let mut case = |name: &str, args: &[&str]| {
            cases.push((name.into(), args.iter().map(|s| s.to_string()).collect()));
        };
        case(
            "launcher_new_hosted",
            &["launcher", "new", "--account", &account, "--hosted"],
        );
        case(
            "bypass_warning",
            &["account", "bypass-warning", &account, "--skip"],
        );
        if *provider != "claude" {
            case("managed_home", &["account", "managed-home", &account]);
        }
        if *provider != "gemini" {
            case(
                "auth_mode",
                &["account", "auth", &account, "--mode", "google"],
            );
        }
        if matches!(*provider, "codex" | "antigravity") {
            case(
                "live_steering",
                &["account", "live-steering", &account, "--enable"],
            );
        }
        if matches!(*provider, "gemini" | "antigravity") {
            case("reset_credit", &["account", "reset-credit", &account]);
        }
        let session = sessions
            .iter()
            .find(|s| s["conversation_id"] == IDS[i])
            .unwrap_or_else(|| panic!("no {provider} session"));
        let id = session["id"].as_str().unwrap().to_owned();
        let generation = session["generation"].as_str().unwrap().to_owned();
        let target = [
            "--session",
            id.as_str(),
            "--expected-generation",
            &generation,
        ];
        let with = |cmd: &[&str], extra: &[&str]| -> Vec<String> {
            cmd.iter()
                .chain(target.iter())
                .chain(extra.iter())
                .map(|s| s.to_string())
                .collect()
        };
        cases.push(("idle_notify".into(), with(&["idle-notify"], &[])));
        cases.push(("observe".into(), with(&["observe"], &[])));
        cases.push((
            "launcher_resume_hosted".into(),
            with(&["launcher", "resume"], &["--hosted"]),
        ));
        if *provider != "codex" {
            cases.push(("turn_target".into(), with(&["turn-target"], &[])));
            cases.push(("steer_target".into(), with(&["steer-target"], &[])));
            cases.push(("turn_notify".into(), with(&["turn-notify"], &["--enable"])));
            cases.push(("restart".into(), with(&["restart"], &[])));
            cases.push((
                "relocate_dry_run".into(),
                with(&["relocate"], &["--to", &project, "--dry-run"]),
            ));
            cases.push(("stop".into(), with(&["stop"], &[])));
            cases.push(("attachment".into(), with(&["attachment"], &[])));
        }
        let mut results = serde_json::Map::new();
        for (name, args) in cases {
            let args: Vec<&str> = args.iter().map(String::as_str).collect();
            let (ok, v) = f.run(&args);
            let v = if ok {
                json!({"ok": true})
            } else {
                let mut v = v;
                // Session ids and generations differ per run.
                let text = v
                    .to_string()
                    .replace(&id, "<session>")
                    .replace(&generation, "<generation>");
                v = serde_json::from_str(&text).unwrap();
                json!({"ok": false, "error": v["error"]})
            };
            results.insert(name, f.normalize(&v));
        }
        out.insert(provider.to_string(), Value::Object(results));
    }
    Value::Object(out)
}

/// Recording is explicit: `SESSIONSATION_GOLDEN_RECORD=1`, and never under CI.
fn recording() -> bool {
    let record = std::env::var_os("SESSIONSATION_GOLDEN_RECORD").is_some_and(|v| v == "1");
    assert!(
        !(record && std::env::var_os("CI").is_some()),
        "SESSIONSATION_GOLDEN_RECORD is set under CI: goldens are recorded by hand only"
    );
    record
}

fn golden(name: &str, actual: &Value) {
    let path = Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("tests/fixtures/provider-ops")
        .join(name);
    let text = serde_json::to_string_pretty(actual).unwrap() + "\n";
    if recording() {
        std::fs::create_dir_all(path.parent().unwrap()).unwrap();
        std::fs::write(&path, &text).unwrap();
        return;
    }
    let expected =
        std::fs::read_to_string(&path).unwrap_or_else(|e| panic!("{}: {e}", path.display()));
    let expected: Value = serde_json::from_str(&expected).unwrap();
    assert_eq!(
        &expected,
        actual,
        "{name} changed; provider::ops is a pure move\nexpected: {}\nactual: {text}",
        serde_json::to_string_pretty(&expected).unwrap()
    );
}

/// Contract-version bookkeeping, which changes with every bump and says nothing about
/// provider dispatch.
fn drop_versions(v: &mut Value) {
    match v {
        Value::Object(m) => {
            m.retain(|k, _| {
                k != "contract_version"
                    && k != "since_contract"
                    && k != "since_revision"
                    && !k.ends_with("_since_contract")
            });
            m.values_mut().for_each(drop_versions);
        }
        Value::Array(a) => a.iter_mut().for_each(drop_versions),
        _ => {}
    }
}

/// The provider-dependent part of `api capabilities`: `providers[]` and every top-level
/// block that names a provider or the unsupported-operation code, without version fields.
fn provider_capabilities(caps: &Value) -> Value {
    let mut out = serde_json::Map::new();
    for (k, v) in caps.as_object().unwrap() {
        let text = v.to_string();
        let dependent = k == "providers"
            || text.contains("UNSUPPORTED_PROVIDER_OPERATION")
            || PROVIDERS.iter().any(|p| text.contains(&format!("\"{p}\"")));
        if dependent {
            out.insert(k.clone(), v.clone());
        }
    }
    let mut out = Value::Object(out);
    drop_versions(&mut out);
    out
}

/// `--cached` reads the supervisor's snapshot, which it rebuilds on its own schedule:
/// wait until two consecutive cached reads agree and list every seeded session.
fn stable_cached_rows(f: &Fixture) -> Value {
    let deadline = Instant::now() + Duration::from_secs(20);
    let mut last = rows(f, true);
    loop {
        std::thread::sleep(Duration::from_millis(400));
        let next = rows(f, true);
        if next == last && next["sessions"].as_object().is_some_and(|o| o.len() == 4) {
            return next;
        }
        assert!(
            Instant::now() < deadline,
            "cached state did not settle: {next}"
        );
        last = next;
    }
}

/// `api capabilities` (provider-dependent part), state rows (live and `--cached`) and
/// per-provider refusals are identical to the goldens recorded before `provider::ops`.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn capabilities_state_rows_and_refusals_are_unchanged_per_provider() {
    let f = fixture().await;
    let caps = f.ok(&["capabilities"]);
    golden(
        "capabilities.json",
        &provider_capabilities(&f.normalize(&caps)),
    );
    for p in PROVIDERS {
        f.ok(&["account", "add", &format!("{p}:g")]);
    }
    seed(&f);
    let live = stable_rows(&f);
    golden("state-rows.json", &f.normalize(&live));
    let cached = stable_cached_rows(&f);
    golden("state-rows-cached.json", &f.normalize(&cached));
    let state = f.ok(&["state"]);
    golden("refusals.json", &refusals(&f, &state));
}
