//! The media pass inside `np embed`: §7's rules for images, executed against a fake provider that records every
//! encode (`docs/design-m2-image-embeddings.md` §6–§8).
//!
//! The corpus is real — notes and figures are written to a tempdir and the notes indexed by [`np_core::scan`] — because
//! half of what is under test is the liveness fence ([`db::live_note_paths`]) and the other half is the filesystem:
//! which bytes were opened, how often, and what a rename did to the row that pointed at them. The *provider* is the fake,
//! so no case needs `local-model` or an mmproj, and "the second run encoded nothing" is an assertion about a recorded
//! call log rather than a hope.

use std::collections::BTreeSet;
use std::fs;
use std::io::Write;
use std::path::{Path, PathBuf};
use std::sync::Mutex;

use np_core::config::{Config, EmbeddingProvider, EmbeddingsConfig};
use np_core::db::{self, Db};
use np_core::embed::{EmbedError, Embedder, Vector};
use np_core::embed_run::{embed, EmbedOutcome};
use np_core::hash::sha256_bytes;
use np_core::media::{self, MediaRefusal};
use np_core::scan;
use tempfile::TempDir;

/// Width the fake declares and produces.
const DIM: usize = 3;
/// The model id every case installs and embeds against.
const MODEL_ID: &str = "fake-vision@1";

// ---------------------------------------------------------------------------
// fixtures
// ---------------------------------------------------------------------------

/// A scanned workspace plus its data dir, both kept alive by one tempdir.
struct Setup {
    data_dir: PathBuf,
    _tmp: TempDir,
    root: PathBuf,
}

impl Setup {
    fn cfg(&self) -> Config {
        Config {
            data_dir: self.data_dir.clone(),
            embeddings: EmbeddingsConfig {
                provider: EmbeddingProvider::LocalModel,
                model: Some(MODEL_ID.to_string()),
                dim: None,
                max_tokens: np_core::config::DEFAULT_EMBEDDING_TOKENS,
                threads: np_core::config::ThreadCount::Auto,
            },
            ..Config::default()
        }
    }

    fn db(&self) -> Db {
        Db::open(&self.cfg().workspace_db(&self.root)).expect("test setup: open workspace db")
    }

    /// The `models` row an embed needs, written the way the tests in `embed_run.rs` write it.
    fn install_model(&self) {
        let db = self.db();
        db::insert_model(&db, MODEL_ID, DIM as i64, Some("00"), "", "", 1_730_000_000)
            .expect("test setup: insert model row");
    }

    /// The artifact file arm (iii) looks for; nothing reads its bytes.
    fn touch_artifact(&self) {
        let dir = self.cfg().workspace_models_dir(&self.root);
        fs::create_dir_all(&dir).expect("test setup: models dir");
        fs::write(dir.join(MODEL_ID), b"not a model").expect("test setup: artifact file");
    }

    /// A workspace path as an OS path, for the cases that mutate a figure.
    fn file(&self, rel: &str) -> PathBuf {
        self.root.join(rel)
    }

    /// `(id, hash, mime)` for every `media` row, in id order.
    fn media_rows(&self) -> Vec<(i64, String, String)> {
        let db = self.db();
        let mut rows: Vec<(i64, String, String)> =
            db.0.prepare("SELECT id, hash, mime FROM media ORDER BY id")
                .expect("test: prepare")
                .query_map([], |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?)))
                .expect("test: query")
                .map(|row| row.expect("test: read media row"))
                .collect();
        rows.sort();
        rows
    }

    /// Every recorded media path, as display text.
    fn paths(&self) -> BTreeSet<String> {
        db::media_paths(&self.db())
            .expect("test: reading paths")
            .into_iter()
            .map(|rel| String::from_utf8_lossy(&rel).to_string())
            .collect()
    }

    /// `(media id, vector length in bytes)` for every stored image vector under [`MODEL_ID`].
    fn embeddings(&self) -> Vec<(i64, usize)> {
        db::media_embeddings(&self.db(), MODEL_ID)
            .expect("test: reading embeddings")
            .into_iter()
            .map(|(id, vector)| (id, vector.len()))
            .collect()
    }

    /// The embedding currency digest for one media row, or `None` when it has no vector.
    fn stamp(&self, media_id: i64) -> Option<String> {
        db::media_stamp(&self.db(), media_id, MODEL_ID).expect("test: reading the stamp")
    }

    /// The path check recorded for one figure, or `None` when nothing vouches for its bytes yet.
    fn path_check(&self, rel: &str) -> Option<(i64, i64, String)> {
        db::media_path_check(&self.db(), rel.as_bytes())
            .expect("test: reading the path check")
            .map(|check| (check.size, check.mtime, check.hash))
    }
}

/// Write `files` (notes and figures alike), then scan once so the notes are live.
fn workspace_at(files: &[(&str, &[u8])]) -> Setup {
    let tmp = TempDir::new().expect("test setup: tempdir");
    let root = tmp.path().join("ws");
    for (name, bytes) in files {
        let path = root.join(name);
        fs::create_dir_all(path.parent().expect("test setup: a parent"))
            .expect("test setup: figure directory");
        fs::File::create(&path)
            .expect("test setup: write file")
            .write_all(bytes)
            .expect("test setup: write bytes");
    }
    let setup = Setup {
        data_dir: tmp.path().join("data"),
        _tmp: tmp,
        root,
    };
    scan(&setup.root, &setup.cfg()).expect("test setup: first scan");
    setup
}

/// A scanned workspace whose notes are `notes` and whose figures are the PNGs in `figures`.
fn scanned(notes: &[(&str, &str)], figures: &[&str]) -> Setup {
    let mut files: Vec<(String, Vec<u8>)> = notes
        .iter()
        .map(|(name, body)| (name.to_string(), body.as_bytes().to_vec()))
        .collect();
    for figure in figures {
        files.push((figure.to_string(), png(figure.len() as u8)));
    }
    let borrowed: Vec<(&str, &[u8])> = files
        .iter()
        .map(|(name, bytes)| (name.as_str(), bytes.as_slice()))
        .collect();
    workspace_at(&borrowed)
}

/// Bytes that pass the content gate as a PNG: the signature, then filler that makes the length figure-specific.
fn png(seed: u8) -> Vec<u8> {
    let mut bytes = media::PNG_SIGNATURE.to_vec();
    bytes.extend((0..24u8).map(|index| index ^ seed));
    bytes
}

/// Bytes that pass as a JPEG, for the case that needs two formats in one run.
fn jpeg() -> Vec<u8> {
    let mut bytes = vec![0xff, 0xd8, 0xff, 0xe0];
    bytes.extend(0..40u8);
    bytes
}

/// Replace a figure's bytes and push its mtime forward, which is the "the file moved" trigger.
fn rewrite(path: &Path, bytes: &[u8]) {
    fs::write(path, bytes).expect("test setup: rewrite the figure");
    let moved = std::time::SystemTime::UNIX_EPOCH + std::time::Duration::from_secs(4_102_444_800);
    let file = fs::OpenOptions::new()
        .append(true)
        .open(path)
        .expect("test setup: open for utime");
    file.set_modified(moved)
        .expect("test setup: move the mtime");
}

// ---------------------------------------------------------------------------
// the fake provider
// ---------------------------------------------------------------------------

/// A provider that records **every** [`Embedder::embed_image`] call, so "encoded nothing" is a fact about a log.
#[derive(Default)]
struct FakeVision {
    /// One entry per `embed_image` call: the bytes handed over, digested so an assertion stays short.
    encodes: Mutex<Vec<String>>,
    /// What [`Embedder::supports_images`] answers — the configured-projector flag.
    vision: bool,
    /// When set, every encode refuses with this §8 class inside [`EmbedError::Unavailable`], as the vision seam does.
    refuse: Option<String>,
}

impl FakeVision {
    /// A provider whose projector is configured, so images are attempted.
    fn vision() -> Self {
        Self {
            vision: true,
            ..Self::default()
        }
    }

    /// A text-only provider: `supports_images() == false`, which is [`Embedder`]'s default answer.
    fn text_only() -> Self {
        Self::default()
    }

    /// A provider that attempts every encode and refuses each one with `class`.
    fn refusing(class: &str) -> Self {
        Self {
            vision: true,
            refuse: Some(class.to_string()),
            ..Self::default()
        }
    }

    /// The digests of the bytes encoded so far — the encode log.
    fn encodes(&self) -> Vec<String> {
        self.encodes.lock().expect("test: lock the log").clone()
    }
}

impl Embedder for FakeVision {
    fn model_id(&self) -> &str {
        MODEL_ID
    }

    fn dim(&self) -> usize {
        DIM
    }

    fn model_inputs(&self, texts: &[&str]) -> Vec<String> {
        texts.iter().map(|text| (*text).to_string()).collect()
    }

    fn embed(&self, texts: &[&str]) -> Result<Vec<Vector>, EmbedError> {
        Ok(texts.iter().map(|_| Vector::new(vec![0.5; DIM])).collect())
    }

    fn supports_images(&self) -> bool {
        self.vision
    }

    fn embed_image(&mut self, bytes: &[u8]) -> Result<Vector, EmbedError> {
        self.encodes
            .lock()
            .expect("test: lock the log")
            .push(sha256_bytes(bytes));
        match &self.refuse {
            Some(class) => Err(EmbedError::Unavailable(class.clone())),
            None => Ok(Vector::new(vec![0.25; DIM])),
        }
    }
}

/// One embed run, with the two assertions every case starts from: it answered, and nothing failed.
fn run(setup: &Setup, provider: &mut FakeVision) -> np_core::embed_run::EmbedReport {
    let report = embed(&setup.root, &setup.cfg(), provider).expect("test: embed runs");
    assert_eq!(report.errors, 0, "an image never becomes an error count");
    report
}

/// The refusals a report carried, as `(locator, class label)`, in report order.
fn classes(report: &np_core::embed_run::EmbedReport) -> Vec<(String, &'static str)> {
    report
        .image_refusals
        .iter()
        .map(|(path, refusal)| (path.clone(), refusal.label()))
        .collect()
}

// ---------------------------------------------------------------------------
// §7 rules 1 and 2: one row per figure, and unchanged bytes cost nothing
// ---------------------------------------------------------------------------

/// One figure named twice in one note is one row, one path and one encode (§7 rule 1).
#[test]
fn one_figure_named_twice_is_one_row_and_one_encode() {
    let setup = scanned(
        &[(
            "a.md",
            "![one](assets/one.png)\n\nagain ![one again](assets/one.png)\n",
        )],
        &["assets/one.png"],
    );
    setup.install_model();
    setup.touch_artifact();
    let mut provider = FakeVision::vision();

    let report = run(&setup, &mut provider);

    assert_eq!(
        report.images_seen, 1,
        "the second mention is the same image"
    );
    assert_eq!(report.images_embedded, 1);
    assert_eq!(report.images_refused, 0);
    assert_eq!(provider.encodes().len(), 1, "one encode, not two");
    assert_eq!(
        setup.paths(),
        BTreeSet::from(["assets/one.png".to_string()])
    );
    assert_eq!(setup.embeddings().len(), 1);
}

/// A second identical run encodes **nothing** and leaves every stamp byte-equal (§7 rule 2).
#[test]
fn a_second_identical_run_encodes_nothing_and_leaves_the_stamps_equal() {
    let setup = scanned(
        &[
            ("a.md", "![one](assets/one.png)\n"),
            ("b.md", "![two](two.png)\n"),
        ],
        &["assets/one.png", "two.png"],
    );
    setup.install_model();
    setup.touch_artifact();
    let mut first = FakeVision::vision();
    let before = run(&setup, &mut first);
    assert_eq!(before.images_embedded, 2, "test setup: both were owed");
    let rows = setup.media_rows();
    let stamps: Vec<Option<String>> = rows.iter().map(|(id, _, _)| setup.stamp(*id)).collect();
    let checks: Vec<Option<(i64, i64, String)>> = vec![
        setup.path_check("assets/one.png"),
        setup.path_check("two.png"),
    ];

    let mut again = FakeVision::vision();
    let report = run(&setup, &mut again);

    assert!(
        again.encodes().is_empty(),
        "an unchanged figure costs no encode"
    );
    assert_eq!(report.images_seen, 2, "seen is still counted");
    assert_eq!(report.images_embedded, 0);
    assert_eq!(report.outcome, EmbedOutcome::Current);
    assert_eq!(setup.media_rows(), rows, "no second row for the same bytes");
    let after: Vec<Option<String>> = rows.iter().map(|(id, _, _)| setup.stamp(*id)).collect();
    assert_eq!(after, stamps, "the currency digests are byte-equal");
    assert_eq!(
        vec![
            setup.path_check("assets/one.png"),
            setup.path_check("two.png")
        ],
        checks,
        "and so are the path checks"
    );
}

/// A byte-identical copy in a second note adds a **path** row and no encode (§6's content identity).
#[test]
fn a_byte_identical_copy_adds_a_path_without_another_encode() {
    let setup = scanned(
        &[
            ("a.md", "![x](assets/one.png)\n"),
            ("b.md", "![x](assets/one.png)\n"),
        ],
        &["assets/one.png"],
    );
    setup.install_model();
    setup.touch_artifact();
    let mut first = FakeVision::vision();
    run(&setup, &mut first);
    assert_eq!(first.encodes().len(), 1, "test setup: one encode");

    // The copy is a second path holding the same content hash.
    fs::copy(setup.file("assets/one.png"), setup.file("assets/copy.png"))
        .expect("test setup: copy the figure");
    fs::write(setup.file("c.md"), "![copy](assets/copy.png)\n").expect("test setup: third note");
    scan(&setup.root, &setup.cfg()).expect("test: rescan");

    let mut again = FakeVision::vision();
    let report = run(&setup, &mut again);

    assert!(again.encodes().is_empty(), "same bytes, no second encode");
    assert_eq!(report.images_seen, 3, "three links, one figure");
    assert_eq!(setup.media_rows().len(), 1, "one content row");
    assert_eq!(
        setup.paths(),
        BTreeSet::from(["assets/copy.png".to_string(), "assets/one.png".to_string()])
    );
}

/// A file whose size and mtime have not moved is answered from the path check **without reading it** (§7's D12 rule).
///
/// The figure is made unreadable between the runs, so a second digest would be an I/O error rather than a silent extra
/// read — and the encode log says nothing was encoded either.
#[test]
fn an_unchanged_file_is_used_from_the_stamp_without_a_second_digest() {
    let setup = scanned(&[("a.md", "![one](assets/one.png)\n")], &["assets/one.png"]);
    setup.install_model();
    setup.touch_artifact();
    let mut first = FakeVision::vision();
    run(&setup, &mut first);
    let media = setup.media_rows()[0].0;
    let digest = setup
        .stamp(media)
        .expect("test setup: the vector is stamped");

    #[cfg(unix)]
    {
        // Mode `000` really denies the read here because the test user is not root (`id -u` is 1000 in CI); a
        // privileged reader would ignore the mode bits and only the encode log would be asserting anything.
        use std::os::unix::fs::PermissionsExt;
        fs::set_permissions(
            setup.file("assets/one.png"),
            fs::Permissions::from_mode(0o000),
        )
        .expect("test setup: make the figure unreadable");
    }
    #[cfg(not(unix))]
    {
        // Without a way to make one file unreadable this case would prove nothing about the read, so it says so.
        eprintln!(
            "no unreadable-file mutation on this platform; asserting the no-encode half only"
        );
    }

    let mut again = FakeVision::vision();
    let report = run(&setup, &mut again);

    assert!(
        again.encodes().is_empty(),
        "the stamp answered the question"
    );
    assert_eq!(report.images_seen, 1);
    assert_eq!(report.images_refused, 0, "a stamp hit is not a refusal");
    assert_eq!(setup.stamp(media).as_deref(), Some(digest.as_str()));
}

/// A figure whose mtime moved is re-hashed and re-encoded, and the media row keeps its identity where it can.
#[test]
fn a_moved_file_is_rehashed_and_reencoded() {
    let setup = scanned(&[("a.md", "![one](assets/one.png)\n")], &["assets/one.png"]);
    setup.install_model();
    setup.touch_artifact();
    let mut first = FakeVision::vision();
    run(&setup, &mut first);

    rewrite(&setup.file("assets/one.png"), &jpeg());
    let mut again = FakeVision::vision();
    let report = run(&setup, &mut again);

    assert_eq!(
        again.encodes().len(),
        1,
        "the moved bytes were read and encoded"
    );
    assert_eq!(report.images_embedded, 1);
    let rows = setup.media_rows();
    assert_eq!(
        rows.len(),
        1,
        "the displaced row went with the re-pointed path (§6's lifecycle), so one figure is one row"
    );
    assert_eq!(
        rows[0].2, "image/jpeg",
        "and it describes what the bytes are now"
    );
    assert_eq!(
        setup.path_check("assets/one.png").expect("checked").2,
        rows[0].1
    );
    assert_eq!(setup.paths().len(), 1);
    assert_eq!(
        setup.embeddings().len(),
        1,
        "the stale vector went with the old row"
    );
}

// ---------------------------------------------------------------------------
// §7 rule 4: lifecycle
// ---------------------------------------------------------------------------

/// A rename moves the path and keeps the media row, its id and its vector — no re-encode (§7 rule 4).
#[test]
fn a_rename_moves_the_path_and_keeps_the_vector() {
    let setup = scanned(&[("a.md", "![one](assets/one.png)\n")], &["assets/one.png"]);
    setup.install_model();
    setup.touch_artifact();
    let mut first = FakeVision::vision();
    run(&setup, &mut first);
    let (media, hash, _) = setup.media_rows()[0].clone();
    let vector = setup.embeddings();

    fs::rename(setup.file("assets/one.png"), setup.file("assets/two.png"))
        .expect("test setup: rename the figure");
    fs::write(setup.file("a.md"), "![one](assets/two.png)\n").expect("test setup: relink the note");
    scan(&setup.root, &setup.cfg()).expect("test: rescan");

    let mut again = FakeVision::vision();
    let report = run(&setup, &mut again);

    assert!(again.encodes().is_empty(), "the bytes did not change");
    assert_eq!(report.images_embedded, 0);
    assert_eq!(
        setup.media_rows(),
        vec![(media, hash.clone(), "image/png".to_string())]
    );
    assert_eq!(
        setup.embeddings(),
        vector,
        "the same row still carries the vector"
    );
    assert_eq!(setup.stamp(media).as_deref(), Some(hash.as_str()));
    assert_eq!(
        setup.paths(),
        BTreeSet::from(["assets/two.png".to_string()]),
        "and the stale path is gone"
    );
}

/// Deleting the last referrer removes the media row, its paths and its vectors at run end (§7 rule 4).
#[test]
fn deleting_the_last_referrer_removes_the_media_row_and_its_vector() {
    let setup = scanned(
        &[
            ("a.md", "![one](assets/one.png)\n"),
            ("b.md", "nothing here\n"),
        ],
        &["assets/one.png"],
    );
    setup.install_model();
    setup.touch_artifact();
    let mut first = FakeVision::vision();
    run(&setup, &mut first);
    assert_eq!(setup.embeddings().len(), 1, "test setup: one vector");

    fs::remove_file(setup.file("assets/one.png")).expect("test setup: delete the figure");
    fs::write(setup.file("a.md"), "the figure is gone\n").expect("test setup: unlink it");
    scan(&setup.root, &setup.cfg()).expect("test: rescan");

    let mut again = FakeVision::vision();
    let report = run(&setup, &mut again);

    assert_eq!(report.images_seen, 0);
    assert!(setup.media_rows().is_empty(), "no path holds it");
    assert!(setup.paths().is_empty());
    assert!(setup.embeddings().is_empty(), "the cascade took the vector");
}

// ---------------------------------------------------------------------------
// §8: one case per refusal class, and none of them is an exit code
// ---------------------------------------------------------------------------

/// A provider with no projector refuses once per image and reports zero errors (§8's `vision-unavailable`).
#[test]
fn a_provider_without_images_refuses_once_per_image_and_reports_no_errors() {
    let setup = scanned(
        &[("a.md", "![one](assets/one.png)\n![two](assets/two.png)\n")],
        &["assets/one.png", "assets/two.png"],
    );
    setup.install_model();
    setup.touch_artifact();

    let mut provider = FakeVision::text_only();
    let report = run(&setup, &mut provider);

    assert_eq!(report.images_seen, 2);
    assert_eq!(report.images_embedded, 0);
    assert_eq!(report.images_refused, 2);
    assert_eq!(report.image_refusals.len(), 2);
    for (path, refusal) in &report.image_refusals {
        assert_eq!(*refusal, MediaRefusal::VisionUnavailable, "for {path}");
    }
    assert_eq!(
        report.outcome,
        EmbedOutcome::Current,
        "and the run still exits 0"
    );
    assert!(
        !setup.media_rows().is_empty(),
        "the bytes were still recorded"
    );
    assert!(
        setup.embeddings().is_empty(),
        "with nothing to encode them with"
    );
}

/// An over-budget encode is counted, keeps the figure's previous currency, and does not change exit status (§8).
#[test]
fn an_over_budget_encode_is_counted_and_the_run_still_succeeds() {
    let setup = scanned(&[("a.md", "![one](assets/one.png)\n")], &["assets/one.png"]);
    setup.install_model();
    setup.touch_artifact();

    let mut provider = FakeVision::refusing(media::OVER_BUDGET);
    let report = run(&setup, &mut provider);

    assert_eq!(report.images_seen, 1);
    assert_eq!(report.images_embedded, 0);
    assert_eq!(report.image_refusals[0].1, MediaRefusal::OverBudget);
    assert_eq!(
        report.outcome,
        EmbedOutcome::Current,
        "exit 0, as §8 rule 6 asks"
    );
    let media = setup.media_rows()[0].0;
    assert_eq!(setup.stamp(media), None, "a refused figure has no currency");
    assert_eq!(
        setup.path_check("assets/one.png").expect("checked").2,
        setup.media_rows()[0].1,
        "but the bytes it did read are still recorded, so the next run does not re-read them"
    );

    // The same figure with a capable provider is then owed, and embedded.
    let mut capable = FakeVision::vision();
    let report = run(&setup, &mut capable);
    assert_eq!(
        report.images_embedded, 1,
        "the refusal left it owed, not broken"
    );
}

/// A malformed raster is `undecodable`, named by the provider's own class string (§8).
#[test]
fn an_undecodable_raster_is_counted_as_undecodable() {
    let setup = scanned(&[("a.md", "![one](assets/one.png)\n")], &["assets/one.png"]);
    setup.install_model();
    setup.touch_artifact();

    let mut provider = FakeVision::refusing(media::UNDECODABLE);
    let report = run(&setup, &mut provider);

    assert_eq!(report.image_refusals[0].1, MediaRefusal::Undecodable);
    assert_eq!(report.images_refused, 1);
}

/// One case per resolution class, and one for the content gate that follows them (§8's rule 8).
///
/// The note is the locator — a target is never reported, because it may be a URL — and the classes arrive in document
/// order. The last section is F26: a file named `.png` whose bytes are not a raster is refused by what it **is**, after
/// resolution succeeded, so `unsupported-format` covers both a rejected suffix and a lie.
#[test]
fn each_resolution_class_is_counted_against_the_note_that_wrote_it() {
    let setup = workspace_at(&[
        (
            "notes/a.md",
            b"![missing](assets/gone.png)\n![outside](../../outside.png)\n![gif](assets/clip.gif)\n"
                .as_slice(),
        ),
        ("notes/assets/clip.gif", b"GIF89a not a raster this build reads".as_slice()),
    ]);
    setup.install_model();
    setup.touch_artifact();

    let mut provider = FakeVision::vision();
    let report = run(&setup, &mut provider);

    let listed = classes(&report);
    let got: Vec<(&str, &str)> = listed
        .iter()
        .map(|(path, label)| (path.as_str(), *label))
        .collect();
    assert_eq!(
        got,
        vec![
            ("notes/a.md", "missing-target"),
            ("notes/a.md", "outside-workspace"),
            ("notes/a.md", "unsupported-format"),
        ],
        "one entry per refusal, in document order, labelled by class"
    );
    assert_eq!(
        report.images_seen, 0,
        "nothing resolved, so nothing was seen"
    );
    assert_eq!(report.images_refused, 3);
    assert_eq!(report.outcome, EmbedOutcome::Current);

    // A `.png` whose bytes are not a raster: resolution succeeds, the content gate refuses, and nothing is opened for
    // the encoder.
    fs::write(setup.file("notes/assets/clip.png"), b"not an image at all")
        .expect("test setup: a file that lies about its format");
    fs::write(setup.file("notes/a.md"), "![lie](assets/clip.png)\n").expect("test setup: relink");
    scan(&setup.root, &setup.cfg()).expect("test: rescan");

    let mut again = FakeVision::vision();
    let report = run(&setup, &mut again);
    assert_eq!(report.images_seen, 1, "it resolved and was opened");
    assert!(again.encodes().is_empty(), "and refused before the encoder");
    assert_eq!(report.image_refusals[0].1, MediaRefusal::UnsupportedFormat);
    assert_eq!(setup.paths().len(), 0, "a refused figure gains no path row");
}

/// The report's image fields are initialised in every arm, including the ones that read nothing.
#[test]
fn an_arm_that_reads_nothing_reports_zero_images() {
    let setup = scanned(&[("a.md", "![one](assets/one.png)\n")], &["assets/one.png"]);
    setup.install_model();
    // No artifact file: arm (iii) answers before any image work, and says so with zeros rather than omitting keys.
    let mut provider = FakeVision::vision();
    let report = embed(&setup.root, &setup.cfg(), &mut provider).expect("test: embed runs");
    assert!(matches!(
        report.outcome,
        EmbedOutcome::ArtifactAbsent { .. }
    ));
    assert_eq!(report.images_seen, 0);
    assert_eq!(report.images_embedded, 0);
    assert_eq!(report.images_refused, 0);
    assert!(report.image_refusals.is_empty());
}
