# `np serve` — a stdio JSON-RPC surface for a frontend

**Status:** design, 2026-12-08. Approach chosen by the operator (option C of three) when asked what a frontend should talk to.
**Companion:** `docs/roadmap-2026-10-27.md` puts the daemon, loopback HTTP API, watcher and autostart in **M4**; this document is deliberately
smaller than M4 and does not pre-implement it.

## Intent and the why

A frontend needs to call `np` without owning a shell. Today the only interface is argv: spawn a process per question, parse stdout as one JSON
document, read an exit code. That works — F44 made concurrent invocations honest — but it costs ~50–100 ms of process start per request on
Windows, and every model load re-reads a 300 MB artifact, because a fresh process has nothing cached. The operator wants to start a frontend
without first building a daemon, an HTTP stack, an auth story or a port to firewall.

So: **one long-lived `np` process that speaks JSON-RPC 2.0 over its standard streams**, inside the existing binary, reusing the exact documents
`--json` already prints. No new transport, no listener, no credentials, no new state on disk.

## Goals

- Ask the same questions the CLI asks and get **byte-identical JSON** for them — one implementation behind both channels, so a screen and a
  terminal can never disagree about what `cosine` or `locked` meant.
- Survive being the only thing a UI talks to for hours: no per-request model reload, no per-request database open, clean shutdown on EOF.
- Keep every existing contract that was paid for in blood: stdout carries protocol bytes and nothing else; states (`locked`, `offline`, `fetched`,
  `prefix_drift`) travel inside results rather than as error strings; no host or URL in any output (§12 rule 8).
- Fail like a program, not like a crash: a bad request gets an error object and the server keeps serving.

## Non-goals

- **No HTTP, no socket, no port.** That is M4's decision with its own auth/loopback design; adding one here would make this the security surface
  nobody reviewed. The dispatcher is written so an HTTP adapter can call it later without changing what a request means.
- **No watcher, no scheduler, no autostart** (M4), no multi-vault session management, no streaming search results.
- **No job queue.** Mutating requests run inline and to completion (see "One request at a time").
- **No new commands.** `serve` exposes existing behaviour; if a frontend needs a verb that does not exist, that is a separate card.

## Constraints this design did not choose

- `np-cli` handlers currently *print*. Reusing their output means separating "produce the document" from "write it to stdout", and doing it in one
  direction only: the CLI keeps its exact bytes, the server adopts them.
- The write lock (**F44**) is held by a process for the life of a mutating command. A server that holds it across two requests would refuse its
  own next request; a second `np serve` pointed at the same vault must therefore behave exactly like a second `np scan` — wait, then report
  `locked`.
- llama.cpp writes its load chatter to stderr (**F33**), which is precisely where protocol bytes must not go. This is the one place where an
  existing contract does most of the design work.
- The inference runtime stays behind `local-model`, fetching behind `fetch`. A build without them must still serve, and answer with the same
  refusals the CLI prints.

## Approaches (as presented, and what was picked)

1. **Subprocess backend.** Frontend server spawns `np --json` per request. Cheapest to build; costs process start plus a model load per request,
   and the caller owns a job queue for minute-long embeds. Rejected: it is the thing this design exists to replace, and its cost lands on every
   search rather than once.
2. **M4's loopback daemon now.** Matches the roadmap, designs concurrency/staleness/autostart once. Rejected *for now*: it needs auth semantics,
   a port policy and an install story — weeks, and a spec of its own — before a screen can show one note.
3. **`np serve`, JSON-RPC over stdio** — **chosen.** One process, no new surface, documents already exist, hours not weeks; the frontend wraps it
   in whatever transport it wants (a desktop shell holds the child; a local web server holds the child and speaks HTTP to the browser).

## The protocol

### Framing and lifecycle

- **Newline-delimited JSON**, one object per line, both directions. A response is never split across lines and never contains raw control
  characters (`serde_json` escapes newlines inside strings), so `readLine()` is a complete frame. Chosen over LSP-style
  `Content-Length` headers because there is no second protocol on the stream and a line is trivially testable from a shell.
- `np serve [workspace]` starts, prints nothing, and waits. A request with `"id"` gets exactly one response with that `id`. A request **without**
  `id` is a notification: no response, ever — including for malformed input, which is dropped with a single stderr line (JSON-RPC 2.0 §5).
- EOF on stdin ends the server after finishing the in-flight request; exit code **0**, because "the client closed the pipe" is not a failure.
  `SIGINT`/`SIGTERM` behave the same way they already do for a running command: the process stops, and F44's lock dies with it (no stale locks —
  that property is why this surface can be killed without a shutdown handshake being mandatory).
- Optional flag mirroring the CLI: `--json` is *implied* (the protocol is JSON); there is no text mode, because stdout is reserved.

### `initialize`, and what a client learns

```jsonc
→ {"jsonrpc":"2.0","id":1,"method":"initialize","params":{"client":"my-ui","protocol":1}}
← {"jsonrpc":"2.0","id":1,"result":{
     "np":"0.3.0","protocol":1,
     "features":{"local_model":true,"fetch":true,"vision":false},
     "methods":["initialize","search","scan","embed","models.list","shutdown"],
     "workspaces":1}}
```

`features` is the compile-time truth (the same facts F40 refuses on), so a frontend can decide at startup whether to render an "embed" button at
all rather than discovering it through a refusal. `protocol` exists so a future incompatible change is a number rather than a mystery — and because
a client that never sends `initialize` still works: **no method requires the handshake**, since every request names its own workspace anyway.

### Requests carry their own workspace

Every method takes `"workspace"` as an absolute path, exactly as argv does today. No session state, no "current vault": a server holding three
vaults is one process, and a stale "which vault am I on" is the kind of state that produces a wrong answer rather than an error. A workspace that
does not exist is refused **before anything is created** (§3 step 0 — the same rule F44 had to obey when it added a lock file).

```jsonc
→ {"jsonrpc":"2.0","id":2,"method":"search","params":{"workspace":"D:/vault","query":"broker migration","limit":5,"mode":"hybrid"}}
← {"jsonrpc":"2.0","id":2,"result":{"hits":[…],"mode":"hybrid","vectors":true,"note":null,"reason":null,"fetched":null,"offline":false}}
```

`result` **is** the object `np search --json` prints, unchanged — including `reason`, `note`, `fetched`, `offline` and per-hit `cosine`/`bm25`. The
same holds for `scan` (its report), `embed` (its report, with `prefix_drift` when F42's notice applies) and `models.list` (its array document).

### One request at a time, and what that costs

Requests are handled **sequentially, in arrival order**. An `embed` of a big vault runs for minutes and a search queued behind it waits. That is
a real cost, stated rather than hidden, and the mitigation is the one F44 already made safe: **run a second `np serve`** (or a plain `np search`)
for reads while an embed runs — readers never take the write lock, which is exactly what Stage 13 measures on Windows. A job-id protocol
(`embed` → `{job}` + `job/progress` notifications) is the obvious next step and is *not* in this design: it changes response shapes for every
mutating method and deserves its own decision.

### Errors, states, and who owns which

- **Protocol faults** (unknown method, bad params, unparseable line, workspace not a directory) → JSON-RPC `"error"` with codes reserved as:
  `-32700` parse, `-32601` method not found, `-32602` invalid params, and `-32000` for "np refused before doing work" (argv/config/workspace
  refusals). The `"message"` is the same sentence `np:` prints on stderr today — one string per state, so a UI can show it verbatim.
- **States are not errors.** A locked workspace (`locked:true`), an offline fetch (`offline:true`), or a degraded hybrid search (`reason`) all come
  back under `"result"` with exit-code-free success, because they are answers about the world and the caller branches on a field, never on prose.
  This is F38/F44/F45's rule carried across: an app must not pattern-match a sentence to learn that the network was down.
- A panic inside a handler is caught at the request boundary (`catch_unwind`), reported as `-32603 internal error`, and **the server keeps
  running**. One bad note must not end a session; today a panicking CLI command ends the process, which is correct there and fatal here.

### Security, stated because stdio is not nothing

The permission check is the operating system's: whoever can write to this process's stdin already owns the user's files, so there is no
authentication to add and no reason to invent one. What follows instead is that the surface must not *widen* what an attacker with that access can
do — so `serve` exposes exactly the existing commands, takes no path outside the caller's own filesystem, never listens on a port, and never
interprets a string from the wire as a command. If anyone later fronts it with HTTP, **that** layer is where origin checks and auth belong, and its
author should re-read this section rather than assume stdio's assumptions travelled.

## What the refactor touches

`scan_workspace`, `embed_workspace`, `search_workspace`, `models_list` and friends print today. Each gains a pure "produce the document" function
that the CLI then prints through its existing locked-stdout handle, so:

- the CLI's bytes are unchanged (existing tests pin them and must stay green untouched), and
- `serve` cannot invent a second vocabulary for the same state — the property that makes this worth doing rather than merely convenient.

Refusals travel as a typed value carrying the sentence plus, where one exists, the machine-readable field (`locked`, `offline`), not as a
`String` error to be re-parsed.

## Open questions left to the builder

- Whether `initialize` should also report the resolved data directory (useful for support screens; risks naming a path in logs — §12 rule 8 is
  about hosts, not paths, but say which you chose and why).
- Whether to expose `setup` as one method (it composes scan + embed and its document already exists) or leave composition to the client.
- Line length: a `scan` report on a 40 000-note vault is one long line. If any client library chokes, the fix is a size cap *plus* an error, not
  pretty-printing across lines (which would break framing).

---

# Builder check and as-built record (same day)

**The independent read did not happen, and this section is how that was compensated for.** Two reviewer agents were dispatched to critique this
document before implementation; both went idle without producing a report (one timed out at 30 minutes, the second was interrupted). The promised
independent check was therefore done here, against code rather than opinion — every answer below cites an implementation fact, and each of the four
"what could go wrong" claims was converted into a test and then killed with a mutant, which is stronger evidence than a reader's reassurance. What it
cannot catch is a design-level blind spot; that risk stays open and belongs to whoever reviews this surface next.

## The four questions, answered from the tree

1. **Can any document break "one object per line"?** Yes, in principle: `snippet` is a window onto note text and holds raw line breaks today (it
   did in the very first test run: `"snippet":"\n[quuxfizz] content…\n"`). serde_json escapes control characters inside strings, so the break travels
   as `\n` data. That mechanism is now asserted rather than assumed by
   `crates/np-cli/tests/serve.rs:a_snippet_spanning_a_line_break_still_fits_on_one_response_line`, which writes a note whose matched text spans a
   newline and holds a tab, quote, backslash and CJK characters, then requires one response per request. Mutant: a `println!` of prose into stdout
   fails **all eleven** cases in that file, so framing cannot rot quietly.
2. **Is byte identity achievable without extraction?** No — and the extraction was smaller than feared. `scan` (`serde_json::to_value(report)`),
   `models.list` (`listing_json`) and `embed` (`embed_report` + `insert_drift`) were already reachable; only search interleaved work with printing,
   so it gained `SearchRun` (the facts) + `search_run` (does the work) + `search_document` (builds the object), with `search_workspace` left as a
   printer. All 47 existing CLI cases stayed green untouched, which is the evidence that the CLI's bytes did not move.
3. **Do the state keys match what the document claimed?** Yes, verified against the tree: search carries
   `fetched, hits, mode, note, offline, reason, vectors`; contention carries `command, locked, waited_ms` (**F44**); embed adds `prefix_drift` only
   when drifting (**F42**). One correction to §"Open questions": a search hit has **no `title` key** — hits carry `path`, `snippet`, ranks and link
   counts. A first draft of the framing test reached for `hit["title"]` and found nothing, which is worth writing down before someone designs a
   result list around a field that does not exist.
4. **What does the workspace lock do inside a long-lived process?** Taken and released **inside each mutating request**. A server that held it for
   its lifetime would refuse its own next command after `DEFAULT_WAIT`, i.e. F44 working correctly against the wrong caller. `LockGrant::Contended`
   exists so contention returns the same document the CLI prints under contention — a JSON-RPC *fault* there would send an app's generic error
   handler off retrying something it should simply re-queue. Mutant: rendering contention as a fault fails exactly
   `a_contended_workspace_is_a_result_and_the_server_keeps_answering`, which also proves readers still answer while a writer holds the workspace and
   that the next write succeeds once it is released.

## Decisions taken where the design left a question

- **`initialize` reports `data_dir`.** A support screen needs the line "where is this index", §12 rule 8 forbids naming an *origin*, and this is a
  path on the caller's own machine — there is no secret in it that they do not already hold.
- **`setup` is not a method.** Its document composes scan + embed, and `embed` already reports every field `vectors/offline/fetched/reason`; a
  client that wants setup sends two requests. One fewer method to keep honest, and the composition is two lines of client code with no verdict logic
  in it. Revisit if a real client writes that composition badly.
- **No size cap on responses.** A 40 000-note `scan` report is one long line; the failure mode being protected against (a client that truncates) is
  the client's framing bug, and answering it by pretty-printing across lines would corrupt every other response. Revisit with a measurement if a
  client actually chokes.
- **Parse errors are answered even when no id is readable** (`id: null`, `-32700`), per JSON-RPC 2.0 §5. The tempting alternative — "no id means it
  was a notification, stay silent" — is unsound in the direction that matters: a client whose buffer was cut mid-request *did* send an id and would
  wait forever. A **blank** line is the one input ignored without a reply; it is a keepalive, not a failed request.

## Method list, deliberately small

`initialize`, `search`, `scan`, `embed`, `models.list`, `shutdown`. Not exposed: `models install`, `models fetch`, `setup`, `models hash`. The two
model-transfer commands involve a consent step designed for a human at a terminal (F38 announces size and origin kind before byte one), and an app
that wants them can run the CLI it already shipped. Keeping them out is what lets this surface hold the "no surprise transfers" promise without a
second consent mechanism to get wrong.

## Where each claim's evidence lives

| Claim | Evidence | Killed by this mutant |
| --- | --- | --- |
| stdout carries only protocol bytes | `tests/serve.rs`: 11 cases through a real pipe (plus 10 unit cases in `src/serve.rs`; the commit message said "twenty" and miscounted by one — the totals here are from `cargo test`, not from prose) | prose `println!` into stdout → 11/11 fail |
| `result` equals the CLI document | `a_search_response_is_the_same_document_np_search_json_prints`, `a_models_list_response_is_the_same_document_…` | extra key in the served document → identity case fails |
| states are results, not faults | `a_contended_workspace_is_a_result_and_the_server_keeps_answering` | contention rendered as a refusal → that case fails |
| notifications run and stay silent | `a_notification_runs_and_answers_nothing` (checks `INDEX.md` on disk *and* the line count) | answering notifications → that case fails |
| a refusal touches no files (§3 step 0) | `a_workspace_that_is_not_a_directory_refuses_without_creating_anything` | `create_dir_all` before the refusal → that case fails |
| EOF is a clean exit | `closing_stdin_ends_the_session_successfully` asserts a zero wait status | — (no mutant run: an `exit(1)` there is caught by the same assertion) |

**One limit stated plainly.** Deleting a key from `search_document` leaves the identity cases **green**, because both channels lose it together;
that schema is pinned by `crates/np-cli/tests/cli.rs`, which asserts the whole seven-key set. The two kinds of test guard different failures and
neither substitutes for the other.

## Accidents during this work, recorded because the rule matters

A mutation harness restored a file with `git checkout -- crates/np-cli/src/main.rs` while that file held an hour of **uncommitted** refactor work, and
destroyed it. Rebuilt from the same edit scripts; the rebuild is what these tests ran against. The rule going forward: **snapshot files with `cp` in
mutation harnesses — never restore with git while uncommitted work exists in the tree.**
