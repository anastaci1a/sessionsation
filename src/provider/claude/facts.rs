//! Per-transcript facts, derived once and remembered across calls (contract 1.57.0).
//!
//! `state` needs four facts of every Claude transcript: the head metadata
//! (`sessionId`, `cwd`, first `timestamp`), the `forkedFrom` source, the
//! `continued-in` hand-off marker and the time of the last record. Reading them
//! used to re-open every transcript several times per call and per account.
//! Here each fact is derived by one bounded read (head: the leading records;
//! tail: the last 128 KiB, widened to 1 MiB only when the marker is undecided),
//! keyed by file identity (device and inode, so two accounts sharing one
//! projects directory share the facts), and kept in memory and in
//! `cache/claude-transcript-facts.json` while the file is unchanged:
//!
//! * tail facts are valid for the exact (size, mtime) they were read at;
//! * head facts depend only on the leading records. Once those are settled
//!   (64 records or 1 MiB read, or every field found) they survive appends:
//!   the file must be at least as large and the first 4 KiB (the size it had,
//!   when smaller) must hash the same. A replaced inode, a truncated file or a
//!   rewritten head is a miss. An unsettled head (a short file) is valid only
//!   for the exact (size, mtime).
//!
//! Every fact is identical to what the one-off readers (`reference_*` in the
//! tests) derive from the same bytes. The file is rewritten only when an entry
//! changed and the transcript stopped changing (a running session's transcript
//! changes on every turn; its facts stay in memory) or an entry went away.
use super::super::activity::{self, TAIL};
use serde::{Deserialize, Serialize};
use serde_json::Value;
use sha2::{Digest, Sha256};
use std::{
    collections::{BTreeMap, BTreeSet},
    fs,
    io::{BufRead, BufReader, Read, Seek, SeekFrom},
    os::unix::fs::MetadataExt,
    path::{Path, PathBuf},
    sync::Mutex,
};

const HEAD_LINES: usize = 64;
const HEAD_BYTES: u64 = 1024 * 1024;
const PREFIX: u64 = 4096;
/// Bounded tail read for the continuation marker.
const CONTINUATION_TAIL: u64 = 1024 * 1024;
/// A transcript modified this recently is still being written: its facts are
/// kept in memory only, so the cache file does not change on every turn.
const SETTLED_SECONDS: i64 = 30;
const VERSION: u32 = 1;

/// Head facts of a transcript.
#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize)]
pub struct Head {
    pub id: Option<String>,
    pub cwd: Option<String>,
    pub at: Option<String>,
    pub forked_from: Option<String>,
}
/// Tail facts of a transcript.
#[derive(Clone, Debug, Default, PartialEq)]
pub struct Tail {
    pub continued_in: Option<String>,
    /// Milliseconds of the newest record time, else of the modification time.
    pub last_ms: Option<i64>,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
struct HeadEntry {
    head: Head,
    /// The leading records are settled: appending cannot change them.
    settled: bool,
    size: u64,
    mtime_ns: i64,
    prefix: u64,
    digest: String,
}
#[derive(Clone, Debug, Serialize, Deserialize)]
struct TailEntry {
    size: u64,
    mtime_ns: i64,
    continued_in: Option<String>,
    last_record_ms: Option<i64>,
}
#[derive(Clone, Debug, Serialize, Deserialize)]
struct Entry {
    stem: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    head: Option<HeadEntry>,
    #[serde(skip_serializing_if = "Option::is_none")]
    tail: Option<TailEntry>,
    /// Changed in memory since the file was written, while the transcript was
    /// still being written.
    #[serde(skip)]
    unsaved: bool,
}
#[derive(Serialize, Deserialize)]
struct Persisted {
    version: u32,
    entries: BTreeMap<String, Entry>,
}

#[derive(Default)]
struct Store {
    file: Option<PathBuf>,
    entries: BTreeMap<String, Entry>,
    dirty: bool,
}
#[cfg(not(test))]
static STORE: std::sync::OnceLock<Mutex<Store>> = std::sync::OnceLock::new();
fn store() -> std::sync::MutexGuard<'static, Store> {
    #[cfg(not(test))]
    let m = STORE.get_or_init(Default::default);
    #[cfg(test)]
    let m = tests::store_mutex();
    m.lock().unwrap_or_else(|e| e.into_inner())
}
// Tests run in parallel threads with their own roots: one store per thread.
fn scanned() {
    #[cfg(test)]
    tests::scanned();
}

/// Use `file` as the persistent copy (loaded once per process and file).
pub(super) fn attach(file: PathBuf) {
    let mut s = store();
    if s.file.as_ref() == Some(&file) {
        return;
    }
    let loaded = fs::read(&file)
        .ok()
        .and_then(|b| serde_json::from_slice::<Persisted>(&b).ok())
        .filter(|f| f.version == VERSION)
        .map(|f| f.entries)
        .unwrap_or_default();
    s.file = Some(file);
    s.entries = loaded;
    s.dirty = false;
}

/// Drop entries of transcripts that are gone (`seen` = identities enumerated
/// now) and write the file when anything changed. Best effort.
pub(super) fn flush(seen: &BTreeSet<String>) {
    let mut s = store();
    let before = s.entries.len();
    s.entries.retain(|k, _| seen.contains(k));
    if s.entries.len() != before {
        s.dirty = true;
    }
    let Some(file) = s.file.clone().filter(|_| s.dirty) else {
        return;
    };
    let bytes = serde_json::to_vec(&Persisted {
        version: VERSION,
        entries: s.entries.clone(),
    });
    if let Some(dir) = file.parent()
        && crate::paths::private_dir(dir).is_ok()
        && let Ok(bytes) = bytes
        && crate::paths::atomic_write(&file, &bytes).is_ok()
    {
        s.dirty = false;
        for e in s.entries.values_mut() {
            e.unsaved = false;
        }
    }
}

struct Identity {
    key: String,
    size: u64,
    mtime_ns: i64,
    hot: bool,
}
/// Identity and stamp of a regular file (not following symlinks).
fn identity(path: &Path) -> Option<Identity> {
    let m = fs::symlink_metadata(path).ok().filter(|m| m.is_file())?;
    let now = crate::daemon::now();
    Some(Identity {
        key: format!("{}:{}", m.dev(), m.ino()),
        size: m.len(),
        mtime_ns: m.mtime() * 1_000_000_000 + m.mtime_nsec(),
        hot: now - m.mtime() < SETTLED_SECONDS,
    })
}
/// The cache key of a transcript (device and inode).
pub(super) fn key_of(path: &Path) -> Option<String> {
    identity(path).map(|i| i.key)
}

fn stem_of(path: &Path) -> String {
    path.file_stem()
        .and_then(|s| s.to_str())
        .unwrap_or_default()
        .to_owned()
}

fn digest_of(path: &Path, len: u64) -> Option<String> {
    let mut file = fs::File::open(path).ok()?;
    let mut bytes = vec![];
    (&mut file).take(len).read_to_end(&mut bytes).ok()?;
    (bytes.len() as u64 == len).then(|| {
        Sha256::digest(&bytes)
            .iter()
            .take(12)
            .map(|b| format!("{b:02x}"))
            .collect()
    })
}

/// Read the leading records: `sessionId`/`cwd`/`timestamp` (first non-empty of
/// each) and the first `forkedFrom.sessionId` that is a UUID other than the
/// transcript's own. Message text is never kept. Returns whether the result is
/// settled (appending cannot change it).
fn scan_head(path: &Path, stem: &str) -> (Head, bool) {
    scanned();
    let mut head = Head::default();
    let Ok(file) = fs::File::open(path) else {
        return (head, false);
    };
    let mut reader = BufReader::new(file.take(HEAD_BYTES));
    let mut line = String::new();
    let (mut lines, mut consumed, mut settled) = (0usize, 0u64, false);
    while lines < HEAD_LINES {
        line.clear();
        let n = reader.read_line(&mut line).unwrap_or(0);
        if n == 0 {
            break;
        }
        lines += 1;
        consumed += n as u64;
        if let Ok(v) = serde_json::from_str::<Value>(&line) {
            let field = |k: &str| v[k].as_str().filter(|s| !s.is_empty()).map(str::to_owned);
            head.id = head.id.or_else(|| field("sessionId"));
            head.cwd = head.cwd.or_else(|| field("cwd"));
            head.at = head.at.or_else(|| field("timestamp"));
            if head.forked_from.is_none()
                && let Some(id) = v["forkedFrom"]["sessionId"].as_str()
                && id != stem
                && uuid::Uuid::parse_str(id).is_ok()
            {
                head.forked_from = Some(id.to_owned());
            }
        }
        if head.id.is_some()
            && head.cwd.is_some()
            && head.at.is_some()
            && head.forked_from.is_some()
        {
            settled = true;
            break;
        }
    }
    if lines >= HEAD_LINES || consumed >= HEAD_BYTES {
        settled = true;
    }
    (head, settled)
}

/// Head facts of a transcript. Without a regular file the leading records are
/// still read (nothing is remembered).
pub fn head(path: &Path) -> Head {
    let stem = stem_of(path);
    let Some(id) = identity(path) else {
        return scan_head(path, &stem).0;
    };
    {
        let mut s = store();
        if let Some(e) = s.entries.get_mut(&id.key)
            && e.stem == stem
            && let Some(h) = e.head.as_mut()
        {
            let exact = h.size == id.size && h.mtime_ns == id.mtime_ns;
            let grown = h.settled
                && id.size >= h.size
                && digest_of(path, h.prefix).as_deref() == Some(h.digest.as_str());
            if exact || grown {
                if !exact {
                    h.size = id.size;
                    h.mtime_ns = id.mtime_ns;
                    e.unsaved = true;
                }
                let result = h.head.clone();
                settle(&mut s, &id);
                return result;
            }
        }
    }
    let (head, settled) = scan_head(path, &stem);
    let prefix = id.size.min(PREFIX);
    let digest = digest_of(path, prefix);
    let mut s = store();
    if let Some(digest) = digest {
        let entry = entry_of(&mut s, &id, stem);
        entry.head = Some(HeadEntry {
            head: head.clone(),
            settled,
            size: id.size,
            mtime_ns: id.mtime_ns,
            prefix,
            digest,
        });
        entry.unsaved = true;
        settle(&mut s, &id);
    }
    head
}

/// The entry of a transcript, started afresh when its stem changed.
fn entry_of<'a>(s: &'a mut Store, id: &Identity, stem: String) -> &'a mut Entry {
    let entry = s.entries.entry(id.key.clone()).or_insert_with(|| Entry {
        stem: stem.clone(),
        head: None,
        tail: None,
        unsaved: false,
    });
    if entry.stem != stem {
        *entry = Entry {
            stem,
            head: None,
            tail: None,
            unsaved: false,
        };
    }
    entry
}

/// An entry that changed while its transcript was still being written is
/// saved once the transcript has settled.
fn settle(s: &mut Store, id: &Identity) {
    if id.hot {
        return;
    }
    if let Some(e) = s.entries.get_mut(&id.key)
        && e.unsaved
    {
        s.dirty = true;
        e.unsaved = false;
    }
}

enum Marker {
    Undecided,
    Decided(Option<String>),
}
/// The continuation marker in a window of whole records (`cut_front`: the
/// window starts mid-file, so its first partial line is dropped). The newest
/// decisive record wins; a user/assistant record after the marker cancels it.
fn marker(bytes: &[u8], cut_front: bool, stem: &str) -> Marker {
    let mut lines: Vec<&[u8]> = bytes.split(|b| *b == b'\n').collect();
    if cut_front && !lines.is_empty() {
        lines.remove(0);
    }
    #[derive(serde::Deserialize)]
    struct Record {
        #[serde(rename = "type")]
        kind: Option<String>,
        #[serde(rename = "sessionId")]
        session_id: Option<String>,
        #[serde(rename = "continuedInSessionId")]
        continued_in: Option<String>,
    }
    for line in lines.into_iter().rev() {
        if line.iter().all(u8::is_ascii_whitespace) {
            continue;
        }
        let Ok(r) = serde_json::from_slice::<Record>(line) else {
            continue;
        };
        match r.kind.as_deref() {
            Some("user" | "assistant") => return Marker::Decided(None),
            Some("continued-in") => {
                let Some(next) = r.continued_in else {
                    return Marker::Decided(None);
                };
                return Marker::Decided(
                    (r.session_id.as_deref() == Some(stem)
                        && next != stem
                        && uuid::Uuid::parse_str(&next).is_ok())
                    .then_some(next),
                );
            }
            _ => {}
        }
    }
    Marker::Undecided
}

fn read_window(file: &mut fs::File, len: u64, limit: u64) -> Option<(Vec<u8>, bool)> {
    let start = len.saturating_sub(limit);
    file.seek(SeekFrom::Start(start)).ok()?;
    let mut bytes = vec![];
    file.take(limit).read_to_end(&mut bytes).ok()?;
    Some((bytes, start > 0))
}

fn scan_tail(path: &Path, stem: &str) -> Option<(Option<String>, Option<i64>)> {
    scanned();
    let mut file = fs::File::open(path).ok()?;
    let len = file.metadata().ok()?.len();
    let (small, cut) = read_window(&mut file, len, TAIL)?;
    let last = activity::last_record_in(&small, cut);
    let continued = match marker(&small, cut, stem) {
        Marker::Decided(next) => next,
        // The whole file was in the window: nothing decisive in it.
        Marker::Undecided if !cut => None,
        Marker::Undecided => {
            let (wide, cut) = read_window(&mut file, len, CONTINUATION_TAIL)?;
            match marker(&wide, cut, stem) {
                Marker::Decided(next) => next,
                Marker::Undecided => None,
            }
        }
    };
    Some((continued, last))
}

/// Tail facts of a transcript, or `None` when it cannot be read.
pub fn tail(path: &Path) -> Option<Tail> {
    let stem = stem_of(path);
    let id = identity(path)?;
    let fallback = Some(id.mtime_ns / 1_000_000).filter(|n| *n > 0);
    {
        let mut s = store();
        if let Some(e) = s.entries.get_mut(&id.key)
            && e.stem == stem
            && let Some(t) = &e.tail
            && t.size == id.size
            && t.mtime_ns == id.mtime_ns
        {
            let result = Tail {
                continued_in: t.continued_in.clone(),
                last_ms: t.last_record_ms.or(fallback),
            };
            settle(&mut s, &id);
            return Some(result);
        }
    }
    let Some((continued_in, last_record_ms)) = scan_tail(path, &stem) else {
        return Some(Tail {
            continued_in: None,
            last_ms: fallback,
        });
    };
    let mut s = store();
    let entry = entry_of(&mut s, &id, stem);
    entry.tail = Some(TailEntry {
        size: id.size,
        mtime_ns: id.mtime_ns,
        continued_in: continued_in.clone(),
        last_record_ms,
    });
    entry.unsaved = true;
    settle(&mut s, &id);
    Some(Tail {
        continued_in,
        last_ms: last_record_ms.or(fallback),
    })
}

#[cfg(test)]
mod tests;
