# R7c — image embeddings and minimal `np search` Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** Embed workspace-local `.png`/`.jpg` diagrams into the same 768-d space as note text, store one vector per distinct content hash, and answer a read-only `np search <query>` over both kinds.

**Architecture:** A `vision` cargo feature (implies `local-model`) compiles llama.cpp's `mtmd` beside the existing engine and exposes a narrow `VisionModel::embed_image(&[u8]) -> Vector`. Media discovery is its own pass inside `np embed`, reading note bytes and resolving image targets relative to the note; results land in migration 10's `media` / `media_path` / `media_embeddings`. `np search` reads both vector tables and writes nothing.

**Tech Stack:** Rust (edition 2021), `rusrus`/`rusqlite` bundled SQLite, hand-written FFI to llama.cpp v0.8.0 + `libmtmd`, clap, serde/serde_json, toml.

**Spec:** `docs/design-m2-image-embeddings.md` — read it with this plan; every value below comes from it. §1.2 carries EmbeddingGemma 2's published contract (unified 768-d space, mean pooling + 512→768 projection, **prefixes are text-only**, shared 8,192-token context, ~280 tokens per image).

## Global Constraints

- Gate list, run by exit code, log read before the code is believed: `cargo fmt --all -- --check` · `cargo clippy --workspace --all-targets -- -D warnings` · `cargo test --workspace` · `env -u HOME -u XDG_CONFIG_HOME cargo test -p np-core --test config` · `RUSTDOCFLAGS="--deny warnings" cargo doc --locked --workspace --no-deps` · the three `.superpowers/sdd/2026-10-23-note-processor-m1-core-catalog/check_*.py` fences · `verify_catalog_goldens.py` → `GOLDEN CHECK: pass`.
- Feature rows add, with `CMAKE=$HOME/tools/cmake/bin/cmake NP_LLAMA_CPP_JOBS=4`: `cargo clippy -p np-core --all-targets --features local-model,vision -- -D warnings`, the same for `--features local-model,fetch`, `RUSTDOCFLAGS="--deny warnings" cargo doc --locked -p np-core --features local-model --no-deps`, and `cargo clippy --locked -p np-cli --features local-model --all-targets -- -D warnings`. Never pass `--locked` after `--` to clippy.
- **Never regenerate goldens.** Any diff under `tests/golden*` stops the task and gets reported.
- No new exit codes: argv problems stay clap's `2`, runtime refusals stay `1`.
- No environment variable for a thread count or a vision toggle; `[embeddings]` config only, plus existing `NP_MODELS_BASE_URL` / `NP_MODELS_SHA256`.
- Published migrations are never renumbered: this plan adds **migration 10** and nothing else.
- Feature discipline: `vision` implies `local-model`; a default build compiles no C++ and its argv must not offer image flags. Release builds keep `local-model` off, so a shipped binary refuses image work (§13's consequence).
- F18: no untested platform or runtime claim in code docs; artifact-gated cases print `NOT COVERAGE` and pass when `NP_TEST_MODEL_PATH` is unset, and the CI floor gets re-measured with the exact command whenever a gated case is added.
- The image path takes **no instruction prefix** (spec §1.2). Text keeps `task: search result | query: ` for queries and `title: … | text: ` for documents.
- Refusals name a class, never a URL or an origin (models-install §12 rule 8).

## Review Focus

Each line is an input the spec implies that no single task's happy path exercises; each gets its test in the named task.

- A **non-UTF-8 image filename** (`caf\xe9.png`) must resolve and store byte-exactly, not round-trip through a lossy decode — Task 3.
- A percent-encoded target (`./diagrams/kafka%20ingest.png`) must resolve to the file with a real space; a **malformed** escape is refused with a class rather than passed through — Task 3.
- An image link that escapes the workspace (`../../etc/passwd.png`, or an absolute path) is refused `outside-workspace` and never opened — Task 3.
- The **same bytes at two paths** produce one vector, one search hit, and `linked_from: 2`; deleting the last linking note deletes the row and its vectors — Task 5 and Task 6.
- A build **without `vision`**, or a model row with no `mmproj` member, refuses image work at construction (`vision-unavailable`) rather than failing mid-run — Task 4 and Task 7.

---

### Task 1: The probe — what this pin actually does with a PNG

Throwaway measurement, no product code (spec §9). Its output becomes constants in later tasks; if it says the builder cannot run, stop the plan and report.

**Files:**
- Create: `crates/np-core/tests/probes/vision_probe.c`
- Create: `crates/np-core/tests/probes/vision-spike-2026-11-02.md` (the note later tasks read)
- Modify: `docs/design-m2-image-embeddings.md` (§9 pointer to the note; status line → "spike measured")

**Interfaces:**
- Consumes: nothing.
- Produces: `vision-spike-2026-11-02.md` recording — the exact `mtmd_*`/`llama_*` call sequence that produced a pooled vector; patch/token cost for one 1024×768 PNG and one 512×384 PNG; whether `mtmd_support_vision` is true for the pinned `mmproj`; the pooled width read back; and the caption-vs-image cosine table against captions and unrelated text.

- [x] **Step 1: Build `libmtmd` from the vendored tree into a scratch directory** — configure the existing pinned source with `-DLLAMA_BUILD_MTMD=ON -DBUILD_SHARED_LIBS=OFF -DGGML_OPENMP=OFF`, build target `llama` plus `mtmd`, and record the cmake command line verbatim in the note.
- [x] **Step 2: Write `vision_probe.c`** that opens the primary GGUF and the `mmproj` (`mtmd_init_from_file`), prints `mtmd_support_vision`, `mtmd_get_marker`, then for a generated PNG runs the tokenize → encode → decode path and prints chunk count, image token count, pooled width, and the first three floats of the pooled row.
- [x] **Step 3: Generate the inputs** — two solid-shape PNGs plus one PNG containing rendered text ("KAFKA → INGEST"), and a captions file. Keep the generator inline in the note so the run is reproducible; Python + Pillow is fine here (throwaway).
- [x] **Step 4: Run it with the pinned artifacts** and copy the raw output into the note verbatim, including any llama.cpp warning lines.
- [x] **Step 5: Score comparability** — print cosine for (caption, its image), (caption, other image), (caption, unrelated sentence). Record the table; state in one line whether captions beat unrelated text on their own images.
- [x] **Step 6: Decide and write it down** — either "recognised, use this call sequence" with the numbers, or "not recognised" with the exact refusal from C. Commit: `git commit -m "spike(r7c): what llama.cpp v0.8.0 does with embeddinggemma-2's gemma4v projector"`.

**Gate:** none beyond compiling — this task ships a note and a probe, not product code. If Step 6 lands on "not recognised", stop here and bring the two exits (re-pin vs OCR) back to the human; do not start Task 2.

---

### Task 2: Migration 10 — `media`, `media_path`, `media_embeddings`, projector stamps

**Files:**
- Modify: `crates/np-core/src/db.rs` (`MIGRATIONS` tail, accessors near the embeddings helpers)
- Test: `crates/np-core/tests/media_db.rs`

**Interfaces:**
- Consumes: `MIGRATIONS`, `db::open`.
- Produces: tables exactly as spec §6; `pub struct MediaRow { pub id: i64, pub hash: String, pub size: i64, pub mime: String }`; `pub fn record_media(conn, hash: &str, size: i64, mime: &str) -> Result<MediaRow, DbError>` (insert-or-get by hash); `pub fn link_media_path(conn, rel: &[u8], abs: &Path, media_id: i64) -> Result<(), DbError>` (no case-folded twin of the key: identity is a content hash, so two spellings already resolve to one media row); `pub fn unlink_media_path(conn, rel: &[u8]) -> Result<usize, DbError>` (returns media rows removed with it); `pub fn record_media_embedding(conn, media_id: i64, model: &str, dim: i64, vector: &[u8], source_hash: &str, now: i64) -> Result<(), DbError>` — **seven args**: `embedded_at` is caller-supplied like every other timestamp in `db.rs` (no SQL-side clock exists in this crate), settled during Task 2 when the plan's six-arg list met a `NOT NULL` column; `pub fn media_embeddings(conn, model: &str) -> Result<Vec<(i64, Vec<u8>)>, DbError>`; `pub fn set_media_stamp(conn, media_id: i64, size: i64, mtime: i64)` and `media_stamp(conn, media_id) -> Option<(i64, i64)>`; `model_files` gains `verified_size` / `verified_mtime` with `member_stamp` / `record_member_stamp` mirroring migration 8's pair for `models`.

- [ ] **Step 1: Write the failing migration tests** in `media_db.rs`: an existing database at version 9 upgrades to 10 and keeps its rows; `media.hash` rejects a 63-character value (`CHECK length(hash) = 64`); `media_path.rel_bytes` refuses TEXT (`typeof = 'blob'`) and accepts non-UTF-8 bytes; `media_embeddings.vector` refuses a length that is not `4 * dim`; two identical hashes produce **one** `media` row via `record_media`; `unlink_media_path` on the last path deletes the media row and its embeddings.
- [ ] **Step 2: Run it, confirm failure** — `cargo test -p np-core --test media_db`, expect compile failure on the missing functions.
- [ ] **Step 3: Add migration 10** with the three tables from spec §6 verbatim (including the F13 `typeof` arms) plus `ALTER TABLE model_files ADD COLUMN verified_size INTEGER` / `verified_mtime INTEGER`, then the accessors above.
- [ ] **Step 4: Run tests, confirm pass**, then the full default gate list and goldens unchanged.
- [ ] **Step 5: Commit** — `feat(migration 10): media content identity, paths, and projector stamps`.

---

### Task 3: Media discovery — markdown image links to byte-exact files

Pure Rust, no runtime, fully testable without an artifact.

**Files:**
- Create: `crates/np-core/src/media.rs`
- Modify: `crates/np-core/src/lib.rs` (`pub mod media;`)
- Test: `crates/np-core/tests/media_resolve.rs`

**Interfaces:**
- Consumes: note bytes, workspace root, note's workspace-relative path.
- Produces: `pub struct ImageLink { pub target: Vec<u8>, pub line: usize }`; `pub fn image_links(body: &[u8]) -> Vec<ImageLink>` (markdown `![alt](target)` only, angle-bracket form `<…>` included, code fences skipped); `pub enum MediaRefusal { MissingTarget, OutsideWorkspace, UnsupportedFormat, Undecodable, OverBudget, VisionUnavailable }` with `pub fn label(&self) -> &'static str` giving the spec §8 kebab-case names; `pub fn resolve_image(root: &Path, note_rel: &[u8], target: &[u8]) -> Result<ResolvedImage, MediaRefusal>` where `ResolvedImage { rel_bytes: Vec<u8>, abs: PathBuf, mime: &'static str }` — **three fields**: the pinned four-field shape carried a `case_key`, which is the same folded twin migration 10 just dropped from `media_path`. It cannot be filled byte-exactly for a non-UTF-8 name at all (the only spelling available is a lossy decode, which this module must not contain), and no task reads it; a report that wants display text lossy-decodes at print time, which is F20's rule and lives in np-cli. `hash::sha256_file_with_size(path) -> Result<(String, u64), HashError>` — the digest-and-count lives in `hash.rs`, next to the one digest definition §12 rule 9 asks for; a second public `sha256_file` in `media.rs` would be two read loops to keep in step.

- [ ] **Step 1: Write the failing tests** — extraction finds `![d](./a.png)` and `![d](<with space.png>)`, ignores `[text](x.png)` (a link, not an image) and anything inside a fenced code block; percent-decoding turns `kafka%20ingest.png` into the file whose name holds a space; a lone `%zz` is **not** decoded, so no such file exists and it refuses as `MissingTarget` (spec §7: `undecodable` means an accepted raster that fails to decode, not a malformed escape); `../../x.png` and `/etc/x.png` give `OutsideWorkspace`; `.gif`, `.webp`, `.svg`, `.bmp` give `UnsupportedFormat` (spec §8: only `.png`/`.jpg`/`.jpeg` accepted); a non-UTF-8 filename resolves and its `rel_bytes` keep the raw bytes; two spellings of one path (`./a/../b.png`) normalise to the same `rel_bytes`.
- [ ] **Step 2: Run, confirm failure** — `cargo test -p np-core --test media_resolve`.
- [ ] **Step 3: Implement `media.rs`** — byte-level scanning over `&[u8]` (never a lossy decode), one percent-decode pass that fails closed, normalisation by component walking (`.` dropped, `..` popped, refusing to pop past the root), and MIME from extension only after the path is proven inside the workspace.
- [ ] **Step 4: Run tests, confirm pass**, plus clippy (no `String::from_utf8_lossy` anywhere in the new module — grep it).
- [ ] **Step 5: Commit** — `feat(media): byte-exact markdown image resolution with refusal classes`.

---

### Task 4: The vision seam — `VisionModel::embed_image`

Behind `vision`; the call sequence comes from Task 1's note.

**Files:**
- Modify: `crates/np-core/Cargo.toml` (`vision = ["local-model"]` feature), `crates/np-core/build.rs` (cmake option `LLAMA_BUILD_MTMD=ON` when the feature is on; a fifth archive in link order), `crates/np-core/src/llama_ffi.rs` (the `mtmd_*` mirrors Task 1 named), `crates/np-core/src/vision.rs`, `crates/np-core/src/lib.rs`
- Test: `crates/np-core/tests/local_vision.rs`

**Interfaces:**
- Consumes: an already-loaded `&LoadedModel`, the `mmproj` path, image bytes.
- Produces: `pub struct VisionModel` (private fields, `Drop` frees both handles); `pub fn open(model: &LoadedModel, mmproj: &Path) -> Result<VisionModel, EmbedError>` refusing `EmbedError::Unavailable("this build cannot read images …")` when the runtime reports no vision support; `pub fn embed_image(&mut self, bytes: &[u8]) -> Result<Vector, EmbedError>` returning a **768-d L2-normalised unit vector** with no prefix applied; `pub fn last_token_cost(&self) -> usize` (patch/token count of the last encode, for §8's budget check).

- [ ] **Step 1: Write the failing tests** in `local_vision.rs` (artifact-gated, printing `NOT COVERAGE` when `NP_TEST_MODEL_PATH` or `NP_TEST_MMPROJ_PATH` is unset): a generated PNG yields a vector whose `dim()` is 768 and whose norm is 1.0 within 1e-3; the same bytes twice give bit-identical components; a caption's cosine with its own image beats its cosine with an unrelated image (Task 1's table, now a fence); a truncated PNG refuses rather than returning a vector; `last_token_cost()` is ≥ 1 and printed on every artifact run.
- [ ] **Step 2: Run, confirm failure** — `cargo test -p np-core --features local-model,vision --test local_vision`.
- [ ] **Step 3: Implement the seam** — FFI mirrors with doc comments saying which header line each mirrors; `open` initialises `mtmd` against the existing model handle and refuses when support is absent; `embed_image` runs Task 1's sequence, pools at `n_embd_out`, L2-normalises, and maps C failures to `EmbedError::Unavailable` naming the class (`undecodable`, `over-budget`) never a path.
- [ ] **Step 4: Run tests with both artifacts set**, confirm pass; run the same suite **without** the artifact and confirm each case prints `NOT COVERAGE`.
- [ ] **Step 5: Feature gates** — `cargo clippy -p np-core --all-targets --features local-model,vision -- -D warnings` and a `--no-default-features` check proving no vision symbol is reachable.
- [ ] **Step 6: Commit** — `feat(vision): mtmd-backed image embedding behind the vision feature`.

---

### Task 5: `np embed` learns to embed images

**Files:**
- Modify: `crates/np-core/src/embed.rs` (`Embedder` gains image capability), `crates/np-core/src/local_model.rs` (lazy provider exposes images when the model declares an `mmproj` member), `crates/np-core/src/embed_run.rs` (report fields + media pass), `crates/np-core/src/db.rs` if a helper is missing
- Test: `crates/np-core/tests/embed_media.rs`, plus edits to `crates/np-core/tests/embed_run.rs`

**Interfaces:**
- Consumes: Task 2's accessors, Task 3's resolver, Task 4's seam.
- Produces: `Embedder::supports_images(&self) -> bool` (default `false`); `Embedder::embed_image(&mut self, bytes: &[u8]) -> Result<Vector, EmbedError>` (default returns `EmbedError::Unavailable("vision-unavailable")`); on `EmbedReport`: `images_seen: usize`, `images_embedded: usize`, `images_refused: usize`, `image_refusals: Vec<(String, MediaRefusal)>` (path + class, JSON only), and text output `images     seen 7 · embedded 4 · refused 3 (missing-target 1, unsupported-format 2)` suppressed entirely when `images_seen == 0`.

- [ ] **Step 1: Write the failing tests** with a fake provider that records image calls: two notes linking one PNG produce **one** media row, one embedding, and `linked_from == 2`; a note whose text changed but whose image did not re-embeds chunks only (assert embedding row's `embedded_at` unchanged); a `.gif` link counts `unsupported-format` and appears in `image_refusals`; a missing file counts `missing-target`; unlinking the last note deletes the media row and its vector; a provider with `supports_images() == false` reports every image as `vision-unavailable` and still embeds all text.
- [ ] **Step 2: Run, confirm failure.**
- [ ] **Step 2b: Pin the timestamp plumbing** — a test writing two embeddings with different `now` values asserts those exact values come back, because an implementation that ignores the parameter and binds 0 still compiles.
- [ ] **Step 3: Implement the pass in `embed_run`** — after chunk work, gather `image_links` per note, resolve, hash, compare against `media.hash`/stamps to decide skip-vs-encode, encode through the provider, and write with `record_media_embedding`. Refusals are counted, never fatal; the run's exit status is unchanged by them.
- [ ] **Step 4: Run tests, confirm pass**, then the workspace suite (existing embed tests must stay green with `images_seen == 0`).
- [ ] **Step 5: Commit** — `feat(embed): image vectors for linked media, content-hash deduplicated`.

---

### Task 6: `np search <query>` — read-only, both vector kinds

**Files:**
- Create: `crates/np-core/src/search.rs`
- Modify: `crates/np-core/src/lib.rs`, `crates/np-cli/src/main.rs` (subcommand + summary), `crates/np-cli/Cargo.toml` if needed
- Test: `crates/np-core/tests/search.rs`, `crates/np-cli/tests/cli.rs`

**Interfaces:**
- Consumes: Task 2's tables, an `Embedder` for the query vector.
- Produces: `pub struct Hit { pub score: f64, pub kind: HitKind, pub path: String, pub source_note: Option<String>, pub linked_from: usize }`, `pub enum HitKind { Note, Image }`, `pub fn search(conn: &Db, model: &str, query_vector: &[u8], limit: usize) -> Result<Vec<Hit>, SearchError>`; CLI `np search <query> [--limit N] [--json]` printing `0.6231  note   notes/ingest.md` / `0.5904  image  notes/diagrams/kafka.png   (from notes/ingest.md)` and JSON `{score, kind, path, source_note, linked_from}`.

- [ ] **Step 1: Write the failing tests** — a fixture index with two note vectors and one media vector ranks as constructed; `--limit` truncates and never reorders equal scores (ties broken by path, stated in the doc comment); an image hit's `source_note` is the lexicographically smallest live linking note and `linked_from` counts all of them; a model with no embeddings prints "nothing indexed" on stdout and exits 0; a query against a model row that has vectors from another model only is refused at exit 1 naming the model id.
- [ ] **Step 2: Run, confirm failure.**
- [ ] **Step 3: Implement** cosine over `embeddings` joined to `chunks`/`notes` and `media_embeddings` joined to `media_path`, one pass, no writes; assert unit-norm on the query vector and refuse otherwise rather than scaling silently.
- [ ] **Step 4: CLI tests** — `np search` with no argument exits `2` (clap); with an unbuilt runtime exits `1` naming the refusal; `--json` prints one object per hit under a `hits` array and nothing else.
- [ ] **Step 5: Run everything, commit** — `feat(search): read-only cosine top-k over notes and images`.

---

### Task 7: Install-time fetch — default origin, capability-shaped member set

**Files:**
- Modify: `crates/np-core/src/pins.rs` (default origin constant + doc), `crates/np-core/src/fetch.rs` (use it when config names none), `crates/np-cli/src/main.rs` (`fetch` default members = what this binary can use), `README.md` (install step), `docs/design-m2-models-install.md` (§12 rule 7 amendment note)
- Test: `crates/np-core/tests/fetch.rs`, `crates/np-cli/tests/cli.rs`

**Interfaces:**
- Consumes: R7b's engine unchanged.
- Produces: `pins::DEFAULT_ORIGIN: &str = "https://huggingface.co/ggml-org/embeddinggemma-2-GGUF/resolve/main"`; `pins::members_for_build() -> &'static [Member]` returning the primary alone for a text-only build, both members when `vision` is on; `models fetch` with no `--member` uses it.

- [ ] **Step 1: Write the failing tests** — with `[models] base_url` absent and no env var, a default-feature build resolves to `DEFAULT_ORIGIN` and plans **one** member; the same with `vision` plans both; an explicit `base_url` still wins (assert the loopback origin in the existing http tests keeps working); a `--member mmproj` request on a text-only build refuses naming the capability, not the URL.
- [ ] **Step 2: Run, confirm failure.**
- [ ] **Step 3: Implement**, keeping every existing refusal (digest mismatch, overwrite, absent target) intact and adding no new refusal class.
- [ ] **Step 4: Docs** — README's install section becomes "place the binary → `np models fetch` → `np scan` → `np embed`", stating the one outbound request and that `[models] base_url` overrides it for a mirror or an air-gapped host; models-install §12 gains the amendment with today's date and who decided it.
- [ ] **Step 5: Re-measure package sizes** for the three shipped shapes plus `local-model,vision`, and write them into README beside R7b's numbers (this is where the fifth archive shows up).
- [ ] **Step 6: Commit** — `feat(fetch): install-time default origin and capability-shaped member set`.

---

### Task 8: Documentation, CI floor, and the honest claims

**Files:**
- Modify: `README.md`, `docs/design-m2-thread-policy.md` (untouched unless a number moved), `docs/backlog-m2-followups.md`, `todo.md`, `.github/workflows/ci.yml`, `crates/np-core/tests/probes/vision-spike-2026-11-02.md`
- Test: none (documentation), plus the CI count

- [ ] **Step 1: Re-measure the `NOT COVERAGE` floor** with the exact CI command extended by `--test local_vision`, and set the threshold to the measured number with a comment naming which cases print and why.
- [ ] **Step 2: Write §14's numbers into the design doc** — token cost per image from Task 1/4, whether the soft-token budget is honoured, and the comparability table; anything that did not get measured stays written as "not measured".
- [ ] **Step 3: Update README** with `np search`, the `vision` feature, accepted formats (`.png`/`.jpg` only), and the refusal classes a user will see in an embed report.
- [ ] **Step 4: Update todo/backlog** — what shipped, what is owed (SVG rasterising; MRL truncation as a storage lever; retrieval-quality measurement against a labelled set).
- [ ] **Step 5: Full gate list one last time**, goldens verified unchanged, then commit — `docs(r7c): measured vision facts, search surface, and what is still owed`.

## Self-Review Checklist (run after writing, before handing over)

- Spec coverage: §6 schema → Task 2; §7 resolution/dedup/lifecycle → Tasks 3+5; §8 formats and classes → Tasks 3+4+5; §9 probe → Task 1; §10 seam and no-prefix rule → Task 4; §11 report → Task 5; §12 search → Task 6; §13 install fetch → Task 7; §14 claims → Task 8.
- Type consistency: `MediaRefusal` labels used identically in Tasks 3, 5 and 8; `Hit.linked_from` defined in Task 6 and referenced in its CLI test; `record_media_embedding` takes seven arguments (…, `now: i64`) wherever Task 5 calls it.
- Proportion: no step contains a body the signature and tests already determine.
