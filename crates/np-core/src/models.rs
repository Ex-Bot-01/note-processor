//! Installing a pinned model artifact and reporting what is on disk (`docs/design-m2-models-install.md`).
//!
//! This is the whole of round 4a's behaviour, as core functions rather than as CLI code: `install` puts a
//! file the human already holds under the workspace's artifact directory, verifies it against a digest the
//! human declared, and records the row; `list` says what each row's artifact looks like. Both take `&Db` and
//! `&Config` so np-cli stays a parser of text into [`Install`] plus a printer of [`InstallOutcome`] and
//! [`Listing`] — the reason §6 names reads and writes in `db.rs` instead of letting a command hold SQL.
//!
//! Nothing here contacts anything: no HTTP client, no URL, no key, no model execution (§5). The one digest
//! implementation is [`crate::hash::sha256_file`] — this module defines no second SHA-256, and §7's test 9
//! exists to keep it that way.
//!
//! Two properties the rest of the crate cares about, both structural rather than incidental:
//!
//! - **The row and the artifact are one workspace's.** Both come from [`Config::workspace_db`] and
//!   [`Config::workspace_models_dir`], which share one private name derivation; a second derivation anywhere
//!   else would separate them by another door (§3 step 0).
//! - **No half-states.** The artifact is written through a temporary name and renamed, the row is inserted
//!   only after the rename, and every outcome is a variant of [`InstallOutcome`] or [`ArtifactState`]. What
//!   cannot be expressed — "the directory could not be listed", which is *not* a per-row verdict — has no
//!   representation in [`Listing`] and so leaves as [`ModelError::ModelsDir`].

use std::collections::HashSet;
use std::ffi::OsString;
use std::fs;
use std::io;
use std::num::NonZeroU16;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU32, Ordering};
use std::time::{SystemTime, UNIX_EPOCH};

use thiserror::Error;

use crate::config::Config;
use crate::db::{self, Db, DbError, ModelRow};
use crate::hash;
use crate::walk;

/// Longest accepted model id, in characters — the `{0,63}` after the mandatory first character of §2's
/// `[a-z0-9][a-z0-9._@-]{0,63}`.
const ID_MAX_CHARS: usize = 64;

/// Length of a sha256 spelled as hex, which is what `--sha256` has to be (§3 step 4) and what
/// [`crate::hash::sha256_file`] returns.
const DIGEST_HEX_CHARS: usize = 64;

/// Suffix of the temporary artifact name, before its `rename` — the same publish discipline as
/// `crate::scan`'s catalog writes ("no reader ever sees half an artifact"), kept separate because a
/// `<generated>.INDEX.md.tmp` corpse and a `<id>.<nonce>.tmp` one are cleaned up by different rules: the
/// scan clears only names [`crate::index_names`] recognises, while a model leftover is *reported* under
/// `list`'s unclaimed heading (§3) rather than deleted, because nothing here deletes anything a row may
/// still be naming.
const TEMP_SUFFIX: &str = ".tmp";

/// Names Windows refuses as file names in any extension (`CON`, `PRN`, `AUX`, `NUL`, `COM1`-`COM9`,
/// `LPT1`-`LPT9`). §2's rule, because an id is a file name: such a row is one whose artifact cannot exist on
/// the platform that shares this program, and installing it would be a promise `list` can never keep.
const WINDOWS_DEVICE_NAMES: [&str; 22] = [
    "con", "prn", "aux", "nul", "com1", "com2", "com3", "com4", "com5", "com6", "com7", "com8",
    "com9", "lpt1", "lpt2", "lpt3", "lpt4", "lpt5", "lpt6", "lpt7", "lpt8", "lpt9",
];

/// Everything `install` was asked for: the six values of a `models` row, plus the file to install from.
///
/// Owned values, because they arrive from argument parsing and outlive the call — the row is what gets
/// recorded, so there is no borrow to shorten here.
#[derive(Debug, Clone)]
pub struct Install {
    /// Model identity, i.e. `models.id` and the artifact's file name. Refused before any file access by
    /// [`validate_id`] (§3 step 1), because a bad id is not a reason to read half a gigabyte.
    pub id: String,
    /// The artifact the human already has. It is **read** and copied; never moved, truncated or deleted, so
    /// a failed install leaves the source where it was.
    pub source: PathBuf,
    /// Declared vector width — declared by the human, never probed (§2).
    ///
    /// `NonZeroU16` is §2's bound expressed as a type rather than as a runtime check this module could skip:
    /// `u16` because [`crate::config::EmbeddingsConfig`] already measures that quantity in `u16`, non-zero
    /// because migration 7's `CHECK (length(vector) = 4 * dim)` admits a zero-byte vector at `dim = 0`, so a
    /// dim-0 row would accept a vector of nothing for every chunk. The command parsing text still reports
    /// the ceiling (`65535`) in its own message; what the type guarantees is that no caller reaches the write
    /// with a width outside it.
    pub dim: NonZeroU16,
    /// The digest this artifact must hash to, as the human spelled it — any case. [`install`] validates it
    /// and lowers it **once** (§3 step 4); everything downstream uses that lowered spelling.
    pub sha256: String,
    /// Prefix added to documents, `''` when the command names none (migration 7's default).
    pub prefix_doc: String,
    /// Prefix added to queries.
    pub prefix_query: String,
}

/// What one `install` call ended as. An enum because these are three different facts about the workspace,
/// and a caller must be able to match on them rather than parse a sentence (§3).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum InstallOutcome {
    /// The artifact was copied and the row inserted — this call is what put the model on the table.
    Installed,
    /// A row with the **whole declared tuple** was already there and its artifact is in place: nothing was
    /// written, including `installed_at`.
    AlreadyInstalled,
    /// The row was there, the tuple agreed, and the artifact was gone; the file was re-materialised from a
    /// source whose digest the row already attests. No column was rewritten — this is re-materialising what
    /// the row attests, not repairing it (§3 step 6).
    Restored,
}

impl InstallOutcome {
    /// The string §3 pins for `--json`'s `"state"` field: `installed`, `already installed`, `restored`.
    ///
    /// Named here rather than left to the printer because §3 pins these spellings *now*, and a value that
    /// only exists in one command's format string can be renamed without anything failing.
    pub fn state_label(self) -> &'static str {
        match self {
            Self::Installed => "installed",
            Self::AlreadyInstalled => "already installed",
            Self::Restored => "restored",
        }
    }
}

/// The state of one row's artifact on disk — **exactly four values**, which is what §3's table fixes.
///
/// A fifth state would be the same mistake as spelling these as strings: `list` would gain a verdict that no
/// rule defines, and the human reading it could not tell "nothing expected" from "something lost". The case
/// that genuinely has no per-row answer — the directory cannot be listed at all — is therefore *not* in this
/// enum; see [`ModelError::ModelsDir`].
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ArtifactState {
    /// The row's digest matches the artifact on disk.
    Present,
    /// The row expects an artifact and none is there — named, never repaired.
    Missing,
    /// `sha256 IS NULL`: a provider with no local weights. Calling this `missing` is a false alarm, and
    /// round 4b creates exactly these rows (§3).
    NoArtifactExpected,
    /// An artifact is there and does not hash to its row — reported, never overwritten. A non-file (a
    /// directory named after the id) lands here too: it can never hash to anything.
    Mismatch,
}

impl ArtifactState {
    /// The string §3 pins for `--json`'s per-row `"state"` field.
    pub fn state_label(self) -> &'static str {
        match self {
            Self::Present => "present",
            Self::Missing => "missing",
            Self::NoArtifactExpected => "no artifact expected",
            Self::Mismatch => "mismatch",
        }
    }
}

/// One `models` row beside the verdict on the artifact it names.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ListedModel {
    /// What the database says.
    pub row: ModelRow,
    /// What the artifact directory says about it.
    pub state: ArtifactState,
}

/// Something under the models directory that no row names, reported under its own heading (§3).
///
/// Reachable **by design**: [`install`] renames before it inserts the row, so a process killed between those
/// two steps leaves an artifact with no row — and step 6's temporary name is a second way in. `list` reports
/// such entries instead of claiming them or deleting them.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Unclaimed {
    /// Lossy display spelling, the way [`crate::walk::UnknownPath::display`] renders a name whose bytes UTF-8
    /// cannot hold. There is deliberately no second (byte) spelling beside it: [`UnknownPath`](crate::walk::UnknownPath)
    /// carries one because a scan *matches* report lines against byte-keyed rows, and F20 §1 shows what
    /// matching on the lossy half does. Nothing matches an unclaimed entry — it is printed and left alone —
    /// so a bytes field here would be a second spelling with no reader, which is the thing this crate refuses
    /// elsewhere rather than a simplification.
    pub name: String,
    /// A subdirectory is **reported as one and not recursed** (§3): whatever is under it belongs to nobody
    /// this command knows about, and walking a user's arbitrary tree inside the data dir is not this
    /// listing's job.
    pub kind: UnclaimedKind,
}

/// Whether an [`Unclaimed`] entry is a directory or some other single name.
///
/// Two kinds rather than three: `read_dir`'s own file type says *directory* or it does not, and everything
/// that is not one — a file, a symlink, a socket — is reported as a file-shaped leftover. What matters to the
/// reader is that nothing below an entry was entered, and that holds for both arms.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum UnclaimedKind {
    /// A plain (or otherwise non-directory) entry.
    File,
    /// A directory, named and left alone.
    Directory,
}

/// What `np models list <workspace>` found.
///
/// Note what is **not** here: no field for "the artifact directory could not be read". That failure cannot be
/// a per-row verdict — reporting `missing` against every row would tell the human that every model lost its
/// weights when in fact nothing was looked at — so [`list`] returns [`ModelError::ModelsDir`] instead of a
/// `Listing`, and the command that prints one prints no rows.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Listing {
    /// The directory that was listed, so a report can name where it looked.
    pub models_dir: PathBuf,
    /// One entry per row, ordered by id ([`db::model_rows`] decides the order so output is deterministic).
    pub models: Vec<ListedModel>,
    /// Entries no row names, sorted by display name (§3). These never change an exit code.
    pub unclaimed: Vec<Unclaimed>,
}

/// What can go wrong installing or listing models. Every variant states one fact; none of them is a status
/// the caller has to re-derive from text.
#[derive(Debug, Error)]
pub enum ModelError {
    /// The workspace operand is not a directory that exists (§3 step 0). Refusing it is the whole point:
    /// [`Config::workspace_db`] is infallible by contract and derives a name from a typo as happily as from
    /// a real path, so accepting one would mint an empty database and an artifact directory no scan will
    /// ever read — a wrong status claim, which this project treats as a defect.
    #[error("workspace {} is not a directory that exists ({source}); refusing before anything is read or written", .workspace.display())]
    Workspace {
        /// The operand as the caller spelled it.
        workspace: PathBuf,
        /// Why not: no such path, not a directory, or not readable.
        source: io::Error,
    },
    /// `--id` broke one of §2's rules. `rule` names **which** one, because "invalid" alone leaves the human
    /// guessing whether the length, the charset, the trailing dot or a reserved word was the problem.
    #[error("refusing model id `{id}`: {rule}")]
    InvalidId {
        /// The rejected spelling.
        id: String,
        /// The rule it broke.
        rule: &'static str,
    },
    /// `--sha256` is not exactly `DIGEST_HEX_CHARS` (private) hex characters (§3 step 4). Refused rather than
    /// treated as "does not match": a truncated paste and a different artifact are different problems, and
    /// only one of them is fixed by re-running `np models hash`.
    #[error("`--sha256` must be exactly {DIGEST_HEX_CHARS} hexadecimal characters; this argument holds {chars}")]
    InvalidDigest {
        /// How many characters arrived. The value itself is not echoed: a mistyped digest is 64 characters
        /// of noise in a message that has to stay readable, and the caller already knows what they typed.
        chars: usize,
    },
    /// The source file is empty (§3 step 2). This refusal happens **before** hashing so that a download
    /// truncated to nothing reads as "the file is empty" rather than as a digest mismatch against a real
    /// artifact — the two diagnoses call for opposite actions.
    #[error("refusing a zero-length artifact: {} hashes to the digest of nothing, which pins no model", .path.display())]
    EmptySource {
        /// The file that turned out to be empty.
        path: PathBuf,
    },
    /// The source does not hash to the declared digest (§3 step 4). **Both digests are named**: the file's
    /// digest is public information about a file the human already holds, and leaving it out is what makes a
    /// mismatch undiagnosable.
    /// The **installed artifact** no longer hashes to its own row (§3 step 6's verification). Distinct from
    /// [`ModelError::DigestMismatch`], which is about the file named by `--path`: saying "nothing was installed"
    /// here would be false — something is installed, and it is the thing that disagrees.
    #[error(
        "the installed artifact {} hashes to {found}, but its row says {expected}; nothing was written, because an artifact that disagrees with its row is reported, not overwritten",
        .path.display()
    )]
    ArtifactDisagrees {
        /// The artifact whose bytes no longer match the row that names it.
        path: PathBuf,
        /// What the row claims.
        expected: String,
        /// What the file holds.
        found: String,
    },
    #[error("{} hashes to {found}, not the declared {declared}; nothing was installed", .path.display())]
    DigestMismatch {
        /// The source that was hashed.
        path: PathBuf,
        /// The lowered declared digest.
        declared: String,
        /// The file's actual digest.
        found: String,
    },
    /// A row already names this id and disagrees with the declaration somewhere (§3 step 5). `column` is the
    /// schema's own name for the first disagreement, and both values are printed side by side because "you
    /// are re-installing a model under a different meaning" is only actionable if it says which meaning
    /// changed. No `--force` exists to override this: replacing a model identity invalidates every vector
    /// stored against it (F13, F19, F23).
    #[error("model `{id}` is already installed with {column} = `{stored}`; this command declares `{declared}`. Refusing to change what an installed model means: nothing was written")]
    AlreadyInstalled {
        /// The identity both commands name.
        id: String,
        /// Which column disagrees — `dim`, `sha256`, `prefix_doc` or `prefix_query`.
        column: &'static str,
        /// What the row holds (`NULL` spelled as such).
        stored: String,
        /// What this call declared.
        declared: String,
    },
    /// The artifact directory could not be created or listed. Named at the **workspace** level on purpose:
    /// it is a fact about one directory, never a verdict on a row (§3), which is why [`Listing`] has no field
    /// able to hold it.
    #[error("cannot use the model artifact directory {}: {source}", .path.display())]
    ModelsDir {
        /// The directory that refused to be created or read.
        path: PathBuf,
        /// Why the directory failed.
        source: io::Error,
    },
    /// An artifact is there but could not be hashed while listing, so no state can be reported for it.
    ///
    /// Not folded into [`ArtifactState::Mismatch`], which claims something was compared and disagreed; here
    /// nothing was read. Nor swallowed: a `list` that quietly skipped the unreadable artifact would report a
    /// workspace as healthy from the rows it managed to check.
    #[error("cannot read the artifact {} to compare it with its row: {source}", .path.display())]
    Artifact {
        /// The artifact this call could not hash.
        path: PathBuf,
        /// Why not.
        source: io::Error,
    },
    /// A copy or rename into the artifact directory failed; `path` is the name that could not be written.
    /// Reading **the human's** file failed — missing, a directory, unreadable (§3 steps 2-3). Distinct from
    /// [`ModelError::Io`], which is about writing our own temp or target: naming the input file "the model
    /// artifact" and calling a read a write puts the wrong fact in the message.
    #[error("cannot read the install source {}: {source}", .path.display())]
    Source {
        /// The `--path` that could not be read.
        path: PathBuf,
        /// Why not.
        source: io::Error,
    },
    #[error("cannot write the model artifact {}: {source}", .path.display())]
    Io {
        /// The path that failed, temp or final.
        path: PathBuf,
        /// Why.
        source: io::Error,
    },
    /// `insert_model` wrote nothing **and** no row naming the id is readable afterwards (§6's concurrent
    /// install arm).
    ///
    /// Recorded because it is what a `false` from [`db::insert_model`] means when the winner has gone away:
    /// the artifact is on disk with no row attesting it. SQLite's write lock makes this ordering practically
    /// unreachable — a second `INSERT` cannot run until the first commits or rolls back, and a rollback
    /// removes the conflict the `DO NOTHING` arm answered — which is why this variant names an outcome to
    /// report rather than a repair to attempt. Nothing here deletes rows to fix it.
    #[error("model `{id}` was not recorded: its artifact is installed but the insert wrote nothing and no row names it; re-run this command")]
    NotRecorded {
        /// The identity that has an artifact and no row.
        id: String,
    },
    /// The workspace database refused one of [`crate::db`]'s model reads or the insert.
    ///
    /// A passthrough in the same spirit as [`DbError::Sqlite`]: SQLite's own text already names the file and
    /// the constraint, so re-wrapping it would only add a prefix. This is also the arm a corrupt or
    /// hand-edited `models` column reaches — a `dim` holding text has nowhere honest to go otherwise.
    #[error("model database: {0}")]
    Db(#[from] DbError),
}

/// Install `request` into `workspace`, whose database is the already-open `db`.
///
/// `db` must be the database of **this** workspace — `Db::open(&config.workspace_db(workspace))` — because
/// this function derives the artifact directory from the same operand through
/// [`Config::workspace_models_dir`] and the two paths are only guaranteed to belong together when the
/// workspace spelling is (§3 step 0's point). A command that opens the database itself has to call
/// [`require_workspace_dir`] *first*: this check cannot un-mint a file it was handed, and `Db::open` creates
/// directories.
///
/// The order is §3's, in full, and each step refuses rather than repairing:
///
/// 0. [`require_workspace_dir`] — the workspace must be a directory that exists.
/// 1. [`validate_id`] — before any file access, naming the rule.
/// 2. an empty source is refused before hashing.
/// 3. **one** [`crate::hash::sha256_file`] call over the source.
/// 4. `--sha256` validated as hex and lowered once; a mismatch names both digests.
/// 5. the existing row, if any, compared on the **whole** declared tuple.
/// 6. copy to `<id>.<nonce>.tmp`, rename to `<id>` — every path through [`walk::to_open_path`].
/// 7. [`db::insert_model`], after the rename and before any echo (F21: durable record first).
///
/// What cannot be atomic is step 6 versus step 7, and that gap is designed for: a file with no row is what a
/// crash leaves, step 5 tolerates it, and step 6 re-materialises the row's artifact around it. The reverse
/// (a row naming nothing) is `list`'s `missing`, also reported rather than repaired.
pub fn install(
    db: &Db,
    config: &Config,
    workspace: &Path,
    request: &Install,
    now: i64,
) -> Result<InstallOutcome, ModelError> {
    require_workspace_dir(workspace)?;
    validate_id(&request.id)?;

    // Step 4's **format** check runs first, deliberately ahead of steps 2 and 3: a mistyped or truncated digest
    // is a fact about argv, and making the human watch up to ~540 MB get read before "must be exactly 64
    // hexadecimal characters" spends the expensive operation to learn something the command line already said.
    // Lowering still happens exactly once (§3), just earlier.
    let declared = normalize_digest(&request.sha256)?;

    // Step 2, now that the arguments are known good: an empty file is a truncated download, and the digest of
    // nothing is a real digest that would only report "mismatch" against whatever the human expected.
    let open_source = walk::to_open_path(&access(&request.source));
    let meta = fs::metadata(&open_source).map_err(|source| ModelError::Source {
        path: request.source.clone(),
        source,
    })?;
    // A directory is not a truncated download, and the two platforms disagree about what reading one even
    // means — `Metadata::len()` reports 0 for a directory on Windows, so a length-only test would answer "a
    // zero-length artifact" there while Linux reached for a hash and failed at open. Name the kind instead:
    // both spellings of "that is not a file" arrive as the same read refusal on every OS.
    if !meta.is_file() {
        return Err(ModelError::Source {
            path: request.source.clone(),
            source: io::Error::new(io::ErrorKind::NotADirectory, "not a regular file"),
        });
    }
    if meta.len() == 0 {
        return Err(ModelError::EmptySource {
            path: request.source.clone(),
        });
    }

    // Step 3, and the only digest computed over the source by this call. Step 6 does not re-hash what it copied:
    // the row's sha equals `--sha256` (step 5) and `--sha256` equals the file's, so a second pass would read
    // ~540 MB to confirm arithmetic (§3).
    let found = hash::sha256_file(&open_source).map_err(|err| ModelError::Source {
        path: request.source.clone(),
        source: hash_io(err),
    })?;
    if declared != found {
        return Err(ModelError::DigestMismatch {
            path: request.source.clone(),
            declared,
            found,
        });
    }

    // Step 5.
    if let Some(row) = db::model_row(db, &request.id)? {
        compare_declaration(&row, request, &declared)?;
        let models_dir = config.workspace_models_dir(workspace);
        if artifact_present(&models_dir, &request.id) {
            // Verify before claiming it. Reusing the source digest here would be sound arithmetic and a false
            // status: `declared` describes the file named by `--path`, and says nothing about the bytes of the
            // artifact that has been sitting on disk since the install being re-run — which is exactly the
            // rot this command's whole trust model exists to catch. So this arm, and only this arm, hashes the
            // artifact (§3: one hash of the source, plus this verification when it is what changes the answer).
            let target = models_dir.join(&request.id);
            let on_disk = hash::sha256_file(&access(&target)).map_err(|err| ModelError::Io {
                path: target.clone(),
                source: hash_io(err),
            })?;
            if on_disk != declared {
                // Refusal, never overwrite: the artifact disagrees with its row, and replacing it from a
                // source that happens to agree with the row would destroy the only evidence of that.
                return Err(ModelError::ArtifactDisagrees {
                    path: target,
                    expected: declared,
                    found: on_disk,
                });
            }
            return Ok(InstallOutcome::AlreadyInstalled);
        }
        // The row attests this digest and the file is gone: re-materialise it, rewriting no column.
        publish_artifact(&models_dir, &request.id, &request.source)?;
        return Ok(InstallOutcome::Restored);
    }

    // Steps 6 and 7, in that order: durable artifact, then the row that names it, then the caller's echo.
    let models_dir = config.workspace_models_dir(workspace);
    publish_artifact(&models_dir, &request.id, &request.source)?;
    let inserted = db::insert_model(
        db,
        &request.id,
        i64::from(request.dim.get()),
        Some(&declared),
        &request.prefix_doc,
        &request.prefix_query,
        now,
    )?;
    if inserted {
        return Ok(InstallOutcome::Installed);
    }

    // A second process inserted this id between step 5 and here. Its row is the authority now, so apply the
    // same full-tuple test rather than reporting a conflict-free success or a bare constraint violation (§6).
    match db::model_row(db, &request.id)? {
        Some(row) => {
            compare_declaration(&row, request, &declared)?;
            Ok(InstallOutcome::AlreadyInstalled)
        }
        None => Err(ModelError::NotRecorded {
            id: request.id.clone(),
        }),
    }
}

/// Report every installed model of `workspace` and what its artifact looks like.
///
/// Reads only: nothing here repairs, re-hashes into place, or deletes — including the unclaimed entries,
/// which are leftovers this command cannot prove are garbage (§3).
///
/// An artifact directory that cannot be **listed** leaves as [`ModelError::ModelsDir`] with no [`Listing`] at
/// all, so a per-row `missing` verdict cannot be produced for rows nobody looked at. A directory that is
/// simply **absent** is not that failure: it is a workspace nothing has been installed into, and its rows —
/// round 4b's `sha256 IS NULL` providers, or a hand-deleted directory — answer `NoArtifactExpected` and
/// `Missing` honestly.
///
/// The directory is therefore looked at **first**, before any row is hashed: it decides whether a per-row
/// verdict is expressible at all, and reading it later would spend a whole artifact pass on a listing that
/// then has to be thrown away.
pub fn list(db: &Db, config: &Config, workspace: &Path) -> Result<Listing, ModelError> {
    require_workspace_dir(workspace)?;
    let rows = db::model_rows(db)?;
    let models_dir = config.workspace_models_dir(workspace);

    let entries = match read_dir_entries(&models_dir) {
        Ok(entries) => entries,
        // No directory yet is the state of a workspace nothing was installed into, not an unreadable one.
        Err(source) if source.kind() == io::ErrorKind::NotFound => EntryList::default(),
        Err(source) => {
            return Err(ModelError::ModelsDir {
                path: models_dir,
                source,
            })
        }
    };

    // Claimed means "some row names this exact file name". Row ids are validated ASCII (§2), so comparing
    // the OS spelling against them needs no lossy round trip — a name UTF-8 cannot hold is by construction
    // not an id and lands in `unclaimed`, rendered with replacements.
    let claimed: HashSet<OsString> = rows.iter().map(|row| OsString::from(&row.id)).collect();
    let unclaimed = entries.unclaimed(&claimed);

    let mut models = Vec::with_capacity(rows.len());
    for row in rows {
        let state = artifact_state(&models_dir, &row)?;
        models.push(ListedModel { row, state });
    }

    Ok(Listing {
        models_dir,
        models,
        unclaimed,
    })
}

/// Refuse a `workspace` operand that is not a directory that exists (§3 step 0).
///
/// Public because the check has to happen **before** [`Config::workspace_db`] is opened, not merely inside
/// [`install`] and [`list`]: deriving a database path is infallible by contract (`config.rs`'s own doc says
/// so), so by the time a command hands a `Db` to this module an empty database for the typo'd name already
/// exists on disk. The order a command keeps is therefore: check the workspace, open the database, call
/// [`install`] or [`list`] — which check again rather than trusting their caller.
///
/// A symlink to a directory passes, because [`fs::metadata`] follows it and the workspace this program
/// indexes may well be one.
pub fn require_workspace_dir(workspace: &Path) -> Result<(), ModelError> {
    let refuse = |source: io::Error| ModelError::Workspace {
        workspace: workspace.to_path_buf(),
        source,
    };
    match fs::metadata(walk::to_open_path(&access(workspace))) {
        Ok(meta) if meta.is_dir() => Ok(()),
        // `is_file` would name one of several non-directories; the kind says what every other reader here
        // says and keeps the message a fact rather than a guess.
        Ok(_) => Err(refuse(io::Error::new(
            io::ErrorKind::NotADirectory,
            "not a directory",
        ))),
        Err(source) => Err(refuse(source)),
    }
}

/// §2's id rule: `[a-z0-9][a-z0-9._@-]{0,63}`, not ending in `.`, and not a Windows reserved device name.
///
/// Public so the command that parses human input can refuse before it opens anything, and because the rule
/// is one place by construction: the id becomes a file name inside the artifact directory, so what `install`
/// accepts and what `list` can find again are the same set. The charset is not cosmetic — uppercase is
/// already refused there, which is why no case-collision rule is needed: `models.id` compares BINARY while
/// NTFS names do not, and the charset is what keeps those two notions from disagreeing.
///
/// The error names the rule that failed, in the order above.
pub fn validate_id(id: &str) -> Result<(), ModelError> {
    let refuse = |rule: &'static str| {
        Err(ModelError::InvalidId {
            id: id.to_string(),
            rule,
        })
    };
    if id.is_empty() {
        return refuse("it is empty");
    }
    if id.chars().count() > ID_MAX_CHARS {
        return refuse("it is longer than 64 characters");
    }
    if let Some(device) = windows_device_name(id) {
        // Ahead of the charset rules on purpose. `NUL.txt` fails both, and the reader who typed it cannot do
        // anything with "use lowercase" — the file name is impossible on Windows whatever the case is, because
        // Windows folds it and refuses the device name either way (§2).
        return Err(ModelError::InvalidId {
            // Naming the reserved word itself, not just the id: in `nul.md` it is `nul` that Windows refuses,
            // and a reader who knows which half is the problem can rename the other half.
            id: format!("{id} (`{device}` is reserved on Windows)"),
            rule: "a file name cannot be a Windows reserved device name, with or without a suffix",
        });
    }
    let mut chars = id.chars();
    // The first character is the one that cannot be punctuation: `-foo` reads as an option and `.foo` as a
    // hidden file, neither of which `list` should ever have to explain.
    if !chars
        .next()
        .is_some_and(|c| c.is_ascii_lowercase() || c.is_ascii_digit())
    {
        return refuse("its first character must be a lowercase letter or a digit");
    }
    if !id[1..]
        .chars()
        .all(|c| c.is_ascii_lowercase() || c.is_ascii_digit() || matches!(c, '.' | '_' | '@' | '-'))
    {
        return refuse(
            "only a-z, 0-9 and `.`, `_`, `@`, `-` are allowed after the first character",
        );
    }
    if id.ends_with('.') {
        // POSIX keeps a trailing dot and NTFS strips it, so `foo` and `foo.` are two BINARY-distinct rows
        // over one artifact file on half the platforms this program runs on.
        return refuse("it ends in `.`, which Windows strips: `foo` and `foo.` would be two rows over one artifact");
    }
    Ok(())
}

/// The reserved device name `id` would collide with, if any: the part before the first `.`, folded
/// case-insensitively the way Windows folds it.
fn windows_device_name(id: &str) -> Option<&'static str> {
    let stem = id.split('.').next().unwrap_or(id).to_ascii_lowercase();
    WINDOWS_DEVICE_NAMES
        .iter()
        .find(|device| **device == stem.as_str())
        .copied()
}

/// Validate a declared digest as exactly `DIGEST_HEX_CHARS` (private) hex characters and return it lowered (§3 step 4).
fn normalize_digest(declared: &str) -> Result<String, ModelError> {
    // Characters on both sides. `len()` is bytes, so a pasted argument holding one non-ASCII character (a stray
    // non-breaking space from a web page is the ordinary cause) was refused with "this argument holds 64" — a
    // message contradicting itself exactly when it was most needed.
    if declared.chars().count() != DIGEST_HEX_CHARS
        || !declared.bytes().all(|byte| byte.is_ascii_hexdigit())
    {
        return Err(ModelError::InvalidDigest {
            chars: declared.chars().count(),
        });
    }
    Ok(declared.to_ascii_lowercase())
}

/// Refuse unless `row` and the declaration agree on all four columns §3 step 5 lists: `dim`, `sha256`,
/// `prefix_doc`, `prefix_query`.
///
/// All four, not just the digest: a sha-only comparison would answer "already installed" while quietly
/// changing `models.dim`, which is the `expected` of every later
/// [`crate::embed::EmbedError::DimensionMismatch`] — the number a human is told to trust. Reported values are
/// the stored and declared spellings side by side, in column order, so the first disagreement is the one
/// named.
fn compare_declaration(
    row: &ModelRow,
    request: &Install,
    declared_sha: &str,
) -> Result<(), ModelError> {
    let conflict =
        |column: &'static str, stored: String, declared: String| ModelError::AlreadyInstalled {
            id: request.id.clone(),
            column,
            stored,
            declared,
        };
    if i64::from(request.dim.get()) != row.dim {
        return Err(conflict(
            "dim",
            row.dim.to_string(),
            request.dim.get().to_string(),
        ));
    }
    let stored_sha = row.sha256.clone().unwrap_or_else(|| "NULL".to_string());
    if stored_sha != declared_sha {
        return Err(conflict("sha256", stored_sha, declared_sha.to_string()));
    }
    if row.prefix_doc != request.prefix_doc {
        return Err(conflict(
            "prefix_doc",
            row.prefix_doc.clone(),
            request.prefix_doc.clone(),
        ));
    }
    if row.prefix_query != request.prefix_query {
        return Err(conflict(
            "prefix_query",
            row.prefix_query.clone(),
            request.prefix_query.clone(),
        ));
    }
    Ok(())
}

/// Copy `source` to `<models_dir>/<id>` through a temporary name (§3 step 6).
///
/// Three paths, all spelled through [`walk::to_open_path`] — the directory creation, the temp write and the
/// rename — because a long temp name can cross the Windows prefix threshold while the final name does not,
/// and mixing verbatim and `\\?\` spellings inside one operation is worse than either.
///
/// The temp name is `<id>.<nonce>.tmp`, pinned by §3 rather than left to taste: `list`'s unclaimed rule is
/// stated in terms of it, and a failed copy leaves that spelling behind on purpose — removing it would be
/// this module deleting a file no row has ever attested, which is not its authority.
/// The spelling to hand to the OS: absolute and lexically normalised, **no symlink resolution**.
///
/// `walk::to_open_path` documents that it expects an absolute path (a prefixed relative path is not valid on
/// Windows) and adds its prefix at ~260 characters, so a long relative `--path` from argv would become
/// `\\?\relative\...` and fail with a raw OS error. Identity is a separate question: the workspace key still
/// comes from `Config::workspace_db`, which hashes the caller's spelling on purpose, and normalising *that*
/// would silently move a database. Only filesystem access goes through here.
pub fn access(path: &Path) -> PathBuf {
    std::path::absolute(path).unwrap_or_else(|_| path.to_path_buf())
}

fn publish_artifact(models_dir: &Path, id: &str, source: &Path) -> Result<(), ModelError> {
    fs::create_dir_all(walk::to_open_path(&access(models_dir))).map_err(|source| {
        ModelError::ModelsDir {
            path: models_dir.to_path_buf(),
            source,
        }
    })?;

    let temp = models_dir.join(format!("{id}.{}{TEMP_SUFFIX}", nonce()));
    let target = models_dir.join(id);
    fs::copy(
        walk::to_open_path(&access(source)),
        walk::to_open_path(&temp),
    )
    .map_err(|source| ModelError::Io {
        path: temp.clone(),
        source,
    })?;
    // Atomic within one directory: a reader either sees the previous artifact or this one, never half.
    fs::rename(walk::to_open_path(&temp), walk::to_open_path(&target)).map_err(|source| {
        ModelError::Io {
            path: target,
            source,
        }
    })?;
    Ok(())
}

/// Installs inside one process need distinct temp names too, so two threads cannot write one name.
static TEMP_COUNTER: AtomicU32 = AtomicU32::new(0);

/// The `<nonce>` half of the temporary artifact name: this process's id, the current time, and a counter —
/// digits and `-` only, so a temp name can never be mistaken for a valid id by [`validate_id`] if someone
/// later feeds it one.
///
/// Process id **and** clock **and** counter because each half alone fails one way: two `np models install`
/// processes started in the same nanosecond share a timestamp, and two calls in one process can share a
/// coarse clock. A clock that cannot be read contributes zeroes, which leaves the pid and the counter — enough
/// for the one guarantee this name needs, which is that it does not collide with itself.
fn nonce() -> String {
    let (secs, subsecs) = match SystemTime::now().duration_since(UNIX_EPOCH) {
        Ok(since) => (since.as_secs(), since.subsec_nanos()),
        Err(_) => (0, 0),
    };
    let counter = TEMP_COUNTER.fetch_add(1, Ordering::Relaxed);
    format!("{}-{}{}-{}", std::process::id(), secs, subsecs, counter)
}

/// Whether `<models_dir>/<id>` is there at all — step 5's missing-file check, which decides
/// [`InstallOutcome::AlreadyInstalled`] against [`InstallOutcome::Restored`].
///
/// "There" means *some name exists*: a directory counts, and install then reports `already installed` rather
/// than writing. The bytes behind it are a separate question: `install` verifies this one artifact when it is
/// already there (§3), and [`artifact_state`] answers it for every row during `list`.
fn artifact_present(models_dir: &Path, id: &str) -> bool {
    fs::metadata(walk::to_open_path(&access(&models_dir.join(id)))).is_ok()
}

/// One row's [`ArtifactState`], from its digest and what the directory holds (§3's table).
fn artifact_state(models_dir: &Path, row: &ModelRow) -> Result<ArtifactState, ModelError> {
    // A provider with no local weights expects no file. Reaching for `missing` here would be a false alarm
    // on every row round 4b creates.
    let Some(expected) = row.sha256.as_deref() else {
        return Ok(ArtifactState::NoArtifactExpected);
    };
    let target = models_dir.join(&row.id);
    match fs::metadata(walk::to_open_path(&access(&target))) {
        Err(source) if source.kind() == io::ErrorKind::NotFound => Ok(ArtifactState::Missing),
        // Anything else — a permission refusal, an unreadable parent — says nothing about whether the
        // artifact is there, so it cannot become a verdict on the row.
        Err(source) => Err(ModelError::Artifact {
            path: target,
            source,
        }),
        Ok(meta) if !meta.is_file() => Ok(ArtifactState::Mismatch),
        Ok(_) => {
            let found =
                hash::sha256_file(&access(&target)).map_err(|err| ModelError::Artifact {
                    path: target.clone(),
                    source: hash_io(err),
                })?;
            Ok(if found == expected {
                ArtifactState::Present
            } else {
                ArtifactState::Mismatch
            })
        }
    }
}

/// The entries of one `read_dir`, kept as the two facts `list` reports: name and directory-or-not.
///
/// A separate type because [`fs::ReadDir`] is a lazy iterator that cannot be re-walked, while [`list`] needs
/// it twice — once to decide what nothing claims, and once for nothing else, since rows are answered from the
/// database rather than from the directory.
#[derive(Debug, Default)]
struct EntryList(Vec<(OsString, UnclaimedKind)>);

impl EntryList {
    /// Every entry no row names, sorted by its lossy display name so output is deterministic (§3).
    fn unclaimed(mut self, claimed: &HashSet<OsString>) -> Vec<Unclaimed> {
        self.0.retain(|(name, _)| !claimed.contains(name));
        let mut unclaimed: Vec<Unclaimed> = self
            .0
            .into_iter()
            .map(|(name, kind)| Unclaimed {
                name: name.to_string_lossy().into_owned(),
                kind,
            })
            .collect();
        unclaimed.sort_by(|left, right| left.name.cmp(&right.name));
        unclaimed
    }
}

/// Collect one directory's entries. An entry whose own metadata cannot be read is reported as a file: the
/// listing's promise is that it entered nothing below any name, and that holds whichever kind it guesses.
fn read_dir_entries(dir: &Path) -> Result<EntryList, io::Error> {
    let mut entries = Vec::new();
    for entry in fs::read_dir(walk::to_open_path(dir))? {
        let entry = entry?;
        let is_dir = entry.file_type().is_ok_and(|kind| kind.is_dir());
        entries.push((
            entry.file_name(),
            if is_dir {
                UnclaimedKind::Directory
            } else {
                UnclaimedKind::File
            },
        ));
    }
    Ok(EntryList(entries))
}

/// [`hash::HashError`] carries the io failure and the path it failed on; this module reports paths in its own
/// variants, so only the reason travels.
fn hash_io(err: hash::HashError) -> io::Error {
    match err {
        hash::HashError::Read { source, .. } => source,
    }
}
