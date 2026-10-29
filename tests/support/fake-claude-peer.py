#!/usr/bin/env python3
"""Fake live interactive Claude session with a cross-session messaging (inbox)
socket, for peer-message tests. No network, model or credential access.

Usage: fake-claude-peer.py SESSION_ID CWD SOCKET_PATH (CLAUDE_CONFIG_DIR = home).
It writes a verified registry entry (pid, procStart, messagingSocketPath,
peerProtocol 1), records each received frame in <home>/fake-peer-frames.jsonl
and answers per <home>/fake-peer-mode:
  accept (default): status delivered, synthetic transcript turn, idle notice
  hold: status held only;  refuse: status refused;  expire: held then expired
  silent: delivered + idle notice but no transcript record (no anchor)
"""
import json
import os
import socket
import sys
import threading
import time
import uuid

home = os.environ["CLAUDE_CONFIG_DIR"]
session, cwd, sock_path = sys.argv[1:4]
slug = cwd.replace("/", "-")
os.makedirs(os.path.join(home, "projects", slug), exist_ok=True)
transcript = os.path.join(home, "projects", slug, session + ".jsonl")
if not os.path.exists(transcript):
    with open(transcript, "w") as f:
        f.write(json.dumps({"type": "user", "sessionId": session, "cwd": cwd, "timestamp": "2026-01-01T00:00:00.000Z",
                            "message": {"role": "user", "content": "private operator prompt"}}) + "\n")
lock = threading.Lock()
idle = {}
waiting = {}


def append(obj):
    obj.setdefault("sessionId", session)
    with lock, open(transcript, "a") as f:
        f.write(json.dumps(obj) + "\n")


def send_to(address, obj):
    path = address[4:] if address.startswith("uds:") else address
    s = socket.socket(socket.AF_UNIX, socket.SOCK_STREAM)
    s.connect(path)
    s.sendall((json.dumps(obj) + "\n").encode())
    s.close()


def status(address, msg_id, value):
    send_to(address, {"type": "peer_message_status", "orig_msg_id": msg_id, "status": value})


def notice(address, msg_id):
    send_to(address, {"type": "peer_idle_notice", "orig_msg_id": msg_id, "state": "idle",
                      "finished_at": int(time.time() * 1000), "detail": None})


def handle_user(frame):
    mode = "accept"
    mode_file = os.path.join(home, "fake-peer-mode")
    if os.path.exists(mode_file):
        mode = open(mode_file).read().strip()
    msg_id = frame.get("msg_id")
    sender = frame.get("from", "")
    text = frame.get("message", {}).get("content", "")
    if frame.get("session_id") != session:
        return
    if mode == "refuse":
        status(sender, msg_id, "refused")
        return
    if mode in ("hold", "expire"):
        status(sender, msg_id, "held")
        if mode == "expire":
            time.sleep(0.3)
            status(sender, msg_id, "expired")
        return
    status(sender, msg_id, "delivered")
    if mode != "silent":
        append({"type": "user", "isMeta": True, "uuid": str(uuid.uuid4()),
                "origin": {"kind": "peer", "from": sender, "msg_id": msg_id},
                "message": {"role": "user", "content": "Another Claude session sent a message: " + text}})
        time.sleep(0.2)
        append({"type": "assistant", "message": {"role": "assistant", "content": [{"type": "text", "text": "peer reply to: " + text}]}})
    idle["done"] = True
    for notify_id, address in list(waiting.items()):
        notice(address, notify_id)
        waiting.pop(notify_id, None)


def connection(conn):
    with conn, conn.makefile("r") as reader:
        for line in reader:
            try:
                frame = json.loads(line)
            except ValueError:
                continue
            with lock, open(os.path.join(home, "fake-peer-frames.jsonl"), "a") as f:
                f.write(json.dumps(frame) + "\n")
            if frame.get("type") == "user":
                idle["done"] = False
                threading.Thread(target=handle_user, args=(frame,), daemon=True).start()
            elif frame.get("type") == "notify_when_idle":
                if idle.get("done"):
                    notice(frame.get("from", ""), frame.get("msg_id"))
                else:
                    waiting[frame.get("msg_id")] = frame.get("from", "")


server = socket.socket(socket.AF_UNIX, socket.SOCK_STREAM)
if os.path.exists(sock_path):
    os.unlink(sock_path)
server.bind(sock_path)
server.listen(8)
start = open("/proc/self/stat").read().rsplit(") ", 1)[1].split()[19]
os.makedirs(os.path.join(home, "sessions"), exist_ok=True)
with open(os.path.join(home, "sessions", "%d.json" % os.getpid()), "w") as f:
    json.dump({"pid": os.getpid(), "sessionId": session, "cwd": cwd, "procStart": start, "kind": "interactive",
               "status": "idle", "version": "2.1.312", "messagingSocketPath": sock_path, "peerProtocol": 1}, f)
while True:
    conn, _ = server.accept()
    threading.Thread(target=connection, args=(conn,), daemon=True).start()
