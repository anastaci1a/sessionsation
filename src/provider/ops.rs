//! Provider dispatch owner (guard role `provider-ops`, centralization step S1).
//!
//! Every per-provider decision outside `src/provider/` asks this module instead of
//! comparing a provider id or name: [`ops`] (or [`named`] for a stored provider string)
//! returns the provider's [`ProviderOps`], whose [`Facts`] table and methods state what the
//! provider does. Each provider's table lives in `provider/<provider>/ops.rs`; a struct
//! literal there names every field, so adding a fact is a compile error until each
//! provider answers it.
//!
//! [`Op`] names the public operations of [`ProviderCapabilities`] and [`OP_KEYS`] maps
//! them onto the capability keys of `api capabilities` (unchanged). Refusals keep their
//! exact texts: [`UNSUPPORTED`] is the bare code, [`unsupported`] the code with a detail,
//!.
use super::{Capability, ProviderCapabilities, ProviderId, antigravity, claude, codex, gemini};
use crate::{config::Config, model::Account, paths::Paths};
use serde_json::Value;
use std::path::Path;

/// The refusal code for an operation a provider does not implement.
pub const UNSUPPORTED: &str = "UNSUPPORTED_PROVIDER_OPERATION";

/// `UNSUPPORTED_PROVIDER_OPERATION: <detail>`, the exact text callers produced before S1.
pub fn unsupported(detail: impl std::fmt::Display) -> anyhow::Error {
    anyhow::anyhow!("{UNSUPPORTED}: {detail}")
}
/// The bare `UNSUPPORTED_PROVIDER_OPERATION` refusal.
pub fn unsupported_bare() -> anyhow::Error {
    anyhow::anyhow!(UNSUPPORTED)
}
/// `Ok(())` when `supported`, else [`unsupported`] with `detail`.
pub fn require(supported: bool, detail: impl std::fmt::Display) -> anyhow::Result<()> {
    if supported {
        Ok(())
    } else {
        Err(unsupported(detail))
    }
}
/// `Ok(())` when `supported`, else the bare refusal.
pub fn require_bare(supported: bool) -> anyhow::Result<()> {
    if supported {
        Ok(())
    } else {
        Err(unsupported_bare())
    }
}

/// One public provider operation, one per [`ProviderCapabilities`] field.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum Op {
    Enrollment,
    AdoptConfigDir,
    Login,
    AuthStatus,
    Usage,
    New,
    Resume,
    Rename,
    NativeRename,
    Stop,
    Restart,
    AccountSwitch,
    Relocate,
    MoveSpace,
    Archive,
    DeleteSession,
    Submit,
    Queue,
    Steer,
    AccountRemoval,
    AccountDeletion,
    AccountRename,
    AccountLogout,
    HeadlessDelivery,
    AccountLimits,
    CrossProviderConversion,
}

/// Each [`Op`] and its key in `api capabilities` (`providers[].<key>`).
pub const OP_KEYS: [(Op, &str); 26] = [
    (Op::Enrollment, "enrollment"),
    (Op::AdoptConfigDir, "adopt_config_dir"),
    (Op::Login, "login"),
    (Op::AuthStatus, "auth_status"),
    (Op::Usage, "usage"),
    (Op::New, "new"),
    (Op::Resume, "resume"),
    (Op::Rename, "rename"),
    (Op::NativeRename, "native_rename"),
    (Op::Stop, "stop"),
    (Op::Restart, "restart"),
    (Op::AccountSwitch, "account_switch"),
    (Op::Relocate, "relocate"),
    (Op::MoveSpace, "move_space"),
    (Op::Archive, "archive"),
    (Op::DeleteSession, "delete_session"),
    (Op::Submit, "submit"),
    (Op::Queue, "queue"),
    (Op::Steer, "steer"),
    (Op::AccountRemoval, "account_removal"),
    (Op::AccountDeletion, "account_deletion"),
    (Op::AccountRename, "account_rename"),
    (Op::AccountLogout, "account_logout"),
    (Op::HeadlessDelivery, "headless_delivery"),
    (Op::AccountLimits, "account_limits"),
    (Op::CrossProviderConversion, "cross_provider_conversion"),
];

impl Op {
    pub const ALL: [Op; 26] = {
        let mut all = [Op::Enrollment; 26];
        let mut i = 0;
        while i < OP_KEYS.len() {
            all[i] = OP_KEYS[i].0;
            i += 1;
        }
        all
    };
    /// The capability key of this operation.
    pub fn key(self) -> &'static str {
        OP_KEYS
            .iter()
            .find(|(op, _)| *op == self)
            .map(|(_, key)| *key)
            .unwrap_or_default()
    }
}

impl ProviderCapabilities {
    /// The capability of one operation.
    pub fn get(&self, op: Op) -> &Capability {
        match op {
            Op::Enrollment => &self.enrollment,
            Op::AdoptConfigDir => &self.adopt_config_dir,
            Op::Login => &self.login,
            Op::AuthStatus => &self.auth_status,
            Op::Usage => &self.usage,
            Op::New => &self.new,
            Op::Resume => &self.resume,
            Op::Rename => &self.rename,
            Op::NativeRename => &self.native_rename,
            Op::Stop => &self.stop,
            Op::Restart => &self.restart,
            Op::AccountSwitch => &self.account_switch,
            Op::Relocate => &self.relocate,
            Op::MoveSpace => &self.move_space,
            Op::Archive => &self.archive,
            Op::DeleteSession => &self.delete_session,
            Op::Submit => &self.submit,
            Op::Queue => &self.queue,
            Op::Steer => &self.steer,
            Op::AccountRemoval => &self.account_removal,
            Op::AccountDeletion => &self.account_deletion,
            Op::AccountRename => &self.account_rename,
            Op::AccountLogout => &self.account_logout,
            Op::HeadlessDelivery => &self.headless_delivery,
            Op::AccountLimits => &self.account_limits,
            Op::CrossProviderConversion => &self.cross_provider_conversion,
        }
    }
}

/// Who reads a provider's usage meter or redeems its rate-limit reset credits.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Reader {
    /// The account supervisor through the provider's app-server (Codex).
    Supervisor,
    /// In process through the Claude OAuth owner (`provider::claude::oauth`).
    OAuth,
    /// The provider reports nothing.
    None,
}

/// How `api stop` stops a running session of the provider.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum StopMethod {
    /// A queued supervisor operation on the external instance (Codex).
    Supervisor,
    /// SIGTERM to the exact registered Claude process (1.59.0).
    ClaudeProcess,
    /// SIGTERM to the exact verified Antigravity process.
    AntigravityProcess,
    /// Refused through `provider::stop_shape::unsupported` (Gemini, 1.63.0).
    Refused,
}

/// What a provider does, one named fact per decision callers used to make by comparing
/// provider ids. Every provider's table names every field.
#[derive(Debug)]
pub struct Facts {
    /// Hosted (Claude `-p` stream) sessions exist for this provider.
    pub hosted_sessions: bool,
    /// The provider predates providers (Codex): its accounts are keyed, homed (legacy
    /// layout `accounts/<name>`) and named in the launcher argv by their bare name; other
    /// providers use the qualified `<provider>:<name>`.
    pub bare_name: bool,
    /// The account supervisor runs the provider's app-server for the account (Codex): usage
    /// polling, device login, scheduling and session assignment, and sessions are external
    /// instances (process table + rollouts) driven through it, not native transcripts.
    pub supervised: bool,
    /// Who reads the usage meter.
    pub meter: Reader,
    /// Who redeems rate-limit reset credits.
    pub reset_credits: Reader,
    /// The account has a Gemini authentication mode (Google login or API key) and its
    /// recorded auth failure is projected onto the account row.
    pub auth_modes: bool,
    /// `account add --config-dir` adopts an existing provider config directory.
    pub adopts_config_dir: bool,
    /// The account row carries Claude Code's bypass-permissions warning setting.
    pub bypass_warning: bool,
    /// `api capabilities` lists `account_logout: true` for the provider.
    pub lists_account_logout: bool,
    /// Submit, queue and steer go through Claude Code peer input delivery
    /// (`capabilities.claude_input_delivery`).
    pub peer_input_delivery: bool,
    /// A native session can switch accounts (Claude account switch).
    pub native_account_switch: bool,
    /// Pending questions are detected outside the terminal.
    pub detects_questions: bool,
    /// Why pending questions are not detected (the row's `pending_questions_unsupported_reason`).
    pub questions_unsupported: Option<&'static str>,
    /// Why a running session cannot be added to HERDR (`api session add-to-herdr` refusal).
    pub herdr_attach_refusal: Option<&'static str>,
    /// How `api stop` stops a running session.
    pub stop: StopMethod,
    /// How the supervisor enrolls a new account (`account/add`).
    pub enrollment: Enrollment,
    /// How the official sign-out runs (`account/logout`).
    pub logout: Logout,
    /// The sign-out mechanism named in results and capabilities.
    pub logout_method: &'static str,
    /// How live steering of running sessions is configured (`account/live-steering`).
    pub live_steering: LiveSteering,
    /// The account `plan` comes from the provider's `auth status` (Claude).
    pub auth_plan: bool,
    /// The model and effort grammar.
    pub model: ModelGrammar,
    /// Sessionsation's status-line hook records each conversation's model, effort and
    /// context (Claude `provider-hook claude-statusline`).
    pub statusline_record: bool,
    /// `session launch` holds the brief until the model is verified and types it into the
    /// terminal (Claude); otherwise it is the `first_turn`.
    pub held_brief: bool,
    /// Claude shared state (1.39.0: `shared migrate`, linked managed homes).
    pub shared_state: bool,
    /// Raw provider CLI options after `--` are passed to new sessions (Codex).
    pub cli_options: bool,
    /// `account add` prints this when the new account is already signed in.
    pub logged_in: Option<&'static str>,
    /// How `root migrate` re-keys the provider's sessions.
    pub rekey: Rekey,
    /// The headless CLI dialect for `api send`.
    pub headless: Headless,
    /// Environment set on headless runs (Gemini cannot answer the folder-trust prompt).
    pub headless_env: &'static [(&'static str, &'static str)],
    /// The files the native archive moves.
    pub archive: ArchiveLayout,
    /// The transcript a running native session's activity is read from.
    pub transcript: TranscriptFormat,
    /// How `api send` types into a running session's terminal.
    pub terminal_input: TerminalInput,
    /// How `api resume-launch` starts a stopped session (contract 1.66.0).
    pub resume_launch: ResumeLaunch,
    /// Human plan labels for raw plan values (lowercase), only where the meaning is certain.
    pub plan_names: &'static [(&'static str, &'static str)],
}

/// How `api resume-launch` starts a stopped session in a HERDR pane (contract 1.66.0). Every
/// supported provider has the same request, receipt and `already_running` answer.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ResumeLaunch {
    /// The HERDR transfer worker, also to another account (Codex).
    Transfer,
    /// The provider's own resume of the saved conversation on its account (`api resume`).
    Native,
    /// Refused with this reason.
    Refused(&'static str),
}

/// How `account/add` creates the account.
#[derive(Clone, Copy)]
pub enum Enrollment {
    /// The supervisor creates the managed home and polls it (Codex).
    Supervisor,
    /// A managed Claude home, or an adopted `--config-dir`.
    ClaudeHome,
    /// A managed native home created by `create_home` and checked by `auth_report`.
    ManagedHome {
        create_home: fn(&Paths, &str) -> anyhow::Result<(std::path::PathBuf, bool)>,
        auth_report: fn(&Path) -> Value,
    },
}
impl std::fmt::Debug for Enrollment {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(match self {
            Enrollment::Supervisor => "Supervisor",
            Enrollment::ClaudeHome => "ClaudeHome",
            Enrollment::ManagedHome { .. } => "ManagedHome",
        })
    }
}

/// How `account/logout` signs the account out.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Logout {
    /// `account/logout` through the app-server, else `codex logout`.
    CodexRpc,
    /// `claude auth logout`.
    ClaudeCli,
    /// Gemini's cached credential files.
    GeminiFiles,
    /// Refused with this text.
    Refused(&'static str),
}

/// How live steering is configured for an account.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum LiveSteering {
    /// Claude Code peer messaging (`channelsEnabled` inbound setting, sender registry).
    ClaudePeer,
    /// Gemini CLI `experimental.modelSteering`.
    GeminiModelSteering,
    /// Not configurable.
    None,
}

/// One provider's operations. Implemented once per provider in `provider/<provider>/ops.rs`.
pub trait ProviderOps: Sync {
    fn id(&self) -> ProviderId;
    /// The provider's capability table (unchanged since before S1).
    fn capabilities(&self) -> ProviderCapabilities;
    /// One operation's capability.
    fn capability(&self, op: Op) -> Capability {
        self.capabilities().get(op).clone()
    }
    fn facts(&self) -> &'static Facts;
    /// The `stop` capability reason of a native session row (`None` = stoppable now).
    fn native_stop_reason(
        &self,
        running: bool,
        observed: Option<&claude::Observation>,
        caps: &ProviderCapabilities,
    ) -> Option<&'static str>;
    /// Add the provider's terminal-input fields (`input`, submit/queue/steer) to a native
    /// session row.
    fn add_session_input(&self, paths: &Paths, value: &mut Value, home: Option<&Path>);
    /// The account's public `plan` object (contract 1.46.7).
    fn account_plan(&self, a: &Account, at: i64) -> Value;
    /// Usage in the shared account shape plus `source`/`semantics`; returns (usage,
    /// usage_fresh, usage_state).
    fn account_usage(
        &self,
        paths: &Paths,
        a: &Account,
        config: &Config,
        at: i64,
    ) -> (Value, bool, String);
}

/// The operations of `provider`.
pub fn ops(provider: ProviderId) -> &'static dyn ProviderOps {
    match provider {
        ProviderId::Codex => &codex::ops::OPS,
        ProviderId::Claude => &claude::ops::OPS,
        ProviderId::Gemini => &gemini::ops::OPS,
        ProviderId::Antigravity => &antigravity::ops::OPS,
    }
}
/// The operations of a stored provider name (catalog rows, JSON); `None` when unknown.
pub fn named(provider: &str) -> Option<&'static dyn ProviderOps> {
    ProviderId::parse(provider).ok().map(ops)
}
/// The facts of `provider`.
pub fn facts(provider: ProviderId) -> &'static Facts {
    ops(provider).facts()
}
/// The facts of a stored provider name; `None` when unknown.
pub fn facts_of(provider: &str) -> Option<&'static Facts> {
    named(provider).map(|o| o.facts())
}
/// Whether the stored provider name is known and `f` holds for its facts.
pub fn holds(provider: &str, f: impl FnOnce(&'static Facts) -> bool) -> bool {
    facts_of(provider).is_some_and(f)
}

/// Shared by the native providers whose stop reason is the capability table's.
pub(super) fn table_stop_reason(caps: &ProviderCapabilities) -> Option<&'static str> {
    caps.stop.reason
}
/// The detail of the hosted-session refusal.
pub const HOSTED_CLAUDE_ONLY: &str = "hosted sessions are Claude-only";

/// The model and effort grammar a provider accepts (launch, `session launch`, set-model).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ModelGrammar {
    /// Codex model ids and `launch::REASONING_EFFORTS`; set-model through the app-server.
    Codex,
    /// Claude Code model ids and `session_model::CLAUDE_EFFORTS` (terminal sessions).
    Claude,
    /// No model selection through Sessionsation.
    None,
}
/// The detail of the refusal of provider options after `--` (Codex launches only).
pub const CLI_OPTIONS_CODEX_ONLY: &str = "provider options after -- are Codex-only";

/// How `root migrate` re-keys a provider's sessions to the new project paths.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Rekey {
    /// Rollout `cwd` and the thread-location record (Codex).
    CodexRollouts,
    /// The `projects/<cwd key>` transcript and memory directories (Claude).
    ClaudeProjects,
    /// Not re-keyed; the session stays on its old path (Gemini, Antigravity since 1.54).
    Unsupported,
}

/// The headless CLI a native provider runs for `api send` (bounded, non-interactive).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Headless {
    /// `claude -p --output-format json`.
    ClaudePrint,
    /// `gemini -p --output-format json` (stderr kept for auth failures).
    GeminiJson,
    /// `agy` with its own argv and input framing; it cannot pre-assign a conversation id.
    AgyJson,
    /// No headless execution (Codex delivers through its native queue).
    None,
}

/// Which provider files Sessionsation's native archive moves for a conversation.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ArchiveLayout {
    /// Codex rollouts, archived through the supervisor (not a native archive).
    Rollouts,
    /// `projects/*/<id>.jsonl` and the per-conversation side directories (Claude).
    ClaudeProjects,
    /// `.gemini/tmp/*/chats` chat files (Gemini).
    GeminiChats,
    /// agy's exact-id files plus its `conversation_summaries.db` row (Antigravity).
    AgyIndex,
}

/// The transcript a running native session's activity is read from.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum TranscriptFormat {
    /// None: Codex activity comes from the app-server thread status or the rollout tail.
    None,
    /// Claude Code `projects/*/<id>.jsonl` records (plus the session registry).
    ClaudeJsonl,
    /// Gemini CLI chat records.
    GeminiChat,
    /// agy's `brain/<id>/.system_generated/logs/transcript.jsonl`.
    AgyLog,
}

/// How `api send` types into a running native session's terminal.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum TerminalInput {
    /// Claude Code: peer messages or typed terminal input.
    Claude,
    /// Gemini CLI typed input (model steering when enabled).
    Gemini,
    /// agy typed input.
    Agy,
    /// No terminal input (Codex delivers through its native queue).
    None,
}

/// Whether the stored provider name is the supervised provider (Codex); `false` when unknown.
pub fn supervised(provider: &str) -> bool {
    holds(provider, |f| f.supervised)
}
/// The account name of a `<provider>:<name>` id of the supervised provider (`codex:<name>`).
pub fn supervised_name(id: &str) -> Option<&str> {
    let (provider, name) = id.split_once(':')?;
    supervised(provider).then_some(name)
}
