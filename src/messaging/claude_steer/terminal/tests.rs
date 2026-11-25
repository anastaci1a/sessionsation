use super::*;
use std::time::{Duration, Instant};
/// The engine's re-read before the first clearing key (`ScreenProfile::REREAD_DRAFT`).
fn draft_unchanged(rows: &[Vec<Cell>], draft: &str) -> bool {
    is_main(&Claude::classify(rows, true), |d| d == draft)
}
const RULE: &str = "────────────────────────────────────────";
fn screen(prompt: &[&str], footer: &[&str]) -> Vec<Vec<Cell>> {
    let mut s = format!("● earlier output\n{RULE}\n");
    for l in prompt {
        s.push_str(l);
        s.push('\n');
    }
    s.push_str(RULE);
    for l in footer {
        s.push('\n');
        s.push_str(l);
    }
    cells(&s)
}
#[test]
fn payloads_are_escaped_or_refused_before_anything_is_typed() {
    assert_eq!(
        deliverable("hello").unwrap(),
        ("hello".into(), false, false)
    );
    assert_eq!(deliverable("/clear now.").unwrap().0, "\\/clear now.");
    assert_eq!(deliverable("!rm -rf x.").unwrap().0, "\\!rm -rf x.");
    assert_eq!(deliverable("# note").unwrap().0, "\\# note");
    assert_eq!(deliverable("?").unwrap().0, "\\?");
    assert_eq!(
        deliverable("  a\r\nb  ").unwrap(),
        ("a\nb".into(), false, true)
    );
    for bad in [
        "tab\there",
        "esc\u{1b}[A",
        "ends with \\",
        "see @src/main.rs",
        "run /compact",
        "   ",
    ] {
        assert!(deliverable(bad).is_err(), "{bad}");
    }
    assert!(deliverable(&"x".repeat(MAX_TEXT + 1)).is_err());
}
#[test]
fn a_draft_typed_after_the_first_read_is_detected_before_clearing() {
    let before = screen(&["❯\u{a0}half a thought\u{1b}[7m \u{1b}[0m"], &[]);
    assert!(draft_unchanged(&before, "half a thought"));
    let after = screen(&["❯\u{a0}half a thought, more\u{1b}[7m \u{1b}[0m"], &[]);
    assert!(!draft_unchanged(&after, "half a thought"));
    let empty = screen(&["❯\u{a0}\u{1b}[7m \u{1b}[0m"], &[]);
    assert!(!draft_unchanged(&empty, "half a thought"));
    assert!(REASONS.contains(&"TERMINAL_DRAFT_CHANGED"));
}
#[test]
fn the_main_prompt_draft_is_read_exactly_or_refused() {
    // Empty prompt with the dim placeholder and the cursor on its first cell.
    let s = screen(
        &["❯\u{a0}\u{1b}[7mT\u{1b}[0m\u{1b}[2mry \"fix lint errors\"\u{1b}[0m"],
        &["  ? for shortcuts"],
    );
    assert_eq!(classify(&s, true), Ok(View::Main(String::new())));
    let s = screen(&["❯\u{a0}\u{1b}[7m \u{1b}[0m"], &[]);
    assert_eq!(classify(&s, true), Ok(View::Main(String::new())));
    let s = screen(
        &["❯\u{a0}half a thought", "  second line\u{1b}[7m \u{1b}[0m"],
        &["  ? for shortcuts"],
    );
    assert_eq!(
        classify(&s, true),
        Ok(View::Main("half a thought\nsecond line".into()))
    );
    for (prompt, code) in [
        (
            vec!["❯\u{a0}[Pasted text #1 +40 lines]"],
            "TERMINAL_DRAFT_UNRECONSTRUCTABLE",
        ),
        (
            vec!["❯\u{a0}look [Image #1]"],
            "TERMINAL_DRAFT_UNRECONSTRUCTABLE",
        ),
        // Typed text followed by a dim suggestion.
        (
            vec!["❯\u{a0}fix\u{1b}[2m the tests\u{1b}[0m"],
            "TERMINAL_DRAFT_UNRECONSTRUCTABLE",
        ),
        // Reaches the right edge: may be soft-wrapped.
        (
            vec!["❯\u{a0}aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa", "  bb"],
            "TERMINAL_DRAFT_UNRECONSTRUCTABLE",
        ),
        (
            vec!["❯\u{a0}\u{1b}[2mMessage @worker…\u{1b}[0m"],
            "TERMINAL_VIEW_NOT_MAIN",
        ),
    ] {
        assert_eq!(
            classify(&screen(&prompt, &[]), true),
            Err(code),
            "{prompt:?}"
        );
    }
    let tall: Vec<String> = (0..20).map(|i| format!("  line {i}")).collect();
    let mut prompt = vec!["❯\u{a0}first"];
    prompt.extend(tall.iter().map(String::as_str));
    assert_eq!(
        classify(&screen(&prompt, &[]), true),
        Err("TERMINAL_DRAFT_UNRECONSTRUCTABLE")
    );
}
#[test]
fn views_and_overlays_are_identified_or_refused() {
    let transcript = cells("● reply\n  Showing detailed transcript · ctrl+o to toggle");
    assert_eq!(classify(&transcript, true), Ok(View::Transcript));
    let help = screen(
        &["❯\u{a0}"],
        &[
            "  ! for shell mode    / for commands",
            "  @ for file paths   double tap esc to clear input",
        ],
    );
    assert_eq!(classify(&help, true), Ok(View::Help));
    let vim = screen(&["❯\u{a0}draft"], &["  -- NORMAL --"]);
    assert_eq!(classify(&vim, true), Err("TERMINAL_VIM_MODE"));
    let menu = screen(
        &["❯\u{a0}/mo"],
        &[
            "  /model     Set the AI model",
            "  /mcp       Manage MCP servers",
        ],
    );
    assert_eq!(classify(&menu, true), Err("TERMINAL_OVERLAY_UNIDENTIFIED"));
    let picker =
        cells("  Select model\n  ❯ 1. Default\n    2. Opus\n  Enter to confirm · Esc to exit");
    assert!(classify(&picker, true).is_err());
    let shell = screen(&["!\u{a0}ls"], &[]);
    assert_eq!(classify(&shell, true), Err("TERMINAL_SHELL_MODE"));
    assert_eq!(
        classify(&cells("plain shell $"), true),
        Err("TERMINAL_VIEW_UNDETERMINED")
    );
    // Typed text is verified leniently (wraps allowed, ghost text ignored).
    let wrapped = screen(
        &[
            "❯\u{a0}aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa",
            "  bb\u{1b}[2m ghost\u{1b}[0m",
        ],
        &[],
    );
    assert_eq!(
        classify(&wrapped, false),
        Ok(View::Main(format!("{}\nbb", "a".repeat(38))))
    );
}
#[test]
fn the_typed_record_is_found_only_after_the_submit() {
    assert_eq!(timestamp_ms("1970-01-01T00:00:01.5Z"), Some(1500));
    assert_eq!(
        timestamp_ms("2026-10-29T00:00:00Z"),
        Some(1_793_232_000_000)
    );
    let d = tempfile::tempdir().unwrap();
    let p = d.path().join("t.jsonl");
    let lines = [
        json!({"type":"user","uuid":"old","timestamp":"2026-10-29T00:00:00.000Z","message":{"content":"status?"}}),
        json!({"type":"attachment","uuid":"peer","timestamp":"2026-10-29T00:00:10.000Z","attachment":{"type":"queued_command","prompt":"status?","origin":{"kind":"peer"}}}),
        json!({"type":"attachment","uuid":"mine","timestamp":"2026-10-29T00:00:11.000Z","attachment":{"type":"queued_command","prompt":"status?","origin":{"kind":"human"}}}),
        json!({"type":"assistant","message":{"content":[{"type":"text","text":"working on it"}]}}),
        json!({"type":"user","message":{"content":[{"type":"tool_result","content":"x"}]}}),
        json!({"type":"user","uuid":"echo","timestamp":"2026-10-29T00:00:12.000Z","message":{"content":"status?"}}),
        json!({"type":"assistant","message":{"content":[{"type":"text","text":"done"}]}}),
        json!({"type":"user","uuid":"next","message":{"content":"another"}}),
    ];
    fs::write(
        &p,
        lines.iter().map(|l| format!("{l}\n")).collect::<String>(),
    )
    .unwrap();
    let since = timestamp_ms("2026-10-29T00:00:05.000Z").unwrap();
    let f = find(&p, 0, "status?", since).unwrap();
    assert_eq!(f.uuid.as_deref(), Some("mine"));
    assert_eq!(f.record.as_deref(), Some("queued_command"));
    assert_eq!(f.reply, ["working on it", "done"]);
    assert!(f.turn_ended);
    // Nothing after the submit: no anchor, no reply.
    let f = find(
        &p,
        0,
        "status?",
        timestamp_ms("2026-10-29T01:00:00Z").unwrap(),
    )
    .unwrap();
    assert_eq!(f, Found::default());
}

/// Real Claude Code 2.1.312 screens (`pane read --format ansi`, redacted).
fn real(name: &str) -> Vec<Vec<Cell>> {
    let path = Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("tests/fixtures/claude-screens")
        .join(format!("{name}.ansi"));
    cells(&fs::read_to_string(&path).unwrap_or_else(|e| panic!("{path:?}: {e}")))
}
#[test]
fn real_main_screens_are_read_exactly_idle_or_busy() {
    for (name, draft) in [
        ("idle-empty", ""),
        ("idle-empty-after-turn", ""),
        ("idle-draft-single", "half a thought about lint"),
        (
            "idle-draft-multi",
            "half a thought about lint\nsecond line here\n  indented third",
        ),
        ("busy-empty", ""),
        ("busy-draft-single", "a busy draft"),
        ("busy-draft-multi", "a busy draft\nline two"),
        // After a busy Enter: dim "Press up to edit queued messages".
        ("busy-queued-placeholder", ""),
        ("busy-draft-restored", "a busy draft\nline two"),
        // Background agents listed under the footer, main shown.
        ("busy-agents-footer", ""),
        // A `claude attach` client of a background session: the session
        // name labels the top rule as plain text (no pill).
        ("bg-attach-idle", ""),
        // Real resumed screen (2026-11-06): a labelled top rule (8 dashes
        // in 58 cells) was read as no rule, so every auto-start prompt was
        // refused TERMINAL_VIEW_UNDETERMINED.
        ("labelled-top-rule-resumed", ""),
    ] {
        for strict in [true, false] {
            assert_eq!(
                classify(&real(name), strict),
                Ok(View::Main(draft.into())),
                "{name} strict={strict}"
            );
        }
    }
    // The regression: a running turn draws the main pointer grey.
    let busy = real("busy-draft-single");
    let p = busy
        .iter()
        .rposition(|r| r.first().is_some_and(|c| c.ch == '❯'))
        .unwrap();
    assert!(busy[p][0].style.fg, "busy pointer is coloured");
}
#[test]
fn real_overlays_and_other_views_are_refused_or_left() {
    assert_eq!(
        classify(&real("transcript-view"), true),
        Ok(View::Transcript)
    );
    assert_eq!(classify(&real("help"), true), Ok(View::Help));
    for (name, code) in [
        ("subagent-view", "TERMINAL_VIEW_NOT_MAIN"),
        ("subagent-view-draft", "TERMINAL_VIEW_NOT_MAIN"),
        ("slash-menu", "TERMINAL_OVERLAY_UNIDENTIFIED"),
        ("at-menu", "TERMINAL_OVERLAY_UNIDENTIFIED"),
        // Keyboard focus moved to the footer pill / agent list.
        ("footer-pill-focused", "TERMINAL_OVERLAY_UNIDENTIFIED"),
        ("agent-list-focused", "TERMINAL_OVERLAY_UNIDENTIFIED"),
        ("permission", "TERMINAL_VIEW_UNDETERMINED"),
        ("trust", "TERMINAL_VIEW_UNDETERMINED"),
    ] {
        for strict in [true, false] {
            assert_eq!(
                classify(&real(name), strict),
                Err(code),
                "{name} strict={strict}"
            );
        }
    }
}
#[test]
fn drafts_that_typing_cannot_reproduce_are_refused() {
    for ok in ["", "plain", "a\nb", "mail me@x.org please", "50% / 2"] {
        assert!(restorable(ok), "{ok:?}");
    }
    for bad in ["/mo", "?", "!ls", "# note", "look at @", "see @src", "a\tb"] {
        assert!(!restorable(bad), "{bad:?}");
    }
}
#[test]
fn a_background_session_is_typed_into_only_through_its_exact_attach_client() {
    // A process whose argv carries `attach <job>`, like `claude attach <job>`.
    let mut client = std::process::Command::new("sh")
        .args(["-c", "sleep 30; true", "attach", "job12345"])
        .spawn()
        .unwrap();
    let pid = client.id();
    // Until the forked child has exec'd, /proc shows the test binary.
    let deadline = Instant::now() + Duration::from_secs(5);
    while !fs::read(format!("/proc/{pid}/cmdline"))
        .is_ok_and(|c| c.windows(6).any(|w| w == b"attach"))
    {
        assert!(Instant::now() < deadline, "attach client did not start");
        std::thread::sleep(Duration::from_millis(10));
    }
    let start = claude::proc_start(pid).unwrap().1;
    let row = |kind: &str, job: &str, host: &str, pid: u32, start: u64| {
        json!({"pid":999_999,"process_start":1,"native":{"kind":kind,"job_id":job},
            "attachment":{"host":host,"target":"herdr/default/w1:p1","pid":pid,"process_start":start}})
    };
    assert_eq!(
        row_reason(&row("bg", "job12345", "herdr", pid, start)),
        None
    );
    for bad in [
        row("interactive", "job12345", "herdr", pid, start),
        row("bg", "job99999", "herdr", pid, start),
        row("bg", "job12345", "herdr", pid, start + 1),
        row("bg", "job12345", "tmux", pid, start),
    ] {
        assert_eq!(row_reason(&bad), Some("TERMINAL_NOT_ATTACHED"), "{bad}");
    }
    // The pane runs the session's own process: exact PID and start.
    let direct = json!({"pid":pid,"process_start":start,"native":{"kind":"interactive"},
        "attachment":{"host":"herdr","target":"herdr/default/w1:p1","pid":pid,"process_start":start}});
    assert_eq!(row_reason(&direct), None);
    let _ = client.kill();
    let _ = client.wait();
}
#[test]
fn a_parked_interactive_pane_is_the_terminal_of_its_job() {
    let t = tempfile::tempdir().unwrap();
    let home = fs::canonicalize(t.path()).unwrap();
    let mut pane = std::process::Command::new("sleep")
        .arg("600")
        .env("CLAUDE_CONFIG_DIR", &home)
        .spawn()
        .unwrap();
    let pid = pane.id();
    let start = claude::proc_start(pid).unwrap().1;
    fs::create_dir_all(home.join("sessions")).unwrap();
    fs::write(
        home.join(format!("sessions/{pid}.json")),
        json!({"pid":pid,"sessionId":"0946656e-447e-4bcd-a8df-d047792a191a",
            "procStart":start.to_string(),"kind":"interactive","parkedJobId":"job12345"})
        .to_string(),
    )
    .unwrap();
    let row = |job: &str, start: u64| {
        json!({"pid":999_999,"process_start":1,"native":{"kind":"bg","job_id":job},
            "attachment":{"host":"herdr","target":"herdr/default/wR:pH","pid":pid,"process_start":start}})
    };
    assert_eq!(row_reason(&row("job12345", start)), None);
    assert_eq!(
        row_reason(&row("job99999", start)),
        Some("TERMINAL_NOT_ATTACHED")
    );
    assert_eq!(
        row_reason(&row("job12345", start + 1)),
        Some("TERMINAL_NOT_ATTACHED")
    );
    let _ = pane.kill();
    let _ = pane.wait();
}
