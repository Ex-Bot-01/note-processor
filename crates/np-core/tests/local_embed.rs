//! R6b2's claim about a **real** GGUF: text goes in and one unit-length 768-component vector comes out.
//!
//! Everything here needs the artifact, because every claim is about what *this* model answers — so each case
//! prints `NOT COVERAGE` and passes when `NP_TEST_MODEL_PATH` is unset (F18: an absent 310 MB file must not look
//! like a failure, and must not look like coverage either). No `#[ignore]`, for the reason R6b1's file states: an
//! ignored test is the silent skip that rule exists to forbid. On a GitHub runner this file contributes twelve
//! printed `NOT COVERAGE` lines and the proof that it compiles against the pinned header — every case asks a model
//! for a vector. That is what `.github/workflows/ci.yml`'s feature row says, and it corrects an earlier claim here
//! of "four lines", which was true of `local_load.rs` and copied across wrongly.
//!
//! ```text
//! NP_TEST_MODEL_PATH=/path/to/embeddinggemma-2-Q8_0.gguf \
//!   cargo test -p np-core --features local-model --test local_embed -- --nocapture
//! ```
//!
//! Three rules the cases are built around:
//!
//! * **One load at a time.** A loaded model is ~420 MiB resident (§14) and this host has ~6 GB, so the artifact
//!   cases share a mutex; two loads in flight is how a green suite turns into an OOM kill.
//! * **Numbers are printed, magnitudes are not asserted** — except where a magnitude *is* the claim (‖v‖ = 1, and
//!   "closer than" for the cosine ordering). A threshold on wall-clock or RSS is a flake factory.
//! * **The semantic test asserts an ordering, never a score.** `cos(cat, feline) > cos(cat, revenue)` with the
//!   three values printed. If this model ever fails that ordering it is a finding about the artifact, not a
//!   sentence to re-tune until the test goes green.

/// The forward-pass claims. All of them are artifact-gated: there is no way to ask for a vector without a model.
#[cfg(feature = "local-model")]
mod embedding {
    use std::path::{Path, PathBuf};
    use std::sync::{Mutex, MutexGuard};
    use std::time::Instant;

    use np_core::db::ModelRow;
    use np_core::embed::{EmbedError, Embedder};
    use np_core::local_model::{self, LazyLocalEmbedder, LoadedModel, LocalEmbedder, TextKind};

    /// The width `embeddinggemma-2-Q8_0.gguf` answers with — the same 768 `tests/local_load.rs` pins from the
    /// file's own header.
    const DIM: usize = 768;

    /// One sentence, measured at **17 tokens with specials** on this artifact (`llama_tokenize` answered `-17`
    /// for a 2-slot buffer). Long enough that a handful of copies clears the 512-token threshold that used to
    /// abort the process, and short enough that the token count of `n` copies is roughly `17n` for the ranges
    /// below — always re-read with [`LoadedModel::count_tokens`] rather than trusted.
    const UNIT: &str =
        "The quick brown fox jumps over the lazy dog while the rain falls softly on the roof. ";

    fn artifact() -> Option<PathBuf> {
        std::env::var_os("NP_TEST_MODEL_PATH").map(PathBuf::from)
    }

    macro_rules! artifact_or_not_coverage {
        () => {
            match artifact() {
                Some(path) => path,
                None => {
                    println!(
                        "NOT COVERAGE: NP_TEST_MODEL_PATH is unset, so no vector was produced by this case. \
                         Re-run with it pointing at embeddinggemma-2-Q8_0.gguf (sha256 2188ac1d…) to embed text."
                    );
                    return;
                }
            }
        };
    }

    /// One loaded model in the process at a time — see this file's header for why that is a memory rule and not
    /// a cleanliness one. Poison is taken through, so one red case cannot turn into a wall of missing coverage.
    fn artifact_lock() -> MutexGuard<'static, ()> {
        static LOCK: Mutex<()> = Mutex::new(());
        LOCK.lock().unwrap_or_else(|poisoned| poisoned.into_inner())
    }

    /// Load for one case. `DIM` is what the artifact answers, so every case that wants a working model gets one.
    fn load(path: &Path) -> LoadedModel {
        local_model::load(path, DIM).unwrap_or_else(|problem| {
            panic!("loading {} should succeed: {problem}", path.display())
        })
    }

    /// A `models` row with the prefixes §5 gives this artifact. Constructed by hand rather than from a database:
    /// nothing in this round reads or writes `embeddings`, and a real row would only add a file to open.
    fn row(prefix_doc: &str, prefix_query: &str) -> ModelRow {
        ModelRow {
            id: "embeddinggemma-2".to_string(),
            dim: DIM as i64,
            sha256: None,
            prefix_doc: prefix_doc.to_string(),
            prefix_query: prefix_query.to_string(),
            installed_at: 0,
        }
    }

    /// The two provider shapes must declare the same bytes for the same row, or the currency stamp stops
    /// describing what the model read.
    ///
    /// Why this needs the artifact: `LocalEmbedder` only exists once a model is loaded, and the claim is about the
    /// pair — the lazy half answers before llama.cpp is running (that is what `embed_run` stamps), the loaded half
    /// is what would be stamped if a caller embedded straight from a resident model. Mutating either declaration to
    /// ignore `prefix_doc` fails this case; the argv-level case in np-cli catches only the lazy half, because that
    /// is the one `np embed` wires up today.
    #[test]
    fn both_provider_shapes_declare_the_same_bytes_for_the_model() {
        let path = artifact_or_not_coverage!();
        let _guard = artifact_lock();

        let prefix_doc = "task: search result | query: ";
        let texts = [
            "the cat sits on the warm mat",
            "revenue rose because the pipeline stopped leaking",
        ];

        let loaded = LocalEmbedder::new(load(&path), &row(prefix_doc, "query: "), 1_024)
            .expect("test setup: a loaded document embedder");
        let lazy = LazyLocalEmbedder::new(&row(prefix_doc, "query: "), &path, DIM, 1_024)
            .expect("test setup: a lazy provider over the same row and file");

        let resident = loaded.model_inputs(&texts);
        let not_yet_loaded = lazy.model_inputs(&texts);
        assert_eq!(
            resident, not_yet_loaded,
            "the bytes stamped before a load must be the bytes a loaded model would read"
        );
        for (input, text) in resident.iter().zip(texts) {
            assert_eq!(
                input,
                &format!("{prefix_doc}{text}"),
                "and the dressing is this row's `prefix_doc`, verbatim and in front"
            );
        }
    }

    /// ‖a‖ to two decimal places — printed everywhere, because "is this row actually scaled" is the first
    /// question a normalisation bug asks and `1.00` answers it in one glance.
    fn norm(v: &[f32]) -> f64 {
        v.iter()
            .map(|x| f64::from(*x) * f64::from(*x))
            .sum::<f64>()
            .sqrt()
    }

    fn cosine(a: &[f32], b: &[f32]) -> f64 {
        let dot: f64 = a
            .iter()
            .zip(b)
            .map(|(x, y)| f64::from(*x) * f64::from(*y))
            .sum();
        let na = norm(a);
        let nb = norm(b);
        assert!(na > 0.0 && nb > 0.0, "a zero vector cannot be compared");
        dot / (na * nb)
    }

    fn components(v: &np_core::embed::Vector) -> Vec<f32> {
        v.components().to_vec()
    }

    /// The token count of `text`, **including** when it exceeds `count_tokens`' own ceiling — where the refusal's
    /// payload is exactly the number the caller wanted. Bytes-per-token has never been load-bearing in this file, and
    /// an over-window input is the one place a test cannot get its count from `Ok`.
    fn count_even_over_ceiling(model: &LoadedModel, text: &str) -> usize {
        match model.count_tokens("", text) {
            Ok(n) => n,
            Err(EmbedError::TokenBudgetExceeded { tokens, budget }) => {
                assert!(tokens > budget, "a budget refusal that names {tokens} against a ceiling of {budget} is not over its own ceiling: this refusal would be a bug, not evidence");
                tokens
            }
            Err(other) => {
                panic!("counting should either answer or refuse on budget, got {other:?}")
            }
        }
    }

    /// `n` copies of [`UNIT`], for inputs measured in tokens rather than bytes.
    fn text_of_about(n_copies: usize) -> String {
        UNIT.repeat(n_copies)
    }

    /// Drop the last `k` space-separated words from `text`, or `None` when there are not that many to drop.
    fn drop_words(text: &str, k: usize) -> Option<String> {
        let mut parts: Vec<&str> = text.split(' ').collect();
        if parts.len() <= k + 1 {
            return None;
        }
        parts.truncate(parts.len() - k);
        Some(parts.join(" "))
    }

    /// Grow `note`, then trim it word by word, until its token count sits on or just above `target` — with the
    /// model's own count as the oracle and **never** a bytes-per-token estimate. Returns the text and the count that
    /// was actually measured, because "just above" is not always reachable: one word can be worth several tokens, so
    /// the caller gets the number rather than the promise. Used where a test needs to be over a line by *one*, which is
    /// the only way to show a refusal fires at the boundary instead of somewhere above it.
    fn note_at_least_tokens(model: &LoadedModel, target: usize) -> (String, usize) {
        let mut note = text_of_about(target / 9);
        let mut n = count_even_over_ceiling(model, &note);
        while n < target {
            note.push_str(UNIT);
            n = count_even_over_ceiling(model, &note);
        }
        // Trim by dropping words from the end, halving the step whenever a cut would cross below the line. The naive
        // one-word-at-a-time loop spends ~450 tokenizations of a 40 KB string closing a ~450-token gap — measured at
        // +55 s on this file — for nothing this test needs; halving converges in a couple of dozen counts.
        let mut step = n - target;
        while n > target && step > 0 {
            let Some(trimmed) = drop_words(&note, step) else {
                break;
            };
            let m = count_even_over_ceiling(model, &trimmed);
            if m >= target {
                note = trimmed;
                n = m;
            } else {
                step /= 2;
            }
        }
        (note, n)
    }

    /// The shape of a real vector: 768 finite components at unit length.
    #[test]
    fn a_sentence_returns_768_components_of_unit_length() {
        let path = artifact_or_not_coverage!();
        let _serial = artifact_lock();
        let model = load(&path);

        let v = model
            .embed_text("", "The cat sat on the mat.", 2_048)
            .expect("a 9-token sentence is inside every budget in play");
        let parts = components(&v);

        // Both are pins against a future change rather than independent evidence: `embed_text` copies exactly
        // `n_embd_out` floats, so nothing here could notice a width the copy itself got wrong. What can fail on its
        // own is the norm below, plus `tests/local_load.rs` reading 768 out of the model.
        assert_eq!(v.dim(), DIM, "the vector is as wide as the model declares");
        assert_eq!(parts.len(), DIM);
        assert!(
            parts.iter().all(|x| x.is_finite()),
            "no component is NaN or infinite"
        );
        let n = norm(&parts);
        assert!(
            (n - 1.0).abs() < 1e-6,
            "the stored vector is unit length, got ‖v‖ = {n}"
        );

        // The raw pooled row this was scaled from measures ‖v‖ ≈ 184–220 (see `normalise` in `local_model.rs`),
        // so the L2 step is doing real work. Printed once here so a future reader can see the claim was checked
        // against a number rather than asserted about.
        println!(
            "EMBED: {DIM} components, ‖v‖ = {n:.9}, first = {}",
            parts[0]
        );
    }

    /// Same text, same process, **bitwise** identical output. Not `≈`: a scan embeds a chunk, compares it to the
    /// stored row for unchanged files, and a component that drifts by 1e-7 between runs would make every note
    /// look re-embedded.
    #[test]
    fn the_same_text_twice_gives_the_same_bits() {
        let path = artifact_or_not_coverage!();
        let _serial = artifact_lock();
        let model = load(&path);

        let first = components(
            &model
                .embed_text(
                    "",
                    "Determinism is a property you only notice when it is gone.",
                    2_048,
                )
                .expect("a short sentence embeds"),
        );
        let second = components(
            &model
                .embed_text(
                    "",
                    "Determinism is a property you only notice when it is gone.",
                    2_048,
                )
                .expect("the same sentence embeds again"),
        );

        assert_eq!(
            first,
            second,
            "two calls on one loaded model disagreed: {:?} vs {:?}",
            &first[..4],
            &second[..4]
        );
        println!(
            "EMBED: two calls agreed bitwise on {} components",
            first.len()
        );
    }

    /// Does it embed *meaning*? An ordering, not a score: the feline sentence must be closer to the cat sentence
    /// than an unrelated one is. All three cosines print either way — if this ever fails, the printed triple is
    /// the finding, and re-wording the sentences until it passes would destroy exactly what makes the test worth
    /// running.
    #[test]
    fn meaning_orders_closer_than_unrelated() {
        let path = artifact_or_not_coverage!();
        let _serial = artifact_lock();
        let model = load(&path);

        let embed = |text: &str| {
            components(
                &model.embed_text("", text, 2_048).unwrap_or_else(|problem| {
                    panic!("{} should embed: {problem}", &text[..24.min(text.len())])
                }),
            )
        };

        let cat = embed("A small cat slept on a warm windowsill.");
        let feline = embed("The kitten was curled up beside the radiator.");
        let revenue = embed("Quarterly revenue rose eleven percent on stronger licensing.");

        let near = cosine(&cat, &feline);
        let far = cosine(&cat, &revenue);
        println!("SEMANTIC: cos(cat, feline) = {near:.6}, cos(cat, revenue) = {far:.6}");
        assert!(
            near > far,
            "the model ranked an unrelated sentence closer than a paraphrase: {near:.6} vs {far:.6}"
        );
    }

    /// Empty and whitespace-only input, **as measured** rather than as ruled.
    ///
    /// `llama_tokenize` with `add_special = true` answers 2 tokens for `""` on this artifact (BOS = 2, EOS = 1 —
    /// both specials are enabled in the file's header) and 3 for `"   "`. So an empty chunk is not an error: it
    /// embeds to a real unit vector of a two-token sequence. Refusing instead would be this round inventing a
    /// rule the model does not have, and `chunk.rs` never produces an empty chunk anyway — the case matters only
    /// because a caller can hand one in, and it must get a vector or a typed refusal, never a panic.
    #[test]
    fn empty_input_embeds_bos_and_eos_rather_than_failing() {
        let path = artifact_or_not_coverage!();
        let _serial = artifact_lock();
        let model = load(&path);

        assert_eq!(
            model.count_tokens("", "").expect("counting an empty string cannot fail"),
            2,
            "measured: BOS + EOS and nothing else — the specials are what remains when the text is nothing"
        );
        assert_eq!(
            model.count_tokens("", "   ").expect("spaces count"),
            3,
            "measured: BOS + one whitespace piece + EOS"
        );

        let empty = components(&model.embed_text("", "", 2_048).expect("empty input embeds"));
        assert_eq!(empty.len(), DIM);
        let n = norm(&empty);
        assert!((n - 1.0).abs() < 1e-6, "‖empty embedding‖ = {n}");

        // And it is not the same vector as a real sentence's — which is what makes this case more than "a call
        // returned Ok": a runtime that ignored its input would pass every other assertion here.
        let cat = components(&model.embed_text("", "cat", 2_048).expect("one word embeds"));
        assert_ne!(
            empty, cat,
            "an empty note must not embed to the same row as a word"
        );
        println!(
            "EMBED: \"\" -> {} tokens, cos with \"cat\" = {:.6} (a real vector, of an empty sequence)",
            model.count_tokens("", "").expect("counted above"),
            cosine(&empty, &cat)
        );
    }

    /// The budget counts **tokens**, not bytes — the distinction §5 insists on, and the first production use of
    /// [`EmbedError::TokenBudgetExceeded`].
    #[test]
    fn the_budget_counts_tokens_and_says_so() {
        let path = artifact_or_not_coverage!();
        let _serial = artifact_lock();
        let model = load(&path);

        // Measured on this artifact: 23 bytes, **9 tokens** with specials. A byte comparison against a budget of
        // 12 would refuse it; the token count accepts it, which is the whole difference between the two rules.
        let sentence = "The cat sat on the mat.";
        // A fixture pin, not a claim about tokenizers: if the string is edited, the two numbers below stop meaning
        // what this case says they mean.
        assert_eq!(sentence.len(), 23);
        assert_eq!(
            model.count_tokens("", sentence).expect("counted"),
            9,
            "the artifact's own count for this string"
        );
        model
            .embed_text("", sentence, 12)
            .expect("23 bytes and 9 tokens fits a budget of 12 — bytes would have refused it");

        // **Boundary equality.** §5 promises refusal only *over* budget, so a chunk measuring exactly its budget must
        // embed. Every other call in this file sits strictly above or strictly below its own count, and Scope B review
        // P2-12 showed what that costs: changing `>` to `>=` at **both** budget sites left all twelve cases here and the
        // whole default suite green (measured, not predicted). This call is the pair that kills it — refused at 8 below,
        // accepted at 9 here.
        model
            .embed_text("", sentence, 9)
            .expect("a chunk exactly at its own count is not over budget");

        // And the refusal reports the number that actually broke the rule. `tokens: 9` rather than `23` is the
        // assertion that this is a token budget with a byte-length implementation or not.
        let problem = model
            .embed_text("", sentence, 8)
            .expect_err("9 tokens cannot fit a budget of 8");
        match problem {
            EmbedError::TokenBudgetExceeded { tokens, budget } => {
                assert_eq!(tokens, 9, "the count is in tokens, not bytes");
                assert_eq!(budget, 8);
                println!("REFUSE: {problem}");
            }
            other => panic!("expected a token-budget refusal, got {other:?}"),
        }
    }

    /// **The regression that matters.** With llama.cpp's default `n_ubatch = 512` and this non-causal artifact, a
    /// 600-token sequence hit `GGML_ASSERT(n_ubatch >= n_tokens)` and the process died with SIGABRT — no return
    /// code, nothing to catch. An ordinary ~1,800-byte chunk is in that range, so this case is the difference
    /// between "the runtime refused" and "np vanished".
    #[test]
    fn a_six_hundred_token_note_embeds_instead_of_aborting() {
        let path = artifact_or_not_coverage!();
        let _serial = artifact_lock();
        let model = load(&path);

        let note = text_of_about(35); // measured 633 tokens with specials on this artifact
        let tokens = model.count_tokens("", &note).expect("counted");
        assert!(
            tokens > 512 && tokens < 1_024,
            "this case is only a regression test if it clears the old 512 threshold: {tokens} tokens"
        );

        let v = components(
            &model
                .embed_text("", &note, 2_048)
                .expect("600 tokens must decode, not abort the process"),
        );
        let n = norm(&v);
        assert!((n - 1.0).abs() < 1e-6, "‖v‖ = {n}");
        println!("EMBED: {tokens} tokens in one decode, ‖v‖ = {n:.9}");
    }

    /// Above the **window** (not the budget) is a typed refusal, and it fires before `llama_decode` sees the
    /// sequence.
    ///
    /// Two honest limits. First: if the window arithmetic regressed so the check let the sequence through, the runtime
    /// would abort the whole test binary rather than fail this assertion — a crash is visible, but it is not this test
    /// passing. Second, and narrower (Scope B review P2-12): nothing asserts that **exactly** `window` tokens is
    /// *accepted*. The refusal side is pinned to one token above; the accepting side at the boundary would cost a
    /// ~90-second decode of a synthetic window-length note (8,192 tokens measured 2026-10-30: 7,999 tokens in 101 s)
    /// on every run of this file, so it is left to R6d's benchmark,
    /// which decodes at those lengths anyway. A `>=` mutant on the window arm therefore survives today — and since the
    /// ceiling now equals the window, it would refuse a chunk no chunker produces (`MAX_CHUNK_BYTES = 1800` is roughly
    /// 500 tokens), which is why this is recorded rather than paid for. What it does prove is that the refusal exists, fires at the right threshold, and
    /// names the window rather than the budget.
    #[test]
    fn over_the_window_is_refused_before_the_runtime_decides() {
        let path = artifact_or_not_coverage!();
        let _serial = artifact_lock();
        let model = load(&path);

        let window = model.window();
        assert!(window > 0, "a context with no window cannot embed anything");

        // Build something longer than the window and **prove** it, rather than trusting bytes-per-token. Proving it
        // is subtler than it looks now §5's decision B made the counting ceiling equal to the window: `count_tokens`
        // refuses above its own ceiling (equal to the window since §5's decision C), so an over-window note never
        // returns a count at all. The refusal is the
        // evidence — `TokenBudgetExceeded { tokens, .. }` carries llama.cpp's own count — which is why this case reads
        // the number out of the error instead of expecting `Ok`.
        //
        // The loop is for an artifact with a wider window than our ceiling (a re-pin); on today's build the first
        // count already refuses. What must not happen is the input growing past the budget handed to `embed_text`
        // below, because then this case would quietly exercise the budget arm instead of the window arm — that guard
        // sits **after** the loop, where a failure is possible (R6b2 review P2-2 caught it living inside).
        let budget = window + 4_096;

        // **The threshold itself, approached from above.** The coarse case below uses ~5,400 tokens, which a mutant that
        // refuses only at *twice* the window would also satisfy — so it proves an arm exists, not where the arm sits. One
        // token past the window is the cheapest thing that pins the boundary, and the boundary is the arm's whole purpose:
        // `n_ubatch >= n_tokens` aborts one token above it (`src/llama-context.cpp:1631`). No decode happens here, so this
        // costs tokenization alone.
        let (tight, tight_tokens) = note_at_least_tokens(&model, window + 1);
        assert!(
            tight_tokens > window,
            "the helper returned {tight_tokens} tokens for a target of {} tokens, so it never crossed the window of {}",
            window + 1,
            window
        );
        match model.embed_text("", &tight, budget) {
            Err(EmbedError::Unavailable(label)) => assert_eq!(
                label,
                "the sequence is longer than this context can hold",
                "one token over the window must be refused by the window arm"
            ),
            Ok(_) => panic!("{tight_tokens} tokens over a window of {window} decoded: the refusal sits above its own boundary"),
            Err(other) => panic!("expected the window refusal, got {other:?}"),
        }
        println!("REFUSE: {tight_tokens} tokens is just over a window of {window}");

        let mut note = text_of_about(300);
        let mut tokens = count_even_over_ceiling(&model, &note);
        while tokens <= window {
            note.push_str(UNIT);
            tokens = count_even_over_ceiling(&model, &note);
        }
        assert!(
            tokens > window,
            "the loop stopped without passing a window of {window}: it counted {tokens}, so this case proves nothing"
        );
        assert!(
            tokens <= budget,
            "the input reached {tokens} tokens passing a window of {window}, past the {budget} budget this case              passes to embed_text: it would then report a budget refusal rather than the window refusal it names"
        );

        let problem = model
            .embed_text("", &note, budget)
            .expect_err("a sequence over the window must be refused, not decoded");
        match &problem {
            EmbedError::Unavailable(label) => assert_eq!(
                label,
                "the sequence is longer than this context can hold",
                "worded for the reader who has to fix it: shorten the chunk; free memory is not the lever"
            ),
            other => panic!("expected a window refusal, got {other:?}"),
        }
        println!("REFUSE: {tokens} tokens over a window of {window} -> {problem}");
    }

    /// Two texts in sequence produce two different rows, both unit length.
    ///
    /// Catches the specific mistake of reading the pooled buffer after something else has rewritten it, or of
    /// pooling a stale sequence: either way the second call would return the first note's vector (or a mixture),
    /// and every other case here uses exactly one text per loaded model.
    #[test]
    fn two_texts_in_sequence_are_different_and_both_unit() {
        let path = artifact_or_not_coverage!();
        let _serial = artifact_lock();
        let model = load(&path);

        let first = components(
            &model
                .embed_text("", "Autumn leaves fell across the empty car park.", 2_048)
                .expect("embeds"),
        );
        let second = components(
            &model
                .embed_text(
                    "",
                    "The compiler reported three errors and one warning about lifetimes.",
                    2_048,
                )
                .expect("embeds"),
        );

        assert_ne!(
            first, second,
            "the second call returned the first note's row"
        );
        for (i, v) in [&first, &second].into_iter().enumerate() {
            let n = norm(v);
            assert!((n - 1.0).abs() < 1e-6, "vector {i} has ‖v‖ = {n}");
        }
        println!(
            "EMBED: two sequential decodes differ, cos = {:.6}",
            cosine(&first, &second)
        );
    }

    /// The prefix reaches the model. `prefix_doc` and `prefix_query` exist in the schema because a document and
    /// a query are not comparable otherwise — so the two paths must produce different rows for one text, and a
    /// provider that formatted the prefix into its own log line instead of the model's input would fail here.
    #[test]
    fn the_document_and_query_prefixes_produce_different_vectors() {
        let path = artifact_or_not_coverage!();
        let _serial = artifact_lock();
        let model = load(&path);

        let embedder = LocalEmbedder::new(model, &row("title: ", "query: "), 2_048)
            .expect("row width matches");
        let text = "Quarterly revenue rose eleven percent.";

        let doc = components(
            &embedder
                .embed_as(text, TextKind::Document)
                .expect("document path"),
        );
        let query = components(
            &embedder
                .embed_as(text, TextKind::Query)
                .expect("query path"),
        );

        // `doc != query` alone survives **swapping** the two prefixes (R6b2 review P2-3), and a swap is the
        // realistic bug: documents embedded under `prefix_doc`, queries under `prefix_query`, both quietly
        // reversed. So each side is compared against the same text embedded with its prefix spelled out here —
        // that decides *which* prefix, not merely that they differ.
        let model = embedder.model();
        assert_eq!(
            doc,
            components(
                &model
                    .embed_text("title: ", text, 2_048)
                    .expect("explicit document prefix")
            ),
            "TextKind::Document did not embed under prefix_doc"
        );
        assert_eq!(
            query,
            components(
                &model
                    .embed_text("query: ", text, 2_048)
                    .expect("explicit query prefix")
            ),
            "TextKind::Query did not embed under prefix_query"
        );

        assert_ne!(
            doc, query,
            "both prefixes produced one row: the prefix never reached the model"
        );

        // And the trait path is the *document* path, which is its own rule (a stored vector is always written
        // under `prefix_doc`) and its own mutation: pointing `Embedder::embed` at `TextKind::Query` survives
        // every other case in this file, because they all call `embed_as` with an explicit kind (R6b2 re-review
        // P2-6). This row is built with empty prefixes on purpose elsewhere; here it uses the real ones.
        let via_trait = embedder
            .embed(&[text])
            .expect("the trait path embeds")
            .remove(0);
        assert_eq!(
            components(&via_trait),
            doc,
            "Embedder::embed did not use the document prefix"
        );
        // The counting side prices the **same string** as the embedding side (R6b2 re-review P1-1): §5's budget is
        // `prefix_doc + chunk + specials`, so a pre-flight count that dropped the prefix would say "it fits" and
        // then watch `embed_text` refuse. Two directions, because either half can regress alone: the prefixed count
        // is strictly larger than the bare one, and it equals the number `embed_text` itself reports when refused
        // one token below.
        let with_prefix = model
            .count_tokens("title: ", text)
            .expect("counting a short note cannot fail");
        let bare = model.count_tokens("", text).expect("counted");
        assert!(
            with_prefix > bare,
            "count_tokens priced the text without its prefix: {with_prefix} prefixed vs {bare} bare"
        );
        match model.embed_text("title: ", text, with_prefix - 1) {
            Err(EmbedError::TokenBudgetExceeded { tokens, budget }) => {
                assert_eq!(
                    tokens, with_prefix,
                    "embed_text counted a different sequence than count_tokens did"
                );
                assert_eq!(budget, with_prefix - 1);
            }
            other => panic!("expected a budget refusal one token below, got {other:?}"),
        }

        for v in [&doc, &query] {
            let n = norm(v);
            assert!((n - 1.0).abs() < 1e-6, "‖v‖ = {n}");
        }
        println!(
            "EMBED: prefix changes the row, cos(doc, query) = {:.6}",
            cosine(&doc, &query)
        );
    }

    /// The [`Embedder`] shape a scan will call: identity, width, one vector per text in order — plus the row
    /// check that stops a provider built from one model's row around another model's handles.
    #[test]
    fn the_provider_answers_the_three_trait_questions() {
        let path = artifact_or_not_coverage!();
        let _serial = artifact_lock();
        let model = load(&path);

        let embedder = LocalEmbedder::new(model, &row("", ""), 2_048).expect("768 matches the row");
        assert_eq!(embedder.model_id(), "embeddinggemma-2");
        // A pin, not evidence: `LocalEmbedder::new` refuses any row whose width is not the model's, and the model
        // was loaded with `DIM`, so this cannot fail today. It documents that the trait's width promise is the
        // model's answer rather than the row's claim. The independent 768 pin is `tests/local_load.rs`.
        assert_eq!(embedder.dim(), DIM);

        let texts = ["first note", "second note", "third note"];
        let vectors = embedder.embed(&texts).expect("three short texts embed");
        assert_eq!(vectors.len(), texts.len(), "one vector per text");
        for v in &vectors {
            // Pin (see above): the width comes from the copy, so this is a regression fence, not a measurement.
            assert_eq!(v.dim(), DIM);
            let n = norm(&components(v));
            assert!((n - 1.0).abs() < 1e-6, "‖v‖ = {n}");
        }

        // Order matters: a scan writes `embeddings` rows keyed by chunk id and never re-reads the text, so a
        // provider that sorted or shuffled its batch would silently attach the wrong vector to each note.
        // Every position, not just index 0. A provider that sorted or shuffled the tail survives an element-0
        // check, and R6c keys `embeddings` rows by chunk id from exactly this order — a shuffle would attach the
        // wrong vector to notes nobody notices were reordered.
        for (i, text) in texts.iter().enumerate() {
            let alone = embedder.embed(&[*text]).expect("single");
            assert_eq!(
                components(&alone[0]),
                components(&vectors[i]),
                "batch position {i} is not the vector this text produces on its own"
            );
        }
    }

    /// A row whose width disagrees with the loaded handles is refused before anything can be embedded.
    ///
    /// Uses `512` deliberately: that is `n_embd_inp`, a real number this same artifact answers, and the one a
    /// reader who confuses the two widths would put in the row.
    #[test]
    fn a_row_that_is_not_the_loaded_model_is_refused() {
        let path = artifact_or_not_coverage!();
        let _serial = artifact_lock();
        let model = load(&path);

        let mut wrong = row("", "");
        wrong.dim = 512; // `n_embd_inp`, the number this artifact also answers — and never a stored width.
        let problem = LocalEmbedder::new(model, &wrong, 2_048)
            .expect_err("a 512-wide row cannot describe 768-wide vectors");
        match problem {
            EmbedError::ProviderDimMismatch {
                configured,
                provider,
            } => {
                // `configured` is the model's width by construction here; the load-bearing half of this arm is
                // `provider == 512`, which is the row's number reaching the message.
                assert_eq!(configured, DIM);
                assert_eq!(provider, 512, "the refusal names the row's number");
                println!("REFUSE: {problem}");
            }
            other => panic!("expected a width refusal, got {other:?}"),
        }
    }

    /// Throughput and first-embed latency, **printed**. §5's ceiling question — closed as option B on 2026-10-30:
    /// budget ceiling 8,192 (this build's window, decision C) and default 1,024 — was decided from numbers like these
    /// rather than
    /// from the artifact's trained window, and §14's OpenMP trade-off is another such decision. Nothing here asserts a
    /// bound: a shared host, a cold page cache and eleven other rounds' build load all move this number by more than
    /// any regression would.
    #[test]
    fn decode_throughput_is_printed_not_asserted() {
        let path = artifact_or_not_coverage!();
        let _serial = artifact_lock();

        let started = Instant::now();
        let model = load(&path);
        let load_seconds = started.elapsed().as_secs_f64();

        for copies in [30_usize, 120] {
            let text = text_of_about(copies);
            let tokens = model.count_tokens("", &text).expect("counted");
            let first = Instant::now();
            model
                .embed_text("", &text, 8_192)
                .expect("both sizes fit the window and the hard ceiling");
            let first_seconds = first.elapsed().as_secs_f64();

            let rounds = 5;
            let started = Instant::now();
            for _ in 0..rounds {
                model
                    .embed_text("", &text, 8_192)
                    .expect("repeated embed of the same text");
            }
            let each = started.elapsed().as_secs_f64() / rounds as f64;
            println!(
                "THROUGHPUT: {tokens} tokens — first {:.3} s, warm {:.3} s/decode, {:.0} tok/s",
                first_seconds,
                each,
                tokens as f64 / each
            );
        }

        println!("TIMING: model load {load_seconds:.3} s (see tests/local_load.rs for the same figure alone)");
    }
}

/// The feature-off shape of this file: no claims, and a printed statement of why. `build.rs` refuses to compile
/// `local-model` anywhere but Linux (§14), so this is what a non-Linux or default-features run sees.
#[cfg(not(feature = "local-model"))]
#[test]
fn local_model_feature_is_off() {
    println!(
        "NOT COVERAGE: built without `local-model`, so no forward pass exists to exercise. \
         Re-run with --features local-model."
    );
}
