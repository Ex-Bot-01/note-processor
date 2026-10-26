# F21 — announce an upgrade's deletions durably, not on a stream

Status: design for the first round of `m3/report-fidelity`'s F21 work. Backlog: `docs/backlog-m2-followups.md`
F21; related: F22 (a refusal that exists only as one report line), R8 (`9541806`, which made every *other*
post-commit stderr write non-fatal). Code: `crates/np-core/src/db.rs`'s `add_note_foreign_keys` → `report_orphans`.

## 1. The defect

Migration 3 removes orphan rows, writes a receipt into `meta` under `FK_ORPHANS_KEY`, and then calls
`report_orphans(&mut io::stderr(), &removed)`, which does `.expect("printing the orphan receipt to stderr")`. That
panic happens **inside the migration transaction**, so a workspace that holds orphans and is opened with descriptor 2
refusing writes (`|& head`, a detached service, `/dev/full`) rolls the whole upgrade back — every time, forever. The
program refuses to upgrade rather than lose one line of output.

The reasoning on record is "never delete rows unannounced", and that principle stays. What does not survive is the
claim that panicking protects the signal: the same receipt is written durably into `meta` **two statements earlier,
in the same transaction**. The panic guards a copy of what it destroys.

## 2. The rule

**An announcement is durable first and echoed second.** The record of what an upgrade deleted lives in the database;
a stream is a convenience for the person watching, and no stream may be load-bearing for a fact the program promises
never to hide. Consequences:

- `report_orphans` becomes best-effort: `let _ = writeln!(…)`. It says why the panic went: not "output does not
  matter", but "this signal has a durable home now, and refusing an upgrade is a worse silence than an unprinted
  line".
- **Never delete unannounced still binds.** The deletion may not commit unless the `meta` receipt write succeeded —
  which it does, in the same transaction, so a failed receipt rolls the cleanup back. Verify that with a test rather
  than trusting statement order: force the receipt write to fail (a read-only connection or a constraint) and assert
  no rows were deleted.
- The receipt is **history, not a queue**: it is never cleared, because "the upgrade removed N rows" stays true.

## 3. Surfacing it again without nagging forever

`np scan` is the only command in this program (`np-cli` has no `status`), so the second announcement has to ride the
scan report. Rule: **a scan echoes the receipt only when its own process performed that migration.**

- `Db::open` remembers, for the lifetime of the handle, whether *this* open ran the orphan cleanup, and keeps the
  value it wrote.
- `ScanReport` gains one additive field — `pub upgrade_orphans: Option<String>` — holding that stored value when this
  process did the cleanup **and the cleanup removed at least one row**, `None` otherwise. The stdout summary prints one
  line naming the total and the counts; `--json` carries the same string, read from the same field, so the two channels
  cannot disagree. Nothing is repeated on later scans, because by then the user has been told once and the record is
  still in `meta` for whoever asks with `sqlite3`.
- Why "this process" rather than "whenever the key is non-zero": a permanent one-line warning on every scan would
  train the user to ignore it, which is how real signals die. And why not clear the key after echoing: clearing makes
  the echo load-bearing again — if both streams fail once, the fact is gone.

### 3.1 The all-zero case, settled in the round

A brand-new workspace runs migration 3 too, and its receipt is `headings=0,tags=0,…`. Three readings were available
for what the field should then hold:

- **(a) print whenever the cleanup ran.** Rejected: the first scan of every fresh workspace would print
  `removed 0 orphan row(s)`, which states a deletion that did not happen — the same noise the pinned all-zero silence
  in `report_orphans` exists to prevent.
- **(b) keep the field as stored, suppress only the stdout line at zero.** Rejected: stdout and `--json` would then
  disagree about one fact, and channel disagreement is what F21 and F22 are about. It should not arrive as a side
  effect of tidiness.
- **(c) gate the field itself on total > 0** — chosen. `upgrade_orphans` means "this upgrade deleted rows; here is the
  receipt", not "migration 3 ran", and both channels follow from that one value by construction.

The durable record is unchanged by this: the all-zero receipt **is** still written to `meta`, because its job is to
prove the check ran ("looked and found none" against "this file never upgraded"). So the receipt and the echoed field
differ at zero deliberately — the receipt records that the look happened, the echo reports a loss.

## 4. Invariants

1. **No panic on a refusing descriptor 2 anywhere in the upgrade path.** R8 established this for post-commit writes;
   this round extends it to migration. A test must open an orphan-holding version-2 workspace with stderr closed and
   assert the upgrade completes and `np scan` exits 0.
2. **Deletion without a durable receipt is impossible.** Enforced by the same transaction, pinned by a test (§2).
3. **`ScanReport` changes additively only**; catalog output and every file under `tests/golden/` stay byte-identical.
4. **No new stream-dependent signal.** If this round needs to tell a person something that must survive, it goes in
   `meta`, not on stderr.

## 5. Tests this round owes

- Version-2 workspace with orphans, descriptor 2 closed (R8's `a2cd0ba` shows the technique — spawn directly, since
  `assert_cmd` replaces inherited stdio): upgrade completes, exit success, rows gone, `meta` receipt present, stdout
  summary names the orphan total.
- Same workspace, second scan: no repeat line, `upgrade_orphans` is `None`, receipt still in `meta`.
- Receipt-write failure: orphans survive (transaction rolled back), and the error surfaces as an error rather than a
  panic.
- The existing `report_orphans` unit tests keep their assertions — they pin wording and the all-zero silence, none of
  which changes; only the failure mode does.
- The all-zero case §3.1 settles: a fresh workspace's first scan has `upgrade_orphans == null`, prints no line, and its
  `meta` still holds the all-zero receipt. (`crates/np-cli/tests/cli.rs`,
  `an_upgrade_that_removed_nothing_is_announced_nowhere`.)
- And the unit-level half of §4.1: a sink that returns `ENOSPC` for every write does not abort the migration
  (`crates/np-core/src/db.rs`, `a_sink_that_refuses_writes_does_not_abort_the_upgrade`). It cannot replace the
  closed-descriptor case above, because only a real descriptor 2 pins the production call site.

## 6. Gates

The standing list, no subsetting: `cargo fmt --check`; `cargo clippy --all-targets -- -D warnings`;
`cargo test --workspace`; `env -u HOME -u XDG_CONFIG_HOME cargo test -p np-core --test config`;
`RUSTDOCFLAGS="--deny warnings" cargo doc --locked --workspace --no-deps`; the cfg dead-code check; the golden
verifier → `GOLDEN CHECK: pass`. CI green on Linux **and** Windows is part of "green"; nothing under `tests/golden/`
is ever regenerated.
