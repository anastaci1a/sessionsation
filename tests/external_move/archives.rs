//! Public CLI archives are compressed, account independent, inactive-only and recoverable.
use super::*;
fn api(f: &Fixture, args: &[&str]) -> Value {
    let mut all = vec!["api"];
    all.extend(args);
    f.invoke(&all)["result"].clone()
}
fn target(f: &Fixture) -> Value {
    api(f, &["state"])["sessions"]
        .as_array()
        .unwrap()
        .iter()
        .find(|s| s["conversation_id"] == ID)
        .unwrap()
        .clone()
}
fn start(f: &Fixture, kind: &str, s: &Value, request: &str) -> Value {
    api(
        f,
        &[
            kind,
            "--session",
            s["id"].as_str().unwrap(),
            "--expected-generation",
            s["generation"].as_str().unwrap(),
            "--request-id",
            request,
            "--confirm",
            s["id"].as_str().unwrap(),
        ],
    )
}
async fn wait(f: &Fixture, id: &str) -> Value {
    let until = Instant::now() + Duration::from_secs(20);
    loop {
        let ops = api(f, &["operations"]);
        let op = ops
            .as_array()
            .unwrap()
            .iter()
            .find(|o| o["id"] == id)
            .unwrap()
            .clone();
        if matches!(op["phase"].as_str(), Some("complete" | "failed")) {
            return op;
        }
        assert!(Instant::now() < until, "{op}");
        tokio::time::sleep(Duration::from_millis(30)).await;
    }
}
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn public_archive_restore_and_delete_preserve_identity_and_other_data() {
    let f = Fixture::new().await;
    let project = f.tmp.path().join("project");
    fs::create_dir(&project).unwrap();
    fs::write(project.join("keep"), "project data").unwrap();
    let original = history(&f.paths.account("personal").unwrap(), &project, ID);
    let bytes = fs::read(&original).unwrap();
    let unrelated = history(
        &f.paths.account("work").unwrap(),
        &project,
        "22222222-2222-4222-8222-222222222222",
    );
    let s = target(&f);
    let rid = "aaaaaaaa-aaaa-4aaa-8aaa-aaaaaaaaaaaa";
    let submitted = start(&f, "archive", &s, rid);
    assert_eq!(submitted["phase"], "queued");
    assert_eq!(start(&f, "archive", &s, rid)["id"], rid);
    let result = wait(&f, rid).await;
    assert_eq!(result["phase"], "complete", "{result}");
    assert!(!original.exists());
    assert!(unrelated.exists());
    assert!(project.join("keep").exists());
    assert!(
        f.paths
            .account("personal")
            .unwrap()
            .join("auth.json")
            .exists()
    );
    assert!(
        !api(&f, &["state"])["sessions"]
            .as_array()
            .unwrap()
            .iter()
            .any(|i| i["id"] == s["id"])
    );
    let list = api(&f, &["archives"]);
    let archived = list[0].clone();
    assert_eq!(archived["id"], s["id"]);
    assert!(archived["compressed_bytes"].as_u64().unwrap() > 0);
    assert_ne!(archived["generation"], s["generation"]);
    let dir = f
        .paths
        .root
        .join("archived-sessions")
        .join(s["id"].as_str().unwrap());
    let compressed = dir.join("bundles").join(rid).join("0.jsonl.gz");
    assert_eq!(&fs::read(&compressed).unwrap()[..2], &[31, 139]);
    assert!(!dir.starts_with(f.paths.root.join("accounts")));
    let restore_id = "bbbbbbbb-bbbb-4bbb-8bbb-bbbbbbbbbbbb";
    start(&f, "restore-session", &archived, restore_id);
    let result = wait(&f, restore_id).await;
    assert_eq!(result["phase"], "complete", "{result}");
    assert!(api(&f, &["archives"]).as_array().unwrap().is_empty());
    let restored = target(&f);
    assert_eq!(restored["id"], s["id"]);
    assert_eq!(restored["conversation_id"], ID);
    assert_eq!(restored["availability"], "saved");
    assert_eq!(
        fs::read(
            dir.join("home").join(
                original
                    .strip_prefix(f.paths.account("personal").unwrap())
                    .unwrap()
            )
        )
        .unwrap(),
        bytes
    );
    let again = "14141414-1414-4414-8414-141414141414";
    start(&f, "archive", &restored, again);
    let rearchive_result = wait(&f, again).await;
    assert_eq!(rearchive_result["phase"], "complete", "{rearchive_result}");
    let rearchived = api(&f, &["archives"])[0].clone();
    let restore_again = "15151515-1515-4515-8515-151515151515";
    start(&f, "restore-session", &rearchived, restore_again);
    let restore_result = wait(&f, restore_again).await;
    assert_eq!(restore_result["phase"], "complete", "{restore_result}");
    let restored = target(&f);
    assert_eq!(restored["id"], s["id"]);
    let delete_id = "cccccccc-cccc-4ccc-8ccc-cccccccccccc";
    start(&f, "delete-session", &restored, delete_id);
    let result = wait(&f, delete_id).await;
    assert_eq!(result["phase"], "complete", "{result}");
    assert!(!compressed.exists());
    assert!(!dir.join("home").exists());
    assert!(unrelated.exists());
    assert!(project.join("keep").exists());
    assert!(
        f.paths
            .account("personal")
            .unwrap()
            .join("auth.json")
            .exists()
    );
    for server in [&f.a, &f.b] {
        assert!(
            !server.lock().unwrap().calls.iter().any(|(m, _)| matches!(
                m.as_str(),
                "turn/start" | "thread/start" | "thread/resume"
            ))
        );
    }
}
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn public_archive_rejects_live_stale_and_unconfirmed_targets_before_export() {
    let f = Fixture::new().await;
    let pane = f.live(ID, false);
    tokio::time::sleep(Duration::from_millis(100)).await;
    let s = target(&f);
    let id = s["id"].as_str().unwrap();
    let generation = s["generation"].as_str().unwrap();
    for (target_generation, confirm) in [(generation, id), ("stale", id), (generation, "wrong")] {
        let out = command(
            &f.paths.root,
            &f.socket,
            &[
                "api",
                "archive",
                "--session",
                id,
                "--expected-generation",
                target_generation,
                "--request-id",
                "dddddddd-dddd-4ddd-8ddd-dddddddddddd",
                "--confirm",
                confirm,
            ],
        );
        assert!(!out.status.success());
    }
    assert!(api(&f, &["operations"]).as_array().unwrap().is_empty());
    assert!(api(&f, &["archives"]).as_array().unwrap().is_empty());
    assert!(
        !tmux(
            &f.socket,
            &["display-message", "-p", "-t", &pane, "#{pane_pid}"]
        )
        .is_empty()
    );
    assert!(
        !f.a.lock()
            .unwrap()
            .calls
            .iter()
            .any(|(m, _)| m == "thread/delete")
    );
}
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn public_archive_exports_paginated_proofs_and_rejects_changed_compressed_data() {
    let f = Fixture::new().await;
    let path = history(&f.paths.account("personal").unwrap(), f.tmp.path(), ID);
    let text = fs::read_to_string(&path).unwrap();
    let mut rows = text
        .lines()
        .map(|l| serde_json::from_str::<Value>(l).unwrap())
        .collect::<Vec<_>>();
    rows[0]["payload"]["history_mode"] = json!("paginated");
    fs::write(
        &path,
        rows.iter()
            .map(Value::to_string)
            .collect::<Vec<_>>()
            .join("\n")
            + "\n",
    )
    .unwrap();
    let s = target(&f);
    let rid = "eeeeeeee-eeee-4eee-8eee-eeeeeeeeeeee";
    start(&f, "archive", &s, rid);
    let result = wait(&f, rid).await;
    assert_eq!(result["phase"], "complete", "{result}");
    let archived = api(&f, &["archives"])[0].clone();
    let dir = f
        .paths
        .root
        .join("archived-sessions")
        .join(s["id"].as_str().unwrap());
    let bundle = dir.join("bundles").join(rid);
    let proof: Value =
        serde_json::from_slice(&fs::read(bundle.join("history-checks.json")).unwrap()).unwrap();
    assert_eq!(proof[0]["native"]["turns"], 70);
    assert_eq!(proof[0]["native"]["items"], 270);
    let compressed = bundle.join("0.jsonl.gz");
    let original = fs::read(&compressed).unwrap();
    fs::write(&compressed, b"corrupt").unwrap();
    let restore = "ffffffff-ffff-4fff-8fff-ffffffffffff";
    start(&f, "restore-session", &archived, restore);
    let result = wait(&f, restore).await;
    assert_eq!(result["phase"], "failed");
    assert!(
        result["detail"]
            .as_str()
            .unwrap()
            .contains("ARCHIVE_CHANGED")
    );
    assert_eq!(api(&f, &["archives"])[0]["id"], s["id"]);
    fs::write(compressed, original).unwrap();
}
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn public_restore_rejects_loaded_threads_and_never_replays_failed_requests() {
    let f = Fixture::new().await;
    let original = history(&f.paths.account("personal").unwrap(), f.tmp.path(), ID);
    let loaded = thread_value(&original, ID, "idle");
    let source = target(&f);
    let archive_id = "12121212-1212-4212-8212-121212121212";
    start(&f, "archive", &source, archive_id);
    assert_eq!(wait(&f, archive_id).await["phase"], "complete");
    let archived = api(&f, &["archives"])[0].clone();
    f.a.lock().unwrap().threads.insert(ID.into(), loaded);
    let restore_id = "13131313-1313-4313-8313-131313131313";
    start(&f, "restore-session", &archived, restore_id);
    let failed = wait(&f, restore_id).await;
    assert_eq!(failed["phase"], "failed", "{failed}");
    assert!(
        failed["detail"]
            .as_str()
            .unwrap()
            .contains("HISTORY_THREAD_LOADED")
    );
    let storage = f
        .paths
        .root
        .join("archived-sessions")
        .join(source["id"].as_str().unwrap());
    assert!(!storage.join("home").exists());
    assert!(
        storage
            .join("bundles")
            .join(archive_id)
            .join("0.jsonl.gz")
            .exists()
    );
    f.a.lock().unwrap().threads.clear();
    assert_eq!(
        start(&f, "restore-session", &archived, restore_id)["phase"],
        "failed"
    );
    assert_eq!(api(&f, &["archives"])[0]["id"], source["id"]);
    assert!(!storage.join("home").exists());
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn external_inactive_history_can_archive_and_delete_without_enrollment() {
    let f = Fixture::new().await;
    let home = f.tmp.path().join("unenrolled-history");
    let original = history(&home, f.tmp.path(), ID);
    let state = Arc::new(Mutex::new(Server::default()));
    state
        .lock()
        .unwrap()
        .threads
        .insert(ID.into(), thread_value(&original, ID, "notLoaded"));
    let server = tokio::spawn(server(home.clone(), state.clone()));
    for _ in 0..100 {
        if home
            .join("app-server-control/app-server-control.sock")
            .exists()
        {
            break;
        }
        tokio::time::sleep(Duration::from_millis(10)).await;
    }
    let external = |args: &[&str]| {
        let out = Command::new(binary())
            .arg("--data-dir")
            .arg(&f.paths.root)
            .arg("api")
            .args(args)
            .env("CODEX_HOME", &home)
            .env("TMUX", format!("{},1,0", f.socket))
            .output()
            .unwrap();
        assert!(
            out.status.success(),
            "{}",
            String::from_utf8_lossy(&out.stderr)
        );
        serde_json::from_slice::<Value>(&out.stdout).unwrap()["result"].clone()
    };
    let snapshot = external(&["state"]);
    let saved = snapshot["sessions"]
        .as_array()
        .unwrap()
        .iter()
        .find(|s| s["conversation_id"] == ID)
        .unwrap();
    assert_eq!(saved["account_id"], "codex:external");
    assert_eq!(saved["capabilities"]["archive"], true);
    assert_eq!(saved["capabilities"]["delete"], true);
    assert_eq!(saved["capabilities"]["resume"], false);
    let request = "99999999-9999-4999-8999-999999999999";
    external(&[
        "archive",
        "--session",
        saved["id"].as_str().unwrap(),
        "--expected-generation",
        saved["generation"].as_str().unwrap(),
        "--request-id",
        request,
        "--confirm",
        saved["id"].as_str().unwrap(),
    ]);
    let result = wait(&f, request).await;
    assert_eq!(result["phase"], "complete", "{result}");
    assert!(!original.exists());
    let archived = api(&f, &["archives"])[0].clone();
    let request = "88888888-8888-4888-8888-888888888888";
    start(&f, "delete-session", &archived, request);
    let result = wait(&f, request).await;
    assert_eq!(result["phase"], "complete", "{result}");
    assert!(api(&f, &["archives"]).as_array().unwrap().is_empty());
    assert!(
        !home.join("auth.json").exists(),
        "no account enrollment or credential creation"
    );
    assert!(
        !state
            .lock()
            .unwrap()
            .calls
            .iter()
            .any(|(m, _)| matches!(m.as_str(), "turn/start" | "turn/steer" | "thread/resume"))
    );
    server.abort();
}
