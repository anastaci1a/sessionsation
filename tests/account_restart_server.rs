//! Contract 1.72.3: `api account restart-server ID` restarts a Codex account's shared app-server.
#![allow(
    clippy::disallowed_methods,
    reason = "integration tests are exempt from owned roles (src/guard.rs)"
)]
use futures_util::{SinkExt, StreamExt};
use serde_json::{Value, json};
use sessionsation::{daemon, paths::Paths};
use std::{
    fs,
    os::unix::fs::PermissionsExt,
    process::Stdio,
    sync::{Arc, Mutex},
    time::Duration,
};
use tokio_tungstenite::tungstenite::Message;

/// The account's control socket, served by this test process (so its pid is the "server" pid).
async fn serve(home: std::path::PathBuf, loaded: Arc<Mutex<Vec<String>>>) {
    let path = home.join("app-server-control/app-server-control.sock");
    fs::create_dir_all(path.parent().unwrap()).unwrap();
    let listener = tokio::net::UnixListener::bind(path).unwrap();
    while let Ok((stream, _)) = listener.accept().await {
        let loaded = loaded.clone();
        tokio::spawn(async move {
            let mut ws = tokio_tungstenite::accept_async(stream).await.unwrap();
            while let Some(Ok(Message::Text(raw))) = ws.next().await {
                let v: Value = serde_json::from_str(&raw).unwrap();
                let Some(id) = v.get("id") else { continue };
                let result = match v["method"].as_str().unwrap() {
                    "thread/loaded/list" => {
                        json!({"data":*loaded.lock().unwrap(),"nextCursor":null})
                    }
                    "thread/read" => {
                        json!({"thread":{"id":v["params"]["threadId"],"status":{"type":"idle"}}})
                    }
                    _ => json!({}),
                };
                let _ = ws
                    .send(Message::Text(
                        json!({"id":id,"result":result}).to_string().into(),
                    ))
                    .await;
            }
        });
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 3)]
async fn restart_runs_the_daemon_restart_refuses_loaded_and_claude() {
    let tmp = tempfile::tempdir().unwrap();
    let real = tmp.path().join("real-codex");
    let fake = tmp.path().join("fake-codex");
    let record = tmp.path().join("record");
    fs::write(&real, include_str!("support/fake-codex.sh")).unwrap();
    fs::write(&fake, format!("#!/bin/sh\nif [ \"$2\" = daemon ]; then echo \"$3|$TMPDIR|$TMP|$TEMP|$CODEX_HOME\" >> {r}; exit 0; fi\nexec {c} \"$@\"\n", r = record.display(), c = real.display())).unwrap();
    for f in [&real, &fake] {
        fs::set_permissions(f, fs::Permissions::from_mode(0o700)).unwrap();
    }
    let paths = Paths::new(Some(tmp.path().join("state"))).unwrap();
    sessionsation::store::save_config(
        &paths,
        &sessionsation::config::Config {
            codex_binary: fake.to_str().unwrap().into(),
            ..Default::default()
        },
    )
    .unwrap();
    let mut supervisor = std::process::Command::new(env!("CARGO_BIN_EXE_sessionsation"))
        .arg("--data-dir")
        .arg(&paths.root)
        .args(["daemon", "serve"])
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .spawn()
        .unwrap();
    let deadline = std::time::Instant::now() + Duration::from_secs(10);
    while daemon::call(&paths, "status", json!({})).await.is_err() {
        assert!(std::time::Instant::now() < deadline, "supervisor not ready");
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
    daemon::call(&paths, "account/add", json!({"name":"a"}))
        .await
        .unwrap();
    for _ in 0..200 {
        let v = daemon::call(&paths, "status", json!({})).await.unwrap();
        if v["accounts"][0]["usage_fresh"] == true {
            break;
        }
        tokio::time::sleep(Duration::from_millis(30)).await;
    }
    let home = paths.account("a").unwrap();
    let loaded = Arc::new(Mutex::new(Vec::new()));
    tokio::spawn(serve(home.clone(), loaded.clone()));
    tokio::time::sleep(Duration::from_millis(200)).await;
    let api = |args: &[&str]| {
        let mut command = tokio::process::Command::new(env!("CARGO_BIN_EXE_sessionsation"));
        command
            .arg("--data-dir")
            .arg(&paths.root)
            .arg("api")
            .args(args);
        async move {
            let out = command.output().await.unwrap();
            let text = if out.status.success() {
                out.stdout
            } else {
                out.stderr
            };
            serde_json::from_slice::<Value>(&text).unwrap()
        }
    };
    let ok = api(&["account", "restart-server", "codex:a"]).await;
    let me = std::process::id();
    assert_eq!(ok["ok"], true, "{ok}");
    let r = &ok["result"];
    assert_eq!(r["account_id"], "codex:a");
    assert_eq!(r["restarted"], true);
    assert_eq!(r["pid"], me);
    assert_eq!(
        r["process_start"],
        fs::read_to_string("/proc/self/stat")
            .unwrap()
            .rsplit_once(") ")
            .unwrap()
            .1
            .split(' ')
            .nth(19)
            .unwrap()
            .parse::<u64>()
            .unwrap()
    );
    let dir = paths.root.join("tmp/codex/a");
    let d = dir.display();
    let want = format!("restart|{d}|{d}|{d}|{}", home.display());
    assert_eq!(
        fs::read_to_string(&record)
            .unwrap()
            .lines()
            .collect::<Vec<_>>(),
        [want.as_str()]
    );

    loaded.lock().unwrap().push("t1".into());
    let busy = api(&["account", "restart-server", "codex:a"]).await;
    assert_eq!(
        busy["error"]["code"], "SESSION_OPERATION_IN_PROGRESS",
        "{busy}"
    );
    assert_eq!(
        fs::read_to_string(&record).unwrap().lines().count(),
        1,
        "a refusal restarts nothing"
    );

    let claude = api(&["account", "restart-server", "claude:x"]).await;
    assert_eq!(
        claude["error"]["code"], "UNSUPPORTED_PROVIDER_OPERATION",
        "{claude}"
    );
    assert_eq!(claude["error"]["supported"], false);
    let caps = api(&["capabilities"]).await;
    let ops = |id: &str| {
        caps["result"]["providers"]
            .as_array()
            .unwrap()
            .iter()
            .find(|p| p["id"] == id)
            .unwrap()["operations"]["account_restart_server"]["supported"]
            .clone()
    };
    assert_eq!((ops("codex"), ops("claude")), (json!(true), json!(false)));
    daemon::call(&paths, "status", json!({}))
        .await
        .expect("supervisor survives");
    let _ = unsafe { libc::kill(supervisor.id() as i32, libc::SIGTERM) };
    let _ = supervisor.wait();
}
