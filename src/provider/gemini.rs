//! Gemini CLI, dropped (it no longer signs in individual accounts). The provider id and
//! the account home path stay so an enrolled `gemini:<name>` can be listed and deleted
//! with its history preserved; every other operation is unsupported with [`REASON`].
use super::{
    ProviderCapabilities, ProviderId,
    claude::{Enrolled, Live, Observation, Transcript},
    no,
    ops::{
        ArchiveLayout, Enrollment, Facts, Headless, LiveSteering, Logout, ModelGrammar,
        ProviderOps, Reader, ResumeLaunch, StopMethod, TerminalInput, TranscriptFormat,
    },
};
use crate::{
    config::Config,
    model::Account,
    paths::{Paths, validate_name},
};
use anyhow::{Result, bail, ensure};
use serde_json::{Value, json};
use std::path::{Path, PathBuf};

pub const REASON: &str = "gemini_cli_dropped_individual_accounts";
/// The refusal of every Gemini operation.
pub fn refusal() -> anyhow::Error {
    super::ops::unsupported(REASON)
}
pub const NO_HERDR_ATTACH: &str = "NO_NATIVE_ATTACH: Gemini CLI has no supported attach to a running session; stop it in its terminal, then use Start Session";
pub const STRIPPED_ENV: &[&str] = &[];

pub fn capabilities() -> ProviderCapabilities {
    let n = || no(REASON);
    ProviderCapabilities {
        enrollment: n(),
        adopt_config_dir: n(),
        login: n(),
        auth_status: n(),
        usage: n(),
        new: n(),
        resume: n(),
        rename: n(),
        native_rename: n(),
        stop: n(),
        restart: n(),
        account_switch: n(),
        relocate: n(),
        move_space: n(),
        archive: n(),
        delete_session: n(),
        submit: n(),
        queue: n(),
        steer: n(),
        account_removal: n(),
        account_deletion: n(),
        account_rename: n(),
        account_logout: n(),
        headless_delivery: n(),
        account_limits: n(),
        cross_provider_conversion: n(),
    }
}

pub fn home(paths: &Paths, name: &str) -> Result<PathBuf> {
    validate_name(name)?;
    Ok(crate::layout::account_home(
        &paths.root,
        ProviderId::Gemini,
        name,
    ))
}
fn create_home(_: &Paths, _: &str) -> Result<(PathBuf, bool)> {
    Err(refusal())
}
/// The enrolled account `name` (the registry snapshot), for deletion and listing.
pub fn account(paths: &Paths, name: &str) -> Result<Enrolled> {
    if paths.root.join(crate::layout::REGISTRY_DB).exists() {
        let state = crate::store::load_state_snapshot(paths)?;
        if let Some(a) = state
            .accounts
            .values()
            .find(|a| a.provider == ProviderId::Gemini && a.name == name)
        {
            return Ok(Enrolled {
                name: a.name.clone(),
                home: home(paths, name)?,
                enabled: a.enabled,
                auth_required: a.auth_required,
            });
        }
    }
    ensure!(
        !deleted(paths, name)?,
        "ACCOUNT_DELETED: gemini:{name} was deleted locally; its conversation history is preserved"
    );
    bail!("ACCOUNT_NOT_FOUND: no enrolled Gemini account with this name")
}
/// True when a completed local deletion removed this (no longer enrolled) account.
pub fn deleted(paths: &Paths, name: &str) -> Result<bool> {
    let id = format!("gemini:{name}");
    Ok(crate::account_deletion::list(paths)?
        .iter()
        .any(|r| r.account_id == id && r.phase == "complete"))
}
pub fn command(_: &Config, _: &Path) -> std::process::Command {
    std::process::Command::new("false")
}
pub fn observe(_: &Path, _: &str, _: Option<&crate::catalog::Attachment>) -> Observation {
    Observation {
        availability: "unknown",
        native_status: None,
        native_name: None,
        pid: None,
        process_start: None,
        directory: None,
        transcript: None,
        transcript_bytes: None,
        started_at: None,
        attachment: None,
        detail: Some(REASON.into()),
        native_kind: None,
        job_id: None,
        attachment_switched: false,
        forked_from: None,
        active_since: None,
    }
}
pub fn transcripts(_: &Path) -> Vec<Transcript> {
    vec![]
}
pub fn live_sessions(_: &Path) -> Result<Vec<Live>> {
    Ok(vec![])
}

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
    questions_unsupported: Some(crate::questions::NO_DETECTION_GEMINI),
    herdr_attach_refusal: Some(NO_HERDR_ATTACH),
    stop: StopMethod::Refused,
    enrollment: Enrollment::ManagedHome {
        create_home,
        auth_report: |_| Value::Null,
    },
    logout: Logout::Refused(REASON),
    logout_method: crate::logout::GEMINI_METHOD,
    live_steering: LiveSteering::None,
    auth_plan: false,
    model: ModelGrammar::None,
    statusline_record: false,
    held_brief: false,
    shared_state: false,
    cli_options: false,
    logged_in: None,
    headless: Headless::None,
    headless_env: &[],
    archive: ArchiveLayout::GeminiChats,
    transcript: TranscriptFormat::None,
    terminal_input: TerminalInput::None,
    resume_launch: ResumeLaunch::Refused(super::conversation::GEMINI_UNSUPPORTED),
    restart_without_history: false,
    plan_names: &[],
};
impl ProviderOps for Ops {
    fn id(&self) -> ProviderId {
        ProviderId::Gemini
    }
    fn capabilities(&self) -> ProviderCapabilities {
        capabilities()
    }
    fn facts(&self) -> &'static Facts {
        &FACTS
    }
    fn native_stop_reason(
        &self,
        _running: bool,
        _observed: Option<&super::claude::Observation>,
        caps: &ProviderCapabilities,
    ) -> Option<&'static str> {
        caps.stop.reason
    }
    fn add_session_input(&self, _: &Paths, _: &mut Value, _: Option<&Path>) {}
    fn account_plan(&self, _: &Account, _: i64) -> Value {
        Value::Null
    }
    fn account_usage(&self, _: &Paths, _: &Account, _: &Config, _: i64) -> (Value, bool, String) {
        (
            json!({"short":null,"weekly":null,"observed_at":null,"exhausted":false,"state":"unsupported",
                "source":"gemini-cli-unsupported","semantics":REASON}),
            false,
            "unsupported".into(),
        )
    }
}

pub const NO_INPUT_API: &str = "no supported native input API for a live Gemini session";
pub const LABEL_ONLY: &str =
    "label is a Sessionsation catalog label; Gemini CLI has no native session rename";
fn unsupported<T>() -> Result<T> {
    Err(refusal())
}
pub fn auth_report(_: &Path) -> Value {
    Value::Null
}
pub fn prelaunch_gate(_: &Path) -> Result<()> {
    unsupported()
}
pub fn find_transcript(_: &Path, _: &str) -> Option<PathBuf> {
    None
}
pub fn env_in_use(_: &[&Path]) -> bool {
    false
}
pub fn launch_observed(_: &crate::catalog::Session) -> bool {
    false
}
pub fn ensure_resumable(_: &crate::catalog::Session, _: &Observation) -> Result<()> {
    unsupported()
}
pub async fn exec_new(_: &Paths, _: &str, _: Option<&str>, _: Option<&str>) -> Result<()> {
    unsupported()
}
pub async fn exec_resume(_: &Paths, _: &str, _: &str) -> Result<()> {
    unsupported()
}
pub fn reconcile(_: &Paths) -> Result<std::collections::BTreeMap<String, (Observation, PathBuf)>> {
    Ok(Default::default())
}
