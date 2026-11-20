#!/usr/bin/env python3
"""Attribute-displacement fence: a `#[cfg]` that ended up on the wrong item.

Why this exists: `docs/archive/roadmap-2026-10-27.md` records this class making Windows CI red twice, both times after every
local gate was green. The shape each time was an insertion made at "the nearest `#[` before this function", which turned
out to be an attribute belonging to the *neighbour*: the new item inherited a gate it never asked for, the old item lost
the gate it depended on, and Linux — where every gated helper exists anyway — compiled happily.

Both failures leave the same local trace, so both are checkable without a Windows machine:

  1. A `#[cfg(...)]` line with a doc comment under it. Rust accepts this (a doc comment is an attribute too, so the gate
     still applies to the item below), which is why the compiler cannot help — but in this repository a doc comment goes
     *above* its gate, and the paste that puts one underneath is the same paste that separates a gate from its item. The
     convention is what makes the mistake visible, so the fence enforces the convention.
  2. Two `#[cfg(...)]` lines stacked on one item. Usually the pasted gate arriving on top of the gate already there: this
     item now carries its neighbour's condition as well as its own, and the neighbour carries none. A deliberately
     two-part condition should be written once with `#[cfg(all(…, …))]`, which also makes it readable.

Usage: python3 .github/scripts/check_attr_displacement.py [repo]
Exit 0 = nothing flagged, 1 = offenders printed. `--self-test` runs the planted-violation cases in this file and needs no
repository.
"""

from __future__ import annotations

import pathlib
import sys
import tempfile

ATTRIBUTES = ("#[cfg(",)

CLEAN = '''//! A gate above its item, a doc comment above that: both shapes this repository uses.
/// Reads the free space under one platform.
#[cfg(unix)]
fn available_bytes() -> u64 { 0 }

fn untouched() {}
'''

DOC_UNDER_GATE = '''#[cfg(unix)]
/// This doc comment arrived with a paste, under the gate.
fn displaced() {}
'''

STACKED = '''#[cfg(unix)]
#[cfg(target_pointer_width = "64")]
fn stacked_gates() {}
'''


def problems_in(path: pathlib.Path) -> list[str]:
    problems: list[str] = []
    lines = path.read_text(encoding="utf-8").splitlines()
    for index, line in enumerate(lines):
        stripped = line.strip()
        if not stripped.startswith(ATTRIBUTES):
            continue
        following = lines[index + 1].strip() if index + 1 < len(lines) else ""
        if following.startswith(("///", "//!")):
            problems.append(
                f"{path}:{index + 1}: `{stripped}` sits above a doc comment — this repository writes the doc comment "
                "first, so a paste that put it underneath is also the paste that moved the gate"
            )
        elif following.startswith(ATTRIBUTES):
            problems.append(
                f"{path}:{index + 1}: two `#[cfg]` attributes stacked — one of these probably belongs to the item "
                "above; a deliberate pair belongs in one `#[cfg(all(…, …))]`"
            )
    return problems


def collect_files(root: pathlib.Path) -> list[pathlib.Path]:
    return sorted((root / "crates").rglob("*.rs"))


def run(root: pathlib.Path) -> int:
    files = collect_files(root)
    if not files:
        # A fence that inspected nothing must not report green: `gates.sh` says the difference between "green" and
        # "green but empty" is the one thing a gate list may never lose.
        print(f"no .rs files under {root / 'crates'} — is that the repository root?")
        return 1
    problems = [problem for path in files for problem in problems_in(path)]
    for problem in problems:
        print(problem)
    if problems:
        print(
            "\nA cfg gate that drifted off its item is invisible on the platform where the gated code exists. "
            "Reattach each attribute to the item it was written for."
        )
        return 1
    print(f"attribute-displacement check: pass ({len(files)} files)")
    return 0


def self_test() -> int:
    """One planted violation per arm, plus the shape that must stay clean and the empty tree."""
    cases = [
        ("clean", CLEAN, 0),
        ("doc comment under a gate", DOC_UNDER_GATE, 1),
        ("two stacked gates", STACKED, 1),
    ]
    failures = 0
    with tempfile.TemporaryDirectory() as tmp:
        root = pathlib.Path(tmp)
        for label, source, expected in cases:
            target = root / "crates" / "x" / "src"
            target.mkdir(parents=True, exist_ok=True)
            (target / "lib.rs").write_text(source, encoding="utf-8")
            found = problems_in(target / "lib.rs")
            status = 1 if found else 0
            ok = status == expected and (bool(found) == (expected == 1))
            print(f"{'ok  ' if ok else 'FAIL'} {label}: {len(found)} problem(s), expected {'a flag' if expected else 'silence'}")
            failures += 0 if ok else 1
        empty = root.parent / "empty-tree"
        empty.mkdir(exist_ok=True)
        ok = run(empty) == 1
        print(f"{'ok  ' if ok else 'FAIL'} a tree with no .rs files exits 1 rather than printing pass")
        failures += 0 if ok else 1
    print("attribute-displacement self-test: " + ("pass" if not failures else f"fail — {failures} case(s)"))
    return 1 if failures else 0


if __name__ == "__main__":
    if "--self-test" in sys.argv[1:]:
        sys.exit(self_test())
    root = pathlib.Path(sys.argv[1]) if len(sys.argv) > 1 else pathlib.Path(__file__).resolve().parents[2]
    sys.exit(run(root))
