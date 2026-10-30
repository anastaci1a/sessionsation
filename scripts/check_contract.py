"""Validate the published schema, fixtures and real CLI envelopes offline."""
import json
import os
from pathlib import Path
import subprocess
import tempfile
from jsonschema import Draft202012Validator, FormatChecker
from jsonschema.exceptions import ValidationError

root = Path(__file__).resolve().parents[1]
schema = json.loads((root / "docs/contract/v1.json").read_text())
Draft202012Validator.check_schema(schema)
checker = FormatChecker()
def validate(value, definition=None):
    spec = schema if definition is None else {"$ref": f"#/$defs/{definition}", "$defs": schema["$defs"]}
    Draft202012Validator(spec, format_checker=checker).validate(value)

fixtures = json.loads((root / "docs/contract/fixtures.json").read_text())
for name in ["placement", "placement_stopped", "placement_outage", "placement_unknown"]:
    validate(fixtures[name], "Placement")
validate(fixtures["placements"], "Placements")
validate(fixtures["checkpoint_request"], "CheckpointRequest")
validate(fixtures["checkpoints"], "Checkpoints")
validate(fixtures["checkpoint_native_error"])
for field in ["delivery_evidence", "sends_input", "starts_process"]:
    try:
        validate({**fixtures["checkpoints"], field: True}, "Checkpoints")
    except ValidationError:
        pass
    else:
        raise AssertionError("Checkpoints must remain read-only and not delivery evidence")
validate(fixtures["runtime_attachment"], "RuntimeAttachment")
validate(fixtures["request"], "MessageRequest")
validate(fixtures["steering_message"], "MessageRequest")
validate(fixtures["steering_target"], "SteeringTarget")
validate(fixtures["turn_target"], "TurnTarget")
for invalid in [{**fixtures["steering_message"], "steering": None}, {**fixtures["steering_message"], "mode": "queue"}, {**fixtures["steering_message"], "steering": {"version": 1, "expected_turn_id": "active"}}]:
    try:
        validate(invalid, "MessageRequest")
    except ValidationError:
        pass
    else:
        raise AssertionError("Incomplete or implicit steering must be rejected")
for invalid in [
    {**fixtures["receipt_steer_accepted"], "facts": {**fixtures["receipt_steer_accepted"]["facts"], "input_recorded": True}},
    {**fixtures["receipt_steer_accepted"], "facts": {**fixtures["receipt_steer_accepted"]["facts"], "turn_completed": True}},
    {**fixtures["receipt_steer_accepted"], "reply": {"item_id": "unrelated", "text": "unrelated turn reply", "phase": "final_answer"}},
]:
    try:
        validate(invalid, "Receipt")
    except ValidationError:
        pass
    else:
        raise AssertionError("Steering acceptance must not imply consumption or reply correlation")
for name in ["receipt_steer_progress", "receipt_steer_reply", "receipt_steer_interrupted", "receipt_steer_not_consumed"]:
    validate(fixtures[name], "Receipt")
progress, final = fixtures["receipt_steer_progress"], fixtures["receipt_steer_reply"]
for invalid in [
    {**progress, "reply": {**progress["reply"], "final": True}},
    {**progress, "correlation": None},
    {**final, "reply": {**final["reply"], "final": False}},
    {**final, "facts": {**final["facts"], "turn_completed": False}},
    {**final, "reply": None},
]:
    try:
        validate(invalid, "Receipt")
    except ValidationError:
        pass
    else:
        raise AssertionError("A steer reply is final only after its turn completed, and needs its input anchor")
validate(fixtures["recovery_message"], "MessageRequest")
validate(fixtures["recovery_plan"], "RecoveryPlan")
validate(fixtures["space_choices"], "SpaceChoices")
validate(fixtures["host_space_choices"], "HostSpaceChoices")
for name in ["add_to_herdr_attached", "add_to_herdr_unconfirmed"]:
    validate(fixtures[name], "AddToHerdrResult")
for invalid in [{**fixtures["add_to_herdr_attached"], "starts_agent": True}, {**fixtures["add_to_herdr_attached"], "attachment": None}]:
    try:
        validate(invalid, "AddToHerdrResult")
    except ValidationError:
        pass
    else:
        raise AssertionError("Add to HERDR must never start an agent or claim an unverified attachment")
validate(fixtures["space_move_operation"], "SpaceMoveOperation")
validate(fixtures["space_move_operation_new_space"], "SpaceMoveOperation")
validate(fixtures["archived_session"], "ArchivedSession")
validate(fixtures["archived_session_claude"], "ArchivedSession")
validate(fixtures["native_conversation_archive"], "NativeConversationArchive")
for invalid, definition in [
    ({**fixtures["native_conversation_archive"], "credentials_touched": True}, "NativeConversationArchive"),
    ({**fixtures["native_conversation_archive"], "delete_removes_catalog_entry": False}, "NativeConversationArchive"),
    ({**fixtures["native_conversation_archive"], "refusal_reasons": ["SESSION_HOSTED"]}, "NativeConversationArchive"),
    ({**fixtures["archived_session_claude"], "provider": "other"}, "ArchivedSession"),
]:
    try:
        validate(invalid, definition)
    except ValidationError:
        pass
    else:
        raise AssertionError("native archives must never touch credentials, keep deleted entries or omit the running refusal")
validate(fixtures["conversation_operation"], "ConversationOperation")
validate(fixtures["account_deletion"], "AccountDeletion")
validate(fixtures["account_deletion_preview"], "AccountDeletionPreview")
for name in ["claude_headless_resume_request", "claude_headless_new_request"]:
    validate(fixtures[name], "ClaudeHeadlessRequest")
for name in ["claude_headless_receipt", "claude_headless_receipt_timeout"]:
    validate(fixtures[name], "ClaudeHeadlessReceipt")
for invalid, definition in [
    ({**fixtures["claude_headless_resume_request"], "mode": "submit"}, "ClaudeHeadlessRequest"),
    ({**fixtures["claude_headless_new_request"], "session_id": fixtures["claude_headless_resume_request"]["session_id"]}, "ClaudeHeadlessRequest"),
    ({**fixtures["claude_headless_receipt_timeout"], "state": "completed_with_reply"}, "ClaudeHeadlessReceipt"),
    ({**fixtures["claude_headless_receipt"], "execution": {**fixtures["claude_headless_receipt"]["execution"], "automatic_replay": True}}, "ClaudeHeadlessReceipt"),
]:
    try:
        validate(invalid, definition)
    except ValidationError:
        pass
    else:
        raise AssertionError("Claude headless delivery must not claim unsupported modes, replay or uncorrelated success")
validate(fixtures["claude_steer_request"], "ClaudeSteerRequest")
validate(fixtures["claude_steer_semantics"], "ClaudeSteerSemantics")
for name in ["claude_steer_receipt_hosted", "claude_steer_receipt_interrupted", "claude_steer_receipt_cut", "claude_steer_receipt_peer", "claude_steer_receipt_peer_unanchored", "claude_steer_receipt_held"]:
    validate(fixtures[name], "ClaudeSteerReceipt")
validate(fixtures["hosted_session"], "HostedSession")
validate(fixtures["live_steering"], "LiveSteering")
validate(fixtures["launch_request_hosted"], "LaunchRequest")
hosted, peer, unanchored = fixtures["claude_steer_receipt_hosted"], fixtures["claude_steer_receipt_peer"], fixtures["claude_steer_receipt_peer_unanchored"]
for invalid, definition in [
    ({**fixtures["claude_steer_request"], "mode": "queue"}, "ClaudeSteerRequest"),
    ({**fixtures["claude_steer_request"], "steering": {"version": 1, "expected_turn_id": "turn"}}, "ClaudeSteerRequest"),
    ({**hosted, "execution": {**hosted["execution"], "automatic_replay": True}}, "ClaudeSteerReceipt"),
    ({**hosted, "correlation": None}, "ClaudeSteerReceipt"),
    ({**hosted, "reply": {**hosted["reply"], "final": False}}, "ClaudeSteerReceipt"),
    # A cut turn's success result is not completion: the cancelled lifecycle wins.
    ({**fixtures["claude_steer_receipt_cut"], "state": "completed_with_reply", "reply": {**fixtures["claude_steer_receipt_cut"]["reply"], "final": True}, "facts": {**fixtures["claude_steer_receipt_cut"]["facts"], "reply_final": True}}, "ClaudeSteerReceipt"),
    ({**fixtures["claude_steer_receipt_cut"], "reply": {**fixtures["claude_steer_receipt_cut"]["reply"], "final": True}}, "ClaudeSteerReceipt"),
    ({**fixtures["claude_steer_semantics"], "interrupts": True}, "ClaudeSteerSemantics"),
    ({**unanchored, "reply": peer["reply"]}, "ClaudeSteerReceipt"),
    ({**unanchored, "state": "completed_with_reply"}, "ClaudeSteerReceipt"),
    ({**peer, "peer_message": None}, "ClaudeSteerReceipt"),
    ({**fixtures["hosted_session"], "effective_permission_mode": "bypassPermissions"}, "HostedSession"),
    ({**fixtures["launch_request_hosted"], "account_id": "codex:fixture"}, "LaunchRequest"),
]:
    try:
        validate(invalid, definition)
    except ValidationError:
        pass
    else:
        raise AssertionError("Claude steering must not replay, claim unanchored peer replies or bypass permissions implicitly")
validate(fixtures["claude_steer_request_terminal"], "ClaudeSteerRequest")
for name in ["claude_steer_receipt_terminal", "claude_steer_receipt_terminal_unconfirmed", "claude_steer_receipt_terminal_refused"]:
    validate(fixtures[name], "ClaudeSteerReceipt")
terminal = fixtures["claude_steer_receipt_terminal"]
for invalid, definition in [
    ({**fixtures["claude_steer_request_terminal"], "delivery": "tmux"}, "ClaudeSteerRequest"),
    ({**terminal, "correlation": {**terminal["correlation"], "input_uuid": None}}, "ClaudeSteerReceipt"),
    ({**terminal, "terminal_input": None}, "ClaudeSteerReceipt"),
    ({**terminal, "terminal_input": {**terminal["terminal_input"], "draft": {**terminal["terminal_input"]["draft"], "text": "user draft"}}}, "ClaudeSteerReceipt"),
    ({**fixtures["claude_steer_receipt_terminal_unconfirmed"], "state": "completed_with_reply"}, "ClaudeSteerReceipt"),
]:
    try:
        validate(invalid, definition)
    except ValidationError:
        pass
    else:
        raise AssertionError("Terminal input must anchor replies to the typed human record and never expose the draft")
validate(fixtures["gemini_steer_request"], "ClaudeSteerRequest")
for name in ["gemini_steer_receipt_queued", "gemini_steer_receipt_steer_applied", "gemini_steer_receipt_steer_after_turn", "gemini_steer_receipt_refused"]:
    validate(fixtures[name], "GeminiSteerReceipt")
    validate(fixtures[name], "ClaudeSteerReceipt")
validate(fixtures["antigravity_steer_receipt_queued"], "ClaudeSteerReceipt")
for invalid in [
    {**fixtures["antigravity_steer_receipt_queued"], "antigravity_input": None},
    {**fixtures["antigravity_steer_receipt_queued"], "gemini_input": {}},
    {**fixtures["antigravity_steer_receipt_queued"], "account_id": "claude:personal"},
    {**fixtures["gemini_steer_receipt_queued"], "provider": "antigravity"},
]:
    try:
        validate(invalid, "ClaudeSteerReceipt")
    except ValidationError:
        pass
    else:
        raise AssertionError("Antigravity receipts need antigravity_input, an antigravity: account and no Gemini fields")
validate(fixtures["gemini_live_steering"], "GeminiLiveSteering")
applied, after = fixtures["gemini_steer_receipt_steer_applied"], fixtures["gemini_steer_receipt_steer_after_turn"]
for invalid, definition in [
    ({**applied, "gemini_input": None}, "GeminiSteerReceipt"),
    ({**applied, "delivery_semantics": "peer_message"}, "GeminiSteerReceipt"),
    ({**applied, "account_id": "claude:personal"}, "GeminiSteerReceipt"),
    ({**after, "gemini_input": {**after["gemini_input"], "steering": {"applied": True, "fate": "applied_at_next_tool_step"}}}, "GeminiSteerReceipt"),
    ({**applied, "gemini_input": {**applied["gemini_input"], "steering": {"applied": False, "fate": None}}}, "GeminiSteerReceipt"),
    ({**fixtures["claude_steer_receipt_terminal"], "gemini_input": applied["gemini_input"]}, "ClaudeSteerReceipt"),
    ({**fixtures["gemini_live_steering"], "setting": "crossSessionInbound"}, "GeminiLiveSteering"),
]:
    try:
        validate(invalid, definition)
    except ValidationError:
        pass
    else:
        raise AssertionError("Gemini receipts must report the steering fate honestly and stay terminal_input only")
validate(fixtures["account_refresh"], "AccountRefresh")
validate(fixtures["account_rename"], "AccountRename")
validate(fixtures["claude_login_plan"], "AccountLoginPlan")
validate(fixtures["launch_request"], "LaunchRequest")
validate(fixtures["launch_receipt"], "LaunchReceipt")
validate(fixtures["resume_request"], "ResumeRequest")
validate(fixtures["resume_directory_request"], "ResumeRequest")
for fixture, definition in [("launch_request", "LaunchRequest"), ("resume_request", "ResumeRequest")]:
    for name in ["a" * 30, "界" * 30]:
        validate({**fixtures[fixture], "name": name}, definition)
    for name in ["a" * 31, "界" * 31]:
        try:
            validate({**fixtures[fixture], "name": name}, definition)
        except ValidationError:
            pass
        else:
            raise AssertionError("Session names longer than 30 must be rejected")
    # Revision 19: optional launch permissions, yolo or default only.
    for permissions in ["yolo", "default"]:
        validate({**fixtures[fixture], "permissions": permissions}, definition)
    try:
        validate({**fixtures[fixture], "permissions": "ask"}, definition)
    except ValidationError:
        pass
    else:
        raise AssertionError("Unknown launch permissions must be rejected")
validate(fixtures["resume_receipt"], "ResumeReceipt")
validate(fixtures["error"])
validate(fixtures["capabilities"])
for name, definition in [("session", "Session"), ("session_host_label", "Session"), ("snapshot", "Snapshot"), ("readiness", "Readiness"), ("readiness_stopped", "Readiness"), ("readiness_steer", "Readiness")]:
    validate(fixtures[name], definition)
# Revision 23: pending questions and answer receipts.
for name in ["pending_question", "pending_question_blocking"]:
    validate(fixtures[name], "PendingQuestion")
    validate({**fixtures["session"], "pending_questions": [fixtures[name]]}, "Session")
validate(fixtures["question_answers"], "QuestionAnswers")
validate(fixtures["question_answer_receipt"], "QuestionAnswerReceipt")
for bad in [{**fixtures["pending_question"], "questions": []}, {**fixtures["pending_question"], "kind": "gemini_ask_user"}]:
    try:
        validate(bad, "PendingQuestion")
    except ValidationError:
        pass
    else:
        raise AssertionError("Invalid pending questions must be rejected")
for name, value in fixtures.items():
    if name.startswith("receipt_"):
        validate(value, "Receipt")
with tempfile.TemporaryDirectory() as directory:
    command = [str(Path(os.environ.get("CARGO_TARGET_DIR", root / "target")) / "debug/sessionsation"), "--data-dir", directory, "api"]
    for args in [["version"], ["capabilities"], ["placements"], ["state"], ["state", "--archive-filter", "all"], ["archives"]]:
        value = json.loads(subprocess.check_output(command + args, text=True))
        validate(value)
        if args[0] == "state":
            validate(value["result"], "Snapshot")
            assert all(isinstance(s["pending_questions"], list) for s in value["result"]["sessions"])
        if args == ["placements"]:
            validate(value["result"], "Placements")
        if args == ["capabilities"]:
            assert value["result"]["contract_revision"] >= 23
            assert value["result"]["contract_version"].startswith("1." + str(value["result"]["contract_revision"]) + ".")
            # Revision 23: pending questions.
            questions = value["result"]["questions"]
            assert questions["since_revision"] == 23 and questions["replay"] == "never"
            assert questions["providers"]["codex"]["answer"] is True
            assert questions["providers"]["claude"]["answer"] == "hosted_only"
            assert questions["providers"]["gemini"]["detect"] is False and questions["providers"]["gemini"]["reason"]
            # Revision 21: per-account Gemini authentication modes.
            gemini = next(p for p in value["result"]["providers"] if p["id"] == "gemini")
            assert [m["id"] for m in gemini["auth_modes"]] == ["google_personal", "google_workspace", "api_key"]
            assert gemini["auth_modes"][0]["available"] is False and gemini["auth_modes"][0]["reason"]
            assert gemini["auth_command"]["since_revision"] == 21
            assert gemini["auth_command"]["key_input"] == "stdin_json"
            # Revision 22: per-account Claude bypass-permissions warning.
            assert value["result"]["claude_bypass_warning"]["since_revision"] == 22
            assert value["result"]["claude_bypass_warning"]["new_accounts_default"] == "skip"
            assert value["result"]["claude_bypass_warning"]["credentials_read"] is False
            # Revision 20: new Space by label; Claude folder pre-trust for yolo starts.
            assert value["result"]["new_space"] == {**value["result"]["new_space"], "since_revision": 20, "resume": True, "add_to_herdr": True, "move_space": True}
            assert value["result"]["claude_folder_trust"]["setting"] == "claude_pretrust_folders"
            assert value["result"]["claude_folder_trust"]["built_in_default"] is True
            assert value["result"]["claude_folder_trust"]["credentials_read"] is False
            assert value["result"]["launch_permissions"]["values"] == ["yolo", "default"]
            assert value["result"]["launch_permissions"]["built_in_default"] == "yolo"
            validate(value["result"]["conversation_archive"]["native"], "NativeConversationArchive")
            assert value["result"]["conversation_archive"]["providers"] == ["codex", "claude", "gemini", "antigravity"]
            # Revision 25: Antigravity folder trust, stop, archive and delete.
            assert value["result"]["contract_revision"] >= 25
            assert value["result"]["antigravity_folder_trust"]["setting"] == "antigravity_pretrust_folders"
            assert value["result"]["antigravity_folder_trust"]["credentials_read"] is False
            assert value["result"]["antigravity_lifecycle"]["stop"]["sigkill"] is False
            for p in value["result"]["providers"]:
                # Revision 25: every provider, Antigravity included, supports archive and delete.
                assert p["operations"]["archive"]["supported"] is True and p["operations"]["delete_session"]["supported"] is True
            agy = [p for p in value["result"]["providers"] if p["id"] == "antigravity"][0]
            assert value["result"]["contract_revision"] >= 24 and agy["account_logout"] is False and agy["stop"] is True
            assert agy["input_delivery"]["herdr_agent"] == "agy" and agy["input_delivery"]["automatic_replay"] is False
            assert "esc" in agy["input_delivery"]["keys"]["never"] and "ctrl+c" in agy["input_delivery"]["keys"]["never"]
            assert value["result"]["antigravity_input_delivery"]["since_revision"] == 24
            assert value["result"]["claude_input_delivery"]["automatic_replay"] is False
            assert value["result"]["claude_input_delivery"]["hosted"]["implicit_bypass"] is False
            assert value["result"]["claude_input_delivery"]["peer_message"]["reply_requires_anchor"] is True
            validate(value["result"]["claude_input_delivery"]["steer"], "ClaudeSteerSemantics")
            assert value["result"]["claude_input_delivery"]["priority"]["steer"] == "next"
            assert "interrupted" in value["result"]["claude_input_delivery"]["states"]
            validate(value["result"]["claude_input_delivery"]["terminal_input"], "ClaudeTerminalInputDelivery")
            assert value["result"]["claude_input_delivery"]["preference"][:2] == ["hosted_stream_json", "terminal_input"]
            validate(value["result"]["gemini_input_delivery"], "GeminiTerminalInputDelivery")
            gemini = [p for p in value["result"]["providers"] if p["id"] == "gemini"][0]
            validate(gemini["input_delivery"], "GeminiTerminalInputDelivery")
            assert gemini["steer"] is True and gemini["input_delivery"]["automatic_replay"] is False
            assert "esc" in gemini["input_delivery"]["keys"]["never"] and "ctrl+c" in gemini["input_delivery"]["keys"]["never"]
            claude = [p for p in value["result"]["providers"] if p["id"] == "claude"][0]
            assert claude["account_removal"] is True and claude["account_deletion"] is True
            assert claude["headless_delivery"]["automatic_replay"] is False
            assert all(p["account_rename"] is True for p in value["result"]["providers"])
            assert value["result"]["checkpoints"]["version"] == 1
            assert value["result"]["checkpoints"]["max_response_bytes"] == 786432
            assert value["result"]["checkpoints"]["history_modes"] == ["legacy", "paginated"]
            assert value["result"]["checkpoints"]["delivery_evidence"] is False
            assert value["result"]["session_names"]["version"] == 1
            assert value["result"]["steering"]["version"] == 1
            assert value["result"]["steering"]["acceptance_only"] is False
            assert value["result"]["steering"]["reply_correlation"] == "after_steer_item"
            assert value["result"]["steering"]["reply_observation"]["items_view"] == "full"
            assert value["result"]["providers"][0]["steer"] is True
        if args == ["version"]:
            validate(value["result"], "ExecutableVersion")
            assert value["result"]["contract_version"].startswith("1." + str(value["result"]["contract_revision"]) + ".")
            validate({**value["result"], "contract_revision":99}, "ExecutableVersion")
    for ok in ["1", "1.25", "1.99.0"]:
        assert json.loads(subprocess.check_output(command + ["--api-version", ok, "version"], text=True))["ok"] is True
    invalid = subprocess.run(command + ["checkpoints"], input=json.dumps({**fixtures["checkpoint_request"], "version":99}), capture_output=True, text=True)
    assert invalid.returncode == 1
    assert json.loads(invalid.stderr)["error"]["code"] == "INVALID_REQUEST"
    batch=json.loads(subprocess.check_output(command + ["observations"], input=json.dumps({"api_version":1,"sessions":[],"policy":"cached_only"}), text=True))
    validate(batch)
    assert batch["result"]["results"] == []
    for args in [["--api-version", "99", "capabilities"], ["--api-version", "2.0.0", "capabilities"], ["rename", "--session", "missing", "--expected-generation", "stale", "--name", "x"], ["answer", "--session", "missing", "--expected-generation", "stale", "--request-id", "r", "--answers", "{}"]]:
        result = subprocess.run(command + args, capture_output=True, text=True)
        assert result.returncode in (1, 2)
        assert not result.stdout
        validate(json.loads(result.stderr))

# Real CLI output for Antigravity (fake agy, disposable data dir, daemon) must satisfy the schema.
import shutil, time, uuid
with tempfile.TemporaryDirectory() as directory:
    data = Path(directory) / "state"
    data.mkdir()
    bindir = Path(directory) / "bin"
    bindir.mkdir()
    fake = bindir / "agy"
    shutil.copy(root / "tests/support/fake-agy.sh", fake)
    fake.chmod(0o700)
    (data / "config.json").write_text(json.dumps({"antigravity_binary": str(fake), "codex_binary": "/nonexistent/codex", "claude_binary": "/nonexistent/claude", "gemini_binary": "/nonexistent/gemini"}))
    project = Path(directory) / "project"
    project.mkdir()
    binary = str(Path(os.environ.get("CARGO_TARGET_DIR", root / "target")) / "debug/sessionsation")
    env = {**os.environ, "XDG_RUNTIME_DIR": directory}
    for key in ["TMUX", "TMUX_PANE", "HERDR_ENV", "AGY_ACCOUNT_HOME"]:
        env.pop(key, None)
    cli = [binary, "--data-dir", str(data)]
    def api(*args, stdin=None):
        out = subprocess.run(cli + ["api", *args], input=stdin, capture_output=True, text=True, env=env)
        assert out.returncode == 0, (args, out.stderr)
        value = json.loads(out.stdout)
        validate(value)
        return value["result"]
    daemon = subprocess.Popen(cli + ["daemon", "serve"], stdin=subprocess.DEVNULL, stdout=subprocess.DEVNULL, stderr=subprocess.DEVNULL, env=env)
    try:
        for _ in range(200):
            if subprocess.run(cli + ["daemon", "status"], capture_output=True, env=env).returncode == 0:
                break
            time.sleep(0.05)
        added = api("account", "add", "antigravity:schema")
        assert added["provider"] == "antigravity"
        subprocess.run(cli + ["account", "login", "antigravity:schema"], check=True, capture_output=True, env=env)
        api("account", "refresh", "antigravity:schema")
        rows = api("account", "list")
        row = next(a for a in rows if a["id"] == "antigravity:schema")
        validate(row, "Account")
        message = str(uuid.uuid4())
        body = {"api_version": 1, "message_id": message, "mode": "new", "account_id": "antigravity:schema", "directory": str(project), "text": "hello", "expires_at": int(time.time()) + 600}
        api("send", stdin=json.dumps(body))
        receipt = None
        for _ in range(400):
            receipt = api("receipt", message)
            if receipt["state"] not in ("pending_local", "execution_started"):
                break
            time.sleep(0.05)
        assert receipt["provider"] == "antigravity" and receipt["account_id"] == "antigravity:schema", receipt
        validate(receipt, "ClaudeHeadlessReceipt")
        snapshot = api("state")
        validate(snapshot, "Snapshot")
        session = next(s for s in snapshot["sessions"] if s["provider"] == "antigravity")
        validate(session, "Session")
        # Operations and requests that name the account.
        refresh = api("account", "refresh", "antigravity:schema")
        validate(refresh, "AccountRefresh")
    finally:
        daemon.terminate()
        daemon.wait(timeout=10)
print("Contract schema, fixtures and CLI envelopes passed")
