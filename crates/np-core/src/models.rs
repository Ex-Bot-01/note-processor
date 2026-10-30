//! Installing a pinned model artifact and reporting what is on disk (`docs/design-m2-models-install.md`).
//!
//! This is the whole of round 4a's behaviour, as core functions rather than as CLI code: `install` puts a
//! file the human already holds under the workspace's artifact directory, verifies it against a digest the
//! human declared, and records the row; `list` says what each row's artifact looks like **and any fetch in
//! progress beside the rows**. Both take `&Db` and
//! `&Config` so np-cli stays a parser of text into [`Install`] plus a printer of [`InstallOutcome`] and
//! [`Listing`] — the reason §6 names reads and writes in `db.rs` instead of letting a command hold SQL.
//!
//! Nothing here contacts anything: no HTTP client, no URL, no key, no model execution (§5). The one digest
//! implementation is [`crate::hash::sha256_file`] — this module defines no second SHA-256, and §7's test 9
//! exists to keep it that way. R7a keeps that sentence true: reading a fetch marker ([`fetch_state`]) opens one
//! file in the workspace's own artifact directory, and nothing in this module can name a host or open a socket.
//! The transfer itself belongs to a separate module behind its own feature (`design-m2-local-embeddings`
//! §12 rule 1), which does not exist yet — so **no code path here can create a marker**, and R7a has nothing of
//! *this program's own* to report. A marker any other hand placed is still reported: [`list`] reads names, not
//! authorship, which is what makes the first-start case work before R7b writes its first byte.
//!
//! **`fetching` is not an [`ArtifactState`]** (§12 rule 3). The four arms above are mutually exclusive verdicts
//! on one artifact, while a fetch is orthogonal to all of them: on a first start there is *no row at all*, and a
//! row that is `present` can be re-fetching. So a fetch is reported beside the rows — [`Listing::fetching`],
//! which the listing renders as its own section — and adding a fifth arm would make "the artifact is fine and it
//! is also being replaced" inexpressible rather than merely unusual.
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
use std::ffi::{OsStr, OsString};
use std::fs;
use std::io::{self, Read};
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

/// Leading dot of both fetch names, and the half that makes them unforgeable: [`validate_id`] refuses an id whose
/// first character is not a lowercase letter or a digit (§12 rule 2).
const FETCH_MARKER_PREFIX: &str = ".fetching.";

/// The marker's extension. Spelled `.json` because the file holds §12's four fields as one JSON object, and read
/// back by [`parse_marker`] rather than by a dependency this crate does not have.
const FETCH_MARKER_SUFFIX: &str = ".json";

/// Leading dot plus the word for a partial download: `.part.<id>.<nonce>` (§12 rule 2). R7a recognises this name
/// and reads nothing from it — its only consumer today is `list`'s rule that a fetch is never announced as junk.
const PART_PREFIX: &str = ".part.";

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

/// Which fetch name one directory entry spells, per [`fetch_name`].
///
/// Two arms because two names exist (§12 rule 2), and the id is borrowed from the entry name rather than copied:
/// the caller that wants an owned one says so.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum FetchName<'a> {
    /// `.fetching.<id>.json` — the marker that says a fetch of `id` is running.
    Marker(&'a str),
    /// `.part.<id>.<nonce>` — bytes of `id`'s artifact that are still arriving.
    Part(&'a str),
}

/// §12 rule 2's **single** recogniser: what fetch name (if any) a directory entry spells, byte-exact and
/// case-sensitive.
///
/// Every caller goes through here — `list`'s exclusion from `unclaimed`, and nothing else reads these names — so
/// "the program recognises this name" has one definition. Two properties make the names safe rather than merely
/// conventional:
///
/// - **No model id can forge either name.** Both start with `.`, and [`validate_id`] requires a first character
///   that is a lowercase letter or a digit, so no id can ever *be* one of these names; the artifact path of
///   every row is therefore distinct from every marker and every partial download. (The first draft's
///   `<id>.fetching.json` was forgeable — `alpha.fetching.json` is a legal id. `tests/models.rs` pins both
///   halves of that asymmetry rather than trusting this sentence.)
/// - **The id half is validated, not guessed.** A name whose middle is not a legal id (`f.fetching.json`,
///   `.fetching..json`, `.FETCHING.a.json`, anything holding a path separator) is *not* a fetch name; it is
///   junk, and `list` says so under `unclaimed` like any other unrecognised entry.
///
/// A name that UTF-8 cannot hold returns `None`: such a name cannot be an id either, so it is junk by the same
/// rule rather than by a decoding guess.
pub fn fetch_name(name: &OsStr) -> Option<FetchName<'_>> {
    let text = name.to_str()?;
    // A separator anywhere means this is not one entry name, whatever the caller was handed. `read_dir` never
    // produces one, so this is the fence against a caller feeding a path in where it meant a file name.
    if text.contains('/') || text.contains('\\') {
        return None;
    }
    if let Some(rest) = text.strip_prefix(FETCH_MARKER_PREFIX) {
        let id = rest.strip_suffix(FETCH_MARKER_SUFFIX)?;
        if validate_id(id).is_ok() {
            Some(FetchName::Marker(id))
        } else {
            None
        }
    } else if let Some(rest) = text.strip_prefix(PART_PREFIX) {
        // `<id>.<nonce>`, split at the **last** dot because ids contain dots: `.part.a.b.4242-1-0` is id `a.b`
        // with nonce `4242-1-0`. The nonce itself is not charset-checked — only `fetch_name` and the listing
        // read this name in R7a, and a partial download whose nonce spelling this build has never written is
        // still a partial download of that id, which is the fact the listing needs.
        let (id, nonce) = rest.rsplit_once('.')?;
        if nonce.is_empty() || validate_id(id).is_err() {
            return None;
        }
        Some(FetchName::Part(id))
    } else {
        None
    }
}

/// The marker path for `id` — the other direction of [`fetch_name`]’s rule, and the only place that spelling is
/// built.
///
/// `tests/models.rs` pins the round trip: for every id shape `validate_id` accepts, the name this returns is
/// recognised back as [`FetchName::Marker`] with that same id. Without that fence the reader and the writer
/// (R7b) could drift apart and `list` would report a fetch it cannot find.
pub fn marker_path(models_dir: &Path, id: &str) -> PathBuf {
    models_dir.join(format!("{FETCH_MARKER_PREFIX}{id}{FETCH_MARKER_SUFFIX}"))
}

/// What one fetch marker says about the download in progress (§12 rules 4 and 12).
///
/// Four fields, all **displayed and never acted on**: §12 rule 4 makes liveness a matter of consent, so
/// `started_at` is printed rather than compared with a clock. `bytes_total` is optional because a chunked
/// transfer or a missing `Content-Length` really does leave it unknown (§12 rule 12); `None` prints as "size
/// unknown", never as a denominator this program invented.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct FetchFields {
    /// The nonce naming the `.part.<id>.<nonce>` file this fetch writes — the human's link between the marker
    /// and the bytes on disk. Kept opaque: `list` prints it and decides nothing from it.
    ///
    /// **Never empty.** [`fetch_name`] refuses `.part.<id>.`, so a marker carrying `""` would point at a name this
    /// program's own recogniser calls junk; the reader rejects it as unreadable rather than storing a link that
    /// resolves to nothing.
    pub nonce: String,
    /// Whole seconds since the Unix epoch, as written. Displayed only.
    pub started_at: i64,
    /// Bytes received so far.
    pub bytes_done: u64,
    /// The size this fetch expects, when the transfer announced one (§12 rule 12).
    pub bytes_total: Option<u64>,
}

/// What reading one workspace's fetch marker for one id said. Never a `Result`: no marker condition is a fault
/// of the listing (§12 rule 3), so every answer below is data the report prints.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum FetchState {
    /// No marker file. The ordinary state of a workspace with no fetch running.
    Absent,
    /// A marker that reads as §12's field set.
    Present(FetchFields),
    /// A marker exists and this build cannot read it — truncated mid-write, not UTF-8, not an object, a field
    /// of the wrong type, or a file whose bytes cannot be opened at all.
    ///
    /// **Reported, never deleted, never skipped** (the F22/F23 posture: record what you found and let the human
    /// decide). `reason` is the parser's own sentence about this file, and `path` is there because §12 rule 4's
    /// advice — "if no fetch is running, delete this file and re-run" — is useless without it.
    Unreadable {
        /// The marker that could not be read.
        path: PathBuf,
        /// What was wrong with it, naming the field or the failure.
        reason: String,
    },
}

/// Read this workspace's fetch marker for `id` (§12 rule 4's "read-only" half).
///
/// `models_dir` comes from [`Config::workspace_models_dir`] like every other path here, and `id` is expected to
/// be one [`validate_id`] accepts — the ids `list` passes come out of [`fetch_name`], which already checked.
///
/// Nothing in this function deletes, renames or repairs. A marker whose bytes are unreadable stays exactly where
/// it is: an implicit deletion would be this program deciding that a download nobody is watching was worthless,
/// and §12 rule 4 refuses that decision to any command but an explicit `--discard-marker` (R7b's, not here).
pub fn fetch_state(models_dir: &Path, id: &str) -> FetchState {
    let path = marker_path(models_dir, id);
    // **Look before opening.** A name alone does not promise a regular file, and one of the alternatives blocks:
    // `fs::File::open` on a FIFO waits for a writer, so a stray `.fetching.<id>.json` pipe would hang
    // `np models list` forever — the exact power [`MARKER_MAX_BYTES`] exists to deny a stray file. Artifacts are
    // read the same way ([`artifact_state`] checks `is_file()` before hashing), and the answer is the one a
    // directory already got: reported as unreadable, never deleted, never "absent".
    //
    // **This guard is defence-in-depth, not a covered branch, and it is written down rather than simulated.** No
    // test distinguishes it: on Linux a *directory* at the marker path still reaches `Unreadable` through the read
    // failing, so deleting the block changes nothing any unit test can see (verified — the mutant survived). What it
    // actually buys was measured end to end instead: with a FIFO planted at `.fetching.gamma.json`, `np models list`
    // blocked until `timeout` killed it (exit 124) before this guard and now completes, printing
    // "it is not a regular file" for that id. A test would need `mkfifo`, i.e. a libc dependency or an external
    // binary plus a skip path on Windows — the skipped-branch class this project refuses to write coverage about.
    match fs::metadata(walk::to_open_path(&path)) {
        Err(source) if source.kind() == io::ErrorKind::NotFound => return FetchState::Absent,
        Err(source) => {
            return FetchState::Unreadable {
                path,
                reason: source.to_string(),
            }
        }
        Ok(meta) if !meta.is_file() => {
            return FetchState::Unreadable {
                path,
                reason: "it is not a regular file".to_string(),
            }
        }
        Ok(_) => {}
    }
    let opened = fs::File::open(walk::to_open_path(&access(&path)));
    let file = match opened {
        Err(source) if source.kind() == io::ErrorKind::NotFound => return FetchState::Absent,
        // A permission refusal — or a name that is a directory, which Linux opens and refuses to read — says
        // nothing about the marker's contents. That is why it shares `Unreadable` with a malformed one rather
        // than becoming `Absent`: "cannot look" must not read as "nothing there".
        Err(source) => {
            return FetchState::Unreadable {
                path,
                reason: source.to_string(),
            }
        }
        Ok(file) => file,
    };
    // Read with a ceiling rather than [`fs::read`], so an oversized file under a marker's name cannot decide how
    // much memory a listing allocates. One byte past the bound is enough to know it has been passed.
    let mut bytes = Vec::new();
    let read = (&file).take(MARKER_MAX_BYTES + 1).read_to_end(&mut bytes);
    if let Err(source) = read {
        return FetchState::Unreadable {
            path,
            reason: source.to_string(),
        };
    }
    if u64::try_from(bytes.len()).unwrap_or(u64::MAX) > MARKER_MAX_BYTES {
        return FetchState::Unreadable {
            path,
            reason: format!("it is larger than {MARKER_MAX_BYTES} bytes"),
        };
    }
    match parse_marker(&bytes) {
        Ok(fields) => FetchState::Present(fields),
        Err(reason) => FetchState::Unreadable { path, reason },
    }
}

/// Largest marker file this program reads, in bytes.
///
/// A marker holds four fields, so the honest size is a couple of hundred bytes. The bound exists because `list`
/// reads this file to write one report line: a name like `.fetching.alpha.json` filled with five gigabytes of
/// anything would otherwise be loaded into memory before this module got to say "that is not a marker", and one
/// stray file must not decide whether `np models list` survives. Refused as unreadable, never truncated silently.
const MARKER_MAX_BYTES: u64 = 64 * 1024;

/// Read one marker's bytes as §12's field set, or say why they are not that.
///
/// **Why this is a hand-written reader rather than a JSON dependency**: `np-core` has no JSON reader today and
/// R7a adds none (no dependency additions, and `serde_json` would arrive in the graph of every binary that links
/// this crate for one four-field file). So the accepted grammar is deliberately narrow — **one flat object of
/// string keys with string, whole-number or `null` values, nothing else** — and everything outside it is reported
/// as unreadable rather than guessed at:
///
/// - a non-object, a nested object or array, a trailing comma, or text after the closing `}`
/// - a field name this build does not know (a marker from a build with rules this one has cannot half-read)
/// - a field twice, a required field absent (`nonce`, `started_at`, `bytes_done` — only `bytes_total` is
///   optional, §12 rule 12), or a value of the wrong kind for its field
/// - a number that is not whole: `41.9`, `1e5` and `0x10` are refused rather than read as whatever a cast gives,
///   because a byte count invented by rounding is exactly the fabricated denominator §12 rule 12 forbids
/// - bytes that are not UTF-8, or a string holding a control character or a `\` escape (this program writes
///   neither, so their presence means the file did not come from this program)
fn parse_marker(bytes: &[u8]) -> Result<FetchFields, String> {
    let mut reader = MarkerReader::new(bytes);
    reader.skip_spaces();
    if reader.peek() != Some(b'{') {
        // Covers the empty file and a truncated write that stopped before the first brace: both are "a marker
        // this build cannot read", which is what `FetchState::Unreadable` exists to report.
        return Err("expected one JSON object".to_string());
    }
    reader.pos += 1;

    let mut nonce: Option<String> = None;
    let mut started_at: Option<i64> = None;
    let mut bytes_done: Option<u64> = None;
    // Two layers on purpose: outer "the field was there", inner "and it was not `null`". Both spellings of
    // unknown size mean the same thing to the report (§12 rule 12), so they meet at `flatten()` below.
    let mut bytes_total: Option<Option<u64>> = None;

    loop {
        reader.skip_spaces();
        if reader.peek() == Some(b'}') {
            // `{}` is a well-formed object that holds none of the four fields; the missing-field refusals below
            // are the honest report, not "expected a field name".
            reader.pos += 1;
            break;
        }
        let key = reader.quoted()?;
        reader.skip_spaces();
        if reader.peek() != Some(b':') {
            return Err(format!("field `{key}` is not followed by `:`"));
        }
        reader.pos += 1;
        reader.skip_spaces();
        let value = reader.value(key)?;
        match key {
            "nonce" => {
                if nonce.is_some() {
                    return Err(duplicate("nonce"));
                }
                // Empty is refused, not stored: `.part.<id>.` is a name [`fetch_name`] rejects, so an empty nonce
                // would be a link to nothing (§12 rule 2). Keeping the two spellings of "no nonce" in step here is
                // cheaper than trusting R7b to write one and read the other.
                let written = value.text("nonce")?;
                if written.is_empty() {
                    return Err(unparsable("nonce", "an empty string"));
                }
                nonce = Some(written);
            }
            "started_at" => {
                if started_at.is_some() {
                    return Err(duplicate("started_at"));
                }
                started_at = Some(value.signed("started_at")?);
            }
            "bytes_done" => {
                if bytes_done.is_some() {
                    return Err(duplicate("bytes_done"));
                }
                bytes_done = Some(value.count("bytes_done")?);
            }
            "bytes_total" => {
                if bytes_total.is_some() {
                    return Err(duplicate("bytes_total"));
                }
                bytes_total = Some(value.optional_count("bytes_total")?);
            }
            other => return Err(format!("unknown field `{other}`")),
        }
        reader.skip_spaces();
        match reader.peek() {
            Some(b',') => {
                reader.pos += 1;
                reader.skip_spaces();
                if reader.peek() == Some(b'}') {
                    return Err("a `,` before the closing `}`".to_string());
                }
            }
            Some(b'}') => {
                reader.pos += 1;
                break;
            }
            // Truncated mid-file lands here too, which is the point: a marker written by a fetch killed halfway
            // is reported with its fields unknown rather than trusted for the half that arrived.
            _ => return Err("expected `,` or `}` after a field".to_string()),
        }
    }

    reader.skip_spaces();
    if !reader.at_end() {
        return Err("text after the closing `}`".to_string());
    }
    Ok(FetchFields {
        nonce: nonce.ok_or_else(|| "no `nonce` field".to_string())?,
        started_at: started_at.ok_or_else(|| "no `started_at` field".to_string())?,
        bytes_done: bytes_done.ok_or_else(|| "no `bytes_done` field".to_string())?,
        bytes_total: bytes_total.flatten(),
    })
}

/// The reason a field arrived twice, which is a contradiction rather than a last-one-wins choice.
fn duplicate(field: &str) -> String {
    format!("field `{field}` appears twice")
}

/// One marker value, before its field name says what it has to be.
#[derive(Debug, Clone, Copy)]
enum MarkerValue<'a> {
    /// A quoted string, unescaped (escapes are refused at [`MarkerReader::quoted`]).
    Text(&'a str),
    /// The digits of a whole number, not yet parsed into a width.
    Count(&'a str),
    /// `null` — legal only for `bytes_total`.
    Null,
}

impl<'a> MarkerValue<'a> {
    fn text(self, field: &str) -> Result<String, String> {
        match self {
            Self::Text(text) => Ok(text.to_owned()),
            _ => Err(wrong_kind(field, "a quoted string")),
        }
    }

    /// A count that may be negative: `started_at` is a clock reading this program only **displays** (§12 rule 4),
    /// so refusing a value it cannot explain would be inventing a rule the design does not state.
    fn signed(self, field: &str) -> Result<i64, String> {
        let digits = self.digits(field)?;
        digits.parse::<i64>().map_err(|_| unparsable(field, digits))
    }

    /// A byte count. Negative is refused here and not at the display: bytes received cannot be fewer than none,
    /// and a report printing `-5 B / 309.9 MB` would be reporting a download that cannot have happened.
    fn count(self, field: &str) -> Result<u64, String> {
        let digits = self.digits(field)?;
        digits.parse::<u64>().map_err(|_| unparsable(field, digits))
    }

    /// `bytes_total`, where `null` and an absent field both mean "size unknown" (§12 rule 12).
    fn optional_count(self, field: &str) -> Result<Option<u64>, String> {
        match self {
            Self::Null => Ok(None),
            other => other.count(field).map(Some),
        }
    }

    fn digits(self, field: &str) -> Result<&'a str, String> {
        match self {
            Self::Count(digits) => Ok(digits),
            _ => Err(wrong_kind(field, "a whole number")),
        }
    }
}

fn wrong_kind(field: &str, expected: &str) -> String {
    format!("`{field}` holds the wrong kind of value; expected {expected}")
}

fn unparsable(field: &str, found: &str) -> String {
    format!("`{field}` is not a whole number this build can read: `{found}`")
}

/// A cursor over one marker's bytes. Working on bytes rather than `&str` keeps the comparison byte-exact (the
/// same rule [`fetch_name`] uses) and lets non-UTF-8 content reach a refusal instead of a lossy decode.
struct MarkerReader<'a> {
    bytes: &'a [u8],
    pos: usize,
}

impl<'a> MarkerReader<'a> {
    fn new(bytes: &'a [u8]) -> Self {
        Self { bytes, pos: 0 }
    }

    fn peek(&self) -> Option<u8> {
        self.bytes.get(self.pos).copied()
    }

    fn at_end(&self) -> bool {
        self.pos >= self.bytes.len()
    }

    /// JSON's four insignificant whitespace bytes.
    fn skip_spaces(&mut self) {
        while matches!(self.peek(), Some(b' ' | b'\t' | b'\n' | b'\r')) {
            self.pos += 1;
        }
    }

    /// One quoted string, with the quotes consumed. Escapes and control bytes are refused rather than decoded:
    /// a marker's two string values are a field name and a nonce of digits and `-`, so anything needing an escape
    /// did not come from this program.
    fn quoted(&mut self) -> Result<&'a str, String> {
        if self.peek() != Some(b'"') {
            return Err("expected a quoted field name".to_string());
        }
        self.pos += 1;
        let start = self.pos;
        loop {
            match self.peek() {
                None => return Err("a quoted string runs off the end of the file".to_string()),
                Some(b'"') => {
                    let bytes: &'a [u8] = self.bytes;
                    let text = &bytes[start..self.pos];
                    self.pos += 1;
                    return std::str::from_utf8(text)
                        .map_err(|_| "text that is not UTF-8".to_string());
                }
                Some(b'\\') => {
                    return Err("a `\\` escape, which this program never writes".to_string())
                }
                Some(byte) if byte < 0x20 => {
                    return Err("a control character inside a quoted string".to_string())
                }
                Some(_) => self.pos += 1,
            }
        }
    }

    /// The digits of one number, ending at the first byte that cannot continue one. That delimiter test is what
    /// refuses `41.9`, `1e5` and `12MB`: a value that continues past the digits is not a count this report may
    /// round into one.
    fn number(&mut self, field: &str) -> Result<&'a str, String> {
        let start = self.pos;
        while matches!(self.peek(), Some(byte) if byte == b'-' || byte.is_ascii_digit()) {
            self.pos += 1;
        }
        let bytes: &'a [u8] = self.bytes;
        let digits = &bytes[start..self.pos];
        if digits.is_empty() {
            return Err(format!(
                "`{field}` is not a number, `null`, or a quoted string"
            ));
        }
        if !matches!(
            self.peek(),
            None | Some(b',' | b'}' | b' ' | b'\t' | b'\n' | b'\r')
        ) {
            return Err(format!("`{field}` is not a whole number"));
        }
        // Every byte collected above is `-` or an ASCII digit, so this decode cannot fail; `map_err` keeps it
        // total without reaching for a panic in a report path.
        std::str::from_utf8(digits).map_err(|_| format!("`{field}` is not a number"))
    }

    fn value(&mut self, field: &str) -> Result<MarkerValue<'a>, String> {
        match self.peek() {
            Some(b'"') => Ok(MarkerValue::Text(self.quoted()?)),
            Some(b'n') => {
                let bytes: &'a [u8] = self.bytes;
                if !bytes[self.pos..].starts_with(b"null") {
                    return Err(format!("`{field}` starts with `n` but is not `null`"));
                }
                self.pos += 4;
                Ok(MarkerValue::Null)
            }
            _ => Ok(MarkerValue::Count(self.number(field)?)),
        }
    }
}

/// One entry of [`Listing::fetching`]: a recognised marker and what it said.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct FetchEntry {
    /// The id the marker's **name** carries — not read from the file, so a marker whose fields disagree with its
    /// own name cannot rename a fetch this listing did not see.
    pub id: String,
    /// The marker file itself, for a report to name (rule 4's delete-this-file advice needs a path).
    pub path: PathBuf,
    /// What reading it produced. `Absent` means the marker vanished between listing the directory and reading
    /// it — a fetch that finished inside that window — and [`list`] drops such an entry rather than announcing
    /// a download that has stopped.
    pub state: FetchState,
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
    /// One entry per marker file, sorted by id — **independent of [`Listing::models`]**, which is what §12 rule
    /// 3 means by orthogonal: an id with no row at all (a first start) appears here and nowhere else, and an id
    /// with a row can appear in both.
    ///
    /// Entries come from the **directory**, not from the rows: a marker nothing claims is exactly the case the
    /// human asked about ("is it downloading?"), and answering it from the row list would drop it.
    pub fetching: Vec<FetchEntry>,
    /// Entries no row names **and this program does not recognise**, sorted by display name (§3). These never
    /// change an exit code. Fetch names are absent by construction — see [`fetch_name`].
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

/// Report every installed model of `workspace`, any fetch in progress, and what each artifact looks like.
///
/// Reads only: nothing here repairs, re-hashes into place, or deletes — including the unclaimed entries,
/// which are leftovers this command cannot prove are garbage (§3), and including a marker that will not parse
/// ([`fetch_state`] reports it and leaves it alone).
///
/// A marker that is malformed, truncated, unreadable, or a directory **cannot fail this call**: `fetching` holds
/// one entry per marker name either way, because "the listing broke" is a worse answer than "here is the file I
/// could not read" (§12 rule 3's section exists to be looked at while a fetch is running).
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
    let (unclaimed, marker_ids) = entries.split(&claimed);

    let mut models = Vec::with_capacity(rows.len());
    for row in rows {
        let state = artifact_state(&models_dir, &row)?;
        models.push(ListedModel { row, state });
    }

    // Markers are read last: every failure above this point throws the whole listing away, and reading (and
    // parsing) a marker for a listing that is then discarded is work with no reader.
    let fetching = fetch_entries(&models_dir, marker_ids);

    Ok(Listing {
        models_dir,
        models,
        fetching,
        unclaimed,
    })
}

/// One [`FetchEntry`] per marker id, in id order, dropping any whose file vanished meanwhile.
///
/// Reading each marker is [`fetch_state`]'s job and this function adds no verdict of its own — see [`Listing::fetching`]
/// for why the ids come from the directory rather than from the rows.
fn fetch_entries(models_dir: &Path, mut marker_ids: Vec<String>) -> Vec<FetchEntry> {
    marker_ids.sort();
    let mut entries = Vec::with_capacity(marker_ids.len());
    for id in marker_ids {
        let state = fetch_state(models_dir, &id);
        if matches!(state, FetchState::Absent) {
            // The fetch finished (or its marker was removed) between `read_dir` and this read. Reporting a
            // download that has stopped is the false status; reporting nothing is honest about a state that has
            // already left the directory, and the next listing answers from what is there.
            continue;
        }
        entries.push(FetchEntry {
            path: marker_path(models_dir, &id),
            id,
            state,
        });
    }
    entries
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

/// The `<nonce>` half of the temporary artifact name: this process's id, the current time, and a counter.
///
/// **The claim this comment used to make — "digits and `-` only, so a temp name can never be mistaken for a valid
/// id" — was false about the whole name, and design review caught it.** The nonce is digits and `-`, but it sits
/// *inside* `<id>.<nonce>.tmp`, and `validate_id` allows `.`, `-` and `_` after the first character: so
/// `alpha.4242-1730000000123456-0.tmp` is a legal model id. What actually protects installs is that a temp name is
/// never *fed* to `validate_id`, plus `list`'s treatment of leftovers as unclaimed entries rather than as rows. The
/// real lesson lives in `docs/design-m2-local-embeddings.md` §12: machinery names must be unforgeable by
/// construction (leading `.`, which the first-character rule forbids for ids), not merely unlikely to collide.
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

/// The entries of one `read_dir`, kept as the two facts a listing can say about a name: its spelling and
/// directory-or-not.
///
/// A separate type because [`fs::ReadDir`] is a lazy iterator that cannot be re-walked, while [`list`] needs
/// it once and in one pass — deciding per name whether it is a fetch name, a claimed artifact, or junk cannot be
/// done three times over three filters without reading the directory three times.
#[derive(Debug, Default)]
struct EntryList(Vec<(OsString, UnclaimedKind)>);

impl EntryList {
    /// Split one directory into the entries nothing claims **and nothing recognises**, and the ids whose marker
    /// file is present.
    ///
    /// Two outputs from one pass because the two rules are one decision, made per name, in this order:
    ///
    /// 1. A name [`fetch_name`] recognises is never unclaimed (§12 rule 2). A marker *and* a `.part` both leave
    ///    here rather than being announced as junk — otherwise a first start reports its own progress file as
    ///    garbage, which is the defect §11's addendum records the first draft having.
    /// 2. Of those names, only a marker contributes an id to report; a `.part` is recognised and left at that,
    ///    because it holds no fields and R7a writes none (§12 rule 5 gives it to the fetcher).
    /// 3. Otherwise a row naming the name claims it; what nothing claims is reported, sorted by its lossy display
    ///    name so output is deterministic (§3).
    fn split(self, claimed: &HashSet<OsString>) -> (Vec<Unclaimed>, Vec<String>) {
        let mut unclaimed = Vec::new();
        let mut markers = Vec::new();
        for (name, kind) in self.0 {
            match fetch_name(&name) {
                Some(FetchName::Marker(id)) => markers.push(id.to_owned()),
                Some(FetchName::Part(_)) => {}
                None => {
                    if !claimed.contains(&name) {
                        unclaimed.push(Unclaimed {
                            name: name.to_string_lossy().into_owned(),
                            kind,
                        });
                    }
                }
            }
        }
        unclaimed.sort_by(|left, right| left.name.cmp(&right.name));
        (unclaimed, markers)
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
