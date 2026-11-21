#!/usr/bin/env python3
"""Flag items that are dead code on a platform this machine does not compile for.

Why this exists: `docs/design-f34-windows-disk-preflight.md` quotes this script's own header — "eight consecutive CI runs
failed on `windows-latest` with `function … is never used [-D warnings]`" while every local gate was green — and declines
to confirm that count, so it is quoted rather than asserted. The reason is structural and does not depend on the count:
a helper called only from `#[cfg(unix)]` tests is perfectly live here and dead there, and no Linux command can see it.
Cross-compiling `cargo check --target x86_64-pc-windows-msvc` is not available in this workspace either, because
`libsqlite3-sys`'s build script needs a Windows `lib.exe`.

So the cheap static proxy runs here: for every function or const that carries no `#[cfg(...)]` of its own, check whether
*all* of its call sites sit inside items gated by one and the same predicate. If they do, it is dead wherever that cfg is
off — and CI is the place that will find out.

Heuristic, not a compiler. What it cannot see: a reference through a macro or a function pointer; an item used only from
a gated `mod { … }` block rather than a gated function; anything whose definition line does not match the shapes in `DEF`.
A gate counts as the item's own only when it is contiguous with the definition — attribute and doc-comment lines
immediately above it, nothing else — because "there is a `#[cfg]` somewhere within four lines" is exactly the shape this
fence exists to catch, not an exemption from it.

Usage: python3 .github/scripts/check_cfg_dead_code.py [repo]
Exit 0 = nothing flagged, 1 = offenders printed. `--self-test` runs the planted cases in this file and needs no repository.
"""

from __future__ import annotations

import pathlib
import re
import sys
import tempfile

# Definitions worth asking about: `fn`, `const`, and their `async`/`unsafe` forms, with any visibility so that
# `pub(super) fn` is not invisible. Group 1 is a bare `pub`; group 2 is the argument of `pub(…)`.
DEF = re.compile(
    r"^\s*(?:(pub)\s+)?(?:pub\(([^)]*)\)\s+)?(?:(?:default|extern\s*\"[^\"]*\")\s+)*"
    r"(?:const |async\s+unsafe\s+fn |async\s+fn |unsafe\s+fn |fn )(\w+)"
)
GATED = re.compile(r"#\[cfg\(([^)]*)\)")
# Lines that may sit between a gate and the item it gates without breaking the connection.
CONTIGUOUS = re.compile(r"^\s*(#\[|///|//!|//)")
# A method's "dead" status is decided by the trait or impl it belongs to, and names like `drop` collide with std
# functions called elsewhere in the file. The shape this fence hunts is a free function or a const.
TAKES_SELF = re.compile(r"^\s*(?:pub(?:\([^)]*\))?\s+)?(?:const|async\s+)?fn\s+\w+\s*\(\s*(?:&\s*(?:'\w+\s+)?(?:mut\s+)?|mut\s+)?self\b")


def gated_blocks(lines: list[str]) -> list[tuple[int, int, str]]:
    """`(first_line, last_line, predicate)` for every `#[cfg(...)]`-gated brace block.

    An item declared inside such a block does not exist where the gate is off, so it can never be the item rustc calls
    dead there — which is what makes `fetch.rs`'s `GetDiskFreeSpaceExW`, declared inside a gated `extern "system"` block,
    live rather than flagged. The gate must sit contiguous with the block's own header line, for the same reason as an
    item's: "a `#[cfg]` somewhere above" is not evidence about this block.
    """
    blocks: list[tuple[int, int, str]] = []
    stack: list[tuple[int, str | None]] = []
    for index, line in enumerate(lines):
        gate = contiguous_gate(lines, index)
        opens, closes = line.count("{"), line.count("}")
        if opens and not closes:
            stack.append((index, gate))
        elif opens and closes:
            if gate:
                blocks.append((index, index, gate))
        if closes:
            for _ in range(closes):
                if stack:
                    start, block_gate = stack.pop()
                    if block_gate:
                        blocks.append((start, index, block_gate))
    return blocks


def contiguous_gate(lines: list[str], index: int) -> str | None:
    """The predicate of a `#[cfg(...)]` written directly above `lines[index]`, or None.

    Only attribute and doc-comment lines may sit in between: that run is what Rust itself treats as one item's header,
    and it is also the shape a paste leaves behind when it moves a gate off its item.
    """
    back = index - 1
    gate = None
    while back >= 0 and CONTIGUOUS.match(lines[back]):
        found = GATED.search(lines[back])
        if found and gate is None:
            gate = found.group(1)
        back -= 1
    return gate


def spans(lines: list[str]) -> list[tuple[str, int, int, str | None]]:
    """`(name, first_line, last_line, gate)` for every fn/const, 0-indexed and inclusive.

    `gate` is the item's own contiguous `#[cfg(...)]`, or the gate of the block it is declared inside. The predicate
    matters: an item called from one `#[cfg(windows)]` function and one `#[cfg(unix)]` function compiles somewhere on
    every platform, while one called only from `#[cfg(unix)]` code is dead on Windows. Grouping by predicate rather than
    by "is gated" is what keeps the check from crying wolf at `config.rs`'s per-platform helpers.
    """
    blocks = gated_blocks(lines)
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
        gate = contiguous_gate(lines, index)
        enclosing = [b for b in blocks if b[0] < index <= b[1]]
        if enclosing:
            gate = gate or enclosing[-1][2]
        # A plain `pub` item is exported API: rustc never reports it dead, so flagging it would be noise. `pub(crate)`
        # and `pub(super)` are not exported, so they stay under the fence.
        public = bool(match.group(1)) and not match.group(2)
        if public or TAKES_SELF.match(lines[index]):
            index = index + 1
            continue
        found.append((match.group(3), index, end, gate))
        index = max(end + 1, index + 1)
    return found


def audit(path: pathlib.Path) -> list[str]:
    lines = path.read_text(encoding="utf-8").splitlines()
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
        # Dead only when every call site is gated **and** they all share one predicate: with `unix` on one side and
        # `windows` on the other, some platform always compiles a caller.
        covered = [item[3] for item in hosts if item[3]]
        predicates = set(covered)
        if len(covered) == len(sites) and len(predicates) == 1:
            problems.append(
                f"{path}:{start + 1}: `{name}` is called only from #[cfg({predicates.pop()})] code"
            )
    return problems


def collect_files(root: pathlib.Path) -> list[pathlib.Path]:
    # Every .rs under crates/, nested modules included: `crates/*/src/*.rs` misses `tests/common/mod.rs`, which is
    # exactly where a platform-only helper lands. The sibling fence counts the same set, so a drop in either is visible.
    return sorted((root / "crates").rglob("*.rs"))


def run(root: pathlib.Path) -> int:
    files = collect_files(root)
    if not files:
        print(f"no .rs files under {root / 'crates'} — is that the repository root?")
        return 1
    problems = [problem for path in files for problem in audit(path)]
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


# ---- self-test: one planted case per thing the fence claims to notice -------------------------------

LIVE_BOTH_PLATFORMS = '''fn shared_helper() -> u32 {
    1
}

#[cfg(unix)]
fn on_unix() -> u32 {
    shared_helper()
}

#[cfg(windows)]
fn on_windows() -> u32 {
    shared_helper()
}
'''

DEAD_ON_WINDOWS = '''fn helper_needing_a_gate() -> u32 {
    7
}

#[cfg(windows)]
fn only_caller() -> u32 {
    helper_needing_a_gate()
}
'''



ASYNC_CALLER = '''fn helper_behind_an_async_caller() -> u32 {
    1
}

#[cfg(windows)]
async fn awaiter() -> u32 {
    helper_behind_an_async_caller()
}
'''

METHOD_IS_NOT_THE_SHAPE = '''#[cfg(unix)]
impl Drop for Guard {
    fn drop(&mut self) {}
}

#[cfg(unix)]
fn user() {
    let g = Guard;
    drop(g);
}
'''


# The exemption rule, tested on `spans` rather than end to end: a gate counts as an item's own only when nothing but
# attributes and doc comments separate the two. "There is a #[cfg] somewhere above" was the exemption that let a helper
# inside a gated module escape the fence while its callers stayed under it.
GATE_SHAPES = [
    ("#[cfg(unix)]\nfn gated() {}\n", "gated", "unix"),
    ("/// docs first\n#[cfg(unix)]\nfn gated() {}\n", "gated", "unix"),
    ('#[cfg(windows)]\nextern "system" {\n    fn imported() -> i32;\n}\n', "imported", "windows"),
    ('#[cfg(windows)]\nmod m {\n    use super::*;\n    fn helper() {}\n}\n', "helper", "windows"),
    ("#[test]\n\nfn spaced_apart() {}\n", "spaced_apart", None),
]


def self_test() -> int:
    cases = [
        ("a caller on each platform is live everywhere", LIVE_BOTH_PLATFORMS, 0),
        ("a helper called only from #[cfg(windows)] code is flagged", DEAD_ON_WINDOWS, 1),
        ("an async gated caller still counts as a caller", ASYNC_CALLER, 1),
        ("a trait method is not the shape this fence hunts", METHOD_IS_NOT_THE_SHAPE, 0),
    ]
    failures = 0
    with tempfile.TemporaryDirectory() as tmp:
        target = pathlib.Path(tmp) / "crates" / "x" / "src"
        target.mkdir(parents=True)
        for label, source, expected in cases:
            path = target / "lib.rs"
            path.write_text(source, encoding="utf-8")
            found = audit(path)
            ok = (1 if found else 0) == expected
            print(f"{'ok  ' if ok else 'FAIL'} {label}: {len(found)} problem(s), expected {'a flag' if expected else 'silence'}")
            failures += 0 if ok else 1
        for source, name, want in GATE_SHAPES:
            path = target / "shape.rs"
            path.write_text(source, encoding="utf-8")
            got = {n: g for n, _, _, g in spans(path.read_text(encoding="utf-8").splitlines())}
            ok = got.get(name) == want
            print(f"{'ok  ' if ok else 'FAIL'} `{name}` gate is {want!r}, read as {got.get(name)!r}")
            failures += 0 if ok else 1
        empty = pathlib.Path(tmp) / "empty"
        empty.mkdir()
        ok = run(empty) == 1
        print(f"{'ok  ' if ok else 'FAIL'} a tree with no .rs files exits 1 rather than printing pass")
        failures += 0 if ok else 1
    print("cfg dead-code self-test: " + ("pass" if not failures else f"fail — {failures} case(s)"))
    return 1 if failures else 0


if __name__ == "__main__":
    if "--self-test" in sys.argv[1:]:
        sys.exit(self_test())
    root = pathlib.Path(sys.argv[1]) if len(sys.argv) > 1 else pathlib.Path(__file__).resolve().parents[2]
    sys.exit(run(root))
