# Conventions for agents working in this repository

This file is for an agent opening the tree with no memory of how it got here: the implementer, the reviewer who
reads what that implementer wrote, and whoever picks up the next round. It is not a product spec and not a logbook.
What `np` does lives in `README.md` and `docs/`; what is open, deferred or owed lives in `todo.md`; what happened,
in which round and on which machine, lives in `git log`.

## Rules

- **Write comments that say why the code is the way it is now.** Decision history, ticket IDs, CI run IDs and
  "measured on …" notes belong in commit messages, ADRs or release notes, because every one of them was true at a
  single moment, and a reader holding only the file cannot tell whether the sentence or the code went stale first.
- **Take every date from `git log` or `date -u`.** `git log -1 --format=%cs <tag>` answers when a release happened,
  `git log -S<phrase> -- <file>` answers when a line of prose was first written, and `date -u` answers when this
  sentence is written. Where that evidence does not exist, write no date: an inferred one reads exactly like a
  sourced one, so nothing downstream can tell them apart.
- **Keep host paths, account names and billing notes out of tracked files.** They describe one machine or one
  account, which makes the claim unchecked by anyone else — and a sentence that only holds on one machine is a
  fact about that machine rather than about this repository.
- **A new feature gets a reviewed design before it gets code.** A design is where scope is argued while it is
  still cheap to change, whereas code settles the same questions silently, in a place nobody reviews them.

## Before you write code

- The design review above comes first: write it beside the others as `docs/design-<id>-<slug>.md` and let it be
  read before the branch exists.
- One task per branch, one change per commit, so that reverting a task reverts one line of work rather than
  someone else's too.
- A refactor changes no behaviour. Where behaviour moved, that is a fix or a feature and needs its own commit, its
  own test, and — if it is a feature — the design again.
- Verify before claiming anything is done: the workspace tests pass, `.github/scripts/gates.sh` passes, and
  `.github/scripts/verify_catalog_goldens.py` prints `GOLDEN CHECK: pass` with no golden file touched. That script
  holds the whole gate list as one command, so run all of it — subsetting a list is how a green log stops meaning
  anything. `docs/developing.md` says what hosted CI grades on top of it, and when.

## Do not put in tracked files

- An absolute path into a home directory or a working tree. Write the repo-relative path, or name the environment
  variable that holds the real one at runtime.
- An account name or owner handle — anything that only makes sense to whoever owns the hosted repository.
- Actions-minute or storage arithmetic. Quota numbers move with the billing period and with whether the
  repository is public, so a figure copied into a tracked file is stale before its first reader sees it.
- A CI run number. It cannot be opened by anyone without access to that host, and it stops resolving whenever the
  runs age out.
- "Measured on <date>" narration. The measurement belongs in the file; the day it happened belongs in the commit
  message that recorded it.

## Where decisions live

`todo.md` is the handoff and it is deliberately short: what is true now, what a human owes, the next three actions.
A decision that has been closed stops being argued there and gets a file in `docs/adr/`, indexed in
`docs/adr/README.md`. Work that shipped goes to `release-notes/`; deferred engineering cards go to
`docs/backlog-m2-followups.md`. If you find yourself adding a fourth section to `todo.md`, it belongs in one of
those instead.

## The machine you are working on

- Rust is in `~/.cargo/bin`; put it on `PATH` before any cargo command. Node and npm exist for the front end (M5).
- Never put a GitHub token on a command line or in a file. `$GH_TOKEN` is already wired for `gh`, and `gh` is
  authenticated — use it rather than re-discovering credentials.
- A subagent endpoint tends to die around thirty minutes in. Keep rounds bounded, have the implementer commit `wip:` as
  soon as it compiles, and resume a stalled round rather than restarting it.
