# Security policy

`np` is a **single-user, local-first** tool: one binary, one database per workspace, no account, and nothing that listens on a port. That shape is the
security posture, so it is stated first and precisely — including which parts of it are tested and which are simply how the program is built.

## What `np` does and does not touch

- **It reads inside the workspace you name, plus its own data directory.** Nothing else. Link targets resolve relative to the linking file, and a
  target that escapes the vault is recorded (`unresolved_links`, or a refused image class) rather than opened — `media::escapes_through_a_link` is the
  check, and it runs on Windows path prefixes too, because a spelled-out `\??\` target escapes for the same reason a `..` does.
- **It never opens a socket on its own.** There is no daemon, no listener, no telemetry, no update check. The only code path that can dial out is a
  model download (`np models fetch`, and auto-fetch from `np embed` / `np setup` / first hybrid `np search`).
- **A download is bounded by a compiled-in list.** Only ids this build *pins* can be fetched, the transfer is verified against a compiled-in SHA-256
  before anything is put in place, and a partial or wrong file is never used. A name that happens to appear in a config file cannot cause a transfer.
  Switch it off with `--no-auto-fetch`, `[models] auto_fetch = false`, or `NP_NO_AUTO_FETCH=1`.
- **Logs and machine output name an origin's *kind*, never its host.** No URL appears in a report, a log line, or a test fixture's expected output —
  so a support bundle someone pastes into an issue cannot leak an internal mirror address.
- **`np serve` speaks over stdin/stdout only.** There is no address to bind and nothing for another user on the machine to connect to; whoever can
  write to that process's stdin already owns everything it can read, which is the same trust boundary as running `np` yourself.
- **Vector formats are refused on purpose.** `.svg`, `.drawio`, `.excalidraw` and `.pdf` links are reported as `unsupported-format`: rasterising SVG
  means a decoder that must disable external references and DTDs — *an SVG is a file that asks to fetch things* — and that dependency and attack
  surface buy a capability nobody has asked for. PNG and JPEG are accepted by extension **and** content signature, so a mislabelled file is refused too.

## What we would treat as a vulnerability

A report along these lines is a bug and we will fix it, in rough order of how much it worries us:

1. **Reading or writing outside the workspace** without being told to — a path traversal through a link, an image reference, a moved file, or a
   symlink that resolves out of the vault.
2. **Accepting an artifact whose bytes do not match its pinned digest**, or a code path that fetches an id outside the pin table.
3. **A crash or unbounded resource use on hostile note content** — a file that panics the parser, exhausts memory, or spins forever during a scan or
   search. `np` already refuses oversized embedding chunks rather than truncating them; a way to defeat that counts.
4. **Corruption of an existing index** by a legitimate run: two writers on one vault are meant to exclude (F44), and a loser is meant to stop *before*
   the database commits rather than between "committed" and "files written".
5. **A machine-readable state that is only reachable as prose.** This is not a confidentiality bug, but `--json` and `np serve` are interfaces people
   build on: if a state (`locked`, `offline`, `reason`) can only be learned by matching a sentence, an application silently mis-handles it.

## What is out of scope

- **Anyone who can already read your files.** The database, `INDEX.md` and the model cache are ordinary files with ordinary permissions; someone with
  your user account has your notes. Encryption at rest is not a feature `np` has.
- **A deliberately hostile vault shared to you by an attacker**, beyond the specific guarantees above. `np` assumes the workspace it is pointed at is
  yours to index; if you index untrusted content, treat its links and titles as untrusted text when you render them somewhere else.
- **The inference runtime's own vulnerabilities.** llama.cpp is vendored at a pinned tag (`v0.8.0`) with its hash recorded in
  `THIRD_PARTY_NOTICES.md`; a flaw upstream is an upstream fix plus a version bump here, which we will do promptly.
- **Anything the platform refuses us.** Windows paths that cannot be represented lossily are *refused* rather than guessed at, and such a refusal is
  the designed behaviour, not an escape.

## How to report

Use GitHub's **private vulnerability reporting** from the Security tab of this repository, or open a normal issue if you think the problem is a
correctness bug rather than a security one — most of what people find here is that, and `docs/backlog-m2-followups.md` shows how we treat those: each
entry names the measurement that found it, and several record that the original report's diagnosis was wrong.

Please include the version (`np --version`) and the operating system. Note that release binaries are built without CI grading (see
`docs/ops-actions-minutes.md`) and that the Windows asset is a mingw cross-build rather than MSVC — both matter when reproducing something.
