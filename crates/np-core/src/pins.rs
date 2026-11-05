//! The pin table: **what a fetched artifact has to be**. No row says where it comes from; one module-level default
//! origin does, on purpose and since 2026-11-04 (`docs/design-m2-local-embeddings.md` §12 rule 7 as amended — the two
//! sections below are the argument for each half).
//!
//! This module is data: nothing here opens a file, names a client, or reaches for a network type. The one `cfg` it
//! carries selects which members a fetch **volunteers** for ([`members_for_build`]) and never which digests exist —
//! every row of [`PINNED`] compiles into every build, because a pin a release build could compile out would pin
//! nothing (§12's premises).
//!
//! ## Why no field names a server
//!
//! The split is deliberate and load-bearing: **a [`PinnedModel`] says what to accept; the host is config**
//! (`[models] base_url`, plus `NP_MODELS_BASE_URL`, in [`crate::config`]) so that a mirror works and the origin is
//! not a compile-time secret. Keep the **rows** that way, and do not "fix" a row's missing URL by adding one — the
//! module-level default below is a separate decision, made deliberately on 2026-11-04: **a public
//! artifact host is not the secret class the parent design's §9 protects** — that section guards the self-hosted
//! chat server's URL and key — so writing a host here buys no secrecy while costing three things the pin table
//! exists to give: mirror-ability, one reviewable list of accepted bytes, and a build that refuses an upstream
//! re-upload instead of following it. §12 rule 8 keeps the other half of the boundary (a *refusal* names a class,
//! never a URL), and `tests/pins.rs` turns both halves into assertions so a future edit has to be deliberate.
//!
//! ## What changed on 2026-11-04: one origin, deliberately
//!
//! The argument above is why there is still no host in any **row**, and it stands. What changed is that one origin is
//! now compiled in beside the table ([`DEFAULT_ORIGIN`]) — decided by the human in the R7c scope decisions recorded in
//! `docs/design-m2-image-embeddings.md` §13, carried into `docs/design-m2-local-embeddings.md` §12 rule 7. The reason
//! is practical rather than theoretical: the documented install step became "place the binary, then run
//! `np models fetch`", and an install that cannot reach a network is an install nobody completes — while at that
//! moment there is no config file yet to read.
//!
//! Three things the amendment does **not** touch, so that nobody restores them by accident:
//!
//! * the digest still decides acceptance, so a different host serving the same bytes is indistinguishable from this
//!   one — which is what makes a baked default safe rather than merely convenient;
//! * a mirror stays one config line away (`[models] base_url`, or `NP_MODELS_BASE_URL` when the file leaves that key
//!   out). `[models] sha256` is read and validated but **no fetch reads it yet** — the door for an unpinned artifact is
//!   `np models install --sha256`; see [`crate::config::ModelsConfig::unpinned_digest_override`].
//!   [`crate::fetch::resolve_origin`] is the whole
//!   precedence, in one place;
//! * rule 8 still forbids this string in every refusal and report line — `tests/fetch.rs` carries the baked origin's
//!   own host on its no-leak list.
//!
//! ## Where the second member's number came from, and why the design's number is not used
//!
//! §12 rule 7 records the mmproj digest as `c4a8a526…`, "from public LFS metadata 2026-10-29". **That prefix belongs
//! to no file in that repository.** On 2026-11-02 the artifact was downloaded and hashed here, and its own metadata
//! agreed byte for byte — see [`PINNED`]'s second row for the value and its provenance. The design's eight characters
//! are kept out of this table on purpose: a digest nobody can reproduce is not an expectation, it is a permanent
//! refusal waiting to fire against good bytes. `docs/design-m2-local-embeddings.md` §12 carries the correction.

/// One accepted file, named by its member role inside a model id's set of files.
///
/// Every field describes bytes this build will accept — never a place to get them from (module docs explain
/// why that boundary is the point of the type).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct PinnedModel {
    /// The model id this member belongs to, in `models.id`'s spelling (`docs/design-m2-models-install.md` §2).
    /// Both members of one pair carry the same id: §12 rule 7 pins a **pair**, not two models.
    pub id: &'static str,
    /// Which file of that id this row is — `main` is the primary artifact whose digest stays in `models.sha256`,
    /// and the name is what §12 rule 10's child table keys on (`model_files(model, name)`), so a fetch and an
    /// install can record the same fact under one name.
    pub member: &'static str,
    /// The file's own name, as the artifact is published. A **name**, not a path: it carries no directory
    /// separator and no host, which `tests/pins.rs` asserts because a `/` here would be a *where* smuggled into
    /// the *what*.
    pub filename: &'static str,
    /// Exact byte length a fetched member must have. §12 rule 6's disk preflight sums these over a whole id —
    /// see [`total_size`] for what this table can and cannot sum today.
    pub size_bytes: u64,
    /// The sha256 the bytes must hash to, lowercase hex as [`crate::hash::Sha256Stream`] spells it. Compared in
    /// full; a prefix is never enough (§12 rule 7's whole claim is that nothing but this number is trusted).
    pub sha256: &'static str,
    /// Provenance for the two numbers above: where and when they were read. Pin rot is real — nobody notices an
    /// upstream re-upload until first start refuses — so a refusal has to be able to say how old the expectation
    /// is without anyone re-deriving it from a design document.
    pub read_on: &'static str,
}

/// Every artifact this build will accept, in id order.
///
/// One entry per member, so an id with two files appears twice with the same [`PinnedModel::id`]. Adding a row
/// here is the only way to teach this build a new artifact, and `tests/pins.rs` refuses rows whose digest is not
/// 64 lowercase hex characters or whose size is zero — a truncated paste and an unknown length are both refusals
/// waiting to be misread as "the download was corrupted".
const MAIN: PinnedModel = PinnedModel {
    id: "embeddinggemma-2",
    member: crate::models::MAIN_FILE,
    filename: "embeddinggemma-2-Q8_0.gguf",
    // Both numbers come from `docs/design-m2-local-embeddings.md` §3 ("The artifact's own header, measured",
    // R6b1, 2026-10-30): the size is the file's length on disk and the digest is sha256 over every one of those
    // bytes, read from the artifact itself rather than from a model card. Re-measured on this host on 2026-11-01
    // with `np models hash /home/agent/models/embeddinggemma-2-Q8_0.gguf`, which printed
    // `2188ac1deca4b77dffefd603c2776a9d76d9d74ec01841392982ebb840b09135` — the value below, and 64 characters.
    size_bytes: 309_855_456,
    sha256: "2188ac1deca4b77dffefd603c2776a9d76d9d74ec01841392982ebb840b09135",
    read_on: "design-m2-local-embeddings.md §3, measured 2026-10-30",
};

/// The second member of the pair: the projector that carries the image and audio encoders.
///
/// **Measured here, not claimed.** On 2026-11-02 this file was fetched from `ggml-org/embeddinggemma-2-GGUF` into a
/// tmpfs, hashed with GNU coreutils `sha256sum`, and the length read from the file: **554,821,056 bytes**, digest
/// below. The host's own LFS metadata (`GET /api/models/ggml-org/embeddinggemma-2-GGUF/tree/main`) reported the same
/// `oid` in the same minute, which is the cross-check that makes the number usable as a pin rather than as one
/// machine's reading — and it is also how this round discovered that §12's recorded prefix (`c4a8a526…`) matches
/// neither this file nor the BF16 sibling.
///
/// What that leaves honestly unstated: nobody has embedded a note with these bytes. R7c's spike owns whether the
/// encoders do anything, and §5 says so; pinning a file is not a claim about its use.
const MMPROJ: PinnedModel = PinnedModel {
    id: "embeddinggemma-2",
    member: MMPROJ_MEMBER,
    filename: "mmproj-embeddinggemma-2-Q8_0.gguf",
    size_bytes: 554_821_056,
    sha256: "0531309472a6bc6f4b146c3736eee7795ec9db1903c25490d3c937490cef6dd8",
    read_on: "downloaded and hashed on this host 2026-11-02; matches the host's LFS metadata the same day",
};

/// Every artifact this build will accept, in id order.
///
/// One entry per member, so an id with two files appears twice with the same [`PinnedModel::id`]. Adding a row here
/// is the only way to teach this build a new artifact, and `tests/pins.rs` refuses rows whose digest is not 64
/// lowercase hex characters or whose size is zero — a truncated paste and an unknown length are both refusals waiting
/// to be misread as "the download was corrupted".
pub const PINNED: &[PinnedModel] = &[MAIN, MMPROJ];

/// The origin a fetch dials when `[models] base_url` and `NP_MODELS_BASE_URL` both leave the choice unmade.
///
/// **The one host in this module, and it is here on purpose** — see "What changed on 2026-11-04" in the module docs
/// for what that amendment did and did not move. It is a module-level default rather than a row field for exactly
/// that reason: no pin gained a URL, so `tests/pins.rs`'s scan of every row still refuses a host-shaped value, and
/// §12 rule 8 still keeps this string out of every refusal and report line.
///
/// The value is where the pinned digests came from — [`PINNED`]'s second row records downloading that file from this
/// repository on 2026-11-02 — so a default pointing elsewhere would be asking a stranger for bytes this build only
/// ever vouched for here. Shape facts three callers rely on, each asserted in `tests/pins.rs`: an `https` scheme, no
/// credentials in the authority, and no trailing `/`, because [`crate::fetch::HttpTransport`] joins
/// `origin` + `/` + filename.
pub const DEFAULT_ORIGIN: &str =
    "https://huggingface.co/ggml-org/embeddinggemma-2-GGUF/resolve/main";

/// The projector member's role name, spelled once — the [`PINNED`] row and [`members_for_build`]'s vision set are the
/// only two places it is needed.
pub const MMPROJ_MEMBER: &str = "mmproj";

/// The members a fetch plans when the caller named none: **what this binary can use**
/// (`docs/design-m2-image-embeddings.md` §13, decided 2026-11-02).
///
/// A build without `vision` has no code that loads the projector, so planning its 554 MB would be waste dressed as
/// convenience; a vision build needs both halves or it cannot embed an image. The primary member is in the set on
/// every row — an install that fetched no weights would have installed nothing.
///
/// **A default, not a mandate.** `--member` still spells the whole plan, and this function decides only what a fetch
/// volunteers. It never refuses anything either: [`pinned_member`] answers for both rows in every build, so anyone
/// fetching on behalf of a different binary can still name a member this one would not ask for — with one exception,
/// which is np-cli's and lives there: asking a text-only build to *fetch* the projector is refused by name, naming
/// the missing capability, because bytes it can never load are not worth 554 MB of someone's bandwidth.
pub fn members_for_build() -> &'static [&'static str] {
    BUILD_MEMBERS
}

/// The vision set: both halves of the pair, because a projector nobody loads is still 554 MB.
#[cfg(feature = "vision")]
const BUILD_MEMBERS: &[&str] = &[crate::models::MAIN_FILE, MMPROJ_MEMBER];
/// The text-only set: the primary artifact alone — here the projector would be verified and then never read.
#[cfg(not(feature = "vision"))]
const BUILD_MEMBERS: &[&str] = &[crate::models::MAIN_FILE];

/// What a model **declares**, independently of how its bytes arrive (§12 rule 10's identity half).
///
/// `models.dim`, `prefix_doc` and `prefix_query` are the facts that decide whether two vectors are comparable at
/// all, and install design §2 insists they are *declared* rather than probed from a header. A fetch has to produce a
/// row, so it needs the same facts from somewhere, and the only honest source is this table: authored by whoever
/// built the pin, reviewed beside the digests, compiled in so no runtime path can vary it.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct PinnedIdentity {
    /// The model id these declarations belong to.
    pub id: &'static str,
    /// Vector width — the number `embeddings`' `CHECK (length(vector) = 4 * dim)` measures in bytes.
    pub dim: u16,
    /// Text prepended to documents before they reach the model.
    pub prefix_doc: &'static str,
    /// Text prepended to queries. Never used to compare against a stored vector (§3).
    pub prefix_query: &'static str,
}

/// One row per fetchable id. `dim` is measured from the artifact's own header (§3, R6b1); **both prefixes are empty**,
/// and that is a statement about evidence rather than an oversight: no instruction convention has been measured for
/// this model in this repository, and every embedding run recorded here — R6b2's enforcement test through R6d's corpus
/// run — used none. Prefixes change vector bytes, so they are part of an id's identity (`@n`), which means a later
/// round that *measures* a convention must ship it as a **new id** rather than as an edit to these strings.
pub const PINNED_IDENTITIES: &[PinnedIdentity] = &[PinnedIdentity {
    id: "embeddinggemma-2",
    dim: 768,
    prefix_doc: "",
    prefix_query: "",
}];

/// The declared identity of `id`, if this build declares one.
///
/// `None` means a fetch must refuse: bytes with no width to record are not a model, and guessing 768 because it is
/// the only width seen so far is exactly the un-declared identity install design §2 refuses.
pub fn identity(id: &str) -> Option<&'static PinnedIdentity> {
    PINNED_IDENTITIES.iter().find(|entry| entry.id == id)
}

/// Every pinned member of `id`, in [`PINNED`] order.
///
/// An id with no pinned member yields nothing, which is what makes "this build cannot fetch that" a fact a caller
/// can reach without a second lookup table. The items are `'static` because the table is compiled in — the only
/// lifetime the returned iterator holds is `id`'s, which is what the `use<'_>` bound spells out: no lifetime appears
/// in the trait bound above, so the hidden closure's borrow of `id` would not be captured without it.
pub fn pinned(id: &str) -> impl Iterator<Item = &'static PinnedModel> + use<'_> {
    PINNED.iter().filter(move |entry| entry.id == id)
}

/// The pinned member `member` of `id`, if this build pins it at all.
///
/// `None` means "refuse", not "unknown": an unpinned member has no digest to check, and §12 rule 7's guarantee is
/// that nothing without one is accepted. This is the arm a fetch reaches for a member name nobody read a digest for.
pub fn pinned_member(id: &str, member: &str) -> Option<&'static PinnedModel> {
    pinned(id).find(|entry| entry.member == member)
}

/// What a fetch of `id` must have room for: the sum of [`PinnedModel::size_bytes`] over every pinned member of that
/// id, which is §12 rule 6's disk preflight ("summed over **both** files — a factor of 2.8 against guessing one").
///
/// For `embeddinggemma-2` this is both files of the pair — **864,676,512 bytes**, which is §5's "864.7 MB" seen from
/// the other end, and the reason rule 6's preflight insists on summing members rather than trusting one: guessing a
/// single file understates the transfer by a factor of 2.8.
///
/// An unknown id sums to `0`, which is correct arithmetic for "nothing is pinned" and must not be read as "no disk
/// needed": a caller that has already resolved an id checks [`pinned`] is non-empty before asking this.
///
/// (`tests/pins.rs` pins the 864,676,512 figure by name, so adding or removing a member has to be a decision made
/// twice — here and in the test that quotes §5.)
pub fn total_size(id: &str) -> u64 {
    pinned(id).map(|entry| entry.size_bytes).sum()
}
