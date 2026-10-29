#!/usr/bin/env python3
"""Fake HERDR (installed as `herdr`) with one pane that shows a fake Claude Code
prompt, for terminal-input tests. No sockets, no real terminal, no model.

State lives in $FAKE_HERDR_DIR/tui.json (written by the test):
  pane, pid           the pane and the Claude process HERDR reports in it
  view                main | transcript | help | subagent | menu
  input               the prompt text (the user's draft); the cursor is at its end
  status              idle | working | blocked (HERDR agent_status)
  vim                 show a vim-mode indicator
  transcript          the session jsonl Enter appends the human record to
  record              human (default) | none (Enter records nothing)
  drop_after_submit   ignore typing after Enter (a restore that fails)
Every command is appended to $FAKE_HERDR_DIR/calls as a JSON argv; Esc,
Ctrl+C, Ctrl+D and Ctrl+L are also logged to $FAKE_HERDR_DIR/forbidden.
Typed text is only accepted without a bracketed-paste wrapper, like the
real `pane send-text`.
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
state.setdefault("view", "main")
state.setdefault("input", "")
state.setdefault("status", "idle")
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


def append(record):
    with open(state["transcript"], "a") as f:
        f.write(json.dumps(record) + "\n")


def render():
    rows = ["● earlier output", ""]
    if state["view"] == "blank":
        return rows + ["  some other program"]
    if state["view"] == "transcript":
        rows.append("  Showing detailed transcript · ctrl+o to toggle")
        return rows
    pointer = "❯"
    if state["view"] == "subagent":
        pointer = "\x1b[38;5;5m❯\x1b[0m"
    rows.append(RULE)
    text = state["input"]
    if text == "":
        if state["view"] == "subagent":
            rows.append(pointer + " \x1b[2mMessage @worker…\x1b[0m")
        else:
            rows.append(pointer + " \x1b[7mT\x1b[0m\x1b[2mry \"fix lint errors\"\x1b[0m")
    else:
        lines = text.split("\n")
        for i, line in enumerate(lines):
            cursor = "\x1b[7m \x1b[0m" if i == len(lines) - 1 else ""
            rows.append((pointer + " " if i == 0 else "  ") + line + cursor)
    rows.append(RULE)
    if state["view"] == "help":
        rows += ["  ! for shell mode        / for commands", "  @ for file paths        double tap esc to clear input"]
    elif state["view"] == "menu":
        rows += ["  /model      Set the AI model", "  /mcp        Manage MCP servers", "  /memory     Edit memory"]
    elif state.get("vim"):
        rows.append("  -- INSERT --")
    else:
        rows.append("  ? for shortcuts")
    return rows


def type_text(text):
    if "\x1b[200~" in text:
        fail("bracketed_paste_not_expected")
    if state.get("submitted") and state.get("drop_after_submit"):
        return
    for ch in text:
        if state["view"] == "transcript":
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
        append({"type": "attachment", "uuid": str(uuid.uuid4()), "timestamp": stamp(),
                "attachment": {"type": "queued_command", "prompt": text, "origin": {"kind": "human"}}})
    else:
        append({"type": "user", "uuid": str(uuid.uuid4()), "timestamp": stamp(),
                "message": {"role": "user", "content": text}})
    append({"type": "assistant", "timestamp": stamp(),
            "message": {"role": "assistant", "content": [{"type": "text", "text": "terminal reply to: " + text}]}})
    state["status"] = "working"
    state["working_polls"] = 2


def keys(names):
    for key in names:
        if key in ("esc", "escape", "ctrl+c", "ctrl+d", "ctrl+l"):
            with open(os.path.join(base, "forbidden"), "a") as f:
                f.write(key + "\n")
            continue
        if state["view"] == "transcript":
            if key == "ctrl+o":
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
            submit()


cmd = " ".join(args[:2])
if cmd == "agent list":
    status = state["status"]
    if state.get("working_polls"):
        state["working_polls"] -= 1
        if state["working_polls"] == 0:
            state["status"] = "idle"
        save()
    out({"result": {"agents": [{"agent": "claude", "agent_status": status, "pane_id": state["pane"]}],
                    "type": "agent_list"}})
elif cmd == "pane process-info":
    out({"result": {"process_info": {"shell_pid": 1,
                                     "foreground_processes": [{"pid": state["pid"], "name": "claude"}]}}})
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
