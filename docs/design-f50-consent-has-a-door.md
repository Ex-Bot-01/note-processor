# F50 — a consent answer has a door: `np` writes its own config, and says what it changed

**Status: built.** `np_core::config_write::{set_embeddings, EmbeddingsTarget}`, the CLI commands
`np embeddings enable|disable`, and the serve methods `embeddings.enable` / `embeddings.disable`, shipped in the issue-#7
round. Numbered here rather than in [`backlog-m2-followups.md`](backlog-m2-followups.md); F49 was the last number taken.

## The gap

An application built on `np serve` asks one consent question — *may np download the embedding model so search can use
meanings?* — and when the person answers yes, nothing in np's surface could act on it. Issue #7 measured `np 0.4.0` over the
pipe with the `[embeddings]` section varied between runs:

| `[embeddings]` in config | what `embed` answered |
|---|---|
| *(no section at all)* | `embedded: 0`, `model: null`, `fetched: null` — nothing attempted, and **a successful response** |
| `provider = "local-model"` only | fault `-32000`: the provider needs `embeddings.model` |
| `provider = "local-model"`, `model = "embeddinggemma-2"` | fetched 309 855 456 bytes with progress, then `embedded: 1` |

The reporter agreed with the first row's default — inference should not start merely because a binary exists — and named the
problem precisely: **there is no way to leave it except by hand**. Eight methods answered over the pipe and none configured
anything; the CLI has no command that writes config; `np setup`, which is "built for an application that wants vectors to
exist", exits 1 on *no provider* because the provider is the one thing it will not set.

So a front end had exactly one move: open np's TOML and edit it. Which is the wrong move, and the reporter said why better
than this file can — validation would be duplicated outside the code that owns it, a global file would change terminal
behaviour because something happened in a window, and consent lives in the app's per-vault registry while np's model rows live
per workspace and its config lives globally.

## Why `np` writes the file rather than teaching others to

Three of #7's three arguments are about **who can check the write**, and they are all true:

- `config.rs` refuses `provider = "openai-compatible"` in its own words, refuses a `local-model` provider with no model,
  bounds `max_tokens`, and since **F35** rejects every unknown key and every misspelled section. A GUI writing TOML has to
  re-implement those rules badly or write values it cannot check — and the failure is *silent*, because row one of the table
  above is a successful `embed` that embedded nothing.
- Only np knows that `[embeddings]` is global while artifacts are per workspace, so only np can say what a consent answer
  means for each.
- A write by np carries np's own provenance, which is the answer to "who set this key?" when the file is opened later — and
  the file is opened later, because it is where a person goes to check what an app did.

The fourth reason is measurement: **the write is validated by loading it.** The new writer puts the prospective bytes in a
temporary file *beside* the real one and runs `Config::load` on that temp path before anything is renamed. Every rule above
runs, from the code that owns it, against exactly what the next run would read — no second validator to drift, and the real
file untouched if the answer is no.

## The door

Two serve methods, two commands, one document, feature-detectable off `initialize`'s `methods` the way `links.list` was:

```jsonc
// →
{"method": "embeddings.enable", "params": {"model": "embeddinggemma-2"}}
// ← what it changed and what that costs, before a single byte is fetched
{"changed": ["embeddings.model", "embeddings.provider"],
 "provider": "local-model",
 "model": "embeddinggemma-2",
 "fetch": {"bytes_total": 309855456, "members": ["main"]},
 "persisted_to": "/home/agent/.config/note-processor/config.toml"}
```

`np embeddings enable embeddinggemma-2` prints the same document with `--json`, and `embeddings.disable` /
`np embeddings disable` answer it too. Nine methods over the pipe, and enabling **fetches nothing**: #7's second row is a
state to avoid, and so is a method that surprises a person with a 309 MB download on the click that only *allowed* one.

**There is no `workspace` parameter, which is a decision rather than an omission.** The file this writes is global; accepting
a workspace would imply a per-vault scope np does not have, and a caller that passed one and got a document describing a
global change would be being lied to. Per-vault consent stays where #7 put it — in the app's registry — and the vault-level
work is what already exists: `scan` and `embed` per workspace, which fetch into *that* workspace's artifact directory under
F38's rules. A client that wants "on for this vault only" has that today by keeping `[embeddings]` off and running nothing; a
client that wants it *while* another vault stays keyword-only needs a per-workspace config section, and that is F50's honest
limit rather than a feature to fake with a parameter.

## The writer's rules

1. **A file that does not load today is refused, not edited.** `Config::load` runs first; if it faults, the answer names the
   path and np's own sentence. Half-editing a file its owner cannot currently read is how a small mistake becomes an unusable
   installation. (`data_dir` missing counts: that config does not load either.)
2. **The edit is surgical.** `np` finds the `[embeddings]` table in the *text* and rewrites only the key it was asked about —
   `provider`, or `model` on enable — inserting after the header when the key is absent, appending the section when the table
   is absent. Comments, blank lines, quoting style and every unrelated key survive verbatim, because the file is the person's
   and np is a guest in it. There is no TOML re-serialiser here on purpose: `toml::to_string` would rewrite the whole file
   alphabetically and take the comments with it.
3. **Nothing to change answers `changed: []` and writes nothing** — not a byte, not a timestamp, not a file that was not
   there. Answering *Allow* twice must not append a second `[embeddings]`, and a write that changes nothing still counts as
   "np modified my config" to anyone watching modification times.
4. **Publish by rename inside the directory**, so a crash cannot leave a half-written config, and never through a path
   outside it (a cross-device rename would copy). The temp file is `config.toml.<pid>.tmp` in the same directory.
5. **Refuse rather than half-configure.** A build without the inference runtime refuses with the token `np search` already
   uses (`local-model-not-built`) and writes nothing; an id this build does not pin refuses and lists what it *does* pin,
   which is the same discipline `np models list`'s `pinned` section keeps — never quote a size for bytes nobody would
   download.
6. **After the rename, read it back.** `Config::load` on the real path must report the provider and model that were asked
   for; if it does not, the previous bytes are put back by the same temp-and-rename route and the run refuses. This is the
   half that catches a rule nobody thought of — a validation added later that rejects what this writer just wrote.
7. **Provenance travels as a comment**, in the section np touched: `# np 0.4.0 set embeddings.provider = "local-model" and
   embeddings.model = "embeddinggemma-2" (np embeddings enable embeddinggemma-2) at 2026-11-14T…Z`. It is a comment because
   the file must stay valid TOML for a run that knows nothing about this feature, and it names the command because that is
   the sentence a person can re-run or undo.
8. **Disable writes one key**: `provider = "none"`. It leaves `embeddings.model` where it is — revoking consent to download
   is not a licence to erase which model was chosen, and a file that still names the model is one key away from being on
   again, which is what a person who changes their mind wants. `changed` therefore lists only `embeddings.provider`, and
   `fetch` is `null` because nothing about disabling costs bytes.

## What the document claims, and what it does not

- `changed` lists **keys np wrote**, in file order — not "keys that differ from a default". An empty list means no write
  happened, and only that.
- `persisted_to` is the same fact About/diagnostics already names. It can be `null` on a machine where nothing resolves a
  config path, which is also when enabling refuses rather than writing somewhere surprising.
- `fetch` quotes `pins::plan_size` — **this build's** plan, the number behind the terminal consent prompt and
  `np models list`'s `pinned`, so three surfaces cannot disagree about what saying *yes* costs. It is a pinned size, never a
  transport `Content-Length` (§12 rule 12's reasoning, applied before the transfer as #3 applied it at consent time). And it is
  priced **only when an `embed` would download**: `disable` leaves `embeddings.model` in place, so quoting 309 MB beside
  `provider: "none"` would be a cost the next run will not incur.
- `changed` is computed **per key**, against what the file resolves to rather than its text. Switching one model for another
  therefore answers `["embeddings.model"]`, and an installation with no config that asks to be *disabled* answers `[]` and
  creates no file — because `provider` is already `none` by default, and writing a TOML to agree with a default is the same
  byte-nobody-asked-for that rule 3 refuses elsewhere.
- Nothing here says the workspace is embedded. Enabling changes intent; `embed` does the work and answers with its own arms,
  including `offline`. A caller that wants vectors *now* calls `embed` after this and reads what comes back.

## What is measured, and what is not

Both arms of "may this build turn embeddings on?" are graded on every host: the capability is a parameter
([`config_write::Runtime`](../crates/np-core/src/config_write.rs)), production passing [`Runtime::of_this_build`], for the same
reason **F37** pushed the data-dir *shape* up to `Config::load_with_platform` — a refusal compiled away on the machine running
the tests is a rule nobody checks, and the default-feature CI row has no runtime at all.

Core cases in `crates/np-core/tests/config_write.rs` pin the surgical edit (comments and unrelated keys intact, section
appended when absent, idempotence writing nothing), the refuse-without-touching paths including "no temp file left in the
directory", the fresh-install arms for both doors, and that a file which does not load today is refused. The two surfaces are covered in `crates/np-cli/tests/cli.rs` and
`crates/np-cli/tests/serve.rs`, including that both answer the *same* document and that a build without the runtime refuses
identically on both sides of the pipe.

The read-back rule (rule 6) has **no reachable trip path today** — the same bytes are loaded twice — and is kept because it is
the fence that catches a validation rule added later rejecting what this writer writes. Saying so here is cheaper than letting a
reader hunt for a test of a branch that cannot be reached.

Not measured, and stated rather than smoothed: **the rename-over-an-existing-file path on Windows** relies on
`std::fs::rename`'s documented replace semantics (`MoveFileEx` with `MOVEFILE_REPLACE_EXISTING`) and has only been run on
Linux; **two writers at once** — np takes no lock on its config file, so a person editing by hand while an app enables
embeddings is last-write-wins, with the reload check covering only np's own write. Both are recorded here because "the file is
the person's" is a claim that needs its failure modes next to it.
