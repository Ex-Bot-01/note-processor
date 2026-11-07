# F38 — vectors that simply exist: `np setup`, and an opt-in automatic fetch

Status: **short design, awaiting approval.** Ruled by the human on 2026-11-08: *"I mean your app calling `np` and wanting vectors to
simply exist."* That sentence is the requirement this document is written against — the consumer is a program, not a person at a
terminal, so every promise below is a machine-readable one.

## 1. Intent, and why

Today an application that wants vectors has to orchestrate: check whether the artifact exists, run `np models fetch`, run `np scan`,
run `np embed`, and interpret three different refusal sentences to work out which step it was allowed to skip. That is our control
flow living inside somebody else's program. The ask is to move it home: **one command that guarantees vectors**, plus an opt-in so the
artifact appears by itself when a command needs it — without ever turning a network call into a side effect of reading a config file.

## 2. Goals, non-goals, anti-goals

**Goals**

* `np setup <workspace>` — one command that ends with vectors present, or fails naming the step that stopped it.
* `[models] auto_fetch` defaulting to **true** — `np embed` and `np setup` on a machine with nothing installed fetch the pinned
  artifact and carry on in the same invocation. The human ruled this on 2026-11-08 (*"I would [have] the default as true for the
  huggingface model"*), after being told plainly that it means a first `np embed` transfers ~310 MB without being asked twice.
* A report an app can act on: what was fetched, how many bytes, whether the digest verified, and which origin kind served it — without
  parsing prose.
* A kill switch that works without a config file: `--no-auto-fetch` and `NP_NO_AUTO_FETCH=1`, for air-gapped machines, metered links
  and CI. With either set, every pre-F38 refusal comes back **word for word** at its old exit code.

**Non-goals**

* No daemon, no scheduler, no background refresh (that is M4, unchanged).
* No automatic projector: a build without `vision` never fetches 554,821,056 bytes it cannot use.
* No new network surface. `fetch` remains the only dial-out path (§12 rule 1), with its pin, its marker and its refuse-to-overwrite rule.

**Anti-goals** — the things that would make this a failure even if it technically works

* A dial-out for anything the build does not pin. Default-on is defensible precisely because the surface is not "the internet": today
  exactly one id (`embeddinggemma-2`) has pinned bytes, from one compiled-in origin, verified by digest before it is published.
* A **read** command downloading anything. `np search` never fetches (§5).
* An app unable to tell "vectors were already fine" from "this run downloaded 310 MB".
* A fetched artifact accepted without digest verification, or a URL printed where §12 rule 8 says only the *kind* of origin may appear.

## 3. Constraints already in force

Digest pinning per model id (§12 rules 6–7) · fetch marker means liveness is consent (rule 4) · publish into an absent target only
(rule 5) · origins are never printed (rule 8) · stdout carries exactly one JSON document · exit 0 success, 1 runtime refusal, 2 argv ·
config is strict, so a new key must be refused when misspelled (**F35**) · **F34 is closed (2026-11-05, `9de3b2d`)**: Windows answers
rule 6 from a hand-declared `GetDiskFreeSpaceExW`, so an automatic download on that platform is *checked* for free space like any other.
An earlier draft of this document said otherwise, having read the card's title instead of its status — corrected here rather than left in,
because a safety argument built on a stale premise is not a safety argument.

## 4. The approaches, and which was chosen

| | Shape | Trades off |
| --- | --- | --- |
| **A — opt-in key** | `[models] auto_fetch = true`; provider resolution fetches a missing pinned artifact before refusing | fully automatic once declared; needs guardrails so it never fires where it cannot succeed |
| **B — prompt on first need** | ask when a TTY is attached | good for people, wrong inside `np embed` mid-batch, and useless to a non-interactive caller |
| **C — one command** | `np setup <ws>` = ensure artifact → scan → embed | nothing automatic; does not by itself serve an app that wants vectors to exist between calls |

**Chosen: A as the core, C as the human-facing path, B folded into C's confirmation prompt.** The reason is the ruling above — the
consumer is an app, so the contract has to be non-interactive and self-describing; `np setup` exists because a person should not have
to remember the sequence either.

## 5. Design

**Where auto-fetch may fire — and where it may not.** Only inside provider resolution for the commands that **write vectors**: `embed`
and `setup`. It runs before any transaction opens, never in the embed batch loop. It fires when `[embeddings] model` names an id this
build **pins** and either the `models` row or the artifact file is absent.

Three states must not trigger it:

* **`RuntimeNotBuilt`** — a binary compiled without inference would download 310 MB it can never read; keep today's refusal and say that
  fetching would not help here.
* **`np search`**, in any mode. Search is a read, F36 made it answer without vectors, and "your query took 4 minutes because it fetched
  a model" on a metered laptop is the surprise this document exists to avoid. *My call, stated so you can overturn it:* if you want search
  to self-heal too, that is one more command in the trigger list and everything else here still holds.
* **Any id without pinned bytes.** No URL, no host, no mirror becomes a download target through config alone; `[models] base_url` changes
  *where a pinned id's bytes come from*, never *whether an unpinned id may be fetched*.

**Consent.** `auto_fetch` defaults to **true**, is refused when misspelled like every other key (**F35**), and is switched off per run by
`--no-auto-fetch` or for the whole process by `NP_NO_AUTO_FETCH=1`. When it fires, the first thing printed (stderr, before byte one) is what
`np models fetch` prints today: id, member names, expected bytes, origin *kind*. Nothing about a host or URL appears in any report (§12 rule 8).

**Hermeticity, which default-on puts on the line.** The suite must never reach a network. It cannot by construction: every fixture uses an
id with no pinned bytes (`fixture@1`, `test-model`), and unpinned ids are refused before a transport is chosen — but that becomes a *load-bearing
invariant* the moment the default flips, so it gets its own test rather than staying an accident, and CI exports `NP_NO_AUTO_FETCH=1` as a
second lock. The fetch tests keep using their injected transport.

**Offline: skip the fetch, never fake the result.** Default-on means the first run on an air-gapped or dead-Wi-Fi machine hits a wall, and
the ruling is to walk around it rather than stop at it. The rule that makes this honest is that **we never ask "is there internet"** — there
is no probe, no captive-portal guess, no separate connectivity command. We attempt the one fetch we needed anyway (which only happens when the
artifact is *already* absent, so a machine that has its model pays nothing) and classify **why it failed**, from the classes `fetch.rs` already
derives:

| why the fetch failed | class today | treated as |
| --- | --- | --- |
| name would not resolve, connection refused, network unreachable, timed out, generic transport failure | `could not resolve the configured host`, `timed out`, `transport failure` | **offline** — skip the fetch and carry on |
| origin answered 404 / 401-403 / 5xx | `not found`, `refused by the server…`, `server error` | a real error: refuse loudly, because the origin *is* reachable and something is wrong |
| redirect to another host | `redirect refused` | a real error — a different origin than config named |
| digest mismatch after bytes arrived | integrity refusal | the loudest refusal of all. A machine that reached the origin and got wrong bytes is not "offline", and calling it that would hide an attack |

**TLS failures are deliberately *not* offline.** A captive portal does produce TLS errors, but so does a hostile network, and "silently skip
because TLS failed" lets anyone on the path suppress vector creation. Refuse, name the class, let the human decide. *My call; overturn it if you
disagree.*

**What each command does when offline with no artifact:**

* `np embed` — **skips the fetch and exits 1**, saying nothing was embedded because the pinned origin could not be reached. It must not exit 0:
  a caller that asked for vectors and got none seeing success is precisely how an application ends up believing it has embeddings.
* `np setup` — **does its useful half and exits 0**: scan runs, index updates, keyword search works the moment scan finishes (F36), and the
  report says `vectors: false, offline: true` with a summary line reading *indexed; vectors skipped — offline*. Setup's contract is "get as far
  as you can and tell me exactly where you are", which is what an app needs to branch on.
* `np search` — unchanged, answers from keywords, never fetches.

Offline therefore degrades the product into the thing it already was before M2: an indexer with exact-word search, no half-built state, no
claim it cannot support. Nothing is stamped, published or counted while the artifact is missing, so re-running once a connection returns starts
from a clean slate.

**Interactive courtesy.** `np setup` asks once before transferring when a terminal is attached (`[y/N]`, with the byte count), and never
asks when one is not — an app gets no prompt to answer, only the report. `--yes` skips the question; `NP_NO_AUTO_FETCH=1` means there is
nothing to ask about.

**The machine contract.** `embed --json` and `setup --json` gain one key, present-and-null in every run:

```json
{ "fetched": { "id": "embeddinggemma-2",
               "members": [ { "name": "main", "bytes": 309855456, "digest_verified": true } ],
               "origin": "compiled-in-pin" },
  "…existing keys…" : "unchanged" }
```

`null` means nothing was downloaded this run — the difference an app needs in one field. `setup --json` also carries `scan` and `embed`
sub-reports so a caller can tell which step produced which counts without running three commands.

**Failure semantics.** Auto-fetch failure is reported with `fetch`'s own sentence, unchanged, at exit 1; the embed that could not proceed
is then *not* attempted, so no partial state is written and no row is stamped.

## 6. Left to the builder

Exact wording of the setup summary lines; whether `setup` prints per-step elapsed times; the injected-transport seam used to test the
fetch path without a network (the existing fetch tests already have one — reuse it). None of these change what gets built.

## 7. What must be true before this can be called done

* With `NP_NO_AUTO_FETCH=1`, every pre-F38 refusal test still passes **unchanged**, sentence and exit code alike — this is the first gate,
  not a footnote: it is the proof that turning the default on did not delete the old behaviour, only reordered it.
* An id with no pinned bytes never triggers a fetch, asserted as its own test so hermeticity stays an invariant and not a coincidence.
* Offline is classified from the failure, not probed: an injected transport that fails with `could not resolve the configured host` makes
  `setup` finish at exit 0 with `vectors:false, offline:true`, while the same transport failing `not found` or a digest mismatch still refuses
  at exit 1 — the two must not collapse into one outcome.
* Nothing is written when the artifact stays absent: no rows stamped, no `models` row created, no partial publish.
* A pinned id with no row and no artifact, key on: one fetch, once, and the embed completes in the same invocation.
* A build without inference never fetches, and says that fetching would not help.
* `fetched` present in every `--json` document, `null` when nothing moved; digest verification reported per member.
* No origin string in any report or log line (asserted, not promised).
* The amendment to **F36 §7** ("nothing dials out because a config key named a model") is recorded where that ruling lives, so the two
  documents cannot tell different stories — and README's "the only command that can dial out" paragraph gains the sentence this ruling
  implies: *`np embed` may fetch ~310 MB on first use; here is how to stop it.*
