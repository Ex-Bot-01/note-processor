//! Every link a note contains, and where in the corpus it lands.
//!
//! Three steps, three functions: [`extract_links`] turns note text into [`RawLink`]s, [`resolve`]
//! places each target in the corpus without ever guessing, and [`store_links`] writes the rows
//! Task 9 counts as `→out ←in` and Task 10 reports as `unresolved_links`.
//!
//! Two rules hold the module together.
//!
//! - **A fenced block holds no links.** The lines scanned come from the same iteration that decides
//!   headings, tags and words (`crate::parse::prose_lines`), so this module cannot have its own
//!   opinion about where prose ends. A link written inside a code sample is an example, not a claim
//!   about the corpus.
//! - **A target that cannot be placed stays unresolved.** Ambiguity is never resolved by picking,
//!   because a wrong edge in a note graph is worse than a missing one and is much harder to spot.

use std::collections::{BTreeMap, HashMap};

use rusqlite::params;

use crate::db::{Db, DbError};
use crate::parse::prose_lines;

/// The only extension that names a note in M1. `.markdown` deliberately is not one: the walker,
/// the parser and this module recognise one spelling, so a second would silently split every
/// corpus into notes and unparseable files.
const MARKDOWN_EXTENSION: &str = "md";

/// Extensions that name an attachment rather than a note — a file that is linked to but never
/// parsed. Anything else with an extension is put in the same bucket (see [`extract_links`]), so
/// this list only has to grow when a milestone needs a kind of attachment treated specially.
const ATTACHMENT_EXTENSIONS: [&str; 7] = ["png", "jpg", "jpeg", "gif", "webp", "bmp", "pdf"];

/// Which syntax pointed at a target, and whether that target is a note.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum LinkKind {
    /// `[[target]]` or its embed form `![[target]]`, whose extension is not an attachment's.
    Wiki,
    /// `[text](destination)` ending in `.md`.
    Md,
    /// A file that is not a note: an image, a PDF, or any other extension.
    Attachment,
}

impl LinkKind {
    /// The `links.kind` spelling, which is inside the schema's CHECK domain and nowhere else.
    pub fn as_str(self) -> &'static str {
        match self {
            LinkKind::Wiki => "wiki",
            LinkKind::Md => "md",
            LinkKind::Attachment => "attachment",
        }
    }
}

/// One link as it was written, before anything is looked up.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RawLink {
    /// What to look for in the corpus: stripped of `|alias`, `#heading` and `^block`, trimmed, in
    /// the author's own case (the resolver case-folds when it has to), `/`-separated, and **not**
    /// percent-decoded.
    pub target: String,
    /// Which form it was written in, decided by the extension.
    pub kind: LinkKind,
    /// 1-based line number in the file — the same number for an LF and a CRLF copy of the note,
    /// because the shared line model trims a trailing `\r` for content only.
    pub line: usize,
}

/// Every link a note's text contains: line by line, and within a line in the order the scan reaches
/// them. That is reading order except at one place — a link nested inside another link's text
/// (`[![badge](img/i.png)](https://x)`) is reported after the destination that surrounds it, because
/// the outer link is only known to be a link once its `)` is read. Both rows carry the same line
/// number and nothing downstream orders by anything finer than that.
///
/// Two syntaxes are links: `[[target]]` with its embed form `![[target]]`, and
/// `[text](destination)`. The kind follows the extension of the target and nothing else: an
/// attachment extension is [`LinkKind::Attachment`], `.md` in a markdown destination is
/// [`LinkKind::Md`], and a wikilink whose target has no attachment extension is
/// [`LinkKind::Wiki`] — which is why `![[scan.png]]` is an attachment and `[[Note A]]` is a note.
///
/// Four cases produce **no row at all**, each because there is nothing in the corpus to point at:
///
/// - anything inside a ``` fenced region, including the fence lines themselves;
/// - a destination with no extension (`[text](relative-path)`), and a target left empty by the
///   strips (`[[]]`, `[x](#anchor)`, `[x]()`);
/// - a destination that leaves the workspace: anything with a scheme before its first `:`
///   (`https:`, `mailto:`, and a Windows drive letter, which is indistinguishable from one), a
///   protocol-relative `//host/path`, or a bare `#section` anchor.
///
/// Those skipped destinations are not counted as unresolved either — [`crate::db::NoteCounts`]
/// numbers are about *missing notes*, and external URLs would break that meaning. A markdown
/// destination with any other extension (`archive.zip`, `page.html`) is kept as an
/// [`LinkKind::Attachment`]: the link is evidence of a real file, and the non-note bucket is where
/// such a file belongs.
///
/// Two limits are deliberate: destinations are not percent-decoded, so `[x](a%20b.md)` only
/// resolves if a file is literally named that; and the whole text is scanned, frontmatter included
/// — this function sees text and not a note, and a wikilink in `related:` is one the author meant.
pub fn extract_links(raw: &str) -> Vec<RawLink> {
    let mut found: Vec<RawLink> = Vec::new();
    for (number, text) in prose_lines(raw, 1) {
        scan_line(text, number, &mut found);
    }
    found
}

/// The links on one prose line, appended in the order the scan reaches them — see
/// [`MarkdownLink::resume`] for the one case where that is not strictly left to right.
///
/// Only ASCII bytes are ever tested or sliced at, so `cursor` is always on a character boundary
/// even when the byte at it is the middle of a multi-byte character — that byte cannot be `[`, and
/// the `starts_with` below is reached only for a real bracket.
///
/// Every per-line decision is taken once, before the loop, because this runs on every note and an
/// unfenced paste of minified JSON or code is one long line of brackets: asking each `[` whether
/// anything closes it would make the line quadratic. `]]`, `](` and `)` are each answered by one
/// search over the line, and a wikilink search that comes up empty ends the search for the whole
/// line — nothing further along can close either.
fn scan_line(text: &str, number: usize, out: &mut Vec<RawLink>) {
    let bytes = text.as_bytes();
    let mut wikilinks_left = text.contains("]]");
    let markdown_links_left = if text.contains("](") && text.contains(')') {
        closed_brackets(text)
    } else {
        HashMap::new()
    };
    let mut cursor = 0usize;
    while cursor < bytes.len() {
        match bytes[cursor] {
            b'[' if wikilinks_left && text[cursor..].starts_with("[[") => {
                // `![[target]]` arrives here as well, two characters later than the `!`: the kind
                // comes from the extension, so the embed marker needs no case of its own.
                let inner = cursor + 2;
                match find(text, "]]", inner) {
                    Some(end) => {
                        push(out, wikilink(&text[inner..end]), number);
                        cursor = end + 2;
                    }
                    // Nothing closes a wikilink between here and the end of the line, so nothing
                    // further along will either. The second bracket still gets its turn at the
                    // markdown syntax on the next character.
                    None => {
                        wikilinks_left = false;
                        cursor += 1;
                    }
                }
            }
            b'[' => {
                match markdown_link(text, cursor, &markdown_links_left) {
                    Some(link) => {
                        push(out, markdown_target(link.destination), number);
                        cursor = link.resume;
                    }
                    // No closing bracket, or no `(` after it: the brackets inside get their own
                    // turn on the next character.
                    None => cursor += 1,
                }
            }
            _ => cursor += 1,
        }
    }
}

/// Each `[` of a line and the `]` that closes it, decided for the whole line in one pass.
///
/// One pass is the point: looking for the close of each bracket separately rescans the tail of the
/// line every time, which turns a line full of unbalanced brackets into quadratic work. A `[` with
/// no entry here never closes, so it opens no link; a `]` with no opener before it is ignored.
fn closed_brackets(text: &str) -> HashMap<usize, usize> {
    let mut openers: Vec<usize> = Vec::new();
    let mut closes: HashMap<usize, usize> = HashMap::new();
    for (index, byte) in text.bytes().enumerate() {
        match byte {
            b'[' => openers.push(index),
            b']' => {
                if let Some(opener) = openers.pop() {
                    closes.insert(opener, index);
                }
            }
            _ => {}
        }
    }
    closes
}

/// Adds what one link syntax yielded, if it yielded anything.
fn push(out: &mut Vec<RawLink>, link: Option<(String, LinkKind)>, line: usize) {
    if let Some((target, kind)) = link {
        out.push(RawLink { target, kind, line });
    }
}

/// The byte index at which `needle` starts, searching from `from`. Every offset this scanner keeps
/// is a byte index into the same `&str`, so they stay comparable.
fn find(text: &str, needle: &str, from: usize) -> Option<usize> {
    text.get(from..)?.find(needle).map(|offset| from + offset)
}

/// A `[text](destination)` that starts at `open`.
struct MarkdownLink<'a> {
    /// The text between `(` and the first `)`.
    destination: &'a str,
    /// Where the line scan continues: the first `[` inside the link text when there is one — so the
    /// image in `[![badge](img/i.png)](https://shields.io)` is still seen even though its own
    /// destination leaves the corpus — and otherwise just past the closing `)`, because a
    /// destination holds no note syntax worth rescanning.
    resume: usize,
}

/// The markdown link at `open`, which has to be a `[` that [`closed_brackets`] matched.
fn markdown_link<'a>(
    text: &'a str,
    open: usize,
    closes: &HashMap<usize, usize>,
) -> Option<MarkdownLink<'a>> {
    // A nested pair (`[see [x]](a.md)`) ends the link text at the bracket that matches the opener,
    // not at the first `]`.
    let close = *closes.get(&open)?;
    // A link needs `(` right after its own closing bracket; `[a [b] c` and `[a] b.md` are prose.
    if text.as_bytes().get(close + 1) != Some(&b'(') {
        return None;
    }
    let destination_start = close + 2;
    let end = find(text, ")", destination_start)?;
    // Both offsets are ASCII brackets, so this slice is on character boundaries.
    let inner = text[open + 1..close]
        .find('[')
        .map(|offset| open + 1 + offset);
    Some(MarkdownLink {
        destination: &text[destination_start..end],
        resume: inner.unwrap_or(end + 1),
    })
}

/// The target and kind of the text between `[[` and `]]`.
fn wikilink(inner: &str) -> Option<(String, LinkKind)> {
    // An alias follows `|`, a heading `#` and a block reference `^`; all three are about how the
    // link is shown or where inside the target it lands, not about which file it is.
    let written = inner.split('|').next().unwrap_or(inner);
    let target = match written.find(['#', '^']) {
        Some(cut) => &written[..cut],
        None => written,
    };
    let target = target.trim();
    if target.is_empty() {
        return None;
    }
    let kind = match extension(target) {
        Some(extension) if is_attachment(extension) => LinkKind::Attachment,
        _ => LinkKind::Wiki,
    };
    Some((target.to_string(), kind))
}

/// The target and kind of a markdown destination, or nothing when it names no file of the corpus.
fn markdown_target(destination: &str) -> Option<(String, LinkKind)> {
    let written = strip_title(destination.trim());
    // An empty destination, an in-page anchor and a protocol-relative URL are not workspace paths,
    // and neither is anything carrying a scheme.
    if written.is_empty()
        || written.starts_with('#')
        || written.starts_with("//")
        || has_scheme(written)
    {
        return None;
    }
    // A destination written for Windows is the same path.
    let mut path = written.replace('\\', "/");
    // A fragment or a query points inside the file rather than at a different one, so it is cut
    // before the lookup — while percent-escapes stay exactly as written (no decoder in M1).
    for marker in ['#', '?'] {
        if let Some(cut) = path.find(marker) {
            path.truncate(cut);
        }
    }
    let path = path.trim();
    // `./` and a leading `/` both mean the workspace root, which every `rel` is already relative
    // to. A `..` is left alone rather than resolved: it names nothing in the registry, and
    // inventing a normalisation rule here would be one more place to keep in step with `walk`.
    let path = path.strip_prefix("./").unwrap_or(path);
    let path = path.strip_prefix('/').unwrap_or(path);
    if path.is_empty() {
        return None;
    }
    // No extension is not a file the corpus can hold, so the link points nowhere storable.
    let extension = extension(path)?;
    let kind = if extension.eq_ignore_ascii_case(MARKDOWN_EXTENSION) {
        LinkKind::Md
    } else {
        LinkKind::Attachment
    };
    Some((path.to_string(), kind))
}

/// A markdown title attribute cut off the destination: `[t](notes/a.md "Release")` is a link to
/// `notes/a.md` with a tooltip, and pandoc and GitHub write them constantly.
///
/// The title has to be quoted and separated from the path by whitespace, which is what keeps a path
/// that merely contains a space — `[t](my photo.png)` — intact. A destination that opens with a
/// quote but never separates one (`[t]("a b")`) is left alone; it names no file either way.
fn strip_title(destination: &str) -> &str {
    if !destination.ends_with('"') {
        return destination;
    }
    match destination.rfind(" \"") {
        Some(cut) => destination[..cut].trim_end(),
        None => destination,
    }
}

/// Whether a destination carries a scheme — `https:`, `mailto:`, and also a Windows drive letter,
/// which looks exactly like one and is equally outside the workspace.
fn has_scheme(destination: &str) -> bool {
    let Some(colon) = destination.find(':') else {
        return false;
    };
    let mut bytes = destination[..colon].bytes();
    let Some(first) = bytes.next() else {
        return false;
    };
    first.is_ascii_alphabetic()
        && bytes.all(|byte| byte.is_ascii_alphanumeric() || matches!(byte, b'+' | b'.' | b'-'))
}

/// The part of a target after the last `.` in its file name, or nothing when it has no such dot.
/// A leading dot names a hidden file (`.gitignore`) rather than an extension.
fn extension(target: &str) -> Option<&str> {
    let name = file_name(target);
    let dot = name.rfind('.').filter(|dot| *dot > 0)?;
    let extension = &name[dot + 1..];
    (!extension.is_empty()).then_some(extension)
}

/// The last `/`-separated component of a target or rel. A target with no slash is its own file
/// name, which is why this cannot fail.
fn file_name(target: &str) -> &str {
    match target.rfind('/') {
        Some(slash) => &target[slash + 1..],
        None => target,
    }
}

/// Whether an extension names a file that is linked to but never parsed. Case-folded, because
/// `PIC.PNG` and `pic.png` are the same kind of attachment.
fn is_attachment(extension: &str) -> bool {
    ATTACHMENT_EXTENSIONS
        .iter()
        .any(|known| extension.eq_ignore_ascii_case(known))
}

/// One link with its target placed in the corpus — the row [`store_links`] writes.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ResolvedLink {
    /// The note the link was written in, `/`-separated as the registry spells it.
    pub src_rel: String,
    /// The target exactly as [`RawLink`] normalised it. Kept even when nothing matched: a later
    /// milestone suggests where to create the missing note, and can only do that from the text.
    pub target: String,
    pub kind: LinkKind,
    pub line: usize,
    /// The file the target names, or `None` when the corpus has no single answer for it.
    pub dst_rel: Option<String>,
}

/// Place every target in the corpus, never guessing.
///
/// `corpus` is the map Task 10 builds — `case_key` → `rel`, one entry per case-folded path — and
/// `links` one `(src_rel, links)` entry per note, so a resolved list needs no second pass to know
/// who wrote each link. Each target is placed in exactly this order:
///
/// 1. **exact `rel`**, the spelling the registry uses;
/// 2. **case-insensitively** by `case_key`, which is what a case-insensitive filesystem types;
/// 3. **by unique basename**, so `[[Beta]]` finds `deep/nested/Beta.md`. Each file is indexed
///    under its lowercased file name *and* that name without its extension, because a wikilink
///    usually omits `.md` while an attachment target keeps it.
///
/// More than one candidate at any step stays unresolved — two `a.md` files in different directories
/// make the target `a` ambiguous, and so does a note `a.md` beside an image `a.png`. Nothing prefers
/// one kind over another: a wikilink may legitimately name an attachment, so preferring `.md` would
/// break a real link rather than guess well. **The author's way out is to spell the path** —
/// `[[notes/a.md]]` or `[text](notes/a.md)` is exact and wins over the ambiguity — which is what a
/// later report should suggest next to an unresolved target. An unknown target is unresolved the same
/// way, and an empty corpus resolves nothing without failing.
pub fn resolve(
    corpus: &BTreeMap<String, String>,
    links: &[(String, Vec<RawLink>)],
) -> Vec<ResolvedLink> {
    let basenames = basename_index(corpus);
    let mut resolved: Vec<ResolvedLink> = Vec::new();
    for (src_rel, raw_links) in links {
        for link in raw_links {
            resolved.push(ResolvedLink {
                src_rel: src_rel.clone(),
                target: link.target.clone(),
                kind: link.kind,
                line: link.line,
                dst_rel: resolve_target(corpus, &basenames, &link.target),
            });
        }
    }
    resolved
}

/// The corpus path one target names, in the order [`resolve`] documents.
fn resolve_target(
    corpus: &BTreeMap<String, String>,
    basenames: &HashMap<String, Vec<String>>,
    target: &str,
) -> Option<String> {
    // One `case_key` lookup answers both of the first two steps: `case_key` is unique in the
    // registry, so what is found here is either spelled exactly as the target (step 1) or its only
    // case variant (step 2).
    if let Some(rel) = corpus.get(&target.to_lowercase()) {
        return Some(rel.clone());
    }
    // Step 3. A file name held by several paths is not resolvable by anybody's rule.
    basenames
        .get(&file_name(target).to_lowercase())
        .filter(|candidates| candidates.len() == 1)
        .map(|candidates| candidates[0].clone())
}

/// Corpus file names to the rels that carry them, lowercased: `deep/nested/Beta.md` is findable as
/// both `beta.md` and `beta`. A `Vec` longer than one entry is what makes a target ambiguous, and
/// a file whose name has no extension contributes one key rather than two — inserting the same
/// path twice under one key would make it ambiguous against itself.
fn basename_index(corpus: &BTreeMap<String, String>) -> HashMap<String, Vec<String>> {
    let mut index: HashMap<String, Vec<String>> = HashMap::new();
    for rel in corpus.values() {
        let name = file_name(rel);
        let name_key = name.to_lowercase();
        index.entry(name_key.clone()).or_default().push(rel.clone());
        let stem = match extension(name) {
            Some(extension) => &name[..name.len() - extension.len() - 1],
            None => name,
        };
        let stem_key = stem.to_lowercase();
        if stem_key != name_key {
            index.entry(stem_key).or_default().push(rel.clone());
        }
    }
    index
}

/// The one statement [`store_links`] writes, named so it is prepared once per call.
const INSERT_LINK: &str = "INSERT INTO links (src_note_id, dst_note_id, kind, target, line, origin)
     VALUES (?1, ?2, ?3, ?4, ?5, ?6)";

/// Write the link rows Task 9 counts and Task 10 reports.
///
/// `src_rel` and `dst_rel` become note ids through `notes.rel`, because a move rewrites a path and
/// not a note — the id is what keeps a link attached across a rename ([`crate::db`]). Two kinds of
/// destination cannot be written as an id:
///
/// - a target the corpus could not place keeps `dst_note_id NULL` plus the raw [`ResolvedLink::target`],
///   which is both the missing-note signal and, later, the input to a placement suggestion;
/// - a target that resolves to a file with no note row — an attachment, which is tracked but never
///   parsed — lands in that same NULL state, because there is genuinely no note on the other end.
///
/// Both mean `unresolved` to [`crate::db::note_counts`], whose `unresolved_links` counts every NULL
/// destination at database level; `ScanReport::unresolved_links` (Task 10) counts only the
/// non-attachment ones, since that number is the "notes I could not place" signal. The two are
/// different questions and must not be compared with each other.
///
/// A link whose **source** has no note row is skipped rather than stored: a non-markdown file is
/// tracked but never parsed, and an orphan row would be counted as an edge from nowhere.
///
/// Nothing is deleted — storing twice writes twice. The caller owns replacement, so a re-parse has
/// to drop that note's `origin = 'prose'` rows in the same transaction before calling this again
/// (Task 10), or every scan duplicates its links.
///
/// `origin` is written verbatim into `links.origin` and so has to be one of the schema's spellings,
/// `prose` or `approved`; anything else is refused by the CHECK constraint and returned as
/// [`DbError::Sqlite`].
pub fn store_links(db: &Db, links: &[ResolvedLink], origin: &str) -> Result<(), DbError> {
    // `prepare_cached`, in this function and in [`note_id`], because a workspace scan writes on the
    // order of 10^4 link rows: `Connection::execute` prepares its statement afresh every call, so
    // using it here would mean 10^4 parses of one INSERT. The cache lives on the connection, so the
    // statement is parsed once and reused by every later call as well.
    let mut ids: HashMap<&str, Option<i64>> = HashMap::new();
    for link in links {
        let Some(src) = note_id(db, &mut ids, &link.src_rel)? else {
            continue;
        };
        let dst = match &link.dst_rel {
            Some(rel) => note_id(db, &mut ids, rel)?,
            None => None,
        };
        db.0.prepare_cached(INSERT_LINK)?.execute(params![
            src,
            dst,
            link.kind.as_str(),
            link.target,
            // No note has a line number near `i64::MAX`, but a bare cast would wrap one.
            i64::try_from(link.line).unwrap_or(i64::MAX),
            origin
        ])?;
    }
    Ok(())
}

/// The note id for one `rel`, memoised per [`store_links`] call: a note's own path repeats on every
/// one of its links, and a popular target repeats across notes.
fn note_id<'a>(
    db: &Db,
    ids: &mut HashMap<&'a str, Option<i64>>,
    rel: &'a str,
) -> Result<Option<i64>, DbError> {
    if let Some(id) = ids.get(rel) {
        return Ok(*id);
    }
    let mut statement =
        db.0.prepare_cached("SELECT id FROM notes WHERE rel = ?1 LIMIT 1")?;
    let mut rows = statement.query(params![rel])?;
    let id = match rows.next()? {
        Some(row) => Some(row.get::<_, i64>(0)?),
        None => None,
    };
    ids.insert(rel, id);
    Ok(id)
}
