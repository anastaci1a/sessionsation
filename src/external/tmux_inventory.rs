//! Plain/ended tmux panes are visible without granting Codex management powers.
use super::*;

pub async fn menu(paths: &Paths) -> Result<Vec<Instance>> {
    let mut items = discover(paths).await?;
    supplement(
        paths,
        &mut items,
        std::env::var("TMUX_PANE").ok().as_deref(),
    )?;
    Ok(items)
}
pub async fn snapshot_with_tmux(paths: &Paths, all: bool) -> Result<Value> {
    let mut items = discover_filtered(paths, all).await?;
    supplement(paths, &mut items, None)?;
    Ok(snapshot(items))
}
fn supplement(paths: &Paths, items: &mut Vec<Instance>, exclude: Option<&str>) -> Result<()> {
    let output = Command::new("tmux").args(["list-panes", "-a", "-F", "#{session_name}\t#{pane_id}\t#{pane_pid}\t#{pane_dead}\t#{pane_current_path}\t#{pane_current_command}"]).output()?;
    if !output.status.success() {
        return Ok(()); // No tmux server: saved conversations still work.
    }
    for line in String::from_utf8(output.stdout)?.lines() {
        let f: Vec<_> = line.split('\t').collect();
        if f.len() != 6 || exclude == Some(f[1]) || items.iter().any(|i| i.pane == f[1]) {
            continue;
        }
        let dead = f[3] == "1";
        items.push(Instance {
            tmux_only: true, tmux_dead: dead, display_name: f[0].into(), last_accessed: None,
            archived: false, inactive: false, account: String::new(), display_account: None,
            block_reason: None, activity: Activity::Unknown,
            detail: if dead { "This tmux pane has exited and retains its output. Open it to inspect diagnostics. It has no running Codex instance; saved conversation history is separate." } else { "An ordinary tmux pane; no Codex process was detected. You can open it, but account switching and conversation actions do not apply." }.into(),
            warnings: vec![], shared_server: false, move_id: None, pane: f[1].into(),
            tmux_session: f[0].into(), pid: f[2].parse().unwrap_or(0), start_time: 0,
            cwd: f[4].into(), thread_id: String::new(), rollout: PathBuf::new(), home: PathBuf::new(),
            binary: f[5].into(), resume_options: vec![],
        });
    }
    access::sort(paths, items)
}
pub fn open_plain(paths: &Paths, key: &str) -> Result<Instance> {
    let mut items = instances()?;
    supplement(paths, &mut items, None)?;
    items
        .into_iter()
        .find(|s| s.tmux_only && s.key() == key)
        .context("this pane changed or is now running Codex; refresh Sessions")
}

/// Reclaim only names held by entirely exited sessions. Keep their diagnostics.
pub(super) fn release_ended_name(name: &str) -> Result<()> {
    let output = Command::new("tmux")
        .args([
            "list-panes",
            "-s",
            "-t",
            &format!("={name}"),
            "-F",
            "#{session_id}\t#{pane_dead}",
        ])
        .output()?;
    if !output.status.success() {
        return Ok(());
    }
    let text = String::from_utf8(output.stdout)?;
    let rows: Vec<_> = text.lines().filter_map(|l| l.split_once('\t')).collect();
    ensure!(
        !rows.is_empty() && rows.iter().all(|(_, dead)| *dead == "1"),
        "{name} is used by another running tmux session. It is shown in Sessions; choose a different name or rename that session first"
    );
    let id = rows[0].0;
    for suffix in 0..100 {
        let retained = format!(
            "ended-{}-{}{}",
            name.trim_start_matches("codex-"),
            id.trim_start_matches('$'),
            if suffix == 0 {
                String::new()
            } else {
                format!("-{suffix}")
            }
        );
        if !Command::new("tmux")
            .args(["has-session", "-t", &format!("={retained}")])
            .output()?
            .status
            .success()
        {
            tmux(&["rename-session", "-t", id, &retained])?;
            return Ok(());
        }
    }
    bail!("could not retain the exited session under a free name")
}
