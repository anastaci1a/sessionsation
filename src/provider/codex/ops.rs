//! Codex's answers to `provider::ops`.
use crate::provider::{
    ProviderCapabilities, ProviderId, claude,
    ops::{
        ArchiveLayout, Enrollment, Facts, Headless, LiveSteering, Logout, ModelGrammar,
        ProviderOps, Reader, StopMethod, TerminalInput, TranscriptFormat, table_stop_reason,
    },
};
use crate::{config::Config, model::Account, paths::Paths};
use serde_json::{Value, json};
use std::path::Path;

pub struct Ops;
pub static OPS: Ops = Ops;
pub const FACTS: Facts = Facts {
    bare_name: true,
    supervised: true,
    meter: Reader::Supervisor,
    reset_credits: Reader::Supervisor,
    auth_modes: false,
    adopts_config_dir: false,
    bypass_warning: false,
    lists_account_logout: true,
    peer_input_delivery: false,
    native_account_switch: false,
    detects_questions: false,
    questions_unsupported: None,
    herdr_attach_refusal: None,
    stop: StopMethod::Supervisor,
    enrollment: Enrollment::Supervisor,
    logout: Logout::CodexRpc,
    logout_method: crate::logout::CODEX_METHOD,
    live_steering: LiveSteering::None,
    auth_plan: false,
    model: ModelGrammar::Codex,
    statusline_record: false,
    held_brief: false,
    shared_state: false,
    cli_options: true,
    logged_in: None,
    headless: Headless::None,
    headless_env: &[],
    archive: ArchiveLayout::Rollouts,
    transcript: TranscriptFormat::None,
    terminal_input: TerminalInput::None,
    resume_launch: crate::provider::ops::ResumeLaunch::Transfer,
    restart_without_history: false,
    plan_names: &[
        ("free", "ChatGPT Free"),
        ("plus", "ChatGPT Plus"),
        ("pro", "ChatGPT Pro"),
        ("team", "ChatGPT Team"),
        ("business", "ChatGPT Business"),
        ("enterprise", "ChatGPT Enterprise"),
    ],
};
impl ProviderOps for Ops {
    fn id(&self) -> ProviderId {
        ProviderId::Codex
    }
    fn capabilities(&self) -> ProviderCapabilities {
        super::capabilities()
    }
    fn facts(&self) -> &'static Facts {
        &FACTS
    }
    /// Codex sessions are not native rows; the table answers.
    fn native_stop_reason(
        &self,
        _running: bool,
        _observed: Option<&claude::Observation>,
        caps: &ProviderCapabilities,
    ) -> Option<&'static str> {
        table_stop_reason(caps)
    }
    /// Codex input goes through the supervisor's native queue; rows carry no terminal input.
    fn add_session_input(&self, _paths: &Paths, _value: &mut Value, _home: Option<&Path>) {}
    fn account_plan(&self, a: &Account, at: i64) -> Value {
        crate::plan::project(
            a.provider,
            crate::plan::from_codex_usage(&a.usage, at).as_ref(),
            "usage_snapshot",
        )
    }
    /// The supervisor's snapshot.
    fn account_usage(
        &self,
        _paths: &Paths,
        a: &Account,
        config: &Config,
        at: i64,
    ) -> (Value, bool, String) {
        let fresh = a.usage.fresh(at, config.stale_seconds);
        let mut usage = json!(a.usage);
        if a.usage.observed_at > at {
            usage["observed_at"] = json!(at);
        }
        if a.usage.observed_at == 0 {
            usage["observed_at"] = Value::Null;
        }
        usage["source"] = json!(super::USAGE_SOURCE);
        // Contract 1.65.0: each window's reset cycle, and the age of the reset credits.
        let windows = crate::provider::usage_cycle::WINDOWS;
        for (field, (key, duration)) in ["short", "weekly"].into_iter().zip(windows) {
            match a.usage_cycles.get(key) {
                Some(c) => c.stamp(&mut usage[field]),
                // A reading stored before 1.65.0: its natural cycle.
                None => crate::provider::usage_cycle::fill(&mut usage[field], duration),
            }
        }
        usage["reset_credits"] = crate::reset_credit::with_age(
            usage["reset_credits"].take(),
            at,
            config.stale_seconds as i64,
        );
        usage["semantics"] = json!(super::USAGE_SEMANTICS);
        let state = if fresh {
            "fresh"
        } else if a.usage.observed_at == 0 {
            "missing"
        } else {
            "stale"
        };
        (usage, fresh, state.into())
    }
}
