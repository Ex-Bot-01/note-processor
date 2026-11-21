#!/usr/bin/env python3
"""Prove that a range of commits moved no code token — only comments, and the blank lines that separated them.

Why this exists: the comment diet (and any later "move code only" refactor) is measured in thousands of lines. Reviewing that
by eye asks a human to confirm a negative across every file, which is exactly the claim people get wrong. So the claim gets
computed instead: strip the comments out of every file the range touched, and compare what is left. If two token streams
differ, a code token changed and the range was not a comment-only change.

    check-comment-only.py BASE [HEAD]        # no HEAD: the base is compared against the working tree, so it can be run
                                             # before committing — which is when a stray token is cheapest to notice

The stripper is deliberately boring: `//` runs to end of line in Rust, `#` does in TOML/shell/Python/YAML, and neither counts
inside a string literal. It does not try to be a parser — it only has to be *conservative*, because the failure mode that
matters is a real code change slipping past as "just comments".

Two things this does not claim, stated so nobody over-reads the green:

  * it compares what remains after comments, so moving a comment from one side of a statement to the other passes. That is in
    scope for "no token changed"; it is not the same as "the prose still reads well", which is a person's judgement;
  * a deleted line that had code *and* a comment fails, correctly — the code part disappeared.
"""

import os
import subprocess
import sys

RUST = (".rs",)
HASH = (".toml", ".sh", ".py", ".yaml", ".yml")


def strip_comments(text, hash_style):
    """Return `text` with comments removed and blank lines dropped.

    Blank lines go because a comment-only commit normally takes the blank line that separated the block from its code, and
    counting that as a change would fail honest diffs for a reason nobody cares about.
    """
    marker = "#" if hash_style else "//"
    out = []
    for line in text.split("\n"):
        kept, i, quote = [], 0, None
        n = len(line)
        while i < n:
            ch = line[i]
            if quote:
                kept.append(ch)
                if ch == "\\" and i + 1 < n:          # \x escapes inside the open literal
                    kept.append(line[i + 1])
                    i += 2
                    continue
                if ch == quote:
                    quote = None
                    if line.startswith("r\"", max(0, i - 1), i + 1) or line.startswith("r#", max(0, i - 2), i):
                        pass
                i += 1
                continue
            if ch in "\"'":
                # A Rust lifetime (`'a`, `'static`) opens no literal; only a char literal closes on the same expression.
                if ch == "'" and not _looks_like_char_literal(line, i):
                    kept.append(ch)
                    i += 1
                    continue
                quote = ch
                kept.append(ch)
                i += 1
                continue
            if line.startswith(marker, i):
                if marker == "//" or i == 0 or line[i - 1] in " \t":
                    break                              # comment starts here; the rest of the line is prose
                kept.append(ch)
                i += 1
                continue
            if marker == "#" and ch == "$":            # shell: $# is a parameter, not a comment
                j = i + 1
                while j < n and (line[j].isalnum() or line[j] == "_"):
                    j += 1
                kept.append(line[i:j])
                i = j
                continue
            kept.append(ch)
            i += 1
        stripped = "".join(kept).rstrip()
        if stripped.strip():
            out.append(stripped)
    return "\n".join(out)


def _looks_like_char_literal(line, i):
    """`'x'` or `'\n'` is a char literal; `'static`, `'a>` and the closing half of `'a'` are not."""
    j = i + 1
    n = len(line)
    if j < n and line[j] == "\\":
        j += 2
        return j < n and line[j] == "'"
    if j >= n:
        return False
    k = j + 1
    return k < n and line[k] == "'"


def changed_files(base, head, root="."):
    spec = f"{base}..{head}" if head else base          # no head: compare the base against the working tree
    out = subprocess.run(["git", "-C", root, "diff", "--name-only", spec], capture_output=True, text=True, check=True)
    return [f for f in out.stdout.split("\n") if f.strip()]


def blob(rev, path, root="."):
    if rev == "":                                        # the working tree, for a range with no explicit head
        full = os.path.join(root, path)
        try:
            with open(full, encoding="utf-8", errors="replace") as fh:
                return fh.read()
        except OSError:
            return None                                  # deleted in the tree: a comment-only change deletes nothing
    r = subprocess.run(["git", "-C", root, "show", f"{rev}:{path}"], capture_output=True, text=True)
    return r.stdout if r.returncode == 0 else None       # absent at that revision: added or deleted file


def check(base, head="", root="."):
    problems = []
    files = changed_files(base, head or "", root)
    for path in files:
        ext = os.path.splitext(path)[1]
        if ext not in RUST and ext not in HASH:
            continue                                    # lockfiles, goldens, data: this fence is about source prose
        before = blob(base, path, root)
        after = blob(head, path, root) if head != "" else blob("", path, root)
        if before is None or after is None:
            problems.append(f"{path}: added or deleted in this range — a comment-only change neither adds nor removes files")
            continue
        style = ext in HASH
        a = strip_comments(before, style)
        b = strip_comments(after, style)
        if a != b:
            first = next((i for i, (x, y) in enumerate(zip(a.split("\n"), b.split("\n"))) if x != y), 0)
            problems.append(
                f"{path}: code token differs after comment-stripping (line {first + 1} of the stripped file); "
                f"this range is not comment-only"
            )
    return files, problems


def self_test():
    import shutil
    import tempfile

    tmp = tempfile.mkdtemp(prefix="comment-only-")
    subprocess.run(["git", "init", "-q", tmp], check=True)

    def commit(files):
        for name, text in files.items():
            p = os.path.join(tmp, name)
            os.makedirs(os.path.dirname(p), exist_ok=True) if os.path.dirname(p) else None
            with open(p, "w", encoding="utf-8") as fh:
                fh.write(text)
        subprocess.run(["git", "-C", tmp, "add", "-A"], check=True)
        subprocess.run(["git", "-C", tmp, "-c", "user.email=t@t", "-c", "user.name=t", "commit", "-qm", "x"], check=True)

    def head_of(n):
        return subprocess.run(["git", "-C", tmp, "rev-parse", f"HEAD~{n}"], capture_output=True, text=True,
                              check=True).stdout.strip()

    cases, failures = [], []
    base_files = {
        "src/a.rs": 'fn main() {\n    // why this runs first: the lock has to exist before the write\n    let x = 1; // one\n    let url = "//not-a-comment";\n    let c = \'q\';\n    println!("{x}");\n}\n',
        "b.toml": '# the pinned toolchain, and why it is pinned\nrust-version = "1.85" # keep with MSRV\n',
    }
    commit(base_files)

    same_but_quieter = dict(base_files)
    same_but_quieter["src/a.rs"] = 'fn main() {\n    let x = 1;\n    let url = "//not-a-comment";\n    let c = \'q\';\n    println!("{x}");\n}\n'
    same_but_quieter["b.toml"] = 'rust-version = "1.85"\n'
    commit(same_but_quieter)
    files, problems = check(head_of(1), head_of(0), tmp)
    if not problems:
        cases.append("comments and their blank lines removed; a string holding `//` and a char literal survive")
    else:
        failures.append(f"a comment-only diff was flagged: {problems}")

    token = dict(same_but_quieter)
    token["src/a.rs"] = same_but_quieter["src/a.rs"].replace("let x = 1;", "let x = 2;")
    commit(token)
    _, problems = check(head_of(1), head_of(0), tmp)
    if any("src/a.rs" in p for p in problems):
        cases.append("one changed token is named by file")
    else:
        failures.append("a changed code token passed as comment-only")

    trailing = dict(token)
    trailing["src/a.rs"] = token["src/a.rs"].replace('println!("{x}");', 'println!("{x}"); // logged')
    commit(trailing)
    _, problems = check(head_of(1), head_of(0), tmp)
    if not problems:
        cases.append("adding a comment after untouched code stays green")
    else:
        failures.append(f"a trailing comment was flagged: {problems}")

    lost = dict(trailing)
    lost["src/a.rs"] = trailing["src/a.rs"].replace('    let c = \'q\';\n', "    // the char is unused now\n")
    commit(lost)
    _, problems = check(head_of(1), head_of(0), tmp)
    if any("src/a.rs" in p for p in problems):
        cases.append("a line that was code-plus-comment cannot be deleted as prose")
    else:
        failures.append("deleting a code-bearing line passed as comment-only")

    added = dict(lost)
    added["new.rs"] = "fn new() {}\n"
    commit(added)
    _, problems = check(head_of(1), head_of(0), tmp)
    if any("new.rs" in p for p in problems):
        cases.append("a file added in the range is named rather than silently skipped")
    else:
        failures.append(f"an added file slipped through: {problems}")

    with open(os.path.join(tmp, "src/a.rs"), "a", encoding="utf-8") as fh:
        fh.write("// a note added but not committed\n")
    _, problems = check(head_of(0), "", tmp)
    if not problems:
        cases.append("an uncommitted comment-only edit compares against the working tree, not against HEAD")
    else:
        failures.append(f"an uncommitted prose edit was flagged: {problems}")
    with open(os.path.join(tmp, "src/a.rs"), "a", encoding="utf-8") as fh:
        fh.write("static LEAK: i32 = 1;\n")
    _, problems = check(head_of(0), "", tmp)
    if any("src/a.rs" in x for x in problems):
        cases.append("an uncommitted code line is caught before it is committed")
    else:
        failures.append("an uncommitted static slipped through")

    shutil.rmtree(tmp, ignore_errors=True)
    print("comment-only self-test")
    for c in cases:
        print(f"  ok   {c}")
    for f in failures:
        print(f"  FAIL {f}")
    return 1 if failures else 0


def main(argv, root=None):
    if root is None:
        top = subprocess.run(["git", "rev-parse", "--show-toplevel"], capture_output=True, text=True)
        # An empty answer means "not a repository", and running on with a bogus root would report every revision as missing —
        # the same shape as a green row built on nothing.
        root = top.stdout.strip() if top.returncode == 0 else "."
    if "--self-test" in argv:
        return self_test()
    if not argv:
        print("usage: check-comment-only.py BASE [HEAD]   (or --self-test)")
        return 2
    base = argv[0]
    head = argv[1] if len(argv) > 1 else ""
    for rev in [base] + ([head] if head else []):
        probe = subprocess.run(["git", "-C", root, "rev-parse", "--verify", "-q", f"{rev}^{{commit}}"], capture_output=True)
        if probe.returncode:
            print(f"check-comment-only: no such revision: {rev}")
            return 2
    files, problems = check(base, head, root)
    for p in problems:
        print(p)
    source_changes = [f for f in files if os.path.splitext(f)[1] in RUST + HASH]
    if not source_changes:
        # A range that touched no source file is not evidence of a comment-only change; say which, loudly.
        print(f"NOT COVERAGE comment-only: {len(files)} file(s) changed, none of them Rust or hash-comment source")
        return 0
    print(f"{len(source_changes)} source file(s) compared against {base}: "
          f"{'code tokens identical' if not problems else str(len(problems)) + ' difference(s)'}")
    return 1 if problems else 0


if __name__ == "__main__":
    sys.exit(main(sys.argv[1:]))
