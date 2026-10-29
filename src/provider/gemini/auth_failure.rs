//! Last observed Gemini authentication failure, per account.
//!
//! Credential files can be present while the provider rejects them (for example
//! Google retiring the individual Gemini CLI login). Sessionsation records a
//! failure only when a Gemini child's output (headless stderr/JSON error, or the
//! login terminal) matches a recognizable authentication-failure pattern; a
//! generic crash never marks an account failed. Only a fixed code, a fixed
//! reason text chosen from the pattern (never raw output), a timestamp and the
//! credential file's stat identity are stored, in a sessionsation-owned file
//! outside the account home. A later successful run or login clears it, and a
//! changed credential file (a newer login by any means) supersedes it.
use crate::paths::{Paths, atomic_write, validate_name};
use anyhow::Result;
use serde::{Deserialize, Serialize};
use serde_json::Value;
use std::{
    fs,
    io::Read,
    path::{Path, PathBuf},
};

/// Bound for text inspected and for the stored reason.
pub const SCAN_BYTES: usize = 64 * 1024;
const MAX_REASON: usize = 300;

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct Failure {
    pub code: String,
    pub reason: String,
    pub observed_at: i64,
    /// (modified seconds, size) of `oauth_creds.json` when the failure was seen.
    #[serde(default)]
    pub credentials: Option<(i64, u64)>,
}

fn file(paths: &Paths, name: &str) -> Result<PathBuf> {
    validate_name(name)?;
    Ok(paths
        .root
        .join("provider-state")
        .join("gemini-auth")
        .join(format!("{name}.json")))
}

fn stamp(home: &Path) -> Option<(i64, u64)> {
    let m = fs::symlink_metadata(home.join(".gemini/oauth_creds.json")).ok()?;
    let secs = m
        .modified()
        .ok()?
        .duration_since(std::time::UNIX_EPOCH)
        .ok()?
        .as_secs() as i64;
    Some((secs, m.len()))
}

/// Recognize an authentication failure in provider output. Returns a stable code
/// and a fixed reason; unrecognized text (a crash, a network error) is `None`.
pub fn classify(text: &str) -> Option<(&'static str, &'static str)> {
    let end = text.len().min(SCAN_BYTES);
    let mut end_boundary = end;
    while !text.is_char_boundary(end_boundary) {
        end_boundary -= 1;
    }
    let t = text[..end_boundary].to_ascii_lowercase();
    let has = |p: &str| t.contains(p);
    if has("client no longer supported") || (has("no longer supported") && has("client")) {
        Some((
            "AUTH_CLIENT_UNSUPPORTED",
            "AUTH_CLIENT_UNSUPPORTED: Google rejected the Gemini CLI sign-in (client no longer supported); this login can no longer be used",
        ))
    } else if has("invalid_grant") {
        Some((
            "AUTH_GRANT_INVALID",
            "AUTH_GRANT_INVALID: the stored Gemini sign-in was rejected (invalid_grant); log in again",
        ))
    } else if has("unauthenticated")
        || has("401 unauthorized")
        || has("status 401")
        || has("status: 401")
        || has("\"code\": 401")
        || has("\"code\":401")
    {
        Some((
            "AUTH_UNAUTHENTICATED",
            "AUTH_UNAUTHENTICATED: Gemini reported the request as unauthenticated (401); log in again",
        ))
    } else if has("re-authenticate") || has("reauthenticate") {
        Some((
            "AUTH_REAUTH_REQUIRED",
            "AUTH_REAUTH_REQUIRED: Gemini asked to re-authenticate; log in again",
        ))
    } else {
        None
    }
}

/// Record a failure when `text` is a recognizable authentication failure.
pub fn record(paths: &Paths, name: &str, home: &Path, text: &str) -> Result<Option<Failure>> {
    let Some((code, reason)) = classify(text) else {
        return Ok(None);
    };
    let failure = Failure {
        code: code.into(),
        reason: reason.chars().take(MAX_REASON).collect(),
        observed_at: crate::daemon::now(),
        credentials: stamp(home),
    };
    atomic_write(&file(paths, name)?, &serde_json::to_vec(&failure)?)?;
    Ok(Some(failure))
}

/// Forget a recorded failure (a later successful run or login).
pub fn clear(paths: &Paths, name: &str) {
    if let Ok(path) = file(paths, name) {
        let _ = fs::remove_file(path);
    }
}

/// The active failure, if any. A credential file that changed since the failure
/// supersedes it.
pub fn current(paths: &Paths, name: &str, home: &Path) -> Option<Failure> {
    let path = file(paths, name).ok()?;
    let mut bytes = vec![];
    fs::File::open(path)
        .ok()?
        .take(8 * 1024)
        .read_to_end(&mut bytes)
        .ok()?;
    let failure: Failure = serde_json::from_slice(&bytes).ok()?;
    (failure.credentials == stamp(home)).then_some(failure)
}

/// Account-row fields for `current`.
pub fn fields(failure: Option<&Failure>) -> Value {
    serde_json::json!({
        "auth_failed": failure.is_some(),
        "auth_failure_reason": failure.map(|f| f.reason.clone()),
        "auth_failure_observed_at": failure.map(|f| f.observed_at),
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    fn setup() -> (tempfile::TempDir, Paths, PathBuf) {
        let t = tempfile::tempdir().unwrap();
        let paths = Paths::new(Some(t.path().join("root"))).unwrap();
        let home = paths.root.join("gemini-accounts/g1");
        fs::create_dir_all(home.join(".gemini")).unwrap();
        fs::write(home.join(".gemini/oauth_creds.json"), b"{}").unwrap();
        (t, paths, home)
    }
    #[test]
    fn only_recognizable_auth_failures_classify() {
        for (text, code) in [
            (
                "Error: This client no longer supported. See issue",
                "AUTH_CLIENT_UNSUPPORTED",
            ),
            ("{\"error\":\"invalid_grant\"}", "AUTH_GRANT_INVALID"),
            (
                "GaxiosError: status: UNAUTHENTICATED",
                "AUTH_UNAUTHENTICATED",
            ),
            ("request failed with status 401", "AUTH_UNAUTHENTICATED"),
            ("Please re-authenticate with /auth", "AUTH_REAUTH_REQUIRED"),
        ] {
            assert_eq!(classify(text).unwrap().0, code, "{text}");
        }
        for text in [
            "",
            "segmentation fault",
            "ECONNRESET",
            "exit 1",
            "model overloaded 503",
        ] {
            assert!(classify(text).is_none(), "{text}");
        }
    }
    #[test]
    fn records_clears_and_is_superseded_by_new_credentials() {
        let (_t, paths, home) = setup();
        assert!(record(&paths, "g1", &home, "ECONNRESET").unwrap().is_none());
        assert!(current(&paths, "g1", &home).is_none());
        let secret = "ya29.SECRET-TOKEN-VALUE";
        let f = record(&paths, "g1", &home, &format!("invalid_grant {secret}"))
            .unwrap()
            .unwrap();
        assert_eq!(f.code, "AUTH_GRANT_INVALID");
        let stored = fs::read_to_string(file(&paths, "g1").unwrap()).unwrap();
        assert!(!stored.contains(secret), "raw output is never stored");
        assert!(stored.len() < 1024);
        assert_eq!(current(&paths, "g1", &home), Some(f));
        // A newer credential file supersedes the failure.
        fs::write(home.join(".gemini/oauth_creds.json"), b"{\"a\":1}").unwrap();
        assert!(current(&paths, "g1", &home).is_none());
        record(&paths, "g1", &home, "invalid_grant").unwrap();
        assert!(current(&paths, "g1", &home).is_some());
        clear(&paths, "g1");
        assert!(current(&paths, "g1", &home).is_none());
        assert_eq!(fields(None)["auth_failed"], false);
    }
}
