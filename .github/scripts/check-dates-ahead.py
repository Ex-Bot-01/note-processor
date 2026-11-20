#!/usr/bin/env python3
"""Fail when a tracked file states a date that has not happened yet.

Why this exists: a stretch of this repository's prose was dated one month ahead of the work it described — a December
stamp on commits made in November, "while cutting v0.3.0" beside a tag dated 2026-11-09 — and nothing caught it, because
a wrong month reads exactly like a right one. Dates here come from `git log` or `date -u`; this is the fence that says so after the fact.

The rule is deliberately mechanical: any calendar date in a tracked file that falls after the HEAD commit's own date is
reported. A commit dated today makes "today" legal, which is what lets a document written today say today's date; it
cannot make tomorrow's date legal, and it cannot rescue a month that was simply typed wrong.

Dates that are not dates — hex colours, version-shaped numbers, a `sha` prefix — are excluded by requiring the string to
be an independent token *and* a real calendar day (month 01-12, day valid for that month and year).

Usage: python3 .github/scripts/check-dates-ahead.py [repo] [--head-date YYYY-MM-DD]
Exit 0 = nothing ahead of HEAD, 1 = offenders printed. `--head-date` overrides the comparison date for testing.
"""

from __future__ import annotations

import datetime as dt
import re
import subprocess
import sys
from pathlib import Path

DATE = re.compile(r"\b(\d{4})-(\d{2})-(\d{2})\b")


def head_date(root: Path) -> dt.date:
    raw = subprocess.run(
        ["git", "-C", str(root), "log", "-1", "--format=%cd", "--date=short"],
        capture_output=True, text=True, check=True,
    ).stdout.strip()
    return dt.date.fromisoformat(raw)


def tracked_dates(root: Path) -> list[tuple[str, int, dt.date]]:
    """Every real calendar date in every tracked file, as (path, line, date).

    `git grep` does the tree walk — it knows what is tracked and skips binaries (`-I`) — but its regex flavour has no
    `\\b` or `\\d`, so it is asked only for the shape `NNNN-NN-NN` and python does the precise matching here: word
    boundaries, and a real calendar day.
    """
    out = subprocess.run(
        ["git", "-C", str(root), "grep", "-InE", "-e", r"[0-9]{4}-[0-9]{2}-[0-9]{2}"],
        capture_output=True, text=True,
    ).stdout
    found: list[tuple[str, int, dt.date]] = []
    for line in out.splitlines():
        path, lineno, text = line.split(":", 2)
        for match in DATE.finditer(text):
            year, month, day = (int(g) for g in match.groups())
            try:
                found.append((path, int(lineno), dt.date(year, month, day)))
            except ValueError:
                continue  # a real calendar day is the only thing worth comparing
    return found


def main() -> int:
    args = [a for a in sys.argv[1:]]
    limit = None
    if "--head-date" in args:
        i = args.index("--head-date")
        limit = dt.date.fromisoformat(args[i + 1])
        del args[i:i + 2]
    root = Path(args[0]) if args else Path(__file__).resolve().parents[2]
    limit = limit or head_date(root)
    offenders = [(p, n, d) for p, n, d in tracked_dates(root) if d > limit]
    for path, lineno, day in sorted(set(offenders)):
        print(f"{path}:{lineno}: {day.isoformat()} is later than HEAD's own date {limit.isoformat()}")
    if offenders:
        print(
            "\nTake dates from `git log` or `date -u`. A date you meant as a guess belongs in a commit message, "
            "not in a tracked file."
        )
        print(f"DATE CHECK: fail — {len(set(offenders))} date(s) ahead of HEAD")
        return 1
    print(f"date check: pass (no tracked date is later than HEAD at {limit.isoformat()})")
    print("DATE CHECK: pass")
    return 0


if __name__ == "__main__":
    sys.exit(main())
