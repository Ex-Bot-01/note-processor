//! Chunking: what [`np_core::chunk::chunk`] promises about the pieces it cuts.
//!
//! Everything here attacks the two ways this module can be wrong without anyone noticing: an offset
//! that no longer reads back as a slice (every citation M2b writes would point one byte off, and only
//! at the reader's end), and a chunker whose output shifts when nothing relevant moved (which turns §5's
//! "re-embed only what moved" into a full re-embed). Both are asserted from the **public** offsets and
//! text, never from internals, because it is that pair the `chunks` rows store.
//!
//! Each test names the production mutation that would make it fail.

use np_core::chunk::{chunk, Chunk, MAX_CHUNK_BYTES};

/// One paragraph of German prose with a distinct topic sentence: ~850 bytes, ten lines, no blank line
/// inside it, so it is exactly one block.
fn paragraph(topic: &str) -> String {
    let sentences = [
        "Der Vertrag mit der Firma wurde am Dienstag geprüft und für gut befunden.",
        "Eine Rückfrage zur Laufzeit ist noch offen und wird nächste Woche geklärt.",
        "Die Anlagen liegen im gemeinsamen Ordner und müssen nicht erneut geschickt werden.",
        "Bitte bis Freitag bestätigen, sonst gilt die stillschweigende Verlängerung.",
        "Ein Vermerk dazu wandert in die Akte und an alle Beteiligten weiter.",
    ];
    let mut out = String::from(topic);
    // Twice, so one paragraph is about half a chunk and six of them span several chunks.
    for _ in 0..2 {
        for sentence in sentences {
            out.push('\n');
            out.push_str(sentence);
        }
    }
    out
}

/// A six-paragraph note, blank line between each pair, terminated by a newline: ~5.1 kB, so several
/// chunks deep and therefore usable for the mutation check below.
fn six_paragraph_note() -> String {
    let topics = [
        "Besprechung 12. März",
        "Urlaub Sommer",
        "Reparatur Fahrrad",
        "Steuererklärung",
        "Buchungsbestätigung Hotel",
        "Reparatur Küche",
    ];
    let mut note = String::new();
    for (index, topic) in topics.iter().enumerate() {
        if index > 0 {
            note.push_str("\n\n");
        }
        note.push_str(&paragraph(topic));
    }
    note.push('\n');
    note
}

/// A note mixing the character widths that matter: 2-byte umlauts, a 4-byte emoji and a flag, 3-byte
/// CJK and Hangul. Four short blocks, so it is also the merge case.
fn wide_text() -> String {
    "# Größen und Wörter\n\n\
     Die Größe für Müller steht im Gespräch über Ökologie und Straße.\n\n\
     Ein 🎉 Emoji und ein 🙂 Smiley mitten im Satz 🚀 und danach Schluss.\n\n\
     中文的注释和日本語の注記, 여기에 한국어까지 한 줄에 섞여 있습니다.\n\n\
     Abschließender Absatz mit ß, ä und einem harten Umbruch am Ende.\n"
        .to_string()
}

/// The byte offsets of every chunk, for failure messages that print the whole shape at once.
fn spans(chunks: &[Chunk]) -> Vec<(u32, usize, usize)> {
    chunks
        .iter()
        .map(|c| (c.ordinal, c.byte_start, c.byte_end))
        .collect()
}

/// Assert the per-chunk invariants that hold for any input: char-boundary offsets, slice identity, the
/// bound, dense ascending ordinals, and no overlap between neighbours.
///
/// Mutations this pins: returning offsets computed independently of the text — say a `byte_end` taken
/// from a line length while `text` is trimmed — trips the slice identity; forgetting to close a run at
/// the bound trips the size check; emitting out of order trips the ordinals.
fn assert_contract(text: &str, chunks: &[Chunk]) {
    for (index, chunk) in chunks.iter().enumerate() {
        assert!(
            text.is_char_boundary(chunk.byte_start) && text.is_char_boundary(chunk.byte_end),
            "chunk {index} spans {}..{}, which are not char boundaries",
            chunk.byte_start,
            chunk.byte_end
        );
        assert_eq!(
            &text[chunk.byte_start..chunk.byte_end],
            chunk.text,
            "chunk {index} does not read back as the slice of its own note"
        );
        assert!(
            chunk.byte_end > chunk.byte_start,
            "chunk {index} is empty: nothing to embed and an ordinal wasted"
        );
        assert!(
            chunk.byte_end - chunk.byte_start <= MAX_CHUNK_BYTES,
            "chunk {index} is {} bytes, over the documented {MAX_CHUNK_BYTES}-byte bound",
            chunk.byte_end - chunk.byte_start
        );
        assert_eq!(
            chunk.ordinal as usize,
            index,
            "ordinals must be 0-based and dense: {:?}",
            spans(chunks)
        );
        if index > 0 {
            assert!(
                chunk.byte_start >= chunks[index - 1].byte_end,
                "chunk {index} overlaps its predecessor: {:?} then {:?}",
                spans(&chunks[..index]),
                spans(&chunks[index..])
            );
        }
    }
}

#[test]
fn the_same_note_twice_yields_the_same_chunks() {
    // Determinism is §4's first word and it is what makes a re-scan comparable with the rows already
    // stored. Mutation this pins: any dependence on iteration order or the clock — grouping blocks in a
    // `HashMap` before merging, or sizing a chunk from the current time — gives two calls over the same
    // bytes different answers.
    let text = six_paragraph_note();
    assert_eq!(chunk(&text), chunk(&text));
    // Same claim on the wide note, where a locale- or collation-dependent split would show up first.
    let wide = wide_text();
    assert_eq!(chunk(&wide), chunk(&wide));
    // And across two independent allocations of equal content: a chunker that read `String` capacity or
    // a pointer value rather than the bytes would disagree here.
    let mut rebuilt = String::new();
    rebuilt.push_str(&wide);
    assert_eq!(
        chunk(&wide_text()),
        chunk(&rebuilt),
        "equal text must chunk identically however it was built"
    );
}

#[test]
fn every_chunk_reads_back_as_the_slice_it_names() {
    // The slice contract on the corpus's hazard mix (umlauts, emoji, CJK, Hangul) plus the merge rule
    // §4 asks for: four short blocks are one chunk, not four. Mutation this pins: counting chunk length
    // in *characters* while storing byte offsets, or trimming a `\r` from the text but not from the
    // offsets — either makes the slicing below panic, or worse, succeed with text off by one.
    let text = wide_text();
    let chunks = chunk(&text);
    assert_contract(&text, &chunks);
    assert_eq!(
        chunks.len(),
        1,
        "every block here is short, so they merge into one chunk: {:?}",
        spans(&chunks)
    );
    for needle in ["Ökologie", "🎉", "中", "한국어"] {
        assert!(
            chunks[0].text.contains(needle),
            "{needle} has to survive into the embedded bytes"
        );
    }
    // A chunk starts on visible text, never on the indentation or terminator that separated blocks.
    for chunk in &chunks {
        assert_eq!(
            chunk.text.trim_start_matches([' ', '\t', '\n', '\r']),
            chunk.text,
            "a chunk starts on whitespace"
        );
    }

    // The same hazard across *several* chunks: one chunk cannot hide an offset that only drifts once the
    // bound is reached.
    let dense = "中文字 🎉 Wörter".repeat(400);
    let many = chunk(&dense);
    assert_contract(&dense, &many);
    assert!(
        many.len() >= 3,
        "a dense note must span chunks, got {}",
        many.len()
    );
}

#[test]
fn empty_text_yields_no_chunks() {
    // An empty note has nothing to embed, and a zero-length chunk would buy a row, a vector and a
    // citation to no text at all. Mutation this pins: dropping the empty case so an empty run is emitted
    // as one chunk spanning 0..0 — `assert_contract` refuses that too, but this test is the one that
    // says why.
    assert_eq!(chunk(""), Vec::<Chunk>::new());
    // A note of nothing but blank lines is the same case in disguise: whitespace carries no meaning for a
    // model, so it must not become a chunk whose text is "\n\n".
    assert_eq!(chunk("\n\n   \n\t\n"), Vec::<Chunk>::new());
}

#[test]
fn one_huge_block_is_cut_at_the_bound_and_tiles_the_note() {
    // A note that is one block with no blank line anywhere, so the merge path cannot help and the cut
    // path stands alone. Mutation this pins: merging without checking the bound (a chunk over
    // `MAX_CHUNK_BYTES`), or cutting at `from + MAX_CHUNK_BYTES` blindly so pieces overlap or leave a gap
    // — either way the concatenation below stops equalling the note.
    let sentence = "Und weiter geht die Zeile mit deutschen Worten und langen Begriffen. ";
    let text = sentence.repeat(120); // ~9 kB on one line, no blank line anywhere
    assert!(text.len() > 4 * MAX_CHUNK_BYTES);

    let chunks = chunk(&text);
    assert_contract(&text, &chunks);
    assert!(
        chunks.len() >= 4,
        "a note four bounds long must yield several chunks, got {}",
        chunks.len()
    );
    // Disjoint and contiguous: the pieces of a cut block cover it with no gap and no overlap, so reading
    // them in order returns the note. v1 puts no overlap between chunks at all — adding the spec's
    // ±overlap is a `CHUNKER_VERSION` bump, not a detail to slip in.
    let rejoined: String = chunks.iter().map(|c| c.text.as_str()).collect();
    assert_eq!(rejoined, text.trim_end_matches([' ', '\t', '\n']));

    // Every cut sits at a word seam: the byte before a chunk starts is the space that ended its
    // predecessor's last word. A forward-cutting mutant or a raw-offset cutter fails this.
    for chunk in chunks.iter().skip(1) {
        assert_eq!(
            text.as_bytes()[chunk.byte_start - 1],
            b' ',
            "chunk at {} was cut mid-word",
            chunk.byte_start
        );
    }
}

#[test]
fn a_huge_block_without_a_seam_is_cut_at_a_char_boundary() {
    // The seam search has to degrade into "last char boundary at or before the limit". Mutation this
    // pins: cutting at the raw limit offset, which slices through a 3-byte character — the panic would
    // come from `str`'s own check in the slicing below, and `assert_contract` catches the quieter variant
    // where a mutant re-spells the text instead of slicing it.
    //
    // Two ASCII bytes in front put every later boundary at `2 mod 3`, so `MAX_CHUNK_BYTES` itself is *not*
    // one: the walk-back has to move rather than get lucky.
    let text = format!("ab{}", "中".repeat(3000));
    assert!(
        !text.is_char_boundary(MAX_CHUNK_BYTES),
        "this fixture exists to make the limit land mid-character"
    );

    let chunks = chunk(&text);
    assert_contract(&text, &chunks);
    assert!(chunks.len() >= 4, "got {}", chunks.len());
    // Cut just short of the bound rather than past it, and never by more than one UTF-8 sequence.
    let first = chunks.first().expect("test setup: chunks");
    assert!(
        first.byte_end - first.byte_start < MAX_CHUNK_BYTES,
        "no boundary walk happened: {} bytes",
        first.byte_end - first.byte_start
    );
    assert!(first.byte_end - first.byte_start > MAX_CHUNK_BYTES - 4);
    // Still tiling, so nothing was dropped between pieces.
    let rejoined: String = chunks.iter().map(|c| c.text.as_str()).collect();
    assert_eq!(rejoined, text);
}

#[test]
fn a_final_line_without_a_newline_is_part_of_the_last_chunk() {
    // How most editors write the last line. Mutation this pins: treating the unterminated tail as no line
    // at all — the note would silently lose its closing sentence, and only someone comparing `notes.body`
    // with the chunks would ever see it.
    let with_newline = six_paragraph_note();
    let without = with_newline.trim_end_matches('\n').to_string();

    let full = chunk(&with_newline);
    let tail = chunk(&without);
    assert_contract(&with_newline, &full);
    assert_contract(&without, &tail);

    // The final line's last byte is inside the last chunk either way...
    assert_eq!(tail.last().expect("chunks").byte_end, without.len());
    // ...and that byte is the same one the terminated reading ends on: a line terminator is a separator,
    // never content, so it belongs to no chunk.
    assert_eq!(full.last().expect("chunks").byte_end, without.len());
    // Which is the whole claim in one line: the trailing newline changes nothing about the chunks.
    assert_eq!(full, tail);
}

#[test]
fn nothing_exceeds_the_bound_by_even_one_byte() {
    // The bound is hard, so the tolerance a caller carries is **zero** — that is the claim under test,
    // over inputs chosen to defeat an overshooting chunker: one long unbroken line, CJK with no spaces at
    // all, and blocks sitting just below and just above the limit.
    // Mutations this pins: `<=` becoming `<` when deciding a merge, or splitting forward to the next seam
    // after the limit instead of back to the previous one.
    let just_under = "x".repeat(MAX_CHUNK_BYTES - 2);
    let cases = [
        six_paragraph_note(),
        wide_text(),
        "y".repeat(6 * MAX_CHUNK_BYTES),
        "中文字".repeat(2000),
        paragraph("Ein Absatz für sich"),
        format!("{just_under}\n\n{just_under}\n\n{just_under}"),
        // A block whose only seam is past the limit: nothing to break at but a char boundary.
        format!("{}{}", "长".repeat(700), "x".repeat(40)),
    ];
    for text in cases {
        let chunks = chunk(&text);
        assert_contract(&text, &chunks);
        for chunk in &chunks {
            assert!(
                !chunk.text.trim().is_empty(),
                "a chunk of nothing but whitespace embeds noise: {:?}",
                spans(&chunks)
            );
        }
    }
}

#[test]
fn one_changed_byte_moves_only_the_chunks_that_cover_it() {
    // §7's mutation check, in F24's shape: a premise asserted for one side of a pair is half an
    // assertion. So both halves are asserted — the chunks whose span holds the byte changed, *and* every
    // other chunk byte-identical at byte-identical offsets.
    //
    // Mutations this pins, each of which passes a one-sided test: re-cutting and re-numbering everything
    // whenever anything changes (all texts differ), deriving ordinals or offsets from content rather than
    // position, and a merge decision that depends on total note length, so an edit in paragraph 4 moves
    // where paragraph 1 ends.
    let before = six_paragraph_note();
    let at = changed_byte(&before);
    let mut bytes = before.clone().into_bytes();
    // Letter for letter, inside paragraph 4: the note keeps its length and its blank-line structure, so
    // only content moved and nothing about the shape did.
    assert_eq!(bytes[at], b'D', "test setup: change a known letter");
    bytes[at] = b'B';
    let after = String::from_utf8(bytes).expect("test setup: still UTF-8");
    assert_eq!(before.len(), after.len());

    let chunks_before = chunk(&before);
    let chunks_after = chunk(&after);
    assert_contract(&before, &chunks_before);
    assert_contract(&after, &chunks_after);
    assert!(
        chunks_before.len() >= 3,
        "the note has to span several chunks for this to say anything, got {}",
        chunks_before.len()
    );

    // Offsets are untouched by a same-length edit: the note's shape did not move.
    assert_eq!(spans(&chunks_before), spans(&chunks_after));

    let changed: Vec<usize> = (0..chunks_before.len())
        .filter(|index| chunks_before[*index].text != chunks_after[*index].text)
        .collect();
    let covering: Vec<usize> = chunks_before
        .iter()
        .enumerate()
        .filter(|(_, c)| c.byte_start <= at && at < c.byte_end)
        .map(|(index, _)| index)
        .collect();

    // The same-length edit above cannot see a merge decision that depends on length rather than content, and it
    // cannot see the seam rule at all -- so the two claims §4 actually makes are pinned here, one by a
    // length-changing edit and once on the untouched note.
    let mut grown = before.clone();
    grown.insert_str(at, "bb");
    for c in chunk(&grown) {
        if c.byte_end <= at {
            let same = chunks_before
                .iter()
                .find(|b| b.byte_start == c.byte_start && b.byte_end == c.byte_end);
            assert!(
                same.is_some_and(|b| b.text == c.text),
                "a byte inserted at {at} must not disturb the chunk ending at {}: {:?}",
                c.byte_end,
                c.text
            );
        }
    }
    // Seam rule, asserted where §4 actually promises it: prose made of blocks under the bound. A note with an
    // oversized block legitimately ends chunks mid-block -- that is `split_block`'s contract, not a violation --
    // so asserting this over the mutation fixture would be asserting something the design never claims.
    let prose = "Erste Abschnitt mit genug Text zum Zählen.

"
    .repeat(200);
    assert!(
        chunk(&prose)
            .iter()
            .all(|c| prose[c.byte_end..].starts_with("\n\n") || c.byte_end == prose.len()),
        "in prose a chunk boundary is a block seam or the end of the note, never a cut through a paragraph"
    );

    assert!(
        !covering.is_empty() && covering.len() < chunks_before.len(),
        "the edited byte must sit inside some chunks and outside others: {covering:?} of {}",
        chunks_before.len()
    );
    assert_eq!(
        changed, covering,
        "exactly the chunks spanning the edit may change text"
    );
    for index in 0..chunks_before.len() {
        if covering.contains(&index) {
            continue;
        }
        assert_eq!(
            chunks_before[index], chunks_after[index],
            "chunk {index} is outside the edited byte and must be untouched, row and all"
        );
    }
}

/// The offset of the byte [`one_changed_byte_moves_only_the_chunks_that_cover_it`] substitutes: one past
/// paragraph 4's topic, which is the `D` of the sentence under it. Resolved through the text so the test
/// cannot quietly point at some other position than it claims.
fn changed_byte(text: &str) -> usize {
    let needle = "Steuererklärung";
    text.find(needle)
        .expect("test setup: paragraph 4 is in the note")
        + needle.len()
        + 1
}

#[test]
fn crlf_text_is_chunked_as_written_and_not_normalised() {
    // §4 accepts CRLF churn instead of normalising it, and this pins the accepted half of that deal: chunk
    // text carries the bytes the note actually holds. The mutation this pins is the tempting "fix" —
    // normalising `\r\n` to `\n` before cutting — which would hand back text that is not a slice of the
    // note and break the contract every citation rests on.
    let lf = six_paragraph_note();
    let crlf = lf.replace("\n", "\r\n");
    let chunks = chunk(&crlf);
    assert_contract(&crlf, &chunks);

    // The line endings survive into the embedded bytes...
    assert!(
        chunks.iter().any(|c| c.text.contains("\r\n")),
        "CRLF must not be normalised away"
    );
    // ...and every offset differs from the LF reading. That is the re-embed cost §4 says is paid
    // knowingly, asserted rather than promised: a chunker that hid it would make these equal.
    assert_ne!(spans(&chunk(&lf)), spans(&chunks));
}
