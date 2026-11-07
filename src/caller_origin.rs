//! Who is calling `api send`: contract 1.41.0 origin detection.
//!
//! A caller that omits `origin` is an agent when its own process tree contains the verified
//! process of a live sessionsation-managed session (catalog attachment pid and start time).
//! Operators (the HERDR UI, a plain shell) have no such ancestor and stay `operator`.

use crate::{catalog, paths::Paths};

/// How many parents are followed before giving up (guards against pid-reuse loops).
const MAX_HOPS: usize = 64;

/// `(origin, origin_source)`; the source is `explicit`, `detected` or `default`.
pub fn resolve(paths: &Paths, explicit: Option<&str>) -> (String, &'static str) {
    if let Some(origin) = explicit {
        return (origin.to_owned(), "explicit");
    }
    if inside_managed_session(paths) {
        ("agent".to_owned(), "detected")
    } else {
        ("operator".to_owned(), "default")
    }
}

fn inside_managed_session(paths: &Paths) -> bool {
    let Ok(sessions) = catalog::list(paths) else {
        return false;
    };
    let attachments: Vec<(u32, u64)> = sessions
        .iter()
        .filter_map(|s| s.attachment.as_ref())
        .map(|a| (a.pid, a.process_start))
        .collect();
    !attachments.is_empty() && chain_hits(std::process::id(), &attachments, proc_stat)
}

/// Walk the parents of `start` (inclusive) and report whether one is an attachment.
fn chain_hits(
    start: u32,
    attachments: &[(u32, u64)],
    stat: impl Fn(u32) -> Option<(u32, u64)>,
) -> bool {
    chain_match(start, attachments, stat).is_some()
}

/// The first attachment found walking up from `start`.
fn chain_match(
    start: u32,
    attachments: &[(u32, u64)],
    stat: impl Fn(u32) -> Option<(u32, u64)>,
) -> Option<(u32, u64)> {
    let mut pid = start;
    for _ in 0..MAX_HOPS {
        if pid <= 1 {
            return None;
        }
        let (parent, started) = stat(pid)?;
        if attachments.contains(&(pid, started)) {
            return Some((pid, started));
        }
        pid = parent;
    }
    None
}

/// Catalog name of the managed session this process runs under (a parent is its attachment);
/// `sessionsation` when called from outside any session (operator, supervisor, system).
pub fn sender_name(paths: &Paths) -> String {
    let found = catalog::list(paths).ok().and_then(|sessions| {
        let hit = chain_match(
            std::process::id(),
            &sessions
                .iter()
                .filter_map(|s| s.attachment.as_ref())
                .map(|a| (a.pid, a.process_start))
                .collect::<Vec<_>>(),
            proc_stat,
        )?;
        sessions
            .into_iter()
            .find(|s| {
                s.attachment
                    .as_ref()
                    .is_some_and(|a| (a.pid, a.process_start) == hit)
            })
            .map(|s| s.name)
    });
    found
        .filter(|n| !n.is_empty())
        .unwrap_or_else(|| "sessionsation".into())
}

/// `(ppid, start ticks)` from /proc.
fn proc_stat(pid: u32) -> Option<(u32, u64)> {
    let stat = std::fs::read_to_string(format!("/proc/{pid}/stat")).ok()?;
    let fields: Vec<&str> = stat.rsplit_once(") ")?.1.split_whitespace().collect();
    Some((fields.get(1)?.parse().ok()?, fields.get(19)?.parse().ok()?))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn walks_the_chain_and_checks_start_time() {
        let table = |pid: u32| match pid {
            50 => Some((40, 500)),
            40 => Some((30, 400)),
            30 => Some((1, 300)),
            _ => None,
        };
        assert!(chain_hits(50, &[(30, 300)], table));
        // pid reuse: same pid, different start time.
        assert!(!chain_hits(50, &[(30, 999)], table));
        assert!(!chain_hits(50, &[(77, 300)], table));
        assert!(chain_hits(40, &[(40, 400)], table));
        assert!(!chain_hits(999, &[(30, 300)], table));
    }

    #[test]
    fn a_parent_loop_terminates() {
        assert!(!chain_hits(5, &[(9, 9)], |p| Some((p % 2 + 2, 1))));
    }

    #[test]
    fn explicit_origin_wins() {
        let dir = tempfile::tempdir().unwrap();
        let paths = Paths::new(Some(dir.path().to_path_buf())).unwrap();
        assert_eq!(
            resolve(&paths, Some("operator")),
            ("operator".into(), "explicit")
        );
        assert_eq!(resolve(&paths, None), ("operator".into(), "default"));
    }
}

#[cfg(test)]
mod catalog_tests {
    use super::*;
    use crate::{catalog::Attachment, provider::ProviderId};

    fn register(paths: &Paths, pid: u32, start: u64) {
        catalog::record_provider_launch(
            paths,
            ProviderId::Claude,
            "personal",
            &uuid::Uuid::new_v4().to_string(),
            "managed",
            None,
            Some(Attachment {
                host: "herdr/x".into(),
                target: "herdr/x/w:p".into(),
                pid,
                process_start: start,
            }),
        )
        .unwrap();
    }

    #[test]
    fn a_caller_inside_a_managed_session_is_an_agent() {
        let dir = tempfile::tempdir().unwrap();
        let paths = Paths::new(Some(dir.path().join("state"))).unwrap();
        let me = std::process::id();
        let (_, start) = proc_stat(me).unwrap();
        // No attachments: a plain shell.
        assert_eq!(resolve(&paths, None), ("operator".into(), "default"));
        // An unrelated live process is not an ancestor.
        register(&paths, 1, 1);
        assert_eq!(resolve(&paths, None), ("operator".into(), "default"));
        // A recycled pid (same pid, other start time) does not count.
        register(&paths, me, start + 1);
        assert_eq!(resolve(&paths, None), ("operator".into(), "default"));
        // This process (and so everything it runs) is inside the managed session.
        register(&paths, me, start);
        assert_eq!(resolve(&paths, None), ("agent".into(), "detected"));
        // An explicit origin always wins.
        assert_eq!(
            resolve(&paths, Some("operator")),
            ("operator".into(), "explicit")
        );
        assert_eq!(resolve(&paths, Some("agent")), ("agent".into(), "explicit"));
    }
}
