# `np serve` — the stdio interface

`np serve` turns the same binary into a long-lived backend. It speaks
[JSON-RPC 2.0](https://www.jsonrpc.org/specification) over **NDJSON**: one request per line on stdin, one response per line on
stdout. No port, no socket, no auth, no file watcher — those are other people's problems, and keeping them out is what makes
the interface safe to spawn from anything.

Everything here describes the shipped protocol **version 1**, which is what `initialize` reports. Progress notifications were
added inside version 1 because they are opt-in and cannot surprise a client that did not ask (see
[Notifications](#notifications)).

## Contents

- [Framing and lifecycle](#framing-and-lifecycle)
- [Methods](#methods)
- [Reading the link graph](#reading-the-link-graph)
- [States travel as data, not as errors](#states-travel-as-data-not-as-errors)
- [Fault codes](#fault-codes)
- [Notifications](#notifications)
- [`status.get`, and what it cannot see](#statusget-and-what-it-cannot-see)
- [Captured transcripts](#captured-transcripts)
- [Client walkthrough](#client-walkthrough)
- [Reproducing the transcripts](#reproducing-the-transcripts)

## Framing and lifecycle

| | |
|---|---|
| Transport | child process, stdin/stdout pipes |
| Request | one UTF-8 JSON object per line, no embedded newlines |
| Response | one UTF-8 JSON object per line, terminated by `\n` |
| Startup | `np serve` — no arguments, nothing read from the terminal |
| Shutdown | close stdin (or send a [`shutdown`](#methods) request); exit code 0 |

**stdout is the protocol.** Nothing else is ever written there: logs, warnings and panics go to stderr, and a line that is not
valid JSON means the process broke its contract, not that you should parse harder. stdout is locked for the life of the loop so
two writers cannot interleave a response in half.

**Requests are handled one at a time, in order.** A search queued behind an embed waits for the embed. That is the documented
cost of a single-threaded server; it is also why readers stay cheap — a search does not take the workspace write lock, so it
does not queue behind another *process*, only behind this connection's own work.

**A blank line is ignored.** A line that is not valid JSON, or not an object, gets a parse or invalid-request fault with
`id: null`, because there may be no id to echo back.

**Diagnostics say which request caused them.** When diagnostics are switched on (`-v`, `NP_LOG`, `[log] level`; see
[`docs/reference-diagnostics.md`](reference-diagnostics.md)) each request opens a span, and every record raised while that
span is open carries **`request`** — the JSON-RPC id — plus **`workspace`** when the work is about one. A notification has
no id, so its records carry no `request` key at all: never `null`, never `"none"`, because "this line belongs to no
request" and "this request had a null id" are different facts an operator would otherwise have to guess between. The four
phases (`scan`, `embed`, `search`, `fetch`) close with an `info` record carrying **`elapsed_ms`**, so a slow request can be
attributed to a phase from the log alone.

**Reading stderr is optional; your requests do not depend on it.** A client that pipes stderr and never drains it is
normal, and np treats that as "nobody is listening": diagnostics are dropped rather than blocking the request loop, up to
1 000 of them counted in the closing notice. Measured before that was the rule — 1 200 requests against a server whose
stderr nobody read, **1 057 answered**, the child stalled inside `write(2)` once the pipe filled at about 64 KiB; now 1
200 in, 1 200 out. Windows has no `poll`, so there an undrained stderr can still slow the loop: read it, discard it, or
point `[log] file` somewhere else and leave the pipe alone.

**A refused configuration does not end the session.** An unparseable `config.toml` is reported as a diagnostic on the
channel the file itself configured and answered as a fault on every request; the loop keeps reading, because a client that
can talk to a server it cannot configure still deserves answers.

## Methods

`initialize` reports the authoritative list; `protocol` stays `1`.

| Method | Params | Result | Built by | Refusals |
|---|---|---|---|---|
| `initialize` | `workspace?`, `notifications?` | `{protocol, np, features, methods, data_dir, progress?}` | its own builder | `-32602` on a non-boolean `notifications` |
| `search` | `workspace`, `query`, `limit?`, `mode?` (`keyword`\|`semantic`\|`hybrid`) | the same document as `np search --json` | `search_document` | `-32602` missing/unknown key; `-32000` workspace unreadable |
| `scan` | `workspace` | the same report as `np scan --json` | `ScanReport` | `-32602`; `-32000` |
| `embed` | `workspace` | the same report as `np embed --json` | `embed_report` | `-32602`; `-32000` (including a build without the inference feature) |
| `models.list` | `workspace` | `{models, fetching, unclaimed, pinned, models_dir_error}` — installed-artifact rows, any fetch in flight, and what a fetch **would** cost before one starts | `listing_json`, the CLI's own builder | `-32602` |
| [`links.list`](#reading-the-link-graph) | `workspace`, `rel?` | `{indexed, edges, unresolved}` — who links each note, and every dead target with the file and line it was written on | `links_json` | `-32602` on an empty or non-string `rel`; `-32000` when the workspace is not a directory |
| `status.get` | `workspace` | `{last, fetch_markers, running}` | its own builder | `-32602` |
| `shutdown` | none | `{protocol}` echoed, then the loop ends | its own builder | — |

Two things are deliberately **not** methods:

- **`setup`.** A client composes it: `scan`, then `embed`. There is no combined state machine to keep honest.
- **Model installation and fetching.** F38's consent step is a human at a terminal reading "this will download 295 MiB". A
  backend that could silently trigger that download would delete the only place that decision lives. Use `np models install`
  or `np models fetch` out of band, or let `embed`/`search` auto-fetch under the usual rules (config `[models] auto_fetch`,
  `NP_NO_AUTO_FETCH=1`).

`initialize` also reports `data_dir` — where artifacts and per-workspace databases live — because a client that wants to warn
"this vault needs 295 MiB" needs to know which disk it means.

## Reading the link graph

`np` computes every wikilink, markdown link and figure embed in a vault on every scan. **`links.list`** is the read of it
(**issue #2**), and it answers three features of one editor with one document: a backlinks pane, a fix-at-source marker on
an unresolved `[[target]]`, and the rewrite pass that has to find every link pointing at a note before it is renamed.

```json
{"id":3,"jsonrpc":"2.0","result":{"edges":[{"dst":"todo.md","kind":"wiki","line":3,"src":"meeting.md","target":"todo.md"},{"dst":"meeting.md","kind":"wiki","line":3,"src":"todo.md","target":"meeting"}],"indexed":true,"unresolved":[{"kind":"wiki","line":5,"src":"meeting.md","target":"someday"}]}}
```

Captured from a two-note vault — `meeting.md` links `[[todo.md]]` and `[[someday]]`, `todo.md` links back to `[[meeting]]`.
The second edge is the interesting one: its target is spelled `meeting` and its destination is `meeting.md`, which is
exactly why this read exists. A client that resolved that itself would be maintaining a second wikilink grammar, and np's
rule (exact path, then case-folded path, then unique basename, and **nothing** when two notes claim one name) is not a rule
two implementations agree on by accident.

| Key | Meaning |
|---|---|
| `edges` | links that land on a live note: `src`, `dst`, `kind` (`wiki` \| `md`), the `target` as its author wrote it, and the 1-based `line` |
| `unresolved` | links no live note answers — never placed, ambiguous, or pointing at a note whose file is gone. **No `dst`:** the destination is the thing in question, and a path beside a dead target looks like somewhere to click |
| `indexed` | whether anybody has ever scanned this folder. Two empty lists on a vault that has never been scanned is not the finding "nothing links here", and a rename pass that reads it as one orphans every note in the vault |

`params.rel` scopes to one note and means **"this note is either end of the edge"** — not "incoming". A backlinks pane and
a fix-at-source marker need two different filters, and one parameter silently meaning one thing for `edges` and another for
`unresolved` is how a pane lists a note as its own backlink; every entry names both ends, so the client says which one it
wanted by comparing `dst` to the path it asked about. An unknown note answers two empty lists (a fact about the folder);
an empty or non-string `rel` is a `-32602` (a client that meant something and sent the wrong thing).

What is **not** in the document, stated rather than hidden:

- **Figure embeds.** A figure's stored target is relative to the linking note's directory and arrives as text that cannot
  hold every file name, so this table cannot say whether an image exists or where it moved. "Which notes embed this
  figure" is answered by a search hit's `linked_notes`, which re-runs the one resolution rule that can.
- **A source whose file is gone.** Those rows are history the database keeps, and `np scan --json` still counts them in its
  unresolved number; a pane a person clicks must not name a file they cannot open. The two counts may therefore disagree on
  a vault with churn in it, which is the correct disagreement — one counts what was written, this lists what can be opened.
- **`approved` edges**, because every reader of this table reads prose only and nothing writes the other kind yet.

It is a reader in the strong sense: no write lock, and **no database creation** — `status.get`'s rule of testing for the
file before opening it, so a pane that polls cannot mint an index for every folder it hovers over. It answers while an
embed holds the workspace. The identity rules a client may rely on, and why each was drawn where it was, are in
[**F48**](design-f48-links-as-a-query.md).

## States travel as data, not as errors

This is the rule that makes the interface usable: **a state is not a failure.** Offline is a state. Contention is a state. An
embed that refused to write because the budget ran out is a state. They arrive in the *result* document, exactly as
`np --json` puts them on stdout, and they never appear in `error`.

| Key | Meaning | Appears on |
|---|---|---|
| `locked: true`, `waited_ms` | another process holds the workspace write lock; nothing was changed | `scan`, `embed` results |
| `offline: true`, `reason` | the network was tried and failed, so no artifact could be fetched | `search`, `embed` results |
| `reason` | a machine token (`provider-disabled`, `local-model-not-built`, `offline`, …) explaining a degraded result. **One token per run**, which is why "not indexed" got its own key below rather than a new token here | `search`, `embed` results |
| `vectors: false` | this workspace holds no vectors — the truth about the index, and deliberately still `true` on a run that could not use them (**F45**), so it is *not* the answer to "which half ran" | `search` results |
| `keyword_used`, `semantic_used` | which halves **this answer** was built from. `mode` names what the caller asked for, and hybrid answers from one half whenever the other is missing; these two are what a client shows when it says how a result was found (**#5**) | `search` results |
| `indexed: false` | nobody has ever scanned this workspace — "this folder is not indexed", which is not the same sentence as "nothing here matches". One `scan` flips it; a search of an unindexed vault does not, because `np_core::scan::indexed` reads the scan stamp and the rows rather than asking whether a file exists | `search` results, their terminal `np/progress` line, and `links.list` (**#5**'s distinction, carried into the method that came after it) |
| `fetched` | what auto-fetch actually downloaded, or `null` | `search`, `embed` results |
| `prefix_drift` | the installed artifact's prefix documents differ from the pin's; absent when there is no drift | `embed` results |
| `outcome` | which arm an embed took (`{offline: …}`, `{local_model_missing: …}`, `over_budget`, …) | `embed` results |

A contention result looks like this, and the server keeps answering afterwards:

```json
{"id":4,"jsonrpc":"2.0","result":{"command":"embed","locked":true,"waited_ms":5011}}
```

## Fault codes

Only protocol-level faults and genuine refusals produce an `error` object.

| Code | Meaning |
|---|---|
| `-32700` | the line was not valid JSON (parse error) |
| `-32600` | the line parsed but was not a valid request object |
| `-32601` | no such method |
| `-32602` | `params` missing, wrong type, or containing an unknown key |
| `-32000` | refused: the command could not be done (`message` says why) |
| `-32603` | internal error, including a handler that panicked — one bad request never kills the session. This arm is a guard: no shipped method is known to panic, so **no test exercises it**, and a change that broke the catch would be caught by review rather than by this suite |

A notification (a request with no `id`) gets **no response line at all**, per JSON-RPC. That includes `np/progress`: the server
never answers its own notifications.

## Notifications

Progress is **opt-in per connection** and off by default, so a client built against v0.3.0 keeps the answers it expects: no line
arrives unsolicited, and every method's result document is unchanged. `initialize`'s own document is the exception, because it now
lists `status.get` in `methods` (it works whether or not you opt in) and adds `"progress": true` when asked:

```json
{"jsonrpc":"2.0","id":1,"method":"initialize","params":{"workspace":"/vault","notifications":true}}
```

The result then carries `"progress": true`. When that key is absent or false, no notification is ever written for that
connection — not one line, at any point.

`methods` does **not** change with the opt-in: it lists what the build accepts, and `status.get` works either way. The flag that
matters is `progress`, because it is the only key whose absence changes what the server sends unasked.

Every notification uses the same envelope, with **no `id` member at all** (not `null`: a client must be able to tell a
notification from a response by key presence):

```json
{"jsonrpc":"2.0","method":"np/progress","params":{"command":"embed","phase":"fetching","since_ms":61,"request":2}}
```

A request the server could not read has no command to name, so it ends without a line: `-32700` (unparseable) and `-32600`
(missing `jsonrpc` or `method`) answer their fault and stay silent otherwise. The invariant is *one terminal line per request
whose command was identified*, not one per line of input, and the difference matters only to a client that counts lines instead of
matching ids.

`params` keys that are **always** present:

| Key | Meaning |
|---|---|
| `command` | `scan`, `embed`, `search`, `models.list`, `status.get`, `initialize`, `shutdown` |
| `phase` | one of the phases below; exactly one line ends every request |
| `since_ms` | milliseconds since **this request** began, not since the connection opened |

and one that is present **only when the request had an `id`**:

| Key | Meaning |
|---|---|
| `request` | the `id` of the request this line is about — a client correlating two in-flight commands needs it, and inventing one (say, `"anon"`) for an id-less request would be a lie that later leaks into a UI |

### Phases

| Phase | Means | Counters carried |
|---|---|---|
| `locking` | waiting for the F44 workspace write lock | — |
| `resolving` | deciding which provider and artifact are needed; may lead to a fetch | — |
| `indexing` | the file walk (`scan`) | closing line: `files_seen`, `notes_seen` |
| `fetching` | bytes arriving for a pinned artifact | `member`, `bytes_done`, `bytes_total`, `members_done`, `members_total` |
| `embedding` | chunks being encoded | `chunks_done`, `embedded`, `chunks_total` |
| `writing` | vectors committed, **only when something was actually committed** | same as `embedding` |

`chunks_total` is the number of chunks this run decided it owed, counted before the writer loop starts — the run already holds that
list, so the total costs nothing. It is **fixed for the whole request** and never recomputed from what remains: chunks leave the owed
set as they are written, so a denominator derived from the remainder would make a progress bar reach 100 % while work was still
queued. A run that stopped early therefore ends with `chunks_done` < `chunks_total`, which is the honest reading of a partial
embed. Absent means the request never reached its writer loop (a refusal, an offline arm), and absence is *unknown*, not zero.
| `completed` | the request ended in a **result** — including degraded ones (`locked`, `offline`, an embed arm) | state keys copied from that result |
| `refused` | the request ended in an **error object** | `code` |

`search` reports `resolving` only when it can resolve something: keyword mode never resolves a provider and never claims to be
doing so, so absence carries information.

`scan` is **bookended**, not live — one `indexing` line when the walk starts, one when it ends with its counts, nothing in
between. A 4 000-note scan measures about 2.6 s; threading a callback through the hottest loop in the indexer to narrate two
seconds costs more than it tells.

### Byte counters are per member

A pinned artifact is a set of members (`main`, `mmproj`). `bytes_done` counts **within one member** and never runs backwards;
it restarts at 0 when the next member starts, while `members_done` steps up. So a two-member fetch legitimately shows
`bytes_done` falling — that is not corruption, and it is why monotonicity is asserted per `(request, member)`.

`bytes_total` is always the **pinned** size, never a transport-announced `Content-Length`: an origin that lies about its length
must not be able to move a progress bar's denominator. Percentages can therefore exceed 100 % when an origin under-announces;
prefer bytes plus `members_done`/`members_total`.

### Throttle

- A phase change emits immediately.
- Within one phase, at most one line per second.
- Bookend lines (`scan`'s closing line) are exempt — a pair that lands in the same second would otherwise lose its ending, and
  a client left with only the opening line believes work is still running.

### Terminal lines mirror the response

The last line for a request matches the kind of what went out:

- a **result** → `phase: "completed"`, plus whichever of `outcome`, `locked`, `offline`, `reason`, `vectors` the result carried;
- an **error object** → `phase: "refused"`, plus `code`.

So `{locked: true}` ends `completed`: it is a state, and a GUI that greyed out the button on `refused` would be reporting a
failure where nothing failed. The terminal line is written **after** the response line — the client can always read the answer
first and use the notification only for continuity.

## `status.get`, and what it cannot see

```json
{"id":3,"jsonrpc":"2.0","result":{"last":{"command":"embed","elapsed_ms":68,"phase":"completed","request":2},"fetch_markers":[{"bytes_done":8388608,"bytes_total":309855456,"id":"embeddinggemma-2","started_at":1794220456,"state":"running"}],"running":null}}
```

All four keys are always present.

- `last` — the finished activity of **this connection**: `{request?, command, phase, elapsed_ms}` plus `outcome` or `code`.
- `fetch_markers` — fetch markers found on disk for this workspace, read by listing the models directory. A marker written by a
  fetch that died is reported (`state: "running"` with its last checkpoint) and **left exactly where it is**: a read-only
  command does not delete or repair another process's state. A marker that cannot be parsed is reported as
  `{state: "unreadable", reason: …}` rather than dropped, because "something is there and I cannot read it" is information.
- `last_scan` — unix seconds of the last scan whose rows **committed** in this workspace, or `null` when none has. Written inside
  the scan's own transaction, so a scan that did not commit cannot leave a timestamp claiming it did; read only when the
  workspace's `.sqlite` already exists, for the same reason `status.get` never creates one. This is the answer to "is what I'm
  showing stale?" — the question a GUI asks far more often than "what is running?", and until now the only way to answer it was to
  run a scan, which is work rather than a reading.
- `running` — **always `null`.** This design is push-only: the server handles one request at a time, so by construction it
  cannot answer "what are you doing right now?" while it is doing it. Live status arrives as notifications; the key exists so
  that a future pull-based answer has a shape to grow into.

`status.get` never opens or creates the workspace database, and never touches a marker file — asking for status on a path that
is not a workspace returns an empty list and leaves no files behind.

## Captured transcripts

Every block below is verbatim stdout from one `np serve` session. Nothing was typed by hand, edited, or pretty-printed; paths
and timestamps are the capture machine's. See [Reproducing the transcripts](#reproducing-the-transcripts).

### A client that did not opt in

Two notes on disk, a keyword search, and no `notifications` key in the handshake. **Note the absence:** not one `np/progress`
line appears in the whole session — every line carries an `id`, so a v0.3.0 client's read loop is untouched.

```json
{"id":1,"jsonrpc":"2.0","result":{"data_dir":"/home/agent/tmp/cap2/A/data","features":{"fetch":true,"local_model":false,"vision":false},"methods":["status.get","initialize","search","scan","embed","models.list","shutdown"],"np":"0.3.1","protocol":1}}
{"id":2,"jsonrpc":"2.0","result":{"added":2,"elapsed_ms":163,"index_files":["INDEX.md"],"merged_notes":null,"modified":0,"moved":0,"notes":2,"recorded_merged_notes":"merged=0;paths=","recorded_upgrade_orphans":"headings=0,tags=0,items=0,links_src=0,links_dst_null=0","removed":0,"skipped":[],"unchanged":0,"unindexable":null,"unresolved_links":0,"unresolved_targets":[],"upgrade_orphans":null}}
{"id":3,"jsonrpc":"2.0","result":{"fetched":null,"hits":[{"bm25":-9.789227166276348e-7,"cosine":null,"keyword_rank":1,"kind":"note","linked_from":0,"linked_notes":[],"path":"meeting.md","score":9.789227166276348e-7,"semantic_rank":null,"snippet":"# Meeting notes\n\nThe [rollout] window opens on [Tuesday].\n","source_note":null}],"indexed":true,"keyword_used":true,"mode":"keyword","note":null,"offline":false,"reason":null,"semantic_used":false,"vectors":false}}
```

This block was re-captured on 2026-11-13, one release after the three below it and on a **default-feature** debug build — which is why its
`features.local_model` reads `false` where B, C and D read `true`. Its third line is also the visible half of **#5**: `indexed`,
`keyword_used` and `semantic_used` are new keys beside `hits`, and `mode` still means "what was asked for". It predates
**#2**, so its handshake lists seven methods where this build answers eight; a client feature-detects off that array, which is
precisely the case the array is there for.

### A first run with no network

Opted in, `[embeddings] provider = "local-model"`, origin unreachable. `locking` appears before the write lock is attempted;
then the answer says **offline**, and so does the terminal line — as a state, `completed`, not `refused`.

```json
{"id":1,"jsonrpc":"2.0","result":{"data_dir":"/home/agent/tmp/cap/run.rA4ZYv/B/data","features":{"fetch":true,"local_model":true,"vision":false},"methods":["status.get","initialize","search","scan","embed","models.list","shutdown"],"np":"0.3.1","progress":true,"protocol":1}}
{"jsonrpc":"2.0","method":"np/progress","params":{"command":"initialize","phase":"completed","request":1,"since_ms":0}}
{"jsonrpc":"2.0","method":"np/progress","params":{"command":"embed","phase":"locking","request":2,"since_ms":0}}
{"id":2,"jsonrpc":"2.0","result":{"artifact_check":null,"busy_gap":0,"chunks_seen":0,"current":0,"embedded":0,"errors":0,"fetched":null,"image_refusals":[],"images_embedded":0,"images_pruned":0,"images_refused":0,"images_seen":0,"images_sweep_ran":false,"images_unchecked":0,"model":"embeddinggemma-2","offline":true,"outcome":{"offline":{"id":"embeddinggemma-2"}},"refused_budget":0,"verification":"self-consistent, not verified","workers":null}}
{"jsonrpc":"2.0","method":"np/progress","params":{"command":"embed","offline":true,"outcome":{"offline":{"id":"embeddinggemma-2"}},"phase":"completed","request":2,"since_ms":66}}
{"id":3,"jsonrpc":"2.0","result":{"fetch_markers":[],"last":{"command":"embed","elapsed_ms":66,"phase":"completed","request":2},"last_scan":null,"running":null}}
{"jsonrpc":"2.0","method":"np/progress","params":{"command":"status.get","phase":"completed","request":3,"since_ms":0}}
```

### A download that got interrupted

Same session shape, origin reachable but the connection closes mid-body after 12 MiB. `bytes_total` is the pinned size
(295 MiB), `bytes_done` moves forward within member `main`, and the ending is a **refusal** carrying the same code as the error
object — because an interrupted transfer really did fail, where the session above really did not.

```json
{"id":1,"jsonrpc":"2.0","result":{"data_dir":"/home/agent/tmp/cap/run.rA4ZYv/C/data","features":{"fetch":true,"local_model":true,"vision":false},"methods":["status.get","initialize","search","scan","embed","models.list","shutdown"],"np":"0.3.1","progress":true,"protocol":1}}
{"jsonrpc":"2.0","method":"np/progress","params":{"command":"initialize","phase":"completed","request":1,"since_ms":0}}
{"jsonrpc":"2.0","method":"np/progress","params":{"command":"embed","phase":"locking","request":2,"since_ms":0}}
{"jsonrpc":"2.0","method":"np/progress","params":{"bytes_done":0,"bytes_total":309855456,"command":"embed","member":"main","members_done":0,"members_total":1,"phase":"fetching","request":2,"since_ms":60}}
{"jsonrpc":"2.0","method":"np/progress","params":{"bytes_done":8388608,"bytes_total":309855456,"command":"embed","member":"main","members_done":0,"members_total":1,"phase":"fetching","request":2,"since_ms":1493}}
{"jsonrpc":"2.0","method":"np/progress","params":{"bytes_done":16777216,"bytes_total":309855456,"command":"embed","member":"main","members_done":0,"members_total":1,"phase":"fetching","request":2,"since_ms":4039}}
{"jsonrpc":"2.0","method":"np/progress","params":{"bytes_done":25165824,"bytes_total":309855456,"command":"embed","member":"main","members_done":0,"members_total":1,"phase":"fetching","request":2,"since_ms":6483}}
{"error":{"code":-32000,"message":"cannot write /home/agent/tmp/cap/run.rA4ZYv/C/data/workspaces/32ed8d1669063346.models/.part.embeddinggemma-2.1530703-179424188689490595-0: response body closed before all bytes were read"},"id":2,"jsonrpc":"2.0"}
{"jsonrpc":"2.0","method":"np/progress","params":{"code":-32000,"command":"embed","phase":"refused","request":2,"since_ms":7537}}
{"id":3,"jsonrpc":"2.0","result":{"fetch_markers":[],"last":{"code":-32000,"command":"embed","elapsed_ms":7537,"phase":"refused","request":2},"last_scan":null,"running":null}}
{"jsonrpc":"2.0","method":"np/progress","params":{"command":"status.get","phase":"completed","request":3,"since_ms":1}}
```

### Asking a second process what is in flight

The `last_scan: null` here is not a gap in the stamp. This session's own `scan` ran while another process held the workspace write
lock, so it was refused as a state (`completed`, with `locked`) and committed nothing — and a timestamp that meant "a scan was
attempted" would be worth less than one that means "these rows are on disk".

One `np serve` is mid-download; a **second** process answers `status.get` immediately, because reading markers never waits for
the writer. This is the case `running: null` cannot cover, and it is why the marker list exists.

```json
{"id":1,"jsonrpc":"2.0","result":{"data_dir":"/home/agent/tmp/cap/marker.678uo7/data","features":{"fetch":true,"local_model":true,"vision":false},"methods":["status.get","initialize","search","scan","embed","models.list","shutdown"],"np":"0.3.1","protocol":1}}
{"id":2,"jsonrpc":"2.0","result":{"command":"scan","locked":true,"waited_ms":2000}}
{"id":3,"jsonrpc":"2.0","result":{"fetch_markers":[{"bytes_done":8388608,"bytes_total":309855456,"id":"embeddinggemma-2","started_at":1794241904,"state":"running"}],"last":{"command":"scan","elapsed_ms":2006,"phase":"completed","request":2},"last_scan":null,"running":null}}
```

## Client walkthrough

The smallest thing that works, in Python — spawn once, keep it alive, read lines as they come:

```python
import json, subprocess

proc = subprocess.Popen(
    ["np", "serve"],
    stdin=subprocess.PIPE, stdout=subprocess.PIPE, stderr=subprocess.DEVNULL,
    text=True, encoding="utf-8", bufsize=1,
)

def call(request_id, method, params=None):
    message = {"jsonrpc": "2.0", "id": request_id, "method": method}
    if params:
        message["params"] = params
    proc.stdin.write(json.dumps(message, separators=(",", ":")) + "\n")
    proc.stdin.flush()

def read_line():
    line = proc.stdout.readline()
    return None if line == "" else json.loads(line)

call(1, "initialize", {"workspace": "/vault", "notifications": True})
while True:
    message = read_line()
    if message is None or message.get("id") == 1:
        break
# The connection now supports progress. One request in flight at a time; read until its response arrives,
# keeping every notification you pass on the way.
call(2, "embed", {"workspace": "/vault"})
last = None
while True:
    message = read_line()
    if message is None or message.get("id") == 2:   # the response ends this command
        break
    if message.get("method") == "np/progress":
        last = message["params"]                    # keep it; see the reload recipe below
```

Five rules that are easy to get wrong:

1. **One request in flight.** Responses come back in order, but a `notification` interleaves anywhere, so match responses by
   `id`, and treat "no `id`" as "this is a notification".
2. **A search hit has no `title` key.** It never did; do not wait for one. Keys are `path`, `snippet`, `kind`, `score`,
   `bm25`, `cosine`, `keyword_rank`, `semantic_rank`, `linked_from`, `linked_notes`, `source_note`.
3. **`snippet` may contain newlines.** They arrive JSON-escaped (`\n` inside a string), so one response really is one line —
   but if you render the snippet, decide what a newline means in your UI rather than letting it break your layout.
4. **Close stdin to stop.** `proc.stdin.close()` then wait; exit code 0 is normal at EOF. `shutdown` also works and lets you
   read a final response first.
5. **Never write to the child's stdin from two threads.** One writer, or serialize behind a lock: interleaving two requests
   into one line corrupts the stream in a way no client can recover from.

### The reload recipe

Under a push-only design, a page that reloads mid-download cannot ask "what is happening right now?" — `running` is `null`, and
the server is busy and will not answer until the command finishes. So the **spawning process** keeps state:

- store the last `np/progress` params per `request` id, in memory (and in your renderer's session, not just the DOM);
- on reload, re-serve that stored line as the current status instead of showing nothing;
- when the response finally arrives, replace it with the real result and clear the stored line;
- if the app restarts while a download is running, `status.get`'s `fetch_markers` is what you have: another process's in-flight
  fetch, with its last checkpoint.

That is the whole contract. `np` remembers nothing about your UI; it hands you enough to remember for it.

## Reproducing the transcripts

Sessions B, C and D came from a debug build of that branch (`cargo build -p np-cli --features local-model,fetch`) driven by
`/home/agent/tmp/cap/capture.sh`. Session A was re-captured on 2026-11-13 by piping three requests to `target/debug/np serve`
(default features) with the same per-session `XDG_DATA_HOME` / `XDG_CACHE_HOME` / `XDG_CONFIG_HOME`, `NP_NO_AUTO_FETCH`
unset, a two-note vault, and a Python origin that answers the pinned `Content-Length` and then dribbles 4 MiB per 1.2 s before
hanging up mid-body. Session A opted out of notifications; B pointed `[models] base_url` at a closed port; C and D pointed it at
the dribbling origin, and D ran its `status.get` from a second process while C's download was in flight. Session A's own shape: two
notes (`meeting.md`, `todo.md`), `scan`, then `search` for `Tuesday` with `"mode": "keyword"`.
