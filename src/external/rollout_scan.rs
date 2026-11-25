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
mod tests;
