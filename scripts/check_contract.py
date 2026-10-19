"""Validate the published schema, fixtures and real CLI envelopes offline."""
import json
import os
from pathlib import Path
import subprocess
import tempfile
from jsonschema import Draft202012Validator, FormatChecker

root = Path(__file__).resolve().parents[1]
schema = json.loads((root / "docs/contract/v1.json").read_text())
Draft202012Validator.check_schema(schema)
checker = FormatChecker()
def validate(value, definition=None):
    spec = schema if definition is None else {"$ref": f"#/$defs/{definition}", "$defs": schema["$defs"]}
    Draft202012Validator(spec, format_checker=checker).validate(value)

fixtures = json.loads((root / "docs/contract/fixtures.json").read_text())
validate(fixtures["request"], "MessageRequest")
validate(fixtures["space_choices"], "SpaceChoices")
validate(fixtures["space_move_operation"], "SpaceMoveOperation")
validate(fixtures["archived_session"], "ArchivedSession")
validate(fixtures["conversation_operation"], "ConversationOperation")
validate(fixtures["account_deletion"], "AccountDeletion")
validate(fixtures["account_deletion_preview"], "AccountDeletionPreview")
validate(fixtures["launch_request"], "LaunchRequest")
validate(fixtures["launch_receipt"], "LaunchReceipt")
validate(fixtures["resume_request"], "ResumeRequest")
validate(fixtures["resume_directory_request"], "ResumeRequest")
validate(fixtures["resume_receipt"], "ResumeReceipt")
validate(fixtures["error"])
validate(fixtures["capabilities"])
for name, definition in [("session", "Session"), ("snapshot", "Snapshot"), ("readiness", "Readiness"), ("readiness_stopped", "Readiness")]:
    validate(fixtures[name], definition)
for name, value in fixtures.items():
    if name.startswith("receipt_"):
        validate(value, "Receipt")
with tempfile.TemporaryDirectory() as directory:
    command = [str(Path(os.environ.get("CARGO_TARGET_DIR", root / "target")) / "debug/sessionsation"), "--data-dir", directory, "api"]
    for args in [["version"], ["capabilities"], ["state"], ["state", "--archive-filter", "all"], ["archives"]]:
        value = json.loads(subprocess.check_output(command + args, text=True))
        validate(value)
        if args[0] == "state":
            validate(value["result"], "Snapshot")
        if args == ["version"]:
            validate(value["result"], "ExecutableVersion")
    for args in [["--api-version", "99", "capabilities"], ["rename", "--session", "missing", "--expected-generation", "stale", "--name", "x"]]:
        result = subprocess.run(command + args, capture_output=True, text=True)
        assert result.returncode in (1, 2)
        assert not result.stdout
        validate(json.loads(result.stderr))
print("Contract schema, fixtures and CLI envelopes passed")
