use std::ffi::OsStr;
use std::fs;
use std::path::{Path, PathBuf};

use crate::db::{Db, DbError};

use crate::index_names;

use crate::catalog;
use crate::path_keys::os_bytes;
use crate::scan::state::set_meta;
use crate::scan::state::INDEX_FILES_KEY;
use crate::scan::ScanError;
use std::io::{self, Read};

/// Suffix of the temporary file every catalog write goes through before its `rename`.
const TEMP_SUFFIX: &str = ".tmp";

/// How many display spellings one record names, most actionable first (see [`Unindexable::names`]).
///
/// Bounded because the value is written on **every** scan: a workspace with an unreadable subtree under a
/// `chmod -R 000` would otherwise grow one `meta` row's text without limit, and a person reading it can act
/// on eight paths no better than on eight hundred.
pub(crate) const NAME_CAP: usize = 8;

/// Record the index files this scan publishes, so a later scan knows which shards to delete.
///
/// Written inside the scan's transaction, before the files exist: `meta` describes the catalog this
/// program is responsible for, and the next scan tolerates naming a file that never arrived
/// ([`write_index_files`] ignores `NotFound`). Committing it afterwards cannot fail safely — a kill in
/// that window leaves a shard on disk that no delete list mentions.
pub(crate) fn set_index_list(db: &Db, rendered: &[(String, String)]) -> Result<(), DbError> {
    let value = rendered
        .iter()
        .map(|(name, _)| name.as_str())
        .collect::<Vec<_>>()
        .join("\n");
    set_meta(db, INDEX_FILES_KEY, &value)
}

/// Write the rendered catalog and delete the shards this scan no longer produces.
///
/// The order is the whole contract, because one set of files cannot be published atomically:
///
/// 1. Every shard, each written to a temporary name and moved in with one `rename`, so a reader never
///    sees a half-written file: the regenerate is whole-file, never in place.
/// 2. `INDEX.md` last. Until it moves, the old catalog still names only files that exist; from then on
///    the new one does too. Renaming it first is what let an entry point advertise shards that were
///    never written while superseded shards of another generation survived beside them.
/// 3. Superseded names, only once the entry point that no longer mentions them is live.
///
/// The paths removed in step 3 come from [`superseded_names`] — the directory and `meta` together — rather
/// than from `meta` alone. A name already gone from disk is the state this call wanted, which is what
/// makes [`set_index_list`] safe to commit ahead of these writes.
///
/// Returns the root entries that wear a generated name without carrying this program's fingerprint: they
/// are left exactly as they are and reach [`ScanReport::skipped`], because this publishes what np wrote and
/// does not remove whatever is merely named like it.
pub(crate) fn write_index_files(
    root: &Path,
    rendered: &[(String, String)],
    previous: &[String],
) -> Result<Vec<String>, ScanError> {
    // Collected before anything is written: the deletion set is what disk and `meta` hold minus this
    // scan's generation, so reading the root first cannot see a file this call is about to publish — and
    // a root that cannot be listed fails the scan before it has touched anything.
    let superseded = superseded_names(root, rendered, previous)?;

    let entry_point = index_names::ENTRY;
    for (name, contents) in rendered.iter().filter(|(name, _)| *name != entry_point) {
        publish(root, name, contents)?;
    }
    for (name, contents) in rendered.iter().filter(|(name, _)| *name == entry_point) {
        publish(root, name, contents)?;
    }

    // Every path here is the one the listing produced, never `root.join` of a display spelling: two root
    // entries can share one lossy spelling and only one of them is the file the verdict was about.
    for path in superseded.delete {
        match fs::remove_file(&path) {
            Err(source) if source.kind() == io::ErrorKind::NotFound => {}
            Err(source) => return Err(ScanError::Io { path, source }),
            Ok(()) => {}
        }
    }
    Ok(superseded.foreign)
}

/// The generated names this workspace holds that this scan does not publish: what `meta` lists and what
/// is actually sitting in the root, unioned and judged **one directory entry at a time**.
///
/// Each entry is keyed by its file-name bytes ([`os_bytes`]), not by its display text. Two byte-distinct
/// entries whose names UTF-8 cannot hold — `INDEX-\xff.md` beside `INDEX-\xef\xbf\xbd.md` — lose to the same
/// `U+FFFD` spelling, and a verdict taken on that spelling answers for whichever one readdir happened to
/// reach first: it can unlink a header-less foreign file at its twin's bytes, or leave our own shard alive
/// in silence. That is the display-text trap on the one code path here that deletes files, so identity is
/// compared as bytes end to end and the lossy text is kept only for what is genuinely a display question —
/// [`index_names::is_generated_index`] and the report line.
///
/// Which leaves one asymmetry to know about: **verdicts are per entry, report lines are not.** `foreign`
/// carries display text only — it was never a path the walk keyed — so the bytes [`ScanReport::skipped`] dedups
/// by are that text's own UTF-8 bytes, and two byte-distinct foreign twins still publish a single line. Read
/// that count as "names needing attention", never as "files on disk"; the paths the *walk* named do get one
/// line each.
///
/// `meta` alone is not enough. It is committed inside the scan's transaction (see [`set_index_list`]), so
/// a kill between that commit and the writes in [`write_index_files`] leaves it naming only the
/// generation that was never published; a superseded shard still on disk appears in no list, and nothing
/// would ever delete it. The listing sees reality instead, and the union covers both directions — which
/// is also what lets `meta` be committed ahead of the files.
///
/// Both sources are filtered by [`index_names::is_generated_index`], which only decides what is *worth
/// asking about*. What decides what is ours to remove is the file's own fingerprint: a candidate is deleted only
/// when it is a regular file opening with [`catalog::HEADER`] byte for byte, in either its LF or its CRLF
/// spelling (see [`np_published`]). A name that merely looks generated belongs to somebody else — a
/// hand-written `INDEX-old.md` in the root is never indexed, because [`walk`] excludes these names, so
/// deleting it would destroy a note no scan ever wrote. Such a candidate, and a directory
/// wearing the name, are returned for the report instead: the user learns about the collision, and a
/// `remove_file` that could only answer `IsADirectory` never breaks every later scan of the workspace.
///
/// The fingerprint is read as the *whole* identity signal, and that has one accepted consequence: a file
/// carrying those two header lines under a generated name **is** deleted even when this program never
/// published it and a person wrote the prose. Authorship is not recoverable from anything else on disk —
/// separating an edited shard from a hand-written note would mean rendering the corpus and comparing
/// bytes, the guess the fingerprint exists to avoid — so the header decides, not who typed the body.
/// `a_hand_written_file_carrying_our_header_under_a_generated_name_is_deleted` pins that as intended
/// behaviour, and it stands as an accepted residual.
///
/// A `meta` name no longer on disk appears in neither list. Its absence is already the state this call
/// wanted — which is exactly how the delete loop in [`write_index_files`] reads an
/// [`io::ErrorKind::NotFound`] — and naming it in the report would be a collision nobody can see.
fn superseded_names(
    root: &Path,
    rendered: &[(String, String)],
    previous: &[String],
) -> Result<Superseded, ScanError> {
    let entries = fs::read_dir(root).map_err(|source| ScanError::Io {
        path: root.to_path_buf(),
        source,
    })?;
    // Every generated-looking entry in the root, tagged with whether it is ours. The verdict is taken while
    // the listing runs, because `entry.file_type` is the cheapest answer to "file or directory" and a
    // second `stat` would race with whatever renamed the entry between the two calls.
    let mut present: Vec<Listed> = Vec::new();
    for entry in entries {
        let entry = entry.map_err(|source| ScanError::Io {
            path: root.to_path_buf(),
            source,
        })?;
        let name = entry.file_name();
        let display = name.to_string_lossy().into_owned();
        if !index_names::is_generated_index(&display) {
            continue;
        }
        // The file type is asked before the contents: a directory has no header to read, and letting
        // `File::open` answer for it would pretend a directory could be this program's output.
        let path = entry.path();
        let ours = entry.file_type().is_ok_and(|kind| kind.is_file()) && np_published(&path);
        present.push(Listed {
            name_bytes: os_bytes(&name),
            path,
            display,
            ours,
        });
    }

    // The union the crash window needs: what `meta` names and what the root holds. A key in either list
    // is judged by what is actually on disk, never by which list produced it. One sort and dedup on the
    // byte keys is also what keeps the deletion set free of duplicates: every entry below is one `root.join`
    // away from its name, so distinct name bytes mean distinct paths.
    let mut keys: Vec<Vec<u8>> = present
        .iter()
        .map(|listed| listed.name_bytes.clone())
        .collect();
    keys.extend(
        previous
            .iter()
            .filter(|name| index_names::is_generated_index(name))
            .map(|name| os_bytes(OsStr::new(name))),
    );
    keys.sort();
    keys.dedup();

    // What this scan publishes, spelled the same way. `publish` writes a name's bytes as the file name, so
    // a key equal to one of these *is* the file just written; an entry that merely displays like a published
    // name is a different file and stays a candidate under its own fingerprint.
    let published: Vec<Vec<u8>> = rendered
        .iter()
        .map(|(name, _)| os_bytes(OsStr::new(name)))
        .collect();

    let mut superseded = Superseded::default();
    for key in keys {
        if published.contains(&key) {
            continue;
        }
        match present.iter().find(|listed| listed.name_bytes == key) {
            // Ours, unlinked at the path this entry actually has.
            Some(listed) if listed.ours => superseded.delete.push(listed.path.clone()),
            // On disk, but not ours: a foreign note or a directory wearing one of our names. Reported as
            // display text because a report line is a display question.
            Some(listed) => superseded.foreign.push(listed.display.clone()),
            // Named by `meta` and already gone — the state this call wanted, which is how the delete loop
            // in [`write_index_files`] reads an [`io::ErrorKind::NotFound`]. Reporting it would name a
            // collision that is nothing but a shard this program never got round to writing.
            None => {}
        }
    }
    Ok(superseded)
}

/// One generated-looking root entry as the listing saw it: enough to judge it, enough to act on it.
struct Listed {
    /// Identity: the file name's OS bytes, which keep two entries sharing a lossy spelling apart. On Windows
    /// those bytes are exact (`wtf8`), so twins stay apart there too. The residual is only the third target —
    /// neither Unix nor Windows — where no exact spelling exists to take.
    name_bytes: Vec<u8>,
    /// What gets unlinked, straight from `entry.path()` — the bytes this entry really has on disk.
    path: PathBuf,
    /// The lossy display spelling: what [`index_names::is_generated_index`] reads and what the report names.
    display: String,
    /// Whether it is a regular file opening with [`catalog::HEADER`]: ours to remove.
    ours: bool,
}

/// The generated names one scan found in its root that it does not publish, split by who owns them.
#[derive(Default)]
struct Superseded {
    /// Files this program published in an earlier generation: safe to unlink. Real paths from the listing,
    /// because a display spelling can name a different entry than the one the verdict judged.
    delete: Vec<PathBuf>,
    /// Entries wearing a generated name that this program did not publish: left on disk and reported, in
    /// the display text a report line is written in.
    foreign: Vec<String>,
}

/// Whether `path` opens with the bytes [`catalog::render`] puts at the top of every file it publishes —
/// two header lines, byte for byte, in either the LF or the CRLF spelling. That is how a scan tells its
/// own output from a note that happens to be called `INDEX-something.md`.
///
/// Only those bytes are read, so a candidate's size never decides what a scan costs, and every failure —
/// unreadable, shorter than the header, an I/O error — answers "not ours". The bias is deliberate:
/// guessing wrong keeps a stale shard on disk rather than deleting a note.
///
/// CRLF is accepted because identity lives in the two header lines and nowhere else: a checkout, editor
/// or sync tool that rewrote the line endings of a shard np published leaves the catalog intact, and
/// refusing it would strand that shard in `skipped` forever. The same rule is why nothing after the
/// header is examined — prose cannot change who wrote the file — and why the match stays line-exact: any
/// other byte difference in those two lines, including a half-converted header, is not ours.
pub(super) fn np_published(path: &Path) -> bool {
    let mut source = match fs::File::open(path) {
        Ok(source) => source,
        Err(_) => return false,
    };
    // The longer of the two accepted spellings sets the window. A file that ends earlier simply yields
    // fewer bytes, which is how a published shard whose header is its whole content still matches.
    let mut head = [0u8; catalog::HEADER_CRLF.len()];
    let read = match read_prefix(&mut source, &mut head) {
        Ok(read) => read,
        Err(_) => return false,
    };
    let head = &head[..read];
    head.starts_with(catalog::HEADER.as_bytes())
        || head.starts_with(catalog::HEADER_CRLF.as_bytes())
}

/// Fill `buf` from `source`, stopping at the end of the file. Returns how many bytes `buf` holds; an I/O
/// error is reported so the caller can answer its own question with "not ours".
fn read_prefix(source: &mut fs::File, buf: &mut [u8]) -> io::Result<usize> {
    let mut read = 0;
    while read < buf.len() {
        match source.read(&mut buf[read..]) {
            // End of file: fewer bytes than asked for, which is an answer rather than a failure.
            Ok(0) => break,
            Ok(got) => read += got,
            Err(err) if err.kind() == io::ErrorKind::Interrupted => {}
            Err(err) => return Err(err),
        }
    }
    Ok(read)
}

/// Move one rendered file into place through a temporary name, so a reader never sees it half-written
/// and a kill leaves either the old bytes or the new ones.
fn publish(root: &Path, name: &str, contents: &str) -> Result<(), ScanError> {
    let temp = root.join(format!("{name}{TEMP_SUFFIX}"));
    let target = root.join(name);
    fs::write(&temp, contents).map_err(|source| ScanError::Io {
        path: temp.clone(),
        source,
    })?;
    fs::rename(&temp, &target).map_err(|source| ScanError::Io {
        path: target,
        source,
    })?;
    Ok(())
}

/// Remove the `INDEX*.tmp` files a rename that died left behind.
///
/// A temporary name is not one [`index_names::is_generated_index`] recognises, so an orphaned corpse left by a
/// killed `rename` would be walked, hashed and catalogued-as-nothing as an ordinary attachment, and
/// nothing would ever clean it up. That is why [`scan`] calls this before the walk rather than where the
/// temporaries are made: cleaned up on the way out, the corpse would still be indexed once by its own
/// scan. Removal stops at directories — something named `INDEX.md.tmp` that is one belongs to somebody
/// else.
///
/// A corpse that will not go away (an immutable or locked file, a root this user may list but not write)
/// comes back in the returned list and reaches [`ScanReport::skipped`] instead of failing the scan: one
/// stuck temporary must not stop every later scan of a workspace whose notes read perfectly well. It is
/// indexed once by the scan that could not remove it — cleanup runs before the walk, so the file is there
/// when the walk looks — and the next scan tries again.
///
/// One limitation to know before someone widens this: cleanup is **root-only**, while
/// [`index_names::is_generated_index`] is applied by [`crate::walk`] to root-level names only (depth 1) —
/// so a `notes/INDEX.md.tmp`
/// would be indexed as an attachment forever. It is unreachable today because [`publish`] joins the root
/// alone and no other code makes these names.
pub(crate) fn clear_temp_corpses(root: &Path) -> Result<Vec<String>, ScanError> {
    let entries = fs::read_dir(root).map_err(|source| ScanError::Io {
        path: root.to_path_buf(),
        source,
    })?;
    remove_corpses(entries, remove_file).map_err(|source| ScanError::Io {
        path: root.to_path_buf(),
        source,
    })
}

/// The loop behind [`clear_temp_corpses`]: every generated-looking temporary in the listing, removed or
/// reported. `remove` is [`remove_file`] in production and a failing stub in the test that pins the
/// tolerance rule, because an undeletable regular file needs filesystem privileges this suite does not
/// have. A failure to *list* stays an error — that is the workspace being unreadable, which no later scan
/// can fix either.
pub(super) fn remove_corpses(
    entries: fs::ReadDir,
    remove: fn(&Path) -> io::Result<()>,
) -> Result<Vec<String>, io::Error> {
    let mut skipped = Vec::new();
    for entry in entries {
        let entry = entry?;
        let name = entry.file_name().to_string_lossy().into_owned();
        let Some(stem) = name.strip_suffix(TEMP_SUFFIX) else {
            continue;
        };
        let is_dir = entry.file_type().is_ok_and(|kind| kind.is_dir());
        if !index_names::is_generated_index(stem) || is_dir {
            continue;
        }
        if let Err(source) = remove(&entry.path()) {
            // Already gone is the state this call wanted; anything else is reported and retried.
            if source.kind() != io::ErrorKind::NotFound {
                skipped.push(name);
            }
        }
    }
    Ok(skipped)
}

/// [`fs::remove_file`] with its parameter fixed to `&Path`, so [`remove_corpses`] can accept it as a
/// function pointer beside a stub that always fails.
pub(super) fn remove_file(path: &Path) -> io::Result<()> {
    fs::remove_file(path)
}
