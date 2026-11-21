#!/usr/bin/env python3
"""CI check: fail if production Rust code uses bare `.unwrap()` or `.expect(`.

Banned by CONSTRAINTS.md §1 (non-test production paths only). Legitimate
`unwrap_or` / `unwrap_or_else` / `unwrap_or_default` are fine, and test-only
`.expect(` is fine.

Excludes:
- files under any `tests/` directory (integration tests)
- `build.rs` (build scripts)
- items inside a `#[cfg(test)]` module or item (tracked by brace depth)

Usage:
    python scripts/check_no_unwrap.py [crates-dir ...]
    (default: `crates` relative to the repo root, i.e. the script's parent)

Exit 0 = clean. Exit 1 = violations found, printed as `file:line:col: <text>`.
Runs on the GitHub Windows runner and locally on the PC (plain Python 3,
no third-party packages).
"""

from __future__ import annotations

import re
import sys
from pathlib import Path

CFG_TEST_RE = re.compile(r"#\[\s*cfg\s*\(\s*test\s*\)\s*\]")
# Bare `.unwrap()` / `.expect(` — note `.unwrap_or*` does NOT match because the
# character after `.unwrap` must be `(`.
VIOLATION_RE = re.compile(r"\.(?:unwrap\(\)|expect\()")


def _is_test_excluded(path: Path) -> bool:
    """True for files that are test code by location, not by content."""
    if path.name == "build.rs":
        return True
    return "tests" in path.parts


def scan_file(path: Path) -> list[tuple[int, int, str]]:
    """Return [(line, col, matched_text)] for violations in one file."""
    text = path.read_text(encoding="utf-8")
    violations: list[tuple[int, int, str]] = []

    # Character-level state machine. Tracks strings/comments so a
    # `.unwrap()` inside a comment or string literal is not flagged, and
    # tracks brace depth + #[cfg(test)] regions so test modules are skipped.
    i = 0
    n = len(text)
    line = 1
    col = 1
    depth = 0
    # Brace depths (after the opening `{`) of currently-open #[cfg(test)] items.
    test_depths: list[int] = []
    pending_test = False

    def advance(k: int = 1) -> None:
        nonlocal i, line, col
        for _ in range(k):
            if i >= n:
                return
            if text[i] == "\n":
                line += 1
                col = 1
            else:
                col += 1
            i += 1

    while i < n:
        ch = text[i]

        # --- strings / chars / comments (NORMAL state only reaches here) ---
        if ch == "/" and i + 1 < n and text[i + 1] == "/":
            while i < n and text[i] != "\n":
                advance()
            continue
        if ch == "/" and i + 1 < n and text[i + 1] == "*":
            advance(2)
            level = 1
            while i < n and level > 0:
                if text[i] == "/" and i + 1 < n and text[i + 1] == "*":
                    level += 1
                    advance(2)
                elif text[i] == "*" and i + 1 < n and text[i + 1] == "/":
                    level -= 1
                    advance(2)
                else:
                    advance()
            continue
        if ch == '"':
            # String literal (also covers raw strings r#"..."# by treating the
            # opening quote as the start; the `r#` prefix is harmless here).
            advance()
            while i < n:
                if text[i] == "\\":
                    advance(2)
                elif text[i] == '"':
                    advance()
                    break
                else:
                    advance()
            continue
        if ch == "'":
            # Char literal, or a lifetime `'a`. A lifetime is followed by an
            # identifier char / `>` / `,` / `)` / `=` / `:` etc. and never by a
            # closing `'` on the same token; treat `'x'` / `'\n'` as a char and
            # a bare lifetime as a single quote to skip.
            j = i + 1
            if j < n and text[j] == "\\":
                j += 2
            elif j < n:
                j += 1
            if j < n and text[j] == "'":
                advance(j - i + 1)
                continue
            advance()
            continue

        # --- #[cfg(test)] attribute ---
        if ch == "#":
            m = CFG_TEST_RE.match(text, i)
            if m:
                pending_test = True
                advance(m.end() - m.start())
                continue

        # --- braces / items ---
        if ch == "{":
            if pending_test:
                test_depths.append(depth + 1)
                pending_test = False
            depth += 1
            advance()
            continue
        if ch == "}":
            depth -= 1
            while test_depths and depth < test_depths[-1]:
                test_depths.pop()
            advance()
            continue
        if ch == ";":
            # Attribute was on a non-brace item (use/static/const): it does
            # not extend to a later `{`.
            pending_test = False
            advance()
            continue

        # --- violation match (only in production code) ---
        if ch == "." and not test_depths:
            m = VIOLATION_RE.match(text, i)
            if m:
                violations.append((line, col, m.group(0)))
                advance(m.end() - m.start())
                continue

        advance()

    return violations


def main(argv: list[str]) -> int:
    roots = [Path(a) for a in argv[1:]] or [Path(__file__).resolve().parent.parent / "crates"]
    violations: list[tuple[Path, int, int, str]] = []
    files_scanned = 0
    for root in roots:
        for path in sorted(root.rglob("*.rs")):
            if _is_test_excluded(path):
                continue
            files_scanned += 1
            for line, col, matched in scan_file(path):
                violations.append((path, line, col, matched))

    if violations:
        for path, line, col, matched in violations:
            print(f"{path}:{line}:{col}: forbidden `{matched}` in production code", file=sys.stderr)
        print(f"check_no_unwrap: {len(violations)} violation(s) in {files_scanned} file(s)", file=sys.stderr)
        return 1
    print(f"check_no_unwrap: clean ({files_scanned} Rust file(s) scanned)")
    return 0


if __name__ == "__main__":
    raise SystemExit(main(sys.argv))
