//! Markdown parse: frontmatter subset, title precedence, headings, tags.
//!
//! The notes under `tests/fixtures/notes` are read as real files so the fixture tree stays
//! reviewable as markdown. The path is resolved through `CARGO_MANIFEST_DIR` because an
//! integration test runs with the package directory as its cwd while the fixtures sit at the
//! workspace root — the same tree Task 10 adds `tests/fixtures/corpus` to.

use std::fs;
use std::path::Path;
use std::time::Instant;

use np_core::parse::{parse_markdown, Heading, TitleSource};

/// Contents of one fixture note, byte for byte as stored. Four fixtures are stored with `\n`
/// and `german-note.md` with `\r\n`; `crlf_matches_lf` normalises whatever it gets into both
/// forms before comparing, so the assertions hold whichever way a checkout laid them down.
fn fixture(name: &str) -> String {
    let path = Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("../../tests/fixtures/notes")
        .join(name);
    fs::read_to_string(&path).unwrap_or_else(|err| panic!("cannot read {}: {err}", path.display()))
}

/// The comparable shape of a heading list, so an assertion failure prints the whole outline.
fn outline(headings: &[Heading]) -> Vec<(u8, &str, usize)> {
    headings
        .iter()
        .map(|h| (h.level, h.text.as_str(), h.line))
        .collect()
}

/// The same LF text with `\r\n` on every other line — a file written by two different editors.
/// A reader that trimmed a `\r` only once per file, or only at the first line, would disagree
/// with the pure LF reading here.
fn mixed_endings(lf: &str) -> String {
    let mut mixed = String::with_capacity(lf.len() + lf.len() / 10);
    for (index, line) in lf.split('\n').enumerate() {
        if index > 0 {
            mixed.push_str(if index % 2 == 0 { "\r\n" } else { "\n" });
        }
        mixed.push_str(line);
    }
    mixed
}

/// The same text with its final line terminator removed — a note saved without a trailing
/// newline, which is how most editors write the last line.
fn no_final_newline(text: &str) -> String {
    text.strip_suffix("\r\n")
        .or_else(|| text.strip_suffix('\n'))
        .unwrap_or(text)
        .to_string()
}

#[test]
fn title_precedence() {
    // Frontmatter `title:` beats the first ATX heading, which beats the fallback.
    let note = parse_markdown(&fixture("frontmatter-note.md"), "fallback-name");
    assert_eq!(note.title, "Vendor Contract");
    assert_eq!(note.title_source, TitleSource::Frontmatter);
    assert_eq!(
        outline(&note.headings),
        [(1, "Ignored heading", 10)],
        "the heading is still recorded even though it lost the title contest"
    );

    let note = parse_markdown(&fixture("release-note.md"), "fallback-name");
    assert_eq!(note.title, "Release Note");
    assert_eq!(note.title_source, TitleSource::Heading);

    // No frontmatter and no heading: the caller's fallback stands.
    let note = parse_markdown(&fixture("prose-only.md"), "fallback-name");
    assert_eq!(note.title, "fallback-name");
    assert_eq!(note.title_source, TitleSource::Filename);

    // An empty or whitespace-only title value falls through instead of yielding "". The closing
    // `---` matters here: without it the block would be unterminated, and the heading would win
    // for that unrelated reason, so the assertion would prove nothing.
    let note = parse_markdown(
        "---\ntitle: \"   \"\n---\n# From Heading\n",
        "fallback-name",
    );
    assert_eq!(note.title, "From Heading");
    assert_eq!(note.title_source, TitleSource::Heading);

    let note = parse_markdown("---\ntitle:\n---\n# From Heading\n", "fallback-name");
    assert_eq!(note.title, "From Heading");
    assert_eq!(note.title_source, TitleSource::Heading);

    // Padding inside the quotes is not part of the title — it would be printed into INDEX.md.
    let note = parse_markdown(
        "---\ntitle: \"  Vendor  \"\n---\n# From Heading\n",
        "fallback-name",
    );
    assert_eq!(note.title, "Vendor");
    assert_eq!(note.title_source, TitleSource::Frontmatter);

    // A heading with no text is an empty title too, so the fallback wins over it.
    let note = parse_markdown("#   \nbody text\n", "fallback-name");
    assert_eq!(note.title, "fallback-name");
    assert_eq!(note.title_source, TitleSource::Filename);

    // Only the *first* usable heading supplies the title.
    let note = parse_markdown("# First\n# Second\n", "fallback-name");
    assert_eq!(note.title, "First");
}

#[test]
fn frontmatter_recognised_subset() {
    // Inline `tags: [a, b]`, quoted scalar title, unknown keys ignored without error.
    let note = parse_markdown(&fixture("frontmatter-note.md"), "fallback-name");
    assert!(note.frontmatter, "the file opens with an exact --- line");
    assert_eq!(note.tags, ["vendor", "beta", "gamma"]);
    assert!(
        !note.tags.iter().any(|t| t == "vc"),
        "`aliases:` is not `tags:`, got {:?}",
        note.tags
    );

    // A block `tags:` list, with quotes stripped from its items.
    let note = parse_markdown(&fixture("block-tags-note.md"), "fallback-name");
    assert!(note.frontmatter);
    assert_eq!(note.title, "Block Tags Note");
    assert_eq!(note.tags, ["vendor", "alpha", "delta"]);

    // A block list item needs a `-` marker plus whitespace: an indented continuation line or
    // a `--nope` line is neither a tag nor an error.
    let note = parse_markdown("---\ntags:\n  - real\n  --nope\n  more\n---\nbody\n", "fb");
    assert!(note.frontmatter);
    assert_eq!(note.tags, ["real"]);

    // Padding inside quotes is stripped from tag items too, so a stored tag never carries it.
    let note = parse_markdown("---\ntags: [ \"  sp  \", b ]\n---\nbody\n", "fb");
    assert_eq!(note.tags, ["sp", "b"]);
    let note = parse_markdown("---\ntags:\n  - '  sp  '\n---\nbody\n", "fb");
    assert_eq!(note.tags, ["sp"]);

    // No frontmatter anywhere: the first line is prose, not `---`.
    for name in ["release-note.md", "prose-only.md"] {
        let note = parse_markdown(&fixture(name), "fallback-name");
        assert!(!note.frontmatter, "{name} has no frontmatter block");
    }

    // The opening line must be exactly `---`; a longer rule or an indented one is prose.
    for opener in ["----", " ---", "--", "***"] {
        let raw = format!("{opener}\ntitle: x\n---\n# Head\n");
        let note = parse_markdown(&raw, "fallback-name");
        assert!(!note.frontmatter, "{opener:?} must not open frontmatter");
    }

    // Inside the block nothing counts as body work: neither a heading nor a tag.
    let note = parse_markdown(
        "---\n# not a heading\nsee #inside\n---\n# real\n",
        "fallback-name",
    );
    assert!(note.frontmatter);
    assert_eq!(outline(&note.headings), [(1, "real", 5)]);
    assert_eq!(note.tags, Vec::<String>::new());

    // An empty frontmatter block is still a block, and the body after it parses normally.
    let note = parse_markdown("---\n---\n# Head #tagged\n", "fallback-name");
    assert!(note.frontmatter);
    assert_eq!(outline(&note.headings), [(1, "Head #tagged", 3)]);
    assert_eq!(note.tags, ["tagged"]);
}

#[test]
fn headings_with_line_numbers() {
    // The `# fake` line inside the fenced block yields no heading and no tag.
    let note = parse_markdown(&fixture("release-note.md"), "fallback-name");
    assert_eq!(
        outline(&note.headings),
        [(1, "Release Note", 1), (2, "Details", 6)],
        "line numbers are 1-based and the fenced `# fake` is skipped"
    );
    assert!(
        !note.tags.iter().any(|t| t == "faketag"),
        "a tag inside a fence is not a tag, got {:?}",
        note.tags
    );

    // A closing fence ends the region, so parsing resumes after it.
    let note = parse_markdown(
        "# one\n```\n# fake\n#faketag\n```\n# two\n",
        "fallback-name",
    );
    assert_eq!(outline(&note.headings), [(1, "one", 1), (1, "two", 6)]);
    assert_eq!(note.tags, Vec::<String>::new());

    // Levels come from the marker run; 7 hashes is not a heading, and neither is `#word`.
    let note = parse_markdown("###### six\n####### seven\n#nospace\n", "fallback-name");
    assert_eq!(outline(&note.headings), [(6, "six", 1)]);

    // Trailing whitespace is not part of the text.
    let note = parse_markdown("## Spaced   \n", "fallback-name");
    assert_eq!(outline(&note.headings), [(2, "Spaced", 1)]);
}

#[test]
fn tags_from_body_hash_syntax() {
    // `#vendor #contract` in prose: lowercased, deduplicated, first-seen order kept. The
    // anchor-only link `[see](#anchor)` is not a tag, nor is a path anchor or `x#nothere`.
    let note = parse_markdown(&fixture("release-note.md"), "fallback-name");
    assert_eq!(note.tags, ["vendor", "contract", "beta-2"]);

    // A heading line's own markers are not a tag; tags inside prose still count.
    let note = parse_markdown("# Title\nSee #Rust and #rust again.\n", "fallback-name");
    assert_eq!(note.tags, ["rust"]);

    // First-seen order survives deduplication.
    let note = parse_markdown("one #bb two #aa three #bb\n", "fallback-name");
    assert_eq!(note.tags, ["bb", "aa"]);

    // The plan's `{1,40}` tail means a one-letter tag is not a tag.
    let note = parse_markdown("one #a two\n", "fallback-name");
    assert_eq!(note.tags, Vec::<String>::new());

    // Tag characters follow the plan's classes: letters, digits, `_`, `-`, `/`.
    let note = parse_markdown("#C++ and #ok_tag and #a/b-c end\n", "fallback-name");
    assert_eq!(note.tags, ["ok_tag", "a/b-c"]);

    // A 41-character tag is truncated at the pattern's limit rather than dropped.
    let long = "a".repeat(60);
    let note = parse_markdown(&format!("text #{long} end\n"), "fallback-name");
    assert_eq!(note.tags, [long[..41].to_string()]);

    // Tags are collected from every body line, including a heading's own text.
    let note = parse_markdown("## About #rust\n", "fallback-name");
    assert_eq!(outline(&note.headings), [(2, "About #rust", 1)]);
    assert_eq!(note.tags, ["rust"]);

    // The `/` half of the boundary rule: a `#` after a path separator is an anchor in a path,
    // in either spelling of "path".
    assert_eq!(
        parse_markdown("see notes/a.md#section and tags/#vendor end\n", "fb").tags,
        Vec::<String>::new()
    );

    // Only `](` right before the `#` is excluded, not every `(`: a parenthesised tag is still a
    // tag. Known limit of that narrow rule — HTML like an `href` attribute holding `#frag`
    // right after an equals sign still yields a tag.
    assert_eq!(parse_markdown("text (#rust) more\n", "fb").tags, ["rust"]);
}

#[test]
fn crlf_matches_lf() {
    for name in [
        "release-note.md",
        "frontmatter-note.md",
        "block-tags-note.md",
        "prose-only.md",
        "german-note.md",
    ] {
        // Normalise whatever the checkout stored into the two canonical forms.
        let lf = fixture(name).replace("\r\n", "\n");
        let crlf = lf.replace('\n', "\r\n");
        assert!(crlf.contains("\r\n"), "{name} needs a CRLF variant");

        // The `\r` is trimmed for content only, so every derived value matches the LF text.
        let unix = parse_markdown(&lf, "fallback-name");
        let dos = parse_markdown(&crlf, "fallback-name");
        assert_eq!(unix.headings, dos.headings, "{name}: heading lines differ");
        assert_eq!(unix.tags, dos.tags, "{name}: tags differ");
        assert_eq!(unix.words, dos.words, "{name}: word count differs");
        assert_eq!(unix, dos, "{name}: the whole document differs");

        // One file may mix both endings: the trim happens per line, not once per file.
        let mixed = mixed_endings(&lf);
        if lf.matches('\n').count() >= 2 {
            assert!(
                mixed.contains("\r\n") && mixed.replace("\r\n", "").contains('\n'),
                "{name}: the mixed variant came out uniform"
            );
        }
        assert_eq!(
            unix,
            parse_markdown(&mixed, "fallback-name"),
            "{name}: mixed endings differ"
        );

        // And the last line may have no terminator at all, in either spelling.
        let trimmed = no_final_newline(&lf);
        assert_ne!(trimmed, lf, "{name} must end with a newline");
        assert_eq!(
            unix,
            parse_markdown(&trimmed, "fallback-name"),
            "{name}: a missing final newline changed the document"
        );
        assert_eq!(
            unix,
            parse_markdown(&no_final_newline(&crlf), "fallback-name"),
            "{name}: CRLF without a final newline differs"
        );
    }

    // A lone trailing `\r` is not part of a heading text either.
    let note = parse_markdown("# Head\r\nbody\r\n", "fallback-name");
    assert_eq!(outline(&note.headings), [(1, "Head", 1)]);
}

#[test]
fn non_ascii_title_headings_and_tags() {
    // `german-note.md` is stored with CRLF and non-ASCII text: nothing in the tag scan may
    // assume one byte per character, because it slices around every `#` by byte offset.
    let stored = fixture("german-note.md");
    assert!(stored.contains('ä'), "the fixture must hold real umlauts");
    let lf = stored.replace("\r\n", "\n");
    let note = parse_markdown(&lf, "fallback-name");

    assert_eq!(note.title, "Händler Vertrag");
    assert_eq!(note.title_source, TitleSource::Heading);
    assert_eq!(
        outline(&note.headings),
        [(1, "Händler Vertrag", 1), (2, "Einzelheiten", 6)],
        "the fenced `# fake Überschrift` yields no heading"
    );
    // `#Vertrag` twice deduplicates after lowercasing, an umlaut directly after the `#` is a
    // normal tag character, and neither `ü#nicht` nor a path anchor becomes a tag.
    assert_eq!(note.tags, ["vertrag", "äner-tag"]);

    // The same note with CRLF everywhere parses to the identical document.
    assert_eq!(
        note,
        parse_markdown(&lf.replace('\n', "\r\n"), "fallback-name"),
        "the umlaut note differs between line endings"
    );
}

#[test]
fn degenerate_inputs() {
    // Empty input: nothing to derive, so the fallback title stands.
    let note = parse_markdown("", "empty-note");
    assert_eq!(note.title, "empty-note");
    assert_eq!(note.title_source, TitleSource::Filename);
    assert!(!note.frontmatter);
    assert!(note.headings.is_empty());
    assert!(note.tags.is_empty());
    assert_eq!(note.words, 0);

    // Unterminated frontmatter is not frontmatter at all: the `---` line and everything
    // after it is body, so its headings and tags survive.
    let note = parse_markdown("---\ntitle: x", "unterminated");
    assert!(!note.frontmatter);
    assert_eq!(note.title, "unterminated");
    assert_eq!(note.title_source, TitleSource::Filename);

    let note = parse_markdown("---\ntitle: x\n# Real Heading\nbody #tag\n", "unterminated");
    assert!(!note.frontmatter);
    assert_eq!(outline(&note.headings), [(1, "Real Heading", 3)]);
    assert_eq!(note.tags, ["tag"]);
    assert_eq!(note.title, "Real Heading");
    assert_eq!(note.title_source, TitleSource::Heading);

    // A terminated frontmatter-only file: the body would start past the last line.
    let note = parse_markdown("---\ntitle: x\n---", "only-frontmatter");
    assert!(note.frontmatter);
    assert_eq!(note.title, "x");
    assert_eq!(note.title_source, TitleSource::Frontmatter);
    assert!(note.headings.is_empty());
    assert!(note.tags.is_empty());
    assert_eq!(note.words, 0);

    // A file that is only an opening `---`, and one that is only newlines.
    let note = parse_markdown("---", "just-rule");
    assert!(!note.frontmatter);
    let note = parse_markdown("\n\n\r\n", "blank-lines");
    assert_eq!(note.title, "blank-lines");
    assert_eq!(note.words, 0);

    // A 5 MB single line must come back in linear time, not quadratic: one pass over the
    // characters, no per-character string building. The bound is generous on purpose.
    let repeats = 5 * 1024 * 1024 / "word ".len();
    let big = format!("start #alpha {}#beta", "word ".repeat(repeats));
    assert!(big.len() >= 5 * 1024 * 1024, "fixture is {}", big.len());

    let started = Instant::now();
    let note = parse_markdown(&big, "huge-line");
    let took = started.elapsed();

    assert_eq!(note.title, "huge-line");
    assert!(note.headings.is_empty());
    assert_eq!(note.tags, ["alpha", "beta"]);
    assert_eq!(note.words, repeats + 3);
    assert!(
        took.as_secs() < 10,
        "parsing a 5 MB line took {took:?}; the scan must stay linear"
    );
}

#[test]
fn words_counted_over_body_only() {
    // Words are whitespace-separated tokens of the body prose: frontmatter is metadata and a
    // fenced block is not prose, so neither counts — the same exclusion headings and tags get.
    assert_eq!(parse_markdown("one two\nthree", "fb").words, 3);
    assert_eq!(
        parse_markdown("---\ntitle: A B C D E F\n---\none two\n", "fb").words,
        2
    );
    assert_eq!(parse_markdown("```\nalpha beta\n```\ngamma", "fb").words, 1);
    assert_eq!(parse_markdown("   \n\t\n", "fb").words, 0);
}
