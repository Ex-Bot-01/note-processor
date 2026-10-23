//! Deterministic workspace discovery: one recursive walk, exclusions applied, sorted output.
//!
//! The result is the input every later task reads files from, so the order has to be
//! reproducible across runs and platforms and no tracked file may be dropped by an
//! odd name.

use std::fs;
use std::io;
use std::path::{Component, Path, PathBuf};
use std::time::SystemTime;

use thiserror::Error;
use walkdir::{DirEntry, WalkDir};

/// Paths of this many characters or more need the Windows extended-length prefix
/// (spec §11: `\\?\` above 260 chars).
const LONG_PATH_CHARS: usize = 260;
/// Windows extended-length path prefix.
const WINDOWS_PREFIX: &str = r"\\?\";
/// Regenerated compressed copies live here; derived output, never an input (spec §7).
const PROCESSED_DIR: &str = "processed";
/// Stem of the catalog file this program writes at the workspace root (spec §7).
const INDEX_STEM: &str = "INDEX";
/// Shards of a catalog too big for one file: `INDEX-<range>.md`.
const INDEX_SHARD_PREFIX: &str = "INDEX-";
/// Markdown extension, matched against the already-lowercased `rel`.
const MARKDOWN_SUFFIX: &str = ".md";

/// What can fail while discovering a workspace.
#[derive(Debug, Error)]
pub enum WalkError {
    /// The workspace root itself could not be read. Entry-level failures never
    /// reach this type: they skip that entry and the walk continues.
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
    /// Path relative to the workspace root, `/`-separated, no leading `./`. Lossily
    /// decoded, so a non-UTF-8 name is readable but not necessarily reproducible.
    pub rel: String,
    /// `rel` lowercased: the comparison key for links, and the case-insensitive
    /// identity Windows needs (spec §11).
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
    /// Extra names to leave out at every depth, matched exactly as the built-in
    /// exclusions are.
    pub extra_excludes: Vec<String>,
}

/// Discover every tracked file under `root`, sorted by `case_key` then `rel`.
///
/// All regular files come back, markdown or not. Symlinks are neither followed nor
/// recorded, so a link back to a parent directory cannot loop. `INDEX.md`,
/// `INDEX-<range>.md`, `processed/`, dot-files and dot-directories stay out; the
/// catalog names only at the root, which is where they are written (spec §7).
///
/// Only a root that cannot be read is an error: an entry whose metadata vanishes
/// mid-walk (deleted, permission revoked) is skipped so one bad file cannot lose the
/// whole workspace. A root that is not a directory simply yields no files.
pub fn walk(root: &Path, opts: &WalkOptions) -> Result<Vec<DiscoveredFile>, WalkError> {
    fs::metadata(root).map_err(|source| WalkError::Root {
        root: root.to_path_buf(),
        source,
    })?;

    let walker = WalkDir::new(root)
        .follow_links(false)
        // `contents_first(false)` is walkdir's spelling of "root first": a directory
        // is yielded before its contents. Named here so a default can never flip.
        .contents_first(false)
        .into_iter()
        .filter_entry(|entry| !is_skipped(entry, &opts.extra_excludes));

    let mut found: Vec<DiscoveredFile> = Vec::new();
    for entry in walker {
        // An unreadable directory or an entry that raced away reports itself as an
        // error and is dropped here; the rest of the workspace is still discovered.
        let Ok(entry) = entry else { continue };
        if entry.depth() == 0 || !entry.file_type().is_file() {
            continue;
        }
        if let Some(file) = record(&entry, root) {
            found.push(file);
        }
    }

    sort_found(&mut found);
    Ok(found)
}

/// `rel` with `/` separators plus the original OS spelling for display:
/// `(rel, display)`. `CurDir` components are dropped, so the workspace root itself
/// normalises to the empty string; input is expected to be relative to that root.
pub fn normalize_rel(os: &Path) -> (String, String) {
    let mut parts: Vec<String> = Vec::new();
    for component in os.components() {
        if let Component::Normal(name) = component {
            parts.push(name.to_string_lossy().into_owned());
        }
    }
    (parts.join("/"), os.to_string_lossy().into_owned())
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
/// added when `windows` is set, `p` is [`LONG_PATH_CHARS`] characters or longer and
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
/// own directory name is.
fn is_skipped(entry: &DirEntry, extra_excludes: &[String]) -> bool {
    if entry.depth() == 0 {
        return false;
    }
    let name = entry.file_name().to_string_lossy();
    is_excluded(&name)
        || extra_excludes
            .iter()
            .any(|excluded| excluded.as_str() == &*name)
        || (entry.depth() == 1 && is_generated_index(&name))
}

/// Names excluded at every depth: dot-files, dot-directories and `processed/`.
fn is_excluded(name: &str) -> bool {
    name.starts_with('.') || name == PROCESSED_DIR
}

/// The catalog files this program writes itself — `INDEX.md` and its shards. They are
/// derived output placed at the workspace root (spec §7), so a nested `INDEX-old.md`
/// is an ordinary note and this exclusion applies only one level below the root.
/// Matching is exact-case because these names are ours to write.
fn is_generated_index(name: &str) -> bool {
    match name.strip_suffix(MARKDOWN_SUFFIX) {
        Some(stem) => stem == INDEX_STEM || stem.starts_with(INDEX_SHARD_PREFIX),
        None => false,
    }
}

/// The record for one regular file, or `None` when its metadata cannot be read — a
/// file deleted or made unreadable between listing and stat is simply not reported.
fn record(entry: &DirEntry, root: &Path) -> Option<DiscoveredFile> {
    let meta = entry.metadata().ok()?;
    let abs = entry.path().to_path_buf();
    let (rel, _display) = normalize_rel(abs.strip_prefix(root).unwrap_or(&abs));
    let case_key = rel.to_lowercase();
    let is_markdown = case_key.ends_with(MARKDOWN_SUFFIX);
    Some(DiscoveredFile {
        rel,
        case_key,
        abs,
        size: meta.len(),
        mtime: epoch_secs(&meta),
        is_markdown,
    })
}

/// Order with no platform dependence: `case_key` ascending, `rel` breaking ties so
/// two names differing only in case (`a.md`, `A.md` on a case-sensitive filesystem)
/// still have one fixed order.
fn sort_found(found: &mut [DiscoveredFile]) {
    found.sort_by(|a, b| a.case_key.cmp(&b.case_key).then_with(|| a.rel.cmp(&b.rel)));
}

/// Modification time as whole seconds since the Unix epoch, `0` when unavailable.
fn epoch_secs(meta: &fs::Metadata) -> i64 {
    meta.modified()
        .ok()
        .and_then(|when| when.duration_since(SystemTime::UNIX_EPOCH).ok())
        .map_or(0, |elapsed| elapsed.as_secs() as i64)
}

#[cfg(test)]
mod tests {
    use super::{is_excluded, is_generated_index, normalize_rel, sort_found, DiscoveredFile};
    use std::path::Path;

    #[test]
    fn normalize_rel_spells_components_with_slash_and_keeps_the_original() {
        let os = Path::new("notes").join("sub").join("a.md");

        let (rel, display) = normalize_rel(&os);

        assert_eq!(rel, "notes/sub/a.md");
        assert_eq!(display, os.to_string_lossy());
    }

    #[test]
    fn normalize_rel_of_the_root_is_empty() {
        assert_eq!(normalize_rel(Path::new("")).0, "");
        assert_eq!(normalize_rel(Path::new(".")).0, "");
    }

    #[test]
    fn generated_index_names_are_recognised_at_exact_case() {
        for name in ["INDEX.md", "INDEX-a-m.md", "INDEX-1-600.md"] {
            assert!(is_generated_index(name), "{name} is generated");
        }
        for name in ["INDEX-old.txt", "index.md", "My-INDEX.md", "notes.md"] {
            assert!(!is_generated_index(name), "{name} is not generated");
        }
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
        let mut found = vec![file("Zeta/b.md"), file("A.md"), file("a.md")];

        sort_found(&mut found);

        let rels: Vec<&str> = found.iter().map(|found| found.rel.as_str()).collect();
        // The tie-break is a plain byte comparison, so the upper-case spelling comes
        // first — the order is fixed, which is all determinism asks for.
        assert_eq!(rels, ["A.md", "a.md", "Zeta/b.md"]);
    }

    /// A record carrying only what the ordering reads.
    fn file(rel: &str) -> DiscoveredFile {
        DiscoveredFile {
            rel: rel.to_string(),
            case_key: rel.to_lowercase(),
            abs: Path::new(rel).to_path_buf(),
            size: 0,
            mtime: 0,
            is_markdown: true,
        }
    }
}
