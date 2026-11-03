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

use std::ffi::CString;
use std::os::unix::ffi::OsStrExt;
use std::path::Path;
use std::ptr::NonNull;

use crate::embed::{EmbedError, Vector};
use crate::llama_ffi::{self, mtmd};
use crate::local_model::LoadedModel;

/// These bytes are not a raster this build can decode (§8). Audio arrives here too: the pinned projector advertises
/// audio support, and §3 makes it a non-goal, so "not an image" and "did not decode" get the same answer — the caller's
/// action is identical either way, and §8's list has no audio class to invent one for.
pub const UNDECODABLE: &str = "undecodable";
/// The encode measured larger than the context can hold (§8). Decided from the patch count this encode produced, never
/// from file size — the spike measured 336 tokens for a 1024×768 image and 88 for 512×384.
///
/// **Not reachable with one image on this pin**, which is a measurement: a 4096×3072 PNG cost 1,064 tokens on 2026-11-03,
/// because mtmd clamps the patch grid to the artifact's own soft-token ceiling and this build's window is 8,192. The check
/// is not decoration — it runs before the decode, and past the window llama.cpp asserts instead of returning a code, so a
/// missing guard turns a refusal into an abort (`tests/local_vision.rs::a_large_image_either_embeds_within_the_window_or_refuses_over_budget`
/// prints whichever branch a given image takes).
pub const OVER_BUDGET: &str = "over-budget";
/// This **build and model pair** cannot serve the request (§8): the projector would not load beside this model, carries no
/// vision graph, tokenized wrong, asked for a position scheme this seam does not implement, refused the rows, or pooled to
/// nothing usable. Not a statement about the caller's bytes — that distinction is why this class exists next to
/// [`UNDECODABLE`].
pub const VISION_UNAVAILABLE: &str = "vision-unavailable";

/// A projector loaded beside one [`LoadedModel`], able to turn image bytes into vectors of that model's width.
///
/// Borrows the model because it must: `mtmd_init_from_file` keeps a raw pointer to the text model
/// (`mtmd.cpp:567`, initialised at `:615`), so the projector cannot outlive it and the lifetime says so rather than a
/// comment doing it.
pub struct VisionModel<'model> {
    /// From [`mtmd::mtmd_init_from_file`]; freed in [`VisionModel::drop`], before the model it borrows dies.
    mctx: NonNull<mtmd::mtmd_context>,
    model: &'model LoadedModel,
    /// The media marker mtmd splits a prompt on — copied out of C memory at open, because the string it returns is
    /// owned by the context and this seam hands that context to `Drop` while still holding the marker.
    marker: CString,
    /// `llama_model_n_embd_inp`: the width of one encoder row (§1.2's 512), which is what the decoder is fed.
    n_inp: usize,
    /// `llama_model_n_embd_out`: the width stored and searched (768). Pooling happens at this width, not at input.
    n_out: usize,
    /// Tokens one forward pass holds, read from the built context rather than the requested number.
    window: usize,
    /// Patch count of the most recent encode attempt, or 0 before any ran.
    last_token_cost: usize,
}

impl<'model> VisionModel<'model> {
    /// Load `mmproj` beside an already loaded model and check it can do vision at all.
    ///
    /// Refuses [`VISION_UNAVAILABLE`] in both shapes of "cannot": this binary built without the feature (a different
    /// module says that), and a projector that loads but carries no vision graph. A projector that will not load at all
    /// is the same refusal — from the caller's side it is one fact, "this build cannot read images", and §12 rule 8 keeps
    /// mtmd's own error text (which names the file) out of what we report.
    pub fn open(model: &'model LoadedModel, mmproj: &Path) -> Result<Self, EmbedError> {
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
            model,
            marker,
            n_inp: facts.n_embd_inp,
            n_out: facts.n_embd_out,
            window: model.window(),
            last_token_cost: 0,
        })
    }

    /// Patch/token count of the most recent encode attempt.
    ///
    /// §8's `over-budget` rule reads cost **after** an encode, because nothing before it knows what the patch grid will
    /// be. Zero means nothing has been attempted yet, which is a different statement from "this image cost nothing".
    pub fn last_token_cost(&self) -> usize {
        self.last_token_cost
    }

    /// One image's bytes to one L2-normalised row of [`LoadedModel::facts`]' output width.
    ///
    /// Every refusal is one of §8's three classes — see this module's header for how they are split — and carries no path,
    /// no URL and none of mtmd's error text (§12 rule 8); mtmd writes its own detail to stderr, which is where a filename
    /// belongs.
    pub fn embed_image(&mut self, bytes: &[u8]) -> Result<Vector, EmbedError> {
        let undecodable = || EmbedError::Unavailable(UNDECODABLE.to_string());
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
        // hands back an `AUDIO` chunk that [`VisionModel::decode_chunks`] does not recognise. The two answer differently on
        // purpose — `undecodable` here, `vision-unavailable` there — so the artifact test pins *which* guard fired: deleting
        // this one makes that case fail instead of passing quietly (verified by mutation on 2026-11-03).
        // SAFETY: `bitmap` is live and owned by us until the free below.
        let is_audio = unsafe { mtmd::mtmd_bitmap_is_audio(bitmap.as_ptr()) };
        if is_audio {
            // SAFETY: first and only free of this bitmap.
            unsafe { mtmd::mtmd_bitmap_free(bitmap.as_ptr()) };
            return Err(undecodable());
        }

        let outcome = self.encode_and_pool(bitmap);
        // SAFETY: first and only free of this bitmap, after every use of it above. The chunk list copies what it needs
        // (`mtmd.h` says the bitmaps keep their owners), so freeing here is safe for the pooled row we already copied.
        unsafe { mtmd::mtmd_bitmap_free(bitmap.as_ptr()) };
        outcome
    }

    /// Tokenize, encode, decode and pool one bitmap. Split out so the bitmap's single free lives in one place — see
    /// [`VisionModel::embed_image`], which calls this and then frees.
    fn encode_and_pool(
        &mut self,
        bitmap: NonNull<mtmd::mtmd_bitmap>,
    ) -> Result<Vector, EmbedError> {
        let unavailable = || EmbedError::Unavailable(VISION_UNAVAILABLE.to_string());

        let chunks = unsafe { mtmd::mtmd_input_chunks_init() };
        if chunks.is_null() {
            // Not expected: C does `new mtmd_input_chunks` with no `try` (`mtmd.cpp:2872-2874`), so an allocation failure
            // throws and aborts the process rather than handing back null. The check stays because a null list would be
            // dereferenced by every call below it, and because reading the mirror as "returns null on failure" is exactly
            // the kind of assumption that should cost nothing to survive.
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

        let outcome = self.decode_chunks(chunks);
        // SAFETY: the list and every chunk inside it go here; `decode_chunks` copied out every row it needed, and the
        // pooled vector it returned is Rust-owned.
        unsafe { mtmd::mtmd_input_chunks_free(chunks) };
        outcome
    }

    /// Decode every chunk of a tokenized prompt into the text context and pool the result.
    fn decode_chunks(
        &mut self,
        chunks: *mut mtmd::mtmd_input_chunks,
    ) -> Result<Vector, EmbedError> {
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
                        // bitmap at a time. That is a limit of this seam, not a fault in the file.
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

                    // M-RoPE would make `llama_batch.pos` section-major
                    // (`src/llama-batch.cpp:924`); this artifact answers false, and the probe note refused that case
                    // rather than decoding it with silently wrong positions. Neither does this seam.
                    // SAFETY: `mctx` live; a non-null chunk asks about that chunk.
                    if unsafe { mtmd::mtmd_decode_use_mrope(self.mctx.as_ptr()) } {
                        return Err(unavailable());
                    }
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
                    // this mirror anyway (`mtmd_input_chunk_get_tokens_text` returns lossy UTF-8, `mtmd.h:266-269`).
                    //
                    // So `n_tokens` here is expected to be non-zero, and skipping it is the design rather than an
                    // oversight: a prompt we wrote ourselves, containing nothing but the marker, cannot carry note text.
                    let _ = n_tokens;
                }
                _ => {
                    // An AUDIO chunk (this projector accepts audio; §3 says we do not) or a type this pin added after the
                    // mirror was written. The named audio guard in [`VisionModel::embed_image`] normally refuses audio
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

        // Cost is recorded for *this* attempt, before the decode that might fail: §8's rule reads the number after an
        // encode, and an over-budget refusal with no number attached would leave the report nothing to say.
        self.last_token_cost = image_tokens;
        if image_tokens > self.window {
            return Err(EmbedError::Unavailable(OVER_BUDGET.to_string()));
        }

        let pooled = self.decode_and_pool(&rows, image_tokens, non_causal)?;
        Ok(Vector::new(pooled))
    }

    /// Feed the encoder's rows through the text decoder and copy out the pooled row at output width.
    fn decode_and_pool(
        &self,
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

        let ctx = self.model.ctx_ptr().as_ptr();
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

impl Drop for VisionModel<'_> {
    fn drop(&mut self) {
        // SAFETY: the handle came from `mtmd_init_from_file` and is freed exactly once, here. The borrowed model outlives
        // this because the borrow checker says so — `mtmd_context` keeps a raw pointer to it (`mtmd.cpp:567`, `:615`).
        unsafe { mtmd::mtmd_free(self.mctx.as_ptr()) };
    }
}
