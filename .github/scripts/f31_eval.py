#!/usr/bin/env python3
"""F31 — the retrieval-quality harness that a labelled set can be poured into.

**What this is not.** It is not evidence that search is good. F31 (`docs/backlog-m2-followups.md`) stays open until a human labels
~20 queries from their own vault, or records that they declined; nothing in this file's output may be quoted as a quality claim
before then. What the harness buys is that the measurement becomes *one command* the moment labels exist, and that its arithmetic
is testable today — `--self-test` checks recall and MRR against hand-computed numbers over planted hit lists, so a wrong metric is
a red exit code rather than a flattering number nobody noticed.

Usage:

    .github/scripts/f31_eval.py <workspace> --labels labels.tsv [--np target/release/np]
                                            [--modes keyword,semantic,hybrid] [--k 5] [--strict]
    .github/scripts/f31_eval.py --self-test

Labels file: one query per line, `query<TAB>expected-path`, optional third column `note` or `image`. Blank lines and lines starting
with `#` are ignored. The path is the indexed path as `np search --json` prints it (workspace-relative, `/` separators).

Three reporting rules, all of them about not fooling ourselves:

* A query whose expected path appears in **no** mode's hit list is reported separately as `unsurfaced`. Rolling those into
  recall@5 would let a stale label file score 1.0 on the queries that happen to be answerable and hide the rest.
* Every metric is printed **per mode**. Hybrid beating keyword by 0.02 on eight queries is not a result; it is noise wearing a
  decimal point, and the table shows both halves so the reader can see which modes even had vectors.
* The run's shape is part of the output: workspace, np version, model present or absent, per-mode hit counts. A number without
  the conditions that produced it is the thing this repository argues about most.
"""

from __future__ import annotations

import argparse
import json
import math
import subprocess
import sys
from dataclasses import dataclass, field


# --------------------------------------------------------------------------- metrics (pure; self-tested)


@dataclass
class QueryResult:
    """Where one query's expected answer landed in one mode's ranked list."""

    query: str
    expected: str
    kind: str
    rank: int | None  # 1-based position of `expected`, or None when it never appeared
    top_kind: str | None  # kind of the hit at rank 1, for the "did a figure outrank the note" count
    surfaced_somewhere: bool = True


def recall_at(results: list[QueryResult], k: int) -> float:
    """Fraction of queries whose answer appeared within the first `k` hits.

    An empty list is 0.0 rather than a crash: a mode that answered nothing scored zero, and "no queries" is caught by the caller
    before any rate is printed.
    """
    if not results:
        return 0.0
    hits = sum(1 for r in results if r.rank is not None and r.rank <= k)
    return hits / len(results)


def mrr(results: list[QueryResult]) -> float:
    """Mean reciprocal rank, counting a query that never surfaced the answer as 0 (not as "excluded").

    Excluding misses would make MRR rise when recall falls, which is the opposite of what a reader assumes.
    """
    if not results:
        return 0.0
    total = sum((1.0 / r.rank) if r.rank else 0.0 for r in results)
    return total / len(results)


def ndcg_at(results: list[QueryResult], k: int) -> float:
    """Mean nDCG@k with **binary** relevance, one labelled answer per query — F31's whole label model, unchanged.

    With a single relevant document the ideal DCG is 1, so the discounted cumulative gain of one query is just its discount:
    `1 / log2(rank + 1)` when the answer lands inside `k`, and `0.0` when it does not or never appears. A miss counts as zero and
    **stays in the denominator** — the same rule [`mrr`] states, for the same reason: averaging over answered queries only would let
    a change that loses recall score an *improvement*, which is precisely the self-flattering mutant `--self-test` refuses.

    Why nDCG beside recall and MRR: recall@k says "did it appear", MRR says "how high", and neither punishes moving an answer from
    rank 2 to rank 4 while both stay unchanged. A rerank stage's whole claim is about ordering inside the window, so a table that
    cannot see that movement cannot grade the stage.
    """
    if not results:
        return 0.0
    total = 0.0
    for r in results:
        if r.rank is not None and r.rank <= k:
            total += 1.0 / math.log2(r.rank + 1)
    return total / len(results)


def image_outranks_note(results: list[QueryResult]) -> int:
    """How often the top hit was an `image` while the labelled answer was a `note`.

    This is the failure mode design §14.4 cares about: a figure winning over the sentence that explains it. It counts only queries
    labelled `note`, because for an image-labelled query a figure on top is the point.
    """
    return sum(1 for r in results if r.kind == "note" and r.top_kind == "image")


def unsurfaced(results: list[QueryResult]) -> list[str]:
    return [r.query for r in results if not r.surfaced_somewhere]


# --------------------------------------------------------------------------- running the binary


@dataclass
class Run:
    modes: list[str] = field(default_factory=list)
    vectors_seen: dict[str, bool] = field(default_factory=dict)


def search(
    np_bin: str,
    workspace: str,
    query: str,
    mode: str,
    rerank: bool = False,
    rerank_text: str | None = None,
) -> dict:
    """One `np search --json`, refused loudly if the answer is not one JSON object.

    A refusal (no provider, no artifact) prints its message on stderr and exits 1 with **no** document; that is a real result about
    this machine — usually "this build has no runtime" — and it is reported as such rather than becoming a zero score in silence.
    """
    # **The harness drives the binary; it does not reimplement a flag.** `--rerank` and `--rerank-text` therefore travel to every
    # query as argv, which is also what makes an un-reranked row and a reranked one comparable: same binary, same code path, and the
    # only difference is what the process was told (**F51**).
    cmd = [np_bin, "search", workspace, query, "--mode", mode, "--json"]
    if rerank:
        cmd.append("--rerank")
    if rerank_text:
        cmd += ["--rerank-text", rerank_text]
    proc = subprocess.run(cmd, capture_output=True, text=True)
    if proc.returncode != 0 and not proc.stdout.strip():
        raise SystemExit(
            f"`{' '.join(cmd)}` refused (exit {proc.returncode}) and printed no document:\n{proc.stderr.strip()}\n"
            "That is a fact about this machine — usually a binary built without `--features local-model`, or a workspace with no "
            "artifact. Run the harness with `--modes keyword` in that shape, or install the model; do not read it as a score of 0."
        )
    try:
        return json.loads(proc.stdout)
    except json.JSONDecodeError as err:
        raise SystemExit(f"`{' '.join(cmd)}` did not print one JSON object: {err}\n{proc.stdout[:400]}")


def evaluate(
    np_bin: str,
    workspace: str,
    labels: list[tuple[str, str, str]],
    modes: list[str],
    k: int,
    rerank: bool = False,
    rerank_text: str | None = None,
) -> dict[str, list[QueryResult]]:
    """Rank position of each labelled answer, per mode, with `surfaced_somewhere` across all modes."""
    per_mode: dict[str, list[QueryResult]] = {m: [] for m in modes}
    documents: dict[str, list[dict]] = {}
    for query, expected, kind in labels:
        for mode in modes:
            doc = search(np_bin, workspace, query, mode, rerank, rerank_text)
            documents.setdefault(mode, []).extend(doc.get("hits") or [])
            hits = doc.get("hits") or []
            rank = None
            for index, hit in enumerate(hits, start=1):
                if hit.get("path") == expected:
                    rank = index
                    break
            top_kind = hits[0].get("kind") if hits else None
            per_mode[mode].append(QueryResult(query, expected, kind, rank, top_kind))

    # `surfaced_somewhere` is a **cross-mode** fact: an answer that appears in hybrid but not keyword is still in the index, and
    # calling it unsurfaced would blame the label file for what is a mode's ranking choice. Everything any mode returned went into
    # `documents`, so one membership test over all of them is the whole question.
    anywhere = {hit.get("path") for hits in documents.values() for hit in hits}
    for results in per_mode.values():
        for result in results:
            result.surfaced_somewhere = result.expected in anywhere
    return per_mode


# --------------------------------------------------------------------------- reporting


def report(per_mode: dict[str, list[QueryResult]], k: int, header: list[str]) -> int:
    print("\n".join(header))
    print()
    print(
        f"{'mode':<10} {'n':>3}  {'recall@1':>9} {f'recall@{k}':>{len(f'recall@{k}') + 1}}  "
        f"{f'ndcg@{k}':>{len(f'ndcg@{k}') + 1}}  {'MRR':>6}  {'img>note':>8}  {'unsurfaced':>10}"
    )
    strict_fail = 0
    for mode, results in per_mode.items():
        misses = unsurfaced(results)
        strict_fail += len(misses)
        print(
            f"{mode:<10} {len(results):>3}  {recall_at(results, 1):>9.3f} "
            f"{recall_at(results, k):>{len(f'recall@{k}') + 1}.3f}  "
            f"{ndcg_at(results, k):>{len(f'ndcg@{k}') + 1}.3f}  {mrr(results):>6.3f}  "
            f"{image_outranks_note(results):>8}  {len(misses):>10}"
        )
    print()
    for mode, results in per_mode.items():
        print(f"--- {mode}: per query")
        for r in results:
            where = "not in the list" if r.rank is None else f"rank {r.rank}"
            flag = "" if r.surfaced_somewhere else "   [unsurfaced: absent from every mode]"
            print(f"  {r.query!r:<48} wants {r.expected} ({r.kind}) -> {where}{flag}")
    print()
    print(
        "This is not a quality claim. F31 stays open until the label set's size and provenance are recorded in design §14, or the\n"
        "measurement is explicitly declined — see docs/backlog-m2-followups.md."
    )
    return strict_fail


# --------------------------------------------------------------------------- self-test: the arithmetic, not the model


def self_test() -> int:
    """Hand-computed expectations for the metric functions. Planted lists, no binary, no inference."""
    results = [
        QueryResult("cat mat", "a.md", "note", rank=1, top_kind="note"),
        QueryResult("broker ledger", "b.md", "note", rank=3, top_kind="image"),
        QueryResult("q3", "c.md", "note", rank=None, top_kind="note", surfaced_somewhere=False),
        QueryResult("diagram", "img/d1.png", "image", rank=2, top_kind="image"),
    ]
    checks = [
        ("recall@1", recall_at(results, 1), 0.25),  # only the first query is at rank 1
        ("recall@3", recall_at(results, 3), 0.75),  # three of four appear within 3 (the miss appears nowhere)
        ("recall@5", recall_at(results, 5), 0.75),
        ("mrr", mrr(results), (1.0 + 1 / 3 + 0.0 + 1 / 2) / 4),
        ("image_outranks_note", float(image_outranks_note(results)), 1.0),  # only the `note`-labelled rank-3 query
        ("unsurfaced_count", float(len(unsurfaced(results))), 1.0),
    ]
    failed = 0
    for name, got, want in checks:
        ok = abs(got - want) < 1e-12
        print(f"{'ok  ' if ok else 'FAIL'} {name}: got {got!r}, want {want!r}")
        failed += 0 if ok else 1

    # Two degenerate shapes that must not crash or divide by zero.
    for name, value in (("empty recall", recall_at([], 5)), ("empty mrr", mrr([]))):
        ok = value == 0.0
        print(f"{'ok  ' if ok else 'FAIL'} {name}: got {value!r}, want 0.0")
        failed += 0 if ok else 1

    # A mutant that excluded misses from MRR would raise the score; assert it does not.
    with_miss = mrr([QueryResult("q", "a.md", "note", rank=1, top_kind="note"),
                     QueryResult("q2", "b.md", "note", rank=None, top_kind="note")])
    ok = abs(with_miss - 0.5) < 1e-12
    print(f"{'ok  ' if ok else 'FAIL'} mrr counts a miss as zero rather than dropping it: got {with_miss!r}, want 0.5")

    # **nDCG, computed on paper.** Four queries whose answers land at ranks 4, never, 1 and 2, graded at k=10 with binary
    # relevance. One relevant document means the ideal gain is 1, so each term is its own discount:
    #   rank 4 -> 1/log2(5) = 0.430676558…   miss -> 0 (and still in the denominator)
    #   rank 1 -> 1/log2(2) = 1              rank 2 -> 1/log2(3) = 0.630929754…
    # Mean of the four: 0.515401578…. Written here as digits rather than recomputed from the same formula, because a self-test that
    # calls the function it grades proves nothing.
    ndcg_results = [
        QueryResult("a", "a.md", "note", rank=4, top_kind="note"),
        QueryResult("b", "b.md", "note", rank=None, top_kind="note"),
        QueryResult("c", "c.md", "note", rank=1, top_kind="note"),
        QueryResult("d", "d.md", "note", rank=2, top_kind="note"),
    ]
    want = 0.515401578
    got = ndcg_at(ndcg_results, 10)
    ok = abs(got - want) < 1e-9 and ok
    print(f"{'ok  ' if abs(got - want) < 1e-9 else 'FAIL'} ndcg@10 over ranks [4, miss, 1, 2]: got {got!r}, want {want}")

    # Truncation is the `@k` in nDCG@k: the same list graded at k=3 cannot see the answer at rank 4.
    want_3 = (0.0 + 0.0 + 1.0 + 0.630929754) / 4
    got_3 = ndcg_at(ndcg_results, 3)
    ok = abs(got_3 - want_3) < 1e-9 and ok
    print(f"{'ok  ' if abs(got_3 - want_3) < 1e-9 else 'FAIL'} ndcg@3 drops the rank-4 answer: got {got_3!r}, want {want_3}")

    # **The mutant that flatters itself.** One query's answer slides from rank 5 to nowhere: recall falls, and so must nDCG — over
    # **all** queries. Averaging over answered queries only would report the *same* number for the second list as for a list where
    # that query never existed, which is how a stage that loses coverage could be written up as an improvement.
    before = [QueryResult("q", "a.md", "note", rank=5, top_kind="note"), QueryResult("r", "b.md", "note", rank=1, top_kind="note")]
    after = [QueryResult("q", "a.md", "note", rank=None, top_kind="note"), QueryResult("r", "b.md", "note", rank=1, top_kind="note")]
    lost = ndcg_at(after, 10) < ndcg_at(before, 10)
    ok = lost and ok
    print(
        f"{'ok  ' if lost else 'FAIL'} nDCG falls when an answer disappears (misses stay in the denominator): "
        f"{ndcg_at(before, 10):.6f} -> {ndcg_at(after, 10):.6f}"
    )
    ok = (ndcg_at([], 10) == 0.0) and ok
    print(f"{'ok  ' if ndcg_at([], 10) == 0.0 else 'FAIL'} empty result set is 0.0, not a crash: got {ndcg_at([], 10)!r}")
    failed += 0 if ok else 1
    return 1 if failed else 0


# --------------------------------------------------------------------------- plumbing


def read_labels(path: str) -> list[tuple[str, str, str]]:
    labels: list[tuple[str, str, str]] = []
    with open(path, encoding="utf-8") as handle:
        for line_number, raw in enumerate(handle, start=1):
            line = raw.rstrip("\n")
            if not line.strip() or line.lstrip().startswith("#"):
                continue
            parts = line.split("\t")
            if len(parts) < 2:
                raise SystemExit(f"{path}:{line_number}: expected `query<TAB>path[<TAB>kind]`, got {line!r}")
            query, expected = parts[0].strip(), parts[1].strip()
            kind = parts[2].strip() if len(parts) > 2 and parts[2].strip() else "note"
            if kind not in ("note", "image"):
                raise SystemExit(f"{path}:{line_number}: kind must be `note` or `image`, got {kind!r}")
            labels.append((query, expected, kind))
    if not labels:
        raise SystemExit(f"{path}: no label rows. A labelled set of zero measures nothing; say so on the F31 card instead.")
    return labels


def main(argv: list[str]) -> int:
    parser = argparse.ArgumentParser(description=__doc__.splitlines()[0])
    parser.add_argument("workspace", nargs="?", help="workspace root to query")
    parser.add_argument("--labels", help="TSV of `query<TAB>expected-path[<TAB>kind]`")
    parser.add_argument("--np", default="target/release/np", help="path to the np binary")
    parser.add_argument("--modes", default="keyword,semantic,hybrid")
    parser.add_argument("--k", type=int, default=5)
    parser.add_argument("--strict", action="store_true", help="exit 3 when any labelled answer is absent from every mode")
    parser.add_argument(
        "--rerank",
        action="store_true",
        help="pass `--rerank` to every query, so a row of the table measures the cross-encoder stage (**F51**)",
    )
    parser.add_argument(
        "--rerank-text",
        choices=["title-body", "body"],
        help="pass `--rerank-text` to every query; the axis that asks whether a note's title helps (**F51**)",
    )
    parser.add_argument("--self-test", action="store_true", help="check the metric arithmetic and exit")
    args = parser.parse_args(argv)

    if args.self_test:
        return self_test()
    if not args.workspace or not args.labels:
        print("need <workspace> and --labels (or --self-test)", file=sys.stderr)
        return 2

    modes = [m.strip() for m in args.modes.split(",") if m.strip()]
    unknown = [m for m in modes if m not in ("keyword", "semantic", "hybrid")]
    if unknown:
        print(f"unknown mode(s): {', '.join(unknown)} — the three names are keyword, semantic, hybrid", file=sys.stderr)
        return 2

    labels = read_labels(args.labels)
    version = subprocess.run([args.np, "--version"], capture_output=True, text=True).stdout.strip()
    header = [
        f"F31 retrieval harness — {len(labels)} labelled queries against {args.workspace}",
        f"binary: {args.np} ({version})",
        f"modes: {', '.join(modes)}   k={args.k}   label file: {args.labels}",
        # **The shape of the run travels above the table**, because a 0.02 difference over eight queries is not a finding unless a
        # reader can see what else was true: whether the stage ran at all, which text it read, and how many queries were asked.
        # Without these lines one row of numbers can be re-quoted beside a different binary, mode set or k and look comparable.
        "rerank: "
        + (
            f"on, text={args.rerank_text or 'title-body (config default)'} (**F51**)"
            if args.rerank
            else "off (retrieval order)"
        ),
    ]
    per_mode = evaluate(args.np, args.workspace, labels, modes, args.k, args.rerank, args.rerank_text)
    misses = report(per_mode, args.k, header)
    if args.strict and misses:
        print(f"\n--strict: {misses} labelled answer(s) never surfaced. Fix the label file or the index before quoting any number.", file=sys.stderr)
        return 3
    return 0


if __name__ == "__main__":
    sys.exit(main(sys.argv[1:]))
