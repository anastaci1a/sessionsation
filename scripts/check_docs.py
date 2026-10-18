"""Validate local Markdown links without reading private state."""
from pathlib import Path
import re

root = Path(__file__).resolve().parents[1]
errors = []
for path in [root / "README.md", root / "AGENTS.md", *sorted((root / "agents").rglob("*.md")), *sorted((root / "docs").rglob("*.md"))]:
    for target in re.findall(r"\[[^\]]*\]\(([^)]+)\)", path.read_text()):
        target = target.split("#", 1)[0]
        if target and not re.match(r"[a-zA-Z]+:", target) and not (path.parent / target).is_file():
            errors.append(f"{path.relative_to(root)}: missing {target}")
if errors:
    raise SystemExit("\n".join(errors))
print("Documentation links passed")
