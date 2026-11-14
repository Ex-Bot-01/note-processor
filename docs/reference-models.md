# Getting a model onto the machine

Where artifacts live, and the two doors by which bytes get there: `np models fetch` (this build's pinned ids only, digest-verified) and
    `np models install` (a file you already hold). A third key turns embeddings on or off without moving any bytes at all —
    [`np embeddings enable`](#the-third-door-that-moves-no-bytes-turning-embeddings-on) (**#7**) — and it is still worth reading
    this page before editing `embeddings.model` by hand.

> *Moved out of `README.md` on 2026-12-09, when the README became a document you read before installing rather than one you read after something broke. The wording is the same as it was there — including the dates, the measured numbers and the claims about what has not been measured — because those are the parts that carry weight.*


Embedding is opt-in twice over: a feature at build time and an artifact at run time. One model identity is pinned in this
repository's build today — `embeddinggemma-2`, 768 components, empty document/query prefixes — and its two artifacts are a
text encoder (309,855,456 bytes) and a vision projector (554,821,056 bytes).

Artifacts live **per workspace**, next to that workspace's database:

```text
<data_dir>/workspaces/<key>.sqlite        the index for one workspace
<data_dir>/workspaces/<key>.models/       its installed artifacts, named by model id
<data_dir>/workspaces.sqlite              the registry of known workspaces
```

`<key>` is a digest of the workspace's absolute path as bytes, so two spellings of one directory share one index and two
directories never share it. **Which spellings collapse** (issue **#4**, since 0.4.0): a trailing separator (`/vault` and
`/vault/`, or `D:\Notes` and the `D:\Notes\` a Windows folder picker returns), a doubled separator, a `.` component
(`np scan .` is the workspace `np scan <absolute>` indexes), and on Windows the separator *style*. What still keys apart is a
path holding `..` — resolving that lexically would be wrong wherever a component is a symlink, and resolving it properly is
canonicalization, which this key deliberately does not do (a key that moved when a link was re-pointed would make the index
depend on the filesystem above the vault). A workspace first indexed under a spelling that 0.4.0 rewrites keeps its old
files where they are and starts empty under the new key: one `np scan` clears it, and `np embed` re-creates the vectors. `<data_dir>` is `$XDG_DATA_HOME` else `$HOME/.local/share`; on Windows it is
`%LOCALAPPDATA%\note-processor` — nested under a folder that says whose it is, like the config file always was (**F37**). Before that
change Windows wrote `workspaces/` and `workspaces.sqlite` straight into `%LOCALAPPDATA%`, which is where every application writes; if
that is where your index is, this release still reads it there and says so on stderr rather than reporting your notes as new.
An explicit `data_dir` in the config file is an instruction, not a location the program moved, and is never overridden.

### The third door that moves no bytes: turning embeddings on

An absent `[embeddings]` section is not a broken installation; it is np's documented default (`provider = "none"`), and
inference does not start merely because a binary was built with the runtime. Until 0.5, the only way to **leave** that state was
to edit `config.toml`. That left an application with a consent answer in hand and nothing to do with it except write np's own
file from outside — re-implementing np's validation badly, which is how you get a config that reads as configured and embeds zero
notes while answering success (**issue #7**, [**F50**](design-f50-consent-has-a-door.md)).

```sh
np embeddings enable embeddinggemma-2      # writes provider and model; fetches nothing
np embeddings disable                      # provider = "none"; the model you chose stays in the file
```

Both commands answer the same document `np serve`'s `embeddings.enable` / `embeddings.disable` answers, so a terminal and a GUI
see one shape: which keys np wrote (`changed`, empty when nothing moved), what the config says afterwards, what a first `np embed`
would cost (`fetch.bytes_total`, from the same pin table as `np models list`'s `pinned`), and `persisted_to` — the file that now
carries the decision.

Four rules worth knowing before you point a button at them:

- **It does not download.** The bytes still move at an `embed`, under F38's announcement and digest rules. This command is the
  consent *answer*, and it quotes the price instead of paying it.
- **The config file is global; artifacts are per workspace.** So enabling happens once, and each vault gets its artifact the
  first time you `np embed` it. That is also why neither command takes a workspace operand: a per-vault consent answer belongs to
  the application's registry, not to np's file.
- **It refuses rather than half-configuring**: an id this build does not pin (the refusal lists the ones it does), a build with no
  inference runtime (`local-model-not-built`), or a config that does not load today (its own error comes back, and np's edit never
  happens). A refusal leaves the file byte-identical.
- **The file stays yours.** Comments, blank lines, quoting style and every unrelated key survive; what np touched is marked with a
  comment naming the command and the instant. There is no TOML re-serialiser in this path on purpose — one would rewrite your file
  alphabetically and take the comments with it.

### Option A — fetch it

```sh
np models fetch ~/Notes --id embeddinggemma-2
```

The command you run when you want to place the model by hand. (It used to be the only thing in the program that opened a socket;
since **F38** `np embed` and `np setup` reach the same origin themselves when the artifact is missing — see the first-run note above for
the three ways to refuse.) Both forms print where they are about to dial, in class terms and never as a URL:

```text
origin      this build's compiled-in default — no [models] base_url is configured (an empty NP_MODELS_BASE_URL counts as unset)
```

A text-only binary fetches only the text encoder; naming `--member mmproj` from such a build is refused, because transferring
554 MB that the binary cannot load is not a service. Interrupted downloads are guarded by a marker file that no clock may
expire — if you know no fetch is running, say so with `--discard-marker`. It will not overwrite an artifact that already
exists:

```text
np: refusing to overwrite the existing artifact <data_dir>/workspaces/<key>.models/embeddinggemma-2
```

### Option B — install a file you already have

```sh
np models hash ~/models/embeddinggemma-2-Q8_0.gguf     # prints "<sha256>  <path>"
np models install ~/Notes --id embeddinggemma-2 --path ~/models/embeddinggemma-2-Q8_0.gguf \
  --dim 768 --sha256 <that digest>
```

`install` reads the file, verifies it against the digest you declared, and copies it into place — it never moves or truncates
your original. One word on stdout: `installed`, or `already installed` when the id is recorded with the same tuple. Re-running
with a **different** dim or prefix for an existing id is refused rather than silently re-stamping history.

The projector of a model — the file that lets a vision build encode figures — installs the same way, from the same offline
file, with `--member`:

```sh
np models hash ~/models/mmproj-embeddinggemma-2-Q8_0.gguf
np models install ~/Notes --id embeddinggemma-2 --member mmproj \
  --path ~/models/mmproj-embeddinggemma-2-Q8_0.gguf --sha256 <that digest>
```

Three things about that command are deliberate. **`--dim` is refused, not ignored**: a projector carries no vector width, and
a flag that reads like a fact about the file would be a quieter failure than a refusal. **The model has to be installed
first** — a member row describes a file belonging to an installed identity, so `--member mmproj` on a workspace with no such
model refuses before reading anything, including the 554 MB you pointed it at. **The digest is still mandatory**: an
unverified projector would encode every figure in your vault with bytes nobody vouched for, and those vectors would then be
compared against pinned-model text vectors forever. `--json` on a member install prints `{"id", "member", "state"}` — the
extra key because `state` alone cannot say which artifact the run placed; the plain model install keeps its two keys.

`np models list ~/Notes` then says what the workspace believes:

```text
models     1
embeddinggemma-2  present  dim 768  sha256 2188ac1deca4  installed_at 1793893791  prefix_doc ""  prefix_query ""  files mmproj:present
unclaimed  0
```

`--json` adds a section beside those rows that answers a question asked **before** any of this exists: `pinned`, what a
fetch would transfer for the id your config names — `{"id", "bytes_total", "members"}` (**issue #3**). It is there so an
application can write *"this needs `embeddinggemma-2`, about 295 MiB, into `<data_dir>`"* and ask, instead of starting a
transfer to discover its length or hardcoding a number that becomes a lie the day the pin moves. Three properties worth
knowing, because each is a choice:

- **the bytes are the pin's**, never a transport's `Content-Length` — §12 rule 12 refuses an announced length while a
  download is running for good reason, and a number quoted *before* the download has fewer excuses; an unannounced size
  is unknown to HTTP and known to this table;
- **it is this build's plan, not the whole pin table** — `bytes_total` sums what this binary would volunteer, which is
  the primary artifact on a text-only build (309,855,456 bytes) and both halves on a `vision` build (864,676,512). The
  disk preflight's number (`np_core::pins::total_size`, always every member) is the second figure; quoting it in a
  consent sentence would over-promise a transfer this build will never make. The terminal prompt in `np setup` adds up
  the same plan, so one machine gives one price;
- **it costs nothing to ask** — no stat, no row, no lock, and it survives even the directory-level failure that leaves
  `models_dir_error` set with no rows at all. Whether *these* bytes are already on disk is `state`, not this: read both
  keys rather than making one mean two things. An empty list means your config names no id **this build pins** — a model
  you installed yourself has no price to name, and borrowing another model's number is the lie the section exists to avoid.

`state` is `present`, or `missing` when the row exists and its file does not — a deleted artifact is reported rather than
forgotten. `files` names the model's **other** members (a projector, fetched or installed offline) and whether each file is
there; it appears only when there is one to name, and `--json` carries the same set as `members`
(`{"name", "file", "present"}`, empty for a model with a single file). It answers by stat, never by hashing — byte
verification happens at use time, where reading 554 MB is already being paid for — so it tells you *a file is there*, not
*those are the right bytes*. Before this field, confirming an offline projector install meant running `np embed` and watching
`projector-absent` go away: a diagnosis standing in for a report. Re-declaring an id with a different width is refused outright, because that would silently change what every stored
vector means; see [Troubleshooting](reference-troubleshooting.md) for the sentence it prints.
