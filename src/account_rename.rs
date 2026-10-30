//! "Rename & update backend" (contract 1.30.0): change an account's display name
//! AND its backend name (registry id, account home folder, session references).
//!
//! The display-name-only rename is a plain registry edit (`account/rename`).
//! This operation moves the account home to the layout-2 path
//! `accounts/<platform>/<new>` (even from a deferred legacy path) and rewrites
//! every reference the backend owns. It is journaled in
//! `operations/account-renames/<id>.json`, runs inside the supervisor (the sole
//! registry writer) under the account operation locks, and is never replayed:
//!
//! ```text
//! admitted -> moving_home -> home_moved -> registry_updated -> complete
//!                  \             \              \
//!                   failed       uncertain      uncertain   (-> recover | abandoned)
//! ```
//!
//! Crash points are resolved by evidence, not by guessing: [`reconcile`] looks at
//! where the home actually is and what the registry says (supervisor start and
//! `api recover`); anything it cannot decide stays `uncertain` until an operator
//! abandons it (`api abandon-operation`). Old receipts, digests and message
//! journals keep their historical ids and are never rewritten.
use crate::{
    accounts,
    model::{Account, State},
    paths::{Paths, atomic_write, private_dir, validate_new_account_name},
    provider::ProviderId,
    store::Store,
};
use anyhow::{Context, Result, bail, ensure};
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use sha2::{Digest, Sha256};
use std::{
    fs,
    path::{Path, PathBuf},
};

pub const PHASES_TERMINAL: &[&str] = &["complete", "failed", "abandoned"];

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct Record {
    pub schema_version: u32,
    pub id: String,
    pub kind: String,
    pub provider: ProviderId,
    pub account_id: String,
    pub new_account_id: String,
    pub old_name: String,
    pub new_name: String,
    pub old_home: PathBuf,
    pub new_home: PathBuf,
    pub display_name_before: Option<String>,
    pub phase: String,
    pub detail: String,
    pub created_at: i64,
    pub updated_at: i64,
}

fn dir(paths: &Paths) -> PathBuf {
    paths.root.join(crate::layout::ACCOUNT_RENAMES)
}
fn file(paths: &Paths, id: &str) -> Result<PathBuf> {
    uuid::Uuid::parse_str(id).context("INVALID_OPERATION_ID")?;
    Ok(dir(paths).join(format!("{id}.json")))
}
fn save(paths: &Paths, r: &mut Record) -> Result<()> {
    r.updated_at = crate::daemon::now();
    private_dir(&dir(paths))?;
    atomic_write(&file(paths, &r.id)?, &serde_json::to_vec_pretty(r)?)
}
pub fn exists(paths: &Paths, id: &str) -> bool {
    file(paths, id).is_ok_and(|f| f.is_file())
}
pub fn read(paths: &Paths, id: &str) -> Result<Record> {
    let bytes = fs::read(file(paths, id)?).context("OPERATION_NOT_FOUND")?;
    serde_json::from_slice(&bytes).context("UNREADABLE_OPERATION_RECORD")
}
pub fn list(paths: &Paths) -> Result<Vec<Record>> {
    let mut all = vec![];
    let Ok(entries) = fs::read_dir(dir(paths)) else {
        return Ok(all);
    };
    for e in entries.flatten() {
        let name = e.file_name();
        let Some(id) = name.to_str().and_then(|n| n.strip_suffix(".json")) else {
            continue;
        };
        if let Ok(r) = read(paths, id) {
            all.push(r);
        }
    }
    all.sort_by_key(|r| (r.created_at, r.id.clone()));
    Ok(all)
}
pub fn is_terminal(r: &Record) -> bool {
    PHASES_TERMINAL.contains(&r.phase.as_str())
}
/// A rename that has neither finished nor been abandoned and involves this
/// account (as its old or its new name).
pub fn unresolved_for(paths: &Paths, provider: ProviderId, name: &str) -> Result<Option<Record>> {
    Ok(list(paths)?.into_iter().find(|r| {
        !is_terminal(r) && r.provider == provider && (r.old_name == name || r.new_name == name)
    }))
}
/// Operation row for `api operations`.
pub fn public(r: &Record) -> Value {
    json!({"id":r.id,"kind":"account_rename","phase":r.phase,"detail":r.detail,
    "account_id":r.account_id,"new_account_id":r.new_account_id,
    "uncertain":r.phase == "uncertain","terminal":is_terminal(r),
    "created_at":r.created_at,"updated_at":r.updated_at,
    "replays":false,
    "next":match r.phase.as_str() {
        "uncertain" => "api recover <operation> to reconcile by evidence, or api abandon-operation <operation> --confirm <operation>",
        "complete" | "failed" | "abandoned" => "none",
        _ => "in progress (reconciled at supervisor start or by api recover)",
    }})
}

/// Stable fingerprint of the identity facts a rename depends on. Clients pass
/// the value from the account row back as `expected_generation`.
pub fn generation(
    provider: &str,
    name: &str,
    display_name: Option<&str>,
    config_home: Option<&str>,
) -> String {
    let digest = Sha256::digest(
        serde_json::to_vec(&json!([provider, name, display_name, config_home]))
            .expect("serializable"),
    );
    format!("{digest:x}")[..16].to_owned()
}
pub fn account_generation(a: &Account) -> String {
    generation(
        a.provider.as_str(),
        &a.name,
        a.display_name.as_deref(),
        a.config_home.as_deref().and_then(Path::to_str),
    )
}

/// Validated request (no side effects).
#[derive(Debug)]
pub struct Plan {
    pub provider: ProviderId,
    pub old_name: String,
    pub new_name: String,
    pub old_home: PathBuf,
    pub new_home: PathBuf,
    pub display_name_before: Option<String>,
}

/// Validate a rename request against the registry and the disk.
/// `new_name` of `None` renames the backend to match the existing display name
/// (the "Update backend" case of a mismatched account).
pub fn plan(
    paths: &Paths,
    state: &State,
    provider: ProviderId,
    name: &str,
    new_name: Option<&str>,
    expected_generation: &str,
) -> Result<Plan> {
    let account = state.resolve_account(name, Some(provider))?;
    ensure!(
        account.deletion_id.is_none(),
        "ACCOUNT_DELETION_IN_PROGRESS"
    );
    ensure!(
        !accounts::removed(paths, provider, name)?,
        "ACCOUNT_REMOVED: restore the local membership before renaming this account"
    );
    ensure!(
        account.config_home.is_none(),
        "ACCOUNT_HOME_ADOPTED: this account uses an adopted config directory that sessionsation does not move; rename the display name only, or run `account managed-home` first"
    );
    ensure!(
        account_generation(account) == expected_generation,
        "STALE_TARGET: the account changed since it was read; refresh the account list"
    );
    if let Some(open) = unresolved_for(paths, provider, name)? {
        bail!(
            "ACCOUNT_RENAME_UNRESOLVED: operation {} ({}) has not finished; recover or abandon it first",
            open.id,
            open.phase
        );
    }
    let new_name = match new_name.map(str::trim).filter(|n| !n.is_empty()) {
        Some(n) => n.to_owned(),
        None => account.display_name.clone().context(
            "INVALID_ARGUMENT: no new name given and the account has no display name to match",
        )?,
    };
    validate_new_account_name(&new_name)?;
    ensure!(
        new_name != account.name,
        "ACCOUNT_NAME_UNCHANGED: the backend name is already '{new_name}'"
    );
    if state.account(provider, &new_name).is_some() {
        bail!(
            "ACCOUNT_NAME_EXISTS: {provider}:{new_name} already exists; names are unique per provider"
        );
    }
    if let Some(owner) = state.accounts.values().find(|a| {
        a.provider == provider
            && a.name != account.name
            && a.display_name.as_deref() == Some(&new_name)
    }) {
        bail!(
            "ACCOUNT_NAME_EXISTS: '{new_name}' is the display name of {}:{}; names and display names are unique per provider",
            owner.provider,
            owner.name
        );
    }
    if unresolved_for(paths, provider, &new_name)?.is_some() {
        bail!(
            "ACCOUNT_RENAME_UNRESOLVED: a rename involving {provider}:{new_name} has not finished"
        );
    }
    let old_home = crate::layout::account_home(&paths.root, provider, &account.name);
    let new_home = crate::layout::target_home(&paths.root, provider, &new_name);
    let legacy_new = crate::layout::legacy_home(&paths.root, provider, &new_name);
    ensure!(
        fs::symlink_metadata(&new_home).is_err()
            && legacy_new.is_none_or(|l| fs::symlink_metadata(l).is_err()),
        "ACCOUNT_HOME_EXISTS: a directory for {provider}:{new_name} already exists on disk"
    );
    Ok(Plan {
        provider,
        old_name: account.name.clone(),
        new_name,
        old_home,
        new_home,
        display_name_before: account.display_name.clone(),
    })
}

/// Refuse while any session or provider process uses the account (running or
/// launching). Callers hold the account operation lock.
pub async fn ensure_idle(paths: &Paths, plan: &Plan) -> Result<()> {
    let busy = |detail: String| anyhow::anyhow!("ACCOUNT_SESSIONS_RUNNING: {detail}");
    if plan.old_home.exists() {
        if plan.provider == ProviderId::Codex {
            if let Ok(lease) = fs::OpenOptions::new()
                .read(true)
                .write(true)
                .open(plan.old_home.join(".switcher-lease"))
            {
                use fs2::FileExt;
                if lease.try_lock_exclusive().is_err() {
                    return Err(busy("ACCOUNT_LEASE_HELD".into()));
                }
                let _ = FileExt::unlock(&lease);
            }
            crate::account_deletion::idle(paths, &plan.old_home)
                .await
                .map_err(|e| busy(format!("{e}")))?;
        } else {
            crate::account_deletion::native_idle(
                paths,
                plan.provider,
                &plan.old_name,
                &plan.old_home,
            )
            .map_err(|e| busy(format!("{e}")))?;
        }
    }
    // A session recorded as attached to a process that may still run is running,
    // even when the provider's own registry cannot see it.
    for s in crate::catalog::list(paths)? {
        if s.provider == plan.provider.as_str()
            && s.account == plan.old_name
            && let Some(a) = &s.attachment
            && !crate::external::attachment_definitely_gone(a.pid, a.process_start)
        {
            return Err(busy(format!("session {} may still be running", s.id)));
        }
    }
    Ok(())
}

/// Stop the account's idle Codex app-server so its home can move (it is started
/// again at the new home by the next `account/prepare`).
pub async fn stop_codex_server(config: &crate::config::Config, home: &Path) -> Result<()> {
    if !home.exists() {
        return Ok(());
    }
    if crate::control::available(home) {
        let out = tokio::time::timeout(
            std::time::Duration::from_secs(15),
            tokio::process::Command::new(&config.codex_binary)
                .args(["app-server", "daemon", "stop"])
                .env("CODEX_HOME", home)
                .env_remove("OPENAI_API_KEY")
                .env_remove("CODEX_API_KEY")
                .env_remove("CODEX_ACCESS_TOKEN")
                .stdin(std::process::Stdio::null())
                .output(),
        )
        .await
        .context("ACCOUNT_SERVER_STOP_UNCERTAIN")??;
        ensure!(out.status.success(), "ACCOUNT_SERVER_STOP_FAILED");
        ensure!(
            !crate::control::available(home),
            "ACCOUNT_SERVER_STILL_RUNNING"
        );
    }
    ensure!(
        !crate::external::home_has_processes(home, false)?,
        "ACCOUNT_SESSIONS_RUNNING: ACCOUNT_HAS_RUNNING_PROCESSES"
    );
    Ok(())
}

/// Move the account in the registry: key, name, display name, policies. Pure.
fn rekey(state: &mut State, r: &Record) -> Result<()> {
    let old_key = crate::model::account_key(r.provider, &r.old_name);
    let mut account = state
        .accounts
        .remove(&old_key)
        .context("ACCOUNT_NOT_FOUND")?;
    account.name = r.new_name.clone();
    if account.display_name.as_deref() == Some(r.new_name.as_str()) {
        account.display_name = None;
    }
    if r.provider == ProviderId::Codex {
        for p in state.automatic_switching.values_mut() {
            for n in p.accounts.iter_mut().filter(|n| **n == r.old_name) {
                *n = r.new_name.clone();
            }
        }
    }
    state.insert_account(account);
    Ok(())
}

/// Idempotent updates of the other backend-owned references.
fn update_references(paths: &Paths, store: &mut Store, r: &Record) -> Result<()> {
    crate::catalog::reassign_account(paths, r.provider.as_str(), &r.old_name, &r.new_name)?;
    if r.provider == ProviderId::Gemini {
        crate::provider::gemini::auth_failure::rename(paths, &r.old_name, &r.new_name);
    }
    let (old_key, new_key) = (
        crate::model::account_key(r.provider, &r.old_name),
        crate::model::account_key(r.provider, &r.new_name),
    );
    for table in ["usage_history", "activity_history"] {
        store.db.execute(
            &format!("UPDATE OR IGNORE {table} SET account=?1 WHERE account=?2"),
            [&new_key, &old_key],
        )?;
    }
    Ok(())
}

fn phase(paths: &Paths, r: &mut Record, p: &str, detail: &str) -> Result<()> {
    r.phase = p.into();
    r.detail = detail.into();
    save(paths, r)
}

/// Run an admitted rename. `hook` is called with the name of each completed
/// step (a failing hook models a crash there: nothing after it runs and no
/// cleanup happens). The caller holds the operation locks and has already
/// stopped the account's helpers.
pub fn execute(
    paths: &Paths,
    state: &mut State,
    store: &mut Store,
    plan: Plan,
    request_id: &str,
    hook: &dyn Fn(&str) -> Result<()>,
) -> Result<Value> {
    let now = crate::daemon::now();
    let mut r = Record {
        schema_version: 1,
        id: request_id.to_owned(),
        kind: "rename_backend".into(),
        provider: plan.provider,
        account_id: format!("{}:{}", plan.provider, plan.old_name),
        new_account_id: format!("{}:{}", plan.provider, plan.new_name),
        old_name: plan.old_name,
        new_name: plan.new_name,
        old_home: plan.old_home,
        new_home: plan.new_home,
        display_name_before: plan.display_name_before,
        phase: "admitted".into(),
        detail: "Admitted; no change made yet.".into(),
        created_at: now,
        updated_at: now,
    };
    ensure!(!exists(paths, request_id), "OPERATION_ALREADY_EXISTS");
    save(paths, &mut r)?;
    hook("admitted")?;
    phase(paths, &mut r, "moving_home", "Moving the account home.")?;
    hook("moving_home")?;
    let moved = (|| -> Result<()> {
        let meta = fs::symlink_metadata(&r.old_home).context("ACCOUNT_HOME_UNAVAILABLE")?;
        ensure!(
            meta.is_dir() && !meta.file_type().is_symlink(),
            "UNSAFE_ACCOUNT_DATA"
        );
        crate::layout::ensure_home_parent(&paths.root, r.provider)?;
        ensure!(
            fs::symlink_metadata(&r.new_home).is_err(),
            "ACCOUNT_HOME_EXISTS"
        );
        fs::rename(&r.old_home, &r.new_home).context("ACCOUNT_HOME_MOVE_FAILED")?;
        if let Some(parent) = r.new_home.parent() {
            fs::File::open(parent)?.sync_all()?;
        }
        Ok(())
    })();
    if let Err(e) = moved {
        let detail = format!("{e:#}. Nothing was changed.");
        phase(paths, &mut r, "failed", &detail)?;
        return Err(e);
    }
    phase(
        paths,
        &mut r,
        "home_moved",
        "Home moved; updating the registry.",
    )?;
    hook("home_moved")?;
    let before = state.clone();
    let updated = rekey(state, &r).and_then(|()| {
        store.save(
            state,
            crate::daemon::now(),
            "account.renamed_backend",
            &r.new_account_id,
        )
    });
    if let Err(e) = updated {
        *state = before;
        // The registry still names the old home: put the folder back.
        return Err(match fs::rename(&r.new_home, &r.old_home) {
            Ok(()) => {
                let detail = format!("{e:#}. The home was moved back; nothing changed.");
                phase(paths, &mut r, "failed", &detail)?;
                e
            }
            Err(back) => {
                let detail = format!(
                    "{e:#}. The home is at {} and could not be moved back ({back}). Run api recover to roll forward.",
                    r.new_home.display()
                );
                phase(paths, &mut r, "uncertain", &detail)?;
                e
            }
        });
    }
    phase(
        paths,
        &mut r,
        "registry_updated",
        "Registry updated; updating references.",
    )?;
    hook("registry_updated")?;
    finish_references(paths, store, &mut r)?;
    Ok(public(&r))
}

fn finish_references(paths: &Paths, store: &mut Store, r: &mut Record) -> Result<()> {
    if let Err(e) = update_references(paths, store, r) {
        let detail = format!(
            "{e:#}. The home and registry are renamed; references are partially updated. Run api recover."
        );
        phase(paths, r, "uncertain", &detail)?;
        return Err(e);
    }
    phase(
        paths,
        r,
        "complete",
        "Renamed. Old receipts and digests keep the old id.",
    )
}

/// Resolve an unfinished rename from evidence: where the home is and what the
/// registry says. Deterministic; changes nothing it cannot prove.
pub fn reconcile(
    paths: &Paths,
    state: &mut State,
    store: &mut Store,
    id: &str,
    hook: &dyn Fn(&str) -> Result<()>,
) -> Result<Value> {
    let mut r = read(paths, id)?;
    if is_terminal(&r) {
        return Ok(public(&r));
    }
    let exists_at = |p: &Path| fs::symlink_metadata(p).is_ok_and(|m| m.is_dir());
    let (old_home, new_home) = (exists_at(&r.old_home), exists_at(&r.new_home));
    let registry_old = state.account(r.provider, &r.old_name).is_some();
    let registry_new = state.account(r.provider, &r.new_name).is_some();
    match (old_home, new_home, registry_old, registry_new) {
        // Nothing moved (a crash before the rename, or a failed one).
        (true, false, true, false) => phase(
            paths,
            &mut r,
            "failed",
            "Reconciled: the home was never moved; nothing changed.",
        )?,
        // Home moved, registry still old: roll forward.
        (false, true, true, false) => {
            rekey(state, &r)?;
            store.save(
                state,
                crate::daemon::now(),
                "account.renamed_backend",
                &r.new_account_id,
            )?;
            phase(
                paths,
                &mut r,
                "registry_updated",
                "Reconciled: registry rolled forward to the moved home.",
            )?;
            hook("registry_updated")?;
            finish_references(paths, store, &mut r)?;
        }
        // Registry already updated: only bookkeeping may be missing.
        (false, true, false, true) => finish_references(paths, store, &mut r)?,
        _ => phase(
            paths,
            &mut r,
            "uncertain",
            "The home and the registry disagree in a way that cannot be resolved automatically. Inspect both, then abandon this operation.",
        )?,
    }
    Ok(public(&r))
}

/// Reconcile every unfinished rename (supervisor start). Never fails the caller.
pub fn reconcile_all(paths: &Paths, state: &mut State, store: &mut Store) {
    let Ok(all) = list(paths) else { return };
    for r in all
        .into_iter()
        .filter(|r| !is_terminal(r) && r.phase != "uncertain")
    {
        if let Err(e) = reconcile(paths, state, store, &r.id, &|_| Ok(())) {
            eprintln!("account rename {} not reconciled: {e:#}", r.id);
        }
    }
}

/// Operator abandon of an `uncertain` rename (terminal; changes no account data).
pub fn abandon(paths: &Paths, id: &str, confirm: &str) -> Result<Value> {
    ensure!(id == confirm, "CONFIRMATION_MISMATCH");
    let mut r = read(paths, id)?;
    ensure!(!is_terminal(&r), "OPERATION_ALREADY_TERMINAL");
    ensure!(r.phase == "uncertain", "OPERATION_NOT_STUCK");
    let from = r.phase.clone();
    phase(
        paths,
        &mut r,
        "abandoned",
        &format!(
            "Abandoned by the operator from '{from}'. No account data was changed by this step."
        ),
    )?;
    Ok(public(&r))
}

#[cfg(test)]
mod tests;
