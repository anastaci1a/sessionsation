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
mod tests;

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
