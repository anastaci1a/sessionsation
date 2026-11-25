use super::*;
use crate::messaging::claude_steer::terminal::cells;

fn screen(name: &str) -> Vec<Vec<Cell>> {
    let path = Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("tests/fixtures/antigravity-screens")
        .join(name);
    cells(&fs::read_to_string(&path).unwrap_or_else(|_| panic!("{}", path.display())))
}

#[test]
fn a_scrolled_box_shows_its_tail_and_is_never_a_user_draft() {
    // Real agy 1.4.12: a 1500-byte draft shows `> ↑ 26 more lines` and its tail.
    let rows = screen("idle-scrolled-draft.ansi");
    let Ok(View::Main { draft, .. }) = classify(&rows, false) else {
        panic!("scrolled box not read: {:?}", classify(&rows, false));
    };
    assert!(!draft.contains("more lines"), "{draft}");
    assert!(draft.starts_with("hip-20261031; latest"), "{draft}");
    assert!(
        draft.ends_with("Release remains audit-gated\nplu"),
        "{draft}"
    );
    assert!(classify(&rows, true).is_err());
}

#[test]
fn real_screens_classify_idle_busy_drafts_and_refuse_overlays() {
    let main = |name: &str| classify(&screen(name), true);
    assert_eq!(
        main("idle.ansi"),
        Ok(View::Main {
            draft: String::new(),
            busy: false
        })
    );
    assert_eq!(
        main("busy.ansi"),
        Ok(View::Main {
            draft: String::new(),
            busy: true
        })
    );
    assert_eq!(
        main("busy-draft.ansi"),
        Ok(View::Main {
            draft: "queued: reply QUEUED-OK".into(),
            busy: true
        })
    );
    assert_eq!(
        main("idle-draft-multiline.ansi"),
        Ok(View::Main {
            draft: "draft line one\ndraft line two".into(),
            busy: false
        })
    );
    assert_eq!(
        main("busy-queued.ansi"),
        Ok(View::Main {
            draft: String::new(),
            busy: true
        })
    );
    assert_eq!(
        main("slash-menu.ansi"),
        Err("TERMINAL_OVERLAY_UNIDENTIFIED")
    );
    assert_eq!(main("usage.ansi"), Err("TERMINAL_OVERLAY_UNIDENTIFIED"));
    assert_eq!(
        main("model-menu.ansi"),
        Err("TERMINAL_OVERLAY_UNIDENTIFIED")
    );
    assert_eq!(main("trust.ansi"), Err("TERMINAL_VIEW_UNDETERMINED"));
    // A tool permission dialog (HERDR also reports the pane `blocked`).
    assert_eq!(
        main("permission.ansi"),
        Err("TERMINAL_OVERLAY_UNIDENTIFIED")
    );
}

#[test]
fn exit_armed_footer_is_refused() {
    let mut rows = screen("idle.ansi");
    let last = rows
        .iter()
        .rposition(|r| row_text(r).contains("? for shortcuts"))
        .unwrap();
    let width = rows[last].len();
    let text = format!("{:<width$}", "press ctrl+c again to exit");
    rows[last] = text
        .chars()
        .map(|ch| Cell {
            ch,
            style: Default::default(),
        })
        .collect();
    assert_eq!(classify(&rows, true), Err("TERMINAL_EXIT_ARMED"));
}

#[test]
fn screen_evidence_reads_the_queue_and_sending_rows() {
    let lines: Vec<String> = screen("steer-and-queue.ansi")
        .iter()
        .map(|r| row_text(r))
        .collect();
    assert_eq!(
        screen_evidence(&lines, "steer note: also say STEERED"),
        Some("sending")
    );
    assert_eq!(
        screen_evidence(&lines, "queued note: reply QUEUED"),
        Some("queued")
    );
    assert_eq!(screen_evidence(&lines, "something else entirely"), None);
}

#[test]
fn payload_rules() {
    assert_eq!(deliverable(" hi there \r\n").unwrap().0, "hi there");
    for bad in ["/help", "?", "!ls", "end with \\", "see @file", "a\tb"] {
        assert!(deliverable(bad).is_err(), "{bad}");
    }
    assert!(deliverable(&"x".repeat(MAX_TEXT + 1)).is_err());
}

#[test]
fn keys_follow_the_queued_messages_setting() {
    use QueueMode::*;
    assert_eq!(
        busy_key("queue", Some(Queue)),
        Ok(("enter", "native_queue"))
    );
    assert_eq!(
        busy_key("steer", Some(Queue)),
        Ok(("alt+m", "send_immediately"))
    );
    assert_eq!(
        busy_key("queue", Some(SendImmediately)),
        Ok(("alt+m", "native_queue"))
    );
    assert_eq!(
        busy_key("steer", Some(SendImmediately)),
        Ok(("enter", "send_immediately"))
    );
    assert_eq!(
        busy_key("submit", Some(Queue)),
        Ok(("enter", "native_queue"))
    );
    assert_eq!(busy_key("submit", None), Ok(("enter", "unknown")));
    assert!(busy_key("steer", None).is_err() && busy_key("queue", None).is_err());
}

#[test]
fn transcript_anchor_and_replies_from_the_real_step_format() {
    let d = tempfile::tempdir().unwrap();
    let p = d.path().join("transcript.jsonl");
    let rec = |i: u32, source: &str, ty: &str, at: &str, content: &str| {
        json!({"step_index":i,"source":source,"type":ty,"status":"DONE","created_at":at,"content":content}).to_string()
    };
    let req = |t: &str| {
        format!(
            "<USER_REQUEST>\n{t}\n</USER_REQUEST>\n<ADDITIONAL_METADATA>\nThe current local time is: x.\n</ADDITIONAL_METADATA>"
        )
    };
    let lines = [
        rec(
            0,
            "USER_EXPLICIT",
            "USER_INPUT",
            "2026-01-01T00:00:00Z",
            &req("start the job"),
        ),
        rec(1, "MODEL", "PLANNER_RESPONSE", "2026-01-01T00:00:01Z", ""),
        rec(
            2,
            "MODEL",
            "GENERIC",
            "2026-01-01T00:00:02Z",
            "Tool is running",
        ),
        rec(
            3,
            "USER_EXPLICIT",
            "USER_INPUT",
            "2026-01-01T00:00:10Z",
            &req("steer note: also say STEERED"),
        ),
        rec(
            4,
            "MODEL",
            "PLANNER_RESPONSE",
            "2026-01-01T00:00:11Z",
            "Noted.",
        ),
        rec(
            5,
            "SYSTEM",
            "SYSTEM_MESSAGE",
            "2026-01-01T00:00:12Z",
            "system",
        ),
        rec(
            6,
            "MODEL",
            "PLANNER_RESPONSE",
            "2026-01-01T00:00:13Z",
            "FINISHED\n\nSTEERED",
        ),
        rec(
            7,
            "USER_EXPLICIT",
            "USER_INPUT",
            "2026-01-01T00:00:14Z",
            &req("queued note: reply QUEUED"),
        ),
        rec(
            8,
            "MODEL",
            "PLANNER_RESPONSE",
            "2026-01-01T00:00:14Z",
            "QUEUED",
        ),
    ];
    fs::write(&p, lines.join("\n") + "\n").unwrap();
    let since = timestamp_ms("2026-01-01T00:00:05Z").unwrap();
    let steer = find(&p, 0, "steer note:  also say STEERED", since).unwrap();
    assert_eq!(steer.id.as_deref(), Some("3"));
    assert!(steer.exact && steer.turn_ended);
    assert_eq!(steer.reply, ["Noted.", "FINISHED\n\nSTEERED"]);
    let queued = find(&p, 0, "queued note: reply QUEUED", since).unwrap();
    assert_eq!(
        (queued.id.as_deref(), queued.turn_ended),
        (Some("7"), false)
    );
    assert_eq!(queued.reply, ["QUEUED"]);
    let late = timestamp_ms("2026-01-01T00:00:20Z").unwrap();
    assert_eq!(
        find(&p, 0, "queued note: reply QUEUED", late).unwrap().id,
        None
    );
    assert_eq!(
        find(&p, 0, "start the job", since).unwrap().id,
        None,
        "before the submit"
    );
}
