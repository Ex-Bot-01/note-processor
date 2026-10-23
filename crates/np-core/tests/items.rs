//! Item candidates: which lines of a note carry a status, and which state the marker on them says.
//!
//! Extraction is pure text in, structs out — no database rows here, because an item's timeline is
//! M3's business and this milestone only records what the note claims. The behaviour that matters
//! is *negative*: prose with no marker yields nothing, because open is the absence of a marker on
//! an item (spec §8.7), not a property of every sentence in the file.

use std::time::{Duration, Instant};

use np_core::config::MarkerConfig;
use np_core::items::{extract_items, ItemCandidate, ItemState};

/// The comparable shape of a candidate list, so an assertion failure prints every item found
/// instead of only the first difference.
fn found(items: &[ItemCandidate]) -> Vec<(&str, usize, ItemState, &str)> {
    items
        .iter()
        .map(|item| {
            (
                item.text.as_str(),
                item.line,
                item.state,
                item.marker.as_str(),
            )
        })
        .collect()
}

/// A marker config with the given lists, in the shape [`MarkerConfig`] deserializes into — so a
/// test that needs its own vocabulary reads like a `config.toml` section.
fn markers_with(done: &[&str], paused: &[&str]) -> MarkerConfig {
    MarkerConfig {
        done: done.iter().map(|marker| (*marker).to_string()).collect(),
        paused: paused.iter().map(|marker| (*marker).to_string()).collect(),
    }
}

#[test]
fn checkbox_states() {
    let markers = MarkerConfig::default();

    // The unchecked box is the Open spelling, and its marker is the box itself without the bullet.
    assert_eq!(
        found(&extract_items("- [ ] buy milk\n", &markers)),
        [("buy milk", 1, ItemState::Open, "[ ]")]
    );
    assert_eq!(
        found(&extract_items("- [x] bought the beans\n", &markers)),
        [("bought the beans", 1, ItemState::Done, "[x]")]
    );
    // An uppercase box is the same claim; the recorded marker stays the canonical spelling,
    // because a checkbox comes from this rule rather than from config.
    assert_eq!(
        found(&extract_items("- [X] UPPERCASE BOX\n", &markers)),
        [("UPPERCASE BOX", 1, ItemState::Done, "[x]")]
    );

    // A checkbox outranks any keyword on the same line, in both directions: the box is the
    // author's own statement about this item's status.
    assert_eq!(
        found(&extract_items("- [ ] vendor contract done\n", &markers)),
        [("vendor contract done", 1, ItemState::Open, "[ ]")]
    );
    assert_eq!(
        found(&extract_items("- [x] still on hold\n", &markers)),
        [("still on hold", 1, ItemState::Done, "[x]")]
    );

    // The line number is the line's own, and a line without a marker contributes nothing.
    assert_eq!(
        found(&extract_items(
            "nothing to see here\n- [ ] second line\nplain text again\n",
            &markers
        )),
        [("second line", 2, ItemState::Open, "[ ]")]
    );
    // One line yields one candidate, never one per box: a second box is part of the item's text.
    assert_eq!(
        found(&extract_items(
            "- [ ] first box [ ] and another\n",
            &markers
        )),
        [("first box [ ] and another", 1, ItemState::Open, "[ ]")]
    );
}

#[test]
fn tick_makes_done() {
    let markers = MarkerConfig::default();

    assert_eq!(
        found(&extract_items("buy milk ✅\n", &markers)),
        [("buy milk", 1, ItemState::Done, "✅")]
    );
    // A tick is a literal marker: it matches wherever it appears, bullet or no bullet.
    assert_eq!(
        found(&extract_items("- ✅ shipped it\n", &markers)),
        [("shipped it", 1, ItemState::Done, "✅")]
    );
    // The tick wins over a keyword further into the line even though `erledigt` also matches,
    // because the done list is scanned in config order and `✅` is its first entry.
    assert_eq!(
        found(&extract_items("on hold ✅\n", &markers)),
        [("on hold", 1, ItemState::Done, "✅")]
    );
}

#[test]
fn german_and_english_keywords() {
    let markers = MarkerConfig::default();

    // The trailing sentence dash went with the marker, not with the text.
    assert_eq!(
        found(&extract_items("vertrag angefragt — erledigt\n", &markers)),
        [("vertrag angefragt", 1, ItemState::Done, "erledigt")]
    );
    assert_eq!(
        found(&extract_items("vendor contract paused\n", &markers)),
        [("vendor contract", 1, ItemState::Paused, "paused")]
    );
    // Case-insensitive matching, and the marker comes back exactly as config wrote it — never the
    // line's spelling and never lowercased, so `items.marker` reads like the author's vocabulary.
    let custom = markers_with(&["Eingetragen"], &["In Wartestellung"]);
    assert_eq!(
        found(&extract_items("sache EINGETRAGEN\n", &custom)),
        [("sache", 1, ItemState::Done, "Eingetragen")]
    );
    assert_eq!(
        found(&extract_items("druck in wartestellung\n", &custom)),
        [("druck", 1, ItemState::Paused, "In Wartestellung")]
    );
    // A configured list replaces the defaults rather than extending them: `done` is not a marker
    // for a workspace that did not configure it.
    assert!(extract_items("task done\n", &custom).is_empty());
}

#[test]
fn no_marker_means_no_item_row() {
    let markers = MarkerConfig::default();

    // Spec §8.7: open is the absence of a marker *on an item*. A sentence that happens to talk
    // about a vendor is not an item, so a scan of ordinary prose yields no candidate at all.
    let prose = "The vendor sent the contract in October and we agreed on quarterly invoices.\n\
                 They will mail the paperwork next week and nothing is due before then.\n";
    assert_eq!(found(&extract_items(prose, &markers)), []);

    // A list bullet without a checkbox and without a keyword is still not a candidate: carrying a
    // marker is what makes a line an item.
    assert!(extract_items("- buy milk\n1. call the vendor\n* send invoice\n", &markers).is_empty());

    // Blank space carries no status either.
    assert!(extract_items("\n   \n\t\n", &markers).is_empty());
}

/// Ignored and release-only: run it with `cargo test --release -- --ignored`, which is what Task
/// 10's fixture pass and CI do.
///
/// The plan's original bound was `< 200 ms` for the 5 MB line. That is a wall-clock constant on an
/// unmeasured machine: the same input measured ~1.09 s in a debug build, so keeping it would mean
/// betting on an unmeasured >= 5.4x release speedup inside an ignored test, which is flaky-CI
/// material. What the case is actually for is *scaling*: per-line work must stay linear in line
/// length, so the 5 MB pass may cost at most 8x the 1 MB pass (5x the size plus room for timing
/// noise), with a deliberately generous absolute ceiling of 1 s for the 5 MB pass. The failure mode
/// this catches is quadratic work per line, not a slow machine.
#[test]
#[ignore = "huge single-line input; release-only (`cargo test --release -- --ignored`)"]
fn empty_and_huge_input() {
    assert!(extract_items("", &MarkerConfig::default()).is_empty());

    // One huge line of ordinary prose with the marker at the far end: every configured marker has
    // to be ruled out at every character that could start one, and nothing short of a full pass
    // finds the hit. A newline-split input would hide this, because per-line work is what scales.
    let filler = "alpha beta gamma delta ";
    let line_of = |repeats: usize| format!("{}erledigt", filler.repeat(repeats));
    let small_line = line_of(5 * 1024 * 1024 / filler.len() / 5 + 1);
    let big_line = line_of(5 * 1024 * 1024 / filler.len());
    assert!(
        small_line.len() >= 1024 * 1024,
        "small fixture is {} bytes",
        small_line.len()
    );
    assert!(
        big_line.len() >= 5 * 1024 * 1024,
        "big fixture is {} bytes",
        big_line.len()
    );

    let started = Instant::now();
    let small = extract_items(&small_line, &MarkerConfig::default());
    let small_took = started.elapsed();

    let started = Instant::now();
    let big = extract_items(&big_line, &MarkerConfig::default());
    let big_took = started.elapsed();

    for found in [&small, &big] {
        assert_eq!(found.len(), 1, "one line yields one candidate");
        assert_eq!(found[0].state, ItemState::Done);
        assert_eq!(found[0].marker, "erledigt");
    }
    assert!(
        big_took <= small_took * 8,
        "5 MB took {big_took:?} against 1 MB {small_took:?}: the scan must stay linear in length"
    );
    assert!(
        big_took < Duration::from_secs(1),
        "a 5 MB single line took {big_took:?}; expected well under a second in release"
    );
}

#[test]
fn keyword_markers_match_on_word_boundaries() {
    let markers = MarkerConfig::default();

    // The ruling's own examples: a keyword is never found inside a longer word, in either
    // language. `download` does not contain `done` and `abandoned` does, but only as part of a
    // word — which is exactly the case that must not fire.
    assert!(extract_items("download the contract\n", &markers).is_empty());
    assert!(extract_items("he abandoned the idea\n", &markers).is_empty());
    assert!(extract_items("donex and xdone\n", &markers).is_empty());
    assert!(extract_items("fertigmaßig geplant\n", &markers).is_empty());
    assert!(extract_items("umgestoppt\n", &markers).is_empty());
    // An underscore continues a word, so `done_done` is one token and not a marker.
    assert!(extract_items("done_done\n", &markers).is_empty());

    // Punctuation, whitespace and the ends of the line are boundaries, and case does not matter.
    assert_eq!(
        found(&extract_items("Task DONE\n", &markers)),
        [("Task", 1, ItemState::Done, "done")]
    );
    // The `!` stays: only a trailing *sentence dash* is dropped with the marker, because anything
    // else the author typed is their own text and this module does not edit prose.
    assert_eq!(
        found(&extract_items("Vendor erledigt!\n", &markers)),
        [("Vendor !", 1, ItemState::Done, "erledigt")]
    );

    // A multi-word marker is a phrase with the same boundary rule at both ends.
    assert_eq!(
        found(&extract_items("the on hold status\n", &markers)),
        // The gap where the phrase was stays: `text` is cut, never rewritten or re-flowed.
        [("the  status", 1, ItemState::Paused, "on hold")]
    );
    assert!(extract_items("the on holdup notice\n", &markers).is_empty());
    // A phrase needs exactly one plain space: `on  hold` (two of them), a tab or an NBSP is not the
    // configured marker, and the line yields no item rather than a near miss.
    assert!(extract_items("the on  hold status\n", &markers).is_empty());
}

#[test]
fn heading_lines_carry_no_status() {
    let markers = MarkerConfig::default();

    // A heading names a topic, not an item, so `## Erledigt` is an outline entry and nothing else.
    assert!(extract_items("## Erledigt\n# done\n", &markers).is_empty());
    // The exclusion is the outline's own ATX rule, six hashes wide: seven is prose, and the bullet
    // stripping that follows knows nothing about headings, which is why the hashes stay in `text`.
    assert_eq!(
        found(&extract_items("####### done\n", &markers)),
        [("#######", 1, ItemState::Done, "done")]
    );
    // Not even a checkbox rescues a heading line.
    assert!(extract_items("## - [x] shipped\n", &markers).is_empty());
}

#[test]
fn fenced_block_holds_no_items() {
    let markers = MarkerConfig::default();

    // The same fence the headings, tags, words and links stop at — `prose_lines` is the one place
    // that decides it, so a code sample can never disagree with the outline about prose.
    assert_eq!(
        found(&extract_items(
            "```\n- [x] example\n```\n- [ ] real\n",
            &markers
        )),
        [("real", 4, ItemState::Open, "[ ]")]
    );
    assert!(extract_items("alpha text\n```\nerledigt ✅\n```\nbeta text\n", &markers).is_empty());
}

#[test]
fn precedence_follows_config_order() {
    let markers = MarkerConfig::default();

    // Done is listed before paused, so a line that mentions both is done — wherever on the line
    // each of them sits.
    assert_eq!(
        found(&extract_items("verschoben, dann erledigt\n", &markers)),
        [("verschoben, dann", 1, ItemState::Done, "erledigt")]
    );
    assert_eq!(
        found(&extract_items("erledigt, dann verschoben\n", &markers)),
        [(", dann verschoben", 1, ItemState::Done, "erledigt")]
    );
    // Inside one list the same rule picks the earlier entry, and it is that marker which is cut
    // out of the text — the other stays where it was written.
    assert_eq!(
        found(&extract_items("finished, then done\n", &markers)),
        [("finished, then", 1, ItemState::Done, "done")]
    );
}

#[test]
fn text_keeps_the_line_and_loses_the_bullet_and_marker() {
    let markers = MarkerConfig::default();

    // Bullets: dash, asterisk, plus, an ordered list, and an indented item.
    assert_eq!(
        found(&extract_items("1. vendor contract — paused\n", &markers)),
        [("vendor contract", 1, ItemState::Paused, "paused")]
    );
    assert_eq!(
        found(&extract_items("  * buy milk ✅\n", &markers)),
        [("buy milk", 1, ItemState::Done, "✅")]
    );
    assert_eq!(
        found(&extract_items("+ call the vendor - finished\n", &markers)),
        [("call the vendor", 1, ItemState::Done, "finished")]
    );
    // A `)` ordered list is a bullet as well…
    assert_eq!(
        found(&extract_items("1) vendor contract paused\n", &markers)),
        [("vendor contract", 1, ItemState::Paused, "paused")]
    );
    // …while a line that merely starts with a digit has no bullet to strip and keeps its text.
    assert_eq!(
        found(&extract_items("2 + 2 = 4 — erledigt\n", &markers)),
        [("2 + 2 = 4", 1, ItemState::Done, "erledigt")]
    );
    // An en dash is a sentence dash too.
    assert_eq!(
        found(&extract_items("vertrag angefragt – vertagt\n", &markers)),
        [("vertrag angefragt", 1, ItemState::Paused, "vertagt")]
    );
    // A dash mid-line and the space around the cut both stay: nothing in the interior is rewritten,
    // collapsed or reordered, so the text can still be found in the note by eye.
    assert_eq!(
        found(&extract_items("alpha — fertig — beta\n", &markers)),
        [("alpha —  — beta", 1, ItemState::Done, "fertig")]
    );
    // A line that is nothing but a marker still carries a status, so it keeps its row with empty
    // text rather than being dropped: `items.text` is NOT NULL, and an empty string is honest.
    assert_eq!(
        found(&extract_items("done\n- [x]\n", &markers)),
        [
            ("", 1, ItemState::Done, "done"),
            ("", 2, ItemState::Done, "[x]"),
        ]
    );
    // A blockquote `>` is prose, not a bullet: it stays in the text, and so does the double space,
    // because cutting the box out leaves its gap behind exactly like every other cut does.
    assert_eq!(
        found(&extract_items("> [ ] buy milk\n", &markers)),
        [(">  buy milk", 1, ItemState::Open, "[ ]")]
    );
}

#[test]
fn frontmatter_lines_are_not_items() {
    let markers = MarkerConfig::default();

    // A recognised frontmatter block is metadata, and nothing inside it claims a status about an
    // item: read as body text, `  - done` would be a bullet carrying the `done` keyword and
    // `status: erledigt` a Done row whose text is `status:`. The scan starts at the parser's own
    // body line (`crate::parse::body_start`), so exactly the one real item below survives.
    let note = "---\ntitle: Vendor\ntags:\n  - done\nstatus: erledigt\n---\n- [ ] buy milk\n";
    assert_eq!(
        found(&extract_items(note, &markers)),
        [("buy milk", 7, ItemState::Open, "[ ]")]
    );

    // Without a closing `---` there is no block at all, so the file stays body from line 1 and the
    // item is found where it was written — the parser's rule reused, not a second one.
    assert_eq!(
        found(&extract_items("---\n- [ ] buy milk\n", &markers)),
        [("buy milk", 2, ItemState::Open, "[ ]")]
    );
}

#[test]
fn checkbox_lookalikes_are_not_checkboxes() {
    let markers = MarkerConfig::default();

    // `[x](notes/a.md)` is a link whose text is one letter and `[[x]]` is a wikilink; neither is a
    // ticked box, and neither has a keyword either, so both lines yield nothing.
    assert!(extract_items("[x](notes/a.md)\n", &markers).is_empty());
    assert!(extract_items("[[x]] other note\n", &markers).is_empty());
    // A bare box on a standalone line is a checkbox: it needs no bullet to be an item.
    assert_eq!(
        found(&extract_items("status [ ] pending\n", &markers)),
        [("status  pending", 1, ItemState::Open, "[ ]")]
    );
    // The box followed straight by its text is still the box — CJK notes write `- [ ]買い物する`.
    assert_eq!(
        found(&extract_items("- [ ]買い物する\n", &markers)),
        [("買い物する", 1, ItemState::Open, "[ ]")]
    );
    // GFM's other box spellings are not boxes here: `[ x]`, `[  ]` and `[-]` match no checkbox and
    // no keyword, so those lines yield no row at all rather than a state nobody wrote.
    assert!(extract_items(
        "- [ x] buy milk\n- [  ] buy milk\n- [-] buy milk\n",
        &markers
    )
    .is_empty());
}

#[test]
fn state_names_are_the_schema_domain() {
    // `items.state` has a CHECK domain in `crate::db`, and these three spellings are the only
    // strings allowed into it; Task 9 writes what `as_str` returns.
    assert_eq!(
        [
            ItemState::Open.as_str(),
            ItemState::Done.as_str(),
            ItemState::Paused.as_str(),
        ],
        ["open", "done", "paused"]
    );
}

#[test]
fn line_numbers_survive_line_endings() {
    let markers = MarkerConfig::default();
    let lf = "- [ ] first\nzweite ✅\n";
    let crlf = "- [ ] first\r\nzweite ✅\r\n";

    // One trailing `\r` is trimmed for content only, by the same line model the parser uses, so an
    // LF and a CRLF copy of a note agree on text, marker and line number.
    assert_eq!(
        found(&extract_items(crlf, &markers)),
        [
            ("first", 1, ItemState::Open, "[ ]"),
            ("zweite", 2, ItemState::Done, "✅"),
        ]
    );
    assert_eq!(
        extract_items(crlf, &markers),
        extract_items(lf, &markers),
        "line endings must not change what a note yields"
    );
    // A last line with no terminator is a line like any other.
    assert_eq!(
        found(&extract_items("- [ ] only", &markers)),
        [("only", 1, ItemState::Open, "[ ]")]
    );
}
