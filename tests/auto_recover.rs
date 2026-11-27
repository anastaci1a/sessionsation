//! Per-session auto-recover (contract 1.35.0) with a metadata-only fake Claude
//! (`support/fake-claude.sh`) and a fake HERDR. Disposable homes; no provider
//! network, credentials, model requests or real panes.
#![allow(
    clippy::disallowed_methods,
    reason = "integration tests are exempt from owned roles (src/guard.rs)"
)]
use serde_json::{Value, json};
use sessionsation::{daemon, paths::Paths};
use std::{
    os::unix::fs::PermissionsExt,
    path::{Path, PathBuf},
    process::{Child, Command, Output, Stdio},
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
fn wait_for(what: &str, mut f: impl FnMut() -> bool) {
    let deadline = Instant::now() + Duration::from_secs(60);
    while !f() {
        assert!(Instant::now() < deadline, "timed out waiting for {what}");
        std::thread::sleep(Duration::from_millis(50));
    }
}
fn read(path: &Path) -> String {
    std::fs::read_to_string(path).unwrap_or_default()
}
fn kill_pid(pid: i32) {
    unsafe {
        libc::kill(pid, libc::SIGKILL);
    }
}
fn gone(pid: i32) -> bool {
    std::fs::read_to_string(format!("/proc/{pid}/stat")).map_or(true, |s| s.contains(") Z "))
}

#[derive(Clone, Copy)]
enum Ending {
    /// SIGKILL with a kernel OOM-kill line.
    OomKill,
    /// SIGKILL with a kernel segfault line.
    Segfault,
    /// SIGKILL and nothing in the kernel log (could be a manual kill).
    HardKill,
    /// `/exit` or any orderly shutdown (registry entry removed).
    Clean,
    /// The terminal closed (SIGHUP; registry entry removed).
    PaneClosed,
}

struct Setup {
    _tmp: tempfile::TempDir,
    paths: Paths,
    project: PathBuf,
    path: String,
    herdr_state: PathBuf,
    spare: Vec<Child>,
    _daemon: Process,
}
impl Drop for Setup {
    fn drop(&mut self) {
        for c in &mut self.spare {
            let _ = c.kill();
            let _ = c.wait();
        }
    }
}
impl Setup {
    async fn new() -> Self {
        let tmp = tempfile::tempdir().unwrap();
        let install = |name: &str, body: &str| {
            let path = tmp.path().join(name);
            std::fs::write(&path, body).unwrap();
            std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o700)).unwrap();
            path
        };
        let claude = install("fake-claude", include_str!("support/fake-claude.sh"));
        let codex = install("fake-codex", include_str!("support/fake-codex.sh"));
        let paths = Paths::new(Some(tmp.path().join("state"))).unwrap();
        let config = sessionsation::config::Config {
            codex_binary: codex.to_str().unwrap().into(),
            claude_binary: claude.to_str().unwrap().into(),
            ..Default::default()
        };
        sessionsation::store::save_config(&paths, &config).unwrap();
        // The test drives every pass itself: the supervisor's own loop is off.
        let daemon = Process(
            Command::new(binary())
                .arg("--data-dir")
                .arg(&paths.root)
                .args(["daemon", "serve"])
                .env("SESSIONSATION_AUTO_RECOVER_SECONDS", "0")
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
        let project = std::fs::canonicalize({
            let p = tmp.path().join("project");
            std::fs::create_dir(&p).unwrap();
            p
        })
        .unwrap();
        let bin = tmp.path().join("fake-bin");
        std::fs::create_dir_all(&bin).unwrap();
        let herdr = bin.join("herdr");
        std::fs::write(&herdr, include_str!("support/fake-herdr.sh")).unwrap();
        std::fs::set_permissions(&herdr, std::fs::Permissions::from_mode(0o700)).unwrap();
        let path = format!("{}:{}", bin.display(), std::env::var("PATH").unwrap());
        let herdr_state = tmp.path().join("herdr-state");
        let t = Self {
            _tmp: tmp,
            paths,
            project,
            path,
            herdr_state,
            spare: vec![],
            _daemon: daemon,
        };
        t.set_boot("boot-1");
        t.add_logged_in("a");
        t.add_logged_in("b");
        t
    }
    fn boot_file(&self) -> PathBuf {
        self.paths.root.parent().unwrap().join("boot-id")
    }
    fn kmsg_file(&self) -> PathBuf {
        self.paths.root.parent().unwrap().join("kmsg")
    }
    /// The host boots (a different boot id) or not (the same one).
    fn set_boot(&self, id: &str) {
        std::fs::write(self.boot_file(), format!("{id}\n")).unwrap();
    }
    fn command(&self, args: &[&str]) -> Command {
        let mut c = Command::new(binary());
        c.arg("--data-dir")
            .arg(&self.paths.root)
            .args(args)
            .env_remove("TMUX")
            .env_remove("TMUX_PANE")
            .env_remove("HERDR_ENV")
            .env_remove("CLAUDE_CONFIG_DIR")
            .env("PATH", &self.path)
            .env("FAKE_HERDR_DIR", &self.herdr_state)
            .env("SESSIONSATION_BOOT_ID_FILE", self.boot_file())
            .env("SESSIONSATION_KERNEL_LOG_FILE", self.kmsg_file())
            .env("SESSIONSATION_AUTO_RECOVER_PROMPT_SECONDS", "1")
            .env("SESSIONSATION_AUTO_RECOVER_NATIVE_SECONDS", "1")
            .env("SESSIONSATION_AUTO_RECOVER_CONFIRM_SECONDS", "30")
            .stdin(Stdio::null());
        c
    }
    fn output(&self, args: &[&str]) -> Output {
        self.command(args).output().unwrap()
    }
    fn api(&self, args: &[&str]) -> Value {
        let mut all = vec!["api"];
        all.extend_from_slice(args);
        let out = self.output(&all);
        assert!(
            out.status.success(),
            "{args:?}: {}",
            String::from_utf8_lossy(&out.stderr)
        );
        serde_json::from_slice::<Value>(&out.stdout).unwrap()["result"].clone()
    }
    fn api_error(&self, args: &[&str]) -> String {
        let mut all = vec!["api"];
        all.extend_from_slice(args);
        let out = self.output(&all);
        assert!(!out.status.success(), "{args:?} unexpectedly succeeded");
        String::from_utf8_lossy(&out.stderr).into_owned()
    }
    fn add_logged_in(&self, name: &str) {
        let out = self.output(&["account", "add", "--platform", "claude", name]);
        assert!(out.status.success());
        let id = format!("claude:{name}");
        wait_for("auth status poll", || {
            self.api(&["account", "list"])
                .as_array()
                .unwrap()
                .iter()
                .any(|a| a["id"] == id.as_str() && a["auth_required"] == false)
        });
    }
    fn home(&self, name: &str) -> PathBuf {
        self.paths.root.join("accounts/claude").join(name)
    }
    fn session(&self, id: &str) -> Value {
        self.api(&["state"])["sessions"]
            .as_array()
            .unwrap()
            .iter()
            .find(|s| s["id"] == id)
            .unwrap()
            .clone()
    }
    fn stopped_session(&self, account: &str) -> (String, String) {
        let native = uuid::Uuid::new_v4().to_string();
        let slug = self.project.to_string_lossy().replace('/', "-");
        let dir = self.home(account).join("projects").join(slug);
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::write(
            dir.join(format!("{native}.jsonl")),
            format!(
                "{}\n",
                json!({"type":"user","sessionId":native,"cwd":self.project,"timestamp":"2026-01-01T00:00:00Z"})
            ),
        )
        .unwrap();
        let s = self.api(&["state"])["sessions"]
            .as_array()
            .unwrap()
            .iter()
            .find(|s| s["conversation_id"] == native.as_str())
            .unwrap()
            .clone();
        assert_eq!(s["availability"], "stopped");
        (s["id"].as_str().unwrap().to_owned(), native)
    }
    /// A session running in the fake HERDR pane w1:p2; returns its process id.
    fn live_session(&self, id: &str) -> i32 {
        let s = self.session(id);
        let out = self.output(&[
            "api",
            "resume",
            "--session",
            id,
            "--expected-generation",
            s["generation"].as_str().unwrap(),
            "--herdr-session",
            "fixture",
            "--space",
            "w1",
        ]);
        assert!(
            out.status.success(),
            "{}",
            String::from_utf8_lossy(&out.stderr)
        );
        // The fake pane starts the session in the background; wait for it to register
        // (the removed three-scan refresh used to hide this race).
        let mut running = self.session(id);
        for _ in 0..100 {
            if running["availability"] == "running" {
                break;
            }
            std::thread::sleep(Duration::from_millis(100));
            running = self.session(id);
        }
        assert_eq!(running["availability"], "running", "{running}");
        assert_eq!(running["attachment"]["target"], "herdr/fixture/w1:p2");
        running["pid"].as_u64().unwrap() as i32
    }
    fn enable(&self, id: &str, extra: &[&str]) -> Result<Value, String> {
        self.set(id, "--enable", extra)
    }
    fn set(&self, id: &str, flag: &str, extra: &[&str]) -> Result<Value, String> {
        let s = self.session(id);
        let mut args = vec![
            "api",
            "auto-recover",
            "--session",
            id,
            "--expected-generation",
            s["generation"].as_str().unwrap(),
            flag,
        ];
        args.extend_from_slice(extra);
        let out = self.output(&args);
        if out.status.success() {
            Ok(serde_json::from_slice::<Value>(&out.stdout).unwrap()["result"].clone())
        } else {
            Err(String::from_utf8_lossy(&out.stderr).into_owned())
        }
    }
    /// One supervisor pass; returns the recoveries it ran.
    fn tick(&self) -> Vec<Value> {
        let out = self.output(&["auto-recover-tick"]);
        assert!(
            out.status.success(),
            "{}",
            String::from_utf8_lossy(&out.stderr)
        );
        serde_json::from_slice(&out.stdout).unwrap()
    }
    /// The checks that must pass before a recovery (consecutive not-live checks).
    fn ticks(&self, n: usize) -> Vec<Value> {
        (0..n).flat_map(|_| self.tick()).collect()
    }
    fn recoveries(&self) -> Vec<Value> {
        self.api(&["operations"])
            .as_array()
            .unwrap()
            .iter()
            .filter(|o| o["kind"] == "auto_recover")
            .cloned()
            .collect()
    }
    fn herdr_calls(&self, what: &str) -> usize {
        read(&self.herdr_state.join("calls")).matches(what).count()
    }
    fn setting(&self, id: &str) -> Value {
        self.session(id)["auto_recover"].clone()
    }
    /// The live process dies of a crash signal the kernel logged (an OOM kill).
    fn crash(&self, pid: i32) {
        self.end(pid, Ending::OomKill);
    }
    /// The live process ends. The supervisor has seen it running first, as it always
    /// has on a real host (a pass every few seconds).
    fn end(&self, pid: i32, ending: Ending) {
        self.tick();
        let signal = match ending {
            Ending::Clean => libc::SIGTERM,
            Ending::PaneClosed => libc::SIGHUP,
            _ => libc::SIGKILL,
        };
        unsafe {
            libc::kill(pid, signal);
        }
        wait_for("process exit", || gone(pid));
        let entry = self.home("a").join("sessions").join(format!("{pid}.json"));
        let line = match ending {
            Ending::OomKill => Some(format!(
                "[99999999.000000] Out of memory: Killed process {pid} (claude) total-vm:1kB\n"
            )),
            Ending::Segfault => Some(format!(
                "[99999999.000001] claude[{pid}]: segfault at 0 ip 0000 sp 0000 error 4\n"
            )),
            _ => None,
        };
        if let Some(line) = line {
            use std::io::Write;
            let mut f = std::fs::OpenOptions::new()
                .create(true)
                .append(true)
                .open(self.kmsg_file())
                .unwrap();
            f.write_all(line.as_bytes()).unwrap();
        }
        // An orderly shutdown removes the registry entry; a hard kill cannot.
        if matches!(ending, Ending::Clean | Ending::PaneClosed) {
            let _ = std::fs::remove_file(entry);
        }
        if let Ok(wrapper) = read(&self.herdr_state.join("pid-w1:p2"))
            .trim()
            .parse::<i32>()
        {
            wait_for("wrapper exit", || gone(wrapper));
        }
    }
    fn stored(&self, id: &str) -> Value {
        serde_json::from_str::<Value>(&read(&self.store_path())).unwrap()["sessions"][id].clone()
    }
    fn store_path(&self) -> PathBuf {
        self.paths.root.join(sessionsation::layout::AUTO_RECOVER)
    }
}
fn cleanup(t: &Setup, id: &str) {
    if let Some(pid) = t.session(id)["pid"].as_u64() {
        kill_pid(pid as i32);
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn enabling_works_on_a_stopped_session_and_disable_is_always_allowed() {
    let t = Setup::new().await;
    let caps = t.api(&["capabilities"]);
    assert_eq!(caps["contract_version"], "1.73.3");
    for p in caps["providers"].as_array().unwrap() {
        assert_eq!(p["new_account_name_max"], 30);
        assert_eq!(p["account_display_name_max"], 64);
        assert_eq!(p["session_name_max"], 64);
    }
    assert_eq!(caps["auto_recover"]["enable_requires"], "none");
    assert_eq!(caps["auto_start"]["enable_requires"], "none");
    assert_eq!(caps["auto_start"]["explicit_stop"], "marks_only");
    let (id, native) = t.stopped_session("a");
    let stopped = t.session(&id);
    assert_eq!(stopped["capabilities"]["auto_recover"], true);
    assert_eq!(stopped["capabilities"]["auto_start"], true);
    assert_eq!(stopped["capability_reasons"]["auto_start"], Value::Null);
    assert_eq!(stopped["auto_recover"]["enabled"], false);
    let on = t.enable(&id, &[]).unwrap();
    assert_eq!(on["changed"], true);
    assert_eq!(on["auto_start"]["mode"], "start_prompt");
    assert_eq!(t.session(&id)["availability"], "stopped");
    let off = t.set(&id, "--disable", &[]).unwrap();
    assert_eq!(off["changed"], true);
    assert_eq!(off["auto_recover"]["enabled"], false);
    t.set(&id, "--disable", &[]).unwrap();
    // Exactly one of --enable / --disable.
    let both = t.api_error(&[
        "auto-recover",
        "--session",
        &id,
        "--expected-generation",
        stopped["generation"].as_str().unwrap(),
    ]);
    assert!(both.contains("INVALID_ARGUMENT"), "{both}");
    // Stale generation.
    let stale = t.api_error(&[
        "auto-recover",
        "--session",
        &id,
        "--expected-generation",
        "0000",
        "--enable",
    ]);
    assert!(stale.contains("STALE_TARGET"), "{stale}");
    // A hosted (stream-json) session is unsupported with its own code.
    let host = t
        .paths
        .root
        .join(sessionsation::layout::CLAUDE_HOSTED)
        .join(&native);
    std::fs::create_dir_all(&host).unwrap();
    let now = daemon::now();
    std::fs::write(
        host.join("host.json"),
        json!({"schema_version":1,"conversation_id":native,"session_id":native,"account":"a",
            "directory":t.project,"origin":"resume","permission_mode":"account",
            "effective_permission_mode":null,"epoch":1,"state":"starting","reason":null,
            "worker_pid":null,"worker_start":null,"child_pid":null,"child_start":null,"busy":false,
            "pending_permissions":[],"created_at":now,"updated_at":now,"started_at":null,"stopped_at":null})
        .to_string(),
    )
    .unwrap();
    let hosted = t.enable(&id, &[]).unwrap_err();
    assert!(hosted.contains("AUTO_RECOVER_UNSUPPORTED"), "{hosted}");
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn enable_and_disable_persist_without_changing_the_generation() {
    let t = Setup::new().await;
    let (id, _) = t.stopped_session("a");
    let _pid = t.live_session(&id);
    let before = t.session(&id);
    assert_eq!(before["capabilities"]["auto_recover"], true, "{before}");
    assert!(before["capability_reasons"]["auto_recover"].is_null());
    let on = t.enable(&id, &["--prompt", "carry on"]).unwrap();
    assert_eq!(on["changed"], true);
    assert_eq!(on["auto_recover"]["enabled"], true);
    assert_eq!(on["auto_recover"]["prompt"], "carry on");
    assert_eq!(on["auto_recover"]["last_pane"], "herdr/fixture/w1:p2");
    assert_eq!(on["auto_recover"]["workspace_label"], "Main");
    assert_eq!(on["generation"], before["generation"]);
    let row = t.session(&id);
    assert_eq!(row["generation"], before["generation"]);
    assert_eq!(row["auto_recover"]["last_pane"], "herdr/fixture/w1:p2");
    assert!(row["auto_recover"]["last_result"].is_null());
    // Same request again changes nothing.
    assert_eq!(
        t.enable(&id, &["--prompt", "carry on"]).unwrap()["changed"],
        false
    );
    // The setting survives in the registry file (crash-safe write) and a fresh read.
    let stored: Value = serde_json::from_str(&read(&t.store_path())).unwrap();
    assert_eq!(stored["sessions"][&id]["enabled"], true);
    let off = t.set(&id, "--disable", &[]).unwrap();
    assert_eq!(off["changed"], true);
    assert_eq!(t.setting(&id)["enabled"], false);
    assert_eq!(t.setting(&id)["last_result"]["code"], "DISABLED");
    cleanup(&t, &id);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_vanished_process_is_resumed_in_its_old_idle_pane_once_and_never_duplicated() {
    let t = Setup::new().await;
    let (id, native) = t.stopped_session("a");
    let pid = t.live_session(&id);
    t.enable(&id, &["--prompt", "carry on"]).unwrap();

    // Still live: any number of passes starts nothing.
    assert!(t.ticks(5).is_empty());
    assert_eq!(t.herdr_calls("pane run"), 1);
    assert!(t.recoveries().is_empty());

    t.crash(pid);
    assert_eq!(t.session(&id)["availability"], "stopped");
    // Two consecutive checks are not enough...
    assert!(t.ticks(2).is_empty());
    assert_eq!(t.herdr_calls("pane run"), 1);
    // ...the third recovers.
    let done = t.tick();
    assert_eq!(done.len(), 1, "{done:?}");
    let op = &done[0];
    assert_eq!(op["kind"], "auto_recover");
    assert_eq!(op["phase"], "complete", "{op}");
    assert_eq!(op["resume"]["state"], "confirmed");
    assert_eq!(op["resume"]["pane"], "herdr/fixture/w1:p2");
    assert_eq!(op["resume"]["pane_reused"], true);
    assert_eq!(op["replays"], false);
    let running = t.session(&id);
    assert_eq!(running["availability"], "running", "{running}");
    assert_eq!(running["attachment"]["target"], "herdr/fixture/w1:p2");
    assert_ne!(running["pid"].as_u64().unwrap() as i32, pid);
    assert_eq!(t.herdr_calls("pane run"), 2);
    // The reused shell's input line is cleared before our command is typed.
    let log = read(&t.herdr_state.join("calls"));
    let keys = log
        .find("pane send-keys w1:p2 ctrl+e ctrl+u")
        .expect("clear keys sent");
    assert_eq!(log.matches("pane send-keys").count(), 1);
    assert!(keys > log.find("pane run").unwrap());
    assert!(
        keys < log.rfind("pane run").unwrap(),
        "cleared before the second command"
    );
    assert_eq!(
        t.herdr_calls("tab create"),
        1,
        "no new tab for a reusable pane"
    );
    assert_eq!(
        read(&t.herdr_state.join("pane-name-w1:p2")).trim(),
        running["name"]
    );
    let launch = read(&t.home("a").join("fake-launch-resume"));
    assert!(launch.contains(&format!("session={native}")), "{launch}");
    // The prompt goes through the normal send path exactly once (the fake pane
    // has no terminal input, so the send is refused and recorded, never retried later).
    assert!(op["prompt"]["attempts"].as_u64().is_some_and(|n| n >= 1));
    let message_id = op["prompt"]["message_id"].clone();
    let state = t.setting(&id);
    assert_eq!(state["enabled"], true);
    assert_eq!(
        t.session(&id)["auto_start"]["last_result"]["state"],
        "started_after_crash",
        "the fake pane cannot take the prompt: not delivered, not reported as delivered"
    );
    assert_eq!(state["last_pane"], "herdr/fixture/w1:p2");
    assert!(state["last_recovered_at"].as_i64().unwrap() > 0);
    assert!(state["last_result"]["operation_id"] == op["id"], "{state}");
    // Journaled: visible in operations, terminal, nothing to recover.
    let ops = t.recoveries();
    assert_eq!(ops.len(), 1);
    assert_eq!(ops[0]["id"], op["id"]);
    assert_eq!(ops[0]["prompt"]["message_id"], message_id);
    assert_eq!(ops[0]["can_recover"], false);
    // Live again: more passes start nothing and send nothing.
    assert!(t.ticks(5).is_empty());
    assert_eq!(t.herdr_calls("pane run"), 2);
    assert_eq!(t.recoveries().len(), 1);
    cleanup(&t, &id);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_busy_or_missing_pane_gets_a_new_tab_in_the_same_workspace() {
    for missing in [false, true] {
        let mut t = Setup::new().await;
        let (id, _) = t.stopped_session("a");
        let pid = t.live_session(&id);
        t.enable(&id, &[]).unwrap();
        t.crash(pid);
        if missing {
            std::fs::write(t.herdr_state.join("missing-w1:p2"), "").unwrap();
        } else {
            // Something else now runs in the old pane: never run over it.
            let sleeper = Command::new("sleep").arg("600").spawn().unwrap();
            std::fs::write(t.herdr_state.join("pid-w1:p2"), sleeper.id().to_string()).unwrap();
            t.spare.push(sleeper);
        }
        let done = t.ticks(3);
        assert_eq!(done.len(), 1, "{done:?}");
        let op = &done[0];
        assert_eq!(op["phase"], "complete", "{op}");
        assert_eq!(op["resume"]["pane"], "herdr/fixture/w1:p3", "{op}");
        assert_eq!(op["resume"]["pane_reused"], false);
        assert_eq!(op["resume"]["recorded_pane"], "herdr/fixture/w1:p2");
        assert_eq!(t.herdr_calls("tab create"), 2);
        assert_eq!(
            read(&t.herdr_state.join("label-w1:p3")).trim(),
            t.session(&id)["name"]
        );
        let running = t.session(&id);
        assert_eq!(running["attachment"]["target"], "herdr/fixture/w1:p3");
        assert_eq!(running["auto_recover"]["last_pane"], "herdr/fixture/w1:p3");
        cleanup(&t, &id);
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn archive_or_delete_disables_and_switch_only_suppresses() {
    let t = Setup::new().await;
    let (id, _) = t.stopped_session("a");
    let pid = t.live_session(&id);
    t.enable(&id, &[]).unwrap();
    // A deliberate account switch (it stops and resumes the process itself) is
    // never "recovered" a second time: recovery is suppressed until the session is live.
    let s = t.session(&id);
    let op = t.api(&[
        "switch",
        "--session",
        &id,
        "--expected-generation",
        s["generation"].as_str().unwrap(),
        "--to",
        "claude:b",
        "--accept-risks",
    ]);
    wait_for("switch", || {
        t.api(&["operations"])
            .as_array()
            .unwrap()
            .iter()
            .any(|o| o["id"] == op["id"] && o["phase"] == "complete")
    });
    let _ = pid;
    assert!(t.ticks(4).is_empty());
    let after = t.setting(&id);
    assert_eq!(after["enabled"], true, "{after}");
    assert_eq!(
        after["suppressed"], false,
        "live again clears the suppression"
    );
    assert!(t.recoveries().is_empty());
    t.set(&id, "--disable", &[]).unwrap();
    cleanup(&t, &id);

    // Archive of a stopped session disables it even though it was enabled before.
    let (id2, _) = t.stopped_session("a");
    std::fs::create_dir_all(t.store_path().parent().unwrap()).unwrap();
    let mut stored: Value = serde_json::from_str(&read(&t.store_path())).unwrap();
    stored["sessions"][&id2] = json!({"enabled":true,"misses":0,"recoveries":[],"enabled_at":1});
    std::fs::write(t.store_path(), stored.to_string()).unwrap();
    assert_eq!(t.stored(&id2)["enabled"], true);
    let s2 = t.session(&id2);
    let request = uuid::Uuid::new_v4().to_string();
    let out = t.output(&[
        "api",
        "archive",
        "--session",
        &id2,
        "--expected-generation",
        s2["generation"].as_str().unwrap(),
        "--request-id",
        &request,
        "--confirm",
        &id2,
    ]);
    let _ = out;
    assert_eq!(t.stored(&id2)["enabled"], false, "archive disables");
    assert_eq!(t.stored(&id2)["last_result"]["code"], "ARCHIVED_OR_DELETED");
    assert!(t.ticks(4).is_empty());
    assert!(t.recoveries().is_empty());
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn at_most_three_automatic_recoveries_per_hour() {
    let t = Setup::new().await;
    let (id, _) = t.stopped_session("a");
    let pid = t.live_session(&id);
    t.enable(&id, &[]).unwrap();
    let mut stored: Value = serde_json::from_str(&read(&t.store_path())).unwrap();
    let now = daemon::now();
    stored["sessions"][&id]["recoveries"] = json!([now - 30, now - 20, now - 10]);
    std::fs::write(t.store_path(), stored.to_string()).unwrap();
    t.crash(pid);
    assert!(t.ticks(6).is_empty());
    assert_eq!(t.herdr_calls("pane run"), 1, "nothing was started");
    assert_eq!(t.session(&id)["availability"], "stopped");
    let result = t.setting(&id)["last_result"].clone();
    assert_eq!(result["state"], "rate_limited", "{result}");
    assert_eq!(result["code"], "RATE_LIMITED");
    assert_eq!(t.setting(&id)["recoveries_last_hour"], 3);
    assert_eq!(t.setting(&id)["enabled"], true);
    // Once the window passes the recovery proceeds.
    let mut stored: Value = serde_json::from_str(&read(&t.store_path())).unwrap();
    stored["sessions"][&id]["recoveries"] = json!([now - 4000, now - 3900, now - 3800]);
    std::fs::write(t.store_path(), stored.to_string()).unwrap();
    let done = t.ticks(3);
    assert_eq!(done.len(), 1, "{done:?}");
    assert_eq!(done[0]["phase"], "complete");
    cleanup(&t, &id);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn nothing_is_started_while_herdr_is_unreachable() {
    let t = Setup::new().await;
    let (id, _) = t.stopped_session("a");
    let pid = t.live_session(&id);
    t.enable(&id, &[]).unwrap();
    t.crash(pid);
    // HERDR down: no pane is touched, the wait is surfaced, and misses keep their count.
    let out = {
        let mut c = t.command(&["auto-recover-tick"]);
        c.env("PATH", "/usr/bin:/bin");
        c.output().unwrap()
    };
    assert!(out.status.success());
    assert_eq!(t.herdr_calls("pane run"), 1);
    let mut down = vec![];
    for _ in 0..3 {
        let mut c = t.command(&["auto-recover-tick"]);
        c.env("PATH", "/usr/bin:/bin");
        down.extend(serde_json::from_slice::<Vec<Value>>(&c.output().unwrap().stdout).unwrap());
    }
    assert!(down.is_empty());
    assert_eq!(t.setting(&id)["last_result"]["code"], "HERDR_UNAVAILABLE");
    assert_eq!(t.session(&id)["availability"], "stopped");
    // HERDR back: the next pass recovers without waiting for new misses.
    let done = t.tick();
    assert_eq!(done.len(), 1, "{done:?}");
    assert_eq!(done[0]["phase"], "complete");
    cleanup(&t, &id);
}

impl Setup {
    fn start(&self, id: &str, mode: &str, extra: &[&str]) -> Result<Value, String> {
        let s = self.session(id);
        let mut args = vec![
            "api",
            "auto-start",
            "--session",
            id,
            "--expected-generation",
            s["generation"].as_str().unwrap(),
            "--mode",
            mode,
        ];
        args.extend_from_slice(extra);
        let out = self.output(&args);
        if out.status.success() {
            Ok(serde_json::from_slice::<Value>(&out.stdout).unwrap()["result"].clone())
        } else {
            Err(String::from_utf8_lossy(&out.stderr).into_owned())
        }
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn auto_start_modes_transition_and_the_deprecated_alias_follows() {
    let t = Setup::new().await;
    let caps = t.api(&["capabilities"]);
    assert_eq!(caps["auto_start"]["default_mode"], "disabled");
    assert_eq!(caps["auto_recover"]["deprecated"], true);
    let (id, _) = t.stopped_session("a");
    let _pid = t.live_session(&id);
    let row = t.session(&id);
    assert_eq!(row["auto_start"]["mode"], "disabled");
    assert_eq!(row["capabilities"]["auto_start"], true);
    // --prompt is only valid with start_prompt.
    for mode in ["start", "disabled"] {
        let e = t.start(&id, mode, &["--prompt", "x"]).unwrap_err();
        assert!(e.contains("INVALID_ARGUMENT"), "{e}");
    }
    assert!(
        t.start(&id, "bogus", &[])
            .unwrap_err()
            .contains("INVALID_ARGUMENT")
    );
    assert!(!t.store_path().exists());
    let on = t.start(&id, "start", &[]).unwrap();
    assert_eq!(on["changed"], true);
    assert_eq!(on["auto_start"]["mode"], "start");
    assert_eq!(on["auto_recover"]["enabled"], true);
    assert_eq!(t.start(&id, "start", &[]).unwrap()["changed"], false);
    let sp = t.start(&id, "start_prompt", &["--prompt", "hi"]).unwrap();
    assert_eq!(sp["changed"], true);
    assert_eq!(sp["auto_start"]["prompt"], "hi");
    assert_eq!(t.stored(&id)["mode"], "start_prompt");
    // Deprecated alias: disable, then enable maps to start_prompt.
    assert_eq!(
        t.set(&id, "--disable", &[]).unwrap()["auto_start"]["mode"],
        "disabled"
    );
    assert_eq!(t.session(&id)["auto_recover"]["enabled"], false);
    let en = t.enable(&id, &[]).unwrap();
    assert_eq!(en["auto_start"]["mode"], "start_prompt");
    assert_eq!(en["auto_recover"]["enabled"], true);
    // Bulk disable.
    let all = t.api(&["auto-start", "--all", "--mode", "disabled"]);
    assert_eq!(all["disabled_count"], 1);
    assert_eq!(t.session(&id)["auto_start"]["mode"], "disabled");
    let e = t.api_error(&["auto-start", "--all", "--mode", "start"]);
    assert!(e.contains("INVALID_ARGUMENT"), "{e}");
    cleanup(&t, &id);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn mode_start_resumes_without_a_prompt_and_the_rate_limit_applies() {
    let t = Setup::new().await;
    let (id, _) = t.stopped_session("a");
    let pid = t.live_session(&id);
    t.start(&id, "start", &[]).unwrap();
    t.crash(pid);
    let done = t.ticks(3);
    assert_eq!(done.len(), 1, "{done:?}");
    let op = &done[0];
    assert_eq!(op["phase"], "complete", "{op}");
    assert_eq!(op["mode"], "start");
    assert_eq!(op["prompt"]["state"], "skipped");
    assert_eq!(op["prompt"]["attempts"], 0);
    assert_eq!(t.session(&id)["availability"], "running");
    let st = t.session(&id)["auto_start"].clone();
    assert_eq!(st["last_result"]["state"], "started_after_crash", "{st}");
    assert!(st["last_started_at"].as_i64().unwrap() > 0);
    assert_eq!(
        t.setting(&id)["last_result"]["state"],
        "recovered_without_prompt"
    );
    // Rate limit still applies in start mode.
    let pid = t.session(&id)["pid"].as_u64().unwrap() as i32;
    let mut stored: Value = serde_json::from_str(&read(&t.store_path())).unwrap();
    let now = daemon::now();
    stored["sessions"][&id]["recoveries"] = json!([now - 30, now - 20, now - 10]);
    std::fs::write(t.store_path(), stored.to_string()).unwrap();
    t.crash(pid);
    assert!(t.ticks(6).is_empty());
    assert_eq!(t.session(&id)["availability"], "stopped");
    assert_eq!(
        t.session(&id)["auto_start"]["last_result"]["state"],
        "rate_limited"
    );
    cleanup(&t, &id);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_pre_1_43_record_is_migrated_to_start_prompt_on_the_next_pass() {
    let t = Setup::new().await;
    let (id, _) = t.stopped_session("a");
    let _pid = t.live_session(&id);
    std::fs::create_dir_all(t.store_path().parent().unwrap()).unwrap();
    std::fs::write(
        t.store_path(),
        json!({"schema_version":1,"sessions":{id.clone():{"enabled":true,"prompt":"old"}}})
            .to_string(),
    )
    .unwrap();
    assert_eq!(t.session(&id)["auto_start"]["mode"], "start_prompt");
    t.tick();
    let s = t.stored(&id);
    assert_eq!(s["mode"], "start_prompt");
    assert_eq!(s["prompt"], "old");
    cleanup(&t, &id);
}

/// Resume the session once (so it has a process), then give it a mode.
fn live_with_mode(t: &Setup, id: &str, mode: &str) -> i32 {
    let pid = t.live_session(id);
    t.start(id, mode, &[]).unwrap();
    pid
}
fn state_of(t: &Setup, id: &str) -> String {
    t.session(id)["auto_start"]["last_result"]["state"]
        .as_str()
        .unwrap_or_default()
        .to_owned()
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_new_boot_starts_stopped_sessions_once_and_the_same_boot_starts_nothing() {
    let t = Setup::new().await;
    let (id, native) = t.stopped_session("a");
    // The mode works on a stopped session; the same boot (and any number of daemon or
    // supervisor restarts, which are just more passes) starts nothing.
    let on = t.start(&id, "start", &[]).unwrap();
    assert_eq!(on["auto_start"]["mode"], "start");
    assert!(t.ticks(6).is_empty());
    assert_eq!(t.session(&id)["availability"], "stopped");
    assert_eq!(t.herdr_calls("pane run"), 0);
    // The host boots: after the settle checks the session is started, with its trigger.
    t.set_boot("boot-2");
    assert!(t.ticks(2).is_empty());
    let done = t.tick();
    assert_eq!(done.len(), 1, "{done:?}");
    assert_eq!(done[0]["phase"], "complete", "{}", done[0]);
    assert_eq!(done[0]["prompt"]["state"], "skipped");
    assert_eq!(t.session(&id)["availability"], "running");
    assert_eq!(state_of(&t, &id), "started_after_boot");
    let launch = read(&t.home("a").join("fake-launch-resume"));
    assert!(launch.contains(&format!("session={native}")), "{launch}");
    assert_eq!(t.stored(&id)["boot_started"], "boot-2");
    // Exactly once per boot: more passes, even after it dies unclassified, start nothing.
    assert!(t.ticks(4).is_empty());
    let pid = t.session(&id)["pid"].as_u64().unwrap() as i32;
    t.end(pid, Ending::HardKill);
    assert!(t.ticks(6).is_empty());
    assert_eq!(t.recoveries().len(), 1);
    // The next boot starts it again.
    t.set_boot("boot-3");
    assert_eq!(t.ticks(3).len(), 1);
    assert_eq!(t.recoveries().len(), 2);
    cleanup(&t, &id);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn an_explicit_stop_keeps_the_mode_and_only_the_next_boot_starts_it() {
    let t = Setup::new().await;
    let (id, _) = t.stopped_session("a");
    let _pid = live_with_mode(&t, &id, "start");
    assert!(t.ticks(2).is_empty());
    // `api stop` marks the session first (note_deliberate_action), then the process ends.
    sessionsation::auto_recover::explicit_stop(&t.paths, &id);
    let pid = t.session(&id)["pid"].as_u64().unwrap() as i32;
    unsafe {
        libc::kill(pid, libc::SIGTERM);
    }
    wait_for("stopped", || gone(pid));
    let _ = std::fs::remove_file(t.home("a").join("sessions").join(format!("{pid}.json")));
    assert_eq!(t.session(&id)["availability"], "stopped");
    assert_eq!(t.session(&id)["auto_start"]["mode"], "start", "mode kept");
    // Same boot: stopped on purpose is not a crash.
    assert!(t.ticks(6).is_empty());
    assert_eq!(state_of(&t, &id), "skipped_explicit_stop");
    assert_eq!(t.session(&id)["auto_start"]["mode"], "start");
    // Next boot: started although it was stopped explicitly before the reboot.
    t.set_boot("boot-2");
    let done = t.ticks(3);
    assert_eq!(done.len(), 1, "{done:?}");
    assert_eq!(state_of(&t, &id), "started_after_boot");
    cleanup(&t, &id);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_clean_exit_and_a_closed_pane_start_nothing() {
    for (ending, label) in [(Ending::Clean, "clean"), (Ending::PaneClosed, "pane")] {
        let t = Setup::new().await;
        let (id, _) = t.stopped_session("a");
        let pid = live_with_mode(&t, &id, "start_prompt");
        t.end(pid, ending);
        assert!(t.ticks(6).is_empty(), "{label}");
        assert_eq!(t.herdr_calls("pane run"), 1, "{label}: nothing was started");
        assert_eq!(state_of(&t, &id), "skipped_clean_exit", "{label}");
        assert_eq!(t.session(&id)["auto_start"]["mode"], "start_prompt");
        assert_eq!(t.setting(&id)["last_result"]["state"], "waiting");
        assert!(t.recoveries().is_empty());
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn crash_signals_start_and_an_unknown_exit_does_not() {
    // A segfault in the kernel log is a crash.
    let t = Setup::new().await;
    let (id, _) = t.stopped_session("a");
    let pid = live_with_mode(&t, &id, "start");
    t.end(pid, Ending::Segfault);
    assert!(t.ticks(2).is_empty());
    let done = t.tick();
    assert_eq!(done.len(), 1, "{done:?}");
    assert_eq!(state_of(&t, &id), "started_after_crash");
    cleanup(&t, &id);

    // A kill with no kernel record cannot be told from a closed pane: no start.
    let t = Setup::new().await;
    let (id, _) = t.stopped_session("a");
    let pid = live_with_mode(&t, &id, "start");
    t.end(pid, Ending::HardKill);
    assert!(t.ticks(6).is_empty());
    assert_eq!(state_of(&t, &id), "exit_unclassified");
    assert_eq!(
        t.session(&id)["auto_start"]["last_result"]["code"],
        "EXIT_UNCLASSIFIED"
    );
    assert_eq!(t.session(&id)["auto_start"]["mode"], "start");
    assert!(t.recoveries().is_empty());
}

// ------------------------------------------------- idle-tab reuse (contract 1.52.0)

impl Setup {
    /// HERDR lists tabs (opt-in in the fake; without it `tab list` is unsupported).
    fn list_tabs(&self) {
        std::fs::create_dir_all(&self.herdr_state).unwrap();
        std::fs::write(self.herdr_state.join("tab-list"), "").unwrap();
    }
    /// An existing tab w1:t<n> with one idle shell pane w1:p<n>.
    fn idle_tab(&self, n: u32, label: &str) {
        std::fs::create_dir_all(&self.herdr_state).unwrap();
        std::fs::write(self.herdr_state.join(format!("label-w1:p{n}")), label).unwrap();
    }
    fn mark(&self, file: &str, body: &str) {
        std::fs::create_dir_all(&self.herdr_state).unwrap();
        std::fs::write(self.herdr_state.join(file), body).unwrap();
    }
    fn name(&self, id: &str) -> String {
        self.session(id)["name"].as_str().unwrap().to_owned()
    }
    /// `api resume --space w1`, then wait until the session runs.
    fn resume_w1(&self, id: &str) -> Value {
        let s = self.session(id);
        let result = self.api(&[
            "resume",
            "--session",
            id,
            "--expected-generation",
            s["generation"].as_str().unwrap(),
            "--herdr-session",
            "fixture",
            "--space",
            "w1",
        ]);
        wait_for("resumed session", || {
            self.session(id)["availability"] == "running"
        });
        result
    }
    fn set_reuse(&self, on: bool) {
        let mut config = sessionsation::store::load_config(&self.paths).unwrap();
        config.reuse_idle_tabs = on;
        sessionsation::store::save_config(&self.paths, &config).unwrap();
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn api_resume_reuses_a_same_named_single_pane_idle_tab() {
    let t = Setup::new().await;
    let (id, _) = t.stopped_session("a");
    let name = t.name(&id);
    t.list_tabs();
    t.idle_tab(6, &format!("{name} 2"));
    t.idle_tab(7, &name);
    t.idle_tab(8, &name);
    let result = t.resume_w1(&id);
    assert_eq!(result["target"], "herdr/fixture/w1:p7", "{result}");
    assert_eq!(result["pane_reused"], true);
    assert_eq!(result["detail"], "Reused an idle tab with the same name.");
    assert_eq!(t.herdr_calls("tab create"), 0);
    assert_eq!(
        t.herdr_calls("pane send-keys"),
        0,
        "only the command is typed"
    );
    let log = read(&t.herdr_state.join("calls"));
    let run = log.lines().find(|l| l.contains("pane run w1:p7 ")).unwrap();
    assert!(
        run.contains("cd -- ") && run.contains("SESSIONSATION_LAUNCHER_BYPASS=1"),
        "{run}"
    );
    assert_eq!(
        t.session(&id)["attachment"]["target"],
        "herdr/fixture/w1:p7"
    );
    cleanup(&t, &id);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn api_resume_reuses_the_sessions_recorded_idle_pane_first() {
    let t = Setup::new().await;
    let (id, _) = t.stopped_session("a");
    let name = t.name(&id);
    let pid = t.live_session(&id);
    // Auto-start records the pane; a clean exit starts nothing.
    t.enable(&id, &[]).unwrap();
    assert_eq!(t.setting(&id)["last_pane"], "herdr/fixture/w1:p2");
    t.end(pid, Ending::Clean);
    assert_eq!(t.session(&id)["availability"], "stopped");
    // Its tab was renamed meanwhile, and a same-named idle tab exists too.
    t.mark("renamed-w1:t2", "elsewhere");
    t.list_tabs();
    t.idle_tab(7, &name);
    let result = t.resume_w1(&id);
    assert_eq!(result["target"], "herdr/fixture/w1:p2", "{result}");
    assert_eq!(result["pane_reused"], true);
    assert_eq!(
        result["detail"],
        "Reused the session's previous pane (an idle shell)."
    );
    assert_eq!(t.herdr_calls("tab create"), 1, "only the first start");
    // The tab takes the name a new tab would get.
    assert_eq!(read(&t.herdr_state.join("renamed-w1:t2")).trim(), name);
    cleanup(&t, &id);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn api_resume_opens_a_new_tab_when_no_idle_tab_qualifies() {
    for case in [
        "busy",
        "two_panes",
        "other_label",
        "setting_off",
        "older_herdr",
    ] {
        let t = Setup::new().await;
        let (id, _) = t.stopped_session("a");
        let name = t.name(&id);
        t.list_tabs();
        t.idle_tab(7, &name);
        match case {
            // The shell is listed, but another process group owns the terminal.
            "busy" => t.mark("fg-group-w1:p7", "4242"),
            "two_panes" => t.mark("extra-pane-w1:t7", "w1:p70"),
            "other_label" => t.idle_tab(7, &format!("{name}x")),
            "setting_off" => t.set_reuse(false),
            _ => {
                let _ = std::fs::remove_file(t.herdr_state.join("tab-list"));
            }
        }
        let result = t.resume_w1(&id);
        assert_eq!(result["target"], "herdr/fixture/w1:p2", "{case}: {result}");
        assert!(result.get("pane_reused").is_none(), "{case}: {result}");
        assert_eq!(t.herdr_calls("tab create"), 1, "{case}");
        assert_eq!(t.herdr_calls("pane run w1:p7"), 0, "{case}");
        cleanup(&t, &id);
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn api_resume_never_takes_another_running_sessions_pane() {
    let t = Setup::new().await;
    let (a, _) = t.stopped_session("a");
    let (b, _) = t.stopped_session("a");
    let name = t.name(&a);
    // B runs in w1:p2; that tab now carries A's name and its pane looks idle.
    t.live_session(&b);
    t.mark("renamed-w1:t2", &name);
    std::fs::remove_file(t.herdr_state.join("pid-w1:p2")).unwrap();
    t.list_tabs();
    assert_eq!(t.session(&b)["attachment"]["target"], "herdr/fixture/w1:p2");
    let result = t.resume_w1(&a);
    assert_eq!(result["target"], "herdr/fixture/w1:p3", "{result}");
    assert!(result.get("pane_reused").is_none());
    cleanup(&t, &a);
    cleanup(&t, &b);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_pane_that_turns_busy_before_typing_gets_a_new_tab() {
    let t = Setup::new().await;
    let (id, _) = t.stopped_session("a");
    let name = t.name(&id);
    t.list_tabs();
    t.idle_tab(7, &name);
    // Idle when picked; busy at the re-check right before typing.
    t.mark("busy-after-w1:p7", "1");
    let result = t.resume_w1(&id);
    assert_eq!(result["target"], "herdr/fixture/w1:p2", "{result}");
    assert!(result.get("pane_reused").is_none());
    assert_eq!(t.herdr_calls("tab create"), 1);
    assert_eq!(t.herdr_calls("pane run w1:p7"), 0);
    cleanup(&t, &id);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn api_launch_start_reuses_a_same_named_idle_tab_and_says_so() {
    launch_into_same_named_tab(false).await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn api_launch_start_opens_a_new_tab_when_the_pane_turns_busy_before_typing() {
    launch_into_same_named_tab(true).await;
}

async fn launch_into_same_named_tab(race: bool) {
    let t = Setup::new().await;
    if race {
        // Idle when picked; busy at the re-check right before typing.
        t.mark("busy-after-w1:p7", "1");
    }
    let socket_dir = t.paths.root.parent().unwrap().join("host");
    std::fs::create_dir_all(&socket_dir).unwrap();
    let socket = socket_dir.join("host.sock");
    let _listener = std::os::unix::net::UnixListener::bind(&socket).unwrap();
    t.list_tabs();
    t.idle_tab(7, "Fresh");
    let request_id = uuid::Uuid::new_v4().to_string();
    let request = json!({"version":1,"request_id":request_id,"account_id":"claude:a","name":"Fresh",
        "host":{"session":"fixture","socket":socket},"space":{"kind":"existing","id":"w1"},
        "directory":{"path":t.project,"create":[],"confirmed_existing":true}});
    let mut child = t
        .command(&["api", "launch", "start"])
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .unwrap();
    {
        use std::io::Write;
        child
            .stdin
            .take()
            .unwrap()
            .write_all(request.to_string().as_bytes())
            .unwrap();
    }
    let out = child.wait_with_output().unwrap();
    assert!(
        out.status.success(),
        "{}",
        String::from_utf8_lossy(&out.stderr)
    );
    let mut receipt = Value::Null;
    wait_for("launch dispatched into the reused pane", || {
        receipt = t.api(&["launch", "status", "--request-id", &request_id]);
        matches!(
            receipt["phase"].as_str(),
            Some("starting_provider" | "attached" | "running")
        )
    });
    let pane = if race { "w1:p2" } else { "w1:p7" };
    assert_eq!(receipt["pane_id"], pane, "{receipt}");
    if race {
        assert!(receipt.get("pane_reused").is_none(), "{receipt}");
        assert_eq!(t.herdr_calls("tab create"), 1);
        assert_eq!(t.herdr_calls("pane run w1:p7"), 0);
    } else {
        assert_eq!(receipt["tab_id"], "w1:t7");
        // `detail` says "Reused an idle tab with the same name." until launch-run
        // reports progress in it; pane_reused stays.
        assert_eq!(receipt["pane_reused"], true);
        assert_eq!(t.herdr_calls("tab create"), 0);
    }
    assert_eq!(t.herdr_calls("pane send-keys"), 0);
    wait_for("launched session", || {
        t.api(&["state"])["sessions"]
            .as_array()
            .unwrap()
            .iter()
            .any(|s| s["name"] == "Fresh" && s["availability"] == "running")
    });
    let launched = t.api(&["state"])["sessions"]
        .as_array()
        .unwrap()
        .iter()
        .find(|s| s["name"] == "Fresh")
        .unwrap()["id"]
        .as_str()
        .unwrap()
        .to_owned();
    assert_eq!(
        t.session(&launched)["attachment"]["target"],
        format!("herdr/fixture/{pane}")
    );
    cleanup(&t, &launched);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn auto_start_takes_a_same_named_idle_tab_before_a_new_one() {
    let mut t = Setup::new().await;
    let (id, _) = t.stopped_session("a");
    let name = t.name(&id);
    let pid = t.live_session(&id);
    t.enable(&id, &[]).unwrap();
    t.crash(pid);
    // The recorded pane is busy; a same-named idle tab exists.
    let sleeper = Command::new("sleep").arg("600").spawn().unwrap();
    std::fs::write(t.herdr_state.join("pid-w1:p2"), sleeper.id().to_string()).unwrap();
    t.spare.push(sleeper);
    t.list_tabs();
    t.idle_tab(7, &name);
    let done = t.ticks(3);
    assert_eq!(done.len(), 1, "{done:?}");
    let op = &done[0];
    assert_eq!(op["phase"], "complete", "{op}");
    assert_eq!(op["resume"]["pane"], "herdr/fixture/w1:p7", "{op}");
    assert_eq!(op["resume"]["pane_reused"], true);
    assert_eq!(op["resume"]["recorded_pane"], "herdr/fixture/w1:p2");
    assert_eq!(t.herdr_calls("tab create"), 1, "only the first start");
    assert_eq!(
        t.session(&id)["attachment"]["target"],
        "herdr/fixture/w1:p7"
    );
    cleanup(&t, &id);
}

impl Setup {
    fn stop(&self, id: &str, extra: &[&str]) -> Value {
        let s = self.session(id);
        let mut args = vec![
            "stop",
            "--session",
            id,
            "--expected-generation",
            s["generation"].as_str().unwrap(),
        ];
        args.extend_from_slice(extra);
        self.api(&args)
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn claude_stop_with_disable_auto_start_disables_it_and_without_the_flag_keeps_the_mode() {
    let t = Setup::new().await;
    let (id, _) = t.stopped_session("a");
    // With the flag on a live session: stopped, then disabled, reported in the result.
    let pid = live_with_mode(&t, &id, "start_prompt");
    let stopped = t.stop(&id, &["--disable-auto-start", "--keep-pane"]);
    assert_eq!(stopped["phase"], "complete", "{stopped}");
    assert_eq!(stopped["signal"], "SIGTERM");
    assert_eq!(stopped["pid"], pid);
    assert_eq!(stopped["pane_closed"], false);
    assert_eq!(stopped["pane_close_reason"], "KEEP_PANE");
    assert!(gone(pid));
    assert_eq!(stopped["auto_start"]["mode"], "disabled", "{stopped}");
    assert_eq!(stopped["auto_start"]["enabled"], false);
    assert_eq!(t.session(&id)["auto_start"]["mode"], "disabled");
    // Without the flag (in the kept pane): the mode is kept and the idle pane is closed.
    let pid = live_with_mode(&t, &id, "start");
    let stopped = t.stop(&id, &[]);
    assert_eq!(stopped["phase"], "complete", "{stopped}");
    assert_eq!(stopped["pid"], pid);
    assert_eq!(stopped["pane_closed"], true, "{stopped}");
    assert_eq!(stopped["pane_close_reason"], Value::Null);
    assert!(gone(pid));
    // Contract 1.63.0: the shared stop key set always has auto_start; null without the flag.
    assert!(stopped["auto_start"].is_null(), "{stopped}");
    assert_eq!(t.session(&id)["auto_start"]["mode"], "start");
    // With the flag on an already stopped session: idempotent, and disabled.
    let again = t.stop(&id, &["--disable-auto-start"]);
    assert_eq!(again["already_stopped"], true, "{again}");
    assert_eq!(again["auto_start"]["mode"], "disabled");
    assert_eq!(t.session(&id)["auto_start"]["mode"], "disabled");
    cleanup(&t, &id);
}

/// Contract 1.67.1: a session-launch record admitted by a launcher that died before the launch
/// receipt was written is admitted again by the repeated request, not failed "never admitted".
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_repeated_session_launch_readmits_an_admitted_record_without_a_receipt() {
    let t = Setup::new().await;
    let socket_dir = t.paths.root.parent().unwrap().join("host");
    std::fs::create_dir_all(&socket_dir).unwrap();
    let socket = socket_dir.join("host.sock");
    let _listener = std::os::unix::net::UnixListener::bind(&socket).unwrap();
    t.list_tabs();
    let brief = t.project.join("brief");
    std::fs::write(&brief, "do the thing").unwrap();
    let id = uuid::Uuid::new_v4().to_string();
    let launch = || {
        let out = t
            .command(&[
                "api",
                "session",
                "launch",
                "--space",
                "w1",
                "--name",
                "Retry",
                "--account",
                "claude:a",
                "--model",
                "claude-fable-5-2",
                "--brief-file",
                brief.to_str().unwrap(),
                "--directory",
                t.project.to_str().unwrap(),
                "--herdr-session",
                "fixture",
                "--herdr-socket",
                socket.to_str().unwrap(),
                "--request-id",
                &id,
            ])
            .output()
            .unwrap();
        assert!(
            out.status.success(),
            "{}{}",
            String::from_utf8_lossy(&out.stdout),
            String::from_utf8_lossy(&out.stderr)
        );
        serde_json::from_slice::<Value>(&out.stdout).unwrap()
    };
    launch();
    let receipt = t
        .paths
        .root
        .join(sessionsation::layout::LAUNCHES)
        .join(format!("{id}.json"));
    wait_for("first launch receipt", || receipt.is_file());
    // The launcher died after saving the record, before the receipt: nothing else exists.
    let record = t
        .paths
        .root
        .join(sessionsation::layout::SESSION_LAUNCHES)
        .join(format!("{id}.json"));
    let mut r: Value = serde_json::from_slice(&std::fs::read(&record).unwrap()).unwrap();
    r["phase"] = json!("admitted");
    r["reason"] = Value::Null;
    r["detail"] = Value::Null;
    std::fs::write(&record, serde_json::to_vec(&r).unwrap()).unwrap();
    std::fs::remove_file(&receipt).unwrap();
    let again = launch();
    assert_eq!(again["result"]["existing"], true, "{again}");
    assert!(receipt.is_file(), "the repeated request admitted again");
    let phase = t.api(&["launch", "status", "--request-id", &id])["phase"].clone();
    assert!(phase.is_string(), "{phase}");
    let rec: Value = serde_json::from_slice(&std::fs::read(&record).unwrap()).unwrap();
    assert_ne!(rec["phase"], "failed", "{rec}");
    assert_ne!(rec["reason"], "launch_failed", "{rec}");
}
