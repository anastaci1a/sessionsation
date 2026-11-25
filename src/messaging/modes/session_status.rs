//! Timestamped, content-free observations. No launcher and no input probes.
use super::*;
mod summary_cache;
#[derive(Deserialize)]
struct Read {
    thread: Thread,
}
#[derive(Deserialize)]
struct Thread {
    id: String,
    status: Status,
    path: Option<PathBuf>,
}
#[derive(Deserialize, PartialEq)]
struct Status {
    #[serde(rename = "type")]
    kind: String,
    #[serde(default, rename = "activeFlags")]
    flags: Vec<String>,
    #[serde(skip)]
    history: Option<PathBuf>,
}
#[derive(Clone, Serialize, Deserialize)]
struct Page {
    data: Vec<Turn>,
}
#[derive(Clone, Serialize, Deserialize)]
struct Turn {
    id: String,
    status: String,
    /// Provider-reported failure of a `failed` turn (short text, no history).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    error: Option<TurnError>,
}
#[derive(Clone, Serialize, Deserialize)]
struct TurnError {
    message: Option<String>,
}

pub(in crate::messaging) struct Observation {
    pub state: String,
    pub reason: Option<&'static str>,
    pub previous: Option<String>,
    pub summary_evidence: Option<Value>,
    /// Status of the latest finished turn (`completed`, `failed`, `interrupted`).
    pub last_turn_status: Option<String>,
    /// The provider's own bounded error text for a failed latest turn.
    pub last_turn_error: Option<String>,
}
async fn read(control: &mut Transport, id: &str) -> Result<Status> {
    let read: Read = control
        .request("thread/read", json!({"threadId":id,"includeTurns":false}))
        .await
        .map_err(|e| {
            if e.0 == "timeout" {
                Fault("native_thread_read_timeout")
            } else {
                e
            }
        })?;
    if read.thread.id != id {
        return Err(Fault("correlation_changed"));
    }
    let mut status = read.thread.status;
    status.history = read.thread.path;
    Ok(status)
}
pub(in crate::messaging) async fn observe_cached(
    control: &mut Transport,
    paths: &Paths,
    attachment: &Attachment,
) -> Result<Observation> {
    observe_inner(control, &attachment.id, Some((paths, attachment)), false).await
}
pub(in crate::messaging) async fn observe_fresh_public(
    control: &mut Transport,
    id: &str,
) -> Result<Observation> {
    tokio::time::timeout(
        std::time::Duration::from_secs(5),
        observe_inner(control, id, None, true),
    )
    .await
    .map_err(|_| Fault("observation_timeout"))?
}
async fn observe_inner(
    control: &mut Transport,
    id: &str,
    context: Option<(&Paths, &Attachment)>,
    share_observation_budget: bool,
) -> Result<Observation> {
    let before = read(control, id).await?;
    if !before.flags.is_empty() {
        return Ok(Observation {
            state: "working".into(),
            reason: Some("approval_or_user_input_required"),
            previous: None,
            summary_evidence: None,
            last_turn_status: None,
            last_turn_error: None,
        });
    }
    let cache = context.and_then(|(paths, attachment)| {
        summary_cache::Cache::new(paths, attachment, before.history.as_deref())
    });
    let mut summary_evidence = None;
    let mut last_turn_status = None;
    let mut last_turn_error = None;
    let (state, previous) = match before.kind.as_str() {
        "active" => ("working", None),
        "notLoaded" => ("disconnected", None),
        "idle" | "systemError" => {
            // Only status and identifier are deserialized; public item bodies are ignored.
            let page: Page = if let Some(cache) = &cache {
                let (page, evidence) = cache.get(control, id).await?;
                summary_evidence = Some(evidence);
                page
            } else if share_observation_budget {
                control.passive_summary(id).await?
            } else {
                control
                .request(
                    "thread/turns/list",
                    json!({"threadId":id,"itemsView":"summary","sortDirection":"desc","limit":1}),
                )
                .await
                .map_err(|e| {
                    if e.0 == "timeout" {
                        Fault("native_turn_summary_timeout")
                    } else {
                        e
                    }
                })?
            };
            if page.data.len() > 1 {
                return Err(Fault("malformed_summary"));
            }
            match page.data.first() {
                Some(turn)
                    if turn.id.is_empty()
                        || turn.id.len() > 256
                        || turn.id.chars().any(char::is_control) =>
                {
                    return Err(Fault("malformed_summary"));
                }
                Some(turn) => {
                    last_turn_status = Some(turn.status.clone());
                    last_turn_error = turn
                        .error
                        .as_ref()
                        .and_then(|e| e.message.as_deref())
                        .and_then(crate::live_activity::clean_error);
                    (
                        match turn.status.as_str() {
                            "completed" if before.kind == "idle" => "idle",
                            "interrupted" => "interrupted",
                            "failed" => "errored",
                            _ => "unknown",
                        },
                        Some(turn.id.clone()),
                    )
                }
                None => (
                    if before.kind == "idle" {
                        "idle"
                    } else {
                        "unknown"
                    },
                    None,
                ),
            }
        }
        _ => ("unknown", None),
    };
    if read(control, id).await? != before {
        return Err(Fault("state_changed"));
    }
    if cache.as_ref().is_some_and(|c| !c.unchanged()) {
        return Err(Fault("history_changed_during_observation"));
    }
    Ok(Observation {
        state: state.into(),
        reason: None,
        previous,
        summary_evidence,
        last_turn_status,
        last_turn_error,
    })
}
