use super::super::*;

const SCHEMA: &str = "CREATE TABLE `conversation_summaries` (`conversation_id` text,`title` text NOT NULL DEFAULT \"\",`step_count` integer NOT NULL DEFAULT 0,`last_modified_time` datetime NOT NULL,`raw_summary` blob,PRIMARY KEY (`conversation_id`))";

fn home() -> (tempfile::TempDir, [String; 2]) {
    let home = tempfile::tempdir().unwrap();
    let state = super::super::super::state_dir(home.path());
    std::fs::create_dir_all(&state).unwrap();
    let db = Connection::open(index_path(home.path())).unwrap();
    db.execute_batch("PRAGMA journal_mode=WAL").ok();
    db.execute_batch(SCHEMA).unwrap();
    let ids = [
        uuid::Uuid::new_v4().to_string(),
        uuid::Uuid::new_v4().to_string(),
    ];
    for (i, id) in ids.iter().enumerate() {
        db.execute(
            "INSERT INTO conversation_summaries VALUES (?1, ?2, ?3, '2026-10-29 21:52:00.5+00:00', ?4)",
            params![id, format!("t{i}"), i as i64 + 1, vec![0u8, 1, 255, i as u8]],
        )
        .unwrap();
    }
    (home, ids)
}

fn dump(home: &Path) -> Vec<(String, String, i64, String, Vec<u8>)> {
    let db = read_only(&index_path(home)).unwrap();
    let mut st = db
        .prepare("SELECT conversation_id, title, step_count, CAST(last_modified_time AS TEXT), raw_summary FROM conversation_summaries ORDER BY 1")
        .unwrap();
    st.query_map([], |r| {
        Ok((r.get(0)?, r.get(1)?, r.get(2)?, r.get(3)?, r.get(4)?))
    })
    .unwrap()
    .map(Result::unwrap)
    .collect()
}

#[test]
fn deletes_exactly_one_row_and_restores_every_column() {
    let (home, ids) = home();
    let before = dump(home.path());
    let work = tempfile::tempdir().unwrap();
    let (backup_file, saved) = (work.path().join("backup.db"), work.path().join("row.db"));
    backup(home.path(), &backup_file).unwrap();
    assert!(save_row(home.path(), &ids[0], &saved).unwrap());
    assert_eq!(delete_row(home.path(), &ids[0]).unwrap(), 1);
    assert_eq!(delete_row(home.path(), &ids[0]).unwrap(), 0);
    let after = dump(home.path());
    assert_eq!(after.len(), 1);
    assert_eq!(after[0].0, ids[1], "the other conversation is untouched");
    assert!(!has_row(home.path(), &ids[0]).unwrap());
    // The backup still has both rows.
    assert_eq!(
        read_only(&backup_file)
            .unwrap()
            .query_row("SELECT count(*) FROM conversation_summaries", [], |r| r
                .get::<_, i64>(0))
            .unwrap(),
        2
    );
    restore_row(home.path(), &ids[0], &saved).unwrap();
    restore_row(home.path(), &ids[0], &saved).unwrap();
    assert_eq!(dump(home.path()), before);
}

#[test]
fn matches_the_exact_id_only_and_saves_nothing_for_a_missing_row() {
    let (home, ids) = home();
    let work = tempfile::tempdir().unwrap();
    let other = uuid::Uuid::new_v4().to_string();
    assert!(!save_row(home.path(), &other, &work.path().join("none.db")).unwrap());
    assert!(!work.path().join("none.db").exists());
    assert_eq!(delete_row(home.path(), &other).unwrap(), 0);
    assert_eq!(dump(home.path()).len(), 2);
    assert!(delete_row(home.path(), "not-a-uuid").is_err());
    assert!(has_row(home.path(), &ids[1]).unwrap());
}

#[test]
fn refuses_to_restore_into_a_changed_schema() {
    let (home, ids) = home();
    let work = tempfile::tempdir().unwrap();
    let saved = work.path().join("row.db");
    save_row(home.path(), &ids[0], &saved).unwrap();
    delete_row(home.path(), &ids[0]).unwrap();
    let db = Connection::open(index_path(home.path())).unwrap();
    db.execute_batch("ALTER TABLE conversation_summaries DROP COLUMN raw_summary")
        .unwrap();
    drop(db);
    let e = restore_row(home.path(), &ids[0], &saved)
        .unwrap_err()
        .to_string();
    assert!(e.contains("AGY_INDEX_SCHEMA"), "{e}");
    assert_eq!(dump_ids(home.path()), vec![ids[1].clone()]);
}

fn dump_ids(home: &Path) -> Vec<String> {
    let db = read_only(&index_path(home)).unwrap();
    let mut st = db
        .prepare("SELECT conversation_id FROM conversation_summaries")
        .unwrap();
    st.query_map([], |r| r.get(0))
        .unwrap()
        .map(Result::unwrap)
        .collect()
}
