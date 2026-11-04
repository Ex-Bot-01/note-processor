# R7c — image embeddings as a second vector source, and a minimal read-only `np search`

Status: **approved; step 0 measured 2026-11-02** (`crates/np-core/tests/probes/vision-spike-2026-11-02.md`). Approach chosen
by the human in conversation: option **A** below, plus a minimal read-only query command "for our proof of concept". The
runtime answered before any schema was written against it: `mtmd_support_vision` is true for this pair, one 1024×768 image
costs **336 tokens** (512×384 costs 88 — the cost follows the patch grid, not the card's flat default), the pooled row is
768 floats arriving un-normalised, and captions beat unrelated sentences against their own images. §9's "not recognised" exit
is therefore closed; §10's seam and §8's budget rule are confirmed rather than assumed.

## 1. Intent, and why

Notes embed today. Diagrams do not: an architecture sketch inside a note is retrievable only through the prose around it,
and `![Kafka ingest layout](./kafka-ingest.png)` is a poor handle on a picture whose whole value is that it says something
the text does not. The human's use case is specific — **software diagrams and architecture drawings**, where the content of
the image is the thing worth finding — so "images are attachments, not parsed" (the approved spec, §files row) stops being
the right promise for this class of file.

What they asked for, in their words: EmbeddingGemma-2 embeds the notes, *each image is embedded separately and linked to the
note*, and a search then returns both — "I can also search for the content of an image". They state, having verified it on
their side, that this model's image vectors are comparable to its text vectors because it ingests multiple formats. That is
accepted as a fact about the model (§1.1). What is not assumed is that **our** runtime produces it — §9's probe.

### 1.1 What the human verified, and what remains open about it

Comparable-in-principle was demonstrated by them outside this codebase. Our path is llama.cpp v0.8.0 plus the pinned
`mmproj` through our own hand-written FFI, which has never carried a pixel. The distinction matters for the plan, not for
the design.

**Measured 2026-11-02, against the real file rather than inferred from its name.** The first builder check argued this pin
probably cannot drive the projector, grepping the vendored tree for the model's name and finding nothing under
`tools/mtmd/`. That grep asked the wrong question: `mtmd` dispatches on the projector's own declared type, not on the model
name. Reading the first 2 MiB of `mmproj-embeddinggemma-2-Q8_0.gguf` over HTTP answers it —

```text
general.architecture        = clip
general.type                = mmproj
general.name                = embeddinggemma-2
clip.vision.projector_type  = gemma4v        # PROJECTOR_TYPE_GEMMA4V exists in v0.8.0, with tools/mtmd/models/gemma4v.cpp
clip.audio.projector_type   = gemma4a        # likewise present; nothing here uses it
```

so the vision graph has a builder in this pin and §9's probe is expected to say *recognised*. What the probe still has to
establish, and what no amount of header reading can: the **patch count** one diagram costs, whether `mtmd_support_vision`
answers true for this file, and whether the pooled vector this build produces behaves like the text vectors the human
measured elsewhere. If it does not, that is a finding to bring back — §12 refuses to blend rather than papering over it.

### 1.2 What the model's own documentation says (retrieved 2026-11-02)

The human asked for this to be built **from EmbeddingGemma 2's documentation**, so its card was read rather than paraphrased
from memory: `https://huggingface.co/google/embeddinggemma-2` (model card, `library_name: transformers`, tags include
`multimodal-embedding`). What it states that this design has to honour:

* **One space, by design.** Text (including code), images, video and audio — *and combinations thereof* — map into a
  **single unified 768-dimensional vector space**; a text-only query such as "waterproof shoes for trail running" is compared
  directly against an embedding made from images. That is the property §12's single ranked list rests on, and it is the
  model's stated behaviour rather than a hope about a projector.
* **Prefixes are text-only.** Task instruction prefixes (`task: search result | query: …`, `title: … | text: …`) "apply to
  text only. **Pass images, video, and audio without any prefix.**" Our text path already uses exactly those two spellings
  (R6c2's §A′ seam), so the image path must pass **nothing** — wrapping a diagram in the document prefix would be a silent
  quality bug of the kind §4's anti-goals are about.
* **Pooling and projection.** Mean pooling, then a learned **512 → 768** projection; native output dimension 768. This is the
  same 512/768 pair `local_model.rs` measured on the artifact (`n_embd_inp = 512`, `n_embd_out = 768`), so §10's contract of
  "pool at out, L2-normalise, compare against 768" matches the published recipe rather than reinterpreting it.
* **One shared 8,192-token context for every modality**, consumed at fixed rates: text 1 token per subword, **image 280
  tokens by default** (~29 images), video 140 per frame, audio 25 per second. A "vision token budget" trades quality for
  tokens between **70 and 1120** soft tokens per image. This is the same 8,192 §5 of the local-embeddings design chose as our
  ceiling — now known to be the model's published limit rather than a llama.cpp accident.
* **Matryoshka truncation** to 768 / 512 / 256 / 128 dimensions is supported, and two rules come with it: a truncated vector
  must be **re-normalised** (slicing a unit vector does not preserve unit length; skipping this "degrades ranking quality
  silently"), and query and corpus **must share a dimension**. Not used this round — we store 768 — but it is the storage
  lever for a large image index later, and the two rules are recorded here so whoever pulls it down does not rediscover them.

Two things the card does *not* settle, which §9's probe exists to answer: what patch count **llama.cpp's `gemma4v` builder**
actually produces for one diagram (280 is the reference implementation's default; our runtime may differ), and whether that
builder honours a soft-token budget at all.

## 2. Goals

* A workspace-local image referenced by a note gets **one vector per file**, stored against the file rather than the note,
  and re-embedded only when its bytes change.
* A query returns notes **and** images from one cosine pass, each with its score and where it came from.
* Every way an image can fail to produce a vector is **reported**, never repaired and never silent: missing file, unsupported
  format, unreadable bytes, over-budget image, runtime built without vision.
* Text-only behaviour is untouched: a build without the vision code, or a workspace with no images, embeds and searches
  exactly as it does today.

## 3. Non-goals

* Clustering, the review queue, ranking cleverness, snippets, pagination (`np search` is exact cosine top-k and nothing more).
* Fetching remote images: `http(s)` and inline `data:` targets are recorded as refused with a class. Fetching *user content*
  is a consent decision this round does not make; §12's fetch rules cover model files we pin, not arbitrary links in notes.
* Audio and video (`mtmd_support_audio` exists; nothing here uses it).
* Thumbnails, previews, image display, anything that renders a byte to a human.
* Backfilling history beyond what the ordinary embed pass does: images are embedded by `np embed` like chunks are, so an
  existing workspace picks them up on its next run.

## 4. Anti-goals

* An index where an image's vector is indistinguishable from a chunk's — a reader (or a later migration) must be able to tell
  which kind of thing a row describes without guessing.
* A search that quietly mixes incomparable vectors and prints one ranked list as though it meant something. If §9 shows the
  spaces differ, `np search` says so per source rather than blending them into noise.
* Re-embedding a diagram because an unrelated note added a link to it.

## 5. Approaches considered

**A — media as a first-class vector source (chosen).** `media` keyed by content hash, `media_embeddings` holding the vectors,
the existing `links` rows supplying the note↔image edge. Trades two new tables and one migration for keeping note-text
assertions out of image rows.

**B — images as chunks.** Reuse `chunks`/`embeddings` with a parent-kind column. Rejected: `chunks` asserts `ordinal`,
`byte_start`/`byte_end`, `chunker` and `source_hash` about *note text*, and `embeddings.source_hash` mirrors it; an image row
would carry fiction in four columns, and the CHECK discipline that migrations 3 and 4 had to learn the hard way (`typeof =
'blob'` alongside declared affinity) would need re-deriving for a shape it was never designed for. Cheaper on paper, expensive
precisely where this project keeps its guarantees.

**C — OCR into text.** Extract drawn labels and embed them as synthetic chunk text with the existing text encoder. Rejected as
the plan: it discards what a diagram communicates by layout, which is the stated reason content matters. Kept as contingency if
§9's probe finds the pin cannot drive this projector at all — it needs no new model and stays inside one space.

## 6. The schema (migration 10)

Two tables, both under the discipline the earlier migrations established: byte-exact paths where paths are stored, `typeof`
arms on blobs, FKs that cascade, and no column that has to lie.

**Content is the identity; a path is where you saw it.** The first draft keyed `media` by resolved path, which embeds one
diagram twice when it exists at two paths and shows two hits for identical bytes. The human asked which was more reasonable;
this is that version, and it is also what F19's "identical content, one row" precedent points at.

```sql
CREATE TABLE media (
    id          INTEGER PRIMARY KEY,
    -- Content identity: the same SHA-256 is one diagram, wherever it sits or how often it is copied.
    hash        TEXT    NOT NULL UNIQUE CHECK (length(hash) = 64),
    size        INTEGER NOT NULL CHECK (size >= 0),
    mime        TEXT    NOT NULL
);

-- Every workspace path those bytes were seen at. Removing the last row removes the media row and, by cascade, its vectors —
-- by `unlink_media_path`'s `NOT EXISTS` delete and the pass's end-of-run sweep, not by trigger: nothing in this schema
-- creates one, and a trigger could not apply §7 rule 4's "did a live note link this path?" anyway.
CREATE TABLE media_path (
    rel_bytes   BLOB    NOT NULL CHECK (typeof(rel_bytes) = 'blob'),
    -- no case-folded twin of `rel_bytes`: identity is a content hash, so two spellings of one figure resolve to one row.
    abs         TEXT    NOT NULL,
    media_id    INTEGER NOT NULL REFERENCES media(id) ON DELETE CASCADE,
    -- D12's stamp pair (§7): one `stat` when the bytes have not moved since the last check, a full hash when they have.
    -- **On the path, not on `media`.** The first draft put them beside the hash, which cannot work: reaching that row needs
    -- the digest, and producing the digest is the reading the stamp exists to skip. A stamp answers a question about a
    -- *path* — "have the bytes here changed?" — so it lives with the path. Re-pointing a path at different content nulls
    -- the pair (`record_model_file`'s rule for `model_files`), or the old check would vouch for bytes that were never there.
    verified_size  INTEGER,
    verified_mtime INTEGER,
    PRIMARY KEY (rel_bytes)
);

CREATE TABLE media_embeddings (
    media_id      INTEGER NOT NULL REFERENCES media(id) ON DELETE CASCADE,
    model         TEXT    NOT NULL REFERENCES models(id) ON DELETE CASCADE,
    dim           INTEGER NOT NULL,
    vector        BLOB    NOT NULL CHECK (typeof(vector) = 'blob' AND length(vector) = 4 * dim),
    source_hash   TEXT    NOT NULL,
    embedded_at   INTEGER NOT NULL,
    PRIMARY KEY (media_id, model)
);
```

Two limits shipped with, recorded here rather than discovered later. **A path check is size plus whole-second mtime**, so an
in-place rewrite that preserves both — with a current vector already stored — is not noticed; closing it would mean hashing
every figure on every run, and backlog **F27** carries the decision. And the pass **re-digests whatever it reads**: when a
vector is owed and the bytes come off disk, the digest comes from those bytes, so a fooled stamp can delay an encode but never
mislabel one.

`media_path` keeps byte-exact paths for the same reason `files` and `chunks` do (F13): a name that is not valid UTF-8 is a
name this program has to store, not one it gets to reject. Renaming a file re-points one row and **does not re-embed**,
because nothing about the bytes changed.

**What `links` is, and what it cannot be.** The first check caught a claim of mine that the code does not support. `links`
records `src_note_id`, `target`, `line` and `kind`, and every attachment link carries `dst_note_id IS NULL` by design — "there
is genuinely no note on the other end". Its `target` is a **TEXT column of lossy display text**: note bodies are read through
a lossy decode (F13/F20's reason), matching is registry-wide by exact `rel`, then case-fold, then unique basename, and it is
**not relative to the linking note's directory**. So `links` cannot be the byte-exact resolver §7 wants, and pretending
otherwise would make a diagram with a non-UTF-8 name in its path unresolvable by construction.

The division this design uses instead:

* **`links` stays what it is** — the prose record of what an author wrote, including targets that place nowhere. Unchanged,
  no new kind, no migration touching it.
* **Media resolution is its own pass inside embed**, reading the note's raw bytes (not `links.target`) and resolving
  `![alt](target)` relative to the note's directory, byte-exact, percent-decoding the target once (`%20`, `%41`) because a
  markdown destination is a URI reference and a space in a filename is ordinary. A target whose escapes do not decode cleanly
  is **not** decoded — the raw spelling names no file, so it refuses as `missing-target` rather than inventing a class for a
  malformed escape. `undecodable` means something narrower and later: an accepted raster whose decode fails.

**Three things §7 had to settle before Task 3 could be written, now that code exists.** *Containment includes links:* a
directory inside the workspace can be a symlink to somewhere outside it, and then every component of the path is "inside"
while the bytes are not, so any symlink in the walk — the file itself included — refuses as `outside-workspace`. Canonicalising
was the other candidate and has two failure modes this does not: it needs the target to exist (turning every absent figure into
an error) or it needs the root canonicalised too (a workspace that is itself a symlink then refuses everything). *A destination
with a title is one reference:* `![d](kafka.png "Kafka ingest")` names `kafka.png`, because markdown ends the destination at
whitespace and a name containing a space is written `<my diagram.png>`; treating the title as filename bytes would refuse a file
that exists with `unsupported-format`, a class reserved for extensions outside the accepted set. *A platform's separators are
that platform's:* `C:\shots\a.png` and `..\..\outside.png` are single, odd filenames on Unix and rooted or escaping on
Windows, where pushing such a component discards the base path entirely — so the rule is chosen by a `PathStyle` the resolver is
given rather than assumed, which also means the Windows escapes are tested on the Linux host that wrote them.

An external destination (`https://cdn.example/x.png`) gets no class of its own: §8's set is closed and "not a local file" is
closest to `missing-target`, which is what it reports on Unix — under the Windows rules a `scheme:` prefix looks like a drive and
refuses as `outside-workspace` first. A note that wants a remote figure shows the refusal count; nothing is fetched.

**Lifecycle, which the first draft left undefined.** **A media row lives while at least one live path in `media_path` exists
and at least one live note links one of those paths.** The embed pass computes that set from the same scan that produced the
links: unlinking a diagram removes its `media_path` row, and when the last one goes the media row goes with it, `ON DELETE
CASCADE` taking the vectors — the same shape as D3/D5's "vectors die only with their row".

**When that stops being true, which is the pass's schedule and not a promise about reads.** The set above is computed by the
text path's sweep, so it becomes true at the next `np embed` — not when a note is deleted. Between those two moments
`np search` can still rank a figure whose last linking note is gone: its `media_path` row survives (§7 rule 4 protects figures
that are merely absent today, so an unmounted volume does not delete anyone's vectors), and `prune_unlinked_media` deletes rows
holding **no path row at all**, which is a different question from "no live note links it". A read-only command cannot close the
window without writing, and writing would break §12's rule that search changes nothing, so the window is documented rather than
closed. Nothing is deleted by `scan`, which never has a runtime in hand.

**Which note a hit reports.** One diagram, five linking notes: the vector belongs to the file, and the report line names one
source **deterministically** — the lexicographically smallest live `rel_bytes` among the linking notes — with the count
available under `--json` (`linked_from: 5`). Picking "the first one found" would make a search result's wording depend on row
order, which is how a stable index produces unstable output.

**Migration 10 carries a third change.** `model_files` (migration 9) records each member's `sha256` and `size` but **no stamp**,
so verifying the projector means hashing all 554,821,056 bytes every run — about 21 s at the 26 MiB/s D12 measured, charged to
every embed that touches images. Migration 10 therefore adds `verified_size` / `verified_mtime` to `model_files`, exactly as
migration 8 did for `models`, and D12's stamp semantics carry over unchanged: one `stat` when nothing moved, a full hash when
size or mtime changed or no check has run yet. It also makes "no projector recorded" distinguishable from "projector recorded
and unchecked", which migration 9 could not express for an `install --path` model.

Not backfilled by hand: migration 10 creates the two tables, adds the two columns, and the next `np embed` fills them.
Migration count moves 9 → 10; published migrations are never renumbered (D12 consumed 8, R7b took 9).

## 7. When an image is (re)embedded

* **Resolution.** Relative to the *note's* directory, byte-exact, inside the workspace. Absolute paths and anything escaping
  the workspace root are refused with a class rather than followed — the same reasoning as refusing to repair a broken link.
* **Identity.** `media` is keyed by content hash; `source_hash` is the file's content hash. A vector is stale exactly when
  `hash` no longer matches what was embedded, which reuses D12's stamp (`verified_size`, `verified_mtime`) to skip re-hashing a
  file that has not moved and to full-hash when it has or was never stamped.
* **Deduplication.** One diagram, whatever its shape on disk: `media` is keyed by content hash, so five notes linking one
  path — or three copies in two folders — produce **one vector** and one search hit. The paths are rows in `media_path`, and
  the hit names them (`linked_from`, §6/§12). Chosen over per-path embedding on 2026-11-02 at the human's instruction; it
  also means a copy or a rename cannot create a stale duplicate that outlives the original.
* **Currency.** A note whose text changed but whose images did not re-embeds its chunks and leaves its media vectors alone.

## 8. Formats, budgets and refusals

**Accepted today: `.png`, `.jpg`/`.jpeg`. Nothing else.** Both decode through `stbi_load_from_memory` inside the pinned
`mtmd` (`tools/mtmd/mtmd-helper.cpp`), so no dependency enters a build that is currently C-and-C++-only. Everything else is
refused with the class `unsupported-format`, because "we could probably decode it" is not the same claim as "this build does".
Decisions taken on 2026-11-02, recorded here rather than in a chat log:

* **`.svg` was asked for and deferred.** It is not raster, so no option turns it on; it needs a renderer added to the build
  (a pure-Rust rasteriser behind `vision`, rendering at a fixed longest side, with external references disabled — an SVG is a
  file that asks to fetch things). The human's answer was to start with `.png` and `.jpg`, which keeps round one dependency-
  free. **This is the known gap for a diagram library**: every `.svg`, `.drawio`, `.excalidraw` and `.pdf` link already
  arrives as a `links.kind = 'attachment'` row today, so round one will print an `unsupported-format` count on machines whose
  diagrams are vector. When that count stops being tolerable, the rasteriser is its own round with its own size measurement —
  not a list edit.
* **`.bmp` is decodable and excluded anyway**, because accepting a format nobody has is a promise with no test corpus behind it.
* **`.webp`/`.tiff`** need codecs this pin does not link (WebP goes through ffmpeg there), so they are a dependency decision.


Refusal classes, all of them counts on the report rather than exit codes (runtime refusals stay **1**, argv refusals stay
**2**; no new exit code):

| Class | Meaning |
|---|---|
| `missing-target` | link resolves inside the workspace, file absent |
| `outside-workspace` | target escapes the root or is absolute |
| `unsupported-format` | extension not in the accepted set |
| `undecodable` | accepted extension, decode failed |
| `over-budget` | the encoded image exceeds the context window's room — ceiling = window = 8,192 (§5 of the local-embeddings design), against a published default cost of **280 tokens per image** (§1.2), so at the default budget this should never fire and firing means something is wrong |
| `vision-unavailable` | built without the vision code, or `mtmd_support_vision` answers false |

`over-budget` is measured after encoding rather than guessed from file size, for the same reason the tokenizer probe came
before the chunk-size constant: a 40 KB PNG and a 4 MB one can cost the same number of patches.

## 9. Step 0, before any of the above: does this pin drive this projector?

The plan's first task is a throwaway probe, not product code: build `libmtmd` from the vendored tree behind a scratch
feature, open the pinned `mmproj` with `mtmd_init_from_file`, and print `mtmd_support_vision`, the marker form, and the patch
count for one generated test image. Two outcomes, both useful:

* **Recognised** — what §1.1's header reading predicts (`clip.vision.projector_type = gemma4v`, and v0.8.0 ships that
  builder). The probe reports four numbers and they become constants or refusals, not folklore: the **patch/token cost of one
  diagram** (against the card's published default of 280), whether a soft-token budget is honoured at all in this builder
  (§1.2's 70–1120 range), the pooled vector's width read back from the C side, and — on two visually different generated
  images plus their captions — whether caption-vs-image cosine beats caption-vs-unrelated-text. That last check is the cheap
  version of "is this the unified space the card describes", run here because our runtime is ours.
* **Not recognised** — we stop before writing schema against a runtime that cannot fill it, and the choice comes back as what
  it is: move the llama.cpp pin (its own round, with `docs/design-m2-local-embeddings.md` §14's identity discipline re-run)
  or take contingency C. Either way the
  design above survives; only the encoder changes.

This is deliberately first: R7b's live fetch found a naming defect that no test could see because every test asserted its own
chosen paths, and the lesson carried forward is to let the runtime answer before writing the thing that depends on its answer.

**Measured 2026-11-02 — see `crates/np-core/tests/probes/vision-spike-2026-11-02.md`.** Both outcomes above were settled by a
run, and three details came back that no document predicted: vision links **six** archives, not five (`libmtmd` needs
`hash_sha256_hex` from `vendor/hash/libvendor-hash.a`, so Task 4's build change is two archives); images decode with
`llama_set_causal_attn(false)` and must restore it afterwards, or later text vectors in the same process go wrong while every
image vector looks fine; and the soft-token budget (70–1120) *is* honoured by this builder even though R7c has no reason to
use it.

## 10. The encoder seam

`local_model` grows a vision path beside the text path, behind the same `local-model` feature plus a new **`vision`** feature
(default off, so no build today changes size or dependencies), with this shape:

```rust
pub fn embed_image(&self, bytes: &[u8]) -> Result<Vector, EmbedError>;
```

Same 768-d unit vector contract as text (§7 of the local-embeddings design): mean pooling, then L2-normalise, at
`n_embd_out = 768` — `n_embd_inp = 512` stays an internal width and never appears in a stored vector. The published recipe is
mean pooling plus a learned 512 → 768 projection (§1.2), which is the same pair the artifact reports, so this is following
the card rather than inventing a convention.

**No prefix on the image path.** §1.2's rule is explicit: instruction prefixes apply to text only and media are passed bare.
The two constants `local_model` carries today (`task: search result | query: ` and `title: … | text: …`) stay unreachable from
`embed_image`, which takes bytes and nothing else — the API shape is what keeps this honest, because a caller cannot pass a
prefix it has no parameter for. A diagram embedded under a document prefix would still produce a plausible unit vector and
rank slightly worse forever. Same artifact checks: D12's
verify-on-change runs against **both** members before any pixel is touched, which is what migration 10's projector stamps buy.
And the image path must not be reachable when the model row declares no projector file: `model_files` records members, so a
missing `mmproj` entry is a `vision-unavailable` refusal at construction rather than a decode failure mid-run.

**One claim from the first draft was wrong and is corrected here.** It said "no build today changes size or dependencies".
It cannot: `build.rs` builds the single cmake target `llama` and links four static archives, and v0.8.0 compiles
`tools/mtmd` only under `LLAMA_BUILD_MTMD`, default **off**. Turning vision on therefore means a new cmake option, a **fifth
archive** in the link order §14's ordering note governs, and a package-size re-measurement beside R7b's numbers — all of it
behind the cargo feature so the three shipped sizes stay what they are. Two consequences stated rather than buried:

* `vision` implies `local-model`; there is no vision in a default build, at any feature setting.
* Release binaries keep `local-model` off exactly as today (§2's refusal), **so the proof of concept runs in a dev build with
  `--features local-model,vision`** — in a release binary `np search` answers the runtime-not-built refusal at exit 1. That is
  the expectation to agree on, not a bug to discover later.

Determinism: whether an image vector is bit-stable across worker counts is unknown and must be measured, not assumed — D13
established bit-identity for text on this artifact by comparing raw dumps, and the same comparator gets an image case before
any claim is written down.

## 11. `np embed` grows an images arm

The report gains counts beside the existing ones, printed in §6's order and mirrored in `--json`:

```text
images     links 7 · embedded 4 · refused 3 (missing-target 1, unsupported-format 2)
```

**The noun changed on the way to the code, and it was not cosmetic.** This section said `seen`, which reads as "figures this run
looked at"; the number is really **written link targets**, de-duplicated per note, so a figure behind a broken link counts there
and nowhere else — which is what makes a corpus of nothing-but-broken-figures still print a line rather than stay silent. `links`
says that; `seen` did not (Task 6 review).

`workers` (§14's R8 field) applies unchanged — an image encode is the same context. Nothing prints a per-image line at
default verbosity; `--json` carries the per-file detail because a person diagnosing one bad file needs its path, and a report
that hides it invites "why is this diagram missing".

## 12. `np search <query>` (read-only, proof of concept)

* One text query, embedded with the **query** prefix the corpus rows already use the **document** spelling of (§1.2's
  asymmetric retrieval pair, and R6c2's §A′ seam); image vectors in the index carry no prefix at all, per the same rule.
  Cosine against every `embeddings` and `media_embeddings` row for that model; top-k across both kinds (`--limit`, default 10).
* Text output is one line per hit with kind, score to four decimals, and where it came from:

```text
0.6231  note     notes/ingest.md#kafka
0.5904  image    notes/diagrams/kafka-ingest.png   (from notes/ingest.md)
```

* `--json` gives `{score, kind, path, source_note, linked_from}` — `source_note` by §6's deterministic rule, `linked_from` the
  count of live notes pointing at that file (absent for a note hit) — and nothing else; no streaming, no caching, no persisted query log — a
  read-only command that writes nothing cannot drift out of sync with the index.
* **It refuses rather than blends** if §9's probe or a later measurement shows the two vector kinds are not comparable:
  per-kind sections instead of one list, and a line saying why.
* Exit codes: no query argument is clap's **2**; an empty index or an unbuilt runtime is a runtime refusal at **1**. No new
  exit code, in keeping with every round so far.

## 13. Install-time fetch of the model pair

**Decided 2026-11-02: installing the program is what pulls the model files**, rather than a later command someone has to
remember. Concretely, the documented install step becomes "place the binary, then run `np models fetch`", and R7b's engine does
the rest: streaming digest, `.part.<id>.<nonce>` partials, the marker file, publish-into-absent-target only, and a refusal that
names a class rather than a URL.

Two consequences of that decision are load-bearing, and both amend existing rules rather than sneak past them:

* **A default origin enters the pin table.** Before this round `fetch` required `[models] base_url` or `NP_MODELS_BASE_URL`, because
  `docs/design-m2-local-embeddings.md` §12 rule 7 makes the *host* configuration and the pin table say only *what to accept*.
  *(This bullet first cited "models-install §12"; that document has no §12 — it ends at §9. Corrected 2026-11-04, when the
  amendment was written into the rule where it actually stands.)* An install-time fetch has no config
  file yet, so it needs a baked-in default: `https://huggingface.co/ggml-org/embeddinggemma-2-GGUF/resolve/main` — where the
  pinned digests came from. **Rule 7's intent survives the amendment**: the digest still decides acceptance, so a different
  host serving the same bytes is indistinguishable from the right one, and `[models] base_url` (or `NP_MODELS_BASE_URL`, when
  the file leaves the key unset) still **moves** the transfer for a mirror or an air-gapped network — moves, does not override:
  a value the config file spelled out wins over the environment, which is every `[models]` key's existing rule. What changes is that a machine with no configuration will make one outbound
  request during installation. That is the human's call, made in these words ("the installation of the program should trigger
  the HuggingFace fetch"), and it is written here so a future reader finds a decision rather than an unexplained default.
  As landed on 2026-11-04, the precedence is `[models] base_url` → `NP_MODELS_BASE_URL` (which fills that key only when the
  file leaves it unset — `config.rs`'s existing rule for this section, not an override) → the baked default, in one function:
  `fetch::resolve_origin`.
* **Fetch what this binary can use.** A release build has no `local-model`, so pulling 309 MB of weights it cannot load would
  be waste dressed as convenience; a `local-model` build without `vision` has no use for the 554 MB projector either. So the
  member set fetched by default is **the members this binary can consume** — primary only for a text-only build, both for a
  vision build — with `--member` still explicit for anyone fetching on behalf of a different build. That same design's rule that a refusal
  never names an origin is unchanged, and the refusal §13 adds names the missing capability: asking a text-only build to fetch
  `mmproj` is refused because it has no image support, not because nothing pins that member.
  As landed on 2026-11-04 the gate is `vision` alone — `pins::members_for_build()` returns the primary artifact on every build
  and adds the projector when `vision` is on. Whether a build with no `local-model` at all should skip the **primary** as well was
  not decided in these words, and shipping that reading would make `np models fetch` install nothing on the release shape §13
  exists for. What ships is the narrower gate above; if a `local-model`-less build should fetch nothing by default, that is a new
  decision to ask for, not a consequence of this one.

What installation does *not* gain: it never re-fetches over an existing artifact (R7b's overwrite refusal stands), and it does
not silently repair a digest mismatch — a wrong file is reported, not replaced.

## 14. Measured, and what is still not claimed

The first version of this section was written before the fact, so it could not contain numbers. It contains them now, each one
with the command that produced it and the date, and every figure measured **once** says single-run — `docs/design-m2-local-embeddings.md`
§8's rule is two runs and the spread, never one lucky number, and a spike is not a spread.

### 14.1 Token cost per image: the patch grid, not the card's flat 280

| Image | image tokens | chunks | measured |
|---|---|---|---|
| `red_square_1024x768.png`, `stripes_1024x768.png` | **336** each | 3 — text 2 + image 336 + text 2 | spike 2026-11-02; re-measured through the shipped seam 2026-11-04, twice, identical |
| `red_square_512x384.png`, `stripes_512x384.png` | **88** each | 3 — same shape | same two dates |
| a 4096×3072 PNG | **1,064** | not printed | 2026-11-03 through the seam, **single-run** |

The re-measurement is the row that earns a claim about *this* build, because it runs `VisionModel::open` + `embed_image` +
`last_token_cost` rather than the spike's C driver. Command, run twice on 2026-11-04 with both artifacts set; both passes
printed `TOKEN COST: 512x384 -> 88, 1024x768 -> 336 (card's flat default: 280)`:

```text
NP_TEST_MODEL_PATH=/home/agent/models/embeddinggemma-2-Q8_0.gguf \
NP_TEST_MMPROJ_PATH=/home/agent/models/mmproj-embeddinggemma-2-Q8_0.gguf \
  cargo test -p np-core --features local-model,vision --test local_vision -- \
    --nocapture --test-threads=1 token_cost
```

The six processes' full output — including the two comparability runs below — sits in this round's transcript at
`.superpowers/sdd/2026-10-23-note-processor-m1-core-catalog/r7c-task8-vision-rerun.log`. That directory is gitignored, so the
transcript is discoverable rather than published: **this round touched no file under `crates/`**, and the two lines it printed
are quoted inline above and in §14.3, which is what has to be re-runnable by someone else.

Three consequences, all in code rather than in prose: the cost is **read back per encode** (`ImageEmbedding::token_cost`, and
`tests/local_vision.rs::token_cost_is_measured_per_encode_and_printed` refuses a cost that is zero or that fails to grow with
the image), file size predicts nothing (§8's reason `over-budget` is measured rather than computed), and **one image cannot
reach `over-budget` on this pair** — mtmd clamps the patch grid to the projector's own ceiling (1,064 tokens for an image whose
pixels would want ~49,000 patches) against a window of 8,192. The guard ships anyway: past that ceiling llama.cpp *asserts*
instead of returning a code, so the check is the difference between a report line and an abort.

### 14.2 The soft-token budget is honoured by this builder, and nothing here uses it

From the spike's budget runs (`vision-spike-2026-11-02.md`, "Budget runs"): with `--image-min-tokens 70 --image-max-tokens 1120`
the 512×384 fixture still costs its native **88** — the budget is an upper bound and that image's grid is smaller — while
`70/70` drops it to **63**. Both are **single-run**: one process per setting, no repeat, so they are the spike's observations
and not a fence. Product code leaves both parameters unset (`-1`) and takes the native grid, which is why re-measuring them is
a C-side job — `./vision_probe MODEL MMPROJ --image-min-tokens 70 --image-max-tokens 70 img:…` against a tree built with
`-DLLAMA_BUILD_MTMD=ON` — and not something `cargo test` can hold. The card's quality/token trade therefore exists in this
runtime; R7c does not offer it, and no stored vector was made at a narrowed budget.

### 14.3 Comparability: our runtime reproduces the spike's ordering on these shapes

| Pairing | spike 2026-11-02 (C driver) | this seam, 2026-11-04 (two runs) |
|---|---|---|
| "a red square on a white background" ↔ its own 1024×768 image | +0.7465 | **+0.746469**, both runs |
| the same caption ↔ the *other* 1024×768 image (stripes) | +0.5578 | **+0.557844**, both runs |
| "blue and white diagonal stripes" ↔ its own image | +0.7675 (1024×768) — see the correction below | not re-run |
| image ↔ image, the two 1024×768 fixtures | +0.7295 | not re-run |
| text ↔ text, the three sentences | +0.6051 … +0.6440 | not re-run |

The correction Task 8 found while transcribing, applied to the spike note where it stands: that file wrote the stripes caption
as "+0.7675 / +0.7774 (two runs)", but its RUN A lists item 3 as `stripes_512x384.png` — a **different bitmap** from item 1's
1024×768 stripes — so those are two pairings, not one pairing measured twice. The ordering claim survives; the "two runs"
annotation did not.

What the table licenses is narrow and is all §12 needs: **one space on these shapes** — a caption beats an unrelated sentence
against its own image, its own image wins, and image-to-image sits in the same band as text-to-text. Four generated images and
three sentences say nothing about a real diagram library, which is why §12's single ranked list rests on the card's published
property plus this sanity check and never on a retrieval claim.

### 14.4 Not measured, and what it would take

* **Image encode throughput. No figure exists.** What exists is libtest's per-case wall clock (30 s for the token-cost case,
  51 s for the caption case, 2026-11-04), which is dominated by loading 309 MB of weights and building the vision graph and
  therefore says nothing about one encode. Closing it: time `embed_image` around the call in an artifact-gated case at both
  fixture sizes, ≥2 runs, tokens/s derived from the recorded cost — only then may §8's evening-rule arithmetic gain an image
  column.
* **Peak RSS with the projector open.** The ~420 MiB figure predates `mtmd` and belongs to a text-only context. Closing it:
  sample `VmHWM`/`VmRSS` the way `tests/local_corpus.rs` does — before load, after `VisionModel::open`, after N encodes.
* **Bit-stability of an image vector across worker counts**, which §10 demanded before any claim. What is fenced is a repeat
  encode *inside one process*: bit-identical components, re-run twice on 2026-11-04. Closing the rest: give
  `crates/np-core/tests/probes/thread_vectors_compare.py` an image case at 1/4/16 workers, and carry D13's caveat — bit-identity
  is an observation about this llama.cpp revision and must be re-checked after a re-pin.
* **Retrieval quality.** Unchanged from the first draft, and it is still the biggest hole: "it finds the right diagram" needs a
  labelled set (query → which figure answers it), reported as rank@k per kind through `np search --json`. `np search` proves the
  plumbing reaches both kinds; it does not prove usefulness, and neither this document nor a report will imply otherwise.
* **Package size of a vision binary**, because no such binary exists: `np-cli` has no `vision` passthrough, so
  `cargo build --release -p np-cli --features local-model,vision` fails at feature resolution (**F28**). R7b's three shipped
  sizes are unchanged by this round precisely because nothing that ships can link the new code.
* **Windows and macOS: nothing.** `build.rs` refuses to build the C++ off Linux, so there is no vision behaviour on those
  platforms to describe — F18's rule, stated here rather than left as a silence.
* **Remote images**, still undecided rather than deferred quietly (§3): refused with a class, never fetched.

## 15. Decisions taken, and what remains open

**Answered by the human on 2026-11-02**, in the order they were asked:

| Question | Their answer | Where it lands |
|---|---|---|
| Release binaries vs dev-build PoC | Not answered directly; they answered *when the bytes arrive* instead | §10 and §13 state the consequence plainly: install-time fetch is real, and a release binary still has no vision code to use what it downloaded. **Still open** — see below. |
| Which formats | ".png, .jpg and .svg" first, then "only .png and .jpg for today" after learning SVG is not decodable | §8: two raster formats, `.svg` deferred with the reason written down |
| One diagram at two paths | "if this is more reasonable" — delegated | §6/§7: content-hash identity, one vector per distinct bytes, paths as rows. My call, made because it removes a stale-duplicate failure mode rather than picking a favourite path. |

The builder check's remaining questions are answered above or settled as my calls (§6's resolver split, lifecycle and member
stamps; §10's build consequences). One of its worries — that this pin has no builder for this projector — turned out to rest on
grepping for the model name instead of the projector type; §1.1 records what the file itself declares.

**The one question still open, and it is yours.** Fetching on install says the *bytes* should arrive without being asked
twice. It cannot say the *capability* will be there: `vision` is compiled C++, and release builds keep `local-model` off, so a
shipped binary will download happily and then refuse to embed images. Either releases start carrying `local-model` (a real
decision about what we ship: package size, Linux-only refusal, the build matrix), or the proof of concept runs in a feature
build and releases keep refusing — and then install-time fetch should probably skip the projector for release builds, which
§13's "fetch what this binary can use" already implies. Nothing in the plan is blocked on it; the answer decides whether your
first search happens in `cargo run --features local-model,vision` or in an installed binary.

## 16. What is left to the builder

Naming of internal types; whether `media` rows are created during scan or during embed (this design says: **during embed**,
because that is when the model and its projector are known to be usable, and a scan must not depend on a runtime it may not
have); how the cosine pass is structured as long as it reads both tables and writes nothing; where the image budget constant
lives once §9 measures it.
