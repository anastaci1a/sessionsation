//! Bounded native public-summary projection, never a full history/item read.
use super::*;
use serde_json::value::RawValue;
mod progress;
use std::{collections::HashSet, time::Duration};

#[derive(Clone, Debug, Serialize, Deserialize, PartialEq)]
pub struct Correlation {
    pub turn_id: String,
    pub user_item_id: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub completed_user_item_id: Option<String>,
    pub observed_generation: String,
}
#[derive(Clone, Debug, Serialize, Deserialize, PartialEq)]
pub struct Reply {
    pub item_id: String,
    pub text: String,
    pub phase: Option<String>,
}
#[derive(Serialize, Deserialize)]
struct Page {
    data: Vec<Turn>,
    #[serde(rename = "nextCursor")]
    next: Option<String>,
}
#[derive(Serialize, Deserialize)]
struct Turn {
    id: String,
    status: String,
    #[serde(rename = "itemsView")]
    view: String,
    items: Vec<Box<RawValue>>,
}
#[derive(Deserialize)]
struct Kind {
    #[serde(rename = "type")]
    kind: String,
}
#[derive(Deserialize)]
struct User {
    id: String,
    #[serde(rename = "clientId")]
    client: Option<String>,
    // Deliberately no content field.
}
#[derive(Deserialize)]
struct Agent {
    id: String,
    text: String,
    phase: Option<String>,
}
fn parse<T: serde::de::DeserializeOwned>(raw: &RawValue) -> Result<T> {
    serde_json::from_str(raw.get()).map_err(|_| Fault("malformed_summary"))
}
fn valid_id(id: &str) -> Result<()> {
    if id.is_empty() || id.len() > 256 || id.chars().any(char::is_control) {
        Err(Fault("malformed_summary"))
    } else {
        Ok(())
    }
}

// Native legacy projections use independent positive item-N counters for live
// turns and persisted history. Other identifier schemes are not evidence.
fn legacy_projection_id(id: &str) -> bool {
    id.strip_prefix("item-")
        .and_then(|n| n.parse::<u64>().ok())
        .is_some_and(|n| n > 0 && id == format!("item-{n}"))
}

fn consume(
    turns: Vec<Turn>,
    receipt: &Receipt,
    generation: &str,
    candidate: &mut Option<Receipt>,
) -> Result<()> {
    for turn in turns {
        if turn.view != "summary" || turn.items.len() > 2 {
            return Err(Fault("unsupported_summary"));
        }
        let mut user: Option<User> = None;
        let mut agent: Option<Agent> = None;
        for item in turn.items {
            // Unknown payloads are skipped structurally, never materialized as Values.
            match parse::<Kind>(&item)?.kind.as_str() {
                "userMessage" if user.is_none() => user = Some(parse(&item)?),
                "agentMessage" if agent.is_none() => agent = Some(parse(&item)?),
                _ => return Err(Fault("unsupported_summary")),
            }
        }
        let Some(user) = user.filter(|u| u.client.as_deref() == Some(&receipt.message_id)) else {
            continue;
        };
        valid_id(&turn.id)?;
        valid_id(&user.id)?;
        if receipt
            .recovery_start
            .as_ref()
            .and_then(|s| s.turn_id.as_deref())
            .is_some_and(|id| id != turn.id)
        {
            return Err(Fault("correlation_changed"));
        }
        if candidate.is_some() {
            return Err(Fault("ambiguous_native_correlation"));
        }
        let mut completed_user_item_id = None;
        let mut original_user_item_id = user.id.clone();
        if let Some(old) = &receipt.correlation {
            if old.turn_id != turn.id {
                return Err(Fault("correlation_changed"));
            }
            if old.user_item_id != user.id {
                // Exact clientId matched above; only the verified completion
                // projection transition can reconcile an item counter.
                if receipt.state != "execution_started"
                    || turn.status != "completed"
                    || old.completed_user_item_id.is_some()
                    || !legacy_projection_id(&old.user_item_id)
                    || !legacy_projection_id(&user.id)
                {
                    return Err(Fault("correlation_changed"));
                }
                original_user_item_id = old.user_item_id.clone();
                completed_user_item_id = Some(user.id.clone());
            }
        }
        let mut observed = receipt.clone();
        observed.correlation = Some(Correlation {
            turn_id: turn.id,
            user_item_id: original_user_item_id,
            completed_user_item_id,
            observed_generation: generation.into(),
        });
        observed.reason = None;
        match turn.status.as_str() {
            "inProgress" => observed.state = "execution_started".into(),
            "failed" | "interrupted" => {
                observed.state = "failed".into();
                observed.reason = Some("native_turn_failed".into());
            }
            "completed" => {
                let agent = agent.ok_or(Fault("reply_not_available"))?;
                valid_id(&agent.id)?;
                if agent.text.is_empty() {
                    return Err(Fault("reply_not_available"));
                }
                if agent.text.len() > 65536 {
                    return Err(Fault("reply_too_large"));
                }
                if agent.phase.as_deref().is_some_and(|p| p != "final_answer") {
                    return Err(Fault("reply_not_final"));
                }
                observed.state = "completed_with_reply".into();
                observed.reply = Some(Reply {
                    item_id: agent.id,
                    text: agent.text,
                    phase: agent.phase,
                });
            }
            _ => return Err(Fault("unsupported_turn_status")),
        }
        *candidate = Some(observed);
    }

    Ok(())
}

/// Scan completes before returning any evidence. No intermediate partial commits.
async fn scan(
    control: &mut Transport,
    receipt: &Receipt,
    generation: &str,
) -> Result<Option<Receipt>> {
    let mut cursor: Option<String> = None;
    let mut cursors = HashSet::new();
    let mut candidate: Option<Receipt> = None;
    for _ in 0..20 {
        let page:Page=control.request("thread/turns/list",json!({"threadId":receipt.session_id,"itemsView":"summary","sortDirection":"desc","limit":50,"cursor":cursor})).await?;
        if page.data.len() > 50 {
            return Err(Fault("malformed_summary"));
        }
        consume(page.data, receipt, generation, &mut candidate)?;
        match page.next {
            None => return Ok(candidate),
            Some(next) => {
                if next.len() > 4096 || !cursors.insert(next.clone()) {
                    return Err(Fault("reply_scan_incomplete"));
                }
                cursor = Some(next);
            }
        }
    }
    Err(Fault("reply_scan_incomplete"))
}

pub(super) async fn read(
    control: &mut Transport,
    receipt: &Receipt,
    generation: &str,
) -> Result<Option<Receipt>> {
    tokio::time::timeout(Duration::from_secs(5), scan(control, receipt, generation))
        .await
        .map_err(|_| Fault("reply_scan_incomplete"))?
}
pub(super) fn commit(ledger: &mut Ledger, receipt: &mut Receipt, observed: Receipt) -> Result<()> {
    // Terminal durable evidence cannot be regressed by a later/stale observation.
    if matches!(
        receipt.state.as_str(),
        "completed_with_reply" | "failed" | "cancelled" | "expired"
    ) {
        return Ok(());
    }
    if *receipt != observed {
        ledger.save(&observed)?;
        *receipt = observed;
    }
    Ok(())
}
async fn refresh(
    paths: &Paths,
    ledger: &mut Ledger,
    receipt: &mut Receipt,
) -> Result<&'static str> {
    let _guard = mutation_lock(paths, &receipt.session_id)?;
    let selected = attachment(paths, &receipt.session_id)?;
    let generation = selected.generation();
    let mut control = selected.connect().await?;
    let observed = read(&mut control, receipt, &generation).await?;
    if attachment(paths, &receipt.session_id)?.generation() != generation {
        return Err(Fault("generation_mismatch"));
    }
    if let Some(observed) = observed {
        commit(ledger, receipt, observed)?;
        Ok("observed")
    } else {
        Ok("uncorrelated")
    }
}

/// Public receipt observation must not hold the root-wide writer lease during
/// provider I/O. The caller reacquires it and commits only if the snapshot still
/// matches; cancellation and concurrent delivery always win that comparison.
pub(super) async fn snapshot(paths: &Paths, receipt: &Receipt) -> Result<Option<Receipt>> {
    let selected = attachment(paths, &receipt.session_id)?;
    let generation = selected.generation();
    let mut control = selected.connect().await?;
    let observed = tokio::time::timeout(
        Duration::from_secs(5),
        progress::read(paths, &selected, &mut control, receipt),
    )
    .await
    .map_err(|_| Fault("reply_scan_incomplete"))??;
    if attachment(paths, &receipt.session_id)?.generation() != generation {
        return Err(Fault("generation_mismatch"));
    }
    Ok(observed)
}
pub(super) async fn result(
    paths: &Paths,
    ledger: &mut Ledger,
    receipt: &mut Receipt,
) -> Result<Value> {
    let observation = match receipt.state.as_str() {
        "unknown" | "native_accepted" | "execution_started" => {
            match refresh(paths, ledger, receipt).await {
                Ok(state) => json!({"state":state}),
                Err(Fault("storage_error")) => return Err(Fault("storage_error")),
                Err(error) => json!({"state":"unavailable","code":error.0}),
            }
        }
        "completed_with_reply" | "failed" => json!({"state":"cached"}),
        _ => json!({"state":"not_applicable"}),
    };
    let mut result = json!(receipt);
    result["observation"] = observation;
    Ok(result)
}
#[cfg(test)]
mod tests;
