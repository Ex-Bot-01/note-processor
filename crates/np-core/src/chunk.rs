//! Chunking: cutting one note's text into the pieces a model is asked to embed.
//!
//! Text in, [`Chunk`] list out — no files, no database, no clock, no tokenizer, no network. This is
//! the function design-m2-embeddings §4 describes, and its contract is asserted here before any
//! provider exists:
//!
//! - **Total and deterministic.** Every input yields a result, and the same input yields byte-identical
//!   results on every platform and every run: no hash map, no time, no locale — and deliberately no
//!   Unicode property lookup either (`char::is_whitespace` and friends). A blank line is four ASCII
//!   bytes and nothing else, because a Unicode revision that reclassified one character would otherwise
//!   move chunk boundaries without anyone changing a rule — the same reason the workspace pins
//!   `unicode-width` exactly (F12) — and because the seam rules below stay readable from the bytes alone.
//! - **Slice identity.** For every chunk, `&text[byte_start..byte_end] == chunk.text`, with both offsets
//!   on char boundaries. Enforced by the [`debug_assert!`]s in [`chunk`] and pinned by `tests/chunk.rs`:
//!   an off-by-one here silently misaligns every citation M2b writes, and a boundary through a multi-byte
//!   character would panic on the German umlauts this corpus is full of.
//! - **Bounded.** No chunk exceeds [`MAX_CHUNK_BYTES`] — not "by a little", by any amount at all, so the
//!   tolerance a reader has to carry is zero.
//! - **Seams at block ends where possible.** A chunk is a run of whole markdown blocks (a maximal run of
//!   non-blank lines) while it fits the bound; only a block larger than the bound is cut, and then at a
//!   whitespace seam near the limit rather than at the limit itself.
//! - **Disjoint, and seam-blind to fences.** Chunks never overlap one another: v1 cuts at blank lines
//!   only, so the spec's "~350 tokens ±overlap" is not implemented here, and a fenced code block that
//!   contains a blank line can still be split in half — §4 wants fenced regions kept whole, which needs
//!   [`crate::parse`]'s notion of a block rather than this module's. Both belong with the tokenizer-aware
//!   cut that bumps [`CHUNKER_VERSION`], where they change every row and so announce themselves.
//! - **Stable under local edits.** A same-length edit changes the text of the chunks covering it and
//!   leaves every other chunk's offsets and bytes alone (`tests/chunk.rs` asserts both halves), which is
//!   what lets §5 re-embed only what moved.
//!
//! ## Why the domain is the decoded text, not the file
//!
//! [`chunk`] takes the same spelling as `notes.body`: the text `crate::scan`'s `decode` produces — a
//! leading BOM stripped, bytes UTF-8 cannot hold replaced with U+FFFD. File offsets are the tempting
//! choice and they are a trap: `crate::hash` digests the **raw bytes on disk**, BOM included, so the two
//! spellings differ for exactly the corpus F13/F18/F20/F23 refuse to key by — every BOM'd Windows export
//! and every non-UTF-8 note. Offsets into the file would therefore mean one thing for ordinary notes and
//! another for the lossy ones, in a table with no column to say which is which. Offsets into the decoded
//! text mean the same thing everywhere, because they index the bytes that were actually embedded. The
//! cost is stated rather than buried: for a note whose `notes.encoding` is `'lossy'` these offsets are an
//! *approximate* citation into the file, and M2b's evidence links have to say so instead of pointing one
//! character off.
//!
//! ## Why CRLF churn is accepted instead of normalised away
//!
//! Chunk text comes from stored note bytes, so a CRLF↔LF rewrite by an editor or a sync client moves
//! every offset in the file and re-embeds the whole note — worst case all ~18k chunks of one language,
//! against spec §"Linux **and** Windows: … line endings … differ". Normalising line endings before
//! chunking is the obvious fix and it is refused: `byte_start`/`byte_end` would then index a string
//! nobody has, which breaks the slice identity above, and a citation resolved through them would point at
//! text the file does not contain. The bytes that get embedded have to be the bytes a reader can go and
//! look at, so the churn is paid instead of laundered — written here so a Windows user's second scan is
//! not filed as a bug (design-m2-embeddings §4).

/// Which chunking code produced a row, stored in `chunks.chunker` (design-m2-embeddings §3).
///
/// Stamped per row rather than assumed globally, so a bump makes old rows *identifiable* instead of
/// merely presumed stale: staleness stays `chunker = CHUNKER_VERSION AND source_hash matches`, and the
/// tokenizer-aware cut §4 promises can land as version 2 without invalidating what version 1 wrote.
/// `i64` because that is what an SQLite `INTEGER` column binds and reads back.
pub const CHUNKER_VERSION: i64 = 1;

/// The hard byte bound every chunk obeys, and the number [`chunk`] measures its tolerance against.
///
/// **Why 1800.** The spec (`spec:223`) puts the target at "~350 tokens ±overlap", and tokens need a tokenizer
/// this milestone does not have (§4), so the bound is stated in bytes and picked to put the corpus's
/// common case near that target: prose here is mostly German and English running ~4 ASCII bytes per
/// subword token, which places 350 tokens at ~1400 bytes, and 1800 is the next round step up. The
/// headroom over 1400 is not slack about the target, it is seam quality — a bound sitting just below a
/// typical paragraph makes nearly every chunk end mid-sentence, and a chunk ending mid-sentence embeds
/// worse than one ending short. Two consequences belong to the number rather than to the implementation:
/// - chunks near the bound sit roughly **25-30% above** the token target — deliberate seam headroom, and the reason
///   this constant will be replaced by a tokenizer rather than tuned toward correctness. The "~4 ASCII bytes per
///   subword token" figure above is an **unmeasured estimate**, pending §6's real corpus; treat 1400 as arithmetic
///   on that estimate, not as a measurement of this vault; and
/// - a CJK- or emoji-dense note (3 bytes per character, ~1 token per character) can reach well past 350 tokens,
///   because **a byte bound cannot be a token bound** — which is exactly why this constant gets replaced by the
///   tokenizer-aware cut as a [`CHUNKER_VERSION`] bump instead of tuned into correctness.
///
/// It must also be at least 4 so one window always holds a whole UTF-8 sequence (see [`split_block`]).
pub const MAX_CHUNK_BYTES: usize = 1800;

// Stated where the constant is, and at compile time: [`split_block`] walks back from the limit to a char
// boundary, which needs one to be within reach of any start position.
const _: () = assert!(
    MAX_CHUNK_BYTES >= 4,
    "one window has to hold the longest UTF-8 sequence"
);

/// One embeddable piece of one note, in the shape `chunks` rows store (§3).
///
/// `text` is the slice, not a re-spelling of it — see the module doc's slice identity. `ordinal` is
/// `u32` because it counts positions within one note, and 4 billion chunks of one note would be ~7 TB of
/// text; [`chunk`] says so at the conversion rather than truncating quietly.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Chunk {
    /// 0-based position within its note, ascending with the byte offsets.
    pub ordinal: u32,
    /// Inclusive start offset into the text [`chunk`] was given. Always a char boundary.
    pub byte_start: usize,
    /// Exclusive end offset into that text. Always a char boundary, and `byte_end - byte_start` is at
    /// most [`MAX_CHUNK_BYTES`].
    pub byte_end: usize,
    /// Exactly `&text[byte_start..byte_end]`.
    pub text: String,
}

/// Cut one note's decoded text into chunks.
///
/// Empty text yields no chunks, and so does text holding nothing but blank lines: there is nothing there
/// for a model to mean. Otherwise the rules, in order:
///
/// 1. Split at **blank lines** — a line carrying nothing but spaces or tabs once its terminator is set
///    aside. That yields blocks, each spanning from the first space-free byte of its first line to the
///    last such byte of its last one, so no chunk starts or ends on whitespace.
/// 2. **Merge adjacent blocks** greedily while the whole span fits [`MAX_CHUNK_BYTES`]. The span is
///    measured start-to-end, which counts the blank lines between the blocks: they are part of the text
///    and therefore part of the cost.
/// 3. A block that alone exceeds the bound is cut by [`split_block`] and stands alone — its last piece
///    never absorbs the following block, so where a chunk ends depends on local structure only and an
///    edit far away cannot move it.
///
/// Every chunk is checked against the char-boundary rule, the slice identity and the bound before it is
/// returned.
pub fn chunk(text: &str) -> Vec<Chunk> {
    // Byte spans first, ordinals and text second: the two halves of the job cannot disagree about a
    // number if only one of them computes offsets.
    let mut spans: Vec<(usize, usize)> = Vec::new();
    // The run of whole blocks being accumulated into the next chunk, as its span so far.
    let mut run: Option<(usize, usize)> = None;

    for block in blocks(text) {
        if block.end - block.start > MAX_CHUNK_BYTES {
            if let Some(held) = run.take() {
                spans.push(held);
            }
            spans.extend(split_block(text, block.start, block.end));
            continue;
        }
        match run {
            // Merging is allowed while the span from this run's first byte to this block's last one
            // still fits — separators between them included.
            Some((start, _)) if block.end - start <= MAX_CHUNK_BYTES => {
                run = Some((start, block.end));
            }
            Some((start, end)) => {
                spans.push((start, end));
                run = Some((block.start, block.end));
            }
            None => run = Some((block.start, block.end)),
        }
    }
    if let Some(held) = run {
        spans.push(held);
    }

    spans
        .into_iter()
        .enumerate()
        .map(|(index, (byte_start, byte_end))| {
            // Boundaries before slicing: a debug build has to report the chunker, not panic inside
            // `str`'s own index check.
            debug_assert!(
                text.is_char_boundary(byte_start) && text.is_char_boundary(byte_end),
                "chunk {index} spans {byte_start}..{byte_end}, which are not char boundaries"
            );
            let chunk = Chunk {
                ordinal: u32::try_from(index).expect("one note holds fewer than 4 billion chunks"),
                byte_start,
                byte_end,
                text: text[byte_start..byte_end].to_string(),
            };
            debug_assert_eq!(
                &text[chunk.byte_start..chunk.byte_end],
                chunk.text,
                "slice identity for chunk {index}"
            );
            debug_assert!(
                chunk.byte_end - chunk.byte_start <= MAX_CHUNK_BYTES,
                "chunk {index} is {} bytes, over the {MAX_CHUNK_BYTES}-byte bound",
                chunk.byte_end - chunk.byte_start
            );
            chunk
        })
        .collect()
}

/// One markdown block as the byte span covering its lines.
struct Block {
    start: usize,
    end: usize,
}

/// The blocks of `text`: maximal runs of non-blank lines, in byte order.
///
/// A block starts at the first space-or-tab-free byte of its first line and ends after the last such
/// byte of its last one, so indentation and line terminators sit *between* blocks rather than inside
/// them. Once two blocks are merged into one chunk the separators between them are inside again — the
/// slice contract decides that, not a preference. `\n` ends a line and a `\r` before it is set aside
/// with the terminator, which is what makes CRLF and LF text produce the same block *structure* while
/// still producing different chunk bytes (module doc).
fn blocks(text: &str) -> Vec<Block> {
    let bytes = text.as_bytes();
    let mut out: Vec<Block> = Vec::new();
    // The block the current line continues, if the previous line was not blank.
    let mut open: Option<Block> = None;
    let mut line_start = 0;

    // `split_inclusive` keeps each terminator, so the offsets of the parts are the text's own offsets
    // and a final line without one needs no special case.
    for line in text.split_inclusive('\n') {
        let line_end = line_start + line.len();
        // The line's content: terminator aside, then the spaces an editor leaves at either end.
        let mut body_end = line_end;
        while body_end > line_start && matches!(bytes[body_end - 1], b'\n' | b'\r') {
            body_end -= 1;
        }
        let mut start = line_start;
        while start < body_end && matches!(bytes[start], b' ' | b'\t') {
            start += 1;
        }
        let mut end = body_end;
        while end > start && matches!(bytes[end - 1], b' ' | b'\t') {
            end -= 1;
        }

        if start == end {
            // A blank line closes the open block and opens nothing: separators are never content.
            if let Some(closed) = open.take() {
                out.push(closed);
            }
        } else {
            match open.as_mut() {
                // Consecutive non-blank lines: the block simply reaches to this line's content.
                Some(block) => block.end = end,
                None => open = Some(Block { start, end }),
            }
        }
        line_start = line_end;
    }
    if let Some(block) = open {
        out.push(block);
    }

    out
}

/// Whether one byte is a place a long block may be cut at. A newline counts because a block's lines are
/// soft breaks inside one paragraph, and breaking there reads better than breaking mid-word. `\r` does
/// not: cutting just before it would leave the CR stranded at a chunk's end.
fn is_break_byte(byte: u8) -> bool {
    matches!(byte, b' ' | b'\t' | b'\n')
}

/// Cut one oversized block into pieces of at most [`MAX_CHUNK_BYTES`] bytes.
///
/// Each cut is the **last** ASCII space, tab or newline at or before the limit — a word seam, so a piece
/// ends after a whole word whenever the text offers one. A block with no such seam in the window (one
/// long token, or CJK prose written without spaces) is cut at the last char boundary within it, which
/// still never splits a character. Both rules look only *backwards* from the limit, so a piece is never
/// over the bound and the tolerance stays zero; the trade is that a piece can end well short of it,
/// which is all "near the limit" means here.
fn split_block(text: &str, start: usize, end: usize) -> Vec<(usize, usize)> {
    let mut pieces = Vec::new();
    let mut from = start;

    while end - from > MAX_CHUNK_BYTES {
        // `from + MAX_CHUNK_BYTES` is inside the text because the loop only runs while more than a
        // bound of bytes remain. It may land mid-character, and walking back finds a boundary: no UTF-8
        // sequence is longer than 4 bytes, so with [`MAX_CHUNK_BYTES`] >= 4 there is always one within
        // reach of `from`.
        let mut cut = from + MAX_CHUNK_BYTES;
        while !text.is_char_boundary(cut) {
            cut -= 1;
        }
        // Then back to a word seam. Any position after an ASCII space, tab or newline is a char boundary
        // by construction, so no boundary test belongs in this walk.
        let mut probe = cut;
        while probe > from + 1 {
            if is_break_byte(text.as_bytes()[probe - 1]) {
                cut = probe;
                break;
            }
            probe -= 1;
        }
        pieces.push((from, cut));
        from = cut;
    }
    pieces.push((from, end));

    pieces
}
