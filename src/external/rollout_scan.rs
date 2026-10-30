//! Bounded reads of Codex rollouts for inventory: tail-first completeness and
//! streaming prefix comparison of duplicate copies. Results match the original
//! whole-file algorithms (kept here as the reference/fallback).
use super::*;

/// The original whole-file completeness scan: the last decisive record
/// (`task_started`/`turn_aborted` → false, `task_complete` → true) wins, and a
/// damaged line after it means not complete.
pub(crate) fn complete_full(reader: impl Read) -> Result<bool> {
    let mut terminal = false;
    let mut seen = false;
    for line in BufReader::new(reader).lines() {
        let line = line?;
        let Ok(value) = serde_json::from_str::<Value>(&line) else {
            // Damage in an old turn must not hide newer positive completion.
            // Damage after the last completion still prevents stopping.
            terminal = false;
            seen = false;
            continue;
        };
        if value["type"] != "event_msg" {
            continue;
        }
        match value["payload"]["type"].as_str() {
            Some("task_started") => {
                terminal = false;
                seen = true
            }
            Some("task_complete") => {
                terminal = true;
                seen = true
            }
            Some("turn_aborted") => {
                terminal = false;
                seen = true
            }
            _ => {}
        }
    }
    Ok(seen && terminal)
}

pub(crate) const TAIL_START: u64 = 64 * 1024;
pub(crate) const TAIL_MAX: u64 = 8 * 1024 * 1024;

/// Decide completeness from the end: the last line that is decisive or damaged
/// determines the full-scan result. `None` when no such complete line lies in
/// the bounded window (the caller then falls back to the full scan).
pub(crate) fn complete_tail(mut reader: impl Read + Seek, max: u64) -> Option<bool> {
    let length = reader.seek(SeekFrom::End(0)).ok()?;
    let mut window = TAIL_START.min(max.max(1));
    loop {
        let start = length.saturating_sub(window);
        reader.seek(SeekFrom::Start(start)).ok()?;
        let mut bytes = Vec::new();
        reader.by_ref().take(window).read_to_end(&mut bytes).ok()?;
        let mut lines: Vec<&[u8]> = bytes.split(|b| *b == b'\n').collect();
        // `lines()` yields no line after a final newline.
        if lines.last().is_some_and(|l| l.is_empty()) {
            lines.pop();
        }
        // The first piece may start inside a record; it is never judged.
        let first = usize::from(start > 0);
        for line in lines.iter().skip(first).rev() {
            let line = line.strip_suffix(b"\r").unwrap_or(line);
            let Ok(text) = std::str::from_utf8(line) else {
                return Some(false);
            };
            let Ok(value) = serde_json::from_str::<Value>(text) else {
                return Some(false);
            };
            if value["type"] != "event_msg" {
                continue;
            }
            match value["payload"]["type"].as_str() {
                Some("task_complete") => return Some(true),
                Some("task_started" | "turn_aborted") => return Some(false),
                _ => {}
            }
        }
        if start == 0 {
            return Some(false);
        }
        if window >= max {
            return None;
        }
        window = (window * 2).min(max);
    }
}

/// Completeness with a bounded tail read; the full scan only when undecided.
pub(crate) fn complete(path: &Path) -> Result<bool> {
    if let Some(done) = complete_tail(File::open(path)?, TAIL_MAX) {
        return Ok(done);
    }
    complete_full(File::open(path)?)
}

/// Lengths of two files and whether the shorter is a byte prefix of the longer
/// (equal content when the lengths match). Streams 1 MiB chunks; never holds a
/// whole file in memory.
pub(crate) fn prefix_relation(a: &Path, b: &Path) -> Result<(u64, u64, bool)> {
    let (la, lb) = (fs::metadata(a)?.len(), fs::metadata(b)?.len());
    let mut ra = BufReader::with_capacity(1 << 20, File::open(a)?);
    let mut rb = BufReader::with_capacity(1 << 20, File::open(b)?);
    let mut remaining = la.min(lb);
    let mut ba = vec![0; 1 << 20];
    let mut bb = vec![0; 1 << 20];
    while remaining > 0 {
        let n = remaining.min(1 << 20) as usize;
        ra.read_exact(&mut ba[..n])?;
        rb.read_exact(&mut bb[..n])?;
        if ba[..n] != bb[..n] {
            return Ok((la, lb, false));
        }
        remaining -= n as u64;
    }
    Ok((la, lb, true))
}

/// Results keyed by (path, size, mtime) so unchanged rollouts are not re-read
/// on every inventory. Stored under the catalog directory; best effort.
pub(crate) struct ScanCache {
    file: PathBuf,
    entries: std::collections::BTreeMap<String, bool>,
    used: std::collections::BTreeSet<String>,
    changed: bool,
}
fn stamp(path: &Path) -> Option<String> {
    let m = fs::metadata(path).ok()?;
    let at = m
        .modified()
        .ok()?
        .duration_since(std::time::UNIX_EPOCH)
        .ok()?
        .as_nanos();
    Some(format!("{}\u{0}{}\u{0}{at}", path.display(), m.len()))
}
impl ScanCache {
    pub(crate) fn load(paths: &Paths) -> Self {
        let file = paths
            .root
            .join(crate::layout::SESSION_CATALOG)
            .join("rollout-scan.json");
        let entries = fs::read(&file)
            .ok()
            .and_then(|b| serde_json::from_slice(&b).ok())
            .unwrap_or_default();
        Self {
            file,
            entries,
            used: Default::default(),
            changed: false,
        }
    }
    fn get(&mut self, key: Option<String>, compute: impl FnOnce() -> Result<bool>) -> Result<bool> {
        let Some(key) = key else {
            return compute();
        };
        self.used.insert(key.clone());
        if let Some(v) = self.entries.get(&key) {
            return Ok(*v);
        }
        let v = compute()?;
        self.entries.insert(key, v);
        self.changed = true;
        Ok(v)
    }
    pub(crate) fn complete(&mut self, path: &Path) -> Result<bool> {
        let key = stamp(path).map(|s| format!("complete\u{0}{s}"));
        self.get(key, || complete(path))
    }
    pub(crate) fn prefix(&mut self, a: &Path, b: &Path) -> Result<(u64, u64, bool)> {
        let (la, lb) = (fs::metadata(a)?.len(), fs::metadata(b)?.len());
        let key = stamp(a)
            .zip(stamp(b))
            .map(|(x, y)| format!("prefix\u{0}{x}\u{0}{y}"));
        let mut lengths = (la, lb);
        let same = self.get(key, || {
            let (x, y, same) = prefix_relation(a, b)?;
            lengths = (x, y);
            Ok(same)
        })?;
        Ok((lengths.0, lengths.1, same))
    }
    pub(crate) fn save(self) {
        let stale = self.entries.len() != self.used.len();
        if !self.changed && !stale {
            return;
        }
        let used = self.used;
        let entries: std::collections::BTreeMap<_, _> = self
            .entries
            .into_iter()
            .filter(|(k, _)| used.contains(k))
            .collect();
        if let Some(dir) = self.file.parent()
            && private_dir(dir).is_ok()
            && let Ok(bytes) = serde_json::to_vec(&entries)
        {
            let _ = atomic_write(&self.file, &bytes);
        }
    }
}

#[cfg(test)]
mod tests {
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
}
