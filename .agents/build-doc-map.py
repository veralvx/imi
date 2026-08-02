#!/usr/bin/env python3
"""Map the project's markdown, then drive a doc review from the map.

Adapted from the map-project skill's build_map.py. The polarity is
inverted: there, code files are nodes and docs are one edge type; here the
docs ARE the nodes and code files are edge targets, because the question
this review answers is "what does this document claim about the code, and
is it still true?".

THREE PROPERTIES, in priority order — a wrong extraction pattern is
visible on the next read, a violated property is not:

  idempotent      running twice produces the same file (bar the timestamp)
  non-destructive review state and findings survive a rescan
  deriving        the checklist is generated, never hand-edited

Usage:
    python3 .agents/build-doc-map.py            # rescan and re-derive
    python3 .agents/build-doc-map.py --audit    # findings never disposed of
    python3 .agents/build-doc-map.py --selftest # verify the three properties
"""

from __future__ import annotations

import json
import pathlib
import re
import subprocess
import sys
from datetime import datetime, timezone
from typing import Any

ROOT = pathlib.Path(__file__).resolve().parent.parent
MAP = ROOT / ".agents" / "doc-map.json"
CHECKLIST = ROOT / ".agents" / "DOC-REVIEW.md"

PROJECT = "imi — documentation"
NODE_GLOB = "**/*.md"

# Excluded from the map. Every entry needs a REASON: an excluded file that
# nobody decided to exclude is indistinguishable from a forgotten one.
EXCLUDED: dict[str, str] = {
    "target": "build output; contains vendored dependency docs, not ours",
    ".git": "not project content",
    ".agents/DOC-REVIEW.md": (
        "this scanner's own output. Without this the map is not idempotent: "
        "run one writes the checklist, run two finds it and adds it as a node."
    ),
}

# What a doc node records about the code it discusses. These are the edges
# the review is for.
CODE_PATH_RE = re.compile(r"\b((?:crates|src|tests|\.agents)/[\w./-]+\.(?:rs|py|sh|toml|json))\b")
DOC_PATH_RE = re.compile(r"\b((?:\.agents/)?(?:docs/)?[\w./-]*\.md)\b")

# Rust items named in backticks. These must still exist, and this session
# has already found docs naming things that had been renamed away.
IDENT_RE = re.compile(r"`([A-Za-z_][A-Za-z0-9_]*(?:::[A-Za-z_][A-Za-z0-9_]*)*)`")

# Claims about the world outside this repo — kernel behaviour, POSIX,
# on-disk formats. Each needs a citable source, not an assertion.
EXTERNAL_RE = re.compile(
    r"\b(kernel|POSIX|udev|udisks2|systemd|ioctl|VPD|GPT|MBR|"
    r"blkdev_|bdev_|sysfs|O_EXCL|O_DIRECT|BLK[A-Z]+)\b"
)

Nodes = dict[str, dict[str, Any]]


def excluded(path: pathlib.Path) -> bool:
    """Match whole path segments, not string prefixes.

    A `startswith` here silently swallowed `.github/` under a `.git`
    exclusion — four templates absent from the map with nobody having
    decided to exclude them, which is the failure this dict exists to
    prevent.
    """
    rel = path.relative_to(ROOT).as_posix()
    parts = rel.split("/")
    return any(pat == rel or pat in parts for pat in EXCLUDED)


def classify(rel: str) -> str:
    """The `kind` field: which review pass applies to this document."""
    if rel.startswith(".github/") or "TEMPLATE" in rel:
        return "template"
    if rel.startswith(".agents/docs/threading-plan/"):
        return "design-deep-dive"
    if rel.startswith(".agents/docs/"):
        return "design-rationale"
    if rel == "AGENTS.md":
        return "agent-instructions"
    if rel == ".agents/REVIEW.md":
        return "process"
    if rel in {"README.md", "SECURITY.md", "CODE_OF_CONDUCT.md", "CONTRIBUTING.md"}:
        return "user-facing"
    if rel == "CHANGELOG.md":
        return "release-record"
    return "other"


def fenced_blocks(text: str) -> list[tuple[str, int]]:
    """Every fenced block as (language, line count)."""
    out = []
    # The info string may carry attributes — ```rust,no_run — so match to
    # end of line and take the language before the first comma. A `\w*`
    # here fails on those fences entirely and then pairs the *closing*
    # fence as an opening one, which undercounted rust blocks and
    # overcounted untagged ones in every doc that uses them.
    for m in re.finditer(r"^```([^\n]*)\n(.*?)^```", text, re.S | re.M):
        lang = m.group(1).split(",")[0].strip().lower()
        out.append((lang or "none", m.group(2).count("\n")))
    return out


def scan() -> Nodes:
    nodes: Nodes = {}
    # Every extension CODE_PATH_RE can match. Building this from *.rs
    # alone meant any document naming a manifest — crates/imi/Cargo.toml
    # — was reported as naming a path that does not exist, because the
    # regex matched it and the resolution set could not contain it.
    code_files = {
        p.relative_to(ROOT).as_posix()
        for ext in ("rs", "py", "sh", "toml", "json")
        for p in ROOT.rglob(f"*.{ext}")
        if "target" not in p.parts and ".git" not in p.parts
    }
    md_files = {
        p.relative_to(ROOT).as_posix()
        for p in ROOT.rglob(NODE_GLOB)
        if not excluded(p)
    }

    for path in sorted(ROOT.rglob(NODE_GLOB)):
        if excluded(path):
            continue
        rel = path.relative_to(ROOT).as_posix()
        text = path.read_text(encoding="utf-8", errors="replace")

        # Edges out: code this doc discusses, and other docs it names.
        # A bare `src/lib.rs` in a tree diagram is shorthand for a path
        # under whichever crate root the diagram showed above it. Resolve
        # it before calling it missing: the first version of this flagged
        # eight such paths in AGENTS.md, all of them legitimate, which
        # would have made the MISSING column noise by its first use.
        def resolve(m: str) -> str | None:
            if m in code_files:
                return m
            # Placeholder notation: `phase_N.rs` stands for phase_0..7.
            # Resolved rather than reported, because an audit that names
            # the same non-defect on every run teaches people to skim it.
            concrete = re.sub(r"_N\b|<[A-Z]>|\{[a-z]+\}", "_0", m)
            if concrete != m and resolve(concrete):
                return "(placeholder) " + m
            hits = [c for c in code_files if c.endswith("/" + m)]
            return hits[0] if len(hits) == 1 else None

        found = {m: resolve(m) for m in set(CODE_PATH_RE.findall(text))}
        code_refs = sorted({v for v in found.values() if v})
        code_missing = sorted({k for k, v in found.items() if v is None})
        doc_refs = sorted(
            {
                m
                for m in DOC_PATH_RE.findall(text)
                if m != rel and (m in md_files or any(d.endswith(m) for d in md_files))
            }
        )

        blocks = fenced_blocks(text)
        idents = sorted(set(IDENT_RE.findall(text)))

        nodes[rel] = {
            "path": rel,
            "kind": classify(rel),
            "lines": text.count("\n"),
            "code_refs": code_refs,
            "code_refs_missing": code_missing,
            "doc_refs": doc_refs,
            "referenced_by_code": [],
            "claims": {
                "fenced_blocks": len(blocks),
                "rust_blocks": sum(1 for lang, _ in blocks if lang in {"rust", "rs"}),
                "output_blocks": sum(1 for lang, _ in blocks if lang in {"none", "text", "console"}),
                "identifiers": len(idents),
                "external_terms": len(set(EXTERNAL_RE.findall(text))),
            },
            "identifiers": idents,
        }
    return nodes


def resolve_back_edges(nodes: Nodes) -> Nodes:
    """Which source files point a reader AT this document.

    A doc the code tells you to read is load-bearing in a way its own
    outbound links do not show.
    """
    for rel in nodes:
        nodes[rel]["referenced_by_code"] = []
    for src in ROOT.rglob("*.rs"):
        if "target" in src.parts:
            continue
        text = src.read_text(encoding="utf-8", errors="replace")
        srel = src.relative_to(ROOT).as_posix()
        for rel in nodes:
            tail = rel.split("/")[-1]
            if tail in text and rel != srel:
                nodes[rel]["referenced_by_code"].append(srel)
    for rel in nodes:
        nodes[rel]["referenced_by_code"] = sorted(set(nodes[rel]["referenced_by_code"]))
    return nodes


# Review order: "what must be understood first", per the skill's guidance
# for documentation — deliberately NOT link depth, because these documents
# barely link to each other while depending heavily on shared context.
#
#   0  the project's own account of itself; everything else elaborates it
#   1  the phase docs, in pipeline order — each assumes the one before
#   2  the threading plans, deep dives on phases already read
#   3  process and release records, which describe the whole
#   4  templates: "what does a stranger who copies this get?"
ORDER_TIERS: list[tuple[int, str]] = [
    (0, "AGENTS.md"),
    (0, "README.md"),
    (1, ".agents/docs/00-cli-and-ux.md"),
    (1, ".agents/docs/01-phase-0-preflight.md"),
    (1, ".agents/docs/02-phase-1-topology.md"),
    (1, ".agents/docs/03-phase-2-exclusive.md"),
    (1, ".agents/docs/04-phase-3-wipe.md"),
    (1, ".agents/docs/05-phase-4-flash.md"),
    (1, ".agents/docs/06-phase-5-verify.md"),
    (1, ".agents/docs/07-phase-6-kernel-sync.md"),
    (1, ".agents/docs/08-phase-7-automount.md"),
    (1, ".agents/docs/09-flashguard.md"),
    (1, ".agents/docs/10-aligned-and-ioctls.md"),
    (2, ".agents/docs/11-threading.md"),
    (3, "CHANGELOG.md"),
    (3, "CONTRIBUTING.md"),
    (3, "SECURITY.md"),
    (3, "CODE_OF_CONDUCT.md"),
    (3, ".agents/REVIEW.md"),
]


def compute_order(nodes: Nodes) -> Nodes:
    explicit = {path: i for i, (_, path) in enumerate(ORDER_TIERS)}
    rest = sorted(p for p in nodes if p not in explicit)
    for i, p in enumerate(rest):
        explicit[p] = len(ORDER_TIERS) + i
    for rel, node in nodes.items():
        node["order"] = explicit[rel]
    return nodes


def merge(fresh: Nodes, existing: pathlib.Path) -> tuple[Nodes, list[str]]:
    """Carry review state forward. THE property that makes rescanning safe."""
    if not existing.exists():
        for n in fresh.values():
            n.setdefault("reviewed", False)
            n.setdefault("findings", [])
        return fresh, []
    old = {f["path"]: f for f in json.loads(existing.read_text(encoding="utf-8"))["files"]}
    vanished: list[str] = []
    for rel, node in fresh.items():
        prev = old.get(rel, {})
        node["reviewed"] = prev.get("reviewed", False)
        node["findings"] = prev.get("findings", [])
        for carried in ("reviewed_at", "verdict", "coverage_note"):
            if carried in prev:
                node[carried] = prev[carried]
    for rel, prev in old.items():
        if rel not in fresh and (prev.get("reviewed") or prev.get("findings")):
            vanished.append(rel)
    return fresh, vanished


def write_map(nodes: Nodes, vanished: list[str]) -> dict[str, Any]:
    doc = {
        "generated_by": ".agents/build-doc-map.py",
        "generated_at": datetime.now(timezone.utc).strftime("%Y-%m-%dT%H:%M:%SZ"),
        "project": PROJECT,
        "node_kind": "markdown file",
        "edge_kinds": {
            "code_refs": "source files this document names by path — what it claims about",
            "code_refs_missing": "paths it names that DO NOT EXIST; each is a defect",
            "doc_refs": "other markdown files it names",
            "referenced_by_code": "source files whose comments send a reader here",
        },
        "claim_kinds": {
            "fenced_blocks": "total fenced blocks; each is a claim about something",
            "rust_blocks": "blocks purporting to be this project's Rust — must match the source",
            "output_blocks": "blocks purporting to be terminal output — must match a real run",
            "identifiers": "backticked Rust items that must still exist",
            "external_terms": "claims about kernel/POSIX/format behaviour needing a citable source",
        },
        "excluded": EXCLUDED,
        "order_strategy": (
            "what must be understood first, in five tiers: the project's own account of "
            "itself, then the phase docs in pipeline order, then the threading deep-dives, "
            "then process and release records, then templates. Deliberately NOT link depth: "
            "these documents barely link to each other while depending heavily on shared "
            "context, so link depth would order them almost arbitrarily."
        ),
        "review_focus": (
            "Verify each document against the code it describes. This session's code review "
            "already found four kinds of drift — a quoted bar template with the wrong "
            "spacing, quoted ioctl snippets that no longer matched, a paraphrase of --help "
            "presented as output, and a phrase naming a dependency the library no longer has "
            "— so quoted code, quoted output, named identifiers and named paths are the four "
            "things to check first."
        ),
        "vanished": vanished,
        "files": [nodes[k] for k in sorted(nodes, key=lambda p: nodes[p]["order"])],
    }
    MAP.write_text(json.dumps(doc, indent=2) + "\n", encoding="utf-8")
    return doc


def write_checklist(doc: dict[str, Any]) -> None:
    files = doc["files"]
    done = sum(1 for f in files if f["reviewed"])
    lines = [
        "# Documentation review checklist",
        "",
        f"<!-- GENERATED by {doc['generated_by']}. Do not edit: regenerated on every run,",
        "     and a tick added by hand disagrees with the map until it vanishes. -->",
        "",
        f"Progress: **{done} / {len(files)}** reviewed.",
        "",
        f"Order: {doc['order_strategy']}",
        "",
        "| # | doc | kind | lines | code refs | rust | output | idents | ext | done |",
        "|---|---|---|---|---|---|---|---|---|---|",
    ]
    for f in files:
        c = f["claims"]
        miss = f" **+{len(f['code_refs_missing'])} MISSING**" if f["code_refs_missing"] else ""
        lines.append(
            f"| {f['order']} | `{f['path']}` | {f['kind']} | {f['lines']} | "
            f"{len(f['code_refs'])}{miss} | {c['rust_blocks']} | {c['output_blocks']} | "
            f"{c['identifiers']} | {c['external_terms']} | {'x' if f['reviewed'] else ' '} |"
        )
    # A finding is open unless its severity says otherwise. "Verified"
    # belongs in the closed set: those are things checked and found
    # correct, recorded so they are not re-checked. Leaving them out of
    # this predicate buried the real open questions under thirty
    # verified-OKs, which is the failure a list of open items exists to
    # prevent.
    closed = ("fixed", "withdrawn", "verified", "resolved", "skipped")
    open_f = [
        (f["path"], fd)
        for f in files
        for fd in f["findings"]
        if not any(c in str(fd.get("severity", "")).lower() for c in closed)
    ]
    if open_f:
        lines += ["", "## Findings still open", ""]
        for p, fd in open_f:
            lines.append(f"- `{p}` [{fd.get('severity','?')}] {fd.get('what','')[:110]}")
    CHECKLIST.write_text("\n".join(lines) + "\n", encoding="utf-8")


def audit(doc: dict[str, Any]) -> int:
    """Findings never disposed of, and paths that do not exist."""
    bad = 0
    for f in doc["files"]:
        for fd in f["findings"]:
            if not fd.get("disposition"):
                print(f"  no disposition: {f['path']} [{fd.get('id','?')}] {fd.get('what','')[:70]}")
                bad += 1
        seen: dict[str, int] = {}
        for fd in f["findings"]:
            fid = str(fd.get("id", ""))
            seen[fid] = seen.get(fid, 0) + 1
        for fid, k in seen.items():
            if k > 1:
                print(f"  duplicate finding id: {f['path']} [{fid}] x{k}")
                bad += 1
        for m in f["code_refs_missing"]:
            print(f"  names a path that does not exist: {f['path']} -> {m}")
            bad += 1
    if doc.get("vanished"):
        for v in doc["vanished"]:
            print(f"  reviewed doc vanished from the tree: {v}")
            bad += 1
    print(f"  {'issues: ' + str(bad) if bad else 'clean'}")
    return bad


def selftest() -> int:
    """The three properties, verified rather than asserted."""
    ok = 0
    before = MAP.read_text(encoding="utf-8") if MAP.exists() else None
    try:
        d1 = main(quiet=True)
        a = json.loads(MAP.read_text(encoding="utf-8"))
        d2 = main(quiet=True)
        b = json.loads(MAP.read_text(encoding="utf-8"))
        a.pop("generated_at"), b.pop("generated_at")
        assert a == b, "not idempotent"
        print("  ok    idempotent (ignoring the timestamp)")
        ok += 1

        assert len({f["order"] for f in d1["files"]}) == len(d1["files"]), "duplicate order"
        print("  ok    every document has a unique order")
        ok += 1

        # non-destructive: plant state, rescan, check it survived
        raw = json.loads(MAP.read_text(encoding="utf-8"))
        raw["files"][0]["reviewed"] = True
        raw["files"][0]["findings"] = [{"id": "T1", "what": "planted", "disposition": "test"}]
        MAP.write_text(json.dumps(raw, indent=2) + "\n", encoding="utf-8")
        main(quiet=True)
        back = json.loads(MAP.read_text(encoding="utf-8"))
        kept = next(f for f in back["files"] if f["path"] == raw["files"][0]["path"])
        assert kept["reviewed"] and kept["findings"], "review state lost on rescan"
        print("  ok    review state and findings survive a rescan")
        ok += 1

        assert "Do not edit" in CHECKLIST.read_text(encoding="utf-8")
        print("  ok    checklist names its source and forbids editing")
        ok += 1
    finally:
        if before is not None:
            MAP.write_text(before, encoding="utf-8")
            main(quiet=True)
    print(f"  {ok}/4 properties hold")
    return 0 if ok == 4 else 1


def main(quiet: bool = False) -> dict[str, Any]:
    nodes = resolve_back_edges(compute_order(scan()))
    nodes, vanished = merge(nodes, MAP)
    doc = write_map(nodes, vanished)
    write_checklist(doc)
    if not quiet:
        done = sum(1 for f in doc["files"] if f["reviewed"])
        print(f"  doc-map: {len(doc['files'])} documents, {done} reviewed")
    return doc


if __name__ == "__main__":
    if "--selftest" in sys.argv:
        sys.exit(selftest())
    d = main()
    if "--audit" in sys.argv:
        sys.exit(1 if audit(d) else 0)
