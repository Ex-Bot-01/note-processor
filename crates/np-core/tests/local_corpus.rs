//! R6d's third round: a **corpus** run — thousands of decodes in one process, measured through the production API.
//!
//! Rounds 1 and 2 could answer everything about *one* decode cheaply (a 403-token chunk in 2.59 s, a near-ceiling one in
//! 98.7 s, the window edge exactly) and one question about OpenMP by building the pinned tree twice. What neither could
//! answer is what §9 still owed: what a **first pass over a corpus** costs, whether memory climbs across thousands of
//! decodes (a per-process harness cannot see a leak), how much of a decode is fixed overhead — the thing batching would
//! amortise — and what §8 calls the number with its threshold applied rather than implied.
//!
//! ```text
//! NP_TEST_MODEL_PATH=/path/to/embeddinggemma-2-Q8_0.gguf \
//!   cargo test -p np-core --features local-model --test local_corpus -- --nocapture
//!
//! # default corpus is 2,048 notes (~10 min on this host); size it to what you can afford:
//! NP_CORPUS_NOTES=64 … -- --nocapture
//! ```
//!
//! **Nothing here changes runtime behaviour.** The batching decision stays gated on D13 (thread count) — this file
//! produces the arithmetic that decision will be argued from, including the per-decode fixed cost, and prints it. It
//! also makes no quality claim: §8 asks for "quality sanity, not quality proof", so the neighbour case prints what a
//! human can read and asserts only the one retrieval fact that needs no labels (a note queried as itself ranks first).
//!
//! Every case is artifact-gated for the standing reason (F18): with `NP_TEST_MODEL_PATH` unset they print `NOT COVERAGE`
//! and pass — an absent 310 MB file is neither a failure nor coverage.

/// The forward-pass claims, all of which need the real runtime and the real weights.
#[cfg(feature = "local-model")]
mod corpus {
    use std::fs;
    use std::path::{Path, PathBuf};
    use std::sync::{Mutex, MutexGuard};
    use std::time::{Duration, Instant};

    use np_core::config::{Config, EmbeddingProvider, EmbeddingsConfig};
    use np_core::db::{self, Db};
    use np_core::embed::Embedder;
    use np_core::embed_run::{self, EmbedOutcome, EMBED_BATCH_CHUNKS};
    use np_core::local_model;
    use np_core::scan;
    use tempfile::TempDir;

    /// The model id every fixture row installs. Any id works; one id keeps the SQL in this file one shape.
    const MODEL_ID: &str = "embeddinggemma-2";

    /// The width `embeddinggemma-2-Q8_0.gguf` answers with — pinned, because a run that stored 512-d vectors under a
    /// row claiming 768 would be the failure §3's width refusal exists to catch.
    const DIM: i64 = 768;

    /// The prefix on every fixture row. Real rows carry dressing like this (`task: search result | query: `); keeping
    /// it here means the token counts below describe what the model actually reads, not the note text alone.
    const PREFIX_DOC: &str = "task: search result | query: ";

    /// Notes in the corpus unless `NP_CORPUS_NOTES` says otherwise.
    ///
    /// 2,048 is §9's "thousands", and each note is deliberately one short chunk rather than a few long ones: at the
    /// measured ~150 tok/s a 250-token chunk costs 1.7 s, so a corpus of realistic *long* notes would need an hour to
    /// reach the same decode count. Short notes are also the common shape in a vault, and they are where fixed
    /// per-decode cost shows up worst — which is the number the batching question needs.
    const DEFAULT_NOTES: usize = 2_048;

    /// §8's threshold, stated rather than implied: the spec's rule is "a full first pass fits in one evening", and this
    /// file takes an evening to be **4 hours** of unattended wall time. The number is arbitrary; writing it down is not —
    /// a projection judged against an unstated bound is a poster.
    const EVENING_HOURS: f64 = 4.0;

    /// Corpus sizes the projection is printed for. Round numbers, not this host's vault.
    const PROJECTION_NOTES: [usize; 3] = [1_000, 5_000, 20_000];

    fn artifact() -> Option<PathBuf> {
        std::env::var_os("NP_TEST_MODEL_PATH").map(PathBuf::from)
    }

    macro_rules! artifact_or_not_coverage {
        () => {
            match artifact() {
                Some(path) => path,
                None => {
                    println!(
                        "NOT COVERAGE: NP_TEST_MODEL_PATH is unset, so this case measured nothing. \
                         Re-run with it pointing at embeddinggemma-2-Q8_0.gguf (sha256 2188ac1d…)."
                    );
                    return;
                }
            }
        };
    }

    fn corpus_notes() -> usize {
        std::env::var("NP_CORPUS_NOTES")
            .ok()
            .and_then(|raw| raw.trim().parse::<usize>().ok())
            .filter(|n| *n > 0)
            .unwrap_or(DEFAULT_NOTES)
    }

    /// One loaded model in the process at a time. The budget is larger than the ~420 MiB of weights: a context reserves
    /// ~1.1 GiB of compute buffer (§14), so two residents at once would put nearly 3 GiB on a host with ~7.4 GB — and
    /// this lock is process-local, so running the suite twice in parallel gets no protection from it. Poison is taken
    /// through, so one red case cannot become a wall of missing coverage.
    fn model_lock() -> MutexGuard<'static, ()> {
        static LOCK: Mutex<()> = Mutex::new(());
        LOCK.lock().unwrap_or_else(|poisoned| poisoned.into_inner())
    }

    // ---------------------------------------------------------------------------
    // the fixture
    // ---------------------------------------------------------------------------

    struct Setup {
        data_dir: PathBuf,
        _tmp: TempDir,
        root: PathBuf,
    }

    impl Setup {
        /// Write `n` notes, scan, and leave a model row installed with [`PREFIX_DOC`].
        ///
        /// Each note is one heading plus two short sentences: measured on this artifact that is **one chunk** (the
        /// chunker merges its blocks while they fit 1,800 bytes) and roughly 40 tokens with the prefix — a realistic
        /// small note, and the shape a first pass over a vault is mostly made of.
        fn new(n: usize) -> Self {
            let tmp = TempDir::new().expect("test setup: tempdir");
            let root = tmp.path().join("ws");
            fs::create_dir_all(&root).expect("test setup: workspace dir");
            for index in 0..n {
                let body = Self::note_text(index);
                fs::write(root.join(format!("note-{index:05}.md")), body)
                    .expect("test setup: write note");
            }
            let setup = Self {
                data_dir: tmp.path().join("data"),
                _tmp: tmp,
                root,
            };
            scan::scan(&setup.root, &setup.cfg()).expect("test setup: first scan");
            setup.install_model();
            setup
        }

        /// A different note per index so the corpus is not one string repeated — a repeated corpus would flatter both
        /// the tokenizer (identical KV work) and the neighbour case below.
        fn note_text(index: usize) -> String {
            let topics = [
                "the kettle boiled dry while the phone rang",
                "a footnote about the treaty's third article",
                "grilled aubergine with yoghurt and mint",
                "the router drops ipv6 every second Tuesday",
                "she kept the receipts in a biscuit tin",
                "quantisation error is not the same as bias",
                "the tram stop moved eighty metres east",
                "a dog called Pastor, three legs, no regrets",
            ];
            let topic = topics[index % topics.len()];
            let second = topics[(index / topics.len()) % topics.len()];
            format!("# Note {index}\n\n{topic}. {second}.\n")
        }

        fn cfg(&self) -> Config {
            Config {
                data_dir: self.data_dir.clone(),
                embeddings: EmbeddingsConfig {
                    provider: EmbeddingProvider::LocalModel,
                    model: Some(MODEL_ID.to_string()),
                    dim: None,
                    max_tokens: np_core::config::DEFAULT_EMBEDDING_TOKENS,
                    // The policy default, spelled out: these fixtures must not inherit a worker count by
                    // accident if `Auto` ever stops being the default (`docs/design-m2-thread-policy.md` §3).
                    threads: np_core::config::ThreadCount::Auto,
                },
                ..Config::default()
            }
        }

        fn db(&self) -> Db {
            Db::open(&self.cfg().workspace_db(&self.root)).expect("test setup: open workspace db")
        }

        /// The row `np models install` would write, with the real artifact's digest filled in by the caller that needs
        /// verification to pass. This suite is about throughput and memory, so it writes the row directly — D12's
        /// verify-on-change has its own coverage in `artifact_verify.rs` and np-cli's argv cases.
        fn install_model(&self) {
            let db = self.db();
            db::insert_model(
                &db,
                MODEL_ID,
                DIM,
                // NULL digest: the row is a fixture for the runtime, not a claim about bytes on disk.
                None,
                PREFIX_DOC,
                "query: ",
                1_730_000_000,
            )
            .expect("test setup: install the model row");
        }

        /// Point the workspace's models directory at the real artifact under the row's id.
        fn link_artifact(&self, artifact: &Path) {
            let dir = self.cfg().workspace_models_dir(&self.root);
            fs::create_dir_all(&dir).expect("test setup: models dir");
            let link = dir.join(MODEL_ID);
            if link.symlink_metadata().is_ok() {
                fs::remove_file(&link).expect("test setup: replace the link");
            }
            std::os::unix::fs::symlink(artifact, &link).expect("test setup: link the artifact");
        }

        /// Rows stored for [`MODEL_ID`], with their widths.
        fn stored(&self) -> Vec<(i64, i64, Vec<u8>)> {
            let db = self.db();
            let conn = &db.0;
            let mut statement = conn
                .prepare("SELECT chunk_id, dim, vector FROM embeddings WHERE model = ?1 ORDER BY chunk_id")
                .expect("test: read the vectors");
            let rows = statement
                .query_map(rusqlite::params![MODEL_ID], |row| {
                    Ok((
                        row.get::<_, i64>(0)?,
                        row.get::<_, i64>(1)?,
                        row.get::<_, Vec<u8>>(2)?,
                    ))
                })
                .expect("test: query")
                .collect::<Result<Vec<_>, _>>()
                .expect("test: rows");
            rows
        }

        /// Every chunk's text, in id order — what the neighbour case feeds the model.
        ///
        /// `chunks.text` is a BLOB by design (F13: note bodies are bytes, not a UTF-8 promise), so this is where the
        /// fixture decides to read them as text. A note that is not valid UTF-8 would panic here, which is the right
        /// outcome for a benchmark that cannot measure what it cannot hand the model.
        fn chunk_texts(&self) -> Vec<String> {
            let db = self.db();
            let conn = &db.0;
            let mut statement = conn
                .prepare("SELECT text FROM chunks ORDER BY id")
                .expect("test: read the chunks");
            statement
                .query_map([], |row| row.get::<_, Vec<u8>>(0))
                .expect("test: query")
                .collect::<Result<Vec<_>, _>>()
                .expect("test: rows")
                .into_iter()
                .map(|bytes| String::from_utf8(bytes).expect("fixture notes are UTF-8"))
                .collect()
        }
    }

    // ---------------------------------------------------------------------------
    // measurement helpers
    // ---------------------------------------------------------------------------

    /// `VmRSS` / `VmHWM` in KiB from this process's own `/proc` entry — the same fields §14's C probes read, so a Rust
    /// number and a probe number mean the same thing.
    fn rss_kib(field: &str) -> u64 {
        let status = fs::read_to_string("/proc/self/status").expect("test: /proc/self/status");
        status
            .lines()
            .find(|line| line.starts_with(field))
            .and_then(|line| line.split_once(':'))
            .and_then(|(_, value)| {
                value
                    .trim()
                    .trim_end_matches(" kB")
                    .trim()
                    .parse::<u64>()
                    .ok()
            })
            .unwrap_or_else(|| panic!("test: {field} is present in /proc/self/status"))
    }

    /// `MemTotal` / `MemAvailable`, so a peak-RSS figure is read against the machine that produced it.
    fn meminfo() -> String {
        let text = fs::read_to_string("/proc/meminfo").expect("test: /proc/meminfo");
        let want = ["MemTotal:", "MemAvailable:"];
        let found: Vec<String> = text
            .lines()
            .filter(|line| want.iter().any(|field| line.starts_with(field)))
            .map(|line| line.trim().replace("  ", " "))
            .collect();
        assert_eq!(
            found.len(),
            2,
            "both memory fields are present in /proc/meminfo"
        );
        found.join(" ")
    }

    fn norm(vector: &[f32]) -> f64 {
        vector
            .iter()
            .map(|component| f64::from(*component) * f64::from(*component))
            .sum::<f64>()
            .sqrt()
    }

    fn cosine(a: &[f32], b: &[f32]) -> f64 {
        let dot: f64 = a
            .iter()
            .zip(b)
            .map(|(x, y)| f64::from(*x) * f64::from(*y))
            .sum();
        let denominator = norm(a) * norm(b);
        if denominator == 0.0 {
            0.0
        } else {
            dot / denominator
        }
    }

    /// Decode a stored row with the **production** decoder. A test-side `from_le_bytes` loop would pass on a storage
    /// format `Vector::from_bytes` refuses, and the width check it performs is part of what corpus rows are checked
    /// against.
    fn stored_vector(bytes: &[u8]) -> np_core::embed::Vector {
        np_core::embed::Vector::from_bytes(bytes, DIM as usize)
            .expect("a stored row decodes at the width its model declares")
    }

    /// Run one pass and return its report with the numbers a report cannot carry: wall time, batches observed, and RSS
    /// sampled between batches.
    struct Pass {
        /// Load + one decode, measured **outside** `wall`. The provider is lazy by design (§14), so a timer that starts
        /// before the first batch charges the model load to whatever decodes it happened to wrap: spread over 128
        /// decodes that is ~16 ms each, over 2,048 it is ~1 ms. Round 3's review caught exactly that (P1), and the
        /// per-decode cost split below is the number it contaminated.
        cold_start: Duration,
        /// Steady-state wall time of the pass itself, with the runtime already resident.
        wall: Duration,
        batches: usize,
        /// `VmRSS` at every [`SAMPLE_EVERY_BATCHES`]-th hook call, KiB.
        rss_samples: Vec<u64>,
        report: embed_run::EmbedReport,
    }

    const SAMPLE_EVERY_BATCHES: usize = 32;

    fn run_pass(setup: &Setup) -> Pass {
        let provider = np_core::local_model::LazyLocalEmbedder::new(
            &db::model_row(&setup.db(), MODEL_ID)
                .expect("row readable")
                .expect("row installed by the fixture"),
            &setup.cfg().workspace_models_dir(&setup.root).join(MODEL_ID),
            usize::try_from(DIM).expect("dim fits usize"),
            np_core::config::DEFAULT_EMBEDDING_TOKENS,
            // **Four workers, spelled out.** §14's corpus figures (16,161 chunks/hour, the drift over 2,048 decodes) are
            // a four-worker measurement, and `docs/design-m2-thread-policy.md` §6 says so; letting this fixture follow
            // the new `auto` default would silently re-define what those committed numbers mean. Re-running the corpus at
            // the auto count is its own measurement event, not a side effect of a config change.
            np_core::config::ThreadCount::Workers(
                std::num::NonZeroU32::new(4).expect("test setup: 4 is non-zero"),
            ),
        )
        .expect("test setup: a lazy provider for the linked artifact");

        let warm_started = Instant::now();
        provider
            .embed(&["warm up the runtime"])
            .expect("test setup: warming the lazy provider");
        let cold_start = warm_started.elapsed();

        let mut batches = 0usize;
        let mut rss_samples: Vec<u64> = Vec::new();
        let started = Instant::now();
        let report =
            embed_run::embed_with_inject(&setup.root, &setup.cfg(), &provider, &mut |_db_path| {
                batches += 1;
                // The first batch too: a corpus small enough to fit in one batch still has a "before and after" worth
                // printing, and sampling only on multiples would report it as no samples at all.
                if batches == 1 || batches.is_multiple_of(SAMPLE_EVERY_BATCHES) {
                    rss_samples.push(rss_kib("VmRSS:"));
                }
            })
            .expect("a corpus pass over an installed model completes");
        let wall = started.elapsed();

        Pass {
            cold_start,
            wall,
            batches,
            rss_samples,
            report,
        }
    }

    /// Sample the tokenizer over at most 64 chunks and return the mean tokens per chunk.
    ///
    /// Counting every chunk would spend minutes tokenising for a number that is already determined by the fixture: all
    /// notes share a shape, so the sample is stated with its size rather than the whole corpus being paid for.
    fn mean_tokens_per_chunk(artifact: &Path, texts: &[String]) -> (f64, usize) {
        let model =
            local_model::load(artifact, DIM as usize).expect("test: load the model for counting");
        let stride = texts.len().div_ceil(64).max(1);
        let mut counts = Vec::new();
        for text in texts.iter().step_by(stride) {
            counts.push(
                model
                    .count_tokens(PREFIX_DOC, text)
                    .expect("a fixture note is far under the ceiling"),
            );
        }
        // A zero-length sample would divide to NaN, and every throughput figure downstream would print `NaN` while
        // still failing its fence quietly — louder than a panic is not what a benchmark needs.
        assert!(
            !counts.is_empty(),
            "the token sample was empty, so there is no mean to report"
        );
        let mean = counts.iter().sum::<usize>() as f64 / counts.len() as f64;
        (mean, counts.len())
    }

    // ---------------------------------------------------------------------------
    // the corpus pass
    // ---------------------------------------------------------------------------

    /// Thousands of decodes in one process: throughput, memory behaviour, the second run's currency cost, and the
    /// projection with its threshold written down.
    #[test]
    fn a_first_pass_over_a_corpus_is_measured_and_projected() {
        let artifact = artifact_or_not_coverage!();
        let notes = corpus_notes();
        let _guard = model_lock();

        let setup = Setup::new(notes);
        setup.link_artifact(&artifact);
        let pass = run_pass(&setup);

        assert_eq!(pass.report.chunks_seen, notes);
        assert_eq!(
            pass.report.embedded, notes,
            "a fresh corpus owes every chunk"
        );
        assert_eq!(pass.report.errors, 0);
        assert_eq!(pass.report.refused_budget, 0);
        assert_eq!(pass.report.busy_gap, 0);
        // A pass that embedded everything and met no gap belongs to arm (i): `Current` is the outcome whose name says
        // "after this run, nothing is owed" — which is exactly what a clean first pass leaves behind.
        assert!(
            matches!(pass.report.outcome, EmbedOutcome::Current),
            "a clean first pass must land in arm (i), got {:?}",
            pass.report.outcome
        );

        let stored = setup.stored();
        assert_eq!(stored.len(), notes, "one row per embedded chunk");
        // Sampled width and unit norm: the same claim as checking all of them, at a bounded cost. The two ends are added
        // explicitly because `step_by` only reaches the last row when the stride divides the count — measured on 2,048
        // rows with stride 52 the walk stops at 2,028, and the tail is exactly where truncation would show up.
        let stride = stored.len().div_ceil(40).max(1);
        let mut checked: Vec<usize> = (0..stored.len()).step_by(stride).collect();
        for end in [0usize, stored.len() - 1] {
            if !checked.contains(&end) {
                checked.push(end);
            }
        }
        for index in checked {
            let (chunk_id, dim, bytes) = &stored[index];
            assert_eq!(*dim, DIM, "chunk {chunk_id}");
            assert_eq!(bytes.len(), DIM as usize * 4, "chunk {chunk_id}");
            let length = norm(stored_vector(bytes).components());
            assert!(
                (length - 1.0).abs() < 1e-3,
                "chunk {chunk_id} stored with ‖v‖ = {length}; §7 normalises"
            );
        }

        let tokens_per_chunk = mean_tokens_per_chunk(&artifact, &setup.chunk_texts());
        let estimated_tokens = tokens_per_chunk.0 * pass.report.embedded as f64;
        let seconds = pass.wall.as_secs_f64();
        let tokens_per_second = estimated_tokens / seconds;
        let chunks_per_hour = pass.report.embedded as f64 / seconds * 3_600.0;

        println!(
            "SUMMARY corpus notes={} chunks_embedded={} batches={} batch_chunks={} \
             wall_s={:.2} tokens_per_chunk_mean={:.1} (sampled {} of {}) tokens_total~={:.0} \
             tokens_per_second={:.1} chunks_per_hour={:.0} rss_first_kib={} rss_last_kib={} \
             rss_samples={} peak_kib={} cold_start_s={:.2}",
            notes,
            pass.report.embedded,
            pass.batches,
            EMBED_BATCH_CHUNKS,
            seconds,
            tokens_per_chunk.0,
            tokens_per_chunk.1,
            pass.report.chunks_seen,
            estimated_tokens,
            tokens_per_second,
            chunks_per_hour,
            // `0` here would read as "resident set was zero", which is impossible and false; the sample count beside it
            // is what tells a reader whether the two figures exist.
            pass.rss_samples.first().copied().unwrap_or(0),
            pass.rss_samples.last().copied().unwrap_or(0),
            pass.rss_samples.len(),
            rss_kib("VmHWM:"),
            pass.cold_start.as_secs_f64(),
        );
        // A context reserves ~1.1 GiB of compute buffer on top of the ~420 MiB of weights (§14), so "peak 437 MiB" only
        // means something against the machine that produced it: print what the host had, not just what we used.
        println!("SUMMARY host {}", meminfo());

        // The regression fence, deliberately wide. §8 asks for the spread and this host runs other work; a tight band
        // would fail on a noisy afternoon and train everyone to ignore this file. 40 tok/s is roughly a quarter of the
        // ~150 tok/s R6d round 1 measured through the same API — losing most of that means something broke, not that
        // the machine was busy.
        assert!(
            tokens_per_second > 40.0,
            "aggregate throughput fell to {tokens_per_second:.1} tok/s over {} chunks; \
             R6d round 1 measured ~150 tok/s through this same path",
            pass.report.embedded
        );

        // Memory: the question a one-decode-per-process harness cannot ask — and stated for what it is, because the
        // arithmetic of this fence is honest only if you do the division. Nine samples over 256 batches, ceiling
        // 262,144 KiB: it fails on retention above ~128 KiB per chunk. A leak of 2 KiB per decode (+4 MiB across the
        // whole pass) walks straight through it. That is a smoke test for "something is inflating without bound", not a
        // leak detector; two recorded runs drifted +1,436 and +1,552 KiB (~0.7 KiB per decode), which is allocator
        // territory. Re-measure the band before changing it — §9's R6d row names the runs that set it.
        // A single sample would make the drift below identically zero and the check silently vacuous — which is what a
        // raised `SAMPLE_EVERY_BATCHES` would do to a smaller corpus. So: whenever there were enough batches to expect
        // more than one, insist that we got them.
        if pass.batches >= SAMPLE_EVERY_BATCHES {
            assert!(
                pass.rss_samples.len() >= 2,
                "{} batches produced {} RSS sample(s); sampling is broken, not the memory behaviour",
                pass.batches,
                pass.rss_samples.len()
            );
        }
        if pass.rss_samples.len() >= 2 {
            let first = pass.rss_samples[0];
            let last = *pass.rss_samples.last().expect("checked non-empty");
            let drift_kib = last.abs_diff(first);
            println!(
                "SUMMARY rss_drift_kib={drift_kib} samples={}",
                pass.rss_samples.len()
            );
            assert!(
                drift_kib < 262_144,
                "VmRSS moved {drift_kib} KiB between the first and last mid-run sample across {} batches; \
                 a corpus pass is not supposed to grow by more than a quarter of a gigabyte",
                pass.batches
            );
        }

        // §8's arithmetic, printed rather than implied.
        println!(
            "PROJECTION threshold = one evening = {EVENING_HOURS:.0} h of unattended wall time \
             (measured rate: {chunks_per_hour:.0} chunks/hour)"
        );
        for size in PROJECTION_NOTES {
            let hours = size as f64 / chunks_per_hour;
            println!(
                "PROJECTION notes={size:>6} hours={hours:>6.2} fits_evening={}",
                if hours <= EVENING_HOURS { "yes" } else { "no" }
            );
            // No assertion about the arithmetic on purpose: `hours` is computed from `chunks_per_hour` two lines above,
            // so any check re-deriving one from the other can never fail (round 3's review named this). What is worth a
            // fence is that the rate is a real number at all — the throughput fence above and this guard both do that.
            assert!(chunks_per_hour > 0.0);
        }

        // The second run: nothing owed, so nothing loaded and nothing written — and the currency query measured at
        // thousands of stamped rows, which is an M4 number nobody had.
        let second = run_pass(&setup);
        assert_eq!(
            second.report.embedded, 0,
            "a workspace that owes nothing embeds nothing"
        );
        assert_eq!(second.report.current, notes);
        println!(
            "SUMMARY second_run wall_s={:.2} current={} batches={}",
            second.wall.as_secs_f64(),
            second.report.current,
            second.batches
        );
    }

    // ---------------------------------------------------------------------------
    // the number batching would be buying
    // ---------------------------------------------------------------------------

    /// Two cohorts of notes in one process — short and medium — so the fixed cost per decode can be solved for instead
    /// of asserted. `a` (ms spent per decode regardless of length) is what a batched forward pass could amortise; if it
    /// comes out near zero, batching across texts has nothing to buy and D13 becomes the only lever left.
    #[test]
    fn short_and_long_cohorts_separate_fixed_cost_from_per_token_cost() {
        let artifact = artifact_or_not_coverage!();
        let _guard = model_lock();

        // Two workspaces, one cohort each, each embedded by its own lazy provider. The guards are scoped so the first
        // model is dropped before the second loads: two residents at once would put ~840 MiB on a host with ~7.4 GB.
        let (short_notes, short_tokens) = {
            let setup = Setup::new(128);
            setup.link_artifact(&artifact);
            let pass = run_pass(&setup);
            assert!(
                pass.report.embedded > 0,
                "a cohort that embedded nothing has no per-decode cost to divide by"
            );
            let mean = mean_tokens_per_chunk(&artifact, &setup.chunk_texts()).0;
            (pass.wall.as_secs_f64() / pass.report.embedded as f64, mean)
        };
        let (long_notes, long_tokens) = {
            // Longer notes: the same fixture shape with a paragraph appended, which stays one chunk (under 1,800 bytes).
            let tmp = TempDir::new().expect("test setup: tempdir");
            let root = tmp.path().join("ws");
            fs::create_dir_all(&root).expect("test setup: workspace dir");
            for index in 0..48 {
                let mut body = Setup::note_text(index);
                for repeat in 0..6 {
                    body.push_str(&format!(
                        "\n\nAdditional paragraph {repeat} about {index}: {}.\n",
                        Setup::note_text(index)
                    ));
                }
                fs::write(root.join(format!("long-{index:04}.md")), body)
                    .expect("test setup: write note");
            }
            let setup = Setup {
                data_dir: tmp.path().join("data"),
                _tmp: tmp,
                root,
            };
            scan::scan(&setup.root, &setup.cfg()).expect("test setup: first scan");
            setup.install_model();
            setup.link_artifact(&artifact);
            let pass = run_pass(&setup);
            assert_eq!(
                pass.report.refused_budget, 0,
                "these notes must stay inside the budget"
            );
            let mean = mean_tokens_per_chunk(&artifact, &setup.chunk_texts()).0;
            (pass.wall.as_secs_f64() / pass.report.embedded as f64, mean)
        };

        // t_decode = a + b·tokens  →  b from the two cohorts, a by substitution.
        let per_token = (long_notes - short_notes) / (long_tokens - short_tokens);
        let fixed_ms = (short_notes - per_token * short_tokens) * 1_000.0;
        println!(
            "SUMMARY cohort_short tokens={:.1} seconds_per_decode={:.4} \
             cohort_long tokens={:.1} seconds_per_decode={:.4} \
             derived_fixed_ms_per_decode={fixed_ms:.1} derived_ms_per_token={:.3}",
            short_tokens,
            short_notes,
            long_tokens,
            long_notes,
            per_token * 1_000.0
        );

        // A fence, not a promise: if the derived fixed cost leaves the range that makes physical sense (you cannot pay
        // negative time, and a fixed cost above a second would have shown up in every earlier round), the linear model
        // or the harness is wrong and this file should say so rather than print arithmetic nobody checked.
        assert!(
            fixed_ms > -50.0 && fixed_ms < 1_000.0,
            "derived per-decode fixed cost of {fixed_ms:.1} ms is outside what the linear model can mean; \
             short cohort {} s @ {:.1} tokens, long cohort {} s @ {:.1} tokens",
            short_notes,
            short_tokens,
            long_notes,
            long_tokens
        );
    }

    // ---------------------------------------------------------------------------
    // quality sanity, not quality proof (§8)
    // ---------------------------------------------------------------------------

    /// Nearest neighbours for three queries over the fixture corpus, printed for a human — spec §13 asks for
    /// spot-checks "in German and English", and F18 is still the reason there is no recall number here: labels are what
    /// we do not have. The one assertion needs no labels either: a note asked as itself must come back first.
    #[test]
    fn a_note_queried_as_itself_ranks_first_and_the_neighbours_are_printed() {
        let artifact = artifact_or_not_coverage!();
        let _guard = model_lock();

        let setup = Setup::new(24);
        setup.link_artifact(&artifact);
        let pass = run_pass(&setup);
        assert_eq!(pass.report.embedded, 24);

        let stored = setup.stored();
        let texts = setup.chunk_texts();
        assert_eq!(stored.len(), texts.len());

        let model = local_model::load(&artifact, DIM as usize).expect("test: load the model");
        let queries = [
            texts[0].clone(),
            "the router drops ipv6 every second Tuesday".to_string(),
            "Wie spät ist es?".to_string(),
        ];
        for query in queries {
            // The document prefix, because these are documents being matched against documents; the row's query
            // prefix is a different dressing and this case is not about that difference (§3).
            let vector = model
                .embed_text(
                    PREFIX_DOC,
                    &query,
                    np_core::config::DEFAULT_EMBEDDING_TOKENS,
                )
                .expect("test: embed the query");
            assert!(
                (norm(vector.components()) - 1.0).abs() < 1e-3,
                "a query vector must be unit length too"
            );
            let mut ranked: Vec<(f64, &str)> = stored
                .iter()
                .zip(texts.iter())
                .map(|((_, _, bytes), text)| {
                    (
                        cosine(vector.components(), stored_vector(bytes).components()),
                        text.as_str(),
                    )
                })
                .collect();
            ranked.sort_by(|a, b| b.0.total_cmp(&a.0));
            println!("QUERY {query:?}");
            for (rank, (score, text)) in ranked.iter().take(3).enumerate() {
                let one_line = text.replace('\n', " ");
                println!(
                    "  {} {:02} {:.4}  {}",
                    if rank == 0 { "*" } else { " " },
                    rank + 1,
                    score,
                    one_line.chars().take(72).collect::<String>()
                );
            }
            // Only the first query has a guaranteed answer: it *is* a note in the corpus.
            if query == texts[0] {
                assert_eq!(
                    ranked[0].1, texts[0],
                    "a note asked as itself did not rank first — §7's pooling would be wrong, not merely unmeasured"
                );
            }
        }
    }
}
