//! Byte-exact goldens of the CLI surface (step C2 of the shrink program, 1.72.0): the
//! table-driven dispatch must not change a byte of `api capabilities`, `api state`, any
//! `-h`/`--help` page (hidden verbs included), any argument-parse error or the refusals
//! below. Recorded on main before the change with
//! `SESSIONSATION_GOLDEN_RECORD=1 cargo test --test cli_golden`; only the contract
//! version fields are normalized (they move with every release).
#![allow(
    clippy::disallowed_methods,
    reason = "integration tests are exempt from owned roles (src/guard.rs)"
)]
use serde_json::{Value, json};
use sessionsation::{daemon, paths::Paths};
use std::{
    collections::BTreeSet,
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

/// One run: exit code, stdout and stderr as text.
fn exec(root: Option<&Path>, args: &[String], stdin: &str) -> (i32, String, String) {
    let mut c = Command::new(binary());
    if let Some(root) = root {
        c.arg("--data-dir").arg(root);
    }
    let mut child = c
        .args(args)
        .env_remove("TMUX")
        .env_remove("TMUX_PANE")
        .env_remove("HERDR_ENV")
        .env_remove("SESSIONSATION_SESSION_ID")
        .env_remove("SESSIONSATION_HOME")
        .env("XDG_RUNTIME_DIR", "/run/user/fixture")
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .unwrap();
    {
        use std::io::Write;
        let mut input = child.stdin.take().unwrap();
        let _ = input.write_all(stdin.as_bytes());
    }
    let out = child.wait_with_output().unwrap();
    (
        out.status.code().unwrap_or(-1),
        String::from_utf8_lossy(&out.stdout).into_owned(),
        String::from_utf8_lossy(&out.stderr).into_owned(),
    )
}

fn argv(words: &[&str]) -> Vec<String> {
    words.iter().map(|s| s.to_string()).collect()
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

/// The contract version fields, which change with every release.
fn unversion(text: &str) -> String {
    let revision = format!(
        "\"contract_revision\":{}",
        sessionsation::api::CONTRACT_MINOR
    );
    let version = format!(
        "\"contract_version\":\"{}\"",
        sessionsation::api::CONTRACT_VERSION
    );
    text.replace(&revision, "\"contract_revision\":<revision>")
        .replace(&version, "\"contract_version\":\"<version>\"")
}

fn golden(name: &str, actual: &str) {
    let path = Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("tests/fixtures/cli-golden")
        .join(name);
    if recording() {
        std::fs::create_dir_all(path.parent().unwrap()).unwrap();
        std::fs::write(&path, actual).unwrap();
        return;
    }
    let expected =
        std::fs::read_to_string(&path).unwrap_or_else(|e| panic!("{}: {e}", path.display()));
    if expected != actual {
        let at = expected
            .bytes()
            .zip(actual.bytes())
            .position(|(a, b)| a != b)
            .unwrap_or(expected.len().min(actual.len()));
        let from = at.saturating_sub(300);
        panic!(
            "{name} differs at byte {at}\nexpected: ...{}\nactual:   ...{}",
            expected
                .get(from..(at + 300).min(expected.len()))
                .unwrap_or(""),
            actual.get(from..(at + 300).min(actual.len())).unwrap_or("")
        );
    }
}

// ------------------------------------------------------------------ help tree

/// Subcommand names listed under `Commands:` (without `help`).
fn listed(help: &str) -> Vec<String> {
    let Some((_, rest)) = help.split_once("\nCommands:\n") else {
        return vec![];
    };
    rest.lines()
        .take_while(|l| l.starts_with("  "))
        .filter_map(|l| l.split_whitespace().next())
        .filter(|n| *n != "help")
        .map(str::to_owned)
        .collect()
}

/// Every hidden subcommand name declared in the sources (`#[command(hide = true)]`).
fn hidden_names() -> BTreeSet<String> {
    let mut out = BTreeSet::new();
    let mut stack = vec![Path::new(env!("CARGO_MANIFEST_DIR")).join("src")];
    while let Some(dir) = stack.pop() {
        for e in std::fs::read_dir(&dir).unwrap() {
            let p = e.unwrap().path();
            if p.is_dir() {
                stack.push(p);
                continue;
            }
            if p.extension().is_none_or(|x| x != "rs") {
                continue;
            }
            let text = std::fs::read_to_string(&p).unwrap();
            let lines: Vec<&str> = text.lines().collect();
            for (i, l) in lines.iter().enumerate() {
                if !(l.contains("#[command(") && l.contains("hide = true")) {
                    continue;
                }
                let Some(variant) = lines[i + 1..]
                    .iter()
                    .map(|l| l.trim())
                    .find(|l| !l.starts_with("#[") && !l.starts_with("///"))
                else {
                    continue;
                };
                let ident: String = variant
                    .chars()
                    .take_while(|c| c.is_alphanumeric() || *c == '_')
                    .collect();
                let mut kebab = String::new();
                for (j, c) in ident.chars().enumerate() {
                    if c.is_uppercase() && j > 0 {
                        kebab.push('-');
                    }
                    kebab.extend(c.to_lowercase());
                }
                if !kebab.is_empty() {
                    out.insert(kebab);
                }
            }
        }
    }
    out
}

/// Every command path, listed or hidden, depth first from the root.
fn command_paths() -> Vec<Vec<String>> {
    let hidden = hidden_names();
    let mut out = vec![];
    let mut stack: Vec<Vec<String>> = vec![vec![]];
    let mut seen = BTreeSet::new();
    while let Some(path) = stack.pop() {
        if !seen.insert(path.clone()) {
            continue;
        }
        let mut words = path.clone();
        words.push("--help".into());
        let (code, help, _) = exec(None, &words, "");
        assert_eq!(code, 0, "{words:?}");
        let mut children = listed(&help);
        if help.contains("\nCommands:\n") {
            for h in &hidden {
                if children.contains(h) {
                    continue;
                }
                let mut probe = path.clone();
                probe.extend([h.clone(), "--help".into()]);
                if exec(None, &probe, "").0 == 0 {
                    children.push(h.clone());
                }
            }
        }
        for c in children.into_iter().rev() {
            let mut p = path.clone();
            p.push(c);
            stack.push(p);
        }
        out.push(path);
    }
    out
}

fn record(out: &mut String, root: Option<&Path>, args: &[String], stdin: &str) -> (i32, String) {
    let (code, stdout, stderr) = exec(root, args, stdin);
    out.push_str(&format!(
        "### {}\nexit {code}\n--- stdout\n{stdout}\n--- stderr\n{stderr}\n",
        args.join(" ")
    ));
    (code, stdout)
}

/// Whether the usage line names a required argument (so a bare run only parses).
fn requires_arguments(help: &str) -> bool {
    let Some(usage) = help.lines().find(|l| l.starts_with("Usage: ")) else {
        return false;
    };
    let mut depth = 0;
    let mut outside = String::new();
    for c in usage.chars() {
        match c {
            '[' => depth += 1,
            ']' => depth -= 1,
            _ if depth == 0 => outside.push(c),
            _ => {}
        }
    }
    outside.contains('<')
}

/// `-h` and `--help` of every command path, the parse error of an unknown flag on each, and
/// the missing-argument error of every path that has a required argument.
#[test]
fn help_tree_and_parse_errors_are_unchanged() {
    let paths = command_paths();
    let mut help = String::new();
    let mut errors = String::new();
    help.push_str(&format!("{} command paths\n", paths.len()));
    let mut pages = vec![];
    for p in &paths {
        for flag in ["-h", "--help"] {
            let mut a = p.clone();
            a.push(flag.into());
            let (_, page) = record(&mut help, None, &a, "");
            if flag == "--help" {
                pages.push(page);
            }
        }
    }
    let mut a = vec!["help".to_owned()];
    record(&mut help, None, &a, "");
    a.push("api".into());
    record(&mut help, None, &a, "");
    record(&mut help, None, &argv(&["--version"]), "");
    for (p, page) in paths.iter().zip(&pages) {
        let mut a = p.clone();
        a.push("--zz-golden-unknown".into());
        record(&mut errors, None, &a, "");
        if requires_arguments(page) {
            record(&mut errors, None, p, "");
        }
    }
    golden("help.txt", &help);
    golden("parse-errors.txt", &errors);
}

// ------------------------------------------------------------------ fixture root

const IDS: [&str; 4] = [
    "0a0a0a0a-0000-4000-8000-00000000c0de",
    "0b0b0b0b-0000-4000-8000-0000000c1a0d",
    "0c0c0c0c-0000-4000-8000-00000000003e",
    "0d0d0d0d-0000-4000-8000-000000000a9e",
];
const PROVIDERS: [&str; 4] = ["codex", "claude", "gemini", "antigravity"];

struct Fixture {
    tmp: tempfile::TempDir,
    paths: Paths,
    project: PathBuf,
    _daemon: Process,
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

impl Fixture {
    fn home(&self, provider: &str) -> PathBuf {
        self.paths.root.join("accounts").join(provider).join("g")
    }
    fn api(&self, args: &[&str]) -> Value {
        let mut a = argv(&["api"]);
        a.extend(argv(args));
        let (code, stdout, stderr) = exec(Some(&self.paths.root), &a, "");
        assert_eq!(code, 0, "{args:?}: {stderr}");
        serde_json::from_str::<Value>(&stdout).unwrap()["result"].clone()
    }
    fn normalize(&self, text: &str, ids: &[(String, String)]) -> String {
        let mut text = unversion(text);
        for (from, to) in [
            (self.project.to_string_lossy().into_owned(), "<project>"),
            (self.paths.root.to_string_lossy().into_owned(), "<root>"),
            (self.tmp.path().to_string_lossy().into_owned(), "<tmp>"),
        ] {
            text = text.replace(&from, to);
        }
        for (from, to) in ids {
            text = text.replace(from, to);
        }
        text
    }
}

/// One stopped conversation per provider, in each provider's own on-disk format (the same
/// seed as `provider_ops_golden`).
fn seed(f: &Fixture) {
    let cwd = &f.project;
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

/// Volatile values of a state read (clocks, usage polls, process and snapshot timing)
/// become `<v>`; everything else, key order included, is compared byte for byte.
fn devolatilize(text: &str) -> String {
    let volatile = regex::Regex::new(
        r#""((?:[a-z_]*_)?(?:at|time|seconds|age|age_seconds|observed|updated|checked|ms|until|since|expires|pid|start|started|epoch|deadline|elapsed|stale|fresh|revision_seen|accessed))":(-?[0-9.]+|"[^"]*")"#,
    )
    .unwrap();
    let text = volatile.replace_all(text, r#""$1":"<v>""#);
    // Epoch seconds or milliseconds wherever they appear (cycle ids, for example).
    let epoch = regex::Regex::new(r"\b1[0-9]{9}(?:[0-9]{3})?\b").unwrap();
    epoch.replace_all(&text, "<t>").into_owned()
}

/// Read `api state` (or `--cached`) until two consecutive reads agree after
/// normalization and every seeded session is listed.
fn stable_state(f: &Fixture, args: &[&str], ids: &[(String, String)]) -> String {
    let deadline = Instant::now() + Duration::from_secs(30);
    let read = || {
        let mut a = argv(&["api"]);
        a.extend(argv(args));
        let (code, stdout, stderr) = exec(Some(&f.paths.root), &a, "");
        assert_eq!(code, 0, "{stderr}");
        // Every object is a sorted map, so a parse and re-serialization is byte-identical;
        // only the row order (equal sort keys) varies between reads.
        let mut v: Value = serde_json::from_str(&stdout).unwrap();
        assert_eq!(serde_json::to_string(&v).unwrap() + "\n", stdout);
        for list in ["sessions", "accounts"] {
            if let Some(rows) = v["result"][list].as_array_mut() {
                rows.sort_by_key(|r| r.to_string());
            }
        }
        devolatilize(&f.normalize(&(serde_json::to_string(&v).unwrap() + "\n"), ids))
    };
    let mut last = read();
    loop {
        std::thread::sleep(Duration::from_millis(500));
        let next = read();
        if next == last && IDS.iter().all(|id| next.contains(id)) {
            return next;
        }
        assert!(Instant::now() < deadline, "state did not settle: {next}");
        last = next;
    }
}

/// Refusals of every verb family that never reach a provider binary or change state.
fn refusal_cases(sessions: &[(String, String)], project: &str) -> Vec<(Vec<String>, String)> {
    let mut out: Vec<(Vec<String>, String)> = vec![];
    let mut case = |words: &[&str], stdin: &str| out.push((argv(words), stdin.to_owned()));
    let unknown = "0f0f0f0f-0000-4000-8000-000000000000";
    // Verbs that take --session/--expected-generation, against an unknown session.
    for verb in [
        &["attachment"][..],
        &["observe"],
        &["turn-target"],
        &["steer-target"],
        &["rename", "--name", "x"],
        &["switch", "--to", "codex:g"],
        &["auto-recover", "--enable"],
        &["turn-notify", "--enable"],
        &["idle-notify"],
        &["relocate", "--to", project, "--dry-run"],
        &["relocate", "--to", project],
        &["restart"],
        &["stop"],
        &["stop", "--disable-auto-start"],
        &["stop", "--discard-queued"],
        &["resume", "--herdr-session", "h"],
        &["spaces"],
        &["spaces", "--herdr-session", "h"],
        &["answer", "--request-id", "r", "--answers", "{}"],
        &["session"],
        &["auto-start", "--mode", "start"],
        &["launcher", "resume"],
    ] {
        for (session, generation) in [(unknown, "g"), ("no-such-name", "g")] {
            let mut w = vec!["api"];
            w.extend_from_slice(&verb[..1]);
            w.extend(["--session", session, "--expected-generation", generation]);
            w.extend_from_slice(&verb[1..]);
            case(&w, "");
        }
    }
    for session in ["", "0f0f", unknown] {
        for verb in [
            &["session", "add-to-herdr", "--herdr-session", "h"][..],
            &["session", "compact", "--when-idle"],
            &["session", "compact"],
            &["session", "set-model", "--model", "m"],
            &["session", "set-model", "--model", "m", "--wait", "100000"],
            &["session", "set-space", "--space", "s"],
            &["session", "resolve"],
            &["move-space", "--space", "s"],
            &["archive"],
            &["restore-session"],
            &["delete-session"],
        ] {
            let mut w = vec!["api"];
            w.extend_from_slice(verb);
            w.extend(["--session", session, "--expected-generation", "g"]);
            case(&w, "");
        }
    }
    // Per seeded session: stale generations and provider-specific refusals.
    for (id, generation) in sessions {
        for g in [generation.as_str(), "stale"] {
            let mutating: &[&[&str]] = if g == "stale" {
                &[
                    &["rename", "--name", "x"],
                    &["session", "set-space", "--space", "s"],
                ]
            } else {
                &[]
            };
            for verb in [
                &["attachment"][..],
                &["switch", "--to", "nobody"],
                &["turn-notify", "--enable", "--disable"],
                &["auto-recover", "--enable", "--disable"],
                &["restart"],
                &["spaces"],
                &["session"],
                &["auto-start", "--mode", "bogus"],
                &["session", "compact"],
                &["session", "resolve"],
            ]
            .into_iter()
            .chain(mutating.iter().copied())
            {
                let mut w = vec!["api"];
                w.extend_from_slice(&verb[..1]);
                let action = verb.len() > 1 && verb[0] == "session";
                if action {
                    w.push(verb[1]);
                }
                w.extend(["--session", id, "--expected-generation", g]);
                w.extend_from_slice(&verb[if action { 2 } else { 1 }..]);
                case(&w, "");
            }
        }
    }
    for words in [
        &["api", "session", "--session", unknown][..],
        &["api", "session", "--expected-generation", "g"],
        &["api", "auto-start", "--all", "--mode", "start"],
        &[
            "api",
            "auto-start",
            "--all",
            "--mode",
            "disabled",
            "--prompt",
            "p",
        ],
        &["api", "session", "compact-status", "--operation", "nope"],
        &["api", "session", "compact-cancel", "--operation", "nope"],
        &["api", "session", "set-model-status", "--operation", "nope"],
        &["api", "session", "set-model-cancel", "--operation", "nope"],
        &["api", "session", "launch-status", "--request-id", "nope"],
        &["api", "respond", "nope", "--sequence", "1", "yes"],
        &["api", "cancel-operation", "nope"],
        &["api", "recover", "nope"],
        &[
            "api",
            "abandon-operation",
            "--operation",
            "nope",
            "--confirm",
            "x",
        ],
        &["api", "receipt", "nope"],
        &["api", "cancel", "nope"],
        &["api", "recovery-reconcile", "nope"],
        &["api", "usage", "--account", "nobody:x"],
        &["api", "hooks", "--account", "nobody:x"],
        &["api", "events", "--limit", "0"],
        &["api", "launch", "status", "--request-id", "nope"],
        &["api", "resume-launch", "status", "--request-id", "nope"],
        &["api", "account", "delete", "nobody"],
        &["api", "account", "show", "nobody"],
        &["api", "launcher", "new", "--account", "nobody"],
        &["api", "operations"],
        &["api", "archives"],
        &["api", "placements"],
        &["account", "show", "nobody"],
        &["account", "logout", "nobody"],
        &["account", "rename", "nobody", "x"],
        &["account", "reorder", "nobody", "1"],
        &["account", "history", "nobody"],
        &["account", "enable", "nobody"],
        &["account", "disable", "nobody"],
        &["account", "login", "nobody"],
        &["account", "managed-home", "nobody"],
        &[
            "account",
            "add",
            "--platform",
            "codex",
            "--config-dir",
            "/x",
            "y",
        ],
        &[
            "account",
            "add",
            "--platform",
            "gemini",
            "--config-dir",
            "/x",
            "y",
        ],
        &["account", "auth", "nobody", "--mode", "google"],
        &["account", "live-steering", "nobody", "--enable"],
        &["account", "bypass-warning", "nobody", "--skip"],
        &["config", "set", "no_such_key", "1"],
        &["root", "migrate"],
        &[
            "root",
            "migrate",
            "--session",
            unknown,
            "--prompt-file",
            "/nonexistent/x",
        ],
        &["shared", "migrate", "--account", "codex:x"],
        &["run", "--account", "nobody", "--session", "x"],
        &["run", "--account", "codex:g", "--hosted"],
        &[
            "run",
            "--account",
            "codex:g",
            "--name",
            "n",
            "--thread",
            "t",
        ],
        &["run", "--account", "claude:g", "--host", "herdr"],
        &["open", "--session", unknown],
        &["rename", "--session", unknown, "--name", "n"],
        &["auto-switch", "--thread", "nope"],
        &["auto-start", "--thread", "nope"],
        &["resume", "--session", unknown, "--expected-generation", "g"],
        &["session", "--session", unknown],
        &["cancel-move", "nope"],
        &["move-response", "nope", "yes"],
        &["recover", "nope"],
        &["retry", "nope"],
        &["background-response", "nope", "yes"],
    ] {
        case(words, "");
    }
    for verb in ["send", "recovery-plan", "checkpoints", "observations"] {
        case(&["api", verb], "");
        case(&["api", verb], "{");
        case(&["api", verb], &"x".repeat(32769));
        case(&["api", verb], r#"{"version":1}"#);
    }
    case(
        &[
            "api",
            "answer",
            "--session",
            unknown,
            "--expected-generation",
            "g",
            "--request-id",
            "r",
        ],
        "{",
    );
    case(
        &["account", "auth", "gemini:g", "--key-stdin"],
        &"k".repeat(4097),
    );
    out
}

/// `api capabilities`, `api state` (live and `--cached`) and the refusals of every verb
/// family on the seeded fixture root, byte for byte.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn capabilities_state_and_refusals_are_byte_identical() {
    let f = fixture().await;
    let (code, caps, _) = exec(Some(&f.paths.root), &argv(&["api", "capabilities"]), "");
    assert_eq!(code, 0);
    golden("capabilities.json", &unversion(&caps));
    for p in PROVIDERS {
        f.api(&["account", "add", &format!("{p}:g")]);
    }
    seed(&f);
    let mut ids = vec![];
    let mut sessions = vec![];
    let deadline = Instant::now() + Duration::from_secs(30);
    while sessions.len() < IDS.len() {
        assert!(Instant::now() < deadline, "fixture sessions did not appear");
        sessions.clear();
        let state = f.api(&["state"]);
        for conversation in IDS {
            if let Some(s) = state["sessions"]
                .as_array()
                .unwrap()
                .iter()
                .find(|s| s["conversation_id"] == *conversation)
            {
                let id = s["id"].as_str().unwrap().to_owned();
                let generation = s["generation"].as_str().unwrap().to_owned();
                sessions.push((id, generation));
            }
        }
        std::thread::sleep(Duration::from_millis(200));
    }
    for (i, (id, generation)) in sessions.iter().enumerate() {
        ids.push((id.clone(), format!("<session-{}>", PROVIDERS[i])));
        ids.push((generation.clone(), format!("<generation-{}>", PROVIDERS[i])));
    }
    golden("state.json", &stable_state(&f, &["state"], &ids));
    golden(
        "state-cached.json",
        &stable_state(&f, &["state", "--cached"], &ids),
    );
    let mut text = String::new();
    let project = f.project.to_string_lossy().into_owned();
    for (words, stdin) in refusal_cases(&sessions, &project) {
        let (code, stdout, stderr) = exec(Some(&f.paths.root), &words, &stdin);
        let shown = if stdin.len() > 64 {
            format!("<{} bytes>", stdin.len())
        } else {
            stdin.clone()
        };
        text.push_str(&format!(
            "### {} <<< {shown}\nexit {code}\n--- stdout\n{}\n--- stderr\n{}\n",
            f.normalize(&words.join(" "), &ids),
            resorted(&devolatilize(&f.normalize(&stdout, &ids))),
            resorted(&devolatilize(&f.normalize(&stderr, &ids)))
        ));
    }
    golden("refusals.txt", &text);
}

/// A normalized JSON line re-serialized: map keys that were random ids sort by their
/// placeholders. Each object is a sorted map, so this is otherwise the original bytes.
fn resorted(text: &str) -> String {
    text.split_inclusive('\n')
        .map(|line| match serde_json::from_str::<Value>(line) {
            Ok(v) if line.trim_start().starts_with('{') => {
                serde_json::to_string(&v).unwrap() + if line.ends_with('\n') { "\n" } else { "" }
            }
            _ => line.to_owned(),
        })
        .collect()
}
