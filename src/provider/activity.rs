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

const TAIL: u64 = 128 * 1024;
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
fn last_record_time(path: &Path, len: u64) -> Option<i64> {
    let mut file = fs::File::open(path).ok()?;
    let from = len.saturating_sub(TAIL);
    file.seek(SeekFrom::Start(from)).ok()?;
    let mut bytes = vec![];
    file.take(TAIL).read_to_end(&mut bytes).ok()?;
    let text = String::from_utf8_lossy(&bytes);
    let mut lines = text.lines();
    if from > 0 {
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
        ProviderId::Claude | ProviderId::Gemini => cached(transcript, |(mtime, len)| {
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
mod tests {
    use super::*;
    use std::io::Write;

    #[test]
    fn rfc3339() {
        assert_eq!(parse_rfc3339("1970-01-02T00:00:00Z"), Some(86_400_000));
        assert_eq!(
            parse_rfc3339("2026-10-29T16:45:47.123Z"),
            Some(1_793_292_347_123)
        );
        assert_eq!(
            parse_rfc3339("2026-10-29T18:45:47+02:00"),
            Some(1_793_292_347_000)
        );
        assert_eq!(parse_rfc3339("garbage"), None);
    }
    #[test]
    fn claude_and_gemini_use_last_record_and_cache_per_mtime() {
        let dir =
            tempfile::tempdir_in(std::env::var("TMPDIR").unwrap_or("/dev/shm".into())).unwrap();
        let p = dir.path().join("s.jsonl");
        let mut f = fs::File::create(&p).unwrap();
        writeln!(f, r#"{{"type":"user","timestamp":"2026-10-29T10:00:00Z"}}"#).unwrap();
        writeln!(
            f,
            r#"{{"type":"assistant","timestamp":"2026-10-29T10:05:00Z"}}"#
        )
        .unwrap();
        writeln!(f, r#"{{"type":"file-history-snapshot"}}"#).unwrap();
        drop(f);
        let want = parse_rfc3339("2026-10-29T10:05:00Z");
        assert_eq!(last_accessed(ProviderId::Claude, &p), want);
        assert_eq!(last_accessed(ProviderId::Gemini, &p), want);
        // Same stamp: served from cache even if the content were unreadable.
        assert_eq!(last_accessed(ProviderId::Claude, &p), want);
        let mut f = fs::OpenOptions::new().append(true).open(&p).unwrap();
        writeln!(f, r#"{{"type":"user","timestamp":"2026-10-29T11:00:00Z"}}"#).unwrap();
        drop(f);
        assert_eq!(
            last_accessed(ProviderId::Claude, &p),
            parse_rfc3339("2026-10-29T11:00:00Z")
        );
        // No timestamps anywhere: mtime fallback.
        let q = dir.path().join("t.jsonl");
        fs::write(&q, "{\"a\":1}\n").unwrap();
        let m = millis(fs::metadata(&q).unwrap().modified().unwrap());
        assert_eq!(last_accessed(ProviderId::Claude, &q), m);
    }
    #[test]
    fn antigravity_uses_brain_log_then_db_mtime_and_missing_is_none() {
        let dir =
            tempfile::tempdir_in(std::env::var("TMPDIR").unwrap_or("/dev/shm".into())).unwrap();
        let id = "11111111-1111-4111-8111-111111111111";
        let db = dir.path().join("conversations").join(format!("{id}.db"));
        fs::create_dir_all(db.parent().unwrap()).unwrap();
        fs::write(&db, "x").unwrap();
        let m = |p: &Path| millis(fs::metadata(p).unwrap().modified().unwrap());
        assert_eq!(last_accessed(ProviderId::Antigravity, &db), m(&db));
        let log = dir
            .path()
            .join("brain")
            .join(id)
            .join(".system_generated/logs/transcript.jsonl");
        fs::create_dir_all(log.parent().unwrap()).unwrap();
        fs::write(&log, "{}\n").unwrap();
        assert_eq!(last_accessed(ProviderId::Antigravity, &db), m(&log));
        let gone = dir.path().join("conversations/none.db");
        assert_eq!(last_accessed(ProviderId::Antigravity, &gone), None);
        assert_eq!(
            last_accessed(ProviderId::Claude, &dir.path().join("nope.jsonl")),
            None
        );
    }
}
