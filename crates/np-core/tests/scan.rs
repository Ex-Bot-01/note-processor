//! End-to-end `scan`: fixture corpus in, registry rows and `INDEX.md` out.
//!
//! Every case works on a **copy** of `tests/fixtures/corpus` inside a tempdir, because a scan
//! writes its catalog into the workspace it is pointed at and one case deletes a note. The
//! database goes to a `data_dir` inside the same tempdir, so no test touches a real user data
//! dir, and each tempdir therefore also gets its own workspace database — scans never inherit
//! state from each other through a shared file.

use std::collections::BTreeMap;
use std::fs;
use std::path::{Path, PathBuf};

use np_core::catalog;
use np_core::config::{CatalogConfig, Config};
use np_core::db::{self, Db, NoteCounts};
use np_core::hash::sha256_file;
use np_core::parse::PARSE_VERSION;
use np_core::{scan, scan_with_inject};
use tempfile::TempDir;

/// Markdown notes in the committed corpus (everything but `scan.png`).
const CORPUS_NOTES: i64 = 16;
/// Non-markdown files in the corpus: one image, tracked but never parsed.
const CORPUS_ATTACHMENTS: i64 = 1;
/// Tracked files of every kind.
const CORPUS_FILES: i64 = CORPUS_NOTES + CORPUS_ATTACHMENTS;

/// A workspace copy plus the config that points its data dir at the same tempdir.
struct Workspace {
    /// Kept alive so the directories outlive the scans run against them.
    _tmp: TempDir,
    root: PathBuf,
    cfg: Config,
}

impl Workspace {
    /// The workspace database, opened the way `scan` opens it.
    fn db(&self) -> Db {
        Db::open(&self.cfg.workspace_db(&self.root)).expect("test setup: open workspace db")
    }
}

/// The committed fixture corpus, next to the repository's `Cargo.toml`.
fn corpus() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR"))
        .parent()
        .and_then(Path::parent)
        .expect("np-core sits two levels under the workspace root")
        .join("tests")
        .join("fixtures")
        .join("corpus")
}

/// A copy of the corpus in a fresh tempdir, with `data_dir` beside it.
fn workspace() -> Workspace {
    let tmp = TempDir::new().expect("test setup: tempdir");
    let root = tmp.path().join("ws");
    let source = corpus();
    assert_no_generated_catalog(&source);
    copy_tree(&source, &root);
    let cfg = Config {
        data_dir: tmp.path().join("data"),
        ..Config::default()
    };
    Workspace {
        _tmp: tmp,
        root,
        cfg,
    }
}

/// Fail loudly when the source corpus holds a generated catalog of its own.
///
/// [`copy_tree`] copies whatever is in the source directory, so an `INDEX.md` left behind by
/// `np scan tests/fixtures/corpus` arrives in every copy and silently adds one file and one note to
/// counts these cases assert as constants. The ignore rule in `.gitignore` keeps it out of a commit;
/// this keeps it out of a red herring.
fn assert_no_generated_catalog(corpus: &Path) {
    let leftovers: Vec<String> = fs::read_dir(corpus)
        .expect("test setup: read source corpus")
        .flatten()
        .map(|entry| entry.file_name().to_string_lossy().into_owned())
        .filter(|name| {
            name.starts_with("INDEX") && (name.ends_with(".md") || name.ends_with(".tmp"))
        })
        .collect();
    assert!(
        leftovers.is_empty(),
        "`{}` holds generated catalog files: {leftovers:?}",
        corpus.display()
    );
}

/// A workspace holding exactly `files` (`rel` → contents), sharded at `per_file` entries.
///
/// For the cases the fixture corpus cannot express: a two-note graph whose target is deleted, and a
/// corpus small enough that `max_entries_per_file` really splits it into shards.
fn scratch(files: &[(&str, &str)], per_file: usize) -> Workspace {
    let tmp = TempDir::new().expect("test setup: tempdir");
    let root = tmp.path().join("ws");
    fs::create_dir_all(&root).expect("test setup: create scratch workspace");
    for (rel, contents) in files {
        let path = root.join(rel);
        fs::create_dir_all(path.parent().unwrap_or(root.as_path()))
            .expect("test setup: create note directory");
        fs::write(&path, contents).expect("test setup: write scratch note");
    }
    let cfg = Config {
        data_dir: tmp.path().join("data"),
        catalog: CatalogConfig {
            max_entries_per_file: per_file,
            ..CatalogConfig::default()
        },
        ..Config::default()
    };
    Workspace {
        _tmp: tmp,
        root,
        cfg,
    }
}

/// `from` copied to `to`, recursively, byte for byte.
fn copy_tree(from: &Path, to: &Path) {
    fs::create_dir_all(to).expect("test setup: create copy dir");
    for entry in fs::read_dir(from).expect("test setup: read source dir") {
        let entry = entry.expect("test setup: readdir entry");
        let target = to.join(entry.file_name());
        if entry.file_type().expect("test setup: file type").is_dir() {
            copy_tree(&entry.path(), &target);
        } else {
            fs::copy(entry.path(), &target).expect("test setup: copy fixture file");
        }
    }
}

/// Every non-generated file under `root` as `rel` → bytes, where "generated" is what this
/// program writes itself (`INDEX.md`, its shards, their `.tmp` form).
fn inputs(root: &Path) -> BTreeMap<String, Vec<u8>> {
    let mut out = BTreeMap::new();
    collect(root, root, &mut out);
    out
}

fn collect(root: &Path, dir: &Path, out: &mut BTreeMap<String, Vec<u8>>) {
    for entry in fs::read_dir(dir).expect("test: read dir") {
        let entry = entry.expect("test: readdir entry");
        let path = entry.path();
        if entry.file_type().expect("test: file type").is_dir() {
            collect(root, &path, out);
            continue;
        }
        let rel = path
            .strip_prefix(root)
            .unwrap()
            .to_string_lossy()
            .into_owned();
        if generated(&rel) {
            continue;
        }
        out.insert(rel, fs::read(&path).expect("test: read fixture"));
    }
}

/// Whether `rel` names a file the scan generates itself.
fn generated(rel: &str) -> bool {
    let name = rel.rsplit('/').next().unwrap_or(rel);
    name.starts_with("INDEX") && (name.ends_with(".md") || name.ends_with(".tmp"))
}

/// Every generated catalog file in the workspace root as name → bytes: the entry point and all its
/// shards, i.e. exactly what a rescan has to reproduce byte-for-byte.
fn generated_catalog(root: &Path) -> BTreeMap<String, Vec<u8>> {
    let mut out = BTreeMap::new();
    for entry in fs::read_dir(root).expect("test: read workspace root") {
        let entry = entry.expect("test: readdir entry");
        let name = entry.file_name().to_string_lossy().into_owned();
        if !generated(&name) || name.ends_with(".tmp") {
            continue;
        }
        out.insert(
            name,
            fs::read(entry.path()).expect("test: read generated file"),
        );
    }
    out
}

/// One `COUNT(*)`-shaped number from the workspace database.
fn scalar(open: &Db, sql: &str) -> i64 {
    open.0
        .query_row(sql, [], |row| row.get(0))
        .expect("test: scalar query")
}

/// One column of text rows from the workspace database.
fn texts(open: &Db, sql: &str) -> Vec<String> {
    let mut statement = open.0.prepare(sql).expect("test: prepare");
    let rows = statement
        .query_map([], |row| row.get::<_, String>(0))
        .expect("test: query");
    rows.map(|row| row.expect("test: row")).collect()
}

/// The `files.state` of one `rel`, or `None` when there is no row.
fn state(open: &Db, rel: &str) -> Option<String> {
    open.0
        .query_row("SELECT state FROM files WHERE rel = ?1", [rel], |row| {
            row.get::<_, String>(0)
        })
        .ok()
}

/// The `INDEX.md` text of a scanned workspace.
fn index_text(root: &Path) -> String {
    fs::read_to_string(root.join("INDEX.md")).expect("scan must write INDEX.md")
}

/// Catalog entry lines, i.e. the `- [[title]](rel)` bullets.
fn entry_lines(text: &str) -> Vec<&str> {
    text.lines()
        .filter(|line| line.starts_with("- [["))
        .collect()
}

/// The rendered entry line for one `rel`, looked for in `INDEX.md` and every shard; `None` when the
/// catalog holds no entry. Titles in the scratch cases are short, so an entry never wraps.
fn entry_line(root: &Path, rel: &str) -> Option<String> {
    let needle = format!("({rel})");
    for entry in fs::read_dir(root)
        .expect("test: read workspace root")
        .flatten()
    {
        let name = entry.file_name().to_string_lossy().into_owned();
        if !name.starts_with("INDEX") || !name.ends_with(".md") {
            continue;
        }
        let text = fs::read_to_string(entry.path()).expect("test: read index file");
        let found = text
            .lines()
            .find(|line| line.starts_with("- [[") && line.contains(&needle));
        if let Some(line) = found {
            return Some(line.to_string());
        }
    }
    None
}

/// The `meta('index_files')` list, one name per line, in the order the scan wrote it.
fn index_files_meta(open: &Db) -> Vec<String> {
    let value: String = open
        .0
        .query_row(
            "SELECT value FROM meta WHERE key = 'index_files'",
            [],
            |row| row.get(0),
        )
        .expect("test: index_files row");
    value.lines().map(str::to_string).collect()
}

/// Link rows written for one note, so a re-parse can be compared against its own history.
fn prose_links(open: &Db, rel: &str) -> i64 {
    let sql = format!(
        "SELECT COUNT(*) FROM links WHERE origin = 'prose' \
         AND src_note_id = (SELECT id FROM notes WHERE rel = '{rel}')"
    );
    scalar(open, &sql)
}

#[test]
fn first_scan_reports_all_added() {
    let ws = workspace();

    let report = scan(&ws.root, &ws.cfg).expect("first scan must succeed");

    // The corpus holds one non-markdown file (`scan.png`) and it is tracked like a note, so the
    // first scan adds it too; `non_markdown_files_are_tracked_not_parsed` pins that it gets no
    // note row and no catalog entry.
    assert_eq!(report.added as i64, CORPUS_FILES, "{report:?}");
    assert_eq!(report.modified, 0);
    assert_eq!(report.moved, 0);
    assert_eq!(report.removed, 0);
    assert_eq!(report.unchanged, 0);
    assert!(
        report.skipped.is_empty(),
        "nothing may be skipped: {report:?}"
    );
    assert_eq!(report.notes, CORPUS_NOTES, "one live note row per note");
    assert_eq!(report.index_files, vec!["INDEX.md".to_string()]);
    assert_eq!(scalar(&ws.db(), "SELECT COUNT(*) FROM files"), CORPUS_FILES);
    assert!(index_text(&ws.root).contains("# INDEX"));
}

#[test]
fn second_scan_is_a_no_op() {
    let ws = workspace();
    scan(&ws.root, &ws.cfg).expect("first scan");
    let first_index = fs::read(ws.root.join("INDEX.md")).expect("INDEX.md after first scan");
    let open = ws.db();
    let links_before = scalar(&open, "SELECT COUNT(*) FROM links");
    let items_before = scalar(&open, "SELECT COUNT(*) FROM items");
    let tags_before = scalar(&open, "SELECT COUNT(*) FROM tags");
    let headings_before = scalar(&open, "SELECT COUNT(*) FROM headings");

    let again = scan(&ws.root, &ws.cfg).expect("second scan");

    assert_eq!(again.added, 0, "nothing new: {again:?}");
    assert_eq!(again.modified, 0, "nothing changed: {again:?}");
    assert_eq!(again.moved, 0);
    assert_eq!(again.removed, 0);
    assert_eq!(
        again.unchanged as i64, CORPUS_FILES,
        "every tracked file is unchanged"
    );
    assert_eq!(again.notes, CORPUS_NOTES);
    let open = ws.db();
    assert_eq!(
        again.unresolved_links,
        scalar(
            &open,
            "SELECT COUNT(*) FROM links WHERE dst_note_id IS NULL AND kind != 'attachment'"
        ),
        "the report counts the notes nobody could place"
    );
    assert_eq!(
        fs::read(ws.root.join("INDEX.md")).expect("INDEX.md after second scan"),
        first_index,
        "the catalog must be byte-identical across scans"
    );
    assert_eq!(
        scalar(&open, "SELECT COUNT(*) FROM links"),
        links_before,
        "an unchanged note must not re-store its links"
    );
    assert_eq!(scalar(&open, "SELECT COUNT(*) FROM items"), items_before);
    assert_eq!(scalar(&open, "SELECT COUNT(*) FROM tags"), tags_before);
    assert_eq!(
        scalar(&open, "SELECT COUNT(*) FROM headings"),
        headings_before
    );
}

#[test]
fn writes_only_index_files() {
    let ws = workspace();
    let before = inputs(&ws.root);

    scan(&ws.root, &ws.cfg).expect("scan");

    assert_eq!(inputs(&ws.root), before, "no input file may change");
    // The comparison above is not vacuous: the catalog really appeared, and no temp file of
    // yours survived the rename.
    assert!(ws.root.join("INDEX.md").is_file());
    // Bound first: the map has to outlive the keys borrowed out of it.
    let after = inputs(&ws.root);
    let leftovers: Vec<&String> = after.keys().filter(|rel| rel.ends_with(".tmp")).collect();
    assert!(leftovers.is_empty(), "temp files must be renamed away");
}

#[test]
fn mutated_during_walk_is_skipped() {
    let ws = workspace();
    scan(&ws.root, &ws.cfg).expect("first scan");

    // Deleted between the walk and the read: unknown, so neither added nor removed.
    let report = scan_with_inject(&ws.root, &ws.cfg, |root| {
        fs::remove_file(root.join("orphan-thoughts.md")).expect("test hook: delete a note");
    })
    .expect("a vanished file must not fail the scan");

    assert!(
        report
            .skipped
            .iter()
            .any(|skipped| skipped == "orphan-thoughts.md"),
        "the vanished file has to be named in `skipped`: {:?}",
        report.skipped
    );
    assert_eq!(report.removed, 0, "unknown is not deleted: {report:?}");
    assert_eq!(report.added, 0);
    assert_eq!(
        state(&ws.db(), "orphan-thoughts.md").as_deref(),
        Some("ok"),
        "its row stays live"
    );

    // Rewritten between the walk and the read: the same rule, caught on the size it no longer
    // has.
    let report = scan_with_inject(&ws.root, &ws.cfg, |root| {
        fs::write(
            root.join("stray-idea.md"),
            "considerably more text than it held during the walk\n",
        )
        .expect("test hook: rewrite a note");
    })
    .expect("a mutated file must not fail the scan");

    assert!(
        report
            .skipped
            .iter()
            .any(|skipped| skipped == "stray-idea.md"),
        "the mutated file has to be named in `skipped`: {:?}",
        report.skipped
    );
    assert_eq!(report.modified, 0);
    assert_eq!(
        state(&ws.db(), "stray-idea.md").as_deref(),
        Some("ok"),
        "its row keeps the bytes it was indexed with"
    );
}

#[test]
fn non_utf8_content_is_lossy_not_fatal() {
    let ws = workspace();

    let report = scan(&ws.root, &ws.cfg).expect("a latin-1 note must not fail the scan");
    assert_eq!(report.skipped, Vec::<String>::new());

    let open = ws.db();
    assert_eq!(
        texts(
            &open,
            "SELECT encoding FROM notes WHERE rel = 'latin1-notes.md'"
        ),
        vec!["lossy".to_string()]
    );
    // Decoded with replacements rather than dropped: its heading is still an outline entry.
    assert_eq!(
        texts(
            &open,
            "SELECT title FROM notes WHERE rel = 'latin1-notes.md'"
        ),
        vec!["Latein-1 Notizen".to_string()]
    );
    // The digest covers the bytes on disk, not the lossy text derived from them.
    let stored: String = open
        .0
        .query_row(
            "SELECT hash FROM files WHERE rel = 'latin1-notes.md'",
            [],
            |row| row.get(0),
        )
        .expect("test: hash row");
    assert_eq!(
        stored,
        sha256_file(&ws.root.join("latin1-notes.md")).expect("test: hash the fixture")
    );
}

#[test]
fn every_fixture_gets_an_entry() {
    let ws = workspace();

    let report = scan(&ws.root, &ws.cfg).expect("scan");

    let text = index_text(&ws.root);
    assert_eq!(
        entry_lines(&text).len() as i64,
        CORPUS_NOTES,
        "one catalog entry per tracked markdown file"
    );
    assert_eq!(report.notes, CORPUS_NOTES);
    // The two shapes a renderer is most likely to lose.
    assert!(text.contains("(empty.md)"), "zero-byte note: {text}");
    assert!(
        text.contains("(frontmatter-only.md)"),
        "frontmatter-only note: {text}"
    );
    // An attachment is tracked but never catalogued.
    assert!(!text.contains("scan.png"));
}

#[test]
fn non_markdown_files_are_tracked_not_parsed() {
    let ws = workspace();

    let report = scan(&ws.root, &ws.cfg).expect("scan");

    let open = ws.db();
    assert_eq!(state(&open, "scan.png").as_deref(), Some("ok"));
    assert_eq!(
        scalar(&open, "SELECT COUNT(*) FROM notes WHERE rel = 'scan.png'"),
        0,
        "an attachment is not a note"
    );
    assert_eq!(
        scalar(
            &open,
            "SELECT COUNT(*) FROM links WHERE dst_note_id IS NULL AND target = 'scan.png'"
        ),
        CORPUS_ATTACHMENTS,
        "the embed lands on the attachment row, which has no note behind it"
    );
    // Database truth and report answer different questions: the DB counts every NULL
    // destination, the report only the ones that were meant to be a note.
    let counts: NoteCounts = db::note_counts(&open).expect("test: counts");
    assert_eq!(counts.notes, CORPUS_NOTES);
    assert_eq!(
        counts.unresolved_links,
        report.unresolved_links + CORPUS_ATTACHMENTS
    );
}

#[test]
fn deleted_file_marks_stale_and_keeps_history() {
    let ws = workspace();
    let first = scan(&ws.root, &ws.cfg).expect("first scan");

    fs::remove_file(ws.root.join("stray-idea.md")).expect("test: delete a note");
    let report = scan(&ws.root, &ws.cfg).expect("scan after the deletion");

    assert_eq!(report.removed, 1, "{report:?}");
    assert_eq!(report.notes, first.notes - 1);
    let open = ws.db();
    assert_eq!(state(&open, "stray-idea.md").as_deref(), Some("stale"));
    assert_eq!(
        scalar(
            &open,
            "SELECT COUNT(*) FROM notes WHERE rel = 'stray-idea.md'"
        ),
        1,
        "the note row survives"
    );
    let text = index_text(&ws.root);
    assert!(
        !text.contains("stray-idea.md"),
        "a stale note leaves the catalog: {text}"
    );
}

#[test]
fn bom_note_keeps_its_frontmatter_title() {
    let ws = workspace();

    scan(&ws.root, &ws.cfg).expect("scan");

    let open = ws.db();
    assert_eq!(
        texts(&open, "SELECT title FROM notes WHERE rel = 'bom-note.md'"),
        vec!["Notiz mit BOM".to_string()]
    );
    assert_eq!(
        texts(
            &open,
            "SELECT title_source FROM notes WHERE rel = 'bom-note.md'"
        ),
        vec!["frontmatter".to_string()],
        "the BOM must not hide the frontmatter block"
    );
    // Stripped for parsing only: the hash covers the file exactly as it stands on disk.
    assert_eq!(
        texts(&open, "SELECT hash FROM files WHERE rel = 'bom-note.md'"),
        vec![sha256_file(&ws.root.join("bom-note.md")).expect("test: hash the fixture")]
    );
}

#[test]
fn reparse_replaces_prose_links() {
    let ws = workspace();
    scan(&ws.root, &ws.cfg).expect("first scan");
    let before = prose_links(&ws.db(), "start-hier.md");
    assert!(before > 0, "the fixture writes links");

    // Same path, new bytes: a `Modified`, so the note is parsed and stored again.
    let mut text = fs::read_to_string(ws.root.join("start-hier.md")).expect("test: read note");
    text.push_str("\nNachtrag: noch ein Link auf [[projekt-alpha]].\n");
    fs::write(ws.root.join("start-hier.md"), text).expect("test: write note");
    let report = scan(&ws.root, &ws.cfg).expect("scan after the edit");

    assert_eq!(report.modified, 1, "{report:?}");
    assert_eq!(
        prose_links(&ws.db(), "start-hier.md"),
        before + 1,
        "one more link line, not a second copy of every row"
    );
}

#[test]
fn unresolved_targets_are_listed_for_the_hint() {
    let ws = workspace();

    let report = scan(&ws.root, &ws.cfg).expect("scan");

    let open = ws.db();
    let targets = db::unresolved_targets(&open).expect("test: unresolved targets");
    assert!(
        targets.iter().any(|target| target == "Gamma"),
        "the ambiguous basename has to be nameable: {targets:?}"
    );
    // An attachment target is unresolved in the database but is not a note nobody could place.
    assert!(
        !targets.iter().any(|target| target == "scan.png"),
        "attachments are not missing notes: {targets:?}"
    );
    // The explicitly spelled path resolves, which is what the hint advises.
    assert_eq!(
        scalar(
            &ws.db(),
            "SELECT COUNT(*) FROM links l
               JOIN notes dst ON dst.id = l.dst_note_id
              WHERE l.target = 'notes-a/gamma.md' AND dst.rel = 'notes-a/gamma.md'"
        ),
        2,
        "both spelled links land on the file they name"
    );
    // The report carries that list, because it is what the CLI prints beside the count: a summary
    // whose number and list come from different queries would name targets its own count denies.
    assert_eq!(
        report.unresolved_targets, targets,
        "the report has to carry the same list the query answers"
    );
    assert!(
        (targets.len() as i64) <= report.unresolved_links,
        "one target written in two notes counts twice and lists once: {report:?}"
    );
}

#[test]
fn a_foreign_note_wearing_a_generated_name_survives_and_is_reported() {
    // The name rule and the delete rule are not the same rule. A workspace that already holds a
    // hand-written root note called `INDEX-old.md` must keep it: the walk never indexes generated
    // names, so deleting it here would destroy a file no scan ever wrote (spec §7).
    let ws = scratch(
        &[
            ("a.md", "one\n"),
            (
                "INDEX-old.md",
                "# Meine alten Notizen\n\nHand-written, and nobody else's to delete.\n",
            ),
        ],
        600,
    );
    let before = fs::read(ws.root.join("INDEX-old.md")).expect("test: read the hand-written note");

    let report =
        scan(&ws.root, &ws.cfg).expect("a foreign INDEX-named note must not fail the scan");

    assert!(
        ws.root.join("INDEX-old.md").is_file(),
        "a note this program never published is never deleted"
    );
    assert_eq!(
        fs::read(ws.root.join("INDEX-old.md")).expect("test: re-read the hand-written note"),
        before,
        "the note survives byte for byte"
    );
    assert!(
        report
            .skipped
            .iter()
            .any(|skipped| skipped == "INDEX-old.md"),
        "the collision has to be reported instead of silently overwritten: {:?}",
        report.skipped
    );
    assert_eq!(
        report.notes, 1,
        "and it stays out of the catalog: {report:?}"
    );
}

#[test]
fn a_hand_written_file_carrying_our_header_under_a_generated_name_is_deleted() {
    // Intended behaviour, pinned here so it cannot be "fixed" by accident: the two header lines are the
    // *whole* identity signal, so a file that opens with them under a generated name is deleted even
    // though np never published it and its prose was typed by hand. The header — not the authorship of
    // the body — is what says "generated output, safe to regenerate", because nothing else on disk can
    // tell that file from one np wrote: the alternative would be rendering the whole corpus and comparing
    // bytes, which is the guess the fingerprint exists to avoid. The sibling case above shows the other
    // half of the rule: without our header, the same name is untouchable.
    let ws = scratch(
        &[
            ("a.md", "one\n"),
            ("b.md", "two\n"),
            (
                "INDEX-a-b.md",
                &format!("{}An entry nobody rendered.\n", catalog::HEADER),
            ),
        ],
        600,
    );
    // Premise: the file is sitting there before any scan has run, opening with np's LF header verbatim.
    let planted =
        fs::read(ws.root.join("INDEX-a-b.md")).expect("test setup: read the planted file");
    assert!(planted.starts_with(catalog::HEADER.as_bytes()));
    // Two short notes render one unsharded `INDEX.md`, so this scan publishes no shard at all: the file
    // above is not a generation it superseded, and it goes purely on what its own header claims.

    let report =
        scan(&ws.root, &ws.cfg).expect("a shard wearing our header must not fail the scan");

    assert!(
        !ws.root.join("INDEX-a-b.md").exists(),
        "the header is the identity signal, so a file carrying it is np's to remove: {report:?}"
    );
    assert!(
        !report
            .skipped
            .iter()
            .any(|skipped| skipped == "INDEX-a-b.md"),
        "a file np treated as its own is not a collision to report: {:?}",
        report.skipped
    );
    assert_eq!(report.notes, 2, "neither note is touched: {report:?}");
    assert_eq!(
        report.index_files,
        vec!["INDEX.md".to_string()],
        "this scan published no shard, so the deletion cannot rest on a superseded list: {report:?}"
    );

    // And it stays gone: nothing about the deletion depends on a first-scan artefact.
    let second = scan(&ws.root, &ws.cfg).expect("rescan after the reclaim");
    assert!(!ws.root.join("INDEX-a-b.md").exists());
    assert!(
        !second
            .skipped
            .iter()
            .any(|skipped| skipped == "INDEX-a-b.md"),
        "{:?}",
        second.skipped
    );
}

#[test]
fn a_directory_wearing_a_generated_name_survives_and_the_scan_succeeds() {
    // A root directory named like a shard used to become a delete candidate: `fs::remove_file`
    // answers `IsADirectory`, which is not `NotFound`, so every later scan of the workspace failed
    // on it. np writes files and removes files; a directory wearing its name is left alone.
    // Two short notes render one unsharded `INDEX.md`, so `INDEX-a-b.md` is a name this scan never
    // publishes — the shape that used to be fatal on every later scan.
    let ws = scratch(&[("a.md", "one\n"), ("b.md", "two\n")], 600);
    fs::create_dir(ws.root.join("INDEX-a-b.md")).expect("test: plant a directory");

    let report = scan(&ws.root, &ws.cfg).expect("a directory must not fail the scan");

    assert!(
        ws.root.join("INDEX-a-b.md").is_dir(),
        "np never removes a directory"
    );
    assert!(
        report
            .skipped
            .iter()
            .any(|skipped| skipped == "INDEX-a-b.md"),
        "the name it could not delete has to be reported: {:?}",
        report.skipped
    );
    assert!(ws.root.join("INDEX.md").is_file(), "the catalog is written");
}

#[test]
fn a_superseded_shard_rewritten_to_crlf_is_deleted_and_a_foreign_one_is_not() {
    // The fingerprint, not the name, is what grants deletion (see `np_published`), and that fingerprint
    // accepts either line ending. A shard whose endings a checkout, an editor or a sync tool rewrote is
    // still the file this program published: its catalog content is untouched, so keeping it would strand
    // a dead shard in `skipped` on every later scan. A shard whose opening line somebody else wrote is
    // not ours whatever it is called: it stays on disk and reaches `skipped`.
    //
    // The scan runs **twice** after the rewrite. One green pass could be a one-shot artefact of the fresh
    // `meta` list: a name reported once and unlinked by the next scan, or one kept only while the delete
    // list still names it, is a leak either way. Stability across scans is the property that matters.
    let ws = scratch(
        &[
            ("a.md", "one\n"),
            ("b.md", "two\n"),
            ("c.md", "three\n"),
            ("d.md", "four\n"),
        ],
        2,
    );
    let first = scan(&ws.root, &ws.cfg).expect("scan that shards");
    assert_eq!(first.index_files.len(), 3, "{:?}", first.index_files);

    // One shard loses its line endings and nothing else: the same information, still ours to remove.
    let crlf_header = catalog::HEADER.replace('\n', "\r\n");
    let reformatted = ws.root.join(&first.index_files[1]);
    let text = fs::read_to_string(&reformatted).expect("test: read the superseded shard");
    fs::write(&reformatted, text.replace('\n', "\r\n")).expect("test: rewrite the shard as CRLF");
    let rewritten = fs::read(&reformatted).expect("test: read back the rewritten shard");
    assert!(
        !rewritten.starts_with(catalog::HEADER.as_bytes()),
        "the rewrite must break the LF spelling of the fingerprint"
    );
    assert!(
        rewritten.starts_with(crlf_header.as_bytes()),
        "and leave the CRLF spelling intact"
    );

    // The other shard gets an opening line this program never writes, so the same pass has a file it must
    // not touch and cannot pass by deleting everything wearing a generated name.
    let foreign = ws.root.join(&first.index_files[2]);
    fs::write(
        &foreign,
        "# Hand-written index\n\nNot generated by anybody.\n",
    )
    .expect("test: overwrite the other shard with foreign prose");
    let foreign_bytes = fs::read(&foreign).expect("test: read back the foreign shard");
    assert!(
        !foreign_bytes.starts_with(crlf_header.as_bytes()),
        "test premise: the foreign shard carries neither fingerprint"
    );

    // Two notes fewer is one file again: both shards are superseded.
    fs::remove_file(ws.root.join("c.md")).expect("test: shrink the corpus");
    fs::remove_file(ws.root.join("d.md")).expect("test: shrink the corpus");

    let reformatted_name = reformatted
        .file_name()
        .unwrap()
        .to_string_lossy()
        .into_owned();
    let foreign_name = foreign.file_name().unwrap().to_string_lossy().into_owned();

    // The first pass after the rewrite.
    let second = scan(&ws.root, &ws.cfg).expect("scan after the shrink");
    assert!(
        !reformatted.exists(),
        "a superseded shard np can prove it published in either line ending is unlinked: {second:?}"
    );
    assert!(
        foreign.is_file(),
        "a superseded shard np cannot prove it published is never unlinked"
    );
    assert_eq!(
        fs::read(&foreign).expect("test: re-read the rewritten shard"),
        foreign_bytes,
        "the foreign file survives byte for byte"
    );
    assert!(
        second
            .skipped
            .iter()
            .any(|skipped| skipped == &foreign_name),
        "{foreign_name} has to be named in the report: {:?}",
        second.skipped
    );
    assert!(
        !second
            .skipped
            .iter()
            .any(|skipped| skipped == &reformatted_name),
        "the shard it deleted is not a collision to report: {:?}",
        second.skipped
    );

    // The pass that decides whether this is a rule or an artefact: `meta` now names only `INDEX.md`, so
    // the surviving file can only be found by listing the root and reading it again.
    let third = scan(&ws.root, &ws.cfg).expect("rescan with the foreign shard still in place");

    assert!(
        foreign.is_file(),
        "a second scan must not delete what the first one promised to keep"
    );
    assert_eq!(
        fs::read(&foreign).expect("test: re-read the shard after the rescan"),
        foreign_bytes,
        "and it survives byte for byte again"
    );
    assert!(
        third.skipped.iter().any(|skipped| skipped == &foreign_name),
        "{foreign_name} has to be reported on every scan: {:?}",
        third.skipped
    );
    assert_eq!(
        second.skipped, third.skipped,
        "the same collision must be reported identically twice"
    );
    assert_eq!(third.index_files, vec!["INDEX.md".to_string()]);
    assert!(
        ws.root.join("INDEX.md").is_file(),
        "and the catalog is still written around it"
    );
}

#[test]
fn a_superseded_shard_holding_our_crlf_header_and_different_prose_is_deleted() {
    // Identity rule: the two header lines are what says note-processor wrote this file, and nothing after
    // them can change that answer. Accepting CRLF rests on exactly that rule — line endings are
    // formatting, not authorship — so it has to hold for the body as well: a shard whose prose was
    // edited or replaced still opens by declaring itself generated, and np reclaims its own name. The
    // alternative is unreadable here: telling an edited shard from a hand-written note would mean
    // rendering the whole corpus and comparing bytes, which is the guess the fingerprint exists to avoid.
    let ws = scratch(
        &[
            ("a.md", "one\n"),
            ("b.md", "two\n"),
            ("c.md", "three\n"),
            ("d.md", "four\n"),
        ],
        2,
    );
    let first = scan(&ws.root, &ws.cfg).expect("scan that shards");
    assert_eq!(first.index_files.len(), 3, "{:?}", first.index_files);

    let crlf_header = catalog::HEADER.replace('\n', "\r\n");
    let edited = ws.root.join(&first.index_files[1]);
    let published = fs::read(&edited).expect("test: read the shard np just wrote");
    fs::write(
        &edited,
        format!("{crlf_header}\r\nAn entry nobody rendered.\r\n"),
    )
    .expect("test: replace the shard's prose behind np's back");

    // The premises this case turns on: the header is there in its CRLF spelling, and it is the only thing
    // of np's that survived the rewrite.
    let rewritten = fs::read(&edited).expect("test: read back the rewritten shard");
    assert!(rewritten.starts_with(crlf_header.as_bytes()));
    assert!(!rewritten.starts_with(catalog::HEADER.as_bytes()));
    assert_ne!(rewritten, published, "the body really is different bytes");

    // Two notes fewer: every shard is superseded, so the edited one is a delete candidate.
    fs::remove_file(ws.root.join("c.md")).expect("test: shrink the corpus");
    fs::remove_file(ws.root.join("d.md")).expect("test: shrink the corpus");
    let name = edited.file_name().unwrap().to_string_lossy().into_owned();

    let second = scan(&ws.root, &ws.cfg).expect("scan after the shrink");
    assert!(
        !edited.exists(),
        "the header identifies the file; the prose after it is irrelevant to that: {second:?}"
    );
    assert!(
        !second.skipped.iter().any(|skipped| skipped == &name),
        "a reclaimed shard is not a collision to report: {:?}",
        second.skipped
    );

    // And it stays gone: a rescan finds nothing wearing that name and reports nothing.
    let third = scan(&ws.root, &ws.cfg).expect("rescan after the reclaim");
    assert!(!edited.exists());
    assert!(
        !third.skipped.iter().any(|skipped| skipped == &name),
        "{:?}",
        third.skipped
    );
    assert_eq!(third.index_files, vec!["INDEX.md".to_string()]);
    assert!(ws.root.join("INDEX.md").is_file());
}

#[test]
fn empty_data_dir_is_an_error_and_not_a_relative_database() {
    let ws = workspace();
    let cfg = Config {
        data_dir: PathBuf::new(),
        ..Config::default()
    };

    let err = scan(&ws.root, &cfg).expect_err("an empty data dir must fail loudly");

    assert!(
        err.to_string().contains("data_dir"),
        "the message has to name the missing value: {err}"
    );
    // Nothing was written beside the corpus: no `workspaces/` directory appeared in it.
    assert!(!ws.root.join("workspaces").exists());
}

#[test]
fn umlaut_and_nested_names_are_resolvable() {
    let ws = workspace();

    scan(&ws.root, &ws.cfg).expect("scan");

    let open = ws.db();
    assert_eq!(state(&open, "große-notiz.md").as_deref(), Some("ok"));
    assert_eq!(state(&open, "deep/nested/beta.md").as_deref(), Some("ok"));
    // `[[beta]]` names a nested file by basename alone and `[Beta](deep/nested/beta.md)` spells
    // the path: both are edges to the same note.
    assert_eq!(
        scalar(
            &open,
            "SELECT COUNT(*) FROM links l
               JOIN notes src ON src.id = l.src_note_id
               JOIN notes dst ON dst.id = l.dst_note_id
              WHERE src.rel = 'projekt-alpha.md' AND dst.rel = 'deep/nested/beta.md'"
        ),
        2,
        "the wikilink and the markdown link both land"
    );
    // The CRLF fixture is a note like any other, frontmatter title and all.
    assert_eq!(
        texts(&open, "SELECT title FROM notes WHERE rel = 'crlf-note.md'"),
        vec!["CRLF Notiz".to_string()]
    );
}

#[cfg(unix)]
#[test]
fn unreadable_directory_does_not_mark_its_files_stale() {
    use std::os::unix::fs::PermissionsExt;

    let ws = workspace();
    scan(&ws.root, &ws.cfg).expect("first scan");
    // `deny_access` captures the mode the directory actually arrived with and restores it on every path out
    // of this block — a hardcoded 0o755 would *widen* permissions under a restrictive umask.
    let locked = ws.root.join("deep");
    let _restore = deny_access(&locked);
    let found_mode = _restore.mode;

    let report = scan(&ws.root, &ws.cfg).expect("an unreadable directory must not fail the scan");

    assert!(
        report.skipped.iter().any(|skipped| skipped == "deep"),
        "the unreadable directory is reported: {:?}",
        report.skipped
    );
    assert_eq!(report.removed, 0, "unknown is not deleted: {report:?}");
    assert_eq!(report.notes, CORPUS_NOTES);
    assert_eq!(
        state(&ws.db(), "deep/nested/beta.md").as_deref(),
        Some("ok"),
        "a permission error must not stale a whole subtree"
    );

    // Restoration asserted rather than assumed, as in the `walk.rs` case: dropping the guard here runs it
    // early, and what comes back has to be the mode this machine made — `0o700` under a `0o077` umask,
    // where restoring a fixed `0o755` would have handed group and other read+execute on a directory
    // nobody asked for. The sibling below is made by the same umask, so an assumed value cannot pass.
    drop(_restore);
    let restored = fs::metadata(&locked)
        .expect("test: re-read the directory mode")
        .permissions()
        .mode()
        & 0o7777;
    assert_eq!(
        restored, found_mode,
        "the guard restores the mode it captured"
    );
    let plain = ws._tmp.path().join("plain-sibling");
    fs::create_dir(&plain).expect("test setup: a directory made by this umask");
    assert_eq!(
        restored,
        fs::metadata(&plain)
            .expect("test: read the sibling mode")
            .permissions()
            .mode()
            & 0o7777,
        "and that mode is this machine's own, not an assumed one"
    );
}

#[test]
fn renamed_to_an_attachment_leaves_the_catalog() {
    let ws = workspace();
    let first = scan(&ws.root, &ws.cfg).expect("first scan");
    assert_eq!(first.notes, CORPUS_NOTES);

    // Give the note an edge to lose: an inbound link is what proves the leftover row is deleted rather
    // than merely hidden.
    let mut host = fs::read_to_string(ws.root.join("sprint-review.md")).expect("test: read note");
    host.push_str("\nSiehe [[stray-idea]].\n");
    fs::write(ws.root.join("sprint-review.md"), host).expect("test: write note");
    scan(&ws.root, &ws.cfg).expect("scan after the edit");

    // A byte-identical rename that drops the extension is a `Moved`, and `notes.rel` cascades onto the
    // new path — where nothing parses it again. An attachment is tracked but never catalogued (§10).
    fs::rename(
        ws.root.join("stray-idea.md"),
        ws.root.join("stray-idea.txt"),
    )
    .expect("test: rename a note to a .txt");
    let report = scan(&ws.root, &ws.cfg).expect("scan after the rename");

    assert_eq!(report.moved, 1, "{report:?}");
    let open = ws.db();
    assert_eq!(
        state(&open, "stray-idea.txt").as_deref(),
        Some("ok"),
        "the renamed path stays tracked as a file"
    );
    assert_eq!(
        scalar(
            &open,
            "SELECT COUNT(*) FROM notes WHERE rel = 'stray-idea.txt'"
        ),
        0,
        "no note row may survive on an attachment path"
    );
    assert_eq!(report.notes, CORPUS_NOTES - 1, "{report:?}");
    let text = index_text(&ws.root);
    assert!(
        !text.contains("stray-idea"),
        "an attachment is not catalogued: {text}"
    );
    assert!(entry_line(&ws.root, "stray-idea.txt").is_none());
    // The edge that pointed at the deleted note row reads as a note nobody could place instead of
    // dangling at a rowid SQLite will hand to some later INSERT.
    assert_eq!(
        report.unresolved_links,
        first.unresolved_links + 1,
        "{report:?}"
    );
    assert_eq!(
        scalar(
            &open,
            "SELECT COUNT(*) FROM links l
               LEFT JOIN notes n ON n.id = l.dst_note_id
              WHERE l.dst_note_id IS NOT NULL AND n.id IS NULL"
        ),
        0,
        "no edge may point at a note id that no longer exists"
    );
}

#[test]
fn a_byte_identical_rename_into_md_creates_the_missing_note_row() {
    // The mirror of [`renamed_to_an_attachment_leaves_the_catalog`]: same bytes, extension gained.
    // Classification reports a `Moved` and no content change, so only the missing-row half of the
    // re-parse condition in `index_file` can index this note — the branch where `existing` is `None`
    // while `rewritten` is false.
    let ws = scratch(&[("idea.txt", "Gedanke ueber den Umbau\n")], 600);
    let first = scan(&ws.root, &ws.cfg).expect("first scan");
    assert_eq!(first.notes, 0, "an attachment is not a note: {first:?}");

    fs::rename(ws.root.join("idea.txt"), ws.root.join("idea.md"))
        .expect("test: rename the attachment to .md");
    let report = scan(&ws.root, &ws.cfg).expect("scan after the rename");

    // The hash is the identity and it did not change, so nothing was reported as content churn.
    assert_eq!(report.moved, 1, "{report:?}");
    assert_eq!(report.added, 0, "{report:?}");
    assert_eq!(report.modified, 0, "{report:?}");

    // `notes.rel` carries no UNIQUE constraint, so a caller-supplied id mis-passed as `None` would
    // insert a second row rather than fail. Count the rows for the path; "at least one" would pass.
    let open = ws.db();
    assert_eq!(
        scalar(&open, "SELECT COUNT(*) FROM notes WHERE rel = 'idea.md'"),
        1,
        "exactly one note row for the renamed path"
    );
    assert_eq!(report.notes, 1, "{report:?}");
    assert!(
        entry_line(&ws.root, "idea.md").is_some(),
        "the rebuilt note is catalogued: {}",
        index_text(&ws.root)
    );

    // A third scan has nothing left to do and must not add a row while doing it.
    let again = scan(&ws.root, &ws.cfg).expect("scan after the rebuild");
    assert_eq!(again.moved, 0, "{again:?}");
    assert_eq!(again.added, 0, "{again:?}");
    assert_eq!(again.notes, 1, "{again:?}");
    assert_eq!(
        scalar(&open, "SELECT COUNT(*) FROM notes WHERE rel = 'idea.md'"),
        1,
        "the no-op scan still found one row"
    );
}

#[test]
fn a_rename_carries_the_note_and_its_rendered_target() {
    // Since migration 4, `notes.rel` is an ordinary TEXT column: its foreign key and its
    // `ON UPDATE CASCADE` — the only thing that ever maintained that column — are gone (F13 §4), and
    // `apply_move` took over the job as a statement of its own. Drop that statement and nothing reports a
    // problem: the report still says nothing but `moved: 1`, while `known_rows` — which also asks about a
    // note by display text — finds none for the new path, so a *second* note row is minted under it and the
    // original one is orphaned on the dead path, invisible to the display-keyed join. All three halves are
    // asserted together for that reason (F13 §5), and the id is asserted with them because the renderable
    // half alone cannot see this failure.
    let ws = scratch(&[("old.md", "# Umzug\n")], 600);
    let first = scan(&ws.root, &ws.cfg).expect("first scan");
    assert_eq!(first.notes, 1, "{first:?}");
    let note_id = scalar(&ws.db(), "SELECT id FROM notes WHERE rel = 'old.md'");
    assert!(
        entry_line(&ws.root, "old.md").is_some(),
        "premise: the note starts out catalogued: {}",
        index_text(&ws.root)
    );

    // Byte-identical, so `classify` pairs the two paths by hash and reports one move with no content churn.
    fs::rename(ws.root.join("old.md"), ws.root.join("new.md")).expect("test: rename the note");
    let report = scan(&ws.root, &ws.cfg).expect("scan after the rename");
    assert_eq!(report.moved, 1, "{report:?}");
    assert_eq!(report.added, 0, "{report:?}");
    assert_eq!(report.removed, 0, "{report:?}");

    let db = ws.db();
    // One: the registry row carries the new path and no row keeps the old one.
    assert_eq!(
        scalar(&db, "SELECT COUNT(*) FROM files WHERE rel = 'new.md'"),
        1
    );
    assert_eq!(
        scalar(&db, "SELECT COUNT(*) FROM files WHERE rel = 'old.md'"),
        0,
        "the old path is renamed, not duplicated"
    );
    // Two: the note's display target moved with those bytes — on the row it already was, since a second
    // row minted under the new name would be a detached note wearing a live catalog entry.
    assert_eq!(
        scalar(&db, "SELECT COUNT(*) FROM notes WHERE rel = 'new.md'"),
        1,
        "the note followed its file"
    );
    assert_eq!(
        scalar(&db, "SELECT COUNT(*) FROM notes WHERE rel = 'old.md'"),
        0,
        "no note row may be left behind on the path that is gone"
    );
    assert_eq!(
        scalar(&db, "SELECT id FROM notes WHERE rel = 'new.md'"),
        note_id,
        "the same note, not a fresh one beside a stale old row"
    );
    // Three: what a person reads. The entry has to point at the new path and no generated file may still
    // name the old one — §5's third half, and once readers key on bytes (round 2) the only half a reader of
    // `INDEX.md` can be shown.
    let line = entry_line(&ws.root, "new.md").unwrap_or_else(|| {
        panic!(
            "the renamed note must be catalogued: {}",
            index_text(&ws.root)
        )
    });
    assert!(line.contains("Umzug"), "{line}");
    for (name, bytes) in generated_catalog(&ws.root) {
        let text = String::from_utf8(bytes).expect("test: the catalog is UTF-8");
        assert!(
            !text.contains("old.md"),
            "{name} still renders the path that no longer exists: {text}"
        );
    }
}

#[test]
fn edge_into_a_deleted_note_reads_as_unresolved() {
    // Two notes linked together, in a corpus small enough to read the entry line off the catalog.
    let ws = scratch(
        &[("a.md", "Siehe [[b]].\n"), ("b.md", "Zurück zu [[a]].\n")],
        600,
    );
    let first = scan(&ws.root, &ws.cfg).expect("first scan");
    assert_eq!(first.unresolved_links, 0, "{first:?}");
    let before = entry_line(&ws.root, "a.md").expect("a.md is catalogued");
    assert!(before.contains("→1"), "{before}");

    // `a.md` is unchanged by the deletion, so it is never re-parsed and its edge keeps the old
    // `dst_note_id`. The destination's file is what tells the truth now.
    fs::remove_file(ws.root.join("b.md")).expect("test: delete the link target");
    let report = scan(&ws.root, &ws.cfg).expect("scan after the deletion");

    assert_eq!(report.removed, 1, "{report:?}");
    assert_eq!(report.notes, 1, "{report:?}");
    assert_eq!(
        report.unresolved_links, 1,
        "the surviving edge is unresolved: {report:?}"
    );
    let line = entry_line(&ws.root, "a.md").expect("a.md is still catalogued");
    assert!(
        line.contains("→0"),
        "no intact link to a hidden note: {line}"
    );

    // The list printed behind that count has to name it too: a number with no name beside it sends the
    // reader looking for a note the status line cannot show (see `db::unresolved_targets`).
    let targets = db::unresolved_targets(&ws.db()).expect("test: unresolved targets");
    assert!(
        targets.iter().any(|target| target == "b"),
        "the stale destination must be nameable: {targets:?}"
    );
}

#[test]
fn a_shard_that_meta_lost_track_of_is_still_deleted() {
    // The window between committing `meta` and writing the files: the list already names only the new
    // generation while the superseded shards of the old one are still sitting on disk. Deleting from
    // `meta` alone would leave them there forever, so deletion candidates come from the directory too.
    let ws = scratch(
        &[
            ("a.md", "one\n"),
            ("b.md", "two\n"),
            ("c.md", "three\n"),
            ("d.md", "four\n"),
        ],
        2,
    );
    let first = scan(&ws.root, &ws.cfg).expect("scan that shards");
    assert_eq!(first.index_files.len(), 3, "{:?}", first.index_files);

    fs::remove_file(ws.root.join("c.md")).expect("test: shrink the corpus");
    fs::remove_file(ws.root.join("d.md")).expect("test: shrink the corpus");
    ws.db()
        .0
        .execute(
            "UPDATE meta SET value = 'INDEX.md' WHERE key = 'index_files'",
            [],
        )
        .expect("test: rewind meta to a committed-but-unwritten generation");

    let second = scan(&ws.root, &ws.cfg).expect("scan after the restart");
    assert_eq!(second.index_files, vec!["INDEX.md".to_string()]);
    for name in &first.index_files[1..] {
        assert!(
            !ws.root.join(name).exists(),
            "shard {name} stranded by the kill must be gone"
        );
    }
    assert_eq!(index_files_meta(&ws.db()), vec!["INDEX.md".to_string()]);
}

#[test]
fn superseded_shards_are_deleted() {
    let ws = scratch(
        &[
            ("a.md", "one\n"),
            ("b.md", "two\n"),
            ("c.md", "three\n"),
            ("d.md", "four\n"),
        ],
        2,
    );

    let first = scan(&ws.root, &ws.cfg).expect("scan that shards");
    assert_eq!(first.index_files.len(), 3, "{:?}", first.index_files);
    assert_eq!(first.index_files[0], "INDEX.md");
    for name in &first.index_files {
        assert!(ws.root.join(name).is_file(), "{name} was rendered");
    }
    assert_eq!(index_files_meta(&ws.db()), first.index_files);

    // Spec §12's byte-identical regeneration, for the sharded shape: a rescan under the same
    // threshold rewrites every generated file to the same bytes and deletes nothing it just wrote.
    // `second_scan_is_a_no_op` proves this only for an unsharded corpus, where one entry point
    // cannot be superseded by anything.
    let sharded = generated_catalog(&ws.root);
    assert_eq!(sharded.len(), 3, "entry point plus two shards: {sharded:?}");
    let again = scan(&ws.root, &ws.cfg).expect("rescan of a sharded catalog");
    assert_eq!(again.index_files, first.index_files, "{again:?}");
    assert_eq!(
        generated_catalog(&ws.root),
        sharded,
        "a sharded catalog must be byte-identical across scans"
    );

    // Two notes fewer is one file again, so both shards are superseded — obligation 7's delete half.
    fs::remove_file(ws.root.join("c.md")).expect("test: shrink the corpus");
    fs::remove_file(ws.root.join("d.md")).expect("test: shrink the corpus");
    let second = scan(&ws.root, &ws.cfg).expect("scan after the shrink");

    assert_eq!(second.index_files, vec!["INDEX.md".to_string()]);
    for name in &first.index_files[1..] {
        assert!(
            !ws.root.join(name).exists(),
            "superseded shard {name} must be gone from disk"
        );
    }
    assert_eq!(index_files_meta(&ws.db()), vec!["INDEX.md".to_string()]);
    let text = index_text(&ws.root);
    assert!(
        !text.contains("INDEX-"),
        "no link to a deleted shard: {text}"
    );
}

#[test]
fn entry_point_is_published_after_its_shards() {
    let ws = scratch(
        &[
            ("a.md", "one\n"),
            ("b.md", "two\n"),
            ("c.md", "three\n"),
            ("d.md", "four\n"),
        ],
        2,
    );
    scan(&ws.root, &ws.cfg).expect("first scan");
    let published = fs::read(ws.root.join("INDEX.md")).expect("INDEX.md after the first scan");

    // A new note opens a third bucket, and a directory sits in the way of that shard's `rename` — so the
    // scan dies with shards written and its entry point still to come.
    fs::write(ws.root.join("e.md"), "five\n").expect("test: add a note");
    fs::create_dir(ws.root.join("INDEX-e-e.md")).expect("test: block the shard rename");

    let failed = scan(&ws.root, &ws.cfg).expect_err("a blocked rename must fail the scan");
    assert!(
        failed.to_string().contains("INDEX-e-e.md"),
        "the message has to name the file: {failed}"
    );

    // The old entry point still names only files that exist. Publishing it first is what left a reader
    // with links to shards that were never written.
    assert_eq!(
        fs::read(ws.root.join("INDEX.md")).expect("INDEX.md after the failed scan"),
        published,
        "INDEX.md must not move until every shard exists"
    );
    // And `meta` already names the attempted set: it is committed with the scan's transaction, so a
    // process killed here cannot leave a shard that no delete list mentions.
    assert_eq!(
        index_files_meta(&ws.db()),
        vec![
            "INDEX.md".to_string(),
            "INDEX-a-b.md".to_string(),
            "INDEX-c-d.md".to_string(),
            "INDEX-e-e.md".to_string(),
        ]
    );
}

#[test]
fn temp_corpse_from_a_failed_rename_is_not_indexed() {
    let ws = workspace();
    // A rename killed halfway leaves its temporary name behind, and `index_names::is_generated_index` does not
    // match `INDEX.md.tmp` — so without cleanup the next scan indexes its own corpse as an attachment.
    // The case where the corpse cannot be removed at all is pinned in `src/scan.rs`'s own tests, since
    // forcing it needs filesystem privileges a test does not have.
    fs::write(ws.root.join("INDEX.md.tmp"), "stale\n").expect("test: plant a corpse");

    let report = scan(&ws.root, &ws.cfg).expect("scan with a corpse in the way");

    assert!(
        !ws.root.join("INDEX.md.tmp").exists(),
        "the corpse is unlinked"
    );
    let open = ws.db();
    assert_eq!(
        scalar(
            &open,
            "SELECT COUNT(*) FROM files WHERE rel = 'INDEX.md.tmp'"
        ),
        0,
        "a corpse is never indexed"
    );
    assert_eq!(report.added as i64, CORPUS_FILES, "{report:?}");
    // The cleanup runs before the writes rather than instead of them.
    assert!(ws.root.join("INDEX.md").is_file());
}

// ---------------------------------------------------------------------------------------------
// F15 — a parse-rule change has to reach notes whose bytes did not change.
//
// `classify` compares hashes, so an unedited note looks finished forever and keeps the derived rows an
// older build wrote. The mechanism under test is one number in `meta` (`PARSE_VERSION_KEY`) compared with
// `np_core::parse::PARSE_VERSION` at scan start; a database whose stored value differs gets every note
// re-parsed once. These cases hand-build such a database the way `tests/db.rs` builds its pre-migration
// fixture — scan once for honest hashes, then rewrite the derived rows and the recorded version by hand,
// leaving the files on disk untouched.
// ---------------------------------------------------------------------------------------------

/// Notes for the F15 cases: frontmatter tags, a body tag, an item, two links, and one note whose opening
/// `---` region is prose — current rules (F7) keep the heading below it in the outline, while the pre-R3
/// parser swallowed that whole section. Tracked files: three notes plus one attachment.
const RULE_NOTES: [(&str, &str); 4] = [
    (
        "alpha.md",
        "---\ntitle: Alpha\ntags: [Vendor]\n---\n\n## Setup\n\nKickoff notes #Alpha with [[beta]] \
         and a task.\n\n- [ ] mail the vendor\n",
    ),
    (
        "beta.md",
        "# Beta Plan\n\nSecond note, linked from [[alpha]]. #Beta\n",
    ),
    (
        "thematic-break.md",
        "---\nOpening prose about the week.\n---\n\n# Real Heading\n\nMore prose under it #gamma\n",
    ),
    ("pic.png", "not really an image\n"),
];

/// The three notes of [`RULE_NOTES`], in `rel` order.
const RULE_NOTE_RELS: [&str; 3] = ["alpha.md", "beta.md", "thematic-break.md"];

/// `RULE_NOTES` indexed from scratch in its own workspace: what the current rules produce, and therefore
/// what a refreshed database has to match row for row and byte for byte.
fn reference_workspace() -> Workspace {
    let ws = scratch(&RULE_NOTES, 50);
    scan(&ws.root, &ws.cfg).expect("reference scan");
    ws
}

/// Rewrite one note's derived rows the way an older build would have left them: no heading rows at all,
/// one tag in a spelling these notes never contain, and a title taken from the file name. The bytes on
/// disk are not touched — that is the situation F15 exists for.
fn write_rows_like_an_older_build(open: &Db, rel: &str) {
    open.0
        .execute(
            "DELETE FROM headings WHERE note_id = (SELECT id FROM notes WHERE rel = ?1)",
            [rel],
        )
        .expect("test setup: drop the heading rows");
    open.0
        .execute(
            "DELETE FROM tags WHERE note_id = (SELECT id FROM notes WHERE rel = ?1)",
            [rel],
        )
        .expect("test setup: drop the tag rows");
    open.0
        .execute(
            "INSERT INTO tags (note_id, tag) SELECT id, 'old-tag' FROM notes WHERE rel = ?1",
            [rel],
        )
        .expect("test setup: plant the stale tag");
    open.0
        .execute(
            "UPDATE notes SET title = 'Old Title', title_source = 'filename', word_count = 1
              WHERE rel = ?1",
            [rel],
        )
        .expect("test setup: stale title and word count");
}

/// The recorded parse rules version, as the database holds it now. `None` is a file that never recorded
/// one — an M1 workspace, which is not a version to compare against.
fn recorded_parse_version(open: &Db) -> Option<u32> {
    open.0
        .query_row(
            "SELECT value FROM meta WHERE key = ?1",
            [np_core::scan::PARSE_VERSION_KEY],
            |row| row.get::<_, Option<String>>(0),
        )
        .ok()
        .flatten()
        .and_then(|value| value.trim().parse().ok())
}

/// Record `version` as the rules this database was parsed under, whether or not a scan wrote a value
/// first — the fixture is hand-built, so it does not assume anything about what the previous scan stored.
///
/// From the scan's point of view writing a *lower* value is exactly what bumping the constant does: the
/// comparison at scan start is between this number and the constant, and nothing else.
fn record_parse_version(open: &Db, version: u32) {
    open.0
        .execute(
            "INSERT INTO meta (key, value) VALUES (?1, ?2)
             ON CONFLICT (key) DO UPDATE SET value = excluded.value",
            rusqlite::params![np_core::scan::PARSE_VERSION_KEY, version.to_string()],
        )
        .expect("test setup: record the lagging version");
    assert_eq!(
        recorded_parse_version(open),
        Some(version),
        "test setup: the lag is in place"
    );
}

/// Whether this filesystem folds `a.md` and `A.md` into one file. A case-insensitive checkout made a single
/// file at setup time, so nothing can ever collide there: the tests that need two distinct spellings say so
/// out loud rather than failing a premise they could never satisfy — or, worse, passing while proving
/// nothing.
#[cfg(unix)]
fn folds_case(root: &Path) -> bool {
    fs::read_dir(root)
        .expect("test setup: read the workspace")
        .filter_map(|entry| entry.ok())
        .filter(|entry| {
            entry
                .file_name()
                .to_string_lossy()
                .eq_ignore_ascii_case("a.md")
        })
        .count()
        < 2
}

/// Whether this filesystem folds a name and its case variant into one file, counted over the entries whose
/// Unicode fold is `folded`. [`folds_case`] is the same gate for the ASCII pair `a.md` / `A.md`; a pair whose
/// difference is not ASCII (`Ämli.md` / `ämli.md`) needs `to_lowercase` to see one name at all. Same reason:
/// a case-insensitive checkout wrote a single file at setup time, so a test that needs both spellings says so
/// instead of passing on a premise it never had.
#[cfg(unix)]
fn folds_the_pair(root: &Path, folded: &str) -> bool {
    fs::read_dir(root)
        .expect("test setup: read the workspace")
        .filter_map(|entry| entry.ok())
        .filter(|entry| entry.file_name().to_string_lossy().to_lowercase() == folded)
        .count()
        < 2
}

/// Restore a captured permission mode on drop, so a failing assertion cannot leave a tempdir unreadable
/// for the cleaner.
#[cfg(unix)]
struct ModeGuard {
    path: PathBuf,
    /// The mode captured before the chmod, so a test can assert the restoration instead of assuming it.
    mode: u32,
}

#[cfg(unix)]
impl Drop for ModeGuard {
    fn drop(&mut self) {
        use std::os::unix::fs::PermissionsExt;
        let _ = fs::set_permissions(&self.path, fs::Permissions::from_mode(self.mode));
    }
}

/// chmod 000 on `path`, handing back the guard that restores whatever mode it actually had — hardcoding
/// 0o755 would *widen* permissions on a runner with a restrictive umask.
#[cfg(unix)]
fn deny_access(path: &Path) -> ModeGuard {
    use std::os::unix::fs::PermissionsExt;
    let mode = fs::metadata(path)
        .expect("test setup: read the permission bits")
        .permissions()
        .mode()
        & 0o7777; // permission bits only; the file-type bits are not ours to set
    fs::set_permissions(path, fs::Permissions::from_mode(0o000)).expect("test: chmod 000");
    ModeGuard {
        path: path.to_path_buf(),
        mode,
    }
}

/// A stored rules version this build must treat as out of date, without assuming [`PARSE_VERSION`] is
/// nonzero — resetting that constant to 0 would underflow every fixture below into "up to date".
fn outdated_parse_version() -> u32 {
    PARSE_VERSION.checked_sub(1).unwrap_or(PARSE_VERSION + 1)
}

/// Forget the recorded version entirely: the shape of a database written before this mechanism existed.
fn forget_parse_version(open: &Db) {
    open.0
        .execute(
            "DELETE FROM meta WHERE key = ?1",
            [np_core::scan::PARSE_VERSION_KEY],
        )
        .expect("test setup: delete the recorded version");
    assert_eq!(
        recorded_parse_version(open),
        None,
        "test setup: no label at all"
    );
}

/// One comparable dump of everything derived from note text: title and word count per note, then its
/// headings, tags, items and resolved edges. Spelled through `rel` only, so two workspaces holding the
/// same notes produce equal output regardless of their rowids or absolute paths.
fn derived_rows(open: &Db) -> Vec<Vec<String>> {
    vec![
        texts(
            open,
            "SELECT rel || '|' || title || '|' || title_source || '|' || word_count
               FROM notes ORDER BY rel",
        ),
        texts(
            open,
            "SELECT n.rel || '|' || h.level || '|' || h.text || '|' || h.line
               FROM headings h JOIN notes n ON n.id = h.note_id ORDER BY 1",
        ),
        texts(
            open,
            "SELECT n.rel || '|' || t.tag FROM tags t JOIN notes n ON n.id = t.note_id
              ORDER BY n.rel, t.rowid",
        ),
        texts(
            open,
            "SELECT n.rel || '|' || i.text || '|' || i.state || '|' || COALESCE(i.marker, '-')
               FROM items i JOIN notes n ON n.id = i.note_id ORDER BY 1",
        ),
        texts(
            open,
            "SELECT src.rel || '|' || l.kind || '|' || l.target || '|' || COALESCE(dst.rel, '?')
               FROM links l JOIN notes src ON src.id = l.src_note_id
                    LEFT JOIN notes dst ON dst.id = l.dst_note_id ORDER BY 1",
        ),
    ]
}

/// The highest `rowid` of each derived table: which *physical* rows are stored, not what they say.
///
/// Re-parsing a note deletes its child rows and inserts new ones (see `store_note`), so these numbers move
/// whenever a scan re-stores anything even when the replacement bytes are identical. That is the cheap seam
/// by which a test outside the crate can tell "wrote the same rows again" apart from "wrote nothing".
fn child_rowids(open: &Db) -> Vec<i64> {
    ["headings", "tags", "items", "links"]
        .iter()
        .map(|table| {
            scalar(
                open,
                &format!("SELECT COALESCE(MAX(rowid), 0) FROM {table}"),
            )
        })
        .collect()
}

/// The tag rows of one note, in stored order.
fn tags_of(open: &Db, rel: &str) -> Vec<String> {
    texts(
        open,
        &format!(
            "SELECT t.tag FROM tags t JOIN notes n ON n.id = t.note_id
              WHERE n.rel = '{rel}' ORDER BY t.rowid"
        ),
    )
}

#[test]
fn a_stale_parse_version_reparses_every_unchanged_note() {
    let ws = scratch(&RULE_NOTES, 50);
    scan(&ws.root, &ws.cfg).expect("first scan");
    let reference = reference_workspace();

    // Hand-build the lagging database: an older build's rows, and a recorded version behind this one.
    let open = ws.db();
    for rel in RULE_NOTE_RELS {
        write_rows_like_an_older_build(&open, rel);
    }
    record_parse_version(&open, outdated_parse_version());
    drop(open);
    assert_eq!(
        scalar(&ws.db(), "SELECT COUNT(*) FROM headings"),
        0,
        "the planted rows must really be stale before the scan can repair them"
    );
    assert_eq!(tags_of(&ws.db(), "alpha.md"), vec!["old-tag".to_string()]);

    let report = scan(&ws.root, &ws.cfg).expect("refresh scan");

    // The report stays the classification truth (F15's decided semantics): no file changed, so nothing is
    // modified and every tracked file still counts as unchanged. The refresh is not advertised here.
    assert_eq!(
        (report.added, report.modified, report.moved, report.removed),
        (0, 0, 0, 0),
        "unchanged bytes are not a modification: {report:?}"
    );
    assert_eq!(
        report.unchanged as i64,
        RULE_NOTES.len() as i64,
        "{report:?}"
    );
    assert!(report.skipped.is_empty(), "{report:?}");
    assert_eq!(report.notes, RULE_NOTE_RELS.len() as i64);

    let open = ws.db();
    assert_eq!(
        recorded_parse_version(&open),
        Some(PARSE_VERSION),
        "the refresh records the rules it wrote with, so it happens once"
    );
    assert_eq!(
        derived_rows(&open),
        derived_rows(&reference.db()),
        "one scan with unchanged bytes must bring every derived row up to current rules"
    );
    drop(open);
    assert_eq!(
        generated_catalog(&ws.root),
        generated_catalog(&reference.root),
        "INDEX.md must show the titles and tags current rules produce"
    );
    // What this pins is *delivery*: current rules put this heading into INDEX.md from bytes nobody
    // edited. That an older parser hid it inside the prose `---` region is why F15 was filed — a catalog
    // rendered by this build cannot show the old hiding place, so that half is not proven here.
    assert!(
        index_text(&ws.root).contains("[[Real Heading]](thematic-break.md)"),
        "{}",
        index_text(&ws.root)
    );
}

/// The heading texts stored for one note, in the order the parser wrote them.
///
/// Hand-built fixtures rewrite this table to imitate rows an older parser left behind, so it also proves
/// a refresh reached the note it was able to read.
fn heading_texts(open: &np_core::db::Db, rel: &str) -> Vec<String> {
    let mut statement = open
        .0
        .prepare(
            "SELECT h.text FROM headings h JOIN notes n ON n.id = h.note_id \
             WHERE n.rel = ?1 ORDER BY h.rowid",
        )
        .expect("test: prepare the heading read");
    let rows = statement
        .query_map([rel], |row| row.get::<_, String>(0))
        .expect("test: read headings");
    rows.map(|row| row.expect("test: heading row")).collect()
}

/// Wipe one note's derived headings: the shape an older parser leaves behind, so a refresh has something
/// visible to put back.
fn forget_headings(open: &np_core::db::Db, rel: &str) {
    open.0
        .execute(
            "DELETE FROM headings WHERE note_id = (SELECT id FROM notes WHERE rel = ?1)",
            [rel],
        )
        .expect("test: wipe the derived headings");
    assert!(
        heading_texts(open, rel).is_empty(),
        "test setup: no headings left for {rel}"
    );
}

#[test]
fn a_refresh_that_missed_a_note_leaves_the_parse_version_unrecorded() {
    // F15's own failure mode, reintroduced by the fix if this is not pinned: a scan that re-parses most of
    // the corpus but skips one path must not label the whole database current. The skipped note keeps the
    // rows an older build wrote, and once the version says "current" nothing revisits them again.
    let ws = scratch(
        &[
            ("covered.md", "---\n# Covered Heading\n---\n\nprose\n"),
            ("missed.md", "---\n# Missed Heading\n---\n\nprose\n"),
        ],
        600,
    );
    scan(&ws.root, &ws.cfg).expect("test: first scan");

    let db = ws.db();
    forget_headings(&db, "covered.md");
    record_parse_version(&db, outdated_parse_version());
    drop(db);

    // `missed.md` disappears between the walk and the read: unknown, so this scan writes nothing for it.
    let report = scan_with_inject(&ws.root, &ws.cfg, |root| {
        fs::remove_file(root.join("missed.md")).expect("test hook: vanish one note");
    })
    .expect("a mutated file must not fail the scan");
    assert!(
        report.skipped.iter().any(|skipped| skipped == "missed.md"),
        "the uncovered path must be named: {:?}",
        report.skipped
    );

    let db = ws.db();
    assert_eq!(
        heading_texts(&db, "covered.md"),
        vec!["Covered Heading".to_string()],
        "the notes this scan could reach still get refreshed"
    );
    assert_eq!(
        recorded_parse_version(&db),
        Some(outdated_parse_version()),
        "a refresh that did not cover the corpus must not label it current"
    );
    drop(db);

    // With nothing in the way, the next scan finishes the job and records once.
    fs::write(
        ws.root.join("missed.md"),
        "---\n# Missed Heading\n---\n\nprose\n",
    )
    .expect("test: put the note back");
    scan(&ws.root, &ws.cfg).expect("scan that completes the refresh");
    assert_eq!(
        recorded_parse_version(&ws.db()),
        Some(PARSE_VERSION),
        "a covering refresh must record, or every later scan re-parses forever"
    );
}

#[cfg(unix)]
#[test]
fn a_refresh_blocked_by_an_unreadable_directory_leaves_the_version_unrecorded() {
    // The other way a note escapes a refresh without reaching `skipped` as a written path: its directory
    // cannot be entered, so its live row is dropped by `known_rows` and never appears in `found` at all.
    let ws = scratch(
        &[
            ("top.md", "---\n# Top Heading\n---\n\nprose\n"),
            ("deep/low.md", "# Low\n"),
        ],
        600,
    );
    scan(&ws.root, &ws.cfg).expect("test: first scan");

    let db = ws.db();
    forget_headings(&db, "top.md");
    record_parse_version(&db, outdated_parse_version());
    drop(db);

    let _restore = deny_access(&ws.root.join("deep"));

    let report = scan(&ws.root, &ws.cfg).expect("an unreadable directory must not fail the scan");
    // Premise first: on a runner where 0o000 does not deny access (a privileged CI user, say) this fixture
    // proves nothing, and that has to be said here rather than surfacing as a confusing red below.
    assert!(
        report.skipped.iter().any(|skipped| skipped == "deep"),
        "the blocked directory must be named, or this test proves nothing: {:?}",
        report.skipped
    );

    let db = ws.db();
    assert_eq!(
        heading_texts(&db, "top.md"),
        vec!["Top Heading".to_string()],
        "the reachable note must still be refreshed"
    );
    assert_eq!(
        recorded_parse_version(&db),
        Some(outdated_parse_version()),
        "rows hidden behind an unreadable directory are not covered by this refresh"
    );
    drop(db);

    drop(_restore);
    scan(&ws.root, &ws.cfg).expect("scan after the directory opens again");
    assert_eq!(
        recorded_parse_version(&ws.db()),
        Some(PARSE_VERSION),
        "once the subtree is visible again the refresh must complete"
    );
}

#[cfg(unix)]
#[test]
fn an_unreadable_attachment_does_not_hold_the_parse_version_back() {
    // Coverage counts notes. No parse rule can stale an image's rows, so charging an unreadable attachment
    // would re-parse the entire corpus on every scan forever — over a file no parser will ever open — and
    // repair nothing.
    // Both ways an attachment can be missed: unreadable as itself, and hidden inside a directory the walk
    // cannot enter — which [`known_rows`] drops, so it never reaches `found` either.
    let ws = scratch(
        &[
            ("note.md", "# Note\n"),
            ("shot.png", "\u{89}PNG not really a png\n"),
            ("deep/clip.png", "\u{89}PNG also not a png\n"),
        ],
        600,
    );
    scan(&ws.root, &ws.cfg).expect("test: first scan");
    let db = ws.db();
    record_parse_version(&db, outdated_parse_version());
    drop(db);

    // Premise for the hidden branch: without a live row behind `deep/clip.png` there is nothing for the
    // `EXISTS` filter to exclude, and the assertion below would prove nothing.
    let db = ws.db();
    assert_eq!(
        scalar(&db, "SELECT COUNT(*) FROM files WHERE state = 'ok'"),
        3,
        "all three paths must be tracked before either is blocked"
    );
    drop(db);

    let _shot = deny_access(&ws.root.join("shot.png"));
    let _deep = deny_access(&ws.root.join("deep"));
    let report = scan(&ws.root, &ws.cfg).expect("an unreadable attachment must not fail the scan");
    for named in ["shot.png", "deep"] {
        assert!(
            report.skipped.iter().any(|skipped| skipped == named),
            "premise: the scan has to have been blocked at {named}: {:?}",
            report.skipped
        );
    }

    assert_eq!(
        recorded_parse_version(&ws.db()),
        Some(PARSE_VERSION),
        "a path that owes no parse must not pin the rules version"
    );
}

#[cfg(unix)]
#[test]
fn a_refused_move_does_not_label_the_note_it_left_behind_current() {
    // The one charge site that reads the wrong path if you are not careful. `old.md`'s bytes turn up at
    // `a.md`, which loses the case-insensitive identity race to `A.md`, so the move is refused — and
    // `classify` has already suppressed `old.md`'s `Removed` because a move is never also a deletion.
    // Its row stays live holding pre-refresh data with nothing at the destination to charge: exactly the
    // stale-labelled-current outcome F15 exists to prevent.
    let ws = scratch(
        &[
            ("old.md", "# Old Heading\n"),
            ("A.md", "# Upper A\n"),
            ("other.md", "# Other\n"),
        ],
        600,
    );
    scan(&ws.root, &ws.cfg).expect("test: first scan");
    if folds_case(&ws.root) {
        eprintln!("skip: this filesystem folds a.md and A.md into one file, so nothing collides");
        return;
    }

    fs::rename(ws.root.join("old.md"), ws.root.join("a.md"))
        .expect("test: move the bytes onto a colliding name");
    let db = ws.db();
    forget_headings(&db, "old.md");
    record_parse_version(&db, outdated_parse_version());
    drop(db);

    let report = scan(&ws.root, &ws.cfg).expect("a refused move must not fail the scan");
    assert!(
        report.skipped.iter().any(|skipped| skipped == "a.md"),
        "premise: the destination has to be refused: {:?}",
        report.skipped
    );

    let db = ws.db();
    assert!(
        heading_texts(&db, "old.md").is_empty(),
        "premise: the note left behind is still stale"
    );
    assert_eq!(
        state(&db, "old.md").as_deref(),
        Some("ok"),
        "a refused move must not stale the path it came from"
    );
    assert_eq!(
        recorded_parse_version(&db),
        Some(outdated_parse_version()),
        "the note this scan left behind is stale, so the rules version must not be recorded"
    );
    drop(db);

    // It heals once the rival *moves* rather than disappears. Deleting `A.md` would not free the identity:
    // a removed path goes `stale` and keeps its `case_bytes`, which is unique over every row, so the only way
    // the destination opens up is a scan that rewrites the rival's own row — and freeing it takes a second
    // scan, because that rewrite happens when the rival's own path is processed while the move wanting the
    // identity can already have been refused in the same pass.
    fs::rename(ws.root.join("A.md"), ws.root.join("B.md"))
        .expect("test: move the rival out of the way");
    scan(&ws.root, &ws.cfg).expect("scan that moves the rival out of the way");
    scan(&ws.root, &ws.cfg).expect("scan that lands the freed move");
    assert_eq!(
        heading_texts(&ws.db(), "a.md"),
        vec!["Old Heading".to_string()],
        "the move must land and its rows be rewritten"
    );
    assert_eq!(
        recorded_parse_version(&ws.db()),
        Some(PARSE_VERSION),
        "a covering refresh after the collision clears must record"
    );
}

#[cfg(unix)]
#[test]
fn a_case_collision_with_no_row_behind_it_does_not_hold_the_version_back() {
    // The last way to pin the rules version forever: `a.md` and `A.md` share one `case_key`, so one of them
    // is refused at insert on every scan. It never has a row, so there is nothing stale to repair — and
    // charging it would re-parse the corpus indefinitely to fix a file the scanner will never index.
    let ws = scratch(
        &[
            ("a.md", "# Lower A\n"),
            ("A.md", "# Upper A\n"),
            ("other.md", "# Other\n"),
        ],
        600,
    );
    scan(&ws.root, &ws.cfg).expect("test: first scan");

    // Which of the two the scanner refuses is an internal detail, so read it back from the registry rather
    // than guess: exactly one of them has no row. On a case-insensitive checkout the pair folded into one
    // file at setup time and neither is refused — that runner says so out loud instead of passing vacuously.
    // The gate is the filesystem, not the registry: on a folding checkout exactly one spelling is missing
    // from `files` too, which reads as a collision this scan refused rather than a fixture that never was.
    if folds_case(&ws.root) {
        eprintln!("skip: this filesystem folds a.md and A.md into one file, so nothing collides");
        return;
    }

    let db = ws.db();
    // Exactly one of the pair may be refused. Returning here when both spellings hold a row — which is what
    // this test used to do — is precisely the state dropping `case_bytes UNIQUE` would produce, so the guard
    // that exists to catch that change could only ever wave it through (F13 §3's P3 residual). Which of the
    // two loses is an internal detail (it is whichever row was written first), so only the count is pinned.
    let missing: Vec<&str> = ["a.md", "A.md"]
        .into_iter()
        .filter(|rel| state(&db, rel).is_none())
        .collect();
    assert_eq!(
        missing.len(),
        1,
        "one of the pair has to hold a row and the other has to be refused; a.md = {:?}, A.md = {:?}",
        state(&db, "a.md"),
        state(&db, "A.md")
    );
    let refused = missing[0];
    record_parse_version(&db, outdated_parse_version());
    drop(db);

    let report = scan(&ws.root, &ws.cfg).expect("a colliding path must not fail the scan");
    assert!(
        report.skipped.iter().any(|skipped| skipped == refused),
        "premise: {refused} has no row and must be named as skipped: {:?}",
        report.skipped
    );
    assert_eq!(
        recorded_parse_version(&ws.db()),
        Some(PARSE_VERSION),
        "a refused path with no rows behind it must not pin the rules version"
    );
}

#[cfg(unix)]
#[test]
fn a_link_naming_a_fold_two_live_paths_share_resolves_to_neither() {
    // Migration 4 dropped `files.case_key UNIQUE` so two live rows may share one display fold (F13 §3) —
    // `Ämli.md` beside `ämli.md`, both folding to `ämli.md`. A target that folds onto that string names two
    // files, and resolution's rule is to decline: the alternative is an edge onto whichever row the corpus
    // map kept, decided by read order.
    //
    // `[[DEEP/ÄMLI.MD]]` belongs in the same note because it is the other half of the rule. Its fold has
    // exactly one owner while three paths claim the file name `Ämli.md`, so it resolves on the display fold —
    // which stays Unicode (F13 §3) — and fails loudly if that step is ever turned into a blanket decline.
    let ws = scratch(
        &[
            ("Ämli.md", "# Ämli\n"),
            ("ämli.md", "# ämli\n"),
            ("deep/Ämli.md", "# Deep Ämli\n"),
            ("index.md", "Siehe [[ÄMLI.MD]] und [[DEEP/ÄMLI.MD]].\n"),
        ],
        600,
    );
    let report = scan(&ws.root, &ws.cfg).expect("a shared fold must not fail the scan");
    if folds_the_pair(&ws.root, "ämli.md") {
        eprintln!(
            "skip: this filesystem folds Ämli.md and ämli.md into one file, so no fold is shared"
        );
        return;
    }

    // Premise: the registry holds both spellings as live rows under one `case_key`, neither refused. Without
    // two owners of the fold there is nothing here for resolution to decline.
    let db = ws.db();
    assert_eq!(
        scalar(
            &db,
            "SELECT COUNT(*) FROM files WHERE state = 'ok' AND case_key = 'ämli.md'"
        ),
        2,
        "both spellings have to be live rows for their fold to be shared"
    );
    assert!(
        report.skipped.is_empty(),
        "a shared fold is not an identity clash: {report:?}"
    );

    // Both edges are written; only one of them may land on a row. `?` beside the other is the declined one —
    // naming it is what tells "declined" apart from "landed on the twin that happened to be read last".
    assert_eq!(
        texts(
            &db,
            "SELECT l.target || '->' || COALESCE(dst.rel, '?')
               FROM links l LEFT JOIN notes dst ON dst.id = l.dst_note_id ORDER BY 1"
        ),
        [
            "DEEP/ÄMLI.MD->deep/Ämli.md".to_string(),
            "ÄMLI.MD->?".to_string()
        ],
        "a fold the corpus shares resolves to neither twin, and a fold with one owner still resolves"
    );
    assert_eq!(
        report.unresolved_links, 1,
        "the declined target is the missing-note signal: {report:?}"
    );
    let targets = db::unresolved_targets(&db).expect("test: unresolved targets");
    assert!(
        targets.iter().any(|target| target == "ÄMLI.MD"),
        "the target nobody owns has to be nameable: {targets:?}"
    );
}

#[test]
fn a_null_meta_value_is_read_as_no_label_rather_than_failing_the_scan() {
    // Someone who pokes at the database by hand can leave `NULL` in `meta.value`. Both readers here
    // document NULL as "no label this build can trust"; reading the column as a plain `String` used to
    // raise InvalidColumnType and fail the entire scan instead.
    let ws = scratch(
        &[
            ("thematic-break.md", "---\n# Real Heading\n---\n\nprose\n"),
            ("plain.md", "two\n"),
        ],
        600,
    );
    scan(&ws.root, &ws.cfg).expect("test: first scan");

    ws.db()
        .0
        .execute("UPDATE meta SET value = NULL", [])
        .expect("test: null both meta values");

    let second = scan(&ws.root, &ws.cfg).expect("a NULL meta value must not fail the scan");
    assert_eq!(second.added, 0, "{second:?}");
    assert_eq!(second.unchanged, 2, "{second:?}");

    // The recovery rewrote both keys, so the next scan is an ordinary no-op again.
    let third = scan(&ws.root, &ws.cfg).expect("scan after recovering from the NULL");
    assert_eq!(third.unchanged, 2, "{third:?}");
    assert_eq!(
        index_files_meta(&ws.db()),
        vec!["INDEX.md".to_string()],
        "the index list must be rewritten from a NULL"
    );
}

#[test]
fn a_database_that_never_recorded_a_parse_version_is_refreshed_too() {
    let ws = scratch(&RULE_NOTES, 50);
    scan(&ws.root, &ws.cfg).expect("first scan");
    let reference = reference_workspace();

    // The real M1 upgrade: no `parse_version` row at all, which is not a version to compare against.
    let open = ws.db();
    for rel in RULE_NOTE_RELS {
        write_rows_like_an_older_build(&open, rel);
    }
    forget_parse_version(&open);
    assert_eq!(recorded_parse_version(&open), None, "test setup: no label");
    drop(open);

    let report = scan(&ws.root, &ws.cfg).expect("scan of an unlabelled database");

    assert_eq!(report.modified, 0, "{report:?}");
    assert_eq!(
        report.unchanged as i64,
        RULE_NOTES.len() as i64,
        "{report:?}"
    );
    let open = ws.db();
    assert_eq!(recorded_parse_version(&open), Some(PARSE_VERSION));
    assert_eq!(derived_rows(&open), derived_rows(&reference.db()));
}

#[test]
fn the_refresh_is_one_shot_and_the_next_scan_writes_no_bytes() {
    let ws = scratch(&RULE_NOTES, 50);
    scan(&ws.root, &ws.cfg).expect("first scan");
    let open = ws.db();
    write_rows_like_an_older_build(&open, "alpha.md");
    record_parse_version(&open, outdated_parse_version());
    drop(open);

    scan(&ws.root, &ws.cfg).expect("refresh scan");
    // Precondition: the refresh did repair the planted rows, so what follows is not proving nothing.
    assert_eq!(tags_of(&ws.db(), "alpha.md"), vec!["Vendor", "Alpha"]);

    let catalog = generated_catalog(&ws.root);
    let rows = derived_rows(&ws.db());
    let rowids = child_rowids(&ws.db());

    let again = scan(&ws.root, &ws.cfg).expect("scan after the refresh");

    assert_eq!(
        (again.added, again.modified, again.moved, again.removed),
        (0, 0, 0, 0),
        "{again:?}"
    );
    assert_eq!(again.unchanged as i64, RULE_NOTES.len() as i64, "{again:?}");
    assert_eq!(
        generated_catalog(&ws.root),
        catalog,
        "the refresh is one-shot: the catalog must be byte-identical after it"
    );
    let open = ws.db();
    assert_eq!(
        derived_rows(&open),
        rows,
        "no row may move on the quiet scan"
    );
    assert_eq!(
        child_rowids(&open),
        rowids,
        "and no row may be deleted and rewritten either: a second refresh would re-store every note"
    );
    assert_eq!(recorded_parse_version(&open), Some(PARSE_VERSION));
}

#[test]
fn a_second_rule_change_refreshes_once_and_the_scan_after_it_is_quiet() {
    let ws = scratch(&RULE_NOTES, 50);
    scan(&ws.root, &ws.cfg).expect("first scan");
    let open = ws.db();
    write_rows_like_an_older_build(&open, "beta.md");
    record_parse_version(&open, outdated_parse_version());
    drop(open);
    scan(&ws.root, &ws.cfg).expect("first refresh");

    // A second rule change: the constant moves again, which the database sees as its recorded value
    // falling behind, and the rows go stale in the same way they did the first time.
    let open = ws.db();
    write_rows_like_an_older_build(&open, "thematic-break.md");
    record_parse_version(&open, outdated_parse_version());
    drop(open);

    let second = scan(&ws.root, &ws.cfg).expect("scan after the second bump");
    assert_eq!(second.modified, 0, "{second:?}");
    let reference = reference_workspace();
    let open = ws.db();
    assert_eq!(
        derived_rows(&open),
        derived_rows(&reference.db()),
        "the second bump must refresh every note too, not only the one that went stale"
    );
    assert_eq!(recorded_parse_version(&open), Some(PARSE_VERSION));
    let catalog = generated_catalog(&ws.root);
    let rowids = child_rowids(&open);
    drop(open);

    let after = scan(&ws.root, &ws.cfg).expect("scan after the second refresh");
    assert_eq!(
        (after.added, after.modified, after.moved, after.removed),
        (0, 0, 0, 0),
        "{after:?}"
    );
    assert_eq!(generated_catalog(&ws.root), catalog, "again one-shot");
    assert_eq!(
        child_rowids(&ws.db()),
        rowids,
        "the second refresh must be one-shot too"
    );
}

#[test]
fn an_up_to_date_database_reparses_only_what_changed() {
    let ws = scratch(&RULE_NOTES, 50);
    let first = scan(&ws.root, &ws.cfg).expect("first scan");

    // A fresh database records the current version, and forces nothing: every note was being parsed as
    // `Added` anyway, so there is nothing for a refresh to add.
    assert_eq!(first.added as i64, RULE_NOTES.len() as i64, "{first:?}");
    assert_eq!(recorded_parse_version(&ws.db()), Some(PARSE_VERSION));

    // The cheap seam for "parses only what changed": rows planted by hand on an up-to-date database must
    // survive a warm scan. A refresh that reached every note would repair them, and there is no cheaper
    // way to see the difference from outside the crate.
    let open = ws.db();
    write_rows_like_an_older_build(&open, "alpha.md");
    drop(open);
    assert_eq!(tags_of(&ws.db(), "alpha.md"), vec!["old-tag".to_string()]);

    let warm = scan(&ws.root, &ws.cfg).expect("warm scan");
    assert_eq!(
        (warm.added, warm.modified, warm.moved, warm.removed),
        (0, 0, 0, 0),
        "{warm:?}"
    );
    assert_eq!(
        tags_of(&ws.db(), "alpha.md"),
        vec!["old-tag".to_string()],
        "an up-to-date database must not be re-parsed: the planted rows have to survive"
    );

    // One edited note is still re-parsed on its own, and only it.
    fs::write(
        ws.root.join("beta.md"),
        "# Beta Plan\n\nRewritten, linked from [[alpha]]. #Beta #Extra\n",
    )
    .expect("test: edit one note");
    let edited = scan(&ws.root, &ws.cfg).expect("scan after the edit");

    assert_eq!(edited.modified, 1, "{edited:?}");
    assert_eq!(
        edited.unchanged as i64,
        RULE_NOTES.len() as i64 - 1,
        "{edited:?}"
    );
    assert_eq!(tags_of(&ws.db(), "beta.md"), vec!["Beta", "Extra"]);
    assert_eq!(
        tags_of(&ws.db(), "alpha.md"),
        vec!["old-tag".to_string()],
        "the note nobody edited stays exactly as stale as it was"
    );
}

// ---------------------------------------------------------------------------
// F13 round 2: identity keyed on the OS bytes, end to end.
//
// Every case here builds its file names with `OsStringExt::from_vec`, so every one is `#[cfg(unix)]` — on
// Windows the same name cannot be built at all and the test would pass by asserting nothing (F13 §5).
// ---------------------------------------------------------------------------

/// A workspace holding exactly `files` (raw OS bytes of the name → contents). The `&str`-named [`scratch`]
/// cannot express a name UTF-8 cannot hold, which is the whole subject here.
#[cfg(unix)]
fn scratch_bytes(files: &[(&[u8], &str)]) -> Workspace {
    use std::ffi::OsString;
    use std::os::unix::ffi::OsStringExt;

    let tmp = TempDir::new().expect("test setup: tempdir");
    let root = tmp.path().join("ws");
    fs::create_dir_all(&root).expect("test setup: create scratch workspace");
    for (name, contents) in files {
        fs::write(root.join(OsString::from_vec((*name).to_vec())), contents)
            .expect("test setup: write a byte-named note");
    }
    let cfg = Config {
        data_dir: tmp.path().join("data"),
        ..Config::default()
    };
    Workspace {
        _tmp: tmp,
        root,
        cfg,
    }
}

/// The bytes one query returns, as hex — so a failure prints `636166E9…` rather than a replacement
/// character that has already lost the fact under test.
#[cfg(unix)]
fn key_hex(open: &Db, sql: &str) -> String {
    texts(open, sql)
        .first()
        .cloned()
        .unwrap_or_else(|| "<no row>".to_string())
}

/// §5's round trip: a name built from Latin-1 bytes is stored as **those** bytes, and the stored key
/// rebuilds a path that opens the file. The registry's answer to
/// `workspace_db_separates_a_non_utf8_path_from_its_lossy_spelling`, for the workspace database rather than
/// the workspace key — and the assertion round 1 could not make, because its writers only ever saw text.
#[cfg(unix)]
#[test]
fn a_latin1_name_is_stored_as_its_os_bytes_and_reopens_the_file() {
    use std::ffi::OsString;
    use std::os::unix::ffi::OsStringExt;

    let ws = scratch_bytes(&[(b"caf\xe9.md", "# Cafe\n")]);
    let report = scan(&ws.root, &ws.cfg).expect("scan a workspace holding one Latin-1 name");
    assert!(report.skipped.is_empty(), "{report:?}");
    assert_eq!((report.added, report.notes), (1, 1), "{report:?}");

    let db = ws.db();
    // `files.rel_bytes` is the exact OS bytes — no U+FFFD anywhere in them.
    assert_eq!(
        key_hex(&db, "SELECT hex(rel_bytes) FROM files"),
        "636166E92E6D64",
        "the identity column holds `caf\\xe9.md`, not the bytes of its lossy spelling"
    );
    // The note carries the same key, so it hangs off this file rather than off the display text.
    assert_eq!(
        key_hex(&db, "SELECT hex(rel_bytes) FROM notes"),
        "636166E92E6D64",
        "a note keyed on the display spelling would match nothing the join compares"
    );

    // The round trip that F13 §1 says was impossible: rebuild a path from what the database stored and open
    // it. Before, the row's `abs` was `/ws/caf\u{FFFD}.md` — a path that exists nowhere.
    let stored: Vec<u8> =
        db.0.query_row("SELECT rel_bytes FROM files", [], |row| row.get(0))
            .expect("test: read the stored key");
    let rebuilt = ws.root.join(PathBuf::from(OsString::from_vec(stored)));
    assert_eq!(
        fs::read(&rebuilt).expect("the stored key has to name the file on disk"),
        b"# Cafe\n"
    );

    // Display text stays honest and stays what the catalog renders: the report line and the entry both spell
    // the lossy name, because that is a display question (F13 §3).
    assert_eq!(texts(&db, "SELECT rel FROM files"), ["caf\u{fffd}.md"]);
    let rendered = index_text(&ws.root);
    let entries = entry_lines(&rendered);
    assert_eq!(entries.len(), 1, "{entries:?}");
    assert!(
        entries[0].contains("caf\u{fffd}.md"),
        "the catalog renders display text: {}",
        entries[0]
    );
}

/// §5's twin pair: `caf\xe8.md` beside `caf\xe9.md` — two files, one display spelling. Round 1 collapsed
/// them into one row whose digest belonged to whichever name sorted last, published a target that opens
/// nothing, and never converged.
#[cfg(unix)]
#[test]
fn two_twins_get_two_rows_two_notes_and_two_catalog_entries() {
    let ws = scratch_bytes(&[(b"caf\xe8.md", "# Cafe\n"), (b"caf\xe9.md", "# Cafe\n")]);
    let first = scan(&ws.root, &ws.cfg).expect("a twin pair must not fail the scan");
    assert!(
        first.skipped.is_empty(),
        "two byte-distinct paths are not an identity clash: {first:?}"
    );
    assert_eq!(
        (first.added, first.modified, first.moved, first.removed),
        (2, 0, 0, 0),
        "both twins are indexed: {first:?}"
    );
    assert_eq!(first.notes, 2, "neither note was swallowed: {first:?}");

    let db = ws.db();
    assert_eq!(scalar(&db, "SELECT COUNT(*) FROM files"), 2);
    assert_eq!(scalar(&db, "SELECT COUNT(*) FROM notes"), 2);
    // Both rows display the identical text — that is the point — and both keys are present and distinct.
    assert_eq!(
        texts(&db, "SELECT rel FROM files ORDER BY rel_bytes"),
        ["caf\u{fffd}.md", "caf\u{fffd}.md"],
        "display text is what the twins share, not identity"
    );
    assert_eq!(
        texts(&db, "SELECT hex(rel_bytes) FROM files ORDER BY rel_bytes"),
        ["636166E82E6D64", "636166E92E6D64"],
        "two byte keys, ordered by the key rather than by a rowid nobody chose"
    );

    // Two catalog entries whose rendered bytes are identical: same title, same target. Nothing here can tell
    // them apart by eye, which is exactly why their order has to come from the byte key.
    let rendered = index_text(&ws.root);
    let lines = entry_lines(&rendered);
    assert_eq!(lines.len(), 2, "{lines:?}");
    assert_eq!(
        lines[0], lines[1],
        "both entries render the same display text"
    );

    // Convergence — §6's second item. Today this workspace reported `added: 2` on every scan forever.
    let second = scan(&ws.root, &ws.cfg).expect("test: second scan");
    assert_eq!(
        (
            second.added,
            second.modified,
            second.moved,
            second.removed,
            second.unchanged
        ),
        (0, 0, 0, 0, 2),
        "the pair converges on the scan after: {second:?}"
    );
    assert!(second.skipped.is_empty(), "{second:?}");

    // And the published bytes are stable across a further scan: the order came from `ORDER BY n.rel_bytes`
    // and `sort_key`'s byte tie-break, not from whichever row SQLite read first.
    let rendered_again = index_text(&ws.root);
    assert_eq!(rendered, rendered_again, "catalog bytes are stable");
    scan(&ws.root, &ws.cfg).expect("test: third scan");
    assert_eq!(
        index_text(&ws.root),
        rendered,
        "a third scan reproduces the same bytes"
    );
}

/// §5's resolution rule over a twin pair, which is the behaviour that forces `links::store_links` to resolve
/// note ids by bytes: while both twins are live a target naming their shared spelling names two files and
/// stays unresolved; once one is gone it must land on the survivor — whose **display text is still identical
/// to the stale twin's**, so an id lookup keyed on that text would find two note rows and guess.
#[cfg(unix)]
#[test]
fn a_target_naming_two_twins_declines_and_then_resolves_onto_the_survivor() {
    let ws = scratch_bytes(&[
        (b"caf\xe8.md", "# Cafe eight\n"),
        (b"caf\xe9.md", "# Cafe nine\n"),
        // The link is spelled the way an author would spell what they can see: the display text both twins
        // share, replacement character and all. Resolution matches on that text; only the answer is bytes.
        (b"index.md".as_slice(), "See [[caf\u{fffd}.md]].\n"),
    ]);
    let first = scan(&ws.root, &ws.cfg).expect("test: first scan");
    assert!(first.skipped.is_empty(), "{first:?}");

    let db = ws.db();
    // Declined, not guessed: one unresolved target and no edge onto either twin.
    assert_eq!(
        first.unresolved_targets,
        vec!["caf\u{fffd}.md".to_string()],
        "{first:?}"
    );
    assert_eq!(
        placed_edges(&db),
        ["caf\u{fffd}.md->?".to_string()],
        "a spelling two live rows share resolves to neither"
    );
    drop(db);

    // One twin goes. Its row and its note stay (stale), so the display text is now held twice over — once
    // live, once stale — and only the byte key tells them apart.
    remove_twin(&ws.root, b"caf\xe8.md");
    fs::write(ws.root.join("index.md"), "See [[caf\u{fffd}.md]] again.\n")
        .expect("test: edit the source note");
    let second = scan(&ws.root, &ws.cfg).expect("test: scan after the deletion");
    assert!(second.skipped.is_empty(), "{second:?}");

    let db = ws.db();
    assert_eq!(
        placed_edges(&db),
        ["caf\u{fffd}.md->636166E92E6D64".to_string()],
        "the edge lands on the surviving twin's row, not on its stale twin's identically spelled note"
    );
    assert_eq!(second.unresolved_links, 0, "{second:?}");
}

/// Every stored edge as `target->hex(destination key)`, with `?` for one that stayed unresolved. The
/// destination is named by **bytes**, because the two twins' display text is one string and cannot tell an
/// assertion which row an edge landed on. (`hex(NULL)` is `''` in this SQLite, hence the explicit `CASE`.)
#[cfg(unix)]
fn placed_edges(db: &Db) -> Vec<String> {
    texts(
        db,
        "SELECT l.target || '->' || CASE WHEN l.dst_note_id IS NULL THEN '?'
                                         ELSE hex(f.rel_bytes) END
           FROM links l LEFT JOIN notes n ON n.id = l.dst_note_id
           LEFT JOIN files f ON f.rel_bytes = n.rel_bytes
          ORDER BY 1",
    )
}

/// Delete a file whose name UTF-8 cannot hold.
#[cfg(unix)]
fn remove_twin(root: &Path, name: &[u8]) {
    fs::remove_file(byte_name(root, name)).expect("test setup: remove one twin");
}

/// The path one byte-spelled root name really names — the spelling `OsStr::from_bytes` builds and the
/// filesystem holds, which is what a lossy `String` cannot express.
#[cfg(unix)]
fn byte_name(root: &Path, name: &[u8]) -> PathBuf {
    use std::ffi::OsString;
    use std::os::unix::ffi::OsStringExt;
    root.join(OsString::from_vec(name.to_vec()))
}

/// A generated shard name holding one raw byte UTF-8 cannot represent.
#[cfg(unix)]
const TWIN_RAW: &[u8] = b"INDEX-\xff.md";
/// The same display spelling built the other way: the encoded replacement character itself. Both are names
/// `np_core::index_names::is_generated_index` recognises, and both decode to `INDEX-\u{fffd}.md`.
#[cfg(unix)]
const TWIN_ENCODED: &[u8] = b"INDEX-\xef\xbf\xbd.md";

/// F13 §1 replayed on the one code path in a scan that deletes files. Two root entries whose names differ
/// only in bytes UTF-8 cannot hold share **one** display spelling, so a verdict taken on that text is a
/// verdict about whichever entry `readdir` happened to reach first — and the deletion then joins that text
/// back onto the root, which is a third file nobody judged.
///
/// Here our own published shard sits under the raw-byte spelling and a header-less foreign file sits under
/// the encoded one. Neither outcome the collapse produces is acceptable: the foreign note must not be
/// unlinked at its twin's bytes because the twin carried our header, and our shard must not survive in
/// silence because the twin decided the verdict. So each entry is judged by its own name bytes.
#[cfg(unix)]
#[test]
fn a_superseded_shard_and_its_foreign_twin_are_judged_by_their_own_bytes() {
    // Premise first: two byte-distinct names, one display spelling, both of them ours to ask about.
    let raw_display = String::from_utf8_lossy(TWIN_RAW).into_owned();
    let encoded_display = String::from_utf8_lossy(TWIN_ENCODED).into_owned();
    assert_eq!(
        raw_display, encoded_display,
        "the fixture has to produce one display spelling for two names"
    );
    for name in [TWIN_RAW, TWIN_ENCODED] {
        assert!(
            np_core::index_names::is_generated_index(&String::from_utf8_lossy(name)),
            "{name:?} has to read as a generated name"
        );
    }

    let ws = scratch(&[("a.md", "one\n"), ("b.md", "two\n")], 1);
    let first = scan(&ws.root, &ws.cfg).expect("scan that shards");
    assert_eq!(first.index_files.len(), 3, "{first:?}");

    // One published shard moves out from under its own name: the same bytes, a spelling this scan will not
    // produce. Renaming rather than copying keeps the header — the fingerprint that makes it ours.
    let published = first.index_files[1].clone();
    let ours = byte_name(&ws.root, TWIN_RAW);
    fs::rename(ws.root.join(&published), &ours)
        .expect("test: rename a published shard to its twin spelling");
    // The foreign twin: same display text, no header, and nothing this program ever wrote.
    let foreign = byte_name(&ws.root, TWIN_ENCODED);
    fs::write(&foreign, "somebody else's prose\n").expect("test: write the foreign twin");

    let second = scan(&ws.root, &ws.cfg).expect("a twin pair must not fail the scan");

    // The foreign file is untouched — judged by its own bytes, which carry no header.
    assert!(
        foreign.is_file(),
        "a header-less twin is not ours to delete: {:?}",
        second.skipped
    );
    assert_eq!(
        fs::read(&foreign).expect("test: re-read the foreign twin"),
        b"somebody else's prose\n",
        "and its bytes are exactly what they were"
    );

    // Our shard is gone, unlinked at the path the listing actually produced.
    assert!(
        !ours.exists(),
        "a superseded shard np can prove it published is unlinked at its own path: {:?}",
        second.skipped
    );

    // The collision is reported once, in display text — all a report line can hold.
    assert!(
        second
            .skipped
            .iter()
            .any(|skipped| skipped == &encoded_display),
        "the foreign twin has to be named: {:?}",
        second.skipped
    );

    // The fixture added no notes and the deletion touched nothing else: both twins are excluded from the
    // corpus by their generated name, and the shard this scan publishes is written again.
    assert_eq!(second.notes, 2, "{second:?}");
    assert_eq!(second.index_files, first.index_files, "{second:?}");
    assert!(
        ws.root.join(&published).is_file(),
        "{published} is republished"
    );
}

/// The other direction of the same union: `meta` names a superseded shard that is **no longer on disk**.
///
/// This is the property that licenses `set_index_list` committing the name list inside the transaction,
/// ahead of the writes it describes — a list that *leads* reality costs nothing, because a name already gone
/// is exactly the state the delete loop wanted. Silence is the only sensible answer here: reporting the name
/// would put a collision in front of the user that they cannot look at, and treating it as an error would
/// break every later scan of a workspace whose shard an editor or a sync tool removed.
#[test]
fn a_superseded_shard_that_is_already_gone_from_disk_is_silent_and_not_fatal() {
    let ws = scratch(
        &[
            ("a.md", "one\n"),
            ("b.md", "two\n"),
            ("c.md", "three\n"),
            ("d.md", "four\n"),
        ],
        2,
    );
    let first = scan(&ws.root, &ws.cfg).expect("scan that shards");
    assert_eq!(first.index_files.len(), 3, "{first:?}");
    // Premise: `meta` names both shards, so the one deleted below is genuinely a name left behind — this is
    // the committed-ahead list, not a disk-only candidate.
    assert_eq!(index_files_meta(&ws.db()), first.index_files);

    fs::remove_file(ws.root.join("c.md")).expect("test: shrink the corpus");
    fs::remove_file(ws.root.join("d.md")).expect("test: shrink the corpus");
    // Both shards are now superseded; one of them is also gone from disk by hand, which is the state a scan
    // meets when something else cleared a shard it still has to account for.
    let gone = first.index_files[1].clone();
    fs::remove_file(ws.root.join(&gone)).expect("test: remove a published shard");

    let second = scan(&ws.root, &ws.cfg)
        .expect("a superseded name already gone from disk must not fail the scan");

    assert!(
        second.skipped.is_empty(),
        "a shard that is simply absent is not a collision anybody can inspect: {:?}",
        second.skipped
    );
    // The ordinary path still runs beside it: the sibling shard, still on disk and carrying our header, is
    // deleted as always, and this generation's list replaces the stale one.
    assert_eq!(second.index_files, vec!["INDEX.md".to_string()]);
    assert!(
        !ws.root.join(&first.index_files[2]).exists(),
        "the superseded shard still on disk is deleted as usual: {:?}",
        generated_catalog(&ws.root)
    );
    assert!(!ws.root.join(&gone).exists(), "{gone} stays gone");
    assert_eq!(index_files_meta(&ws.db()), vec!["INDEX.md".to_string()]);
}

/// The F13 §1 twin pair again with a **directory** on the foreign side, which is the case that puts the
/// file-type check to work: it runs per entry, off the listing, before anything is opened. A verdict taken on
/// the shared display text would answer for whichever entry readdir reached first, and either answer is a
/// bug — ours unlinked at nothing (the directory's verdict), or `remove_file` on a directory answering
/// `IsADirectory` rather than `NotFound`, which is not survivable by any later scan of the workspace.
#[cfg(unix)]
#[test]
fn a_directory_twinning_our_published_shard_is_judged_by_its_own_bytes() {
    let ws = scratch(&[("a.md", "one\n"), ("b.md", "two\n")], 1);
    let first = scan(&ws.root, &ws.cfg).expect("scan that shards");
    assert_eq!(first.index_files.len(), 3, "{first:?}");

    // Our own published shard, moved to the raw-byte twin spelling: same bytes, a name this scan will not
    // produce, so it is ours and superseded.
    let published = first.index_files[1].clone();
    let ours = byte_name(&ws.root, TWIN_RAW);
    fs::rename(ws.root.join(&published), &ours)
        .expect("test: rename a published shard to its twin spelling");
    // The foreign twin is a directory wearing the other byte spelling of the same display text, with prose of
    // its own inside. np publishes files and unlinks files; it never opens or removes a directory here.
    let dir = byte_name(&ws.root, TWIN_ENCODED);
    fs::create_dir(&dir).expect("test: plant a directory under the twin spelling");
    let inside = dir.join("inside.md");
    fs::write(&inside, "somebody else's prose\n").expect("test: write the directory's content");

    let second = scan(&ws.root, &ws.cfg).expect("a directory twin must not fail the scan");

    // Untouched as a directory, and untouched inside: nothing fingerprinted it or tried to unlink it.
    assert!(
        dir.is_dir(),
        "np never removes a directory, however much its name looks like ours: {:?}",
        second.skipped
    );
    assert_eq!(
        fs::read(&inside).expect("test: re-read the file inside the directory"),
        b"somebody else's prose\n",
        "and nothing below it is opened or rewritten"
    );

    // Our shard keeps its own verdict and is unlinked at the path the listing produced. Had the directory's
    // verdict covered both entries sharing the display text, this file would have survived in silence.
    assert!(
        !ours.exists(),
        "a superseded shard np can prove it published is unlinked at its own path: {:?}",
        second.skipped
    );

    // The collision is reported once, in the display text a report line can hold — and it is the only thing
    // this scan had to say about the root.
    let display = String::from_utf8_lossy(TWIN_ENCODED).into_owned();
    assert_eq!(
        second.skipped,
        vec![display.clone()],
        "the directory has to be named and nothing else reported"
    );

    // Both twins stay out of the corpus on their generated name — the walk prunes the directory at depth 1,
    // so its own content is never indexed either — and the shard this scan publishes is written again.
    assert_eq!(second.notes, 2, "{second:?}");
    assert_eq!(second.index_files, first.index_files, "{second:?}");
    assert!(
        ws.root.join(&published).is_file(),
        "{published} is republished"
    );

    // And it is stable rather than fatal: a directory nothing may delete must not turn the next scan into an
    // error, which is the failure mode this branch exists to avoid.
    let third = scan(&ws.root, &ws.cfg).expect("a rescan with the directory still in place");
    assert!(dir.is_dir(), "the directory outlives a second scan");
    assert_eq!(third.skipped, vec![display], "{third:?}");
}

// ---------------------------------------------------------------------------
// F20: the report-side filters match on bytes, so an unreadable (or refused) directory cannot hide a twin it
// never saw. `walk` builds both spellings of every unknown path; these are the cases where one of them used to
// be thrown away — see `docs/design-f20-bytes-in-reports.md` §1 and §5.
// ---------------------------------------------------------------------------

/// Latin-1 `café`: a directory name no `String` can hold, decoding to `caf\u{FFFD}`.
#[cfg(unix)]
const TWIN_DIR_NINE: &[u8] = b"caf\xe9";
/// Latin-1 `cafè`: the twin. A **different** directory, and before F20 one display spelling was enough for the
/// unreadable one of this pair to hide the readable one from `known_rows`.
#[cfg(unix)]
const TWIN_DIR_EIGHT: &[u8] = b"caf\xe8";
/// The display text both of them decode to, which is what the report prints and what a lossy comparison saw.
#[cfg(unix)]
const TWIN_DIR_DISPLAY: &str = "caf\u{fffd}";
/// `rel_bytes` of `<twin>/note.md`, as hex for the queries below: `state`-by-`rel` cannot address one twin of a
/// pair whose display text is one string, so every assertion here names rows by their stored key.
#[cfg(unix)]
const TWIN_EIGHT_KEY: &str = "636166E82F6E6F74652E6D64";
#[cfg(unix)]
const TWIN_NINE_KEY: &str = "636166E92F6E6F74652E6D64";
/// `rel_bytes` of `<readable twin>/renamed.md`: the row the rename step at the end of the fixture has to land on.
#[cfg(unix)]
const TWIN_EIGHT_RENAMED_KEY: &str = "636166E82F72656E616D65642E6D64";

/// The shared premise of both cases below, asserted rather than assumed: two byte-distinct directory names that
/// decode to one display spelling. A filesystem that folds them into one name would make the fixtures prove
/// nothing, so `plant_twin_dir` reports failure and the caller says `NOT COVERAGE`.
#[cfg(unix)]
fn assert_twin_dirs() {
    assert_ne!(TWIN_DIR_NINE, TWIN_DIR_EIGHT);
    assert_eq!(
        String::from_utf8_lossy(TWIN_DIR_NINE).into_owned(),
        String::from_utf8_lossy(TWIN_DIR_EIGHT).into_owned(),
        "the fixture has to produce one display spelling for two directories"
    );
    assert_eq!(String::from_utf8_lossy(TWIN_DIR_NINE), TWIN_DIR_DISPLAY);
}

/// One directory under a byte-spelled name, holding `note.md`. [`scratch_bytes`] writes root-level **files** and
/// cannot make a directory, so this composes the existing `byte_name` helper rather than replacing it. `false`
/// means this filesystem declined the name — the caller prints `NOT COVERAGE` instead of passing quietly (F18's
/// rule), because a fixture that never arrived proves nothing.
#[cfg(unix)]
fn plant_twin_dir(root: &Path, dir: &[u8]) -> bool {
    let path = byte_name(root, dir);
    fs::create_dir(&path).is_ok() && fs::write(path.join("note.md"), "# Twin Heading\n").is_ok()
}

/// Whether 0o000 really denied access to this directory. Checked directly rather than inferred from the scan's
/// own report: a privileged runner (or any platform where the mode is advisory) has to hear `NOT COVERAGE`, and
/// a report that lost a line must still be allowed to fail.
#[cfg(unix)]
fn denied(path: &Path) -> bool {
    fs::read_dir(path).is_err()
}

/// F20 §5's fixture: two sibling directories whose names differ only in bytes UTF-8 cannot hold — latin-1
/// `caf\xE9` beside latin-1 `caf\xE8`, one display spelling `caf\u{FFFD}` — each holding a note, the first made
/// unreadable. Both report-side filters used to compare display text, so the unreadable twin declared the
/// readable one unknown: its live row left `known`, a deletion inside it was never reported, and a rename there
/// became a second live row instead of a move (F20 §1).
///
/// What this pins, in §1's order of harm: the readable twin's row stays in the comparison, so its deletion is
/// still `Removed`; the note behind the unreadable twin keeps its live row (unknown is not absent) and is
/// charged **once** — the charge count itself lives in
/// `scan::tests::one_note_behind_an_unknown_path_is_charged_in_exactly_one_counter`, because `ScanReport`
/// deliberately carries no gap count to assert against.
#[cfg(unix)]
#[test]
fn an_unreadable_directory_does_not_hide_its_readable_twin() {
    assert_twin_dirs();

    let ws = scratch(&[("top.md", "# Top Heading\n")], 600);
    for dir in [TWIN_DIR_NINE, TWIN_DIR_EIGHT] {
        if !plant_twin_dir(&ws.root, dir) {
            eprintln!(
                "NOT COVERAGE: this filesystem refused to create a directory named {dir:?}; \
                 the matcher units in `scan::tests` still cover the byte comparison"
            );
            return;
        }
    }

    let first = scan(&ws.root, &ws.cfg).expect("a twin pair must not fail the scan");
    assert!(first.skipped.is_empty(), "{first:?}");
    assert_eq!((first.added, first.notes), (3, 3), "{first:?}");

    // Premise: two live rows, one display text, distinguished only by their stored bytes.
    let db = ws.db();
    assert_eq!(
        texts(
            &db,
            "SELECT hex(rel_bytes) FROM files WHERE rel = 'caf\u{fffd}/note.md' ORDER BY rel_bytes"
        ),
        [TWIN_EIGHT_KEY, TWIN_NINE_KEY],
        "both twins are tracked under the same display spelling"
    );
    // Make the next scan a refresh with visible work to do: withholding the version is only evidence if the
    // refresh really ran and really reached the note it could read.
    forget_headings(&db, "top.md");
    record_parse_version(&db, outdated_parse_version());
    drop(db);

    let _restore = deny_access(&byte_name(&ws.root, TWIN_DIR_NINE));
    // The premise that decides whether this scan can say anything at all, checked off the disk and not off the
    // report, so a lost report line fails below instead of being mistaken for a platform that cannot chmod.
    if !denied(&byte_name(&ws.root, TWIN_DIR_NINE)) {
        eprintln!(
            "NOT COVERAGE: 0o000 did not deny access to the unreadable twin on this runner \
             (a privileged user?), so this scan hid nothing"
        );
        return;
    }
    fs::remove_file(byte_name(&ws.root, TWIN_DIR_EIGHT).join("note.md"))
        .expect("test setup: delete the readable twin's note");

    let second = scan(&ws.root, &ws.cfg).expect("an unreadable directory must not fail the scan");
    assert_eq!(
        second.skipped,
        vec![TWIN_DIR_DISPLAY.to_string()],
        "the unreadable directory is named, and nothing else is: {second:?}"
    );

    // F20's claim. The deleted note sits under the **readable** twin, so this scan saw its absence and has to
    // report it; before the fix the unreadable twin's identical display spelling hid the row from `known_rows`
    // and the deletion went unreported forever.
    assert_eq!(
        (second.added, second.modified, second.moved, second.removed),
        (0, 0, 0, 1),
        "a deletion inside the readable twin is a deletion: {second:?}"
    );
    let db = ws.db();
    assert_eq!(
        scalar(
            &db,
            &format!(
                "SELECT COUNT(*) FROM files WHERE rel_bytes = X'{TWIN_EIGHT_KEY}' \
                      AND state = 'stale'"
            )
        ),
        1,
        "the readable twin's row is the one marked stale"
    );
    assert_eq!(
        scalar(
            &db,
            &format!(
                "SELECT COUNT(*) FROM files WHERE rel_bytes = X'{TWIN_NINE_KEY}' \
                      AND state = 'ok'"
            )
        ),
        1,
        "unknown is not absent: the note behind the unreadable twin keeps its live row"
    );
    assert_eq!(
        heading_texts(&db, "top.md"),
        vec!["Top Heading".to_string()],
        "the refresh still did its work on the side this scan could read"
    );
    // The charge. Exactly one, which is §4.4's answer: `hidden_by_unknown` owns this note and `written.uncovered`
    // cannot see it, because a walk that could not enter the directory never listed the file. Only their sum is
    // observable from here — a nonzero sum is what withholds the version.
    assert_eq!(
        recorded_parse_version(&db),
        Some(outdated_parse_version()),
        "the note behind the unreadable twin is charged to this refresh"
    );
    drop(db);
    assert_eq!(second.notes, 2, "{second:?}");

    // Convergence: once the subtree is readable again the next scan sees nothing new to report — the deletion
    // was told once, not on every later scan — and the refresh completes.
    drop(_restore);
    let third = scan(&ws.root, &ws.cfg).expect("scan after the directory opens again");
    assert!(third.skipped.is_empty(), "{third:?}");
    assert_eq!(
        (third.added, third.modified, third.moved, third.removed),
        (0, 0, 0, 0),
        "the deletion was reported once: {third:?}"
    );
    assert_eq!(third.notes, 2, "{third:?}");
    assert_eq!(
        recorded_parse_version(&ws.db()),
        Some(PARSE_VERSION),
        "with the subtree visible again the refresh must complete"
    );

    // F20 §1's second consequence, as its own step so nothing above it is re-read: a **rename** inside the
    // readable twin. The deleted note first returns onto the row it never lost, then the unreadable twin is
    // blocked again and the note is renamed where it stands. Byte matching keeps the readable twin's row in
    // `known`, so the new path pairs with its own old row and travels with its `notes.id`; matching display text
    // — what this did before F20 — hid that row from the comparison, so the rename paired with nothing and came
    // back as `Added` beside a still-live old row: one note, two identities, no move reported.
    fs::write(
        byte_name(&ws.root, TWIN_DIR_EIGHT).join("note.md"),
        "# Twin Heading\n",
    )
    .expect("test setup: put the readable twin's note back");
    let fourth = scan(&ws.root, &ws.cfg).expect("scan after the note returns");
    assert_eq!(
        (fourth.added, fourth.moved, fourth.removed),
        (1, 0, 0),
        "a returning file is `Added` onto the row it still has: {fourth:?}"
    );
    let eight_note = scalar(
        &ws.db(),
        &format!("SELECT id FROM notes WHERE rel_bytes = X'{TWIN_EIGHT_KEY}'"),
    );

    let eight = byte_name(&ws.root, TWIN_DIR_EIGHT);
    let _blocked = deny_access(&byte_name(&ws.root, TWIN_DIR_NINE));
    fs::rename(eight.join("note.md"), eight.join("renamed.md"))
        .expect("test setup: rename the readable twin's note");

    let fifth =
        scan(&ws.root, &ws.cfg).expect("a rename beside an unreadable twin must not fail the scan");
    assert_eq!(
        fifth.skipped,
        vec![TWIN_DIR_DISPLAY.to_string()],
        "{fifth:?}"
    );
    assert_eq!(
        (fifth.added, fifth.moved, fifth.removed),
        (0, 1, 0),
        "the rename is a move and not a second live row beside the old one: {fifth:?}"
    );
    assert_eq!(fifth.notes, 3, "`top.md` plus one note per twin: {fifth:?}");
    let db = ws.db();
    assert_eq!(
        scalar(
            &db,
            &format!(
                "SELECT COUNT(*) FROM files WHERE rel_bytes = X'{TWIN_EIGHT_RENAMED_KEY}' \
                      AND state = 'ok'"
            )
        ),
        1,
        "the move landed on the readable twin's own row"
    );
    assert_eq!(
        scalar(
            &db,
            &format!("SELECT id FROM notes WHERE rel_bytes = X'{TWIN_EIGHT_RENAMED_KEY}'")
        ),
        eight_note,
        "and the note travelled with the id it already had"
    );
}

/// The same pair with **both** directories unreadable, which is F20 §3's report claim on a real scan: two paths
/// are two lines even when they print identically, because dedup keys on bytes. Display-text dedup — what this
/// list did before — published one line for two refused or unreadable paths, so the reader could not tell that
/// two subtrees went missing from the catalog. `scan::tests::skipped_lines_dedups_by_bytes_and_not_by_display`
/// is the same rule over a hand-built refused pair, which no Unix walk can produce.
#[cfg(unix)]
#[test]
fn two_unreadable_twins_publish_two_report_lines_that_look_alike() {
    assert_twin_dirs();

    let ws = scratch(&[("top.md", "# Top Heading\n")], 600);
    for dir in [TWIN_DIR_NINE, TWIN_DIR_EIGHT] {
        if !plant_twin_dir(&ws.root, dir) {
            eprintln!(
                "NOT COVERAGE: this filesystem refused to create a directory named {dir:?}; \
                 `scan::tests::skipped_lines_dedups_by_bytes_and_not_by_display` covers the rule meanwhile"
            );
            return;
        }
    }
    scan(&ws.root, &ws.cfg).expect("test: first scan");

    let nine = byte_name(&ws.root, TWIN_DIR_NINE);
    let eight = byte_name(&ws.root, TWIN_DIR_EIGHT);
    let _nine = deny_access(&nine);
    let _eight = deny_access(&eight);
    // Premise off the disk: both really were blocked. If this platform cannot deny access the case is not
    // coverage and says so; if it can, one collapsed line below is a real failure.
    for path in [&nine, &eight] {
        if !denied(path) {
            eprintln!(
                "NOT COVERAGE: 0o000 did not deny access to {} on this runner \
                 (a privileged user?), so nothing was reported twice",
                path.display()
            );
            return;
        }
    }

    let report =
        scan(&ws.root, &ws.cfg).expect("two unreadable directories must not fail the scan");
    assert_eq!(
        report.skipped,
        vec![TWIN_DIR_DISPLAY.to_string(), TWIN_DIR_DISPLAY.to_string()],
        "two byte-distinct paths, two lines — alike because display text cannot tell them apart: {:?}",
        report.skipped
    );
    assert_eq!(report.removed, 0, "unknown is not absent: {report:?}");
    assert_eq!(
        report.notes, 3,
        "both hidden notes stay live in the catalog: {report:?}"
    );
    let db = ws.db();
    assert_eq!(
        scalar(&db, "SELECT COUNT(*) FROM files WHERE state = 'ok'"),
        3,
        "neither subtree was marked stale"
    );
}

/// Rewrite one row's two byte keys into the bytes of its own display text — precisely what migration 4 leaves
/// behind for a name whose OS bytes were never held: `db::copy_files_rows` binds
/// `path_keys::rel_bytes_of_display(rel)` for `rel_bytes` and folds those same bytes into `case_bytes`. The note
/// follows onto the new key with its id, since `notes.rel_bytes` cascades off `files` and `apply_move` rewrites
/// `notes.rel` by hand for a path that changes spelling.
///
/// This is the shape `crates/np-core/tests/db.rs::a_migrated_lossy_key_is_rewritten_by_the_first_byte_keyed_scan`
/// reaches by migrating a real version-three database; writing the two UPDATEs here keeps the v3 schema in the
/// binary that tests migrations rather than carrying it into the scan suite.
#[cfg(unix)]
fn rekey_row_like_migration_four(db: &Db, from_key: &[u8], rel: &str) {
    // The parent first: `foreign_keys` is ON for every opened `Db`, so a child moved to the new key before its
    // parent holds it would be refused, while the cascade does the move for free. With enforcement off the
    // second statement is what carries the note, and it needs no parent at all — same result either way.
    db.0.execute(
        "UPDATE files SET rel = ?1, rel_bytes = ?2, case_bytes = ?3 WHERE rel_bytes = ?4",
        rusqlite::params![
            rel,
            rel.as_bytes(),
            np_core::path_keys::case_fold(rel.as_bytes()),
            from_key
        ],
    )
    .expect("test setup: rekey the row like migration 4");
    db.0.execute(
        "UPDATE notes SET rel = ?1, rel_bytes = ?2 WHERE rel_bytes = ?3",
        rusqlite::params![rel, rel.as_bytes(), from_key],
    )
    .expect("test setup: rekey the note like migration 4");
}

/// F20 §4.2's residual end to end. A row whose stored key is **display text** — migration 4's shape for a path
/// whose OS bytes were never held — cannot be byte-matched against an unreadable directory, because the bytes it
/// holds are the `U+FFFD` spelling of a name whose real bytes are `63 61 66 E9 …`. Matching only bytes called
/// such a row absent: one live note reported `removed: 1`, was marked stale and left the catalog while
/// `hidden_by_unknown` under-charged, so `PARSE_VERSION` was recorded over rows nobody re-parsed — and when the
/// directory became readable again the note came back as a **second `notes.id`**.
///
/// Pinned in that order: the note is not reported removed, keeps its live row under the key it holds, keeps its
/// `notes.id`, and **is** charged — the version stays withheld while the reachable note is still refreshed. Then
/// the recovery, which is what bounds the fallback to legacy rows: the next scan that can see the path re-keys it
/// from the walk's bytes (F13 §4.2's one-time `Moved`, id intact), the refresh completes, and only byte matching
/// can hide this row again.
#[cfg(unix)]
#[test]
fn a_migrated_text_keyed_row_behind_an_unreadable_directory_is_not_reported_removed() {
    const REAL_KEY: &str = "636166E92F6E6F74652E6D64";
    const LOSSY_KEY: &str = "636166EFBFBD2F6E6F74652E6D64";

    let ws = scratch(&[("top.md", "# Top Heading\n")], 600);
    if !plant_twin_dir(&ws.root, TWIN_DIR_NINE) {
        eprintln!(
            "NOT COVERAGE: this filesystem refused to create a directory named {TWIN_DIR_NINE:?}; \
             `scan::tests::a_display_keyed_row_falls_back_to_text_and_a_byte_keyed_row_does_not` covers the \
             matcher meanwhile"
        );
        return;
    }
    scan(&ws.root, &ws.cfg).expect("test: first scan");

    // The premise, checked on the row rather than assumed: a lossy display spelling over real byte-named bytes,
    // keyed the way a version-three database was copied.
    let db = ws.db();
    rekey_row_like_migration_four(&db, b"caf\xE9/note.md", "caf\u{fffd}/note.md");
    assert_eq!(
        key_hex(
            &db,
            "SELECT hex(rel_bytes) FROM files WHERE rel = 'caf\u{fffd}/note.md' ORDER BY rel_bytes"
        ),
        LOSSY_KEY,
        "test setup: the row now holds the bytes of its own display text"
    );
    let note_id = scalar(
        &db,
        &format!("SELECT id FROM notes WHERE rel_bytes = X'{LOSSY_KEY}'"),
    );
    // Give the refresh visible work on the one note this scan will be able to reach, so a withheld version
    // below means "the hidden note was charged" and not "no refresh ran".
    forget_headings(&db, "top.md");
    record_parse_version(&db, outdated_parse_version());
    drop(db);

    let blocked = byte_name(&ws.root, TWIN_DIR_NINE);
    let _restore = deny_access(&blocked);
    if !denied(&blocked) {
        eprintln!(
            "NOT COVERAGE: 0o000 did not deny access to {} on this runner (a privileged user?), so this \
             scan hid nothing",
            blocked.display()
        );
        return;
    }

    let report = scan(&ws.root, &ws.cfg).expect("an unreadable directory must not fail the scan");
    assert_eq!(
        report.skipped,
        vec![TWIN_DIR_DISPLAY.to_string()],
        "the blocked directory is named, and nothing else is: {report:?}"
    );
    assert_eq!(
        (report.added, report.modified, report.moved, report.removed),
        (0, 0, 0, 0),
        "unknown is not absent: the live note behind it is not `Removed`: {report:?}"
    );
    assert_eq!(
        report.notes, 2,
        "the hidden note stays in the catalog: {report:?}"
    );

    let db = ws.db();
    assert_eq!(
        scalar(
            &db,
            &format!(
                "SELECT COUNT(*) FROM files WHERE rel_bytes = X'{LOSSY_KEY}' AND state = 'ok'"
            )
        ),
        1,
        "its row is not marked stale"
    );
    assert_eq!(
        scalar(
            &db,
            &format!("SELECT id FROM notes WHERE rel_bytes = X'{LOSSY_KEY}'")
        ),
        note_id,
        "and it is still the same note, not a fresh row behind a new id"
    );
    assert_eq!(
        heading_texts(&db, "top.md"),
        vec!["Top Heading".to_string()],
        "the note this scan could read is refreshed all the same"
    );
    assert_eq!(
        recorded_parse_version(&db),
        Some(outdated_parse_version()),
        "the hidden note is charged in `hidden_by_unknown`: a refresh that missed it may not call the corpus current"
    );
    drop(db);

    // The bound on the fallback, measured rather than asserted in prose: once the directory can be read, this
    // row is keyed from real bytes and text matching has nothing left to say about it.
    drop(_restore);
    let healed = scan(&ws.root, &ws.cfg).expect("scan after the directory opens again");
    assert!(healed.skipped.is_empty(), "{healed:?}");
    assert_eq!(
        (healed.added, healed.moved, healed.removed),
        (0, 1, 0),
        "the one-time re-key F13 §4.2 predicts, carried rather than removed-and-re-added: {healed:?}"
    );
    let db = ws.db();
    assert_eq!(
        texts(
            &db,
            "SELECT hex(rel_bytes) FROM files WHERE hex(rel_bytes) LIKE '636166%' AND state = 'ok' \
             ORDER BY rel_bytes"
        ),
        [REAL_KEY],
        "the row is keyed from the walk's bytes now, with no stale twin left behind"
    );
    assert_eq!(
        scalar(
            &db,
            &format!("SELECT id FROM notes WHERE rel_bytes = X'{REAL_KEY}'")
        ),
        note_id,
        "with the same `notes.id` it has always had"
    );
    assert_eq!(
        recorded_parse_version(&db),
        Some(PARSE_VERSION),
        "the refresh that finally covered the corpus is allowed to record"
    );
}
