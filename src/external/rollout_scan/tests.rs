use super::*;
use std::io::Cursor;
fn event(kind: &str) -> String {
    format!("{}\n", json!({"type":"event_msg","payload":{"type":kind}}))
}
fn filler(bytes: usize) -> String {
    let pad = "x".repeat(200);
    let line = format!(
        "{}\n",
        json!({"type":"response_item","payload":{"text":pad}})
    );
    line.repeat(bytes / line.len() + 1)
}
/// Counts bytes actually read, to prove bounded reads.
struct Counting<R> {
    inner: R,
    read: std::rc::Rc<std::cell::Cell<u64>>,
}
impl<R: Read> Read for Counting<R> {
    fn read(&mut self, buf: &mut [u8]) -> std::io::Result<usize> {
        let n = self.inner.read(buf)?;
        self.read.set(self.read.get() + n as u64);
        Ok(n)
    }
}
impl<R: Seek> Seek for Counting<R> {
    fn seek(&mut self, pos: SeekFrom) -> std::io::Result<u64> {
        self.inner.seek(pos)
    }
}
#[test]
fn tail_decision_matches_the_full_scan_on_every_shape() {
    let big = filler(300 * 1024);
    let cases = [
        String::new(),
        big.clone(),
        format!("{}{big}", event("task_complete")),
        format!("{big}{}{big}", event("task_complete")),
        format!("{}{big}{}", event("task_complete"), event("task_started")),
        format!("{big}{}not json\n", event("task_complete")),
        format!("not json\n{}{big}", event("task_complete")),
        format!("{big}{}{}", event("task_started"), event("turn_aborted")),
        format!("{}\n{big}", event("task_complete").trim_end()),
        format!("{big}{}\n", event("task_complete")),
        format!("{big}{}", event("task_complete").trim_end()),
        format!("{big}{}\r\n", event("task_complete").trim_end()),
    ];
    for (index, case) in cases.iter().enumerate() {
        let expected = complete_full(case.as_bytes()).unwrap();
        let tail = complete_tail(Cursor::new(case.as_bytes()), TAIL_MAX);
        assert_eq!(tail, Some(expected), "case {index}");
    }
}
#[test]
fn ambiguous_tail_falls_back_to_the_full_scan() {
    // The decisive record is older than the bound: undecided, never guessed.
    let case = format!("{}{}", event("task_complete"), filler(200 * 1024));
    assert_eq!(
        complete_tail(Cursor::new(case.as_bytes()), 128 * 1024),
        None
    );
    let t = tempfile::tempdir().unwrap();
    let path = t.path().join("rollout.jsonl");
    fs::write(&path, &case).unwrap();
    assert!(
        complete(&path).unwrap(),
        "full scan decides beyond the bound"
    );
    // A line longer than the window is never judged from its middle.
    let long = format!("{}{}", event("task_complete"), "y".repeat(100 * 1024));
    assert_eq!(complete_tail(Cursor::new(long.as_bytes()), 64 * 1024), None);
}
#[test]
fn large_rollouts_and_duplicate_copies_are_read_in_bounded_amounts() {
    let t = tempfile::tempdir().unwrap();
    let body = format!("{}{}", filler(24 * 1024 * 1024), event("task_complete"));
    let read = std::rc::Rc::new(std::cell::Cell::new(0));
    let reader = Counting {
        inner: Cursor::new(body.as_bytes()),
        read: read.clone(),
    };
    assert_eq!(complete_tail(reader, TAIL_MAX), Some(true));
    assert!(read.get() <= TAIL_START, "read {} bytes", read.get());
    assert!(complete_full(body.as_bytes()).unwrap());
    // Duplicates across homes: same, grown and diverged copies.
    let a = t.path().join("a.jsonl");
    let b = t.path().join("b.jsonl");
    fs::write(&a, &body).unwrap();
    fs::write(&b, &body).unwrap();
    let whole = |x: &Path, y: &Path| {
        let (x, y) = (fs::read(x).unwrap(), fs::read(y).unwrap());
        (
            x.len() as u64,
            y.len() as u64,
            x.starts_with(&y) || y.starts_with(&x),
        )
    };
    assert_eq!(prefix_relation(&a, &b).unwrap(), whole(&a, &b));
    let grown = format!("{body}{}", event("task_started"));
    fs::write(&b, &grown).unwrap();
    assert_eq!(prefix_relation(&a, &b).unwrap(), whole(&a, &b));
    assert_eq!(prefix_relation(&b, &a).unwrap(), whole(&b, &a));
    let mut diverged = body.clone().into_bytes();
    let middle = diverged.len() / 2;
    diverged[middle] ^= 1;
    fs::write(&b, &diverged).unwrap();
    assert_eq!(prefix_relation(&a, &b).unwrap(), whole(&a, &b));
    assert!(!prefix_relation(&a, &b).unwrap().2);
    // The cache answers unchanged pairs without re-reading.
    let p = Paths::new(Some(t.path().join("root"))).unwrap();
    let mut cache = ScanCache::load(&p);
    assert!(cache.complete(&a).unwrap());
    assert!(!cache.prefix(&a, &b).unwrap().2);
    cache.save();
    let mut cache = ScanCache::load(&p);
    assert_eq!(cache.entries.len(), 2);
    assert!(!cache.changed || cache.prefix(&a, &b).is_ok());
    assert!(cache.complete(&a).unwrap());
    assert!(!cache.changed);
}
