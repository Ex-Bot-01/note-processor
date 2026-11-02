//! Installing a pinned model artifact and listing what is on disk — round 4a's §7 cases, offline.
//!
//! Everything here goes through [`np_core::models`] rather than through a command: round 4a is np-core only,
//! so §7's cases are exercised at the seam np-cli will call. Two consequences worth stating rather than
//! leaving implicit:
//!
//! - **§7 case 9 is not here.** It compares `np models hash`'s output with [`np_core::hash::sha256_file`],
//!   and this round adds no command. What np-core can pin — that `models.rs` defines no second SHA-256 — it
//!   pins by having none: every digest in these fixtures comes from `np_core::hash`.
//! - **The cross-process race is not simulated.** §7 case 13's `ON CONFLICT (id) DO NOTHING` arm is reached
//!   by calling [`np_core::db::insert_model`] a second time, which is what the install path hands the conflict
//!   to; two OS processes racing one artifact directory is a claim these fixtures do not make.
//!
//! Ordering claims are tested the way §3 states them: a bad id is asserted against a `--path` **that does not
//! exist**, and an empty file against **the true digest of nothing**, so a mutant that reorders or drops a step
//! changes which error appears rather than merely which message text.

use std::ffi::OsStr;
use std::fs;
use std::num::NonZeroU16;
use std::path::{Path, PathBuf};

use np_core::config::Config;
use np_core::db::{self, Db, ModelRow};
use np_core::hash::{sha256_bytes, sha256_file};
use np_core::models::{
    self, ArtifactState, FetchFields, Install, InstallOutcome, ListedModel, ModelError, Unclaimed,
    UnclaimedKind,
};
use rusqlite::params;
use tempfile::TempDir;

/// The timestamp every install in these fixtures passes as `now`. One value throughout, so "the row was
/// rewritten" shows up as a changed `installed_at` rather than as a plausible-looking second clock reading.
const NOW: i64 = 1_730_000_000;

/// A data dir with a config file naming it, and a workspace directory that exists — the two preconditions
/// §3 step 0 refuses everything else on.
struct Fixture {
    /// Held so the paths below stay alive for the length of a test.
    _tmp: TempDir,
    config: Config,
    workspace: PathBuf,
}

impl Fixture {
    fn new() -> Self {
        let tmp = TempDir::new().expect("test setup: tempdir");
        let data_dir = tmp.path().join("data");
        fs::create_dir_all(&data_dir).expect("test setup: data dir");
        let workspace = tmp.path().join("workspace");
        fs::create_dir_all(&workspace).expect("test setup: workspace dir");
        Fixture {
            _tmp: tmp,
            config: config(&data_dir),
            workspace,
        }
    }

    /// This workspace's database, opened the way a command opens it — after [`models::require_workspace_dir`],
    /// which is the order §3 step 0 exists to make possible.
    fn db(&self) -> Db {
        models::require_workspace_dir(&self.workspace).expect("test setup: the workspace exists");
        Db::open(&self.config.workspace_db(&self.workspace))
            .expect("test setup: open the workspace db")
    }

    /// The artifact directory `install` is supposed to use — derived from the config, never spelled here.
    fn models_dir(&self) -> PathBuf {
        self.config.workspace_models_dir(&self.workspace)
    }

    /// The artifact path one id claims.
    fn artifact(&self, id: &str) -> PathBuf {
        self.models_dir().join(id)
    }

    /// Write a source file the human supposedly already has, returning its path.
    fn source(&self, name: &str, bytes: &[u8]) -> PathBuf {
        let path = self._tmp.path().join(name);
        fs::write(&path, bytes).expect("test setup: write the source artifact");
        path
    }

    /// A workspace directory that does not exist, for §3 step 0.
    fn missing_workspace(&self) -> PathBuf {
        self._tmp.path().join("typo-in-the-name")
    }
}

/// A config whose `data_dir` is `dir`, so nothing is created outside the tempdir. The separators of a Windows
/// tempdir path are escaped because `data_dir` travels as a TOML basic string.
fn config(dir: &Path) -> Config {
    let path = dir.join("config.toml");
    let text = dir.display().to_string().replace('\\', "\\\\");
    fs::write(&path, format!("data_dir = \"{text}\"\n")).expect("test setup: write the config");
    Config::load(Some(&path)).expect("test setup: load the config")
}

/// The declaration a test installs, with no prefixes — which is what makes "the prefixes travelled" assertions
/// in [`installs_the_artifact_and_the_row`] mean something.
fn request(id: &str, source: &Path, sha256: &str, dim: u16) -> Install {
    Install {
        id: id.to_string(),
        source: source.to_path_buf(),
        dim: NonZeroU16::new(dim).expect("test fixture: a declared dim is never zero"),
        sha256: sha256.to_string(),
        prefix_doc: String::new(),
        prefix_query: String::new(),
    }
}

/// Install with the fixture's workspace, failing with the error's own text so a wrong refusal is readable in
/// the assertion output.
fn install(fx: &Fixture, db: &Db, req: &Install) -> Result<InstallOutcome, ModelError> {
    models::install(db, &fx.config, &fx.workspace, req, NOW)
}

/// The names one directory holds, sorted — the count and the spelling of what an install left behind, so a
/// temporary that was never renamed cannot hide inside "the artifact is there".
fn names(dir: &Path) -> Vec<String> {
    let mut found: Vec<String> = fs::read_dir(dir)
        .unwrap_or_else(|err| panic!("test: {} should be listable: {err}", dir.display()))
        .map(|entry| {
            entry
                .expect("test: readable directory entry")
                .file_name()
                .to_string_lossy()
                .into_owned()
        })
        .collect();
    found.sort();
    found
}

/// The per-row verdicts of one listing, as `(id, state)` pairs in listing order.
fn states(listing: &models::Listing) -> Vec<(&str, ArtifactState)> {
    listing
        .models
        .iter()
        .map(|ListedModel { row, state }| (row.id.as_str(), *state))
        .collect()
}

/// The single row these fixtures install, read back or panicked over.
fn row(db: &Db, id: &str) -> ModelRow {
    db::model_row(db, id)
        .expect("test: models is readable")
        .unwrap_or_else(|| panic!("test: no row names `{id}`"))
}

/// One seeded vector under `model`, so §7-8's "embeddings unchanged" claim is about a real row rather than
/// about an empty table. The note and chunk rows are what the foreign keys require.
fn seed_vector(db: &Db, model: &str, dim: usize) {
    // `notes.rel` is a foreign key onto `files.rel`, so the registry row has to exist first — without it the
    // seed below fails with a constraint violation and the test reports a setup error instead of a fact.
    // Hand-seeded rather than built through `upsert_file`, which needs a whole `Snapshot`; the price is that
    // every NOT NULL column has to be named, and migration 4's byte spellings are easy to forget.
    db.0.execute(
        "INSERT INTO files (rel_bytes, rel, case_bytes, case_key, abs, hash, size, mtime, state, last_seen)
         VALUES (?1, 'a.md', ?2, 'a.md', '/ws/a.md', 'seed-digest', 4, 1, 'ok', 1)",
        params![b"a.md".as_slice(), b"a.md".as_slice()],
    )
    .expect("test setup: registry row");
    db.0.execute(
        "INSERT INTO notes (rel_bytes, rel, title, title_source, encoding, word_count, body)
         VALUES (?1, ?2, 'seed', 'filename', 'utf-8', 3, 'body')",
        params![b"a.md".as_slice(), "a.md"],
    )
    .expect("test setup: note row");
    let note_id = db.0.last_insert_rowid();
    db.0.execute(
        "INSERT INTO chunks (note_id, ordinal, chunker, text, byte_start, byte_end, source_hash)
         VALUES (?1, 0, 1, ?2, 0, 4, 'seed-digest')",
        params![note_id, b"body".as_slice()],
    )
    .expect("test setup: chunk row");
    let chunk_id = db.0.last_insert_rowid();
    let vector = np_core::embed::Vector::new(vec![0.25; dim]).to_bytes();
    db::upsert_vector(db, chunk_id, model, dim, &vector, "seed-digest", NOW)
        .expect("test setup: vector row");
}

/// What one model's vectors say, for the before/after comparison in [`a_deleted_artifact_is_listed_missing_and_installed_back_as_restored`].
fn vectors(db: &Db, model: &str) -> Vec<(i64, i64, String)> {
    db.0.prepare_cached(
        "SELECT dim, embedded_at, source_hash FROM embeddings WHERE model = ?1 ORDER BY chunk_id",
    )
    .expect("test: embeddings is readable")
    .query_map(params![model], |row| {
        Ok((row.get(0)?, row.get(1)?, row.get(2)?))
    })
    .expect("test: embedding rows")
    .collect::<Result<Vec<_>, _>>()
    .expect("test: embedding values")
}

/// §7 case 1: the artifact lands at the per-workspace path, the row holds every declared value, and nothing
/// else is left in the directory.
#[test]
fn installs_the_artifact_and_the_row() {
    let fx = Fixture::new();
    let source = fx.source("weights-v1.bin", b"weights v1");
    let digest = sha256_file(&source).expect("test setup: hash the source");
    let db = fx.db();

    let mut declared = request("alpha", &source, &digest, 8);
    declared.prefix_doc = "d: ".to_string();
    declared.prefix_query = "q: ".to_string();
    let outcome = install(&fx, &db, &declared).expect("a matching digest installs");

    assert_eq!(outcome, InstallOutcome::Installed);
    assert_eq!(outcome.state_label(), "installed");
    // The artifact is where the *config* says this workspace's artifacts go — spelled nowhere in this file.
    assert_eq!(fx.artifact("alpha"), fx.models_dir().join("alpha"));
    assert_eq!(
        fs::read(fx.artifact("alpha")).expect("test: the artifact is readable"),
        b"weights v1"
    );
    // One entry: the rename left no `<id>.<nonce>.tmp` behind.
    assert_eq!(names(&fx.models_dir()), ["alpha"]);

    let installed = row(&db, "alpha");
    assert_eq!(installed.id, "alpha");
    assert_eq!(installed.dim, 8);
    assert_eq!(installed.sha256.as_deref(), Some(digest.as_str()));
    assert_eq!(installed.prefix_doc, "d: ");
    assert_eq!(installed.prefix_query, "q: ");
    assert_eq!(installed.installed_at, NOW);

    // A second id in the same workspace gets its own row and its own artifact, so one install cannot be
    // mistaken for a whole-directory claim.
    let other = fx.source("weights-v2.bin", b"weights v2");
    let other_digest = sha256_file(&other).expect("test setup: hash the second source");
    install(&fx, &db, &request("beta", &other, &other_digest, 4)).expect("second install");
    assert_eq!(row(&db, "beta").dim, 4);
    assert_eq!(names(&fx.models_dir()), ["alpha", "beta"]);
}

/// §7 case 2: a declared digest the file does not match is refused with **both** digests named, and leaves
/// neither an artifact nor a row.
#[test]
fn a_digest_mismatch_names_both_and_leaves_no_file_and_no_row() {
    let fx = Fixture::new();
    let source = fx.source("weights-v1.bin", b"weights v1");
    let found = sha256_file(&source).expect("test setup: hash the source");
    // A well-formed hex digest of *other* bytes: the refusal has to be the mismatch, not a format complaint.
    let declared = sha256_bytes(b"some other artifact entirely");
    let db = fx.db();

    let failure = install(&fx, &db, &request("alpha", &source, &declared, 8))
        .expect_err("a wrong digest cannot install");
    assert!(
        matches!(failure, ModelError::DigestMismatch { .. }),
        "expected the mismatch arm, got {failure}"
    );
    let text = failure.to_string();
    assert!(
        text.contains(&found) && text.contains(&declared),
        "a mismatch that names only one digest is not diagnosable: {text}"
    );

    assert!(!fx.artifact("alpha").exists(), "no artifact was written");
    assert!(
        !fx.models_dir().exists(),
        "the directory was never even created, so there is nothing half-installed to find"
    );
    assert!(
        db::model_row(&db, "alpha")
            .expect("test: models is readable")
            .is_none(),
        "no row either"
    );
}

/// An argument that is not 64 hex characters is refused as the format problem it is (§3 step 4), rather than
/// being compared against a real digest and reported as a mismatch.
#[test]
fn a_malformed_digest_is_refused_before_the_source_is_touched() {
    // The ordering this round moved. A bad `--sha256` is a fact about argv, so it must not cost a read of the
    // file: killed by moving `normalize_digest` back below the digest step, which discovers the missing file
    // first and answers with a read failure instead.
    let fx = Fixture::new();
    let db = fx.db();
    let missing = fx._tmp.path().join("never-downloaded.bin");
    let failure = install(&fx, &db, &request("alpha", &missing, "deadbeef", 8))
        .expect_err("a truncated digest is refused whatever the file says");
    assert!(
        matches!(failure, ModelError::InvalidDigest { .. }),
        "the format refusal wins over a source that cannot even be opened: {failure}"
    );
}

#[test]
fn a_source_that_cannot_be_read_names_the_source_and_not_the_artifact() {
    // `--path` pointing at a directory is the ordinary way to hit this. The message must say "install source"
    // and name a *read*: calling the human's input "the model artifact" is the wrong-fact-in-the-message class,
    // which this project treats as a defect rather than as cosmetics.
    let fx = Fixture::new();
    let db = fx.db();
    let failure = install(
        &fx,
        &db,
        &request("alpha", fx._tmp.path(), &"0".repeat(64), 8),
    )
    .expect_err("a directory is not an artifact");
    match failure {
        ModelError::Source { path, .. } => assert!(
            path.ends_with(
                fx._tmp
                    .path()
                    .file_name()
                    .expect("test: the tempdir has a name")
            ),
            "the refusal names the input it could not read: {path:?}"
        ),
        other => panic!("a directory as `--path` is a read refusal, got {other}"),
    }
}

/// An artifact that cannot be hashed fails the **whole** listing rather than degrading into a per-row verdict:
/// "I could not read it" must never be reported as "its bytes disagree".
#[cfg(unix)]
#[test]
fn an_unreadable_artifact_fails_the_listing_instead_of_becoming_a_verdict() {
    use std::os::unix::fs::PermissionsExt;

    let fx = Fixture::new();
    let source = fx.source("weights-v1.bin", b"weights v1");
    let digest = sha256_file(&source).expect("test setup: hash the source");
    let db = fx.db();
    install(&fx, &db, &request("alpha", &source, &digest, 8)).expect("first install");

    fs::set_permissions(fx.artifact("alpha"), fs::Permissions::from_mode(0o000))
        .expect("test setup: take every bit away from the artifact");
    let listing = models::list(&db, &fx.config, &fx.workspace);
    fs::set_permissions(fx.artifact("alpha"), fs::Permissions::from_mode(0o600))
        .expect("test: give the artifact back to its owner");

    match listing {
        Err(ModelError::Artifact { path, .. }) => assert!(
            path.ends_with("alpha"),
            "the refusal names the artifact it could not hash: {path:?}"
        ),
        other => panic!("an unreadable artifact is a read failure, not a state: {other:?}"),
    }
}

#[test]
fn a_digest_that_is_not_64_hex_characters_is_refused_as_a_format_problem() {
    let fx = Fixture::new();
    let source = fx.source("weights-v1.bin", b"weights v1");
    let db = fx.db();

    for given in [
        "",
        "deadbeef",
        &"z".repeat(64),
        &"0".repeat(63),
        &"0".repeat(65),
    ] {
        let failure = install(&fx, &db, &request("alpha", &source, given, 8))
            .expect_err("`--sha256` is not optional and is not free-form");
        assert!(
            matches!(failure, ModelError::InvalidDigest { .. }),
            "`{given}` is a format problem, not a mismatch: {failure}"
        );
    }
    assert!(!fx.artifact("alpha").exists());
    assert!(db::model_row(&db, "alpha")
        .expect("test: readable")
        .is_none());
}

/// §7 case 3: every bad-id class is refused **before any file access**, proved by pointing `--path` at a file
/// that does not exist and `--sha256` at text that is not a digest. If any step ran first, its error would
/// speak instead of the id rule.
#[test]
fn a_bad_id_is_refused_before_anything_is_read() {
    let fx = Fixture::new();
    // Never written: `assert!(!missing.exists())` is what makes the class of error below evidence about
    // ordering rather than about the file.
    let missing = fx._tmp.path().join("never-written.bin");
    assert!(!missing.exists(), "test fixture: this path must not exist");
    let db = fx.db();

    for (id, fragment) in [
        ("Alpha", "first character"),
        ("-lead", "first character"),
        (".hidden", "first character"),
        ("bad name", "only a-z"),
        ("bad/name", "only a-z"),
        (&"a".repeat(65), "longer than 64"),
        ("trailing.", "ends in"),
        ("nul", "reserved"),
        ("NUL.txt", "reserved"),
        ("com1", "reserved"),
        ("aux.md", "reserved"),
    ] {
        let failure = install(&fx, &db, &request(id, &missing, "not-a-digest", 8))
            .expect_err("a bad id must be refused");
        match failure {
            ModelError::InvalidId { rule, .. } => assert!(
                rule.contains(fragment),
                "`{id}` should fail the `{fragment}` rule; the refusal said: {rule}"
            ),
            other => panic!("`{id}` reached {other} instead of the id rule"),
        }
    }

    // Nothing anywhere else was touched by eleven refused arguments.
    assert!(!fx.models_dir().exists());
    // The boundary §2 states rather than implies: 64 characters is legal, and the charset allows `._@-`.
    models::validate_id(&"a".repeat(64)).expect("64 characters is inside the rule");
    models::validate_id("miniLM.v2@ctx-8")
        .expect_err("`L` and `M` are uppercase, which §2 refuses");
    models::validate_id("mini-lm.v2@ctx_8").expect("the whole legal charset round-trips");
}

/// §7 case 4: an empty source gets the zero-length refusal. The declared digest is **the true digest of
/// nothing**, so a mutant that dropped §3 step 2 would find the digests agreeing and install an empty artifact
/// rather than failing this test with a mismatch.
#[test]
fn an_empty_source_gets_the_zero_length_refusal_rather_than_a_digest_mismatch() {
    let fx = Fixture::new();
    let source = fx.source("empty.bin", b"");
    let empty_digest = sha256_bytes(b"");
    let db = fx.db();

    let failure = install(&fx, &db, &request("alpha", &source, &empty_digest, 8))
        .expect_err("a zero-length artifact pins nothing and must be refused");
    assert!(
        matches!(failure, ModelError::EmptySource { .. }),
        "the diagnosis has to be \"the file is empty\", not a digest comparison: {failure}"
    );
    assert!(
        failure.to_string().contains("zero-length"),
        "and it has to say so: {failure}"
    );
    assert!(!fx.artifact("alpha").exists());
    assert!(db::model_row(&db, "alpha")
        .expect("test: readable")
        .is_none());
}

/// §7 case 5: the same declaration twice reports `already installed` and rewrites nothing — including
/// `installed_at` and the artifact's own modification time, which is what proves the second call copied.
#[test]
fn the_same_declaration_twice_reports_already_installed_and_rewrites_nothing() {
    let fx = Fixture::new();
    let source = fx.source("weights-v1.bin", b"weights v1");
    let digest = sha256_file(&source).expect("test setup: hash the source");
    let db = fx.db();
    let declared = request("alpha", &source, &digest, 8);

    install(&fx, &db, &declared).expect("first install");
    let first = row(&db, "alpha");
    let written = fs::metadata(fx.artifact("alpha"))
        .expect("test: the artifact is readable")
        .modified()
        .expect("test: the artifact carries a modification time");

    let again =
        install(&fx, &db, &declared).expect("a repeat of the same declaration is not an error");
    assert_eq!(again, InstallOutcome::AlreadyInstalled);
    assert_eq!(again.state_label(), "already installed");

    let second = row(&db, "alpha");
    assert_eq!(
        second, first,
        "the whole row is untouched, not just one column"
    );
    assert_eq!(second.installed_at, NOW, "`installed_at` is written once");
    assert_eq!(
        fs::metadata(fx.artifact("alpha"))
            .expect("test: the artifact is readable")
            .modified()
            .expect("test: the artifact carries a modification time"),
        first_modified(written),
        "the artifact was not re-copied"
    );
    assert_eq!(names(&fx.models_dir()), ["alpha"]);
}

/// Unwrapping the pinned mtime so the assertion above reads as one comparison.
fn first_modified(mtime: std::time::SystemTime) -> std::time::SystemTime {
    mtime
}

/// §7 case 6: a second source with **its own honest** digest, under an id that is already installed, is
/// refused on the `sha256` column — and changes neither the artifact or the row.
#[test]
fn a_second_source_with_its_own_digest_is_refused_against_the_installed_row() {
    let fx = Fixture::new();
    let source = fx.source("weights-v1.bin", b"weights v1");
    let digest = sha256_file(&source).expect("test setup: hash the source");
    let db = fx.db();
    install(&fx, &db, &request("alpha", &source, &digest, 8)).expect("first install");

    let other = fx.source("weights-v2.bin", b"weights v2, different bytes");
    let other_digest = sha256_file(&other).expect("test setup: hash the second source");
    let failure = install(&fx, &db, &request("alpha", &other, &other_digest, 8))
        .expect_err("one identity cannot mean two artifacts");
    match failure {
        ModelError::AlreadyInstalled {
            id,
            column,
            stored,
            declared,
        } => {
            assert_eq!(id, "alpha");
            assert_eq!(column, "sha256");
            assert_eq!(stored, digest);
            assert_eq!(declared, other_digest);
        }
        other => panic!("expected the full-tuple refusal, got {other}"),
    }

    assert_eq!(
        fs::read(fx.artifact("alpha")).expect("test: readable"),
        b"weights v1"
    );
    assert_eq!(row(&db, "alpha").sha256.as_deref(), Some(digest.as_str()));
    assert_eq!(
        names(&fx.models_dir()),
        ["alpha"],
        "the refused source was not copied in"
    );
}

/// §7 case 6b: the same digest with a **different dim** is refused, naming stored and declared widths. This
/// is the blind spot a sha-only comparison leaves — `dim` is the `expected` of every later dimension refusal.
#[test]
fn the_same_digest_with_a_different_dim_is_refused_naming_both_widths() {
    let fx = Fixture::new();
    let source = fx.source("weights-v1.bin", b"weights v1");
    let digest = sha256_file(&source).expect("test setup: hash the source");
    let db = fx.db();
    install(&fx, &db, &request("alpha", &source, &digest, 8)).expect("first install");

    let failure = install(&fx, &db, &request("alpha", &source, &digest, 16))
        .expect_err("a dim change is a different model identity in every later refusal");
    match failure {
        ModelError::AlreadyInstalled { column, .. } => assert_eq!(column, "dim"),
        other => panic!("expected the full-tuple refusal, got {other}"),
    }
    let text = failure.to_string();
    for named in ["8", "16"] {
        assert!(
            text.contains(named),
            "the refusal has to name both widths; it said: {text}"
        );
    }
    assert_eq!(
        row(&db, "alpha").dim,
        8,
        "the stored dim is what it always was"
    );

    // A prefix change is the same class of refusal: §3 step 5 compares all four columns.
    let mut prefixed = request("alpha", &source, &digest, 8);
    prefixed.prefix_doc = "d: ".to_string();
    match install(&fx, &db, &prefixed).expect_err("a prefix is part of what the row means") {
        ModelError::AlreadyInstalled { column, .. } => assert_eq!(column, "prefix_doc"),
        other => panic!("expected the full-tuple refusal, got {other}"),
    }
}

/// §7 case 6c: an uppercase `--sha256` installs **lowercased**, and re-installing the lowercase spelling then
/// reports `already installed`. Normalising nowhere would make the second call a sha conflict and leave that id
/// permanently un-installable, since there is no `--force`.
#[test]
fn an_uppercase_digest_is_stored_lowercase_so_the_lowercase_spelling_is_a_repeat() {
    let fx = Fixture::new();
    let source = fx.source("weights-v1.bin", b"weights v1");
    let lowered = sha256_file(&source).expect("test setup: hash the source");
    assert!(
        lowered.chars().any(char::is_numeric),
        "test fixture: this digest has to contain characters a case fold could change the reading of"
    );
    let db = fx.db();

    install(
        &fx,
        &db,
        &request("alpha", &source, &lowered.to_uppercase(), 8),
    )
    .expect("an uppercase declaration is legal input");
    assert_eq!(
        row(&db, "alpha").sha256.as_deref(),
        Some(lowered.as_str()),
        "the column holds the lowered spelling from the first install"
    );

    let again = install(&fx, &db, &request("alpha", &source, &lowered, 8))
        .expect("the lowercase spelling of the same digest is the same declaration");
    assert_eq!(again, InstallOutcome::AlreadyInstalled);
    assert_eq!(row(&db, "alpha").sha256.as_deref(), Some(lowered.as_str()));
    assert_eq!(row(&db, "alpha").installed_at, NOW);
    assert_eq!(names(&fx.models_dir()), ["alpha"]);
}

/// §7 case 8: a row whose artifact is gone lists as `missing` and repairs nothing; installing the digest the
/// row already attests re-materialises the file as `restored`, with `installed_at` and every vector left
/// alone.
#[test]
fn a_deleted_artifact_is_listed_missing_and_installed_back_as_restored() {
    let fx = Fixture::new();
    let source = fx.source("weights-v1.bin", b"weights v1");
    let digest = sha256_file(&source).expect("test setup: hash the source");
    let db = fx.db();
    install(&fx, &db, &request("alpha", &source, &digest, 4)).expect("first install");
    seed_vector(&db, "alpha", 4);
    let before = vectors(&db, "alpha");
    assert_eq!(before.len(), 1, "test fixture: one seeded vector");

    fs::remove_file(fx.artifact("alpha")).expect("test setup: delete the artifact");
    let listing =
        models::list(&db, &fx.config, &fx.workspace).expect("listing a live row is readable");
    assert_eq!(states(&listing), [("alpha", ArtifactState::Missing)]);
    assert!(
        !fx.artifact("alpha").exists(),
        "`list` names a missing artifact and does not go looking for one to recreate"
    );

    let outcome = install(&fx, &db, &request("alpha", &source, &digest, 4))
        .expect("re-installing is allowed");
    assert_eq!(outcome, InstallOutcome::Restored);
    assert_eq!(outcome.state_label(), "restored");
    assert_eq!(
        fs::read(fx.artifact("alpha")).expect("test: readable"),
        b"weights v1"
    );
    assert_eq!(
        row(&db, "alpha").installed_at,
        NOW,
        "`restored` rewrites no column"
    );
    assert_eq!(vectors(&db, "alpha"), before, "no vector was touched");
    assert_eq!(names(&fx.models_dir()), ["alpha"]);

    // And the listing agrees now.
    let after = models::list(&db, &fx.config, &fx.workspace).expect("listing after the restore");
    assert_eq!(states(&after), [("alpha", ArtifactState::Present)]);
}

/// §7 case 7 (the portable half): `<db-hash>.models` is an existing **regular file**, so directory creation
/// fails on both OSes while the database beside it stays writable. Install fails and writes no row.
#[test]
fn a_regular_file_where_the_models_dir_belongs_refuses_install_and_writes_no_row() {
    let fx = Fixture::new();
    let source = fx.source("weights-v1.bin", b"weights v1");
    let digest = sha256_file(&source).expect("test setup: hash the source");
    {
        let models_dir = fx.models_dir();
        let parent = models_dir
            .parent()
            .expect("test setup: the models dir has a parent");
        fs::create_dir_all(parent).expect("test setup: the workspaces directory");
    }
    fs::write(fx.models_dir(), b"not a directory")
        .expect("test setup: blocker file in the models dir's place");
    let db = fx.db();

    let failure = install(&fx, &db, &request("alpha", &source, &digest, 8))
        .expect_err("a regular file cannot hold an artifact directory");
    assert!(
        matches!(failure, ModelError::ModelsDir { .. }),
        "the refusal has to name the directory, not the file it could not write: {failure}"
    );
    assert!(
        failure
            .to_string()
            .contains(&fx.models_dir().display().to_string()),
        "and it has to say which path: {failure}"
    );
    assert!(db::model_row(&db, "alpha")
        .expect("test: readable")
        .is_none());
}

/// §7 case 12: the same fixture seen by `list`. The directory-level failure is the whole answer — there is no
/// [`models::Listing`] to carry a per-row verdict, and [`ArtifactState`] has exactly four values, none of them
/// "unknown", so degrading every row to `missing` is not expressible.
#[test]
fn an_unlistable_models_directory_is_reported_for_the_workspace_and_not_against_rows() {
    let fx = Fixture::new();
    let source = fx.source("weights-v1.bin", b"weights v1");
    let digest = sha256_file(&source).expect("test setup: hash the source");
    let db = fx.db();
    install(&fx, &db, &request("alpha", &source, &digest, 8)).expect("first install");

    // Replace the directory with a file, keeping the row live.
    fs::remove_dir_all(fx.models_dir()).expect("test setup: clear the models dir");
    {
        let models_dir = fx.models_dir();
        let parent = models_dir
            .parent()
            .expect("test setup: the models dir has a parent");
        fs::create_dir_all(parent).expect("test setup: the workspaces directory");
    }
    fs::write(fx.models_dir(), b"not a directory").expect("test setup: blocker file");

    let listing = models::list(&db, &fx.config, &fx.workspace);
    match listing {
        Err(ModelError::ModelsDir { path, .. }) => assert_eq!(path, fx.models_dir()),
        other => panic!(
            "an unreadable directory must be one workspace-level failure with no rows at all, got {other:?}"
        ),
    }
}

/// §7 case 10: a leftover temporary from a rename that never finished is **unclaimed** — reported under its
/// own heading, in the exact spelling §3 step 6 promises, with nothing deleted and no row disturbed.
#[test]
fn a_leftover_temporary_is_unclaimed_and_survives_the_listing() {
    let fx = Fixture::new();
    let source = fx.source("weights-v1.bin", b"weights v1");
    let digest = sha256_file(&source).expect("test setup: hash the source");
    let db = fx.db();
    install(&fx, &db, &request("alpha", &source, &digest, 8)).expect("first install");

    // `<id>.<nonce>.tmp`, planted rather than crashed-out: §3 pins the spelling so that this case is nameable.
    let leftover = "alpha.4242-1730000000123456-0.tmp";
    fs::write(fx.models_dir().join(leftover), b"half a download")
        .expect("test setup: plant a leftover temp");

    let listing =
        models::list(&db, &fx.config, &fx.workspace).expect("leftovers do not fail a listing");
    assert_eq!(states(&listing), [("alpha", ArtifactState::Present)]);
    assert_eq!(
        listing.unclaimed,
        [Unclaimed {
            name: leftover.to_string(),
            kind: UnclaimedKind::File,
        }]
    );
    assert!(
        fx.models_dir().join(leftover).exists(),
        "`list` reports an unclaimed entry; deleting it is not this command's authority"
    );
}

/// §7 case 11: a row seeded with `sha256 = NULL` — round 4b's provider with no local weights — lists as
/// `no artifact expected`. Reporting `missing` here would be a false alarm on every such row.
#[test]
fn a_row_with_no_digest_expects_no_artifact() {
    let fx = Fixture::new();
    let db = fx.db();
    assert!(
        db::insert_model(&db, "endpoint", 3072, None, "", "", NOW).expect("seed the NULL-sha row"),
        "nothing claims this id yet"
    );

    let listing = models::list(&db, &fx.config, &fx.workspace).expect("listing is readable");
    assert_eq!(
        states(&listing),
        [("endpoint", ArtifactState::NoArtifactExpected)]
    );
    // The directory was never created, and that is still not `missing`.
    assert!(!fx.models_dir().exists());
    assert!(listing.unclaimed.is_empty());
}

/// §7 case 13: two installs of one id leave exactly one row and an unchanged `installed_at`, and the second
/// writer's own insert reports that it wrote nothing — the `ON CONFLICT (id) DO NOTHING` arm, which the
/// sequential path above never reaches because §3 step 5 answers first.
#[test]
fn two_installs_of_one_id_leave_one_row_and_the_second_writer_writes_nothing() {
    let fx = Fixture::new();
    let source = fx.source("weights-v1.bin", b"weights v1");
    let digest = sha256_file(&source).expect("test setup: hash the source");
    let db = fx.db();

    install(&fx, &db, &request("alpha", &source, &digest, 8)).expect("first install");
    let after_first = row(&db, "alpha");
    assert_eq!(db::model_rows(&db).expect("test: readable").len(), 1);

    let again = install(&fx, &db, &request("alpha", &source, &digest, 8)).expect("second install");
    assert_eq!(again, InstallOutcome::AlreadyInstalled);
    let after_second = row(&db, "alpha");
    assert_eq!(after_second, after_first);

    // The arm itself: a writer that got past step 5 and finds the id taken is told `false`, not a
    // `PRIMARY KEY` violation (§6). A *different* declaration through the same helper still writes nothing.
    assert!(
        !db::insert_model(
            &db,
            "alpha",
            16,
            Some("0".repeat(64).as_str()),
            "",
            "",
            NOW + 99
        )
        .expect("the conflict arm must not raise"),
        "a conflicting insert reports that it wrote nothing"
    );
    assert_eq!(row(&db, "alpha"), after_first, "and changes nothing");
    assert_eq!(db::model_rows(&db).expect("test: readable").len(), 1);
}

/// §3 step 0's P0, as a fixture rather than an argument: the artifact directory and the database carry **one**
/// key, even for a workspace name UTF-8 cannot hold — where a derivation over `path::Display` diverges. Two
/// twin directories whose lossy spellings are identical must get two databases *and* two artifact directories.
#[cfg(unix)]
#[test]
fn the_artifact_directory_and_the_database_share_one_workspace_key() {
    let fx = Fixture::new();
    // `café` written two ways, one of them not valid UTF-8: both render as `caf\u{FFFD}`.
    let twins = [
        (
            b"caf\xe9".as_slice(),
            b"weights for the first twin".as_slice(),
        ),
        (
            b"caf\xe8".as_slice(),
            b"weights for the second twin".as_slice(),
        ),
    ];
    let mut dirs = Vec::new();
    let mut artifacts = Vec::new();

    for (name, bytes) in twins {
        use std::os::unix::ffi::OsStrExt;
        let workspace = fx._tmp.path().join(std::ffi::OsStr::from_bytes(name));
        fs::create_dir_all(&workspace).expect("test setup: create the non-UTF-8 workspace");
        let db_path = fx.config.workspace_db(&workspace);
        let models_dir = fx.config.workspace_models_dir(&workspace);
        assert_eq!(
            models_dir.file_stem(),
            db_path.file_stem(),
            "one workspace, one key: {} vs {}",
            db_path.display(),
            models_dir.display()
        );

        let source = fx.source("twin-source.bin", bytes);
        let digest = sha256_file(&source).expect("test setup: hash the twin's source");
        let db = Db::open(&db_path).expect("test setup: open the twin's database");
        models::install(
            &db,
            &fx.config,
            &workspace,
            &request("alpha", &source, &digest, 8),
            NOW,
        )
        .expect("installing into a non-UTF-8 workspace");
        assert_eq!(
            fs::read(models_dir.join("alpha")).expect("test: readable"),
            bytes
        );
        dirs.push(models_dir);
        artifacts.push(bytes);
    }

    assert_ne!(
        dirs[0], dirs[1],
        "two byte-distinct workspaces must not share one artifact directory"
    );
    for (dir, bytes) in dirs.iter().zip(artifacts) {
        assert_eq!(fs::read(dir.join("alpha")).expect("test: readable"), bytes);
    }
}

/// A workspace nothing was installed into lists empty rather than failing: an **absent** artifact directory is
/// not the same fact as an unreadable one (§3), and listing must not be what creates it.
#[test]
fn a_workspace_nothing_was_installed_into_lists_empty() {
    let fx = Fixture::new();
    let db = fx.db();

    let listing =
        models::list(&db, &fx.config, &fx.workspace).expect("an empty workspace lists cleanly");
    assert_eq!(listing.models_dir, fx.models_dir());
    assert!(listing.models.is_empty());
    assert!(listing.unclaimed.is_empty());
    assert!(!fx.models_dir().exists(), "listing creates nothing");
}

/// A subdirectory under the artifact directory is reported as one and **not recursed** (§3).
#[test]
fn a_subdirectory_is_reported_as_one_and_not_recurse() {
    let fx = Fixture::new();
    let source = fx.source("weights-v1.bin", b"weights v1");
    let digest = sha256_file(&source).expect("test setup: hash the source");
    let db = fx.db();
    install(&fx, &db, &request("alpha", &source, &digest, 8)).expect("first install");

    fs::create_dir(fx.models_dir().join("somebody-elses")).expect("test setup: subdirectory");
    fs::write(
        fx.models_dir().join("somebody-elses").join("inner.bin"),
        b"x",
    )
    .expect("test setup: file inside the subdirectory");

    let listing = models::list(&db, &fx.config, &fx.workspace)
        .expect("listing tolerates a stranger's directory");
    assert_eq!(
        listing.unclaimed,
        [Unclaimed {
            name: "somebody-elses".to_string(),
            kind: UnclaimedKind::Directory,
        }],
        "the directory is named as one and nothing below it appears"
    );
}

/// A name UTF-8 cannot hold renders with replacements, the way [`np_core::walk::UnknownPath`] does (§3). The
/// entry is still reported; a listing that dropped it would claim the directory held nothing strange.
#[cfg(unix)]
#[test]
fn an_unclaimed_name_that_is_not_utf8_renders_lossily() {
    use std::os::unix::ffi::OsStrExt;

    let fx = Fixture::new();
    let source = fx.source("weights-v1.bin", b"weights v1");
    let digest = sha256_file(&source).expect("test setup: hash the source");
    let db = fx.db();
    install(&fx, &db, &request("alpha", &source, &digest, 8)).expect("first install");

    fs::write(
        fx.models_dir()
            .join(std::ffi::OsStr::from_bytes(b"stray\xff")),
        b"x",
    )
    .expect("test setup: plant a non-UTF-8 leftover");

    let listing = models::list(&db, &fx.config, &fx.workspace)
        .expect("a strange name does not fail the listing");
    assert_eq!(
        listing.unclaimed,
        [Unclaimed {
            name: "stray\u{fffd}".to_string(),
            kind: UnclaimedKind::File,
        }]
    );
}

/// §3 step 0 with teeth: a workspace operand that is not a directory that exists is refused **before any
/// write**, so a typo cannot mint an empty database and an artifact directory no scan will ever read.
#[test]
fn a_workspace_that_does_not_exist_is_refused_before_anything_is_written() {
    let fx = Fixture::new();
    let source = fx.source("weights-v1.bin", b"weights v1");
    let digest = sha256_file(&source).expect("test setup: hash the source");
    let typo = fx.missing_workspace();
    let db = fx.db();

    for failure in [
        models::install(
            &db,
            &fx.config,
            &typo,
            &request("alpha", &source, &digest, 8),
            NOW,
        )
        .expect_err("a workspace that is not there cannot host an install"),
        models::list(&db, &fx.config, &typo).expect_err("nor a listing"),
    ] {
        assert!(
            matches!(failure, ModelError::Workspace { .. }),
            "the refusal has to be about the workspace: {failure}"
        );
        assert!(
            failure.to_string().contains(&typo.display().to_string()),
            "and name the path it refused: {failure}"
        );
    }
    // The hash-derived names a typo would have minted do not exist.
    assert!(!fx.config.workspace_models_dir(&typo).exists());
    assert!(!fx.config.workspace_db(&typo).exists());

    // A regular file is refused too — "exists" is not the test, "is a directory" is (§3 step 0).
    let file = fx.source("plain-file", b"not a workspace");
    match models::require_workspace_dir(&file) {
        Err(ModelError::Workspace { .. }) => {}
        other => panic!("a regular file is not a workspace, got {other:?}"),
    }
}

/// The wire spellings §3 pins for `--json`'s `"state"` field, named in the library and asserted here so that
/// renaming one is a failing test rather than a silent protocol change.
#[test]
fn the_state_labels_are_the_ones_the_design_pins() {
    assert_eq!(InstallOutcome::Installed.state_label(), "installed");
    assert_eq!(
        InstallOutcome::AlreadyInstalled.state_label(),
        "already installed"
    );
    assert_eq!(InstallOutcome::Restored.state_label(), "restored");
    assert_eq!(ArtifactState::Present.state_label(), "present");
    assert_eq!(ArtifactState::Missing.state_label(), "missing");
    assert_eq!(
        ArtifactState::NoArtifactExpected.state_label(),
        "no artifact expected"
    );
    assert_eq!(ArtifactState::Mismatch.state_label(), "mismatch");
}

/// An artifact that is there but does not hash to its row is `mismatch` — reported, never overwritten. The
/// bytes on disk afterwards are the stranger's, which is the whole point: `list` has no repair authority.
#[test]
fn a_tampered_artifact_is_a_mismatch_and_is_left_alone() {
    let fx = Fixture::new();
    let source = fx.source("weights-v1.bin", b"weights v1");
    let digest = sha256_file(&source).expect("test setup: hash the source");
    let db = fx.db();
    install(&fx, &db, &request("alpha", &source, &digest, 8)).expect("first install");

    fs::write(fx.artifact("alpha"), b"somebody edited 540 MB").expect("test setup: tamper");
    let listing =
        models::list(&db, &fx.config, &fx.workspace).expect("a mismatch is still a listing");
    assert_eq!(states(&listing), [("alpha", ArtifactState::Mismatch)]);
    assert_eq!(
        fs::read(fx.artifact("alpha")).expect("test: readable"),
        b"somebody edited 540 MB"
    );

    // Installing the honest source again is refused rather than allowed to overwrite (§3 step 6).
    let failure = install(&fx, &db, &request("alpha", &source, &digest, 8))
        .expect_err("a row whose artifact was tampered with is not silently rewritten");
    // The refusal names the artifact and both digests (§3 step 6): tuples agree, so what this call found is a
    // file that no longer matches its own row. Killed by removing the verification from install's
    // already-installed arm — without it the call answers `already installed` over a rotted artifact.
    match failure {
        ModelError::ArtifactDisagrees {
            path,
            expected,
            found,
        } => {
            assert!(
                path.ends_with("alpha"),
                "the refusal is about the installed artifact, not the source: {path:?}"
            );
            assert_ne!(
                expected, found,
                "and it states the disagreement it observed"
            );
        }
        other => panic!("an artifact that disagrees with its row is refused by name, got {other}"),
    }
    assert_eq!(
        fs::read(fx.artifact("alpha")).expect("test: read the artifact"),
        b"somebody edited 540 MB",
        "the refusal left the disagreeing artifact exactly as it found it"
    );
}

/// §7 case 7's additional `#[cfg(unix)]` half: an artifact directory that exists but refuses writes. The
/// refusal lands on the temporary write, and no row is left naming an artifact that was never written.
#[cfg(unix)]
#[test]
fn an_unwritable_models_directory_is_refused_on_unix() {
    use std::os::unix::fs::PermissionsExt;

    let fx = Fixture::new();
    let source = fx.source("weights-v1.bin", b"weights v1");
    let digest = sha256_file(&source).expect("test setup: hash the source");
    {
        let models_dir = fx.models_dir();
        let parent = models_dir
            .parent()
            .expect("test setup: the models dir has a parent");
        fs::create_dir_all(parent).expect("test setup: the workspaces directory");
    }
    fs::create_dir(fx.models_dir()).expect("test setup: create the models dir");
    fs::set_permissions(fx.models_dir(), fs::Permissions::from_mode(0o500))
        .expect("test setup: chmod 500 the models dir");
    let db = fx.db();

    let failure = install(&fx, &db, &request("alpha", &source, &digest, 8))
        .err()
        .unwrap_or_else(|| {
            // Restore first, or the tempdir cleaner inherits a directory it cannot remove.
            let _ = fs::set_permissions(fx.models_dir(), fs::Permissions::from_mode(0o700));
            panic!("a models directory that refuses writes has to fail the install");
        });
    fs::set_permissions(fx.models_dir(), fs::Permissions::from_mode(0o700))
        .expect("test: restore the models dir");

    assert!(
        matches!(failure, ModelError::Io { .. }),
        "the failure is the temp write inside that directory: {failure}"
    );
    assert!(db::model_row(&db, "alpha")
        .expect("test: readable")
        .is_none());
    assert_eq!(
        names(&fx.models_dir()),
        [] as [String; 0],
        "and no corpse either"
    );

    // §7 case 12's fixture, and it needs **its own** mode: 0500 refuses writes but still grants read, so
    // `read_dir` succeeds there and a list built on that premise would pass without ever testing the state.
    // Taking the read bit away is what makes the directory unreadable.
    fs::set_permissions(fx.models_dir(), fs::Permissions::from_mode(0o300))
        .expect("test: take the read bit away");
    let unreadable = models::list(&db, &fx.config, &fx.workspace);
    fs::set_permissions(fx.models_dir(), fs::Permissions::from_mode(0o700))
        .expect("test: restore the models dir again");
    match unreadable {
        Err(ModelError::ModelsDir { .. }) => {}
        other => panic!("an unreadable directory is a workspace-level failure, got {other:?}"),
    }
}

// ---------------------------------------------------------------------------
// R7a — the fetch marker: reserved names, reading them, reporting them
// (`docs/design-m2-local-embeddings.md` §12 rules 2, 3 and 4).
//
// **No writer exists.** Nothing in this round can create a marker, so every marker below is planted by hand —
// which is also why the reading half is testable at all before R7b's network path exists. Nothing here dials
// out: these fixtures touch one tempdir and nothing else.
// ---------------------------------------------------------------------------

/// Plant bytes at one exact name inside the workspace's artifact directory, creating the directory (a fetch
/// writes its marker into a directory that `install` would otherwise have made).
fn plant(fx: &Fixture, name: &str, bytes: &[u8]) -> PathBuf {
    fs::create_dir_all(fx.models_dir()).expect("test setup: the models dir");
    let path = fx.models_dir().join(name);
    fs::write(&path, bytes).expect("test setup: plant the file");
    path
}

/// Plant a marker for `id` holding `body`, at the path **the library** derives.
///
/// Spelling `.fetching.<id>.json` out here instead would let a test pass against a build whose reader and
/// writer disagree — the same argument this file already makes about never spelling the artifact directory.
fn plant_marker(fx: &Fixture, id: &str, body: &[u8]) -> PathBuf {
    let path = models::marker_path(&fx.models_dir(), id);
    fs::create_dir_all(fx.models_dir()).expect("test setup: the models dir");
    fs::write(&path, body).expect("test setup: plant the marker");
    path
}

/// The one marker body these fixtures plant when the fields are not the subject.
const MARKER_BODY: &[u8] =
    br#"{"nonce":"4242-1762000000123456789-0","started_at":1762000000,"bytes_done":43970560,"bytes_total":324839936}"#;

/// The fix against the bug it replaces (§11's addendum, §12 rule 2).
///
/// The first draft named the marker `<id>.fetching.json`, and that is a **legal model id** — so row `alpha`'s
/// artifact path and row `alpha`'s marker path were the same file, and writing a marker could overwrite a
/// verified artifact. Both halves are pinned here rather than argued in prose: the leading dot is refused as an
/// id by the first-character rule, and the old spelling is accepted as an id by the very charset that makes it
/// dangerous.
#[test]
fn no_model_id_can_name_a_fetch_file_and_the_old_scheme_could() {
    for name in [".fetching.alpha.json", ".part.alpha.1-2-3"] {
        let failure = models::validate_id(name).expect_err(&format!(
            "`{name}` starts with a dot, so no id can be spelled that way"
        ));
        assert!(
            matches!(&failure, ModelError::InvalidId { rule, .. } if rule.contains("first character")),
            "the refusal has to name the rule that makes these names unforgeable: {failure}"
        );
    }

    // The asymmetry itself: `alpha.fetching.json` satisfies `[a-z0-9][a-z0-9._@-]{0,63}` and installs. That is
    // not a hole to patch in `validate_id` — ids allow dots on purpose — it is the reason the fetch names do
    // not start where ids can.
    models::validate_id("alpha.fetching.json")
        .expect("the old scheme's name really was a legal id");
}

/// The reserved shapes, accepted — including the round trip from [`models::marker_path`], which is what keeps
/// the reader and R7b's writer from drifting apart silently.
#[test]
fn the_recogniser_accepts_exactly_the_two_reserved_shapes() {
    let fx = Fixture::new();
    let cases: &[(&str, Option<models::FetchName<'_>>)] = &[
        (
            ".fetching.alpha.json",
            Some(models::FetchName::Marker("alpha")),
        ),
        // Ids carry dots, `@`, `-`, `_` and digits (§2), so the recogniser has to survive all of them.
        (
            ".fetching.embeddinggemma-2-text-q8_0@1.json",
            Some(models::FetchName::Marker("embeddinggemma-2-text-q8_0@1")),
        ),
        (".fetching.a.b.json", Some(models::FetchName::Marker("a.b"))),
        (
            ".part.alpha.4242-1730000000123456789-0",
            Some(models::FetchName::Part("alpha")),
        ),
        // `<id>.<nonce>` splits at the **last** dot, because `a.b` is an id and not two fields.
        (".part.a.b.1-2-3", Some(models::FetchName::Part("a.b"))),
        // Nothing else in the directory is a fetch name — including a row's own artifact and install's temp.
        ("alpha", None),
        ("alpha.4242-1730000000123456789-0.tmp", None),
    ];
    for (name, expected) in cases {
        assert_eq!(
            models::fetch_name(OsStr::new(name)),
            *expected,
            "`{name}` was classified wrongly"
        );
    }

    // Round trip: every id shape `validate_id` accepts is recognised back from the path the library builds.
    for id in [
        "a",
        "alpha",
        "0",
        "embeddinggemma-2-text-q8_0@1",
        "a.b.c",
        "x-y_z",
    ] {
        models::validate_id(id).expect("test: these are legal ids");
        let path = models::marker_path(&fx.models_dir(), id);
        let name = path
            .file_name()
            .unwrap_or_else(|| panic!("test: `{path:?}` has a file name"));
        assert_eq!(
            models::fetch_name(name),
            Some(models::FetchName::Marker(id)),
            "`{name:?}` must read back as the marker of `{id}`"
        );
    }
}

/// The near-misses, refused — each one a name that would either invent a fetch for an id that cannot exist, or
/// hide a real junk file from `unclaimed`.
#[test]
fn the_recogniser_refuses_every_near_miss_of_either_shape() {
    for name in [
        // No id, or an empty one.
        ".fetching.json",
        ".fetching..json",
        ".part.x",
        ".part.",
        // Not a prefix of this program.
        "x.fetching.json",
        "fetching.alpha.json",
        ".fetchin.alpha.json",
        // Byte-exact and case-sensitive: an uppercase name is not one of ours, and neither is an id that could
        // never install. `validate_id` refusing `A` is what makes `.fetching.A.json` junk rather than a fetch
        // of a model no row can ever name.
        ".FETCHING.a.json",
        ".Part.a.1-0",
        ".fetching.A.json",
        ".fetching.a b.json",
        // Missing half.
        ".fetching.a",
        ".fetching.a.txt",
        ".part.alpha",
        ".part.alpha.",
        // A path, not an entry name: `read_dir` never hands one over, so this is the fence against a caller
        // that passes a path where it meant a file name.
        ".fetching.a/b.json",
        ".fetching.../../etc.json",
        ".part.a\\b.1-0",
        // **The two cases where only the separator guard can refuse.** Above, a separator also breaks
        // `validate_id`'s charset, so deleting that guard changes nothing observable — which is how an unpinned
        // defence hides. Here the id is legal (`x`, `a`) and the separator sits in the **nonce**, which is
        // deliberately not charset-checked: without the guard these read as fetches of `x` and `a`.
        ".part.x.y/z",
        ".part.a.b\\c",
        // The empty name and a plain dot.
        "",
        ".",
    ] {
        assert_eq!(
            models::fetch_name(OsStr::new(name)),
            None,
            "`{name}` is not one of the two reserved shapes"
        );
    }

    // Pinned rather than assumed: a temporary for a **marker** rewrite (`.fetching.<id>.json.<nonce>.tmp`, which
    // §12 rule 2 forecasts) is *not* recognised today, because the suffix rule that makes `.fetching.a.json` a
    // marker also refuses anything hanging off it. R7a writes no marker, so this name cannot exist yet; when
    // R7b rewrites one atomically it must either choose a spelling this recogniser already covers or extend
    // this test deliberately. Leaving it unrecognised means such a temp is reported under `unclaimed` — noisy,
    // but never invisible, which is the posture §12 rule 2 keeps.
    assert_eq!(
        models::fetch_name(OsStr::new(".fetching.a.json.1-0.tmp")),
        None
    );
}

/// Reading a marker yields §12's four fields typed, and both spellings of "no size announced" mean the same
/// thing: unknown, never zero (§12 rule 12).
#[test]
fn fetch_state_reads_the_fields_and_both_spellings_of_unknown_size() {
    let fx = Fixture::new();

    // Nothing planted at all: absent, which is the ordinary state of a workspace with no fetch running.
    assert_eq!(
        models::fetch_state(&fx.models_dir(), "alpha"),
        models::FetchState::Absent
    );

    plant_marker(&fx, "alpha", MARKER_BODY);
    let read = models::fetch_state(&fx.models_dir(), "alpha");
    let models::FetchState::Present(fields) = read else {
        panic!("a complete marker reads as present, got {read:?}");
    };
    assert_eq!(
        fields,
        FetchFields {
            nonce: "4242-1762000000123456789-0".to_string(),
            started_at: 1_762_000_000,
            bytes_done: 43_970_560,
            bytes_total: Some(324_839_936),
        }
    );

    // `"bytes_total": null` and an absent field are the same fact (§12 rule 12).
    for body in [
        r#"{"nonce":"n","started_at":1,"bytes_done":2,"bytes_total":null}"#,
        r#"{"nonce":"n","started_at":1,"bytes_done":2}"#,
    ] {
        plant_marker(&fx, "beta", body.as_bytes());
        let read = models::fetch_state(&fx.models_dir(), "beta");
        let models::FetchState::Present(fields) = read else {
            panic!("{body} reads as present, got {read:?}");
        };
        assert_eq!(fields.bytes_total, None, "for {body}");
        assert_eq!(fields.nonce, "n");
    }

    // Field order and insignificant whitespace carry no meaning, so a marker written by a different build in a
    // different order still reads.
    plant_marker(
        &fx,
        "gamma",
        b"{\n  \"bytes_done\" : 7,\n  \"nonce\":\"z\",\n  \"started_at\": -1\n}\n",
    );
    let read = models::fetch_state(&fx.models_dir(), "gamma");
    let models::FetchState::Present(fields) = read else {
        panic!("a reordered marker reads, got {read:?}");
    };
    assert_eq!(
        fields,
        FetchFields {
            nonce: "z".to_string(),
            // `started_at` is **displayed**, never compared with a clock (§12 rule 4), so a value this program
            // would never write is still reported rather than refused: refusing it would invent a rule the
            // design does not state.
            started_at: -1,
            bytes_done: 7,
            bytes_total: None,
        }
    );
}

/// A marker that will not parse is **reported**, with a reason that says what was wrong — never read as absent,
/// which would make a truncated write look like "no fetch is running".
#[test]
fn a_marker_that_will_not_parse_is_reported_with_a_reason() {
    let fx = Fixture::new();
    // `(body, a substring the reason must name)`. `""` means "any non-empty reason".
    let cases: &[(&str, &str)] = &[
        // Truncated mid-write — the shape of a fetch killed while flushing.
        (r#"{"nonce":"n","started_at":1,"bytes_done":2,"#, ""),
        ("", "object"),
        ("{", ""),
        ("[]", "object"),
        (r#"[{"nonce":"n"}]"#, "object"),
        // A required field absent, and a field the parser has never heard of.
        (r#"{"nonce":"n","started_at":1}"#, "bytes_done"),
        (r#"{"started_at":1,"bytes_done":2}"#, "nonce"),
        (
            r#"{"nonce":"n","started_at":1,"bytes_done":2,"member":"mmproj.gguf"}"#,
            "member",
        ),
        // Two values for one field is a contradiction, not a last-one-wins choice.
        (
            r#"{"nonce":"a","nonce":"b","started_at":1,"bytes_done":2}"#,
            "twice",
        ),
        // Wrong kind per field: the shape of "we read the wrong thing" rather than "there was nothing".
        (r#"{"nonce":5,"started_at":1,"bytes_done":2}"#, "nonce"),
        (
            r#"{"nonce":"n","started_at":"yesterday","bytes_done":2}"#,
            "started_at",
        ),
        (
            r#"{"nonce":"n","started_at":1,"bytes_done":"41 MB"}"#,
            "bytes_done",
        ),
        // `null` is legal for `bytes_total` only.
        (
            r#"{"nonce":"n","started_at":1,"bytes_done":null}"#,
            "bytes_done",
        ),
        (r#"{"nonce":null,"started_at":1,"bytes_done":2}"#, "nonce"),
        // A number that is not whole, or not a number: read as whatever a cast gives, each one would put a
        // fabricated byte count in front of a human (§12 rule 12).
        (
            r#"{"nonce":"n","started_at":1,"bytes_done":41.9}"#,
            "bytes_done",
        ),
        (
            r#"{"nonce":"n","started_at":1,"bytes_done":1e7}"#,
            "bytes_done",
        ),
        (
            r#"{"nonce":"n","started_at":1,"bytes_done":-5}"#,
            "bytes_done",
        ),
        (
            r#"{"nonce":"n","started_at":1,"bytes_done":0x10}"#,
            "bytes_done",
        ),
        (
            r#"{"nonce":"n","started_at":1,"bytes_done":+5}"#,
            "bytes_done",
        ),
        // Grammar a strict reader refuses.
        (r#"{"nonce":"n","started_at":1,"bytes_done":2,}"#, ","),
        (
            r#"{"nonce":"n","started_at":1,"bytes_done":2} trailing"#,
            "",
        ),
        (r#"{nonce:"n"}"#, "quoted"),
    ];
    for (index, (body, expected)) in cases.iter().enumerate() {
        let id = format!("id{index}");
        plant_marker(&fx, &id, body.as_bytes());
        let read = models::fetch_state(&fx.models_dir(), &id);
        let models::FetchState::Unreadable { path, reason } = read else {
            panic!("`{body}` must be reported as unreadable, got {read:?}");
        };
        assert_eq!(path, models::marker_path(&fx.models_dir(), &id));
        assert!(!reason.is_empty(), "`{body}` refused with no reason");
        assert!(
            expected.is_empty() || reason.contains(expected),
            "`{body}` reported as {reason:?}, which does not name `{expected}`"
        );
    }

    // Non-UTF-8 **content** (a name that is not UTF-8 is the `unclaimed` case, already pinned above) and a
    // backslash escape: both arrive as refusals naming what they are.
    plant_marker(
        &fx,
        "raw",
        b"{\"nonce\":\"a\xff\",\"started_at\":1,\"bytes_done\":2}",
    );
    let raw = models::fetch_state(&fx.models_dir(), "raw");
    assert!(
        matches!(&raw, models::FetchState::Unreadable { reason, .. } if reason.contains("UTF-8")),
        "bytes that are not UTF-8 have to say so: {raw:?}"
    );
    plant_marker(
        &fx,
        "escaped",
        br#"{"nonce":"a\nb","started_at":1,"bytes_done":2}"#,
    );
    assert!(
        matches!(&models::fetch_state(&fx.models_dir(), "escaped"), models::FetchState::Unreadable { reason, .. } if reason.contains("escape")),
        "an escape means this file did not come from this program, and the reason has to say that"
    );
}

/// A name that cannot be read at all — a directory wearing a marker's name, or a file too big to be one — is
/// `Unreadable`, not `Absent`. "Cannot look" must never read as "nothing there".
#[test]
fn a_marker_that_cannot_be_read_at_all_is_unreadable_not_absent() {
    let fx = Fixture::new();
    fs::create_dir_all(fx.models_dir()).expect("test setup: the models dir");

    // A directory at the marker path. Which OS-level error produces that is platform business and **not claimed
    // here** (CI cannot start jobs, so no Windows behaviour is earned — roadmap §4's Windows note): the OS may
    // refuse the open or refuse the read, and this program now refuses even to open it. Either way the one claim
    // both platforms can keep is pinned: reported, with a reason, never treated as absent.
    let path = models::marker_path(&fx.models_dir(), "alpha");
    fs::create_dir(&path).expect("test setup: a directory at the marker path");
    let dir_as_marker = models::fetch_state(&fx.models_dir(), "alpha");
    assert!(
        matches!(dir_as_marker, models::FetchState::Unreadable { .. }),
        "a directory under a marker's name is not a fetch report, got {dir_as_marker:?}"
    );

    // Larger than any marker can be: refused at the bound rather than loaded to discover it is not one.
    fs::remove_dir(&path).expect("test setup: clear the directory");
    let oversized = vec![b' '; 64 * 1024 + 8];
    fs::write(&path, &oversized).expect("test setup: write an oversized marker");
    let read = models::fetch_state(&fx.models_dir(), "alpha");
    assert!(
        matches!(&read, models::FetchState::Unreadable { reason, .. } if reason.contains("larger than")),
        "an oversized file has to name the bound it broke: {read:?}"
    );
}

/// §12 rule 3's section, end to end at the seam np-cli calls: an id with **no row at all** is reported (the
/// first-start case the human asked about), and neither reserved name is announced as junk.
#[test]
fn a_fetch_is_listed_for_an_id_with_no_row_and_never_reported_as_unclaimed() {
    let fx = Fixture::new();
    let source = fx.source("weights-v1.bin", b"weights v1");
    let digest = sha256_file(&source).expect("test setup: hash the source");
    let db = fx.db();
    install(&fx, &db, &request("alpha", &source, &digest, 8)).expect("first install");

    // A fetch of a second model that has no row yet — exactly the state `np models fetch` is in for its first
    // byte — plus the partial download it is writing, plus one piece of genuine junk.
    plant_marker(&fx, "beta", MARKER_BODY);
    plant(&fx, ".part.beta.4242-1762000000123456789-0", b"half of it");
    plant(&fx, "notes-backup.txt", b"not a model thing at all");

    let listing =
        models::list(&db, &fx.config, &fx.workspace).expect("a fetch never fails a listing");

    assert_eq!(
        listing
            .fetching
            .iter()
            .map(|entry| entry.id.as_str())
            .collect::<Vec<_>>(),
        ["beta"],
        "the marker of an id no row names still gets reported"
    );
    let models::FetchState::Present(fields) = &listing.fetching[0].state else {
        panic!(
            "the planted marker parses, got {:?}",
            listing.fetching[0].state
        );
    };
    assert_eq!(fields.bytes_total, Some(324_839_936));
    assert_eq!(
        listing.fetching[0].path,
        models::marker_path(&fx.models_dir(), "beta")
    );

    // The rows answer what they answered before this section existed: a fetch is orthogonal to `ArtifactState`,
    // which is why there are four arms and not five.
    assert_eq!(states(&listing), [("alpha", ArtifactState::Present)]);

    // Neither reserved name is junk, and the real leftover still is.
    assert_eq!(
        listing
            .unclaimed
            .iter()
            .map(|entry| entry.name.as_str())
            .collect::<Vec<_>>(),
        ["notes-backup.txt"],
        "a fetch that was going to be announced as garbage is the defect §11's addendum records"
    );
}

/// §12 rule 3's *other* half: a model that is already installed can be re-fetching. The row keeps its own verdict
/// and the fetch appears beside it — which is exactly where a refactor is tempted to "resolve" the overlap by
/// dropping one of them, so it is pinned rather than left to the module doc's sentence about orthogonality.
#[test]
fn a_re_fetch_of_an_installed_model_shows_the_row_and_the_fetch_together() {
    let fx = Fixture::new();
    let source = fx.source("weights-v1.bin", b"weights v1");
    let digest = sha256_file(&source).expect("test setup: hash the source");
    let db = fx.db();
    install(&fx, &db, &request("alpha", &source, &digest, 8)).expect("first install");

    plant_marker(&fx, "alpha", MARKER_BODY);

    let listing =
        models::list(&db, &fx.config, &fx.workspace).expect("a re-fetch never fails a listing");
    assert_eq!(
        states(&listing),
        [("alpha", ArtifactState::Present)],
        "the artifact keeps its own verdict while a newer copy is arriving"
    );
    assert_eq!(
        listing
            .fetching
            .iter()
            .map(|entry| entry.id.as_str())
            .collect::<Vec<_>>(),
        ["alpha"],
        "and the fetch is reported beside it, not instead of it"
    );
    assert!(
        listing.unclaimed.is_empty(),
        "the marker is a recognised name: {:?}",
        listing.unclaimed
    );
}

/// The read bound is pinned at both edges: a marker of exactly `MARKER_MAX_BYTES` is read (it is a marker, and the
/// bound exists to refuse *oversized* files), and one byte more is refused. Without the exact-size case, `>` and
/// `>=` are indistinguishable — a 64 KiB marker would silently flip from `Present` to `Unreadable`.
#[test]
fn a_marker_of_exactly_the_read_bound_is_read_and_one_byte_more_is_not() {
    let fx = Fixture::new();
    // The nonce is opaque and unchecked, so it is the honest place to grow a marker to a chosen size.
    let fixed = r#"{"nonce":"","started_at":1,"bytes_done":1}"#.len();
    let bound = 64 * 1024;

    let exact = format!(
        r#"{{"nonce":"{}","started_at":1,"bytes_done":1}}"#,
        "n".repeat(bound - fixed)
    );
    assert_eq!(exact.len(), bound, "test setup: exactly at the bound");
    plant_marker(&fx, "alpha", exact.as_bytes());
    let read = models::fetch_state(&fx.models_dir(), "alpha");
    assert!(
        matches!(read, models::FetchState::Present(_)),
        "a marker of exactly the bound is a marker, got {read:?}"
    );

    let over = format!(
        r#"{{"nonce":"{}","started_at":1,"bytes_done":1}}"#,
        "n".repeat(bound - fixed + 1)
    );
    assert_eq!(over.len(), bound + 1, "test setup: one past the bound");
    plant_marker(&fx, "alpha", over.as_bytes());
    let read = models::fetch_state(&fx.models_dir(), "alpha");
    assert!(
        matches!(&read, models::FetchState::Unreadable { reason, .. } if reason.contains("larger than")),
        "one byte past the bound is refused by name: {read:?}"
    );
}

/// An empty nonce names no `.part` file — `fetch_name` refuses `.part.<id>.` outright — so a marker carrying one is
/// unreadable rather than a link to nothing. Without this, R7b could write one spelling and read the other.
#[test]
fn an_empty_nonce_is_refused_because_it_names_no_partial_download() {
    let fx = Fixture::new();
    plant_marker(
        &fx,
        "alpha",
        br#"{"nonce":"","started_at":1,"bytes_done":1}"#,
    );
    let read = models::fetch_state(&fx.models_dir(), "alpha");
    assert!(
        matches!(&read, models::FetchState::Unreadable { reason, .. }
            if reason.contains("nonce") && reason.contains("empty")),
        "an empty nonce is refused with a sentence naming the field: {read:?}"
    );
}

/// A malformed marker keeps its place in the listing, keeps its bytes on disk, and does not fail the call —
/// the F22/F23 posture applied to a file this program wrote but cannot read back.
#[test]
fn an_unreadable_marker_is_listed_and_left_on_disk() {
    let fx = Fixture::new();
    let db = fx.db();
    let body = br#"{"nonce":"n","started_at":1762000000,"bytes_done"#;
    let path = plant_marker(&fx, "alpha", body);

    let listing =
        models::list(&db, &fx.config, &fx.workspace).expect("a broken marker is data, not a fault");
    assert_eq!(listing.fetching.len(), 1);
    assert_eq!(listing.fetching[0].id, "alpha");
    let models::FetchState::Unreadable { reason, .. } = &listing.fetching[0].state else {
        panic!(
            "a truncated marker reports itself unreadable, got {:?}",
            listing.fetching[0].state
        );
    };
    assert!(!reason.is_empty());

    // Still there, byte for byte. Nothing in this round deletes a marker: §12 rule 4 makes liveness consent,
    // and an implicit cleanup here would be `list` deciding a download nobody watched was worthless.
    assert_eq!(
        fs::read(&path).expect("test: the marker is still readable"),
        body,
        "`list` reports a marker it cannot parse; deleting it is not this command's authority"
    );
}

/// A workspace nothing was installed into has no artifact directory at all. That is not an unreadable
/// directory (§3's distinction), and it lists as no fetches rather than as a failure.
#[test]
fn a_workspace_with_no_models_directory_lists_no_fetch() {
    let fx = Fixture::new();
    let db = fx.db();

    let listing =
        models::list(&db, &fx.config, &fx.workspace).expect("an absent directory is listable");
    assert!(listing.fetching.is_empty());
    assert!(listing.unclaimed.is_empty());
}

#[test]
fn installing_records_the_primary_member_by_name() {
    // Migration 9's writer. `models.sha256` keeps meaning "the **primary** artifact's digest" (§12 rule 10), and this
    // is where a reader learns that the same bytes are also `model_files(name = 'main')` — the name a fetch will use
    // for the same file, so install-then-fetch cannot record one artifact twice under two spellings.
    let fx = Fixture::new();
    let db = fx.db();
    let bytes = b"an artifact small enough to test with".to_vec();
    let source = fx.source("artifact.gguf", &bytes);
    let digest = sha256_bytes(&bytes);

    install(&fx, &db, &request("alpha", &source, &digest, 4)).expect("install");

    assert_eq!(
        db::model_files(&db, "alpha").expect("read the members back"),
        [db::ModelFile {
            name: models::MAIN_FILE.to_string(),
            sha256: digest.clone(),
            size: bytes.len() as i64,
        }],
        "`main` is recorded with the same digest the row carries"
    );
}

#[test]
fn a_repeated_install_records_the_member_once_and_still_writes_no_columns() {
    // §7 case 5's contract ("repeating a declaration rewrites nothing") holds for the child table too: the Already
    // Installed arm hashes nothing and records nothing, so the member row is whatever the first install wrote. A
    // second row, or a refreshed `size`, would mean this command's answer to "nothing changed" was a lie.
    let fx = Fixture::new();
    let db = fx.db();
    let bytes = b"an artifact small enough to test with".to_vec();
    let source = fx.source("artifact.gguf", &bytes);
    let digest = sha256_bytes(&bytes);

    install(&fx, &db, &request("alpha", &source, &digest, 4)).expect("first install");
    let first = db::model_files(&db, "alpha").expect("members after the first install");
    match install(&fx, &db, &request("alpha", &source, &digest, 4)) {
        Ok(InstallOutcome::AlreadyInstalled) => {}
        other => panic!("the second call must change nothing, got {other:?}"),
    }
    assert_eq!(
        db::model_files(&db, "alpha").expect("members after the second install"),
        first
    );
}

#[test]
fn a_marker_mid_rewrite_is_recognised_and_never_called_unclaimed() {
    // §12 rule 2's last sentence, made executable: the atomic marker rewrite passes through
    // `.fetching.<id>.json.tmp<nonce>`, and `unclaimed` has to keep meaning "nothing this program recognises".
    // Without this arm a fetch would show its own bookkeeping as junk someone ought to delete.
    for (name, expected) in [
        (".fetching.alpha.json.tmp1-2-3", Some("alpha")),
        // Ids contain dots, so the split happens at the **last** `.json.` — same rule the marker itself reads by.
        (".fetching.a.b.json.tmp9", Some("a.b")),
        (".fetching.alpha.json.backup", None),
        (".fetching..json.tmp1", None),
        // No `tmp` prefix, or no id at all, is not a temp name this program writes. The marker itself
        // (`.fetching.alpha.json`) is pinned as [`FetchName::Marker`] by the case above — it must keep answering as
        // that, and this loop says nothing about it either way.
    ] {
        let found = models::fetch_name(std::ffi::OsStr::new(name));
        let wanted = expected.map(models::FetchName::MarkerTemp);
        assert_eq!(found, wanted, "{name}");
    }

    // And the listing agrees: a directory holding only a marker mid-rewrite has nothing to report — no fetch (the
    // real marker is absent) and nothing unclaimed.
    let fx = Fixture::new();
    let db = fx.db();
    let models_dir = fx.models_dir();
    fs::create_dir_all(&models_dir).expect("test setup: models dir");
    fs::write(models_dir.join(".fetching.alpha.json.tmp1-2-3"), b"{}")
        .expect("test setup: marker temp");

    let listing = models::list(&db, &fx.config, &fx.workspace).expect("listing");
    assert!(listing.models.is_empty());
    assert!(
        listing.fetching.is_empty(),
        "a marker mid-rewrite says nothing about whether a fetch runs (§12 rule 3)"
    );
    assert!(
        listing.unclaimed.is_empty(),
        "a recognised transient name must not reach `unclaimed`: {:?}",
        listing.unclaimed
    );
}
