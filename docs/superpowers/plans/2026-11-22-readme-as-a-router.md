# README-as-a-router Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** Replace `README.md`'s 28 KB wall of text with a ~9–11 KB router that answers "what is it / how do I use it / how do I integrate it" in one pass, with every displaced claim landing in a `docs/` file that already exists.

**Architecture:** Three moves, in order: first prove each departing paragraph has a home (and write the home where one is missing), then rewrite the README against a fixed heading order, then integrate to `main` and push. No code, tests or gates change — the diff is `.md` files only, which is what makes "no cargo row can flip" a fact rather than a hope.

**Tech Stack:** Markdown; the repository's own python fences (`.github/scripts/check-links.py`, `check-dates-ahead.py`).

**Spec:** `docs/superpowers/specs/2026-11-22-readme-as-a-router-design.md`

## Global Constraints

- Size: new `README.md` between 9 KB and 11 KB, and no prose line over 200 characters.
- No ticket markers (`#7`, `issue #14`, `F51`) and no "measured on <date>" narration — `AGENTS.md` forbids both in tracked files.
- Dates only from `git log` or `date -u`; a date later than HEAD's commit date fails `check-dates-ahead.py`.
- Every transcript stays verbatim capture already present in a `docs/reference-*.md`, cited as a capture. Nothing hand-typed.
- One unit spelling for the pinned embedding artifact: **295 MiB** (309,855,456 bytes), as `docs/reference-models.md` quotes it.
- Every markdown link and backticked path in the new README must resolve — `.github/scripts/check-links.py` exits non-zero otherwise.
- `docs/README.md` describes `../README.md` as answering "what `np` does, every command, and the first five minutes with a real folder"; the rewrite must keep all three true without editing that line.

## Review Focus

- **A silently dropped claim** — an integrator cannot find exit codes, or the twelve `config.toml` keys, in the new README or in any doc it can reach from there. Expected: every departing paragraph's destination is named before the deletion happens.
- **A link that goes red** — a rewrite rewords a sentence and carries `[text](docs/reference-x.md)` into a shape the fence rejects, or points at a heading that no longer exists. Expected: `check-links.py` green, 0 broken.
- **Output that was never captured** — a tidy JSON or transcript example typed by hand reads exactly like real output. Expected: every code block quoting `np` is copied from a reference file and says which capture it came from.
- **The README stops being the entry point** — it shrinks past the point where a newcomer can run `np scan`, or loses the six-command table that `docs/README.md` promises. Expected: try-it-in-two-commands and the command table both survive.
- **Honesty edited out** — trimming makes "Limits, stated plainly" disappear along with the unmeasured-Windows and no-hosted-CI admissions. Expected: that section survives intact.

---

### Task 1: Prove every departing paragraph has a home

**Files:**
- Modify: `docs/reference-serve.md`, `docs/reference-cli-and-reports.md`, `docs/reference-troubleshooting.md` — only where the audit finds a claim with no destination.

**Interfaces:**
- Consumes: the current `README.md` as the list of claims.
- Produces: an ordered list of `(claim, destination)` pairs — one per block leaving the README — used by Task 2's commit message and by its "nothing lost" check.

- [ ] **Step 0: Start the branch**

  ```bash
  git checkout -b docs/readme-as-a-router origin/main
  ```

- [ ] **Step 1: List the blocks that leave the README**

  Write down each block being cut or compressed to a pointer: the `np serve` JSON transcript; the two documentation index tables; the per-flag prose inside the six-command table (`--prefix`, `--rerank` wording); the long keyword-`AND` bullet; the seven-row troubleshooting table's tail rows; the per-version release-caveat sentences; the diagnostics paragraph.

- [ ] **Step 2: For each block, name the destination file and prove the claim is there**

  Run one grep per block for its distinguishing phrase in the file that should hold it, e.g.:

  ```bash
  grep -n "response body closed before all bytes were read" docs/reference-serve.md
  grep -n "prefix_used\|rerank_top_n\|--rerank-text" docs/reference-cli-and-reports.md
  grep -n "locked: another process is writing" docs/reference-troubleshooting.md
  ```

  Expected: one hit per block. A block with no hit gets its paragraph **added to that destination file** in this task, before any deletion — never deleted on the assumption that some other file covers it.

- [ ] **Step 3: Run the two doc fences**

  Run: `python3 .github/scripts/check-links.py && python3 .github/scripts/check-dates-ahead.py`
  Expected: `0 broken`, `DATE CHECK: pass`.

- [ ] **Step 4: Commit only if Step 2 added anything**

  ```bash
  git add docs/ && git commit -m "docs(reference): make room for the README's moved claims"
  ```

### Task 2: Write the router README

**Files:**
- Modify: `README.md` (complete rewrite)

**Interfaces:**
- Consumes: Task 1's `(claim, destination)` list; the heading order fixed in the spec's "Decided shape".
- Produces: a `README.md` of 9–11 KB whose section order is: pitch · What it does · Try it · The six commands · three scripting facts · Configure it · Use `np` from your app · Images, briefly · When something looks wrong · Limits, stated plainly · Where to read next · License.

- [ ] **Step 1: Write the new README in that order**

  Carry over verbatim where accuracy depends on it: the scan/setup/search output blocks, the six-command table (one line per command, flags condensed), the three-key TOML sample, the four troubleshooting rows a first-time user actually meets, and the whole "Limits, stated plainly" section. Condense each scripting fact to two lines with a pointer. Quote `np serve` with the verbatim `v0.6.0` handshake line from `docs/reference-serve.md` plus one response line, naming the capture it came from.

- [ ] **Step 2: Check size and line width**

  Run: `wc -c README.md && awk 'length > 200 {print FILENAME": "NR}' README.md`
  Expected: byte count between 9000 and 11000; the `awk` prints nothing.

- [ ] **Step 3: Check nothing forbidden survived**

  Run: `grep -nE '\(\*\*(issue )?#?[A-Za-z]?[0-9]+\*\*\)|measured on|~309 MB|~310 MB' README.md`
  Expected: no output (exit 1) — no ticket markers, no measurement narration, and one unit spelling for the artifact.

- [ ] **Step 4: Check what must survive did**

  Run: `grep -nc '^## ' README.md` then `grep -n '^## \|np models install\|Limits, stated plainly' README.md`
  Expected: twelve `##` sections in the order named in this task's Interfaces block, the six-command table present with `np models install` and `np embeddings enable` in it, and the Limits section intact including its admissions about hosted CI and Windows.

- [ ] **Step 4b: Check every quoted block is a real capture**

  For each code block that shows `np` output or protocol lines, grep a distinguishing substring from it in the reference file the README names as its source. Expected: one hit each; a block with no hit is replaced by the capture or dropped.

- [ ] **Step 5: Run the doc fences**

  Run: `python3 .github/scripts/check-links.py && python3 .github/scripts/check-dates-ahead.py`
  Expected: `0 broken`, `DATE CHECK: pass`.

- [ ] **Step 6: Check nothing was lost, claim by claim**

  For each pair from Task 1, confirm the claim is reachable: either still in `README.md` (`grep -n`) or present in its destination file. Expected: every pair accounted for; a missing one is fixed before committing, not noted afterwards.

- [ ] **Step 7: Commit**

  ```bash
  git add README.md && git commit -m "docs(readme): the front page becomes a router"
  ```

### Task 3: Integrate and push

**Files:** none (git operations only)

- [ ] **Step 1: Confirm the diff is prose only**

  Run: `git diff --name-only origin/main...HEAD`
  Expected: `.md` paths only — no file under `crates/`, `.github/scripts/` or `Cargo.*`. This is the evidence that no cargo or fence row can change state, so it is reported instead of a full `gates.sh` run.

- [ ] **Step 2: Merge to `main` with the repository's merge-message style**

  ```bash
  git checkout main && git merge --no-ff docs/readme-as-a-router \
    -m "Merge branch 'docs/readme-as-a-router' — the front page becomes a router, detail keeps its home"
  ```

- [ ] **Step 3: Push and verify the remote agrees**

  ```bash
  git push origin main && git ls-remote origin main && git rev-parse HEAD
  ```

  Expected: the `ls-remote` sha equals `rev-parse HEAD`.
