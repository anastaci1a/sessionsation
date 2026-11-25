//! `last_accessed` for native providers: the time of the conversation's most
//! recent activity, in Unix milliseconds (the unit Codex's `last_accessed` uses).
//! Claude and Gemini read the timestamp of the last transcript record from a
//! bounded tail of the jsonl file; Antigravity and any unreadable tail use the
//! transcript file's mtime. Results are cached per (path, mtime, length), so an
//! unchanged file is never read again. No honest source -> None.
use super::ProviderId;
use std::{
    collections::HashMap,
    fs,
    io::{Read, Seek, SeekFrom},
    path::{Path, PathBuf},
    sync::{Mutex, OnceLock},
    time::{SystemTime, UNIX_EPOCH},
};

pub(crate) const TAIL: u64 = 128 * 1024;
const CACHE_MAX: usize = 4096;
type Stamp = (SystemTime, u64);
type Cache = Mutex<HashMap<PathBuf, (Stamp, Option<i64>)>>;
static CACHE: OnceLock<Cache> = OnceLock::new();

fn millis(t: SystemTime) -> Option<i64> {
    Some(t.duration_since(UNIX_EPOCH).ok()?.as_millis() as i64).filter(|n| *n > 0)
}
fn stamp(path: &Path) -> Option<Stamp> {
    let m = fs::symlink_metadata(path).ok().filter(|m| m.is_file())?;
    Some((m.modified().ok()?, m.len()))
}
/// RFC 3339 -> Unix milliseconds.
pub(crate) fn parse_rfc3339(s: &str) -> Option<i64> {
    let b = s.as_bytes();
    if b.len() < 20 || b[4] != b'-' || b[7] != b'-' || !matches!(b[10], b'T' | b't' | b' ') {
        return None;
    }
    let n = |r: std::ops::Range<usize>| s.get(r)?.parse::<i64>().ok();
    let (y, mo, d, h, mi, sec) = (
        n(0..4)?,
        n(5..7)?,
        n(8..10)?,
        n(11..13)?,
        n(14..16)?,
        n(17..19)?,
    );
    if !(1..=12).contains(&mo) || !(1..=31).contains(&d) || h > 23 || mi > 59 || sec > 60 {
        return None;
    }
    let mut i = 19;
    let mut frac = 0;
    if b.get(i) == Some(&b'.') {
        let start = i + 1;
        i = start;
        while b.get(i).is_some_and(u8::is_ascii_digit) {
            i += 1;
        }
        let digits = &s[start..i];
        if digits.is_empty() {
            return None;
        }
        frac = format!("{:0<3}", &digits[..digits.len().min(3)])
            .parse::<i64>()
            .ok()?;
    }
    let offset = match s.get(i..)? {
        "Z" | "z" => 0,
        z if z.len() == 6 && matches!(z.as_bytes()[0], b'+' | b'-') && z.as_bytes()[3] == b':' => {
            let m = z[1..3].parse::<i64>().ok()? * 60 + z[4..6].parse::<i64>().ok()?;
            if z.starts_with('-') { -m } else { m }
        }
        _ => return None,
    };
    let (y2, m2) = if mo <= 2 {
        (y - 1, mo + 9)
    } else {
        (y, mo - 3)
    };
    let era = y2.div_euclid(400);
    let yoe = y2 - era * 400;
    let doy = (153 * m2 + 2) / 5 + d - 1;
    let doe = yoe * 365 + yoe / 4 - yoe / 100 + doy;
    let days = era * 146097 + doe - 719468;
    let ms = ((days * 86400 + h * 3600 + mi * 60 + sec - offset * 60) * 1000) + frac;
    Some(ms).filter(|n| *n > 0)
}
pub(crate) fn last_record_time(path: &Path, len: u64) -> Option<i64> {
    let mut file = fs::File::open(path).ok()?;
    let from = len.saturating_sub(TAIL);
    file.seek(SeekFrom::Start(from)).ok()?;
    let mut bytes = vec![];
    file.take(TAIL).read_to_end(&mut bytes).ok()?;
    last_record_in(&bytes, from > 0)
}
/// The newest record time in a window of complete records; `cut_front`: the window starts
/// mid-file, so its first (possibly partial) line is dropped.
pub(crate) fn last_record_in(bytes: &[u8], cut_front: bool) -> Option<i64> {
    let text = String::from_utf8_lossy(bytes);
    let mut lines = text.lines();
    if cut_front {
        lines.next(); // possibly cut mid-record
    }
    lines.rev().filter(|l| l.starts_with('{')).find_map(|l| {
        let v: serde_json::Value = serde_json::from_str(l).ok()?;
        ["timestamp", "lastUpdated"]
            .iter()
            .find_map(|k| v[*k].as_str().and_then(parse_rfc3339))
    })
}
fn cached(path: &Path, compute: impl FnOnce(&Stamp) -> Option<i64>) -> Option<i64> {
    let st = stamp(path)?;
    let cache = CACHE.get_or_init(Default::default);
    if let Some((s, v)) = cache.lock().ok()?.get(path)
        && *s == st
    {
        return *v;
    }
    let value = compute(&st);
    let mut cache = cache.lock().ok()?;
    if cache.len() >= CACHE_MAX {
        cache.clear();
    }
    cache.insert(path.to_path_buf(), (st, value));
    value
}
/// `transcript` is the observation's transcript path (jsonl for Claude/Gemini,
/// the conversation db for Antigravity).
pub fn last_accessed(provider: ProviderId, transcript: &Path) -> Option<i64> {
    match provider {
        ProviderId::Claude => super::claude::last_record_ms(transcript),
        ProviderId::Gemini => cached(transcript, |(mtime, len)| {
            last_record_time(transcript, *len).or_else(|| millis(*mtime))
        }),
        ProviderId::Antigravity => {
            // <state>/conversations/<id>.db  ->  <state>/brain/<id>/.system_generated/logs/transcript.jsonl
            let log = transcript
                .file_stem()
                .zip(transcript.parent().and_then(Path::parent))
                .map(|(id, state)| {
                    state
                        .join("brain")
                        .join(id)
                        .join(".system_generated/logs/transcript.jsonl")
                });
            let from = |p: &Path| cached(p, |(m, _)| millis(*m));
            log.and_then(|l| from(&l)).or_else(|| from(transcript))
        }
        ProviderId::Codex => None,
    }
}

#[cfg(test)]
mod tests;
