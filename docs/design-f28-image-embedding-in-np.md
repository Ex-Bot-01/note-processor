# F28 — image embedding in the shipped `np`, and what "linked to where it is placed" means

**Ruled by the human 2026-11-06**, answering the question `docs/backlog-m2-followups.md` F28 posed: *"np should also embed
images"*, and *"image embeddings need their own separate vector, linked to the place where an image is mentioned or placed in
notes."* A second ruling arrived with the merge instruction: **the link is figure → note**, not a coordinate inside the note.

This note records what the tree already does — measured, with file:line, because the card's description of the gap turned out to be
half wrong — and what therefore actually needs building.

## 1. The two halves of the requirement, measured

**"Its own separate vector" — shipped, and it is not a footnoted version of a text chunk.** `media` keys a figure by content hash
(`hash TEXT NOT NULL UNIQUE`, `crates/np-core/src/db.rs:326-332`), `media_path` records every workspace path those bytes were seen at
(`:347-359`), and `media_embeddings` holds one row per `(media_id, model)` with `dim > 0` and `length(vector) = 4 * dim`
(`:362-375`). Migration 10's own comment states the reason it is a separate table rather than a `chunks` row: an image is not prose
whose `ordinal` and byte offsets could be recorded honestly. Nothing here needs changing.

**"Linked to the place where it is mentioned" — already derived, then thrown away.** The card said this part does not exist; that was
too pessimistic, and the correction matters because it removes a migration from the plan:

- `links` stores every written attachment target with its **source note** (`src_note_id`, `kind = 'attachment'`, `target`, `line`;
  `db.rs:123-132`), so a note → figure relation exists in the database today.
- `np search` already reads it. `linker_index` (`crates/np-core/src/search.rs:484-510`) selects every live note's attachment links and
  re-runs the one resolution rule the embed pass uses (`media::link_target_rel`, called at `:501`), and `media_report` (`:437-469`) unions
  those linkers across every path holding the figure, producing `linked_from = N` — "one figure copied to two folders, linked from three
  notes, is one hit with `linked_from: 3`, not two hits" (`:458`).

What it produces **besides** the count is the defect: `source_note` is `notes.first()` of that set — the lexicographically smallest
linking note (`:465-467`, documented as such at `:67`). A diagram placed in `plans/architecture.md` and in three daily notes reports
one of them because it sorts first, and the other three are a number. That is the half of the ruling the tree does not honour.

`search.rs:480` explains why the query was built that way — *"a `note → media` edge would be an eleventh migration, which Task 6
forbids"* — and its doc comment names the cost it accepted: a target whose bytes do not round-trip through `links.target`'s TEXT column
loses that linker, so `linked_from` can undercount. Both facts are load-bearing below.

## 2. Decision: no eleventh migration for placement

An edge table (`note_media(note_id, media_id)`, written by the embed pass, refreshed per note the way `links` is) would buy two things:
exactness against the lossy-`target` undercount, and a place to hang a coordinate. The second is ruled out — the link is figure → note —
and the first is not worth a schema version and a second lifecycle to keep in step with `links`, `files.state` and §7 rule 4's sweep:

- The derivation already runs, is already tested for the cases that matter (a deleted note stops counting, `tests/search.rs:306`; one
  figure at two paths answers once, `:314-331`), and needs no workspace root, which §12's read-only `np search` requires.
- A second source of the same fact is a second way to be wrong. `links` is written by scan; `media_path` by embed; an edge table written
  by embed would disagree with `links` after a scan that has not been followed by an embed, and every disagreement would need a rule about
  which one wins — the class of defect this project's F-series is a list of.
- The undercount stays, and stays documented (`Hit::source_note`'s "undercounting a linker is a reportable fact; inventing one is not").
  It is a name that cannot survive TEXT, not a lost relationship. If it ever costs a real hit, the fix is a `BLOB` target column in
  `links` — one column, on a table whose lifecycle already exists — not a parallel edge.

**So placement is a rendering and contract change, not a schema change**: an image hit carries *the set* of linking notes rather than one
chosen by byte order. See §4.

## 3. Decision: `vision` becomes a buildable capability of `np`, still default-off

`crates/np-cli/Cargo.toml` declares `local-model = ["np-core/local-model"]` and `fetch = ["np-core/fetch"]` and has no `vision`, which is
the mechanical half of F28: the pair `--features local-model,vision` fails at feature resolution for `np` while passing for `np-core`, so
every gate row that "proves vision" proves the library. Add `vision = ["np-core/vision"]`.

**Default stays off, and §9's sentence survives.** The shipped binary links no C++ (`local-model` is default-off for the same reason), and
`vision` implies `local-model` in np-core, so turning it on by default would put a C++ runtime — and an expectation of a 554,821,056-byte
projector — into the artifact every user downloads. The claim README must be able to make is *"this binary can encode figures if you build
it with `--features local-model,vision`"*, measured rather than asserted (§14's ~40 MB ceiling is about the artifact a user downloads, and
the row that measures it has to be the one that says which features were on).

Three things follow from enabling the feature, none of which is a one-line passthrough:

1. **The projector has to be handed over.** `np-cli` builds its provider with `LazyLocalEmbedder::new(...)` (`crates/np-cli/src/main.rs:619-625`)
   and never calls `with_mmproj`, which is the whole reason `supports_images()` answers `false` in every shipped binary. The call needs a path,
   so the projector must be *located*: same workspace models directory as the main artifact, filename from the pin table
   (`pins::MMPROJ.member = "mmproj"`, `filename = "mmproj-embeddinggemma-2-Q8_0.gguf"`, `crates/np-core/src/pins.rs:111-118`), verified by the
   same stamp rule that guards the main artifact (`verify_artifact`) rather than a looser one — a projector whose bytes moved is an encoder
   producing vectors nobody asked for.
2. **Absence has to be sayable, and it is not `vision-unavailable`.** §8's refusal set is closed and its catch-all maps anything unknown to
   `vision-unavailable` (`embed_run.rs:1215-1230`). "This build has no image code" and "this build can encode but the projector is not
   installed" both land there today, and after this change they are different sentences a user acts on differently — install the file, versus
   rebuild. The capability therefore needs one more arm (`MediaRefusal::ProjectorAbsent`) rather than a reuse, plus README's table row for it;
   reusing the existing arm would keep the count right and make the advice wrong, which is F32's lesson in the other direction.
3. **`np models install` cannot place a projector.** `Install` takes one artifact with a declared digest (`crates/np-cli/src/main.rs:104-127`)
   and records it as the id's main file; only `fetch` knows about members (`pins::members_for_build`, `pins.rs:159-168`). So an offline
   install — the reason `install` exists, for a machine that cannot dial Hugging Face — could not enable image support at all. Add
   `--member <main|mmproj>` defaulting to `main`, with `--dim/--prefix-doc/--prefix-query` belonging to the main member only: a projector has
   no declared width and pretending otherwise would record a fact about a file that does not carry one.

## 4. What an image hit reports

`Hit` grows one field; nothing leaves, because README documents that `np search --json` emits *the same keys whatever the kind*
(`README.md:270`) and a consumer that ignores an added key keeps working while a renamed one breaks.

| Field | Meaning after this round |
| --- | --- |
| `path` | unchanged — smallest `media_path.rel_bytes`, §6's content identity |
| `linked_from` | unchanged — how many live notes link it |
| `source_note` | unchanged value (smallest linker), now documented as *the first of `linked_notes`* rather than an independent choice |
| `linked_notes` | **new** — every live linking note, byte-ascending; `[]` for a note hit and for a figure no live note names |

The text line prints the primary note and, when there are others, `(+N more)` — the count is already honest, so hiding the surplus costs
nothing that the JSON does not already carry. Ordering stays byte-ascending because it is the same rule the tie-break already uses on those
bytes (`search.rs:67`, and `HitKind`'s derive order is part of the ordering contract), which means one more rule for a test to keep:
**`source_note == linked_notes.first()` whenever `linked_notes` is non-empty**, asserted rather than assumed, because two fields describing
one relation are how drift starts.

## 5. Order of work, and what each step has to prove

1. **Capability.** `vision` in np-cli; projector location + verification; `with_mmproj` hand-off; `ProjectorAbsent`; `install --member`.
   Red first at the CLI layer — a `--features local-model,vision` build against a scratch workspace with a planted projector must stop
   reporting `vision-unavailable`, and today it cannot even be compiled, which is F28's own measurement. The end-to-end encode is
   artifact-gated (`NP_TEST_MODEL_PATH`, `NP_TEST_MMPROJ_PATH`) and prints `NOT COVERAGE` without them; "it compiled" is never "it ran".
2. **Sizes.** Five shapes — default, `local-model`, `local-model,vision`, each release+`strip`, plus the no-default row — measured in one
   session on this host, with the projector's 554,821,056 bytes recorded as a *fetched* cost beside the binary's own. §14's ceiling sentence
   gets rewritten from whatever the table says, including if `local-model,vision` breaks it: the answer then is "the vision build exceeds the
   ceiling", not a quiet omission of the row.
3. **Placement.** `linked_notes` through `Hit`, printer and JSON, README's key list and example, and the drift assertion above. Existing
   cases (`tests/search.rs:293-351`) keep their assertions — they check the smallest-linker and the count, both of which stay true — and new
   ones name the *second* linker, which is what today's renderer discards.

Catalog goldens are untouched by all three (config and search render no catalog), but `verify_catalog_goldens.py` runs anyway: a golden diff
is a stop signal in this repository, not a review note.

## 6. Out of scope, decided elsewhere

- **Position inside the note** — ruled out above; it is what an eleventh migration would have been for.
- **`.svg`, `.drawio`, `.excalidraw`, `.pdf`** (F29) — ruled the same day to stay out, with README to say so as a product limit. A binary that
  can encode raster figures makes this the next question a user will ask, so the README wording is not optional decoration.
- **Matryoshka truncation** (F30) — storage for a big image index, unmeasured, untouched.
- **`vision` in the default build** — refused in §3; it changes what every download links and expects on disk.
