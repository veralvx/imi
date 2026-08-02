#!/usr/bin/env python3
"""Regenerate REVIEW.md's checklist and progress count from review-map.json.

Run after marking a file reviewed, in the same commit as the review.
"""
import json
import pathlib

root = pathlib.Path(__file__).resolve().parent
data = json.loads((root / "review-map.json").read_text())
files = sorted(data["files"], key=lambda e: e["order"])
done = sum(1 for e in files if e["reviewed"])

rows = ["| # | file | stakes | lines | refs | done |", "|---|---|---|---|---|---|"]
for e in files:
    rows.append(
        f"| {e['order']} | `{e['path']}` | {e['stakes']} | {e['lines']} | "
        f"{len(e['referenced_by_rust'])} | {'x' if e['reviewed'] else ' '} |"
    )

md = (root / "REVIEW.md").read_text()
begin, end = "<!-- CHECKLIST:BEGIN -->", "<!-- CHECKLIST:END -->"
md = (
    md[: md.index(begin) + len(begin)]
    + "\n<!-- regenerated from .agents/review-map.json -->\n\n"
    + "\n".join(rows)
    + "\n\n"
    + md[md.index(end) :]
)
old = md[md.index("Progress: **") : md.index("** reviewed.") + len("** reviewed.")]
md = md.replace(old, f"Progress: **{done} / {len(files)}** reviewed.", 1)
(root / "REVIEW.md").write_text(md)
print(f"checklist regenerated: {done}/{len(files)} reviewed")
