//! Steer reply attribution (contract revision 15). Verified against native
//! 0.165 item history: a steer is persisted as a `userMessage` whose `clientId`
//! is the steer's `clientUserMessageId`, positioned inside the steered turn;
//! assistant items produced after it follow it in the same turn. An assistant
//! item still streaming when the steer lands may be discarded natively, so the
//! anchor's last agent item is only a consistency check, never required.
//! Only item IDs, client IDs and assistant text are materialized; user content,
//! reasoning, tool output and unknown items are skipped structurally.
use super::*;
use serde_json::value::RawValue;

const MAX_PAGES: usize = 8;
const MAX_ITEMS: usize = 8192;
const MAX_REPLY_BYTES: usize = 65536;

#[derive(Deserialize)]
struct Page {
    data: Vec<FullTurn>,
    #[serde(rename = "nextCursor")]
    next: Option<String>,
}
#[derive(Deserialize)]
struct FullTurn {
    id: String,
    status: String,
    #[serde(rename = "itemsView")]
    view: String,
    items: Items,
}
/// One native item reduced while parsing: only the type and the fields a
/// projection may read are kept as raw JSON, and every other field
/// (tool output, reasoning, user content) is skipped without being copied.
#[derive(Deserialize)]
struct RawItem {
    #[serde(rename = "type")]
    kind: String,
    id: Option<Box<RawValue>>,
    #[serde(rename = "clientId")]
    client: Option<Box<RawValue>>,
    text: Option<Box<RawValue>>,
    phase: Option<Box<RawValue>>,
}
/// Turn items, retaining at most `MAX_ITEMS`; a larger turn is consumed without
/// allocation and reported as too large instead of growing memory.
struct Items {
    items: Vec<RawItem>,
    overflow: bool,
}
impl<'de> Deserialize<'de> for Items {
    fn deserialize<D: serde::Deserializer<'de>>(d: D) -> std::result::Result<Self, D::Error> {
        struct Visit;
        impl<'de> serde::de::Visitor<'de> for Visit {
            type Value = Items;
            fn expecting(&self, f: &mut std::fmt::Formatter) -> std::fmt::Result {
                f.write_str("an item array")
            }
            fn visit_seq<A: serde::de::SeqAccess<'de>>(
                self,
                mut seq: A,
            ) -> std::result::Result<Items, A::Error> {
                let mut out = Items {
                    items: Vec::new(),
                    overflow: false,
                };
                while out.items.len() < MAX_ITEMS {
                    match seq.next_element()? {
                        Some(item) => out.items.push(item),
                        None => return Ok(out),
                    }
                }
                while seq.next_element::<serde::de::IgnoredAny>()?.is_some() {
                    out.overflow = true;
                }
                Ok(out)
            }
        }
        d.deserialize_seq(Visit)
    }
}
struct User {
    id: String,
    client: Option<String>,
    // Deliberately no content field.
}
struct Agent {
    id: String,
    text: String,
    phase: Option<String>,
}
enum Item {
    User(User),
    Agent(Agent),
    Other,
}
struct Turn {
    status: String,
    items: Vec<Item>,
}
fn valid(id: &str) -> bool {
    !id.is_empty() && id.len() <= 256 && !id.chars().any(char::is_control)
}
fn parse<T: serde::de::DeserializeOwned>(raw: Option<&RawValue>) -> Result<T> {
    serde_json::from_str(raw.map_or("null", RawValue::get)).map_err(|_| Fault("malformed_response"))
}
fn project(raw: Items) -> Result<Vec<Item>> {
    if raw.overflow {
        return Err(Fault("steer_turn_too_large"));
    }
    raw.items
        .into_iter()
        .map(|item| {
            Ok(match item.kind.as_str() {
                "userMessage" => {
                    let u = User {
                        id: parse(item.id.as_deref())?,
                        client: parse(item.client.as_deref())?,
                    };
                    if !valid(&u.id) {
                        return Err(Fault("malformed_response"));
                    }
                    Item::User(u)
                }
                "agentMessage" => {
                    let a = Agent {
                        id: parse(item.id.as_deref())?,
                        text: parse(item.text.as_deref())?,
                        phase: parse(item.phase.as_deref())?,
                    };
                    if !valid(&a.id) {
                        return Err(Fault("malformed_response"));
                    }
                    Item::Agent(a)
                }
                _ => Item::Other,
            })
        })
        .collect()
}
fn mentions(items: &[Item], client: &str) -> bool {
    items
        .iter()
        .any(|i| matches!(i, Item::User(u) if u.client.as_deref() == Some(client)))
}

/// Reads the exact turn newest-first. A client ID in any other turn means the
/// input was not recorded where it was steered: fail closed, never attribute.
async fn locate(c: &mut Transport, thread: &str, turn: &str, client: &str) -> Result<Turn> {
    let mut cursor: Option<String> = None;
    for _ in 0..MAX_PAGES {
        let page: Page = c.steer_turn_page(thread, cursor.as_deref()).await?;
        if page.data.len() > 1 {
            return Err(Fault("malformed_response"));
        }
        for t in page.data {
            if t.view != "full" {
                return Err(Fault("unsupported_items_view"));
            }
            let items = project(t.items)?;
            if t.id == turn {
                return Ok(Turn {
                    status: t.status,
                    items,
                });
            }
            if mentions(&items, client) {
                return Err(Fault("correlation_changed"));
            }
        }
        match page.next {
            Some(next) if next.len() <= 4096 && Some(&next) != cursor.as_ref() => {
                cursor = Some(next)
            }
            _ => return Err(Fault("steer_turn_not_found")),
        }
    }
    Err(Fault("steer_turn_not_found"))
}

/// Pre-dispatch anchor: the steered turn must be the latest, in progress and
/// must not already contain this client ID.
pub(super) async fn anchor(
    c: &mut Transport,
    thread: &str,
    turn: &str,
    client: &str,
) -> Result<SteerAnchor> {
    let mut page: Page = c.steer_turn_page(thread, None).await?;
    if page.data.len() > 1 {
        return Err(Fault("malformed_response"));
    }
    let t = page
        .data
        .pop()
        .filter(|t| t.id == turn && t.status == "inProgress" && t.view == "full")
        .ok_or(Fault("active_turn_changed"))?;
    let items = project(t.items)?;
    if mentions(&items, client) {
        return Err(Fault("ambiguous_native_correlation"));
    }
    Ok(SteerAnchor {
        turn_id: turn.into(),
        user_items_before: items.iter().filter(|i| matches!(i, Item::User(_))).count() as u32,
        items_before: items.len() as u32,
        last_agent_item_id: items.iter().rev().find_map(|i| match i {
            Item::Agent(a) => Some(a.id.clone()),
            _ => None,
        }),
    })
}

/// Pure attribution over one projected turn; see module documentation.
fn attribute(turn: Turn, receipt: &Receipt, generation: &str) -> Result<Option<Receipt>> {
    let client = receipt.message_id.as_str();
    let previous = receipt.steer.clone().unwrap_or_default();
    let positions: Vec<usize> = turn
        .items
        .iter()
        .enumerate()
        .filter(|(_, i)| matches!(i, Item::User(u) if u.client.as_deref() == Some(client)))
        .map(|(n, _)| n)
        .collect();
    let terminal = match turn.status.as_str() {
        "inProgress" => false,
        "completed" | "interrupted" | "failed" => true,
        _ => return Err(Fault("unsupported_turn_status")),
    };
    let mut observed = receipt.clone();
    let Some(&position) = positions.first() else {
        if previous.input_item_id.is_some() {
            // A once-recorded steer input never silently disappears (compaction
            // or history rewrite): keep the durable evidence, report the gap.
            return Err(Fault("steer_anchor_lost"));
        }
        if terminal && receipt.state == "native_accepted" {
            observed.state = "failed".into();
            observed.reason = Some("input_not_consumed".into());
            observed.steer = Some(SteerEvidence {
                turn_status: Some(turn.status),
                observed_generation: Some(generation.into()),
                ..previous
            });
            return Ok(Some(observed));
        }
        // In progress (not yet recorded) or an uncertain dispatch: no claim.
        return Ok(None);
    };
    if positions.len() > 1 {
        return Err(Fault("ambiguous_native_correlation"));
    }
    let Item::User(input) = &turn.items[position] else {
        unreachable!()
    };
    // Codex renumbers legacy `item-N` ids when it rebuilds a turn from history
    // at turn end. Exactly as for submit (`observe::legacy_projection_id`), only
    // that verified transition reconciles an id: the client ID still matched
    // exactly once, the earlier id came from the in-progress projection, and
    // both ids are legacy counters. Anything else stays `correlation_changed`.
    let mut original_input_id = input.id.clone();
    let mut completed_input_id = None;
    if let Some(old) = &previous.input_item_id {
        let known = previous.completed_input_item_id.as_ref().unwrap_or(old);
        if known != &input.id {
            if !terminal
                || previous.completed_input_item_id.is_some()
                || previous.turn_status.as_deref() != Some("inProgress")
                || !observe::legacy_projection_id(old)
                || !observe::legacy_projection_id(&input.id)
            {
                return Err(Fault("correlation_changed"));
            }
            completed_input_id = Some(input.id.clone());
        } else if previous.completed_input_item_id.is_some() {
            completed_input_id = Some(input.id.clone());
        }
        original_input_id = old.clone();
    }
    if let Some(anchor) = &previous.anchor {
        let users_before = turn.items[..position]
            .iter()
            .filter(|i| matches!(i, Item::User(_)))
            .count();
        // The anchor's agent id was read from the in-progress projection; in a
        // finished turn rebuilt with renumbered legacy ids, the same counter may
        // name a different item, so it is no evidence either way there.
        let renumbered = terminal
            && anchor
                .last_agent_item_id
                .as_deref()
                .is_some_and(observe::legacy_projection_id);
        let late_agent = !renumbered
            && anchor.last_agent_item_id.as_ref().is_some_and(|id| {
                turn.items[position + 1..]
                    .iter()
                    .any(|i| matches!(i, Item::Agent(a) if &a.id == id))
            });
        if anchor.turn_id != receipt.submit.as_ref().map_or("", |s| &s.expected_turn_id)
            || users_before < anchor.user_items_before as usize
            || late_agent
        {
            return Err(Fault("correlation_changed"));
        }
    }
    // The reply window ends at the next user input (a later steer) or turn end.
    let window = &turn.items[position + 1..];
    let end = window
        .iter()
        .position(|i| matches!(i, Item::User(_)))
        .unwrap_or(window.len());
    let next_input = window.get(end).and_then(|i| match i {
        Item::User(u) => Some(u.id.clone()),
        _ => None,
    });
    let agents: Vec<&Agent> = window[..end]
        .iter()
        .filter_map(|i| match i {
            Item::Agent(a) if !a.text.is_empty() => Some(a),
            _ => None,
        })
        .collect();
    let reply = agents
        .iter()
        .rev()
        .find(|a| a.phase.as_deref() == Some("final_answer"))
        .or(agents.last())
        .copied();
    if reply.is_some_and(|a| a.text.len() > MAX_REPLY_BYTES) {
        return Err(Fault("reply_too_large"));
    }
    observed.steer = Some(SteerEvidence {
        anchor: previous.anchor,
        input_item_id: Some(original_input_id),
        completed_input_item_id: completed_input_id,
        next_input_item_id: next_input.clone(),
        turn_status: Some(turn.status.clone()),
        agent_items_after: agents.len() as u32,
        shared_with_turn_tail: next_input.is_none() && reply.is_some(),
        observed_generation: Some(generation.into()),
    });
    observed.reply = reply.map(|a| observe::Reply {
        item_id: a.id.clone(),
        text: a.text.clone(),
        phase: a.phase.clone(),
    });
    observed.reason = None;
    observed.state = match turn.status.as_str() {
        "inProgress" => "execution_started",
        "completed" if observed.reply.is_some() => "completed_with_reply",
        "completed" => "completed_without_reply",
        _ => {
            observed.reason = Some("steer_turn_interrupted".into());
            "failed"
        }
    }
    .into();
    Ok(Some(observed))
}

pub(super) fn terminal(state: &str) -> bool {
    matches!(
        state,
        "completed_with_reply"
            | "completed_without_reply"
            | "failed"
            | "rejected"
            | "cancelled"
            | "expired"
    )
}

/// Read-only observation without the writer lease; the caller commits only if
/// the receipt did not change meanwhile.
pub(in crate::messaging) async fn snapshot(
    paths: &Paths,
    receipt: &Receipt,
) -> Result<Option<Receipt>> {
    let turn = receipt
        .submit
        .as_ref()
        .map(|s| s.expected_turn_id.clone())
        .ok_or(Fault("steer_anchor_missing"))?;
    let selected = attachment(paths, &receipt.session_id)?;
    let generation = selected.generation();
    // The receipt budget grows with the one full-turn frame it may read.
    let budget = crate::messaging::transport::frame_budget(
        std::time::Duration::from_secs(5),
        crate::messaging::transport::STEER_FRAME_LIMIT,
    );
    let observed = tokio::time::timeout(budget, async {
        let mut c = selected.connect_steer().await?;
        let t = locate(&mut c, &receipt.session_id, &turn, &receipt.message_id).await?;
        attribute(t, receipt, &generation)
    })
    .await
    .map_err(|_| Fault("observation_timeout"))??;
    if attachment(paths, &receipt.session_id)?.generation() != generation {
        return Err(Fault("generation_mismatch"));
    }
    Ok(observed)
}

#[cfg(test)]
mod tests;
