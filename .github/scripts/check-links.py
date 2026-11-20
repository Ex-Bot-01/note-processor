#!/usr/bin/env python3
"""Every link this repository offers has to resolve, or it is not offered.

Why this exists: prose here cites files two ways — a markdown link in `README.md`, and a path inside backticks in a design
note (`docs/reference-serve.md`, "line 199"). Both are promises to a reader, and both went stale silently when documents
moved directories. A checker that reads only `[text](target)` would have missed most of the breakage, so this one reads both
spellings — and then declines to check the shapes that were never promises.

**What is checked.**

  * `[label](target)`, ignoring `http(s):`, `mailto:`, bare `#anchors`, anything with a glob or shell variable in it, and
    anything inside a fenced code block (a captured transcript is sample data, not navigation);
  * a backticked token that is a **path**, meaning: it contains a `/`, its first component is a top-level entry of this
    repository's tracked files, and it ends in a known document/source extension. An optional `:line` suffix is stripped —
    the line number points into a file rather than being part of its name.

**What is deliberately not checked**, and why each exclusion has a reason:

  * a bare name in backticks (`scan.rs`, `INDEX.md`, `config.toml`) names a thing, not a location; half of this repository's
    prose is of that shape and none of it is a link;
  * **an inline code span.** `[text](relative.md)` written inside backticks is showing syntax, and the repository says so in
    its own words: "fenced blocks never yield links, and neither do inline spans" (`README.md`). Checking those would fail
    the very sentence that explains them;
  * **anything under `docs/archive/`.** An archived plan quotes the paths it was written against — including generated ones
    like `tests/fixtures/corpus/INDEX.md` that existed only while somebody was following it. Rewriting a step to keep a
    checker quiet would falsify the record, so archive is quoted, not linked; see `docs/archive/README.md`.
  * a path whose first component is not tracked here (`notes/x.md`, `daily/2026-01-01.md`, `src/llama-context.cpp:1631`,
    `caf\xe9.md`) describes somebody's vault, llama.cpp's source, or a byte-level case. The tree does not contain them and
    was never meant to;
  * files under `tests/fixtures/`: a note in a fixture that links to `notes/other.md` is testing what `np` does with a
    dangling link.

**How a target resolves.** Relative to the file that names it, or relative to the repository root, or as a path suffix of any
tracked file — that last one because prose says `tests/scan.rs` meaning `crates/np-core/tests/scan.rs`, and a checker that
called that broken would be wrong more often than right.

`--self-test` plants one break per checked shape and requires this script to name it, and pins the exclusions so they cannot
quietly widen into a checker that passes everything.
"""

import os
import re
import subprocess
import shutil
import sys
import tempfile

INLINE_CODE = re.compile(r"(`+)(?:(?!\1).)*\1", re.S)
MD_LINK = re.compile(r"\[[^\]]*\]\(([^)\s]+)(?:\s+\"[^\"]*\")?\)")
BACKTICK = re.compile(r"`([^`\n]+)`")
FENCE = re.compile(r"^\s*(```|~~~)")
EXTENSIONS = (".md", ".rs", ".toml", ".sh", ".py", ".json", ".yml", ".yaml", ".txt")
IGNORE_PREFIXES = ("http://", "https://", "mailto:", "#", "git@", "./.git/")


def repo_root():
    here = os.path.dirname(os.path.abspath(__file__))
    return os.path.abspath(os.path.join(here, "..", ".."))


def tracked(root):
    out = subprocess.run(["git", "-C", root, "ls-files"], capture_output=True, text=True)
    if out.returncode != 0:
        raise SystemExit(f"check-links: cannot list files in {root}: {out.stderr.strip()}")
    return set(p for p in out.stdout.split() if p)


def markdown_files(root, files):
    return [p for p in sorted(files)
            if p.endswith(".md") and not p.startswith(("tests/fixtures/", "docs/archive/"))]


def candidates(raw, heads):
    """Yield (kind, spelling, body) for the links on one line of prose."""
    prose = INLINE_CODE.sub(lambda m: " " * len(m.group(0)), raw)  # code spans show syntax; they are not links
    for kind, finder in (("markdown", MD_LINK), ("backtick", BACKTICK)):
        source = raw if kind == "backtick" else prose
        for token in finder.findall(source):
            t = token.strip()
            if not t or t.startswith(IGNORE_PREFIXES):
                continue
            if any(c in t for c in "*$<>~{}") or " " in t:
                continue
            body = t.split("#", 1)[0]
            if ":" in body.rsplit("/", 1)[-1]:
                head, _, tail = body.rpartition(":")
                if tail.isdigit():
                    body = head
            if not body or "." not in os.path.basename(body) or not body.endswith(EXTENSIONS):
                continue
            if "/" not in body:
                if kind == "backtick":
                    continue  # `scan.rs` is a name, not a path
            elif body.split("/", 1)[0] not in heads:
                continue  # somebody's vault, or llama.cpp's source tree
            yield kind, t, body


def resolves(body, rel_path, files, root):
    here = os.path.normpath(os.path.join(os.path.dirname(rel_path), body))
    there = os.path.normpath(body)
    if here in files or there in files:
        return True
    if os.path.exists(os.path.join(root, there)):  # untracked but real (a build product, a local log dir)
        return True
    tail = "/" + there.lstrip("/")
    return any(f.endswith(tail) for f in files)


def check_file(rel, files, heads, root):
    broken = []
    path = os.path.join(root, rel)
    try:
        text = open(path, encoding="utf-8", errors="replace").read()
    except OSError as e:  # unreadable is a finding, not a skip
        return [f"{rel}: cannot read ({e})"]
    fenced = False
    for lineno, raw in enumerate(text.split("\n"), 1):
        if FENCE.match(raw):
            fenced = not fenced
            continue
        if fenced:
            continue
        for kind, spelling, body in candidates(raw, heads):
            if not resolves(body, rel, files, root):
                broken.append(f"{rel}:{lineno}: {kind} link resolves to nothing: {spelling}")
    return broken


def check(root):
    files = tracked(root)
    heads = {p.split("/", 1)[0] for p in files}
    docs = markdown_files(root, files)
    broken, links = [], 0
    for rel in docs:
        text = open(os.path.join(root, rel), encoding="utf-8", errors="replace").read()
        fenced = False
        for raw in text.split("\n"):
            if FENCE.match(raw):
                fenced = not fenced
                continue
            if not fenced:
                links += sum(1 for _ in candidates(raw, heads))
        broken += check_file(rel, files, heads, root)
    return broken, links, len(docs)


def self_test():
    """One planted break per checked shape, plus the exclusions that must stay green."""
    tmp = tempfile.mkdtemp(prefix="check-links-selftest-")
    try:
        subprocess.run(["git", "-C", tmp, "init", "-q"], check=True, capture_output=True)
        os.makedirs(os.path.join(tmp, "docs"), exist_ok=True)
        os.makedirs(os.path.join(tmp, ".github", "scripts"), exist_ok=True)
        shutil.copy(os.path.abspath(__file__), os.path.join(tmp, ".github", "scripts", "check-links.py"))
        os.makedirs(os.path.join(tmp, "crates", "np-core", "tests"), exist_ok=True)
        open(os.path.join(tmp, "crates", "np-core", "tests", "scan.rs"), "w").write("fn main() {}\n")

        def write(rel, text):
            p = os.path.join(tmp, rel)
            os.makedirs(os.path.dirname(p), exist_ok=True)
            open(p, "w", encoding="utf-8").write(text)

        write("docs/exists.md", "# Exists\n\nNothing to see.\n")
        write("tests/fixtures/notes/fixture.md", "[dangling](notes/other.md) is sample data for a test.\n")
        subprocess.run(["git", "-C", tmp, "add", "-A"], check=True, capture_output=True)

        cases, failures = [], []

        def run():
            return check(tmp)[0]

        clean = (
            "# Index\n\n"
            "A [markdown link](docs/exists.md), a root-relative one [here](docs/exists.md), and a backticked path "
            "`docs/exists.md:7`. A crate-relative test `crates/np-core/tests/scan.rs`, and the short form prose uses: "
            "`tests/scan.rs:41`. Then the things that are not links: https://example.com/x.md, an [anchor](#heading), a "
            "bare word `nothing`, a bare file name `scan.rs`, a glob `docs/*.md`, a variable `$TMPDIR/cap/x.md`, somebody's "
            "vault `notes/imaginary.md` and `daily/2026-01-01.md`, a byte case `caf\\xe9.md`, llama.cpp's own source "
            "`src/llama-context.cpp:1631`, syntax shown in backticks (`` `[text](docs/gone.md)` ``), and a link inside a "
            "fence:\n\n"
            "```json\n{\"result\": {\"data_dir\": \"/tmp/x/data\"}, \"see\": \"[captured](docs/gone.md)\"}\n```\n"
        )
        write("README.md", clean)
        subprocess.run(["git", "-C", tmp, "add", "-A"], check=True, capture_output=True)
        found = run()
        if found:
            failures.append(f"a tree whose links all resolve reported: {found}")
        else:
            cases.append("a resolving tree passes, and every exclusion stays green")

        planted = [
            ("markdown link", "# I\n\n[Nowhere](docs/gone.md).\n", "docs/gone.md"),
            ("backticked path", "# I\n\nCites `docs/never-written.md:12`.\n", "docs/never-written.md"),
            ("crate-relative backticked path", "# I\n\nSee `crates/np-core/tests/nope.rs`.\n", "tests/nope.rs"),
        ]
        for name, doc, needle in planted:
            write("README.md", doc)
            subprocess.run(["git", "-C", tmp, "add", "-A"], check=True, capture_output=True)
            found = run()
            if any(needle in b for b in found):
                cases.append(f"a planted {name} is named and fails")
            else:
                failures.append(f"planted {name} ({needle}) not reported; got {found}")

        # A fixture's dangling link is test data, so the tree must stay green.
        write("README.md", "# I\n\nFixtures hold sample notes.\n")
        write("tests/fixtures/notes/fixture.md", "[dangling](notes/other.md) and `notes/x.md` are sample data.\n")
        subprocess.run(["git", "-C", tmp, "add", "-A"], check=True, capture_output=True)
        found = run()
        if any("notes/" in b for b in found):
            failures.append(f"a fixture's sample link was reported as broken: {found}")
        else:
            cases.append("a fixture's dangling sample link is not a broken promise")

        # Vacuity: no markdown at all must be said out loud, not counted as passing.
        empty = tempfile.mkdtemp(prefix="check-links-empty-")
        try:
            subprocess.run(["git", "-C", empty, "init", "-q"], check=True, capture_output=True)
            b0, links0, files0 = check(empty)
            if files0 or links0:
                failures.append(f"empty tree reported {files0} files / {links0} links")
            else:
                cases.append("an empty tree is detected as empty rather than as passing")
        finally:
            shutil.rmtree(empty, ignore_errors=True)

        print("check-links self-test")
        for c in cases:
            print(f"  ok   {c}")
        for f in failures:
            print(f"  FAIL {f}")
        return 1 if failures else 0
    finally:
        shutil.rmtree(tmp, ignore_errors=True)


def main(argv):
    if "--self-test" in argv:
        return self_test()
    positional = [a for a in argv if not a.startswith("-")]
    root = positional[0] if positional else repo_root()
    broken, links, files = check(root)
    for b in broken:
        print(b)
    if not files or not links:
        print(f"NOT COVERAGE: check-links scanned {files} markdown files and resolved {links} links")
        return 1
    print(f"{files} markdown files, {links} links checked, {len(broken)} broken")
    return 1 if broken else 0


if __name__ == "__main__":
    sys.exit(main(sys.argv[1:]))
