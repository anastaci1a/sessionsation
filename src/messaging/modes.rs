//! Native Queue with durable receipts; Submit remains capability-gated.
//! The isolated fixture exercises the durable dispatch boundary without sockets.
use super::*;
mod engine;
mod native_evidence;
pub(super) mod queue;
pub(super) mod session_status;
mod submit;
#[cfg(test)]
mod tests;
const VERSION: &str = "1.4.0";
const ORIGIN: &str = "https://satellite.bleak-opaleye.ts.net:10000";
#[derive(clap::Subcommand)]
pub enum Command {
    Capabilities,
    Request,
    Status {
        #[arg(long)]
        request: String,
    },
    Events {
        #[arg(long)]
        cursor: Option<String>,
    },
    Cancel {
        #[arg(long)]
        request: String,
    },
    /// Owner-controlled emergency switch; never dispatches a message.
    Sending {
        #[arg(long, conflicts_with = "disable")]
        enable: bool,
        #[arg(long)]
        disable: bool,
    },
    /// Read-only native turn precondition assessment; does not send input.
    SubmitCheck {
        #[arg(long)]
        session: String,
        #[arg(long)]
        expected_turn: Option<String>,
    },
    /// Discover existing owner-managed targets; never starts a worker.
    Sessions,
    /// Read-only managed target state; never starts a worker.
    Target {
        #[arg(long)]
        session: String,
    },
    /// Run the deterministic isolated fixture from JSON stdin. No native connection.
    Fixture,
}
#[derive(Clone, Debug, Serialize, Deserialize, PartialEq)]
#[serde(rename_all = "snake_case")]
pub enum Mode {
    Submit,
    Queue,
}
#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Request {
    pub request_id: String,
    pub session_id: String,
    pub mode: Mode,
    pub text: String,
    pub expected_generation: String,
    pub expected_turn_id: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub expected_previous_turn_id: Option<String>,
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub resume: bool,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub expected_stop_id: Option<String>,
    pub capability_id: String,
    pub created_at: i64,
    pub expires_at: i64,
}
impl Request {
    fn validate(&self) -> Result<()> {
        uuid(&self.request_id)?;
        uuid(&self.session_id)?;
        if let Some(id) = &self.expected_stop_id {
            uuid(id)?;
            if !self.resume {
                return Err(Fault("invalid_precondition"));
            }
        }
        if self.text.is_empty()
            || self.text.len() > 16384
            || self.expected_generation.len() != 64
            || !self
                .expected_generation
                .bytes()
                .all(|b| b.is_ascii_hexdigit())
            || self.capability_id.is_empty()
            || self.capability_id.len() > 128
            || self
                .expected_turn_id
                .as_ref()
                .is_some_and(|t| t.is_empty() || t.len() > 256 || t.chars().any(char::is_control))
            || self.created_at < 0
            || self.expires_at <= self.created_at
            || self.expires_at - self.created_at > 3600
        {
            return Err(Fault("invalid_request"));
        }
        if self
            .expected_previous_turn_id
            .as_ref()
            .is_some_and(|s| s.is_empty() || s.len() > 256 || s.chars().any(char::is_control))
        {
            return Err(Fault("invalid_precondition"));
        }
        if self.expected_turn_id.is_some()
            && (self.resume || self.expected_previous_turn_id.is_some())
        {
            return Err(Fault("invalid_precondition"));
        }
        if self.mode != Mode::Submit
            && (self.expected_turn_id.is_some()
                || self.expected_previous_turn_id.is_some()
                || self.resume)
        {
            return Err(Fault("invalid_precondition"));
        }
        Ok(())
    }
    fn fingerprint(&self) -> Result<String> {
        // Typed struct serializes in fixed field order; null defaults normalize omitted fields.
        Ok(format!(
            "{:x}",
            Sha256::digest(serde_json::to_vec(self).map_err(|_| Fault("invalid_request"))?)
        ))
    }
}
fn capabilities(paths: &Paths) -> Result<Value> {
    let enabled = queue::sending_enabled(paths)?;
    Ok(
        json!({"protocol_version":VERSION,"adapter_version":env!("CARGO_PKG_VERSION"),"deployment":"stock_submit_queue_and_deliberate_resume","native_sending_enabled":enabled,"live_validated":false,"native_runtime_build":null,"native_runtime_build_reason":"no_unconfigured_endpoint_probe","inspected_profile":"0.163.0","inspected_source":"83086188ae0614bd882818ff1aa23f3194962012","modes":{"submit":{"label_idle":"Send","label_busy":"Steer","fixture":true,"live":enabled,"idle_supported":enabled,"busy_supported":enabled,"method":"turn/steer","receipt_guarantee":"acceptance_only_consumption_and_reply_unknown","idle_method":"turn/start","idle_precondition":"read_only_preflight_not_atomic","idle_race":"may_steer_a_concurrently_started_turn","idle_race_accepted_by_owner":true,"resume":"explicit_new_input_to_existing_loaded_terminal_session","busy_requires":"expected_turn_id","busy_limitation":"pending_acceptance_not_consumption_and_no_durable_steer_reply_link"},"queue":{"label":"Queue","fixture":true,"live":enabled,"method":"thread/queue/add","delivery":"native_fifo_start_if_idle","deduplication":"adapter_only_never_replay_unknown","expiry":"native_submission_deadline_not_native_execution_deadline","legacy_interface":"message enqueue (1.2.1) unchanged"}},"target_policy":"all_owner_managed_sessions","recipient_registration_required":false,"authentication":"trusted_local_os_owner; remote_auth_csrf_origin_required_in_agent_dm","operations":{"sessions":true,"target":true,"request":"queue_submit_and_explicit_resume_when_target_capable","status":true,"events":true,"cancel":true,"sending":true,"submit_check":true,"fixture":true},"polling":{"minimum_seconds":5,"max_backoff_seconds":60,"jitter":"nonnegative_0_to_1_seconds"},"capability_validity":{"kind":"contract_profile_generation_digest_not_authorization","revalidate":"native_version_ownership_generation_and_global_gate_at_dispatch","native_build_attestation":"not_available"},"mailbox_authentication_owner":"agent-dm","required_origin":ORIGIN,"runtime_network_dependencies":[]}),
    )
}
fn input<T: serde::de::DeserializeOwned>() -> Result<T> {
    let mut bytes = Vec::new();
    std::io::stdin().take(65537).read_to_end(&mut bytes)?;
    if bytes.len() > 65536 {
        return Err(Fault("input_too_large"));
    }
    serde_json::from_slice(&bytes).map_err(|_| Fault("invalid_request"))
}
// Use the authoritative managed binding, process lifetime, catalog owner and safe
// native endpoint checks. No recipient policy or caller-supplied home/socket.
async fn target(paths: &Paths, id: &str) -> Result<Value> {
    session_status::target(paths, id).await
}

async fn execute(paths: &Paths, command: Command) -> Result<Value> {
    match command {
        Command::Capabilities => capabilities(paths),
        Command::Request => queue::request(paths, input()?).await,
        Command::Status { request } => queue::status(paths, &request).await,
        Command::Events { cursor } => queue::events(paths, cursor.as_deref()),
        Command::Cancel { request } => queue::cancel_request(paths, &request).await,
        Command::Sending { enable, disable } => queue::sending(paths, enable, disable),
        Command::SubmitCheck {
            session,
            expected_turn,
        } => submit::check(paths, &session, expected_turn.as_deref()).await,
        Command::Target { session } => target(paths, &session).await,
        Command::Sessions => {
            let mut ids: Vec<_> =
                external::messaging_inventory(paths).map_err(|_| Fault("discovery_unavailable"))?;
            ids.sort();
            ids.dedup();
            if ids.len() > 256 {
                return Err(Fault("discovery_limit"));
            }
            let mut sessions = Vec::new();
            for id in ids {
                match target(paths, &id).await {
                    Ok(value) => sessions.push(value),
                    Err(error) => sessions
                        .push(json!({"session_id":id,"display_name":null,"account_name":null,"state":"unknown","availability":"unavailable","generation":null,"reason":error.0,"observed_at":crate::daemon::now(),"valid_until":crate::daemon::now()+5,"actions":{"submit":{"available":false,"reason":error.0},"queue":{"available":false,"reason":error.0},"resume":{"available":false,"reason":error.0}}})),
                }
            }
            Ok(json!({"sessions":sessions}))
        }
        Command::Fixture => engine::fixture(input()?),
    }
}
pub(super) async fn run(paths: &Paths, command: Command) -> bool {
    let (ok, v) = match execute(paths, command).await {
        Ok(v) => (
            true,
            json!({"contract_version":VERSION,"ok":true,"result":v}),
        ),
        Err(e) => (
            false,
            json!({"contract_version":VERSION,"ok":false,"error":{"code":e.0}}),
        ),
    };
    println!("{v}");
    ok
}
