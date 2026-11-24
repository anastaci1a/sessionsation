//! Claude's answers to `provider::ops`.
use crate::provider::{
    ProviderCapabilities, ProviderId, claude,
    ops::{
        ArchiveLayout, Enrollment, Facts, Headless, LiveSteering, Logout, ModelGrammar,
        ProviderOps, Reader, Rekey, StopMethod, TerminalInput, TranscriptFormat,
    },
};
use crate::{config::Config, model::Account, paths::Paths};
use serde_json::{Value, json};
use std::path::Path;

pub struct Ops;
pub static OPS: Ops = Ops;
pub const FACTS: Facts = Facts {
    hosted_sessions: true,
    bare_name: false,
    supervised: false,
    meter: Reader::OAuth,
    reset_credits: Reader::OAuth,
    auth_modes: false,
    adopts_config_dir: true,
    bypass_warning: true,
    lists_account_logout: true,
    peer_input_delivery: true,
    native_account_switch: true,
    detects_questions: true,
    questions_unsupported: None,
    herdr_attach_refusal: None,
    stop: StopMethod::ClaudeProcess,
    enrollment: Enrollment::ClaudeHome,
    logout: Logout::ClaudeCli,
    logout_method: crate::logout::CLAUDE_METHOD,
    live_steering: LiveSteering::ClaudePeer,
    auth_plan: true,
    model: ModelGrammar::Claude,
    statusline_record: true,
    held_brief: true,
    shared_state: true,
    cli_options: false,
    logged_in: Some("logged in (claude.ai subscription)"),
    rekey: Rekey::ClaudeProjects,
    headless: Headless::ClaudePrint,
    headless_env: &[],
    archive: ArchiveLayout::ClaudeProjects,
    transcript: TranscriptFormat::ClaudeJsonl,
    terminal_input: TerminalInput::Claude,
    plan_names: &[("pro", "Claude Pro")],
};
impl ProviderOps for Ops {
    fn id(&self) -> ProviderId {
        ProviderId::Claude
    }
    fn capabilities(&self) -> ProviderCapabilities {
        super::capabilities()
    }
    fn facts(&self) -> &'static Facts {
        &FACTS
    }
    /// Stop (1.59.0): exactly while a verified, registered interactive process runs it.
    fn native_stop_reason(
        &self,
        running: bool,
        observed: Option<&claude::Observation>,
        _caps: &ProviderCapabilities,
    ) -> Option<&'static str> {
        if !running {
            return Some("SESSION_NOT_RUNNING");
        }
        match observed {
            Some(o)
                if o.native_kind.as_deref().is_some_and(|k| k != "interactive")
                    || o.job_id.is_some() =>
            {
                Some(claude::stop::BACKGROUND)
            }
            // Verified by the registry (not only by a launch's argv: no conversation yet).
            Some(o)
                if o.pid.is_some() && o.detail.as_deref() != Some(claude::NO_CONVERSATION_YET) =>
            {
                None
            }
            _ => Some(claude::stop::NOT_REGISTERED),
        }
    }
    fn add_session_input(&self, paths: &Paths, value: &mut Value, home: Option<&Path>) {
        crate::messaging::claude_steer::apply_session_input(paths, value, home);
    }
    /// The last `auth status` observation.
    fn account_plan(&self, a: &Account, _at: i64) -> Value {
        crate::plan::project(a.provider, a.plan.as_ref(), "auth_status")
    }
    /// Projected from the statusline recorder.
    fn account_usage(
        &self,
        paths: &Paths,
        a: &Account,
        _config: &Config,
        at: i64,
    ) -> (Value, bool, String) {
        match crate::provider::account_home(paths, a) {
            Ok(home) => {
                let (usage, fresh, state) = super::project_usage(&home, at);
                (usage, fresh, state.into())
            }
            Err(_) => (
                json!({"short":null,"weekly":null,"observed_at":null,"exhausted":false,"state":"error",
                    "source":super::USAGE_SOURCE,"semantics":super::USAGE_SEMANTICS}),
                false,
                "error".into(),
            ),
        }
    }
}
