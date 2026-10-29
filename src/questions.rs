//! Pending agent questions (contract revision 23).
//!
//! Detection is passive: provider history is scanned incrementally (byte offset
//! cache per history file) and never contacts a provider. Answering is explicit,
//! generation-checked and journaled before any native write; an attempted answer
//! is never sent twice.
//!
//! - Codex `request_user_input` (Plan mode, blocking): the app-server sends the
//!   server request `item/tool/requestUserInput` to subscribed clients and replays
//!   it to a client that subscribes with `thread/resume`; the first response
//!   `{answers:{<question id>:{answers:[..]}}}` wins and every client receives
//!   `serverRequest/resolved`.
//! - Codex `request_user_input_async` (Default mode): the tool returns at once and
//!   the reply is an ordinary later user message. The answer is delivered through
//!   the public non-steering queue (`api send` mode `queue`), as the TUI does.
//! - Claude `AskUserQuestion`: hosted sessions answer the `can_use_tool` request
//!   with `updatedInput.answers`; terminal sessions are read-only.
//! - Gemini `ask_user`: Gemini CLI records tool calls only after completion, so a
//!   pending question is not visible outside its terminal.
use crate::{catalog, paths::Paths, provider::ProviderId};
use anyhow::{Context, Result, bail, ensure};
use fs2::FileExt;
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use sha2::{Digest, Sha256};
use std::{
    collections::BTreeMap,
    fs,
    io::{Read, Seek, SeekFrom},
    os::unix::fs::MetadataExt,
    path::{Path, PathBuf},
    time::Duration,
};

pub const KIND_CODEX_BLOCKING: &str = "codex_request_user_input";
pub const KIND_CODEX_ASYNC: &str = "codex_request_user_input_async";
pub const KIND_CLAUDE: &str = "claude_ask_user_question";
pub const KIND_CLAUDE_HOSTED: &str = "claude_hosted_ask_user_question";
const MAX_QUESTIONS: usize = 16;
const MAX_TEXT: usize = 4096;
const MAX_ANSWER_BYTES: usize = 16 * 1024;
const SCAN_VERSION: u32 = 1;

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct QuestionOption {
    pub id: String,
    pub label: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub description: Option<String>,
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct Question {
    pub id: String,
    pub text: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub header: Option<String>,
    pub options: Vec<QuestionOption>,
    pub multi: bool,
    pub allow_free_text: bool,
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct Pending {
    pub request_id: String,
    pub asked_at: i64,
    pub kind: String,
    /// The agent's turn waits for the answer (Codex Plan mode, Claude).
    pub blocking: bool,
    #[serde(default)]
    pub turn_id: Option<String>,
    pub questions: Vec<Question>,
    /// Filled when projected for a row; never cached.
    #[serde(default)]
    pub answerable: bool,
    #[serde(default)]
    pub unanswerable_reason: Option<String>,
}

fn clip(s: &str) -> String {
    let mut out = String::new();
    for c in s.chars() {
        if out.len() + c.len_utf8() > MAX_TEXT {
            break;
        }
        out.push(c);
    }
    out
}

fn options_from(values: Option<&Vec<Value>>) -> Vec<QuestionOption> {
    values
        .into_iter()
        .flatten()
        .enumerate()
        .filter_map(|(i, o)| {
            let (label, description) = match o {
                Value::String(s) => (s.clone(), None),
                _ => (
                    o["label"].as_str()?.to_owned(),
                    o["description"].as_str().map(clip),
                ),
            };
            Some(QuestionOption {
                id: format!("o{}", i + 1),
                label: clip(&label),
                description,
            })
        })
        .collect()
}

/// Codex `request_user_input` arguments: `{questions:[{id,header,question,options:[{label,description}]}]}`.
pub fn codex_blocking_questions(arguments: &Value) -> Vec<Question> {
    arguments["questions"]
        .as_array()
        .into_iter()
        .flatten()
        .take(MAX_QUESTIONS)
        .enumerate()
        .filter_map(|(i, q)| {
            Some(Question {
                id: q["id"]
                    .as_str()
                    .map(str::to_owned)
                    .unwrap_or_else(|| format!("q{}", i + 1)),
                text: clip(q["question"].as_str()?),
                header: q["header"].as_str().map(clip),
                options: options_from(q["options"].as_array()),
                multi: false,
                // The app-server always adds a free-form "Other" (`isOther`).
                allow_free_text: q["isOther"].as_bool().unwrap_or(true),
            })
        })
        .collect()
}

/// Codex `request_user_input_async` arguments: `{questions:[{title,options:[string]}]}`.
pub fn codex_async_questions(arguments: &Value) -> Vec<Question> {
    arguments["questions"]
        .as_array()
        .into_iter()
        .flatten()
        .take(MAX_QUESTIONS)
        .enumerate()
        .filter_map(|(i, q)| {
            Some(Question {
                id: format!("q{}", i + 1),
                text: clip(q["title"].as_str()?),
                header: None,
                options: options_from(q["options"].as_array()),
                multi: false,
                allow_free_text: true,
            })
        })
        .collect()
}

/// Claude `AskUserQuestion` input: `{questions:[{question,header,options:[{label,description}],multiSelect}]}`.
pub fn claude_questions(input: &Value) -> Vec<Question> {
    input["questions"]
        .as_array()
        .into_iter()
        .flatten()
        .take(MAX_QUESTIONS)
        .enumerate()
        .filter_map(|(i, q)| {
            Some(Question {
                id: format!("q{}", i + 1),
                text: clip(q["question"].as_str()?),
                header: q["header"].as_str().map(clip),
                options: options_from(q["options"].as_array()),
                multi: q["multiSelect"].as_bool().unwrap_or(false),
                allow_free_text: true,
            })
        })
        .collect()
}

// ------------------------------------------------------------------ scanning

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Format {
    Codex,
    Claude,
}

#[derive(Default, Serialize, Deserialize)]
struct ScanState {
    version: u32,
    path: PathBuf,
    ino: u64,
    offset: u64,
    open: Vec<Pending>,
}

fn timestamp(v: &Value) -> i64 {
    v["timestamp"]
        .as_str()
        .and_then(crate::messaging::claude_steer::terminal::timestamp_ms)
        .map(|ms| ms / 1000)
        .unwrap_or_else(crate::daemon::now)
}

/// Apply one history record. Codex: a new turn (`task_started`) supersedes every
/// earlier question; a finished/aborted turn ends its blocking requests; a
/// `function_call_output` answers a blocking request. Claude: `tool_result`.
fn apply(format: Format, open: &mut Vec<Pending>, line: &[u8]) {
    let interesting: &[&[u8]] = match format {
        Format::Codex => &[
            b"request_user_input",
            b"function_call_output",
            b"task_started",
            b"task_complete",
            b"turn_aborted",
        ],
        Format::Claude => &[b"AskUserQuestion", b"tool_result"],
    };
    if !interesting
        .iter()
        .any(|needle| line.windows(needle.len()).any(|w| w == *needle))
    {
        return;
    }
    let Ok(v) = serde_json::from_slice::<Value>(line) else {
        return;
    };
    match format {
        Format::Codex => {
            let p = &v["payload"];
            match (v["type"].as_str(), p["type"].as_str()) {
                (Some("response_item"), Some("function_call")) => {
                    let name = p["name"].as_str().unwrap_or_default();
                    let blocking = match name {
                        "request_user_input" => true,
                        "request_user_input_async" => false,
                        _ => return,
                    };
                    let Some(call) = p["call_id"].as_str() else {
                        return;
                    };
                    let arguments = p["arguments"]
                        .as_str()
                        .and_then(|a| serde_json::from_str::<Value>(a).ok())
                        .unwrap_or(Value::Null);
                    let questions = if blocking {
                        codex_blocking_questions(&arguments)
                    } else {
                        codex_async_questions(&arguments)
                    };
                    if questions.is_empty() {
                        return;
                    }
                    open.retain(|q| q.request_id != call);
                    open.push(Pending {
                        request_id: call.to_owned(),
                        asked_at: timestamp(&v),
                        kind: if blocking {
                            KIND_CODEX_BLOCKING
                        } else {
                            KIND_CODEX_ASYNC
                        }
                        .into(),
                        blocking,
                        turn_id: p["internal_chat_message_metadata_passthrough"]["turn_id"]
                            .as_str()
                            .map(str::to_owned),
                        questions,
                        answerable: false,
                        unanswerable_reason: None,
                    });
                }
                (Some("response_item"), Some("function_call_output")) => {
                    if let Some(call) = p["call_id"].as_str() {
                        open.retain(|q| !(q.blocking && q.request_id == call));
                    }
                }
                (Some("event_msg"), Some("task_started")) => open.clear(),
                (Some("event_msg"), Some("task_complete" | "turn_aborted")) => {
                    open.retain(|q| !q.blocking)
                }
                _ => {}
            }
        }
        Format::Claude => {
            let content = v["message"]["content"].as_array();
            match v["type"].as_str() {
                Some("assistant") => {
                    for block in content.into_iter().flatten() {
                        if block["type"] == "tool_use" && block["name"] == "AskUserQuestion" {
                            let Some(id) = block["id"].as_str() else {
                                continue;
                            };
                            let questions = claude_questions(&block["input"]);
                            if questions.is_empty() {
                                continue;
                            }
                            open.retain(|q| q.request_id != id);
                            open.push(Pending {
                                request_id: id.to_owned(),
                                asked_at: timestamp(&v),
                                kind: KIND_CLAUDE.into(),
                                blocking: true,
                                turn_id: None,
                                questions,
                                answerable: false,
                                unanswerable_reason: None,
                            });
                        }
                    }
                }
                Some("user") => {
                    for block in content.into_iter().flatten() {
                        if block["type"] == "tool_result"
                            && let Some(id) = block["tool_use_id"].as_str()
                        {
                            open.retain(|q| q.request_id != id);
                        }
                    }
                }
                _ => {}
            }
        }
    }
}

fn scan_file(paths: &Paths, history: &Path) -> PathBuf {
    let key = format!(
        "{:x}",
        Sha256::digest(history.as_os_str().as_encoded_bytes())
    );
    paths
        .root
        .join("questions/scan")
        .join(format!("{}.json", &key[..32]))
}

/// Pending questions in a history file. Reads only bytes appended since the
/// previous scan; a replaced or truncated file is scanned again from the start.
pub fn scan(paths: &Paths, format: Format, history: &Path) -> Result<Vec<Pending>> {
    let cache = scan_file(paths, history);
    let mut file = fs::File::open(history)?;
    let meta = file.metadata()?;
    let mut state = fs::read(&cache)
        .ok()
        .and_then(|b| serde_json::from_slice::<ScanState>(&b).ok())
        .filter(|s| {
            s.version == SCAN_VERSION
                && s.path == history
                && s.ino == meta.ino()
                && s.offset <= meta.len()
        })
        .unwrap_or_else(|| ScanState {
            version: SCAN_VERSION,
            path: history.to_owned(),
            ino: meta.ino(),
            ..Default::default()
        });
    if state.offset == meta.len() {
        return Ok(state.open);
    }
    file.seek(SeekFrom::Start(state.offset))?;
    let mut bytes = Vec::new();
    file.take(meta.len() - state.offset)
        .read_to_end(&mut bytes)?;
    // Only complete records; a partial final line is read again next time.
    let complete = bytes.iter().rposition(|b| *b == b'\n').map_or(0, |i| i + 1);
    for line in bytes[..complete].split(|b| *b == b'\n') {
        if !line.is_empty() {
            apply(format, &mut state.open, line);
        }
    }
    state.offset += complete as u64;
    if let Some(parent) = cache.parent() {
        crate::paths::private_dir(parent)?;
    }
    let _ = crate::paths::atomic_write(&cache, &serde_json::to_vec(&state)?);
    Ok(state.open)
}

// ------------------------------------------------------------------ receipts

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct AnswerReceipt {
    pub session_id: String,
    pub request_id: String,
    pub kind: String,
    pub answers_digest: String,
    /// `dispatching` (journaled before the native write), `answered`,
    /// `queued` (async answer accepted by the public queue), `delivery_unknown`
    /// or `rejected` (nothing was sent).
    pub state: String,
    #[serde(default)]
    pub reason: Option<String>,
    pub created_at: i64,
    pub updated_at: i64,
    #[serde(default)]
    pub message_request: Option<crate::messaging::PublicRequest>,
    #[serde(default)]
    pub native_request_id: Option<Value>,
}

fn answers_dir(paths: &Paths, session: &str) -> PathBuf {
    paths.root.join("questions/answers").join(session)
}

fn receipt_path(paths: &Paths, session: &str, request: &str) -> PathBuf {
    let key = format!("{:x}", Sha256::digest(request.as_bytes()));
    answers_dir(paths, session).join(format!("{}.json", &key[..32]))
}

pub fn read_receipt(paths: &Paths, session: &str, request: &str) -> Option<AnswerReceipt> {
    fs::read(receipt_path(paths, session, request))
        .ok()
        .and_then(|b| serde_json::from_slice(&b).ok())
        .filter(|r: &AnswerReceipt| r.request_id == request && r.session_id == session)
}

fn save_receipt(paths: &Paths, r: &mut AnswerReceipt) -> Result<()> {
    r.updated_at = crate::daemon::now();
    let path = receipt_path(paths, &r.session_id, &r.request_id);
    crate::paths::private_dir(path.parent().context("receipt path")?)?;
    crate::paths::atomic_write(&path, &serde_json::to_vec(r)?)?;
    event(paths, r);
    Ok(())
}

fn event(paths: &Paths, r: &AnswerReceipt) {
    use std::io::Write;
    let dir = paths.root.join("questions");
    if crate::paths::private_dir(&dir).is_err() {
        return;
    }
    let line = json!({"at":crate::daemon::now(),"kind":"question_answer","session_id":r.session_id,
        "request_id":r.request_id,"question_kind":r.kind,"state":r.state,"reason":r.reason});
    if let Ok(mut f) = fs::OpenOptions::new()
        .create(true)
        .append(true)
        .open(dir.join("events.jsonl"))
    {
        let _ = writeln!(f, "{line}");
    }
}

/// A receipt that hides its question: anything except a definite rejection.
fn settled(r: &AnswerReceipt) -> bool {
    r.state != "rejected"
}

pub fn public_receipt(r: &AnswerReceipt, message: Option<Value>) -> Value {
    json!({"session_id":r.session_id,"request_id":r.request_id,"kind":r.kind,"state":r.state,
        "reason":r.reason,"answers_digest":r.answers_digest,"created_at":r.created_at,
        "updated_at":r.updated_at,"replay":"never","message_receipt":message})
}

// ------------------------------------------------------------------ answers

/// Caller answers: `{"<question id>": {"option_ids": ["o1"], "text": "..."}}`.
#[derive(Clone, Debug, Default, Deserialize, Serialize, PartialEq)]
#[serde(deny_unknown_fields)]
pub struct Answer {
    #[serde(default)]
    pub option_ids: Vec<String>,
    #[serde(default)]
    pub text: Option<String>,
}

pub type Answers = BTreeMap<String, Answer>;

pub fn parse_answers(raw: &str) -> Result<Answers> {
    ensure!(raw.len() <= MAX_ANSWER_BYTES, "INVALID_ANSWERS: too large");
    serde_json::from_str(raw).map_err(|e| anyhow::anyhow!("INVALID_ANSWERS: {e}"))
}

/// Every question needs an option and/or text; ids and limits are checked.
pub fn validate(questions: &[Question], answers: &Answers) -> Result<()> {
    for id in answers.keys() {
        ensure!(
            questions.iter().any(|q| &q.id == id),
            "INVALID_ANSWERS: unknown question id {id}"
        );
    }
    for q in questions {
        let a = answers
            .get(&q.id)
            .with_context(|| format!("INVALID_ANSWERS: question {} is unanswered", q.id))?;
        let text = a.text.as_deref().map(str::trim).filter(|t| !t.is_empty());
        ensure!(
            !a.option_ids.is_empty() || text.is_some(),
            "INVALID_ANSWERS: question {} needs an option or text",
            q.id
        );
        ensure!(
            q.multi || a.option_ids.len() <= 1,
            "INVALID_ANSWERS: question {} accepts one option",
            q.id
        );
        let mut seen = std::collections::BTreeSet::new();
        for o in &a.option_ids {
            ensure!(
                q.options.iter().any(|x| &x.id == o) && seen.insert(o),
                "INVALID_ANSWERS: invalid option {o} for question {}",
                q.id
            );
        }
        ensure!(
            text.is_none() || q.allow_free_text,
            "INVALID_ANSWERS: question {} does not accept free text",
            q.id
        );
        ensure!(
            text.is_none_or(|t| t.len() <= MAX_TEXT && !t.contains('\0')),
            "INVALID_ANSWERS: text too long"
        );
    }
    Ok(())
}

fn labels(q: &Question, a: &Answer) -> Vec<String> {
    a.option_ids
        .iter()
        .filter_map(|o| q.options.iter().find(|x| &x.id == o))
        .map(|o| o.label.clone())
        .collect()
}

fn text(a: &Answer) -> Option<&str> {
    a.text.as_deref().map(str::trim).filter(|t| !t.is_empty())
}

/// Codex `ToolRequestUserInputResponse`: selected labels, then the free-form
/// note as `user_note: ...` (the Codex TUI's encoding of "Other").
pub fn codex_response(questions: &[Question], answers: &Answers) -> Value {
    let mut map = serde_json::Map::new();
    for q in questions {
        let a = &answers[&q.id];
        let mut values = labels(q, a);
        if let Some(t) = text(a) {
            values.push(format!("user_note: {t}"));
        }
        map.insert(q.id.clone(), json!({"answers":values}));
    }
    json!({"answers":map})
}

/// The follow-up user message for an async Codex question.
pub fn async_message(questions: &[Question], answers: &Answers) -> String {
    let mut out = String::from("Answer to your request_user_input_async question");
    out.push_str(if questions.len() > 1 { "s:\n" } else { ":\n" });
    for (i, q) in questions.iter().enumerate() {
        let a = &answers[&q.id];
        let mut parts = labels(q, a);
        if let Some(t) = text(a) {
            parts.push(t.to_owned());
        }
        out.push_str(&format!(
            "{}. Q: {}\n   A: {}\n",
            i + 1,
            q.text,
            parts.join("; ")
        ));
    }
    out
}

/// Claude `AskUserQuestion` `updatedInput`: answers keyed by question text,
/// multiple selections joined with ", ".
pub fn claude_updated_input(original: &Value, questions: &[Question], answers: &Answers) -> Value {
    let mut map = serde_json::Map::new();
    for q in questions {
        let a = &answers[&q.id];
        let mut parts = labels(q, a);
        if let Some(t) = text(a) {
            parts.push(t.to_owned());
        }
        map.insert(q.text.clone(), json!(parts.join(", ")));
    }
    let mut input = original.clone();
    if !input.is_object() {
        input = json!({});
    }
    input["answers"] = Value::Object(map);
    input
}

/// Answer state of an async question from its public message receipt.
fn async_state(message: &Value) -> &'static str {
    match message["state"].as_str() {
        Some("rejected" | "expired" | "cancelled") => "rejected",
        Some("delivery_unknown") => "delivery_unknown",
        Some("pending_local") => "dispatching",
        _ => "queued",
    }
}

fn digest(request: &str, answers: &Answers) -> String {
    let bytes = serde_json::to_vec(&json!([request, answers])).unwrap_or_default();
    format!("{:x}", Sha256::digest(bytes))
}

// ------------------------------------------------------------------ projection

pub const NO_EXTERNAL_ANSWER_CLAUDE_TERMINAL: &str = "PROVIDER_TERMINAL_ONLY: Claude Code answers AskUserQuestion only in its own terminal; a Sessionsation-hosted session can be answered";
pub const NO_DETECTION_GEMINI: &str = "Gemini CLI records ask_user tool calls only after they complete, so a pending question is not visible outside its terminal";

fn project(paths: &Paths, session: &str, open: Vec<Pending>) -> Vec<Pending> {
    open.into_iter()
        .filter(|p| read_receipt(paths, session, &p.request_id).is_none_or(|r| !settled(&r)))
        .collect()
}

/// Codex row: questions of a running conversation's rollout.
pub fn codex_pending(paths: &Paths, session: &str, rollout: &Path) -> Vec<Pending> {
    let open = scan(paths, Format::Codex, rollout).unwrap_or_default();
    project(paths, session, open)
        .into_iter()
        .map(|mut p| {
            p.answerable = true;
            p
        })
        .collect()
}

/// Claude row: hosted prompts are answerable; terminal ones are read-only.
pub fn claude_pending(
    paths: &Paths,
    session: &str,
    conversation: &str,
    transcript: Option<&Path>,
) -> Vec<Pending> {
    use crate::provider::claude::hosted;
    if let Some(h) = hosted::accepting(paths, conversation) {
        let open = h
            .pending_permissions
            .iter()
            .filter(|p| p["tool_name"] == "AskUserQuestion")
            .filter_map(|p| {
                let questions = claude_questions(&p["input"]);
                (!questions.is_empty()).then(|| Pending {
                    request_id: p["request_id"].as_str().unwrap_or_default().to_owned(),
                    asked_at: p["requested_at"].as_i64().unwrap_or(0),
                    kind: KIND_CLAUDE_HOSTED.into(),
                    blocking: true,
                    turn_id: None,
                    questions,
                    answerable: true,
                    unanswerable_reason: None,
                })
            })
            .filter(|p| !p.request_id.is_empty())
            .collect();
        return project(paths, session, open);
    }
    let Some(path) = transcript else {
        return vec![];
    };
    scan(paths, Format::Claude, path)
        .unwrap_or_default()
        .into_iter()
        .map(|mut p| {
            p.unanswerable_reason = Some(NO_EXTERNAL_ANSWER_CLAUDE_TERMINAL.into());
            p
        })
        .collect()
}

pub fn to_values(pending: Vec<Pending>) -> Value {
    json!(pending)
}

pub fn capability() -> Value {
    json!({"version":1,"since_revision":23,"state_field":"pending_questions",
        "answer_command":"api answer --session ID --expected-generation G --request-id R [--answers JSON | stdin]",
        "answers_shape":{"<question id>":{"option_ids":["o1"],"text":"free text"}},
        "replay":"never","receipts":"repeat the identical answer to read its receipt",
        "providers":{
            "codex":{"detect":true,"answer":true,"kinds":[KIND_CODEX_BLOCKING,KIND_CODEX_ASYNC],
                "blocking":"item/tool/requestUserInput response via thread/resume replay",
                "async":"follow-up user message on the public non-steering queue"},
            "claude":{"detect":true,"answer":"hosted_only","kinds":[KIND_CLAUDE,KIND_CLAUDE_HOSTED],
                "reason":NO_EXTERNAL_ANSWER_CLAUDE_TERMINAL},
            "gemini":{"detect":false,"answer":false,"reason":NO_DETECTION_GEMINI}}})
}

// ------------------------------------------------------------------ answering

pub struct AnswerRequest {
    pub session: String,
    pub expected_generation: String,
    pub request_id: String,
    pub answers: Answers,
}

fn session_lock(paths: &Paths, session: &str) -> Result<fs::File> {
    let dir = paths.root.join("questions/answers");
    crate::paths::private_dir(&dir)?;
    let file = crate::paths::lock_file(&dir.join(format!("{session}.lock")))?;
    file.try_lock_exclusive().map_err(|_| {
        anyhow::anyhow!("OPERATION_BUSY: another answer for this session is in progress")
    })?;
    Ok(file)
}

enum Admission {
    /// An earlier attempt already exists for the identical answer.
    Existing(AnswerReceipt),
    New(AnswerReceipt),
}

/// Idempotency and staleness: an identical repeat returns the original receipt;
/// a different answer after an attempt is `QUESTION_ALREADY_ANSWERED`; a question
/// that is no longer pending is `QUESTION_NOT_PENDING`.
fn admit(
    paths: &Paths,
    session: &str,
    pending: Option<&Pending>,
    request_id: &str,
    answers: &Answers,
) -> Result<Admission> {
    let digest = digest(request_id, answers);
    if let Some(r) = read_receipt(paths, session, request_id).filter(settled) {
        ensure!(
            r.answers_digest == digest,
            "QUESTION_ALREADY_ANSWERED: an answer was already sent for this question; it is never replaced or resent"
        );
        return Ok(Admission::Existing(r));
    }
    let p = pending.context(
        "QUESTION_NOT_PENDING: the question was answered elsewhere, superseded by a later turn, or is gone",
    )?;
    if let Some(reason) = &p.unanswerable_reason {
        bail!("{reason}");
    }
    validate(&p.questions, answers)?;
    let now = crate::daemon::now();
    Ok(Admission::New(AnswerReceipt {
        session_id: session.into(),
        request_id: request_id.into(),
        kind: p.kind.clone(),
        answers_digest: digest,
        state: "pending_local".into(),
        reason: None,
        created_at: now,
        updated_at: now,
        message_request: None,
        native_request_id: None,
    }))
}

pub async fn answer(paths: &Paths, request: AnswerRequest) -> Result<Value> {
    let s = catalog::resolve(paths, &request.session, &request.expected_generation)?;
    let _lock = session_lock(paths, &s.id)?;
    let provider = ProviderId::parse(&s.provider)?;
    let (pending, codex_endpoint) = match provider {
        ProviderId::Codex => {
            let source = crate::api::codex_source(paths, &s).await?;
            let rollout = source
                .as_ref()
                .filter(|i| !i.inactive)
                .map(|i| i.rollout.clone());
            let open = match &rollout {
                Some(path) => scan(paths, Format::Codex, path)?,
                None => vec![],
            };
            (
                open,
                source.map(|i| i.home.join("app-server-control/app-server-control.sock")),
            )
        }
        ProviderId::Claude => {
            let observed = crate::provider::native::reconcile(paths).unwrap_or_default();
            let transcript = observed.get(&s.id).map(|(_, h)| h.clone());
            (
                claude_pending(paths, &s.id, &s.conversation_id, transcript.as_deref()),
                None,
            )
        }
        ProviderId::Gemini => bail!("UNSUPPORTED_PROVIDER_OPERATION: {NO_DETECTION_GEMINI}"),
    };
    let found = pending.iter().find(|p| p.request_id == request.request_id);
    let mut receipt = match admit(paths, &s.id, found, &request.request_id, &request.answers)? {
        Admission::Existing(mut r) => {
            let message = match r.message_request.clone() {
                // The public ledger owns the async attempt: the identical request
                // returns (or reconciles) its original receipt and never resends.
                Some(m) => {
                    let m = crate::messaging::send_public(paths, m).await?;
                    let state = async_state(&m);
                    if r.state != state {
                        r.state = state.into();
                        r.reason = m["reason"].as_str().map(str::to_owned);
                        save_receipt(paths, &mut r)?;
                    }
                    Some(m)
                }
                None => {
                    if r.state == "dispatching" || r.state == "pending_local" {
                        // An interrupted attempt: its outcome is unknown and never resent.
                        r.state = "delivery_unknown".into();
                        r.reason = Some("interrupted_attempt".into());
                        save_receipt(paths, &mut r)?;
                    }
                    None
                }
            };
            return Ok(public_receipt(&r, message));
        }
        Admission::New(r) => r,
    };
    let p = found.context("QUESTION_NOT_PENDING")?.clone();
    match p.kind.as_str() {
        KIND_CODEX_BLOCKING => {
            let endpoint = codex_endpoint
                .filter(|e| e.exists())
                .context("QUESTION_NOT_ANSWERABLE: the conversation's app-server is unavailable")?;
            let _messaging = crate::external::messaging_lock(paths, &s.conversation_id)
                .map_err(|e| anyhow::anyhow!("{}", e.to_ascii_uppercase()))?;
            codex_blocking(
                paths,
                &endpoint,
                &s.conversation_id,
                &p,
                &request.answers,
                &mut receipt,
            )
            .await?;
            Ok(public_receipt(&receipt, None))
        }
        KIND_CODEX_ASYNC => {
            let message = crate::messaging::PublicRequest {
                api_version: crate::api::VERSION,
                message_id: uuid::Uuid::new_v4().to_string(),
                session_id: s.id.clone(),
                expected_generation: request.expected_generation.clone(),
                mode: crate::messaging::PublicMode::Queue,
                text: async_message(&p.questions, &request.answers),
                expires_at: crate::daemon::now() + 3600,
                recovery: None,
                steering: None,
            };
            receipt.message_request = Some(message.clone());
            receipt.state = "dispatching".into();
            save_receipt(paths, &mut receipt)?;
            let result = crate::messaging::send_public(paths, message).await;
            match &result {
                Ok(m) => {
                    receipt.state = async_state(m).into();
                    receipt.reason = m["reason"].as_str().map(str::to_owned);
                }
                Err(e) => {
                    // Admission errors precede any native write.
                    receipt.state = "rejected".into();
                    receipt.reason = Some(e.to_string());
                }
            }
            save_receipt(paths, &mut receipt)?;
            let message = result?;
            Ok(public_receipt(&receipt, Some(message)))
        }
        KIND_CLAUDE_HOSTED => {
            use crate::provider::claude::hosted;
            let h = hosted::accepting(paths, &s.conversation_id)
                .context("QUESTION_NOT_PENDING: the hosted session is not running")?;
            let original = h
                .pending_permissions
                .iter()
                .find(|x| x["request_id"] == p.request_id.as_str())
                .map(|x| x["input"].clone())
                .context("QUESTION_NOT_PENDING")?;
            let updated = claude_updated_input(&original, &p.questions, &request.answers);
            receipt.state = "dispatching".into();
            save_receipt(paths, &mut receipt)?;
            let response = hosted::call(
                paths,
                &s.conversation_id,
                json!({"op":"respond","request_id":p.request_id,"behavior":"allow","updated_input":updated}),
                Duration::from_secs(10),
            )
            .await;
            match response {
                Ok(v) if v["ok"] == true => receipt.state = "answered".into(),
                Ok(v) if v["error"] == "PERMISSION_REQUEST_NOT_FOUND" => {
                    receipt.state = "rejected".into();
                    receipt.reason = Some("QUESTION_NOT_PENDING".into());
                }
                Ok(v) if v["error"] == "HOST_STOPPING" => {
                    receipt.state = "rejected".into();
                    receipt.reason = Some("HOST_STOPPING".into());
                }
                _ => {
                    receipt.state = "delivery_unknown".into();
                    receipt.reason = Some("hosted_response_unconfirmed".into());
                }
            }
            save_receipt(paths, &mut receipt)?;
            Ok(public_receipt(&receipt, None))
        }
        _ => bail!(
            "UNSUPPORTED_PROVIDER_OPERATION: this question cannot be answered outside its terminal"
        ),
    }
}

// ------------------------------------------------------------------ Codex blocking

type Socket = tokio_tungstenite::WebSocketStream<tokio::net::UnixStream>;

async fn next_frame(socket: &mut Socket, deadline: tokio::time::Instant) -> Option<Value> {
    use futures_util::StreamExt;
    use tokio_tungstenite::tungstenite::Message;
    loop {
        let frame = tokio::time::timeout_at(deadline, socket.next())
            .await
            .ok()??
            .ok()?;
        match frame {
            Message::Text(text) => return serde_json::from_str(text.as_str()).ok(),
            Message::Close(_) => return None,
            _ => continue,
        }
    }
}

async fn send(socket: &mut Socket, v: Value) -> Result<()> {
    use futures_util::SinkExt;
    socket
        .send(tokio_tungstenite::tungstenite::Message::Text(
            v.to_string().into(),
        ))
        .await?;
    Ok(())
}

/// Wait for the response to `id`, collecting server requests seen meanwhile.
async fn call(
    socket: &mut Socket,
    id: u64,
    method: &str,
    params: Value,
    seen: &mut Vec<Value>,
) -> Result<Value> {
    send(socket, json!({"id":id,"method":method,"params":params})).await?;
    let deadline = tokio::time::Instant::now() + Duration::from_secs(5);
    while let Some(v) = next_frame(socket, deadline).await {
        if v.get("method").is_some() {
            if v.get("id").is_some() {
                seen.push(v);
            }
            continue;
        }
        if v["id"] == id {
            ensure!(
                v.get("error").is_none(),
                "NATIVE_REJECTED: {method} (code {})",
                v["error"]["code"]
            );
            return Ok(v["result"].clone());
        }
    }
    bail!("ENDPOINT_UNAVAILABLE: no response to {method}")
}

/// Subscribe, find the replayed `item/tool/requestUserInput` for this call id,
/// journal, answer once, then confirm with `serverRequest/resolved`.
pub(crate) async fn codex_blocking(
    paths: &Paths,
    endpoint: &Path,
    thread: &str,
    pending: &Pending,
    answers: &Answers,
    receipt: &mut AnswerReceipt,
) -> Result<()> {
    let reject = |receipt: &mut AnswerReceipt, reason: &str| -> Result<()> {
        receipt.state = "rejected".into();
        receipt.reason = Some(reason.into());
        save_receipt(paths, receipt)?;
        bail!("{reason}")
    };
    let stream = tokio::time::timeout(
        Duration::from_secs(3),
        tokio::net::UnixStream::connect(endpoint),
    )
    .await
    .map_err(|_| anyhow::anyhow!("ENDPOINT_UNAVAILABLE"))??;
    let config = tokio_tungstenite::tungstenite::protocol::WebSocketConfig::default()
        .max_message_size(Some(16 * 1024 * 1024));
    let (mut socket, _) =
        tokio_tungstenite::client_async_with_config("ws://localhost/", stream, Some(config))
            .await?;
    let mut seen = Vec::new();
    let init = call(
        &mut socket,
        1,
        "initialize",
        json!({"clientInfo":{"name":"sessionsation_questions","version":env!("CARGO_PKG_VERSION")},
            "capabilities":{"experimentalApi":true}}),
        &mut seen,
    )
    .await?;
    let version = init["userAgent"]
        .as_str()
        .and_then(|u| u.split_whitespace().next())
        .and_then(|s| s.rsplit_once('/'))
        .map(|(_, v)| v.to_owned());
    if !version
        .as_deref()
        .is_some_and(|v| crate::messaging::NATIVE_PROFILES.contains(&v))
    {
        return reject(receipt, "UNSUPPORTED_NATIVE_VERSION");
    }
    send(&mut socket, json!({"method":"initialized"})).await?;
    // Never load a conversation: a blocking question needs a running turn.
    let state = call(
        &mut socket,
        2,
        "thread/read",
        json!({"threadId":thread,"includeTurns":false}),
        &mut seen,
    )
    .await?;
    let waiting = state["thread"]["id"] == thread
        && state["thread"]["status"]["activeFlags"]
            .as_array()
            .is_some_and(|f| f.iter().any(|x| x == "waitingOnUserInput"));
    if !waiting {
        return reject(receipt, "QUESTION_NOT_PENDING");
    }
    call(
        &mut socket,
        3,
        "thread/resume",
        json!({"threadId":thread,"excludeTurns":true}),
        &mut seen,
    )
    .await?;
    let matches = |v: &Value| {
        v["method"] == "item/tool/requestUserInput"
            && v["params"]["threadId"] == thread
            && v["params"]["itemId"] == pending.request_id.as_str()
    };
    let mut request = seen.into_iter().find(|v| matches(v));
    let deadline = tokio::time::Instant::now() + Duration::from_secs(3);
    while request.is_none() {
        match next_frame(&mut socket, deadline).await {
            Some(v) if matches(&v) => request = Some(v),
            Some(_) => {}
            None => break,
        }
    }
    let Some(request) = request else {
        return reject(receipt, "QUESTION_NOT_PENDING");
    };
    // The replayed questions are authoritative for the ids.
    let live = codex_blocking_questions(&request["params"]);
    if validate(&live, answers).is_err() {
        return reject(receipt, "QUESTION_CHANGED");
    }
    let id = request["id"].clone();
    receipt.native_request_id = Some(id.clone());
    receipt.state = "dispatching".into();
    save_receipt(paths, receipt)?;
    if send(
        &mut socket,
        json!({"id":id,"result":codex_response(&live, answers)}),
    )
    .await
    .is_err()
    {
        receipt.state = "delivery_unknown".into();
        receipt.reason = Some("disconnected".into());
        save_receipt(paths, receipt)?;
        return Ok(());
    }
    let deadline = tokio::time::Instant::now() + Duration::from_secs(5);
    let mut resolved = false;
    while let Some(v) = next_frame(&mut socket, deadline).await {
        if v["method"] == "serverRequest/resolved"
            && v["params"]["threadId"] == thread
            && v["params"]["requestId"] == id
        {
            resolved = true;
            break;
        }
    }
    receipt.state = if resolved {
        "answered"
    } else {
        "delivery_unknown"
    }
    .into();
    receipt.reason = (!resolved).then(|| "resolution_not_observed".into());
    save_receipt(paths, receipt)?;
    Ok(())
}

#[cfg(test)]
mod tests;
