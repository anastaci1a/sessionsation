//! Access order follows the conversation across panes/accounts; monitoring never bumps it.
use super::*;
use std::collections::BTreeMap;

fn read(paths: &Paths) -> Result<BTreeMap<String, i64>> {
    let path = paths.root.join(crate::layout::SESSION_ACCESS);
    if !path.exists() {
        return Ok(BTreeMap::new());
    }
    Ok(serde_json::from_slice(&fs::read(path)?)?)
}
pub fn record(paths: &Paths, thread: &str) -> Result<()> {
    uuid::Uuid::parse_str(thread).context("invalid conversation identity")?;
    let lock = crate::paths::lock_file(&paths.root.join(crate::layout::SESSION_ACCESS_LOCK))?;
    lock.lock_exclusive()?;
    let mut access = read(paths)?;
    let now = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)?
        .as_millis() as i64;
    let previous = access.values().copied().max().unwrap_or(0);
    access.insert(thread.into(), now.max(previous.saturating_add(1)));
    atomic_write(
        &paths.root.join(crate::layout::SESSION_ACCESS),
        &serde_json::to_vec(&access)?,
    )
}
pub(super) fn sort(paths: &Paths, items: &mut [Instance]) -> Result<()> {
    let mut access = read(paths)?;
    let attached: BTreeMap<String, i64> = tmux(&[
        "list-sessions",
        "-F",
        "#{session_name}\t#{session_last_attached}",
    ])
    .unwrap_or_default()
    .lines()
    .filter_map(|line| {
        let (name, value) = line.split_once('\t')?;
        Some((name.into(), value.parse::<i64>().ok()?.saturating_mul(1000)))
    })
    .collect();
    // Persist real tmux attachment timestamps before a pane disappears. Discovery
    // never substitutes its own current time for a user's open/attach event.
    let observed: BTreeMap<String, i64> = items
        .iter()
        .filter_map(|i| {
            if i.inactive || i.tmux_dead || uuid::Uuid::parse_str(&i.thread_id).is_err() {
                return None;
            }
            let at = *attached.get(&i.tmux_session)?;
            (at > 0 && at > access.get(&i.thread_id).copied().unwrap_or(0))
                .then(|| (i.thread_id.clone(), at))
        })
        .fold(BTreeMap::new(), |mut result, (id, at)| {
            let value = result.entry(id).or_insert(at);
            *value = (*value).max(at);
            result
        });
    if !observed.is_empty() {
        let lock = crate::paths::lock_file(&paths.root.join(crate::layout::SESSION_ACCESS_LOCK))?;
        lock.lock_exclusive()?;
        access = read(paths)?;
        for (id, at) in observed {
            let value = access.entry(id).or_insert(at);
            *value = (*value).max(at);
        }
        atomic_write(
            &paths.root.join(crate::layout::SESSION_ACCESS),
            &serde_json::to_vec(&access)?,
        )?;
    }
    for i in items.iter_mut() {
        i.last_accessed = access
            .get(&i.thread_id)
            .copied()
            .into_iter()
            .chain(attached.get(&i.tmux_session).copied())
            .max()
            .filter(|n| *n > 0);
    }
    // No fabricated "accessed" date for older saved conversations. Their history
    // modification time only breaks ties when no access record exists.
    items.sort_by_cached_key(|i| {
        (
            if i.inactive || i.tmux_dead || i.pane.is_empty() {
                2
            } else if i.activity == Activity::Busy {
                0
            } else {
                1
            },
            std::cmp::Reverse(i.last_accessed),
            std::cmp::Reverse(fs::metadata(&i.rollout).and_then(|m| m.modified()).ok()),
            i.key(),
        )
    });
    Ok(())
}
