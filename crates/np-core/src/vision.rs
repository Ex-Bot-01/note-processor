//! Image vectors: PNG/JPEG bytes in, one unit-length row out (§10 of
//! `docs/design-m2-image-embeddings.md`).
//!
//! Built only under `vision`, which implies `local-model`: the projector is loaded **against an already loaded text
//! model**, so there is no seam here that can exist without the runtime. The call sequence is the one
//! `crates/np-core/tests/probes/vision-spike-2026-11-02.md` measured on this pin — bitmap, tokenize, encode, decode, pool —
//! with two steps that probe taught us are easy to lose:
//!
//! * **The causal flag is restored.** `mtmd_decode_use_non_causal` answers true for this architecture, so the image decode
//!   runs with causal attention off, and a seam that left it flipped would be changing a flag the text path shares. The
//!   seam reads the flag before changing it and puts back what it read, rather than assuming it was ever true.
//!
//!   **On this pin that restore changes nothing observable**, and that is a measurement rather than an argument: the text
//!   context loads with `llama_get_causal_attn` answering false (checked here on 2026-11-03, and printed as
//!   `ctx.causal_attn = false` in the spike note's run), so setting false over false is a no-op and deleting the restore
//!   left `tests/local_vision.rs::text_still_matches_what_it_was_after_an_image_ran` passing. It stays for the re-pin whose
//!   default is causal, and because the seam should not be the thing that leaves shared state modified.
//! * **The pooled row arrives un-normalised** (measured ‖v‖ = 180.17 and 206.77 on two probe images), so the seam divides by
//!   the L2 norm itself. Skipping that still yields 768 finite components; only a norm assertion notices.
//!
//! **Two types, one pipeline.** [`Projector`] owns an `mtmd_context` and takes the model as a parameter on every encode;
//! [`VisionModel`] is the façade over it that keeps the pair together by **borrowing** the model. The split exists because
//! a provider cannot store a borrow of its own model: `mtmd_init_from_file` keeps a raw pointer to the text model
//! (`mtmd.cpp:567`), so an image capability living inside a provider — which is where `Embedder::supports_images` says it
//! lives — must hold an owning projector next to the model it was opened against, and storing a borrow of one's own field
//! is self-reference. [`VisionModel`]'s lifetime is the compile-time answer for a caller that can borrow; field declaration
//! order is the answer for a provider that owns ([`crate::local_model::LocalEmbedder`] declares its projector first, and
//! says so). Same pipeline, same refusals, same arithmetic either way — asserted byte-for-byte by
//! `tests/local_vision.rs::the_provider_seam_and_the_facade_agree_bit_for_bit`.
//!
//! **Which class a refusal carries.** §8 lists three and this seam uses exactly those three, split by *whose* fault it is.
//! `undecodable` is about these bytes: empty, not a raster, audio, or the vision encoder refusing them.
//! `vision-unavailable` is about this build and model pair: no projector, no vision graph, tokenization refused, a chunk
//! shape or position scheme this seam does not implement, a decoder that would not take the rows, a pooled row that is not
//! usable. `over-budget` is the measured cost. A refusal carries no prose (§12 rule 8), so the class itself has to say
//! whether the file or the runtime is the problem — that is the whole difference between "re-encode it later" and
//! "this machine cannot serve images".
//!
//! **No instruction prefix exists here.** §1.2: prefixes apply to text, and media must be passed bare — so there is no
//! parameter that could take one, which is stronger than a rule telling callers not to use it.

use std::cell::Cell;
use std::ffi::CString;
use std::os::unix::ffi::OsStrExt;
use std::path::Path;
use std::ptr::NonNull;

use crate::embed::{EmbedError, Vector};
use crate::llama_ffi::{self, mtmd};
use crate::local_model::LoadedModel;

/// This **build and model pair** cannot serve the request (§8): the projector would not load beside this model, carries no
/// vision graph, tokenized wrong, asked for a position scheme this seam does not implement, refused the rows, or pooled to
/// nothing usable. Not a statement about the caller's bytes — that distinction is why this class exists next to
/// [`UNDECODABLE`].
///
/// Re-exported from [`crate::media`], which holds the single definition shared by this seam,
/// [`crate::embed::Embedder::embed_image`]'s default refusal and `MediaRefusal::label`. A re-pin that needs a different
/// wording changes `media.rs` — nowhere else, because there is no other spelling to find.
pub use crate::embed::VISION_UNAVAILABLE;
/// The encode measured larger than the context can hold (§8). Decided from the patch count this encode produced, never
/// from file size — the spike measured 336 tokens for a 1024×768 image and 88 for 512×384.
///
/// **Not reachable with one image on this pin**, which is a measurement: a 4096×3072 PNG cost 1,064 tokens on 2026-11-03,
/// because mtmd clamps the patch grid to the artifact's own soft-token ceiling and this build's window is 8,192. The check
/// is not decoration — it runs before the decode, and past the window llama.cpp asserts instead of returning a code, so a
/// missing guard turns a refusal into an abort (`tests/local_vision.rs::a_large_image_either_embeds_within_the_window_or_refuses_over_budget`
/// prints whichever branch a given image takes). Re-exported from [`crate::media`], which owns §8's spellings.
pub use crate::media::OVER_BUDGET;
/// These bytes are not a raster this build can decode (§8). Audio arrives here too: the pinned projector *does* accept
/// audio bytes, and §3 makes it a non-goal, so "not an image" and "did not decode" get the same answer — the caller's
/// action is identical either way, and §8's list has no audio class to invent one for.
///
/// Re-exported from [`crate::media`], which owns §8's spellings; see this module's header for how the three classes are
/// divided between "about these bytes" and "about this build".
pub use crate::media::UNDECODABLE;

/// One image's row, and what the encode that produced it cost.
///
/// The two travel together because §8's `over-budget` rule is defined over **this** encode's patch count: a caller that
/// got a vector but not its cost cannot tell whether the next larger image will fit, and a caller that got a cost without
/// a vector would have nothing to store.
pub struct ImageEmbedding {
    /// One L2-normalised row at the opened-against model's output width (the `n_out` read at open, 768 on this artifact).
    pub vector: Vector,
    /// Patch/token count of the encode that produced [`Self::vector`], read from the chunk list rather than computed from
    /// the file's bytes (the spike measured 336 at 1024×768 against 88 at 512×384).
    pub token_cost: usize,
}

/// A projector that **owns** an `mtmd_context` and borrows nothing.
///
/// This is the half a provider can keep as a field. [`VisionModel`] promises the same lifetime rule with `&'model`, but a
/// provider owns its [`LoadedModel`] and cannot store a borrow of its own field, so the owning type takes the model on
/// every call instead: [`Projector::embed_image`] asks for the model whose context the rows go through, and the type that
/// holds both is responsible for the one thing the compiler can no longer check — that the projector dies first. See
/// [`crate::local_model::LocalEmbedder`], which gets it right by declaring fields in that order.
///
/// **Neither `Send` nor `Sync`, for two independent reasons**: the [`NonNull`] context handle llama.cpp does not document
/// as thread-safe, and the [`Cell`] holding the last attempt's cost. Nothing here wraps either in a lock or claims
/// otherwise; one projector is driven by one provider on one thread.
pub struct Projector {
    /// From [`mtmd::mtmd_init_from_file`]; freed in [`Projector::drop`]. The context keeps a raw pointer to the text model
    /// it was opened against (`mtmd.cpp:567`, initialised at `:615`) without retaining it, which is why the owning type
    /// has to be destroyed before that model — see this type's header for who does that.
    mctx: NonNull<mtmd::mtmd_context>,
    /// The media marker mtmd splits a prompt on — copied out of C memory at open, because the string it returns is
    /// owned by the context and this seam hands that context to `Drop` while still holding the marker.
    marker: CString,
    /// The identity of the [`LoadedModel`] this context was built beside, as a `Weak` token — **never dereferenced, never
    /// compared by address**. It exists because [`Projector::embed_image`] takes a model as a parameter, and handing one
    /// model's encoder rows to another model's decoder is not a wrong answer but an out-of-bounds one: the rows were
    /// copied at this projector's `n_inp` while the batch reads each row at the passed model's width
    /// (`llama-batch.cpp:1470-1474`).
    ///
    /// A token rather than a pointer because a pointer answers the wrong question. An address can be handed back by the
    /// allocator after its model is freed, and then a stale projector would pass a pointer check while mtmd's stored
    /// `text_model` (`mtmd.cpp:567`) dangles; upgrading this `Weak` says plainly whether *that* instance is still alive.
    /// Two loads of one file get two tokens, so a mismatch refuses even when both artifacts share 512 and 768. The owning
    /// type keeps the pair matched by construction ([`crate::local_model::LocalEmbedder`]); this turns a caller that does
    /// not into a refusal instead of memory unsafety.
    opened_against: std::sync::Weak<()>,
    /// `llama_model_n_embd_inp`: the width of one encoder row (§1.2's 512), which is what the decoder is fed.
    n_inp: usize,
    /// `llama_model_n_embd_out`: the width stored and searched (768). Pooling happens at this width, not at input.
    n_out: usize,
    /// Patch count of the **last attempt**, including attempts that refused after measuring — §8's rule reads the number
    /// *after* an encode, so a refusal that lost it would leave the report nothing to quote. 0 means no attempt has got as
    /// far as a measured patch grid yet, which is a different statement from "this image cost nothing".
    ///
    /// Two cases pin this: `tests/local_vision.rs::token_cost_is_measured_per_encode_and_printed` (a later failed encode
    /// must not rewrite the cost of the last one that ran) and
    /// `tests/local_vision.rs::a_large_image_either_embeds_within_the_window_or_refuses_over_budget` (an `over-budget`
    /// refusal still has to print the cost it measured). Deleting the write in [`Projector::decode_chunks`] turns the first
    /// one red.
    last_cost: Cell<usize>,
    /// Tokens one forward pass holds, read from the model this projector was opened against rather than the requested
    /// number. A fact copied at open, like `n_inp` and `n_out` — see [`Projector::embed_image`] for what that means for a
    /// caller holding two models.
    window: usize,
}

impl Projector {
    /// Load `mmproj` beside an already loaded model and check it can do vision at all.
    ///
    /// Refuses [`VISION_UNAVAILABLE`] in both shapes of "cannot": this binary built without the feature (a different
    /// module says that), and a projector that loads but carries no vision graph. A projector that will not load at all
    /// is the same refusal — from the caller's side it is one fact, "this build cannot read images", and §12 rule 8 keeps
    /// mtmd's own error text (which names the file) out of what we report.
    ///
    /// `model` is borrowed only for this call: its handles go into `mtmd_init_from_file`, and its widths are copied into
    /// the new projector. Nothing here keeps a reference to it, which is exactly what lets a provider store the result —
    /// and exactly what moves the "projector dies before the model" rule from the borrow checker into field order.
    pub fn open(model: &LoadedModel, mmproj: &Path) -> Result<Self, EmbedError> {
        let unavailable = || EmbedError::Unavailable(VISION_UNAVAILABLE.to_string());
        // The whole feature is Linux-only in practice — `build.rs` refuses to build the runtime anywhere else (§14) —
        // and this is where that shows up in Rust: bytes-to-OsStr is a unix trait, so the spelling stays honest about it.
        // A path containing a NUL cannot reach C at all. That is a fact about this machine's paths, not about any image,
        // so it answers `vision-unavailable` rather than blaming bytes that were never read.
        let path =
            CString::new(mmproj.as_os_str().as_bytes().to_vec()).map_err(|_| unavailable())?;

        // SAFETY: `path` is a live NUL-terminated string; the params come from mtmd's own defaults with four fields
        // turned off (GPU, timings, warmup) and the token budget left at -1 = "read from the artifact".
        let mctx = unsafe {
            let mut params = mtmd::mtmd_context_params_default();
            params.use_gpu = false;
            params.print_timings = false;
            params.warmup = false;
            params.image_min_tokens = -1;
            params.image_max_tokens = -1;
            mtmd::mtmd_init_from_file(path.as_ptr(), model.model_ptr().as_ptr(), params)
        };
        let mctx = NonNull::new(mctx).ok_or_else(unavailable)?;

        // SAFETY: `mctx` is live for the rest of this block; both calls only read facts out of it.
        let supports_vision = unsafe { mtmd::mtmd_support_vision(mctx.as_ptr()) };
        if !supports_vision {
            // SAFETY: first and only free of this handle, which we own and are discarding.
            unsafe { mtmd::mtmd_free(mctx.as_ptr()) };
            return Err(unavailable());
        }

        // The marker is borrowed from the context, so it is copied here rather than stored as a pointer.
        // SAFETY: `mctx` live; the returned pointer is a valid C string owned by the context.
        let marker = unsafe {
            let raw = mtmd::mtmd_get_marker(mctx.as_ptr());
            if raw.is_null() {
                CString::new("<__media__>").expect("a literal with no interior NUL")
            } else {
                CString::new(std::ffi::CStr::from_ptr(raw).to_bytes().to_vec())
                    .expect("mtmd's marker is a C string, so it holds no interior NUL")
            }
        };

        let facts = model.facts();
        Ok(Self {
            mctx,
            marker,
            opened_against: model.identity_token(),
            n_inp: facts.n_embd_inp,
            n_out: facts.n_embd_out,
            window: model.window(),
            last_cost: Cell::new(0),
        })
    }

    /// Patch/token count of the most recent encode **attempt** — see [`Projector::last_cost`] for what that includes and
    /// which two cases pin it.
    fn last_token_cost(&self) -> usize {
        self.last_cost.get()
    }

    /// One image's bytes to one L2-normalised row of `model`'s output width, plus the cost of the encode.
    ///
    /// Every refusal is one of §8's three classes — see this module's header for how they are split — and carries no path,
    /// no URL and none of mtmd's error text (§12 rule 8); mtmd writes its own detail to stderr, which is where a filename
    /// belongs.
    ///
    /// **`model` must be the model this projector was opened against.** Its context is where the encoder's rows are
    /// decoded, and `n_inp`, `n_out` and `window` are the widths and limit read off that artifact at
    /// [`Projector::open`]. Nothing in the signature enforces the pairing — that is the price of owning instead of
    /// borrowing, and it is why the two types exist: a caller that can borrow writes [`VisionModel`] and gets the rule
    /// checked, while a provider that owns holds exactly one model and passes *that* one. Handing a projector a different
    /// live model would feed rows of one artifact's width into another's decoder, which is a bug in the caller rather than
    /// a state this seam can report on.
    pub fn embed_image(
        &self,
        model: &LoadedModel,
        bytes: &[u8],
    ) -> Result<ImageEmbedding, EmbedError> {
        let unavailable = || EmbedError::Unavailable(VISION_UNAVAILABLE.to_string());
        let undecodable = || EmbedError::Unavailable(UNDECODABLE.to_string());
        // The context encodes for one specific model, so the model handed here has to be that one. Comparing identities
        // rather than widths: two artifacts can share 512/768 and still be different networks whose rows mean different
        // things, and a matched pair of widths is exactly the case that would otherwise read as safe.
        if !model.same_instance_as(&self.opened_against) {
            return Err(unavailable());
        }
        if bytes.is_empty() {
            return Err(undecodable());
        }

        // SAFETY: `bytes` is a live buffer for the duration of the call; the bitmap copies it, so nothing here keeps
        // the pointer. `placeholder` is false because a placeholder is a token-counting stand-in that
        // `mtmd_encode_chunk` refuses by contract.
        let wrapper = unsafe {
            let opt = mtmd::mtmd_helper_init_opt_default();
            mtmd::mtmd_helper_bitmap_init_from_buf(
                self.mctx.as_ptr(),
                bytes.as_ptr(),
                bytes.len(),
                false,
                opt,
            )
        };
        // `MTMD_VIDEO` is ON in our configure, so the wrapper's second handle is a real possibility and a real leak if
        // ignored — even though an image path leaves it null.
        if !wrapper.video_ctx.is_null() {
            // SAFETY: first and only free of this handle, which the wrapper handed us by value.
            unsafe { mtmd::mtmd_helper_video_free(wrapper.video_ctx) };
        }
        let bitmap = NonNull::new(wrapper.bitmap).ok_or_else(undecodable)?;

        // Audio is a §3 non-goal that this projector really does accept: measured 2026-11-03, a PCM buffer produced a live
        // bitmap here with `mtmd_bitmap_is_audio` answering true. Refusing it by name keeps the reason in the code and stops
        // the encoder being run on a modality nothing downstream knows how to store.
        //
        // Not the only guard: remove this arm and the same bytes are refused again one step later, when `mtmd_tokenize`
        // hands back an `AUDIO` chunk that [`Projector::decode_chunks`] does not recognise. The two answer differently on
        // purpose — `undecodable` here, `vision-unavailable` there — so the artifact test pins *which* guard fired: deleting
        // this one makes that case fail instead of passing quietly (verified by mutation on 2026-11-03).
        // SAFETY: `bitmap` is live and owned by us until the free below.
        let is_audio = unsafe { mtmd::mtmd_bitmap_is_audio(bitmap.as_ptr()) };
        if is_audio {
            // SAFETY: first and only free of this bitmap.
            unsafe { mtmd::mtmd_bitmap_free(bitmap.as_ptr()) };
            return Err(undecodable());
        }

        let outcome = self.encode_and_pool(model, bitmap);
        // SAFETY: first and only free of this bitmap, after every use of it above. The chunk list copies what it needs
        // (`mtmd.h` says the bitmaps keep their owners), so freeing here is safe for the pooled row we already copied.
        unsafe { mtmd::mtmd_bitmap_free(bitmap.as_ptr()) };
        outcome
    }

    /// Tokenize, encode, decode and pool one bitmap. Split out so the bitmap's single free lives in one place — see
    /// [`Projector::embed_image`], which calls this and then frees.
    fn encode_and_pool(
        &self,
        model: &LoadedModel,
        bitmap: NonNull<mtmd::mtmd_bitmap>,
    ) -> Result<ImageEmbedding, EmbedError> {
        let unavailable = || EmbedError::Unavailable(VISION_UNAVAILABLE.to_string());

        let chunks = unsafe { mtmd::mtmd_input_chunks_init() };
        if chunks.is_null() {
            // Not expected, and honest about why we cannot promise what happens instead: C does `new mtmd_input_chunks`
            // with no `try`, so an allocation failure throws a C++ exception that this crate neither catches nor claims to
            // survive — unwinding through Rust frames is outside our control, whatever the platform ends up doing. The
            // null check stays because a null list would be dereferenced by every call below it if a future pin turns this
            // into a checked API; it is not evidence that failure is graceful, and no test here pretends otherwise.
            return Err(unavailable());
        }

        // The prompt is the marker and nothing else: no instruction prefix (§1.2), no surrounding prose.
        // `parse_special` has to be true or the marker is ordinary text and matches no bitmap.
        let marker = self.marker.clone();
        let input = mtmd::mtmd_input_text {
            text: marker.as_ptr(),
            text_len: marker.as_bytes().len(),
            add_special: true,
            parse_special: true,
        };
        let bitmap_ptr = bitmap.as_ptr() as *const mtmd::mtmd_bitmap;

        // SAFETY: `mctx` live; `chunks` from `mtmd_input_chunks_init`; `input.text` outlives the call (owned by this
        // binding until it returns); `bitmaps` is an array of one live bitmap pointer.
        let tokenized =
            unsafe { mtmd::mtmd_tokenize(self.mctx.as_ptr(), chunks, &input, &bitmap_ptr, 1) };
        if tokenized != 0 {
            // SAFETY: the list is ours whatever happened to its contents, and this is its only free.
            unsafe { mtmd::mtmd_input_chunks_free(chunks) };
            // Neither answer is about the caller's bytes. 1 means the bitmap count did not match the marker count
            // (`mtmd.cpp:1453-1457`, `:1492-1496`) — this prompt holds exactly one marker and we pass exactly one bitmap,
            // so a 1 here is a bug in this crate. 2 is a caught `std::exception` from inside the tokenizer
            // (`mtmd.cpp:2073-2078`). Both are "this pair cannot serve it", which is what [`VISION_UNAVAILABLE`] means.
            return Err(unavailable());
        }

        let outcome = self.decode_chunks(model, chunks);
        // SAFETY: the list and every chunk inside it go here; `decode_chunks` copied out every row it needed, and the
        // pooled vector it returned is Rust-owned.
        unsafe { mtmd::mtmd_input_chunks_free(chunks) };
        outcome
    }

    /// Decode every chunk of a tokenized prompt into `model`'s context and pool the result.
    fn decode_chunks(
        &self,
        model: &LoadedModel,
        chunks: *mut mtmd::mtmd_input_chunks,
    ) -> Result<ImageEmbedding, EmbedError> {
        let undecodable = || EmbedError::Unavailable(UNDECODABLE.to_string());
        let unavailable = || EmbedError::Unavailable(VISION_UNAVAILABLE.to_string());

        // SAFETY: `chunks` is live and non-null for the whole loop; every chunk pointer comes from it.
        let count = unsafe { mtmd::mtmd_input_chunks_size(chunks) };
        let mut image_rows: Option<(Vec<f32>, usize, bool)> = None;
        for index in 0..count {
            // SAFETY: `index` is bounded by the size read immediately above from the same list.
            let chunk = unsafe { mtmd::mtmd_input_chunks_get(chunks, index) };
            // SAFETY: `chunk` belongs to the live list.
            let kind = unsafe { mtmd::mtmd_input_chunk_get_type(chunk) };
            // SAFETY: as above.
            let n_tokens = unsafe { mtmd::mtmd_input_chunk_get_n_tokens(chunk) };
            match kind {
                mtmd::MTMD_INPUT_CHUNK_TYPE_IMAGE => {
                    if n_tokens == 0 {
                        // The decoder produced no patches for these pixels: the encode failed on the bytes' behalf.
                        return Err(undecodable());
                    }
                    if image_rows.is_some() {
                        // Two image chunks would need a pooling decision §1.2 never made, because this seam is handed one
                        // bitmap at a time. That is a limit of this seam, not a fault in the file. (The cost on record
                        // after this refusal is the first chunk's — an attempt did measure a grid.)
                        return Err(unavailable());
                    }
                    // The patch count is known here, so **this** is where the attempt's cost goes on record — not after the
                    // loop. Review found the difference: with the write at the end, four refusals that happen once the grid
                    // exists (null rows, a width that cannot multiply, M-RoPE, a second image chunk) left the *previous*
                    // encode's number on record for an attempt that had measured patches, contradicting this field's own
                    // doc. §8 reads the number after an attempt, so it has to be this attempt's from the moment it is knowable.
                    //
                    // **No test can tell the two placements apart on this pin**, measured 2026-11-03: moving the write back
                    // below left all 11 cases green, because every refusal it protects is a path where the runtime
                    // contradicts itself and this artifact never takes one. It is defensive correctness, written down as
                    // such rather than sold as a fixed bug.
                    self.last_cost.set(n_tokens);

                    // M-RoPE would make `llama_batch.pos` section-major (`src/llama-batch.cpp:924`), and asking about it
                    // needs no encoder run — so it is asked **before** one. This artifact answers false; the probe note
                    // refused that case rather than decoding it with silently wrong positions, and neither does this seam.
                    // SAFETY: `mctx` live; a null chunk asks the projector-level question.
                    if unsafe { mtmd::mtmd_decode_use_mrope(self.mctx.as_ptr()) } {
                        return Err(unavailable());
                    }
                    // SAFETY: `mctx` live, `chunk` belongs to it. This runs the vision encoder alone — no text context,
                    // no KV — which is why an image never disturbs the text path's state.
                    let encoded = unsafe { mtmd::mtmd_encode_chunk(self.mctx.as_ptr(), chunk) };
                    if encoded != 0 {
                        return Err(undecodable());
                    }
                    // SAFETY: `mctx` live; the rows belong to it and are valid until the next encode, which is why they
                    // are copied here rather than referenced later.
                    let raw = unsafe { mtmd::mtmd_get_output_embd(self.mctx.as_ptr()) };
                    if raw.is_null() {
                        // The encoder said it succeeded and then handed back nothing — the runtime contradicting itself,
                        // which is not an answer about these bytes.
                        return Err(unavailable());
                    }
                    // Rows are `n_embd_inp` wide (§1.2's 512), not the stored width — that is what the decoder eats and
                    // what pooling then projects up to 768 inside the model.
                    let needed = self.n_inp.checked_mul(n_tokens).ok_or_else(unavailable)?;
                    let mut rows = Vec::with_capacity(needed);
                    // SAFETY: mtmd promises `n_inp * n_tokens` floats at this pointer for this chunk.
                    rows.extend_from_slice(unsafe { std::slice::from_raw_parts(raw, needed) });

                    // Whether *this* chunk needs a non-causal mask — asked, not assumed, so a re-pin that changes the
                    // answer changes our behaviour instead of leaving a stale hard-coded `false`.
                    // SAFETY: `mctx` live and `chunk` belongs to it.
                    let non_causal =
                        unsafe { mtmd::mtmd_decode_use_non_causal(self.mctx.as_ptr(), chunk) };
                    image_rows = Some((rows, n_tokens, non_causal));
                }
                mtmd::MTMD_INPUT_CHUNK_TYPE_TEXT => {
                    // The marker's own wrappers — `<|image>` before, `<image|>` after, **two tokens each side** on this pin
                    // (spike note: `chunk[0] text n_tokens=2`, `chunk[2] text n_tokens=2`) — are tokenized but deliberately
                    // **not** decoded. §1.2 embeds media on its own and the spike measured comparability with exactly this
                    // sequence: image rows alone through the decoder, pooled at sequence 0. Decoding the wrappers would
                    // shift every position and change what was measured; reading their token ids is not possible through
                    // this mirror anyway (`mtmd.h:266-269`).
                    //
                    // So `n_tokens` here is expected to be non-zero, and skipping it is the design rather than an
                    // oversight: a prompt we wrote ourselves, containing nothing but the marker, cannot carry note text.
                    let _ = n_tokens;
                }
                _ => {
                    // An AUDIO chunk (this projector accepts audio; §3 says we do not) or a type this pin added after the
                    // mirror was written. The named audio guard in [`Projector::embed_image`] normally refuses audio
                    // first — measured here on 2026-11-03, `mtmd_bitmap_is_audio` answered true for PCM — so reaching
                    // this arm with audio means that guard stopped working, and the class differs on purpose: a refusal
                    // that says "this seam cannot serve it" instead of "these bytes did not decode" is the difference
                    // between a bug in our crate and a bad file in someone's vault.
                    return Err(unavailable());
                }
            }
        }
        // Every image chunk stored above has a non-zero token count by construction, so the only ways to leave this loop
        // without rows are "no image chunk at all" and "the tokenizer produced something we did not recognise" — both
        // answers about this pair, not about the file.
        let (rows, image_tokens, non_causal) = image_rows.ok_or_else(unavailable)?;

        // The cost was written when this chunk's grid was measured, above, and it stays on the record through every
        // refusal below — see [`Projector::last_cost`] for which cases prove that.
        if image_tokens > self.window {
            return Err(EmbedError::Unavailable(OVER_BUDGET.to_string()));
        }

        let pooled = self.decode_and_pool(model, &rows, image_tokens, non_causal)?;
        Ok(ImageEmbedding {
            vector: Vector::new(pooled),
            token_cost: image_tokens,
        })
    }

    /// Feed the encoder's rows through `model`'s decoder and copy out the pooled row at output width.
    fn decode_and_pool(
        &self,
        model: &LoadedModel,
        rows: &[f32],
        n_tokens: usize,
        non_causal: bool,
    ) -> Result<Vec<f32>, EmbedError> {
        let unavailable = || EmbedError::Unavailable(VISION_UNAVAILABLE.to_string());

        // One sequence for the whole batch, so every row's `seq_id` pointer targets this same id. It is declared
        // `mut` and passed as a mutable pointer because that is what `llama_batch` asks for; llama.cpp only reads it.
        let mut shared_seq: llama_ffi::llama_seq_id = 0;
        let shared_seq_ptr: *mut llama_ffi::llama_seq_id = &mut shared_seq;
        let mut seq_ids: Vec<*mut llama_ffi::llama_seq_id> = vec![shared_seq_ptr; n_tokens];
        let mut positions: Vec<llama_ffi::llama_pos> = (0..n_tokens as i32).collect();
        let mut n_seq_id: Vec<i32> = vec![1; n_tokens];
        // Only the last token's output is needed for pooling; marking the rest would ask the graph to keep them.
        let mut logits: Vec<i8> = vec![0; n_tokens];
        if let Some(last) = logits.last_mut() {
            *last = 1;
        }
        let mut embd = rows.to_vec();

        let batch = llama_ffi::llama_batch {
            n_tokens: n_tokens as i32,
            token: std::ptr::null_mut(),
            embd: embd.as_mut_ptr(),
            pos: positions.as_mut_ptr(),
            n_seq_id: n_seq_id.as_mut_ptr(),
            seq_id: seq_ids.as_mut_ptr(),
            logits: logits.as_mut_ptr(),
        };

        let ctx = model.ctx_ptr().as_ptr();
        // SAFETY: `ctx` is live for as long as its `LoadedModel`; `batch` points into buffers owned by this function,
        // all alive until after `llama_decode` returns. The flag is read first and **put back to what it was** rather
        // than to an assumed true — see this module's header for why that distinction is the whole point. When the
        // chunk does not ask for non-causal attention, the flag is never touched at all.
        let decoded = unsafe {
            let was_causal = llama_ffi::llama_get_causal_attn(ctx);
            if non_causal {
                llama_ffi::llama_set_causal_attn(ctx, false);
            }
            let rc = llama_ffi::llama_decode(ctx, batch);
            if non_causal {
                llama_ffi::llama_set_causal_attn(ctx, was_causal);
            }
            rc
        };
        if decoded != 0 {
            // The text context refused rows it was given (`llama.h:1002` lists the reasons — no KV slot, batch too large,
            // eval failed). None of them is a statement about the file.
            return Err(unavailable());
        }

        // SAFETY: `ctx` live; the pooled buffer belongs to it and is copied immediately.
        let pooled = unsafe { llama_ffi::llama_get_embeddings_seq(ctx, 0) };
        if pooled.is_null() {
            // A decode that succeeded and pooled to nothing is the runtime contradicting itself.
            return Err(unavailable());
        }
        // SAFETY: the context promises `n_embd_out` floats for sequence 0 when pooling is on (the artifact answers
        // MEAN, which §1.2's recipe expects).
        let mut vector = Vec::with_capacity(self.n_out);
        vector.extend_from_slice(unsafe { std::slice::from_raw_parts(pooled, self.n_out) });

        let norm: f64 = vector
            .iter()
            .map(|x| f64::from(*x) * f64::from(*x))
            .sum::<f64>()
            .sqrt();
        if !norm.is_finite() || norm == 0.0 {
            // Not normalisable, so not a vector — and the pixels decoded fine to get here.
            return Err(unavailable());
        }
        let scale = 1.0 / norm as f32;
        for component in &mut vector {
            *component *= scale;
        }
        Ok(vector)
    }
}

impl Drop for Projector {
    fn drop(&mut self) {
        // SAFETY: the handle came from `mtmd_init_from_file` and is freed exactly once, here. What has to be true is that
        // the model this context keeps a pointer to (`mtmd.cpp:567`, `:615`) is still alive — for [`VisionModel`] the
        // borrow guarantees it, and for a provider **field declaration order** does it: the projector field is declared
        // before the field that owns the model, so it is dropped first. See `local_model::LocalEmbedder`.
        unsafe { mtmd::mtmd_free(self.mctx.as_ptr()) };
    }
}

/// Manual rather than derived, following [`LoadedModel`]'s precedent: what a projector *is* is its facts, and the one
/// field left out on purpose is `mctx`. A context address in a debug line invites a reader to diff two runs and read
/// meaning into a number that only says where the allocator put it.
impl std::fmt::Debug for Projector {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Projector")
            .field("marker", &self.marker)
            .field("n_inp", &self.n_inp)
            .field("n_out", &self.n_out)
            .field("window", &self.window)
            .field("last_token_cost", &self.last_cost)
            .finish_non_exhaustive()
    }
}

/// A [`Projector`] plus the model it was opened against, held by **borrow**.
///
/// Borrows the model because it must: `mtmd_init_from_file` keeps a raw pointer to the text model
/// (`mtmd.cpp:567`, initialised at `:615`), so the projector cannot outlive it and the lifetime says so rather than a
/// comment doing it.
///
/// **This is the façade over [`Projector`], and the split exists because a provider cannot store a borrow of its own
/// model.** The type that owns a `LoadedModel` is also the type that has to offer image capability, so it needs an owning
/// projector and must get the drop order right by field declaration instead; [`Projector`] is that half, and this one stays
/// for every caller that can borrow — including the artifact tests, whose API this task does not change.
pub struct VisionModel<'model> {
    /// The half that owns the `mtmd_context`. It holds no reference to [`Self::model`]; this façade supplies that on
    /// every call, which is what makes the borrow the guarantee it claims to be.
    projector: Projector,
    /// The model whose context the encoder's rows are decoded through, and whose widths and window were read at open.
    model: &'model LoadedModel,
}

impl<'model> VisionModel<'model> {
    /// Load `mmproj` beside an already loaded model and check it can do vision at all.
    ///
    /// The refusals are [`Projector::open`]'s — see it for the two shapes of "cannot" this answers with
    /// [`VISION_UNAVAILABLE`].
    pub fn open(model: &'model LoadedModel, mmproj: &Path) -> Result<Self, EmbedError> {
        Ok(Self {
            projector: Projector::open(model, mmproj)?,
            model,
        })
    }

    /// Patch/token count of the most recent encode attempt.
    ///
    /// §8's `over-budget` rule reads cost **after** an encode, because nothing before it knows what the patch grid will
    /// be. Zero means nothing has been attempted yet, which is a different statement from "this image cost nothing".
    ///
    /// Read straight through to the projector rather than mirrored into a field here: one number, in the type that
    /// measured it, so the façade and a provider driven through [`Projector`] cannot report different costs for one
    /// encode.
    pub fn last_token_cost(&self) -> usize {
        self.projector.last_token_cost()
    }

    /// One image's bytes to one L2-normalised row of [`LoadedModel::facts`]' output width.
    ///
    /// `&mut self` keeps this façade's signature as it was while [`Projector::embed_image`] is `&self`: the pipeline
    /// itself mutates no Rust state (the measured cost goes through a [`Cell`]), and a caller holding one of these should
    /// not be able to swap the model out from under it mid-encode.
    ///
    /// Every refusal is [`Projector::embed_image`]'s — one of §8's three classes, carrying no path, no URL and none of
    /// mtmd's error text (§12 rule 8).
    pub fn embed_image(&mut self, bytes: &[u8]) -> Result<Vector, EmbedError> {
        self.projector
            .embed_image(self.model, bytes)
            .map(|embedding| embedding.vector)
    }
}
