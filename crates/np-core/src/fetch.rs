//! Fetching a pinned artifact: stream it, hash it while it arrives, verify it, publish it by rename
//! (`docs/design-m2-local-embeddings.md` §12 rules 4–8 and 12).
//!
//! ## Where the origin comes from — corrected 2026-11-04
//!
//! This module still names no host: what reaches it from outside is a `&dyn Transport` and an already-resolved origin
//! string, and [`resolve_origin`] is where that string is chosen.
//!
//! The sentence this replaces read "**Nothing in this module names a host**, because §12 rule 7 keeps the origin out of
//! the binary", and it is now wrong in its second half. Rule 7 permits exactly one compiled-in origin since
//! 2026-11-04 — [`crate::pins::DEFAULT_ORIGIN`] — decided by the human in the R7c scope decisions
//! (`docs/design-m2-image-embeddings.md` §13), because an install that cannot reach a network is an install nobody
//! completes and `np models fetch` is now the documented install step. The default lives in [`crate::pins`] beside the
//! bytes it was derived from, not here.
//!
//! What the old argument still buys, and why the default sits at the **end** of a precedence instead of replacing it:
//! a mirror stays one config line away (`[models] base_url`, or `NP_MODELS_BASE_URL` when the file leaves that key
//! out), the accepted-bytes list stays one reviewable table, and — the half this module owns — §12 rule 8 keeps the
//! origin out of every *refusal*, because client error strings carry URLs and this program's reports land in CI logs.
//! [`FetchError`] is therefore built from classes and counts, never from a URL. `tests/fetch.rs` checks that claim where it
//! can bite — the arms whose text comes from something this module was handed (a transport failure, an `io::Error`) — by
//! fetching from a loopback address and asserting that neither that address nor the baked default appears; the arms built
//! purely from ids, counts and paths are named as such there rather than swept silently.
//!
//! ## What is deferred, on purpose
//!
//! A fetch writes **bytes**, not a model row. Creating the row needs `dim`, `prefix_doc` and `prefix_query`, and the
//! pin table deliberately carries none of them (§12 rule 7 lists what it holds: what to accept). Inventing a width at
//! download time would be a declared-by-nothing identity — §2 of the install design makes the human declare it, and
//! `models.dim` is fenced by migration 7's `CHECK`. So this stage ends with a verified artifact on disk plus its member
//! rows when a row exists to hang them on, and the "what does a fetched model *declare*" question is recorded in
//! `todo.md` rather than answered by a constant here.
//!
//! ## The lifecycle one call runs
//!
//! 1. Refuse if a marker already exists (§12 rule 4: liveness is consent, never a clock).
//! 2. Preflight free space against **every** pinned member of the id plus a margin (rule 6) — an admission check, not
//!    a promise, and skipped with a stated reason where the platform cannot answer.
//! 3. Write `.fetching.<id>.json`, atomically, then for each member stream into `.part.<id>.<nonce>` **inside the
//!    models directory** (same filesystem, so the publish is a rename and never a second 310 MB copy — rule 5),
//!    hashing while counting and rewriting the marker as progress changes.
//! 4. Verify length first, then digest (rule 6's two different sentences), then `rename` into an **absent** target.
//! 5. Remove this call's marker and any `.part` it left, on every path out — including the error paths. A marker only
//!    ever means "a fetch is running in this process", which is a fact this function can still tell truthfully when
//!    it returns.

use std::collections::BTreeSet;
use std::fs::{self, OpenOptions};
use std::io::{self, Read, Write};
use std::path::{Path, PathBuf};

use thiserror::Error;

use crate::hash::Sha256Stream;
use crate::models::{self, FetchFields, FetchState, MAIN_FILE};
use crate::pins::{self, PinnedModel};
use crate::walk;

/// Free bytes the preflight demands **above** the pinned total (§12 rule 6).
///
/// 64 MiB is not a guess at a filesystem's behaviour — it is the size of the two things this command writes that no
/// pinned number covers: the marker and its temp, plus whatever the platform's journal wants. It cannot make ENOSPC
/// impossible (free bytes are not quota, and a mid-file failure stays possible); it removes the certain failure of
/// starting a 310 MB download onto a full disk.
pub const PREFLIGHT_MARGIN_BYTES: u64 = 64 * 1024 * 1024;

/// How often a running fetch rewrites its marker.
///
/// Progress that updates per 16 KiB buffer would rewrite the file ~19,000 times for one artifact and buy nothing a
/// human can read; 8 MiB is ~38 rewrites across the pair, each an atomic rename. The value is a UX choice about how
/// stale "still downloading" may look, not a fact about any transfer.
const MARKER_INTERVAL_BYTES: u64 = 8 * 1024 * 1024;

/// What one member of a fetch must be for the transfer to be accepted.
///
/// **Two spellings, one rule.** Production answers with [`crate::pins::PinnedModel`] — `&'static` fields compiled in
/// by [`crate::pins::PINNED`], which is what makes an expectation impossible to change at runtime. Tests answer with
/// their own type holding a few dozen bytes, because verifying §12's rules against the real 309,855,456-byte artifact
/// would need a disk this host does not have. The trait exists so *the rules* are the same code in both cases; it is
/// not an extension point for a table someone assembles at runtime, and nothing outside a test passes anything else.
pub trait MemberPin {
    /// The model id this member belongs to.
    fn id(&self) -> &str;
    /// Which file of that id (`main`, or a later member's name).
    fn member(&self) -> &str;
    /// The file name the verified bytes are published under.
    fn filename(&self) -> &str;
    /// Exact length that must arrive.
    fn size_bytes(&self) -> u64;
    /// Lowercase hex sha256 the bytes must hash to.
    fn sha256(&self) -> &str;
}

impl MemberPin for PinnedModel {
    fn id(&self) -> &str {
        self.id
    }
    fn member(&self) -> &str {
        self.member
    }
    fn filename(&self) -> &str {
        self.filename
    }
    fn size_bytes(&self) -> u64 {
        self.size_bytes
    }
    fn sha256(&self) -> &str {
        self.sha256
    }
}

/// One member's worth of bytes, as a [`Transport`] hands them over.
pub struct Body {
    /// The body itself. Boxed because a transport decides what type that is.
    pub reader: Box<dyn Read + Send>,
    /// The size the transfer announced, when it announced one (§12 rule 12: chunked or absent `Content-Length`
    /// leaves this `None`, and the report prints "size unknown" rather than inventing a denominator).
    pub bytes_total: Option<u64>,
}

/// Where the bytes come from.
///
/// **A seam, not an abstraction layer:** it exists so every rule in this module is testable without a network (the
/// pinned digests in `tests/fetch.rs` are of two-hundred-byte fixtures, not of 310 MB), and so a mirror is a config
/// edit rather than a code change. The one production implementation is the HTTP transport in `np-cli`, behind cargo
/// feature `fetch`; this crate keeps no client of its own, which is what lets `np scan` remain unable to reach a
/// network type at all.
pub trait Transport {
    /// Open `member` for reading.
    ///
    /// The error must name a **class** (`"not found"`, `"transport failure"`), never a URL: whatever a transport
    /// knows about hosts stays inside it ([`FetchError::Transport`]'s contract).
    fn open(&self, member: &dyn MemberPin) -> Result<Body, FetchError>;
}

/// What can stop a fetch. Each arm is §12 rule 6's or rule 4's named case, and none of them is an accusation this
/// program cannot support.
#[derive(Debug, Error)]
pub enum FetchError {
    /// The id argv handed is not one [`crate::models::validate_id`] accepts. Kept distinct from [`FetchError::NothingPinned`]
    /// because the two refusals send the human to different places: this one to the command line, that one to the pin
    /// table.
    #[error("`{id}` is not a model id this program can name: {rule}")]
    BadId {
        /// The id as argv spelled it.
        id: String,
        /// The rule it broke, in `validate_id`'s words.
        rule: String,
    },
    /// `id` is not something this build pins anything for. Reported before a byte is requested: a fetch that dials
    /// out to discover it has no expectation to check against would download into a disk it cannot verify.
    #[error("no artifact is pinned for model id `{id}`, so there is nothing a fetch could accept")]
    NothingPinned {
        /// The id the caller asked for.
        id: String,
    },
    /// A pin exists for the id but not for this member — today that is `mmproj`, whose digest nobody has recorded
    /// (`crate::pins::MMPROJ_DIGEST_PREFIX_ONLY`). Refusing beats guessing: a digested-by-nobody expectation is
    /// indistinguishable from a real one.
    #[error("member `{member}` of model `{id}` has no recorded digest in this build, so it cannot be verified")]
    MemberUnpinned {
        /// The id asked for.
        id: String,
        /// Which member was missing.
        member: String,
    },
    /// A marker already exists (§12 rule 4). **Refuse, and hand the human the decision** — the fields are displayed,
    /// never compared with a clock, because a resumed laptop moves wall time further than any heartbeat.
    #[error(
        "a fetch of `{id}` is already recorded: {} (started_at {started_at}, {bytes_done} bytes done{}, \
         part nonce {nonce}) — if no fetch is running, delete that file and re-run; to discard it now, re-run \
         with --discard-marker",
        .path.display(),
        .total_label.as_deref().map(|t| format!(", of {t} total")).unwrap_or_default()
    )]
    MarkerExists {
        /// The id being fetched.
        id: String,
        /// The marker that caused the refusal — §12 rule 4's advice is useless without the path.
        path: PathBuf,
        /// The nonce linking the marker to its `.part` file, as written.
        nonce: String,
        /// Displayed only.
        started_at: i64,
        /// Displayed only.
        bytes_done: u64,
        /// Pre-formatted so the message can say "size unknown" without a nested conditional (§12 rule 12).
        total_label: Option<String>,
    },
    /// A marker exists and cannot be read. Reported, never deleted, never treated as absent — the same posture as
    /// [`FetchState::Unreadable`].
    #[error("the fetch marker {} cannot be read ({reason}), so this command will not overwrite it", .path.display())]
    MarkerUnreadable {
        /// The unreadable marker.
        path: PathBuf,
        /// What was wrong with it.
        reason: String,
    },
    /// The id has pinned bytes but no declared identity. Refused before any request: 865 MB that cannot be recorded
    /// as a model is a download with no purpose, and inventing a `dim` at the end of one would be an identity
    /// declared by nobody (install design §2).
    #[error("`{id}` has pinned artifacts but no declared dim or prefixes in this build, so a fetch could not record it")]
    NoIdentity {
        /// The id asked for.
        id: String,
    },
    /// A row for this id already exists and declares something else. **Both sides named, nothing written** — the
    /// same shape `install` uses for a disagreeing declaration, because "your workspace said 1024 and this build
    /// pins 768" is a fact the human has to resolve, not one this command may overwrite.
    #[error("`{id}` is already declared as {stored}; this build pins {pinned}")]
    IdentityDisagrees {
        /// The id whose row disagrees.
        id: String,
        /// The workspace's declaration, spelled by this module's private `declare`: width plus both prefixes, with
        /// an empty one shown as `""` rather than vanishing from the sentence.
        stored: String,
        /// What this build pins, in the same spelling, so the two read side by side.
        pinned: String,
    },
    /// The workspace database refused the row or the member rows. The artifact may already be published at this
    /// point, which is the same crash state `install`'s rename-then-insert gap leaves and `list` reports as a model
    /// with no recorded files — reported here rather than hidden behind a success.
    #[error("cannot record the fetched model in this workspace: {source}")]
    Db {
        /// The database's own failure.
        source: crate::db::DbError,
    },
    /// Free space is below the pinned total plus [`PREFLIGHT_MARGIN_BYTES`] (rule 6). Both numbers, no advice about
    /// what to delete.
    #[error(
        "not enough free space to fetch this model: {needed} bytes needed (pinned members plus a {margin} byte \
         margin), {available} available"
    )]
    DiskShortfall {
        /// Pinned total plus margin.
        needed: u64,
        /// What the filesystem reported.
        available: u64,
        /// The margin included in `needed`, spelled out so the arithmetic is checkable.
        margin: u64,
    },
    /// The transport could not deliver a stream at all — refused connection, a status with no body, a DNS failure.
    ///
    /// **`class` is the whole payload on purpose.** A client's own error string names the host and the full URL; §12
    /// rule 8 says those do not belong in a report that lands in CI logs, so transports summarise into a class here
    /// (`"not found"`, `"forbidden"`, `"server error"`, `"transport failure"`) and log detail stays where the human
    /// who can reach the host will look for it.
    #[error("could not open {member} for {id}: {class}")]
    Transport {
        /// The id being fetched.
        id: String,
        /// Which member failed.
        member: String,
        /// A class, never a URL.
        class: String,
    },
    /// The stream ended short of the pinned length, or announced a length it did not deliver (rule 6's
    /// `FetchInterrupted`). Saying "short" is supportable; saying "corrupted" is not — nothing here knows why.
    #[error("the transfer for member `{member}` of `{id}` ended after {got} bytes; this build pins {expected}")]
    Interrupted {
        /// The id being fetched.
        id: String,
        /// Which member came up short.
        member: String,
        /// Bytes actually received.
        got: u64,
        /// The pinned length.
        expected: u64,
    },
    /// Right number of bytes, different digest (rule 6's `FetchDigestMismatch`). The wording is §12's: the published
    /// file **is not the file this build pins** — an upstream re-upload or an alteration in transit are both
    /// consistent with what we know, and "your download is corrupted" blames the human for either.
    #[error(
        "member `{member}` of `{id}` is not the artifact this build pins: expected sha256 {expected}, got \
         {found}, at {size} bytes (the size matches). Verify with `np models hash`, or install a file you trust \
         with `np models install --path`"
    )]
    DigestMismatch {
        /// The id being fetched.
        id: String,
        /// Which member disagreed.
        member: String,
        /// What this build pins.
        expected: String,
        /// What arrived.
        found: String,
        /// The length both agree on — printed because it is the fact that separates this from [`FetchError::Interrupted`].
        size: u64,
    },
    /// The artifact path is occupied. Rule 5 publishes into an **absent** target and refuses otherwise: overwriting
    /// verified bytes is `install`'s `ArtifactDisagrees` problem, not a fetch's answer.
    #[error("refusing to overwrite the existing artifact {}", .path.display())]
    TargetExists {
        /// The occupied target.
        path: PathBuf,
    },
    /// A file this command needed to create, read or remove could not be. Paths only — never a URL, which is why no
    /// arm of this enum carries one.
    #[error("cannot write {}: {source}", .path.display())]
    Io {
        /// What was being touched.
        path: PathBuf,
        /// The OS's reason.
        source: io::Error,
    },
}

/// One member that made it all the way: verified bytes, published by rename.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct FetchedMember {
    /// `main`, or a later member's name (§12 rule 10's `model_files.name`).
    pub member: String,
    /// Where the verified bytes now live.
    pub path: PathBuf,
    /// Bytes received — equal to the pinned size, because anything else refused ([`FetchError::Interrupted`]).
    pub bytes: u64,
    /// The digest that matched the pin, as lowercase hex.
    pub sha256: String,
}

/// What one successful `fetch` call did.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct FetchReport {
    /// The id that was fetched.
    pub id: String,
    /// Every published member, in pin order.
    pub members: Vec<FetchedMember>,
    /// Sum of [`FetchedMember::bytes`].
    pub total_bytes: u64,
    /// What the disk preflight saw, when the platform could say (see this module's private `available_bytes`).
    /// `None` is reported as
    /// "free space not checked on this platform", never as a pass.
    pub free_bytes: Option<u64>,
}

impl FetchReport {
    /// Whether any member's bytes were published. A caller that reports "fetched nothing" and a caller that reports
    /// an internal error are describing different runs, and only this method can tell them apart.
    pub fn wrote_anything(&self) -> bool {
        !self.members.is_empty()
    }
}

/// The name a fetched member gets **on this disk**, which is not its name on the origin.
///
/// `install` publishes the primary artifact at `<models_dir>/<id>`, and every existing reader assumes exactly that —
/// `list`'s state, D12's verify-on-change, and the loader that turns a row into weights. So a fetch may not invent its
/// own spelling: publishing under the upstream file name leaves the row pointing at nothing (`state: missing`) while the
/// real bytes sit beside it as `unclaimed` junk. That is not a cosmetic disagreement — an artifact the program cannot
/// find is an artifact that will be re-downloaded, or deleted as clutter, by whoever notices first.
///
/// The primary member therefore takes the id, matching install byte for byte, and any later member takes
/// `<id>.<member>`: a name `validate_id` could never produce on its own (ids cannot contain a leading dot, and this one
/// has none) so it cannot collide with another model's artifact, and it still carries no host or directory.
///
/// **The pin's `filename` is the remote name only** — what [`HttpTransport`] asks the origin for. Two names for two
/// places, each decided in exactly one function: [`crate::models::member_artifact_name`] here, and the URL there.
/// This wrapper exists so a fetch reads as naming a *fetch* target; the rule itself lives with the other artifact
/// naming, where `list` and the loader can see it too.
pub fn local_name(id: &str, member: &str) -> String {
    models::member_artifact_name(id, member)
}

/// Spell one model's declaration the same way on both sides of a disagreement.
///
/// Six fields became two strings here because the `Err` variant of this module's result was growing into something
/// clippy reasonably objects to moving, and the *sentence* is what a human reads: pre-formatting it loses nothing and
/// keeps an empty prefix visible (`""`) where six separate fields would have let it disappear.
fn declare(dim: i64, prefix_doc: &str, prefix_query: &str) -> String {
    format!("dim {dim} with prefixes {prefix_doc:?} / {prefix_query:?}")
}

/// Everything a fetch must accept and record, as one value.
///
/// Grouped because the eight separate parameters this function would otherwise take tripped its own argument-count
/// fence — and grouping is the honest fix rather than an `#[allow]`: the pin table and the declaration are **one
/// decision** (which bytes, and what model they make), and passing them apart invites a caller to pair one id's
/// digests with another id's width.
pub struct Expectation<'a, T> {
    /// The members this call may accept, and what each must hash to.
    pub table: &'a [T],
    /// The identity the row records when the bytes land.
    pub declaration: &'a Declaration,
}

/// What a completed fetch records in `models`: width and both prefixes.
///
/// Production builds this from [`crate::pins::identity`] — the same reviewed table that pins the bytes, so an id's
/// identity travels with its digests instead of being re-typed per call. A test builds its own, which is what lets
/// the row-creation path be exercised without downloading 865 MB.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Declaration {
    /// Vector width to record.
    pub dim: u16,
    /// Document prefix to record.
    pub prefix_doc: String,
    /// Query prefix to record.
    pub prefix_query: String,
}

/// Fetch every pinned member of `id` into `models_dir` and record the model.
///
/// **Order matters and is the point:** the marker exists before byte one (so a listing during the transfer can say
/// so), the `.part` lives in the destination directory (so publishing is a rename, rule 5), length is checked before
/// digest (rule 6's two different sentences), and the target must be absent. The marker this call wrote is removed on
/// every path out, success or failure — see the module docs for why that is the honest lifetime rather than a cleanup.
/// The member names to fetch are **passed in**, and every one of them must be pinned. That shape is what makes
/// [`FetchError::MemberUnpinned`] reachable rather than decorative: the CLI asks for the whole pair by default, and a
/// build that pins only `main` refuses out loud instead of quietly downloading half of it.
pub fn fetch(
    db: &crate::db::Db,
    models_dir: &Path,
    id: &str,
    requested: &[&str],
    transport: &dyn Transport,
    now: i64,
) -> Result<FetchReport, FetchError> {
    // **The pin table answers first.** An id with no members is a typo or an unsupported model, and the answer it
    // needs is "this build pins nothing for that"; asking about identity first would tell someone who mistyped an id
    // that it has "pinned artifacts but no declared dim" — a sentence about a different problem, and the kind of
    // message that sends a human to edit the wrong file.
    if crate::pins::pinned(id).next().is_none() {
        return Err(FetchError::NothingPinned { id: id.into() });
    }
    let pinned =
        crate::pins::identity(id).ok_or_else(|| FetchError::NoIdentity { id: id.into() })?;
    let declaration = Declaration {
        dim: pinned.dim,
        prefix_doc: pinned.prefix_doc.to_string(),
        prefix_query: pinned.prefix_query.to_string(),
    };
    let plan = Expectation {
        table: crate::pins::PINNED,
        declaration: &declaration,
    };
    fetch_with_table(db, models_dir, id, requested, transport, now, &plan)
}

/// [`fetch`] against an explicit pin table.
///
/// **The table is a parameter; the values are not.** Production calls this through [`fetch`], which passes the
/// compiled-in [`crate::pins::PINNED`], so nothing at runtime can swap an expectation. The seam exists because every
/// rule below — digest mismatch, short stream, progress marker, preflight — has to be *tested*, and testing them
/// against a 310 MB pinned artifact would mean downloading 310 MB per case on a host with 2.8 GB free. A test table
/// holds a few dozen bytes per member whose digests the test computed, which is what makes the mismatch and
/// interruption cases reachable at all. `T` is generic rather than `dyn` so the production call keeps the concrete
/// `PinnedModel` slice it already has.
pub fn fetch_with_table<T: MemberPin>(
    db: &crate::db::Db,
    models_dir: &Path,
    id: &str,
    requested: &[&str],
    transport: &dyn Transport,
    now: i64,
    plan: &Expectation<'_, T>,
) -> Result<FetchReport, FetchError> {
    let Expectation { table, declaration } = plan;
    // An id that names nothing cannot be fetched, and naming it wrongly is an argv fact — not a file fact, which is
    // why this does not travel through `Io`.
    if let Err(err) = models::validate_id(id) {
        return Err(FetchError::BadId {
            id: id.into(),
            rule: err.to_string(),
        });
    }

    let lookup = |name: &str| {
        table
            .iter()
            .find(|entry| entry.id() == id && entry.member() == name)
    };
    if table.iter().all(|entry| entry.id() != id) || requested.is_empty() {
        // An empty request is not "fetch nothing, successfully": it is a caller that resolved no members, and the
        // answer it needs is the same refusal an unknown id gets.
        return Err(FetchError::NothingPinned { id: id.into() });
    }
    // **Resolve every requested member before byte one.** Discovering at member two that its digest is unknown, after
    // member one landed, would leave a half-pair on disk and a refusal that cannot say "the other half never arrived"
    // without having written the first half to prove it.
    let mut members: Vec<&T> = Vec::with_capacity(requested.len());
    for name in requested {
        match lookup(name) {
            Some(entry) => members.push(entry),
            None => {
                return Err(FetchError::MemberUnpinned {
                    id: id.into(),
                    member: (*name).into(),
                })
            }
        }
    }

    // **The row's declared identity is checked before byte one**, like the target-exists rule: it is a fact this
    // command can learn without transferring anything, and a workspace that declared 1,024 dimensions must not have
    // 865 MB arrive and then be told its model disagrees.
    if let Some(row) = crate::db::model_row(db, id).map_err(|source| FetchError::Db { source })? {
        let primary = members
            .iter()
            .find(|member| member.member() == PRIMARY_MEMBER)
            .copied()
            .unwrap_or(members[0]);
        let stored_prefixes_differ = row.prefix_doc != declaration.prefix_doc
            || row.prefix_query != declaration.prefix_query;
        let width_differs = row.dim != i64::from(declaration.dim);
        // The digest is compared only when the workspace actually recorded one: `models.sha256` is NULL for a
        // provider with no weights, and "no digest on file" is not evidence of disagreement.
        let digest_differs = match row.sha256.as_deref() {
            Some(stored) => stored != primary.sha256(),
            None => false,
        };
        if width_differs || stored_prefixes_differ || digest_differs {
            return Err(FetchError::IdentityDisagrees {
                id: id.into(),
                stored: declare(row.dim, &row.prefix_doc, &row.prefix_query),
                pinned: declare(
                    i64::from(declaration.dim),
                    &declaration.prefix_doc,
                    &declaration.prefix_query,
                ),
            });
        }
    }

    // Rule 4, before any byte: a marker is consent, and consent was already given to someone else.
    match models::fetch_state(models_dir, id) {
        FetchState::Absent => {}
        FetchState::Present(fields) => {
            return Err(FetchError::MarkerExists {
                id: id.into(),
                path: models::marker_path(models_dir, id),
                nonce: fields.nonce,
                started_at: fields.started_at,
                bytes_done: fields.bytes_done,
                total_label: fields
                    .bytes_total
                    .map(|total| total.to_string())
                    .or_else(|| Some("size unknown".to_string())),
            });
        }
        FetchState::Unreadable { path, reason } => {
            return Err(FetchError::MarkerUnreadable { path, reason });
        }
    }

    // Summed over the members this call will actually write. [`pins::total_size`] sums the whole table, and once a
    // second member exists the two numbers differ whenever a caller asks for one file.
    let needed = members
        .iter()
        .map(|member| member.size_bytes())
        .sum::<u64>()
        .saturating_add(PREFLIGHT_MARGIN_BYTES);
    // Bound before it is judged, because the report prints exactly this number: asking the OS a second time would let the line
    // describe a measurement nobody compared against `needed`.
    let free = available_bytes(models_dir);
    preflight(needed, free)?;

    fs::create_dir_all(models_dir).map_err(|source| FetchError::Io {
        path: models_dir.to_path_buf(),
        source,
    })?;

    // Rule 5's other half, checked for **every** member before any byte of any member moves. This used to sit inside the
    // transfer loop below, where it was correct for one member and wrong for two: the first artifact was published, then the
    // second target refused, leaving a file on disk with no `models` row to explain it and a retry that refuses forever.
    // Two-member plans stopped being hypothetical when §13 made a projector a fetchable member of the default set, so the
    // loop below now trusts this one and does not re-check.
    for member in &members {
        let target = models_dir.join(local_name(id, member.member()));
        if fs::symlink_metadata(&target).is_ok() {
            return Err(FetchError::TargetExists { path: target });
        }
    }

    let nonce = models::nonce();
    let mut report = FetchReport {
        id: id.into(),
        members: Vec::new(),
        total_bytes: 0,
        free_bytes: free,
    };

    for member in &members {
        // Every target was proved absent above, so this loop publishes into what is still an empty place.
        let target = models_dir.join(local_name(id, member.member()));

        write_marker(
            models_dir,
            id,
            &nonce,
            FetchFields {
                nonce: nonce.clone(),
                started_at: now,
                bytes_done: 0,
                bytes_total: Some(member.size_bytes()),
            },
        )?;

        let outcome = receive_member(models_dir, id, *member, transport, &nonce, now);
        let published = match outcome {
            Ok(published) => published,
            Err(err) => {
                // The `.part` is this call's own garbage once the digest or length refused it; the marker goes with it
                // (there is no resume in R7b — `--discard-marker` is the only door, §12 rule 4). Both removals are
                // best-effort: a refusal whose cleanup also failed still reports the *reason*, not the cleanup.
                remove_part(models_dir, id, &nonce);
                let _ = fs::remove_file(models::marker_path(models_dir, id));
                return Err(err);
            }
        };

        fs::rename(&published.temp, &target).map_err(|source| {
            remove_part(models_dir, id, &nonce);
            FetchError::Io {
                path: target.clone(),
                source,
            }
        })?;

        report.total_bytes += published.bytes;
        report.members.push(FetchedMember {
            member: member.member().to_string(),
            path: target,
            bytes: published.bytes,
            sha256: published.sha256,
        });
    }

    // Durable artifact first, then the rows that name it — `install`'s order, for the same reason (F21: a record
    // before the echo). A crash between the rename and here is the state `list` already reports as an artifact with
    // no row, and re-running resolves it: the target-exists rule refuses until the human says otherwise, which is
    // why the row insert happens last rather than first.
    let primary = members
        .iter()
        .find(|member| member.member() == PRIMARY_MEMBER)
        .copied()
        .unwrap_or(members[0]);
    crate::db::insert_model(
        db,
        id,
        i64::from(declaration.dim),
        Some(primary.sha256()),
        &declaration.prefix_doc,
        &declaration.prefix_query,
        now,
    )
    .map_err(|source| FetchError::Db { source })?;
    record_members(db, &report).map_err(|source| FetchError::Db { source })?;

    let _ = fs::remove_file(models::marker_path(models_dir, id));
    Ok(report)
}

/// Record the fetched members on an existing model row (migration 9).
///
/// **Separate from [`fetch`] because the row is a different decision.** `models.dim`, `prefix_doc` and `prefix_query`
/// are declared by a human (install design §2), and nothing in the pin table declares them, so a fetch cannot create
/// the row honestly. When one exists — installed, or declared by whatever command that gap ends up needing — this is
/// where the pair's digests get stored under their names.
pub fn record_members(db: &crate::db::Db, report: &FetchReport) -> Result<(), crate::db::DbError> {
    for member in &report.members {
        crate::db::record_model_file(
            db,
            &report.id,
            &member.member,
            &member.sha256,
            i64::try_from(member.bytes).unwrap_or(i64::MAX),
        )?;
    }
    Ok(())
}

/// What [`receive_member`] produced: bytes on disk under a temp name, verified and ready to rename.
struct Published {
    temp: PathBuf,
    bytes: u64,
    sha256: String,
}

/// Stream one member into `.part.<id>.<nonce>`, hashing while counting, and verify it.
fn receive_member(
    models_dir: &Path,
    id: &str,
    member: &dyn MemberPin,
    transport: &dyn Transport,
    nonce: &str,
    now: i64,
) -> Result<Published, FetchError> {
    let fail = |class: String| FetchError::Transport {
        id: id.into(),
        member: member.member().into(),
        class,
    };
    // Keep the transport's **class** and drop anything else it carried: every arm of `FetchError` other than
    // `Transport` describes this module's own file work, so a member that never opened stays a transport refusal
    // (§12 rule 8).
    let body = transport.open(member).map_err(|err| fail(err.class()))?;

    let temp = models_dir.join(format!(".part.{id}.{nonce}"));
    let mut file = OpenOptions::new()
        .create(true)
        .truncate(true)
        .write(true)
        .open(walk::to_open_path(&temp))
        .map_err(|source| FetchError::Io {
            path: temp.clone(),
            source,
        })?;

    let mut digest = Sha256Stream::new();
    let mut next_marker = MARKER_INTERVAL_BYTES;
    let mut count: u64 = 0;
    let mut buffer = vec![0_u8; READ_BYTES];
    let mut reader = body.reader;
    loop {
        let read = reader.read(&mut buffer).map_err(|source| FetchError::Io {
            path: temp.clone(),
            source,
        })?;
        if read == 0 {
            break;
        }
        file.write_all(&buffer[..read])
            .map_err(|source| FetchError::Io {
                path: temp.clone(),
                source,
            })?;
        digest.update(&buffer[..read]);
        count += read as u64;
        if count >= next_marker {
            next_marker = count.saturating_add(MARKER_INTERVAL_BYTES);
            write_marker(
                models_dir,
                id,
                nonce,
                FetchFields {
                    nonce: nonce.into(),
                    started_at: now,
                    bytes_done: count,
                    bytes_total: Some(member.size_bytes()),
                },
            )?;
        }
    }
    file.sync_all().map_err(|source| FetchError::Io {
        path: temp.clone(),
        source,
    })?;
    drop(file);

    // Length before digest (§12 rule 6): "short" and "different bytes of the same length" are different facts with
    // different advice, and a mismatch message that had to guess which one it was would be the less useful of them.
    if count != member.size_bytes() {
        return Err(FetchError::Interrupted {
            id: id.into(),
            member: member.member().into(),
            got: count,
            expected: member.size_bytes(),
        });
    }
    let found = digest.finish_hex();
    if found != member.sha256() {
        return Err(FetchError::DigestMismatch {
            id: id.into(),
            member: member.member().into(),
            expected: member.sha256().into(),
            found,
            size: count,
        });
    }

    Ok(Published {
        temp,
        bytes: count,
        sha256: found,
    })
}

/// Write the marker atomically: temp in the same directory, then rename (§12 rule 2, which also names why the temp's
/// name has to be recognisable — [`models::FetchName::MarkerTemp`]).
fn write_marker(
    models_dir: &Path,
    id: &str,
    nonce: &str,
    fields: FetchFields,
) -> Result<(), FetchError> {
    let path = models::marker_path(models_dir, id);
    let temp = models_dir.join(format!(".fetching.{id}.json.tmp{nonce}"));
    let written = render_marker(&fields);
    {
        let mut file = OpenOptions::new()
            .create(true)
            .truncate(true)
            .write(true)
            .open(walk::to_open_path(&temp))
            .map_err(|source| FetchError::Io {
                path: temp.clone(),
                source,
            })?;
        file.write_all(written.as_bytes())
            .and_then(|()| file.sync_all())
            .map_err(|source| FetchError::Io {
                path: temp.clone(),
                source,
            })?;
    }
    fs::rename(walk::to_open_path(&temp), walk::to_open_path(&path)).map_err(|source| {
        let _ = fs::remove_file(&temp);
        FetchError::Io {
            path: path.clone(),
            source,
        }
    })?;
    Ok(())
}

/// The marker's four fields as one JSON object — the exact shape [`models::fetch_state`] parses.
///
/// Hand-written for the same reason the reader is: `np-core` has no JSON dependency, and a fourth party to this format
/// (a serde derive on one side only) would let writer and reader drift into "unreadable marker" reports that are really
/// a schema disagreement. **`tests/fetch.rs` round-trips every field spelling through the reader**, which is what keeps
/// the two halves honest.
fn render_marker(fields: &FetchFields) -> String {
    let total = match fields.bytes_total {
        Some(total) => total.to_string(),
        None => "null".to_string(),
    };
    format!(
        "{{\"nonce\":\"{}\",\"started_at\":{},\"bytes_done\":{},\"bytes_total\":{}}}",
        escape_json(&fields.nonce),
        fields.started_at,
        fields.bytes_done,
        total
    )
}

/// Escape what [`crate::models::parse_marker`] un-escapes.
///
/// A nonce is written by [`models::nonce`] and contains neither a quote nor a backslash; escaping it anyway costs one
/// loop and means the writer cannot produce a marker this build's own reader refuses, whatever a future nonce looks
/// like. Control characters become `\u00XX` because JSON forbids them raw.
fn escape_json(value: &str) -> String {
    let mut out = String::with_capacity(value.len());
    for ch in value.chars() {
        match ch {
            '"' => out.push_str("\\\""),
            '\\' => out.push_str("\\\\"),
            '\n' => out.push_str("\\n"),
            '\r' => out.push_str("\\r"),
            '\t' => out.push_str("\\t"),
            c if (c as u32) < 0x20 => {
                out.push_str(&format!("\\u{:04x}", c as u32));
            }
            c => out.push(c),
        }
    }
    out
}

/// Delete this call's `.part`, whatever it holds. Best-effort: a leftover partial download is reported by `list` as a
/// recognised fetch name, never as junk, and a refusal that also lost the disk fight still has to report its reason.
fn remove_part(models_dir: &Path, id: &str, nonce: &str) {
    let _ = fs::remove_file(models_dir.join(format!(".part.{id}.{nonce}")));
}

/// Free bytes on the filesystem holding `path`, or `None` where this platform cannot say.
///
/// **What this host answers with is measured; what the other arms claim is not** (F18). This half is `statvfs`, whose block
/// size times free blocks is the number §12 rule 6's preflight wants.
///
/// The sentence here used to read "Windows and macOS get `None`", and it was wrong about macOS: macOS is `cfg(unix)`, so it
/// compiles **this** arm (`libc` declares `statvfs` for every unix but linux-gnu, and np-core's `libc` dependency is gated on
/// `cfg(unix)`), which means the shipped `aarch64-apple-darwin` binary answers rule 6 with a number today. What nobody has measured
/// is whether that number is right — no macOS runner exists here, so this round says nothing about it either way.
///
/// Where an arm genuinely cannot answer it returns `None`, which the report prints as "free space not checked on this platform" —
/// never as a pass, because a check that silently did not happen is worse than one whose absence is stated.
#[cfg(unix)]
fn available_bytes(path: &Path) -> Option<u64> {
    use std::ffi::CString;
    use std::os::unix::ffi::OsStrExt;

    // **Climb to something that exists.** A first fetch of a workspace has no models directory yet — it is about to
    // create one — and `statvfs` on a missing path answers ENOENT, which would turn the preflight into a silent
    // "cannot check" on exactly the fresh machine §12's amendment is about. Walking up to the nearest existing
    // ancestor asks the question actually meant: how much room is on the filesystem this directory will live on.
    let mut cursor = path;
    let c_path = loop {
        match std::fs::symlink_metadata(cursor) {
            Ok(_) => break CString::new(cursor.as_os_str().as_bytes()).ok()?,
            Err(_) => match cursor.parent() {
                Some(parent) if !parent.as_os_str().is_empty() => cursor = parent,
                _ => return None,
            },
        }
    };

    // SAFETY: `c_path` is a valid NUL-terminated path and `stat` is written to by the callee into storage we own.
    let stats = unsafe {
        let mut stat = std::mem::zeroed::<libc::statvfs>();
        if libc::statvfs(c_path.as_ptr(), &mut stat) != 0 {
            return None;
        }
        stat
    };
    // `f_bavail`, not `f_bfree`: blocks available to an **unprivileged** writer, which is who this command runs as. The
    // difference on unix is root-reserved blocks; the Windows arm's analogue is quota (see its own note).
    let block = stats.f_frsize;
    let free = stats.f_bavail;
    if block == 0 {
        // A zero block size means the answer is unusable, and reporting "0 bytes free" would refuse every fetch on
        // a filesystem that merely answered oddly.
        return None;
    }
    block.checked_mul(free)
}

/// §12 rule 6's admission check, given what the platform could say about free space.
///
/// Pure **because of how this check was once written**: the decision and the OS query were fused in one inline `if let Some(_)`, so
/// the only test of it asserted a refusal that only a `statvfs` answer can produce, and "the preflight does not happen on Windows"
/// survived two rounds of green gates until a runner existed to say otherwise (F34). Split out, all three outcomes are covered on
/// every host; what stays platform-shaped is the query alone.
fn preflight(needed: u64, available: Option<u64>) -> Result<(), FetchError> {
    match available {
        Some(available) if available < needed => Err(FetchError::DiskShortfall {
            needed,
            available,
            margin: PREFLIGHT_MARGIN_BYTES,
        }),
        // Room enough, by the platform's own answer.
        Some(_) => Ok(()),
        // Cannot say — which is a stated absence in the report, not a pass. Rule 6 is an admission check rather than a promise, and
        // the checks after it (a marker that lives, a target that exists) run regardless.
        None => Ok(()),
    }
}

/// [`available_bytes`] on Windows: free bytes **available to the caller**, the quota-aware number, which is what an unprivileged
/// fetch may actually write. `total_number_of_free_bytes` would over-report it — the same *shape* of choice as this module's unix
/// half preferring `f_bavail` to `f_bfree`, though there the difference is reserved blocks rather than quota.
///
/// Design and the evidence behind each row: `docs/design-f34-windows-disk-preflight.md`.
#[cfg(windows)]
fn available_bytes(path: &Path) -> Option<u64> {
    // Climb to something that exists, for the reason the unix half gives in full: this runs *before* `create_dir_all`, so a first
    // fetch of a workspace is always asking about a directory that is not there yet.
    let mut cursor = path;
    let existing = loop {
        match fs::symlink_metadata(cursor) {
            Ok(_) => break cursor,
            Err(_) => match cursor.parent() {
                Some(parent) if !parent.as_os_str().is_empty() => cursor = parent,
                _ => return None,
            },
        }
    };
    // Ask about the **same spelling every other OS call on this path uses**. `walk::to_open_path` hands the `.part` write and the
    // publish rename a `\\?\` prefix when a long absolute path needs one; querying the bare path here would decline ("not checked")
    // for exactly the workspace whose bytes it then writes. The helper's own condition carries over: the prefix needs an absolute
    // path, so a relative `[paths] data_dir` keeps the plain spelling and may answer `None` — stated, never passed.
    let spelled = PathBuf::from(walk::long_prefix(&existing.to_string_lossy(), true));
    let name = wide_name(spelled.as_os_str())?;
    // Zero-initialised so a failing call that writes nothing cannot be read as data, and so `Some(0)` can only mean "the volume
    // answered zero" — which rule 6 refuses, correctly.
    let mut available: u64 = 0;
    let mut total: u64 = 0;
    let mut total_free: u64 = 0;
    // SAFETY: `name` is a NUL-terminated UTF-16 buffer that outlives the call, and the three out-parameters are `&mut u64`s we own.
    // The return is a `BOOL`, so success is read as non-zero rather than as `1`.
    let ok =
        unsafe { GetDiskFreeSpaceExW(name.as_ptr(), &mut available, &mut total, &mut total_free) };
    if ok != 0 {
        Some(available)
    } else {
        None
    }
}

/// The one kernel32 symbol this module needs. Declared by hand rather than pulled in as a bindings crate — the pattern
/// [`crate::llama_ffi`] documents, and the right trade for a single function: a dependency here would sit in every binary, including
/// the ones that never fetch.
#[cfg(windows)]
#[link(name = "kernel32")]
extern "system" {
    fn GetDiskFreeSpaceExW(
        directory_name: *const u16,
        free_bytes_available_to_caller: *mut u64,
        total_number_of_bytes: *mut u64,
        total_number_of_free_bytes: *mut u64,
    ) -> i32;
}

/// `name` as a NUL-terminated UTF-16 string, or `None` when it holds U+0000.
///
/// The check is load-bearing, not hygiene: `encode_wide` encodes a NUL faithfully and Win32 ends the string there, so without this
/// a name containing one would answer about its **surviving prefix**. A wrong number in a report that reads as a measurement is
/// worse than no number, and it is why the decline arm here is explicit rather than "the call will fail anyway".
#[cfg(windows)]
fn wide_name(name: &std::ffi::OsStr) -> Option<Vec<u16>> {
    use std::os::windows::ffi::OsStrExt;
    let mut units: Vec<u16> = name.encode_wide().collect();
    if units.contains(&0) {
        return None;
    }
    units.push(0);
    Some(units)
}

/// [`available_bytes`] where the platform has no answer at all.
///
/// Gated on `all(not(unix), not(windows))` rather than `not(unix)` for the reason this repository already states at
/// [`crate::path_keys`]: the Windows body needs `std::os::windows::ffi`, which exists on no other target, so a wider gate would not
/// be a fallback — it would be a build failure on any target in between.
#[cfg(all(not(unix), not(windows)))]
fn available_bytes(_path: &Path) -> Option<u64> {
    None
}

/// Bytes read per iteration while streaming.
///
/// 1 MiB: large enough that a 310 MB artifact is ~300 iterations rather than 200,000 syscalls' worth of 16 KiB reads,
/// small enough that the fetcher's footprint stays negligible next to the model it is downloading. Not a tuned number —
/// if a later round measures one that matters, it replaces this sentence.
const READ_BYTES: usize = 1024 * 1024;

/// A transport's own summary of why it failed, for [`FetchError::Transport`].
///
/// Lives here (rather than as a `String` field every transport invents) so the "classes only" rule has one place to be
/// enforced. The default is deliberately generic: an unknown transport's detail may name a host, and this program must
/// not repeat it.
pub trait FailureClass {
    /// A class suitable for a report.
    fn class(&self) -> String;
}

impl FailureClass for FetchError {
    fn class(&self) -> String {
        match self {
            FetchError::Transport { class, .. } => class.clone(),
            FetchError::Io { source, .. } => format!("{:?}", source.kind()),
            _ => "refused".to_string(),
        }
    }
}

/// Summarise a transport's own error text into a class that is safe to print (§12 rule 8).
///
/// **This function never echoes its input, and that is its whole contract.** HTTP client errors read like
/// `https://mirror.example/models/main.gguf: status code 404`, and §12 rule 8 says a host and a full URL do not belong
/// in a report that lands in CI logs — so the branches below return **fixed strings** chosen from what the text says,
/// never the text itself. A caller that wants detail logs it where the human who can reach the host will look; this is
/// the program's public sentence about the failure.
///
/// The classes are deliberately coarse: "not found" and "forbidden" change what a human does next (fix the member name,
/// or fix the mirror's ACL), while "server error" and "transport failure" both mean try again. Distinguishing a 500
/// from a 503 would buy nothing and cost a number in a message that must stay class-shaped.
pub fn failure_class(raw: &str) -> String {
    let lower = raw.to_ascii_lowercase();
    // Status codes are matched as words with their neighbours, because a URL can contain `404` as part of a path or
    // port and echo it back inside a class name would be exactly the leak this function exists to prevent.
    if lower.contains("status code 404")
        || lower.contains("404 not found")
        || lower.contains("not found")
    {
        "not found".to_string()
    } else if lower.contains("status code 401")
        || lower.contains("status code 403")
        || lower.contains("unauthor")
    {
        "refused by the server (authentication or permissions)".to_string()
    } else if lower.contains("status code 5") {
        "server error".to_string()
    } else if lower.contains("redirect") {
        // Not followed silently: a redirect that changes host is a different origin than the one config named.
        "redirect refused".to_string()
    } else if lower.contains("dns")
        || lower.contains("resolve")
        || lower.contains("name or service not known")
    {
        "could not resolve the configured host".to_string()
    } else if lower.contains("timed out") || lower.contains("timeout") {
        "timed out".to_string()
    } else if lower.contains("tls") || lower.contains("certificate") || lower.contains("handshake")
    {
        "TLS failure".to_string()
    } else {
        "transport failure".to_string()
    }
}

/// The origin a fetch dials, from whatever `[models] base_url` resolved to (§12 rule 7 as amended 2026-11-04).
///
/// **One question, one answer, in one place**: the configured value if there is one, else
/// [`crate::pins::DEFAULT_ORIGIN`]. Config has already merged its two sources before a caller reaches here — the file's
/// own `base_url` wins, and `NP_MODELS_BASE_URL` fills a key the file left unset, which is `crate::config`'s precedence
/// for every key in that section (`tests/config.rs` pins it) — so this function deliberately does not read the
/// environment itself. A second reader of `NP_MODELS_BASE_URL` would be a second answer to "which origin did this run
/// dial", and only one of them could be right.
///
/// The default is a fallback, never an override: the operator who mirrors these artifacts changes one config line and
/// gets their own origin here, while acceptance stays the digest's decision either way.
pub fn resolve_origin(configured: Option<&str>) -> &str {
    configured.unwrap_or(crate::pins::DEFAULT_ORIGIN)
}

/// The member names this build pins for `id`, in [`crate::pins::PINNED`] order — what a caller passes as `fetch`'s
/// `requested`, and what `model_files` should hold after a fetch of the whole pair.
pub fn pinned_member_names(id: &str) -> Vec<&'static str> {
    pins::pinned(id).map(|member| member.member).collect()
}

/// Every distinct id this build can fetch — the pin table's, and nothing else's (§12 rule 7: the binary decides).
pub fn fetchable_ids() -> Vec<&'static str> {
    let mut ids = BTreeSet::new();
    for pinned in crate::pins::PINNED {
        ids.insert(pinned.id);
    }
    ids.into_iter().collect()
}

/// The primary member's name, spelled once ([`MAIN_FILE`]) so a fetch and an install cannot record the same artifact
/// under two names.
pub const PRIMARY_MEMBER: &str = MAIN_FILE;

/// The production transport: plain HTTP GETs against an origin the operator configured.
///
/// **Lives behind cargo feature `fetch`, which is default-on** (§12's premises: a fetch path release builds compile
/// out would defeat the amendment; opting out is an explicit `--no-default-features`). It is also the only place in
/// the workspace that names a network client, and it holds one string — the origin — which arrives from
/// `[models] base_url`, `NP_MODELS_BASE_URL`, or [`crate::pins::DEFAULT_ORIGIN`] via [`resolve_origin`]. Nothing here
/// knows or cares which host that is; §12 rule 7 keeps the accepted bytes in the binary and the origin in
/// config-plus-default so a mirror is an edit, not a rebuild.
///
/// # The URL shape, and why the model id is not in it
///
/// `{base_url}/{filename}`, one path segment per member. The id is deliberately absent: [`crate::pins`] already names
/// each member's file (`embeddinggemma-2-Q8_0.gguf`, `mmproj-embeddinggemma-2-Q8_0.gguf`), so a mirror lays files out
/// however it likes — including under a path like `/ggml-org/embeddinggemma-2-GGUF/resolve/main`, which is what an
/// origin has to look like for the upstream layout — and this module never assembles a repository path from an id,
/// which would be one more way for a name from argv to reach a request line.
#[cfg(feature = "fetch")]
pub struct HttpTransport {
    agent: ureq::Agent,
    base_url: String,
}

#[cfg(feature = "fetch")]
impl HttpTransport {
    /// Dial the origin [`resolve_origin`] settled on.
    ///
    /// The value is assumed already validated by [`crate::config`] (scheme, authority, no credentials, one trailing
    /// `/` removed). This does not re-validate: a second parser would be a second opinion about what a valid origin
    /// is, and the config layer's refusal is the one the human sees.
    pub fn new(base_url: impl Into<String>) -> Self {
        HttpTransport {
            // Timeouts exist because a fetch runs in front of a person. `timeout_read` bounds **one read**, not the
            // transfer, so a nine-hundred-megabyte download over a slow link is not cut off — while a server that
            // accepts the connection and then stops answering is reported instead of hanging until the terminal is
            // closed. These two numbers are an operator-UX choice, not a measurement.
            agent: ureq::AgentBuilder::new()
                .timeout_connect(std::time::Duration::from_secs(15))
                .timeout_read(std::time::Duration::from_secs(60))
                .build(),
            base_url: base_url.into(),
        }
    }

    /// Where a member's bytes are asked for. Public so a report can say *which file* was requested without printing
    /// the origin — the caller composes `filename` and this module keeps the host to itself.
    pub fn url_for(&self, member: &dyn MemberPin) -> String {
        format!("{}/{}", self.base_url, member.filename())
    }
}

#[cfg(feature = "fetch")]
impl Transport for HttpTransport {
    fn open(&self, member: &dyn MemberPin) -> Result<Body, FetchError> {
        let url = self.url_for(member);
        match self.agent.get(&url).call() {
            Ok(response) => {
                // `Content-Length` is the only size hint that means anything here. A chunked transfer omits it and
                // §12 rule 12 wants "size unknown", never an invented denominator — and a `transfer-encoding: chunked`
                // response decoded by the client would otherwise tempt us to report the decoded length as if the
                // server had announced it.
                let bytes_total = response
                    .header("content-length")
                    .and_then(|value| value.trim().parse::<u64>().ok());
                Ok(Body {
                    reader: Box::new(response.into_reader()),
                    bytes_total,
                })
            }
            // The class is the only thing that leaves this function. `ureq`'s own text names the URL and sometimes
            // the resolved address; §12 rule 8 keeps both out of a report that lands in CI logs.
            Err(err) => Err(FetchError::Transport {
                id: member.id().to_string(),
                member: member.member().to_string(),
                class: http_class(&err),
            }),
        }
    }
}

/// Map a client error onto one of [`failure_class`]'s classes.
///
/// Redirects are **followed** (`ureq`'s default, up to five hops) and that is safe here for a reason worth writing
/// down: the digest pin decides acceptance, so a hop that changes host cannot substitute bytes this build would
/// accept — it can only waste bandwidth. Refusing every redirect instead would break the ordinary upstream layout,
/// where `/resolve/main/<file>` answers 302 pointing at a CDN. The `"redirect refused"` class in
/// [`failure_class`] therefore stays defensive (a client that surfaces a hop as an error still gets a class), and no
/// test claims it fires on this path, because on this path it does not.
#[cfg(feature = "fetch")]
fn http_class(err: &ureq::Error) -> String {
    match err {
        ureq::Error::Status(code, _) => failure_class(&format!("status code {code}")),
        ureq::Error::Transport(transport) => failure_class(&transport.to_string()),
    }
}

/// What `--discard-marker` did (§12 rule 4: the explicit door, and the only one).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Discarded {
    /// Whether a marker file was removed. `false` means there was nothing to discard, which is a fact worth printing
    /// rather than a silent success — the human ran the flag because they believed a fetch was stuck.
    pub marker_removed: bool,
    /// The `.part` files removed alongside it, named so a report can say what disappeared. Only the part **this**
    /// marker names is touched: another id's partial download, or a part whose nonce no marker claims, is left for
    /// the listing to report rather than deleted on a guess.
    pub parts_removed: Vec<PathBuf>,
}

/// Remove this workspace's fetch marker for `id`, and the partial download it names.
///
/// **An unreadable marker is still removed.** §12 rule 4's advice is "if no fetch is running, delete this file and
/// re-run", and a marker truncated mid-write is exactly the case where the human knows best whether a fetch is
/// running — refusing to help because *this build* cannot parse the file would make the door unusable in the one
/// situation it exists for. The part file, by contrast, can only be named if the marker could be read, so an
/// unreadable marker discards the marker alone and leaves its bytes for `list` to report.
pub fn discard_marker(models_dir: &Path, id: &str) -> Result<Discarded, FetchError> {
    let path = models::marker_path(models_dir, id);
    // Read the nonce **before** removing anything: after the unlink there is nothing left to ask.
    let nonce = match models::fetch_state(models_dir, id) {
        FetchState::Present(fields) => Some(fields.nonce),
        _ => None,
    };
    let mut parts_removed = Vec::new();
    if let Some(nonce) = nonce {
        let part = models_dir.join(format!(".part.{id}.{nonce}"));
        if fs::symlink_metadata(&part).is_ok() {
            fs::remove_file(&part).map_err(|source| FetchError::Io {
                path: part.clone(),
                source,
            })?;
            parts_removed.push(part);
        }
    }
    let marker_removed = match fs::remove_file(&path) {
        Ok(()) => true,
        // "No marker to discard" is not a failure: the flag says *if one exists*, and running it twice must not
        // produce a different exit code than running it once.
        Err(source) if source.kind() == io::ErrorKind::NotFound => false,
        Err(source) => return Err(FetchError::Io { path, source }),
    };
    Ok(Discarded {
        marker_removed,
        parts_removed,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The three arms of rule 6, asserted directly rather than through an OS query — which is the point of extracting
    /// [`preflight`]: on a Linux runner these are the arms a Windows-only `available_bytes` can never reach.
    #[test]
    fn preflight_arms_are_the_three_outcomes() {
        // Too little room: the refusal carries both numbers and the margin, so the arithmetic stays checkable by a reader.
        let err = preflight(1_000, Some(999)).expect_err("999 < 1000 must refuse");
        match err {
            FetchError::DiskShortfall {
                needed,
                available,
                margin,
            } => {
                assert_eq!((needed, available), (1_000, 999));
                assert_eq!(margin, PREFLIGHT_MARGIN_BYTES);
            }
            other => panic!("expected DiskShortfall, got {other:?}"),
        }
        // A volume that answers zero is refused, not excused: the answer arrived, it simply was not a welcome one.
        assert!(matches!(
            preflight(1, Some(0)),
            Err(FetchError::DiskShortfall { available: 0, .. })
        ));
        assert!(preflight(1_000, Some(1_000)).is_ok());
        assert!(preflight(u64::MAX, Some(u64::MAX)).is_ok());
        // Cannot say is the third outcome, and it is not a shortfall. The report's "not checked on this platform" line is
        // what makes this honest; a silent pass here would be the defect F34 was filed for, one level down.
        assert!(preflight(u64::MAX, None).is_ok());
    }

    /// The decline arm, reachable on **every** host: a relative path whose every ancestor is absent runs the climb out
    /// before any OS call, so this cannot drift into "answered something about a different directory".
    #[test]
    fn available_bytes_declines_when_no_ancestor_exists() {
        let orphan = Path::new("f34-no-such-root").join("models");
        assert_eq!(
            available_bytes(&orphan),
            None,
            "a path with no existing ancestor must decline rather than guess"
        );
    }

    /// A real directory answers on the platform running this test — and on Windows that is the first executed proof that
    /// the hand-written `GetDiskFreeSpaceExW` binding links, succeeds, and writes an out-parameter.
    #[cfg(windows)]
    #[test]
    fn available_bytes_answers_a_plausible_number_for_an_existing_directory() {
        let got = available_bytes(&std::env::temp_dir());
        assert!(
            matches!(got, Some(n) if n > 1_048_576),
            "a CI runner's temp filesystem must report more than a megabyte free: {got:?}"
        );
    }

    /// **The only check that pins parameter order.** Every other test here reads one number, so swapping
    /// `free_bytes_available_to_caller` and `total_number_of_free_bytes` would pass them all while over-reporting what the
    /// fetch may write — which is the exact distinction this arm argues for.
    #[cfg(windows)]
    #[test]
    fn the_three_out_parameters_keep_their_order() {
        let dir = std::env::temp_dir();
        let name = wide_name(dir.as_os_str()).expect("a temp dir path encodes");
        let mut available: u64 = 0;
        let mut total: u64 = 0;
        let mut total_free: u64 = 0;
        // SAFETY: `name` is a NUL-terminated UTF-16 buffer we own and outlive it here; the out-parameters are ours too.
        let ok = unsafe {
            GetDiskFreeSpaceExW(name.as_ptr(), &mut available, &mut total, &mut total_free)
        };
        assert_ne!(ok, 0, "GetDiskFreeSpaceExW failed for {}", dir.display());
        assert!(
            available <= total_free,
            "caller-available {available} exceeds total-free {total_free}: the free out-parameters are swapped"
        );
        assert!(
            total_free <= total,
            "total-free {total_free} exceeds total {total}: the size out-parameters are swapped"
        );
        assert!(
            total > 0,
            "a volume that reports zero total is not a volume"
        );
    }

    /// The climb this whole arm depends on: the preflight runs before `create_dir_all`, so a first fetch always asks about
    /// a directory that does not exist yet.
    #[cfg(windows)]
    #[test]
    fn available_bytes_climbs_out_of_a_directory_that_does_not_exist_yet() {
        let missing = std::env::temp_dir().join("f34-absent").join("models");
        assert!(
            available_bytes(&missing).is_some(),
            "a first fetch must get an answer from the filesystem it is about to write into"
        );
    }

    /// The encoder's zero-unit check fires instead of asking Win32 about a truncated prefix.
    #[cfg(windows)]
    #[test]
    fn available_bytes_declines_for_a_name_holding_an_interior_nul() {
        // Deterministic by construction: the decline happens in `wide_name`, before any call, so it cannot depend on how
        // Win32 chooses to treat a name that cannot exist.
        assert_eq!(available_bytes(Path::new("\u{0}f34")), None);
        assert_eq!(wide_name(std::ffi::OsStr::new("a\u{0}b")), None);
    }

    /// Parity for the unix half's own guard: an unencodable name declines rather than answering about something else.
    #[cfg(unix)]
    #[test]
    fn the_unix_arm_declines_a_name_it_cannot_encode() {
        // Interior NUL cannot be named on unix either — `symlink_metadata` refuses it first, so what this pins is the
        // *outcome* both halves agree on for a path that has an existing ancestor: it answers about that ancestor, and never
        // invents a decline. Recorded here because §4 of the F34 design claims exactly this asymmetry.
        let tmp = std::env::temp_dir();
        assert!(available_bytes(&tmp).is_some());
    }
}
