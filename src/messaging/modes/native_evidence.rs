//! Exact-profile public evidence only. Not a subscription or native sender.
//! item/completed(userMessage) proves recording, not model consumption or reply causality.
use super::*;
use serde_json::value::RawValue;
#[derive(Deserialize)]
struct Notification {
    method: String,
    params: Box<RawValue>,
}
#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct ItemNotification {
    thread_id: String,
    turn_id: String,
    item: Box<RawValue>,
}
#[derive(Deserialize)]
struct Kind {
    #[serde(rename = "type")]
    kind: String,
}
#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct User {
    id: String,
    client_id: Option<String>,
    // User content and every other field are skipped, never exported or stored.
}
fn parse<T: serde::de::DeserializeOwned>(raw: &RawValue) -> Result<T> {
    serde_json::from_str(raw.get()).map_err(|_| Fault("malformed_public_event"))
}
pub(super) fn recorded_input(
    raw: &RawValue,
    session: &str,
    turn: &str,
    client: &str,
) -> Result<String> {
    if raw.get().len() > 65536 {
        return Err(Fault("input_too_large"));
    }
    let event: Notification = parse(raw)?;
    if event.method != "item/completed" {
        return Err(Fault("unsupported_public_event"));
    }
    let params: ItemNotification = parse(&event.params)?;
    if params.thread_id != session || params.turn_id != turn {
        return Err(Fault("correlation_changed"));
    }
    let kind: Kind = parse(&params.item)?;
    if kind.kind != "userMessage" {
        return Err(Fault("unsupported_public_event"));
    }
    let user: User = parse(&params.item)?;
    if user.client_id.as_deref() != Some(client) {
        return Err(Fault("uncorrelated"));
    }
    if user.id.is_empty() || user.id.len() > 256 || user.id.chars().any(char::is_control) {
        return Err(Fault("malformed_public_event"));
    }
    Ok(user.id)
}
