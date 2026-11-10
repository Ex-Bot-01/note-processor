# What np's diagnostics cost

Measurement, not design — and **not a gate**. One memory-tight box produced every number below, in one afternoon, with
nothing else loud on the machine. Nobody should read these as a fence a CI run can assert against: they are here so that
"diagnostics are free when you do not ask for them" is a number somebody wrote down rather than an assertion nobody
tested. The design decisions they support are in
[`docs/superpowers/specs/2026-12-10-np-diagnostics-design.md`](superpowers/specs/2026-12-10-np-diagnostics-design.md);
how to read the output is in [`docs/reference-diagnostics.md`](reference-diagnostics.md).

## Method

* **Binary:** `np` at commit `6cd7bbb`, built with `cargo build --release -p np-cli`. Same binary for every row.
* **Host:** 16 CPUs, 7,419 MB total RAM (`free -m`), Linux, the same box F30's measurement was taken on
  (`docs/measurement-f30-vector-surface.md`). Debug builds were not used: a debug `np` spends its time on assertions and
  would hide a 15 % effect inside its own noise.
* **Work:** `np scan <vault>` against a **warm database** — one untimed scan first, then timed runs against the same
  data directory, so what is measured is a warm pass over an indexed vault rather than cold catalog construction. The
  page cache is warm too: every setting re-reads the same files.
* **Corpora.** Two, because one number from one corpus is how measurements become misleading:
  * the repository's own fixture corpus, `tests/fixtures/corpus/` — **16 notes, 84 KiB**, copied to a scratch vault. It
    is small enough that process start-up dominates and honest about that: this is what a tiny vault costs to open, not
    what a vault costs to scan.
  * a generated vault, `notes/note-00000.md` … `note-01999.md`, one heading and ~700 bytes of prose each — **2 000 notes,
    8 MiB**. At measurement time the vault held **2 005** markdown files, because scanning writes an index file into the
    vault it scans and previous runs left four disambiguated siblings behind. That is 0.25 % of the work and it is left
    in rather than tidied away.
* **Settings**, varied only by environment and config — never by rebuilding:

  | Setting          | How asked                                       | What it exercises                                                        |
  | ---------------- | ----------------------------------------------- | ------------------------------------------------------------------------ |
  | `off`            | `NP_LOG=off`, no `[log]` keys                    | the machinery exists and nothing is emitted                                |
  | `trace-scan-off` | `NP_LOG='trace,np_core::scan=off'`               | the filter's hot path on every record, with the noisy phase switched out   |
  | `trace-stderr`   | `NP_LOG=trace`                                   | every record formatted and written to stderr                               |
  | `trace-file`     | `[log] level = "trace"`, `[log] file = PATH`     | both channels: a trace config asks for stderr as well as the file          |

* **Timing:** 9 rounds, one run per setting per round, **interleaved** so that a load spike lands on all four settings
  instead of on whichever ran last. Reported are the minimum (the machine's honest floor) and the median (what a person
  would notice). `capture_output` in the harness, so pipes were drained continuously — a full pipe is a different
  question and it has its own answer in the reference.
* **RSS:** peak resident set of one child, measured with `os.wait4` (`rusage.ru_maxrss`), minimum of 3 runs per setting.
* **Not measured here,** because nothing in this build can ask for it: embedding and fetch cost. Both are dominated by
  model work or network transfer, three and six orders of magnitude above a formatted line; the diagnostic lines those
  phases emit number in the tens per run.

## Results

Warm `np scan`, wall clock:

| Corpus                       | setting        | min ms | median ms | stderr bytes | peak RSS KiB |
| ---------------------------- | -------------- | ------ | --------- | ------------ | ------------ |
| fixture corpus, 16 notes     | `off`          | 2.4    | 2.5       | 26           | 15 548       |
| fixture corpus, 16 notes     | `trace-scan-off`| 2.4   | 2.6       | 28           | 15 548       |
| fixture corpus, 16 notes     | `trace-stderr` | 2.5    | 2.8       | 3 050        | 15 548       |
| fixture corpus, 16 notes     | `trace-file`   | 2.7    | 2.7       | 3 055        | 15 548       |
| generated vault, 2 005 files | `off`          | 56.8   | 57.0      | 26           | 21 276       |
| generated vault, 2 005 files | `trace-scan-off`| 56.6  | 56.9      | 28           | 21 276       |
| generated vault, 2 005 files | `trace-stderr` | 63.2   | 63.8      | 165 126      | 21 276       |
| generated vault, 2 005 files | `trace-file`   | 66.9   | 67.6      | 165 131      | 21 276       |

Against `off` on the same corpus:

| Setting          | 16 notes | 2 005 files |
| ---------------- | -------- | ----------- |
| `trace-scan-off` | −2.9 %   | −0.2 %      |
| `trace-stderr`   | +2.1 %   | +11.3 %     |
| `trace-file`     | +11.2 %  | +17.8 %     |

## What the numbers say

**Asking for nothing costs nothing measurable.** On the 2 005-file vault, `off` and `trace-scan-off` differ by 0.2 ms
across a 57 ms run — less than the spread *within* either setting's nine rounds. That is the claim spec point 9 needed:
a span opened, a target compared, a level checked, and the process is the same speed. It is also the shape the design
asked for — an event whose level no filter admits formats nothing, so `trace-scan-off` is not "trace with one phase
muted" but "the whole pipeline running, producing almost no bytes" (28 of them, and 26 of those is the announce line).

**The cost is in what you asked to be written, and it scales with bytes, not with records considered.** Asking for
`trace` everywhere writes 165 KiB on this corpus and costs 11 %; asking for the same level in a file as well costs 18 %.
Both numbers are honest and neither is the machinery's fault: an unfiltered trace of a 2 000-file scan is roughly one
line per file, and writing two copies of ~1 400 lines of JSONL and human text is the work being measured. A vault twice
the size pays twice that — this is a cost per byte written, which is why stderr is capped at 1 000 lines and the file at
a budget rather than left open-ended.

**Peak RSS did not move**, identical to the KiB across all four settings on both corpora. The subscriber, its level
tables and one buffered file writer are small enough that this box's allocator does not notice; nothing in the diagnostic
path accumulates per-note state, which is what would have shown up here.

**The 16-note corpus cannot resolve any of this.** Its whole run is 2.4 ms against a ~2 ms process start-up, so its
spread (−2.9 % to +11.2 %) is mostly the noise floor and should be read as "within measurement error", not as a ranking.
It is reported anyway because it is the corpus in the repository, and anyone can reproduce it in one command.

## Reproducing

```console
$ cargo build --release -p np-cli
```

The driver script is not checked in — a measurement harness that nobody maintains is a lie waiting to be re-run — so here
is what it has to do for these numbers to mean what they say, in order: copy `tests/fixtures/corpus/` to a scratch vault;
generate the 2 000 notes; give **each setting its own data directory and config file** (sharing them measures one
setting's cache under another's name, which is exactly the bug this harness had before it produced any of the numbers
above); warm each with an untimed scan; then run 9 interleaved rounds and report minimum and median per setting.
`NP_NO_AUTO_FETCH=1` throughout, so no row silently depends on the network. One caution from building it: a harness that
warms a directory and then reuses the *warm* environment for the timed runs measures `off` four times and reports the
difference as zero — which is what the first version of this table said before it was fixed.

## The behavioural half

Wall clock is one half of "free when off"; the other is that nothing is *formatted* — a level check that passes still
builds a string, and that is what would quietly cost real time in a loop. That half is asserted, not measured:

* `stderr_stays_silent_at_default_level` (`crates/np-cli/tests/diag.rs`) runs a real command with nothing asked for and
  asserts stderr holds zero bytes — not "a small amount". The other half of that promise, that stdout's summary is the same
  bytes it was before diagnostics existed, is pinned where stdout is pinned: `scan_writes_the_catalog_and_reports_it` in
  `crates/np-cli/tests/cli.rs`, which compares whole documents and counted lines against a fixture vault.
* `verbose_prints_the_effective_level_and_its_source` pins the other edge: asking changes exactly one line plus whatever
  passed the filter, so a level cannot silently widen what a run formats.
* Every event assertion in `crates/np-core/tests/diagnostics.rs` goes through the capture subscriber in
  `crates/np-core/tests/capture.rs`, which records only what some level admitted. The two that run at `trace` —
  `no_diagnostic_carries_a_note_body` and `every_rule_this_crate_emits_is_a_published_token` — are the ones an
  unfiltered emission would break: an event that ignored its filter would appear as an unexpected record in a list this
  file compares exactly, rather than as slack in somebody's scan.
