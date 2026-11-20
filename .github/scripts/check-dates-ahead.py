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


def run(root: Path, limit: dt.date | None = None) -> int:
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


def self_test() -> int:
    """Plant dates in a throwaway repository and require the fence to tell them apart.

    The cases are the three ways this rule could be wrong: blind to a future date, allergic to a past one, or reading a
    number shaped like a date as a calendar day.
    """
    import os
    import tempfile

    def commit(repo: Path, name: str, body: str) -> None:
        (repo / name).write_text(body, encoding="utf-8")
        env = dict(
            os.environ,
            GIT_AUTHOR_DATE="2026-01-01T00:00:00",
            GIT_COMMITTER_DATE="2026-01-01T00:00:00",
            GIT_AUTHOR_NAME="fence self-test", GIT_AUTHOR_EMAIL="fence@example.invalid",
            GIT_COMMITTER_NAME="fence self-test", GIT_COMMITTER_EMAIL="fence@example.invalid",
        )
        for command in (["git", "add", name], ["git", "-c", "commit.gpgsign=false", "commit", "-q", "-m", "fixture"]):
            subprocess.run(command, cwd=repo, env=env, capture_output=True, check=True)

    future = (dt.date.today() + dt.timedelta(days=400)).isoformat()
    cases = [
        ("a date after HEAD is named", "notes.md", f"Measured on {future}, which has not happened.\n", 1),
        ("a date before HEAD passes", "notes.md", "Measured on 2025-03-14, long ago.\n", 0),
        ("an impossible month is not a date", "notes.md", "Hash fragment 2026-13-45 is not a calendar day.\n", 0),
        ("an impossible day is not a date", "notes.md", "Counterfactual 2026-02-30 never existed.\n", 0),
    ]
    failures = 0
    with tempfile.TemporaryDirectory() as tmp:
        for label, name, body, expected in cases:
            repo = Path(tmp) / f"case-{label[:12].strip().replace(' ', '_')}"
            repo.mkdir(parents=True, exist_ok=True)
            subprocess.run(["git", "init", "-q"], cwd=repo, capture_output=True, check=True)
            commit(repo, name, body)
            status = run(repo, dt.date(2026, 6, 1))
            ok = status == expected
            print(f"{'ok  ' if ok else 'FAIL'} {label}: exit={status}, expected {expected}")
            failures += 0 if ok else 1
        # The comparison date is the commit's own, not today's: a checkout of an old commit must not fail on dates that
        # were current when it was written.
        repo = Path(tmp) / "case-head-date"
        repo.mkdir(parents=True)
        subprocess.run(["git", "init", "-q"], cwd=repo, capture_output=True, check=True)
        commit(repo, "notes.md", "Written 2026-01-01.\n")
        status = run(repo)
        ok = status == 0 and head_date(repo) == dt.date(2026, 1, 1)
        print(f"{'ok  ' if ok else 'FAIL'} HEAD's own date is the limit ({head_date(repo)}): exit={status}")
        failures += 0 if ok else 1
    print("date-fence self-test: " + ("pass" if not failures else f"fail — {failures} case(s)"))
    return 1 if failures else 0


if __name__ == "__main__":
    args = [a for a in sys.argv[1:]]
    if "--self-test" in args:
        sys.exit(self_test())
    limit = None
    if "--head-date" in args:
        i = args.index("--head-date")
        limit = dt.date.fromisoformat(args[i + 1])
        del args[i:i + 2]
    root = Path(args[0]) if args else Path(__file__).resolve().parents[2]
    sys.exit(run(root, limit))
