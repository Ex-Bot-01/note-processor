//! One writer per workspace (**F44**).
//!
//! Two `np` processes pointed at one vault used to interleave and one of them died with a raw SQLite string: measured on the packaged 0.2.3
//! binary, two concurrent `scan` runs produced `added 1` and `np: workspace database: sqlite: database is locked` after **5 ms**, exit 1, and —
//! the part that matters to an app — **zero bytes on stdout** under `--json`. The failure was not merely unhelpful wording. `scan` renders
//! `INDEX.md` *inside* its write transaction, so a loser can stop between "the database wrote" and "the files were written", leaving a vault
//! whose index says something its files do not. SQLite cannot roll that back, because the files are not in it.
//!
//! # The lock is a held write transaction on a file of its own
//!
//! `BEGIN IMMEDIATE` on `<data>/<id>.lock.db`, kept open for the duration of the run, and nothing else. No table, no rows, no TTL, no PID
//! bookkeeping: mutual exclusion comes from SQLite's own write lock, which the operating system releases when a process dies — so a killed
//! `np` cannot leave a workspace permanently locked, which is the failure mode every "lock file with a timestamp in it" design has. The
//! rollback journal stays the default for this database on purpose: a held-but-unwritten transaction creates no `-wal`/`-shm` pair, so the cost
//! is one zero-byte file per workspace that has ever been written to.
//!
//! A second writer's `BEGIN IMMEDIATE` then waits ([`DEFAULT_WAIT`], bounded) and reports [`LockError::Held`]. That wait is not a retry loop
//! pretending to be a fix: it exists because the common real case is two commands from one person, seconds apart, and the loser should get an
//! answer rather than a refusal. It is bounded because the other case — a long `embed` on 40k notes — must not turn a `status`-adjacent script
//! into a ten-minute hang.
//!
//! # Why a second file instead of the workspace database
//!
//! Taking the write lock on `notes.db` itself would be fewer files and would say "you are locked out of *your* data" in the error. It cannot
//! work: `scan` and `embed` write through their own connections, so a long-running command would block **itself** for [`DEFAULT_WAIT`] at its
//! first statement and then refuse its own work. A side file gives exclusion between processes without contending with the work that is being
//! protected. It also keeps readers honest: readers never take this lock, and in both journal modes a held-but-uncommitted writer does not stop
//! them — which is why `np search` and `np models list` answer while an `embed` runs, measured in `crates/np-cli/tests/cli.rs`.
//!
//! [`DEFAULT_WAIT`]: WorkspaceLock::DEFAULT_WAIT
//! [`LockError::Held`]: LockError::Held

use std::fs;
use std::io;
use std::path::{Path, PathBuf};
use std::time::Duration;

use rusqlite::Connection;

/// Why a workspace could not be locked for writing.
#[derive(Debug, thiserror::Error)]
pub enum LockError {
    /// Another process holds the write lock, and gave it up too late to matter. This is a **state**, not a defect: the caller reports it the
    /// way it reports `offline` — exit 1 plus a machine-readable field — rather than as prose an app would have to pattern-match.
    #[error("another process is writing this workspace (waited {} ms)", .waited.as_millis())]
    Held {
        /// How long the lock was waited for before giving up, so the message and the JSON can agree with each other.
        waited: Duration,
    },
    /// The lock database could not be opened or created — a real failure, and deliberately not folded into [`LockError::Held`], because "someone
    /// else is busy here" and "this machine will not let me create a file" want different remedies.
    #[error("cannot open the workspace lock at {}: {source}", .path.display())]
    Io {
        path: PathBuf,
        #[source]
        source: io::Error,
    },
    /// Anything else SQLite said. A busy code is reported as [`LockError::Held`] before this arm, so reaching it means something we did not
    /// plan for (a corrupt lock file, an unreadable directory), and the run fails loudly instead of reporting a contention that is not there.
    #[error("workspace lock: {0}")]
    Sqlite(#[from] rusqlite::Error),
}

/// A workspace held for writing by this process. Dropping it releases the lock, which is why no command takes it without binding it to a name:
/// `let _lock = …` would drop on the same line and protect nothing.
#[derive(Debug)]
pub struct WorkspaceLock {
    /// Held only to keep the transaction (and therefore the file lock) alive; no statement runs against it after acquisition.
    conn: Connection,
    path: PathBuf,
}

impl WorkspaceLock {
    /// How long a second writer waits before it calls the workspace locked. Two seconds is a measurement-informed guess, not a constant from a
    /// standard: the observed loser failed in 5–6 ms (no wait at all), and a person typing two commands rarely overlaps by more than this. A
    /// full `embed` of a real corpus runs far longer, and that case is the refusal's, not the wait's.
    pub const DEFAULT_WAIT: Duration = Duration::from_millis(2_000);

    /// Lock workspace `id` for writing, waiting up to `wait`.
    ///
    /// `data_dir` is where the workspace database lives; the lock sits beside it as `<id>.lock.db` so one workspace's state stays in one place —
    /// copying or deleting a workspace's data takes its lock with it, and two workspaces never contend.
    pub fn acquire(data_dir: &Path, id: &str, wait: Duration) -> Result<WorkspaceLock, LockError> {
        let path = data_dir.join(format!("{id}.lock.db"));
        if let Some(parent) = path.parent() {
            if !parent.as_os_str().is_empty() {
                fs::create_dir_all(parent).map_err(|source| LockError::Io {
                    path: path.clone(),
                    source,
                })?;
            }
        }
        let conn = Connection::open(&path)?;
        // The wait is a PRAGMA rather than a loop so SQLite owns the backoff, and it is set per connection because that is where SQLite reads
        // it from: a busy timeout on some *other* connection changes nothing here.
        conn.execute_batch(&format!(
            "PRAGMA busy_timeout = {};",
            wait.as_millis().min(u128::from(u32::MAX))
        ))?;
        match conn.execute_batch("BEGIN IMMEDIATE;") {
            Ok(()) => Ok(WorkspaceLock { conn, path }),
            Err(source) => {
                // Only a busy code means contention. `SQLITE_CANTOPEN` on a directory we cannot read, or a lock file that is not a database,
                // must not be reported to the user as "someone else is writing here" — that sentence teaches them to retry a command that will
                // never succeed.
                if is_busy(&source) {
                    Err(LockError::Held { waited: wait })
                } else {
                    Err(LockError::Sqlite(source))
                }
            }
        }
    }

    /// The lock file's path, for messages and for tests that need to hold a lock from outside the `np` process.
    #[must_use]
    pub fn path(&self) -> &Path {
        &self.path
    }
}

impl Drop for WorkspaceLock {
    fn drop(&mut self) {
        // Rollback, not commit: nothing was written, and rollback is what releases the write lock. Errors are discarded because a failure here
        // means the process is already unable to talk to this file, and there is no remedy left to report — the transaction also dies with the
        // process regardless, which is the property this whole module rests on.
        let _ = self.conn.execute_batch("ROLLBACK;");
    }
}

/// Is this SQLite saying "someone else holds the write lock"?
///
/// Exactly one code means that. `SQLITE_CANTOPEN` in particular is excluded even though it can surface from the same statement: it means the
/// directory or file is unreadable/unwritable, and reporting that as contention would tell the user to retry a command that will never succeed.
fn is_busy(source: &rusqlite::Error) -> bool {
    match source {
        rusqlite::Error::SqliteFailure(err, _) => err.code == rusqlite::ErrorCode::DatabaseBusy,
        _ => false,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A lock is taken, held, and released by dropping — and the same workspace can be locked again afterwards. In-process contention cannot be
    /// tested here: POSIX record locks are per *process*, so a second connection in the same test binary would not conflict, and pretending
    /// otherwise would produce a green test that never measured exclusion. The cross-process behaviour lives in `crates/np-cli/tests/cli.rs`,
    /// where a real second `np` exists.
    #[test]
    fn acquiring_and_dropping_leaves_the_workspace_lockable_again() {
        let dir = tempfile::tempdir().expect("test setup: tempdir");
        let first = WorkspaceLock::acquire(dir.path(), "workspace", Duration::from_millis(50))
            .expect("first acquire succeeds");
        assert!(
            first.path().ends_with("workspace.lock.db"),
            "the lock sits beside the workspace database under a name derived from it: {}",
            first.path().display()
        );
        drop(first);
        WorkspaceLock::acquire(dir.path(), "workspace", Duration::from_millis(50))
            .expect("a released lock is acquirable");
    }

    /// The wait must be bounded by what the caller asked for, or a locked workspace becomes a hang. Measured rather than asserted about
    /// semantics: a wall-clock floor of "it waited roughly this long" and a ceiling of "it did not wait forever".
    #[test]
    fn a_wait_is_a_bound_and_not_a_suggestion() {
        let dir = tempfile::tempdir().expect("test setup: tempdir");
        // Hold the lock from a *child* process, because in-process contention does not exist (see the test above). The child is this same test
        // binary re-invoked with an env var naming the directory; it acquires and sleeps.
        let hold = std::env::current_exe().expect("test setup: this test binary");
        let wait = Duration::from_millis(400);
        let marker = dir.path().join("held");
        let _ = fs::remove_file(&marker);
        let mut child = std::process::Command::new(hold)
            // Re-invoking the test binary with `--ignored` and a name filter runs exactly one test: the holder below. Anything else would run
            // the suite in the child and make this test's own assertions about process counts meaningless.
            .args(["--ignored", "lock_holder_child"])
            .env("NP_TEST_LOCK_DIR", dir.path())
            .env("NP_TEST_LOCK_MS", "1500")
            .stdout(std::process::Stdio::null())
            .stderr(std::process::Stdio::null())
            .spawn()
            .expect("test setup: spawn the holder");
        // Wait for the child to announce, rather than sleeping and hoping: a slow machine must not turn this into "the parent acquired it
        // first", which would fail the assertion below for reasons that have nothing to do with locking.
        let announced = std::time::Instant::now();
        while !marker.exists() && announced.elapsed() < Duration::from_secs(20) {
            std::thread::sleep(Duration::from_millis(20));
        }
        assert!(
            marker.exists(),
            "the holder child never announced the lock; it died or the harness refused to run an ignored test"
        );

        let started = std::time::Instant::now();
        let refused = WorkspaceLock::acquire(dir.path(), "workspace", wait);
        let elapsed = started.elapsed();

        let Err(LockError::Held { waited }) = refused else {
            // Reaped on every path, including this one: a killed-but-never-waited child is a zombie until the harness exits, and a test that
            // leaks processes while failing makes the *next* run's contention behaviour harder to read.
            let _ = child.kill();
            let _ = child.wait();
            panic!(
                "a held workspace must refuse as Held, not as {:?}",
                refused.map(|_| "acquired")
            );
        };
        assert_eq!(waited, wait, "and report the wait it actually honoured");
        assert!(
            elapsed >= Duration::from_millis(300),
            "the caller asked to wait {wait:?} before refusing; it gave up after {elapsed:?}"
        );
        assert!(
            elapsed < Duration::from_secs(5),
            "a bounded wait must stay bounded: waited {elapsed:?}"
        );
        // The child sleeps past this assertion on purpose (it must still be holding the lock while we measure); stop it and reap it.
        let _ = child.kill();
        let _ = child.wait();
    }

    /// This test binary, re-invoked as the holder: acquire, then sleep. Reached only through `NP_TEST_LOCK_DIR`.
    #[test]
    #[ignore = "runs as the child of `a_wait_is_a_bound_and_not_a_suggestion`, not on its own"]
    fn lock_holder_child() {
        let Ok(dir) = std::env::var("NP_TEST_LOCK_DIR") else {
            return;
        };
        let ms: u64 = std::env::var("NP_TEST_LOCK_MS")
            .ok()
            .and_then(|v| v.parse().ok())
            .unwrap_or(1_000);
        let _held =
            WorkspaceLock::acquire(Path::new(&dir), "workspace", Duration::from_millis(200))
                .expect("child: acquire the lock");
        // Announce only after the write lock is held, so the parent's attempt races nothing but the lock itself.
        fs::write(Path::new(&dir).join("held"), b"held").expect("child: announce");
        std::thread::sleep(Duration::from_millis(ms));
    }
}
