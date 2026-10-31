//! R6d's measurements: where the window boundary actually sits, what a realistic chunk costs, and what verifying
//! the artifact by digest would cost per run.
//!
//! Every case here needs the real artifact — each one is a claim about what *this* model answers — so with
//! `NP_TEST_MODEL_PATH` unset they print `NOT COVERAGE` and pass, for the standing reason (F18): an absent 310 MB
//! file must not look like a failure, and must not look like coverage either.
//!
//! ```text
//! NP_TEST_MODEL_PATH=/path/to/embeddinggemma-2-Q8_0.gguf \
//!   cargo test -p np-core --features local-model --test local_window -- --nocapture
//! ```
//!
//! Three things this file exists to settle, in the order they cost time:
//!
//! * **the window boundary is a real edge, not a rounding margin.** `local_embed.rs` refuses *above* the window and
//!   says so plainly, but it never asserts that **exactly** `window` tokens is accepted — Scope B's P2-12, deferred
//!   because pinning it means a ~90 s decode of a synthetic window-length note. R6d is the round that pays for that
//!   decode anyway, so the assertion lives here: accept at exactly `window`, refuse at `window + 1`. A `>` written
//!   where `>=` belongs now fails instead of surviving with a comment explaining that it would.
//! * **what one realistic chunk costs**, at the default budget and near the ceiling, printed rather than asserted.
//!   §5 quotes "~8 s per worst-case chunk at 120–141 tok/s", an interpolation between two probe points; this turns
//!   that into a measurement over chunks the chunker can actually produce (`MAX_CHUNK_BYTES = 1800`).
//! * **what D12 would cost**: one streaming SHA-256 over the artifact per `np embed` run, timed against the digest
//!   the pin table says it must be.

/// The forward-pass claims. All of them are artifact-gated: there is no way to ask for a vector without a model.
#[cfg(feature = "local-model")]
mod window {
    use std::path::{Path, PathBuf};
    use std::sync::{Mutex, MutexGuard};
    use std::time::Instant;

    use np_core::embed::EmbedError;
    use np_core::hash::sha256_file;
    use np_core::local_model::{self, LoadedModel};

    /// The width `embeddinggemma-2-Q8_0.gguf` answers with — the same 768 every other artifact suite pins from the
    /// file's own header.
    const DIM: usize = 768;

    /// The artifact's own digest: sha256 of its 309,855,456 bytes, the same value
    /// `docs/design-m2-local-embeddings.md` §3 records for this file.
    ///
    /// Pinned rather than "some hex of the right length", because the point of this case is what *this* artifact
    /// costs to verify — a benchmark against an impostor would be a number about nothing. The pin earns its keep
    /// immediately: the first version of this constant was written from memory and the assertion failed on the real
    /// file, which is the argument for taking the value from the record instead of from a head.
    const ARTIFACT_SHA256: &str =
        "2188ac1deca4b77dffefd603c2776a9d76d9d74ec01841392982ebb840b09135";

    /// One sentence, 17 tokens with specials on this artifact (`local_embed.rs` measures it the same way). Long
    /// enough that a coarse search converges fast, short enough that a single word is usually worth one token.
    const UNIT: &str =
        "The quick brown fox jumps over the lazy dog while the rain falls softly on the roof. ";

    /// Words that count as one token each on this artifact, used to close a gap of a few tokens exactly. Greedy
    /// search verifies each choice with the model's own count rather than trusting this list — if an artifact ever
    /// tokenises one of these differently, the loop takes the next candidate instead of landing off by one.
    const SINGLE_TOKEN: &[&str] = &["a", "an", "the", "of", "and", "is", "on", "in", "to", "ox"];

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

    /// One loaded model in the process at a time: ~420 MiB resident each, and this host has ~6 GB. Poison is taken
    /// through, so one red case cannot turn into a wall of missing coverage.
    fn artifact_lock() -> MutexGuard<'static, ()> {
        static LOCK: Mutex<()> = Mutex::new(());
        LOCK.lock().unwrap_or_else(|poisoned| poisoned.into_inner())
    }

    /// Count `text` with no prefix, taking the count from a budget refusal when there is one: `count_tokens` caps
    /// itself at `MAX_EMBEDDING_TOKENS`, which since §5's decision C **equals** the window — so the only way to ask
    /// "how long is window + 1 tokens?" is to read the number back out of the refusal that says it is over.
    fn count(model: &LoadedModel, text: &str) -> usize {
        match model.count_tokens("", text) {
            Ok(n) => n,
            Err(EmbedError::TokenBudgetExceeded { tokens, budget }) => {
                assert!(
                    tokens > budget,
                    "a refusal naming {tokens} against a ceiling of {budget} is not over its own ceiling"
                );
                tokens
            }
            Err(other) => panic!("counting should answer or refuse on budget, got {other:?}"),
        }
    }

    /// `n` copies of [`UNIT`].
    fn text_of(n_copies: usize) -> String {
        UNIT.repeat(n_copies)
    }

    /// A chunk whose token count is **exactly** `target`, found with the model as the only oracle.
    ///
    /// Two phases because a fine search over a 40 KB string is not free (`local_embed.rs` measured ~55 s for 450
    /// tokenisations of one): a binary search finds the largest number of [`UNIT`] copies that still fits, then
    /// single-token words close the remaining gap one at a time, each verified by counting rather than assumed.
    fn note_at_exactly_tokens(model: &LoadedModel, target: usize) -> (String, usize) {
        // Phase one: largest k with count(k · UNIT) ≤ target. The count is monotone in k, so bisect — 14 counts
        // cover the range for a window of 8,192 tokens.
        let mut low = 0usize;
        let mut high = target / 4 + 16; // generous upper bound: UNIT is 17 tokens, this is not tight
        while low < high {
            let mid = low + (high - low).div_ceil(2);
            if count(model, &text_of(mid)) <= target {
                low = mid;
            } else {
                high = mid - 1;
            }
        }
        let mut note = text_of(low);
        let mut n = count(model, &note);

        // Phase two: close the gap with one-token appends, verified each time. If a candidate is worth more than
        // one token on some future artifact, the next candidate is tried; if none of them fits exactly, that is a
        // finding about this helper and it says so rather than landing off by one.
        while n < target {
            let mut advanced = false;
            for word in SINGLE_TOKEN {
                let candidate = format!("{note} {word}");
                let m = count(model, &candidate);
                if m <= target {
                    note = candidate;
                    n = m;
                    advanced = true;
                    break;
                }
            }
            assert!(
                advanced,
                "no single-token word fits between {n} and {target}; this helper cannot build an \
                 exactly-{target}-token input and the boundary claim needs it to"
            );
        }
        (note, n)
    }

    /// Peak resident set of this process in KiB, from its own `/proc` entry — the same field §14's C probes read
    /// (`VmHWM`), so a Rust-side number and a probe-side one mean the same thing.
    fn peak_rss_kib() -> u64 {
        let status = std::fs::read_to_string("/proc/self/status")
            .expect("test: /proc/self/status is readable");
        status
            .lines()
            .find_map(|line| {
                let (_, value) = line.split_once(':')?;
                line.starts_with("VmHWM:").then(|| {
                    value
                        .trim()
                        .trim_end_matches(" kB")
                        .trim()
                        .parse::<u64>()
                        .ok()
                })?
            })
            .expect("test: VmHWM is present in /proc/self/status")
    }

    /// ‖v‖ for a unit-length vector, printed wherever a decode happens.
    fn norm(vector: &[f32]) -> f64 {
        vector
            .iter()
            .map(|component| f64::from(*component) * f64::from(*component))
            .sum::<f64>()
            .sqrt()
    }

    /// The boundary claim, and the ~100 s this file exists to spend.
    ///
    /// Mutations that die here and died nowhere else (Scope B's P2-12, surviving since R6b2):
    /// * `n > budget` → `n >= budget` inside [`np_core::local_model::LoadedModel::embed_text`]: an exactly-window
    ///   length chunk is then refused, and the first assertion below fails;
    /// * the window check's `>` → `>=`: `window + 1` tokens is then handed to `llama_decode`, which is the abort
    ///   §5's whole boundary rule exists to avoid — this case would either refuse with the wrong sentence or die in
    ///   C, and both are louder than the comment that currently admits the mutant survives.
    #[test]
    fn exactly_the_window_is_accepted_and_one_token_more_is_not() {
        let path = artifact_or_not_coverage!();
        let _guard = artifact_lock();
        let model = local_model::load(&path, DIM).unwrap_or_else(|problem| {
            panic!("loading {} should succeed: {problem}", path.display())
        });
        let window = model.window();
        assert!(window > 0, "a context with no window cannot embed anything");

        let (note, tokens) = note_at_exactly_tokens(&model, window);
        assert_eq!(
            tokens, window,
            "the point of this case is the exact edge; landing at {tokens} would test something else"
        );

        let started = Instant::now();
        let vector = model
            .embed_text("", &note, window)
            .unwrap_or_else(|problem| {
                panic!("{tokens} tokens equals the window ({window}) and must be accepted, got {problem:?}")
            });
        let elapsed = started.elapsed();
        let seconds = elapsed.as_secs_f64();
        println!(
            "BENCH: window boundary — accepted exactly {tokens} tokens in {seconds:.1} s \
             ({:.0} tok/s), ‖v‖ = {:.6}, peak RSS {} KiB",
            f64::from(tokens as u32) / seconds,
            norm(vector.components()),
            peak_rss_kib()
        );
        assert!(
            (norm(vector.components()) - 1.0).abs() < 1e-3,
            "a near-ceiling chunk still has to come back normalised"
        );

        // One token more, and the budget arm answers before `llama_decode` is reached — so this half costs counts,
        // not a decode, and can assert the numbers rather than a substring.
        let over = format!("{note} a");
        let over_tokens = count(&model, &over);
        assert_eq!(
            over_tokens,
            window + 1,
            "the over-by-one input has to be over by exactly one, or the edge is not being tested"
        );
        let refused = model
            .embed_text("", &over, window)
            .expect_err("window + 1 tokens must never reach the runtime");
        match refused {
            EmbedError::TokenBudgetExceeded { tokens, budget } => {
                assert_eq!(tokens, window + 1, "the refusal names what was asked for");
                assert_eq!(budget, window, "and what was allowed");
                println!("BENCH: window boundary — refused {tokens} against a budget of {budget}, before any decode");
            }
            other => panic!("expected the budget refusal at the window edge, got {other:?}"),
        }
    }

    /// What a chunk the chunker can actually produce costs, at the default budget and near the ceiling.
    ///
    /// Printed, not thresholded: §14's rule is that magnitudes are never asserted (a wall-clock gate is a flake
    /// factory on shared hardware), but the numbers become the source for §5's quoted "~8 s per worst-case chunk",
    /// which today is an interpolation between two probe points rather than a measurement of this path.
    #[test]
    fn a_realistic_chunk_is_timed_at_the_default_budget_and_near_the_ceiling() {
        let path = artifact_or_not_coverage!();
        let _guard = artifact_lock();
        let model = local_model::load(&path, DIM).unwrap_or_else(|problem| {
            panic!("loading {} should succeed: {problem}", path.display())
        });

        // Six chunks of `MAX_CHUNK_BYTES`, which is the widest thing the chunker emits and therefore the worst case
        // a normal run sees. Six because four is enough to see variance and twenty would buy nothing.
        const CHUNK_BYTES: usize = np_core::chunk::MAX_CHUNK_BYTES;
        const CHUNKS: usize = 6;
        let filler = "The quick brown fox jumps over the lazy dog. ";
        let raw: String = filler.repeat(CHUNK_BYTES / filler.len() + 1)[..CHUNK_BYTES].to_string();
        let tokens = count(&model, &raw);
        assert!(
            tokens <= np_core::config::DEFAULT_EMBEDDING_TOKENS,
            "a {CHUNK_BYTES}-byte chunk counts {tokens} tokens, over the default budget of {} — §5's \
             arithmetic that says it should fit has drifted",
            np_core::config::DEFAULT_EMBEDDING_TOKENS
        );

        let mut seconds = 0.0;
        for index in 0..CHUNKS {
            let started = Instant::now();
            let vector = model
                .embed_text("", &raw, np_core::config::DEFAULT_EMBEDDING_TOKENS)
                .unwrap_or_else(|problem| {
                    panic!("chunk {index} at the default budget: {problem:?}")
                });
            let each = started.elapsed().as_secs_f64();
            seconds += each;
            assert!(
                (norm(vector.components()) - 1.0).abs() < 1e-3,
                "chunk {index} came back un-normalised"
            );
            println!("BENCH: chunk {index} — {tokens} tokens in {each:.2} s");
        }
        println!(
            "BENCH: default budget — {CHUNKS} chunks of {CHUNK_BYTES} bytes ({tokens} tokens each) in \
             {seconds:.2} s, mean {:.2} s per chunk ({:.0} tok/s), peak RSS {} KiB",
            seconds / CHUNKS as f64,
            f64::from((tokens * CHUNKS) as u32) / seconds,
            peak_rss_kib()
        );

        // And the same path near the ceiling, one chunk only: this is the shape §5 decision C was taken for, and
        // the number quoted there ("7,999 tokens in 101 s") came from a C probe rather than from this API.
        let ceiling = np_core::config::MAX_EMBEDDING_TOKENS;
        let (big, big_tokens) = note_at_exactly_tokens(&model, ceiling.saturating_sub(200));
        let started = Instant::now();
        let vector = model
            .embed_text("", &big, ceiling)
            .unwrap_or_else(|problem| {
                panic!("a {big_tokens}-token chunk at a ceiling of {ceiling}: {problem:?}")
            });
        let big_seconds = started.elapsed().as_secs_f64();
        println!(
            "BENCH: near the ceiling — one chunk of {big_tokens} tokens in {big_seconds:.1} s \
             ({:.0} tok/s), ‖v‖ = {:.6}, peak RSS {} KiB",
            f64::from(big_tokens as u32) / big_seconds,
            norm(vector.components()),
            peak_rss_kib()
        );
    }

    /// D12's price: what verifying the artifact against `models.sha256` would cost on every `np embed` run.
    ///
    /// The digest is asserted, not just timed — a benchmark of the wrong file is a number about nothing, and this
    /// is the same pin `third_party/llama-cpp.toml`'s discipline was taken for: measure the thing you ship.
    #[test]
    fn verifying_the_artifact_by_digest_is_timed_and_pinned() {
        let _guard = artifact_lock();
        let path: PathBuf = artifact_or_not_coverage!();
        let size = std::fs::metadata(&path)
            .expect("test: stat the artifact")
            .len();

        let started = Instant::now();
        let digest = sha256_file(&path).expect("bench: hash the artifact");
        let seconds = started.elapsed().as_secs_f64();
        println!(
            "BENCH: digest verification — {size} bytes in {seconds:.2} s \
             ({:.0} MiB/s); this is what D12 would add to every `np embed` run",
            (size as f64 / (1024.0 * 1024.0)) / seconds
        );
        assert_eq!(
            digest, ARTIFACT_SHA256,
            "the artifact under test is not the one this file's numbers describe: {digest}"
        );
    }

    /// The path helper exists for symmetry with the other artifact suites; asserting it does not silently become a
    /// second source of truth about where the model lives.
    #[allow(dead_code)]
    fn artifact_is_a_file(path: &Path) -> bool {
        path.is_file()
    }
}
