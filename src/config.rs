use anyhow::{Result, ensure};
use serde::{Deserialize, Serialize};

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
            terminal_input_enabled: true,
            receipt_retention_days: 14,
            receipt_unknown_horizon_days: 90,
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
            (1..=3650).contains(&self.receipt_retention_days)
                && (self.receipt_retention_days..=3650)
                    .contains(&self.receipt_unknown_horizon_days),
            "receipt_retention_days must be 1..=3650 and receipt_unknown_horizon_days at least that and at most 3650"
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
