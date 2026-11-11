//! HERDR owns live tab labels. The catalog retains the last observation for history.
use super::*;
use crate::{
    external,
    launch::{Host, host_call},
};
use serde_json::Value;
use std::time::Duration;

fn lock(paths: &Paths) -> Result<fs::File> {
    let dir = paths.root.join(crate::layout::SESSION_CATALOG);
    private_dir(&dir)?;
    let lock = lock_file(&dir.join("names.lock"))?;
    lock.try_lock_exclusive().context("NAME_SYNC_BUSY")?;
    Ok(lock)
}
fn host(paths: &Paths, s: &Session) -> Result<Host> {
    let a = s.attachment.as_ref().context("NO_LIVE_ATTACHMENT")?;
    ensure!(a.host == "herdr", "SESSION_NOT_IN_HERDR");
    ensure!(
        !external::attachment_definitely_gone(a.pid, a.process_start),
        "STALE_TARGET"
    );
    let (server, _) = crate::terminal::target(&a.target)?;
    Ok(Host {
        session: server.into(),
        socket: match external::hosting::socket_for(paths, &a.target)? {
            Some(socket) => socket,
            None => external::hosting::live_process_socket(a.pid, a.process_start)
                .context("HOST_SOCKET_UNAVAILABLE")?,
        },
    })
}
async fn snapshot(host: &Host) -> Result<Value> {
    // One bounded snapshot per server, never per session/provider history read.
    Ok(tokio::time::timeout(
        Duration::from_secs(1),
        host_call(host, &["api", "snapshot"]),
    )
    .await
    .context("HOST_NAME_UNOBSERVED")??["snapshot"]
        .clone())
}
/// One pane's host presentation in a snapshot.
#[derive(Debug, PartialEq)]
struct Observed<'a> {
    tab_id: &'a str,
    /// The tab label as HERDR shows it.
    label: &'a str,
    /// Label of the pane's workspace (Space), when the snapshot lists it.
    space: Option<&'a str>,
    /// The pane's own label (`pane rename`), when set.
    pane_label: Option<&'a str>,
}
fn tab<'a>(snapshot: &'a Value, target: &str) -> Result<Observed<'a>> {
    let (_, pane_id) = crate::terminal::target(target)?;
    let panes = snapshot["panes"]
        .as_array()
        .context("HOST_NAME_UNOBSERVED")?;
    let mut matches = panes.iter().filter(|p| p["pane_id"] == pane_id);
    let pane = matches.next().context("HOST_PANE_UNOBSERVED")?;
    ensure!(matches.next().is_none(), "AMBIGUOUS_HOST_PANE");
    let id = pane["tab_id"].as_str().context("HOST_TAB_UNOBSERVED")?;
    let tabs = snapshot["tabs"]
        .as_array()
        .context("HOST_NAME_UNOBSERVED")?;
    let mut matches = tabs.iter().filter(|t| t["tab_id"] == id);
    let tab = matches.next().context("HOST_TAB_UNOBSERVED")?;
    ensure!(matches.next().is_none(), "AMBIGUOUS_HOST_TAB");
    let label = tab["label"].as_str().context("HOST_NAME_UNOBSERVED")?;
    ensure!(
        !label.is_empty() && !label.chars().any(char::is_control),
        "INVALID_HOST_NAME"
    );
    let workspace = pane["workspace_id"]
        .as_str()
        .or_else(|| tab["workspace_id"].as_str());
    let space = workspace.and_then(|w| {
        let mut found = snapshot["workspaces"]
            .as_array()?
            .iter()
            .filter(|s| s["workspace_id"] == w);
        let one = found.next()?;
        found.next().is_none().then_some(())?;
        one["label"]
            .as_str()
            .filter(|l| !l.is_empty() && !l.chars().any(char::is_control))
    });
    let pane_label = pane["label"].as_str().filter(|l| !l.is_empty());
    Ok(Observed {
        tab_id: id,
        label,
        space,
        pane_label,
    })
}
/// The bare session name a tab label stands for (§6.1 of the 1.48 design): a label
/// without `:` as is; `<space>:<name>` whose prefix is the pane's own Space label
/// (case-insensitive) is its suffix; any other label with `:` is `INVALID_HOST_NAME`.
fn host_name<'a>(label: &'a str, space: Option<&str>) -> Result<&'a str> {
    match label.split_once(':') {
        None => Ok(label),
        Some((prefix, name))
            if !name.is_empty()
                && !name.contains(':')
                && name.trim() == name
                && space.is_some_and(|s| scope::fold(s) == scope::fold(prefix)) =>
        {
            Ok(name)
        }
        Some(_) => anyhow::bail!("INVALID_HOST_NAME"),
    }
}
/// One observation: the host name (None when the label is not a valid name) and
/// the observed Space label (None when unobserved).
type Observation = (Session, Option<String>, Option<String>);
fn observation(s: &Session, o: &Observed<'_>) -> Observation {
    (
        s.clone(),
        host_name(o.label, o.space).ok().map(str::to_owned),
        o.space.map(str::to_owned),
    )
}
fn apply(paths: &Paths, observations: Vec<Observation>) -> Result<()> {
    transaction(paths, |c| {
        for (before, name, space) in observations {
            let Some(current) = c.sessions.get_mut(&before.id) else {
                continue;
            };
            if current.generation() != before.generation() {
                continue;
            }
            // Host labels are presentation, not unique identities. Never invent a
            // suffix or truncate a host-owned label, even for split-pane sessions.
            if let Some(name) = name
                && current.name != name
            {
                current.name = name;
                current.revision += 1;
            }
            // HERDR owns a live session's Space too. Recording a Space where none
            // was known is not a change (no revision, like the 1.48 backfill); a
            // different Space is a move and changes the generation.
            if let Some(space) = space
                && current.space.as_deref() != Some(space.as_str())
            {
                if current.space.is_none() {
                    current.space = Some(space);
                    c.space_notes.insert(current.id.clone(), "host_observed");
                } else {
                    scope::assign(
                        current,
                        &mut c.space_notes,
                        Some(space),
                        "host_observed",
                        true,
                    );
                }
            }
        }
        Ok(())
    })
}
/// Whether a HERDR-attached session is verified live. Codex sessions match a
/// discovered instance; native-provider sessions (no Codex instance) require the exact
/// recorded process (PID and start time) of their verified attachment.
fn attached_live(s: &Session, instances: &[external::Instance]) -> bool {
    if crate::provider::native::of(&s.provider).is_some() {
        return s.attachment.as_ref().is_some_and(|a| {
            external::attachment_is_live(a.pid, a.process_start).unwrap_or(false)
        });
    }
    instances.iter().any(|i| crate::api::source_matches(s, i))
}
pub async fn refresh(paths: &Paths, instances: &[external::Instance]) -> Result<()> {
    let Ok(_lock) = lock(paths) else {
        return Ok(());
    };
    let mut snapshots = BTreeMap::new();
    let mut observations = vec![];
    let mut placements = vec![];
    for s in list(paths)? {
        let Some(a) = &s.attachment else {
            continue;
        };
        if a.host != "herdr" {
            continue;
        }
        let Ok(host) = host(paths, &s) else {
            continue;
        };
        if !attached_live(&s, instances) {
            placements.push((s.clone(), host, None));
            continue;
        }
        let key = (host.session.clone(), host.socket.clone());
        if !snapshots.contains_key(&key) {
            snapshots.insert(
                key.clone(),
                snapshot(&host)
                    .await
                    .ok()
                    .map(|value| (crate::daemon::now(), value)),
            );
        }
        placements.push((s.clone(), host, snapshots.get(&key).cloned().flatten()));
        if let Some(Some((_, value))) = snapshots.get(&key)
            && let Ok(observed) = tab(value, &a.target)
        {
            observations.push(observation(&s, &observed));
        }
    }
    apply(paths, observations)?;
    super::placement::record(paths, placements)
}
pub async fn rename(paths: &Paths, id: &str, generation: &str, name: &str) -> Result<Session> {
    Ok(rename_reported(paths, id, generation, name).await?.0)
}
/// `api rename`. `name` is bare or `<space>:<name>` of the session's own Space;
/// HERDR always gets the bare name. Also returns the pane-label step's outcome
/// (`None`: the pane has no label of the old name, nothing sent; `confirmed`;
/// `unconfirmed`: the catalog name and verified tab label stand regardless).
pub async fn rename_reported(
    paths: &Paths,
    id: &str,
    generation: &str,
    name: &str,
) -> Result<(Session, Option<&'static str>)> {
    let parsed = super::parse_name(name)?;
    let _lock = lock(paths)?;
    let s = resolve(paths, id, generation)?;
    if s.attachment.as_ref().is_none_or(|a| a.host != "herdr") {
        return Ok((super::rename(paths, id, generation, name)?, None));
    }
    scope::ensure_agrees(
        parsed,
        s.space.as_deref(),
        if s.space.is_none() {
            "the session is unscoped; use `api session set-space`"
        } else {
            "rename never moves; use `api move-space`"
        },
    )?;
    let name = parsed.name;
    if s.name == name {
        return Ok((s, None));
    }
    ensure!(
        !super::space_taken(paths, s.space.as_deref(), name, id)?,
        "SESSION_NAME_EXISTS: \"{name}\" is already used in this Space"
    );
    let _operation =
        external::messaging_lock(paths, &s.conversation_id).map_err(|e| anyhow::anyhow!(e))?;
    let host = host(paths, &s)?;
    let target = &s.attachment.as_ref().unwrap().target;
    let before = snapshot(&host).await?;
    let observed = tab(&before, target)?;
    let tab_id = observed.tab_id.to_owned();
    let old_pane_label = observed.pane_label.map(str::to_owned);
    let (_, pane_id) = crate::terminal::target(target)?;
    let process = tokio::time::timeout(
        Duration::from_secs(1),
        host_call(&host, &["pane", "process-info", "--pane", pane_id]),
    )
    .await
    .context("HOST_PROCESS_UNOBSERVED")??;
    let processes = process["process_info"]["foreground_processes"]
        .as_array()
        .context("HOST_PROCESS_UNOBSERVED")?;
    ensure!(
        processes.len() == 1
            && processes[0]["pid"].as_u64() == Some(s.attachment.as_ref().unwrap().pid as u64),
        "STALE_TARGET"
    );
    self::host(paths, &s)?;
    // The catalog remains only a cached label for live HERDR sessions. A tab may
    // contain several sessions; all acquire the host label on their next refresh.
    resolve(paths, id, generation)?;
    let result = tokio::time::timeout(
        Duration::from_secs(3),
        host_call(&host, &["tab", "rename", &tab_id, name]),
    )
    .await;
    let after = snapshot(&host)
        .await
        .context("HOST_RENAME_UNCONFIRMED: refresh session names before retrying")?;
    let observed = tab(&after, target)?;
    ensure!(observed.tab_id == tab_id, "STALE_TARGET");
    // Always retain authoritative host evidence, including a concurrent host edit.
    apply(paths, vec![observation(&s, &observed)])?;
    ensure!(
        host_name(observed.label, observed.space).is_ok_and(|n| n == name),
        "HOST_RENAME_NOT_APPLIED: refresh session names before retrying"
    );
    // A matching read-back reconciles a lost response; it never replays a mutation.
    drop(result);
    let current = list(paths)?
        .into_iter()
        .find(|v| v.id == id)
        .context("SESSION_NOT_FOUND")?;
    ensure!(
        current.attachment == s.attachment && current.name == name,
        "STALE_TARGET"
    );
    // A pane label showing the old name follows the rename (presentation only).
    let pane_label = match old_pane_label {
        Some(label) if scope::fold(&label) == scope::fold(&s.name) => {
            let _ = tokio::time::timeout(
                Duration::from_secs(3),
                host_call(&host, &["pane", "rename", pane_id, name]),
            )
            .await;
            let confirmed = snapshot(&host)
                .await
                .ok()
                .and_then(|v| tab(&v, target).ok().map(|o| o.pane_label == Some(name)))
                .unwrap_or(false);
            Some(if confirmed {
                "confirmed"
            } else {
                "unconfirmed"
            })
        }
        _ => None,
    };
    Ok((current, pane_label))
}
#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn tab_labels_with_the_pane_space_prefix_read_as_the_bare_name() {
        for (label, space, expected) in [
            ("LEAD", Some("localica"), Some("LEAD")),
            ("Lead", Some("localica"), Some("Lead")),
            ("localica:LEAD", Some("localica"), Some("LEAD")),
            ("Localica:LEAD", Some("localica"), Some("LEAD")),
            ("other:LEAD", Some("localica"), None),
            ("a:b:c", Some("a"), None),
            (":x", Some("localica"), None),
            ("x:", Some("x"), None),
            ("localica:LEAD", None, None),
        ] {
            assert_eq!(host_name(label, space).ok(), expected, "{label}");
        }
        let v = serde_json::json!({"panes":[{"pane_id":"w1:p1","tab_id":"w1:t1","workspace_id":"w1","label":"old"}],
            "tabs":[{"tab_id":"w1:t1","label":"localica:LEAD"}],"workspaces":[{"workspace_id":"w1","label":"localica"}]});
        let o = tab(&v, "herdr/test/w1:p1").unwrap();
        assert_eq!((o.space, o.pane_label), (Some("localica"), Some("old")));
    }
    #[test]
    fn host_observations_set_but_never_clear_a_space() {
        let tmp = tempfile::tempdir().unwrap();
        let p = Paths::new(Some(tmp.path().join("data"))).unwrap();
        let s = record_launch(&p, "fixture", "t1", "LEAD", None).unwrap();
        // First observation of a Space: recorded without a new generation.
        apply(
            &p,
            vec![(s.clone(), Some("LEAD".into()), Some("localica".into()))],
        )
        .unwrap();
        let seen = list(&p).unwrap().remove(0);
        assert_eq!(seen.canonical().as_deref(), Some("localica:LEAD"));
        assert_eq!(seen.generation(), s.generation());
        // An invalid host label keeps the name; an unobserved Space keeps the Space.
        apply(&p, vec![(seen.clone(), None, None)]).unwrap();
        let kept = list(&p).unwrap().remove(0);
        assert_eq!(kept.canonical().as_deref(), Some("localica:LEAD"));
        assert_eq!(kept.generation(), seen.generation());
        // A live session placed in another Space moved (host wins): new generation.
        apply(
            &p,
            vec![(
                kept.clone(),
                Some("LEAD".into()),
                Some("sessionstuffs".into()),
            )],
        )
        .unwrap();
        let moved = list(&p).unwrap().remove(0);
        assert_eq!(moved.canonical().as_deref(), Some("sessionstuffs:LEAD"));
        assert_ne!(moved.generation(), kept.generation());
        // A stale observation never undoes it.
        apply(
            &p,
            vec![(kept, Some("LEAD".into()), Some("localica".into()))],
        )
        .unwrap();
        assert_eq!(list(&p).unwrap()[0].space.as_deref(), Some("sessionstuffs"));
    }
    #[test]
    fn host_labels_are_identity_mapped_and_not_generated_from_titles() {
        let v = serde_json::json!({"panes":[{"pane_id":"w1:p1","tab_id":"w1:t2"}],"tabs":[{"tab_id":"w1:t1","label":"wrong"},{"tab_id":"w1:t2","label":"host name"}]});
        {
            let o = tab(&v, "herdr/test/w1:p1").unwrap();
            assert_eq!((o.tab_id, o.label, o.space), ("w1:t2", "host name", None));
        }
        assert!(tab(&v, "herdr/test/w1:p2").is_err());
    }
    #[test]
    fn host_duplicates_remain_labels_and_unchanged_reads_do_not_change_generation() {
        let tmp = tempfile::tempdir().unwrap();
        let p = Paths::new(Some(tmp.path().join("data"))).unwrap();
        let a = record_launch(
            &p,
            "fixture",
            &uuid::Uuid::new_v4().to_string(),
            "one",
            None,
        )
        .unwrap();
        let b = record_launch(
            &p,
            "fixture",
            &uuid::Uuid::new_v4().to_string(),
            "two",
            None,
        )
        .unwrap();
        apply(
            &p,
            vec![
                (a, Some("same".into()), None),
                (b, Some("same".into()), None),
            ],
        )
        .unwrap();
        let before = list(&p).unwrap();
        assert_ne!(before[0].id, before[1].id);
        assert!(before.iter().all(|s| s.name == "same"));
        assert_eq!(
            super::super::rename(&p, &before[0].id, &before[0].generation(), "same")
                .unwrap()
                .generation(),
            before[0].generation()
        );
        apply(
            &p,
            before
                .iter()
                .cloned()
                .map(|s| (s, Some("same".into()), None))
                .collect(),
        )
        .unwrap();
        assert_eq!(
            before.iter().map(Session::generation).collect::<Vec<_>>(),
            list(&p)
                .unwrap()
                .iter()
                .map(Session::generation)
                .collect::<Vec<_>>()
        );
    }
    #[test]
    fn observed_labels_survive_exit_but_stale_observations_never_undo_a_rename() {
        let tmp = tempfile::tempdir().unwrap();
        let p = Paths::new(Some(tmp.path().join("data"))).unwrap();
        let s = record_launch(
            &p,
            "fixture",
            &uuid::Uuid::new_v4().to_string(),
            "old",
            None,
        )
        .unwrap();
        apply(&p, vec![(s.clone(), Some("HERDR label".into()), None)]).unwrap();
        let current = list(&p).unwrap().remove(0);
        assert_eq!(current.id, s.id);
        assert_eq!(current.conversation_id, s.conversation_id);
        assert_eq!(current.name, "HERDR label");
        apply(&p, vec![(s, Some("stale".into()), None)]).unwrap();
        assert_eq!(list(&p).unwrap()[0].name, "HERDR label");
        assert_eq!(reconcile(&p, &[]).unwrap()[0].name, "HERDR label");
    }
    fn claude_session(p: &Paths, label: &str, attachment: Option<Attachment>) -> Session {
        record_provider_launch(
            p,
            ProviderId::Claude,
            "fixture",
            &uuid::Uuid::new_v4().to_string(),
            label,
            Some("/work"),
            attachment,
        )
        .unwrap()
    }
    #[test]
    fn claude_host_labels_use_the_codex_observation_rules() {
        let tmp = tempfile::tempdir().unwrap();
        let p = Paths::new(Some(tmp.path().join("data"))).unwrap();
        let a = claude_session(&p, "one", None);
        let b = record_launch(
            &p,
            "fixture",
            &uuid::Uuid::new_v4().to_string(),
            "two",
            None,
        )
        .unwrap();
        // Host labels are presentation: shared across providers, never suffixed.
        apply(
            &p,
            vec![
                (a.clone(), Some("same".into()), None),
                (b, Some("same".into()), None),
            ],
        )
        .unwrap();
        let before = list(&p).unwrap();
        assert!(before.iter().all(|s| s.name == "same"));
        apply(
            &p,
            before
                .iter()
                .cloned()
                .map(|s| (s, Some("same".into()), None))
                .collect(),
        )
        .unwrap();
        assert_eq!(
            before.iter().map(Session::generation).collect::<Vec<_>>(),
            list(&p)
                .unwrap()
                .iter()
                .map(Session::generation)
                .collect::<Vec<_>>(),
            "unchanged reads do not change generation"
        );
        let current = list(&p)
            .unwrap()
            .into_iter()
            .find(|s| s.id == a.id)
            .unwrap();
        apply(
            &p,
            vec![(current.clone(), Some("HERDR label".into()), None)],
        )
        .unwrap();
        let renamed = list(&p)
            .unwrap()
            .into_iter()
            .find(|s| s.id == a.id)
            .unwrap();
        assert_eq!(renamed.name, "HERDR label");
        assert_eq!(renamed.conversation_id, a.conversation_id);
        assert_eq!(renamed.directory.as_deref(), Some("/work"));
        apply(&p, vec![(current, Some("stale".into()), None)]).unwrap();
        let kept = list(&p)
            .unwrap()
            .into_iter()
            .find(|s| s.id == a.id)
            .unwrap();
        assert_eq!(
            kept.name, "HERDR label",
            "stale observations never undo a rename"
        );
    }
    #[test]
    fn claude_sessions_are_live_only_through_their_exact_verified_attachment() {
        let tmp = tempfile::tempdir().unwrap();
        let p = Paths::new(Some(tmp.path().join("data"))).unwrap();
        let pid = std::process::id();
        let (_, start) = crate::provider::claude::proc_start(pid).unwrap();
        let attach = |start| Attachment {
            host: "herdr".into(),
            target: "herdr/test/w1:p1".into(),
            pid,
            process_start: start,
        };
        let live = claude_session(&p, "live", Some(attach(start)));
        assert!(attached_live(&live, &[]));
        let reused = claude_session(&p, "reused", Some(attach(start + 1)));
        assert!(!attached_live(&reused, &[]), "PID reuse is not liveness");
        let detached = claude_session(&p, "detached", None);
        assert!(!attached_live(&detached, &[]));
        // Codex keeps requiring a discovered instance, never a bare PID.
        let codex = record_launch(
            &p,
            "fixture",
            &uuid::Uuid::new_v4().to_string(),
            "codex",
            Some(attach(start)),
        )
        .unwrap();
        assert!(!attached_live(&codex, &[]));
    }
    #[test]
    fn gemini_labels_and_liveness_follow_the_native_provider_rules() {
        let tmp = tempfile::tempdir().unwrap();
        let p = Paths::new(Some(tmp.path().join("data"))).unwrap();
        let pid = std::process::id();
        let (_, start) = crate::provider::claude::proc_start(pid).unwrap();
        let attach = |start| Attachment {
            host: "herdr".into(),
            target: "herdr/test/w1:p1".into(),
            pid,
            process_start: start,
        };
        let gemini = |label: &str, attachment| {
            record_provider_launch(
                &p,
                ProviderId::Gemini,
                "fixture",
                &uuid::Uuid::new_v4().to_string(),
                label,
                Some("/work"),
                attachment,
            )
            .unwrap()
        };
        let live = gemini("live", Some(attach(start)));
        assert!(attached_live(&live, &[]));
        assert!(!attached_live(
            &gemini("reused", Some(attach(start + 1))),
            &[]
        ));
        assert!(!attached_live(&gemini("detached", None), &[]));
        // A HERDR tab label becomes the catalog label (presentation only);
        // stale observations never undo it and identity is unchanged.
        apply(&p, vec![(live.clone(), Some("HERDR tab".into()), None)]).unwrap();
        let renamed = list(&p)
            .unwrap()
            .into_iter()
            .find(|s| s.id == live.id)
            .unwrap();
        assert_eq!(renamed.name, "HERDR tab");
        assert_eq!(renamed.conversation_id, live.conversation_id);
        assert_eq!(renamed.directory.as_deref(), Some("/work"));
        apply(&p, vec![(live.clone(), Some("stale".into()), None)]).unwrap();
        let kept = list(&p)
            .unwrap()
            .into_iter()
            .find(|s| s.id == live.id)
            .unwrap();
        assert_eq!(kept.name, "HERDR tab");
    }
}
