use super::super::*;
use crate::provider::claude;

/// Read-only probe of one real Claude home: `SESSIONSATION_PROBE_HOME=<home> cargo test
/// native_activity::live_probe -- --ignored --nocapture`. Prints state, source and
/// evidence time per running session; never message text.
#[test]
#[ignore]
fn print_live_claude_activity() {
    let Some(home) = std::env::var_os("SESSIONSATION_PROBE_HOME").map(PathBuf::from) else {
        return;
    };
    let now = crate::daemon::now();
    let live = claude::probe(&home).unwrap_or_default();
    for l in &live {
        let o = claude::observe_in(&home, &l.session_id, None, &Ok(live.clone()));
        let out = facts(ProviderId::Claude, &home, &o, now * 1000);
        let shown = out.map(|(f, at)| {
            (
                live_activity::public(Some(&f), at / 1000),
                activity_label(&f),
            )
        });
        println!("pid {} -> {:?}", l.pid, shown);
    }
}
