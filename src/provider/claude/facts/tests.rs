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

// Tests run in parallel threads with their own roots: one store per thread.
thread_local! {
    static STORE: &'static Mutex<Store> = Box::leak(Box::default());
    pub(super) static SCANS: std::cell::Cell<usize> = const { std::cell::Cell::new(0) };
}
pub(super) fn store_mutex() -> &'static Mutex<Store> {
    STORE.with(|m| *m)
}
pub(super) fn scanned() {
    SCANS.with(|c| c.set(c.get() + 1));
}
