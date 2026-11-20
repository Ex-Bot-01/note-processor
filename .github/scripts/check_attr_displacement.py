#!/usr/bin/env python3
"""Attribute-displacement fence.

Two insertions in one evening made Windows CI red, and neither was a platform mystery: a block of new code was
pasted at "the nearest `#[` before this function", which turned out to be an attribute belonging to the *neighbour*.
The gate then applied to the new item, the old item lost it, and Linux — where every gated helper exists anyway —
compiled happily.

Both failures left the same local trace, so both are checkable without a Windows machine:

  1. `#[cfg(...)]` followed by a doc comment. A cfg attribute documents nothing; if a `///` line sits under it, the
     attribute has been separated from the item it gates (or stolen from it).
  2. Two consecutive `#[cfg(...)]` lines on one item. Almost always a gate that arrived with pasted text landing on
     top of a gate that was already there — the shape of "this item now carries its neighbour's condition twice, and
     the neighbour carries none".

Usage: python3 .github/scripts/check_attr_displacement.py [repo]
"""

from __future__ import annotations

import pathlib
import sys

ATTRIBUTES = ("#[cfg(", "#[cfg(")


def main() -> int:
    problems: list[str] = []
    # Default to the repository holding this script, so it runs from any working directory.
    root = pathlib.Path(sys.argv[1]) if len(sys.argv) > 1 else pathlib.Path(__file__).resolve().parents[2]
    files = sorted((root / "crates").rglob("*.rs"))
    for path in files:
        lines = path.read_text().splitlines()
        for index, line in enumerate(lines):
            stripped = line.strip()
            if not stripped.startswith(ATTRIBUTES):
                continue
            following = lines[index + 1].strip() if index + 1 < len(lines) else ""
            if following.startswith("///") or following.startswith("//!"):
                problems.append(
                    f"{path}:{index + 1}: `{stripped}` is followed by a doc comment — the gate is no longer "
                    "attached to the item it gates"
                )
            elif following.startswith(ATTRIBUTES):
                problems.append(
                    f"{path}:{index + 1}: two `#[cfg]` attributes in a row — one of these probably belongs to the "
                    "item above, not this one"
                )
    if problems:
        print("\n".join(problems))
        print(
            "\nA cfg gate that drifted off its item is invisible on the platform where the gated code exists. "
            "Reattach each attribute to the item it was written for."
        )
        return 1
    print(f"attribute-displacement check: pass ({len(files)} files)")
    return 0


if __name__ == "__main__":
    sys.exit(main())
