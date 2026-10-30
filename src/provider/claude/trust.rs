//! Folder pre-trust for yolo Claude starts (contract revision 20).
//!
//! Claude Code 2.1.x asks "trust this folder?" the first time an interactive
//! session opens a directory. It records the answer in the account's global
//! config, `$CLAUDE_CONFIG_DIR/.claude.json`, as
//! `projects["<absolute directory>"].hasTrustDialogAccepted = true`, and a later
//! start in that directory (or below it, up to its git root) skips the dialog.
//! Before a yolo start Sessionsation writes exactly that key for the launch
//! directory, so a new session never stops at the dialog.
//!
//! Only that key changes: every other byte of the file is kept (top-level and
//! `projects` members are copied as raw JSON text, in order), the file is
//! replaced atomically with its mode and owner, a symlink is refused, and a
//! missing or unparsable file is left alone. Credentials (`.credentials.json`)
//! are never read.
use anyhow::{Context, Result, bail, ensure};
use serde::de::{Deserialize, Deserializer, MapAccess, Visitor};
use serde_json::value::RawValue;
use std::{
    fs,
    io::Write,
    os::unix::fs::{MetadataExt, PermissionsExt},
    path::{Path, PathBuf},
};

pub const CONFIG_FILE: &str = ".claude.json";
pub const TRUST_KEY: &str = "hasTrustDialogAccepted";

/// What a pre-trust attempt did.
#[derive(Debug, PartialEq, Eq)]
pub enum Outcome {
    /// The key was written.
    Trusted,
    /// The directory was already trusted; nothing was written.
    AlreadyTrusted,
    /// The account has no global config yet; nothing was written.
    NoConfig,
}

/// A JSON object kept as ordered members with raw (unparsed) values.
pub(crate) struct Members(pub(crate) Vec<(String, Box<RawValue>)>);
impl<'de> Deserialize<'de> for Members {
    fn deserialize<D: Deserializer<'de>>(d: D) -> std::result::Result<Self, D::Error> {
        struct V;
        impl<'de> Visitor<'de> for V {
            type Value = Members;
            fn expecting(&self, f: &mut std::fmt::Formatter) -> std::fmt::Result {
                f.write_str("a JSON object")
            }
            fn visit_map<A: MapAccess<'de>>(
                self,
                mut map: A,
            ) -> std::result::Result<Members, A::Error> {
                let mut out = Vec::new();
                while let Some((k, v)) = map.next_entry::<String, Box<RawValue>>()? {
                    out.push((k, v));
                }
                Ok(Members(out))
            }
        }
        d.deserialize_map(V)
    }
}
impl Members {
    pub(crate) fn parse(text: &str) -> Option<Self> {
        serde_json::from_str(text).ok()
    }
    pub(crate) fn get(&self, key: &str) -> Option<&RawValue> {
        self.0
            .iter()
            .rev()
            .find(|(k, _)| k == key)
            .map(|(_, v)| &**v)
    }
    /// Replace every member named `key` (keeping its position), or append it.
    pub(crate) fn set(&mut self, key: &str, value: Box<RawValue>) {
        let mut found = false;
        for (k, v) in &mut self.0 {
            if k == key {
                *v = value.clone();
                found = true;
            }
        }
        if !found {
            self.0.push((key.to_owned(), value));
        }
    }
    /// `JSON.stringify(_, null, 2)` layout at `depth`, so untouched members,
    /// which carry their own nested indentation, keep their original bytes.
    pub(crate) fn render(&self, depth: usize) -> String {
        if self.0.is_empty() {
            return "{}".into();
        }
        let inner = "  ".repeat(depth + 1);
        let body = self
            .0
            .iter()
            .map(|(k, v)| {
                format!(
                    "{inner}{}: {}",
                    serde_json::to_string(k).expect("string key"),
                    v.get()
                )
            })
            .collect::<Vec<_>>()
            .join(",\n");
        format!("{{\n{body}\n{}}}", "  ".repeat(depth))
    }
}

pub(crate) fn raw(text: String) -> Box<RawValue> {
    RawValue::from_string(text).expect("valid JSON")
}

/// Claude's own per-project defaults (2.1.312), used for a directory the config
/// does not know yet, as Claude itself writes them when the dialog is accepted.
fn new_project_entry() -> Members {
    Members(
        [
            ("allowedTools", "[]"),
            ("mcpContextUris", "[]"),
            ("mcpServers", "{}"),
            ("enabledMcpjsonServers", "[]"),
            ("disabledMcpjsonServers", "[]"),
            (TRUST_KEY, "true"),
            ("hasClaudeMdExternalIncludesApproved", "false"),
            ("hasClaudeMdExternalIncludesWarningShown", "false"),
        ]
        .map(|(k, v)| (k.to_owned(), raw(v.to_owned())))
        .to_vec(),
    )
}

/// The edited config text, or `None` when every key is already trusted.
fn edit(text: &str, keys: &[String]) -> Result<Option<String>> {
    let mut top = Members::parse(text)
        .context("CLAUDE_CONFIG_UNREADABLE: .claude.json is not a JSON object")?;
    let mut projects = match top.get("projects") {
        Some(p) if p.get().trim() == "null" => Members(vec![]),
        Some(p) => Members::parse(p.get())
            .context("CLAUDE_CONFIG_UNREADABLE: .claude.json projects is not an object")?,
        None => Members(vec![]),
    };
    let mut changed = false;
    for key in keys {
        let entry = match projects.get(key) {
            None => Some(new_project_entry()),
            Some(entry) => {
                let mut entry = Members::parse(entry.get()).with_context(
                    || "CLAUDE_CONFIG_UNREADABLE: a .claude.json project entry is not an object",
                )?;
                if entry
                    .get(TRUST_KEY)
                    .is_some_and(|v| v.get().trim() == "true")
                {
                    None
                } else {
                    entry.set(TRUST_KEY, raw("true".into()));
                    Some(entry)
                }
            }
        };
        if let Some(entry) = entry {
            projects.set(key, raw(entry.render(2)));
            changed = true;
        }
    }
    if !changed {
        return Ok(None);
    }
    top.set("projects", raw(projects.render(1)));
    let trailing = &text[text.trim_end().len()..];
    Ok(Some(format!("{}{trailing}", top.render(0))))
}

/// Project keys Claude may look up for a start in `directory`: the directory
/// as the process sees it (canonical) and, when different, as recorded.
fn project_keys(directory: &Path) -> Result<Vec<String>> {
    ensure!(directory.is_absolute(), "directory must be absolute");
    let canonical = fs::canonicalize(directory)
        .with_context(|| format!("directory {} is unavailable", directory.display()))?;
    let mut keys = Vec::new();
    for path in [canonical.as_path(), directory] {
        let key = path.to_str().context("directory is not UTF-8")?.to_owned();
        let key = if key.len() > 1 {
            key.trim_end_matches('/').to_owned()
        } else {
            key
        };
        if !keys.contains(&key) {
            keys.push(key);
        }
    }
    Ok(keys)
}

/// Pre-accept Claude's folder trust for `directory` in the account at `home`.
pub fn pretrust(home: &Path, directory: &Path) -> Result<Outcome> {
    let keys = project_keys(directory)?;
    let file: PathBuf = home.join(CONFIG_FILE);
    // Bounded retries: a concurrent Claude write between our read and rename
    // is detected and the edit is redone on the new content.
    for _ in 0..5 {
        let meta = match fs::symlink_metadata(&file) {
            Ok(m) => m,
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(Outcome::NoConfig),
            Err(e) => return Err(e).context("CLAUDE_CONFIG_UNREADABLE"),
        };
        ensure!(
            !meta.file_type().is_symlink(),
            "CLAUDE_CONFIG_SYMLINK: refusing to replace a symlinked .claude.json"
        );
        ensure!(
            meta.file_type().is_file(),
            "CLAUDE_CONFIG_UNREADABLE: .claude.json is not a regular file"
        );
        let text = fs::read_to_string(&file).context("CLAUDE_CONFIG_UNREADABLE")?;
        let Some(updated) = edit(&text, &keys)? else {
            return Ok(Outcome::AlreadyTrusted);
        };
        let mut tmp = tempfile::Builder::new()
            .prefix(".claude.json.pretrust-")
            .tempfile_in(home)?;
        tmp.as_file()
            .set_permissions(fs::Permissions::from_mode(meta.mode() & 0o7777))?;
        if tmp.as_file().metadata()?.uid() != meta.uid()
            || tmp.as_file().metadata()?.gid() != meta.gid()
        {
            std::os::unix::fs::fchown(tmp.as_file(), Some(meta.uid()), Some(meta.gid()))
                .context("could not keep the owner of .claude.json")?;
        }
        tmp.write_all(updated.as_bytes())?;
        tmp.as_file().sync_all()?;
        // Compare-and-swap: replace only the exact file and content we edited.
        let now = fs::symlink_metadata(&file)?;
        if now.ino() != meta.ino()
            || now.dev() != meta.dev()
            || fs::read_to_string(&file).ok().as_deref() != Some(text.as_str())
        {
            continue;
        }
        tmp.persist(&file).map_err(|e| e.error)?;
        fs::File::open(home)?.sync_all()?;
        return Ok(Outcome::Trusted);
    }
    bail!("CLAUDE_CONFIG_BUSY: .claude.json kept changing")
}

/// Best-effort pre-trust before a start: failures never block the launch.
pub fn pretrust_before_start(home: &Path, directory: &Path) {
    if let Err(e) = pretrust(home, directory) {
        eprintln!(
            "sessionsation: Claude folder trust was not pre-accepted for {}: {e:#}",
            directory.display()
        );
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::{Value, json};

    fn pretty(v: &Value) -> String {
        // serde_json's pretty printer matches JSON.stringify(_, null, 2).
        serde_json::to_string_pretty(v).unwrap()
    }

    #[test]
    fn writes_only_the_trust_key_and_keeps_every_other_byte() {
        let home = tempfile::tempdir().unwrap();
        let dir = tempfile::tempdir().unwrap();
        let d = fs::canonicalize(dir.path()).unwrap();
        let d = d.to_str().unwrap();
        let original = json!({
            "numStartups": 12,
            "zeta": {"nested": [1, 2.50, {"x": null}]},
            "projects": {
                "/other": {"allowedTools": ["Bash"], "hasTrustDialogAccepted": true},
                d: {"allowedTools": [], "lastCost": 1.25, "hasTrustDialogAccepted": false}
            },
            "oauthAccount": {"emailAddress": "kept@example.invalid"},
            "alpha": "last"
        });
        // Byte-exact number spelling and key order must survive.
        let text = pretty(&original).replace("2.5", "2.50") + "\n";
        let file = home.path().join(CONFIG_FILE);
        fs::write(&file, &text).unwrap();
        fs::set_permissions(&file, fs::Permissions::from_mode(0o640)).unwrap();
        fs::write(home.path().join(".credentials.json"), "secret").unwrap();
        fs::set_permissions(
            home.path().join(".credentials.json"),
            fs::Permissions::from_mode(0o000),
        )
        .unwrap();

        assert_eq!(pretrust(home.path(), dir.path()).unwrap(), Outcome::Trusted);
        let after = fs::read_to_string(&file).unwrap();
        assert_eq!(
            after,
            text.replace(
                "\"hasTrustDialogAccepted\": false",
                "\"hasTrustDialogAccepted\": true"
            )
        );
        assert_eq!(fs::metadata(&file).unwrap().mode() & 0o7777, 0o640);
        assert_eq!(
            fs::read(home.path().join(".credentials.json")).unwrap(),
            b"secret"
        );
        // Idempotent: a trusted directory is never rewritten.
        let ino = fs::metadata(&file).unwrap().ino();
        assert_eq!(
            pretrust(home.path(), dir.path()).unwrap(),
            Outcome::AlreadyTrusted
        );
        assert_eq!(fs::metadata(&file).unwrap().ino(), ino);
        // No temporary files are left beside the config.
        let names: Vec<_> = fs::read_dir(home.path())
            .unwrap()
            .map(|e| e.unwrap().file_name().into_string().unwrap())
            .collect();
        assert!(names.iter().all(|n| !n.contains("pretrust")), "{names:?}");
    }

    #[test]
    fn unknown_directory_gets_claude_defaults_and_order_is_kept() {
        let home = tempfile::tempdir().unwrap();
        let dir = tempfile::tempdir().unwrap();
        let d = fs::canonicalize(dir.path()).unwrap();
        // Literal text: unsorted keys, as Claude writes them.
        let text = "{\n  \"b\": 1,\n  \"a\": {\n    \"projects\": \"not top\"\n  },\n  \"projects\": {\n    \"/x\": {}\n  }\n}";
        fs::write(home.path().join(CONFIG_FILE), text).unwrap();
        pretrust(home.path(), dir.path()).unwrap();
        let after = fs::read_to_string(home.path().join(CONFIG_FILE)).unwrap();
        let v: Value = serde_json::from_str(&after).unwrap();
        let entry = &v["projects"][d.to_str().unwrap()];
        assert_eq!(entry["hasTrustDialogAccepted"], true);
        assert_eq!(entry["allowedTools"], json!([]));
        assert_eq!(v["projects"]["/x"], json!({}));
        assert_eq!(v["a"], json!({"projects": "not top"}));
        assert!(after.starts_with("{\n  \"b\": 1,\n  \"a\""), "{after}");
        // Claude's JSON.stringify(_, null, 2) layout: the old text is a prefix.
        let kept = "{\n  \"b\": 1,\n  \"a\": {\n    \"projects\": \"not top\"\n  },\n  \"projects\": {\n    \"/x\": {},\n    ";
        assert!(after.starts_with(kept), "{after}");
        assert!(after.contains("\n      \"allowedTools\": [],\n"), "{after}");
        assert!(after.ends_with("\n    }\n  }\n}"), "{after}");
    }

    #[test]
    fn config_without_projects_gains_one_entry() {
        let home = tempfile::tempdir().unwrap();
        let dir = tempfile::tempdir().unwrap();
        fs::write(home.path().join(CONFIG_FILE), "{\"numStartups\":1}").unwrap();
        pretrust(home.path(), dir.path()).unwrap();
        let v: Value =
            serde_json::from_slice(&fs::read(home.path().join(CONFIG_FILE)).unwrap()).unwrap();
        assert_eq!(v["numStartups"], 1);
        let key = fs::canonicalize(dir.path()).unwrap();
        assert_eq!(
            v["projects"][key.to_str().unwrap()]["hasTrustDialogAccepted"],
            true
        );
    }

    #[test]
    fn refuses_symlinks_and_never_touches_bad_or_missing_files() {
        let home = tempfile::tempdir().unwrap();
        let dir = tempfile::tempdir().unwrap();
        // Missing: nothing is created.
        assert_eq!(
            pretrust(home.path(), dir.path()).unwrap(),
            Outcome::NoConfig
        );
        assert!(!home.path().join(CONFIG_FILE).exists());
        // Symlink: refused, target unchanged.
        let target = home.path().join("real.json");
        fs::write(&target, "{}").unwrap();
        std::os::unix::fs::symlink(&target, home.path().join(CONFIG_FILE)).unwrap();
        let err = pretrust(home.path(), dir.path()).unwrap_err();
        assert!(
            format!("{err:#}").contains("CLAUDE_CONFIG_SYMLINK"),
            "{err:#}"
        );
        assert_eq!(fs::read_to_string(&target).unwrap(), "{}");
        fs::remove_file(home.path().join(CONFIG_FILE)).unwrap();
        // Unparsable or wrongly shaped: refused, unchanged.
        for bad in [
            "{not json",
            "[]",
            "{\"projects\": []}",
            "{\"projects\": {\"KEY\": 3}}",
        ] {
            let key = fs::canonicalize(dir.path()).unwrap();
            let bad = bad.replace("KEY", key.to_str().unwrap());
            fs::write(home.path().join(CONFIG_FILE), &bad).unwrap();
            assert!(pretrust(home.path(), dir.path()).is_err(), "{bad}");
            assert_eq!(
                fs::read_to_string(home.path().join(CONFIG_FILE)).unwrap(),
                bad
            );
        }
    }

    #[test]
    fn symlinked_directory_is_trusted_under_both_spellings() {
        let home = tempfile::tempdir().unwrap();
        let dir = tempfile::tempdir().unwrap();
        let link = home.path().join("link");
        std::os::unix::fs::symlink(dir.path(), &link).unwrap();
        fs::write(home.path().join(CONFIG_FILE), "{}").unwrap();
        pretrust(home.path(), &link).unwrap();
        let v: Value =
            serde_json::from_slice(&fs::read(home.path().join(CONFIG_FILE)).unwrap()).unwrap();
        let real = fs::canonicalize(dir.path()).unwrap();
        for key in [real.to_str().unwrap(), link.to_str().unwrap()] {
            assert_eq!(v["projects"][key]["hasTrustDialogAccepted"], true, "{key}");
        }
    }
}
