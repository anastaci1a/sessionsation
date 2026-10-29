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
validate(fixtures["resume_receipt"], "ResumeReceipt")
validate(fixtures["error"])
validate(fixtures["capabilities"])
for name, definition in [("session", "Session"), ("session_host_label", "Session"), ("snapshot", "Snapshot"), ("readiness", "Readiness"), ("readiness_stopped", "Readiness"), ("readiness_steer", "Readiness")]:
    validate(fixtures[name], definition)
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
        if args == ["placements"]:
            validate(value["result"], "Placements")
        if args == ["capabilities"]:
            assert value["result"]["contract_revision"] >= 18
            validate(value["result"]["conversation_archive"]["native"], "NativeConversationArchive")
            assert value["result"]["conversation_archive"]["providers"] == ["codex", "claude", "gemini"]
            for p in value["result"]["providers"]:
                assert p["operations"]["archive"]["supported"] is True and p["operations"]["delete_session"]["supported"] is True
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
            validate({**value["result"], "contract_revision":99}, "ExecutableVersion")
    invalid = subprocess.run(command + ["checkpoints"], input=json.dumps({**fixtures["checkpoint_request"], "version":99}), capture_output=True, text=True)
    assert invalid.returncode == 1
    assert json.loads(invalid.stderr)["error"]["code"] == "INVALID_REQUEST"
    batch=json.loads(subprocess.check_output(command + ["observations"], input=json.dumps({"api_version":1,"sessions":[],"policy":"cached_only"}), text=True))
    validate(batch)
    assert batch["result"]["results"] == []
    for args in [["--api-version", "99", "capabilities"], ["rename", "--session", "missing", "--expected-generation", "stale", "--name", "x"]]:
        result = subprocess.run(command + args, capture_output=True, text=True)
        assert result.returncode in (1, 2)
        assert not result.stdout
        validate(json.loads(result.stderr))
print("Contract schema, fixtures and CLI envelopes passed")
