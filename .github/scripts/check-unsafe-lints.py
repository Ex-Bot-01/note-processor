#!/usr/bin/env python3
"""The unsafe lints are crate-level denials, and this fence keeps them that way.

Both crates open with `#![deny(unsafe_op_in_unsafe_fn, clippy::undocumented_unsafe_blocks)]`. A denial is only worth what its
weakest exception is worth: one `#[allow(...)]` above the block that matters, in a tree with 57 `unsafe` sites, and the gate
goes green over exactly the code that can corrupt memory. So this checks two things and nothing else:

  * each crate root still denies **both** lints (a half-denial — one lint kept, one dropped in a rebase — fails);
  * no file under `crates/` allows either of them, including in tests, where an inconvenient lint is easiest to mute.

`--self-test` plants each break in a throwaway tree and requires the named failure, plus the case that must stay green, so a
checker that prints nothing cannot pass its own test.
"""

import os
import re
import subprocess
import shutil
import sys
import tempfile

ROOTS = ["crates/np-core/src/lib.rs", "crates/np-cli/src/main.rs"]
LINTS = ["unsafe_op_in_unsafe_fn", "clippy::undocumented_unsafe_blocks"]
DENY = re.compile(r"#!\[deny\(([^)]*)\)\]")


def crate_roots(root):
    return [r for r in ROOTS if os.path.exists(os.path.join(root, r))]


def rust_files(root):
    out = subprocess.run(["git", "-C", root, "ls-files", "*.rs"], capture_output=True, text=True)
    if out.returncode != 0:  # an untracked checkout (the self-test tree) still has to be checkable
        names = []
        for base, _, files in os.walk(root):
            names += [os.path.relpath(os.path.join(base, f), root) for f in files if f.endswith(".rs")]
        return sorted(names)
    return sorted(p for p in out.stdout.split() if p.startswith("crates/"))


def check(root):
    broken = []
    roots = crate_roots(root)
    for rel in roots:
        text = open(os.path.join(root, rel), encoding="utf-8", errors="replace").read()
        denied = set()
        for group in DENY.findall(text):
            denied |= {d.strip() for d in group.split(",")}
        for lint in LINTS:
            if lint not in denied:
                broken.append(f"{rel}: does not deny `{lint}` at crate level")
    allow = re.compile(r"allow\((?:[^)]*\b)?(?:%s)" % "|".join(re.escape(l) for l in LINTS))
    for rel in rust_files(root):
        for lineno, line in enumerate(open(os.path.join(root, rel), encoding="utf-8", errors="replace"), 1):
            if allow.search(line):
                broken.append(f"{rel}:{lineno}: opts a block out of the unsafe lints")
    return broken, len(roots)


def self_test():
    tmp = tempfile.mkdtemp(prefix="unsafe-lints-selftest-")
    try:
        def build(root, deny_core=None, extra_allow=False):
            for rel in ROOTS:
                p = os.path.join(root, rel)
                os.makedirs(os.path.dirname(p), exist_ok=True)
                deny = "#![deny(unsafe_op_in_unsafe_fn, clippy::undocumented_unsafe_blocks)]" if deny_core is None else deny_core
                open(p, "w").write(f"//! crate\n{deny}\nmod x;\n")
            if extra_allow:
                p = os.path.join(root, "crates/np-core/src/local_model.rs")
                open(p, "w").write("#[allow(clippy::undocumented_unsafe_blocks)]\nfn f() {}\n")

        cases, failures = [], []

        build(tmp)
        broken, n = check(tmp)
        if broken or n != 2:
            failures.append(f"a compliant tree reported {broken}")
        else:
            cases.append("both crates denying both lints passes")

        build(tmp, deny_core="#![deny(unsafe_op_in_unsafe_fn)]")
        broken, _ = check(tmp)
        if any("undocumented_unsafe_blocks" in b for b in broken):
            cases.append("a crate that dropped one of the two denials is named")
        else:
            failures.append(f"half-denial not caught: {broken}")

        build(tmp, extra_allow=True)
        broken, _ = check(tmp)
        if any("opts a block out" in b for b in broken):
            cases.append("an `allow` above an unsafe block is named, file and line")
        else:
            failures.append(f"allow not caught: {broken}")

        empty = tempfile.mkdtemp(prefix="unsafe-lints-empty-")
        try:
            _, n = check(empty)
            if n:
                failures.append(f"empty tree found {n} crate roots")
            else:
                cases.append("a tree with no crates is detected as empty, not as passing")
        finally:
            shutil.rmtree(empty, ignore_errors=True)

        print("unsafe-lints self-test")
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
    pos = [a for a in argv if not a.startswith("-")]
    root = pos[0] if pos else os.path.abspath(os.path.join(os.path.dirname(os.path.abspath(__file__)), "..", ".."))
    broken, roots = check(root)
    for b in broken:
        print(b)
    if not roots:
        print("NOT COVERAGE: unsafe-lints found no crate roots to read")
        return 1
    print(f"{roots} crate roots checked for both denials; {len(broken)} exceptions")
    return 1 if broken else 0


if __name__ == "__main__":
    sys.exit(main(sys.argv[1:]))
