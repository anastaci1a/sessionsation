//! agy's conversation index (`conversation_summaries.db`), edited only for
//! archive, restore and delete of one exact conversation id. Every write is one
//! `BEGIN IMMEDIATE` transaction through the SQLite library; a consistent
//! backup (`VACUUM INTO`) is taken before the first change, and the removed row
//! is first saved into a one-row database with the table's own schema so that
//! restore can put back every column (including the raw summary blob).
use anyhow::{Context, Result, bail, ensure};
use rusqlite::{Connection, OpenFlags, params};
use std::{path::Path, time::Duration};

const TABLE: &str = "conversation_summaries";
const KEY: &str = "conversation_id";

pub fn index_path(home: &Path) -> std::path::PathBuf {
    super::state_dir(home).join("conversation_summaries.db")
}

fn read_only(path: &Path) -> Result<Connection> {
    let db = Connection::open_with_flags(
        path,
        OpenFlags::SQLITE_OPEN_READ_ONLY | OpenFlags::SQLITE_OPEN_URI,
    )
    .with_context(|| format!("AGY_INDEX_UNREADABLE: {}", path.display()))?;
    db.busy_timeout(Duration::from_secs(5))?;
    Ok(db)
}
fn read_write(path: &Path) -> Result<Connection> {
    ensure!(
        std::fs::symlink_metadata(path)?.file_type().is_file(),
        "AGY_INDEX_UNREADABLE: {} is not a regular file",
        path.display()
    );
    let db = Connection::open_with_flags(
        path,
        OpenFlags::SQLITE_OPEN_READ_WRITE | OpenFlags::SQLITE_OPEN_URI,
    )
    .with_context(|| format!("AGY_INDEX_UNREADABLE: {}", path.display()))?;
    db.busy_timeout(Duration::from_secs(5))?;
    Ok(db)
}
/// SQL-quote a path for ATTACH / VACUUM INTO (single quotes doubled).
fn quoted(path: &Path) -> Result<String> {
    let text = path.to_str().context("path is not UTF-8")?;
    Ok(text.replace('\'', "''"))
}
fn uri(path: &Path, mode: &str) -> Result<String> {
    // SQLite file URI: only '%', '?' and '#' need escaping in a plain path.
    let text = path.to_str().context("path is not UTF-8")?;
    let escaped = text
        .replace('%', "%25")
        .replace('?', "%3f")
        .replace('#', "%23");
    Ok(format!("file:{escaped}?mode={mode}"))
}

/// A consistent copy of the whole index at `dest` (which must not exist).
pub fn backup(home: &Path, dest: &Path) -> Result<()> {
    ensure!(!dest.exists(), "AGY_INDEX_BACKUP_EXISTS");
    let db = read_only(&index_path(home))?;
    db.execute_batch(&format!("VACUUM INTO '{}'", quoted(dest)?))
        .context("AGY_INDEX_BACKUP_FAILED")?;
    let copy = read_only(dest)?;
    let check: String = copy.query_row("PRAGMA integrity_check", [], |r| r.get(0))?;
    ensure!(check == "ok", "AGY_INDEX_BACKUP_FAILED: integrity {check}");
    Ok(())
}

/// Whether the index has a row for exactly this conversation.
pub fn has_row(home: &Path, id: &str) -> Result<bool> {
    let path = index_path(home);
    if !path.exists() {
        return Ok(false);
    }
    let db = read_only(&path)?;
    Ok(db.query_row(
        &format!("SELECT count(*) FROM {TABLE} WHERE {KEY} = ?1"),
        params![id],
        |r| r.get::<_, i64>(0),
    )? > 0)
}

/// Save the row for `id` into a new database at `dest` using the index
/// table's own schema. Returns whether a row existed.
pub fn save_row(home: &Path, id: &str, dest: &Path) -> Result<bool> {
    ensure!(!dest.exists(), "AGY_INDEX_ROW_EXISTS");
    uuid::Uuid::parse_str(id)?;
    let out = Connection::open(dest)?;
    out.execute_batch(&format!(
        "ATTACH DATABASE '{}' AS src",
        quoted(Path::new(&uri(&index_path(home), "ro")?))?
    ))?;
    let sql: String = out
        .query_row(
            "SELECT sql FROM src.sqlite_master WHERE type='table' AND name=?1",
            params![TABLE],
            |r| r.get(0),
        )
        .context("AGY_INDEX_SCHEMA: conversation_summaries table not found")?;
    out.execute_batch(&sql)?;
    let tx = out.unchecked_transaction()?;
    let n = tx.execute(
        &format!("INSERT INTO main.{TABLE} SELECT * FROM src.{TABLE} WHERE {KEY} = ?1"),
        params![id],
    )?;
    tx.commit()?;
    out.execute_batch("DETACH DATABASE src")?;
    drop(out);
    if n == 0 {
        let _ = std::fs::remove_file(dest);
    } else {
        std::fs::File::open(dest)?.sync_all()?;
    }
    Ok(n > 0)
}

/// Delete exactly this conversation's row in one transaction. Returns the
/// number of rows removed (0 or 1).
pub fn delete_row(home: &Path, id: &str) -> Result<usize> {
    uuid::Uuid::parse_str(id)?;
    let path = index_path(home);
    if !path.exists() {
        return Ok(0);
    }
    let mut db = read_write(&path)?;
    let tx = db.transaction_with_behavior(rusqlite::TransactionBehavior::Immediate)?;
    let n = tx.execute(
        &format!("DELETE FROM {TABLE} WHERE {KEY} = ?1"),
        params![id],
    )?;
    if n > 1 {
        bail!("AGY_INDEX_AMBIGUOUS: {n} rows matched one conversation id; nothing was changed");
    }
    tx.commit()?;
    Ok(n)
}

/// Put a saved row back (every column the saved row has). Idempotent: an
/// existing row for the id is left as it is.
pub fn restore_row(home: &Path, id: &str, saved: &Path) -> Result<()> {
    uuid::Uuid::parse_str(id)?;
    let mut db = read_write(&index_path(home))?;
    db.execute_batch(&format!(
        "ATTACH DATABASE '{}' AS saved",
        quoted(Path::new(&uri(saved, "ro")?))?
    ))?;
    let columns = |schema: &str| -> Result<Vec<String>> {
        let mut st = db.prepare(&format!("PRAGMA {schema}.table_info({TABLE})"))?;
        let rows = st.query_map([], |r| r.get::<_, String>(1))?;
        Ok(rows.collect::<std::result::Result<_, _>>()?)
    };
    let (have, want) = (columns("main")?, columns("saved")?);
    ensure!(
        !want.is_empty() && want.iter().all(|c| have.contains(c)),
        "AGY_INDEX_SCHEMA: the saved row has columns this agy index no longer has; nothing was restored"
    );
    let list = want
        .iter()
        .map(|c| format!("\"{}\"", c.replace('"', "\"\"")))
        .collect::<Vec<_>>()
        .join(", ");
    let tx = db.transaction_with_behavior(rusqlite::TransactionBehavior::Immediate)?;
    let exists: i64 = tx.query_row(
        &format!("SELECT count(*) FROM main.{TABLE} WHERE {KEY} = ?1"),
        params![id],
        |r| r.get(0),
    )?;
    if exists == 0 {
        let n = tx.execute(
            &format!(
                "INSERT INTO main.{TABLE} ({list}) SELECT {list} FROM saved.{TABLE} WHERE {KEY} = ?1"
            ),
            params![id],
        )?;
        ensure!(n == 1, "ARCHIVE_CORRUPT: the saved index row is missing");
    }
    tx.commit()?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    const SCHEMA: &str = "CREATE TABLE `conversation_summaries` (`conversation_id` text,`title` text NOT NULL DEFAULT \"\",`step_count` integer NOT NULL DEFAULT 0,`last_modified_time` datetime NOT NULL,`raw_summary` blob,PRIMARY KEY (`conversation_id`))";

    fn home() -> (tempfile::TempDir, [String; 2]) {
        let home = tempfile::tempdir().unwrap();
        let state = super::super::state_dir(home.path());
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
}

// ------------------------------------------------------------------ jetbox

/// agy's `jetbox_summaries_proto.pb`: a cache whose top level is a repeated
/// map entry (`0a <len> { 0a <len> <conversation id>, 12 <len> <summary> }`).
/// Delete removes the one entry whose key is exactly this id, and only when the
/// whole file parses as that shape; anything else leaves the file untouched.
fn varint(bytes: &[u8], at: usize) -> Option<(usize, usize)> {
    let (mut value, mut shift) = (0usize, 0u32);
    for (i, b) in bytes.get(at..)?.iter().take(5).enumerate() {
        value |= usize::from(b & 0x7f) << shift;
        if b & 0x80 == 0 {
            return Some((value, at + i + 1));
        }
        shift += 7;
    }
    None
}
/// The file without the entry for `id`, or None when the shape is not the
/// expected one. `Some(same bytes)` means the id had no entry.
fn without_entry(bytes: &[u8], id: &str) -> Option<Vec<u8>> {
    let mut out = Vec::with_capacity(bytes.len());
    let mut at = 0;
    while at < bytes.len() {
        if bytes[at] != 0x0a {
            return None;
        }
        let (len, body) = varint(bytes, at + 1)?;
        let end = body.checked_add(len).filter(|e| *e <= bytes.len())?;
        let entry = &bytes[body..end];
        // The key: field 1, length-delimited.
        if entry.first() != Some(&0x0a) {
            return None;
        }
        let (klen, kbody) = varint(entry, 1)?;
        let key = entry.get(kbody..kbody.checked_add(klen)?)?;
        if key != id.as_bytes() {
            out.extend_from_slice(&bytes[at..end]);
        }
        at = end;
    }
    Some(out)
}
/// Remove the cached summary of exactly this conversation id, atomically.
/// Returns whether an entry was removed.
pub fn remove_cached_summary(home: &Path, id: &str) -> Result<bool> {
    use std::{
        io::Write,
        os::unix::fs::{MetadataExt, PermissionsExt},
    };
    uuid::Uuid::parse_str(id)?;
    let file = super::state_dir(home).join("jetbox_summaries_proto.pb");
    let Ok(meta) = std::fs::symlink_metadata(&file) else {
        return Ok(false);
    };
    ensure!(
        meta.file_type().is_file(),
        "AGY_CACHE_UNREADABLE: jetbox_summaries_proto.pb is not a regular file"
    );
    let bytes = std::fs::read(&file)?;
    let Some(updated) = without_entry(&bytes, id) else {
        return Ok(false); // unknown shape: leave agy's cache alone
    };
    if updated.len() == bytes.len() {
        return Ok(false);
    }
    let mut tmp = tempfile::Builder::new()
        .prefix(".jetbox.delete-")
        .tempfile_in(super::state_dir(home))?;
    tmp.as_file()
        .set_permissions(std::fs::Permissions::from_mode(meta.mode() & 0o7777))?;
    tmp.write_all(&updated)?;
    tmp.as_file().sync_all()?;
    let now = std::fs::symlink_metadata(&file)?;
    if now.ino() != meta.ino() || std::fs::read(&file)? != bytes {
        return Ok(false); // agy rewrote it meanwhile; its own state wins
    }
    tmp.persist(&file).map_err(|e| e.error)?;
    Ok(true)
}

#[cfg(test)]
mod jetbox_tests {
    use super::*;

    fn entry(id: &str, body: &[u8]) -> Vec<u8> {
        let mut inner = vec![0x0a, id.len() as u8];
        inner.extend_from_slice(id.as_bytes());
        inner.extend_from_slice(&[0x12, body.len() as u8]);
        inner.extend_from_slice(body);
        let mut out = vec![0x0a];
        // inner may exceed 127: two-byte varint.
        let n = inner.len();
        if n < 128 {
            out.push(n as u8);
        } else {
            out.extend_from_slice(&[(n & 0x7f) as u8 | 0x80, (n >> 7) as u8]);
        }
        out.extend(inner);
        out
    }

    #[test]
    fn removes_exactly_one_entry_and_nothing_else() {
        let ids: Vec<String> = (0..3).map(|_| uuid::Uuid::new_v4().to_string()).collect();
        let home = tempfile::tempdir().unwrap();
        std::fs::create_dir_all(super::super::state_dir(home.path())).unwrap();
        let file = super::super::state_dir(home.path()).join("jetbox_summaries_proto.pb");
        let parts = [
            entry(&ids[0], &[1; 5]),
            entry(&ids[1], &[7; 200]),
            entry(&ids[2], &[3; 9]),
        ];
        std::fs::write(&file, parts.concat()).unwrap();
        assert!(remove_cached_summary(home.path(), &ids[1]).unwrap());
        assert_eq!(
            std::fs::read(&file).unwrap(),
            [parts[0].clone(), parts[2].clone()].concat()
        );
        // Absent id: unchanged.
        assert!(!remove_cached_summary(home.path(), &ids[1]).unwrap());
        assert!(!remove_cached_summary(home.path(), &uuid::Uuid::new_v4().to_string()).unwrap());
    }

    #[test]
    fn an_unexpected_shape_is_left_alone() {
        let id = uuid::Uuid::new_v4().to_string();
        let home = tempfile::tempdir().unwrap();
        std::fs::create_dir_all(super::super::state_dir(home.path())).unwrap();
        let file = super::super::state_dir(home.path()).join("jetbox_summaries_proto.pb");
        let mut bytes = entry(&id, &[1; 5]);
        bytes.extend_from_slice(&[0x12, 0x01, 0x00]); // a different top-level field
        std::fs::write(&file, &bytes).unwrap();
        assert!(!remove_cached_summary(home.path(), &id).unwrap());
        assert_eq!(std::fs::read(&file).unwrap(), bytes);
        let mut truncated = entry(&id, &[1; 5]);
        truncated.truncate(truncated.len() - 2);
        std::fs::write(&file, &truncated).unwrap();
        assert!(!remove_cached_summary(home.path(), &id).unwrap());
        assert_eq!(std::fs::read(&file).unwrap(), truncated);
    }
}
