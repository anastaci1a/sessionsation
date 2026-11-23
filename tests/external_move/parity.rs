//! Contract 1.61.0: `session launch --brief-delivery initial-prompt` for Codex and
//! `api stop --wait` for Codex.
use super::*;
use std::os::unix::fs::PermissionsExt;

fn run(f: &Fixture, args: &[&str], envs: &[(&str, &str)]) -> (bool, Value) {
    let mut c = Command::new(binary());
    c.env("TMUX", format!("{},1,0", f.socket))
        .env(
            "PATH",
            format!(
                "{}:{}",
                f.tmp.path().display(),
                std::env::var("PATH").unwrap()
            ),
        )
        .arg("--data-dir")
        .arg(&f.paths.root)
        .arg("api")
        .args(args);
    for (k, v) in envs {
        c.env(k, v);
    }
    let o = c.output().unwrap();
    let text = if o.stdout.is_empty() {
        &o.stderr
    } else {
        &o.stdout
    };
    let v = serde_json::from_slice(text)
        .unwrap_or_else(|e| panic!("{args:?}: {e}: {}", String::from_utf8_lossy(text)));
    (o.status.success(), v)
}

fn session(f: &Fixture) -> Value {
    f.invoke(&["api", "state"])["result"]["sessions"]
        .as_array()
        .unwrap()
        .iter()
        .find(|s| s["conversation_id"] == ID)
        .unwrap()
        .clone()
}

fn stop(f: &Fixture, extra: &[&str], envs: &[(&str, &str)]) -> (bool, Value) {
    let s = session(f);
    let mut args = vec![
        "stop",
        "--session",
        s["id"].as_str().unwrap(),
        "--expected-generation",
        s["generation"].as_str().unwrap(),
    ];
    args.extend(extra);
    run(f, &args, envs)
}

fn auto_start(f: &Fixture, mode: &str) {
    let s = session(f);
    let on = f.invoke(&[
        "api",
        "auto-start",
        "--session",
        s["id"].as_str().unwrap(),
        "--expected-generation",
        s["generation"].as_str().unwrap(),
        "--mode",
        mode,
    ]);
    assert_eq!(on["result"]["auto_start"]["mode"], mode, "{on}");
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn codex_initial_prompt_delivery_is_first_turn() {
    let f = Fixture::new().await;
    let socket = f.tmp.path().join("host.sock");
    let _listener = std::os::unix::net::UnixListener::bind(&socket).unwrap();
    let script = f.tmp.path().join("herdr");
    fs::write(
        &script,
        "#!/bin/sh\ncase \"$1 $2\" in\n 'workspace list') printf '%s\\n' '{\"result\":{\"workspaces\":[{\"workspace_id\":\"w1\",\"label\":\"fixture\"}]}}';;\n *) exit 42;;\nesac\n",
    )
    .unwrap();
    fs::set_permissions(&script, fs::Permissions::from_mode(0o700)).unwrap();
    let brief = f.tmp.path().join("brief");
    fs::write(&brief, "do the thing").unwrap();
    let launch = |delivery: Option<&str>| {
        let mut args = vec![
            "session",
            "launch",
            "--space",
            "w1",
            "--name",
            "parity",
            "--account",
            "codex:personal",
            "--model",
            "gpt-6-astra",
            "--brief-file",
            brief.to_str().unwrap(),
            "--directory",
            f.tmp.path().to_str().unwrap(),
            "--herdr-session",
            "fixture",
            "--herdr-socket",
            socket.to_str().unwrap(),
            "--dry-run",
        ];
        if let Some(d) = delivery {
            args.extend(["--brief-delivery", d]);
        }
        run(&f, &args, &[])
    };
    let (ok, v) = launch(Some("initial-prompt"));
    assert!(ok, "{v}");
    assert_eq!(v["result"]["brief"]["delivery"], "first_turn", "{v}");
    assert_eq!(
        v["result"]["brief"]["requested_delivery"], "initial_prompt",
        "{v}"
    );
    // Without the flag nothing was requested.
    let (ok, v) = launch(None);
    assert!(ok, "{v}");
    assert_eq!(v["result"]["brief"]["delivery"], "first_turn", "{v}");
    assert!(v["result"]["brief"]["requested_delivery"].is_null(), "{v}");
    // The Codex size limit still applies.
    fs::write(&brief, "a".repeat(16385)).unwrap();
    let (ok, v) = launch(Some("initial-prompt"));
    assert!(!ok);
    assert_eq!(v["error"]["code"], "INPUT_TOO_LARGE", "{v}");
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn codex_stop_wait_returns_after_the_terminate_job_is_complete() {
    let f = Fixture::new().await;
    f.live(ID, false);
    let (ok, v) = stop(&f, &["--wait"], &[]);
    assert!(ok, "{v}");
    assert_eq!(v["result"]["kind"], "terminate", "{v}");
    assert_eq!(v["result"]["phase"], "complete", "{v}");
    // The journal agrees at the moment the call returned.
    let id = v["result"]["id"].as_str().unwrap();
    let jobs = f.invoke(&["moves"]);
    let job = jobs
        .as_array()
        .unwrap()
        .iter()
        .find(|j| j["id"] == id)
        .unwrap();
    assert_eq!(job["phase"], "complete", "{job}");
    // Already stopped: the same result with or without --wait.
    let (ok, again) = stop(&f, &["--wait"], &[]);
    assert!(ok, "{again}");
    assert_eq!(again["result"]["already_stopped"], true, "{again}");
    assert_eq!(again["result"]["phase"], "complete");
    let (ok, plain) = stop(&f, &[], &[]);
    assert!(ok, "{plain}");
    assert_eq!(plain["result"], again["result"]);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn codex_stop_wait_times_out_and_does_not_disable_auto_start() {
    let f = Fixture::new().await;
    // A busy turn keeps the stop waiting for idle.
    f.live(ID, true);
    auto_start(&f, "start");
    let (ok, v) = stop(
        &f,
        &["--wait", "--disable-auto-start"],
        &[("SESSIONSATION_OPERATION_WAIT_MS", "800")],
    );
    assert!(!ok, "{v}");
    assert_eq!(v["error"]["code"], "OPERATION_TIMEOUT", "{v}");
    assert_eq!(v["error"]["retryable"], true, "{v}");
    assert_eq!(v["error"]["operation"]["kind"], "terminate", "{v}");
    assert_ne!(v["error"]["operation"]["phase"], "complete", "{v}");
    // Not disabled at admission: the stop did not finish.
    assert_eq!(session(&f)["auto_start"]["mode"], "start");
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn codex_stop_wait_disables_auto_start_after_completion() {
    let f = Fixture::new().await;
    f.live(ID, false);
    auto_start(&f, "start");
    let (ok, v) = stop(&f, &["--wait", "--disable-auto-start"], &[]);
    assert!(ok, "{v}");
    assert_eq!(v["result"]["phase"], "complete", "{v}");
    assert_eq!(v["result"]["auto_start"]["mode"], "disabled", "{v}");
    assert_eq!(session(&f)["auto_start"]["mode"], "disabled");
}
