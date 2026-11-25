//! Conversation facts for `api observe` (contract 1.63.0, SM4): the last reply, the last
//! instruction, the last compaction time and (with `--history`) the recent turns, in one
//! shape for every provider.
//!
//! Every field has `<field>_source` and `<field>_reason`: a value comes with the file it was
//! read from and no reason; a value nobody recorded is `null` with the reason, never a guess.
//! Every read is a bounded tail of one provider file (see [`TAILS`]); nothing is cached and no
//! provider process is contacted.
//!
//! - Codex: the conversation's rollout (`<CODEX_HOME>/sessions/.../rollout-*-<thread>.jsonl`):
//!   `event_msg` `user_message` (instruction), `agent_message` (reply), `task_complete` /
//!   `turn_aborted` (turn end), and `compacted` / `event_msg` `context_compacted` (compaction).
//!   Source `rollout`.
//! - Claude: the session transcript (`projects/<key>/<id>.jsonl`): human `user` records (not
//!   meta, not tool results, not the compaction summary), the text blocks of `assistant`
//!   records, and `system` `compact_boundary` records. Source `transcript`.
//! - Antigravity: agy's step log (`brain/<id>/.system_generated/logs/transcript.jsonl`; the
//!   conversation database `conversations/<id>.db` is agy's private binary format):
//!   `USER_INPUT` steps from `USER_EXPLICIT` (the text inside `<USER_REQUEST>`) and
//!   `PLANNER_RESPONSE` steps. agy records no compaction step, so `compacted_at` is null with
//!   the reason. Source `transcript_log`.
//! - Gemini: `supported:false` with [`GEMINI_UNSUPPORTED`]; nothing is read.
use super::ProviderId;
use serde_json::{Value, json};
use std::path::{Path, PathBuf};

pub const FIELDS: [&str; 4] = [
    "last_reply",
    "last_instruction",
    "compacted_at",
    "recent_turns",
];
/// Gemini CLI can no longer run sessions for individual accounts (coordinator probe 11-23).
pub const GEMINI_UNSUPPORTED: &str =
    "Gemini CLI no longer supports individual accounts (migrate to Antigravity)";
pub const NOT_REQUESTED: &str = "history_not_requested: pass --history [N] for the recent turns";
pub const NO_FILE: &str = "no_history_file: the provider has not written this conversation's history file yet (it may never have had a turn)";
pub const UNREADABLE: &str = "history_unreadable: the provider history file could not be read";
pub const AGY_NO_COMPACTION: &str =
    "not_recorded: the Antigravity step log records no compaction step";
/// Default and maximum `--history` turn counts.
pub const DEFAULT_TURNS: usize = 10;
pub const MAX_TURNS: usize = 50;
/// Byte caps for one text: `last_reply` / `last_instruction`, and each turn's texts.
pub const MAX_TEXT: usize = 8192;
pub const MAX_TURN_TEXT: usize = 2048;
/// Tail windows tried in order until everything asked for is found or the file is whole.
pub const TAILS: [u64; 3] = [256 * 1024, 1024 * 1024, 4 * 1024 * 1024];

#[derive(Clone, Debug, PartialEq)]
enum Event {
    Instruction(String, Option<i64>),
    Reply(String, Option<i64>),
    TurnEnd(Option<i64>),
    Compacted(Option<i64>),
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Format {
    Rollout,
    Transcript,
    TranscriptLog,
}
impl Format {
    fn of(provider: ProviderId) -> Option<Self> {
        match provider {
            ProviderId::Codex => Some(Self::Rollout),
            ProviderId::Claude => Some(Self::Transcript),
            ProviderId::Antigravity => Some(Self::TranscriptLog),
            ProviderId::Gemini => None,
        }
    }
    fn source(self) -> &'static str {
        match self {
            Self::Rollout => "rollout",
            Self::Transcript => "transcript",
            Self::TranscriptLog => "transcript_log",
        }
    }
    fn compaction_recorded(self) -> bool {
        self != Self::TranscriptLog
    }
    fn events(self, line: &str) -> Vec<Event> {
        let Ok(v) = serde_json::from_str::<Value>(line) else {
            return vec![];
        };
        match self {
            Self::Rollout => rollout_event(&v).into_iter().collect(),
            Self::Transcript => transcript_event(&v).into_iter().collect(),
            Self::TranscriptLog => log_event(&v).into_iter().collect(),
        }
    }
}

fn time(v: &Value, key: &str) -> Option<i64> {
    v[key]
        .as_str()
        .and_then(crate::reset_credit::claude::parse_time)
}
pub(crate) fn text_blocks(content: &Value) -> Vec<String> {
    match content {
        Value::String(s) => vec![s.clone()],
        Value::Array(items) => items
            .iter()
            .filter(|b| b["type"] == "text")
            .filter_map(|b| b["text"].as_str().map(str::to_owned))
            .collect(),
        _ => vec![],
    }
}
fn nonblank(s: String) -> Option<String> {
    (!s.trim().is_empty()).then_some(s)
}
fn rollout_event(v: &Value) -> Option<Event> {
    let at = time(v, "timestamp");
    let p = &v["payload"];
    match (v["type"].as_str()?, p["type"].as_str()) {
        ("compacted", _) | ("event_msg", Some("context_compacted")) => Some(Event::Compacted(at)),
        ("event_msg", Some("user_message")) => {
            nonblank(p["message"].as_str()?.to_owned()).map(|t| Event::Instruction(t, at))
        }
        ("event_msg", Some("agent_message")) => {
            nonblank(p["message"].as_str()?.to_owned()).map(|t| Event::Reply(t, at))
        }
        ("event_msg", Some("task_complete" | "turn_aborted")) => Some(Event::TurnEnd(at)),
        _ => None,
    }
}
fn transcript_event(v: &Value) -> Option<Event> {
    let at = time(v, "timestamp");
    match v["type"].as_str()? {
        "system" if v["subtype"] == "compact_boundary" => Some(Event::Compacted(at)),
        "assistant" => {
            nonblank(text_blocks(&v["message"]["content"]).join("")).map(|t| Event::Reply(t, at))
        }
        "user" => {
            let content = &v["message"]["content"];
            let tool_only = content
                .as_array()
                .is_some_and(|a| !a.is_empty() && a.iter().all(|b| b["type"] == "tool_result"));
            let human = v["origin"].is_null() || v["origin"]["kind"] == "human";
            if tool_only || v["isMeta"] == true || v["isCompactSummary"] == true || !human {
                return None;
            }
            nonblank(text_blocks(content).join("\n")).map(|t| Event::Instruction(t, at))
        }
        _ => None,
    }
}
fn log_event(v: &Value) -> Option<Event> {
    let at = time(v, "created_at");
    let content = v["content"].as_str()?;
    match v["type"].as_str()? {
        "USER_INPUT" if v["source"] == "USER_EXPLICIT" => {
            let text = content
                .find("<USER_REQUEST>")
                .map(|s| &content[s + "<USER_REQUEST>".len()..])
                .and_then(|rest| rest.find("</USER_REQUEST>").map(|e| &rest[..e]))
                .unwrap_or(content);
            nonblank(text.trim().to_owned()).map(|t| Event::Instruction(t, at))
        }
        "PLANNER_RESPONSE" => nonblank(content.to_owned()).map(|t| Event::Reply(t, at)),
        _ => None,
    }
}

/// `{text, at, truncated}` with the text cut at a character boundary below `limit` bytes.
fn text(t: &str, at: Option<i64>, limit: usize) -> Value {
    let mut end = t.len().min(limit);
    while !t.is_char_boundary(end) {
        end -= 1;
    }
    json!({"text":&t[..end],"at":at,"truncated":end < t.len()})
}

#[derive(Default, Debug)]
struct Turn {
    instruction: Option<(String, Option<i64>)>,
    reply: Option<(String, Option<i64>)>,
    ended_at: Option<i64>,
}
#[derive(Default, Debug)]
struct Facts {
    turns: Vec<Turn>,
    last_reply: Option<(String, Option<i64>)>,
    last_instruction: Option<(String, Option<i64>)>,
    compacted_at: Option<Option<i64>>,
    whole: bool,
}
fn fold(events: Vec<Event>, whole: bool) -> Facts {
    let mut f = Facts {
        whole,
        ..Default::default()
    };
    for e in events {
        match e {
            Event::Instruction(t, at) => {
                f.last_instruction = Some((t.clone(), at));
                f.turns.push(Turn {
                    instruction: Some((t, at)),
                    ..Default::default()
                });
            }
            Event::Reply(t, at) => {
                f.last_reply = Some((t.clone(), at));
                if f.turns.is_empty() {
                    f.turns.push(Turn::default());
                }
                if let Some(turn) = f.turns.last_mut() {
                    turn.reply = Some((t, at));
                }
            }
            Event::TurnEnd(at) => {
                if let Some(turn) = f.turns.last_mut() {
                    turn.ended_at = at;
                }
            }
            Event::Compacted(at) => f.compacted_at = Some(at),
        }
    }
    f
}
fn read(format: Format, path: &Path, wanted_turns: usize) -> Option<Facts> {
    let mut last = None;
    for limit in TAILS {
        let (body, whole) = super::session_meters::tail(path, limit)?;
        let events = body.lines().flat_map(|l| format.events(l)).collect();
        let f = fold(events, whole);
        let complete = f.last_reply.is_some()
            && f.last_instruction.is_some()
            && f.turns.len() > wanted_turns
            && (f.compacted_at.is_some() || !format.compaction_recorded());
        if complete || whole {
            return Some(f);
        }
        last = Some(f);
    }
    last
}

/// One field: value, source, reason (a value has a source and no reason; null has a reason).
fn field(out: &mut Value, name: &str, value: Value, source: &str, reason: &str) {
    let known = !value.is_null();
    out[name] = value;
    out[format!("{name}_source")] = json!(known.then_some(source));
    out[format!("{name}_reason")] = json!((!known).then_some(reason));
}
/// The block with every field null and one reason.
pub fn unknown(supported: bool, reason: &str, history: Option<usize>) -> Value {
    let mut out = json!({"supported":supported,"reason":if supported {Value::Null} else {json!(reason)},
        "source":null,"history_turns":history});
    for f in FIELDS {
        let r = if f == "recent_turns" && history.is_none() && supported {
            NOT_REQUESTED
        } else {
            reason
        };
        field(&mut out, f, Value::Null, "", r);
    }
    out
}

fn block(format: Format, path: Option<&Path>, history: Option<usize>) -> Value {
    let Some(path) = path.filter(|p| p.is_file()) else {
        return unknown(true, NO_FILE, history);
    };
    let wanted = history.unwrap_or(0).min(MAX_TURNS);
    let Some(f) = read(format, path, wanted) else {
        return unknown(true, UNREADABLE, history);
    };
    let src = format.source();
    let window = if f.whole {
        "the whole file".to_owned()
    } else {
        format!(
            "the newest {} MiB of the file",
            TAILS[TAILS.len() - 1] / (1024 * 1024)
        )
    };
    let mut out = json!({"supported":true,"reason":null,"source":src,"history_turns":history});
    let at = |x: &Option<(String, Option<i64>)>, limit: usize| {
        x.as_ref()
            .map_or(Value::Null, |(t, at)| text(t, *at, limit))
    };
    field(
        &mut out,
        "last_reply",
        at(&f.last_reply, MAX_TEXT),
        src,
        &format!("not_recorded: no reply in {window}"),
    );
    field(
        &mut out,
        "last_instruction",
        at(&f.last_instruction, MAX_TEXT),
        src,
        &format!("not_recorded: no instruction in {window}"),
    );
    let compacted = if format.compaction_recorded() {
        match f.compacted_at {
            Some(Some(t)) => (json!(t), String::new()),
            Some(None) => (
                Value::Null,
                "not_recorded: the compaction record has no timestamp".to_owned(),
            ),
            None => (
                Value::Null,
                format!("not_compacted: no compaction in {window}"),
            ),
        }
    } else {
        (Value::Null, AGY_NO_COMPACTION.to_owned())
    };
    field(&mut out, "compacted_at", compacted.0, src, &compacted.1);
    match history {
        None => field(&mut out, "recent_turns", Value::Null, src, NOT_REQUESTED),
        Some(_) => {
            let skip = f.turns.len().saturating_sub(wanted);
            let turns: Vec<Value> = f.turns[skip..]
                .iter()
                .map(|t| {
                    let started = t.instruction.as_ref().and_then(|x| x.1);
                    let ended = t.ended_at.or_else(|| t.reply.as_ref().and_then(|x| x.1));
                    json!({"instruction":at(&t.instruction, MAX_TURN_TEXT),"reply":at(&t.reply, MAX_TURN_TEXT),
                        "started_at":started,"ended_at":ended})
                })
                .collect();
            field(&mut out, "recent_turns", json!(turns), src, "");
        }
    }
    out
}

/// The conversation block of one session. `path` is the provider file (see [`history_file`]).
pub fn observe(provider: ProviderId, path: Option<&Path>, history: Option<usize>) -> Value {
    match Format::of(provider) {
        Some(format) => block(format, path, history),
        None => unknown(false, GEMINI_UNSUPPORTED, history),
    }
}
/// The provider's own file for this conversation, if it exists.
pub fn history_file(provider: ProviderId, home: &Path, conversation: &str) -> Option<PathBuf> {
    match provider {
        ProviderId::Codex => crate::external::rollout_by_thread(home, conversation)
            .ok()
            .flatten(),
        ProviderId::Claude => super::claude::find_transcript(home, conversation),
        ProviderId::Antigravity => {
            Some(super::antigravity::transcript_log(home, conversation)).filter(|p| p.is_file())
        }
        ProviderId::Gemini => None,
    }
}
/// Whether the provider supports observation at all (Gemini does not).
pub fn supported(provider: ProviderId) -> Result<(), &'static str> {
    match provider {
        ProviderId::Gemini => Err(GEMINI_UNSUPPORTED),
        _ => Ok(()),
    }
}

#[cfg(test)]
mod tests;
