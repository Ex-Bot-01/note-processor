#!/usr/bin/env python3
"""What this tree links, checked offline: licenses, and one copy of each major version.

Why a fence in the repository rather than only `cargo deny` in CI: the hosted checks need network access to an advisory
database and a license index, so on any machine without one they are silent. This script reads `Cargo.lock` and
`cargo metadata --offline`, which work with nothing but the vendored registry — so "the dependency rules held" is a claim you
can make about a tree you are standing in, not only about a run someone else triggered.

Three rules:

1. **Every dependency offers a permissive license.** `np` is MIT and ships static binaries, so a dependency whose *only*
   option is copyleft is a distribution question, not a build detail. A crate that offers MIT alongside something else (the
   common `MIT OR Apache-2.0 OR LGPL-2.1-or-later`) is fine — the distributor picks — but it is named in the generated report
   so a human sees which option was picked and why. `THIRD_PARTY_NOTICES.md` says this check belongs in tooling rather than
   in prose; this is that tooling.
2. **One major version per crate**, unless an exception is recorded with a reason. Five duplicates exist today, each listed in
   `DUPLICATE_EXCEPTIONS` below with what pulls it; an unlisted duplicate fails, because the usual cause is two direct
   dependencies drifting apart for no reason.
3. **The generated report is current.** `docs/dependency-licenses.md` is written by `--write-report`; without that flag the
   script compares and fails on drift, which is what stops "derived, not hand-written" from meaning "nobody regenerated it
   since March".

`--self-test` plants one violation per rule in a throwaway metadata file and requires each to be named.
"""

import argparse
import collections
import json
import os
import subprocess
import shutil
import sys
import tempfile

# Permissive, in the sense that matters for shipping a static binary: no obligation runs into `np`'s own source.
PERMISSIVE = {
    "MIT", "Apache-2.0", "ISC", "Zlib", "Unlicense", "BSD-2-Clause", "BSD-3-Clause",
    "Unicode-3.0", "Unicode-DFS-2016", "CDLA-Permissive-2.0", "CC0-1.0", "MulanPSL-2.0",
}
# An `OR` of these plus something else is still a free choice; the report names them so the choice is visible.
NOTABLE_MIXED = ("LGPL", "GPL")

DUPLICATE_EXCEPTIONS = {
    # crate -> why two majors are linked, and what would remove one.
    "getrandom": "0.4.x is the newer line some async/uuid dependencies require while 0.2.x is still pinned by others; "
                 "both are only used for entropy, so no state is shared between them.",
    "hashbrown": "one major comes from the indexmap line, the other from a transitive dependency that moved first; neither "
                 "table crosses the boundary between them.",
    "webpki-roots": "a root-certificate table is data, and two generations of it are pulled by different TLS stacks; only "
                    "one stack is used per request.",
    "windows-sys": "the Windows bindings are generated per API set and version; two majors coexist without a single symbol "
                   "resolving ambiguously because the modules differ.",
    "winnow": "two parser generations, one per parent crate, each compiled into its own parser.",
}

REPORT_HEADING = "# Dependency licenses, as derived from `Cargo.lock`"


def metadata(root):
    """`cargo metadata --offline`, or a clear failure. Never a silent empty list."""
    out = subprocess.run(["cargo", "metadata", "--offline", "--format-version", "1",
                          "--manifest-path", os.path.join(root, "Cargo.toml")],
                         capture_output=True, text=True)
    if out.returncode != 0:
        raise SystemExit(f"check-deps: `cargo metadata --offline` failed (no vendored registry?):\n{out.stderr.strip()[:800]}")
    return json.loads(out.stdout)


def _split_top(expr, keyword):
    """Split on `keyword` only outside parentheses."""
    parts, depth, current, i = [], 0, [], 0
    needle = f" {keyword} "
    while i < len(expr):
        if expr.startswith(needle, i) and depth == 0:
            parts.append("".join(current)); current = []; i += len(needle); continue
        c = expr[i]
        if c == "(":
            depth += 1
        elif c == ")":
            depth -= 1
        current.append(c); i += 1
    parts.append("".join(current))
    return [x.strip() for x in parts if x.strip()]


def _outer_parens(expr):
    while expr.startswith("(") and expr.endswith(")"):
        depth = 0
        for i, c in enumerate(expr):
            if c == "(":
                depth += 1
            elif c == ")":
                depth -= 1
                if depth == 0 and i != len(expr) - 1:
                    return expr
        expr = expr[1:-1].strip()
    return expr


def groups(expression):
    """The expression as OR-groups of licenses that apply **together** (disjunctive normal form).

    Deliberately not an SPDX parser, but it does distribute `(A OR B) AND C` into `{A,C}` and `{B,C}`, because "one of these
    pairs applies" is the question a distributor has to answer. Handles `OR`, `AND`, parentheses, `WITH` and the `MIT/Apache-2.0`
    slash shorthand — the whole vocabulary in this tree's 150 packages. An expression it cannot read classifies as unknown and
    fails, so an unreadable license is a loud problem rather than a silent pass.
    """
    expr = _outer_parens((expression or "").strip())
    if not expr:
        return []
    ors = _split_top(expr, "OR")
    if len(ors) > 1:
        return [g for part in ors for g in groups(part)]
    ands = _split_top(expr, "AND")
    if len(ands) > 1:
        combos = [frozenset()]
        for part in ands:
            sub = groups(part)
            combos = [c | other for c in combos for other in (sub or [frozenset()])]
        return combos
    if "/" in expr:
        return groups(expr.replace("/", " OR "))
    leaf = expr.split(" WITH ")[0].strip()
    return [frozenset([leaf])] if leaf else []


def split_options(expression):
    return sorted({x for g in groups(expression) for x in g})


def classify(expression):
    """permissive | mixed | copyleft-only | unknown.

    `OR` offers a choice; `AND` means both apply. So a group is safe only when **every** license in it is permissive, and the
    expression is safe when **some** group is — which is the distributor's actual freedom. A group that mixes (`GPL-3.0 AND
    MIT`) is not rescued by its permissive half, because both halves bind.
    """
    gs = groups(expression)
    if not gs:
        return "unknown", []
    options = sorted({x for g in gs for x in g})
    safe = [g for g in gs if all(x in PERMISSIVE for x in g)]
    if not safe:
        return "copyleft-only", options
    return ("permissive" if len(safe) == len(gs) else "mixed"), options


def check(root, meta=None):
    data = meta or metadata(root)
    problems, rows = [], []
    for pkg in data.get("packages", []):
        name, version, lic = pkg["name"], pkg["version"], pkg.get("license") or ""
        kind, options = classify(lic)
        rows.append((name, version, lic or "(none)", kind))
        if kind == "copyleft-only":
            problems.append(f"{name} {version}: no permissive option ({lic}) — shipping it in a static binary is a "
                            f"distribution decision, not a build detail")
        elif kind == "unknown":
            problems.append(f"{name} {version}: no license field in metadata — an unlicensed dependency is not MIT-compatible "
                            f"by default; record it explicitly or drop it")
    by_name = collections.defaultdict(set)
    for name, version, _, _ in rows:
        by_name[name].add(version.split(".", 1)[0])
    for name, majors in sorted(by_name.items()):
        if len(majors) > 1 and name not in DUPLICATE_EXCEPTIONS:
            problems.append(f"{name}: linked at {sorted(majors)} with no recorded exception — pick one, or add a reason to "
                            f"DUPLICATE_EXCEPTIONS")
    return sorted(problems), rows


def report_text(rows):
    mixed = [r for r in rows if r[3] == "mixed"]
    lines = [
        REPORT_HEADING,
        "",
        "Generated by `.github/scripts/check-deps.py --write-report`; the same script fails the build when this file no longer",
        "matches what it would write, so \"derived, not hand-written\" cannot quietly become \"stale\". Rules and the permissive",
        f"allowlist live in that script. {len(rows)} packages; {len(mixed)} offer a choice of licenses.",
        "",
    ]
    if mixed:
        lines += ["## Crates where a choice was made", ""]
        for name, version, lic, _ in sorted(mixed):
            note = next((n for n in NOTABLE_MIXED if n in lic), None)
            lines.append(f"- **{name} {version}** — `{lic}`" + (f"; the copyleft-sounding half ({note}) is not the option taken."
                          if note else "; every option is permissive anyway."))
        lines.append("")
    lines += ["## Every package", "", "| Crate | Version | License expression | Class |", "| --- | --- | --- | --- |"]
    for name, version, lic, kind in sorted(rows):
        lines.append(f"| {name} | {version} | `{lic}` | {kind} |")
    lines.append("")
    return "\n".join(lines)


def report_is_current(root, rows):
    path = os.path.join(root, "docs/dependency-licenses.md")
    if not os.path.exists(path):
        return ["docs/dependency-licenses.md does not exist — run `.github/scripts/check-deps.py --write-report`"]
    if open(path, encoding="utf-8").read() != report_text(rows):
        return ["docs/dependency-licenses.md is stale — a dependency changed and the report did not; run --write-report"]
    return []


def self_test():
    tmp = tempfile.mkdtemp(prefix="check-deps-selftest-")
    try:
        def meta(packages):
            return {"packages": [{"name": n, "version": v, "license": l} for n, v, l in packages]}

        cases, failures = [], []
        good = meta([("a", "1.0.0", "MIT OR Apache-2.0"), ("b", "2.1.0", "Apache-2.0")])
        problems, rows = check(tmp, good)
        if problems:
            failures.append(f"a clean dependency set reported: {problems}")
        else:
            cases.append("a permissive tree passes")

        for label, pkg, needle in [
            ("copyleft-only license", ("gplcrate", "1.0.0", "GPL-3.0-only"), "no permissive option"),
            ("copyleft bound by AND", ("andcrate", "1.0.0", "GPL-3.0 AND MIT"), "no permissive option"),
            ("missing license field", ("nologo", "1.0.0", None), "no license field"),
        ]:
            problems, _ = check(tmp, meta([("a", "1.0.0", "MIT"), pkg]))
            if any(needle in p for p in problems):
                cases.append(f"a planted {label} is named")
            else:
                failures.append(f"{label} not caught: {problems}")

        problems, _ = check(tmp, meta([("dup", "1.0.0", "MIT"), ("dup", "2.0.0", "MIT")]))
        if any("no recorded exception" in p for p in problems):
            cases.append("an unexcepted duplicate major is named")
        else:
            failures.append(f"duplicate majors not caught: {problems}")

        # `AND` of two permissive licenses applies both, and both are fine — no false alarm (ring is this case).
        problems, _ = check(tmp, meta([("andok", "1.0.0", "Apache-2.0 AND ISC")]))
        if problems:
            failures.append(f"`Apache-2.0 AND ISC` was called unsafe: {problems}")
        else:
            cases.append("an AND of two permissive licenses passes")

        # A mixed expression must pass, and must show up in the report as a choice that was made.
        problems, rows = check(tmp, meta([("mix", "1.0.0", "MIT OR Apache-2.0 OR LGPL-2.1-or-later")]))
        text = report_text(rows)
        if problems or "choice was made" not in text or "LGPL" not in text:
            failures.append(f"a mixed expression should pass and be listed: {problems}; report head={text[:200]}")
        else:
            cases.append("a crate offering a permissive option passes, and the choice is written down")

        # Staleness: the report must match what the script would write.
        os.makedirs(os.path.join(tmp, "docs"), exist_ok=True)
        open(os.path.join(tmp, "docs/dependency-licenses.md"), "w").write(report_text(rows))
        if report_is_current(tmp, rows):
            failures.append("a current report was reported as stale")
        else:
            cases.append("a current report is accepted")
        open(os.path.join(tmp, "docs/dependency-licenses.md"), "w").write(report_text(rows) + "\nextra\n")
        if not report_is_current(tmp, rows):
            failures.append("a stale report passed as current")
        else:
            cases.append("a stale report fails the fence")

        # Vacuity: a metadata file with no packages is not a pass.
        problems, rows = check(tmp, meta([]))
        if rows:
            failures.append("empty metadata produced rows")
        else:
            cases.append("an empty dependency list is detected as empty, not as passing")

        print("check-deps self-test")
        for c in cases:
            print(f"  ok   {c}")
        for f in failures:
            print(f"  FAIL {f}")
        return 1 if failures else 0
    finally:
        shutil.rmtree(tmp, ignore_errors=True)


def main(argv=None):
    ap = argparse.ArgumentParser(description=__doc__.split("\n")[0])
    ap.add_argument("root", nargs="?", default=None)
    ap.add_argument("--self-test", action="store_true")
    ap.add_argument("--write-report", action="store_true", help="write docs/dependency-licenses.md and exit")
    ap.add_argument("--skip-report-check", action="store_true", help="check rules only (used before the report exists)")
    args = ap.parse_args(argv)
    if args.self_test:
        return self_test()
    root = args.root or os.path.abspath(os.path.join(os.path.dirname(os.path.abspath(__file__)), "..", ".."))
    problems, rows = check(root)
    if not rows:
        print("NOT COVERAGE: check-deps resolved 0 packages")
        return 1
    if args.write_report:
        path = os.path.join(root, "docs/dependency-licenses.md")
        open(path, "w", encoding="utf-8").write(report_text(rows))
        print(f"wrote {os.path.relpath(path, root)} for {len(rows)} packages")
        return 1 if problems else 0
    if not args.skip_report_check:
        problems += report_is_current(root, rows)
    for p in problems:
        print(p)
    mixed = sum(1 for r in rows if r[3] == "mixed")
    print(f"{len(rows)} packages, {mixed} with a license choice, {len(problems)} problems")
    return 1 if problems else 0


if __name__ == "__main__":
    sys.exit(main())
