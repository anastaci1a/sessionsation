#!/usr/bin/env python3
"""Fake HERDR (installed as `herdr`) with one pane that replays REAL Claude Code
2.1.312 screens (tests/fixtures/claude-screens/*.ansi, captured with
`herdr pane read --format ansi`), for terminal-input tests. The input box of
the matching screen is replaced by the current input, drawn the way Claude Code
draws it (the pointer keeps the screen's own style: grey while a turn runs).
No sockets, no real terminal, no model.

State lives in $FAKE_HERDR_DIR/tui.json (written by the test):
  pane, pid           the pane and the process HERDR reports in its foreground
  shell_foreground    HERDR reports the pane's bare shell in front (the provider exited)
  screens             the fixture directory
  view                main | transcript | help | subagent | menu | agents | blank
  input               the prompt text (the user's draft); the cursor is at its end
  status              idle | working | blocked (HERDR agent_status)
  vim                 show a vim-mode indicator
  transcript          the session jsonl Enter appends the human record to
  record              human (default) | none (Enter records nothing)
  drop_after_submit   ignore typing after Enter (a restore that fails)
  garble_once         drop the last character of the next typed chunk
  fail_text_at, no_clear, keep_on_enter, on_type   golden fault options (see the
                      send-text and send-keys handlers)
  wrap, window        soft-wrap the box at `wrap` columns and show only its last `window` rows
  fail_enter          `send-keys enter` fails and submits nothing
  break_journal       directory of Sessionsation's request records; after the first typed text each
                      <id>.json there becomes a directory (the worker's journal writes then fail,
                      the way a full disk fails them); the original is kept as <id>.json.held
  break_screen_after_type   after the first typed text `pane read` fails
  break_at_chunk      the typed chunk (1-based) after which the two breaks above happen
  hold_queued         mid-turn input is recorded as queued_command and answered only when the turn ends
  ($FAKE_HERDR_DIR/hold-read: `pane read` waits while it exists; hold-send-text: `pane send-text`
   waits while it exists, then fails without typing)
  ($FAKE_HERDR_DIR/end-turn: the next `agent list` ends the running turn)
The HERDR agent status at each `send-text` goes to $FAKE_HERDR_DIR/typed-status.
Every command is appended to $FAKE_HERDR_DIR/calls as a JSON argv; Esc,
Ctrl+C, Ctrl+D and Ctrl+L are also logged to $FAKE_HERDR_DIR/forbidden.
Typed text is only accepted without a bracketed-paste wrapper, like the
real `pane send-text`.
"""
import datetime
import json
import os
import re
import sys
import time
import uuid

base = os.environ["FAKE_HERDR_DIR"]
state_file = os.path.join(base, "tui.json")
args = sys.argv[1:]
with open(os.path.join(base, "calls"), "a") as f:
    f.write(json.dumps(args) + "\n")
# The typing golden tests (tests/support/typing_golden.rs) read the calls made by a
# terminal delivery worker only: $FAKE_HERDR_DIR/worker-calls, one JSON object per call
# with its argv and a digest of what it printed (stdout and stderr).
try:
    with open("/proc/%d/cmdline" % os.getppid(), "rb") as f:
        _parent = f.read().split(b"\0")
except OSError:
    _parent = []
if any(a.endswith(b"-terminal-worker") for a in _parent):
    import atexit
    import hashlib
    import io
    _logged_args = list(args)
    _printed = io.StringIO()

    class _Tee:
        def __init__(self, real):
            self.real = real

        def write(self, s):
            _printed.write(s)
            return self.real.write(s)

        def flush(self):
            self.real.flush()

    sys.stdout = _Tee(sys.stdout)
    sys.stderr = _Tee(sys.stderr)

    def _log_call():
        digest = hashlib.sha256(_printed.getvalue().encode()).hexdigest()[:16]
        with open(os.path.join(base, "worker-calls"), "a") as f:
            f.write(json.dumps({"args": _logged_args, "out": digest}) + "\n")

    atexit.register(_log_call)
while args and args[0] == "--session":
    args = args[2:]
state = json.load(open(state_file))
state.setdefault("view", "main")
state.setdefault("input", "")
state.setdefault("status", "idle")
SGR = re.compile(r"\x1b\[[0-9;]*m")
POINTER = re.compile("^((?:\x1b\\[[0-9;]*m)*❯[  ])")


def save():
    with open(state_file + ".tmp", "w") as f:
        json.dump(state, f)
    os.replace(state_file + ".tmp", state_file)


def out(obj):
    print(json.dumps(obj))


def fail(code):
    sys.stderr.write(json.dumps({"error": {"code": code, "message": code}}) + "\n")
    sys.exit(1)


def stamp():
    return datetime.datetime.now(datetime.timezone.utc).strftime("%Y-%m-%dT%H:%M:%S.%f")[:-3] + "Z"


def append(record):
    with open(state["transcript"], "a") as f:
        f.write(json.dumps(record) + "\n")


def screen(name):
    with open(os.path.join(state["screens"], name + ".ansi"), encoding="utf-8") as f:
        return [line.rstrip("\r") for line in f.read().split("\n")]


def plain(line):
    return SGR.sub("", line)


def box(rows):
    """(pointer row, closing rule row) of the screen's input box."""
    for i in range(len(rows) - 1, 0, -1):
        if plain(rows[i]).startswith("❯") and plain(rows[i - 1]).startswith("─"):
            for j in range(i + 1, len(rows)):
                if plain(rows[j]).startswith("─"):
                    return i, j
    raise SystemExit("fixture without an input box")


def splice(rows, text):
    """Draw `text` in the input box, keeping the pointer's real style."""
    p, end = box(rows)
    prefix = POINTER.match(rows[p]).group(1) + "\x1b[0m"
    lines = text.split("\n")
    wrap = state.get("wrap")
    if wrap:
        # Soft-wrap every line to `wrap` columns, like the real box.
        lines = [l[i:i + wrap] for l in lines for i in range(0, max(len(l), 1), wrap)]
    drawn = [prefix + lines[0]] + ["  " + line for line in lines[1:]]
    window = state.get("window")
    if window and len(drawn) > window:
        # The real box is limited in height and shows only the tail.
        drawn = drawn[-window:]
        drawn[0] = prefix + drawn[0][2:]
    return rows[:p] + drawn + rows[end:]


def render():
    view, text, status = state["view"], state["input"], state["status"]
    if view == "blank":
        return ["● earlier output", "", "  some other program"]
    if view == "transcript":
        return screen("transcript-view")
    if view == "help":
        return screen("help")
    if view == "agents":
        return screen("agent-list-focused")
    if view == "menu":
        rows = screen("slash-menu")
        return rows if text == "/mo" else splice(rows, text)
    if view == "subagent":
        return screen("subagent-view") if text == "" else splice(screen("subagent-view-draft"), text)
    if status == "blocked":
        return screen("permission")
    if status == "working":
        if text == "":
            return screen("busy-queued-placeholder" if state.get("queued") else "busy-empty")
        rows = splice(screen("busy-empty"), text)
    else:
        rows = screen("idle-empty") if text == "" else splice(screen("idle-empty"), text)
    if state.get("vim"):
        rows = rows + ["  -- INSERT --"]
    return rows


def type_text(text):
    if "\x1b[200~" in text:
        fail("bracketed_paste_not_expected")
    if state.get("submitted") and state.get("drop_after_submit"):
        return
    if state.pop("garble_once", False):
        text = text[:-1]
    for ch in text:
        if state["view"] in ("transcript", "agents"):
            continue
        if ch == "?" and state["input"] == "" and state["view"] in ("main", "help"):
            state["view"] = "help" if state["view"] == "main" else "main"
            continue
        if state["view"] == "help":
            state["view"] = "main"
        state["input"] += ch


def kill_end():
    # The cursor is always at the end of the input in this fake.
    return


def kill_start():
    text = state["input"]
    if text.endswith("\n"):
        state["input"] = text[:-1]
    else:
        state["input"] = text[: text.rfind("\n") + 1]


def submit():
    text = state["input"]
    state["input"] = ""
    state["submitted"] = True
    if state.get("record", "human") == "none" or text == "":
        return
    if state["status"] == "working":
        # Mid-turn: queued as the user's own input; the box shows the
        # "Press up to edit queued messages" placeholder.
        state["queued"] = True
        append({"type": "attachment", "uuid": str(uuid.uuid4()), "timestamp": stamp(),
                "attachment": {"type": "queued_command", "prompt": text, "origin": {"kind": "human"}}})
        if state.get("hold_queued"):
            # The running turn reaches it (and replies) only when it ends.
            state.setdefault("pending_replies", []).append(text)
            return
    else:
        append({"type": "user", "uuid": str(uuid.uuid4()), "timestamp": stamp(),
                "message": {"role": "user", "content": text}})
    append({"type": "assistant", "timestamp": stamp(),
            "message": {"role": "assistant", "content": [{"type": "text", "text": "terminal reply to: " + text}]}})
    state["status"] = "working"
    # Long enough that the draft is typed back while the turn still runs.
    state["working_polls"] = 6


def keys(names):
    for key in names:
        if key in ("esc", "escape", "ctrl+c", "ctrl+d", "ctrl+l"):
            with open(os.path.join(base, "forbidden"), "a") as f:
                f.write(key + "\n")
            continue
        if state["view"] in ("transcript", "agents"):
            if key == "ctrl+o" and state["view"] == "transcript":
                state["view"] = "main"
            continue
        if key == "ctrl+o":
            state["view"] = "transcript"
        elif key == "ctrl+k":
            kill_end()
        elif key == "ctrl+u":
            kill_start()
        elif key == "ctrl+j":
            type_text("\n")
        elif key == "enter" and state["view"] in ("main", "subagent"):
            if state.get("fail_enter"):
                save()
                fail("pane_write_failed")
            submit()


cmd = " ".join(args[:2])


def hold(name):
    """Block while $FAKE_HERDR_DIR/<name> exists (at most 60 s); True when it held."""
    path = os.path.join(base, name)
    held = os.path.exists(path)
    for _ in range(1200):
        if not os.path.exists(path):
            break
        time.sleep(0.05)
    return held


if cmd == "agent list":
    if os.path.exists(os.path.join(base, "end-turn")):
        # The test ends the running turn (a file, so no read-modify-write race on tui.json).
        os.remove(os.path.join(base, "end-turn"))
        state["working_polls"] = 1
    status = state["status"]
    if state.get("working_polls"):
        state["working_polls"] -= 1
        if state["working_polls"] == 0:
            state["status"] = "idle"
            state.pop("queued", None)
            for text in state.pop("pending_replies", []):
                append({"type": "assistant", "timestamp": stamp(),
                        "message": {"role": "assistant", "content": [{"type": "text", "text": "terminal reply to: " + text}]}})
        save()
    out({"result": {"agents": [{"agent": "claude", "agent_status": status, "pane_id": state["pane"]}],
                    "type": "agent_list"}})
elif cmd == "pane process-info":
    front = {"pid": 1, "name": "bash"} if state.get("shell_foreground") else {"pid": state["pid"], "name": "claude"}
    out({"result": {"process_info": {"shell_pid": 1, "foreground_processes": [front]}}})
elif cmd == "pane read":
    hold("hold-read")
    if args[2] != state["pane"]:
        fail("pane_not_found")
    if state.get("screen_broken"):
        fail("pane_read_failed")
    sys.stdout.write("\r\n".join(render()))
elif cmd == "pane send-text":
    if hold("hold-send-text"):
        fail("pane_write_failed")
    if args[2] != state["pane"]:
        fail("pane_not_found")
    # Golden fault options: the Nth send-text fails untyped (fail_text_at); typing after
    # an Enter is ignored (drop_after_submit).
    state["texts"] = state.get("texts", 0) + 1
    if state.get("fail_text_at") == state["texts"]:
        save()
        fail("pane_write_failed")
    with open(os.path.join(base, "typed-status"), "a") as f:
        f.write(state["status"] + "\n")
    type_text(" ".join(args[3:]))
    # on_type: state patched in after the first typed text (busy, blocked, shell in front).
    if "on_type" in state:
        state.update(state.pop("on_type"))
        save()
    save()
    state["chunks"] = state.get("chunks", 0) + 1
    save()
    if state.get("break_journal") and not state.get("journal_broken") and state["chunks"] >= state.get("break_at_chunk", 1):
        d = state["break_journal"]
        for n in os.listdir(d):
            if n.endswith(".json") and os.path.isfile(os.path.join(d, n)):
                os.rename(os.path.join(d, n), os.path.join(d, n) + ".held")
                os.mkdir(os.path.join(d, n))
        state["journal_broken"] = True
        save()
    if state.get("break_screen_after_type") and not state.get("screen_broken") and state["chunks"] >= state.get("break_at_chunk", 1):
        state["screen_broken"] = True
        save()
elif cmd == "pane send-keys":
    if args[2] != state["pane"]:
        fail("pane_not_found")
    # Golden fault options: clearing keys do nothing (no_clear); the input box keeps its
    # text after a submit key (keep_on_enter).
    names = args[3:]
    if state.get("no_clear"):
        names = [k for k in names if k not in ("ctrl+u", "ctrl+k", "ctrl+e", "backspace", "alt+>")]
    submits = any(k in ("enter", "alt+m") for k in names)
    before = state["input"]
    keys(names)
    if submits and state.get("keep_on_enter"):
        state["input"] = before
    save()
else:
    fail("unsupported_fake_command")
