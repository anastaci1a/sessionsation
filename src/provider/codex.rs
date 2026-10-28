//! Codex provider surface. The behavior itself remains in the supervisor
//! (`daemon`), discovery/launch (`external`) and lifecycle modules; this file
//! names what Codex supports so dispatch stays explicit.
use super::{ProviderCapabilities, no, yes};
use crate::paths::Paths;
use anyhow::Result;
use std::path::PathBuf;

/// Usage provenance reported alongside the unchanged Codex usage snapshot.
pub const USAGE_SOURCE: &str = "codex-app-server-rate-limits";
pub const USAGE_SEMANTICS: &str = "provider-reported used percentage of the 5-hour and weekly ChatGPT plan windows, polled by the account supervisor";

/// CODEX_HOME for an enrolled Codex account.
pub fn home(paths: &Paths, name: &str) -> Result<PathBuf> {
    paths.account(name)
}

pub fn capabilities() -> ProviderCapabilities {
    ProviderCapabilities {
        enrollment: yes(),
        adopt_config_dir: no("Codex homes are created and owned by sessionsation"),
        login: yes(),
        auth_status: yes(),
        usage: yes(),
        new: yes(),
        resume: yes(),
        rename: yes(),
        native_rename: yes(),
        stop: yes(),
        restart: yes(),
        account_switch: yes(),
        relocate: yes(),
        move_space: yes(),
        archive: yes(),
        delete_session: yes(),
        submit: yes(),
        queue: yes(),
        steer: yes(),
        account_removal: yes(),
        account_deletion: yes(),
        account_rename: yes(),
        headless_delivery: no(
            "Codex delivery uses the verified native queue (submit/queue/steer/resume)",
        ),
        account_limits: yes(),
        cross_provider_conversion: no("conversations are bound to their provider"),
    }
}
