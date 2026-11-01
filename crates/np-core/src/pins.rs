//! The pin table: **what a fetched artifact has to be**, with no statement about where it comes from
//! (`docs/design-m2-local-embeddings.md` §12 rule 7).
//!
//! This module is data, so it has no dependency and no feature gate: nothing here opens a file, names a
//! host, or reaches for a client. The fetch engine that reads it arrives in a later stage of R7b and lives
//! behind its own cargo feature; the table it consults is compiled into every build, because a pin a release
//! build could compile out would pin nothing (§12's premises).
//!
//! ## Why no field names a server
//!
//! The split is deliberate and load-bearing: **a [`PinnedModel`] says what to accept; the host is config**
//! (`[models] base_url`, plus `NP_MODELS_BASE_URL`, in [`crate::config`]) so that a mirror works and the origin is
//! not a compile-time secret. Keep it that way, and do not "fix" the missing URL by adding one: **a public
//! artifact host is not the secret class the parent design's §9 protects** — that section guards the self-hosted
//! chat server's URL and key — so writing a host here buys no secrecy while costing three things the pin table
//! exists to give: mirror-ability, one reviewable list of accepted bytes, and a build that refuses an upstream
//! re-upload instead of following it. §12 rule 8 keeps the other half of the boundary (a *refusal* names a class,
//! never a URL), and `tests/pins.rs` turns both halves into assertions so a future edit has to be deliberate.
//!
//! ## What is not here yet
//!
//! Only one member of the `embeddinggemma-2` pair is pinned. See [`MMPROJ_DIGEST_PREFIX_ONLY`] for why the
//! second one is absent rather than guessed, and what that costs [`total_size`].

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
pub const PINNED: &[PinnedModel] = &[PinnedModel {
    id: "embeddinggemma-2",
    member: "main",
    filename: "embeddinggemma-2-Q8_0.gguf",
    // Both numbers come from `docs/design-m2-local-embeddings.md` §3 ("The artifact's own header, measured",
    // R6b1, 2026-10-30): the size is the file's length on disk and the digest is sha256 over every one of those
    // bytes, read from the artifact itself rather than from a model card. Re-measured on this host on 2026-11-01
    // with `np models hash /home/agent/models/embeddinggemma-2-Q8_0.gguf`, which printed
    // `2188ac1deca4b77dffefd603c2776a9d76d9d74ec01841392982ebb840b09135` — the value below, and 64 characters.
    size_bytes: 309_855_456,
    sha256: "2188ac1deca4b77dffefd603c2776a9d76d9d74ec01841392982ebb840b09135",
    read_on: "design-m2-local-embeddings.md §3, measured 2026-10-30",
}];

/// TODO(R7b): the mmproj half of the `embeddinggemma-2` pair is **not pinned**, and this is the gap on record.
///
/// §12 rule 7 names its digest as `c4a8a526…` — eight characters, "from public LFS metadata 2026-10-29" — and §5
/// gives its name as `mmproj-embeddinggemma-2-Q8_0.gguf` at "554.8 MB". Nothing in this repository or on this host
/// holds the remaining 56 hex digits or the exact byte length: the file itself is not here (`/home/agent/models`
/// carries the text artifact alone), and no fetch has ever run. Writing a plausible 64-character value would be
/// the worst available outcome — a digest nobody can check is indistinguishable from a real one, and it is compared
/// against 310 MB of bytes as though it were provenance. So this constant carries **only the prefix §12 records**,
/// labelled as unverified, and `PINNED` carries no `mmproj` member: a fetch that cannot pin a member must refuse it,
/// not improvise one.
///
/// **This is not a digest and must never be compared as one.** Landing the real number means replacing this
/// constant with a second [`PinnedModel`] row — and `tests/pins.rs`'s gap test fails when the member arrives, which
/// is what forces whoever adds it to delete the fence on purpose rather than by accident.
///
/// Until then [`total_size`] understates §12 rule 6's preflight for this id, and that is the cost of the gap: see
/// its doc.
pub const MMPROJ_DIGEST_PREFIX_ONLY: &str = "c4a8a526";

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
/// that nothing without one is accepted — which is exactly the state [`MMPROJ_DIGEST_PREFIX_ONLY`] records.
pub fn pinned_member(id: &str, member: &str) -> Option<&'static PinnedModel> {
    pinned(id).find(|entry| entry.member == member)
}

/// What a fetch of `id` must have room for: the sum of [`PinnedModel::size_bytes`] over every pinned member of that
/// id, which is §12 rule 6's disk preflight ("summed over **both** files — a factor of 2.8 against guessing one").
///
/// **Today this table carries one member for `embeddinggemma-2`, so the sum covers only the primary artifact.**
/// That is [`MMPROJ_DIGEST_PREFIX_ONLY`]'s cost, stated where a caller will read it: the real transfer needs room for
/// both files, so a preflight built on this number alone admits a fetch that cannot finish. The honest fix is the
/// missing digest, not a magic constant added here — an invented size would make the sum look authoritative while
/// being exactly as unknown as the digest it would have to travel with.
///
/// An unknown id sums to `0`, which is correct arithmetic for "nothing is pinned" and must not be read as "no disk
/// needed": a caller that has already resolved an id checks [`pinned`] is non-empty before asking this.
pub fn total_size(id: &str) -> u64 {
    pinned(id).map(|entry| entry.size_bytes).sum()
}
