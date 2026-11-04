//! Every image a note links to, and the workspace file that link names.
//!
//! Two steps, in this order and no other: [`image_links`] reads note bytes for markdown image syntax, then
//! [`resolve_image`] places one target in the workspace, and [`crate::hash::sha256_file_with_size`] digests whatever it
//! found — the digest stays in `hash.rs`, where §12 rule 9 keeps the one definition. Design §7 of
//! `docs/design-m2-image-embeddings.md` owns the rules; this module is where they are executed.
//!
//! **Everything here works on bytes.** A note body, a link target and a resolved path are `&[u8]` end to end, and no
//! function decodes one — F13's `caf\xe9.png` is a legal name on Unix and the image it names has to be embeddable.
//! That is also why resolution does not reuse [`crate::links`]: its targets are display text, resolved through
//! case-folded lookups, and §7 makes media resolution byte-exact with a percent-decode and nothing else. A fold would
//! not merely be redundant here (media identity is a content hash, so two spellings of one figure already reach one
//! row — see `crate::db::link_media_path`) — on a case-sensitive filesystem it would embed a *different* file than
//! the author wrote.
//!
//! Refusals are classes ([`MediaRefusal`]), never messages: no path, URL or origin crosses this boundary (§8's rule
//! 8), and none of them is an exit code — every one is a count on an embed report.

use serde::Serialize;
use std::ffi::OsStr;
use std::io;
use std::path::{Path, PathBuf};

use crate::walk;

/// The component spellings normalisation drops without looking at the filesystem.
const CURRENT_DIR: &[u8] = b".";
const PARENT_DIR: &[u8] = b"..";

/// One image link as it was written in a note.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ImageLink {
    /// The destination exactly as written — percent-escapes still in it, resolution decodes ([`resolve_image`]) —
    /// with the angle brackets of the `<…>` form removed, because those are syntax and no file name holds them.
    pub target: Vec<u8>,
    /// 1-based line number in the file, the same number an editor shows for a CRLF or an LF copy of the note.
    pub line: usize,
}

/// The image links in a note body, in reading order.
///
/// **Only markdown image syntax counts**: `![alt](target)`, and its angle-bracket form `![alt](<target with a
/// space>)`, which exists for exactly that one reason. Three things are therefore not images —
///
/// - `[text](x.png)`: a link, whose destination may be an attachment but which promises no picture;
/// - `![[scan.png]]`: an embed wikilink, which has no `(` and belongs to [`crate::links`];
/// - anything inside a fenced code block — ``` or `~~~`, and an unclosed fence runs to the end of the file — where
///   an image is an example of syntax rather than a figure the author placed.
///
/// An image nested in a link's own text (`[![badge](i.png)](https://shields.io)`) **is** extracted: the outer
/// destination leaves the corpus, the inner target is a real file.
///
/// Nothing is decoded, trimmed of escapes or checked against the disk here — one function reads syntax, the other
/// resolves paths. An unterminated `![alt](target` produces no link at all rather than a name sliced out of the rest
/// of the sentence, and an empty destination (`![alt]()`) produces a link with an empty target, which
/// [`resolve_image`] refuses as it refuses any path that names no file.
pub fn image_links(body: &[u8]) -> Vec<ImageLink> {
    let mut found: Vec<ImageLink> = Vec::new();
    let mut fenced = false;
    for (number, line) in split_lines(body) {
        if is_fence(line) {
            fenced = !fenced;
            continue;
        }
        if fenced {
            continue;
        }
        scan_line(line, number, &mut found);
    }
    found
}

/// The lines of a body with their 1-based numbers and a trailing `\r` removed.
///
/// The `\r` is syntax rather than content — left attached it would end up inside a target (`here.png\r`), which then
/// refuses as `unsupported-format` on every CRLF note. Same line model as `crate::parse`.
fn split_lines(body: &[u8]) -> impl Iterator<Item = (usize, &[u8])> {
    body.split(|byte| *byte == b'\n')
        .enumerate()
        .map(|(index, line)| (index + 1, strip_cr(line)))
}

/// `line` without a trailing carriage return.
fn strip_cr(line: &[u8]) -> &[u8] {
    match line.split_last() {
        Some((b'\r', rest)) => rest,
        _ => line,
    }
}

/// A line that opens or closes a fenced code block: three backticks **at column 0**, which is exactly
/// [`crate::parse`]'s rule and deliberately not a wider one.
///
/// Two definitions of "inside a code block" would have to be kept in step forever, and this one is not the authority on
/// markdown — §5's rule 3 is, and `parse::prose_lines` implements it for headings, tags, words and links. So an image in
/// a `~~~`-fenced example *is* embedded (the chunker indexes that block's words as prose too), and an indented ``` fence
/// is not a fence here either, because it is not one for the chunker. The CommonMark-faithful reading may well be the
/// better markdown rule; adopting it here while `parse` kept the column-0 rule would mean a diagram being embedded from a
/// region whose caption was indexed as prose, which is worse than either consistent choice.
///
/// **An unclosed fence runs to the end of the file**, matching `prose_lines`: the toggle stays set, so a paste truncated
/// mid-block does not start embedding its tail.
fn is_fence(line: &[u8]) -> bool {
    line.starts_with(b"```")
}

/// The image links on one prose line, appended in the order the scan reaches them.
fn scan_line(line: &[u8], number: usize, out: &mut Vec<ImageLink>) {
    let mut cursor = 0usize;
    while cursor + 1 < line.len() {
        if line[cursor] == b'!' && line[cursor + 1] == b'[' {
            if let Some((target, resume)) = image_at(line, cursor) {
                out.push(ImageLink {
                    target,
                    line: number,
                });
                cursor = resume;
                continue;
            }
        }
        cursor += 1;
    }
}

/// The target of the `![alt](target)` whose `!` sits at `bang`, and the index the scan resumes at.
fn image_at(line: &[u8], bang: usize) -> Option<(Vec<u8>, usize)> {
    // The alt text ends at the bracket that matches the one after the `!`, so a nested pair inside it (`![a [x]](p.png)`)
    // does not end the syntax early.
    let close = matching_bracket(line, bang + 1)?;
    // A markdown image needs `(` right after its own alt text. This is also what skips `![[scan.png]]`, whose next
    // character is another `]`.
    if line.get(close + 1) != Some(&b'(') {
        return None;
    }
    let start = close + 2;
    let (target, end) = if line.get(start) == Some(&b'<') {
        // Inside `<…>` the bytes *are* the name — spaces included, which is the whole reason the form exists — so
        // nothing is trimmed off them. The `)` still has to be there or the syntax was never closed.
        let angle_end = find(line, b'>', start + 1)?;
        (
            line.get(start + 1..angle_end)?.to_vec(),
            find(line, b')', angle_end)?,
        )
    } else {
        let close_paren = find(line, b')', start)?;
        // A destination ends at the first ASCII whitespace: everything after it is a title (`![a](x.png "Kafka ingest")`),
        // and a name that genuinely contains a space has to be written `<with space.png>` — which is the branch above,
        // where nothing is trimmed because there the bytes *are* the name. Without this cut the title becomes part of the
        // extension and a real file refuses as `unsupported-format`, a class §8 reserves for extensions outside the set.
        let destination = line[start..close_paren]
            .trim_ascii_start()
            .split(|byte| byte.is_ascii_whitespace())
            .next()
            .unwrap_or_default();
        (destination.to_vec(), close_paren)
    };
    Some((target, end + 1))
}

/// The `]` that closes the `[` at `open`, counting nested pairs.
fn matching_bracket(line: &[u8], open: usize) -> Option<usize> {
    let mut depth = 0usize;
    for (index, byte) in line.iter().enumerate().skip(open + 1) {
        match byte {
            b'[' => depth += 1,
            // A `]` at depth 0 is the one that matches `open`; deeper ones belong to a nested pair.
            b']' if depth == 0 => return Some(index),
            b']' => depth -= 1,
            _ => {}
        }
    }
    None
}

/// The index of `needle` at or after `from`, or nothing when it is absent (or `from` is past the end).
fn find(haystack: &[u8], needle: u8, from: usize) -> Option<usize> {
    haystack
        .get(from..)?
        .iter()
        .position(|byte| *byte == needle)
        .map(|offset| from + offset)
}

/// Where one link's target points, and what kind of file it names.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ResolvedImage {
    /// The workspace-relative path in `/`-separated bytes, normalised: this is the identity `media_path.rel_bytes`
    /// stores, so two spellings of one file (`./a/../b.png`, `b.png`) produce one row. Raw bytes are kept rather than
    /// text because a non-UTF-8 name has to key apart from its lossy spelling (F13). No case-folded twin travels with
    /// it: media identity is the content hash, and `media_path` stopped carrying a folded column for it in `e2180da`.
    pub rel_bytes: Vec<u8>,
    /// The same path under `root`, spelled the way this OS wants it — what the caller opens.
    pub abs: PathBuf,
    /// `image/png` or `image/jpeg`, decided by extension and nothing else (§8's accepted set).
    pub mime: &'static str,
}

/// Why one image link will not be embedded, in §8's words.
///
/// A class and nothing else: no variant carries a path, a URL or an origin (models-install §12 rule 8), because these
/// are counts on a report and the row that produced one already names itself. None of them is an exit code either —
/// runtime refusals stay `1`, argv problems stay `2`, and an image that will not embed changes neither.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "kebab-case")]
pub enum MediaRefusal {
    /// The link resolves inside the workspace and no file is there — including a name this platform cannot spell and
    /// a target that names a directory rather than a file.
    MissingTarget,
    /// An absolute target, or one that pops past the workspace root. Decided from the path text, before anything is
    /// opened.
    OutsideWorkspace,
    /// An extension outside the accepted set (§8: `.png`, `.jpg`, `.jpeg` and nothing else).
    UnsupportedFormat,
    /// An accepted extension whose bytes do not decode to an image. Produced by the encoder (Task 4), never by
    /// resolution — a malformed percent-escape is `MissingTarget`, because a name that matches no file needs no new
    /// class to say so.
    Undecodable,
    /// The encoded image needs more of the context window than the run allows (§8 measures this after encoding).
    OverBudget,
    /// This build cannot read images, or the loaded model carries no projector.
    VisionUnavailable,
}

/// These bytes are not a raster this build can decode (§8). **The one definition**; `crate::vision` re-exports it, and
/// `MediaRefusal::label` answers with it, so the seam's refusal and the report's label cannot drift apart.
pub const UNDECODABLE: &str = "undecodable";
/// The encode measured larger than the context can hold (§8). **The one definition**, same reasoning as
/// [`UNDECODABLE`].
pub const OVER_BUDGET: &str = "over-budget";
/// This build and model pair cannot serve the request (§8). **The one definition**, shared by three readers: this label,
/// [`crate::embed::Embedder::embed_image`]'s default refusal, and the vision seam that emits it most often (a module
/// compiled only under the `vision` feature, so it is named in prose rather than linked — a doc link here would break
/// every default-build `cargo doc`).
pub const VISION_UNAVAILABLE: &str = "vision-unavailable";

impl MediaRefusal {
    /// The §8 kebab-case spelling, which is what an embed report prints and its JSON repeats; Tasks 5, 6 and 8 label
    /// through this one function so a class cannot be spelled two ways.
    ///
    /// **Three of these six return a `const`, not a literal** — [`UNDECODABLE`], [`OVER_BUDGET`] and
    /// [`VISION_UNAVAILABLE`] are also emitted by code outside this module (the vision seam, and
    /// [`crate::embed::Embedder::embed_image`]'s default refusal), so they have exactly one spelling in the tree. A
    /// measured consequence of keeping them literal: renaming `embed`'s copy on 2026-11-03 left every default-build test
    /// green (534 passed, 0 failed) because the only assertion comparing the two sits behind `--features vision`.
    /// `missing-target`, `outside-workspace` and `unsupported-format` are emitted here alone, so they stay literals —
    /// pinned by name in `tests/media_resolve.rs`.
    pub fn label(&self) -> &'static str {
        match self {
            MediaRefusal::MissingTarget => "missing-target",
            MediaRefusal::OutsideWorkspace => "outside-workspace",
            MediaRefusal::UnsupportedFormat => "unsupported-format",
            MediaRefusal::Undecodable => UNDECODABLE,
            MediaRefusal::OverBudget => OVER_BUDGET,
            MediaRefusal::VisionUnavailable => VISION_UNAVAILABLE,
        }
    }
}

/// Place one link target in the workspace, relative to the directory of the note that wrote it.
///
/// `note_rel` is the linking note's own workspace-relative path (its bytes, as the catalog holds them) and `target`
/// is an [`ImageLink::target`] as written. The order is the whole function and each step refuses:
///
/// 1. **one percent-decode pass**, which fails closed — a malformed escape leaves the name
///    exactly as written, so it matches no file and lands in `missing-target`;
/// 2. **absolute targets are refused** (§7), whatever follows the leading `/`;
/// 3. **a target that names nothing** (``, `.`, `./`) is `missing-target`: §8 has no class for "that is a directory",
///    and "no file behind this link" is the truth;
/// 4. **normalisation by component walking** — `.` and empty components drop, `..` pops, and popping past the start
///    is `outside-workspace`. The base is the note's own directory, walked by the same rule, so the result is inside
///    the workspace *by construction* and no path ever leaves it. This happens before any filesystem call, which is
///    what keeps `/etc/x.png` a refusal rather than a read;
/// 5. **then the extension**: outside §8's accepted set is `unsupported-format`, whether or not the file exists —
///    a format we would not decode is not looked for on disk;
/// 6. **then the file**: absent, unreadable-as-metadata, or not a regular file is `missing-target`. A directory with
///    an image suffix belongs here: its name passed step 5, so what is wrong about it is that it is not a file.
///
/// Containment before format before existence is deliberate and asymmetric on purpose: `../../etc/passwd.png` names
/// nothing we should have opened, and `diagram.gif` names a format this build refuses — reporting the second as
/// "missing" would send an author off to look for a file that is there.
pub fn resolve_image(
    root: &Path,
    note_rel: &[u8],
    target: &[u8],
) -> Result<ResolvedImage, MediaRefusal> {
    resolve_image_in(PathStyle::native(), root, note_rel, target)
}

/// Which platform's spelling rules decide what counts as a separator and as a rooted name.
///
/// This is public so the rule can be *tested* rather than hoped for: the escape that only exists on Windows
/// (`C:\shots\a.png`, `\\srv\share\a.png`, `..\..\outside.png`) has to be refused by code this Linux host can run,
/// because a claim about Windows behaviour nobody executed is exactly what F18 forbids. The refusal happens in the walk,
/// before any [`PathBuf::push`], so it does not depend on how the host OS would have interpreted the component.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PathStyle {
    /// `/` separates and only a leading `/` is rooted. A backslash is an ordinary byte: `a\b.png` is one legal name.
    Unix,
    /// `/` and `\` both separate, and a component that carries a drive (`C:`) or a prefix (`\server`, `\share`) is
    /// rooted rather than named.
    Windows,
}

impl PathStyle {
    /// The style of the platform this binary is running on.
    pub fn native() -> Self {
        if cfg!(windows) {
            Self::Windows
        } else {
            Self::Unix
        }
    }

    fn is_separator(self, byte: u8) -> bool {
        byte == b'/' || (self == Self::Windows && byte == b'\\')
    }

    /// Whether one component names a root instead of a file inside one. Checked per component and **before** it is
    /// pushed, because `PathBuf::push` on Windows discards everything already in the path when the argument carries a
    /// prefix — an escape has to be refused as the spelling it is, never discovered after the fact.
    fn is_rooted(self, name: &[u8]) -> bool {
        match self {
            Self::Unix => name.first() == Some(&b'/'),
            Self::Windows => {
                name.first() == Some(&b'/')
                    || name.first() == Some(&b'\\')
                    // `C:` / `c:` — an ASCII letter followed by a colon, then anything.
                    || (name.len() >= 2
                        && name[0].is_ascii_alphabetic()
                        && name[1] == b':')
            }
        }
    }

    fn split(self, bytes: &[u8]) -> Vec<&[u8]> {
        if self == Self::Unix {
            return bytes.split(|byte| *byte == b'/').collect();
        }
        bytes
            .split(|byte| *byte == b'/' || *byte == b'\\')
            .collect()
    }
}

/// [`resolve_image`] with the platform rules named instead of taken from the host.
pub fn resolve_image_in(
    style: PathStyle,
    root: &Path,
    note_rel: &[u8],
    target: &[u8],
) -> Result<ResolvedImage, MediaRefusal> {
    // Failing closed means passing the bytes through, so a file literally named `kafka%zz.png` still resolves.
    let written = percent_decode(target).unwrap_or_else(|| target.to_vec());
    if style.is_rooted(&written) {
        return Err(MediaRefusal::OutsideWorkspace);
    }
    // A NUL byte names no file on any platform this program runs on, so it is refused here rather than discovered three
    // steps later by a failed `stat`. **This one is not observable behaviour on Unix**: deleting the check still reports
    // `missing-target`, because the OS call refuses the same path for its own reason (measured — the mutant survived).
    // What it buys is determinism on Windows, where the same bytes reach `component_name` and would surface as whatever
    // that conversion failure happens to be, and a guarantee the reader can use: no component that reaches the OS contains
    // a NUL, which is what makes `component_name`'s "this platform cannot spell it" arm defence in depth rather than an
    // unreachable branch with a story attached.
    if written.contains(&0) {
        return Err(MediaRefusal::MissingTarget);
    }
    if names_nothing(&written) {
        return Err(MediaRefusal::MissingTarget);
    }
    // A trailing separator names a directory (`![a](assets.png/)`), so a regular file at that spelling is not what the
    // author pointed at. §8 has no class for it; "names nothing" is the honest one, because at that spelling it does.
    let names_a_directory = style.is_separator(*written.last().unwrap_or(&b'/'));

    // Step 4: the note's directory, then the target, in one walk. A `note_rel` that itself pops past the root is not a
    // workspace path and gets the same refusal — nothing may reach outside through the base either. The rooted test runs
    // on the base's components too: a registry key that spells a Windows prefix escapes for the same reason a target does.
    let mut path: Vec<Vec<u8>> = Vec::new();
    let mut directories = style.split(note_rel);
    // The last component is the note's file name; a root-level note has no directory left to walk.
    directories.pop();
    for name in directories.into_iter().chain(style.split(&written)) {
        if style.is_rooted(name) || !push_component(&mut path, name) {
            return Err(MediaRefusal::OutsideWorkspace);
        }
    }

    // A target that walked away to nothing (`a/..` on a root-level note) names the workspace root itself, which is a
    // directory and therefore no more an image than `.` was.
    let Some(name) = path.last() else {
        return Err(MediaRefusal::MissingTarget);
    };
    let mime = extension_of(name)
        .and_then(mime_of)
        .ok_or(MediaRefusal::UnsupportedFormat)?;
    let abs = abs_path(root, &path).ok_or(MediaRefusal::MissingTarget)?;
    // Containment is not finished once the components have been walked: a directory *inside* the workspace can be a
    // symlink to somewhere outside it, and then every byte above is true of a path whose contents are not in the vault.
    // So each component from the root down is lstat'd, and any link — including the file itself — refuses as
    // `outside-workspace`. Canonicalising instead would need the target to exist (turning every absent figure into an
    // error path) or would have to canonicalise the root too (a workspace that is itself a symlink then refuses
    // everything); neither problem exists here, and the cost is one `stat` per directory level.
    if escapes_through_a_link(root, &path) {
        return Err(MediaRefusal::OutsideWorkspace);
    }
    // `to_open_path` because this is the spelling an OS call wants (a Windows path past `MAX_PATH` still has to be
    // statable), and `is_file` because a directory read would hand back a digest of nothing.
    match fs_metadata(&abs) {
        Ok(metadata) if metadata.is_file() && !names_a_directory => {}
        _ => return Err(MediaRefusal::MissingTarget),
    }
    Ok(ResolvedImage {
        rel_bytes: join_rel(&path),
        abs,
        mime,
    })
}

/// Whether any component of `components`, from `root` down, is a symbolic link.
///
/// The root itself is not tested: the operator chose where the workspace lives, and a workspace that happens to be a
/// link is still the workspace. Everything below it has to be bytes the vault actually holds.
fn escapes_through_a_link(root: &Path, components: &[Vec<u8>]) -> bool {
    let mut path = root.to_path_buf();
    for component in components {
        let Some(name) = component_name(component) else {
            // A component this platform cannot spell never reaches the disk, so it cannot be a link either; the caller
            // turns the same condition into `missing-target` at `abs_path`.
            return false;
        };
        path.push(name);
        if let Ok(metadata) = std::fs::symlink_metadata(&path) {
            if metadata.file_type().is_symlink() {
                return true;
            }
        }
    }
    false
}

/// Whether a decoded target holds no name at all: only empty components and `.`, with no parent step anywhere.
fn names_nothing(target: &[u8]) -> bool {
    target
        .split(|byte| *byte == b'/')
        .all(|name| name.is_empty() || name == CURRENT_DIR)
}

/// Applies one `/`-separated component to the path built so far, answering `false` when it leaves the workspace.
///
/// `.` and an empty component (a doubled or trailing slash) drop out; `..` pops the last name and **refuses** to pop
/// past the start, which is what makes an escape visible to [`resolve_image`] instead of quietly producing a path
/// above the root. Every other name is pushed exactly as written — no case fold, no sort, no second decode (§7).
fn push_component(path: &mut Vec<Vec<u8>>, name: &[u8]) -> bool {
    if name.is_empty() || name == CURRENT_DIR {
        return true;
    }
    if name == PARENT_DIR {
        // Below the workspace root there is nothing this program owns, so there is nothing to pop.
        return path.pop().is_some();
    }
    path.push(name.to_vec());
    true
}

/// The bytes after the last dot in one file name, or nothing when it has no such dot.
///
/// A leading dot names a hidden file rather than an extension (`.png` is a name, not a format), matching
/// `crate::links`. Because callers hand this a single component, a dot anywhere else in the path cannot leak in:
/// `archive.png/README` has no extension.
fn extension_of(name: &[u8]) -> Option<&[u8]> {
    let dot = name.iter().rposition(|byte| *byte == b'.')?;
    (!name[..dot].is_empty() && !name[dot + 1..].is_empty()).then_some(&name[dot + 1..])
}

/// The eight bytes every PNG starts with (§7's format list, straight from the format's own signature block).
pub const PNG_SIGNATURE: &[u8; 8] = b"\x89PNG\r\n\x1a\n";

/// What a file **actually is**, judged from its first bytes rather than its name — `image/png`, `image/jpeg`, or nothing.
///
/// This exists because the extension gate alone was measurably not the promise. On 2026-11-03 a BMP payload reached the
/// pinned decoder under a `.png` filename and came back a live bitmap, then a vector at 80 image tokens; `stb_image`
/// inside `libmtmd` also takes tga, gif, psd, pnm and qoi. So §7's "png/jpg/jpeg only" needs enforcing where the bytes are
/// read, which is what closes backlog **F26**.
///
/// **Why a pure prefix function rather than a check inside [`resolve_image`]:** that function's whole job is placing a path
/// in the workspace — every refusal it makes is about *where* bytes live, and its 37 cases say so. Format is a question
/// about the bytes, the caller already has the file open to hash it, and a resolver that read content would have to decide
/// what `missing-target` means for a file that exists but lies. The two-step story is pinned by
/// `tests/media_resolve.rs::a_mislabelled_file_resolves_and_then_fails_the_content_check`: resolution succeeds, the content
/// check refuses, and only the second refusal is reportable as `unsupported-format`.
///
/// A short prefix simply answers nothing: a three-byte JPEG start-of-image marker cannot be confirmed from two bytes, and
/// guessing from `\xff\xd8` alone would call a truncated file a JPEG. Callers hand this at least eight bytes; fewer is
/// not an error, it is just not enough to say.
pub fn image_kind_of_prefix(prefix: &[u8]) -> Option<&'static str> {
    if prefix.starts_with(PNG_SIGNATURE) {
        return Some("image/png");
    }
    if prefix.starts_with(&[0xff, 0xd8, 0xff]) {
        return Some("image/jpeg");
    }
    None
}

/// The MIME type of an accepted extension, or nothing for one §8 does not accept.
///
/// Case-folded, because `DIAGRAM.PNG` is the same format as `diagram.png`: this fold judges a **kind of file**, which
/// is a different question from which file a path names — and the one place in this module where case may be ignored.
fn mime_of(extension: &[u8]) -> Option<&'static str> {
    if extension.eq_ignore_ascii_case(b"png") {
        return Some("image/png");
    }
    if extension.eq_ignore_ascii_case(b"jpg") || extension.eq_ignore_ascii_case(b"jpeg") {
        return Some("image/jpeg");
    }
    None
}

/// `components` joined with `/` — the workspace-relative byte key §7's normalisation produced.
///
/// Not [`crate::path_keys::rel_bytes`], which builds a key out of OS components: these bytes came out of a note, not
/// out of a directory entry, and putting them through `OsStr` on the way would be where a non-UTF-8 name gets lost on
/// the platforms whose paths are not byte strings.
fn join_rel(components: &[Vec<u8>]) -> Vec<u8> {
    let mut joined: Vec<u8> = Vec::new();
    for component in components {
        if !joined.is_empty() {
            joined.push(b'/');
        }
        joined.extend_from_slice(component);
    }
    joined
}

/// The OS path for a workspace root and normalised components, or nothing when this platform cannot spell them.
fn abs_path(root: &Path, components: &[Vec<u8>]) -> Option<PathBuf> {
    let mut abs = root.to_path_buf();
    for component in components {
        abs.push(component_name(component)?);
    }
    Some(abs)
}

/// The OS path for a `/`-separated workspace-relative byte path under `root`, or nothing when this platform cannot
/// spell one of its components.
///
/// This is the reader's half of `join_rel` (the writer that produces a stored `rel_bytes`): paths come out of notes and out of `media_path.rel_bytes` as bytes, and a
/// non-UTF-8 name has to reopen the file it names rather than the U+FFFD spelling of it (F13). The refusal arm is
/// therefore load-bearing on Windows and inert here, where the conversion is the identity.
pub fn workspace_path(root: &Path, rel: &[u8]) -> Option<PathBuf> {
    let components: Vec<Vec<u8>> = rel
        .split(|byte| *byte == b'/')
        .map(<[u8]>::to_vec)
        .collect();
    abs_path(root, &components)
}

/// The OS name of one byte-spelled component. Unix paths *are* bytes, so this is the identity — which is the entire
/// reason a `caf\xe9.png` resolves here and nowhere else in this function's callers could have found it.
#[cfg(unix)]
fn component_name(bytes: &[u8]) -> Option<&OsStr> {
    use std::os::unix::ffi::OsStrExt;
    Some(OsStr::from_bytes(bytes))
}

/// [`component_name`] where a path is not a byte string.
///
/// A name that is not valid UTF-8 answers `None`, and [`resolve_image`] turns that into `missing-target`: this
/// platform cannot spell the name, so it must refuse rather than substitute the U+FFFD spelling of it and embed
/// whichever other file happens to carry that. Same stance as [`crate::path_keys::name_is_lossless`] (F18) — refuse
/// what cannot be represented exactly — and unlike `os_bytes`, which has to produce *something* for a row the walk
/// already found on disk.
#[cfg(not(unix))]
fn component_name(bytes: &[u8]) -> Option<&OsStr> {
    std::str::from_utf8(bytes).ok().map(OsStr::new)
}

/// `metadata` for the spelling an OS call wants, so one unreadable path is a refusal and not a panic.
fn fs_metadata(abs: &Path) -> io::Result<std::fs::Metadata> {
    std::fs::metadata(walk::to_open_path(abs))
}

/// One percent-decode pass over a target, or nothing when an escape is malformed.
///
/// Only `%XX` with two hex digits becomes a byte. Anything else — a lone `%`, `%z`, `%2`, a `%` ending the string —
/// abandons **the whole decode**, so no caller can end up with half a name decoded and half as written; the raw bytes
/// go back to [`resolve_image`], which then finds no such file. Decoding yields bytes, never characters: `%E9` is the
/// single byte 0xe9 that F13's non-UTF-8 names are built from, and one pass is what §7's "percent-decoded" means —
/// `%2520` becomes a literal `%20`, not a space.
fn percent_decode(target: &[u8]) -> Option<Vec<u8>> {
    let mut decoded: Vec<u8> = Vec::with_capacity(target.len());
    let mut index = 0usize;
    while index < target.len() {
        if target[index] != b'%' {
            decoded.push(target[index]);
            index += 1;
            continue;
        }
        let high = hex_value(*target.get(index + 1)?)?;
        let low = hex_value(*target.get(index + 2)?)?;
        decoded.push(high * 16 + low);
        index += 3;
    }
    Some(decoded)
}

/// The value of one hex digit, upper or lower case, or nothing when the byte is not one.
fn hex_value(byte: u8) -> Option<u8> {
    match byte {
        b'0'..=b'9' => Some(byte - b'0'),
        b'a'..=b'f' => Some(byte - b'a' + 10),
        b'A'..=b'F' => Some(byte - b'A' + 10),
        _ => None,
    }
}
