//! Codex provider surface. The behavior itself remains in the supervisor
//! (`daemon`), discovery/launch (`external`) and lifecycle modules; this file
//! names what Codex supports so dispatch stays explicit.
use super::{Capability, ProviderCapabilities, no, yes};
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
        account_logout: Capability {
            supported: true,
            reason: Some(crate::logout::CODEX_METHOD),
        },
        headless_delivery: no(
            "Codex delivery uses the verified native queue (submit/queue/steer/resume)",
        ),
        account_limits: yes(),
        cross_provider_conversion: no("conversations are bound to their provider"),
    }
}

/// Codex's managed app-server daemon settings, relative to CODEX_HOME. Read by
/// `codex app-server daemon start/restart` and re-read by the running
/// `pid-update-loop` before every scheduled check (upstream
/// `app-server-daemon/src/settings.rs`, 0.165.0 and 0.166.0).
pub const DAEMON_SETTINGS: &str = "app-server-daemon/settings.json";

/// Pin the account's managed app-server daemon to its selected release.
///
/// Codex defaults `updater.autoUpdateEnabled` to true: the `pid-update-loop`
/// installs the latest release hourly and the daemon switches to it, which can
/// move the native protocol outside Sessionsation's verified profiles. With the
/// flag false, `daemon start`/`restart` stop the updater and a running loop exits
/// at its next check, so the release changes only by a deliberate
/// `codex app-server daemon update`. Every other key is preserved. Returns
/// whether the file changed. An unreadable or non-object file is left untouched
/// (Codex itself refuses scheduled updates while it cannot parse it).
pub fn pin_daemon_release(home: &std::path::Path) -> Result<bool> {
    use anyhow::{Context, ensure};
    use serde_json::{Map, Value};
    let path = home.join(DAEMON_SETTINGS);
    let mut settings = match std::fs::symlink_metadata(&path) {
        Ok(meta) => {
            ensure!(
                meta.is_file(),
                "unsafe Codex daemon settings {}",
                path.display()
            );
            let bytes = std::fs::read(&path)?;
            match serde_json::from_slice::<Value>(&bytes)
                .with_context(|| format!("unreadable Codex daemon settings {}", path.display()))?
            {
                Value::Object(map) => map,
                _ => anyhow::bail!("Codex daemon settings {} are not an object", path.display()),
            }
        }
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Map::new(),
        Err(e) => return Err(e.into()),
    };
    let updater = settings
        .entry("updater")
        .or_insert_with(|| Value::Object(Map::new()));
    let Some(updater) = updater.as_object_mut() else {
        anyhow::bail!(
            "Codex daemon settings {} have a non-object updater",
            path.display()
        );
    };
    if updater.get("autoUpdateEnabled") == Some(&Value::Bool(false)) {
        return Ok(false);
    }
    updater.insert("autoUpdateEnabled".into(), Value::Bool(false));
    let bytes = serde_json::to_vec_pretty(&Value::Object(settings))?;
    crate::paths::atomic_write(&path, &bytes)?;
    Ok(true)
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::{Value, json};

    fn read(home: &std::path::Path) -> Value {
        serde_json::from_slice(&std::fs::read(home.join(DAEMON_SETTINGS)).unwrap()).unwrap()
    }

    #[test]
    fn pins_missing_settings_and_is_idempotent() {
        let dir = tempfile::tempdir().unwrap();
        assert!(pin_daemon_release(dir.path()).unwrap());
        assert_eq!(
            read(dir.path()),
            json!({"updater":{"autoUpdateEnabled":false}})
        );
        assert!(!pin_daemon_release(dir.path()).unwrap());
        use std::os::unix::fs::PermissionsExt;
        let mode = std::fs::metadata(dir.path().join(DAEMON_SETTINGS))
            .unwrap()
            .permissions()
            .mode();
        assert_eq!(mode & 0o777, 0o600);
    }

    #[test]
    fn pin_preserves_other_settings() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::create_dir_all(dir.path().join("app-server-daemon")).unwrap();
        std::fs::write(
            dir.path().join(DAEMON_SETTINGS),
            br#"{"remoteControlEnabled":true,"featureOverrides":{"x":true},"shutdownGraceSeconds":30,"updater":{"autoUpdateEnabled":true,"updateIntervalMinutes":15}}"#,
        )
        .unwrap();
        assert!(pin_daemon_release(dir.path()).unwrap());
        assert_eq!(
            read(dir.path()),
            json!({"remoteControlEnabled":true,"featureOverrides":{"x":true},"shutdownGraceSeconds":30,"updater":{"autoUpdateEnabled":false,"updateIntervalMinutes":15}})
        );
    }

    #[test]
    fn pin_leaves_unreadable_settings_untouched() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::create_dir_all(dir.path().join("app-server-daemon")).unwrap();
        for bad in [&b"{not json"[..], b"[]", br#"{"updater":3}"#] {
            std::fs::write(dir.path().join(DAEMON_SETTINGS), bad).unwrap();
            assert!(pin_daemon_release(dir.path()).is_err());
            assert_eq!(
                std::fs::read(dir.path().join(DAEMON_SETTINGS)).unwrap(),
                bad
            );
        }
    }
}
