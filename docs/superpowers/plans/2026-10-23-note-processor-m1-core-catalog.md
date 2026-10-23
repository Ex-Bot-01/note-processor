# note-processor M1 — Core, Registry and Catalog Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** Build the deterministic core that tracks a markdown workspace and writes `INDEX.md` — walk, detect changes by content hash identity, parse notes into SQLite, resolve links, extract open/done/paused item candidates, render a byte-stable catalog.

**Architecture:** One dependency-light Rust library (`np-core`) does discovery, change classification, parsing and catalog rendering; a thin binary (`np`) drives it. SQLite holds the registry and derived rows. No network, no LLM, no UI in this milestone — those are M2–M6 and must not be scaffolded here.

**Tech Stack:** Rust stable (edition 2021), `walkdir`, `sha2`, `rusqlite` (bundled), `serde` + `toml`, `clap` (derive), `thiserror`, `anyhow`, `regex`, `tempfile` (dev). CI matrix ubuntu + windows.

**Spec:** `docs/superpowers/specs/2026-10-23-note-processor-design.md` — read §5, §7, §8.3, §8.7, §9 (Storage), §10, §11 before starting. This plan implements its build-order step 1.

## Global Constraints

- Rust stable, edition 2021; `cargo fmt --check` and `cargo clippy --all-targets -- -D warnings` clean on every task.
- `np-core` makes **no** network calls and has no HTTP, LLM or embedding dependency (spec §5, §9).
- Never write into a note body. The only file this milestone writes is `INDEX.md` / `INDEX-<range>.md` at the workspace root (spec §7).
- Read-only fidelity: files are never rewritten; CRLF and non-UTF-8 bytes are tolerated on read and preserved byte-for-byte on disk.
- Internal paths are `/`-normalised `String`s (`rel`); display spelling is the original OS string. Windows path comparison is case-insensitive via `str::to_lowercase()` (spec §11).
- Generated files use `\n` line endings on every platform.
- Status markers default (spec §8.7), loaded from config: done = `✅`, `- [x]`, `done`, `finished`, `erledigt`, `fertig`, `abgeschlossen`; paused = `paused`, `pausiert`, `on hold`, `gestoppt`, `verschoben`, `vertagt`; open = everything else, including `- [ ]`.
- Catalog shard threshold default 600 entries per file; continuation lines indented two spaces (spec §2).
- One active workspace; its DB lives in the OS data dir (`$XDG_DATA_HOME`/`%LOCALAPPDATA%`), keyed by a hash of the absolute workspace path (spec §11).
- Excluded from tracking: `INDEX.md`, `INDEX-*.md`, `processed/`, `.np/`, any dotfile or dot-directory.
- Conventional commits, one commit per task step where a step is the commit point.

## Review Focus

The spec is silent on these inputs; each line gets its test in the owning task, and the expected behaviour is stated here so no one has to guess:

1. **Non-UTF-8 bytes** — in file *content* (a latin-1 umlaut) or in a *filename* on Linux: scan records it, decodes lossily for parsing, flags `encoding = "lossy"`, and never crashes nor rewrites the file.
2. **Ambiguous wikilink targets** — two notes whose basenames collide, or paths differing only by case on Windows: the link is stored unresolved (`target_note_id IS NULL`) rather than guessed, and counted in `unresolved_links`.
3. **CRLF, mixed endings, no trailing newline** — item/link/tag line numbers must be identical to the LF version of the same content; a file with `\r\n` stays `\r\n` on disk after any number of scans.
4. **Degenerate notes** — zero-byte file, frontmatter-only file, unterminated `---` block, and a 5 MB single-line file: each gets a catalog entry (title falls back to filename) and no panic, no timeout, no skipped note.
5. **File mutated or deleted between walk and read** — the scan skips that file, reports it under `skipped`, keeps the previous registry row, and finishes the rest of the corpus.

---

### Task 1: Cargo workspace, `np` binary, CI matrix

**Files:**
- Create: `Cargo.toml`, `.gitignore`, `.github/workflows/ci.yml`, `crates/np-core/Cargo.toml`, `crates/np-core/src/lib.rs`, `crates/np-cli/Cargo.toml`, `crates/np-cli/src/main.rs`
- Test: `crates/np-core/tests/version.rs`

**Interfaces:**
- Consumes: nothing.
- Produces: crate names `np-core` (lib) and `np-cli` (bin `np`); `np_core::M_VERSION: &str` = workspace version.

- [ ] **Step 0: Toolchain preflight** — `cargo --version && rustup component list --installed | grep -q clippy`; if either fails, install with `curl --proto '=https' --tlsv1.2 -sSf https://sh.rustup.rs | sh -s -- -y --profile minimal --default-toolchain stable` then `rustup component add clippy rustfmt`. Rust is **not** present in this environment yet (spec §5).
- [ ] **Step 1: Write the failing test** — `crates/np-core/tests/version.rs`:

```rust
#[test]
fn version_is_semver() {
    let parts: Vec<&str> = np_core::M_VERSION.split('.').collect();
    assert_eq!(parts.len(), 3, "expected MAJOR.MINOR.PATCH");
    assert!(parts.iter().all(|p| p.chars().all(|c| c.is_ascii_digit())));
}
```

- [ ] **Step 2: Run it to verify it fails** — `cargo test -p np-core version_is_semver` → compile error, `M_VERSION` not found.
- [ ] **Step 3: Scaffold.** Workspace `Cargo.toml` with `members = ["crates/np-core", "crates/np-cli"]`, `[workspace.package] version = "0.1.0"`, `edition = "2021"`, and `[workspace.dependencies]` listing the Tech Stack crates once (children use `workspace = true`). `np-core/src/lib.rs`: `pub const M_VERSION: &str = env!("CARGO_PKG_VERSION");`. `np-cli/src/main.rs`: clap `#[command(version, about)]` with a placeholder `Scan` subcommand that prints `not implemented` and exits 2. `.gitignore`: `/target`, `/ui/node_modules`, `/.superpowers`, `*.sqlite*`. CI: two jobs (`ubuntu-latest`, `windows-latest`) running `cargo fmt --check`, `cargo clippy --all-targets -- -D warnings`, `cargo test --workspace`.
- [ ] **Step 4: Run tests** — `cargo test --workspace` → PASS.
- [ ] **Step 5: Verify the binary** — `cargo run -p np-cli --bin np -- scan .` → exit code 2, stderr contains `not implemented`.
- [ ] **Step 6: Commit** — `git add -A && git commit -m "chore: cargo workspace, np binary skeleton, cross-platform CI"`

### Task 2: Config file, data dir, known workspaces

**Files:**
- Create: `crates/np-core/src/config.rs`, `crates/np-core/tests/config.rs`
- Modify: `crates/np-core/src/lib.rs` (add `pub mod config;`)

**Interfaces:**
- Consumes: `np_core::M_VERSION`.
- Produces:
  - `pub struct CatalogConfig { pub max_entries_per_file: usize, pub continuation_indent: usize }` (defaults 600, 2)
  - `pub struct MarkerConfig { pub done: Vec<String>, pub paused: Vec<String> }` with `MarkerConfig::default()` holding the Global Constraints list verbatim, lowercase-matched
  - `pub struct Config { pub data_dir: PathBuf, pub catalog: CatalogConfig, pub markers: MarkerConfig }`
  - `pub fn config_file_path() -> Option<PathBuf>` — `$XDG_CONFIG_HOME|~/.config`/`note-processor/config.toml`, `%APPDATA%` on Windows
  - `impl Config { pub fn load(override_: Option<&Path>) -> Result<Config>; pub fn workspace_db(&self, workspace: &Path) -> PathBuf }`
  - `pub struct WorkspaceRegistry; impl WorkspaceRegistry { pub fn open(data_dir: &Path) -> Result<Self>; pub fn record(&mut self, workspace: &Path, now: i64) -> Result<()>; pub fn recent(&self) -> Result<Vec<(String, i64)>> }` (SQLite `data_dir/workspaces.sqlite`, most recent first)

- [ ] **Step 1: Write failing tests** in `tests/config.rs`:
  - `defaults_match_spec` — `MarkerConfig::default().done` contains `"✅"`, `"- [x]"`, `"erledigt"`; `.paused` contains `"pausiert"`; catalog defaults are 600 and 2.
  - `toml_overrides_and_keeps_defaults` — a config file setting only `catalog.max_entries_per_file = 50` yields 50 and *keeps* the default markers.
  - `workspace_db_is_stable_and_unique` — same path twice → same `PathBuf`; `/a/notes` vs `/a/other` differ; filename ends `.sqlite`.
- [ ] **Step 2: Run** `cargo test -p np-core config` → FAIL (module missing).
- [ ] **Step 3: Implement** `config.rs`. TOML with `#[serde(default)]` on every struct so partial files keep defaults; `data_dir` default from `$XDG_DATA_HOME` else `$HOME/.local/share`, Windows `%LOCALAPPDATA%`; `workspace_db` = `data_dir/workspaces/<sha256(abs_path)[0..16]>.sqlite`. Return `thiserror` enum `ConfigError { Io, Parse, MissingField(&'static str) }`.
- [ ] **Step 4: Run** → PASS.
- [ ] **Step 5: Commit** — `git commit -am "feat(core): config file, data dir, known-workspace registry"`

### Task 3: Workspace walker with exclusions and deterministic order

**Files:**
- Create: `crates/np-core/src/walk.rs`, `crates/np-core/tests/walk.rs`
- Modify: `crates/np-core/src/lib.rs`

**Interfaces:**
- Consumes: nothing.
- Produces:
  - `pub struct DiscoveredFile { pub rel: String, pub case_key: String, pub abs: PathBuf, pub size: u64, pub mtime: i64, pub is_markdown: bool }`
  - `pub fn normalize_rel(os: &Path) -> (String, String)` → (`rel` with `/`, display original), plus `case_key = rel.to_lowercase()`
  - `pub fn to_open_path(abs: &Path) -> PathBuf` — on Windows prefixes `\\?\` when the path is ≥ 260 chars and not already prefixed; identity elsewhere (spec §11). Every later file open goes through it.
  - `pub struct WalkOptions { pub extra_excludes: Vec<String> }`
  - `pub fn walk(root: &Path, opts: &WalkOptions) -> Result<Vec<DiscoveredFile>>` — sorted by `case_key`, `follow_links(false)`

- [ ] **Step 1: Write failing tests** in `tests/walk.rs` (build trees with a `tree(&[("a/b.md", "…")])` helper into `tempfile::TempDir`):
  - `sorted_and_normalized` — nested dirs come back `case_key`-ascending, every `rel` uses `/`, no leading `./`.
  - `excludes_derived_and_hidden` — `INDEX.md`, `INDEX-a-m.md`, `processed/x.md`, `.np/state.sqlite`, `.git/config` are absent; `notes/INDEX-old.md` **is** present (only the exact catalog names are excluded).
  - `symlink_loop_does_not_hang` — a symlink to its own parent directory terminates and contributes nothing.
  - Review Focus 1a: `non_utf8_filename_recorded` (`#[cfg(unix)]`, build the name from `[0xffu8]`) → one entry, `rel` non-empty, no error.
  - `long_path_gets_prefix_only_when_needed` — pure, no filesystem: a 300-char Windows path (`#[cfg(windows)]`-independent because it takes `&str` and a `windows: bool`) comes back prefixed once; a 20-char path unchanged; an already-prefixed path unchanged.
- [ ] **Step 2: Run** `cargo test -p np-core walk` → FAIL.
- [ ] **Step 3: Implement** `walk.rs` with `walkdir::WalkDir::root_first(true)`; skip names via a `fn is_excluded(name: &str) -> bool` covering the Global Constraints exclusion list; `mtime` = modified time as epoch seconds (`0` when the platform hides it).
- [ ] **Step 4: Run** → PASS.
- [ ] **Step 5: Commit** — `git commit -am "feat(core): deterministic recursive workspace walker"`

### Task 4: Content hash and change classification

**Files:**
- Create: `crates/np-core/src/hash.rs`, `crates/np-core/src/change.rs`, `crates/np-core/tests/change.rs`

**Interfaces:**
- Consumes: `walk::DiscoveredFile`.
- Produces:
  - `pub fn sha256_file(p: &Path) -> Result<String>` (lowercase hex), `pub fn sha256_bytes(b: &[u8]) -> String`
  - `pub struct Snapshot { pub rel: String, pub case_key: String, pub hash: String, pub size: u64, pub mtime: i64 }`
  - `pub enum Change { Added, Modified, Moved { from: String }, Removed }` with `Debug, PartialEq`
  - `pub fn classify(known: &[Snapshot], found: &[Snapshot], case_sensitive: bool) -> Vec<(String, Change)>` — output sorted by `rel`; cheap trigger (size+mtime) gates the hash, identity is the hash; `case_sensitive` is `!cfg!(windows)` at the call site
- [ ] **Step 1: Write failing tests** in `tests/change.rs` (pure, no filesystem beyond `sha256_bytes`):
  - `unchanged_files_produce_no_change` — identical vectors → empty result.
  - `same_hash_new_path_is_moved` — `a/x.md` → `b/x.md`, same hash → `Moved { from: "a/x.md" }` and no `Removed`.
  - `case_only_rename_on_windows_is_not_a_move` — when `cfg!(windows)` semantics are forced via a `case_sensitive: bool` argument, differing only in case yields no change.
  - `content_change_is_modified`, `missing_file_is_removed`, `new_file_is_added`.
  - Review Focus 5: `stale_hash_never_produced_when_read_fails` — a `found` snapshot with `hash = String::new()` is dropped from the result entirely (the caller reports it under `skipped`).
- [ ] **Step 2: Run** → FAIL.
- [ ] **Step 3: Implement.** `classify` keeps its cheap path: compare `(size, mtime)` first and emit no change when both match; otherwise hash. Identity rule from spec §5 — same hash + different `case_key` = Moved; same `case_key` + different hash = Modified.
- [ ] **Step 4: Run** → PASS.
- [ ] **Step 5: Commit** — `git commit -am "feat(core): content-hash identity and change classification"`

### Task 5: SQLite schema, persistence, stale-and-rejoin semantics

**Files:**
- Create: `crates/np-core/src/db.rs`, `crates/np-core/tests/db.rs`
- Modify: `crates/np-core/src/lib.rs`

**Interfaces:**
- Consumes: `change::Change`, `hash::Snapshot`, `config::Config`.
- Produces:
  - `pub struct Db(pub rusqlite::Connection)` with `Db::open(path: &Path) -> Result<Db>` (creates dirs, `PRAGMA journal_mode=WAL; foreign_keys=ON`) and `Db::in_memory() -> Result<Db>`
  - Tables created by an ordered `MIGRATIONS: &[&str]` recorded in `schema_version`: `files(rel TEXT PK, case_key TEXT UNIQUE, abs TEXT, hash TEXT, size INTEGER, mtime INTEGER, state TEXT CHECK(state IN ('ok','stale')), last_seen INTEGER)`; `notes(id INTEGER PK, rel TEXT REFERENCES files(rel), title TEXT, title_source TEXT, encoding TEXT, word_count INTEGER, body TEXT)`; `headings(note_id, level, text, line)`; `tags(note_id, tag)`; `links(src_note_id, dst_note_id NULL, kind TEXT CHECK(kind IN ('wiki','md','attachment')), target TEXT, line INTEGER, origin TEXT CHECK(origin IN ('prose','approved')))`; `items(note_id, text, line, state TEXT CHECK(state IN ('open','done','paused')), marker TEXT)`; `meta(key TEXT PK, value TEXT)`. Indexes on `files(case_key)`, `notes(rel)`, `links(dst_note_id)`, `tags(tag)`.
  - `pub fn upsert_file(db: &Db, s: &Snapshot, abs: &Path, now: i64) -> Result<()>` (sets `state='ok'`)
  - `pub fn apply_move(db: &Db, from: &str, to: &str, now: i64) -> Result<()>` — rewrites `files.rel`, `notes.rel`, keeps ids and links
  - `pub fn mark_stale(db: &Db, rel: &str) -> Result<()>` — sets `state='stale'`; rows are never deleted
  - `pub fn note_counts(db: &Db) -> Result<NoteCounts>` where `pub struct NoteCounts { pub notes: i64, pub links: i64, pub unresolved_links: i64, pub items_open: i64, pub items_done: i64, pub items_paused: i64 }`

- [ ] **Step 1: Write failing tests** in `tests/db.rs`:
  - `migrations_are_idempotent` — `Db::open` twice on the same file, `schema_version` has one row per migration.
  - `move_keeps_note_identity` — insert note + outgoing link, `apply_move`, then the link and `notes.id` are unchanged and `rel` is the new path.
  - `stale_then_reappear_rejoins` — `mark_stale` then `upsert_file` with the same hash restores `state='ok'` on the same row (`id`-stable via `SELECT rowid`).
  - `case_key_is_unique` — inserting two files whose `case_key`s collide fails with a constraint error, not a silent overwrite.
- [ ] **Step 2: Run** → FAIL.
- [ ] **Step 3: Implement** `db.rs`. No transaction helper: Task 10 wraps its writes in one `Connection::unchecked_transaction()`.
- [ ] **Step 4: Run** → PASS.
- [ ] **Step 5: Commit** — `git commit -am "feat(core): sqlite registry schema with move and stale semantics"`

### Task 6: Markdown parse — frontmatter, title, headings, tags

**Files:**
- Create: `crates/np-core/src/parse.rs`, `crates/np-core/tests/parse.rs`, `tests/fixtures/notes/*.md`

**Interfaces:**
- Consumes: nothing (pure text in, structs out).
- Produces:
  - `pub enum TitleSource { Frontmatter, Heading, Filename }`
  - `pub struct Heading { pub level: u8, pub text: String, pub line: usize }`
  - `pub struct NoteDoc { pub title: String, pub title_source: TitleSource, pub frontmatter: bool, pub headings: Vec<Heading>, pub tags: Vec<String>, pub words: usize }`
  - `pub fn parse_markdown(raw: &str, fallback_title: &str) -> NoteDoc` — lines are 1-based, split on `\n` with a trailing `\r` trimmed for content only
- [ ] **Step 1: Write failing tests** in `tests/parse.rs`:
  - `title_precedence` — frontmatter `title:` wins over first `#`, which wins over `fallback_title`; `title_source` reflects which.
  - `frontmatter_recognised_subset` — `title:`, `tags: [a, b]` and a block `tags:` list are read; unknown keys ignored; `frontmatter = true` only when the file's first line is exactly `---`.
  - `headings_with_line_numbers` — fenced code containing `# fake` yields no heading.
  - `tags_from_body_hash_syntax` — `#vendor #contract` in prose, deduplicated, lowercase, and a markdown link `[see](#anchor)` does **not** become a tag.
  - Review Focus 3: `crlf_matches_lf` — the same content with `\r\n` yields byte-identical `headings` line numbers and tags.
  - Review Focus 4: `degenerate_inputs` — `""`, `"---\ntitle: x"` (unterminated), and a 5 MB single-line string each return without panic; empty file's title is the fallback.
- [ ] **Step 2: Run** → FAIL.
- [ ] **Step 3: Implement** `parse.rs`. Hand-rolled frontmatter subset (no YAML dependency) per spec §9's "no surprise deps"; heading regex `^(#{1,6})\s+(.*)$` outside fenced blocks tracked by a ``` fence toggle; tag regex `(?<![\w/])#([\p{L}][\p{L}\p{N}_/-]{1,40})`.
- [ ] **Step 4: Run** → PASS.
- [ ] **Step 5: Commit** — `git commit -am "feat(core): markdown frontmatter, title, heading and tag parsing"`

### Task 7: Link extraction and resolution

**Files:**
- Create: `crates/np-core/src/links.rs`, `crates/np-core/tests/links.rs`
- Modify: `crates/np-core/src/parse.rs` (add `pub links: Vec<RawLink>` to `NoteDoc` and fill it)

**Interfaces:**
- Consumes: `parse::NoteDoc`, `db::Db`.
- Produces:
  - `pub enum LinkKind { Wiki, Md, Attachment }`
  - `pub struct RawLink { pub target: String, pub kind: LinkKind, pub line: usize }` (target stripped of `|alias`, `#heading`, `^block`)
  - `pub fn extract_links(raw: &str) -> Vec<RawLink>` — `[[...]]`, `[t](path.md)`, `[t](image.png)`; nothing inside fenced code
  - `pub struct ResolvedLink { pub src_rel: String, pub target: String, pub kind: LinkKind, pub line: usize, pub dst_rel: Option<String> }`
  - `pub fn resolve(corpus: &BTreeMap<String /*case_key*/, String /*rel*/>, links: &[(String, Vec<RawLink>)]) -> Vec<ResolvedLink>` — exact `rel`, then case-insensitive `case_key`, then unique basename; ambiguous or unknown → `dst_rel = None`
  - `pub fn store_links(db: &Db, links: &[ResolvedLink], origin: &str) -> Result<()>` — unresolved rows keep `dst_note_id NULL` and `target` for later placement suggestions
- [ ] **Step 1: Write failing tests** in `tests/links.rs`:
  - `wikilink_forms_normalised` — `[[Note A|alias]]`, `[[Note A#section]]`, `[[note a]]` all yield target `Note A` (alias/heading stripped, original case kept).
  - `attachment_vs_md_by_extension` — `.md` → `Md`; `.png`, `.pdf`, `.jpg` → `Attachment`; extension-less → `Wiki`-less skip with no row.
  - `resolution_order` — exact path beats basename; unique basename resolves across directories.
  - Review Focus 2: `ambiguous_basename_is_unresolved` — `x/a.md` and `y/a.md`, link `[[a]]` → `dst_rel = None`; after storing, `note_counts().unresolved_links == 1`.
  - Review Focus 3: `line_numbers_stable_across_endings` — LF and CRLF fixtures give the same `line`.
- [ ] **Step 2: Run** → FAIL.
- [ ] **Step 3: Implement.** Case-insensitive lookup uses the corpus map keyed by `case_key` built in Task 10; basename index is `HashMap<String, Vec<String>>` and a `Vec` longer than one entry means ambiguous.
- [ ] **Step 4: Run** → PASS.
- [ ] **Step 5: Commit** — `git commit -am "feat(core): link extraction with ambiguity-safe resolution"`

### Task 8: Item candidates and status markers

**Files:**
- Create: `crates/np-core/src/items.rs`, `crates/np-core/tests/items.rs`

**Interfaces:**
- Consumes: `config::MarkerConfig`.
- Produces:
  - `pub enum ItemState { Open, Done, Paused }` with `as_str()` → `"open"|"done"|"paused"`
  - `pub struct ItemCandidate { pub text: String, pub line: usize, pub state: ItemState, pub marker: String }`
  - `pub fn extract_items(raw: &str, markers: &MarkerConfig) -> Vec<ItemCandidate>` — bullet/numbered list items and standalone lines that carry a checkbox, a tick, or a configured keyword; matching is case-insensitive on the lowercased line
- [ ] **Step 1: Write failing tests** in `tests/items.rs`:
  - `checkbox_states` — `- [ ] buy milk` → Open with marker `"[ ]"`; `- [x] …` → Done.
  - `tick_makes_done` — `buy milk ✅` → Done, marker `"✅"`.
  - `german_and_english_keywords` — `vertrag angefragt — erledigt` → Done; `vendor contract paused` → Paused; markers recorded verbatim from config.
  - `no_marker_means_no_item_row` — a paragraph with no list marker and no keyword yields nothing (Open comes from an *item*, not from prose: spec §8.7 "open is the absence of a marker on an item").
  - Review Focus 4: `empty_and_huge_input` — `""` → empty; 5 MB single line → returns without panic in under 200 ms on a release build assertion (`#[ignore]`d by default, run in Task 10's fixture pass).
- [ ] **Step 2: Run** → FAIL.
- [ ] **Step 3: Implement** `items.rs`. Order of precedence per line: checkbox → tick/keyword scan; first match wins; `text` is the line trimmed of list bullet and marker.
- [ ] **Step 4: Run** → PASS.
- [ ] **Step 5: Commit** — `git commit -am "feat(core): item candidate extraction with configurable markers"`

### Task 9: Catalog render, wrap and shard

**Files:**
- Create: `crates/np-core/src/catalog.rs`, `crates/np-core/tests/catalog.rs`, `tests/golden/INDEX.md`, `tests/golden/INDEX-a-m.md`

**Interfaces:**
- Consumes: `config::CatalogConfig`.
- Produces:
  - `pub struct CatalogEntry { pub title: String, pub rel: String, pub size_human: String, pub tags: Vec<String>, pub out: u32, pub inc: u32 }`
  - `pub fn size_human(bytes: u64) -> String` — `9 B`, `1.4 KB`, `2.1 MB` (one decimal from 10 KB up)
  - `pub fn render(entries: &[CatalogEntry], cfg: &CatalogConfig) -> Vec<(String, String)>` → `(filename, contents)`, always including `INDEX.md` as the entry point; shards are `INDEX-<first>-<last>.md` by title's lowercase first letter
  - Entry line format (exact, spec §14): `- [[{title}]]({rel}) · {size_human} · {tags} · →{out} ←{inc}` with `· #tag` joined tags or `· —` when none; a wrapped entry continues on lines indented two spaces, breaking before the token that would exceed 100 columns
- [ ] **Step 1: Write failing tests** in `tests/catalog.rs`:
  - `entry_format_exact` — one known note renders the documented line byte-for-byte.
  - `alphabetized_case_insensitive_with_tiebreak` — ordering by lowercased title, ties broken by `rel`; output is stable across input permutations (shuffle with a fixed seed).
  - `wraps_at_hundred_columns` — an entry with twelve tags produces continuation lines each starting with two spaces and none longer than 100 columns.
  - `shards_over_threshold_and_index_links_them` — 7 entries with `max_entries_per_file = 3` → `INDEX.md` plus three shard files; `INDEX.md` lists each shard as `- [a–m](INDEX-a-m.md)` and contains no note lines.
  - `byte_stable` — rendering the same entries twice returns identical strings, and every file ends with exactly one `\n`, uses no `\r`.
  - Golden: fixture corpus render equals `tests/golden/INDEX.md` (and its shard) after normalising nothing — compare raw bytes.
- [ ] **Step 2: Run** → FAIL.
- [ ] **Step 3: Implement** `catalog.rs`; header of `INDEX.md` is fixed text: `# INDEX` then `_generated by note-processor; do not edit_` then a blank line, so regeneration diffs are noise-free (spec §10).
- [ ] **Step 4: Run** → PASS.
- [ ] **Step 5: Commit** — `git commit -am "feat(core): byte-stable INDEX.md render with wrapping and shards"`

### Task 10: `np scan` end-to-end, idempotency, fixture corpus

**Files:**
- Create: `crates/np-core/src/lib.rs` (`scan`), `crates/np-core/tests/scan.rs`, `tests/fixtures/corpus/**` (≥12 messy notes incl. German text, appended sections, orphans, near-duplicates, one CRLF file, one umlaut filename, one zero-byte, one frontmatter-only)
- Modify: `crates/np-cli/src/main.rs`

**Interfaces:**
- Consumes: every module above.
- Produces:
  - `pub struct ScanReport { pub added: usize, pub modified: usize, pub moved: usize, pub removed: usize, pub unchanged: usize, pub skipped: Vec<String>, pub notes: i64, pub unresolved_links: i64, pub index_files: Vec<String>, pub elapsed_ms: u128 }` (serde `Serialize`)
  - `pub fn scan(root: &Path, cfg: &Config) -> Result<ScanReport>` — walk → classify → hash/read → parse → store → resolve links → items → render catalog → write index files (atomic temp+rename), then `WorkspaceRegistry::record`. DB path comes from `cfg.workspace_db(root)`.
  - `#[doc(hidden)] pub fn scan_with_inject(root: &Path, cfg: &Config, after_walk: impl FnOnce(&Path)) -> Result<ScanReport>` — the seam Task 10's race test uses; `scan` calls it with a no-op closure
  - CLI: `np scan <WORKSPACE> [--json]`, exit 0 on success, 1 with the error message on failure

- [ ] **Step 1: Write failing tests** in `tests/scan.rs`:
  - `first_scan_reports_all_added` — fixture corpus → `added == number of .md files`, `notes > 0`, `index_files == ["INDEX.md"]`.
  - Review Focus idempotency (spec §10): `second_scan_is_a_no_op` — second report has all zero changes and unchanged counts equal, and `INDEX.md` bytes are identical after both runs.
  - `writes_only_index_files` — before/after directory hashes of everything except `INDEX*.md` are equal: originals untouched byte-for-byte.
  - Review Focus 5: `mutated_during_walk_is_skipped` — inject a file whose `mtime` differs between walk and read (test-only hook `scan_with_inject(root, cfg, after_walk: impl FnOnce(&Path))` deleting one file) → that path in `skipped`, report still succeeds, its old row keeps `state='ok'`.
  - Review Focus 1b: `non_utf8_content_is_lossy_not_fatal` — a latin-1 fixture parses with `encoding = "lossy"` in `notes`.
  - Review Focus 4: `every_fixture_gets_an_entry` — catalog entry count equals tracked markdown file count, including the zero-byte and frontmatter-only notes.
  - Spec §5/§10: `non_markdown_files_are_tracked_not_parsed` — a `.png` fixture gets a `files` row, no `notes` row, and no catalog entry.
  - `deleted_file_marks_stale_and_keeps_history` — delete a fixture, rescan → `removed == 1`, its note rows still present with `files.state='stale'`, and it disappears from `INDEX.md`.
- [ ] **Step 2: Run** → FAIL.
- [ ] **Step 3: Implement** `scan()` as the single orchestrator (no new public types beyond the report); write index files with `fs::write` to `INDEX.md.tmp` then `rename`, and delete shard files no longer produced (tracked in `meta('index_files')`).
- [ ] **Step 4: Run** → PASS, and confirm `cargo test --workspace` plus the `#[ignore]`d huge-file test via `cargo test --release -- --ignored`.
- [ ] **Step 5: Verify by hand** — `cargo run -p np-cli --bin np -- scan tests/fixtures/corpus --json` prints the report; open `tests/fixtures/corpus/INDEX.md` and read three entries for sanity (title, size, tags, link counts).
- [ ] **Step 6: Commit** — `git commit -am "feat: np scan end-to-end with idempotent catalog regeneration"`

---

## Not in this plan (later milestones)

Embeddings and clustering (M2), grounded compression into `processed/` (M2b), item timelines and tick extraction (M3), the daemon, API, watcher and scheduler (M4), the web UI (M5), the Tauri shell and installers (M6). Do not create empty modules, feature flags or DB tables for them; M1's schema is extended by a new migration when each arrives.

The spec's entry line also carries `cluster` and status columns. Those arrive with M2 and M3 — Task 9 renders the M1 subset (`title · path · size · tags · links`) and its format string is the single place the later columns get added.
