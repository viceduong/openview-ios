#!/usr/bin/env python3
"""Check relative links in the repository's Markdown files.

Verifies that every relative link target ([text](path), [text](path#anchor),
<img src="path">) exists, and that #anchors match a heading in the target
file (GitHub-style slugs). External links (http, https, mailto) are not
fetched.

    python scripts/check_links.py          # exit 1 if a link is broken
"""

from __future__ import annotations

import re
import sys
from pathlib import Path

ROOT = Path(__file__).resolve().parent.parent
SKIP_DIRS = {"target", ".git", "dist", "build", "DerivedData", "node_modules"}

LINK_RE = re.compile(r"(?<!!)\[[^\]]*\]\(([^)\s]+)(?:\s+\"[^\"]*\")?\)|!\[[^\]]*\]\(([^)\s]+)\)")
SRC_RE = re.compile(r"""<(?:img|a)\s[^>]*(?:src|href)=["']([^"']+)["']""")
HEADING_RE = re.compile(r"^(#{1,6})\s+(.*?)\s*#*\s*$")
FENCE_RE = re.compile(r"^\s*(```|~~~)")


def slug(heading: str) -> str:
    """GitHub's anchor for a heading."""
    text = re.sub(r"`([^`]*)`", r"\1", heading)
    text = re.sub(r"\[([^\]]*)\]\([^)]*\)", r"\1", text)
    text = text.strip().lower()
    text = re.sub(r"[^\w\- ]", "", text)
    return text.replace(" ", "-")


def anchors(path: Path, cache: dict[Path, set[str]]) -> set[str]:
    if path not in cache:
        found: set[str] = set()
        counts: dict[str, int] = {}
        in_fence = False
        for line in path.read_text(encoding="utf-8").splitlines():
            if FENCE_RE.match(line):
                in_fence = not in_fence
                continue
            if in_fence:
                continue
            m = HEADING_RE.match(line)
            if m:
                s = slug(m.group(2))
                n = counts.get(s, 0)
                counts[s] = n + 1
                found.add(s if n == 0 else f"{s}-{n}")
        cache[path] = found
    return cache[path]


def links(text: str):
    in_fence = False
    for lineno, line in enumerate(text.splitlines(), 1):
        if FENCE_RE.match(line):
            in_fence = not in_fence
            continue
        if in_fence:
            continue
        line = re.sub(r"`[^`]*`", "", line)  # inline code is not a link
        for m in LINK_RE.finditer(line):
            yield lineno, m.group(1) or m.group(2)
        for m in SRC_RE.finditer(line):
            yield lineno, m.group(1)


def main() -> int:
    cache: dict[Path, set[str]] = {}
    errors = 0
    files = [
        p
        for p in ROOT.rglob("*.md")
        if not any(part in SKIP_DIRS for part in p.relative_to(ROOT).parts)
    ]
    for md in sorted(files):
        for lineno, target in links(md.read_text(encoding="utf-8")):
            if re.match(r"^[a-z][a-z0-9+.-]*:", target, re.I):
                continue  # http:, https:, mailto:, ...
            path_part, _, anchor = target.partition("#")
            dest = md if not path_part else (md.parent / path_part).resolve()
            rel = md.relative_to(ROOT)
            if not dest.exists():
                print(f"{rel}:{lineno}: missing target {target}")
                errors += 1
                continue
            if anchor and dest.is_file() and dest.suffix == ".md":
                if anchor.lower() not in anchors(dest, cache):
                    print(f"{rel}:{lineno}: no heading for #{anchor} in {dest.relative_to(ROOT)}")
                    errors += 1
    print(f"checked {len(files)} Markdown files: {errors} broken link(s)")
    return 1 if errors else 0


if __name__ == "__main__":
    sys.exit(main())
