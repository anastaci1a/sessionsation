//! One `api send` request shape for every provider (contract 1.64.0, SM1).
//!
//! A caller sends the same live request to a Codex, Claude, Antigravity or Gemini session:
//! a mode intent (`deliver`, `when_idle`, `interrupt`) or one of the provider modes
//! (`submit`, `queue`, `steer`), plus any of the per-provider options. [`plan`] maps the
//! intent to the provider's own mode and busy handling, and takes out every option the
//! target's provider does not use, naming it in `ignored_options` with one `ignored_reason`
//! instead of refusing the request. The provider senders then run unchanged: their
//! journaling, per-session serialization, peer protocol and no-replay rules stay as they are.
//! Every result and receipt carries the same `delivery` block ([`block`]).
use super::{ProviderId, native};
use serde::{Deserialize, Serialize};
use serde_json::{Map, Value, json};

/// Provider-neutral mode intents.
pub const INTENTS: [&str; 3] = ["deliver", "when_idle", "interrupt"];
/// Every mode the live send path accepts (resume and new are headless or recovery sends).
const LIVE_MODES: [&str; 6] = [
    "submit",
    "queue",
    "steer",
    "deliver",
    "when_idle",
    "interrupt",
];
/// Options a caller may send to any provider; the ones a provider does not use are ignored.
pub const OPTIONS: [&str; 8] = [
    "delivery",
    "await_reply",
    "timeout_seconds",
    "if_busy",
    "recovery",
    "steering.expected_turn_id",
    "steering.expected_runtime_generation",
    "steering.interrupt",
];
const CLAUDE_PATHS: [&str; 3] = ["peer_message", "native", "hosted_stream_json"];

/// What the unified layer decided for one request; stored with the provider's record so a
/// receipt reports it too.
#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize)]
pub struct Plan {
    /// The request's `mode` as the caller sent it (an intent or a provider mode).
    pub mode_requested: String,
    #[serde(default)]
    pub ignored_options: Vec<String>,
    #[serde(default)]
    pub ignored_reason: Option<String>,
    /// A Codex `interrupt` that found no turn it could steer was delivered as `deliver`;
    /// why (the steer target's reason).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub fallback_reason: Option<String>,
}
impl Plan {
    /// The mode intent, when the caller sent one.
    pub fn intent(&self) -> Option<&str> {
        INTENTS.iter().find(|i| **i == self.mode_requested).copied()
    }
}

/// Why each option does not apply to a provider (None: it applies).
fn not_used(provider: ProviderId, option: &str, value: &Value) -> Option<&'static str> {
    let codex = provider == ProviderId::Codex;
    let claude = provider == ProviderId::Claude;
    match option {
        "if_busy" if !codex => Some("a Codex busy fallback; busy handling follows the mode"),
        "recovery" if !codex => Some("a Codex message recovery intent"),
        "steering.expected_turn_id" | "steering.expected_runtime_generation" if !codex => {
            Some("a Codex turn target")
        }
        "steering.interrupt" if !claude => {
            Some("only a hosted Claude session can interrupt its running turn")
        }
        "timeout_seconds" if codex => {
            Some("a terminal and peer worker timeout; Codex deliveries use the native queue")
        }
        "await_reply" if !claude => Some("a Claude peer-message idle subscription"),
        "delivery" => {
            let d = value.as_str().unwrap_or_default();
            if codex && (CLAUDE_PATHS.contains(&d) || d == "terminal_input") {
                Some("a Claude delivery path; Codex delivers through its native queue")
            } else if !codex && !claude && CLAUDE_PATHS.contains(&d) {
                Some("a Claude delivery path; this provider takes typed terminal input only")
            } else {
                None
            }
        }
        _ => None,
    }
}

/// Map one raw live `api send` request for the target's provider (None: the session is not
/// in the catalog; the Codex contract answers it). Returns None, leaving the request as it
/// is, for every other mode (`resume`, `new`, a missing or unknown mode).
pub fn plan(provider: Option<ProviderId>, v: &mut Value) -> Option<Plan> {
    let provider = provider.unwrap_or(ProviderId::Codex);
    let obj = v.as_object_mut()?;
    let requested = obj.get("mode")?.as_str()?.to_owned();
    if !LIVE_MODES.contains(&requested.as_str()) {
        return None;
    }
    let codex = provider == ProviderId::Codex;
    let mut ignored: Vec<(String, &'static str)> = vec![];
    let intent = INTENTS.contains(&requested.as_str());
    for key in ["if_busy", "recovery", "timeout_seconds", "delivery"] {
        let Some(value) = obj.get(key).filter(|x| !x.is_null()) else {
            continue;
        };
        if let Some(why) = not_used(provider, key, value) {
            obj.remove(key);
            ignored.push((key.to_owned(), why));
        }
    }
    if obj.get("await_reply") == Some(&Value::Bool(true))
        && let Some(why) = not_used(provider, "await_reply", &Value::Bool(true))
    {
        // Accepted (and stored) by every provider's request; only reported here.
        ignored.push(("await_reply".to_owned(), why));
    }
    if let Some(Value::Object(steering)) = obj.get_mut("steering") {
        for key in [
            "expected_turn_id",
            "expected_runtime_generation",
            "interrupt",
        ] {
            let name = format!("steering.{key}");
            if let Some(value) = steering.get(key)
                && let Some(why) = not_used(provider, &name, value)
            {
                steering.remove(key);
                ignored.push((name, why));
            }
        }
        // Codex steering is a turn target: a version alone is no target.
        if steering.keys().all(|k| k == "version") && (codex || steering.is_empty()) {
            obj.remove("steering");
        }
    }
    if intent {
        // The intent chooses busy handling; a Codex `if_busy` is the intent's to set.
        if codex
            && let Some(old) = obj.remove("if_busy")
            && !(requested == "deliver" && old == "queue")
        {
            ignored.push((
                "if_busy".to_owned(),
                "busy handling follows the mode intent",
            ));
        }
        let (mode, if_busy) = mode_for(provider, &requested);
        obj.insert("mode".into(), json!(mode));
        if let Some(b) = if_busy {
            obj.insert("if_busy".into(), json!(b));
        }
    }
    let ignored_reason = (!ignored.is_empty()).then(|| {
        let parts: Vec<String> = ignored
            .iter()
            .map(|(k, why)| format!("{k} ({why})"))
            .collect();
        format!(
            "{} sessions do not use {}; the message was sent without them",
            native::title(provider),
            parts.join(", ")
        )
    });
    Some(Plan {
        mode_requested: requested,
        ignored_options: ignored.into_iter().map(|(k, _)| k).collect(),
        ignored_reason,
        fallback_reason: None,
    })
}

/// The provider mode (and Codex `if_busy`) an intent maps to.
fn mode_for(provider: ProviderId, intent: &str) -> (&'static str, Option<&'static str>) {
    match (provider, intent) {
        // Idle: starts a turn; busy: waits in the native queue (never refused as busy).
        (ProviderId::Codex, "deliver") => ("submit", Some("queue")),
        (_, "deliver") => ("submit", None),
        (_, "when_idle") => ("queue", None),
        // Codex resolves the active turn itself (see `messaging::public`).
        _ => ("steer", None),
    }
}

/// The uniform `delivery` block of a send result and receipt. `mode_used` is the provider
/// mode that ran, `framing` the provider path, `queued` that the provider holds the message
/// in its own queue behind a running turn, `held` that it is kept back (by Sessionsation or
/// the provider) before it reaches the conversation.
pub fn block(
    plan: Option<&Plan>,
    mode_used: &str,
    framing: &str,
    queued: bool,
    held: bool,
) -> Value {
    json!({"mode_requested":plan.map_or(mode_used, |p| p.mode_requested.as_str()),
        "intent":plan.and_then(Plan::intent),"mode_used":mode_used,"framing":framing,
        "queued":queued,"held":held,"fallback_reason":plan.and_then(|p| p.fallback_reason.as_deref())})
}

/// Add the uniform fields (`delivery`, `ignored_options`, `ignored_reason`) to a result or
/// receipt.
pub fn annotate(out: &mut Value, plan: Option<&Plan>, delivery: Value) {
    out["delivery"] = delivery;
    out["ignored_options"] = json!(plan.map_or(&[][..], |p| p.ignored_options.as_slice()));
    out["ignored_reason"] = json!(plan.and_then(|p| p.ignored_reason.as_deref()));
}

/// `capabilities.send_request`.
pub fn capability() -> Value {
    let mut providers = Map::new();
    for p in ProviderId::ALL {
        let modes: Map<String, Value> = INTENTS
            .iter()
            .map(|i| {
                let (mode, if_busy) = mode_for(p, i);
                ((*i).to_owned(), json!({"mode":mode,"if_busy":if_busy}))
            })
            .collect();
        let ignored: Vec<&str> = OPTIONS
            .iter()
            .filter(|o| {
                let probe = if **o == "delivery" {
                    json!("peer_message")
                } else {
                    json!(true)
                };
                not_used(p, o, &probe).is_some()
            })
            .copied()
            .collect();
        providers.insert(
            p.as_str().into(),
            json!({"intents":modes,"ignored_options":ignored}),
        );
    }
    json!({"since_contract":"1.64.0","command":"send","intents":INTENTS,"provider_modes":["submit","queue","steer"],
        "options":OPTIONS,"unused_options":"ignored_options_not_refused","result_fields":["delivery","ignored_options","ignored_reason"],
        "delivery_keys":["mode_requested","intent","mode_used","framing","queued","held","fallback_reason"],
        "automatic_replay":false,"providers":providers})
}

#[cfg(test)]
mod tests;
