#!/usr/bin/env python3
"""Fake `claude -p --input-format stream-json --output-format stream-json` for
hosted-session tests. No network, model or credential access.

Behaviour by message text: "slow" (3 s turn with tool rounds), "long" (3 s
turn without tool rounds), "refuse-me" (lifecycle refused), "needs-permission"
(a can_use_tool control request that waits for the host's answer), "crash-now"
(exit 3 after the turn started), "cut-by-now" (0.3 s in, the turn is cut as
if another client's `now` input had arrived).

Priorities model Claude Code 2.1.312 (see docs/cli.md, hosted sessions):
- `next` arriving mid-turn is folded into the running turn at its next tool
  round ("slow" turns); a turn without another tool round ("long") finishes
  first and the message runs as its own turn. Folded inputs report
  `completed` BEFORE the turn's result, fresh-turn inputs AFTER it.
- `now` arriving mid-turn CANCELS the running turn: the cut turn still emits a
  `success` result (is_error false, partial text) and then `cancelled` for
  its inputs; the `now` message starts a new turn.
- `later` never folds; it runs as its own turn.
- An interrupt control request aborts the running turn the same way (an
  `error_during_execution` result, then `cancelled`).
End of input ends the process after the queued work. The transcript is
appended so a later `--resume` works.
"""
import json
import os
import queue
import sys
import threading
import time
import uuid

home = os.environ["CLAUDE_CONFIG_DIR"]
args = sys.argv[1:]


def opt(name):
    return args[args.index(name) + 1] if name in args else None


required = ["-p", "--verbose", "--replay-user-messages"]
if (
    any(flag not in args for flag in required)
    or opt("--input-format") != "stream-json"
    or opt("--output-format") != "stream-json"
    or opt("--permission-prompt-tool") != "stdio"
):
    sys.exit(2)
session = opt("--session-id") or opt("--resume")
mode = "new" if "--session-id" in args else "resume"
cwd = os.getcwd()
count_file = os.path.join(home, "fake-stream-count")
count = int(open(count_file).read()) if os.path.exists(count_file) else 0
open(count_file, "w").write(str(count + 1))
with open(os.path.join(home, "fake-stream-launch.json"), "w") as f:
    json.dump(
        {
            "args": args,
            "mode": mode,
            "session": session,
            "is_sandbox": os.environ.get("IS_SANDBOX", ""),
            "cwd": cwd,
            "pid": os.getpid(),
            "api_key": "present" if os.environ.get("ANTHROPIC_API_KEY") else "",
            "herdr_pane": os.environ.get("HERDR_PANE_ID", ""),
            "config_dir": home,
            "autoupdater": os.environ.get("DISABLE_AUTOUPDATER"),
        },
        f,
    )
slug = cwd.replace("/", "-")
os.makedirs(os.path.join(home, "projects", slug), exist_ok=True)
transcript = os.path.join(home, "projects", slug, session + ".jsonl")
if mode == "new":
    with open(transcript, "w") as f:
        f.write(json.dumps({"type": "system", "sessionId": session, "cwd": cwd, "timestamp": "2026-01-01T00:00:00.000Z"}) + "\n")
elif not os.path.exists(transcript):
    sys.exit(1)


def record(obj):
    obj.setdefault("sessionId", session)
    with open(transcript, "a") as f:
        f.write(json.dumps(obj) + "\n")


def emit(obj):
    sys.stdout.write(json.dumps(obj) + "\n")
    sys.stdout.flush()


def lifecycle(uid, state):
    emit({"type": "command_lifecycle", "command_uuid": uid, "state": state, "session_id": session})


def assistant(text):
    emit({"type": "assistant", "session_id": session, "message": {"role": "assistant", "content": [{"type": "text", "text": text}]}})
    record({"type": "assistant", "message": {"role": "assistant", "content": [{"type": "text", "text": text}]}})


emit({"type": "system", "subtype": "init", "session_id": session, "permissionMode": opt("--permission-mode") or "default"})

inbox = queue.Queue()


def reader():
    for line in sys.stdin:
        try:
            inbox.put(json.loads(line))
        except ValueError:
            pass
    inbox.put(None)


threading.Thread(target=reader, daemon=True).start()
RANK = {"now": 0, "next": 1, "later": 2}
pending = []
turn = None
answers = {}
eof = False


def text_of(frame):
    content = frame.get("message", {}).get("content", "")
    if isinstance(content, list):
        return "".join(b.get("text", "") for b in content if b.get("type") == "text")
    return content


def handle(msg):
    global turn
    kind = msg.get("type")
    if kind == "user":
        uid = msg.get("uuid") or str(uuid.uuid4())
        text = text_of(msg)
        emit({"type": "user", "uuid": uid, "session_id": session, "isReplay": True, "message": msg.get("message")})
        if "refuse-me" in text:
            lifecycle(uid, "refused")
            return
        lifecycle(uid, "queued")
        if turn and msg.get("priority") == "now":
            # Real Claude Code cuts the running turn for a `now` input.
            cut(turn, "success", False)
            turn = None
            pending.insert(0, msg)
            return
        if turn and turn["tools"] and not turn["interrupted"] and not turn["perm"] and msg.get("priority", "next") == "next":
            # Folded at the running turn's next tool round.
            turn["uuids"].append(uid)
            turn["folded"].append(text)
            lifecycle(uid, "started")
            record({"type": "user", "uuid": uid, "message": {"role": "user", "content": text}})
            return
        pending.append(msg)
        pending.sort(key=lambda m: RANK.get(m.get("priority"), 1))
    elif kind == "control_request":
        request = msg.get("request", {})
        if request.get("subtype") == "interrupt":
            if turn:
                turn["interrupted"] = True
            emit({"type": "control_response", "response": {"subtype": "success", "request_id": msg.get("request_id")}})
    elif kind == "control_response":
        response = msg.get("response", {})
        answers[response.get("request_id")] = response.get("response", {}).get("behavior")


def cut(t, subtype, is_error):
    emit({"type": "result", "subtype": subtype, "is_error": is_error, "session_id": session,
          "num_turns": 1, "duration_ms": 5, "user_message_uuids": t["uuids"], "queued_turn_count": len(pending)})
    for uid in t["uuids"]:
        lifecycle(uid, "cancelled")


def advance():
    global turn
    now = time.time()
    if turn is None and pending:
        m = pending.pop(0)
        uid = m.get("uuid")
        text = text_of(m)
        duration = 3.0 if ("slow" in text or "long" in text) else 0.2
        turn = {"uuids": [uid], "text": text, "folded": [], "deadline": now + duration, "interrupted": False,
                "perm": None, "tools": "slow" in text,
                "cut_at": now + 0.3 if "cut-by-now" in text else None}
        lifecycle(uid, "started")
        record({"type": "user", "uuid": uid, "message": {"role": "user", "content": text}})
        assistant("working on: " + text)
        if "crash-now" in text:
            os._exit(3)
        if "needs-permission" in text:
            rid = str(uuid.uuid4())
            turn["perm"] = rid
            emit({"type": "control_request", "request_id": rid, "request": {"subtype": "can_use_tool", "tool_name": "Bash", "input": {"command": "ls"}}})
        return
    if turn is None:
        return
    if turn["interrupted"]:
        cut(turn, "error_during_execution", True)
        turn = None
        return
    if turn["cut_at"] and now >= turn["cut_at"]:
        cut(turn, "success", False)
        turn = None
        return
    if turn["perm"] and turn["perm"] not in answers:
        return
    if now < turn["deadline"]:
        return
    last = "fake reply to: " + turn["text"]
    if turn["perm"]:
        last = "permission " + str(answers[turn["perm"]])
    assistant(last)
    for text in turn["folded"]:
        last = "noted: " + text
        assistant(last)
    first = turn["uuids"][0]
    for uid in turn["uuids"][1:]:
        lifecycle(uid, "completed")
    emit({"type": "result", "subtype": "success", "is_error": False, "result": last, "session_id": session,
          "num_turns": 1, "duration_ms": 5, "usage": {"input_tokens": 3, "output_tokens": 4},
          "user_message_uuids": turn["uuids"], "queued_turn_count": len(pending)})
    lifecycle(first, "completed")
    turn = None


while True:
    try:
        msg = inbox.get(timeout=0.05)
    except queue.Empty:
        msg = "tick"
    if msg is None:
        eof = True
    elif msg != "tick":
        handle(msg)
    advance()
    if eof and turn is None and not pending:
        break
sys.exit(0)
