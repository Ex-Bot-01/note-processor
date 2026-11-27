# F52 — A second writer waits its turn: a fair, cancellable wait for the workspace lock instead of a 2-second refusal

Status: **parked — no code, not to be built until the reopening condition in the F52 card (`docs/backlog-m2-followups.md`) is met.**
§3 explains why: the client that reported the failures does not produce the contention this design addresses. Written before any code, per
`AGENTS.md`. Prior art: F44 (`docs/backlog-m2-followups.md`, the
workspace lock and why its wait is bounded), `crates/np-core/src/lock.rs`, `writer_lock` / `server_writer_lock` in
`crates/np-cli/src/commands/common.rs`, the `locking` progress phase and the `cancel` table in `docs/reference-serve.md`, and
ADR 0001 (the daemon owns everything).

## 1. What is wrong, in one sentence

When two processes write one workspace, the one that arrives second waits at most two seconds and then gets `locked: true` — and
because SQLite's busy wait is a sleep-and-poll rather than a queue, a process that writes in a loop can keep the other one out
indefinitely, so "a second writer" degrades into "the second writer almost never gets in".

## 2. What happens today

* `WorkspaceLock::acquire` runs `BEGIN IMMEDIATE` on `<data-dir>/<workspace-key>.lock.db` with `busy_timeout = 2000`. The holder keeps the
  transaction open for the whole command; `Drop` rolls it back. Five CLI commands take it (`scan`, `embed`, `setup`, `models install`,
  `models fetch`); in `np serve`, `scan` and `embed` take it per request. Readers never do.
* A writer that does not get the lock within 2 s returns the **state** `{"command":…,"locked":true,"waited_ms":2000}` (exit 1 on the CLI, a
  result — not a fault — in `np serve`) and writes one `locked` warning to the log.
* F44 bounded the wait on purpose: the case it was protecting against is a script calling `np scan` while a long `embed` runs, which must get
  an answer rather than hang for minutes.

### 2.1 The wait is not fair

SQLite's busy handler sleeps for growing steps (up to 100 ms each) between attempts. A holder that releases the lock and takes it again
within a few milliseconds — `np serve` answering one scan and starting the next — almost always wins the race against a waiter that is
asleep. Measured with two `np serve` processes on one vault for five minutes (one scanning and embedding in a loop, one scanning and
searching):

| process | scans | refused as `locked` |
| --- | --- | --- |
| loops scan + embed | 4 196 | 0 |
| scan + search | 186 | **119 (64 %)** |

Every refusal was logged, and none was an error — the lock did what F44 specified. But "bounded wait" in practice meant "the busier process
always wins", which a longer timeout alone would not change: it would turn 2-second refusals into long waits with the same loser.

### 2.2 A wait inside `np serve` is not free

`np serve` answers one request at a time per connection (only `status.get` and `cancel` overtake). A `scan` waiting for the lock therefore
holds up every search that client queued behind it. And the wait happens inside SQLite's `BEGIN IMMEDIATE`, where `cancel` has no
checkpoint: a client that gives up can only wait out the timeout. At 2 s that is tolerable; at anything longer it is not.

## 3. Who is the second writer?

ADR 0001 says the daemon owns everything, so in the product's intended shape one process writes and the lock only ever meets:

* a terminal `np scan` / `np embed` run while the app's `np serve` is working — the case F44 measured;
* two `np serve` processes on one vault — two app windows, or an app plus a script that drives `np serve` itself.

Inside **one** `np serve` there is no contention at all: requests are serialised before they reach the lock. The design below is for the
cross-process cases.

**The client that reported the failures** is the front-end app that uses np as its search engine. Its np pool, read for this card:

* it is a single-instance app, so two windows never run two pools on one vault;
* it runs **one writer** `np serve` (every `scan` and `embed`) and **one reader** `np serve` (`search`, `models.list`), so its writes are
  already serialised before they reach the workspace lock — the reader never takes it;
* a writer child is replaced only after its stdout has closed, and children are killed when dropped, so a replacement never overlaps a
  live writer inside one app session.

So in steady state that app produces **no** workspace-lock contention, and the failures it saw were SQLite's `database is locked` (a
`-32000` fault), not `locked: true`. Those had other writers to the workspace *database* — not the workspace lock — behind them: migrations
on a reader's first open, and the scan's deferred write transaction meeting another connection's commit. Both are fixed by the
`BEGIN IMMEDIATE` changes on `main`. One unlocked database writer remains on the reader side: a hybrid search that performs the
first-install fetch records the `models` row without taking the workspace lock. That is once per install, not a steady rate.

What would still put two writers on one workspace from that app is an `np serve` that outlived the window that started it (an app that
exits without dropping its pool leaves a child that finishes its current request before it sees end-of-input), or a terminal `np` command
beside the app. Neither is fixed by waiting longer; both are fixed by the wait being fair (§4.3) and visible (§4.2).

## 4. The proposal

Three changes, each useful alone, shipped together because the first one is unsafe without the second and pointless without the third.

### 4.1 The wait is chosen by the caller

| channel | how | default |
| --- | --- | --- |
| CLI | `--wait <seconds>` on the five writing commands; `--wait 0` means "refuse at once" | **2 s, unchanged** — scripts keep today's answer |
| `np serve` | `wait_ms` in the params of `scan` and `embed`; a negative, non-integer or over-ceiling value is refused with `-32602` | see §9.2 |

A ceiling exists (proposed: 10 minutes) so that no value means "forever"; the client who wants longer asks again. `waited_ms` in the
`locked` document and `expected` / `actual` in the `locked` log record report the wait that was actually chosen, so they keep agreeing with
each other.

### 4.2 The wait is cancellable and visible

The wait moves out of SQLite's busy handler and into `WorkspaceLock::acquire`: a loop of `BEGIN IMMEDIATE` attempts with
`busy_timeout = 0` and a short sleep between them (proposed: 25 ms), checking a cancel flag on every turn. Concretely, `acquire` takes a
`&dyn Fn() -> bool` "should I stop?" argument; the CLI passes `|| false`, `np serve` passes the request's cancel flag.

* In `np serve`, `cancel` on a request that is waiting for the lock answers `state: "running"` and the request ends with `-32800` at its
  next turn — within one sleep step — instead of after the timeout. The `cancel` table in `reference-serve.md` gains that row.
* The `locking` progress phase, which today is sent once before the attempt, repeats on the normal throttle while the wait lasts, carrying
  `waited_ms` so far. A client can draw "waiting for another writer (12 s)" instead of a spinner that looks identical to a slow scan.

Owning the loop is also what makes §4.3 possible: SQLite's own handler cannot be made fair from outside.

### 4.3 The wait is fair

A second lock file, `<workspace-key>.queue.db`, held only while *waiting*:

```text
acquire:
  1. take queue.db   (same cancellable loop)
  2. take lock.db    (same cancellable loop)
  3. release queue.db
  … work under lock.db …
release: drop lock.db
```

A holder that finishes and immediately wants the lock again must pass step 1 first — and a waiter is sitting in step 2 holding `queue.db`,
so the holder blocks until the waiter has had its turn. With two processes that is strict alternation; with three or more it is "nobody is
passed over twice in a row" rather than strict FIFO, which is enough for a single-user machine. Both files are held SQLite transactions, so
F44's property survives: a killed process releases both, and nothing stale is ever left to clean up.

Rejected: a ticket number in a table (needs expiry when a waiter dies, which is exactly the stale-lock failure F44 chose SQLite to avoid),
and a lock-free retry inside `scan` (does not protect `INDEX.md`, the reason the lock exists).

## 5. What does not change

* Readers never take either lock; `search` and `models list` keep answering while a writer works.
* `locked: true` stays a **state**, documented and logged as today, for whoever's wait ran out.
* The CLI default stays 2 s, so no existing script changes behaviour unless it passes `--wait`.
* The `database-busy` and `failed` log records, and the `BEGIN IMMEDIATE` write transactions inside the workspace database, are unrelated
  and untouched.

## 6. Costs

* **One more file per workspace** (`.queue.db`, zero bytes, rollback journal, same lifetime as `.lock.db`).
* **A client's queue stalls while its write waits** (§2.2). Cancellation bounds it, but a client that sends `scan` with a long `wait_ms` and
  then a search will see the search wait too. Letting reads overtake a waiting write would change the "one at a time, in order" promise in
  `reference-serve.md` and is **out of scope** — listed so a reviewer can disagree.
* **Polling costs a wake-up every 25 ms while waiting.** Negligible against a scan; named because it replaces SQLite's backoff.
* **Windows is unmeasured.** SQLite uses `LockFileEx` there; F44's lock is believed equivalent but has never been measured on Windows, and
  this design adds a second lock on the same mechanism.

## 7. Tests

All cross-process, because POSIX record locks are per process and an in-process second connection would pass whether or not exclusion
works (the reason `lock.rs` already re-invokes its test binary as a holder).

1. **Bounded:** `acquire` with a wait shorter than the holder's hold returns `Held { waited }` with `waited` equal to the wait — today's
   test, kept.
2. **Cancellable:** a holder child holds for 5 s; `acquire` with a 10-second wait and a stop flag set at 300 ms returns within one sleep step
   of 300 ms, as a cancel rather than as `Held`.
3. **Fair:** a holder child loops "take, hold 50 ms, release, take again" for 5 s; a waiter with a 2-second wait must get the lock on every
   one of 20 attempts. Fails on today's code (that is the §2.1 table in test form).
4. **`np serve` cancel:** a `scan` with `wait_ms: 10000` against a held lock, then `cancel` → `-32800` well under 10 s; and a repeated
   `locking` notification carrying a growing `waited_ms`.
5. **Parameters:** `--wait` and `wait_ms` reject negatives, non-integers and values over the ceiling; `--wait 0` refuses at once.
6. **Unchanged default:** with no flag, a CLI writer against a held lock refuses after 2 s with today's exact document.

And one measurement, not a test: the two-`np serve` stress run of §2.1, repeated with `wait_ms` set to cover a scan. Expected: zero
`locked` results for the scan-and-search process, and the looping writer's throughput down by roughly the other's share.

## 8. Order of work

One branch, three commits, each passing the gate list on its own:

1. `acquire` owns its wait loop and takes a stop callback (behaviour-preserving apart from cancellability; tests 1, 2).
2. The queue file (tests 3; the stress measurement).
3. `--wait` / `wait_ms`, the repeated `locking` notification, and the docs: `reference-serve.md` (params, `cancel` table, `locking` row),
   `reference-cli-and-reports.md`, `reference-troubleshooting.md` (the `locked` row), `reference-diagnostics.md` (what `expected` means
   now) (tests 4–6).

## 9. Rulings needed before code

1. **Is this card worth doing now?** §3 found that the reporting app causes no workspace-lock contention in steady state, and the
   `database is locked` failures it saw are fixed on `main`. The recommendation is to **ship the fixes to the app first** and watch its log
   for `locked` and `database-busy` records: if neither appears, this card waits; if `locked` appears, the log's `request` and timestamps
   say which second process was holding the workspace, and §4.2 + §4.3 are the part to build.
2. **`np serve`'s default wait.** Options: (a) keep 2 s and let the client opt into longer — fully compatible; (b) raise it (for example
   to 60 s) because a GUI can draw the wait and cancel it, while a script cannot — changes what an unmodified client sees. Recommendation:
   **(a)**, because the app is the only `np serve` client and can pass `wait_ms` itself.
3. **The ceiling.** 10 minutes proposed. A long `embed` of a large vault can exceed it, and the waiter then gets `locked` after 10 minutes
   instead of after 2 s — is that the right answer, or should the ceiling be lower so a forgotten wait surfaces sooner?
