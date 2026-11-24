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
# terminal delivery worker only: $FAKE_HERDR_DIR/worker-calls.
try:
    with open("/proc/%d/cmdline" % os.getppid(), "rb") as f:
        _parent = f.read().split(b"\0")
except OSError:
    _parent = []
if any(a.endswith(b"-terminal-worker") for a in _parent):
    with open(os.path.join(base, "worker-calls"), "a") as f:
        f.write(json.dumps(args) + "\n")
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
    out({"result": {"process_info": {"shell_pid": 1,
                                     "foreground_processes": [{"pid": state["pid"], "name": "agy"}]}}})
elif cmd == "pane read":
    if args[2] != state["pane"]:
        fail("pane_not_found")
    sys.stdout.write("\r\n".join(render()))
elif cmd == "pane send-text":
    if args[2] != state["pane"]:
        fail("pane_not_found")
    text = " ".join(args[3:])
    if state.pop("garble_once", False):
        text = text[:-1]
    state["input"] += text
    save()
elif cmd == "pane send-keys":
    if args[2] != state["pane"]:
        fail("pane_not_found")
    keys(args[3:])
    save()
else:
    fail("unsupported_fake_command")
