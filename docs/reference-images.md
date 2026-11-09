# Images in the index

Which figures `np` will encode, which it refuses by decision, and what each refusal class tells you to do.

> *Moved out of `README.md` on 2026-12-09, when the README became a document you read before installing rather than one you read after something broke. The wording is the same as it was there — including the dates, the measured numbers and the claims about what has not been measured — because those are the parts that carry weight.*

A note links a figure with ordinary markdown image syntax — `![alt](diagram.png)` — and the path resolves exactly like a note
link: relative to the linking file, case-insensitively, inside the workspace only. Accepted extensions are `.png`, `.jpg` and
`.jpeg`, **and** the bytes must carry that format's signature: an accepted name wrapping a BMP or a GIF is refused as
`unsupported-format` rather than handed to a decoder that would happily read it (F26).

Figures are content-addressed, so identical bytes across notes encode once, and every refusal is counted under one of six
classes on the `images` line:

| Class | What it means, and what to do |
| --- | --- |
| `missing-target` | No readable file at that path: absent, a directory rather than a file, unopenable, or unreadable part-way through hashing. Fix the target or restore the file |
| `outside-workspace` | The target is absolute or escapes the workspace root (including through a symlink). Nothing outside the vault is read; move the figure in, or link it relatively |
| `unsupported-format` | Not `.png`/`.jpg`/`.jpeg`, or an accepted name whose bytes are not that raster — including every `.svg`, `.drawio`, `.excalidraw` and `.pdf`, which are out by decision, not oversight (see below). Re-export as PNG or JPEG |
| `undecodable` | Accepted name and signature, but the decode failed — a truncated or damaged file is the usual cause. Re-export the figure; nothing partial was stored |
| `over-budget` | The encode measured more image patches than the context window holds (8,192 tokens). **Never observed on this artifact pair**: one size was measured — a 4096×3072 PNG cost 1,064 tokens — and larger sizes have not been. If you see it, suspect a narrower window or a re-pinned artifact rather than a slightly big diagram |
| `vision-unavailable` | Two meanings, one label: this binary has no image code, or **a provider refused something this report has no class for**. Every release build of `np` is the first case (`vision` is off by default, like `local-model`), so every figure that *resolves and passes the content gate* lands here — and a build made with `--features local-model,vision` reports `projector-absent` instead until the projector is installed |
| `projector-absent` | This binary **does** have the image code and this workspace has no projector for it to load: the `mmproj` member is not recorded by this database, or is on disk but not the bytes that record vouches for. Fetch or install it. Split out of `vision-unavailable` by **F28**, because the two advise different actions — one file versus a rebuild — and the old folded label kept the count honest while sending someone to the wrong command |

**Vector and diagram source formats are out on purpose ([F29](backlog-m2-followups.md)).** `.svg`, `.drawio`, `.excalidraw` and `.pdf` links
resolve like any other attachment and then refuse as `unsupported-format`. That is a product limit, not a gap waiting for an implementation note:
rasterising them means a decoder that must disable external references and DTDs — an SVG is a file that asks to fetch things — which is a dependency,
an attack surface, and a per-encode cost this project's rules say gets measured before it is added, all of it for a capability nobody has asked for yet.
So the answer to "what will `np` encode" is **PNG and JPEG**, extension *and* content signature both.

What a refused figure still costs you is nothing but the count: the note around it is indexed as text like any other, its prose searchable, and the link
itself recorded in `links` — so `np search` finds the note by what you wrote about the diagram. It will not find it by what the diagram depicts, which is
the half a rasteriser would buy.

The counts count what their nouns say: `links` is written targets de-duplicated per note (a refused link still counts),
`unchecked` means some note could not be read so the sweep was held off, and `pruned` reports vectors deleted along with the
figure they belonged to.

## Known gaps, stated plainly

build's corpus lands in `vision-unavailable`. A build made with `--features local-model,vision` does encode them — the passthrough **F28** was is
in place, and the run hands over only a projector this workspace has recorded and whose digest (or stamp) matches. What that build needs from you is
the file itself, and either door is open now: `np models fetch --member mmproj`, or `np models install --member mmproj` from a
file you copied there yourself (**F28** step 3, offline, digest verified). And `np models list` now shows what landed:
`files mmproj:present` on the row, or `members` in `--json` (**F28**'s reporting gap, closed — it was a listing gap, not an
install one, and fetched projectors were invisible too). Image throughput, resident-set growth with a projector open, cross-worker bit-identity and
retrieval quality have **not** been measured — the list is in `docs/design-m2-image-embeddings.md` §14. Windows and macOS get a
written refusal from `build.rs` for feature builds and have never been executed here.
