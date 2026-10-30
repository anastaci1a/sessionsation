//! Folder pre-trust for yolo Antigravity starts (contract revision 25).
//!
//! `agy` 1.4.10 asks "Do you trust the contents of this project?" the first time
//! an interactive session opens a directory. Accepting it appends the exact
//! directory to `trustedWorkspaces` in the account's
//! `<home>/.gemini/antigravity-cli/settings.json` (observed from a real
//! acceptance; the match is the exact path, a subdirectory of a trusted
//! directory asks again). Before a yolo start Sessionsation appends that one
//! array element, so a new session never stops at the dialog.
//!
//! Only that key changes: every other member of the file is kept as raw JSON
//! text in order, the file is replaced atomically with its mode and owner kept,
//! a symlink is refused, and an unparsable file is left alone. The OAuth token
//! file is never read.
use crate::provider::claude::trust::{Members, raw};
use anyhow::{Context, Result, bail, ensure};
use serde_json::value::RawValue;
use std::{
    fs,
    io::Write,
    os::unix::fs::{MetadataExt, PermissionsExt},
    path::{Path, PathBuf},
};

pub const KEY: &str = "trustedWorkspaces";

/// What a pre-trust attempt did.
#[derive(Debug, PartialEq, Eq)]
pub enum Outcome {
    /// The directory was appended to `trustedWorkspaces`.
    Trusted,
    /// Every key was already trusted; nothing was written.
    AlreadyTrusted,
}

fn settings_file(home: &Path) -> PathBuf {
    super::state_dir(home).join("settings.json")
}

/// Raw array elements as text, or an error when the value is not an array.
fn elements(value: &RawValue) -> Result<Vec<Box<RawValue>>> {
    if value.get().trim() == "null" {
        return Ok(vec![]);
    }
    serde_json::from_str(value.get())
        .context("AGY_SETTINGS_UNREADABLE: settings.json trustedWorkspaces is not an array")
}

/// The edited settings text, or `None` when every key is already trusted.
fn edit(text: &str, keys: &[String]) -> Result<Option<String>> {
    let mut top = if text.trim().is_empty() {
        Members(vec![])
    } else {
        Members::parse(text)
            .context("AGY_SETTINGS_UNREADABLE: settings.json is not a JSON object")?
    };
    let mut list = match top.get(KEY) {
        Some(v) => elements(v)?,
        None => vec![],
    };
    let mut changed = false;
    for key in keys {
        let quoted = serde_json::to_string(key).expect("string");
        let present = list.iter().any(|e| {
            serde_json::from_str::<String>(e.get())
                .map(|s| s == *key)
                .unwrap_or(false)
        });
        if !present {
            list.push(raw(quoted));
            changed = true;
        }
    }
    if !changed {
        return Ok(None);
    }
    let body = list
        .iter()
        .map(|e| format!("    {}", e.get()))
        .collect::<Vec<_>>()
        .join(",\n");
    top.set(KEY, raw(format!("[\n{body}\n  ]")));
    let trailing = &text[text.trim_end().len()..];
    Ok(Some(format!("{}{trailing}", top.render(0))))
}

/// Directory spellings agy may compare: as the process sees it (canonical)
/// and, when different, as given.
fn keys(directory: &Path) -> Result<Vec<String>> {
    ensure!(directory.is_absolute(), "directory must be absolute");
    let canonical = fs::canonicalize(directory)
        .with_context(|| format!("directory {} is unavailable", directory.display()))?;
    let mut out = Vec::new();
    for path in [canonical.as_path(), directory] {
        let key = path.to_str().context("directory is not UTF-8")?.to_owned();
        let key = if key.len() > 1 {
            key.trim_end_matches('/').to_owned()
        } else {
            key
        };
        if !out.contains(&key) {
            out.push(key);
        }
    }
    Ok(out)
}

/// Pre-trust `directory` in the Antigravity account at `home`.
pub fn pretrust(home: &Path, directory: &Path) -> Result<Outcome> {
    let keys = keys(directory)?;
    let state = super::state_dir(home);
    let file = settings_file(home);
    for _ in 0..5 {
        let meta = match fs::symlink_metadata(&file) {
            Ok(m) => Some(m),
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => None,
            Err(e) => return Err(e).context("AGY_SETTINGS_UNREADABLE"),
        };
        if let Some(m) = &meta {
            ensure!(
                !m.file_type().is_symlink(),
                "AGY_SETTINGS_SYMLINK: refusing to replace a symlinked settings.json"
            );
            ensure!(
                m.file_type().is_file(),
                "AGY_SETTINGS_UNREADABLE: settings.json is not a regular file"
            );
        } else {
            ensure!(
                state.is_dir() && !fs::symlink_metadata(&state)?.file_type().is_symlink(),
                "AGY_SETTINGS_UNAVAILABLE: the account state directory does not exist"
            );
        }
        let text = match &meta {
            Some(_) => fs::read_to_string(&file).context("AGY_SETTINGS_UNREADABLE")?,
            None => String::new(),
        };
        let Some(mut updated) = edit(&text, &keys)? else {
            return Ok(Outcome::AlreadyTrusted);
        };
        if meta.is_none() {
            updated.push('\n');
        }
        let mut tmp = tempfile::Builder::new()
            .prefix(".settings.json.pretrust-")
            .tempfile_in(&state)?;
        let mode = meta.as_ref().map_or(0o600, |m| m.mode() & 0o7777);
        tmp.as_file()
            .set_permissions(fs::Permissions::from_mode(mode))?;
        if let Some(m) = &meta
            && (tmp.as_file().metadata()?.uid() != m.uid()
                || tmp.as_file().metadata()?.gid() != m.gid())
        {
            std::os::unix::fs::fchown(tmp.as_file(), Some(m.uid()), Some(m.gid()))
                .context("could not keep the owner of settings.json")?;
        }
        tmp.write_all(updated.as_bytes())?;
        tmp.as_file().sync_all()?;
        // Compare-and-swap: replace only the exact file and content we edited.
        let unchanged = match (&meta, fs::symlink_metadata(&file)) {
            (Some(before), Ok(now)) => {
                now.ino() == before.ino()
                    && now.dev() == before.dev()
                    && fs::read_to_string(&file).ok().as_deref() == Some(text.as_str())
            }
            (None, Err(e)) => e.kind() == std::io::ErrorKind::NotFound,
            _ => false,
        };
        if !unchanged {
            continue;
        }
        tmp.persist(&file).map_err(|e| e.error)?;
        fs::File::open(&state)?.sync_all()?;
        return Ok(Outcome::Trusted);
    }
    bail!("AGY_SETTINGS_BUSY: settings.json kept changing")
}

/// Best-effort pre-trust before a start: failures never block the launch.
pub fn pretrust_before_start(home: &Path, directory: &Path) {
    if let Err(e) = pretrust(home, directory) {
        eprintln!(
            "sessionsation: Antigravity folder trust was not pre-accepted for {}: {e:#}",
            directory.display()
        );
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn setup() -> (tempfile::TempDir, tempfile::TempDir, PathBuf) {
        let home = tempfile::tempdir().unwrap();
        let dir = tempfile::tempdir().unwrap();
        let state = super::super::state_dir(home.path());
        fs::create_dir_all(&state).unwrap();
        let file = state.join("settings.json");
        (home, dir, file)
    }

    #[test]
    fn appends_one_element_and_keeps_every_other_byte() {
        let (home, dir, file) = setup();
        let d = fs::canonicalize(dir.path()).unwrap();
        let text = "{\n  \"queuedMessages\": \"queue\",\n  \"nested\": {\"x\": [1, 2.50, null]},\n  \"trustedWorkspaces\": [\n    \"/root\"\n  ],\n  \"tail\": 1\n}\n";
        fs::write(&file, text).unwrap();
        fs::set_permissions(&file, fs::Permissions::from_mode(0o640)).unwrap();
        let token = super::super::state_dir(home.path()).join("antigravity-oauth-token");
        fs::write(&token, "secret").unwrap();
        fs::set_permissions(&token, fs::Permissions::from_mode(0o000)).unwrap();

        assert_eq!(pretrust(home.path(), dir.path()).unwrap(), Outcome::Trusted);
        let after = fs::read_to_string(&file).unwrap();
        let expected = text.replace(
            "    \"/root\"\n",
            &format!("    \"/root\",\n    \"{}\"\n", d.display()),
        );
        assert_eq!(after, expected);
        assert_eq!(fs::metadata(&file).unwrap().mode() & 0o777, 0o640);
        assert_eq!(
            fs::read_dir(super::super::state_dir(home.path()))
                .unwrap()
                .count(),
            2,
            "only settings.json and the untouched token remain"
        );
        // Idempotent.
        assert_eq!(
            pretrust(home.path(), dir.path()).unwrap(),
            Outcome::AlreadyTrusted
        );
        assert_eq!(fs::read_to_string(&file).unwrap(), after);
    }

    #[test]
    fn creates_the_list_and_the_file_when_missing() {
        let (home, dir, file) = setup();
        fs::write(&file, "{\n  \"queuedMessages\": \"send\"\n}").unwrap();
        pretrust(home.path(), dir.path()).unwrap();
        let v: serde_json::Value = serde_json::from_slice(&fs::read(&file).unwrap()).unwrap();
        assert_eq!(v["queuedMessages"], "send");
        assert_eq!(v[KEY].as_array().unwrap().len(), 1);

        fs::remove_file(&file).unwrap();
        pretrust(home.path(), dir.path()).unwrap();
        let v: serde_json::Value = serde_json::from_slice(&fs::read(&file).unwrap()).unwrap();
        assert_eq!(v[KEY].as_array().unwrap().len(), 1);
        assert_eq!(fs::metadata(&file).unwrap().mode() & 0o777, 0o600);
    }

    #[test]
    fn refuses_symlinks_and_leaves_unparsable_files_alone() {
        let (home, dir, file) = setup();
        let real = home.path().join("real.json");
        fs::write(&real, "{}").unwrap();
        std::os::unix::fs::symlink(&real, &file).unwrap();
        let e = pretrust(home.path(), dir.path()).unwrap_err().to_string();
        assert!(e.contains("AGY_SETTINGS_SYMLINK"), "{e}");
        assert_eq!(fs::read_to_string(&real).unwrap(), "{}");

        fs::remove_file(&file).unwrap();
        fs::write(&file, "not json").unwrap();
        assert!(pretrust(home.path(), dir.path()).is_err());
        assert_eq!(fs::read_to_string(&file).unwrap(), "not json");

        fs::write(&file, "{\"trustedWorkspaces\": 5}").unwrap();
        assert!(pretrust(home.path(), dir.path()).is_err());
        assert_eq!(
            fs::read_to_string(&file).unwrap(),
            "{\"trustedWorkspaces\": 5}"
        );
    }

    #[test]
    fn a_missing_state_directory_is_not_created() {
        let home = tempfile::tempdir().unwrap();
        let dir = tempfile::tempdir().unwrap();
        assert!(pretrust(home.path(), dir.path()).is_err());
        assert!(!super::super::state_dir(home.path()).exists());
    }
}
