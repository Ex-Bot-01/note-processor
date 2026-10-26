//! The contention harness shared by the two binaries that prove the busy timeout works.
//!
//! `db.rs` asks the question of a workspace database (`Db`) and `config.rs` of the registry
//! (`Connection` + [`np_core::config::WorkspaceRegistry`]); the handle type and the statement are all
//! that differ. The handoff, the hold times and the two timings live here once so a change to the
//! timeout moves both binaries together.

use std::sync::mpsc;
use std::thread;
use std::time::{Duration, Instant};

/// How long the first connection holds its write transaction while the second one waits, in
/// milliseconds. Comfortably inside this crate's busy timeout (`BUSY_TIMEOUT_MS` is 1800 ms, see
/// [`np_core::db`]), so a write that waits has to win; short enough that a lock nobody releases fails
/// the test rather than hanging it.
pub const HOLD_MS: u64 = 40;

/// A hold time past that same 1800 ms and still far below the 5000 ms rusqlite installs on every
/// connection it opens of its own (`rusqlite-0.40.2/src/inner_connection.rs:118`). The gap between the
/// two is what keeps a test using this value a discriminator rather than a tautology: at 3000 ms a
/// connection with this crate's timeout has already given up, while one left on the driver default
/// would still have been waiting and would have succeeded.
pub const HOLD_PAST_TIMEOUT_MS: u64 = 3000;

/// One contended write: whatever the call returned, plus two timings that answer different questions.
pub struct Contention<T> {
    /// The outcome of the write that met the lock.
    pub outcome: T,
    /// From just *before* the writer announced itself to just after the call returned. A lower bound on
    /// it is safe against scheduling: time the writer spends waiting to be reached counts here, so a
    /// loaded runner can inflate this figure but cannot shrink it below the hold.
    pub waited: Duration,
    /// Time spent strictly inside the contended call — what SQLite itself waited or refused within.
    /// Only upper bounds belong on it ("it refused at once", "it gave up near our timeout"), because
    /// that is the one window a deschedule cannot be blamed for in advance.
    pub inside_call: Duration,
}

/// Run `work` on a second thread while the write lock is held, sleeping `hold` and then calling
/// `release` to hand it back.
///
/// The contended call cannot run on this thread: only this thread can release the lock it is blocked
/// on, so whatever it needs travels to the writer and comes back inside `T`.
pub fn contended<T, W, R>(hold: Duration, work: W, release: R) -> Contention<T>
where
    T: Send + 'static,
    W: FnOnce() -> T + Send + 'static,
    R: FnOnce(),
{
    let (tx, rx) = mpsc::channel();
    let writer = thread::spawn(move || {
        // The timestamp is taken *before* the announcement on purpose. Signalling first would let a
        // deschedule between the signal and the call eat part of the hold unnoticed, and a test
        // asserting "it waited" would then fail on a busy machine rather than on the code. With the
        // order reversed, that delay lands inside `waited`, and the release cannot happen before the
        // announcement the writer has already made.
        let started = Instant::now();
        tx.send(()).expect("test: the writer reached its statement");
        let entered = Instant::now();
        let outcome = work();
        Contention {
            outcome,
            waited: started.elapsed(),
            inside_call: entered.elapsed(),
        }
    });

    rx.recv()
        .expect("test: the writer announced its contended call");
    thread::sleep(hold);
    release();
    writer.join().expect("test: writer thread")
}

/// Take the write lock on a handle that is already open: begin a transaction and write one row inside
/// it, so the lock is a real write lock rather than a read snapshot. `exec` runs one statement on the
/// handle, which is all either caller can offer — one holds a `Db`, the other a `Connection`.
pub fn hold_write_lock(mut exec: impl FnMut(&str) -> rusqlite::Result<()>, insert: &str) {
    exec("BEGIN IMMEDIATE").expect("test: take the write lock");
    exec(insert).expect("test: write inside the transaction");
}
