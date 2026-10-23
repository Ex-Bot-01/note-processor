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

/// Contents of one fixture note, with the platform's own line endings left untouched
/// (the fixtures are stored with `\n`, which is what the CRLF test transforms).
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

    // An empty or whitespace-only title value falls through instead of yielding "".
    let note = parse_markdown("---\ntitle: \"   \"\n# From Heading\n", "fallback-name");
    assert_eq!(note.title, "From Heading");
    assert_eq!(note.title_source, TitleSource::Heading);

    let note = parse_markdown("---\ntitle:\n# From Heading\n", "fallback-name");
    assert_eq!(note.title, "From Heading");
    assert_eq!(note.title_source, TitleSource::Heading);

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
}

#[test]
fn crlf_matches_lf() {
    for name in [
        "release-note.md",
        "frontmatter-note.md",
        "block-tags-note.md",
        "prose-only.md",
    ] {
        let lf = fixture(name);
        let crlf = lf.replace('\n', "\r\n");
        assert!(crlf.contains("\r\n"), "{name} needs a CRLF variant");

        // The `\r` is trimmed for content only, so every derived value matches the LF file.
        let unix = parse_markdown(&lf, "fallback-name");
        let dos = parse_markdown(&crlf, "fallback-name");
        assert_eq!(unix.headings, dos.headings, "{name}: heading lines differ");
        assert_eq!(unix.tags, dos.tags, "{name}: tags differ");
        assert_eq!(unix.words, dos.words, "{name}: word count differs");
        assert_eq!(unix, dos, "{name}: the whole document differs");
    }

    // A lone trailing `\r` is not part of a heading text either.
    let note = parse_markdown("# Head\r\nbody\r\n", "fallback-name");
    assert_eq!(outline(&note.headings), [(1, "Head", 1)]);
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
