//! Progress reporting through the seams that a GUI would watch (**F48** design:
//! `docs/superpowers/specs/2026-12-09-np-serve-progress-design.md`).
//!
//! The fetch cases here reuse [`np_core::fetch`]’s test seam rather than inventing one, for the reason
//! `tests/fetch.rs` gives: production pins a 309 MB artifact, and a progress case that downloaded it would be slow
//! enough to skip and vague enough to not notice. What is *not* faked is size — the primary fixture here is 9 MiB, so
//! the real 8 MiB checkpoint interval fires inside one member instead of only at its ends. A test that saw two events
//! per member would pass with a reporter that never fires mid-transfer, and mid-transfer is the entire feature.

use std::cell::RefCell;
use std::collections::BTreeMap;
use std::io::Cursor;
use std::path::{Path, PathBuf};
use std::sync::Mutex;

use np_core::db::Db;
use np_core::fetch::MemberPin;
use np_core::fetch::{self, Body, Declaration, Expectation, FetchError, Transport};
use np_core::hash::sha256_bytes;
use np_core::progress::{Phase, Progress, ProgressEvent};
use tempfile::TempDir;

/// What a member's body does when opened. Only the shapes a progress test needs.
enum Serve {
    Full(Vec<u8>),
    /// Deliver these bytes with no announced length: **§12 rule 12**, and the case that proves the denominator in an
    /// event is the pinned size rather than whatever the transport happened to announce.
    UnknownLen(Vec<u8>),
    /// Refuse to open at all, carrying only a failure class (§12 rule 8).
    Fail,
}

struct Fake {
    responses: RefCell<BTreeMap<String, Serve>>,
}

impl Fake {
    fn new() -> Self {
        Fake {
            responses: RefCell::new(BTreeMap::new()),
        }
    }

    fn serve(&self, member: &str, serve: Serve) {
        self.responses
            .borrow_mut()
            .insert(member.to_string(), serve);
    }
}

impl Transport for Fake {
    fn open(&self, member: &dyn MemberPin) -> Result<Body, FetchError> {
        let bytes = match self.responses.borrow_mut().remove(member.member()) {
            Some(Serve::Full(bytes)) => (bytes, Some(member.size_bytes())),
            Some(Serve::UnknownLen(bytes)) => (bytes, None),
            Some(Serve::Fail) => {
                return Err(FetchError::Transport {
                    id: "test-model".into(),
                    member: member.member().into(),
                    class: "connection-refused".into(),
                })
            }
            // An unconfigured member is a test bug. Serving empty bytes would hash to a real digest and turn the
            // mistake into a passing case that proved nothing.
            None => panic!("no response configured for member `{}`", member.member()),
        };
        Ok(Body {
            reader: Box::new(Cursor::new(bytes.0)),
            bytes_total: bytes.1,
        })
    }
}

struct Spec {
    member: &'static str,
    filename: &'static str,
    size_bytes: u64,
    sha256: String,
}

impl MemberPin for Spec {
    fn id(&self) -> &str {
        "test-model"
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
        &self.sha256
    }
}

/// Bytes that are not one repeated block, so a digest is a real check rather than a formality.
fn filler(len: usize) -> Vec<u8> {
    (0..len).map(|i| ((i * 31 + i / 251) % 251) as u8).collect()
}

/// Nine MiB: one full checkpoint interval plus a remainder, so a member produces a start line, a middle line and an
/// end line.
const MAIN_LEN: usize = 9 * 1024 * 1024;
/// Deliberately smaller than the checkpoint interval: this member produces a start line and an end line only, which
/// is what makes "the counters restart" visible instead of buried in another member's checkpoints.
const EXTRA_LEN: usize = 64 * 1024;

fn table() -> Vec<Spec> {
    let main = filler(MAIN_LEN);
    let extra = filler(EXTRA_LEN);
    vec![
        Spec {
            member: "main",
            filename: "test-model.gguf",
            size_bytes: main.len() as u64,
            sha256: sha256_bytes(&main),
        },
        Spec {
            member: "extra",
            filename: "test-model-mmproj.gguf",
            size_bytes: extra.len() as u64,
            sha256: sha256_bytes(&extra),
        },
    ]
}

fn declaration() -> Declaration {
    Declaration {
        dim: 4,
        prefix_doc: "doc: ".into(),
        prefix_query: "q: ".into(),
    }
}

/// One event as the test remembers it. Owned, because a reporter is handed a borrow and must copy what it wants to
/// say later — which is exactly what `np serve`'s reporter does on its way to JSON.
#[derive(Debug, Clone)]
struct Seen {
    phase: Phase,
    member: Option<String>,
    bytes_done: u64,
    bytes_total: Option<u64>,
    members_done: usize,
    members_total: usize,
}

/// A reporter that keeps everything. `Progress` requires `Sync`, hence the lock rather than a `RefCell`.
struct Recorder {
    seen: Mutex<Vec<Seen>>,
}

impl Recorder {
    fn new() -> Self {
        Recorder {
            seen: Mutex::new(Vec::new()),
        }
    }

    fn taken(&self) -> Vec<Seen> {
        std::mem::take(&mut *self.seen.lock().expect("the recorder is never poisoned"))
    }
}

impl Progress for Recorder {
    fn report(&self, event: &ProgressEvent<'_>) {
        self.seen
            .lock()
            .expect("the recorder is never poisoned")
            .push(Seen {
                phase: event.phase,
                member: event.member.map(str::to_string),
                bytes_done: event.bytes_done,
                bytes_total: event.bytes_total,
                members_done: event.members_done,
                members_total: event.members_total,
            });
    }
}

fn fetch(
    models: &Path,
    db: &Db,
    requested: &[&str],
    transport: &dyn Transport,
    progress: &dyn Progress,
) -> Result<fetch::FetchReport, FetchError> {
    let table = table();
    let plan = Expectation {
        table: &table,
        declaration: &declaration(),
    };
    let transfer = fetch::Transfer::new(transport, 1_730_000_000).reporting(progress);
    fetch::fetch_with_progress(db, models, "test-model", requested, &plan, &transfer)
}

fn db() -> Db {
    Db::in_memory().expect("test setup: in-memory database")
}

#[test]
fn a_single_member_fetch_reports_monotonic_byte_checkpoints() {
    let tmp = TempDir::new().expect("test setup: tempdir");
    let models = tmp.path().join("models");
    let db = db();
    let fake = Fake::new();
    fake.serve("main", Serve::Full(filler(MAIN_LEN)));
    let recorder = Recorder::new();

    fetch(&models, &db, &["main"], &fake, &recorder).expect("single member fetches");
    let seen = recorder.taken();

    assert!(
        seen.len() >= 3,
        "a 9 MiB member crosses the 8 MiB checkpoint interval, so a start line, a middle line and an end line: {seen:?}"
    );
    assert!(
        seen.iter().all(|event| event.phase == Phase::Fetching),
        "every fetch event says which phase it is in: {seen:?}"
    );
    assert_eq!(
        seen[0].member.as_deref(),
        Some("main"),
        "the first line names the member before byte one"
    );
    assert_eq!(
        seen[0].bytes_done, 0,
        "byte zero is reported: a client that opens a progress window mid-fetch needs to know nothing has arrived yet"
    );
    for pair in seen.windows(2) {
        assert!(
            pair[1].bytes_done > pair[0].bytes_done,
            "bytes only move forward within a member: {:?}",
            seen
        );
    }
    let last = seen.last().expect("at least one event");
    assert_eq!(
        last.bytes_done, MAIN_LEN as u64,
        "the closing line reports the whole member"
    );
    for event in &seen {
        assert_eq!(
            event.bytes_total,
            Some(MAIN_LEN as u64),
            "the denominator is the pinned size on every line, including the first"
        );
        assert_eq!(event.members_total, 1);
        assert_eq!(event.members_done, 0);
    }
}

#[test]
fn a_two_member_fetch_restarts_bytes_at_the_second_member() {
    let tmp = TempDir::new().expect("test setup: tempdir");
    let models = tmp.path().join("models");
    let db = db();
    let fake = Fake::new();
    // The second member announces no size, so if the reporter ever took its denominator from the transport this line
    // would read `None` and the assertions below would fail for the right reason.
    fake.serve("main", Serve::Full(filler(MAIN_LEN)));
    fake.serve("extra", Serve::UnknownLen(filler(EXTRA_LEN)));
    let recorder = Recorder::new();

    fetch(&models, &db, &["main", "extra"], &fake, &recorder).expect("both members fetch");
    let seen = recorder.taken();

    let main: Vec<&Seen> = seen
        .iter()
        .filter(|event| event.member.as_deref() == Some("main"))
        .collect();
    let extra: Vec<&Seen> = seen
        .iter()
        .filter(|event| event.member.as_deref() == Some("extra"))
        .collect();
    assert!(
        !main.is_empty() && !extra.is_empty(),
        "both members narrate themselves: {seen:?}"
    );
    assert_eq!(
        main.last().expect("main finished").bytes_done,
        MAIN_LEN as u64,
        "the first member reaches its full size before the second starts"
    );

    // **Review Focus 2.** The counters are per member, so the sequence is allowed to fall — and a client that divides
    // each line by its own member's total never sees a nonsense percentage from that fall.
    let first_extra = extra.first().expect("extra started");
    assert_eq!(
        first_extra.bytes_done, 0,
        "the second member starts at zero of its own bytes"
    );
    assert!(
        first_extra.bytes_done < main.last().expect("main finished").bytes_done,
        "bytes restart at the second member rather than continuing a cumulative count: {seen:?}"
    );
    assert_eq!(first_extra.members_done, 1, "one member is finished");
    assert_eq!(first_extra.members_total, 2, "this call has two members");
    assert_eq!(
        main.first().expect("main started").members_done,
        0,
        "the first member counts itself as the first"
    );
    for event in &seen {
        let total = event
            .bytes_total
            .expect("every line carries the pinned total");
        assert!(
            event.bytes_done <= total,
            "no line ever claims more bytes than its member holds — a client computing a percentage would print over 100%: {event:?}"
        );
        assert_eq!(
            event.members_total, 2,
            "members_total is the size of this call on every line"
        );
    }
    assert_eq!(
        extra.last().expect("extra finished").bytes_done,
        EXTRA_LEN as u64,
        "the closing line of the second member reports its whole size"
    );
}

#[test]
fn a_fetch_that_never_opens_reports_nothing() {
    let tmp = TempDir::new().expect("test setup: tempdir");
    let models = tmp.path().join("models");
    let db = db();
    let fake = Fake::new();
    // The transport refuses at open: a reporter that fired "0 of 309 MB" for work that never started would leave a GUI
    // showing a stalled download forever.
    fake.serve("main", Serve::Fail);
    let recorder = Recorder::new();
    let err = fetch(&models, &db, &["main"], &fake, &recorder).map(|_| ());
    assert!(
        err.is_err(),
        "a transport that refuses yields a refusal: {err:?}"
    );
    assert!(
        recorder.taken().is_empty(),
        "a refusal before byte one reports no byte progress"
    );
}

#[test]
fn the_noop_reporter_keeps_the_old_fetch_signature_working() {
    // `fetch` and `fetch_with_table` must keep compiling and behaving for every existing caller — the CLI's bytes do
    // not move, so its call sites pass no reporter at all.
    let tmp = TempDir::new().expect("test setup: tempdir");
    let models = tmp.path().join("models");
    let db = db();
    let fake = Fake::new();
    fake.serve("main", Serve::Full(filler(1024)));
    let table = vec![Spec {
        member: "main",
        filename: "test-model.gguf",
        size_bytes: 1_024,
        sha256: sha256_bytes(&filler(1024)),
    }];
    let declaration = declaration();
    let report = fetch::fetch_with_table(
        &db,
        &models,
        "test-model",
        &["main"],
        &fake,
        1_730_000_000,
        &Expectation {
            table: &table,
            declaration: &declaration,
        },
    )
    .expect("the wrapper still fetches");
    assert_eq!(report.total_bytes, 1_024);
}

// ---------------------------------------------------------------------------
// the embed pass
// ---------------------------------------------------------------------------
//
// Fixtures here mirror `tests/embed_run.rs` rather than importing it: an integration test binary cannot borrow
// another binary's helpers, and a shared crate for two fixtures would hide which case needs which provider. What they
// share is the posture — a real scanned vault, a fake [`Embedder`] so the 310 MB artifact stays out of it, and one
// model row so arm (iii) does not answer before the writer loop starts.

use std::fs;
use std::sync::Once;

use np_core::config::{Config, DEFAULT_EMBEDDING_TOKENS};
use np_core::config::{EmbeddingProvider, EmbeddingsConfig};
use np_core::db::{self, Db as WorkspaceDb};
use np_core::embed::{EmbedError, Embedder, Vector};
use np_core::embed_run::{embed_with_inject, EMBED_BATCH_CHUNKS};

/// Width the fake declares and produces: small, and not a power of two.
const FAKE_DIM: usize = 3;
const FAKE_MODEL: &str = "fake-model@1";

struct Setup {
    data_dir: PathBuf,
    root: PathBuf,
    _tmp: TempDir,
}

impl Setup {
    fn cfg(&self) -> Config {
        Config {
            data_dir: self.data_dir.clone(),
            embeddings: EmbeddingsConfig {
                provider: EmbeddingProvider::LocalModel,
                model: Some(FAKE_MODEL.to_string()),
                dim: None,
                max_tokens: DEFAULT_EMBEDDING_TOKENS,
                threads: np_core::config::ThreadCount::Auto,
                log_level: None,
            },
            ..Config::default()
        }
    }

    /// A `models` row and an artifact file, so the run reaches the writer loop instead of refusing on a missing model.
    fn prepare(&self) {
        let db = WorkspaceDb::open(&self.db_path()).expect("test setup: open workspace db");
        db::insert_model(
            &db,
            FAKE_MODEL,
            FAKE_DIM as i64,
            Some("00"),
            "",
            "",
            1_730_000_000,
        )
        .expect("test setup: model row");
        let dir = self.cfg().workspace_models_dir(&self.root);
        fs::create_dir_all(&dir).expect("test setup: models dir");
        fs::write(dir.join(FAKE_MODEL), b"not a model").expect("test setup: artifact");
    }

    fn db_path(&self) -> PathBuf {
        self.cfg().workspace_db(&self.root)
    }
}

/// `n` one-chunk notes, scanned once, with the model row an embed requires.
fn vault(n: usize) -> Setup {
    let tmp = TempDir::new().expect("test setup: tempdir");
    let root = tmp.path().join("ws");
    fs::create_dir_all(&root).expect("test setup: workspace dir");
    for index in 0..n {
        fs::write(
            root.join(format!("note-{index:03}.md")),
            format!("# Note {index}\n\nbody text {index} for the model to read\n"),
        )
        .expect("test setup: write note");
    }
    let setup = Setup {
        data_dir: tmp.path().join("data"),
        root,
        _tmp: tmp,
    };
    np_core::scan(&setup.root, &setup.cfg()).expect("test setup: first scan");
    setup.prepare();
    setup
}

/// A provider that answers every batch, or refuses from a chosen call onward.
struct Provider {
    dim: usize,
    calls: Mutex<usize>,
    /// Refuse this call (1-based) and every later one; `0` never refuses.
    fail_from: usize,
    /// Refuse **every text** as over budget. This is the shape that keeps the writer loop running while writing
    /// nothing: an over-budget batch is retried item by item, each refusal counted, and the loop goes on to the next
    /// batch — so a reporter that claims a write per lap can be caught here, where a hard refusal (`fail_from`)
    /// leaves the loop early and cannot.
    over_budget_all: bool,
}

impl Provider {
    fn answering() -> Self {
        Provider {
            dim: FAKE_DIM,
            calls: Mutex::new(0),
            fail_from: 0,
            over_budget_all: false,
        }
    }

    fn failing_from(call: usize) -> Self {
        let mut provider = Self::answering();
        provider.fail_from = call;
        provider
    }

    fn over_budget() -> Self {
        Provider {
            over_budget_all: true,
            ..Self::answering()
        }
    }

    fn calls(&self) -> usize {
        *self.calls.lock().expect("the provider is never poisoned")
    }
}

impl Embedder for Provider {
    fn model_id(&self) -> &str {
        FAKE_MODEL
    }
    fn model_inputs(&self, texts: &[&str]) -> Vec<String> {
        texts.iter().map(|text| (*text).to_string()).collect()
    }
    fn dim(&self) -> usize {
        self.dim
    }
    fn embed(&self, texts: &[&str]) -> Result<Vec<Vector>, EmbedError> {
        let call = {
            let mut guard = self.calls.lock().expect("the provider is never poisoned");
            *guard += 1;
            *guard
        };
        if self.over_budget_all {
            return Err(EmbedError::TokenBudgetExceeded {
                tokens: 4_242,
                budget: 1_024,
            });
        }
        if self.fail_from != 0 && call >= self.fail_from {
            return Err(EmbedError::RuntimeNotBuilt {
                model_id: FAKE_MODEL.to_string(),
            });
        }
        Ok((0..texts.len())
            .map(|_| Vector::new(vec![0.5; self.dim]))
            .collect())
    }
}

/// One embed event as the test remembers it.
#[derive(Debug, Clone)]
struct EmbedSeen {
    phase: Phase,
    chunks_done: usize,
    embedded: usize,
}

struct EmbedRecorder {
    seen: Mutex<Vec<EmbedSeen>>,
}

impl EmbedRecorder {
    fn new() -> Self {
        EmbedRecorder {
            seen: Mutex::new(Vec::new()),
        }
    }

    fn taken(&self) -> Vec<EmbedSeen> {
        std::mem::take(&mut *self.seen.lock().expect("the recorder is never poisoned"))
    }
}

impl Progress for EmbedRecorder {
    fn report(&self, event: &ProgressEvent<'_>) {
        self.seen
            .lock()
            .expect("the recorder is never poisoned")
            .push(EmbedSeen {
                phase: event.phase,
                chunks_done: event.chunks_done,
                embedded: event.embedded,
            });
    }
}

/// Proves the recorder below records, so a later "no `Writing` line" is evidence rather than a broken listener.
static EMBED_RECORDER_CHECK: Once = Once::new();

fn assert_embed_recorder_works() {
    EMBED_RECORDER_CHECK.call_once(|| {
        let recorder = EmbedRecorder::new();
        recorder.report(&ProgressEvent {
            phase: Phase::Writing,
            ..Default::default()
        });
        assert_eq!(
            recorder.taken().len(),
            1,
            "the recorder keeps what it is handed"
        );
    });
}

#[test]
fn an_embed_run_reports_chunk_progress_and_ends_where_the_report_does() {
    assert_embed_recorder_works();
    let setup = vault(40);
    let recorder = EmbedRecorder::new();
    let mut provider = Provider::answering();

    let report = embed_with_inject(
        &setup.root,
        &setup.cfg(),
        &mut provider,
        &mut |_| {},
        &recorder,
    )
    .expect("40 chunks embed");
    let seen = recorder.taken();

    assert_eq!(report.embedded, 40, "the fixture embeds all forty");
    assert!(
        seen.iter()
            .any(|event| event.phase == Phase::Embedding && event.chunks_done == 0),
        "the pass announces itself at zero chunks, before the first batch is handed over: {seen:?}"
    );
    for pair in seen.windows(2) {
        assert!(
            pair[1].chunks_done >= pair[0].chunks_done,
            "chunk counts never go backwards within one embed: {seen:?}"
        );
    }
    let writing: Vec<&EmbedSeen> = seen
        .iter()
        .filter(|event| event.phase == Phase::Writing)
        .collect();
    // One line per committed batch, and the batches are bounded by `EMBED_BATCH_CHUNKS`: this pins the count to the
    // writes rather than to the loop, which is what Task 6's terminal `completed` line will be summed against.
    let expected = report.embedded.div_ceil(EMBED_BATCH_CHUNKS);
    assert_eq!(
        writing.len(),
        expected,
        "one `writing` line per committed batch ({expected} for {} chunks at {EMBED_BATCH_CHUNKS} a batch): {seen:?}",
        report.embedded
    );
    let last = seen.last().expect("the run reported");
    assert_eq!(
        last.embedded, report.embedded,
        "the closing line agrees with the report a client also receives"
    );
    assert_eq!(last.chunks_done, 40, "every chunk was handed over");
}

#[test]
fn a_batch_that_refused_reports_no_write() {
    assert_embed_recorder_works();
    let setup = vault(20);
    let recorder = EmbedRecorder::new();
    // Batch one answers (8 chunks), batch two refuses: the run reports 8 embedded and must not claim a second write.
    let mut provider = Provider::failing_from(2);

    let report = embed_with_inject(
        &setup.root,
        &setup.cfg(),
        &mut provider,
        &mut |_| {},
        &recorder,
    )
    .expect("a refused batch degrades the run, it does not fault it");
    let seen = recorder.taken();

    assert_eq!(provider.calls(), 2, "the second batch was attempted");
    assert_eq!(report.embedded, EMBED_BATCH_CHUNKS, "one batch committed");
    let writing: Vec<&EmbedSeen> = seen
        .iter()
        .filter(|event| event.phase == Phase::Writing)
        .collect();
    assert_eq!(
        writing.len(),
        1,
        "the refused batch wrote nothing, so it earns no `writing` line: {seen:?}"
    );
    assert_eq!(
        writing[0].embedded, EMBED_BATCH_CHUNKS,
        "the one write line counts the chunks it committed"
    );
    assert_eq!(
        seen.last().expect("the run reported").phase,
        Phase::Writing,
        "the run's last progress line is the write that landed, not a claim about the batch that refused: {seen:?}"
    );
}

#[test]
fn chunks_that_never_committed_report_no_write() {
    assert_embed_recorder_works();
    let setup = vault(20);
    let recorder = EmbedRecorder::new();
    // Every text is over budget, so every batch is retried item by item and nothing is written — and the loop *keeps
    // going*, unlike a hard refusal, which breaks out. This is the case that tells a claimed write from a real one:
    // with `phase` set unconditionally at the end of each lap, the assertions below fail.
    let mut provider = Provider::over_budget();

    let report = embed_with_inject(
        &setup.root,
        &setup.cfg(),
        &mut provider,
        &mut |_| {},
        &recorder,
    )
    .expect("an over-budget corpus is a degraded run, not a fault");
    let seen = recorder.taken();

    assert_eq!(report.embedded, 0, "nothing was embedded");
    assert!(
        report.refused_budget > 0,
        "the fixture really did refuse on the budget: {:?}",
        report.refused_budget
    );
    assert!(
        seen.iter().any(|event| event.phase == Phase::Embedding),
        "the pass still says where it is: {seen:?}"
    );
    assert!(
        !seen.iter().any(|event| event.phase == Phase::Writing),
        "not one write happened, so not one line may claim one: {seen:?}"
    );
    let last = seen.last().expect("the run reported");
    assert_eq!(
        last.chunks_done, 20,
        "every chunk was still handed to the provider, and the count says so: {seen:?}"
    );
}
