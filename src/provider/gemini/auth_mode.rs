//! Per-account Gemini authentication mode and the API-key secret.
//!
//! The mode is the account's own `security.auth.selectedType` in
//! `<home>/.gemini/settings.json` (Gemini CLI 0.66.0 prefers it over every
//! environment variable): `gemini-api-key` is `api_key`, anything else
//! (`oauth-personal`, the seeded default) is `google`. An API key lives only in
//! `<home>/secrets/gemini-api-key` (0600 file in a 0700 directory, written
//! atomically, never through a symlink). It reaches that account's Gemini
//! children only through the environment (`GEMINI_API_KEY`, plus
//! [`WRAPPER_ENV`] for the host `gemini` wrapper, which strips ambient keys) and
//! never appears in argv, state, receipts, logs or any output.
use crate::paths::{atomic_write, private_dir};
use anyhow::{Context, Result, bail, ensure};
use std::{
    fmt, fs,
    io::Read,
    os::unix::fs::{MetadataExt, OpenOptionsExt},
    path::{Path, PathBuf},
};

/// `security.auth.selectedType` for Login with Google (personal or Workspace).
pub const GOOGLE_TYPE: &str = "oauth-personal";
/// `security.auth.selectedType` for a Gemini API key (AuthType.USE_GEMINI).
pub const API_KEY_TYPE: &str = "gemini-api-key";
/// The variable the host `gemini` wrapper turns back into `GEMINI_API_KEY` after it
/// strips ambient keys; set only by sessionsation for an `api_key` account.
pub const WRAPPER_ENV: &str = "SESSIONSATION_GEMINI_API_KEY";
const MAX_KEY: usize = 512;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Mode {
    Google,
    ApiKey,
}
impl Mode {
    pub fn as_str(self) -> &'static str {
        match self {
            Mode::Google => "google",
            Mode::ApiKey => "api_key",
        }
    }
    pub fn selected_type(self) -> &'static str {
        match self {
            Mode::Google => GOOGLE_TYPE,
            Mode::ApiKey => API_KEY_TYPE,
        }
    }
    /// CLI spelling (`google`, `api-key`; `api_key` accepted).
    pub fn parse(text: &str) -> Result<Self> {
        match text {
            "google" => Ok(Mode::Google),
            "api-key" | "api_key" => Ok(Mode::ApiKey),
            "vertex" => bail!(
                "UNSUPPORTED_AUTH_MODE: Vertex AI authentication is not supported yet; use google or api-key"
            ),
            _ => bail!("INVALID_ARGUMENT: --mode must be google or api-key"),
        }
    }
}

/// An API key held in memory. Debug and Display never show it.
#[derive(Clone, PartialEq, Eq)]
pub struct ApiKey(String);
impl ApiKey {
    /// Accept one key: surrounding whitespace trimmed, then 1..=512 visible ASCII
    /// characters. The error never contains the input.
    pub fn new(text: &str) -> Result<Self> {
        let key = text.trim();
        ensure!(
            !key.is_empty() && key.len() <= MAX_KEY && key.bytes().all(|b| b.is_ascii_graphic()),
            "INVALID_API_KEY_FORMAT: the API key must be 1-{MAX_KEY} visible ASCII characters without spaces"
        );
        Ok(Self(key.to_owned()))
    }
    pub fn expose(&self) -> &str {
        &self.0
    }
}
impl fmt::Debug for ApiKey {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str("ApiKey(<redacted>)")
    }
}
impl fmt::Display for ApiKey {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str("<redacted>")
    }
}

pub fn secrets_dir(home: &Path) -> PathBuf {
    home.join("secrets")
}
pub fn key_path(home: &Path) -> PathBuf {
    secrets_dir(home).join("gemini-api-key")
}

/// The account's mode from its settings (`google` when unset or unreadable).
pub fn mode(home: &Path) -> Mode {
    let selected = (|| {
        let mut bytes = vec![];
        fs::File::open(home.join(".gemini/settings.json"))
            .ok()?
            .take(1024 * 1024)
            .read_to_end(&mut bytes)
            .ok()?;
        let v: serde_json::Value = serde_json::from_slice(&bytes).ok()?;
        v["security"]["auth"]["selectedType"]
            .as_str()
            .map(str::to_owned)
    })();
    if selected.as_deref() == Some(API_KEY_TYPE) {
        Mode::ApiKey
    } else {
        Mode::Google
    }
}

/// The secret file's metadata when it is a safe, non-empty key file: a regular
/// file (no symlink) owned by this user, not readable by others.
fn safe_key_metadata(home: &Path) -> Option<fs::Metadata> {
    let m = fs::symlink_metadata(key_path(home)).ok()?;
    (m.file_type().is_file()
        && m.len() > 0
        && m.uid() == unsafe { libc::geteuid() }
        && m.mode() & 0o077 == 0)
        .then_some(m)
}
/// True when a usable key file is stored (stat only).
pub fn key_present(home: &Path) -> bool {
    safe_key_metadata(home).is_some()
}
/// (modified seconds, size) of the key file: a new key supersedes a recorded failure.
pub fn key_stamp(home: &Path) -> Option<(i64, u64)> {
    let m = safe_key_metadata(home)?;
    Some((m.mtime(), m.len()))
}

/// Read the stored key (no symlink, owner-only file), or None.
pub fn read_key(home: &Path) -> Option<ApiKey> {
    safe_key_metadata(home)?;
    let mut text = String::new();
    fs::OpenOptions::new()
        .read(true)
        .custom_flags(libc::O_NOFOLLOW)
        .open(key_path(home))
        .ok()?
        .take(MAX_KEY as u64 + 2)
        .read_to_string(&mut text)
        .ok()?;
    ApiKey::new(&text).ok()
}

/// Store the key atomically as `<home>/secrets/gemini-api-key` (0600 in 0700).
/// A symlinked secrets directory or key file is refused and left unchanged.
pub fn store_key(home: &Path, key: &ApiKey) -> Result<()> {
    let dir = secrets_dir(home);
    if let Ok(m) = fs::symlink_metadata(&dir) {
        ensure!(
            m.file_type().is_dir(),
            "UNSAFE_SECRET_PATH: {} must be a directory, not a symlink; it was left unchanged",
            dir.display()
        );
    }
    private_dir(&dir).context("UNSAFE_SECRET_PATH: the secrets directory is not private")?;
    let path = key_path(home);
    if let Ok(m) = fs::symlink_metadata(&path) {
        ensure!(
            m.file_type().is_file(),
            "UNSAFE_SECRET_PATH: {} must be a regular file, not a symlink; it was left unchanged",
            path.display()
        );
    }
    atomic_write(&path, key.expose().as_bytes())
        .context("SECRET_WRITE_FAILED: could not store the API key")
}

/// Remove the stored key (logout of an `api_key` account). Missing is fine.
pub fn remove_key(home: &Path) -> Result<bool> {
    let path = key_path(home);
    match fs::symlink_metadata(&path) {
        Ok(_) => {
            fs::remove_file(&path).context("LOGOUT_FAILED: could not remove the API key")?;
            Ok(true)
        }
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(false),
        Err(e) => Err(e.into()),
    }
}

/// Select the mode in the account's settings (`security.auth.selectedType`).
/// Returns whether the file changed. Other keys are preserved.
pub fn set_mode(home: &Path, mode: Mode) -> Result<bool> {
    super::seed_settings(home)?;
    super::update_settings(
        home,
        &[(
            &["security", "auth", "selectedType"][..],
            serde_json::json!(mode.selected_type()),
            true,
        )],
    )
}

/// Environment additions for a Gemini child of this account: the stored key under
/// `GEMINI_API_KEY` and [`WRAPPER_ENV`], only when the account uses `api_key`.
pub fn child_env(home: &Path) -> Vec<(&'static str, ApiKey)> {
    if mode(home) != Mode::ApiKey {
        return vec![];
    }
    match read_key(home) {
        Some(key) => vec![("GEMINI_API_KEY", key.clone()), (WRAPPER_ENV, key)],
        None => vec![],
    }
}

#[cfg(test)]
mod tests;
