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

use crate::db::{self, Db, DbError};
use crate::parse::prose_lines;
use crate::path_keys::RelPath;

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
///
/// Source and destination are [`RelPath`]s, not display strings. Placement still works on text (see
/// [`resolve`]'s order), but the answer has to name **which row** it landed on: two live rows can spell one
/// display path identically, and a link written against that text would otherwise be stored against whichever
/// note the id lookup happened to reach (F13 §3).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ResolvedLink {
    /// The note the link was written in.
    pub src: RelPath,
    /// The target exactly as [`RawLink`] normalised it. Kept even when nothing matched: a later
    /// milestone suggests where to create the missing note, and can only do that from the text.
    pub target: String,
    pub kind: LinkKind,
    pub line: usize,
    /// The file the target names, or `None` when the corpus has no single answer for it.
    pub dst: Option<RelPath>,
}

/// Place every target in the corpus, never guessing.
///
/// `corpus` is the map a scan builds — `case_key` → every live path carrying that fold — and `links`
/// one `(src, links)` entry per note, so a resolved list needs no second pass to know who wrote
/// each link. **The matching stays display text end to end** (F13 §4.1: an author names a title, not a byte
/// sequence); what the byte key in each [`RelPath`] decides is only which row a placed target writes.
/// Each target is placed in exactly this order:
///
/// 1. **exact `rel`**, the spelling the registry uses;
/// 2. **case-insensitively** by `case_key`, which is what a case-insensitive filesystem types — and,
///    like every step here, a fold several paths share places nothing;
/// 3. **by unique basename**, so `[[Beta]]` finds `deep/nested/Beta.md`. Each file is indexed
///    under its lowercased file name *and* that name without its extension, because a wikilink
///    usually omits `.md` while an attachment target keeps it.
///
/// More than one candidate at any step stays unresolved — two `a.md` files in different directories
/// make the target `a` ambiguous, and so does a note `a.md` beside an image `a.png`, or two paths that
/// differ only in case (`Ämli.md` beside `ämli.md`) and therefore share one `case_key`. Nothing prefers
/// one kind over another: a wikilink may legitimately name an attachment, so preferring `.md` would
/// break a real link rather than guess well. **The author's way out is usually to spell the path** —
/// `[[notes/a.md]]` or `[text](notes/a.md)` is exact and wins over the ambiguity — which is what a
/// later report should suggest next to an unresolved target. The exception is a pair differing only in case:
/// every spelling of `Ämli.md` folds the same as every spelling of `ämli.md`, so no amount of qualifying places
/// either, and those targets stay unresolved by design (§3 accepts this rather than picking). An unknown target is unresolved the same
/// way, and an empty corpus resolves nothing without failing.
pub fn resolve(
    corpus: &BTreeMap<String, Vec<RelPath>>,
    links: &[(RelPath, Vec<RawLink>)],
) -> Vec<ResolvedLink> {
    let basenames = basename_index(corpus);
    let mut resolved: Vec<ResolvedLink> = Vec::new();
    for (src, raw_links) in links {
        for link in raw_links {
            resolved.push(ResolvedLink {
                src: src.clone(),
                target: link.target.clone(),
                kind: link.kind,
                line: link.line,
                dst: resolve_target(corpus, &basenames, &link.target),
            });
        }
    }
    resolved
}

/// The corpus path one target names, in the order [`resolve`] documents.
fn resolve_target(
    corpus: &BTreeMap<String, Vec<RelPath>>,
    basenames: &HashMap<String, Vec<RelPath>>,
    target: &str,
) -> Option<RelPath> {
    // One `case_key` lookup answers both of the first two steps: what is found here is either spelled
    // exactly as the target (step 1) or its only case variant (step 2) — but only when one path owns
    // that fold. `files.case_key UNIQUE` is gone (F13 §3), so `Ämli.md` and `ämli.md` can both be live
    // rows under the same fold, and then the fold names no file: falling through to step 3 hands the
    // pair to its "held by several paths" decline instead of resolving onto whichever row the corpus
    // map happened to keep.
    if let Some(found) = corpus
        .get(&target.to_lowercase())
        .filter(|candidates| candidates.len() == 1)
        .map(|candidates| candidates[0].clone())
    {
        return Some(found);
    }
    // Step 3. A file name held by several paths is not resolvable by anybody's rule — and two byte-distinct
    // twins carry the *same* display name, so they are two entries here even though a reader cannot tell them
    // apart (F13 §3: that is why the corpus lists rows rather than deduplicating on text).
    basenames
        .get(&file_name(target).to_lowercase())
        .filter(|candidates| candidates.len() == 1)
        .map(|candidates| candidates[0].clone())
}

/// Corpus file names to the rows that carry them, lowercased: `deep/nested/Beta.md` is findable as
/// both `beta.md` and `beta`. A `Vec` longer than one entry is what makes a target ambiguous, and
/// a file whose name has no extension contributes one key rather than two — inserting the same
/// path twice under one key would make it ambiguous against itself. **Do not "fix" that with a dedup on
/// display text.** Two live rows can carry byte-identical display text (F13 §1's twins), so a dedup there
/// would collapse them into one candidate and silently reopen the guess this function exists to refuse;
/// each row is pushed as its own entry whatever its spelling.
fn basename_index(corpus: &BTreeMap<String, Vec<RelPath>>) -> HashMap<String, Vec<RelPath>> {
    let mut index: HashMap<String, Vec<RelPath>> = HashMap::new();
    for paths in corpus.values() {
        for path in paths {
            let name = file_name(&path.rel);
            let name_key = name.to_lowercase();
            index
                .entry(name_key.clone())
                .or_default()
                .push(path.clone());
            let stem = match extension(name) {
                Some(extension) => &name[..name.len() - extension.len() - 1],
                None => name,
            };
            let stem_key = stem.to_lowercase();
            if stem_key != name_key {
                index.entry(stem_key).or_default().push(path.clone());
            }
        }
    }
    index
}

/// The one statement [`store_links`] writes, named so it is prepared once per call.
const INSERT_LINK: &str = "INSERT INTO links (src_note_id, dst_note_id, kind, target, line, origin)
     VALUES (?1, ?2, ?3, ?4, ?5, ?6)";

/// Write the link rows Task 9 counts and Task 10 reports.
///
/// Source and destination become note ids through `notes.rel_bytes`, because a move rewrites a path and
/// not a note — the id is what keeps a link attached across a rename ([`crate::db`]) — and because display
/// text is no longer a spelling that names one row (F13 §3). Two kinds of
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
    let mut ids: HashMap<&[u8], Option<i64>> = HashMap::new();
    for link in links {
        let Some(src) = note_id(db, &mut ids, &link.src)? else {
            continue;
        };
        let dst = match &link.dst {
            Some(path) => note_id(db, &mut ids, path)?,
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

/// The note id for one path's byte key, memoised per [`store_links`] call: a note's own path repeats on every
/// one of its links, and a popular target repeats across notes.
///
/// Keyed on bytes, and read through [`db::note_ids`], which returns **every** row behind the key. Two rows on
/// one key answer `None` here rather than a pick: this function has no report to name a duplicate in, and the
/// write pass that owns the file has already put it in `skipped` (F13 §7). Storing an edge onto whichever row
/// a query reached first is the silent guess the same rule refuses in [`crate::scan`].
fn note_id<'a>(
    db: &Db,
    ids: &mut HashMap<&'a [u8], Option<i64>>,
    path: &'a RelPath,
) -> Result<Option<i64>, DbError> {
    if let Some(id) = ids.get(path.rel_bytes.as_slice()) {
        return Ok(*id);
    }
    let behind_key = db::note_ids(db, &path.rel_bytes)?;
    let id = match behind_key.as_slice() {
        [only] => Some(*only),
        // No row (an attachment, or a note this scan refused) or a legacy duplicate: either way there is no
        // honest id to write.
        _ => None,
    };
    ids.insert(path.rel_bytes.as_slice(), id);
    Ok(id)
}

// ---------------------------------------------------------------------------
// Reading the graph back
// ---------------------------------------------------------------------------

/// One stored edge whose target a reader can follow: **both** ends are live notes.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct LinkEdge {
    /// The display path of the note the link is written in.
    pub src: String,
    /// The display path of the note on the other end — a path, never a row id, because every question this read answers
    /// ends with a person opening a file.
    pub dst: String,
    /// `links.kind`: `wiki`, `md` or `attachment`.
    pub kind: String,
    /// The target exactly as its author wrote it, which is the text a fix replaces.
    pub target: String,
    /// 1-based line in [`LinkEdge::src`] — the number an editor jumps to.
    pub line: i64,
}

/// One stored edge that no live note answers.
///
/// There is deliberately **no `dst` here**: the destination is the thing in question, and naming a stale path beside a
/// dead target would make it look like somewhere to click. The [`Self::target`] is what a fix edits — which is why this
/// row carries a line and `ScanReport::unresolved_targets`, which deduplicates names for an author reading a count, does
/// not.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct UnresolvedEdge {
    /// The display path of the note the link is written in.
    pub src: String,
    /// `links.kind`.
    pub kind: String,
    /// The target as written.
    pub target: String,
    /// 1-based line in [`UnresolvedEdge::src`].
    pub line: i64,
}

/// What [`listing`] reads out of the `links` table.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct LinkListing {
    /// Edges that land on a live note.
    pub edges: Vec<LinkEdge>,
    /// Edges that do not — a target nobody placed, or one whose note has gone stale since.
    pub unresolved: Vec<UnresolvedEdge>,
}

/// Which part of the graph one [`listing`] call covers.
#[derive(Debug, Clone, Copy)]
pub enum LinkScope<'a> {
    /// Every edge in the workspace.
    All,
    /// Every edge with this note at **either** end.
    ///
    /// A subject rather than a direction, because the three features that asked for this read need two different filters
    /// (a backlinks pane wants the edges *arriving* at a note; fix-at-source wants the dead links *written in* it) and one
    /// parameter that silently meant "incoming" for one list and "outgoing" for the other is how a GUI ends up listing a
    /// note as its own backlink. Every entry names both of its ends, so a client says which end it wanted by comparing
    /// `dst` to the path it asked about — no fourth key has to be invented to express it.
    ///
    /// The path is matched on its **byte key**, like every other identity in this database (F13 §4). A caller holding a
    /// path that came off the disk holds those bytes already; one that typed or received a spelling gets
    /// [`RelPath::from_display`], which is F13's documented rule for text-only callers and inherits its residual: two live
    /// rows whose raw names differ where UTF-8 cannot round-trip are addressed by their display spelling, not apart.
    Note(&'a RelPath),
}

/// The stored link graph, spelled in display paths (**issue #2**).
///
/// `np` computes the whole graph on every scan and had no read for any of it: an application wanting a backlinks pane
/// could re-parse every note with its own link parser — whose idea of a wikilink drifts from np's in ways no test can
/// catch — or open `<data_dir>/workspaces/<key>.sqlite` and build a shipping product on an internal schema that
/// migrations reshape without warning. This is the third option, and the reason the rows carry `(src, line)` rather than
/// counts: "who links here", "where is this dead target written" and "what must I rewrite when I rename that note" are
/// three questions about one edge list.
///
/// Four rules decide what appears, each of them a reader's question rather than a property of the table:
///
/// * **`prose` origins only.** Every other reader of `links` — an image hit's linker set, a scan's unresolved list —
///   reads prose, because an `approved` edge is a claim a human made about a note that may no longer exist. One rule
///   everywhere is what lets `np scan --json`'s number and this list be read side by side.
/// * **Attachments appear in neither list.** A figure's `target` is relative to the linking note's directory and arrives
///   in TEXT already lossy-decoded (§6), so this table cannot say whether a figure exists, let alone where it moved.
///   "Which notes embed this figure" is answered by [`crate::search`]'s `linked_notes`, which re-runs the one resolution
///   rule that can — and a rename tool that needs figure edges must go through that rule rather than guessing here.
/// * **The source note has to be live.** `files.state = 'stale'` is history np keeps on purpose, and a scan's unresolved
///   count still counts it; a pane a person clicks must not name a file they cannot open. So this predicate is search's
///   linker-set one (`state = 'ok'`), not the author-facing count's, and the two numbers may legitimately disagree on a
///   corpus with churn in it — one counts what was written, this lists what can be opened.
/// * **A stale destination is unresolved**, by `db::LIVE_EDGE` (private, and the reason it is named in text: one predicate, four readers) — the same predicate behind `ScanReport`'s
///   `unresolved_links` count, so a count and the list printed under it cannot disagree about which edges are intact.
///
/// # Order
///
/// Source path in byte order, then line, then insertion: the order a reader would walk a corpus in, pinned because both
/// channels print this list and a pane that reshuffles between two identical scans is indistinguishable from a bug.
pub fn listing(db: &Db, scope: LinkScope<'_>) -> Result<LinkListing, DbError> {
    // One statement for both scopes rather than two texts that can drift: the only difference is the subject predicate,
    // and a reader comparing the two should see the shared half once. The join is on `rel_bytes` throughout (F13 §3) —
    // matching or ordering on display text would cross-join a pair of live rows that spell themselves identically.
    // The scope clause goes **before** the `ORDER BY`, which is why the two halves are named apart rather than one string
    // with a suffix appended: the first version of this appended `AND …` onto a statement that already ended in
    // `ORDER BY sn.rel_bytes, l.line, l.rowid`, SQLite read it as an expression to sort by, and the scoped query answered
    // every edge in the corpus in a different order instead of refusing to answer them at all — silently, and only for
    // the caller who asked about one note.
    let sql = match scope {
        LinkScope::All => format!("{LISTING_WHERE}{LISTING_ORDER}"),
        LinkScope::Note(_) => {
            format!("{LISTING_WHERE} AND (sn.rel_bytes = ?2 OR dn.rel_bytes = ?2){LISTING_ORDER}")
        }
    };
    let mut statement = db.0.prepare(&sql)?;
    let map = |row: &rusqlite::Row<'_>| {
        Ok((
            row.get::<_, String>(0)?,
            row.get::<_, Option<String>>(1)?,
            row.get::<_, i64>(2)?,
            row.get::<_, String>(3)?,
            row.get::<_, String>(4)?,
            row.get::<_, i64>(5)?,
        ))
    };
    // Read out fully before bucketing, so no read statement is open across the loop that consumes it.
    let mut raw: Vec<(String, Option<String>, i64, String, String, i64)> = match scope {
        LinkScope::All => statement
            .query_map(params![db::PROSE_ORIGIN], map)?
            .collect::<Result<_, _>>()?,
        LinkScope::Note(path) => statement
            .query_map(params![db::PROSE_ORIGIN, path.rel_bytes], map)?
            .collect::<Result<_, _>>()?,
    };

    let mut listing = LinkListing::default();
    for (src, dst, live, kind, target, line) in raw.drain(..) {
        match (live == 1, dst) {
            (true, Some(dst)) => listing.edges.push(LinkEdge {
                src,
                dst,
                kind,
                target,
                line,
            }),
            _ => listing.unresolved.push(UnresolvedEdge {
                src,
                kind,
                target,
                line,
            }),
        }
    }
    Ok(listing)
}

/// The shared half of [`listing`]'s statement: the scoped arm appends its `AND …` here and **nothing else may vary**.
///
/// `live` is a column rather than a `WHERE` clause because one pass has to fill both buckets: an edge whose destination
/// row exists but whose file went stale belongs in the unresolved list, and dropping it in SQL would lose the target the
/// list exists to name.
const LISTING_WHERE: &str = "SELECT sn.rel, dn.rel,
           COALESCE(df.state = 'ok', 0),
           l.kind, l.target, l.line
      FROM links l
      JOIN notes sn ON sn.id = l.src_note_id
      JOIN files sf ON sf.rel_bytes = sn.rel_bytes AND sf.state = 'ok'
 LEFT JOIN notes dn ON dn.id = l.dst_note_id
 LEFT JOIN files df ON df.rel_bytes = dn.rel_bytes
     WHERE l.origin = ?1 AND l.kind != 'attachment'";

/// [`listing`]'s order, in its own constant so both arms of the scope match sort by it and neither can forget to.
const LISTING_ORDER: &str = " ORDER BY sn.rel_bytes, l.line, l.rowid";
