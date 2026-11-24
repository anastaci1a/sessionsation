#!/usr/bin/env python3
"""Fake HERDR (installed as `herdr`) with one pane that shows a fake Antigravity CLI
(agy 1.4.10) prompt, drawn like tests/fixtures/antigravity-screens, for the unified-send
tests. No sockets, no real terminal, no model. Modelled on fake-herdr-tui.py.

State lives in $FAKE_HERDR_DIR/tui.json (written by the test):
  pane, pid           the pane and the agy process HERDR reports in its foreground
  input               the prompt text (the user's draft); the cursor is at its end
  status              idle | working | blocked (HERDR agent_status)
  working_polls       `agent list` calls until a running turn ends (then queued input runs)
  ($FAKE_HERDR_DIR/end-turn: the next `agent list` ends the running turn)
  queued_mode         queue (default) | send-immediately: what Enter does mid-turn
  transcript          brain/<id>/.system_generated/logs/transcript.jsonl agy appends steps to
  garble_once         drop the last character of the next typed text
  fail_text_at, fail_enter, no_clear, keep_on_enter, drop_after_submit, on_type,
  shell_foreground    golden fault options (see the send-text and send-keys handlers)
Enter while idle records a user turn and its reply and starts a short turn. Mid-turn the
key that queues (Enter with queue, alt+m with send-immediately) lists the text in agy's
queue row (`▸ queued: ... (alt+m to send immediately)`); queued texts are recorded, each
as a user turn with a reply, when the running turn ends. The other key sends it to the
running turn at once. Every command is appended to $FAKE_HERDR_DIR/calls as a JSON argv;
Esc, Ctrl+C, Ctrl+D, Ctrl+K, Ctrl+L, up and down go to $FAKE_HERDR_DIR/forbidden.
"""
import datetime
import json
import os
import sys

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
for k, v in [("input", ""), ("status", "idle"), ("queue", []), ("step", 100),
             ("queued_mode", "queue")]:
    state.setdefault(k, v)
WIDTH = 60
RULE = "─" * WIDTH


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


def step(kind, source, content):
    state["step"] += 1
    with open(state["transcript"], "a") as f:
        f.write(json.dumps({"step_index": state["step"], "source": source, "type": kind,
                            "status": "DONE", "created_at": stamp(), "content": content}) + "\n")


def turn(text):
    step("USER_INPUT", "USER_EXPLICIT", "<USER_REQUEST>\n" + text + "\n</USER_REQUEST>")
    step("PLANNER_RESPONSE", "MODEL", "agy reply to: " + text)


def render():
    rows = ["  Antigravity CLI 1.4.10", "  fixture-user@example.invalid (Google AI Pro)",
            "  Gemini 3.8 Flash (High)", "  /work/project", ""]
    for q in state["queue"]:
        rows.append("▸ queued: " + q + " (alt+m to send immediately)")
    working = state["status"] == "working"
    if working:
        rows.append("⣟  Generating...")
    rows.append(RULE)
    lines = state["input"].split("\n")
    rows.append("> " + lines[0] if lines[0] else ">")
    rows.extend("  " + line for line in lines[1:])
    rows.append(RULE)
    left = "esc to cancel" if working else "? for shortcuts"
    rows.append(left.ljust(43) + "Gemini 3.8 Flash · high")
    return rows


def submit(key):
    text = state["input"]
    state["input"] = ""
    if text == "":
        return
    if state["status"] == "working":
        queues = "enter" if state["queued_mode"] == "queue" else "alt+m"
        if key == queues:
            state["queue"].append(text)
        else:
            turn(text)
        return
    turn(text)
    state["status"] = "working"
    state["working_polls"] = 4


def keys(names):
    for key in names:
        if key in ("esc", "escape", "ctrl+c", "ctrl+d", "ctrl+k", "ctrl+l", "up", "down"):
            with open(os.path.join(base, "forbidden"), "a") as f:
                f.write(key + "\n")
            continue
        if key == "ctrl+u":
            text = state["input"]
            state["input"] = text[: text.rfind("\n") + 1]
        elif key == "backspace":
            state["input"] = state["input"][:-1]
        elif key == "ctrl+j":
            state["input"] += "\n"
        elif key in ("enter", "alt+m"):
            if state.get("fail_enter"):
                save()
                fail("pane_write_failed")
            state["submitted"] = True
            submit(key)


cmd = " ".join(args[:2])
if cmd == "agent list":
    if os.path.exists(os.path.join(base, "end-turn")):
        # The test ends the running turn (a file, so no read-modify-write race on tui.json).
        os.remove(os.path.join(base, "end-turn"))
        state["working_polls"] = 1
    status = state["status"]
    if state.get("working_polls"):
        state["working_polls"] -= 1
        if state["working_polls"] == 0:
            for q in state["queue"]:
                turn(q)
            state["queue"] = []
            state["status"] = "idle"
        save()
    out({"result": {"agents": [{"agent": "agy", "agent_status": status, "pane_id": state["pane"]}],
                    "type": "agent_list"}})
elif cmd == "pane process-info":
    front = {"pid": 1, "name": "bash"} if state.get("shell_foreground") else {"pid": state["pid"], "name": "agy"}
    out({"result": {"process_info": {"shell_pid": 1, "foreground_processes": [front]}}})
elif cmd == "pane read":
    if args[2] != state["pane"]:
        fail("pane_not_found")
    sys.stdout.write("\r\n".join(render()))
elif cmd == "pane send-text":
    if args[2] != state["pane"]:
        fail("pane_not_found")
    # Golden fault options: the Nth send-text fails untyped (fail_text_at); typing after
    # an Enter is ignored (drop_after_submit).
    state["texts"] = state.get("texts", 0) + 1
    if state.get("fail_text_at") == state["texts"]:
        save()
        fail("pane_write_failed")
    text = " ".join(args[3:])
    if state.pop("garble_once", False):
        text = text[:-1]
    if not (state.get("drop_after_submit") and state.get("submitted")):
        state["input"] += text
    # on_type: state patched in after the first typed text (busy, blocked, shell in front).
    if "on_type" in state:
        state.update(state.pop("on_type"))
        save()
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
