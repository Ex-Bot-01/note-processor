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
    /// `/`-normalised paths, spelled like `rel`, of directories whose contents could
    /// not be read (permission denied, or a directory that vanished mid-walk). Empty
    /// in the common case. A caller must **not** treat files under one of these as
    /// deleted: they are unknown, not absent, and reporting them as Removed would mark
    /// live notes stale.
    pub unreadable: Vec<String>,
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
pub fn walk(root: &Path, opts: &WalkOptions) -> Result<Walked, WalkError> {
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
    let walker = WalkDir::new(root)
        .follow_links(false)
        // `contents_first(false)` is walkdir's spelling of "root first": a directory
        // is yielded before its contents. Named here so a default can never flip.
        .contents_first(false)
        .into_iter()
        .filter_entry(|entry| !is_skipped(entry, &extra_excludes));

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
        if entry.depth() == 0 || !entry.file_type().is_file() {
            continue;
        }
        if let Some(file) = record(&entry, root) {
            files.push(file);
        }
    }

    sort_found(&mut files);
    unreadable.sort();
    unreadable.dedup();
    Ok(Walked { files, unreadable })
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
    use super::{is_excluded, normalize_rel, sort_found, DiscoveredFile};
    use crate::path_keys;
    use std::path::Path;

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
