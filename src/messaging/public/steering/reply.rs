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
fn parse<T: serde::de::DeserializeOwned>(raw: &RawValue) -> Result<T> {
    serde_json::from_str(raw.get()).map_err(|_| Fault("malformed_response"))
}
fn project(raw: Vec<Box<RawValue>>) -> Result<Vec<Item>> {
    if raw.len() > MAX_ITEMS {
        return Err(Fault("steer_turn_too_large"));
    }
    raw.iter()
        .map(|item| {
            Ok(match parse::<Kind>(item)?.kind.as_str() {
                "userMessage" => {
                    let u: User = parse(item)?;
                    if !valid(&u.id) {
                        return Err(Fault("malformed_response"));
                    }
                    Item::User(u)
                }
                "agentMessage" => {
                    let a: Agent = parse(item)?;
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
    if previous
        .input_item_id
        .as_ref()
        .is_some_and(|id| id != &input.id)
    {
        return Err(Fault("correlation_changed"));
    }
    if let Some(anchor) = &previous.anchor {
        let users_before = turn.items[..position]
            .iter()
            .filter(|i| matches!(i, Item::User(_)))
            .count();
        let late_agent = anchor.last_agent_item_id.as_ref().is_some_and(|id| {
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
        input_item_id: Some(input.id.clone()),
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
    let observed = tokio::time::timeout(std::time::Duration::from_secs(5), async {
        let mut c = selected.connect().await?;
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
