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
            assert value["result"]["contract_revision"] >= 13
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
            assert value["result"]["steering"]["acceptance_only"] is True
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
