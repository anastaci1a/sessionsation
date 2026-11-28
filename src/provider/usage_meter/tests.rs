use super::*;

#[test]
fn every_provider_has_the_same_meter_keys_and_nothing_is_invented() {
    let now = 1_000_000;
    let codex = json!({"short":{"used_percent":23.0,"duration_minutes":300,"resets_at":now+100},
        "weekly":{"used_percent":42.0,"duration_minutes":10080,"resets_at":now+1000},"observed_at":now-10});
    let claude = json!({"short":{"used_percent":31.0,"resets_at":now+100,"observed_at":now-50,"source":"oauth"},
        "weekly":{"used_percent":56.5,"resets_at":now+1000,"observed_at":now-5,"source":"statusline"},
        "observed_at":now-5});
    let keys = |v: &Value| {
        let mut k: Vec<String> = v.as_object().unwrap().keys().cloned().collect();
        k.sort();
        k
    };
    let rows = [
        of(ProviderId::Codex, &codex, now, 300),
        of(ProviderId::Claude, &claude, now, 300),
        of(ProviderId::Gemini, &json!({}), now, 300),
        of(ProviderId::Antigravity, &json!({}), now, 300),
        of(
            ProviderId::Claude,
            &json!({"short":null,"weekly":null,"observed_at":null}),
            now,
            300,
        ),
    ];
    for r in &rows {
        assert_eq!(keys(r), keys(&rows[0]));
    }
    let c = &rows[0];
    assert_eq!(c["source"], "app_server");
    assert_eq!(c["requires_session"], false);
    assert_eq!(c["fresh"], true);
    assert_eq!(c["age_seconds"], 10);
    assert_eq!(c["windows"]["five_hour"]["used_percent"], 23.0);
    let c = &rows[1];
    assert_eq!(c["source"], "statusline", "the newest window's source");
    assert_eq!(c["observed_at"], now - 5);
    assert_eq!(c["windows"]["five_hour"]["source"], "oauth");
    assert_eq!(rows[2]["supported"], false);
    assert!(rows[2]["reason"].is_string());
    assert_eq!(rows[4]["fresh"], false);
    assert!(rows[4]["windows"]["five_hour"].is_null());
    assert!(rows[4]["source"].is_null());
    // Past a reset, or too old: not fresh.
    assert_eq!(
        of(ProviderId::Codex, &codex, now + 200, 300)["fresh"],
        false
    );
    assert_eq!(of(ProviderId::Codex, &codex, now + 50, 30)["fresh"], false);
}

#[test]
fn each_window_has_its_own_freshness_and_age_from_its_own_observed_at() {
    // Fake clock: Claude's five_hour is from the statusline just now, seven_day from an
    // OAuth read 256 s earlier (the live lag): same bound, different ages.
    let now = 1_000_000;
    let usage = json!({"short":{"used_percent":31.0,"resets_at":now+100,"observed_at":now-3,"source":"statusline"},
        "weekly":{"used_percent":56.5,"resets_at":now+1000,"observed_at":now-256,"source":"oauth"},
        "observed_at":now-3});
    let m = of(ProviderId::Claude, &usage, now, 300);
    let (five, seven) = (&m["windows"]["five_hour"], &m["windows"]["seven_day"]);
    assert_eq!(
        (five["age_seconds"].as_i64(), seven["age_seconds"].as_i64()),
        (Some(3), Some(256))
    );
    assert_eq!(
        (five["fresh"].as_bool(), seven["fresh"].as_bool()),
        (Some(true), Some(true))
    );
    assert_eq!(
        (m["fresh"].as_bool(), m["all_windows_fresh"].as_bool()),
        (Some(true), Some(true))
    );
    assert_eq!(m["age_seconds"], 3, "meter.fresh stays the newest reading");
    // The weekly read ages past the bound while the statusline keeps the meter fresh.
    let later = now + claude::USAGE_STALE_SECONDS;
    let usage = json!({"short":{"used_percent":31.0,"resets_at":later+100,"observed_at":later-3,"source":"statusline"},
        "weekly":{"used_percent":56.5,"resets_at":later+1000,"observed_at":now-256,"source":"oauth"},
        "observed_at":later-3});
    let m = of(ProviderId::Claude, &usage, later, 300);
    assert_eq!(m["windows"]["five_hour"]["fresh"], true);
    assert_eq!(m["windows"]["seven_day"]["fresh"], false);
    assert_eq!(
        m["windows"]["seven_day"]["age_seconds"],
        claude::USAGE_STALE_SECONDS + 256
    );
    assert_eq!(
        (m["fresh"].as_bool(), m["all_windows_fresh"].as_bool()),
        (Some(true), Some(false))
    );
    // A window past its own reset is not fresh and says why; the other one is unaffected.
    let usage = json!({"short":{"used_percent":31.0,"resets_at":now-1,"observed_at":now-3,"source":"statusline"},
        "weekly":{"used_percent":56.5,"resets_at":now+1000,"observed_at":now-4,"source":"oauth"},
        "observed_at":now-3});
    let m = of(ProviderId::Claude, &usage, now, 300);
    assert_eq!(m["windows"]["five_hour"]["fresh"], false);
    assert_eq!(m["windows"]["seven_day"]["fresh"], true);
    assert_eq!(m["window_reasons"]["five_hour"], reset_reason(now - 1));
    assert_eq!(m["all_windows_fresh"], false);
    // Unsupported and absent windows are never fresh.
    assert_eq!(
        of(ProviderId::Gemini, &json!({}), now, 300)["all_windows_fresh"],
        false
    );
    let none = of(
        ProviderId::Claude,
        &json!({"short":null,"weekly":null}),
        now,
        300,
    );
    assert_eq!(none["all_windows_fresh"], false);
    assert!(none["windows"]["five_hour"].is_null());
}

#[test]
fn upstream_identity_is_a_provider_scoped_fingerprint_or_null() {
    let dir = tempfile::tempdir().unwrap();
    let (codex, claude) = (dir.path().join("codex"), dir.path().join("claude"));
    std::fs::create_dir_all(&codex).unwrap();
    std::fs::create_dir_all(&claude).unwrap();
    let id = |p, h: &std::path::Path| upstream_identity(p, h);
    assert!(id(ProviderId::Codex, &codex).is_null(), "no auth.json");
    std::fs::write(codex.join("auth.json"), "{").unwrap();
    assert!(id(ProviderId::Codex, &codex).is_null(), "invalid auth.json");
    std::fs::write(
        codex.join("auth.json"),
        r#"{"tokens":{"account_id":"fixture-upstream"}}"#,
    )
    .unwrap();
    let uuid = "0b7c1e2a-4d5f-4a6b-8c9d-1e2f3a4b5c6d";
    std::fs::write(
        claude.join(".claude.json"),
        json!({"oauthAccount":{"accountUuid":uuid}}).to_string(),
    )
    .unwrap();
    use sha2::{Digest, Sha256};
    let want = |s: &str| json!(format!("{:x}", Sha256::digest(s)));
    assert_eq!(
        id(ProviderId::Codex, &codex),
        want("codex:fixture-upstream")
    );
    assert_eq!(
        id(ProviderId::Claude, &claude),
        want(&format!("claude:{uuid}"))
    );
    for p in [ProviderId::Gemini, ProviderId::Antigravity] {
        assert!(id(p, &codex).is_null());
    }
}

/// Contract 1.73.6 (review): a Codex workspace `account_id` is narrowed to the id_token's
/// user (`chatgpt_user_id`, else `sub`); a rewritten file is read again (mtime/size cache).
#[test]
fn codex_upstream_identity_names_the_user_within_a_workspace() {
    use sha2::{Digest, Sha256};
    let want = |s: &str| json!(format!("{:x}", Sha256::digest(s)));
    // Base64url payloads (no padding) of fixture claims; header and signature are ignored.
    const USER1: &str = "eyJzdWIiOiJGSVhUVVJFLVNVQi0xIiwiaHR0cHM6Ly9hcGkub3BlbmFpLmNvbS9hdXRoIjp7ImNoYXRncHRfdXNlcl9pZCI6IkZJWFRVUkUtVVNFUi0xIn19";
    const USER2: &str = "eyJzdWIiOiJGSVhUVVJFLVNVQi0yIiwiaHR0cHM6Ly9hcGkub3BlbmFpLmNvbS9hdXRoIjp7ImNoYXRncHRfdXNlcl9pZCI6IkZJWFRVUkUtVVNFUi0yIn19";
    const SUB_ONLY: &str = "eyJzdWIiOiJGSVhUVVJFLVNVQi0zIn0";
    let dir = tempfile::tempdir().unwrap();
    let write = |token: Option<&str>| {
        let mut tokens = json!({"account_id":"FIXTURE-WORKSPACE"});
        if let Some(t) = token {
            tokens["id_token"] = json!(t);
        }
        std::fs::write(
            dir.path().join("auth.json"),
            json!({"tokens":tokens}).to_string(),
        )
        .unwrap();
        upstream_identity(ProviderId::Codex, dir.path())
    };
    let seat1 = write(Some(&format!("h.{USER1}.s")));
    assert_eq!(seat1, want("codex:FIXTURE-WORKSPACE:FIXTURE-USER-1"));
    let seat2 = write(Some(&format!("h.{USER2}.sig")));
    assert_eq!(seat2, want("codex:FIXTURE-WORKSPACE:FIXTURE-USER-2"));
    assert_ne!(seat1, seat2, "two seats of one workspace");
    assert_eq!(
        write(Some(&format!("hd.{SUB_ONLY}.s"))),
        want("codex:FIXTURE-WORKSPACE:FIXTURE-SUB-3"),
        "sub without chatgpt_user_id"
    );
    assert_eq!(write(None), want("codex:FIXTURE-WORKSPACE"), "no id_token");
    assert_eq!(
        write(Some("not-a-jwt!")),
        want("codex:FIXTURE-WORKSPACE"),
        "an undecodable id_token falls back to the account"
    );
}

/// Contract 1.73.9: a cache miss reads with the lock released (another file is served meanwhile).
#[test]
fn upstream_identity_miss_reads_outside_the_cache_lock() {
    let stamp = (None, 1, 1, 0, 0);
    let (tx, rx) = std::sync::mpsc::channel();
    let slow = fingerprint_cached("/fixture/slow".into(), stamp, || {
        std::thread::spawn(move || {
            tx.send(fingerprint_cached("/fixture/other".into(), stamp, || {
                json!("b")
            }))
        });
        rx.recv_timeout(std::time::Duration::from_secs(10))
            .expect("another lookup waited on the slow read")
    });
    assert_eq!(slow, json!("b"));
}

/// Contract 1.73.9: a file atomically replaced at the same mtime and length is read again.
#[test]
fn upstream_identity_rereads_a_replaced_file_with_the_same_mtime_and_len() {
    use sha2::{Digest, Sha256};
    let dir = tempfile::tempdir().unwrap();
    let at = std::time::SystemTime::UNIX_EPOCH + std::time::Duration::from_secs(1_800_000_000);
    let put = |name: &str, account: &str| {
        let path = dir.path().join(name);
        std::fs::write(&path, json!({"tokens":{"account_id":account}}).to_string()).unwrap();
        std::fs::File::options()
            .write(true)
            .open(&path)
            .unwrap()
            .set_modified(at)
            .unwrap();
        path
    };
    put("auth.json", "FIXTURE-A");
    let first = upstream_identity(ProviderId::Codex, dir.path());
    assert_eq!(
        first,
        json!(format!("{:x}", Sha256::digest("codex:FIXTURE-A")))
    );
    std::fs::rename(
        put("auth.json.tmp", "FIXTURE-B"),
        dir.path().join("auth.json"),
    )
    .unwrap();
    assert_eq!(
        upstream_identity(ProviderId::Codex, dir.path()),
        json!(format!("{:x}", Sha256::digest("codex:FIXTURE-B")))
    );
}
