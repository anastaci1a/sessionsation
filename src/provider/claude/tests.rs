use super::*;
mod launch_tests;
mod unit;

/// Reference reader of the head metadata (the cached facts must equal it).
pub(super) fn transcript_metadata(path: &Path) -> (Option<String>, Option<String>, Option<String>) {
    let (mut id, mut cwd, mut at) = (None, None, None);
    let Ok(file) = fs::File::open(path) else {
        return (id, cwd, at);
    };
    let mut reader = BufReader::new(file.take(1024 * 1024));
    let mut line = String::new();
    for _ in 0..64 {
        line.clear();
        if reader.read_line(&mut line).unwrap_or(0) == 0 {
            break;
        }
        let Ok(v) = serde_json::from_str::<Value>(&line) else {
            continue;
        };
        let field = |k: &str| v[k].as_str().filter(|s| !s.is_empty()).map(str::to_owned);
        id = id.or_else(|| field("sessionId"));
        cwd = cwd.or_else(|| field("cwd"));
        at = at.or_else(|| field("timestamp"));
        if id.is_some() && cwd.is_some() && at.is_some() {
            break;
        }
    }
    (id, cwd, at)
}

/// `forkedFrom.sessionId` on the leading records: Claude copies a branched
/// conversation into a new session and stamps each copied record with it.
pub(super) fn forked_from(path: &Path) -> Option<String> {
    let stem = path.file_stem()?.to_str()?;
    let file = fs::File::open(path).ok()?;
    let mut reader = BufReader::new(file.take(1024 * 1024));
    let mut line = String::new();
    for _ in 0..64 {
        line.clear();
        if reader.read_line(&mut line).unwrap_or(0) == 0 {
            break;
        }
        #[derive(serde::Deserialize)]
        struct Fork {
            #[serde(rename = "forkedFrom")]
            forked_from: Option<ForkSource>,
        }
        #[derive(serde::Deserialize)]
        struct ForkSource {
            #[serde(rename = "sessionId")]
            session_id: Option<String>,
        }
        if let Ok(Fork {
            forked_from: Some(ForkSource {
                session_id: Some(id),
            }),
        }) = serde_json::from_str::<Fork>(&line)
            && id != stem
            && uuid::Uuid::parse_str(&id).is_ok()
        {
            return Some(id);
        }
    }
    None
}
