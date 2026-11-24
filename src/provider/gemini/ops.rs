//! Gemini's answers to `provider::ops`.
use crate::provider::{
    ProviderCapabilities, ProviderId, claude,
    ops::{
        ArchiveLayout, Enrollment, Facts, Headless, LiveSteering, Logout, ModelGrammar,
        ProviderOps, Reader, Rekey, StopMethod, TerminalInput, TranscriptFormat, table_stop_reason,
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
    auth_modes: true,
    adopts_config_dir: false,
    bypass_warning: false,
    lists_account_logout: true,
    peer_input_delivery: false,
    native_account_switch: false,
    detects_questions: false,
    questions_unsupported: Some(crate::questions::NO_DETECTION_GEMINI),
    herdr_attach_refusal: Some(super::NO_HERDR_ATTACH),
    stop: StopMethod::Refused,
    enrollment: Enrollment::ManagedHome {
        create_home: super::create_home,
        auth_report: super::auth_report,
    },
    logout: Logout::GeminiFiles,
    logout_method: crate::logout::GEMINI_METHOD,
    live_steering: LiveSteering::GeminiModelSteering,
    auth_plan: false,
    model: ModelGrammar::None,
    statusline_record: false,
    held_brief: false,
    shared_state: false,
    cli_options: false,
    logged_in: Some("logged in (Login with Google)"),
    rekey: Rekey::Unsupported,
    headless: Headless::GeminiJson,
    headless_env: &[("GEMINI_CLI_TRUST_WORKSPACE", "true")],
    archive: ArchiveLayout::GeminiChats,
    transcript: TranscriptFormat::GeminiChat,
    terminal_input: TerminalInput::Gemini,
    plan_names: &[],
};
impl ProviderOps for Ops {
    fn id(&self) -> ProviderId {
        ProviderId::Gemini
    }
    fn capabilities(&self) -> ProviderCapabilities {
        super::capabilities()
    }
    fn facts(&self) -> &'static Facts {
        &FACTS
    }
    fn native_stop_reason(
        &self,
        _running: bool,
        _observed: Option<&claude::Observation>,
        caps: &ProviderCapabilities,
    ) -> Option<&'static str> {
        table_stop_reason(caps)
    }
    fn add_session_input(&self, paths: &Paths, value: &mut Value, home: Option<&Path>) {
        crate::messaging::claude_steer::gemini::apply_session_input(
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
