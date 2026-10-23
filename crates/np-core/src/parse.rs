//! Markdown parse: the frontmatter subset, the title, the heading outline and `#tags`.
//!
//! Pure text in, structs out — no files, no database, no clock. Task 7 fills in links from
//! the same raw text, Task 8 items, so this module stays the single place that decides what
//! a note *is* before anything is stored about it.
//!
//! Two rules hold everything together:
//!
//! - **Lines are 1-based and end at `\n`**, with one trailing `\r` trimmed for content only
//!   (`lines`). Nothing here rewrites the input, and an LF and a CRLF copy of the same note
//!   produce identical headings, tags and line numbers — later tasks print those numbers into
//!   `INDEX.md`, so they may not depend on how the file happens to be stored.
//! - **A fenced block is not prose.** A line starting with ``` toggles fence state (a closing
//!   fence ends the region) and nothing inside counts as a heading, a tag, a word or a link.
//!   `prose_lines` is that decision and the only place it is written: [`crate::links`] iterates the
//!   same lines, so extraction can never see a link inside a region the outline ignored.

use std::collections::HashSet;
use std::sync::OnceLock;

use regex::Regex;

use crate::links::{extract_links, RawLink};

/// Which source supplied a note's title, so the catalog can say where it came from.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum TitleSource {
    /// A `title:` key inside the frontmatter block.
    Frontmatter,
    /// The first ATX heading with usable text.
    Heading,
    /// Neither of the above: [`parse_markdown`]'s `fallback_title` won, which is normally the
    /// file name without its extension. This is what a note with no title anywhere gets.
    #[default]
    Filename,
}

impl TitleSource {
    /// The `notes.title_source` spelling, so the word that reaches the database is written here and
    /// not at each call site — [`crate::scan`] stores it for every note it indexes.
    pub fn as_str(self) -> &'static str {
        match self {
            TitleSource::Frontmatter => "frontmatter",
            TitleSource::Heading => "heading",
            TitleSource::Filename => "filename",
        }
    }
}

/// One ATX heading of the outline.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Heading {
    /// Number of `#` markers, 1..=6.
    pub level: u8,
    /// Heading text with surrounding whitespace removed. May be empty (`##   `), which is why
    /// title selection skips such a heading instead of titling the note `""`.
    pub text: String,
    /// 1-based line number in the file.
    pub line: usize,
}

/// What one markdown note yields before anything is stored about it.
///
/// Built at exactly one place — the end of [`parse_markdown`] — so that Task 7's `links` field
/// is filled in by one constructor rather than by scattered struct literals.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct NoteDoc {
    /// Winning title, never empty unless the caller passed an empty `fallback_title`.
    pub title: String,
    /// Which source won it.
    pub title_source: TitleSource,
    /// Whether the file opened with a recognised frontmatter block.
    pub frontmatter: bool,
    /// Outline in document order, fenced blocks excluded.
    pub headings: Vec<Heading>,
    /// `#tags` from the frontmatter and the body: lowercased, unique, first-seen order kept.
    pub tags: Vec<String>,
    /// Whitespace-separated tokens of the body prose. Frontmatter is metadata and a fenced
    /// block is not prose, so neither counts — the same exclusion headings and tags get.
    pub words: usize,
    /// Every link the note contains, in reading order, from [`crate::links::extract_links`] over
    /// the same raw text. Unlike the three fields above it this includes the frontmatter block:
    /// a wikilink written in `related:` is prose the author meant.
    pub links: Vec<RawLink>,
}

/// Read one note's text.
///
/// `fallback_title` is the last-resort title (the caller normally passes the file name
/// without its extension) and is used verbatim, so an empty argument yields an empty title.
///
/// No input is rejected: an empty string, an unterminated frontmatter block and a 5 MB
/// single-line file all come back as a [`NoteDoc`]. The scan is one pass over the characters,
/// with both patterns compiled once per process.
pub fn parse_markdown(raw: &str, fallback_title: &str) -> NoteDoc {
    let head = read_frontmatter(raw);

    // Frontmatter tags come first: they are the author's own ordering.
    let mut tags = TagList::default();
    for tag in &head.tags {
        tags.push(tag);
    }

    let mut headings: Vec<Heading> = Vec::new();
    let mut words = 0usize;
    for (number, text) in prose_lines(raw, head.body_start) {
        if let Some(heading) = heading_at(text, number) {
            headings.push(heading);
        }
        collect_tags(text, &mut tags);
        words += text.split_whitespace().count();
    }

    let (title, title_source) = pick_title(&head, &headings, fallback_title);
    // The one `NoteDoc` literal, so no field can be filled by one caller and missed by another.
    NoteDoc {
        title,
        title_source,
        frontmatter: head.recognised,
        headings,
        tags: tags.into_tags(),
        words,
        links: extract_links(raw),
    }
}

/// The frontmatter block, if the file has one, and where its body begins.
struct Frontmatter {
    /// `title:` value with quotes stripped and then trimmed, so it is never blank. `None` when
    /// the key is absent or its value is empty — a blank value must fall through to the next
    /// source, which is why [`pick_title`] needs no emptiness check on this branch.
    title: Option<String>,
    /// `tags:` values as written; [`parse_markdown`] lowercases and deduplicates them.
    tags: Vec<String>,
    /// 1-based number of the first body line. Every line below it belongs to the block, so it
    /// is neither scanned for headings nor counted as words.
    body_start: usize,
    /// `true` only when the file's very first line is exactly `---` **and** a closing `---`
    /// line follows. Without the closing line the whole file is body.
    recognised: bool,
}

/// The hand-rolled YAML subset of spec §9: a scalar `title:` and `tags:` as either an inline
/// `[a, b]` list or an indented `- item` block. Every other key is ignored without error —
/// frontmatter may hold anything the note editor puts there, and none of it may fail a scan.
fn read_frontmatter(raw: &str) -> Frontmatter {
    let absent = || Frontmatter {
        title: None,
        tags: Vec::new(),
        body_start: 1,
        recognised: false,
    };

    let mut rest = lines(raw);
    if rest.next().map(|(_, text)| text) != Some("---") {
        return absent();
    }

    let mut head = absent();
    // `true` while the indented `- item` list under a bare `tags:` is still being read.
    let mut block_tags = false;
    for (number, text) in rest {
        if text == "---" {
            head.body_start = number + 1;
            head.recognised = true;
            return head;
        }
        if let Some(value) = text.strip_prefix("title:") {
            block_tags = false;
            if head.title.is_none() {
                head.title = scalar(value);
            }
            continue;
        }
        if let Some(value) = text.strip_prefix("tags:") {
            let value = value.trim();
            block_tags = value.is_empty();
            if !block_tags {
                head.tags.extend(list_values(value));
            }
            continue;
        }
        if block_tags && text.starts_with([' ', '\t']) {
            if let Some(tag) = list_item(text.trim()) {
                head.tags.push(tag);
            }
            continue;
        }
        // Any other key ends the tag list and is then ignored.
        block_tags = false;
    }

    // Unterminated: no block, and the body starts at line 1 so nothing is swallowed.
    absent()
}

/// Which source wins the title, in the plan's order: frontmatter, first usable heading,
/// fallback. An empty or whitespace-only value from any of them falls through to the next.
fn pick_title(
    head: &Frontmatter,
    headings: &[Heading],
    fallback_title: &str,
) -> (String, TitleSource) {
    if let Some(title) = &head.title {
        return (title.clone(), TitleSource::Frontmatter);
    }
    if let Some(heading) = headings.iter().find(|h| !h.text.trim().is_empty()) {
        return (heading.text.clone(), TitleSource::Heading);
    }
    (fallback_title.to_string(), TitleSource::Filename)
}

/// The file's lines, numbered from 1, with one trailing `\r` trimmed **for content only**.
///
/// `raw` is borrowed, never rewritten, so a caller can still hash or slice the original bytes.
fn lines(raw: &str) -> impl Iterator<Item = (usize, &str)> {
    raw.split('\n')
        .enumerate()
        .map(|(index, text)| (index + 1, text.strip_suffix('\r').unwrap_or(text)))
}

/// The lines that are prose: numbered from 1, `lines`-trimmed, starting at `first`, with every
/// fenced region **and both of its fence lines** left out.
///
/// This is the single place that decides what "inside a fenced block" means, shared with
/// [`crate::links`] so that headings, tags, words and links all stop at the same fence. `first` is
/// the first line eligible to be prose — [`parse_markdown`] passes the line after the frontmatter
/// block, [`crate::links::extract_links`] passes 1 because a link in frontmatter is still a link.
///
/// The state machine reads nothing but the start of a line, so it costs one `starts_with` per line
/// and never looks back.
pub(crate) fn prose_lines(raw: &str, first: usize) -> impl Iterator<Item = (usize, &str)> {
    let mut fenced = false;
    lines(raw).filter_map(move |(number, text)| {
        if number < first {
            return None;
        }
        if is_fence(text) {
            fenced = !fenced;
            return None;
        }
        (!fenced).then_some((number, text))
    })
}

/// A line that opens or closes a fenced code block.
fn is_fence(text: &str) -> bool {
    text.starts_with("```")
}

/// The ATX heading on this line, or nothing.
///
/// The pattern is the plan's `^(#{1,6})\\s+(.*)$`: seven hashes is not a heading, and neither
/// is `#word` — the marker run has to be followed by whitespace. Checking for `#` first keeps
/// prose lines out of the regex engine entirely.
fn heading_at(text: &str, number: usize) -> Option<Heading> {
    if !text.starts_with('#') {
        return None;
    }
    let caps = heading_pattern().captures(text)?;
    Some(Heading {
        level: caps[1].len() as u8,
        text: caps[2].trim().to_string(),
        line: number,
    })
}

/// Whether this line is an ATX heading, by the same test [`heading_at`] applies.
///
/// The shared decision [`crate::items`] uses to keep headings out of the item list: a heading names
/// a topic, so `## Erledigt` is an outline entry and not a done item. Checking for `#` first keeps
/// prose lines out of the regex engine here too.
pub(crate) fn is_heading_line(text: &str) -> bool {
    text.starts_with('#') && heading_pattern().is_match(text)
}

/// The first body line: 1 when the file has no recognised frontmatter block, otherwise the line
/// after its closing `---`.
///
/// The same `body_start` [`read_frontmatter`] already computes for [`parse_markdown`], exposed so
/// [`crate::items`] can keep metadata out of the item scan instead of re-implementing the block rule.
/// [`crate::links::extract_links`] deliberately does not use it: a link written in `related:` is
/// prose the author meant, while a status word written in frontmatter claims nothing about an item.
pub(crate) fn body_start(raw: &str) -> usize {
    read_frontmatter(raw).body_start
}

/// The `#tags` on this line, appended to `tags`.
///
/// The plan's pattern is `(?<![\\w/])#([\\p{L}][\\p{L}\\p{N}_/-]{1,40})`. Rust's `regex` crate
/// has no look-around (ruling P6), so the boundary is checked here from the preceding
/// character instead of in the pattern; [`tag_tail_pattern`] keeps the tail exactly as written
/// and the loop below keeps the position rules. The result is the same set of tags, plus one
/// deliberate addition: `[text](#section)` is an in-page anchor, which the plan's pattern
/// would accept as a tag but which the mandated behaviour excludes (`](` before the `#`).
fn collect_tags(text: &str, tags: &mut TagList) {
    let mut prev: Option<char> = None;
    for (index, ch) in text.char_indices() {
        if ch == '#' {
            let anchor = text[..index].ends_with("](");
            let joined = prev.is_some_and(blocked_by);
            if !anchor && !joined {
                // `#` is one byte, so `index + 1` is a char boundary even before a multi-byte
                // character such as the `ä` of `#äner-tag`.
                if let Some(tag) = tag_tail_pattern().find(&text[index + 1..]) {
                    tags.push(tag.as_str());
                }
            }
        }
        prev = Some(ch);
    }
}

/// Whether a character right before `#` means this is not a tag: a letter, digit or underscore
/// continues a word (`x#nothere`), and `/` makes the `#` an anchor inside a path
/// (`notes/a.md#section`, `tags/#vendor`).
///
/// The keyword-side counterpart [`crate::items::is_word_char`] looks almost the same and differs on
/// `/` deliberately: here a slash has to block, because it makes the `#` part of a path anchor;
/// there a slash sits between two markers (`status done/paused`) and must not block. The two stay
/// separate on purpose.
fn blocked_by(prev: char) -> bool {
    prev.is_alphanumeric() || prev == '_' || prev == '/'
}

/// A frontmatter scalar: one layer of matching quotes removed, then trimmed on both ends, and
/// `None` when nothing usable is left. Trimming happens **after** unquoting, so a quoted blank
/// (`title: "   "`) falls through to the next title source instead of becoming a title of three
/// spaces, and `title: "  Vendor  "` cannot put padded text into `INDEX.md`.
fn scalar(value: &str) -> Option<String> {
    let value = unquote(value.trim()).trim();
    (!value.is_empty()).then(|| value.to_string())
}

/// One `- item` line of a block `tags:` list. The marker is `-` plus whitespace (or nothing),
/// so an indented continuation line or a `--something` is not an item and is ignored.
fn list_item(line: &str) -> Option<String> {
    let rest = line.strip_prefix('-')?;
    if rest.is_empty() || rest.starts_with([' ', '\t']) {
        scalar(rest)
    } else {
        None
    }
}

/// The values of a `tags:` line written inline: `[a, b]`, and also a plain `a` or `a, b`.
fn list_values(value: &str) -> Vec<String> {
    let inner = value.strip_prefix('[').unwrap_or(value);
    let inner = inner.strip_suffix(']').unwrap_or(inner);
    inner.split(',').filter_map(scalar).collect()
}

/// One layer of matching `'` or `"` quotes removed from a trimmed scalar.
fn unquote(value: &str) -> &str {
    let bytes = value.as_bytes();
    let quoted = bytes.len() >= 2
        && (bytes[0] == b'"' || bytes[0] == b'\'')
        && bytes.last() == Some(&bytes[0]);
    if quoted {
        // Both ends are ASCII quotes, so these offsets are char boundaries.
        &value[1..value.len() - 1]
    } else {
        value
    }
}

/// Tags in first-seen order, lowercased and deduplicated — the three rules applied in one
/// place so frontmatter tags and body tags behave identically.
#[derive(Default)]
struct TagList {
    order: Vec<String>,
    seen: HashSet<String>,
}

impl TagList {
    fn push(&mut self, tag: &str) {
        let folded = tag.to_lowercase();
        if self.seen.insert(folded.clone()) {
            self.order.push(folded);
        }
    }

    fn into_tags(self) -> Vec<String> {
        self.order
    }
}

/// `^(#{1,6})\\s+(.*)$`, compiled once per process — a pattern must never be rebuilt inside the
/// line loop.
fn heading_pattern() -> &'static Regex {
    static PATTERN: OnceLock<Regex> = OnceLock::new();
    PATTERN.get_or_init(|| Regex::new(r"^(#{1,6})\s+(.*)$").expect("heading pattern compiles"))
}

/// The tail of the plan's tag pattern, `\\A[\\p{L}][\\p{L}\\p{N}_/-]{1,40}`, compiled once.
/// Anchored at the start of what follows a `#`, so it is a bounded check per candidate and a
/// greedy read of at most 41 characters in total — which is why `#a` is not a tag but a 60
/// character run yields its first 41.
fn tag_tail_pattern() -> &'static Regex {
    static PATTERN: OnceLock<Regex> = OnceLock::new();
    PATTERN.get_or_init(|| {
        Regex::new(r"\A[\p{L}][\p{L}\p{N}_/-]{1,40}").expect("tag tail pattern compiles")
    })
}
