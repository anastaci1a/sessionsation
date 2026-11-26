use super::*;
use std::fs;
use terminal::cells;
const TOP: &str = "▄▄▄▄▄▄▄▄▄▄▄▄▄▄▄▄▄▄▄▄▄▄▄▄▄▄▄▄▄▄▄▄▄▄▄▄▄▄▄▄▄▄▄▄▄▄▄▄▄▄▄▄▄▄▄▄▄▄▄▄";
const BOTTOM: &str = "▀▀▀▀▀▀▀▀▀▀▀▀▀▀▀▀▀▀▀▀▀▀▀▀▀▀▀▀▀▀▀▀▀▀▀▀▀▀▀▀▀▀▀▀▀▀▀▀▀▀▀▀▀▀▀▀▀▀▀▀";
fn screen(above: &[&str], prompt: &[&str], footer: &[&str]) -> Vec<Vec<Cell>> {
    let mut s = String::from(" ✦ earlier output\n");
    for l in above {
        s.push_str(l);
        s.push('\n');
    }
    s.push_str(TOP);
    s.push('\n');
    for l in prompt {
        s.push_str(l);
        s.push('\n');
    }
    s.push_str(BOTTOM);
    for l in footer {
        s.push('\n');
        s.push_str(l);
    }
    cells(&s)
}
const FOOT: &[&str] = &[" ~/project        no sandbox        gemini-2.5-pro"];
#[test]
fn payloads_are_escaped_or_refused() {
    assert_eq!(deliverable("hi").unwrap(), ("hi".into(), false, false));
    assert_eq!(deliverable("/clear it.").unwrap().0, "\\/clear it.");
    assert_eq!(deliverable("!ls now.").unwrap().0, "\\!ls now.");
    assert_eq!(deliverable("?").unwrap().0, "\\?");
    assert_eq!(deliverable("# heading").unwrap().0, "# heading");
    for bad in ["a\tb", "ends \\", "see @a.rs", "run /x", " "] {
        assert!(deliverable(bad).is_err(), "{bad}");
    }
    assert!(deliverable(&"x".repeat(MAX_TEXT + 1)).is_err());
    assert!(deliverable(&"x\n".repeat(MAX_LINES + 1)).is_err());
}
#[test]
fn the_prompt_and_draft_are_read_exactly_or_refused() {
    let empty = screen(
        &[],
        &[" > \u{1b}[7m \u{1b}[0m\u{1b}[38;5;245m Type your message or @path/to/file\u{1b}[39m"],
        FOOT,
    );
    assert_eq!(
        classify(&empty, true),
        Ok(View::Main {
            draft: String::new(),
            busy: false
        })
    );
    let unfocused = screen(&[], &[" >   Type your message or @path/to/file"], FOOT);
    assert!(
        matches!(classify(&unfocused, true), Ok(View::Main { ref draft, .. }) if draft.is_empty())
    );
    let draft = screen(
        &[" ⠋ Thinking (esc to cancel, 3s)"],
        &[" > half a thought", "   second line\u{1b}[7m \u{1b}[0m"],
        FOOT,
    );
    assert_eq!(
        classify(&draft, true),
        Ok(View::Main {
            draft: "half a thought\nsecond line".into(),
            busy: true
        })
    );
    let help = screen(
        &[
            "────────────────────",
            " Shortcuts See /help for more",
            "  ! shell mode   @ select file",
        ],
        &[" >   Type your message or @path/to/file"],
        FOOT,
    );
    assert_eq!(classify(&help, true), Ok(View::Help));
    for (above, prompt, footer, code) in [
        (
            vec![],
            vec![" ! Type your shell command"],
            FOOT.to_vec(),
            "TERMINAL_SHELL_MODE",
        ),
        (
            vec![],
            vec![" >   Press 'i' for INSERT mode."],
            FOOT.to_vec(),
            "TERMINAL_VIM_MODE",
        ),
        (
            vec![],
            vec![" > [Pasted Text: 12 lines]"],
            FOOT.to_vec(),
            "TERMINAL_DRAFT_UNRECONSTRUCTABLE",
        ),
        // Ghost text in a second colour.
        (
            vec![],
            vec![" > fix\u{1b}[38;5;245m the tests\u{1b}[39m"],
            FOOT.to_vec(),
            "TERMINAL_DRAFT_UNRECONSTRUCTABLE",
        ),
        // Possible soft wrap: the next row's first word would have fit badly.
        (
            vec![],
            vec![
                " > aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa",
                "   bbbb",
            ],
            FOOT.to_vec(),
            "TERMINAL_DRAFT_UNRECONSTRUCTABLE",
        ),
        (
            vec![],
            vec![" (r:) hist"],
            FOOT.to_vec(),
            "TERMINAL_OVERLAY_UNIDENTIFIED",
        ),
        (
            vec![],
            vec![" > /mo"],
            vec![
                "   /model   Set the model",
                "   /mcp     Servers",
                "   /memory  Memory",
                "   /more    More",
            ],
            "TERMINAL_OVERLAY_UNIDENTIFIED",
        ),
    ] {
        assert_eq!(
            classify(&screen(&above, &prompt, &footer), true),
            Err(code),
            "{prompt:?}"
        );
    }
    // A dialog in place of the prompt: the last `>` row is history.
    let dialog = cells(&format!(
        "{TOP}\n > earlier question\n{BOTTOM}\n╭──────────────╮\n│ Allow once?  │\n│ ● 1. Allow   │\n╰──────────────╯"
    ));
    assert!(classify(&dialog, true).is_err());
    assert_eq!(
        classify(&cells("$ plain shell"), true),
        Err("TERMINAL_VIEW_UNDETERMINED")
    );
    let tall: Vec<String> = (0..10).map(|i| format!("   line {i}")).collect();
    let mut prompt = vec![" > first".to_owned()];
    prompt.extend(tall);
    let prompt: Vec<&str> = prompt.iter().map(String::as_str).collect();
    assert!(classify(&screen(&[], &prompt, FOOT), true).is_err());
}
#[test]
fn the_anchor_is_the_first_user_record_carrying_the_text() {
    let d = tempfile::tempdir().unwrap();
    let p = d.path().join("s.jsonl");
    let lines = [
        json!({"sessionId":"x","startTime":"2026-10-29T00:00:00.000Z"}),
        json!({"id":"old","timestamp":"2026-10-29T00:00:00.000Z","type":"user","content":[{"text":"status?"}]}),
        json!({"id":"batch","timestamp":"2026-10-29T00:00:10.000Z","type":"user","content":[{"text":"first\n\nstatus?"}]}),
        json!({"id":"g1","timestamp":"2026-10-29T00:00:11.000Z","type":"gemini","content":"partial"}),
        json!({"$set":{"lastUpdated":"2026-10-29T00:00:11.000Z"}}),
        json!({"id":"g1","timestamp":"2026-10-29T00:00:11.000Z","type":"gemini","content":"working on it"}),
        json!({"id":"t","timestamp":"2026-10-29T00:00:12.000Z","type":"user","content":[{"functionResponse":{"name":"x"}}]}),
        json!({"id":"g2","timestamp":"2026-10-29T00:00:13.000Z","type":"gemini","content":"done"}),
        json!({"id":"next","timestamp":"2026-10-29T00:00:14.000Z","type":"user","content":[{"text":"another"}]}),
    ];
    fs::write(
        &p,
        lines.iter().map(|l| format!("{l}\n")).collect::<String>(),
    )
    .unwrap();
    let since = timestamp_ms("2026-10-29T00:00:05.000Z").unwrap();
    let f = find(&p, 0, "status?", since).unwrap();
    assert_eq!(f.id.as_deref(), Some("batch"));
    assert_eq!(f.outcome, Some("submitted"));
    assert!(!f.exact, "a queued batch shares the record");
    assert_eq!(f.reply, ["working on it", "done"]);
    assert!(f.turn_ended);
    // Steering updates: with a tool response (applied) or alone (after the turn).
    let hint = format!(
        "{STEERING_PREFIX}\n<user_input>\nalso   check\nthe docs.\n</user_input>\nInternal instruction: ..."
    );
    let lines = [
        json!({"id":"h1","timestamp":"2026-10-29T00:00:10.000Z","type":"user","content":[{"text":hint},{"functionResponse":{"name":"x"}}]}),
        json!({"id":"g","timestamp":"2026-10-29T00:00:11.000Z","type":"gemini","content":"adjusted"}),
    ];
    fs::write(
        &p,
        lines.iter().map(|l| format!("{l}\n")).collect::<String>(),
    )
    .unwrap();
    let f = find(&p, 0, "also check\nthe docs.", since).unwrap();
    assert_eq!(
        (f.id.as_deref(), f.outcome),
        (Some("h1"), Some("steering_applied"))
    );
    assert_eq!(f.reply, ["adjusted"]);
    let lines = [
        json!({"id":"h2","timestamp":"2026-10-29T00:00:10.000Z","type":"user","content":[{"text":hint}]}),
    ];
    fs::write(
        &p,
        lines.iter().map(|l| format!("{l}\n")).collect::<String>(),
    )
    .unwrap();
    let f = find(&p, 0, "also check the docs.", since).unwrap();
    assert_eq!(f.outcome, Some("steering_after_turn"));
    // Nothing after the submit: no anchor.
    assert_eq!(
        find(
            &p,
            0,
            "also check the docs.",
            timestamp_ms("2026-10-29T01:00:00.000Z").unwrap()
        )
        .unwrap(),
        Found::default()
    );
}
#[test]
fn modes_follow_the_steering_setting() {
    let on = steering::Effective {
        enabled: Some(true),
        configured: Some(true),
        source: "user",
        restart_required: false,
        reason: None,
    };
    let off = steering::Effective {
        enabled: Some(false),
        ..on.clone()
    };
    let unknown = steering::Effective {
        enabled: None,
        restart_required: true,
        reason: Some(steering::RESTART_REQUIRED),
        ..on.clone()
    };
    assert_eq!(mode_refusal("steer", &on, None), None);
    assert_eq!(
        code(mode_refusal("steer", &off, None).unwrap()),
        "GEMINI_STEERING_DISABLED"
    );
    assert_eq!(
        code(mode_refusal("queue", &on, None).unwrap()),
        "GEMINI_QUEUE_WOULD_STEER"
    );
    assert_eq!(
        code(mode_refusal("queue", &unknown, None).unwrap()),
        "GEMINI_STEERING_RESTART_REQUIRED"
    );
    assert_eq!(mode_refusal("submit", &on, Some(false)), None);
    assert_eq!(
        code(mode_refusal("submit", &on, Some(true)).unwrap()),
        "GEMINI_SUBMIT_WOULD_STEER"
    );
    assert_eq!(mode_refusal("submit", &off, Some(true)), None);
    assert!(mode_refusal("submit", &unknown, Some(true)).is_some());
}
