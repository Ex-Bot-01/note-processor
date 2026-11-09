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

## Methods

`initialize` reports the authoritative list; `protocol` stays `1`.

| Method | Params | Result | Built by | Refusals |
|---|---|---|---|---|
| `initialize` | `workspace?`, `notifications?` | `{protocol, np, features, methods, data_dir, progress?}` | its own builder | `-32602` on a non-boolean `notifications` |
| `search` | `workspace`, `query`, `limit?`, `mode?` (`keyword`\|`semantic`\|`hybrid`) | the same document as `np search --json` | `search_document` | `-32602` missing/unknown key; `-32000` workspace unreadable |
| `scan` | `workspace` | the same report as `np scan --json` | `ScanReport` | `-32602`; `-32000` |
| `embed` | `workspace` | the same report as `np embed --json` | `embed_report` | `-32602`; `-32000` (including a build without the inference feature) |
| `models.list` | `workspace` | installed-artifact rows | `models_list_document` | `-32602` |
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

## States travel as data, not as errors

This is the rule that makes the interface usable: **a state is not a failure.** Offline is a state. Contention is a state. An
embed that refused to write because the budget ran out is a state. They arrive in the *result* document, exactly as
`np --json` puts them on stdout, and they never appear in `error`.

| Key | Meaning | Appears on |
|---|---|---|
| `locked: true`, `waited_ms` | another process holds the workspace write lock; nothing was changed | `scan`, `embed` results |
| `offline: true`, `reason` | the network was tried and failed, so no artifact could be fetched | `search`, `embed` results |
| `reason` | a machine token (`provider-disabled`, `local-model-not-built`, `offline`, …) explaining a degraded result | `search`, `embed` results |
| `vectors: false` | the semantic half did not run, so a hybrid answer is keyword-only | `search` results |
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
| `-32603` | internal error, including a handler that panicked — one bad request never kills the session |

A notification (a request with no `id`) gets **no response line at all**, per JSON-RPC. That includes `np/progress`: the server
never answers its own notifications.

## Notifications

Progress is **opt-in per connection** and off by default, so a client built against v0.3.0 sees byte-identical responses:

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
| `embedding` | chunks being encoded | `chunks_done`, `embedded`, sometimes `chunks_total` |
| `writing` | vectors committed | same as `embedding` |
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

All three keys are always present.

- `last` — the finished activity of **this connection**: `{request?, command, phase, elapsed_ms}` plus `outcome` or `code`.
- `fetch_markers` — fetch markers found on disk for this workspace, read by listing the models directory. A marker written by a
  fetch that died is reported (`state: "running"` with its last checkpoint) and **left exactly where it is**: a read-only
  command does not delete or repair another process's state. A marker that cannot be parsed is reported as
  `{state: "unreadable", reason: …}` rather than dropped, because "something is there and I cannot read it" is information.
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
{"id":1,"jsonrpc":"2.0","result":{"data_dir":"/home/agent/tmp/cap/run.1I3IGW/A/data","features":{"fetch":true,"local_model":true,"vision":false},"methods":["status.get","initialize","search","scan","embed","models.list","shutdown"],"np":"0.3.0","protocol":1}}
{"id":2,"jsonrpc":"2.0","result":{"added":2,"elapsed_ms":161,"index_files":["INDEX.md"],"merged_notes":null,"modified":0,"moved":0,"notes":2,"recorded_merged_notes":"merged=0;paths=","recorded_upgrade_orphans":"headings=0,tags=0,items=0,links_src=0,links_dst_null=0","removed":0,"skipped":[],"unchanged":0,"unindexable":null,"unresolved_links":0,"unresolved_targets":[],"upgrade_orphans":null}}
{"id":3,"jsonrpc":"2.0","result":{"fetched":null,"hits":[{"bm25":-1e-6,"cosine":null,"keyword_rank":1,"kind":"note","linked_from":0,"linked_notes":[],"path":"meeting.md","score":1e-6,"semantic_rank":null,"snippet":"# Meeting notes\n\nThe [rollout] window opens on Tuesday.\n","source_note":null}],"mode":"keyword","note":null,"offline":false,"reason":null,"vectors":false}}
```

### A first run with no network

Opted in, `[embeddings] provider = "local-model"`, origin unreachable. `locking` appears before the write lock is attempted;
then the answer says **offline**, and so does the terminal line — as a state, `completed`, not `refused`.

```json
{"id":1,"jsonrpc":"2.0","result":{"data_dir":"/home/agent/tmp/cap/run.1I3IGW/B/data","features":{"fetch":true,"local_model":true,"vision":false},"methods":["status.get","initialize","search","scan","embed","models.list","shutdown"],"np":"0.3.0","progress":true,"protocol":1}}
{"jsonrpc":"2.0","method":"np/progress","params":{"command":"initialize","phase":"completed","request":1,"since_ms":0}}
{"jsonrpc":"2.0","method":"np/progress","params":{"command":"embed","phase":"locking","request":2,"since_ms":0}}
{"id":2,"jsonrpc":"2.0","result":{"artifact_check":null,"busy_gap":0,"chunks_seen":0,"current":0,"embedded":0,"errors":0,"fetched":null,"image_refusals":[],"images_embedded":0,"images_pruned":0,"images_refused":0,"images_seen":0,"images_sweep_ran":false,"images_unchecked":0,"model":"embeddinggemma-2","offline":true,"outcome":{"offline":{"id":"embeddinggemma-2"}},"refused_budget":0,"verification":"self-consistent, not verified","workers":null}}
{"jsonrpc":"2.0","method":"np/progress","params":{"command":"embed","offline":true,"outcome":{"offline":{"id":"embeddinggemma-2"}},"phase":"completed","request":2,"since_ms":68}}
{"id":3,"jsonrpc":"2.0","result":{"fetch_markers":[],"last":{"command":"embed","elapsed_ms":68,"phase":"completed","request":2},"running":null}}
{"jsonrpc":"2.0","method":"np/progress","params":{"command":"status.get","phase":"completed","request":3,"since_ms":0}}
```

### A download that got interrupted

Same session shape, origin reachable but the connection closes mid-body after 12 MiB. `bytes_total` is the pinned size
(295 MiB), `bytes_done` moves forward within member `main`, and the ending is a **refusal** carrying the same code as the error
object — because an interrupted transfer really did fail, where the session above really did not.

```json
{"id":1,"jsonrpc":"2.0","result":{"data_dir":"/home/agent/tmp/cap/run.1I3IGW/C/data","features":{"fetch":true,"local_model":true,"vision":false},"methods":["status.get","initialize","search","scan","embed","models.list","shutdown"],"np":"0.3.0","progress":true,"protocol":1}}
{"jsonrpc":"2.0","method":"np/progress","params":{"command":"initialize","phase":"completed","request":1,"since_ms":0}}
{"jsonrpc":"2.0","method":"np/progress","params":{"command":"embed","phase":"locking","request":2,"since_ms":0}}
{"jsonrpc":"2.0","method":"np/progress","params":{"bytes_done":0,"bytes_total":309855456,"command":"embed","member":"main","members_done":0,"members_total":1,"phase":"fetching","request":2,"since_ms":61}}
{"jsonrpc":"2.0","method":"np/progress","params":{"bytes_done":8388608,"bytes_total":309855456,"command":"embed","member":"main","members_done":0,"members_total":1,"phase":"fetching","request":2,"since_ms":1494}}
{"error":{"code":-32000,"message":"cannot write /home/agent/tmp/cap/run.1I3IGW/C/data/workspaces/c14fe6ff867a7e0a.models/.part.embeddinggemma-2.1235066-1794220420933722328-0: response body closed before all bytes were read"},"id":2,"jsonrpc":"2.0"}
{"jsonrpc":"2.0","method":"np/progress","params":{"code":-32000,"command":"embed","phase":"refused","request":2,"since_ms":3871}}
{"id":3,"jsonrpc":"2.0","result":{"fetch_markers":[],"last":{"code":-32000,"command":"embed","elapsed_ms":3871,"phase":"refused","request":2},"running":null}}
{"jsonrpc":"2.0","method":"np/progress","params":{"command":"status.get","phase":"completed","request":3,"since_ms":0}}
```

### Asking a second process what is in flight

One `np serve` is mid-download; a **second** process answers `status.get` immediately, because reading markers never waits for
the writer. This is the case `running: null` cannot cover, and it is why the marker list exists.

```json
{"id":1,"jsonrpc":"2.0","result":{"data_dir":"/home/agent/tmp/cap/marker.iUNlaJ/data","features":{"fetch":true,"local_model":true,"vision":false},"methods":["status.get","initialize","search","scan","embed","models.list","shutdown"],"np":"0.3.0","protocol":1}}
{"id":2,"jsonrpc":"2.0","result":{"fetch_markers":[{"bytes_done":8388608,"bytes_total":309855456,"id":"embeddinggemma-2","started_at":1794220456,"state":"running"}],"last":{"command":"initialize","elapsed_ms":0,"phase":"completed","request":1},"running":null}}
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

The blocks above came from a debug build of this branch (`cargo build -p np-cli --features local-model,fetch`) driven by
`/home/agent/tmp/cap/capture.sh`, with per-session `XDG_DATA_HOME` / `XDG_CACHE_HOME` / `XDG_CONFIG_HOME`, `NP_NO_AUTO_FETCH`
unset, a two-note vault, and a Python origin that answers the pinned `Content-Length` and then dribbles 4 MiB per 1.2 s before
hanging up mid-body. Session A opted out of notifications; B pointed `[models] base_url` at a closed port; C and D pointed it at
the dribbling origin, and D ran its `status.get` from a second process while C's download was in flight.
