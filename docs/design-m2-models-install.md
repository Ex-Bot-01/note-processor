# M2a Round 4a — `np models install`: pin a model artifact without a network path

Date 2026-10-29, revised same day after design review (verdict: blocked → corrections below). Branch
`m12/embeddings` (`06302c4`). Parent design: `docs/design-m2-embeddings.md` §2 (provider stays `none` by default),
§3 (`models` table, migration 7), §5 (transaction rule), §6 (benchmark order and the secret rule).

## 1. Why this round exists, and why it is cut here

Migration 7 created `models(id, dim, sha256, prefix_doc, prefix_query, installed_at)` and `embeddings.model` has a
foreign key to it. So today **nothing can be embedded even if a provider existed**: there is no way to put a model row
on the table, and no way to put an artifact on disk that the row could name. This round supplies exactly that, from
the side that needs no network and no model decision.

The cut is deliberate:

- **4a (this design)** — install a file the human already has, verify it, record it. Offline, CI-testable, no
  credentials, and it changes nothing about a workspace that never runs the command.
- **4b** — transport (HTTP) and the benchmark. Both need facts I do not have: whether the self-hosted server serves
  `/v1/embeddings` at all, which model, what its vectors are worth. §6 makes the benchmark gate the model choice, so
  writing an HTTP client first would choose the answer by convenience.

**Deliberate deviation from the spec, recorded rather than hidden.** `spec:88` and `spec:236` describe
`np models install` as *fetching* an artifact and slicing it into the data dir. 4a installs a local file only. §9 asks
whether it ever accepts a URL; until that is answered this stands as a deviation from those two spec lines, not as
their implementation.

Environment facts this cut rests on (checked 2026-10-29 on `pi`, euid 1000): `NP_EMBED_BASE_URL` and
`NP_EMBED_API_KEY` are **unset**; ~9.1 GB free on `/home`; egress to github.com answers; no HTTP client is a
dependency of any crate here (np-cli depends on clap, np-core, serde_json — `rusqlite` is dev-only, used by
`tests/cli.rs`).

## 2. Command surface

```
np models install <workspace> --id <id> --path <file> --dim <n> --sha256 <hex>
                            [--prefix-doc <s>] [--prefix-query <s>] [--json]
np models hash <file>
np models list <workspace> [--json]
```

- **`<workspace>` is required on `install` and `list`.** This is the review's P0 and it was right: `models` lives in
  the **per-workspace** database (`Config::workspace_db`, `config.rs:287-293`, which derives
  `<data_dir>/workspaces/<hash-of-abs-path>.sqlite`; migration 7 creates the table there). The registry database
  holds only `workspaces`. A command with no workspace operand cannot resolve a database at all, and a global artifact
  directory next to per-workspace rows would let workspace B's install rename over the file workspace A's row names.
  So both the row and the artifact are scoped to one workspace (§4). `hash` stays path-only — it touches no database.
- **`--sha256` is mandatory for `install`.** The point of the row is that a vector written against model `X` can be
  trusted to come from bytes `X`. A human who does not know a digest gets it from `np models hash <file>` and pastes
  it into `install` — two honest steps instead of one command that records a checksum it computed from the very file
  it is installing, which pins nothing.
- **`--dim` is declared by the human, not probed**, and is bounded: it parses as `u16`, so `0` is refused (a dim-0 row
  admits zero-byte vectors that satisfy migration 7's `length(vector) = 4 * dim`) and anything above `65535` is
  refused naming that ceiling. This matches the width already chosen for the same quantity in
  `EmbeddingsConfig::dim: Option<u16>`. Refusing CLI-side keeps §6's "no new migration". A *wrong* dim is caught later
  by `EmbedError::DimensionMismatch` — and that write path is currently raw-`DbError` debt (round 3b review), recorded
  here rather than fixed sideways.
- **`--id`** must match `[a-z0-9][a-z0-9._@-]{0,63}`, **not end in `.`**, and not be a Windows reserved device name
  (`con prn aux nul com1-9 lpt1-9`, with or without a suffix). The charset is not cosmetic — the id becomes a
  filename. Trailing `.` is legal on POSIX and stripped by NTFS, so `foo` and `foo.` would be two BINARY-distinct rows
  over one artifact; a reserved name is a row whose file cannot exist. Uppercase is already rejected, which is why
  there is no case-collision finding to make: `models.id` compares with BINARY collation while NTFS names do not, and
  the charset is what keeps those two notions from disagreeing.
- **No `--force`.** Replacing a model identity invalidates every vector stored against it; this project refuses that
  (F13, F19, F23).

## 3. What install does, in order

0. Resolve `<workspace>` and **refuse it when it does not exist**. `Config::workspace_db` is infallible by contract
   and deliberately derives a database for a path that is not there (`config.rs:276-280`), and the name it derives is
   a hash — so a typo would "succeed" into an empty database and an artifact directory hundreds of megabytes deep that
   no scan will ever read. That is a wrong status claim, which this project treats as a defect (`scan.rs:358-361`
   fails outright on an unopenable workspace). So: `metadata(workspace)` must say *directory*, else refuse naming the
   path. Then open that workspace's database through `Config::workspace_db`, and derive the artifact directory through
   **a new `Config::workspace_models_dir(&Path) -> PathBuf`** placed beside `workspace_db` and sharing its private
   name-derivation helper.

   That helper is not a convenience. `abs_path` is `pub(crate)` (`config.rs:325`), and `hex_prefix` and `DB_HASH_BYTES`
   are private (`config.rs:354`, `:19`), so without it np-cli would have to string-munge the `workspace_db` path or
   re-implement the digest — and a re-implementation over `path.display()` diverges for non-UTF-8 paths, while one
   using `canonicalize()` diverges because `workspace_db` deliberately resolves no symlinks. Either mistake separates
   rows from artifacts by another door, which is exactly the P0 this section exists to close.
1. Validate `--id` (charset, trailing `.`, reserved names) **before any file access**, naming the rule that failed.
2. Refuse an empty source: `fs::metadata(&path)?.len() == 0` → "refusing a zero-length artifact", before hashing. A
   truncated-to-nothing download must get that diagnosis, not a digest mismatch.
3. Digest with **`np_core::hash::sha256_file(&path)`** — do not open the file in np-cli. `hash.rs` streams
   (`READ_CHUNK_BYTES`, `hash.rs:19,111-120`) and returns lowercase hex (`hex`, `hash.rs:129`). There is no public
   `Read`-based API and none is added: adding one so np-cli can hand-roll the loop would introduce a second SHA-256
   definition and a `sha2` dependency, which §7 test 9 exists to forbid.
4. Validate `--sha256` as exactly 64 hex characters and **lowercase it once**; every later comparison and the stored
   column use that lowered spelling. Without this, an install with an uppercase digest produces a row that every
   subsequent lowercase comparison calls "different sha256" — and with no `--force`, that id is permanently
   un-installable. On mismatch, refuse and print **both** digests: the file's digest is public information about a
   file the human already holds, and its absence is what makes a mismatch diagnosable.
5. Read the existing row for `id` (new helper `db::model_row`). If one exists, compare the **whole declared tuple** —
   `(dim, sha256, prefix_doc, prefix_query)`. Any disagreement → refuse, naming stored and declared values side by
   side; a sha-only comparison would report "already installed" while quietly changing `models.dim`, which is the
   `expected` of every later dimension refusal. Full agreement → step 6's missing-file check, then report
   `already installed` and write nothing.
6. Artifact directory: `<data_dir>/workspaces/<db-hash>.models/`, from `workspace_models_dir` — the *same* key as
   `workspace_db`, so one row and one file always belong to one workspace (`.models` rather than a directory named
   `<db-hash>` keeps that listing free of a name that is both file and folder; SQLite writes only
   `<hash>.sqlite{-wal,-shm}` there). Copy the source to a temporary name **`<id>.<nonce>.tmp`** in that directory and
   `rename` it into place as `<dir>/<id>` — the catalog publish's discipline: no reader ever sees half an artifact, and
   a failed copy leaves no partial file. The temp spelling is pinned because §7 test 10's premise is precisely a
   leftover from a crash here: a `*.tmp` entry is never claimed by a row and `list` reports it under its own heading.
   **One hash of the source** (step 3). The review asked for exactly one `sha256_file` call per install, on the
   arithmetic that row sha = `--sha256` = the source's digest — and that arithmetic is right about *whether to copy*.
   It is wrong about **what may be claimed**: when an artifact is already present, the thing being reported on is that
   file, not the source, and the source's digest says nothing about bytes that have sat on disk since the earlier
   install. Answering `already installed` over a rotted artifact is precisely the false status claim this project
   treats as a defect (F21), so **this one arm — row agrees and artifact present — also hashes the artifact**, and
   refuses with `ArtifactDisagrees { path, expected, found }` rather than overwriting it. Cost: one read of the
   artifact on a repeat install, which is an interactive command where the alternative is lying. Nothing else in
   install re-reads anything.

   `list` does hash every artifact with a non-null digest, because its four states are claims about bytes and an
   unverified "present" would be a weaker word than the design promises. For several hundred-megabyte models that is
   seconds per invocation; it is an explicit human command, not something the scan calls. If that ever needs to be
   cheap, the honest shape is a separate `--quick` that says *unverified* out loud rather than quietly softening
   `present`.

   Every path in that operation — `create_dir_all`, the temp write, the rename — goes through `walk::to_open_path`,
   not just the final name: a long temp name can cross the prefix threshold while the final name does not
   (`walk.rs:384-400`), and mixing verbatim and `\\?\` spellings inside one operation is worse than either.
   - **Missing artifact with a live row** (row exists, digests agree, file is gone): verify the file against the
     **row's** sha256; if it matches, re-copy and report `restored`, rewriting no column — this is re-materialising
     what the row already attests, not repair. If the file exists and does not match, that is step 4's refusal, never
     an overwrite.
7. Insert the row with a new np-core helper **`db::insert_model(db, id, dim, sha256, prefix_doc, prefix_query, now)`**
   after the rename succeeds, and print nothing before it (F21: durable record first, echo second). No transaction
   language: one `INSERT` is already atomic in SQLite, and np-cli has no `rusqlite` dependency to build one with —
   naming a helper rather than "in one transaction" also keeps this repo's rule that every write is a named function
   taking `&Db`. What genuinely cannot be atomic is **rename vs row insert**; the crash window between them yields a
   file with no row, which step 5 tolerates and step 6 re-materialises.

`np models hash <file>` prints `<sha256>  <path>` and touches no database.

`np models list <workspace>` prints id, dim, sha256 (12 hex, full value under `--json`), `installed_at`, prefixes, and
a per-row state drawn from **exactly four** values:

| state | meaning |
|---|---|
| `present` | the row's digest matches the artifact on disk |
| `missing` | the row expects an artifact and none is there — named, never repaired |
| `no artifact expected` | `sha256 IS NULL`: a provider with no local weights. Calling this `missing` is a false alarm, and 4b creates exactly those rows |
| `mismatch` | an artifact is there and does not hash to the row — reported, never overwritten |

When the models directory cannot be listed at all (`read_dir` fails — §7 test 7's fixture makes it a regular file),
that is **its own named state** for the workspace and the command exits non-zero; it must never degrade into a per-row
`missing` verdict, which would tell the human that every model lost its weights.

`list` also reports **unclaimed entries** — anything under `<db-hash>.models/` no row names, including leftover
`<id>.<nonce>.tmp` files from step 6 — under their own heading, because step 7's ordering makes that state reachable by
design. A name that is not valid UTF-8 renders lossily, the way `walk::UnknownPath` does (`walk.rs:61-64`); a
subdirectory is reported as one rather than recursed; unclaimed entries never change the exit code.

**Machine-readable output is pinned here, not left to the implementer.** `--json` emits one object whose key set is
identical for every state — per row `{ "id", "dim", "sha256", "prefix_doc", "prefix_query", "installed_at", "state" }`,
plus `"unclaimed": [...]`, plus `"models_dir_error"` when listing fails, plus — since R7a, on the authority of
`design-m2-local-embeddings.md` §12 rule 3 — `"fetching": [...]`, one entry per recognised marker, plus — since the
issue-#3 round — `"pinned": [...]`, one entry `{ "id", "bytes_total", "members" }` for the configured id **this build**
pins, empty when none is named or the named id is unpinned. It answers "what would a fetch cost?" from the pin table and
nothing else: no stat, no row, no transport announcement (§12 rule 12's reason for refusing `Content-Length` applies
just as hard to a number quoted *before* the transfer). It is the build's plan (`pins::plan_size`, the sum the terminal
consent prompt prints), **not** §12 rule 6's preflight total (`pins::total_size`, every member of the id): on a text-only
build those are 309,855,456 and 864,676,512 bytes, and quoting the second in a consent sentence over-promises a transfer
that build will never make. **The key set is a
sorted-set pin, not a byte-order pin**: `serde_json` is built without `preserve_order`, so a document prints its keys
alphabetically and no consumer may read meaning in their order; the order that is real and pinned is the text channel's. Install's three outcomes are the strings
`installed`, `already installed`, `restored`, in that same `state` field. `list --json` wraps those rows in a top-level `"models"` array;
`install --json` is the object `{ "id", "state" }` and nothing else. Both names are frozen by tests, so they are
pinned here too — otherwise a later rename breaks a consumer and no document says who decided it. **Exit codes
split on purpose**: argv refusals (bad flag, missing operand) exit **2** from clap; every runtime refusal exits
**1**. A script that wants to tell "you typed it wrong" from "the workspace refused" branches on that. Precedent: `cli.rs:777`, *one JSON shape for
every outcome, not one per outcome* — and if it is not named now, a later round renames `state` or `unclaimed` and no
test dies.

## 4. Why the artifact is scoped per workspace

`<data_dir>/workspaces/<db-hash>.models/<id>`. Not content-addressed: the id **is** the identity vectors reference, so
two ids for two builds of the same weights is the correct way to say "these vectors came from different bytes". The
digest lives in the row and is verified at install; re-verifying on every embed is 4b's cost decision (it reads the
artifact anyway).

**The cost of that choice, written down as a deviation.** `spec:84` and `spec:236` say an artifact is "downloaded
once". Per-workspace placement makes that **N copies for N workspaces**, plus a transient second copy during step 6's
copy-then-rename: two models at ~540 MB each is ~1.1 GB on disk and ~1.6 GB at the moment of install, against the
~9.1 GB free measured in §1. What it buys is the property that no install in one workspace can ever move a file
another workspace's row points at — the P0. If N copies becomes unacceptable the fix is a shared content-addressed
store with per-workspace references, which is a design change and not this round.

**One window install does not close.** With no row at step 5 it publishes the artifact first and learns of a conflict
only when `insert_model` reports that it inserted nothing — so two processes installing one id with *different* bytes
can end with the loser's rename having replaced the winner's artifact before the loser is refused. Closing it properly
needs a lock; what makes it survivable rather than silent is that `list` hashes artifacts and will call the result
`mismatch`, and nothing embeds against a mismatching row. Recorded rather than papered over, since §7 declines to
simulate the cross-process race.

Orphans get heavier too: renaming or deleting a workspace already orphans `<hash>.sqlite` forever (`config.rs:416`,
"Rows are never deleted", and `np repair` was ruled out), and 4a adds an orphan class measured in hundreds of
megabytes. `list` sees only its own workspace's directory, so it cannot even name another workspace's leftovers.
Recorded in §8 rather than pretended away.

Destination writes go through `walk::to_open_path` for the same long-path spelling the rest of the codebase uses;
mixing verbatim and `\\?\` spellings within one operation is worse than either.

## 5. Non-goals

**Where the origin rules live (added 2026-11-04).** This document stops at an offline `install`, and it has no §12 — the fetch
rules, including the one compiled-in default origin (`pins::DEFAULT_ORIGIN`) and the `[models] base_url` →
`NP_MODELS_BASE_URL`-fills-an-unset-key → default precedence, are `docs/design-m2-local-embeddings.md` §12 rules 7 and 8 as
amended that day. `install --path --sha256` remains the offline door that amendment keeps.

**Corrected 2026-11-04:** this round's non-goal list said "no HTTP client, no TLS dependency, no URL scheme". R7b shipped
all three behind the default-on `fetch` feature, so they are a *scope* boundary now rather than an absence: `np models install`
still performs no network I/O of its own and remains the offline door. What has not arrived is model execution — no candle, no
`local-model` inference inside `install`
(per-OS `cargo check --locked -p np-core --features local-model` stays a recorded debt). No embedding calls; nothing
writes to `embeddings` except tests. No config change: `[embeddings].provider` keeps its `none` default — installing a
model makes choosing one *possible*, and choosing is the human's call (parent §2).

## 6. What must not move, and what this round makes stale

- Catalog bytes and every golden: this adds subcommands and touches no scan path. Any diff under `tests/golden*` is a
  stop-and-report.
- Existing `scan` output including stderr prose. Verified against the current CLI suite: no test asserts usage text or
  an unknown-subcommand message, none asserts argv shape, and stdout/stderr assertions are substring/count assertions
  on scan output — so a new subcommand cannot satisfy or break them. **Two cautions for the implementer:** the test
  helper `Workspace::command` appends the workspace root last (`cli.rs:56`) and ~20 cases depend on that shape, so the
  new commands get their own builder rather than a generalisation of it; and np-cli has no `rusqlite` dependency,
  which §3 step 7 respects by naming an np-core helper. **Reads are named too**, for the same reason:
  - `db::model_row(db, id) -> Result<Option<ModelRow>, DbError>` with
    `ModelRow { id: String, dim: i64, sha256: Option<String>, prefix_doc: String, prefix_query: String, installed_at: i64 }`.
    `dim` and `installed_at` stay `i64` because SQLite's `INTEGER` is untyped and a hand-edited row must be *reported*,
    not narrowed into a `u16` that would panic or wrap. `sha256: None` is the schema's ordinary case, not an error:
    migration 7 allows it for a provider with no local weights.
  - `db::model_rows(db) -> Result<Vec<ModelRow>, DbError>` for `list`, ordered by id so output is deterministic.
  - Concurrent installs of one id: `insert_model` uses `ON CONFLICT(id) DO NOTHING` and reports whether it inserted, so
    two processes that both pass step 5 do not end with a bare `PRIMARY KEY` violation as the user's error.
- **No new migration.** `models` exists since migration 7 with exactly the six columns needed, and
  `crates/np-core/tests/db.rs:188-254` pins the table and column list by name — a new column fails that test, which is
  the fence wanted here. If this round seems to want a migration, that is a design gap to report.
- **Premises this round retires.** Three sites in two documents state that `np scan` is the only command
  (`docs/design-f21-durable-upgrade-announcement.md:48`, `docs/backlog-m2-followups.md:315`, `:433-434`). Nothing in
  code depends on it — receipts ride `ScanReport`. Recorded here, and those lines get a note in this round's commit so
  the next reader is not told something false. `install` is deliberately **not** a second path through migration or
  merge logic, which is what F19's ruling actually forbade.

## 7. Tests (offline, tempdirs, Linux and windows-latest)

1. install happy path: artifact at the expected per-workspace path, row with declared dim/sha/prefixes, ok exit.
2. digest mismatch: no file at the target **and** no row; message names both digests.
3. bad id (uppercase, trailing `.`, `nul`, over 64 chars): refused before any read — assert a `--path` that does not
   exist still yields the charset error first.
4. zero-length file → the empty-artifact refusal, not a digest mismatch (asserts step 2 precedes step 3).
5. same id + same full tuple twice → second reports `already installed`, `installed_at` unchanged, one file on disk.
6. same id + different digest → refused; original artifact and row untouched.
   6b. same id, same digest, **different dim** → refused naming stored and declared dim (the sha-only blind spot).
   6c. install with an uppercase `--sha256`, then re-install lowercase → `already installed`, and the stored column is
   lowercase both times.
7. artifact directory cannot be created: make `<db-hash>.models` an existing **regular file**, so directory creation
   fails on both OSes while the database stays writable. The chmod-0500 variant stays as an additional
   `#[cfg(unix)]` case, matching `tests/scan.rs:5540-5565`; it is not the portable test, and an unwritable *data dir*
   would fail earlier at `Db::open` (`db.rs:384-396`) and pin nothing. Assert: command fails, no row.
8. live row, artifact deleted → `list` reports missing-on-disk and repairs nothing; `install` with the matching digest
   re-copies and reports `restored` with `installed_at` and `embeddings` unchanged.
9. `models hash` on a known byte string equals `np_core::hash::sha256_file` on the same bytes (one SHA-256, two
   callers).
10. `list --json` names an unclaimed **leftover temp** — plant a literal `<id>.<nonce>.tmp` file in the models
    directory, the exact spelling step 6 promises — under `"unclaimed"`, with the exit code unchanged.
11. a row installed with `sha256 = NULL` (seeded directly, the way `tests/cli.rs` seeds other states) lists as
    `no artifact expected`, never `missing`.
12. models directory unreadable (§7 test 7's regular-file fixture): `list` reports the directory-level state and exits
    non-zero, and **does not** print `missing` against any row.
13. two installs of one id in sequence with the same tuple: second is `already installed`; a `model_row` read after
    each shows one row and an unchanged `installed_at` (the `ON CONFLICT DO NOTHING` arm — the cross-process race
    itself is not simulated, and saying so beats implying it is).

Mutations each assertion dies to are named in code; at least these get run: skipping the digest comparison, writing the
row before the rename, dropping temp-name+rename for a direct write, turning the different-digest refusal into an
overwrite, comparing only sha256 in step 5, normalising the digest nowhere (so uppercase installs become
un-installable), accepting a `<workspace>` that does not exist (must fail before any write — otherwise it mints an
empty database and a models directory no scan will ever read), and re-deriving the artifact path in np-cli instead of
through `Config::workspace_models_dir` (the mutation is to compute the digest over `path.display()`; with a non-UTF-8
workspace name the row and the artifact land under different hashes, which is the P0 arriving by another door).

## 8. Standing debts this round does not pretend away

The wrong-dim provider answer is still raw `DbError` at the write; ordinal drift still makes "re-embed only what moved"
a row-identity promise. (The claim here that the chunk-gap stderr line had no capture seam was false — `np-cli/tests/cli.rs`
captures a child's stderr for its sibling line, and `an_incomplete_chunk_pass_reports_its_own_gap_on_stderr` now does the
same for this one.) Added by this round:
per-workspace artifacts mean N copies (§4) and a new orphan class that nothing reclaims; and `models list` verifies by
hashing, which costs one whole read per installed model per invocation (§3).

## 9. Open questions for the human (blocking 4b, not 4a)

1. Does the self-hosted server expose `/v1/embeddings`, or only chat/completions?
2. Which artifact first — EmbeddingGemma-2 text tower (~540 MB) or MiniLM (~7 MB)? ~9 GB free handles either; the
   download and benchmark both wait on this answer.
3. Should `install` ever accept a URL once transport exists, or stay local-file-only with the human downloading?
