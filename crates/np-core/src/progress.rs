//! The vocabulary of "what is np doing right now", for a caller that wants to answer that question without
//! parsing prose.
//!
//! Kept deliberately small and deliberately JSON-free: `np-core` has no `serde_json` dependency, so an event
//! crossing this seam is plain Rust and the phase→key mapping belongs to whoever prints it (today: `np serve`).

/// Which stretch of work a reporter is inside. Names are stable: `np serve` maps each one to a key in an
/// `np/progress` notification, and a client branches on the spelling, so renaming one is a protocol change.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum Phase {
    /// Deciding which provider, artifact or workspace this work belongs to. The default because it is where every
    /// request starts — and because a reporter that never says otherwise is claiming it is still deciding, which is
    /// true of a half-written caller rather than false.
    #[default]
    Resolving,
    /// Waiting for the F44 workspace write lock.
    Locking,
    /// Walking and parsing the vault.
    Indexing,
    /// Transferring pinned bytes. Carries `member`, so a two-member fetch reads as two runs of bytes rather than one
    /// number that falls backwards.
    Fetching,
    /// Encoding chunks.
    Embedding,
    /// The closing write: a batch into the database, or the catalog onto disk.
    Writing,
}

/// One checkpoint of one running command. Every counter is what the reporter already knew at that moment; nothing
/// here is derived, because a number computed from an estimate is how a progress bar ends up lying.
#[derive(Debug, Clone, Default)]
pub struct ProgressEvent<'a> {
    /// The phase this event belongs to.
    pub phase: Phase,
    /// Which artifact of the model a `Fetching` event counts bytes for — `"main"` or `"mmproj"`, spelled as the pin table
    /// spells them (`models::MAIN_FILE`, `pins::MMPROJ_MEMBER`). Absent means the
    /// phase has no members, not that the member is unknown.
    pub member: Option<&'a str>,
    /// Bytes written for `member` so far.
    pub bytes_done: u64,
    /// The **pinned** size of `member`. Deliberately not the size the transport announced (**§12 rule 12**): a chunked
    /// transfer announces nothing, and two denominators behind one key is how a client renders two different
    /// percentages from the same field. Absent means this build has no total to offer.
    pub bytes_total: Option<u64>,
    /// Members finished before this one.
    pub members_done: usize,
    /// Members in this call, so a client can say "2 of 2" without knowing the member names.
    pub members_total: usize,
    /// Chunks seen by the embed pass so far. No total travels with it unless one is cheap — see the spec's
    /// "left to the builder": an absent total means unknown, and unknown is not zero.
    pub chunks_done: usize,
    /// Chunks whose vectors are committed.
    pub embedded: usize,
}

/// The seam a long-running command reports through.
///
/// A reporter is called on the thread doing the work, so there is no `Sync` bound: the honest implementation writes to
/// a locked stdout handle, which cannot be shared anyway. A caller that ever wants to report from another thread wraps
/// its sink in something that adds the synchronization itself.
///
/// **Implementations must not block and must not panic.** This is called inline — inside the transfer loop, between
/// batches — so a reporter that takes a lock someone else holds stalls the work it is reporting on, and a reporter
/// that panics would turn a status line into a failed command. The server's reporter serialises and writes; if the
/// client has gone, it counts the failure and says nothing.
///
/// Reporting is not accounting: a command reports where it already computed numbers for its own report, which is why
/// the fetch hook sits beside the marker write rather than on a timer of its own.
pub trait Progress {
    /// Report one checkpoint.
    fn report(&self, event: &ProgressEvent<'_>);
}

/// The reporter that does nothing — what every CLI path passes, so `np scan --json` keeps printing exactly what it
/// printed before this seam existed. A no-op has to be a value rather than an associated constant because a trait
/// object cannot live in one.
#[derive(Debug, Default)]
struct Noop;

impl Progress for Noop {
    fn report(&self, _event: &ProgressEvent<'_>) {}
}

/// The shared no-op reporter.
pub fn none() -> &'static dyn Progress {
    &Noop
}

#[cfg(test)]
mod tests {
    use super::{none, Phase, ProgressEvent};

    #[test]
    fn an_event_carries_only_what_the_reporter_knows() {
        let event = ProgressEvent {
            phase: Phase::Fetching,
            bytes_done: 4096,
            ..Default::default()
        };
        assert_eq!(event.phase, Phase::Fetching);
        assert_eq!(event.bytes_done, 4096);
        // Absent means unknown, and unknown is not zero: a total nobody announced must not read as an empty file.
        assert_eq!(event.bytes_total, None);
        assert_eq!(event.member, None);
    }

    #[test]
    fn the_noop_reporter_accepts_an_event_and_returns() {
        let event = ProgressEvent {
            phase: Phase::Embedding,
            chunks_done: 7,
            embedded: 3,
            ..Default::default()
        };
        none().report(&event);
    }
}
