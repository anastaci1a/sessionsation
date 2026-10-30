use anyhow::{Result, ensure};
use serde::{Deserialize, Serialize};

/// Built-in default for `claude_default_statusline_command`.
pub const DEFAULT_STATUSLINE_COMMAND: &str = "bash /mnt/user/appdata/claude/statusline-command.sh";

#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct Config {
    pub short_ceiling: f64,
    pub weekly_ceiling: f64,
    pub drain_margin: f64,
    pub balance_increment: f64,
    pub balance_hysteresis: f64,
    pub min_assignment_seconds: u64,
    pub active_poll_seconds: u64,
    pub near_limit_poll_seconds: u64,
    pub idle_poll_seconds: u64,
    pub stale_seconds: u64,
    pub codex_binary: String,
    /// Claude Code executable used for every Claude account.
    pub claude_binary: String,
    /// Optional HOME for Claude children; unset inherits the caller's HOME.
    pub claude_session_home: Option<std::path::PathBuf>,
    /// Gemini CLI executable used for every Gemini account.
    pub gemini_binary: String,
    /// Antigravity CLI (`agy`) executable used for every Antigravity account.
    pub antigravity_binary: String,
    /// Operator kill switch for typed terminal input into HERDR panes (Claude
    /// and Gemini). Off: rows report `terminal_input` unavailable with
    /// `TERMINAL_INPUT_DISABLED` and no request types into a pane.
    pub terminal_input_enabled: bool,
    /// Days a certain-terminal message receipt is kept before it is retired
    /// to a tombstone (contract revision 18).
    pub receipt_retention_days: u32,
    /// Days after which an unfinished or uncertain receipt is recorded as
    /// `expired` (`outcome_unknown_at_retention_horizon`) and retired.
    pub receipt_unknown_horizon_days: u32,
    /// Permission mode for every new provider process Sessionsation starts
    /// (launch, resume, restart, account switch, hosted and headless runs):
    /// `yolo` (default) or `default` (the provider's own prompts). Contract revision 19.
    pub launch_permissions: LaunchPermissions,
    /// Before a yolo Claude start (interactive or hosted), pre-accept Claude Code's
    /// folder trust for the launch directory in the account's `.claude.json`
    /// (`projects[dir].hasTrustDialogAccepted`). Contract revision 20.
    pub claude_pretrust_folders: bool,
    /// Before a yolo Antigravity start, pre-accept agy's folder trust for the launch
    /// directory (`trustedWorkspaces` in the account's `settings.json`). Contract revision 25.
    pub antigravity_pretrust_folders: bool,
    /// Command seeded as `statusLine` in the settings.json of a NEW managed Claude
    /// account home (never adopted or existing). Empty disables seeding. Contract 1.27.0.
    pub claude_default_statusline_command: String,
    /// Days a rotated log (`logs/<name>.log.1`) is kept. See `retention`.
    pub log_retention_days: u32,
    /// A log larger than this is rotated to `<name>.log.1` and truncated.
    pub log_max_bytes: u64,
    /// Days an untouched cache entry (`cache/observations/`) is kept.
    pub cache_retention_days: u32,
    /// Days a finished operation journal is kept (unfinished or uncertain ones
    /// and tombstones are never removed).
    pub operation_retention_days: u32,
}
impl Default for Config {
    fn default() -> Self {
        Self {
            short_ceiling: 90.,
            weekly_ceiling: 90.,
            drain_margin: 5.,
            balance_increment: 5.,
            balance_hysteresis: 0.05,
            min_assignment_seconds: 300,
            active_poll_seconds: 30,
            near_limit_poll_seconds: 10,
            idle_poll_seconds: 120,
            stale_seconds: 300,
            codex_binary: "codex".into(),
            claude_binary: "claude".into(),
            claude_session_home: None,
            gemini_binary: "gemini".into(),
            antigravity_binary: "agy".into(),
            terminal_input_enabled: true,
            receipt_retention_days: 14,
            receipt_unknown_horizon_days: 90,
            launch_permissions: LaunchPermissions::Yolo,
            claude_pretrust_folders: true,
            antigravity_pretrust_folders: true,
            claude_default_statusline_command: DEFAULT_STATUSLINE_COMMAND.into(),
            log_retention_days: 30,
            log_max_bytes: 8 * 1024 * 1024,
            cache_retention_days: 14,
            operation_retention_days: 90,
        }
    }
}
impl Config {
    pub fn validate(&self) -> Result<()> {
        for (name, x) in [
            ("short_ceiling", self.short_ceiling),
            ("weekly_ceiling", self.weekly_ceiling),
        ] {
            ensure!(
                x.is_finite() && x > 0. && x <= 100.,
                "{name} must be in (0, 100]"
            );
        }
        ensure!(
            self.drain_margin.is_finite()
                && self.drain_margin > 0.
                && self.drain_margin < self.short_ceiling.min(self.weekly_ceiling),
            "invalid drain_margin"
        );
        ensure!(
            self.balance_increment.is_finite()
                && self.balance_increment > 0.
                && self.balance_increment <= 100.,
            "invalid balance_increment"
        );
        ensure!(
            self.balance_hysteresis.is_finite()
                && self.balance_hysteresis > 0.
                && self.balance_hysteresis <= 1.,
            "invalid balance_hysteresis"
        );
        ensure!(
            self.near_limit_poll_seconds > 0
                && self.active_poll_seconds >= self.near_limit_poll_seconds
                && self.stale_seconds >= self.active_poll_seconds
                && self.stale_seconds >= self.idle_poll_seconds.saturating_add(90)
                && self.idle_poll_seconds > 0,
            "invalid poll intervals"
        );
        ensure!(
            !self.codex_binary.trim().is_empty(),
            "codex_binary is empty"
        );
        ensure!(
            !self.claude_binary.trim().is_empty(),
            "claude_binary is empty"
        );
        ensure!(
            !self.gemini_binary.trim().is_empty(),
            "gemini_binary is empty"
        );
        ensure!(
            !self.antigravity_binary.trim().is_empty(),
            "antigravity_binary is empty"
        );
        ensure!(
            (1..=3650).contains(&self.receipt_retention_days)
                && (self.receipt_retention_days..=3650)
                    .contains(&self.receipt_unknown_horizon_days),
            "receipt_retention_days must be 1..=3650 and receipt_unknown_horizon_days at least that and at most 3650"
        );
        for (name, days) in [
            ("log_retention_days", self.log_retention_days),
            ("cache_retention_days", self.cache_retention_days),
            ("operation_retention_days", self.operation_retention_days),
        ] {
            ensure!((1..=3650).contains(&days), "{name} must be 1..=3650");
        }
        ensure!(
            self.log_max_bytes >= 65536,
            "log_max_bytes must be at least 65536"
        );
        ensure!(
            self.claude_session_home
                .as_ref()
                .is_none_or(|h| h.is_absolute()),
            "claude_session_home must be absolute"
        );
        Ok(())
    }
}

/// Permission mode for a newly started provider process.
/// - `yolo`: Codex approval `never` + sandbox `danger-full-access`; Claude
///   `--dangerously-skip-permissions`; Gemini `--approval-mode yolo`.
/// - `default`: no override; the provider's own configuration and prompts
///   (Codex resumes keep the conversation's recorded permissions).
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize, Deserialize, clap::ValueEnum)]
#[serde(rename_all = "snake_case")]
pub enum LaunchPermissions {
    #[default]
    Yolo,
    Default,
}
impl LaunchPermissions {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Yolo => "yolo",
            Self::Default => "default",
        }
    }
    pub fn is_yolo(self) -> bool {
        self == Self::Yolo
    }
}

static LAUNCH_OVERRIDE: std::sync::OnceLock<LaunchPermissions> = std::sync::OnceLock::new();

/// Record this process's explicit per-launch choice (`--permissions`, or the
/// `permissions` field of an admitted launch/resume request). First call wins.
pub fn override_launch_permissions(mode: LaunchPermissions) {
    let _ = LAUNCH_OVERRIDE.set(mode);
}
/// The explicit per-launch choice of this process, if any.
pub fn launch_permissions_override() -> Option<LaunchPermissions> {
    LAUNCH_OVERRIDE.get().copied()
}
impl Config {
    /// The mode for a provider process started now: explicit override, then config.
    pub fn effective_launch_permissions(&self) -> LaunchPermissions {
        launch_permissions_override().unwrap_or(self.launch_permissions)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn launch_permissions_default_to_yolo_and_parse_both_modes() {
        assert_eq!(
            Config::default().launch_permissions,
            LaunchPermissions::Yolo
        );
        let legacy: Config = serde_json::from_str("{}").unwrap();
        assert_eq!(legacy.launch_permissions, LaunchPermissions::Yolo);
        let chosen: Config =
            serde_json::from_value(serde_json::json!({"launch_permissions":"default"})).unwrap();
        assert_eq!(chosen.launch_permissions, LaunchPermissions::Default);
        assert!(
            serde_json::from_value::<Config>(serde_json::json!({"launch_permissions":"ask"}))
                .is_err()
        );
        assert_eq!(
            serde_json::to_value(Config::default()).unwrap()["launch_permissions"],
            "yolo"
        );
    }
}
