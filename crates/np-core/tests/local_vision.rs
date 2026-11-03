//! R7c's claim about a **real** projector: PNG bytes go in and one unit-length 768-component vector comes out,
//! from the same space the text path already writes into (`docs/design-m2-image-embeddings.md` §10).
//!
//! Every case here needs both artifacts, because every claim is about what *this* projector answers — so each one
//! prints `NOT COVERAGE` and passes when `NP_TEST_MODEL_PATH` or `NP_TEST_MMPROJ_PATH` is unset (F18: an absent
//! 310 MB or 554 MB file must not look like a failure, and must not look like coverage either). No `#[ignore]`,
//! for the standing reason: an ignored test is the silent skip that rule exists to forbid. One case —
//! [`refusal_classes_are_the_report_labels_verbatim`] — needs the feature but no artifact, and it is the one line
//! in this file that runs on a CI row without model files.
//!
//! ```text
//! NP_TEST_MODEL_PATH=/path/to/embeddinggemma-2-Q8_0.gguf \
//! NP_TEST_MMPROJ_PATH=/path/to/mmproj-embeddinggemma-2-Q8_0.gguf \
//!   cargo test -p np-core --features local-model,vision --test local_vision -- --nocapture
//! ```
//!
//! Four rules the cases are built around:
//!
//! * **One load at a time.** A loaded model is ~420 MiB resident and the projector's vision graph sits on top of
//!   it, so the cases share a mutex (`tests/local_embed.rs` has the same rule for the same reason).
//! * **The fixtures are Task 1's files**, not new ones: `tests/probes/vision/*.png`, the four images
//!   `crates/np-core/tests/probes/vision-spike-2026-11-02.md` measured. Reusing them is what lets the caption case
//!   below be a fence against a recorded table rather than a number chosen until the test went green.
//! * **Magnitudes are printed, orderings are asserted.** ‖v‖ = 1 and "the caption's own image wins" are claims;
//!   a specific cosine is evidence, so it prints.
//! * **Token cost is read back, never assumed.** `last_token_cost()` is what §8's `over-budget` rule will be
//!   measured against, so the flat 280 from the model card gets checked against what this builder really does.

/// The projector-gated claims. All of them need `vision`, and all but one need both artifacts.
#[cfg(feature = "vision")]
mod vision {
    use std::fs;
    use std::path::{Path, PathBuf};
    use std::sync::{Mutex, MutexGuard};

    use np_core::db::ModelRow;
    use np_core::embed::{EmbedError, Embedder, Vector};
    use np_core::local_model::{self, LazyLocalEmbedder, LoadedModel, LocalEmbedder};
    use np_core::media::MediaRefusal;
    use np_core::vision::{Projector, VisionModel, OVER_BUDGET, UNDECODABLE, VISION_UNAVAILABLE};

    /// The width `embeddinggemma-2-Q8_0.gguf` answers with — the same 768 `tests/local_load.rs` pins from the
    /// file's header and §1.2's published output dimension.
    const DIM: usize = 768;

    /// One of Task 1's four generated images, read from the committed fixture.
    fn image(name: &str) -> Vec<u8> {
        let path = Path::new(env!("CARGO_MANIFEST_DIR"))
            .join("tests/probes/vision")
            .join(name);
        fs::read(&path).unwrap_or_else(|problem| {
            panic!(
                "{} is a committed probe fixture, so reading it cannot fail: {problem}",
                path.display()
            )
        })
    }

    /// Both artifacts, or the reason there is no coverage. A half-set pair is reported as the half that is
    /// missing rather than as a load failure — the projector is the file someone forgot to copy.
    fn artifacts() -> Option<(PathBuf, PathBuf)> {
        let model = std::env::var_os("NP_TEST_MODEL_PATH").map(PathBuf::from)?;
        let mmproj = std::env::var_os("NP_TEST_MMPROJ_PATH").map(PathBuf::from)?;
        Some((model, mmproj))
    }

    macro_rules! artifacts_or_not_coverage {
        () => {
            match artifacts() {
                Some(pair) => pair,
                None => {
                    println!(
                        "NOT COVERAGE: NP_TEST_MODEL_PATH and/or NP_TEST_MMPROJ_PATH is unset, so no image was \
                         encoded by this case. Re-run with both pointing at embeddinggemma-2-Q8_0.gguf \
                         (sha256 2188ac1d…) and mmproj-embeddinggemma-2-Q8_0.gguf (sha256 05313094…)."
                    );
                    return;
                }
            }
        };
    }

    /// One load in the process at a time — see this file's header for why that is a memory rule.
    fn artifact_lock() -> MutexGuard<'static, ()> {
        static LOCK: Mutex<()> = Mutex::new(());
        LOCK.lock().unwrap_or_else(|poisoned| poisoned.into_inner())
    }

    /// The text side, loaded with the width the artifact answers. A refusal here is a finding, not a skip.
    fn load(model: &Path) -> LoadedModel {
        local_model::load(model, DIM).unwrap_or_else(|problem| {
            panic!("loading {} should succeed: {problem}", model.display())
        })
    }

    /// The seam for one case. It borrows `model` — the projector is initialised *against* that loaded model, and
    /// `LoadedModel` keeps owning both handles — which is why the two live in the same scope rather than coming
    /// back from one helper as a tuple.
    fn open<'a>(model: &'a LoadedModel, mmproj: &Path) -> VisionModel<'a> {
        VisionModel::open(model, mmproj).unwrap_or_else(|problem| {
            panic!("opening {mmproj:?} beside that model should succeed: {problem}")
        })
    }

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
        let (na, nb) = (norm(a), norm(b));
        assert!(na > 0.0 && nb > 0.0, "a zero vector cannot be compared");
        dot / (na * nb)
    }

    fn components(v: &Vector) -> Vec<f32> {
        v.components().to_vec()
    }

    /// The row a provider in this file is built from. Every field but `dim` and the prefixes is a stamp no case reads:
    /// `LocalEmbedder::new` compares `dim` against what the artifact answers and copies the prefixes, and nothing here
    /// writes a vector row.
    fn provider_row() -> ModelRow {
        ModelRow {
            id: "embeddinggemma-2".to_string(),
            dim: i64::try_from(DIM).expect("768 fits i64"),
            sha256: None,
            prefix_doc: "task: search result | query: ".to_string(),
            prefix_query: "query: ".to_string(),
            installed_at: 1_730_000_000,
            verified_size: None,
            verified_mtime: None,
        }
    }

    /// **The two shapes of the seam are one pipeline.** §10's image call now has an owning half ([`Projector`], which a
    /// provider stores beside the model it points into) and the borrowing [`VisionModel`] façade. A row that differed
    /// between them would mean `np embed` storing vectors that no search through the other shape could reproduce, so this
    /// case asserts **bit-identical** components — not `≈`, for the same reason
    /// [`the_same_bytes_twice_give_bit_identical_components`] refuses a tolerance: an embed run compares a fresh row against
    /// a stored one to decide whether an image is current.
    ///
    /// Three drivers, one loaded model: the façade, a [`Projector`] opened by hand, and the same projector held by a
    /// [`LocalEmbedder`] — which is the shape the drop rule in `local_model::LocalEmbedder` exists for, so this is also the
    /// artifact-gated proof that a provider opening its own projector encodes like one opened beside it. The model is
    /// **moved** into the provider after the first two are dropped (420 MiB once rather than twice), and the same move is
    /// what proves the projector inside it dies before the model: this case would abort on a re-pin whose context reads its
    /// model during `~mtmd_context` if the fields were declared the other way round — nothing here demonstrates that today,
    /// and the field comment says so rather than this sentence pretending otherwise.
    ///
    /// The last block is capability, not arithmetic: a provider with no projector must answer `supports_images() == false`
    /// and refuse an image **without loading** (`loaded()` stays false), because §8's `vision-unavailable` is the cheap
    /// answer and laziness is the reason `LazyLocalEmbedder` exists.
    #[test]
    fn the_provider_seam_and_the_facade_agree_bit_for_bit() {
        let (model_path, mmproj_path) = artifacts_or_not_coverage!();
        let _serial = artifact_lock();
        let model = load(&model_path);
        let bytes = image("red_square_512x384.png");

        // Driver 1: the façade that borrows the model.
        let mut facade = open(&model, &mmproj_path);
        let through_facade = components(
            &facade
                .embed_image(&bytes)
                .expect("the fixture embeds through the façade"),
        );
        let facade_cost = facade.last_token_cost();
        assert!(
            facade_cost >= 1,
            "an encode that produced a row reported a cost of {facade_cost}"
        );

        // Driver 2: the owning projector, handed the model per call — no borrow anywhere in this type.
        let projector = Projector::open(&model, &mmproj_path)
            .unwrap_or_else(|problem| panic!("opening {mmproj_path:?} should succeed: {problem}"));
        let owned = projector
            .embed_image(&model, &bytes)
            .expect("the same bytes through the owning half");
        assert_eq!(
            components(&owned.vector),
            through_facade,
            "the façade and the owning projector disagreed about one byte string: {:?} vs {:?}",
            &components(&owned.vector)[..4],
            &through_facade[..4]
        );
        assert_eq!(
            owned.token_cost, facade_cost,
            "the cost carried in ImageEmbedding is not the cost the facade reported for one encode"
        );

        // Driver 3: a provider that owns the model and opens its own projector on first image need. Both earlier
        // projectors are gone by now, which is also why the move below type-checks: nothing borrows `model` any more.
        drop(projector);
        drop(facade);
        let mut provider = LocalEmbedder::new(
            model,
            &provider_row(),
            np_core::config::DEFAULT_EMBEDDING_TOKENS,
        )
        .expect("the row declares the width this artifact answers")
        .with_mmproj(mmproj_path.clone());
        assert!(
            provider.supports_images(),
            "a provider built with an mmproj must say so before anyone is asked to read a file"
        );
        let through_provider = components(
            &provider
                .embed_image(&bytes)
                .expect("a provider that opened its own projector encodes the fixture"),
        );
        assert_eq!(
            through_provider, through_facade,
            "the row a provider stored differs from the row the façade produces for the same bytes"
        );

        // Capability without a load: two providers over the same artifact, one with a projector file and one without.
        let mut bare = LazyLocalEmbedder::new(
            &provider_row(),
            &model_path,
            DIM,
            np_core::config::DEFAULT_EMBEDDING_TOKENS,
            np_core::config::ThreadCount::Auto,
        )
        .expect("test setup: the artifact is readable, so a lazy provider builds");
        assert!(
            !bare.supports_images(),
            "a provider with no mmproj must answer that it cannot serve an image — one that always answered true would \
             send every note's linked file into an encode that can only refuse"
        );
        match bare.embed_image(&bytes) {
            Err(EmbedError::Unavailable(label)) => assert_eq!(
                label, VISION_UNAVAILABLE,
                "the refusal is §8's class for a build and model pair, not a statement about these bytes"
            ),
            other => panic!(
                "a provider with no projector must refuse rather than produce a row, got {other:?}"
            ),
        }
        assert!(
            !bare.loaded(),
            "refusing an image this provider cannot serve cost a load: the answer was already in its fields"
        );

        let with_projector = LazyLocalEmbedder::new(
            &provider_row(),
            &model_path,
            DIM,
            np_core::config::DEFAULT_EMBEDDING_TOKENS,
            np_core::config::ThreadCount::Auto,
        )
        .expect("test setup: the same artifact again")
        .with_mmproj(mmproj_path);
        assert!(
            with_projector.supports_images(),
            "and true once a projector file is configured"
        );
        assert!(
            !with_projector.loaded(),
            "`supports_images` is a question about the configuration, not an invitation to load 420 MiB"
        );

        println!(
            "TWO SEAMS: {DIM} components bit-identical through the façade, a hand-opened Projector and a \
             LocalEmbedder's own projector; ‖v‖ = {:.9}, {facade_cost} image tokens each (the card's \
             flat default is 280). A provider with no projector refused without loading.",
            norm(&through_facade)
        );
    }

    /// The owning half of the split hands its model in per call, so **a caller can hand in the wrong one** — and that is
    /// not a wrong answer but an out-of-bounds one: the encoder's rows were copied at this projector's `n_embd_inp` while
    /// the decoder reads each row at the passed model's (`llama-batch.cpp:1470-1474`). The guard is a pointer comparison,
    /// so two artifacts that happen to share 512/768 are still refused rather than read as safe.
    ///
    /// This is the one case here holding **two** loaded models (~420 MiB each) at once, because nothing lighter can show
    /// it: with a single model there is no second pointer to hand in. The refusal has to arrive before any encoder work, so
    /// the assertion runs first and the matched pair is checked afterwards to prove the guard is not simply breaking the seam.
    #[test]
    fn a_projector_refuses_a_model_it_was_not_opened_against() {
        let (model_path, mmproj_path) = artifacts_or_not_coverage!();
        let _serial = artifact_lock();
        let opened = load(&model_path);
        let other = load(&model_path);
        let projector = Projector::open(&opened, &mmproj_path)
            .expect("the pinned projector opens beside the pinned model");
        let bytes = image("red_square_512x384.png");

        match projector.embed_image(&other, &bytes) {
            Err(EmbedError::Unavailable(label)) => assert_eq!(
                label, VISION_UNAVAILABLE,
                "a mismatched pair is a fact about this build's pairing, not about the file"
            ),
            Ok(_) => panic!(
                "a projector fed a model it was not opened against produced a row — the rows would have been \
                 copied at one width and read at another"
            ),
            Err(other) => panic!("expected vision-unavailable, got {other:?}"),
        }

        let matched = projector
            .embed_image(&opened, &bytes)
            .expect("the model this projector was opened against still encodes after the refusal");
        assert_eq!(matched.vector.dim(), DIM);
        println!(
            "MISMATCHED MODEL: refused as {VISION_UNAVAILABLE}; the opened-against model encoded afterwards at {} \
             tokens",
            matched.token_cost
        );
    }

    /// §8's class names and `np embed`'s report labels are one list, so the seam cannot spell a class in a way
    /// the report then has to guess at. Needs no artifact: it is a statement about two modules' constants.
    #[test]
    fn refusal_classes_are_the_report_labels_verbatim() {
        assert_eq!(UNDECODABLE, MediaRefusal::Undecodable.label());
        assert_eq!(OVER_BUDGET, MediaRefusal::OverBudget.label());
        assert_eq!(VISION_UNAVAILABLE, MediaRefusal::VisionUnavailable.label());
        println!(
            "CLASSES: the seam's three constants are media's labels verbatim — {UNDECODABLE}, {OVER_BUDGET}, \
             {VISION_UNAVAILABLE}. No artifact was needed, and none was used: §8 lists six classes in all, and this \
             file covers the three the vision seam can answer."
        );
    }

    /// The shape of a real image vector: 768 finite components at unit length (§10's contract, §1.2's recipe).
    ///
    /// The norm is the load-bearing assertion in this file. llama.cpp hands back the pooled row **un-normalised**
    /// — the spike note measured ‖v‖ = 180.17 (`stripes_1024x768.png`) and 206.77 (`red_square_1024x768.png`), so on the
    /// **1024×768** pair rather than the 512×384 file this case embeds — and a seam that skipped the L2 step would still
    /// return 768 finite components. Mutating the normalisation away on 2026-11-03 turned this case and
    /// [`text_still_matches_what_it_was_after_an_image_ran`] red (it asserts ‖v‖ ≈ 1 too) and left the rest green, so the
    /// norm contract is carried by two cases rather than one.
    #[test]
    fn an_image_embeds_to_768_components_of_unit_length() {
        let (model_path, mmproj_path) = artifacts_or_not_coverage!();
        let _serial = artifact_lock();
        let model = load(&model_path);
        let mut seam = open(&model, &mmproj_path);

        let vector = seam
            .embed_image(&image("red_square_512x384.png"))
            .expect("a generated PNG is inside every bound in play");
        let parts = components(&vector);

        assert_eq!(vector.dim(), DIM, "the same width the text path stores");
        assert_eq!(parts.len(), DIM);
        assert!(
            parts.iter().all(|x| x.is_finite()),
            "no component is NaN or infinite"
        );
        let n = norm(&parts);
        assert!(
            (n - 1.0).abs() < 1e-3,
            "the stored vector is unit length, got ‖v‖ = {n} — the pooled row arrives un-normalised (spike note: \
             180.17), so this is the seam's own arithmetic"
        );
        println!(
            "EMBED IMAGE: {DIM} components, ‖v‖ = {n:.9}, {} image tokens, first three = {:.6} {:.6} {:.6}",
            seam.last_token_cost(),
            parts[0],
            parts[1],
            parts[2]
        );
        // And it is a vector *of these bytes*: the same model on different pixels must not answer with this row.
        let other = components(
            &seam
                .embed_image(&image("stripes_512x384.png"))
                .expect("the second fixture embeds"),
        );
        assert_ne!(parts, other, "two different images produced one row");
        println!(
            "EMBED IMAGE: the two 512x384 fixtures differ, cos = {:.6}",
            cosine(&parts, &other)
        );
    }

    /// Same bytes, same process, **bitwise** identical components. Not `≈`: an embed run compares a fresh vector
    /// against the stored one to decide whether an image is current, and a component that drifts by 1e-7 between
    /// two encodes would make every diagram look re-embedded forever (§10's determinism question, measured).
    #[test]
    fn the_same_bytes_twice_give_bit_identical_components() {
        let (model_path, mmproj_path) = artifacts_or_not_coverage!();
        let _serial = artifact_lock();
        let model = load(&model_path);
        let mut seam = open(&model, &mmproj_path);

        let bytes = image("stripes_512x384.png");
        let first = components(
            &seam
                .embed_image(&bytes)
                .expect("the first encode of the fixture succeeds"),
        );
        // A different image in between, so a seam that cached one row and handed it back for every call — or
        // read a pooled buffer something had since rewritten — cannot pass by accident.
        let between = components(
            &seam
                .embed_image(&image("red_square_512x384.png"))
                .expect("the other fixture embeds"),
        );
        let second = components(
            &seam
                .embed_image(&bytes)
                .expect("the same bytes encode again"),
        );

        assert_eq!(
            first,
            second,
            "two encodes of one byte string disagreed: {:?} vs {:?}",
            &first[..4],
            &second[..4]
        );
        assert_ne!(first, between, "the two fixtures produced one row");
        println!(
            "EMBED IMAGE: {} components bit-identical across two encodes ({} tokens each)",
            first.len(),
            seam.last_token_cost()
        );
    }

    /// §8's `over-budget` rule needs a cost measured per encode, and the seam checks it **before** decoding. The reason
    /// it has to come first is recorded in `crates/np-core/tests/probes/decode_probe.c:71` — "a batch wider than the built
    /// ubatch aborts the encoder path instead of returning a code", against `src/llama-context.cpp:1631` — and an abort is
    /// not a refusal. That claim is *not* in `vision-spike-2026-11-02.md`; an earlier draft of this comment quoted it from
    /// there, which review caught on 2026-11-03.
    ///
    /// Whether this pin can even reach that line is a fact about the artifact — mtmd caps one image at a soft-token
    /// ceiling read from the projector file — so the case needs a third variable, `NP_TEST_VISION_LARGE`, pointing at an
    /// image large enough to probe it. It prints the cost and accepts either honest outcome (a vector inside the window,
    /// or `over-budget`); anything else is a finding. Without the variable it says what it did not measure.
    ///
    /// **Measured on 2026-11-03, and it answers the question:** a 4096×3072 PNG cost **1,064** tokens — mtmd shrank the
    /// grid to the artifact's own ceiling instead of asking for the ~49,000 patches that image's pixels would give. The
    /// window this build makes is 8,192, so one image cannot reach `over-budget` on this pin and this case ran the *embed*
    /// branch. The guard stays: a narrower artifact, a re-pin with a larger ceiling, or several images in one prompt would
    /// all put llama.cpp's assert (not a refusal) in reach, which is exactly what §8's rule exists to avoid.
    #[test]
    fn a_large_image_either_embeds_within_the_window_or_refuses_over_budget() {
        let (model_path, mmproj_path) = artifacts_or_not_coverage!();
        let Some(large) = std::env::var_os("NP_TEST_VISION_LARGE").map(PathBuf::from) else {
            println!(
                "NOT COVERAGE: NP_TEST_VISION_LARGE is unset, so no oversized image was encoded and the over-budget \
                 boundary is unmeasured. Set it to a PNG large enough to want more tokens than the window holds."
            );
            return;
        };
        let _serial = artifact_lock();
        let model = load(&model_path);
        let mut seam = open(&model, &mmproj_path);
        let bytes = fs::read(&large).unwrap_or_else(|problem| {
            panic!(
                "NP_TEST_VISION_LARGE names {} which cannot be read: {problem}",
                large.display()
            )
        });

        match seam.embed_image(&bytes) {
            Ok(vector) => {
                let cost = seam.last_token_cost();
                println!(
                    "LARGE IMAGE: {} bytes -> embedded, {cost} image tokens, {} components, unit length",
                    bytes.len(),
                    vector.dim()
                );
                assert!(cost >= 1, "an encode that produced a vector reported a cost of zero");
            }
            Err(EmbedError::Unavailable(label)) if label == OVER_BUDGET => {
                println!(
                    "LARGE IMAGE: {} bytes -> refused over-budget at {} image tokens",
                    bytes.len(),
                    seam.last_token_cost()
                );
            }
            Err(other) => panic!(
                "an oversized image must embed or refuse over-budget, and this one answered {other:?}"
            ),
        }
    }

    #[test]
    /// Task 1's comparability table, now a fence (§9's cheap version of "is this the unified space the card
    /// describes"): a caption must sit closer to **its own** image than to the other one.
    ///
    /// The caption is embedded through the existing text path with an empty prefix — §1.2 says instruction
    /// prefixes apply to text only, and the probe measured these cosines that way. Recorded values from
    /// `vision-spike-2026-11-02.md`: own image +0.7465, other image +0.5578 for this caption. An ordering that
    /// flips is a finding about the artifact or the pin, so both numbers print; re-wording the caption until the
    /// test goes green would destroy what makes it worth running.
    fn a_caption_beats_an_unrelated_image() {
        let (model_path, mmproj_path) = artifacts_or_not_coverage!();
        let _serial = artifact_lock();
        let model = load(&model_path);
        let mut seam = open(&model, &mmproj_path);

        let own = components(
            &seam
                .embed_image(&image("red_square_1024x768.png"))
                .expect("the fixture the table measured"),
        );
        let other = components(
            &seam
                .embed_image(&image("stripes_1024x768.png"))
                .expect("the second 1024x768 fixture"),
        );
        let caption = model
            .embed_text("", "a red square on a white background", 2_048)
            .expect("a caption is eight tokens of text");
        let words = components(&caption);

        let near = cosine(&words, &own);
        let far = cosine(&words, &other);
        println!("COMPARABILITY: cos(caption, own image) = {near:.6} (note: +0.7465), cos(caption, other image) = {far:.6} (note: +0.5578)");
        assert!(
            near > far,
            "the caption sat closer to an unrelated image than to its own: {near:.6} vs {far:.6}"
        );
    }

    /// `last_token_cost()` is what §8's `over-budget` rule measures, so it has to be a real count of the last
    /// encode and not a constant: zero before anything ran, at least one after, and larger for the image that
    /// needs more patches. The card publishes 280 as a flat default; this builder sizes the grid from the image
    /// (measured 336 at 1024×768 and 88 at 512×384), which is exactly why §8 refuses to compute cost from bytes.
    #[test]
    fn token_cost_is_measured_per_encode_and_printed() {
        let (model_path, mmproj_path) = artifacts_or_not_coverage!();
        let _serial = artifact_lock();
        let model = load(&model_path);
        let mut seam = open(&model, &mmproj_path);

        assert_eq!(
            seam.last_token_cost(),
            0,
            "nothing has been encoded yet, so there is no cost to report"
        );

        seam.embed_image(&image("stripes_512x384.png"))
            .expect("the small fixture embeds");
        let small = seam.last_token_cost();
        assert!(
            small >= 1,
            "an encoded image costs at least one token, got {small}"
        );

        seam.embed_image(&image("stripes_1024x768.png"))
            .expect("the large fixture embeds");
        let large = seam.last_token_cost();

        println!("TOKEN COST: 512x384 -> {small}, 1024x768 -> {large} (card's flat default: 280)");
        assert!(
            large > small,
            "the cost did not grow with the image: {small} for 512x384 against {large} for 1024x768. Either the cost is \
             not a count of this encode, or mtmd clamped both fixtures to one soft-token ceiling (it did that at \
             1,064 tokens for a 4096x3072 image) — check which before blaming the seam"
        );

        // A refusal still leaves the last measured cost on the record — §8's rule reads the number *after* the
        // attempt, and a refusal that zeroed it would hide the evidence.
        seam.embed_image(&[]).expect_err("no bytes cannot decode");
        assert_eq!(
            seam.last_token_cost(),
            large,
            "a failed encode rewrote the cost of the last one that ran"
        );
    }

    /// A truncated PNG is refused, not embedded (§8's `undecodable`).
    ///
    /// The bytes keep a valid signature and IHDR, so this is the case that matters: a decoder that read the header
    /// and stopped at the missing scanlines would answer with *some* image, and a vector made of whatever it
    /// filled in would store as though the diagram had been read.
    #[test]
    fn a_truncated_png_refuses_rather_than_returning_a_vector() {
        let (model_path, mmproj_path) = artifacts_or_not_coverage!();
        let _serial = artifact_lock();
        let model = load(&model_path);
        let mut seam = open(&model, &mmproj_path);

        let whole = image("red_square_512x384.png");
        assert!(
            whole.starts_with(b"\x89PNG\r\n\x1a\n"),
            "the fixture is a PNG, so this truncation keeps the signature"
        );
        let mut truncated = whole.clone();
        truncated.truncate(whole.len() / 5);
        // The claim is "a decoder that read the header and stopped would answer with *some* image", so the header has
        // to survive the cut for this case to be that test: signature (8 bytes) plus IHDR (length, `IHDR`, dimensions)
        // live inside the first 33.
        assert!(
            truncated.len() > 33 && truncated[12..16] == *b"IHDR",
            "a fifth of the fixture is {} bytes, which no longer holds an IHDR chunk, so this would refuse for the \
             wrong reason",
            truncated.len()
        );

        let problem = seam
            .embed_image(&truncated)
            .expect_err("a fifth of a PNG must be refused rather than embedded");
        match &problem {
            EmbedError::Unavailable(label) => assert_eq!(
                label, UNDECODABLE,
                "the refusal names §8's class and nothing else — no path, no C text"
            ),
            other => panic!("expected an undecodable refusal, got {other:?}"),
        }
        println!(
            "REFUSE: {} of {} bytes -> {problem}",
            truncated.len(),
            whole.len()
        );

        // Empty input is the same class for the same reason.
        match seam
            .embed_image(&[])
            .expect_err("no bytes cannot produce pixels")
        {
            EmbedError::Unavailable(label) => assert_eq!(label, UNDECODABLE),
            other => panic!("expected an undecodable refusal, got {other:?}"),
        }

        // The seam survives its own refusals: the next good image still embeds.
        let after = seam
            .embed_image(&whole)
            .expect("a refused encode does not poison the projector");
        assert_eq!(after.dim(), DIM);
    }

    /// Audio is a §3 non-goal, and this projector *does* accept audio bytes. That is a provenance note rather than
    /// something this case can see: on 2026-11-03 the seam was instrumented to print `mtmd_bitmap_is_audio`'s answer for
    /// these exact bytes and it printed **true**, so the refusal below comes from the named guard. What the case asserts
    /// is only the class — any path that answered `undecodable` would pass it identically.
    ///
    /// **This case does pin which guard fires**, and that is a change made after review: the fallback refusal (an `AUDIO`
    /// chunk reaching `decode_chunks`, whose type this seam does not implement) answers `vision-unavailable`, not
    /// `undecodable`. Deleting the named guard therefore turns this assertion red instead of letting it pass quietly —
    /// verified by mutation on 2026-11-03, after an earlier version of both guards answered `undecodable` and the mutant
    /// survived. The two classes differ on purpose: "these bytes are not an image" is a fact about the vault, "this seam
    /// cannot serve it" is a bug in our crate.
    #[test]
    fn audio_bytes_refuse_because_this_seam_only_makes_image_vectors() {
        let (model_path, mmproj_path) = artifacts_or_not_coverage!();
        let _serial = artifact_lock();
        let model = load(&model_path);
        let mut seam = open(&model, &mmproj_path);

        // 8 kHz mono PCM, 50 ms of silence: small, valid, and unmistakably not a raster.
        let samples: u32 = 400;
        let mut wav: Vec<u8> = Vec::new();
        wav.extend_from_slice(b"RIFF");
        wav.extend_from_slice(&(36 + samples * 2).to_le_bytes());
        wav.extend_from_slice(b"WAVEfmt ");
        wav.extend_from_slice(&16u32.to_le_bytes());
        wav.extend_from_slice(&1u16.to_le_bytes()); // PCM
        wav.extend_from_slice(&1u16.to_le_bytes()); // mono
        wav.extend_from_slice(&8000u32.to_le_bytes());
        wav.extend_from_slice(&16000u32.to_le_bytes());
        wav.extend_from_slice(&2u16.to_le_bytes());
        wav.extend_from_slice(&16u16.to_le_bytes());
        wav.extend_from_slice(b"data");
        wav.extend_from_slice(&(samples * 2).to_le_bytes());
        wav.extend_from_slice(&vec![0u8; (samples * 2) as usize]);

        let problem = seam
            .embed_image(&wav)
            .expect_err("audio is not an image this seam was built to embed");
        match &problem {
            EmbedError::Unavailable(label) => assert_eq!(label, UNDECODABLE, "§8's list has no audio class, and \
                 \"these bytes are not an image\" is the same answer as \"they did not decode\": the caller's \
                 action is identical"),
            other => panic!("expected an undecodable refusal, got {other:?}"),
        }
        println!(
            "REFUSE: {} ms of PCM ({} bytes of file) -> {problem}",
            samples * 1000 / 8000,
            wav.len()
        );
    }

    /// **The regression the spike note called load-bearing, and what this host can actually prove about it.**
    /// `mtmd_decode_use_non_causal` answers true for this architecture, so the seam switches causal attention off for
    /// the image decode and puts the flag back afterwards.
    ///
    /// **This artifact cannot demonstrate the restore.** Measured on 2026-11-03: the text context loads with
    /// `llama_get_causal_attn` answering **false** (the spike note's own run printed the same, `ctx.causal_attn = false`),
    /// so setting false over false and restoring false is a no-op here — deleting the restore left this test passing,
    /// which is why it is recorded instead of argued. The restore stays because the seam must not be the thing that
    /// leaves a shared flag flipped on a pin whose default *is* causal, and because reading-then-restoring costs nothing.
    /// What this case does pin is the user-visible promise: an image encode leaves no state behind that changes text.
    ///
    /// Bitwise equality is the point of that assertion: "still close" would pass with a context that had drifted. The image side is
    /// checked both ways round as well, so a one-way fix (restore the flag but leave the KV-free encoder in some
    /// other state) does not pass either.
    #[test]
    fn text_still_matches_what_it_was_after_an_image_ran() {
        let (model_path, mmproj_path) = artifacts_or_not_coverage!();
        let _serial = artifact_lock();
        let model = load(&model_path);
        let mut seam = open(&model, &mmproj_path);

        const SENTENCE: &str = "Quarterly revenue rose eleven percent on stronger licensing.";
        let before = components(
            &model
                .embed_text("", SENTENCE, 2_048)
                .expect("text embeds before any image"),
        );

        let bytes = image("red_square_512x384.png");
        let first_image = components(&seam.embed_image(&bytes).expect("the image decodes"));
        assert!(
            (norm(&first_image) - 1.0).abs() < 1e-3,
            "the image vector is unit length before the text path runs again"
        );

        let after = components(
            &model
                .embed_text("", SENTENCE, 2_048)
                .expect("text embeds after an image"),
        );
        assert_eq!(
            before, after,
            "an image encode changed what the same text produces: cos = {:.6}, first components {:?} vs {:?}",
            cosine(&before, &after),
            &before[..4],
            &after[..4]
        );

        // A second sentence still embeds to its own row (so the assertion above cannot be satisfied by a text
        // path that had stopped reading its input), and the image is unchanged by having text run around it.
        let other_text = components(
            &model
                .embed_text("", "The kitten was curled up beside the radiator.", 2_048)
                .expect("a second sentence embeds"),
        );
        assert_ne!(
            before, other_text,
            "the text path stopped distinguishing its input"
        );
        let second_image = components(
            &seam
                .embed_image(&bytes)
                .expect("the same image encodes again"),
        );
        assert_eq!(
            first_image, second_image,
            "text running between two image encodes changed the image row"
        );

        println!(
            "CAUSAL RESTORE: text is bit-identical across an image encode; cos(text, image) = {:.6}",
            cosine(&after, &first_image)
        );
    }
}

/// The feature-off shape of this file: no claims, and a printed statement of why. `vision` implies `local-model`,
/// and `build.rs` refuses to build either anywhere but Linux (§14), so this is what every other run sees.
#[cfg(not(feature = "vision"))]
#[test]
fn vision_feature_is_off() {
    println!(
        "NOT COVERAGE: built without `vision`, so no projector was opened and no image was encoded. \
         Re-run with --features local-model,vision."
    );
}
