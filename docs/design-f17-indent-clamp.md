# F17 — refuse an impossible `continuation_indent` at config load

Design note, 2026-10-27. Backlog: `docs/backlog-m2-followups.md` §F17. Roadmap: Track A item **A2**. Branch:
`m8/indent-clamp`. Spec anchor: the catalog's 100-column budget (`catalog::COLUMN_LIMIT`) and spec §14's rule that a path is
never broken.

## 1. What is accepted today, and what it costs

`CatalogConfig::continuation_indent` is a plain `usize` with `#[serde(default)]` (`config.rs:52`, default 2) and **no
validation anywhere on the load path**. Two consequences, in ascending order of badness:

1. **Layout collapse.** `wrap(line, indent)` charges `indent` columns to every continuation line before its first token
   (`catalog.rs:214`). At `indent = 95` a continuation line has four columns of budget, so the catalog renders one token per
   line — legal bytes, absurd output, and no warning. At `indent ≥ COLUMN_LIMIT` the budget is negative-in-effect and *every*
   continuation line is a single token by the wide-token exemption.
2. **Allocation abort.** The indent becomes `" ".repeat(indent)` (`catalog.rs:230`). `continuation_indent = 4000000000` in
   `config.toml` asks for four gigabytes per wrapped entry; `usize::MAX` asks for the address space. A typo in a text file
   therefore does not produce odd output, it **aborts the process**. That is the reason this belongs at load time and not in
   `wrap`: by the time the renderer runs, the failure mode is memory, not layout.

The sibling field already defends itself — `max_entries_per_file` is used as `.max(1)` (`catalog.rs:94`) — so this asymmetry
is the only unvalidated numeric knob left in `CatalogConfig`, and it is the one whose bad value costs memory.

## 2. Decision D1 — reject, do not clamp

Clamping would render bytes the user did not ask for. The catalog is published output whose bytes are pinned by goldens and
compared by a fingerprint (`np_published`); silently substituting 16 for the reader's 4000 means the file on disk disagrees
with the config that produced it, which is the same class of surprise F21 refused to introduce for upgrade notices. **Reject
at load**, the way a `data_dir` that resolves to nothing is refused rather than defaulted (`config.rs:151-158`).

## 3. Decision D2 — where the error type goes, without touching the mandated enum

The plan fixes the error shape: `ConfigError { Io, Parse, MissingField(&'static str) }`
(`docs/superpowers/plans/2026-10-23-note-processor-m1-core-catalog.md:88`). Options weighed:

- **New variant** `InvalidValue { .. }` — cleanest API, and rejected: it amends a mandated shape for a message. If the human
  wants it, it is one variant plus a plan edit, and nothing here would have to change but the constructor.
- **`ConfigError::Parse`** via `toml::de::Error::custom` — displays as *"config parse error: …"*, which accuses the reader's
  TOML syntax of being broken when it parsed perfectly. Rejected as misleading.
- **Chosen: `ConfigError::Io` with `ErrorKind::InvalidData`.** There is exact precedent two screens away: SQLite failures
  travel through `Io` because "the mandated enum has no dedicated variant, so the driver message is preserved as the payload"
  (`config.rs:36-38`). The kind is the honest one — the file was read and its **data** was rejected — and a caller that cares
  can match `InvalidData` instead of parsing prose.

The composed CLI line then reads:

```text
configuration error: config io: invalid catalog.continuation_indent = 999 (allowed 0..=16)
```

with exit code 1, which is what the backlog's "done when" asks for: a failure naming the key rather than odd rendering. The
`config io:` prefix is the price of D2 and is stated here rather than discovered in review.

## 4. The bound

`pub const MAX_CONTINUATION_INDENT: usize = 16;` — **inclusive**, `0..=MAX`, so `0` (no indent) stays legal.

Where the number comes from, said plainly because it is not derivable from `COLUMN_LIMIT`: any indent below 100 still
*renders*, and any indent above a few columns already costs a sixth of the line to whitespace. 16 is a product bound — wider
than anything this catalog has ever been laid out with, comfortably inside the budget — chosen so that a typo (`2` fat-fingered
to `20`, or a stray `9`) fails loudly instead of quietly reflowing published bytes. It is **not** a mathematical limit and the
doc comment must not imply one; the mathematical hazard (allocation) is removed by any finite bound whatsoever, which is why
the specific value is a judgement call rather than a proof.

Validation runs at the end of `Config::load`, after file and environment merge, so it covers every source a value can arrive
from, including a future env override. It does **not** run on a `Config` built in-process: catalog tests construct
`CatalogConfig { continuation_indent: 5 }` directly and `wrap` keeps accepting any in-range-or-not value it is handed — the
renderer's job is layout, the loader's job is refusal. That split is also what keeps this change from touching a single
golden byte: every fixture that renders uses the default or an explicit in-process value.

## 5. Tests

1. **Load refusal** (`crates/np-core/tests/config.rs`): a file with `[catalog] continuation_indent = 17` fails `load`, and
   the message names the key, the offending value and `0..=16`. Assert on the *message*, not only on `is_err()`: the backlog's
   done-when is that a person can tell which key broke.
2. **Boundaries**: `16` loads (and round-trips into `cfg.catalog.continuation_indent`), `0` loads, and the default is still 2
   when the key is absent — the last one is what proves this did not quietly change defaults.
3. **CLI channel** (`crates/np-cli/tests/cli.rs`): `np scan` against a workspace whose config names an oversized indent exits
   1 with stderr naming `continuation_indent`. The library test cannot show the exit code, and F21's rule applies: the channel
   a person actually reads needs its own test.
4. **No allocation on the refusal path**: implied by (1) — the process never reaches `wrap` — but worth one comment saying so,
   because it is the reason the check is not in `catalog.rs`.

## 6. Out of scope

`max_entries_per_file` (already defended at use), any change to `wrap`'s algorithm or its wide-token exemption, and any new
config key. No migration: config values are not stored, so a workspace that scanned under an old bad value simply scans again
after the file is fixed — nothing on disk has to be reconciled, and no goldens move.
