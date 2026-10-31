#![cfg(feature = "local-model")]
//! Gated behind `local-model` because the function under test lives in `local_model` — **not** because a case here
//! needs a model file. On a CI runner with the feature and no artifact, all eight cases run for real and none of them
//! prints `NOT COVERAGE`.

//! D12's verify-on-change: what `local_model::verify_artifact` proves about an artifact, what it declines to prove,
//! and what it costs.
//!
//! **No model file is needed here.** The check is a `stat`, a comparison with the stamp on the row, and (only when
//! those disagree) a hash of whatever bytes are there — so every case runs against a few hundred bytes of fixture
//! rather than the 309.9 MB artifact §14 measures. That matters for what these cases pin: the *rule*, not the price.
//! The price is measured in `docs/design-m2-local-embeddings.md` §14 (11.32 s to hash the pinned artifact, ~0.09 s to
//! page it in) and cited by the function's own doc.
//!
//! Two of these cases exist to keep the prose honest about the shortcut:
//! [`an_unchanged_stat_skips_the_hash_which_is_the_shortcut_and_its_limit`] shows a rewritten artifact passing, and
//! [`a_row_written_with_an_uppercase_digest_still_matches`] shows the comparison being about hex digits rather than
//! typography. Both are documented on `verify_artifact`; a limitation nobody tests is a limitation that quietly
//! becomes a feature.

use std::fs::{self, File};
use std::io::Write;
use std::path::{Path, PathBuf};
use std::time::{Duration, SystemTime};

use np_core::db::{self, Db};
use np_core::embed::ArtifactCheck;
use np_core::hash::sha256_bytes;
use np_core::local_model::verify_artifact;
use tempfile::TempDir;

/// Whole seconds since the epoch, far enough back that "later" is unambiguous and no clock skew is involved.
const T0: u64 = 1_700_000_000;

fn db() -> Db {
    Db::in_memory().expect("test setup: in-memory database")
}

/// A file on disk plus the row that claims to describe it, installed as a pair.
struct Fixture {
    /// Held for its `Drop` and read by nothing: the artifact and the database both live under it, so the case that
    /// finishes must not delete the file another assertion is still looking at.
    _dir: TempDir,
    db: Db,
    path: PathBuf,
}

impl Fixture {
    /// Write `bytes` as the artifact and install a row whose `sha256` is the digest of **`declared`** — which the
    /// caller may deliberately make different from what is on disk. The row starts unstamped, like every row written
    /// before this build existed.
    fn new(name: &str, bytes: &[u8], declared: &str) -> Self {
        let dir = TempDir::new().expect("test setup: tempdir");
        let db = db();
        let path = dir.path().join(name);
        fs::write(&path, bytes).expect("test setup: write the artifact");
        // `insert_model` then read back, so the row this test hands the verifier is the row a caller would read:
        // same types, same NULL arms, and no chance of testing against a literal the database never held.
        db::insert_model(&db, "gemma-test", 4, Some(declared), "doc|", "query|", 1)
            .expect("test setup: install the model");
        Self {
            _dir: dir,
            db,
            path,
        }
    }

    fn row(&self) -> np_core::db::ModelRow {
        db::model_row(&self.db, "gemma-test")
            .expect("read the row")
            .expect("the row is there")
    }

    /// Rewrite the file and pin its mtime to `secs`, which is how a test gets a *known* stat instead of one racing
    /// the clock. `set_modified` is the only portable way to do that without a dependency.
    fn rewrite(&self, bytes: &[u8], secs: u64) {
        let mut file = File::create(&self.path).expect("rewrite the artifact");
        file.write_all(bytes).expect("rewrite the artifact");
        drop(file);
        self.set_mtime(secs);
    }

    fn set_mtime(&self, secs: u64) {
        let when = SystemTime::UNIX_EPOCH + Duration::from_secs(secs);
        File::open(&self.path)
            .expect("open to stamp the mtime")
            .set_modified(when)
            .expect("set the mtime");
    }

    fn stamp(&self) -> (Option<i64>, Option<i64>) {
        let row = self.row();
        (row.verified_size, row.verified_mtime)
    }
}

fn verify(fx: &Fixture) -> Result<ArtifactCheck, np_core::embed::EmbedError> {
    verify_artifact(&fx.db, &fx.row(), &fx.path)
}

/// The first run under this build has no stamp, so it hashes — and the stamp it writes describes exactly the bytes
/// that hash covered.
#[test]
fn an_unstamped_artifact_is_hashed_and_the_stamp_records_the_bytes_it_covered() {
    let bytes = b"gguf-ish bytes, three hundred and nine million of them in production".to_vec();
    let digest = sha256_bytes(&bytes);
    let fx = Fixture::new("artifact", &bytes, &digest);
    assert_eq!(fx.stamp(), (None, None), "a fresh install carries no stamp");

    let check = verify(&fx).expect("the artifact matches its row");
    assert_eq!(
        check,
        ArtifactCheck::VerifiedNow {
            bytes: bytes.len() as u64
        },
        "nothing was stamped, so this run had to read the file"
    );

    let size = i64::try_from(bytes.len()).expect("fixture size fits i64");
    let mtime = fx
        .path
        .metadata()
        .and_then(|m| m.modified())
        .map(|when| {
            when.duration_since(SystemTime::UNIX_EPOCH)
                .expect("mtime")
                .as_secs() as i64
        })
        .expect("fixture mtime is readable");
    assert_eq!(
        fx.stamp(),
        (Some(size), Some(mtime)),
        "the stamp records the size and whole-second mtime of the file that was hashed"
    );

    // And the second run, with nothing touched, says so without reading a byte.
    assert_eq!(
        verify(&fx).expect("still matches"),
        ArtifactCheck::UnchangedSinceStamp {
            bytes: bytes.len() as u64
        }
    );
}

/// The shortcut, and the hole that comes with it, in one case.
///
/// Same size, same mtime, different bytes reports `UnchangedSinceStamp`. That is not a bug to fix here — checking it
/// would mean hashing every run, which is the 11.32 s §14 measured and the whole reason for the stamp. It is the
/// limitation `ArtifactCheck`'s doc states ("detects changed bytes, not a crafted mtime"), pinned so the sentence
/// cannot drift into a stronger claim than the code makes.
#[test]
fn an_unchanged_stat_skips_the_hash_which_is_the_shortcut_and_its_limit() {
    let original = b"the artifact as installed".to_vec();
    let digest = sha256_bytes(&original);
    let fx = Fixture::new("artifact", &original, &digest);
    verify(&fx).expect("first run verifies");

    // Same length, same mtime, different content: whoever can write the file can also set its mtime. Flipping one
    // bit is the honest way to get "equal size, different bytes" without counting characters by hand.
    let mut swapped = original.clone();
    swapped[0] ^= 0x20;
    assert_eq!(
        swapped.len(),
        original.len(),
        "the limit needs equal length"
    );
    assert_ne!(
        sha256_bytes(&swapped),
        digest,
        "and genuinely different bytes"
    );
    let stamped_mtime = fx
        .row()
        .verified_mtime
        .expect("stamped by the first verify");
    fx.rewrite(&swapped, stamped_mtime as u64);

    assert_eq!(
        verify(&fx).expect("the stat matches, so nothing is hashed and nothing can disagree"),
        ArtifactCheck::UnchangedSinceStamp {
            bytes: swapped.len() as u64
        },
        "a same-size swap inside the stamped second is invisible — documented, not fixed"
    );
}

/// Change the bytes (and so the size): the stamp no longer matches, the file is re-hashed, and the refusal names both
/// digests before any vector is written.
#[test]
fn a_different_digest_is_refused_naming_what_the_row_says_and_what_the_file_is() {
    let original = b"artifact bytes v1".to_vec();
    let digest = sha256_bytes(&original);
    let fx = Fixture::new("artifact", &original, &digest);
    verify(&fx).expect("first run verifies");
    let stamp_before = fx.stamp();

    let swapped = b"artifact bytes v1 + one more byte than the row was told about".to_vec();
    fx.rewrite(&swapped, T0 + 60);

    let err = verify(&fx).expect_err("a swapped artifact must not be loaded");
    match err {
        np_core::embed::EmbedError::ArtifactDigestMismatch {
            path,
            expected,
            found,
        } => {
            assert_eq!(path, fx.path);
            assert_eq!(expected, digest, "the row's claim is reported verbatim");
            assert_eq!(found, sha256_bytes(&swapped), "and so is the file's");
        }
        other => panic!("a digest disagreement is its own variant, not {other:?}"),
    }

    // The refusal leaves the stamp where it was: a run that did not verify anything may not leave a fresh reason for
    // the next one to skip verifying. `original`'s size is what the first verification recorded.
    assert_eq!(
        fx.stamp(),
        stamp_before,
        "a refused verification writes nothing"
    );
}

/// A touched-but-unchanged artifact (restored from a backup, rsync'd, `touch`ed) re-hashes, matches, and moves the
/// stamp forward. Without this the next run would pay the same hash again forever.
#[test]
fn a_touched_artifact_re_verifies_and_moves_the_stamp_forward() {
    let bytes = b"restored artifact, byte for byte".to_vec();
    let digest = sha256_bytes(&bytes);
    let fx = Fixture::new("artifact", &bytes, &digest);
    verify(&fx).expect("first run verifies");
    let before = fx.row().verified_mtime.expect("stamped");

    fx.set_mtime((before as u64) + 120);
    assert_eq!(
        verify(&fx).expect("the bytes still match"),
        ArtifactCheck::VerifiedNow {
            bytes: bytes.len() as u64
        },
        "a moved mtime means the file is read again"
    );
    let after = fx.row().verified_mtime.expect("re-stamped");
    assert_eq!(after, before + 120, "and the stamp follows the file");
    assert_eq!(
        verify(&fx).expect("now it is current again"),
        ArtifactCheck::UnchangedSinceStamp {
            bytes: bytes.len() as u64
        }
    );
}

/// `models.sha256` is NULL for a provider with no local weights. That is not a failed check and must not be reported
/// as one — and it must not leave a stamp either, because there is nothing for a stamp to describe.
#[test]
fn a_row_without_a_digest_verifies_nothing_and_stamps_nothing() {
    let dir = TempDir::new().expect("test setup: tempdir");
    let db = db();
    let path = dir.path().join("weights-not-applicable");
    fs::write(&path, b"not a model, and no row claims otherwise")
        .expect("test setup: write a file");
    db::insert_model(&db, "endpoint-ish", 4, None, "", "", 1)
        .expect("test setup: install a weightless row");

    let row = db::model_row(&db, "endpoint-ish")
        .expect("read")
        .expect("present");
    assert_eq!(
        verify_artifact(&db, &row, &path).expect("nothing to verify is not a failure"),
        ArtifactCheck::NothingToVerify
    );
    // Read the row back rather than trusting `row`: the value handed to `verify_artifact` is a snapshot from before
    // the call and would read (None, None) no matter what this run wrote.
    let reread = db::model_row(&db, "endpoint-ish")
        .expect("read")
        .expect("present");
    assert_eq!(
        (reread.verified_size, reread.verified_mtime),
        (None, None),
        "the run that verified nothing writes no stamp"
    );
}

/// `sha256` is a plain TEXT column a human can edit; `2A88…` and `2a88…` are the same digest. Refusing on case would
/// be a refusal about typography, and would also make `--sha256` spelled uppercase at install time unembeddable.
#[test]
fn a_row_written_with_an_uppercase_digest_still_matches() {
    let bytes = b"identical bytes, differently spelled row".to_vec();
    let digest = sha256_bytes(&bytes).to_ascii_uppercase();
    let fx = Fixture::new("artifact", &bytes, &digest);

    assert_eq!(
        verify(&fx).expect("hex case is not a disagreement"),
        ArtifactCheck::VerifiedNow {
            bytes: bytes.len() as u64
        }
    );
}

/// A half stamp — size without mtime, or the reverse — is treated as no stamp at all.
///
/// Reachable only by someone editing the database by hand (`db.rs` writes both halves together and its own test pins
/// that), which is exactly why it needs a case: the alternative reading, "one half matched, skip the hash", would
/// turn a NULL into a permanent verification.
#[test]
fn a_half_stamp_reads_as_no_stamp_and_forces_a_hash() {
    let bytes = b"artifact with a hand-edited stamp".to_vec();
    let digest = sha256_bytes(&bytes);
    let fx = Fixture::new("artifact", &bytes, &digest);
    verify(&fx).expect("first run verifies");

    fx.db
        .0
        .execute(
            "UPDATE models SET verified_mtime = NULL WHERE id = 'gemma-test'",
            rusqlite::params![],
        )
        .expect("test setup: drop one half of the stamp");

    assert_eq!(
        verify(&fx).expect("the file still matches its row"),
        ArtifactCheck::VerifiedNow {
            bytes: bytes.len() as u64
        },
        "a missing half is not a match"
    );
    assert!(
        fx.row().verified_mtime.is_some(),
        "and the run re-writes the pair it found incomplete"
    );
}

/// A stamp write can fail — another process holds the index past the busy timeout, or the row was removed under us by
/// `np models remove`. The bytes matched, so the embed must go ahead; what it may **not** do is report `VerifiedNow`,
/// which promises the next run gets to skip the hash. It also may not arrive as `ArtifactUnreadable`: this file was
/// just read and proved.
#[test]
fn a_matched_artifact_that_cannot_be_stamped_reports_the_hash_without_the_stamp() {
    let bytes = b"verified bytes, unwritable index".to_vec();
    let digest = sha256_bytes(&bytes);
    let fx = Fixture::new("artifact", &bytes, &digest);

    // The blunt way to make the write fail without touching production code: delete the table the stamp goes into.
    // `verify_artifact` has already read its row by this point, so only the write can notice.
    let check = {
        let db = &fx.db;
        let row = fx.row();
        db.0.execute("DROP TABLE models", [])
            .expect("test setup: remove the stamp's table");
        verify_artifact(db, &row, &fx.path).expect("a matched artifact is still embeddable")
    };

    match check {
        ArtifactCheck::VerifiedUnstampable {
            bytes: n,
            ref reason,
        } => {
            assert_eq!(n, bytes.len() as u64);
            assert!(
                reason.contains("could not be written"),
                "the reason has to name the stamp, not the file: {reason}"
            );
        }
        other => panic!("a run that recorded nothing may not claim otherwise: {other:?}"),
    }
    assert!(
        check.label().contains("no stamp recorded"),
        "and the report says so where the reader will see why the next run is slow again: {}",
        check.label()
    );
}

/// A file whose mtime cannot be turned into a whole second since the epoch has nothing to stamp, so this run hashes and
/// says the stamp did not travel. Reporting `VerifiedNow` here would be the exact failure mode the type exists to
/// avoid: the next run would have no stamp to compare, hash again anyway, and every report along the way would have
/// claimed the bytes were freshly checked *and recorded*.
#[test]
fn an_artifact_with_no_representable_mtime_verifies_without_stamping() {
    let bytes = b"restored from a 1960s backup".to_vec();
    let digest = sha256_bytes(&bytes);
    let fx = Fixture::new("artifact", &bytes, &digest);

    // Before the epoch: `metadata::modified()` answers, `duration_since(UNIX_EPOCH)` does not. Whether a filesystem
    // accepts such an mtime is a fact about this host, so the case reports rather than assumes: if `set_modified`
    // refuses it, the test says which half could not be built here.
    let before_epoch = SystemTime::UNIX_EPOCH - Duration::from_secs(86_400);
    match File::open(&fx.path)
        .expect("open")
        .set_modified(before_epoch)
    {
        Ok(()) => {}
        Err(err) => panic!(
            "this filesystem refuses a pre-epoch mtime ({err}); the guard it would exercise is \
             `verify_artifact`'s `mtime: None` arm, which then has no coverage here"
        ),
    }

    let check = verify(&fx).expect("the bytes still match their row");
    match check {
        ArtifactCheck::VerifiedUnstampable {
            bytes: n,
            ref reason,
        } => {
            assert_eq!(n, bytes.len() as u64);
            assert!(reason.contains("modification time"), "{reason}");
        }
        other => panic!("no stamp was possible, so none may be claimed: {other:?}"),
    }
    assert_eq!(
        fx.stamp(),
        (None, None),
        "and the database really does not hold a stamp this run could not have written"
    );
}

/// `verify_artifact` refuses a directory the way the provider does, rather than letting a read error arrive as a
/// digest disagreement.
#[test]
fn an_artifact_path_that_is_a_directory_is_refused_as_unreadable() {
    let dir = TempDir::new().expect("test setup: tempdir");
    let db = db();
    let subdir: &Path = &dir.path().join("a-directory");
    fs::create_dir(subdir).expect("test setup: make a directory");
    db::insert_model(&db, "gemma-test", 4, Some(&"ab".repeat(32)), "", "", 1)
        .expect("test setup: install the model");
    let row = db::model_row(&db, "gemma-test")
        .expect("read")
        .expect("present");

    let err = verify_artifact(&db, &row, subdir).expect_err("a directory is not an artifact");
    assert!(
        matches!(err, np_core::embed::EmbedError::ArtifactUnreadable { .. }),
        "the path is unreadable as a file, which is the variant that says so: {err:?}"
    );
}
