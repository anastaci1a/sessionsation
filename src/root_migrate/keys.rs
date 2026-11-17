//! Claude project keys: locating a conversation's files, the retained copy,
//! the no-overwrite move between keys, and the per-file memory merge (§4.2-4.4).
use super::*;
use crate::external::archiving::native::tree_digest;
use std::collections::BTreeMap;
use std::io::Read;
use std::os::unix::fs::{DirBuilderExt, MetadataExt};

/// Claude Code's project-directory name for a working directory: every
/// non-alphanumeric character becomes `-`.
pub fn key(directory: &str) -> String {
    directory
        .chars()
        .map(|c| if c.is_ascii_alphanumeric() { c } else { '-' })
        .collect()
}
/// Longer keys are truncated by Claude with a hash suffix; never re-keyed.
pub const KEY_MAX: usize = 200;

pub(super) fn sha256_file(path: &Path) -> Result<(u64, String)> {
    let mut f = fs::OpenOptions::new()
        .read(true)
        .custom_flags(libc::O_NOFOLLOW)
        .open(path)?;
    ensure!(
        f.metadata()?.is_file(),
        "HISTORY_NOT_REGULAR: {}",
        path.display()
    );
    let mut hash = Sha256::new();
    let mut buf = vec![0; 1 << 16];
    let mut n_total = 0u64;
    loop {
        let n = f.read(&mut buf)?;
        if n == 0 {
            break;
        }
        n_total += n as u64;
        hash.update(&buf[..n]);
    }
    Ok((n_total, format!("{:x}", hash.finalize())))
}
/// sha256 of the first `bytes` bytes (the pre-move transcript as a prefix of the live one).
pub(super) fn sha256_prefix(path: &Path, bytes: u64) -> Result<Option<String>> {
    let mut f = fs::OpenOptions::new()
        .read(true)
        .custom_flags(libc::O_NOFOLLOW)
        .open(path)?;
    if f.metadata()?.len() < bytes {
        return Ok(None);
    }
    let mut hash = Sha256::new();
    let mut left = bytes;
    let mut buf = vec![0; 1 << 16];
    while left > 0 {
        let want = left.min(buf.len() as u64) as usize;
        let n = f.read(&mut buf[..want])?;
        ensure!(n > 0, "HISTORY_CHANGED: {} shrank", path.display());
        hash.update(&buf[..n]);
        left -= n as u64;
    }
    Ok(Some(format!("{:x}", hash.finalize())))
}
/// sha256 of the last MiB (cheap identity check of a large transcript).
pub(super) fn sha256_tail(path: &Path) -> Result<String> {
    use std::io::{Seek, SeekFrom};
    let mut f = fs::File::open(path)?;
    let len = f.metadata()?.len();
    let start = len.saturating_sub(1 << 20);
    f.seek(SeekFrom::Start(start))?;
    let mut buf = vec![];
    f.read_to_end(&mut buf)?;
    Ok(format!("{:x}", Sha256::digest(&buf)))
}

/// One conversation's files in one project directory.
#[derive(Clone, Debug, Serialize, Deserialize, PartialEq)]
pub struct Found {
    pub conversation_id: String,
    pub key: String,
    pub transcript: PathBuf,
    pub sidecar: Option<PathBuf>,
}
/// `<id>.jsonl` (and the `<id>/` sidecar beside it) for every lineage id, in every
/// project directory. More than one transcript for an id is `ROOT_REKEY_CONFLICT`.
pub fn locate(projects: &Path, ids: &[String]) -> Result<Vec<Found>> {
    let mut out = vec![];
    for id in ids {
        uuid::Uuid::parse_str(id).context("INVALID_CONVERSATION_ID")?;
        let mut hits = vec![];
        if let Ok(dirs) = fs::read_dir(projects) {
            for d in dirs.flatten() {
                if !d.file_type().is_ok_and(|t| t.is_dir()) {
                    continue;
                }
                let t = d.path().join(format!("{id}.jsonl"));
                if fs::symlink_metadata(&t).is_ok_and(|m| m.file_type().is_file()) {
                    hits.push((d.file_name().to_string_lossy().into_owned(), t));
                }
            }
        }
        hits.sort();
        ensure!(
            hits.len() <= 1,
            "ROOT_REKEY_CONFLICT: conversation {id} has a transcript in {} project directories ({}); nothing was moved",
            hits.len(),
            hits.iter()
                .map(|(k, _)| k.as_str())
                .collect::<Vec<_>>()
                .join(", ")
        );
        if let Some((k, t)) = hits.pop() {
            let side = t.with_file_name(id);
            let sidecar = fs::symlink_metadata(&side)
                .is_ok_and(|m| m.file_type().is_dir())
                .then_some(side);
            out.push(Found {
                conversation_id: id.clone(),
                key: k,
                transcript: t,
                sidecar,
            });
        }
    }
    Ok(out)
}
/// Names this session would add to `K_new` that already exist there (whatever
/// their content). Names already in `K_new` because the transcript lives there
/// are not collisions.
pub fn collisions(projects: &Path, found: &[Found], key_new: &str) -> Vec<String> {
    let dst = projects.join(key_new);
    let mut out = vec![];
    for f in found.iter().filter(|f| f.key != key_new) {
        for name in names(f) {
            let p = dst.join(&name);
            if fs::symlink_metadata(&p).is_ok() {
                out.push(p.to_string_lossy().into_owned());
            }
        }
    }
    out
}
fn names(f: &Found) -> Vec<String> {
    let mut n = vec![format!("{}.jsonl", f.conversation_id)];
    if f.sidecar.is_some() {
        n.push(f.conversation_id.clone());
    }
    n
}

/// One retained (copied and verified) name.
#[derive(Clone, Debug, Serialize, Deserialize, PartialEq)]
pub struct Retained {
    pub name: String,
    pub key: String,
    pub source: PathBuf,
    pub copy: PathBuf,
    pub dir: bool,
    pub bytes: u64,
    /// Transcript: sha256 of the whole file; sidecar: tree digest.
    pub sha256: String,
    #[serde(default)]
    pub tail_sha256: Option<String>,
    pub verified: bool,
}
/// Copy every name of `found` to `dir/<key>/<name>` (reflink, else a full copy
/// only with `allow_full_copy`) and verify the copy against the digests taken
/// before copying. The copy is never deleted by any 1.54 command.
pub fn retain(found: &[Found], dir: &Path, allow_full_copy: bool) -> Result<Vec<Retained>> {
    let mut out = vec![];
    for f in found {
        for name in names(f) {
            let source = f.transcript.with_file_name(&name);
            let is_dir = fs::symlink_metadata(&source)?.is_dir();
            let (bytes, sha256) = if is_dir {
                tree_digest(&source)?
            } else {
                sha256_file(&source)?
            };
            let tail = if is_dir {
                None
            } else {
                Some(sha256_tail(&source)?)
            };
            let base = dir.join(&f.key);
            fs::DirBuilder::new()
                .recursive(true)
                .mode(0o700)
                .create(&base)?;
            let copy = base.join(&name);
            ensure!(
                fs::symlink_metadata(&copy).is_err(),
                "BACKUP_NOT_EMPTY: retained copy {} already exists",
                copy.display()
            );
            let mut stats = crate::shared_state::migrate::CopyStats::default();
            crate::shared_state::migrate::copy_tree(&source, &copy, allow_full_copy, &mut stats)?;
            let (copied_bytes, copied) = if is_dir {
                tree_digest(&copy)?
            } else {
                sha256_file(&copy)?
            };
            ensure!(
                copied == sha256 && copied_bytes == bytes,
                "RETAIN_VERIFY_FAILED: the copy of {} does not match it; nothing was moved",
                source.display()
            );
            // The source must still be the bytes that were copied.
            let now = if is_dir {
                tree_digest(&source)?.1
            } else {
                sha256_file(&source)?.1
            };
            ensure!(
                now == sha256,
                "HISTORY_CHANGED: {} changed while it was copied; nothing was moved",
                source.display()
            );
            out.push(Retained {
                name,
                key: f.key.clone(),
                source,
                copy,
                dir: is_dir,
                bytes,
                sha256,
                tail_sha256: tail,
                verified: true,
            });
        }
    }
    Ok(out)
}

#[derive(Clone, Debug, Serialize, Deserialize, PartialEq)]
pub struct Moved {
    pub name: String,
    pub from: PathBuf,
    pub to: PathBuf,
    pub digest: String,
}
/// Move every retained name from its key to `K_new` with `RENAME_NOREPLACE`.
/// `K_new` is created (0700) only when absent; nothing in it is overwritten. A
/// name that appears after the preflight stops the move and the names already
/// moved are moved back. A rerun after a crash counts a recorded name with no
/// source and a matching destination digest as done.
pub fn move_names(projects: &Path, retained: &[Retained], key_new: &str) -> Result<Vec<Moved>> {
    let dst_dir = projects.join(key_new);
    if fs::symlink_metadata(&dst_dir).is_err() {
        fs::DirBuilder::new().mode(0o700).create(&dst_dir)?;
    }
    ensure!(
        fs::symlink_metadata(&dst_dir).is_ok_and(|m| m.is_dir() && !m.file_type().is_symlink()),
        "ROOT_REKEY_CONFLICT: {} is not a directory",
        dst_dir.display()
    );
    let mut moved: Vec<Moved> = vec![];
    for r in retained.iter().filter(|r| r.key != key_new) {
        let to = dst_dir.join(&r.name);
        let digest_of = |p: &Path| -> Result<String> {
            Ok(if r.dir {
                tree_digest(p)?.1
            } else {
                sha256_file(p)?.1
            })
        };
        if fs::symlink_metadata(&r.source).is_err()
            && fs::symlink_metadata(&to).is_ok()
            && digest_of(&to)? == r.sha256
        {
            moved.push(Moved {
                name: r.name.clone(),
                from: r.source.clone(),
                to,
                digest: r.sha256.clone(),
            });
            continue;
        }
        let result = (|| -> Result<()> {
            ensure!(
                digest_of(&r.source)? == r.sha256,
                "HISTORY_CHANGED: {} changed after it was retained",
                r.source.display()
            );
            crate::shared_state::rename_noreplace(&r.source, &to).map_err(|e| {
                if e.kind() == std::io::ErrorKind::AlreadyExists {
                    anyhow::anyhow!(
                        "ROOT_REKEY_CONFLICT: {} appeared in the target key; it was not overwritten",
                        to.display()
                    )
                } else {
                    anyhow::anyhow!("ROOT_REKEY_MOVE_FAILED: {}: {e}", r.source.display())
                }
            })?;
            Ok(())
        })();
        if let Err(e) = result {
            let _ = move_back(&moved);
            return Err(e);
        }
        fs::File::open(&dst_dir)?.sync_all()?;
        moved.push(Moved {
            name: r.name.clone(),
            from: r.source.clone(),
            to: to.clone(),
            digest: r.sha256.clone(),
        });
        let after = digest_of(&to)?;
        if after != r.sha256 {
            let _ = move_back(&moved);
            bail!("HISTORY_CHANGED: {} changed during the move", to.display());
        }
    }
    Ok(moved)
}
/// Move names back to where they came from (never overwriting).
pub fn move_back(moved: &[Moved]) -> Result<()> {
    for m in moved.iter().rev() {
        if fs::symlink_metadata(&m.to).is_err() && fs::symlink_metadata(&m.from).is_ok() {
            continue; // already back
        }
        crate::shared_state::rename_noreplace(&m.to, &m.from).with_context(|| {
            format!(
                "ROOT_REKEY_REVERT_FAILED: {} -> {}",
                m.to.display(),
                m.from.display()
            )
        })?;
        if let Some(parent) = m.from.parent() {
            fs::File::open(parent)?.sync_all()?;
        }
    }
    Ok(())
}

// ------------------------------------------------------------------ memory

#[derive(Clone, Debug, Default, Serialize, Deserialize, PartialEq)]
pub struct MemoryResult {
    pub copied: Vec<String>,
    pub fast_forwarded: Vec<String>,
    pub identical: Vec<String>,
    pub kept_newer: Vec<String>,
    pub conflicts: Vec<String>,
    pub only_in_target: Vec<String>,
    pub skipped_symlinks: Vec<String>,
}
impl MemoryResult {
    pub fn clean(&self) -> bool {
        self.conflicts.is_empty()
    }
}

/// Merge bases: per unordered key pair, per file, the digest the last sync left on both sides.
#[derive(Clone, Debug, Default, Serialize, Deserialize)]
pub struct Bases(pub BTreeMap<String, BTreeMap<String, String>>);
pub fn pair(a: &str, b: &str) -> String {
    if a <= b {
        format!("{a} {b}")
    } else {
        format!("{b} {a}")
    }
}
fn files_of(dir: &Path, base: &Path, out: &mut Vec<(String, bool)>) {
    let Ok(entries) = fs::read_dir(dir) else {
        return;
    };
    let mut entries: Vec<_> = entries.flatten().collect();
    entries.sort_by_key(|e| e.file_name());
    for e in entries {
        let p = e.path();
        let rel = p
            .strip_prefix(base)
            .unwrap_or(&p)
            .to_string_lossy()
            .into_owned();
        match e.file_type() {
            Ok(t) if t.is_symlink() => out.push((rel, true)),
            Ok(t) if t.is_dir() => files_of(&p, base, out),
            Ok(t) if t.is_file() => out.push((rel, false)),
            _ => {}
        }
    }
}
fn replace_with(src: &Path, dst: &Path) -> Result<()> {
    let parent = dst.parent().context("memory file has no parent")?;
    fs::DirBuilder::new()
        .recursive(true)
        .mode(0o700)
        .create(parent)?;
    let tmp = parent.join(format!(".rootmig-{}", uuid::Uuid::new_v4()));
    let mut stats = crate::shared_state::migrate::CopyStats::default();
    crate::shared_state::migrate::copy_tree(src, &tmp, true, &mut stats)?;
    if let Ok(m) = fs::metadata(src) {
        let _ = fs::set_permissions(&tmp, fs::Permissions::from_mode(m.mode() & 0o7777));
    }
    fs::rename(&tmp, dst)?;
    fs::File::open(parent)?.sync_all()?;
    Ok(())
}
fn save_version(path: &Path, dir: &Path, rel: &str, tag: &str) -> Result<PathBuf> {
    let to = dir.join(format!("{rel}.{tag}"));
    if let Some(p) = to.parent() {
        fs::DirBuilder::new()
            .recursive(true)
            .mode(0o700)
            .create(p)?;
    }
    if fs::symlink_metadata(&to).is_err() {
        let mut stats = crate::shared_state::migrate::CopyStats::default();
        crate::shared_state::migrate::copy_tree(path, &to, true, &mut stats)?;
    }
    Ok(to)
}

/// Merge the memory directory `src` into `dst` per file (§4.3). Nothing is ever
/// deleted or truncated on either side; a file is replaced only by a newer
/// version of itself (fast-forward against the merge base); a real conflict
/// keeps both files in place, copies both into `backup/memory-conflicts/` and
/// is returned. `dry_run` computes the same result without writing.
pub fn sync_memory(
    src: &Path,
    dst: &Path,
    base: &mut BTreeMap<String, String>,
    backup: Option<&Path>,
    label: &str,
    dry_run: bool,
) -> Result<MemoryResult> {
    let mut r = MemoryResult::default();
    if fs::symlink_metadata(src).is_err() {
        return Ok(r);
    }
    ensure!(
        fs::symlink_metadata(src).is_ok_and(|m| m.is_dir() && !m.file_type().is_symlink()),
        "MEMORY_UNSAFE: {} is not a directory",
        src.display()
    );
    let mut src_files = vec![];
    files_of(src, src, &mut src_files);
    let mut dst_files = vec![];
    files_of(dst, dst, &mut dst_files);
    for (rel, link) in &dst_files {
        if !link && !src_files.iter().any(|(s, _)| s == rel) {
            r.only_in_target.push(rel.clone());
        }
    }
    for (rel, link) in src_files {
        if link {
            r.skipped_symlinks.push(rel);
            continue;
        }
        let s = src.join(&rel);
        let d = dst.join(&rel);
        let (_, sd) = sha256_file(&s)?;
        let dmeta = fs::symlink_metadata(&d).ok();
        match dmeta {
            None => {
                if !dry_run {
                    if let Some(p) = d.parent() {
                        fs::DirBuilder::new()
                            .recursive(true)
                            .mode(0o700)
                            .create(p)?;
                    }
                    let mut stats = crate::shared_state::migrate::CopyStats::default();
                    crate::shared_state::migrate::copy_tree(&s, &d, true, &mut stats)?;
                    ensure!(
                        sha256_file(&d)?.1 == sd,
                        "MEMORY_VERIFY_FAILED: {}",
                        d.display()
                    );
                    base.insert(rel.clone(), sd);
                }
                r.copied.push(rel);
            }
            Some(m) if m.file_type().is_symlink() || !m.is_file() => {
                r.conflicts.push(rel);
            }
            Some(_) => {
                let (_, dd) = sha256_file(&d)?;
                let b = base.get(&rel).cloned();
                if dd == sd {
                    if !dry_run {
                        base.insert(rel.clone(), sd);
                    }
                    r.identical.push(rel);
                } else if b.as_deref() == Some(dd.as_str()) {
                    if !dry_run {
                        if let Some(dir) = backup {
                            save_version(
                                &d,
                                &dir.join("memory-versions").join(label),
                                &rel,
                                &dd[..12],
                            )?;
                        }
                        replace_with(&s, &d)?;
                        ensure!(
                            sha256_file(&d)?.1 == sd,
                            "MEMORY_VERIFY_FAILED: {}",
                            d.display()
                        );
                        base.insert(rel.clone(), sd);
                    }
                    r.fast_forwarded.push(rel);
                } else if b.as_deref() == Some(sd.as_str()) {
                    r.kept_newer.push(rel);
                } else {
                    if !dry_run && let Some(dir) = backup {
                        let c = dir.join("memory-conflicts").join(label);
                        save_version(&s, &c, &rel, "source")?;
                        save_version(&d, &c, &rel, "target")?;
                    }
                    r.conflicts.push(rel);
                }
            }
        }
    }
    Ok(r)
}

// ------------------------------------------------------------- .claude.json

/// Clone each `projects["<dir>"]` entry whose key maps through `alias` and whose
/// logical key is absent; the old entry stays. Returns the keys added with the
/// sha256 of the cloned raw value. Uses the raw-member editor with the
/// concurrent-write retry, so every other byte of the file is kept.
pub fn clone_project_entries(
    home: &Path,
    alias: &crate::alias::Alias,
    only: Option<&str>,
    dry_run: bool,
) -> Result<Vec<(String, String)>> {
    use crate::provider::claude::trust::{Members, raw};
    let mut added = vec![];
    crate::provider::claude::trust::edit_config(home, |text| {
        added.clear();
        let mut top = Members::parse(text).context("CLAUDE_CONFIG_UNREADABLE")?;
        let Some(projects) = top.get("projects") else {
            return Ok(None);
        };
        if projects.get().trim() == "null" {
            return Ok(None);
        }
        let mut projects = Members::parse(projects.get()).context("CLAUDE_CONFIG_UNREADABLE")?;
        let existing: Vec<(String, String)> = projects
            .0
            .iter()
            .map(|(k, v)| (k.clone(), v.get().to_owned()))
            .collect();
        for (k, v) in &existing {
            let mapped = alias.logical(Path::new(k)).to_string_lossy().into_owned();
            if mapped == *k || only.is_some_and(|o| o != k) {
                continue;
            }
            if projects.get(&mapped).is_some() || added.iter().any(|(a, _)| *a == mapped) {
                continue;
            }
            projects.0.push((mapped.clone(), raw(v.clone())));
            added.push((mapped, format!("{:x}", Sha256::digest(v.as_bytes()))));
        }
        if added.is_empty() || dry_run {
            return Ok(None);
        }
        top.set("projects", raw(projects.render(1)));
        let trailing = &text[text.trim_end().len()..];
        Ok(Some(format!("{}{trailing}", top.render(0))))
    })?;
    Ok(added)
}
/// Remove cloned entries whose value is still the cloned value (rollback).
pub fn remove_project_entries(home: &Path, keys: &[(String, String)]) -> Result<Vec<String>> {
    use crate::provider::claude::trust::{Members, raw};
    let mut removed = vec![];
    crate::provider::claude::trust::edit_config(home, |text| {
        removed.clear();
        let mut top = Members::parse(text).context("CLAUDE_CONFIG_UNREADABLE")?;
        let Some(projects) = top.get("projects") else {
            return Ok(None);
        };
        let mut projects = Members::parse(projects.get()).context("CLAUDE_CONFIG_UNREADABLE")?;
        projects.0.retain(|(k, v)| {
            let drop = keys.iter().any(|(key, sha)| {
                key == k && format!("{:x}", Sha256::digest(v.get().as_bytes())) == *sha
            });
            if drop {
                removed.push(k.clone());
            }
            !drop
        });
        if removed.is_empty() {
            return Ok(None);
        }
        top.set("projects", raw(projects.render(1)));
        let trailing = &text[text.trim_end().len()..];
        Ok(Some(format!("{}{trailing}", top.render(0))))
    })?;
    Ok(removed)
}
