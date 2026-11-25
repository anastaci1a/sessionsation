//! Session events in one shape for every provider (contract 1.64.0, SM5).
//!
//! One bounded journal, `registry/events.jsonl`, holds every event as
//! `{seq, session_id, provider, kind, at, source, provider_conversation_id, detail}`;
//! `api events --after SEQ [--wait N]` reads it with an explicit `gap` flag. Sources:
//!
//! - Claude: Claude Code's own hooks. Sessionsation installs one async command hook per
//!   event it needs into the account's settings (see [`super::claude::hooks`]); the hook
//!   appends a content-free line to `<home>/.sessionsation/hook-events.jsonl`, and
//!   [`ingest`] moves new lines into the journal (the supervisor every
//!   [`INGEST_SECONDS`], and every `api events` / full `api state` call). Source `hook`.
//! - Codex and Antigravity: the supervisor compares each full state observation of a
//!   running session's turn facts (Codex rollout/app-server, agy step log) with the
//!   previous one ([`observe_turns`]) and emits turn starts and ends. A Codex turn end is
//!   identified by its rollout timestamp, so a turn shorter than the refresh is still
//!   reported once; agy turns are seen as transitions only. Source `supervisor`.
//! - Compactions Sessionsation ran itself ([`record`]). Source `operation`.
//! - Gemini: none (its CLI no longer signs in individual accounts).
//!
//! No event carries prompt, reply or tool text.
use crate::{
    live_activity::{self, TurnFacts},
    paths::{Paths, atomic_write, lock_file, private_dir},
    provider::ProviderId,
};
use anyhow::{Result, ensure};
use fs2::FileExt;
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use std::{
    collections::{BTreeMap, BTreeSet},
    fs,
    io::{Read, Seek, SeekFrom, Write},
    path::{Path, PathBuf},
    time::Duration,
};

/// Every event kind, in documentation order.
pub const KINDS: [&str; 10] = [
    "turn_started",
    "turn_completed",
    "subagent_started",
    "subagent_stopped",
    "background_task_started",
    "background_task_finished",
    "compacted",
    "idle",
    "session_started",
    "session_ended",
];
/// Events kept; when the journal grows past this the oldest are dropped down to [`KEEP`].
pub const MAX_EVENTS: usize = 10_000;
const KEEP: usize = 5_000;
pub const MAX_WAIT_SECONDS: u64 = 300;
pub const DEFAULT_LIMIT: usize = 500;
pub const MAX_LIMIT: usize = 2_000;
/// Supervisor ingest interval (`SESSIONSATION_EVENTS_SECONDS`, 0 disables).
pub const INGEST_SECONDS: u64 = 2;
/// Bytes read from one hook file per ingest pass.
const MAX_INGEST_BYTES: u64 = 4 * 1024 * 1024;
/// A fully ingested hook file larger than this is emptied (under the writers' lock).
const TRUNCATE_AT: u64 = 1024 * 1024;
/// Trackers and observations not touched for this long are dropped.
const PRUNE_SECONDS: i64 = 7 * 86_400;
/// Out-of-order stops remembered per conversation (async hooks may finish out of order).
const MAX_EARLY_STOPS: usize = 64;

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct Event {
    pub seq: u64,
    /// The Sessionsation session id; null for a conversation the catalog does not know.
    pub session_id: Option<String>,
    pub provider: String,
    pub kind: String,
    /// Unix seconds when the provider (hook) or the supervisor saw it.
    pub at: i64,
    /// `hook` | `supervisor` | `operation`
    pub source: String,
    pub provider_conversation_id: Option<String>,
    pub detail: Value,
}

/// Running subagents and background tasks of one conversation, from its hook events.
#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize)]
pub struct Tasks {
    /// Unix seconds of the last `SessionStart` (startup or resume) of this conversation.
    #[serde(default)]
    pub process_started_at: Option<i64>,
    #[serde(default)]
    pub subagents: BTreeMap<String, String>,
    #[serde(default)]
    pub early_stops: BTreeSet<String>,
    #[serde(default)]
    pub background: BTreeSet<String>,
    /// Unix seconds of the newest `PostCompact` event.
    #[serde(default)]
    pub last_compacted_at: Option<i64>,
    #[serde(default)]
    pub ended: bool,
    #[serde(default)]
    pub last_event_at: i64,
}

#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize)]
struct Observed {
    state: String,
    #[serde(default)]
    ended_at: Option<String>,
    #[serde(default)]
    runtime: Option<i64>,
    at: i64,
}

#[derive(Clone, Debug, Default, Serialize, Deserialize)]
struct State {
    #[serde(default)]
    schema_version: u32,
    /// The last assigned sequence number.
    #[serde(default)]
    latest_seq: u64,
    /// The oldest sequence number still kept (0 while the journal is empty).
    #[serde(default)]
    oldest_seq: u64,
    #[serde(default)]
    kept: usize,
    /// Hook file → bytes already ingested.
    #[serde(default)]
    cursors: BTreeMap<String, u64>,
    /// Hook lines that could not be read (not JSON, or no session id).
    #[serde(default)]
    unreadable_lines: u64,
    /// Supervisor-observed sessions (Codex, Antigravity): session id → last look.
    #[serde(default)]
    observed: BTreeMap<String, Observed>,
    /// Claude conversation id → tracker.
    #[serde(default)]
    tasks: BTreeMap<String, Tasks>,
    /// Hook file → Unix seconds of the newest ingested line.
    #[serde(default)]
    last_hook_at: BTreeMap<String, i64>,
}

fn journal(paths: &Paths) -> PathBuf {
    paths.root.join(crate::layout::EVENTS)
}
fn state_file(paths: &Paths) -> PathBuf {
    paths.root.join(crate::layout::EVENTS_STATE)
}
fn load(paths: &Paths) -> State {
    fs::read(state_file(paths))
        .ok()
        .and_then(|b| serde_json::from_slice(&b).ok())
        .unwrap_or_default()
}
/// One read-modify-write of the state and journal at a time; never held across an await.
fn with_state<R>(paths: &Paths, f: impl FnOnce(&mut State, &mut Vec<Event>) -> R) -> Result<R> {
    private_dir(&paths.root.join(crate::layout::REGISTRY))?;
    let lock = lock_file(&paths.root.join(crate::layout::EVENTS_LOCK))?;
    lock.lock_exclusive()?;
    let mut state = load(paths);
    let before = serde_json::to_vec(&state)?;
    let mut fresh = Vec::new();
    let out = f(&mut state, &mut fresh);
    if !fresh.is_empty() {
        append(paths, &mut state, fresh)?;
    }
    state.schema_version = 1;
    let after = serde_json::to_vec(&state)?;
    if before != after {
        atomic_write(&state_file(paths), &after)?;
    }
    Ok(out)
}
/// Number the new events, append them, and trim the journal past [`MAX_EVENTS`].
fn append(paths: &Paths, state: &mut State, fresh: Vec<Event>) -> Result<()> {
    // A crash between the journal append and the state write leaves numbered lines the state
    // does not know: continue after them, never reuse a number.
    state.latest_seq = state.latest_seq.max(last_seq(paths));
    let mut text = String::new();
    for mut event in fresh {
        state.latest_seq += 1;
        event.seq = state.latest_seq;
        if state.oldest_seq == 0 {
            state.oldest_seq = event.seq;
        }
        text.push_str(&serde_json::to_string(&event)?);
        text.push('\n');
        state.kept += 1;
    }
    let mut file = fs::OpenOptions::new()
        .create(true)
        .append(true)
        .open(journal(paths))?;
    file.write_all(text.as_bytes())?;
    if state.kept > MAX_EVENTS {
        let all = read_all(paths);
        let keep = &all[all.len().saturating_sub(KEEP)..];
        let mut out = String::new();
        for e in keep {
            out.push_str(&serde_json::to_string(e)?);
            out.push('\n');
        }
        atomic_write(&journal(paths), out.as_bytes())?;
        state.kept = keep.len();
        state.oldest_seq = keep.first().map_or(0, |e| e.seq);
    }
    Ok(())
}
/// The sequence number of the journal's last line (0 when there is none).
fn last_seq(paths: &Paths) -> u64 {
    super::session_meters::tail(&journal(paths), 16 * 1024)
        .and_then(|(text, _)| {
            text.lines()
                .rev()
                .find_map(|l| serde_json::from_str::<Value>(l).ok()?["seq"].as_u64())
        })
        .unwrap_or(0)
}
fn read_all(paths: &Paths) -> Vec<Event> {
    fs::read_to_string(journal(paths))
        .unwrap_or_default()
        .lines()
        .filter_map(|l| serde_json::from_str(l).ok())
        .collect()
}

fn event(
    provider: ProviderId,
    session_id: Option<String>,
    conversation: Option<&str>,
    kind: &str,
    at: i64,
    source: &str,
    detail: Value,
) -> Event {
    Event {
        seq: 0,
        session_id,
        provider: provider.as_str().into(),
        kind: kind.into(),
        at,
        source: source.into(),
        provider_conversation_id: conversation.map(str::to_owned),
        detail,
    }
}

/// Journal one event of an operation Sessionsation ran itself (a finished compaction).
pub fn record(
    paths: &Paths,
    provider: ProviderId,
    session_id: &str,
    conversation: &str,
    kind: &str,
    detail: Value,
) -> Result<()> {
    with_state(paths, |_, fresh| {
        fresh.push(event(
            provider,
            Some(session_id.into()),
            Some(conversation),
            kind,
            crate::daemon::now(),
            "operation",
            detail,
        ));
    })
}

// ------------------------------------------------------------------ Claude hooks

/// Where a Claude account's hook writes its lines (inside the account home).
pub fn hook_file(home: &Path) -> PathBuf {
    home.join(".sessionsation/hook-events.jsonl")
}
pub(crate) fn hook_lock(home: &Path) -> PathBuf {
    home.join(".sessionsation/hook-events.lock")
}

/// The content-free fields the hook keeps from one Claude Code hook payload.
pub fn hook_line(input: &Value, at_ms: i64) -> Option<Value> {
    let name = input["hook_event_name"].as_str()?;
    let session = input["session_id"].as_str()?;
    let short = |v: &Value| v.as_str().map(|s| s.chars().take(120).collect::<String>());
    Some(
        json!({"at_ms":at_ms,"event":short(&json!(name)),"session_id":short(&json!(session)),
        "agent_id":short(&input["agent_id"]),"agent_type":short(&input["agent_type"]),
        "source":short(&input["source"]),"reason":short(&input["reason"]),
        "trigger":short(&input["trigger"]),"notification_type":short(&input["notification_type"]),
        "tool_name":short(&input["tool_name"]),
        "background_task_id":short(&input["tool_response"]["backgroundTaskId"]),
        "error":short(&input["error"])}),
    )
}

/// The event (kind, detail) of one hook line, if it maps to one.
fn from_hook(line: &Value) -> Option<(&'static str, Value)> {
    let s = |k: &str| line[k].as_str().map(str::to_owned);
    Some(match line["event"].as_str()? {
        "UserPromptSubmit" => ("turn_started", json!({})),
        "Stop" => ("turn_completed", json!({"status":"completed"})),
        "StopFailure" => (
            "turn_completed",
            json!({"status":"failed","error":s("error")}),
        ),
        "SubagentStart" => (
            "subagent_started",
            json!({"agent_id":s("agent_id")?,"agent_type":s("agent_type")}),
        ),
        "SubagentStop" => (
            "subagent_stopped",
            json!({"agent_id":s("agent_id")?,"agent_type":s("agent_type")}),
        ),
        "PostToolUse" => (
            "background_task_started",
            json!({"task_id":s("background_task_id")?,"tool_name":s("tool_name")}),
        ),
        "PostCompact" => ("compacted", json!({"trigger":s("trigger")})),
        "Notification" if line["notification_type"] == "idle_prompt" => ("idle", json!({})),
        "SessionStart" => ("session_started", json!({"source":s("source")})),
        "SessionEnd" => ("session_ended", json!({"reason":s("reason")})),
        _ => return None,
    })
}

/// Apply one hook event to its conversation's tracker.
fn track(tasks: &mut Tasks, kind: &str, detail: &Value, at: i64) {
    tasks.last_event_at = tasks.last_event_at.max(at);
    let id = |k: &str| detail[k].as_str().map(str::to_owned);
    match kind {
        "session_started" => {
            // A new or resumed process has no subagents or background tasks yet.
            if matches!(detail["source"].as_str(), Some("startup" | "resume") | None) {
                *tasks = Tasks {
                    process_started_at: Some(at),
                    last_event_at: at,
                    ..Default::default()
                };
            }
        }
        "session_ended" => {
            tasks.subagents.clear();
            tasks.background.clear();
            tasks.early_stops.clear();
            tasks.ended = true;
        }
        "subagent_started" => {
            if let Some(a) = id("agent_id")
                && !tasks.early_stops.remove(&a)
            {
                tasks
                    .subagents
                    .insert(a, id("agent_type").unwrap_or_default());
            }
        }
        "subagent_stopped" => {
            if let Some(a) = id("agent_id")
                && tasks.subagents.remove(&a).is_none()
                && tasks.early_stops.len() < MAX_EARLY_STOPS
            {
                tasks.early_stops.insert(a);
            }
        }
        "background_task_started" => {
            if let Some(t) = id("task_id") {
                tasks.background.insert(t);
            }
        }
        "compacted" => {
            tasks.last_compacted_at = Some(tasks.last_compacted_at.map_or(at, |c| c.max(at)))
        }
        _ => {}
    }
}

/// Read the new lines of one hook file into `fresh`; returns the newest line time.
fn ingest_file(
    paths: &Paths,
    state: &mut State,
    home: &Path,
    fresh: &mut Vec<Event>,
) -> Option<i64> {
    let file = hook_file(home);
    let key = file.to_string_lossy().into_owned();
    let meta = fs::symlink_metadata(&file).ok().filter(|m| m.is_file())?;
    let mut offset = state.cursors.get(&key).copied().unwrap_or(0);
    if meta.len() < offset {
        // Emptied or replaced by something else: start over.
        offset = 0;
    }
    if meta.len() == offset {
        return None;
    }
    let mut f = fs::File::open(&file).ok()?;
    f.seek(SeekFrom::Start(offset)).ok()?;
    let mut bytes = vec![];
    f.take(MAX_INGEST_BYTES).read_to_end(&mut bytes).ok()?;
    // Only whole lines; a line still being written waits for the next pass.
    let whole = bytes.iter().rposition(|b| *b == b'\n').map_or(0, |i| i + 1);
    let mut sessions: BTreeMap<String, Option<String>> = BTreeMap::new();
    let mut newest = None;
    for line in bytes[..whole].split(|b| *b == b'\n') {
        if line.is_empty() {
            continue;
        }
        let Some(v) = serde_json::from_slice::<Value>(line)
            .ok()
            .filter(|v| v["session_id"].is_string())
        else {
            state.unreadable_lines += 1;
            continue;
        };
        let at = v["at_ms"].as_i64().unwrap_or(0) / 1000;
        newest = Some(newest.map_or(at, |n: i64| n.max(at)));
        let Some((kind, detail)) = from_hook(&v) else {
            continue;
        };
        let conversation = v["session_id"].as_str().unwrap_or_default().to_owned();
        let tasks = state.tasks.entry(conversation.clone()).or_default();
        track(tasks, kind, &detail, at);
        let session = sessions
            .entry(conversation.clone())
            .or_insert_with(|| {
                crate::catalog::id_for_conversation(paths, ProviderId::Claude, &conversation)
            })
            .clone();
        fresh.push(event(
            ProviderId::Claude,
            session,
            Some(&conversation),
            kind,
            at,
            "hook",
            detail,
        ));
    }
    offset += whole as u64;
    state.cursors.insert(key.clone(), offset);
    if let Some(at) = newest {
        let slot = state.last_hook_at.entry(key.clone()).or_insert(at);
        *slot = (*slot).max(at);
    }
    if offset == meta.len() && offset > TRUNCATE_AT {
        // Empty the consumed file under the writers' lock, re-checking that nothing was added.
        if let Ok(lock) = lock_file(&hook_lock(home))
            && lock.lock_exclusive().is_ok()
            && fs::metadata(&file).is_ok_and(|m| m.len() == offset)
            && fs::OpenOptions::new()
                .write(true)
                .open(&file)
                .and_then(|f| f.set_len(0))
                .is_ok()
        {
            state.cursors.insert(key, 0);
        }
    }
    newest
}

/// Move new hook lines of every Claude account into the journal.
pub fn ingest(paths: &Paths) -> Result<usize> {
    let homes: Vec<PathBuf> = super::claude::enrolled(paths)
        .unwrap_or_default()
        .into_iter()
        .map(|a| a.home)
        .collect();
    if !homes.iter().any(|h| hook_file(h).is_file()) {
        return Ok(0);
    }
    with_state(paths, |state, fresh| {
        for home in &homes {
            ingest_file(paths, state, home, fresh);
        }
        let now = crate::daemon::now();
        state
            .tasks
            .retain(|_, t| now - t.last_event_at < PRUNE_SECONDS);
        fresh.len()
    })
}

/// Newest ingested hook line of a Claude account home (Unix seconds).
pub fn last_hook_at(paths: &Paths, home: &Path) -> Option<i64> {
    load(paths)
        .last_hook_at
        .get(hook_file(home).to_string_lossy().as_ref())
        .copied()
}

/// Every conversation's tracker (read-only; the last ingest's view).
pub fn tasks(paths: &Paths) -> BTreeMap<String, Tasks> {
    load(paths).tasks
}

// ------------------------------------------------------------------ supervisor

/// One running session's turn facts from a full state observation.
#[derive(Clone, Debug)]
pub struct Turns {
    pub session_id: String,
    pub provider: ProviderId,
    pub conversation_id: String,
    /// The process identity (start time) of the observed runtime; a new one re-baselines.
    pub runtime: Option<i64>,
    pub facts: TurnFacts,
}

/// How a provider's events are produced.
pub fn source(provider: ProviderId) -> Option<&'static str> {
    match provider {
        ProviderId::Claude => Some("hook"),
        ProviderId::Codex | ProviderId::Antigravity => Some("supervisor"),
        ProviderId::Gemini => None,
    }
}

/// The events one round of observations implies, updating the stored looks. Pure of I/O.
fn compare(state: &mut State, rounds: &[Turns], now: i64) -> Vec<Event> {
    let mut out = vec![];
    for t in rounds
        .iter()
        .filter(|t| source(t.provider) == Some("supervisor"))
    {
        let kind = live_activity::classify(&t.facts);
        let prior = state.observed.get(&t.session_id).cloned();
        let look = Observed {
            state: kind.into(),
            ended_at: t
                .facts
                .ended_at
                .clone()
                .or_else(|| prior.as_ref().and_then(|p| p.ended_at.clone())),
            runtime: t.runtime,
            at: now,
        };
        state.observed.insert(t.session_id.clone(), look);
        // The first look at a runtime only records it.
        let Some(prior) = prior.filter(|p| p.runtime == t.runtime) else {
            continue;
        };
        if kind == "unknown" {
            continue;
        }
        let emit = |kind: &str, detail: Value| {
            event(
                t.provider,
                Some(t.session_id.clone()),
                Some(&t.conversation_id),
                kind,
                now,
                "supervisor",
                detail,
            )
        };
        let ended = matches!(kind, "turn_ended" | "error")
            && match (&t.facts.ended_at, &prior.ended_at) {
                (Some(now), Some(before)) => now != before,
                (Some(_), None) => true,
                (None, _) => matches!(prior.state.as_str(), "working" | "blocked"),
            };
        if matches!(kind, "working" | "blocked")
            && !matches!(prior.state.as_str(), "working" | "blocked")
        {
            out.push(emit("turn_started", json!({})));
        }
        if ended {
            let status = t.facts.last_status.clone().unwrap_or_else(|| {
                if kind == "error" {
                    "failed".into()
                } else {
                    "completed".into()
                }
            });
            out.push(emit(
                "turn_completed",
                json!({"status":status,"error":t.facts.error.as_deref().filter(|_| kind == "error")}),
            ));
        }
    }
    state.observed.retain(|_, o| now - o.at < PRUNE_SECONDS);
    out
}

/// Journal the turn starts and ends a full state observation shows (Codex, Antigravity).
pub fn observe_turns(paths: &Paths, rounds: &[Turns]) -> Result<usize> {
    if !rounds
        .iter()
        .any(|t| source(t.provider) == Some("supervisor"))
    {
        return Ok(0);
    }
    with_state(paths, |state, fresh| {
        let events = compare(state, rounds, crate::daemon::now());
        let n = events.len();
        fresh.extend(events);
        n
    })
}

// ------------------------------------------------------------------ reading

pub struct Query<'a> {
    pub after: u64,
    pub wait: u64,
    pub session: Option<&'a str>,
    pub kind: Option<&'a str>,
    pub limit: Option<usize>,
}

/// `api events`: events after `after`, optionally of one session or kind, waiting up to
/// `wait` seconds for the first one. `cursor` is what to pass as the next `after`; `gap` is
/// true when events after `after` were already dropped from the journal.
pub async fn read(paths: &Paths, q: Query<'_>) -> Result<Value> {
    ensure!(
        q.wait <= MAX_WAIT_SECONDS,
        "INVALID_ARGUMENT: --wait is at most {MAX_WAIT_SECONDS} seconds"
    );
    let limit = q.limit.unwrap_or(DEFAULT_LIMIT);
    ensure!(
        (1..=MAX_LIMIT).contains(&limit),
        "INVALID_ARGUMENT: --limit is 1 to {MAX_LIMIT}"
    );
    if let Some(kind) = q.kind {
        ensure!(
            KINDS.contains(&kind),
            "INVALID_ARGUMENT: --kind is one of {}",
            KINDS.join(", ")
        );
    }
    let deadline = std::time::Instant::now() + Duration::from_secs(q.wait);
    loop {
        let _ = ingest(paths);
        let state = load(paths);
        let latest = state.latest_seq;
        let gap = state.oldest_seq > 0 && q.after + 1 < state.oldest_seq && latest > q.after;
        let mut events = vec![];
        let mut cursor = q.after;
        let mut more = false;
        if latest > q.after {
            cursor = latest;
            for e in read_all(paths).into_iter().filter(|e| e.seq > q.after) {
                if q.session
                    .is_some_and(|s| e.session_id.as_deref() != Some(s))
                    || q.kind.is_some_and(|k| e.kind != k)
                {
                    continue;
                }
                if events.len() == limit {
                    more = true;
                    cursor = events.last().map_or(cursor, |e: &Event| e.seq);
                    break;
                }
                events.push(e);
            }
        }
        if !events.is_empty() || std::time::Instant::now() >= deadline {
            return Ok(json!({"events":events,"cursor":cursor,"gap":gap,
                "oldest_seq":if state.oldest_seq == 0 {Value::Null} else {json!(state.oldest_seq)},
                "latest_seq":latest,"more":more,"waited":q.wait > 0}));
        }
        tokio::time::sleep(Duration::from_millis(500)).await;
    }
}

/// The supervisor loop (`SESSIONSATION_EVENTS_SECONDS`, default 2; 0 disables).
pub async fn monitor(paths: Paths) {
    let every = std::env::var("SESSIONSATION_EVENTS_SECONDS")
        .ok()
        .and_then(|v| v.parse().ok())
        .unwrap_or(INGEST_SECONDS);
    if every == 0 {
        return;
    }
    loop {
        let _ = ingest(&paths);
        tokio::time::sleep(crate::timing::secs(every)).await;
    }
}

pub fn capability() -> Value {
    json!({"since_contract":"1.64.0","command":"events",
        "args":["--after","--wait","--session","--kind","--limit"],
        "keys":["seq","session_id","provider","kind","at","source","provider_conversation_id","detail"],
        "kinds":KINDS,"cursor":"pass the returned cursor as the next --after",
        "gap":"true when events after --after were already dropped (the journal keeps the newest events)",
        "max_events":MAX_EVENTS,"kept_after_trim":KEEP,"max_wait_seconds":MAX_WAIT_SECONDS,
        "default_limit":DEFAULT_LIMIT,"max_limit":MAX_LIMIT,"journal":crate::layout::EVENTS,
        "sources":{"claude":"hook","codex":"supervisor","antigravity":"supervisor","gemini":null},
        "providers":{
            "claude":{"supported":true,"kinds":["turn_started","turn_completed","subagent_started","subagent_stopped","background_task_started","compacted","idle","session_started","session_ended"],
                "via":"Claude Code hooks installed by Sessionsation (api hooks)","ingest_seconds":INGEST_SECONDS},
            "codex":{"supported":true,"kinds":["turn_started","turn_completed","compacted"],
                "via":"full state observations of the rollout/app-server turn facts (daemon refresh)","short_turns":"reported once (rollout turn-end timestamp)"},
            "antigravity":{"supported":true,"kinds":["turn_started","turn_completed"],
                "via":"full state observations of the agy step log","short_turns":"may be missed between two observations"},
            "gemini":{"supported":false,"reason":super::conversation::GEMINI_UNSUPPORTED}},
        "never_included":["prompt text","reply text","tool input or output"],
        "turn_events":"api turn-events stays the Codex turn-notify journal"})
}

#[cfg(test)]
mod tests;
