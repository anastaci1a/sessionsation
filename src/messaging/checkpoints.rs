//! Read-only, exact-turn public assistant snapshots. Never delivery evidence.
use super::*;
use serde_json::value::RawValue;
use std::{collections::BTreeSet, time::Duration};

/// Entire compact success envelope, including its trailing newline.
pub const MAX_RESPONSE_BYTES: usize = 768 * 1024;

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Request {
    pub version: u32,
    pub session_id: String,
    pub expected_generation: String,
    pub expected_runtime_generation: String,
    pub expected_turn_id: String,
    pub cursor: Option<String>,
}
#[derive(Deserialize)]
struct Page {
    data: Vec<Entry>,
    #[serde(rename = "nextCursor")]
    next: Option<String>,
}
#[derive(Deserialize)]
struct Entry {
    #[serde(rename = "turnId")]
    turn: String,
    item: Box<RawValue>,
}
#[derive(Deserialize)]
struct Kind {
    #[serde(rename = "type")]
    kind: String,
}
#[derive(Deserialize)]
struct Assistant {
    id: String,
    text: String,
    phase: Option<String>,
}
#[derive(Serialize, Deserialize, Default)]
struct Journal {
    epoch: String,
    sequence: u64,
    next: Option<String>,
    seen: BTreeSet<String>,
    events: Vec<Value>,
}
fn digest(value: &impl Serialize) -> Result<String> {
    Ok(format!(
        "{:x}",
        Sha256::digest(serde_json::to_vec(value).map_err(|_| Fault("invalid_request"))?)
    ))
}
fn valid(s: &str) -> bool {
    !s.is_empty() && s.len() <= 256 && !s.chars().any(char::is_control)
}
impl Journal {
    fn position(&self, cursor: Option<&str>) -> Result<u64> {
        let position = if let Some(c) = cursor {
            let (epoch, n) = c.split_once(':').ok_or(Fault("checkpoint_cursor_gap"))?;
            if epoch != self.epoch {
                return Err(Fault("checkpoint_cursor_gap"));
            }
            n.parse::<u64>()
                .map_err(|_| Fault("checkpoint_cursor_gap"))?
        } else {
            self.events
                .first()
                .and_then(|v| v["sequence"].as_u64())
                .unwrap_or(1)
                .saturating_sub(1)
        };
        let first = self
            .events
            .first()
            .and_then(|v| v["sequence"].as_u64())
            .unwrap_or(self.sequence + 1);
        if position > self.sequence || position < first.saturating_sub(1) {
            return Err(Fault("checkpoint_cursor_gap"));
        }
        Ok(position)
    }
    fn response(&self, position: u64, mut result: Value) -> Result<Value> {
        result["events"] = json!([]);
        result["cursor"] = json!(format!("{}:{position}", self.epoch));
        result["has_more"] = json!(position < self.sequence);
        result["byte_limited"] = json!(false);
        // Reserve cursor digit growth and boolean length changes. Count JSON
        // escaping and UTF-8 bytes, not source text characters.
        let mut bytes = serde_json::to_vec(&json!({"api_version":1,"ok":true,"result":result}))
            .map_err(|_| Fault("storage_error"))?
            .len()
            + 1
            + 64;
        let mut events = Vec::new();
        let mut last = position;
        for event in self
            .events
            .iter()
            .filter(|e| e["sequence"].as_u64().unwrap_or(0) > position)
            .take(50)
        {
            let extra = serde_json::to_vec(event)
                .map_err(|_| Fault("storage_error"))?
                .len()
                + usize::from(!events.is_empty());
            if bytes + extra > MAX_RESPONSE_BYTES {
                if events.is_empty() {
                    return Err(Fault("checkpoint_response_limit"));
                }
                result["byte_limited"] = json!(true);
                break;
            }
            bytes += extra;
            last = event["sequence"].as_u64().ok_or(Fault("storage_error"))?;
            events.push(event.clone());
        }
        result["events"] = json!(events);
        result["cursor"] = json!(format!("{}:{last}", self.epoch));
        result["has_more"] = json!(last < self.sequence);
        let wire = serde_json::to_vec(&json!({"api_version":1,"ok":true,"result":result}))
            .map_err(|_| Fault("storage_error"))?;
        if wire.len() + 1 > MAX_RESPONSE_BYTES {
            return Err(Fault("checkpoint_response_limit"));
        }
        Ok(result)
    }
    fn ingest(&mut self, scope: &str, turn: &str, page: Page, now: i64) -> Result<()> {
        if page.data.len() > 50
            || page
                .next
                .as_ref()
                .is_some_and(|c| c.len() > 4096 || Some(c) == self.next.as_ref())
        {
            return Err(Fault("checkpoint_invalid_page"));
        }
        for e in page.data {
            if e.turn != turn {
                return Err(Fault("checkpoint_turn_mismatch"));
            }
            let kind: Kind =
                serde_json::from_str(e.item.get()).map_err(|_| Fault("malformed_response"))?;
            // No reasoning, user input, tool output or unknown payload is materialized.
            if kind.kind != "agentMessage" {
                continue;
            }
            let a: Assistant =
                serde_json::from_str(e.item.get()).map_err(|_| Fault("malformed_response"))?;
            if !matches!(
                a.phase.as_deref(),
                Some("commentary" | "partial_answer" | "final_answer")
            ) {
                continue;
            }
            if !valid(&a.id) || a.text.len() > 65536 {
                return Err(Fault("checkpoint_item_limit"));
            }
            if a.text.is_empty() {
                continue;
            }
            let id = digest(&(scope, &a.id, &a.phase, &a.text))?;
            if self.seen.contains(&id) {
                continue;
            }
            if self.seen.len() >= 4096 {
                return Err(Fault("checkpoint_retention_limit"));
            }
            self.seen.insert(id.clone());
            self.sequence += 1;
            self.events.push(json!({"event_id":id,"sequence":self.sequence,"item_id":a.id,"text":a.text,"phase":a.phase,"observed_at":now,"association":"turn_only","delivery_evidence":false}));
            if self.events.len() > 256 {
                self.events.remove(0);
            }
        }
        self.next = page.next;
        Ok(())
    }
}
async fn turn(c: &mut Transport, thread: &str, expected: &str) -> Result<String> {
    #[derive(Deserialize)]
    struct Turns {
        data: Vec<Turn>,
    }
    #[derive(Deserialize)]
    struct Turn {
        id: String,
        status: String,
    }
    let p: Turns = c.passive_summary(thread).await?;
    let t = p
        .data
        .first()
        .filter(|t| p.data.len() == 1 && t.id == expected)
        .ok_or(Fault("checkpoint_stale_turn"))?;
    if !matches!(
        t.status.as_str(),
        "inProgress" | "completed" | "failed" | "interrupted"
    ) {
        return Err(Fault("checkpoint_turn_unobserved"));
    }
    Ok(t.status.clone())
}
pub async fn read(paths: &Paths, r: Request) -> anyhow::Result<Value> {
    anyhow::ensure!(
        r.version == 1
            && valid(&r.expected_turn_id)
            && r.cursor.as_ref().is_none_or(|s| s.len() <= 128),
        "INVALID_REQUEST"
    );
    let s = crate::catalog::resolve(paths, &r.session_id, &r.expected_generation)?;
    anyhow::ensure!(s.provider == "codex", "UNSUPPORTED_PROVIDER_OPERATION");
    let a = readiness::recorded(paths, &s)?.ok_or(Fault("no_live_attachment"))?;
    anyhow::ensure!(
        a.generation() == r.expected_runtime_generation,
        "STALE_TARGET"
    );
    let scope = digest(&(
        &r.session_id,
        &r.expected_runtime_generation,
        &r.expected_turn_id,
    ))?;
    let directory = paths.root.join("messaging/checkpoints");
    private_dir(&directory)?;
    let file = directory.join(format!("{scope}.json"));
    let lock = lock_file(&file.with_extension("lock"))?;
    lock.try_lock_exclusive()
        .map_err(|_| Fault("checkpoint_observer_busy"))?;
    let mut j: Journal = match fs::read(&file) {
        Ok(b) => serde_json::from_slice(&b).map_err(|_| Fault("storage_error"))?,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Journal {
            epoch: uuid::Uuid::new_v4().to_string(),
            ..Default::default()
        },
        Err(_) => return Err(Fault("storage_error").into()),
    };
    let position = j.position(r.cursor.as_deref())?;
    let replay = position < j.sequence;
    let observed = tokio::time::timeout(Duration::from_secs(8), async {
        let mut c = a.connect().await?;
        let before = turn(&mut c, &a.id, &r.expected_turn_id).await?;
        // Drain retained events before advancing the native scan: byte-limited
        // consumers must not lose unread events to retention while catching up.
        let page = if replay {
            None
        } else {
            Some(
                c.checkpoint_items::<Page>(&a.id, &r.expected_turn_id, j.next.as_deref())
                    .await?,
            )
        };
        let page_observed_at = if replay {
            j.events
                .last()
                .and_then(|v| v["observed_at"].as_i64())
                .unwrap_or(0)
        } else {
            crate::daemon::now()
        };
        let after = turn(&mut c, &a.id, &r.expected_turn_id).await?;
        if before != after {
            return Err(Fault("checkpoint_turn_changed"));
        }
        Ok::<_, Fault>((page, after, page_observed_at))
    })
    .await
    .map_err(|_| Fault("observation_timeout"))?;
    let (page, status, observed_at) = observed?;
    crate::catalog::resolve(paths, &r.session_id, &r.expected_generation)?;
    anyhow::ensure!(
        readiness::recorded(paths, &s)?
            .as_ref()
            .map(Attachment::generation)
            == Some(r.expected_runtime_generation.clone()),
        "STALE_TARGET"
    );
    let now = crate::daemon::now();
    if let Some(page) = page {
        j.ingest(&scope, &r.expected_turn_id, page, observed_at)?;
    }
    // Persist before returning: crash/retry replays identical event IDs and order.
    crate::paths::atomic_write(&file, &serde_json::to_vec(&j)?)?;
    let position = j.position(r.cursor.as_deref())?;
    Ok(j.response(position,
        json!({"version":1,"session_id":r.session_id,"generation":r.expected_generation,"runtime_generation":r.expected_runtime_generation,"turn_id":r.expected_turn_id,"turn_status":status,"scan_pending":j.next.is_some(),"native_page_read":!replay,"observed_at":observed_at,"valid_until":observed_at+5,"checked_at":now,"freshness":if now<observed_at+5{"fresh"}else{"stale"},"history_gap":r.cursor.is_none() && position>0,"association":"turn_only","sends_input":false,"starts_process":false,"delivery_evidence":false})
    )?)
}

#[cfg(test)]
mod tests;
