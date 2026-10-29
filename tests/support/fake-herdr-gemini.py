#!/usr/bin/env python3
"""Fake HERDR (installed as `herdr`) with one pane that shows a fake Gemini CLI
0.66.0 prompt, for Gemini terminal-input tests. No sockets, no real terminal,
no model. Modelled on fake-herdr-tui.py.

State lives in $FAKE_HERDR_DIR/tui.json (written by the test):
  pane, pid           the pane and the Gemini process HERDR reports in it
  view                main | help | dialog | blank
  input               the prompt text (the user's draft); the cursor is at its end
  status              idle | working | blocked (HERDR agent_status)
  vim, shell          show vim mode / shell mode
  steering            the session's experimental.modelSteering
  tool_follows        a steering update reaches a tool step (default true)
  transcript          the chat jsonl user/gemini records are appended to
  record              on (default) | none (Enter records nothing)
Enter while idle records a user turn. While working it queues the text (steering
off; all queued texts become one prompt joined by a blank line when the turn
ends) or keeps it as a steering hint (steering on), applied with the next tool
response or, without one, submitted as its own steering prompt after the turn.
Every command is appended to $FAKE_HERDR_DIR/calls as a JSON argv; Esc, Ctrl+C,
Ctrl+D and Ctrl+L are also logged to $FAKE_HERDR_DIR/forbidden.
"""
import datetime
import json
import os
import sys
import uuid

base = os.environ["FAKE_HERDR_DIR"]
state_file = os.path.join(base, "tui.json")
args = sys.argv[1:]
with open(os.path.join(base, "calls"), "a") as f:
    f.write(json.dumps(args) + "\n")
while args and args[0] == "--session":
    args = args[2:]
state = json.load(open(state_file))
for k, v in [("view", "main"), ("input", ""), ("status", "idle"), ("queue", []), ("hints", []),
             ("pending", []), ("tool_follows", True), ("steering", False)]:
    state.setdefault(k, v)
WIDTH = 60
TOP = "▄" * WIDTH
BOTTOM = "▀" * WIDTH
PLACEHOLDER = "Type your message or @path/to/file"
INSTRUCTION = "Internal instruction: Re-evaluate the active plan using this user steering update."


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
    record.setdefault("id", str(uuid.uuid4()))
    record.setdefault("timestamp", stamp())
    with open(state["transcript"], "a") as f:
        f.write(json.dumps(record) + "\n")


def steering_prompt(text):
    return "User steering update:\n<user_input>\n" + " ".join(text.split()) + "\n</user_input>\n" + INSTRUCTION


def start_turn(text):
    append({"type": "user", "content": [{"text": text}]})
    append({"type": "gemini", "content": "terminal reply to: " + text})
    state["status"] = "working"
    state["working_polls"] = 3


def render():
    rows = [" ✦ earlier output", ""]
    if state["view"] == "blank":
        return rows + ["$ some other program"]
    if state["view"] == "dialog":
        return rows + [TOP, " > earlier question", BOTTOM, "╭" + "─" * 20 + "╮",
                       "│ Allow execution?   │", "│ ● 1. Allow once     │",
                       "╰" + "─" * 20 + "╯"]
    for h in state["hints"]:
        rows.append(" \U0001f4a1 Steering Hint: " + h.replace("\n", " "))
    if state["queue"]:
        rows.append("  Queued (press ↑ to edit):")
        rows += ["    " + q.replace("\n", " ") for q in state["queue"]]
    if state["status"] == "working":
        rows.append(" ⠋ Thinking... (esc to cancel, 3s)")
    if state["view"] == "help":
        rows += ["─" * WIDTH, " Shortcuts See /help for more", "  ! shell mode       @ select file"]
    rows.append(TOP)
    marker = "!" if state.get("shell") else ">"
    text = state["input"]
    if text == "":
        hint = PLACEHOLDER
        if state.get("shell"):
            hint = "Type your shell command"
        elif state.get("vim"):
            hint = "Press 'i' for INSERT mode."
        rows.append(" " + marker + " \x1b[7m \x1b[0m\x1b[38;5;245m " + hint + "\x1b[39m")
    else:
        lines = text.split("\n")
        for i, line in enumerate(lines):
            cursor = "\x1b[7m \x1b[0m" if i == len(lines) - 1 else ""
            rows.append((" " + marker + " " if i == 0 else "   ") + line + cursor)
    rows.append(BOTTOM)
    rows.append(" ~/project        no sandbox        gemini-2.5-pro")
    return rows


def type_text(text):
    if "\x1b[200~" in text:
        fail("bracketed_paste_not_expected")
    if state["view"] in ("dialog", "blank"):
        return
    for ch in text:
        if state["view"] == "help":
            state["view"] = "main"
        elif state["input"] == "" and ch == "?":
            state["view"] = "help"
            continue
        elif state["input"] == "" and ch == "!":
            state["shell"] = not state.get("shell")
            continue
        state["input"] += ch


def submit():
    text = state["input"]
    state["input"] = ""
    state["submitted"] = True
    if text == "":
        return
    if state.get("record", "on") == "none":
        return
    if state["status"] == "idle":
        start_turn(text)
    elif state["steering"]:
        state["hints"].append(text)
        state["pending"].append(text)
        state["working_polls"] = 3
    else:
        state["queue"].append(text)
        state["working_polls"] = 3


def poll():
    """One `agent list`: a running turn advances only after a submit."""
    if state["status"] != "working" or not state.get("submitted"):
        return
    if state["pending"] and state["tool_follows"]:
        hint = "\n".join(state["pending"])
        state["pending"] = []
        append({"type": "user", "content": [{"text": steering_prompt(hint)},
                                            {"functionResponse": {"name": "run_shell_command", "response": {"output": "ok"}}}]})
        append({"type": "gemini", "content": "adjusted for: " + " ".join(hint.split())})
    state["working_polls"] = state.get("working_polls", 3) - 1
    if state["working_polls"] > 0:
        return
    state["status"] = "idle"
    state["hints"] = []
    if state["pending"]:
        hint = "\n".join(state["pending"])
        state["pending"] = []
        start_turn(steering_prompt(hint))
    elif state["queue"]:
        combined = "\n\n".join(state["queue"])
        state["queue"] = []
        start_turn(combined)


def keys(names):
    for key in names:
        if key in ("esc", "escape", "ctrl+c", "ctrl+d", "ctrl+l"):
            with open(os.path.join(base, "forbidden"), "a") as f:
                f.write(key + "\n")
            continue
        if state["view"] in ("dialog", "blank"):
            continue
        if state["view"] == "help":
            state["view"] = "main"
            if key == "backspace":
                continue
        text = state["input"]
        if key == "ctrl+e":
            pass  # the cursor is always at the end in this fake
        elif key == "ctrl+u":
            state["input"] = text[: text.rfind("\n") + 1]
        elif key == "ctrl+k":
            pass  # nothing after the cursor
        elif key == "backspace":
            if text == "" and state.get("shell"):
                state["shell"] = False
            state["input"] = text[:-1]
        elif key == "ctrl+j":
            state["input"] += "\n"
        elif key == "enter":
            submit()


cmd = " ".join(args[:2])
if cmd == "agent list":
    poll()
    save()
    out({"result": {"agents": [{"agent": "gemini", "agent_status": state["status"], "pane_id": state["pane"]}],
                    "type": "agent_list"}})
elif cmd == "pane process-info":
    out({"result": {"process_info": {"shell_pid": 1,
                                     "foreground_processes": [{"pid": state["pid"], "name": "node"}]}}})
elif cmd == "pane read":
    if args[2] != state["pane"]:
        fail("pane_not_found")
    sys.stdout.write("\r\n".join(render()) + "\r\n")
elif cmd == "pane send-text":
    if args[2] != state["pane"]:
        fail("pane_not_found")
    type_text(" ".join(args[3:]))
    save()
elif cmd == "pane send-keys":
    if args[2] != state["pane"]:
        fail("pane_not_found")
    keys(args[3:])
    save()
else:
    fail("unsupported_fake_command")
