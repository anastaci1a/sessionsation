//! S3 compatibility table: every kind moved onto the engine keeps its directory, file name and
//! serde shape. A record as the pre-engine code wrote it (pretty JSON in struct order; the
//! structs are unchanged) loads, and the engine writes it back byte for byte, so old binaries
//! read new records. Plus the lock semantics: one writer under the directory lock, the
//! `<id>.worker.lock` worker lock and stale-worker detection.
use super::*;
use crate::messaging::{compaction, model_change};
use serde_json::{Value, json};

const ID: &str = "0b6f8e47-3f1c-4c55-9d1a-7f3e2a1b9c10";

fn root() -> (tempfile::TempDir, Paths) {
    let tmp = tempfile::tempdir().unwrap();
    let paths = Paths::new(Some(tmp.path().join("state"))).unwrap();
    (tmp, paths)
}

/// `fixture` stored where the old code put it loads, and is written back unchanged.
fn round_trip<R: Journal>(dir: &str, fixture: &str) {
    let (_tmp, paths) = root();
    assert_eq!(R::DIR, dir, "directory");
    let bytes = pretty(&fixture.replace("$ID", ID)).into_bytes();
    let file = paths.root.join(dir).join(format!("{ID}.json"));
    assert_eq!(R::file(&paths, ID).unwrap(), file, "file name");
    crate::paths::atomic_write(&file, &bytes).unwrap();
    assert!(R::exists(&paths, ID));
    let r = R::read(&paths, ID).unwrap();
    assert_eq!(R::list(&paths).unwrap().len(), 1);
    fs::remove_file(&file).unwrap();
    r.write(&paths).unwrap();
    assert_eq!(
        String::from_utf8(fs::read(&file).unwrap()).unwrap(),
        String::from_utf8(bytes).unwrap(),
        "{dir}: not byte-equal"
    );
    assert_eq!(
        R::worker_lock(&paths, ID).unwrap(),
        paths.root.join(dir).join(format!("{ID}.worker.lock"))
    );
}

/// Pretty JSON (`serde_json::to_vec_pretty` layout) of compact JSON text, keeping key order.
fn pretty(compact: &str) -> String {
    let (mut out, mut depth, mut in_str, mut esc) = (String::new(), 0usize, false, false);
    let indent = |out: &mut String, d: usize| {
        out.push('\n');
        out.push_str(&"  ".repeat(d));
    };
    let chars: Vec<char> = compact.chars().collect();
    let mut i = 0;
    while i < chars.len() {
        let c = chars[i];
        if in_str {
            out.push(c);
            (esc, in_str) = (c == '\\' && !esc, c != '"' || esc);
        } else {
            match c {
                '"' => {
                    in_str = true;
                    out.push(c);
                }
                '{' | '[' if matches!(chars.get(i + 1), Some('}' | ']')) => {
                    out.push(c);
                    out.push(chars[i + 1]);
                    i += 1;
                }
                '{' | '[' => {
                    out.push(c);
                    depth += 1;
                    indent(&mut out, depth);
                }
                '}' | ']' => {
                    depth -= 1;
                    indent(&mut out, depth);
                    out.push(c);
                }
                ',' => {
                    out.push(c);
                    indent(&mut out, depth);
                }
                ':' => out.push_str(": "),
                c if c.is_whitespace() => {}
                c => out.push(c),
            }
        }
        i += 1;
    }
    out
}

#[test]
fn every_migrated_kind_keeps_its_file_and_shape() {
    round_trip::<crate::reset_credit::Record>(
        "operations/account-reset-credits",
        r#"{"schema_version":1,"id":"$ID","kind":"account_reset_credit","provider":"codex",
            "account_id":"codex:a","credit_id":null,"idempotency_key":"$ID","phase":"uncertain",
            "outcome":null,"attempts":2,"detail":"no answer","codex_version":"0.165.0",
            "created_at":1,"updated_at":2}"#,
    );
    round_trip::<crate::account_deletion::Record>(
        "operations/account-deletions",
        r#"{"id":"$ID","account_id":"claude:a","kind":"account_delete","phase":"complete",
            "detail":"done","created_at":1,"updated_at":2,"history_home":"/r/preserved/home",
            "source_inode":7,"can_cancel":false,"can_recover":false,"history_preserved":true,
            "home_disposition":"deleted"}"#,
    );
    round_trip::<crate::auto_recover::Record>(
        "operations/auto-recoveries",
        r#"{"schema_version":1,"id":"$ID","kind":"auto_recover","trigger":"crash",
            "session_id":"s","conversation_id":"c","provider":"claude","account":"a","name":"n",
            "herdr_session":"default","recorded_pane":"herdr/default/w1:p1","workspace_label":null,
            "directory":"/w","pane":null,"pane_reused":null,"mode":"start_prompt","prompt":"p",
            "message_id":"m","prompt_state":"pending","prompt_attempts":0,"prompt_detail":"",
            "resume":"dispatching","phase":"uncertain","detail":"d","uncertain_from":"resuming",
            "generation_after":null,"created_at":1,"updated_at":2}"#,
    );
    round_trip::<compaction::Record>(
        "operations/compactions",
        r#"{"schema_version":1,"id":"$ID","session_id":"s","accepted_generation":"g",
            "conversation_id":"c","runtime_generation":"rg","phase":"running","reason":null,
            "created_at":1,"updated_at":2,"expires_at":3,"started_at":2,"finished_at":null,
            "attempts":1,"baseline":["t1"],"compaction_turn_id":null,"hold_released":false,
            "release_claimed_at":null,"provider":"codex","account":"a"}"#,
    );
    round_trip::<crate::account_rename::Record>(
        "operations/account-renames",
        r#"{"schema_version":1,"id":"$ID","kind":"account_rename","provider":"codex",
            "account_id":"codex:a","new_account_id":"codex:b","old_name":"a","new_name":"b",
            "old_home":"/r/accounts/codex/a","new_home":"/r/accounts/codex/b",
            "display_name_before":null,"phase":"moving_home","detail":"d","created_at":1,
            "updated_at":2}"#,
    );
    round_trip::<crate::claude_idle::Record>(
        "operations/claude-idle-notify",
        r#"{"schema_version":1,"id":"$ID","session_id":"s","conversation_id":"c","account":"a",
            "name":"n","pid":7,"process_start":9,"socket":"/r/s.sock","notify_msg_id":"m",
            "notify_session":null,"phase":"subscribed","state":"waiting","reason":null,
            "reply_socket":"/r/reply.sock","event_seq":3,"created_at":1,"updated_at":2,
            "expires_at":5}"#,
    );
    let observed = r#"{"model":"gpt-6","effort":"high","observed_at":1,"source":"codex_thread"}"#;
    round_trip::<model_change::Record>(
        "operations/model-changes",
        &r#"{"schema_version":1,"id":"$ID","session_id":"s","accepted_generation":"g",
            "conversation_id":"c","provider":"codex","account":"a","when":"idle","runtime":"rg",
            "requested_model":"gpt-6-astra","requested_effort":null,"effort_source":"preserved",
            "before":$OBS,"observed":$OBS,"applied":null,"phase":"applying","reason":null,
            "detail":null,"step":"model","steps":["model"],"attempts":1,"created_at":1,
            "updated_at":2,"expires_at":3,"started_at":2,"finished_at":null,"hold_released":false,
            "release_claimed_at":null}"#
            .replace("$OBS", observed),
    );
}

#[test]
fn records_from_before_later_fields_still_load() {
    // A compaction from before 1.64.0 (no provider, account or release fields) is a Codex one.
    let (_tmp, paths) = root();
    let old = json!({"schema_version":1,"id":ID,"session_id":"s","accepted_generation":"g",
        "conversation_id":"c","runtime_generation":"rg","phase":"complete","reason":"completed",
        "created_at":1,"updated_at":2,"expires_at":3,"started_at":2,"finished_at":3,"attempts":1,
        "baseline":[],"compaction_turn_id":"t"});
    let file = compaction::Record::file(&paths, ID).unwrap();
    crate::paths::atomic_write(&file, &serde_json::to_vec_pretty(&old).unwrap()).unwrap();
    compaction::Record::read(&paths, ID)
        .unwrap()
        .write(&paths)
        .unwrap();
    let new: Value = serde_json::from_slice(&fs::read(&file).unwrap()).unwrap();
    for (k, v) in old.as_object().unwrap() {
        assert_eq!(&new[k], v, "{k}");
    }
    assert_eq!(
        (&new["provider"], &new["hold_released"]),
        (&json!("codex"), &json!(false))
    );
}

#[test]
fn questions_receipts_stay_compact_json() {
    let (_tmp, paths) = root();
    let file = paths.root.join("messaging/questions/answers/s/r.json");
    let receipt = json!({"session_id":"s","request_id":"r","kind":"codex_request_user_input",
        "answers_digest":"d","state":"answered","reason":null,"created_at":1,"updated_at":2,
        "message_request":null,"native_request_id":7});
    store(&file, &receipt).unwrap();
    assert_eq!(
        fs::read(&file).unwrap(),
        serde_json::to_vec(&receipt).unwrap()
    );
    let back: crate::questions::AnswerReceipt = load(&file).unwrap();
    assert_eq!(serde_json::to_value(back).unwrap(), receipt);
    assert!(load::<Value>(&file.with_extension("missing")).is_none());
}

#[test]
fn ids_are_checked_and_unreadable_records_are_skipped_or_refused() {
    let (_tmp, paths) = root();
    assert!(
        compaction::Record::file(&paths, "../x")
            .unwrap_err()
            .to_string()
            .contains("INVALID_OPERATION_ID")
    );
    assert!(
        crate::account_deletion::Record::file(&paths, "x")
            .unwrap_err()
            .to_string()
            .contains("INVALID_REQUEST_ID")
    );
    assert!(crate::reset_credit::Record::check_id(&ID.to_uppercase()).is_err());
    let Err(err) = crate::account_deletion::Record::read(&paths, ID) else {
        panic!("a missing record was read");
    };
    assert!(format!("{err:#}").contains("ACCOUNT_DELETION_NOT_FOUND"));
    let file = crate::account_deletion::Record::file(&paths, ID).unwrap();
    crate::paths::atomic_write(&file, b"{").unwrap();
    assert!(
        crate::account_deletion::Record::list(&paths)
            .unwrap()
            .is_empty()
    );
    // As before the engine: account deletion lists refuse an unreadable record and report the
    // parser's own message; the other kinds name it UNREADABLE_OPERATION_RECORD.
    assert!(crate::account_deletion::list(&paths).is_err());
    let Err(err) = crate::account_deletion::status(&paths, ID) else {
        panic!("an unreadable record was read");
    };
    let serde = serde_json::from_slice::<Value>(b"{")
        .unwrap_err()
        .to_string();
    assert_eq!(format!("{err:#}"), serde);
    let file = compaction::Record::file(&paths, ID).unwrap();
    crate::paths::atomic_write(&file, b"{").unwrap();
    let Err(err) = compaction::Record::read(&paths, ID) else {
        panic!("an unreadable record was read");
    };
    assert!(format!("{err:#}").starts_with("UNREADABLE_OPERATION_RECORD: "));
}

#[test]
fn a_worker_lock_marks_a_running_worker_and_a_free_one_a_stopped_worker() {
    use fs2::FileExt;
    let (_tmp, paths) = root();
    type R = crate::auto_recover::Record;
    // No lock file at all: no worker ever ran.
    assert!(R::worker_stopped(&paths, ID).unwrap());
    crate::paths::private_dir(&R::dir(&paths)).unwrap();
    let held = crate::paths::lock_file(&R::worker_lock(&paths, ID).unwrap()).unwrap();
    held.lock_exclusive().unwrap();
    assert!(!R::worker_stopped(&paths, ID).unwrap());
    // A second worker of the same record is refused while the first holds the lock.
    let second = crate::paths::lock_file(&R::worker_lock(&paths, ID).unwrap()).unwrap();
    assert!(second.try_lock_exclusive().is_err());
    drop(held);
    assert!(R::worker_stopped(&paths, ID).unwrap());
}

#[test]
fn updates_are_serialized_by_the_directory_lock() {
    let (_tmp, paths) = root();
    let mut r: compaction::Record = serde_json::from_value(json!({"schema_version":1,"id":ID,
        "session_id":"s","accepted_generation":"g","conversation_id":"c","runtime_generation":"rg",
        "phase":"waiting","reason":null,"created_at":1,"updated_at":1,"expires_at":9,
        "started_at":null,"finished_at":null,"attempts":0,"baseline":[],"compaction_turn_id":null}))
    .unwrap();
    // Saving never stamps an idle-point record (only an update does); `updated_at` stays 1.
    r.save(&paths).unwrap();
    let stored: Value =
        serde_json::from_slice(&fs::read(compaction::Record::file(&paths, ID).unwrap()).unwrap())
            .unwrap();
    assert_eq!(stored["updated_at"], 1);
    let threads: Vec<_> = (0..8)
        .map(|_| {
            let paths = paths.clone();
            std::thread::spawn(move || {
                for _ in 0..10 {
                    compaction::Record::update(&paths, ID, |r| {
                        let mut v = serde_json::to_value(&*r).unwrap();
                        v["attempts"] = json!(v["attempts"].as_u64().unwrap() + 1);
                        *r = serde_json::from_value(v).unwrap();
                        true
                    })
                    .unwrap();
                }
            })
        })
        .collect();
    for t in threads {
        t.join().unwrap();
    }
    let done: Value = serde_json::to_value(compaction::Record::read(&paths, ID).unwrap()).unwrap();
    assert_eq!(done["attempts"], 80, "a concurrent update was lost");
    assert!(done["updated_at"].as_i64().unwrap() > 1);
    assert!(paths.root.join(crate::layout::COMPACTIONS_LOCK).is_file());
    // A declined edit writes nothing.
    assert!(
        compaction::Record::update(&paths, ID, |_| false)
            .unwrap()
            .is_none()
    );
}

#[test]
fn reset_credit_saves_stamp_updated_at() {
    let (_tmp, paths) = root();
    let mut r: crate::reset_credit::Record = serde_json::from_value(json!({"schema_version":1,
        "id":ID,"kind":"account_reset_credit","provider":"codex","account_id":"codex:a",
        "credit_id":null,"idempotency_key":ID,"phase":"requesting","outcome":null,"attempts":1,
        "detail":"","codex_version":"v","created_at":1,"updated_at":1}))
    .unwrap();
    r.save(&paths).unwrap();
    assert!(
        crate::reset_credit::Record::read(&paths, ID)
            .unwrap()
            .updated_at
            > 1
    );
}

/// Account renames and reset credits list their directory plainly, as before the engine: an
/// unreadable directory lists nothing and an unreadable entry is skipped, never an error.
#[test]
fn plain_listings_skip_what_cannot_be_read() {
    use std::io::{Error, ErrorKind};
    const {
        assert!(
            crate::account_rename::Record::PLAIN_LIST && crate::reset_credit::Record::PLAIN_LIST
        );
        assert!(!compaction::Record::PLAIN_LIST && !crate::auto_recover::Record::PLAIN_LIST);
    }
    let unreadable: std::io::Result<std::vec::IntoIter<std::io::Result<PathBuf>>> =
        Err(Error::from(ErrorKind::PermissionDenied));
    assert!(plain_files(unreadable).is_empty());
    let entries = vec![
        Ok(PathBuf::from("/d/a.json")),
        Err(Error::from(ErrorKind::Other)),
        Ok(PathBuf::from("/d/a.worker.lock")),
        Ok(PathBuf::from("/d/b.json")),
    ];
    assert_eq!(
        plain_files(Ok(entries.into_iter())),
        [PathBuf::from("/d/a.json"), PathBuf::from("/d/b.json")]
    );
    // End to end: a directory the process cannot read lists nothing (only observable when not
    // root; root reads every directory).
    let (_tmp, paths) = root();
    let dir = crate::reset_credit::Record::dir(&paths);
    crate::paths::private_dir(&dir).unwrap();
    use std::os::unix::fs::PermissionsExt;
    fs::set_permissions(&dir, fs::Permissions::from_mode(0o000)).unwrap();
    assert!(
        crate::reset_credit::Record::list(&paths)
            .unwrap()
            .is_empty()
    );
    assert!(
        crate::account_rename::Record::list(&paths)
            .unwrap()
            .is_empty()
    );
    fs::set_permissions(&dir, fs::Permissions::from_mode(0o700)).unwrap();
}
