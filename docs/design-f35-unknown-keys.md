# F35 — one rule for unknown config keys: refuse, everywhere

**Decided by the human 2026-11-06**, on the recommendation recorded in `docs/backlog-m2-followups.md` F35: an
invented **top-level** section or key is refused, not ignored. The card's own warning was that denying
"reverses a possible deliberate stance", and it was right — the leniency was deliberate *and pinned*, by three
tests that named the mutation they were guarding (`an_unknown_key_outside_embeddings_still_loads`,
`the_models_strictness_is_scoped_to_the_models_section`, and a scoping argument quoted inside
`the_new_refusals_are_scoped_to_the_provider_that_needs_them`). Reversing a pinned stance is exactly why this
round needed a ruling rather than a judgement call, and the fixtures were rewritten rather than deleted so the
reversal is on the record in both directions.

## 1. What was broken

`[embeddings] typo = 3` refused, naming the file and the line:

```text
config parse error in /home/…/config.toml: unknown field `provide`, expected one of `provider`, `model`, …
```

while a misspelled *section* loaded clean (`[catalogg]` stands in for `[paths]` here; both are invented, and the
second is the one that decides which database a command opens):

```toml
data_dir = "/srv/np"
[catalogg]              # one letter, and the whole section is now fiction
max_entries_per_file = 200
```

That file loads with `catalog.max_entries_per_file = 600` — the default — and prints nothing. Two asymmetries
made it worse than a cosmetic inconsistency:

- **A bad section loses more than a bad key.** A wrong key inside `[embeddings]` costs one setting; a wrong
  *section name* costs the entire subsystem's settings, silently, including the ones that would have made the
  file say something false about the corpus. The card's own instance was the worst shape of it: a top-level
  `data_dir` written inside an invented `[paths]` section goes unread, so the command opens the workspace under
  `$XDG_DATA_HOME` — a *different database and artifact directory* than the file names, with nothing on screen
  saying so (§6 runs exactly that file against the built binary).
- **The failure shape is the one this program exists to refuse.** `[catalogg]` produces a catalog rendered at
  defaults while its owner believes they configured it — the same class as F32's "a sentence about a
  configuration the reader does not have", and the reason `[embeddings]`/`[models]` were made strict in the
  first place. The original argument (§7 of the M2a design, §12 rule 7 of the fetch design) never said the
  rule should stop at a section boundary; it just did not reach above them.

## 2. The rule

Every struct that reads a piece of `config.toml` carries `deny_unknown_fields`: `Config`, `CatalogConfig`,
`MarkerConfig`, `EmbeddingsConfig`, `ModelsConfig`. One sentence describes the file now — *a key this build does
not know is refused, naming the file, the line and the key as written* — instead of "three sections refuse and
two do not, and you have to know which".

The refusal needs no new machinery: `ConfigError::Parse` already carries the path beside serde's diagnostic, so
the top level gets the same message shape `[embeddings]` has always had, at the same exit code (1 — a runtime
refusal, not clap's 2). No new error variant, because "unknown field" is already what `toml` reports and the
only thing that was missing was the container asking for it.

**Which key gets named when a file holds two unknowns: the nested one, measured.** A fixture with
`stray_top_level = 1` on line 2 and `stray = "x"` inside `[catalog]` on line 5 refuses at **line 5**, naming
`` `stray` `` — the descent into a table happens before the root's own unknown-field check. Two consequences,
both acted on: every refusal fixture in `tests/config.rs` holds exactly one unknown key, so the name in the
message is the name the case is about; and one case (`an_unknown_key_inside_a_section_is_the_one_named_when_the_
root_has_one_too`) pins the ordering itself, because a reader who assumed "first line wins" would write the
other assertions wrong, and whoever changes that ordering should re-decide it there rather than meet it in a red
suite. The message is toml's, unedited — this round adds no suggestion list and no rewording (§5).

## 3. What this costs, said plainly

**A config file that works today can start refusing.** Any user who ever left a comment-shaped key, a scratch
key (`todo = "check later"`), or a section from a newer build in `config.toml` now gets a refusal on upgrade.
That is the whole argument for the lenient posture, and it is not free:

- **Forward compatibility is deliberately given up.** A binary that meets a section invented after it was built
  refuses rather than ignoring it. The alternative — ignore unknown sections so a future release's keys are
  inert today — keeps files loadable across versions but keeps exactly the silent-loss failure this round
  removes. Chosen: refuse, because this program ships one binary per workspace and `np --version` is one command
  away, whereas an invisible dropped section can stand for years. If forward compatibility is ever wanted it
  needs a *named* allowance (an allowlist of known-future sections), not a return to silence.
- **The error arrives at every command**, since all three (`scan`, `embed`, `search`) load config. A file that
  used to work-but-lie now stops the command — which is the point, and why the message has to be good enough to
  act on: it names the key as written and the line it sits on.

## 4. Tests, and what each one can lose

1. **Top-level stray key refused** (`stray_top_level = 1`): the load fails with `Parse`, the message contains the
   file path and `` `stray_top_level` ``. Mutation: dropping `deny_unknown_fields` from `Config`.
2. **Invented section refused** (`[catalogg]`): same shape, naming `` `catalogg` `` — the card's actual case.
   This is the one that used to load clean and is the reason the round exists.
3. **`[catalog]` and `[markers]` stray keys refused**: two more containers, so a future struct cannot quietly
   rejoin the lenient set. Mutations: dropping the attribute from either struct.
4. **A valid file still loads** — every section present with real keys, asserting the values arrive. Without it,
   "refuse unknown" could be satisfied by a container that refuses *everything*, which is how the first attempt
   at a strict root would most plausibly fail.
5. **The rewritten fences.** `an_unknown_key_outside_embeddings_still_loads` becomes the top-level refusal case
   rather than being deleted, and `the_models_strictness_is_scoped_to_the_models_section` becomes
   "strictness no longer stops at a boundary": both keep their fixtures (same TOML bytes) and invert what they
   demand of them, so the file's history shows a stance reversed on purpose.

Catalog goldens are untouched by construction — config parsing decides where files go and how wide an entry is,
and no fixture here renders a catalog. `GOLDEN CHECK: pass` is re-run anyway because it is cheap and because
this repository treats a golden diff as a stop signal rather than a review note.

## 5. Out of scope

- **Value-level leniency.** An empty string still means unset for the path and origin keys (`NP_MODELS_BASE_URL=`
  etc.), which is a separate documented rule with its own tests; this round changes *which keys are legal*, not
  what a legal value means.
- **Environment variables.** `NP_*` names are not part of the file's key space; an unset-looking one already
  refuses by name where it matters (§12 rule 7).
- **A suggestion list for near-miss section names.** serde prints the legal field names for a struct, which is
  why `[catalogg]` reads as "unknown field `catalogg`, expected one of `data_dir`, `catalog`, …" — good enough
  to act on; building our own edit-distance advice would be a second thing to keep in agreement with the schema.

## 6. What was measured, not assumed

**Mutants (production edits, reverted after each).** `cargo test -p np-core --test config`, one attribute removed
at a time:

| Mutation | Killed by |
| --- | --- |
| `deny_unknown_fields` off `Config` | `a_stray_key_at_the_top_level_is_refused_naming_key_and_file`, `an_invented_section_is_refused_naming_the_section_and_its_line`, `strictness_no_longer_stops_at_a_section_boundary` (51 passed, 3 failed) |
| off `CatalogConfig` | `every_section_refuses_an_unknown_key`, `an_unknown_key_inside_a_section_is_the_one_named_when_the_root_has_one_too` (52/2) |
| off `MarkerConfig` | `every_section_refuses_an_unknown_key` — its `[markers]` row only, which is why that case holds two fixtures instead of one (53/1) |
| `read_config` refuses **every** file, valid or not | 42 of 54 cases, including `a_file_of_real_keys_still_loads_every_section` and every refusal case whose assertion names a specific key (12/42) |

The last row is the one that answers "could the new rule be satisfied by refusing everything?": no, because each
refusal case asserts *which* key is named and at *which line*, which a blanket refusal cannot produce. The
positive case is what keeps the opposite failure visible.

**Red before green.** With the tests written and the production change absent, four cases failed, and the one that
matters printed the card's own symptom: `the file must be refused; it loaded with catalog.max_entries_per_file =
600` — a config claiming 200 publishing the default.

**The shipped binary, on the card's exact scenario.** Built from this branch (`cargo build -p np-cli`, dev
profile) and run against a scratch workspace with `XDG_CONFIG_HOME` pointed at a temp tree:

```text
[paths]                       → np: configuration error: config parse error in …/config.toml:
data_dir = "…"                  TOML parse error at line 1, column 2
                                unknown field `paths`, expected one of `data_dir`, `catalog`,
                                                              `markers`, `embeddings`, `models`
                                                                      exit=1
stray_top_level = 1           → … parse error at line 3, column 2
[catalogg]                      unknown field `catalogg`, expected one of …   exit=1
max_entries_per_file = 200
(data_dir on line 2)

all keys real                 → scan completes: skipped 0 / index INDEX.md / elapsed 89 ms   exit=0
```

The middle case is §2's ordering rule in the wild: `stray_top_level` sits on line 1 and goes unnamed while
`[catalogg]` on line 3 gets named. And `[paths]` — the section this card was filed about, which its author had
been carrying in a scratch config for several measured runs — is now the first thing the binary complains about,
naming the legal section names so the fix needs no documentation.
