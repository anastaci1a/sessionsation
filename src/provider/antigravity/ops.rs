//! Antigravity's answers to `provider::ops`.
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
    hosted_sessions: false,
    bare_name: false,
    supervised: false,
    meter: Reader::None,
    reset_credits: Reader::None,
    auth_modes: false,
    adopts_config_dir: false,
    bypass_warning: false,
    lists_account_logout: false,
    peer_input_delivery: false,
    native_account_switch: false,
    detects_questions: false,
    questions_unsupported: Some(crate::questions::NO_DETECTION_ANTIGRAVITY),
    herdr_attach_refusal: Some(super::NO_HERDR_ATTACH),
    stop: StopMethod::AntigravityProcess,
    enrollment: Enrollment::ManagedHome {
        create_home: super::create_home,
        auth_report: super::auth_report,
    },
    logout: Logout::Refused(super::NO_LOGOUT),
    logout_method: crate::logout::ANTIGRAVITY_METHOD,
    live_steering: LiveSteering::None,
    auth_plan: false,
    model: ModelGrammar::None,
    statusline_record: false,
    held_brief: false,
    shared_state: false,
    cli_options: false,
    logged_in: Some("logged in (Google OAuth)"),
    rekey: Rekey::Unsupported,
    headless: Headless::AgyJson,
    headless_env: &[],
    archive: ArchiveLayout::AgyIndex,
    transcript: TranscriptFormat::AgyLog,
    terminal_input: TerminalInput::Agy,
    plan_names: &[],
};
impl ProviderOps for Ops {
    fn id(&self) -> ProviderId {
        ProviderId::Antigravity
    }
    fn capabilities(&self) -> ProviderCapabilities {
        super::capabilities()
    }
    fn facts(&self) -> &'static Facts {
        &FACTS
    }
    /// Stop: exactly while a verified Antigravity process runs the conversation.
    fn native_stop_reason(
        &self,
        running: bool,
        observed: Option<&claude::Observation>,
        _caps: &ProviderCapabilities,
    ) -> Option<&'static str> {
        if !running {
            Some("SESSION_NOT_RUNNING")
        } else if observed.is_some_and(|o| o.pid.is_some()) {
            None
        } else {
            Some("SESSION_STATE_UNVERIFIED: no verified Antigravity process")
        }
    }
    fn add_session_input(&self, paths: &Paths, value: &mut Value, home: Option<&Path>) {
        crate::messaging::claude_steer::antigravity::apply_session_input(
            value,
            home,
            crate::messaging::claude_steer::terminal::enabled(paths),
        );
    }
    fn account_plan(&self, _a: &Account, _at: i64) -> Value {
        Value::Null
    }
    /// No usage is reported (state `unsupported`).
    fn account_usage(
        &self,
        _paths: &Paths,
        _a: &Account,
        _config: &Config,
        _at: i64,
    ) -> (Value, bool, String) {
        (
            json!({"short":null,"weekly":null,"observed_at":null,"exhausted":false,"state":"unsupported",
                "source":super::USAGE_SOURCE,"semantics":super::USAGE_SEMANTICS}),
            false,
            "unsupported".into(),
        )
    }
}
