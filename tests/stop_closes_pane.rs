//! Stop closes the HERDR pane (contract 1.36.0): the safety rules, against the fake HERDR.
use sessionsation::{external::hosting::close_pane, paths::Paths};
use std::{os::unix::fs::PermissionsExt, path::Path};

fn calls(dir: &Path) -> String {
    std::fs::read_to_string(dir.join("calls")).unwrap_or_default()
}

// One test: it sets PATH and FAKE_HERDR_DIR for the process.
#[test]
fn only_an_idle_recorded_herdr_pane_is_closed() {
    let tmp = tempfile::tempdir().unwrap();
    let bin = tmp.path().join("bin");
    std::fs::create_dir(&bin).unwrap();
    let herdr = bin.join("herdr");
    std::fs::write(&herdr, include_str!("support/fake-herdr.sh")).unwrap();
    std::fs::set_permissions(&herdr, std::fs::Permissions::from_mode(0o700)).unwrap();
    let state = tmp.path().join("herdr-state");
    std::fs::create_dir(&state).unwrap();
    unsafe {
        std::env::set_var(
            "PATH",
            format!("{}:{}", bin.display(), std::env::var("PATH").unwrap()),
        );
        std::env::set_var("FAKE_HERDR_DIR", &state);
    }
    let paths = Paths::new(Some(tmp.path().join("root"))).unwrap();
    let closes = || {
        calls(&state)
            .lines()
            .filter(|l| l.contains("pane close"))
            .count()
    };

    // Idle shell only: the exact pane is closed.
    assert_eq!(close_pane(&paths, "herdr/fixture/w1:p2"), (true, None));
    assert_eq!(closes(), 1);
    assert!(calls(&state).contains("pane close w1:p2"));
    assert!(state.join("closed-w1:p2").exists());

    // Already gone: reported, nothing closed.
    let (closed, reason) = close_pane(&paths, "herdr/fixture/w1:p2");
    assert!(!closed);
    assert_eq!(reason.as_deref(), Some("PANE_MISSING"));
    std::fs::write(state.join("missing-w1:p5"), "").unwrap();
    assert_eq!(
        close_pane(&paths, "herdr/fixture/w1:p5").1.as_deref(),
        Some("PANE_MISSING")
    );
    assert_eq!(closes(), 1);

    // Another program in the pane: left open, never closed.
    std::fs::write(state.join("busy-w1:p3"), "").unwrap();
    let (closed, reason) = close_pane(&paths, "herdr/fixture/w1:p3");
    assert!(!closed);
    assert!(reason.unwrap().starts_with("PANE_BUSY"));
    assert_eq!(closes(), 1);

    // tmux and malformed targets are never closed.
    assert!(
        close_pane(&paths, "%3")
            .1
            .unwrap()
            .starts_with("PANE_CLOSE_UNSUPPORTED")
    );
    assert!(!close_pane(&paths, "herdr/bad session/x").0);
    assert_eq!(closes(), 1);
}
