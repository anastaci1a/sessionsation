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
#[cfg(not(test))]
fn store() -> std::sync::MutexGuard<'static, Store> {
    STORE
        .get_or_init(Default::default)
        .lock()
        .unwrap_or_else(|e| e.into_inner())
}
// Tests run in parallel threads with their own roots: one store per thread.
#[cfg(test)]
thread_local! {
    static STORE: &'static Mutex<Store> = Box::leak(Box::default());
    static SCANS: std::cell::Cell<usize> = const { std::cell::Cell::new(0) };
}
#[cfg(test)]
fn store() -> std::sync::MutexGuard<'static, Store> {
    STORE.with(|m| *m).lock().unwrap_or_else(|e| e.into_inner())
}
#[cfg(test)]
fn scanned() {
    SCANS.with(|c| c.set(c.get() + 1));
}
#[cfg(not(test))]
fn scanned() {}

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
mod tests {
    use super::super::{continued_in, forked_from, transcript_metadata};
    use super::*;
    use serde_json::json;
    use std::io::Write;

    const OLD: &str = "3f1c1a52-0b7e-4c53-9d36-6d1f6a0b2c11";
    const NEXT: &str = "9a8b7c6d-1e2f-4a3b-8c4d-5e6f7a8b9c0d";

    fn uuid() -> String {
        uuid::Uuid::new_v4().to_string()
    }
    fn user(id: &str, n: usize) -> String {
        format!(
            "{}\n",
            json!({"type":"user","sessionId":id,"cwd":"/work/x","timestamp":format!("2026-11-01T00:00:{:02}Z", n % 60),"message":{"content":"hi"}})
        )
    }
    fn noise(n: usize, bytes: usize) -> String {
        format!(
            "{}\n",
            json!({"type":"system","timestamp":format!("2026-11-01T01:00:{:02}Z", n % 60),"pad":"p".repeat(bytes)})
        )
    }
    fn marker(id: &str, next: &str) -> String {
        format!(
            "{}\n",
            json!({"type":"continued-in","sessionId":id,"continuedInSessionId":next})
        )
    }
    fn append(path: &Path, text: &str) {
        let mut f = fs::OpenOptions::new().append(true).open(path).unwrap();
        f.write_all(text.as_bytes()).unwrap();
    }
    fn age(path: &Path) {
        let old = std::time::SystemTime::now() - std::time::Duration::from_secs(3600);
        fs::OpenOptions::new()
            .write(true)
            .open(path)
            .unwrap()
            .set_modified(old)
            .unwrap();
    }
    fn make(dir: &Path, id: &str, text: &str) -> PathBuf {
        let p = dir.join(format!("{id}.jsonl"));
        fs::write(&p, text).unwrap();
        p
    }
    /// The cached facts equal the one-off readers on the same bytes.
    fn same_as_reference(path: &Path) {
        let h = head(path);
        let (id, cwd, at) = transcript_metadata(path);
        assert_eq!((h.id, h.cwd, h.at), (id, cwd, at), "{path:?}");
        assert_eq!(h.forked_from, forked_from(path), "{path:?}");
        let t = tail(path).unwrap();
        assert_eq!(t.continued_in, continued_in(path), "{path:?}");
        let len = fs::metadata(path).unwrap().len();
        let mtime = fs::metadata(path)
            .unwrap()
            .modified()
            .unwrap()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_millis() as i64;
        assert_eq!(
            t.last_ms,
            activity::last_record_time(path, len).or(Some(mtime)),
            "{path:?}"
        );
    }

    #[test]
    fn facts_equal_the_one_off_readers_on_every_shape() {
        let dir = tempfile::tempdir().unwrap();
        let d = dir.path();
        let id = uuid();
        let other = uuid();
        let mut cases: Vec<(String, String)> = vec![];
        let mut body = String::new();
        (0..200).for_each(|n| body += &user(&id, n));
        cases.push((id.clone(), body));
        // Short (unsettled) head, and no record time anywhere.
        let short = uuid();
        cases.push((short.clone(), "{\"type\":\"summary\"}\n".into()));
        // Fork source on the third record; the own id is never a source.
        let forked = uuid();
        let fork_line = format!(
            "{}\n",
            json!({"type":"user","sessionId":forked,"forkedFrom":{"sessionId":other},"timestamp":"2026-11-01T00:00:09Z"})
        );
        let own_line = format!(
            "{}\n",
            json!({"type":"user","forkedFrom":{"sessionId":forked}})
        );
        cases.push((forked.clone(), user(&forked, 0) + &own_line + &fork_line));
        // A first record over the 1 MiB head limit.
        let huge = uuid();
        cases.push((huge.clone(), noise(0, 1_200_000) + &user(&huge, 1)));
        // More than 64 records before the first sessionId.
        let late = uuid();
        let mut body = String::new();
        (0..70).for_each(|n| body += &noise(n, 10));
        body += &user(&late, 1);
        cases.push((late.clone(), body));
        // Hand-off marker: effective, cancelled by a later record, wrong id,
        // and far enough from the end to need the wide window.
        let (hand, resumed, wrong, far) = (uuid(), uuid(), uuid(), uuid());
        cases.push((hand.clone(), user(&hand, 0) + &marker(&hand, NEXT)));
        cases.push((
            resumed.clone(),
            user(&resumed, 0) + &marker(&resumed, NEXT) + &user(&resumed, 1),
        ));
        cases.push((wrong.clone(), user(&wrong, 0) + &marker(OLD, NEXT)));
        let mut body = user(&far, 0) + &marker(&far, NEXT);
        (0..30).for_each(|n| body += &noise(n, 16_000));
        cases.push((far.clone(), body));
        let mut body = user(&far, 0) + &marker(&far, NEXT);
        (0..80).for_each(|n| body += &noise(n, 16_000));
        let beyond = uuid();
        cases.push((beyond.clone(), body.replace(&far, &beyond)));
        for (id, text) in cases {
            let p = make(d, &id, &text);
            same_as_reference(&p);
            same_as_reference(&p); // a hit gives the same facts
            age(&p);
            same_as_reference(&p);
        }
        // Spot-check the interesting ones are not all `None`.
        assert_eq!(
            head(&d.join(format!("{forked}.jsonl"))).forked_from,
            Some(other)
        );
        assert_eq!(
            tail(&d.join(format!("{hand}.jsonl")))
                .unwrap()
                .continued_in
                .as_deref(),
            Some(NEXT)
        );
        assert_eq!(
            tail(&d.join(format!("{far}.jsonl")))
                .unwrap()
                .continued_in
                .as_deref(),
            Some(NEXT),
            "the wide window finds a marker 480 KB before the end"
        );
    }

    #[test]
    fn an_unchanged_transcript_is_read_once_and_an_appended_one_by_its_tail_only() {
        let dir = tempfile::tempdir().unwrap();
        let id = uuid();
        let mut body = String::new();
        (0..200).for_each(|n| body += &user(&id, n));
        let p = make(dir.path(), &id, &body);
        age(&p);
        let before = SCANS.with(|c| c.get());
        same_as_reference(&p);
        same_as_reference(&p);
        let scans = |from: usize| SCANS.with(|c| c.get()) - from;
        // The reference readers do not count: only the cached side does.
        assert_eq!(scans(before), 2, "one head scan and one tail scan");
        let before = SCANS.with(|c| c.get());
        // Appended between calls: the settled head survives, the tail is re-read.
        append(&p, &marker(&id, NEXT));
        let t = tail(&p).unwrap();
        assert_eq!(t.continued_in.as_deref(), Some(NEXT));
        assert_eq!(head(&p).cwd.as_deref(), Some("/work/x"));
        assert_eq!(scans(before), 1, "tail only");
        same_as_reference(&p);
        // Appended again, cancelling the marker.
        append(&p, &user(&id, 5));
        assert_eq!(tail(&p).unwrap().continued_in, None);
        same_as_reference(&p);
    }

    #[test]
    fn an_unsettled_head_follows_the_file_as_it_grows() {
        let dir = tempfile::tempdir().unwrap();
        let id = uuid();
        let p = make(dir.path(), &id, "{\"type\":\"summary\"}\n");
        same_as_reference(&p);
        assert_eq!(head(&p).cwd, None);
        append(&p, &user(&id, 1));
        same_as_reference(&p);
        assert_eq!(head(&p).cwd.as_deref(), Some("/work/x"));
        append(
            &p,
            &format!("{}\n", json!({"forkedFrom":{"sessionId":NEXT}})),
        );
        same_as_reference(&p);
        assert_eq!(head(&p).forked_from.as_deref(), Some(NEXT));
    }

    #[test]
    fn a_truncated_or_replaced_transcript_is_never_served_stale() {
        let dir = tempfile::tempdir().unwrap();
        let id = uuid();
        let mut body = String::new();
        (0..100).for_each(|n| body += &user(&id, n));
        let p = make(dir.path(), &id, &body);
        age(&p);
        same_as_reference(&p);
        assert_eq!(head(&p).cwd.as_deref(), Some("/work/x"));
        // Truncated in place (same inode, smaller).
        fs::write(&p, user(&id, 1).replace("/work/x", "/work/y")).unwrap();
        age(&p);
        same_as_reference(&p);
        assert_eq!(head(&p).cwd.as_deref(), Some("/work/y"));
        // Rewritten in place, larger, with another head (same inode).
        let mut body = String::new();
        (0..300).for_each(|n| body += &user(&id, n).replace("/work/x", "/work/z"));
        fs::write(&p, &body).unwrap();
        age(&p);
        same_as_reference(&p);
        assert_eq!(head(&p).cwd.as_deref(), Some("/work/z"));
        // Replaced by another file (new inode) of the same size and mtime.
        let len = fs::metadata(&p).unwrap().len();
        let mtime = fs::metadata(&p).unwrap().modified().unwrap();
        fs::remove_file(&p).unwrap();
        fs::write(&p, body.replace("/work/z", "/work/q")).unwrap();
        assert_eq!(fs::metadata(&p).unwrap().len(), len);
        fs::OpenOptions::new()
            .write(true)
            .open(&p)
            .unwrap()
            .set_modified(mtime)
            .unwrap();
        same_as_reference(&p);
        assert_eq!(head(&p).cwd.as_deref(), Some("/work/q"));
    }

    #[test]
    fn accounts_sharing_one_projects_directory_share_the_facts() {
        let dir = tempfile::tempdir().unwrap();
        let shared = dir.path().join("shared/projects");
        let project = shared.join("-work-x");
        fs::create_dir_all(&project).unwrap();
        let mut ids = vec![];
        for n in 0..5 {
            let id = uuid();
            make(&project, &id, &(user(&id, n) + &user(&id, n + 1)));
            ids.push(id);
        }
        let (a, b) = (dir.path().join("a"), dir.path().join("b"));
        for home in [&a, &b] {
            fs::create_dir_all(home).unwrap();
            std::os::unix::fs::symlink(&shared, home.join("projects")).unwrap();
        }
        let before = SCANS.with(|c| c.get());
        let first = super::super::transcripts(&a);
        let second = super::super::transcripts(&b);
        assert_eq!(first.len(), 5);
        assert_eq!(
            first
                .iter()
                .map(|t| (&t.session_id, &t.cwd, &t.started_at))
                .collect::<Vec<_>>(),
            second
                .iter()
                .map(|t| (&t.session_id, &t.cwd, &t.started_at))
                .collect::<Vec<_>>()
        );
        assert_eq!(
            SCANS.with(|c| c.get()) - before,
            5,
            "one head scan per transcript, not per account"
        );
    }

    #[test]
    fn the_cache_file_is_rewritten_only_when_something_changed() {
        let dir = tempfile::tempdir().unwrap();
        let projects = dir.path().join("projects");
        fs::create_dir_all(&projects).unwrap();
        let cache = dir.path().join("cache/claude-transcript-facts.json");
        let (id, gone) = (uuid(), uuid());
        let p = make(&projects, &id, &(user(&id, 0) + &marker(&id, NEXT)));
        let q = make(&projects, &gone, &user(&gone, 0));
        attach(cache.clone());
        let keys = |paths: &[&Path]| {
            paths
                .iter()
                .filter_map(|p| key_of(p))
                .collect::<BTreeSet<_>>()
        };
        for f in [&p, &q] {
            head(f);
            tail(f);
        }
        // A transcript still being written is not worth a cache file.
        flush(&keys(&[&p, &q]));
        assert!(!cache.exists());
        age(&p);
        age(&q);
        head(&p);
        tail(&p);
        head(&q);
        tail(&q);
        flush(&keys(&[&p, &q]));
        let first = fs::metadata(&cache).unwrap();
        // Nothing changed: no rewrite (same inode), whatever is asked.
        for _ in 0..3 {
            same_as_reference(&p);
            flush(&keys(&[&p, &q]));
        }
        assert_eq!(fs::metadata(&cache).unwrap().ino(), first.ino());
        // A new process (empty store) reads the file instead of the transcripts.
        *store() = Store::default();
        attach(cache.clone());
        let before = SCANS.with(|c| c.get());
        assert_eq!(tail(&p).unwrap().continued_in.as_deref(), Some(NEXT));
        assert_eq!(head(&p).cwd.as_deref(), Some("/work/x"));
        assert_eq!(SCANS.with(|c| c.get()) - before, 0);
        flush(&keys(&[&p, &q]));
        assert_eq!(fs::metadata(&cache).unwrap().ino(), first.ino());
        // A transcript that went away is dropped from the file.
        fs::remove_file(&q).unwrap();
        flush(&keys(&[&p]));
        assert_ne!(fs::metadata(&cache).unwrap().ino(), first.ino());
        let written: Persisted = serde_json::from_slice(&fs::read(&cache).unwrap()).unwrap();
        assert_eq!(written.entries.len(), 1);
        // The file holds identities and facts, never message text.
        let text = fs::read_to_string(&cache).unwrap();
        assert!(!text.contains("\"hi\""), "{text}");
    }
}
