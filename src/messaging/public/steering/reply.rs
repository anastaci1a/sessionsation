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
mod tests {
    use super::*;
    use crate::messaging::tests::{attachment_fixture, fixture, scripted_server};
    use std::sync::{Arc as SArc, Mutex as SMutex};
    const TURN: &str = "turn-big";
    /// A live-shaped full in-progress turn whose tool output pushes one native
    /// frame past `bytes`; only the small user and agent items matter.
    fn big_turn(bytes: usize) -> Value {
        json!({"data":[{"id":TURN,"status":"inProgress","itemsView":"full","items":[
            {"type":"userMessage","id":"u0","clientId":null,"content":[{"type":"text","text":"start"}]},
            {"type":"commandExecution","id":"c0","aggregatedOutput":"x".repeat(bytes),"command":"cat big"},
            {"type":"plan","id":"p0","text":"plan"},
            {"type":"agentMessage","id":"a0","text":"working","phase":"commentary"}
        ]}],"nextCursor":null})
    }
    async fn steer_against(
        page: Value,
        steer: bool,
    ) -> (Result<SteerAnchor>, Result<String>, Vec<String>) {
        let (_dir, paths, input, _) = fixture();
        let a = attachment_fixture(&paths, &input);
        let id = a.id.clone();
        let calls = SArc::new(SMutex::new(Vec::new()));
        let seen = calls.clone();
        let server = scripted_server(&a.endpoint(), move |req| {
            let method = req["method"].as_str().unwrap().to_owned();
            seen.lock().unwrap().push(method.clone());
            match method.as_str() {
                "thread/turns/list" => {
                    assert_eq!(req["params"]["itemsView"], "full");
                    Some(page.clone())
                }
                "turn/steer" => Some(json!({"turnId":TURN})),
                _ => None,
            }
        })
        .await;
        let mut c = a.connect_steer().await.unwrap();
        let anchored = anchor(&mut c, &id, TURN, ME).await;
        let steered = if steer && anchored.is_ok() {
            c.steer(&id, ME, TURN, "hello").await
        } else {
            Err(Fault("not_sent"))
        };
        drop(c);
        server.abort();
        let calls = calls.lock().unwrap().clone();
        (anchored, steered, calls)
    }
    #[tokio::test]
    async fn steer_to_a_turn_larger_than_one_mib_is_anchored_and_sent() {
        // localica-lead: the latest full turn was 1.86 MB on the 1 MiB connection.
        let (anchored, steered, calls) = steer_against(big_turn(3 * 1024 * 1024), true).await;
        let anchored = anchored.unwrap();
        assert_eq!((anchored.user_items_before, anchored.items_before), (1, 4));
        assert_eq!(anchored.last_agent_item_id.as_deref(), Some("a0"));
        assert_eq!(steered.unwrap(), TURN);
        assert_eq!(calls, ["thread/turns/list", "turn/steer"]);
    }
    #[tokio::test]
    async fn ordinary_connection_keeps_its_one_mib_limit() {
        let (_dir, paths, input, _) = fixture();
        let a = attachment_fixture(&paths, &input);
        let id = a.id.clone();
        let page = big_turn(2 * 1024 * 1024);
        let server = scripted_server(&a.endpoint(), move |_| Some(page.clone())).await;
        let mut c = a.connect().await.unwrap();
        let e = anchor(&mut c, &id, TURN, ME).await.unwrap_err();
        assert_eq!(e.0, "native_frame_limit");
        server.abort();
    }
    #[tokio::test]
    async fn frame_over_the_steer_bound_fails_closed_without_sending() {
        let over = crate::messaging::transport::STEER_FRAME_LIMIT + 1024;
        let (anchored, steered, calls) = steer_against(big_turn(over), true).await;
        assert_eq!(anchored.unwrap_err().0, "native_frame_limit");
        assert_eq!(steered.unwrap_err().0, "not_sent");
        assert_eq!(calls, ["thread/turns/list"]);
    }
    #[test]
    fn projection_retains_only_identifiers_and_bounded_item_count() {
        let page: Page = serde_json::from_value(big_turn(1024 * 1024)).unwrap();
        let items = &page.data[0].items.items;
        assert!(!page.data[0].items.overflow);
        // Tool output and user content are skipped, never retained.
        let retained: usize = items
            .iter()
            .map(|i| {
                [&i.id, &i.client, &i.text, &i.phase]
                    .iter()
                    .map(|f| f.as_ref().map_or(0, |r| r.get().len()))
                    .sum::<usize>()
            })
            .sum();
        assert!(retained < 256, "{retained}");
        let many = json!({"data":[{"id":TURN,"status":"inProgress","itemsView":"full",
            "items":vec![json!({"type":"sleep","id":"s"}); MAX_ITEMS + 5]}],"nextCursor":null});
        let page: Page = serde_json::from_value(many).unwrap();
        let t = page.data.into_iter().next().unwrap();
        assert!(t.items.overflow && t.items.items.len() == MAX_ITEMS);
        assert_eq!(project(t.items).err().unwrap().0, "steer_turn_too_large");
    }
    fn user(id: &str, client: Option<&str>) -> Item {
        Item::User(User {
            id: id.into(),
            client: client.map(Into::into),
        })
    }
    fn agent(id: &str, text: &str, phase: &str) -> Item {
        Item::Agent(Agent {
            id: id.into(),
            text: text.into(),
            phase: Some(phase.into()),
        })
    }
    fn receipt(state: &str, anchor: Option<SteerAnchor>) -> Receipt {
        Receipt {
            recovery_start: None,
            message_id: "11111111-1111-4111-8111-111111111111".into(),
            session_id: "22222222-2222-4222-8222-222222222222".into(),
            generation: "g".into(),
            state: state.into(),
            native_queue_id: None,
            reason: None,
            correlation: None,
            reply: None,
            submit: Some(SubmitEvidence {
                expected_turn_id: "turn-1".into(),
                acknowledged_turn_id: Some("turn-1".into()),
                action: Some("public_steer".into()),
                resume: false,
            }),
            steer: Some(SteerEvidence {
                anchor,
                ..Default::default()
            }),
        }
    }
    const ME: &str = "11111111-1111-4111-8111-111111111111";
    fn run(status: &str, items: Vec<Item>, r: &Receipt) -> Result<Option<Receipt>> {
        attribute(
            Turn {
                status: status.into(),
                items,
            },
            r,
            "gen",
        )
    }
    #[test]
    fn profile_0_166_flat_item_timings_and_new_fields_project_unchanged() {
        // 0.166.0 adds optional flat `startedAtMs`/`completedAtMs` to every
        // ThreadItem and `generationId` to imageGeneration (generated schema diff);
        // the identifiers steer attribution reads are unchanged.
        let page: Page = serde_json::from_value(json!({"data":[{"id":"turn-1","status":"completed","itemsView":"full","startedAt":1,"completedAt":2,"durationMs":1000,"items":[
            {"type":"userMessage","id":"u0","clientId":null,"content":[{"type":"text","text":"start"}],"startedAtMs":1793305461849_i64,"completedAtMs":null},
            {"type":"imageGeneration","id":"ig0","status":"completed","revisedPrompt":null,"result":"","generationId":"gen-1","startedAtMs":null,"completedAtMs":null},
            {"type":"userMessage","id":"u1","clientId":ME,"content":[{"type":"text","text":"steer"}],"startedAtMs":1793305461900_i64,"completedAtMs":1793305461900_i64},
            {"type":"agentMessage","id":"msg_a","text":"done","phase":"final_answer","memoryCitation":null,"startedAtMs":1793305462000_i64,"completedAtMs":1793305463000_i64}
        ]}],"nextCursor":null})).unwrap();
        let t = page.data.into_iter().next().unwrap();
        let items = project(t.items).unwrap();
        let anchor = SteerAnchor {
            turn_id: "turn-1".into(),
            user_items_before: 1,
            items_before: 2,
            last_agent_item_id: None,
        };
        let o = run(
            "completed",
            items,
            &receipt("native_accepted", Some(anchor)),
        )
        .unwrap()
        .unwrap();
        assert_eq!(o.state, "completed_with_reply");
        assert_eq!(o.steer.unwrap().input_item_id.as_deref(), Some("u1"));
        assert_eq!(o.reply.unwrap().item_id, "msg_a");
    }
    #[test]
    fn live_shape_single_steer_reply_is_shared_with_turn_tail() {
        // Live 0.165: [user(initial), user(steer clientId), agent(final_answer)];
        // the agent item streaming at steer time was discarded natively.
        let anchor = SteerAnchor {
            turn_id: "turn-1".into(),
            user_items_before: 1,
            items_before: 1,
            last_agent_item_id: None,
        };
        let r = receipt("native_accepted", Some(anchor));
        let items = || {
            vec![
                user("u0", Some("other")),
                user("u1", Some(ME)),
                agent("msg_a", "one two STEERED", "final_answer"),
            ]
        };
        let o = run("completed", items(), &r).unwrap().unwrap();
        assert_eq!(o.state, "completed_with_reply");
        let s = o.steer.unwrap();
        assert_eq!(s.input_item_id.as_deref(), Some("u1"));
        assert!(s.shared_with_turn_tail);
        assert_eq!(o.reply.unwrap().item_id, "msg_a");
        let o = run("inProgress", items(), &r).unwrap().unwrap();
        assert_eq!(o.state, "execution_started");
        let mut early = items();
        early.pop();
        let o = run("inProgress", early, &r).unwrap().unwrap();
        assert_eq!(o.state, "execution_started");
        assert!(o.reply.is_none() && !o.steer.unwrap().shared_with_turn_tail);
    }
    /// Live shape (steer 5e998050): anchored and first observed in progress
    /// with legacy ids, then Codex rebuilt the finished turn with new counters.
    fn renumbered_receipt() -> Receipt {
        let r: Receipt = serde_json::from_value(json!({
            "message_id":ME,"session_id":"22222222-2222-4222-8222-222222222222",
            "generation":"g","state":"execution_started","native_queue_id":null,"reason":null,
            "submit":{"expected_turn_id":"turn-1","acknowledged_turn_id":"turn-1","action":"public_steer"},
            "steer":{"anchor":{"turn_id":"turn-1","user_items_before":1,"items_before":4,"last_agent_item_id":"item-2"},
                "input_item_id":"item-3","turn_status":"inProgress","agent_items_after":0,
                "shared_with_turn_tail":false,"observed_generation":"gen"}
        }))
        .unwrap();
        r
    }
    fn renumbered_turn() -> Vec<Item> {
        vec![
            user("item-1", None),
            user("item-4", Some(ME)),
            // The old anchor counter now names the reply after the steer.
            agent("item-2", "done STEERED", "final_answer"),
        ]
    }
    #[test]
    fn completion_renumbering_of_legacy_ids_reaches_a_terminal_reply() {
        let stuck = renumbered_receipt();
        let o = run("completed", renumbered_turn(), &stuck)
            .unwrap()
            .unwrap();
        assert_eq!(o.state, "completed_with_reply");
        let s = o.steer.clone().unwrap();
        assert_eq!(s.input_item_id.as_deref(), Some("item-3"));
        assert_eq!(s.completed_input_item_id.as_deref(), Some("item-4"));
        assert_eq!(s.anchor, stuck.steer.as_ref().unwrap().anchor);
        assert_eq!(o.reply.as_ref().unwrap().text, "done STEERED");
        // Re-observing the reconciled evidence is stable, never a new change.
        let again = run("completed", renumbered_turn(), &o).unwrap().unwrap();
        assert_eq!(again, o);
        // Interrupted turns reconcile the same way and stay an honest failure.
        let o = run("interrupted", renumbered_turn(), &stuck)
            .unwrap()
            .unwrap();
        assert_eq!(
            (o.state.as_str(), o.reason.as_deref()),
            ("failed", Some("steer_turn_interrupted"))
        );
    }
    #[test]
    fn only_the_verified_completion_transition_reconciles_an_input_id() {
        let stuck = renumbered_receipt();
        // Still in progress: the same projection must keep the same id.
        assert_eq!(
            run("inProgress", renumbered_turn(), &stuck).unwrap_err().0,
            "correlation_changed"
        );
        // Non-legacy ids are stable identities; a change is never reconciled.
        let mut stable = stuck.clone();
        stable.steer.as_mut().unwrap().input_item_id = Some("u-3".into());
        let mut items = renumbered_turn();
        items[1] = user("u-4", Some(ME));
        assert_eq!(
            run("completed", items, &stable).unwrap_err().0,
            "correlation_changed"
        );
        let mut items = renumbered_turn();
        items[1] = user("u-4", Some(ME));
        assert_eq!(
            run("completed", items, &stuck).unwrap_err().0,
            "correlation_changed"
        );
        // A second renumbering after reconciliation is not accepted.
        let done = run("completed", renumbered_turn(), &stuck)
            .unwrap()
            .unwrap();
        let mut items = renumbered_turn();
        items[1] = user("item-7", Some(ME));
        assert_eq!(
            run("completed", items, &done).unwrap_err().0,
            "correlation_changed"
        );
        // The earlier-input count is still enforced on a renumbered turn.
        assert_eq!(
            run(
                "completed",
                vec![
                    user("item-4", Some(ME)),
                    agent("item-2", "x", "final_answer")
                ],
                &stuck
            )
            .unwrap_err()
            .0,
            "correlation_changed"
        );
    }
    #[test]
    fn multiple_steers_are_bounded_by_the_next_input() {
        let r = receipt("native_accepted", None);
        let o = run(
            "completed",
            vec![
                user("u0", None),
                agent("a0", "before", "commentary"),
                user("u1", Some(ME)),
                Item::Other,
                agent("a1", "mine-commentary", "commentary"),
                agent("a2", "mine", "final_answer"),
                user("u2", Some("later-steer")),
                agent("a3", "tail", "final_answer"),
            ],
            &r,
        )
        .unwrap()
        .unwrap();
        assert_eq!(o.state, "completed_with_reply");
        assert_eq!(o.reply.as_ref().unwrap().item_id, "a2");
        let s = o.steer.unwrap();
        assert_eq!(s.next_input_item_id.as_deref(), Some("u2"));
        assert_eq!(s.agent_items_after, 2);
        assert!(!s.shared_with_turn_tail);
        // Steer immediately followed by another steer: no reply of its own.
        let o = run(
            "completed",
            vec![
                user("u1", Some(ME)),
                user("u2", Some("x")),
                agent("a", "t", "final_answer"),
            ],
            &r,
        )
        .unwrap()
        .unwrap();
        assert_eq!(o.state, "completed_without_reply");
        assert!(o.reply.is_none());
    }
    #[test]
    fn interrupted_and_unconsumed_and_uncertain_outcomes() {
        let r = receipt("native_accepted", None);
        for status in ["interrupted", "failed"] {
            let o = run(
                status,
                vec![user("u1", Some(ME)), agent("a1", "partial", "commentary")],
                &r,
            )
            .unwrap()
            .unwrap();
            assert_eq!(o.state, "failed");
            assert_eq!(o.reason.as_deref(), Some("steer_turn_interrupted"));
            assert_eq!(o.reply.unwrap().text, "partial");
            let o = run(status, vec![user("u0", None)], &r).unwrap().unwrap();
            assert_eq!(o.reason.as_deref(), Some("input_not_consumed"));
        }
        let o = run("completed", vec![user("u0", None)], &r)
            .unwrap()
            .unwrap();
        assert_eq!(
            (o.state.as_str(), o.reason.as_deref()),
            ("failed", Some("input_not_consumed"))
        );
        // Not yet recorded in an active turn: no claim.
        assert!(
            run("inProgress", vec![user("u0", None)], &r)
                .unwrap()
                .is_none()
        );
        // Uncertain dispatch never becomes a negative claim, only positive evidence.
        let unknown = receipt("unknown", None);
        assert!(
            run("completed", vec![user("u0", None)], &unknown)
                .unwrap()
                .is_none()
        );
        let o = run(
            "completed",
            vec![user("u1", Some(ME)), agent("a", "r", "final_answer")],
            &unknown,
        )
        .unwrap()
        .unwrap();
        assert_eq!(o.state, "completed_with_reply");
    }
    #[test]
    fn reverification_fails_closed() {
        let mut r = receipt("execution_started", None);
        r.steer.as_mut().unwrap().input_item_id = Some("u1".into());
        // Compaction or rewrite that drops a recorded steer is a gap, not a negative.
        assert_eq!(
            run("completed", vec![user("u0", None)], &r).unwrap_err().0,
            "steer_anchor_lost"
        );
        assert_eq!(
            run("completed", vec![user("other-id", Some(ME))], &r)
                .unwrap_err()
                .0,
            "correlation_changed"
        );
        let r = receipt("native_accepted", None);
        assert_eq!(
            run(
                "completed",
                vec![user("a", Some(ME)), user("b", Some(ME))],
                &r
            )
            .unwrap_err()
            .0,
            "ambiguous_native_correlation"
        );
        assert_eq!(
            run("weird", vec![], &r).unwrap_err().0,
            "unsupported_turn_status"
        );
        let anchor = SteerAnchor {
            turn_id: "turn-1".into(),
            user_items_before: 2,
            items_before: 3,
            last_agent_item_id: Some("a0".into()),
        };
        let r = receipt("native_accepted", Some(anchor.clone()));
        // Fewer earlier inputs than at steer time.
        assert_eq!(
            run(
                "completed",
                vec![user("u0", None), user("u1", Some(ME))],
                &r
            )
            .unwrap_err()
            .0,
            "correlation_changed"
        );
        // The pre-steer agent item cannot appear after the steer input.
        assert_eq!(
            run(
                "completed",
                vec![
                    user("u0", None),
                    user("u0b", None),
                    user("u1", Some(ME)),
                    agent("a0", "x", "final_answer")
                ],
                &r
            )
            .unwrap_err()
            .0,
            "correlation_changed"
        );
        // A compaction marker or discarded streaming item before it is fine.
        let o = run(
            "completed",
            vec![
                user("u0", None),
                user("u0b", None),
                Item::Other,
                user("u1", Some(ME)),
                agent("a1", "y", "final_answer"),
            ],
            &r,
        )
        .unwrap()
        .unwrap();
        assert_eq!(o.steer.unwrap().anchor, Some(anchor));
        let mut big = receipt("native_accepted", None);
        big.steer = None;
        assert_eq!(
            run(
                "completed",
                vec![
                    user("u1", Some(ME)),
                    agent("a", &"x".repeat(MAX_REPLY_BYTES + 1), "final_answer")
                ],
                &big
            )
            .unwrap_err()
            .0,
            "reply_too_large"
        );
    }
}
