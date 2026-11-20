#!/usr/bin/env python3
"""Flag items that are dead code on a platform this machine does not compile for.

Why this exists: eight consecutive CI runs failed on `windows-latest` with
`function ... is never used  [-D warnings]` while every local gate was green. The reason is structural —
a helper called only from `#[cfg(unix)]` tests is perfectly live here and dead there, and no Linux command
can see it. Cross-compiling `cargo check --target x86_64-pc-windows-msvc` does not work in this workspace
either, because `libsqlite3-sys`'s build script needs a Windows `lib.exe`.

So the cheap static proxy runs here instead: for every function or const that is not itself `#[cfg(...)]`
gated, check whether *all* of its call sites sit inside gated items. If they do, it is dead wherever that
cfg is off.

Heuristic, not a compiler: it follows lexical spans rather than the module tree, so an item referenced only
from a gated `mod { ... }` (rather than a gated function) can be missed, and a reference through a macro or
a function pointer is invisible. It exists to catch the common shape — a test helper added next to a
`#[cfg(unix)]` test — not to replace CI. Run it before pushing; run the CI watch after.

Usage: python3 .github/scripts/check_cfg_dead_code.py [repo]
Exit 0 = nothing flagged, 1 = flagged items printed.
"""

from __future__ import annotations

import glob
import pathlib
import re
import sys

DEF = re.compile(r"^\s*(?:(pub)(?:\((crate)\))?\s+)?(?:const |fn )(\w+)")
GATED = re.compile(r"#\[cfg\(([^)]*)\)")


def spans(lines: list[str]) -> list[tuple[str, int, int, str | None]]:
    """`(name, first_line, last_line, gate)` for every fn/const, 0-indexed and inclusive.

    `gate` is the predicate of an immediately preceding `#[cfg(...)]`, or None. The predicate matters: an
    item called from one `#[cfg(windows)]` function and one `#[cfg(unix)]` function compiles everywhere,
    while one called only from `#[cfg(unix)]` code is dead on Windows. Grouping by predicate rather than by
    "is gated" is what keeps the check from crying wolf at `config.rs`'s per-platform helpers."""
    found: list[tuple[str, int, int, str | None]] = []
    index = 0
    while index < len(lines):
        match = DEF.match(lines[index])
        if not match:
            index += 1
            continue
        depth = 0
        end = index
        while end < len(lines):
            depth += lines[end].count("{") - lines[end].count("}")
            if depth <= 0 and end > index:
                break
            end += 1
        gates = [m.group(1) for line in lines[max(0, index - 4) : index] if (m := GATED.search(line))]
        # A plain `pub` item is exported API: rustc never reports it dead, so flagging it would be noise.
        public = bool(match.group(1)) and not match.group(2)
        if public:
            index = index + 1
            continue
        found.append((match.group(3), index, end, gates[-1] if gates else None))
        index = max(end + 1, index + 1)
    return found


def audit(path: pathlib.Path) -> list[str]:
    lines = path.read_text().splitlines()
    items = spans(lines)
    problems: list[str] = []
    for name, start, end, gate in items:
        if gate or name == "main":
            continue
        call = re.compile(r"\b" + re.escape(name) + r"\s*[(<]")
        sites = [i for i, line in enumerate(lines) if call.search(line) and not start <= i <= end]
        if not sites:
            continue
        hosts = [item for site in sites for item in items if item[1] < site <= item[2]]
        # Dead only when every call site is gated **and** they all share one predicate: with `unix` on one
        # side and `windows` on the other, some platform always compiles a caller.
        covered = [item[3] for item in hosts if item[3]]
        predicates = set(covered)
        if len(covered) == len(sites) and len(predicates) == 1:
            problems.append(
                f"{path}:{start + 1}: `{name}` is called only from #[cfg({predicates.pop()})] code"
            )
    return problems


def main() -> int:
    # Default to the repository holding this script, so it runs from any working directory.
    root = pathlib.Path(sys.argv[1]) if len(sys.argv) > 1 else pathlib.Path(__file__).resolve().parents[2]
    files = sorted(glob.glob(str(root / "crates" / "*" / "src" / "*.rs")))
    files += sorted(glob.glob(str(root / "crates" / "*" / "tests" / "*.rs")))
    problems = [problem for path in map(pathlib.Path, files) for problem in audit(path)]
    for problem in problems:
        print(problem)
    if problems:
        print(
            "\nGate the item with the same #[cfg(...)] as its callers, or add a non-gated user. "
            "CI compiles windows-latest with -D warnings; this machine does not."
        )
        return 1
    print(f"cfg dead-code check: pass ({len(files)} files)")
    return 0


if __name__ == "__main__":
    sys.exit(main())
