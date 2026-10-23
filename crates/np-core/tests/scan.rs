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

use np_core::config::Config;
use np_core::db::{self, Db, NoteCounts};
use np_core::hash::sha256_file;
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
    copy_tree(&corpus(), &root);
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

    scan(&ws.root, &ws.cfg).expect("scan");

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
    use std::fs::Permissions;
    use std::os::unix::fs::PermissionsExt;

    let ws = workspace();
    scan(&ws.root, &ws.cfg).expect("first scan");
    let locked = ws.root.join("deep");
    fs::set_permissions(&locked, Permissions::from_mode(0o000)).expect("test: chmod 000");
    // Restored on every path out of this block, so the tempdir can clean up after itself even if
    // an assertion below fails.
    struct Restore(PathBuf);
    impl Drop for Restore {
        fn drop(&mut self) {
            let _ = fs::set_permissions(&self.0, Permissions::from_mode(0o755));
        }
    }
    let _restore = Restore(locked.clone());

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
}
