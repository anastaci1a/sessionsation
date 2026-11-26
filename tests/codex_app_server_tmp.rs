//! Contract 1.72.2: the Codex app-server start command carries the session temp variables.
#![allow(
    clippy::disallowed_methods,
    reason = "integration tests are exempt from owned roles (src/guard.rs)"
)]
use serde_json::json;
use sessionsation::{daemon, paths::Paths};
use std::{
    fs,
    os::unix::fs::PermissionsExt,
    process::{Command, Stdio},
    time::Duration,
};

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn app_server_start_gets_the_temp_env_none_passes_none_unsafe_refuses() {
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
    let save = |root: Option<&str>| {
        sessionsation::store::save_config(
            &paths,
            &sessionsation::config::Config {
                codex_binary: fake.to_str().unwrap().into(),
                session_tmp_root: root.map(Into::into),
                ..Default::default()
            },
        )
        .unwrap();
    };
    save(None);
    let mut server = Command::new(env!("CARGO_BIN_EXE_sessionsation"))
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
    let home = paths.account("a").unwrap();
    let prepare = || daemon::call(&paths, "account/prepare", json!({"name":"a"}));
    // The fake starts no socket, so the call fails after the start command ran.
    for _ in 0..200 {
        let v = daemon::call(&paths, "status", json!({})).await.unwrap();
        if v["accounts"][0]["usage_fresh"] == true {
            break;
        }
        tokio::time::sleep(Duration::from_millis(30)).await;
    }
    let _ = prepare().await;
    let dir = paths.root.join("tmp/codex/a");
    let d = dir.display();
    let want = format!("start|{d}|{d}|{d}|{}", home.display());
    assert_eq!(
        fs::read_to_string(&record)
            .unwrap()
            .lines()
            .collect::<Vec<_>>(),
        [want.as_str()]
    );
    save(Some("none"));
    let _ = prepare().await;
    assert_eq!(
        fs::read_to_string(&record).unwrap().lines().last().unwrap(),
        format!("start||||{}", home.display())
    );
    save(None);
    fs::remove_dir_all(&dir).unwrap();
    let outside = tmp.path().join("outside");
    fs::create_dir(&outside).unwrap();
    std::os::unix::fs::symlink(&outside, &dir).unwrap();
    let before = fs::read_to_string(&record).unwrap();
    let err = format!("{:#}", prepare().await.unwrap_err());
    assert!(err.contains("SESSION_TMP_UNSAFE"), "{err}");
    assert_eq!(
        fs::read_to_string(&record).unwrap(),
        before,
        "an unsafe directory must not start the server"
    );
    daemon::call(&paths, "status", json!({}))
        .await
        .expect("supervisor survives the refusal");
    let _ = unsafe { libc::kill(server.id() as i32, libc::SIGTERM) };
    let _ = server.wait();
}
