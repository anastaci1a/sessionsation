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
        .join(crate::layout::PROVIDER_STATE)
        .join("gemini-auth")
        .join(format!("{name}.json")))
}

/// Identity of the account's current credential: the API key file in `api_key`
/// mode, otherwise `oauth_creds.json`. A new key or login supersedes a failure.
fn stamp(home: &Path) -> Option<(i64, u64)> {
    use super::auth_mode;
    if auth_mode::mode(home) == auth_mode::Mode::ApiKey {
        return auth_mode::key_stamp(home);
    }
    let m = fs::symlink_metadata(home.join(".gemini/oauth_creds.json")).ok()?;
    let secs = m
        .modified()
        .ok()?
        .duration_since(std::time::UNIX_EPOCH)
        .ok()?
        .as_secs() as i64;
    Some((secs, m.len()))
}

/// Gemini CLI's exit status for a fatal authentication error
/// (`FatalAuthenticationError`, raised while establishing the sign-in).
pub const EXIT_AUTHENTICATION: i32 = 41;

/// One recognized authentication or authorization failure.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct Classified {
    pub code: &'static str,
    pub reason: &'static str,
    /// The failure happened while the CLI was establishing the sign-in (user setup,
    /// tier check), which precedes any model request.
    pub at_sign_in: bool,
}

fn lowercase_prefix(text: &str) -> String {
    let mut end = text.len().min(SCAN_BYTES);
    while !text.is_char_boundary(end) {
        end -= 1;
    }
    text[..end].to_ascii_lowercase()
}

/// Recognize an authentication failure in provider output. Returns a stable code
/// and a fixed reason; unrecognized text (a crash, a network error) is `None`.
pub fn classify(text: &str) -> Option<(&'static str, &'static str)> {
    classify_output(text, None).map(|c| (c.code, c.reason))
}

/// Classify a finished Gemini run from its (stderr or JSON error) text and exit
/// status. The text patterns cover the observed messages; exit status 41 (the CLI's
/// fatal authentication exit) counts even when the text is unrecognized.
pub fn classify_output(text: &str, exit: Option<i32>) -> Option<Classified> {
    let t = lowercase_prefix(text);
    let has = |p: &str| t.contains(p);
    // Gemini CLI prints "Error authenticating: ..." when the sign-in itself (user
    // setup) fails, before any prompt is sent.
    let at_sign_in = has("error authenticating")
        || has("ineligibletiererror")
        || has("fatalauthenticationerror")
        || exit == Some(EXIT_AUTHENTICATION);
    let found = |code, reason, setup: bool| {
        Some(Classified {
            code,
            reason,
            at_sign_in: at_sign_in || setup,
        })
    };
    if has("api_key_invalid")
        || has("api key not valid")
        || has("api key expired")
        || has("api_key_expired")
    {
        // Gemini API (AI Studio key): `400 INVALID_ARGUMENT`, reason API_KEY_INVALID.
        found(
            "AUTH_API_KEY_INVALID",
            "AUTH_API_KEY_INVALID: the Gemini API rejected this API key (not valid or expired); store a new key",
            false,
        )
    } else if has("must specify the gemini_api_key") {
        // AuthType gemini-api-key selected without a key in the environment.
        found(
            "AUTH_API_KEY_MISSING",
            "AUTH_API_KEY_MISSING: Gemini started in API-key mode without a key (none stored, or the host gemini wrapper removed it); store a key or update the wrapper",
            true,
        )
    } else if has("unsupported_client")
        || has("client is no longer supported")
        || has("client no longer supported")
        || (has("no longer supported") && has("client"))
        || has("migrate to the antigravity")
    {
        // Google retired the individual "Login with Google" for Gemini Code Assist
        // (gemini-cli#28229): the tier check rejects the client during setup.
        found(
            "AUTH_CLIENT_UNSUPPORTED",
            "AUTH_CLIENT_UNSUPPORTED: Google rejected the Gemini CLI sign-in (client no longer supported for Gemini Code Assist for individuals; migrate to Antigravity or use another sign-in); this login can no longer be used",
            true,
        )
    } else if has("ineligibletiererror") || has("ineligible tier") {
        found(
            "AUTH_TIER_INELIGIBLE",
            "AUTH_TIER_INELIGIBLE: Google reported this Gemini sign-in as ineligible for Gemini Code Assist; use another sign-in",
            true,
        )
    } else if has("invalid_grant") {
        found(
            "AUTH_GRANT_INVALID",
            "AUTH_GRANT_INVALID: the stored Gemini sign-in was rejected (invalid_grant); log in again",
            false,
        )
    } else if has("unauthenticated")
        || has("401 unauthorized")
        || has("status 401")
        || has("status: 401")
        || has("\"code\": 401")
        || has("\"code\":401")
    {
        found(
            "AUTH_UNAUTHENTICATED",
            "AUTH_UNAUTHENTICATED: Gemini reported the request as unauthenticated (401); log in again",
            false,
        )
    } else if has("permission_denied")
        || has("403 forbidden")
        || has("status 403")
        || has("status: 403")
        || has("\"code\": 403")
        || has("\"code\":403")
    {
        found(
            "AUTH_PERMISSION_DENIED",
            "AUTH_PERMISSION_DENIED: Gemini refused this sign-in access (403 permission denied); use another sign-in",
            false,
        )
    } else if has("re-authenticate") || has("reauthenticate") {
        found(
            "AUTH_REAUTH_REQUIRED",
            "AUTH_REAUTH_REQUIRED: Gemini asked to re-authenticate; log in again",
            false,
        )
    } else if at_sign_in
        && ![
            "econnreset",
            "econnrefused",
            "enotfound",
            "etimedout",
            "eai_again",
            "socket hang up",
            "fetch failed",
            "network",
        ]
        .iter()
        .any(|p| has(p))
    {
        // A sign-in that failed for an unrecognized reason, but not a network
        // failure (that says nothing about the credentials).
        found(
            "AUTH_FAILED",
            "AUTH_FAILED: the Gemini CLI could not establish its sign-in; log in again",
            true,
        )
    } else {
        None
    }
}

/// Record a failure when `text` is a recognizable authentication failure.
pub fn record(paths: &Paths, name: &str, home: &Path, text: &str) -> Result<Option<Failure>> {
    record_output(paths, name, home, text, None).map(|r| r.map(|(f, _)| f))
}

/// Record a classified failure of a finished run (`classify_output`).
pub fn record_output(
    paths: &Paths,
    name: &str,
    home: &Path,
    text: &str,
    exit: Option<i32>,
) -> Result<Option<(Failure, Classified)>> {
    let Some(classified) = classify_output(text, exit) else {
        return Ok(None);
    };
    let failure = Failure {
        code: classified.code.into(),
        reason: classified.reason.chars().take(MAX_REASON).collect(),
        observed_at: crate::daemon::now(),
        credentials: stamp(home),
    };
    atomic_write(&file(paths, name)?, &serde_json::to_vec(&failure)?)?;
    Ok(Some((failure, classified)))
}

/// Forget a recorded failure (a later successful run or login).
/// Move the recorded failure with its account (backend rename); best effort.
pub fn rename(paths: &Paths, old: &str, new: &str) {
    if let (Ok(from), Ok(to)) = (file(paths, old), file(paths, new))
        && from.is_file()
        && !to.exists()
    {
        let _ = fs::rename(from, to);
    }
}

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
pub(crate) mod tests;
