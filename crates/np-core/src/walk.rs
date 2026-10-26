//! Deterministic workspace discovery: one recursive walk, exclusions applied, sorted output.
//!
//! The result is the input every later task reads files from, so the order has to be
//! reproducible across runs and platforms and no tracked file may be dropped by an
//! odd name.

use std::ffi::OsStr;
use std::fs;
use std::io;
use std::path::{Component, Path, PathBuf};
use std::time::SystemTime;

use thiserror::Error;
use walkdir::{DirEntry, WalkDir};

use crate::index_names;
use crate::path_keys;

/// Paths of this many characters or more need the Windows extended-length prefix
/// (spec §11: `\\?\` above 260 chars).
const LONG_PATH_CHARS: usize = 260;
/// Windows extended-length path prefix.
const WINDOWS_PREFIX: &str = r"\\?\";
/// Regenerated compressed copies live here; derived output, never an input (spec §7).
const PROCESSED_DIR: &str = "processed";
/// Markdown extension, matched against the already-lowercased `rel`. Whether a name is one of the catalog
/// files this program writes is a different question, and [`crate::index_names`] answers it.
const MARKDOWN_SUFFIX: &str = ".md";

/// What can fail while discovering a workspace.
#[derive(Debug, Error)]
pub enum WalkError {
    /// The workspace root is missing, is not a directory, or may not be read.
    /// Nothing below the root reaches this type: an unreadable directory is named in
    /// [`Walked::unreadable`] and an unreadable entry is skipped, so the walk keeps
    /// going wherever it can.
    #[error("cannot read workspace root {}: {source}", .root.display())]
    Root {
        /// The root that could not be read, as the caller spelled it.
        root: PathBuf,
        /// Why the root could not be read.
        source: io::Error,
    },
}

/// One file found in the workspace, before its contents have been read.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DiscoveredFile {
    /// The identity of this path: the OS bytes of the `/`-separated relative path, no leading
    /// `./`. Every later stage keys on these bytes (F13 §4.1), so a name holding a byte UTF-8 cannot
    /// represent stays two distinct files instead of collapsing into one lossy string.
    pub rel_bytes: Vec<u8>,
    /// [`path_keys::case_fold`] of `rel_bytes`: the ASCII-case identity guard that still reports
    /// `a.md` beside `A.md` as one file on a case-sensitive filesystem.
    pub case_bytes: Vec<u8>,
    /// Display spelling of the relative path: `rel_bytes` decoded with replacements. Reports and the
    /// catalog render this; nothing uses it to decide which file a row is (F13 §3).
    pub rel: String,
    /// `rel` lowercased — the Unicode display fold link resolution matches an author's target against
    /// (spec §11). Not an identity: two live rows may share it.
    pub case_key: String,
    /// The real OS path, byte-exact even when `rel` had to be decoded lossily.
    /// This is what [`to_open_path`] turns into an argument for `open()`.
    pub abs: PathBuf,
    /// Size in bytes from the entry's metadata.
    pub size: u64,
    /// Modification time as whole seconds since the Unix epoch; `0` when the
    /// platform hides it or stores a time before the epoch.
    pub mtime: i64,
    /// Whether the name ends in `.md`, matched case-insensitively. Non-markdown
    /// files are still tracked: they get a registry row and can be link targets.
    pub is_markdown: bool,
}

/// Knobs for [`walk`].
#[derive(Debug, Clone, Default)]
pub struct WalkOptions {
    /// Extra names to leave out at every depth. Matched case-insensitively, because
    /// these come from user config and Windows path comparison folds case (spec §11);
    /// the built-in exclusions stay exact-case, since this program writes those names.
    pub extra_excludes: Vec<String>,
}

/// What a walk found, and what it could not look at.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Walked {
    /// Every tracked file under the root, sorted by `case_key` then the byte identity `rel_bytes`.
    pub files: Vec<DiscoveredFile>,
    /// `/`-normalised paths, spelled like `rel`, of directories whose contents this walk **did not read**:
    /// either it could not enter them (permission denied, or a directory that vanished mid-walk) or the
    /// directory's own name was refused below. Empty in the common case. A caller must **not** treat files
    /// under one of these as deleted: they are unknown, not absent, and reporting them as Removed would mark
    /// live notes stale.
    ///
    /// Both shapes belong here because both hide a whole subtree, so both need the same answer — the scan
    /// matches this list by prefix ([`mod@crate::scan`] keeps the rows of every path below an entry) rather
    /// than naming each file inside it. A reason is deliberately not part of this type: the report names the
    /// path either way, and what a caller must do is identical.
    pub unreadable: Vec<String>,
    /// `/`-normalised paths, spelled like `rel`, of **files whose own name** the byte key cannot hold —
    /// on Windows, a name carrying an unpaired surrogate, which `path_keys::name_is_lossless` refuses (F18).
    /// They are left out of [`Walked::files`] rather than indexed under a key two different names may share.
    ///
    /// The two lists answer the same question and differ only in what they name and how wide a hole they cut:
    /// [`Self::unreadable`] holds **directories whose contents were not read** — unreadable, or refused by
    /// name — so everything below one is unknown and it is matched by prefix, while this list holds **one
    /// file** whose contents were never in doubt, only its identity is unspellable, and nothing beside it is
    /// in the dark. Both mean "unknown is not absent", and a caller must suppress `Removed` for both:
    /// [`mod@crate::scan`] names every entry in [`crate::ScanReport::skipped`] and keeps the row each path
    /// already has. Empty on Unix, where `os_bytes` is exact and no name can fail the test.
    pub unrepresentable: Vec<String>,
}

/// Discover every tracked file under `root`, sorted by `case_key` then the byte
/// identity `rel_bytes` (see `sort_found` for why the byte key needs no further tie-break).
///
/// All regular files come back, markdown or not. Symlinks are neither followed nor
/// recorded, so a link back to a parent directory cannot loop and a link to a file is
/// never counted as a second copy of its target. `INDEX.md`, `INDEX-<range>.md`,
/// `processed/`, dot-files and dot-directories stay out; the catalog names only at the
/// root, which is where they are written (spec §7).
///
/// `root` must be a readable directory: missing, unreadable and non-directory roots
/// all fail as [`WalkError::Root`], because returning an empty list for them would
/// look exactly like an emptied workspace. Below the root nothing is silent: a
/// directory that cannot be traversed is named in [`Walked::unreadable`] while the
/// rest of the workspace is still discovered, and an individual entry whose metadata
/// vanishes mid-walk (deleted, permission revoked) is skipped so one bad file cannot
/// lose the walk.
///
/// A path whose **own name** cannot survive the byte spelling ([`crate::path_keys`] refuses it, F18) is not
/// indexed: it is left out of [`Walked::files`], because indexing it would mean keying it by a lossy spelling
/// another live name may already hold. A refused **file** is named in [`Walked::unrepresentable`]; a refused
/// **directory** is never entered at all — its name is the first component of every path below it, so nothing
/// there can be keyed either — and lands in [`Walked::unreadable`], the list that means "this subtree was not
/// read". That is a refusal on one platform only: on Unix `os_bytes` is exact, so nothing reaches either list.
pub fn walk(root: &Path, opts: &WalkOptions) -> Result<Walked, WalkError> {
    walk_with_predicate(root, opts, path_keys::name_is_lossless)
}

/// [`walk`] with the name test handed in rather than fixed to [`crate::path_keys`].
///
/// The refusal is a Windows-only fact about real names: no Linux syscall will put an unpaired surrogate in a
/// file name, so the predicate itself cannot be exercised from a filesystem here. Passing it in is what makes
/// the *consequence* of a refusal testable on every platform — which entries stop being indexed, which are
/// left alone, and which list records the path. Nothing else about the walk reads the predicate, so a stub can
/// only change the refusal, and [`walk`] is the one place that names the real one.
fn walk_with_predicate(
    root: &Path,
    opts: &WalkOptions,
    lossless: fn(&OsStr) -> bool,
) -> Result<Walked, WalkError> {
    // `read_dir`, not `metadata`: stat succeeds on a directory nobody may read, and an
    // unreadable root must not be reported as an empty workspace.
    fs::read_dir(root).map_err(|source| WalkError::Root {
        root: root.to_path_buf(),
        source,
    })?;

    // Config-supplied names are compared folded; see `WalkOptions::extra_excludes`.
    let extra_excludes: Vec<String> = opts
        .extra_excludes
        .iter()
        .map(|name| name.to_lowercase())
        .collect();
    // A refusal records one path, and which list it goes to is the whole difference between the two shapes:
    // a refused file names itself exactly, while a refused directory stands for everything below it and so
    // joins [`Walked::unreadable`], where the scan already matches by prefix. There is no third channel
    // because nothing would do with it that `unreadable` does not already do.
    let mut refused_dirs: Vec<String> = Vec::new();
    let mut refused_files: Vec<String> = Vec::new();
    let walker = WalkDir::new(root)
        .follow_links(false)
        // `contents_first(false)` is walkdir's spelling of "root first": a directory
        // is yielded before its contents. Named here so a default can never flip.
        .contents_first(false)
        .into_iter()
        // The refusal is decided in this filter, not in the loop below, because returning `false` here is the
        // only thing that keeps walkdir out of a directory: a refused subtree has to contribute no entries at
        // all, rather than one report line per child it could not key. Exclusions are still settled first, so
        // a name this program never reads (`processed`, `.git`, a root catalog name) is not newly named here.
        .filter_entry(|entry| {
            if is_skipped(entry, &extra_excludes) {
                return false;
            }
            match refusal(entry, lossless) {
                None => true,
                Some(Refusal::File) => {
                    refused_files.push(entry_rel(entry, root));
                    false
                }
                Some(Refusal::Subtree) => {
                    refused_dirs.push(entry_rel(entry, root));
                    false
                }
            }
        });

    let mut files: Vec<DiscoveredFile> = Vec::new();
    let mut unreadable: Vec<String> = Vec::new();
    for item in walker {
        let entry = match item {
            Ok(entry) => entry,
            // A directory that could not be opened reports an error carrying its own
            // path; everything else (a single unreadable readdir entry) has no name to
            // report and is skipped with the entry it belonged to.
            Err(err) => {
                if let Some(dir) = err.path() {
                    unreadable.push(unreadable_rel(dir, root));
                }
                continue;
            }
        };
        // Exclusions and refusals were settled by the filter above, so what is left is the root, a directory
        // this walk is entering, or a file to record.
        if entry.depth() == 0 || !entry.file_type().is_file() {
            continue;
        }
        if let Some(file) = record(&entry, root) {
            files.push(file);
        }
    }

    sort_found(&mut files);
    // Refused directories and unreadable ones are the same claim about the same shape, so they are sorted and
    // deduped as one list; a directory cannot be both (a refused one was never opened).
    unreadable.extend(refused_dirs);
    let mut unrepresentable = refused_files;
    unreadable.sort();
    unreadable.dedup();
    unrepresentable.sort();
    unrepresentable.dedup();
    Ok(Walked {
        files,
        unreadable,
        unrepresentable,
    })
}

/// Which half of F18's refusal an entry falls under, if any.
#[derive(Debug, PartialEq, Eq)]
enum Refusal {
    /// A **file** whose own name cannot be spelled. One path, so the report names exactly that path and the
    /// scan suppresses exactly that row.
    File,
    /// A **directory** whose own name cannot be spelled. Nothing below it can be keyed either — this name is
    /// the first component of every child's `rel_bytes` — so its whole subtree is refused, and two such
    /// directories (or one beside a directory literally named with U+FFFD) cannot share a key by overwriting
    /// each other's rows one level down.
    Subtree,
}

/// The walk's per-entry refusal decision, taken before anything is keyed.
///
/// F18's rule: a name the byte spelling cannot hold would take its key from U+FFFD, and the row lookup would
/// then rewrite whichever twin already owns that key. Never index it — name it, and let the scan keep the row
/// it has. Always `None` on Unix, where the OS bytes are the key (`crate::path_keys::name_is_lossless`).
fn refusal(entry: &DirEntry, lossless: fn(&OsStr) -> bool) -> Option<Refusal> {
    // The root's own name is in no `rel`: every path below it is spelled relative to it, so refusing it would
    // refuse the whole workspace. A root that cannot be read at all is already [`WalkError::Root`].
    if entry.depth() == 0 || lossless(entry.file_name()) {
        return None;
    }
    if entry.file_type().is_dir() {
        Some(Refusal::Subtree)
    } else if entry.file_type().is_file() {
        Some(Refusal::File)
    } else {
        // A symlink is neither followed nor recorded, so it was never going to hold a row and there is no
        // overwrite here to prevent — naming one in the report would be a new claim about a path this
        // program ignores.
        None
    }
}

/// The `rel` spelling of one entry, which is how both report lists name a path.
fn entry_rel(entry: &DirEntry, root: &Path) -> String {
    let abs = entry.path();
    let (_rel_bytes, rel, _display) = normalize_rel(abs.strip_prefix(root).unwrap_or(abs));
    rel
}

/// Relative spelling of a directory the walk could not read. The root itself has no
/// relative path, and `"."` names it rather than dropping the failure.
fn unreadable_rel(dir: &Path, root: &Path) -> String {
    let (_rel_bytes, rel, _display) = normalize_rel(dir.strip_prefix(root).unwrap_or(dir));
    if rel.is_empty() {
        return ".".to_string();
    }
    rel
}

/// The spellings of one path relative to the workspace root:
/// `(rel_bytes, rel_display, display)`.
///
/// `CurDir` components are dropped, so the workspace root itself normalises to empty in every column;
/// input is expected to be relative to that root. The byte key and the display text come out of **one**
/// component walk (F13 §4.1 takes `to_string_lossy` out of the middle of it rather than adding a second
/// pass), so a path cannot be keyed by one spelling and reported by another.
pub fn normalize_rel(os: &Path) -> (Vec<u8>, String, String) {
    let mut names: Vec<&OsStr> = Vec::new();
    for component in os.components() {
        if let Component::Normal(name) = component {
            names.push(name);
        }
    }
    let rel_bytes = path_keys::rel_bytes(names.iter().copied());
    let rel = names
        .iter()
        .map(|name| name.to_string_lossy().into_owned())
        .collect::<Vec<_>>()
        .join("/");
    (rel_bytes, rel, os.to_string_lossy().into_owned())
}

/// Path spelling to hand to `open()` and friends. Identity everywhere except
/// Windows, where a long path gets the extended-length prefix so `MAX_PATH` cannot
/// truncate it (spec §11). Every file open in this crate goes through here.
#[cfg(not(windows))]
pub fn to_open_path(abs: &Path) -> PathBuf {
    abs.to_path_buf()
}

/// Path spelling to hand to `open()` and friends. Identity everywhere except
/// Windows, where a long path gets the extended-length prefix so `MAX_PATH` cannot
/// truncate it (spec §11). Every file open in this crate goes through here.
#[cfg(windows)]
pub fn to_open_path(abs: &Path) -> PathBuf {
    PathBuf::from(long_prefix(&abs.to_string_lossy(), true))
}

/// Testable core of [`to_open_path`]: `p` with the Windows extended-length prefix
/// added when `windows` is set, `p` is `LONG_PATH_CHARS` characters or longer and
/// does not already carry the prefix. Deliberately not `#[cfg]`-gated so one machine
/// tests both rules; it expects an absolute path, because a prefixed relative path
/// is not valid on Windows.
pub fn long_prefix(p: &str, windows: bool) -> String {
    if !windows || p.starts_with(WINDOWS_PREFIX) || p.chars().count() < LONG_PATH_CHARS {
        return p.to_string();
    }
    format!("{WINDOWS_PREFIX}{p}")
}

/// Whether `entry` — and everything below it when it is a directory — stays out of
/// the walk. The root itself is never excluded: the user named it, however hidden its
/// own directory name is. `extra_excludes` arrives already lowercased.
fn is_skipped(entry: &DirEntry, extra_excludes: &[String]) -> bool {
    if entry.depth() == 0 {
        return false;
    }
    let name = entry.file_name().to_string_lossy();
    if is_excluded(&name) {
        return true;
    }
    if !extra_excludes.is_empty() {
        let folded = name.to_lowercase();
        if extra_excludes.iter().any(|excluded| excluded == &folded) {
            return true;
        }
    }
    // Generated catalog names are written at the root alone, so this exclusion applies only one level
    // below it; a nested `INDEX-old.md` is an ordinary note.
    entry.depth() == 1 && index_names::is_generated_index(&name)
}

/// Names excluded at every depth: dot-files, dot-directories and `processed/`.
fn is_excluded(name: &str) -> bool {
    name.starts_with('.') || name == PROCESSED_DIR
}

/// The record for one regular file, or `None` when its metadata cannot be read — a
/// file deleted or made unreadable between listing and stat is simply not reported.
fn record(entry: &DirEntry, root: &Path) -> Option<DiscoveredFile> {
    let meta = entry.metadata().ok()?;
    let abs = entry.path().to_path_buf();
    let (rel_bytes, rel, _display) = normalize_rel(abs.strip_prefix(root).unwrap_or(&abs));
    // Both folds through `path_keys` rather than locally (F13 §4): `case_bytes` is the ASCII byte fold
    // that guards identity and `case_key` the Unicode display fold link resolution reads, and this walk
    // is the one place either is derived — neither is ever written without the other.
    let case_bytes = path_keys::case_fold(&rel_bytes);
    let case_key = path_keys::display_fold(&rel);
    let is_markdown = case_key.ends_with(MARKDOWN_SUFFIX);
    Some(DiscoveredFile {
        rel_bytes,
        case_bytes,
        rel,
        case_key,
        abs,
        size: meta.len(),
        mtime: epoch_secs(&meta),
        is_markdown,
    })
}

/// Order with no platform dependence and no ties: `case_key` ascending, then the byte identity
/// `rel_bytes`.
///
/// The byte key is what makes this total on its own. Comparing display `rel` needed an `abs` tie-break,
/// because two different raw names decode to one lossy `rel` and their order then fell to whichever path
/// was read last (F13 §1). Byte keys cannot tie, so the absolute path is no longer part of the answer.
fn sort_found(found: &mut [DiscoveredFile]) {
    found.sort_by(|a, b| {
        a.case_key
            .cmp(&b.case_key)
            .then_with(|| a.rel_bytes.cmp(&b.rel_bytes))
    });
}

/// Modification time as whole seconds since the Unix epoch, `0` when unavailable.
///
/// Shared with [`crate::scan`], which re-states a file before reading it: comparing that second
/// against the walk's is what catches a file rewritten mid-scan, so both readings have to round
/// time the same way.
pub(crate) fn epoch_secs(meta: &fs::Metadata) -> i64 {
    meta.modified()
        .ok()
        .and_then(|when| when.duration_since(SystemTime::UNIX_EPOCH).ok())
        .map_or(0, |elapsed| elapsed.as_secs() as i64)
}

#[cfg(test)]
mod tests {
    use super::{
        is_excluded, normalize_rel, refusal, sort_found, walk_with_predicate, DiscoveredFile,
        Refusal, WalkOptions,
    };
    use crate::path_keys;
    use std::ffi::OsStr;
    use std::fs;
    use std::path::Path;
    use walkdir::WalkDir;

    /// A tree with both refusal shapes side by side, for the tests that walk it with the real predicate.
    const TREE: [&str; 6] = [
        "refused-dir/a.md",
        "refused-dir/nested/b.md",
        "refused-file.md",
        "kept.md",
        "sibling/peer.md",
        "sibling/refused-nested.md",
    ];

    /// The name prefix [`refuse_that_name`] refuses — a stand-in for the unpaired surrogate that only Windows
    /// can put into a directory entry (F18). Two words are refused so one tree can hold both shapes: the
    /// directory `refused-dir` and the file `refused-file.md`.
    const REFUSED: &str = "refused";

    /// A predicate refusing every name that starts with [`REFUSED`] and keeping everything else.
    ///
    /// What matters is not the rule but what it lets a Linux test build: a refusal the walk has to react to,
    /// made of ordinary names. It is compared as text because the real predicate reads UTF-16 units, which no
    /// `&str` here can hold.
    fn refuse_that_name(name: &OsStr) -> bool {
        !name.to_string_lossy().starts_with(REFUSED)
    }

    /// A workspace root holding `names` (`/`-separated, parents created as needed).
    fn tree(names: &[&str]) -> tempfile::TempDir {
        let tmp = tempfile::TempDir::new().expect("test setup: tempdir");
        for name in names {
            let path = tmp.path().join(name);
            if let Some(parent) = path.parent() {
                fs::create_dir_all(parent).expect("test setup: create parent");
            }
            fs::write(&path, "body").expect("test setup: write a file");
        }
        tmp
    }

    /// The refused-directory half of F18's refusal, which no filesystem here can express as a name.
    ///
    /// A directory named with an unpaired surrogate used to be descended: its children keyed as
    /// `dir\u{fffd}/n.md`, so two such directories — or one beside a directory literally named U+FFFD — shared
    /// `rel_bytes` and the row lookup overwrote one note with the other. Same silent overwrite, one level down.
    #[test]
    fn a_refused_directory_is_recorded_as_a_prefix_and_its_subtree_is_never_read() {
        // Only the directory shape, so "nothing else was recorded" is a question one list can answer.
        let tmp = tree(&[
            "refused-dir/a.md",
            "refused-dir/nested/b.md",
            "kept.md",
            "sibling/c.md",
        ]);

        let walked = walk_with_predicate(tmp.path(), &WalkOptions::default(), refuse_that_name)
            .expect("a refusal is not a walk failure");

        // (i) nothing below the refused directory was indexed, at either depth, and (ii) every neighbour is
        // untouched: the refusal costs one subtree, not the workspace.
        let rels: Vec<String> = walked.files.iter().map(|file| file.rel.clone()).collect();
        assert_eq!(
            rels,
            ["kept.md", "sibling/c.md"],
            "only the refused subtree's files are missing: {rels:?}"
        );
        // (iii) recorded **once**, as the directory itself — two files lived below it, so a per-child record
        // would name three paths here.
        assert_eq!(
            walked.unreadable,
            ["refused-dir".to_string()],
            "one prefix entry, not one per unreadable child"
        );
        // And recorded in the list the scan matches by **prefix**: a refused directory hides a subtree, which
        // is what [`super::Walked::unreadable`] means. Exact equality would leave every row below it exposed
        // to a `Removed` the next scan cannot support.
        assert!(
            walked.unrepresentable.is_empty(),
            "a directory is not one file: {:?}",
            walked.unrepresentable
        );
    }

    /// A refusal is recorded by **path**, not by file name. Every other refusal fixture puts the refused
    /// directory at depth 1, where the two spellings coincide — so a `rel` built from `file_name()` instead of
    /// `strip_prefix(root)` would pass them all and then hide the wrong subtree the moment a real workspace
    /// nested one (F18 review).
    #[test]
    fn a_refused_directory_is_recorded_by_its_path_not_its_file_name() {
        let tmp = tree(&["deep/refused-dir/x.md", "refused-dir/y.md", "kept.md"]);

        let walked = walk_with_predicate(tmp.path(), &WalkOptions::default(), refuse_that_name)
            .expect("a refusal is not a walk failure");

        assert_eq!(
            walked.unreadable,
            ["deep/refused-dir", "refused-dir"]
                .iter()
                .map(|name| name.to_string())
                .collect::<Vec<_>>(),
            "the nested refusal must carry its parent, or the prefix rule hides nothing: {:?}",
            walked.unreadable
        );
    }

    /// The refused-file half, kept exact on purpose: a refused file says nothing about the paths beside it.
    #[test]
    fn a_refused_file_is_recorded_as_its_own_path_alone() {
        // Only the file shape, so an empty subtree list means what it says.
        let tmp = tree(&[
            "refused-file.md",
            "kept.md",
            "sibling/peer.md",
            "sibling/refused-nested.md",
        ]);

        let walked = walk_with_predicate(tmp.path(), &WalkOptions::default(), refuse_that_name)
            .expect("a refusal is not a walk failure");

        assert_eq!(
            walked.unrepresentable,
            [
                "refused-file.md".to_string(),
                "sibling/refused-nested.md".to_string()
            ],
            "each refused file names itself, relative to the root"
        );
        // The sibling that shares a directory with a refused file is indexed, and nothing about either one
        // reaches the subtree list: `sibling/` was read, in full.
        let rels: Vec<String> = walked.files.iter().map(|file| file.rel.clone()).collect();
        assert!(
            rels.contains(&"sibling/peer.md".to_string()),
            "a refused file suppresses itself only: {rels:?}"
        );
        assert!(walked.unreadable.is_empty(), "{:?}", walked.unreadable);
    }

    /// (iv) The real predicate refuses nothing this walk can reach, so all of the above is Windows-only
    /// behaviour in production: the same tree through [`super::walk`] indexes every file and names nothing.
    #[test]
    fn the_real_predicate_refuses_nothing_this_walk_can_reach() {
        let tmp = tree(&TREE);

        let walked = super::walk(tmp.path(), &WalkOptions::default()).expect("walk");

        assert!(
            walked.unrepresentable.is_empty(),
            "{:?}",
            walked.unrepresentable
        );
        assert!(walked.unreadable.is_empty(), "{:?}", walked.unreadable);
        let rels: Vec<String> = walked.files.iter().map(|file| file.rel.clone()).collect();
        assert_eq!(rels.len(), TREE.len(), "every file is indexed: {rels:?}");

        // The same answer one entry earlier: the decision function, fed the real predicate and real directory
        // entries. Without this, a stub-only test would leave `path_keys::name_is_lossless` unconnected to the
        // branch it drives on this platform.
        for entry in WalkDir::new(tmp.path())
            .into_iter()
            .map(|entry| entry.expect("test setup: every entry of a tree just written is listable"))
        {
            assert_eq!(
                refusal(&entry, path_keys::name_is_lossless),
                None,
                "{} is a name Unix keys exactly",
                entry.path().display()
            );
        }
    }

    /// The decision table itself: which shape a refused name cuts, including the two cases that must **not**
    /// be refused — the workspace root, and anything that was never going to hold a row.
    #[test]
    fn the_refusal_decision_names_the_shape_that_was_refused() {
        let tmp = tree(&TREE);
        // A root whose own name would be refused: its name is in no `rel`, so refusing it would mean refusing
        // the whole workspace, and a root nobody can open is already `WalkError::Root`.
        let refused_root = tmp.path().join("refused-root");
        fs::create_dir_all(refused_root.join("inner")).expect("test setup: refused root subdir");

        let entry_of = |root: &Path, wanted: &str| {
            WalkDir::new(root)
                .into_iter()
                .filter_map(Result::ok)
                .find(|entry| entry.file_name().to_string_lossy() == wanted)
                .unwrap_or_else(|| panic!("test setup: {wanted} is in the tree"))
        };

        assert_eq!(
            refusal(&entry_of(&refused_root, "refused-root"), refuse_that_name),
            None,
            "depth 0 is never refused"
        );
        assert_eq!(
            refusal(&entry_of(tmp.path(), "refused-dir"), refuse_that_name),
            Some(Refusal::Subtree)
        );
        assert_eq!(
            refusal(&entry_of(tmp.path(), "refused-file.md"), refuse_that_name),
            Some(Refusal::File)
        );
        assert_eq!(
            refusal(&entry_of(tmp.path(), "kept.md"), refuse_that_name),
            None,
            "a name the predicate accepts is indexed"
        );

        // A symlink is neither followed nor recorded, so it holds no row and there is no overwrite to prevent:
        // refusing one would add a report line about a path this program ignores.
        #[cfg(unix)]
        {
            std::os::unix::fs::symlink(tmp.path().join("kept.md"), tmp.path().join("refused-link"))
                .expect("test setup: symlink to a file");
            assert_eq!(
                refusal(&entry_of(tmp.path(), "refused-link"), refuse_that_name),
                None,
                "a symlink was never going to be indexed"
            );
        }
    }

    #[test]
    fn normalize_rel_spells_components_with_slash_and_keeps_the_original() {
        let os = Path::new("notes").join("sub").join("a.md");

        let (rel_bytes, rel, display) = normalize_rel(&os);

        assert_eq!(rel_bytes, b"notes/sub/a.md".to_vec());
        assert_eq!(rel, "notes/sub/a.md");
        assert_eq!(display, os.to_string_lossy());
    }

    #[test]
    fn normalize_rel_of_the_root_is_empty() {
        assert_eq!(normalize_rel(Path::new("")).0, Vec::<u8>::new());
        assert_eq!(normalize_rel(Path::new(".")).0, Vec::<u8>::new());
    }

    #[test]
    fn dot_names_and_processed_are_excluded_everywhere() {
        assert!(is_excluded(".git"));
        assert!(is_excluded(".draft.md"));
        assert!(is_excluded("processed"));
        assert!(!is_excluded("notes"));
        assert!(
            !is_excluded("INDEX.md"),
            "a root-level rule, not a name rule"
        );
    }

    #[test]
    fn sort_orders_by_case_key_then_rel() {
        // Given in the reverse of the tie order, so a comparator that stops at
        // `case_key` would leave `a.md` first and fail.
        let mut found = vec![
            file("Zeta/b.md", "Zeta/b.md"),
            file("a.md", "a.md"),
            file("A.md", "A.md"),
        ];

        sort_found(&mut found);

        let rels: Vec<&str> = found.iter().map(|found| found.rel.as_str()).collect();
        // The tie-break is a plain byte comparison, so the upper-case spelling comes
        // first — the order is fixed, which is all determinism asks for.
        assert_eq!(rels, ["A.md", "a.md", "Zeta/b.md"]);
    }

    #[test]
    fn sort_is_total_when_lossy_names_collide() {
        // F13 §1's collision, built so the collision is *inside* the fixture: 0xFF and the UTF-8 encoding
        // of a literal replacement character both spell `we\u{fffd}.md`, so their display `rel` and their
        // `case_key` are the same string and `case_key` cannot decide this order. Every other field is
        // deliberately identical too — same `abs`, same size, same mtime — which leaves `rel_bytes` as the
        // only thing in a `DiscoveredFile` that can separate them.
        let mut found = vec![
            file_bytes(b"we\xff.md", "we\u{fffd}.md"),
            file_bytes("we\u{fffd}.md".as_bytes(), "we\u{fffd}.md"),
        ];

        sort_found(&mut found);

        let keys: Vec<Vec<u8>> = found.iter().map(|found| found.rel_bytes.clone()).collect();
        // Byte order, not display order: 0xEF (the first byte of U+FFFD) precedes 0xFF at index 2. The
        // input order is the reverse of this on purpose — `sort_by` is stable, so a comparator that stops
        // at `case_key`, or compares display `rel`, leaves the pair as it was handed over and fails here.
        assert_eq!(
            keys,
            ["we\u{fffd}.md".as_bytes().to_vec(), b"we\xff.md".to_vec()],
            "one display spelling, two byte identities: the order comes from the key"
        );
    }

    /// A record carrying only what the ordering reads, keyed by its display spelling.
    fn file(rel: &str, abs: &str) -> DiscoveredFile {
        DiscoveredFile {
            rel_bytes: path_keys::rel_bytes_of_display(rel),
            case_bytes: path_keys::case_fold(rel.as_bytes()),
            rel: rel.to_string(),
            case_key: path_keys::display_fold(rel),
            abs: Path::new(abs).to_path_buf(),
            size: 0,
            mtime: 0,
            is_markdown: true,
        }
    }

    /// The same record from the raw bytes a Unix walk would have recorded.
    fn file_bytes(rel_bytes: &[u8], rel: &str) -> DiscoveredFile {
        let mut found = file(rel, "/ws");
        found.rel_bytes = rel_bytes.to_vec();
        found.case_bytes = path_keys::case_fold(rel_bytes);
        found
    }
}
