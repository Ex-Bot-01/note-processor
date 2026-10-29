# M2a Round 4a — `np models install`: pin a model artifact without a network path

Date 2026-10-29. Branch `m12/embeddings` (`be78415`). Parent design:
`docs/design-m2-embeddings.md` §2 (provider stays `none` by default), §3 (`models` table, migration 7), §6
(benchmark order and the secret rule).

## 1. Why this round exists, and why it is cut here

Migration 7 created `models(id, dim, sha256, prefix_doc, prefix_query, installed_at)` and `embeddings.model` has a
foreign key to it. So today **nothing can be embedded even if a provider existed**: there is no way to put a model row
on the table, and no way to put an artifact on disk that the row could name. This round supplies exactly that, from
the side that needs no network and no model decision.

The cut is deliberate. The remaining M2a work splits cleanly:

- **4a (this design)** — install a file the human already has, verify it, record it. Testable offline, on CI, with no
  credentials, and it changes nothing about a workspace that never runs the command.
- **4b** — transport (HTTP) and the benchmark. Both need facts I do not have: whether the self-hosted server serves
  `/v1/embeddings` at all, which model, and what its vectors are worth. Design §6 makes the benchmark gate the model
  choice, so writing an HTTP client first would be choosing the answer by convenience.

Environment facts this cut rests on (checked 2026-10-29 on `pi`, euid 1000): `NP_EMBED_BASE_URL` and `NP_EMBED_API_KEY`
are **unset**; ~9.1 GB free on `/home`; egress to github.com answers; no HTTP client is currently a dependency of any
crate in this workspace.

## 2. Command surface

```
np models install --id <id> --path <file> --dim <n> --sha256 <hex> [--prefix-doc <s>] [--prefix-query <s>]
np models hash <file>
np models list
```

- **`--sha256` is mandatory for `install`.** The point of the row is that a vector written against model `X` can be
  trusted to come from bytes `X`. A human who downloaded an artifact and does not know its digest gets it from
  `np models hash <file>` and pastes it into the next command — two steps, both honest, instead of one command that
  records a checksum it just computed from the same file it is installing (which pins nothing).
- **`--dim` is declared by the human, not probed.** Probing needs to run the model, which is Round 4b's machinery and,
  for an endpoint, a network call. A wrong `--dim` is caught later by the provider seam's `DimensionMismatch` — and
  that path is currently raw-`DbError` debt (round 3b review), recorded here rather than silently fixed sideways.
- **`--id`** must match `[a-z0-9][a-z0-9._@-]{0,63}` and is the `models.id` primary key. The charset is not cosmetic:
  the id lands in a filename (§4), so a `/` or a leading `.` would be a path-traversal bug wearing a UI hat.
- **No `--force`.** Replacing a model identity is exactly the operation that invalidates every vector stored against
  it, and this project's answer to "silently make past work meaningless" has been refusal every time (F13, F19, F23).

## 3. What install does, in order

1. Validate `--id` against the charset; refuse with a message naming the bad characters.
2. Open `--path`, stream it through SHA-256 (`crate::hash` already digests by streaming; no whole-file read — the
   artifacts under discussion are hundreds of megabytes and the machine has 9 GB free).
3. Compare against `--sha256`, case-insensitively on the hex, and refuse on mismatch with **both** digests in the
   message: the file's digest is public information about a file the human already holds, so printing it leaks
   nothing and its absence is what makes a mismatch diagnosable.
4. Refuse a zero-length file (a truncated download is the failure this command exists to catch).
5. Look up `models.id`: if a row exists with a **different** `sha256`, refuse (§2's no-`--force`). If it exists with
   the same `sha256`, do nothing and report `already installed` — idempotence matters because installs will be retried
   after Ctrl-C.
6. Copy to `<data_dir>/models/<id>` through a temporary name in the same directory and one `rename`, the same
   discipline as the catalog publish: a reader (or a later install) never sees half an artifact, and a failure leaves
   no partial file behind. A workspace root that refuses writes must fail before any row is written.
7. Insert the `models` row **after** the rename succeeds, in one transaction with nothing else. Ordering rationale: a
   row naming a missing file is a lie the embed round would act on; a file with no row is inert and re-runnable.

`np models hash <file>` prints `<sha256>  <path>` and does not touch the database. `np models list` prints id, dim,
sha256 (truncated to 12 hex with the full value available via `--json`), installed_at, and whether the artifact is
present on disk — because a hand-deleted file with a live row is a state the human needs named, not one the tool
should "helpfully" repair by deleting vectors.

## 4. Where the artifact lives

`<data_dir>/models/<id>`. Not content-addressed (`<sha>.bin`) and not per-version: the id **is** the identity the
vectors reference, so two ids for two builds of the same weights is the correct way to express "these vectors came
from different bytes". The digest lives in the row and is verified at install; re-verifying on every embed is Round
4b's cost decision (it reads the artifact anyway, so the marginal cost is a second pass, and §5 of the parent design
already says what happens when stored vectors disagree with reality).

## 5. Non-goals for this round

- **No HTTP client, no TLS dependency, no URL scheme.** Transport arrives with 4b's answer about the endpoint.
- **No model execution** — no candle, no `local-model` feature yet (`cargo check --locked -p np-core --features
  local-model` stays a per-OS debt recorded in §6 of the parent design).
- **No embedding calls.** `chunk()` still has no production caller beyond the scan; nothing writes to `embeddings`
  except tests.
- **No config change.** `[embeddings].provider` keeps its `none` default; installing a model makes it *possible* to
  choose one, and choosing is the human's call (parent design §2).

## 6. What must not move

- Catalog bytes and every golden: this adds a subcommand and touches no scan path. Any diff under `tests/golden*` is a
  stop-and-report, as ever.
- Existing CLI output for `scan`: unchanged, including stderr prose. The new commands print only what they did.
- Migration set: **no new migration.** `models` exists since migration 7 and this round needs no column. If an
  implementer finds themselves wanting one, that is a design gap to report, not a migration to add.

## 7. Tests (all offline, all tempdirs)

1. install happy path: artifact present at the expected path, row present with the declared dim/sha/prefixes, exit ok.
2. digest mismatch: nothing installed — no file at the target **and** no row — and the message names both digests.
3. bad id charset: refused before any read (assert the file was never opened, e.g. a path that does not exist still
   produces the charset error first).
4. zero-length file refused.
5. same id + same digest twice → second reports `already installed`, row unchanged (`installed_at` not rewritten), and
   exactly one file on disk.
6. same id + different digest → refused, original artifact and row untouched.
7. write-refusing data dir → command fails, no row (the ordering of §3 step 7 pinned by the failure, not by prose).
8. `models list` reports a live row whose file was deleted as missing-on-disk rather than repairing it.
9. `models hash` on a known byte string equals the digest `crate::hash` computes for the same bytes (one definition of
   SHA-256, two callers).

Mutations each assertion must die to are named per test in the code, and at least these are run: skipping the digest
comparison, writing the row before the rename, dropping the temp-name+rename for a direct write, and turning the
"different digest" refusal into an overwrite.

## 8. Open questions for the human (blocking 4b, not 4a)

1. Does the self-hosted server expose `/v1/embeddings`, or only chat/completions? (Asked rather than assumed, per the
   parent design's rule.)
2. Which artifact should be pinned first — EmbeddingGemma-2 text tower (~540 MB) or MiniLM (~7 MB)? Disk here has
   ~9 GB free; the download and the benchmark are both gated on that answer.
3. Should `install` ever accept a URL, once transport exists, or stay local-file-only with the human doing downloads?
