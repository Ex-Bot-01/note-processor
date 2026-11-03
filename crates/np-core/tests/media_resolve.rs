//! Task 3 (R7c): markdown image links → byte-exact workspace files, with §8's refusal classes.
//!
//! Two halves, matching the two things `media.rs` promises:
//!
//! - [`np_core::media::image_links`] is pure over bytes — no filesystem — so every case states its whole expectation
//!   as a body and a list of `(target, line)` pairs;
//! - [`np_core::media::resolve_image`] touches the disk only to ask whether one path exists, and only after it has
//!   proved that path is inside the workspace and carries an accepted extension. Cases that must *not* reach the
//!   disk therefore create the file they refuse, so a wrong order of checks cannot pass by accident.
//!
//! Each case names in one comment line the production mutation it catches, like `tests/media_db.rs` does. The
//! mutations worth defending against are the ones this module could plausibly ship with: a lossy decode of the target
//! (F13's non-UTF-8 name then names a different file), a case-folded fallback (design §7 forbids it, and `e2180da`
//! deleted the column that would have stored one), containment checked after the extension or after the filesystem, a
//! percent-decode applied twice or not at all, and line numbers off by one.
//!
//! The digest half ([`np_core::media::sha256_file`]) is compared against [`np_core::hash::sha256_bytes`], so a second
//! SHA-256 spelling in `media.rs` fails here rather than drifting quietly (local-embeddings §12 rule 9).

use std::fs;
use std::path::{Path, PathBuf};

use np_core::hash::sha256_bytes;
use np_core::media::{image_links, resolve_image, sha256_file, MediaRefusal};
use tempfile::TempDir;

/// The extraction as `(target, line)` pairs, so a case states its whole expectation in one literal.
fn extracted(body: &str) -> Vec<(Vec<u8>, usize)> {
    image_links(body.as_bytes())
        .into_iter()
        .map(|link| (link.target, link.line))
        .collect()
}

/// A temp directory holding a workspace root with room above it.
///
/// The root is `<temp>/ws`, not the temp dir itself: the refusals under test are about escaping the workspace, so
/// each test needs a place that is genuinely outside it and still cleaned up afterwards — writing escape targets
/// into `$TMPDIR` would leave them behind.
struct Workspace {
    _dir: TempDir,
    root: PathBuf,
}

impl Workspace {
    fn new() -> Self {
        let dir = tempfile::tempdir().expect("test setup: temp directory");
        let root = dir.path().join("ws");
        fs::create_dir_all(&root).expect("test setup: workspace root");
        Workspace { _dir: dir, root }
    }

    /// The workspace root `resolve_image` is called with.
    fn root(&self) -> &Path {
        &self.root
    }

    /// Write `content` at the `/`-separated workspace-relative `rel`, creating parent directories.
    fn write(&self, rel: &str, content: &[u8]) -> PathBuf {
        let abs = self.join(rel);
        fs::create_dir_all(abs.parent().expect("test setup: a path has a parent"))
            .expect("test setup: create directories");
        fs::write(&abs, content).expect("test setup: write file");
        abs
    }

    /// One workspace-relative path segment at a time, so `rel` never has to be an OS path itself.
    fn join(&self, rel: &str) -> PathBuf {
        let mut abs = self.root.clone();
        for component in rel.split('/') {
            abs.push(component);
        }
        abs
    }

    /// Write a file one level **above** the workspace root — reachable by `..` steps and refused as
    /// `outside-workspace` — and return its absolute path.
    fn write_outside(&self, name: &str) -> PathBuf {
        let abs = self
            .root
            .parent()
            .expect("test setup: the root has a parent")
            .join(name);
        fs::write(&abs, b"not an image you may embed").expect("test setup: write outside file");
        abs
    }
}

/// Resolves against a note at `notes/deep/a.md`: two directories deep, so both the base directory and every pop
/// count are visible in the result. The note itself is never written — resolution reads paths, not notes.
const NOTE: &[u8] = b"notes/deep/a.md";

// ---------------------------------------------------------------- extraction

#[test]
fn both_markdown_image_forms_are_extracted() {
    // Mutation: the angle-bracket form dropped, or its `<`/`>` left inside the target so the file it names
    // (`with space.png`) stops existing.
    let body = "![diagram](./a.png)\n![wide](<with space.png>)\n";
    assert_eq!(
        extracted(body),
        vec![(b"./a.png".to_vec(), 1), (b"with space.png".to_vec(), 2)]
    );
}

#[test]
fn a_plain_link_is_not_an_image() {
    // Mutation: matching `[text](target)` as well, which would embed whatever a text link points at and count every
    // external badge in `images_seen`.
    let body = "[text](x.png) and [![nested]](notes/b.md)\n";
    assert_eq!(extracted(body), Vec::<(Vec<u8>, usize)>::new());
}

#[test]
fn an_embed_wikilink_is_not_a_markdown_image() {
    // Mutation: treating `![[scan.png]]` as markdown image syntax. It has no `(`, so it names its target through
    // the wikilink rules and belongs to `crate::links`, not here.
    let body = "![[scan.png]]\n![ok](scan.png)\n";
    assert_eq!(extracted(body), vec![(b"scan.png".to_vec(), 2)]);
}

#[test]
fn a_backtick_fence_holds_no_image_and_its_close_resumes_prose() {
    // Mutation: fence state never turned back off, so the image after the closing fence is lost as well.
    let body = "before\n```\n![hidden](hidden.png)\n```\nafter\n![shown](shown.png)\n";
    assert_eq!(extracted(body), vec![(b"shown.png".to_vec(), 6)]);
}

#[test]
fn a_tilde_fence_holds_no_image() {
    // Mutation: only ``` recognised as a fence, so a `~~~` sample leaks its example images into the run.
    let body = "~~~\n![hidden](hidden.png)\n~~~\n![shown](shown.png)\n";
    assert_eq!(extracted(body), vec![(b"shown.png".to_vec(), 4)]);
}

#[test]
fn an_unclosed_fence_hides_the_rest_of_the_file() {
    // Mutation: fence state reset at the next line, or an unterminated fence treated as prose — either way a
    // truncated paste embeds the sample images it is showing as examples. The fence runs to end of file.
    let body = "top\n```\n![one](one.png)\nstill code\n![two](two.png)\n";
    assert_eq!(extracted(body), Vec::<(Vec<u8>, usize)>::new());
}

#[test]
fn an_image_is_reported_on_its_own_line() {
    // Mutation: a 0-based line, or a counter that advances only on scanned lines — the number a report prints has to
    // be the number an editor shows.
    let body = "one\ntwo\nthree\n![here](here.png)\nfive\n";
    assert_eq!(extracted(body), vec![(b"here.png".to_vec(), 4)]);
}

#[test]
fn a_crlf_body_reports_the_same_lines_and_a_target_without_the_cr() {
    // Mutation: splitting on `\n` alone, which leaves `\r` glued to the target — `here.png\r` then refuses as
    // `unsupported-format` on every Windows-written note.
    let body = "one\r\ntwo\r\nthree\r\n![here](here.png)\r\nfive\r\n";
    assert_eq!(extracted(body), vec![(b"here.png".to_vec(), 4)]);
}

#[test]
fn two_images_on_one_line_keep_their_order() {
    // Mutation: resuming the scan at a fixed offset that swallows the second image, or keeping only the first.
    let body = "before ![one](one.png) between ![two](two.png) after\n";
    assert_eq!(
        extracted(body),
        vec![(b"one.png".to_vec(), 1), (b"two.png".to_vec(), 1)]
    );
}

#[test]
fn an_image_inside_a_link_is_still_extracted() {
    // Mutation: resuming after the outer link's `)`, which is how `[![badge](i.png)](https://shields.io)` loses its
    // image. The outer destination is prose; the image inside it is a real target.
    let body = "[![badge](i.png)](https://shields.io)\n";
    assert_eq!(extracted(body), vec![(b"i.png".to_vec(), 1)]);
}

#[test]
fn an_unterminated_image_syntax_produces_nothing() {
    // Mutation: slicing to the end of the line when no `)` closes the target, which invents a file name out of the
    // rest of the sentence.
    let body = "![broken](missing.png and then some prose\n";
    assert_eq!(extracted(body), Vec::<(Vec<u8>, usize)>::new());
}

// ------------------------------------------------------------- percent-decode

#[test]
fn a_percent_encoded_target_names_the_file_with_a_space() {
    // Mutation: no decode pass, so `kafka%20ingest.png` refuses `missing-target` while the file the author wrote
    // sits right there.
    let ws = Workspace::new();
    ws.write("notes/deep/kafka ingest.png", b"png");
    let resolved = resolve_image(ws.root(), NOTE, b"./kafka%20ingest.png")
        .expect("%20 names the file with a space");
    assert_eq!(resolved.rel_bytes, b"notes/deep/kafka ingest.png".to_vec());
    assert_eq!(resolved.mime, "image/png");
}

#[test]
fn a_double_escape_is_decoded_once() {
    // Mutation: decoding twice, which turns `%2520` into a space and embeds a different file than the author wrote.
    // One pass is what §7's "percent-decoded" means.
    let ws = Workspace::new();
    ws.write("notes/deep/kafka%20ingest.png", b"png");
    let resolved = resolve_image(ws.root(), NOTE, b"kafka%2520ingest.png")
        .expect("one decode leaves a literal %20 in the name");
    assert_eq!(
        resolved.rel_bytes,
        b"notes/deep/kafka%20ingest.png".to_vec()
    );
}

#[test]
fn a_malformed_escape_is_left_as_written_and_refuses_missing_target() {
    // Mutation: inventing `undecodable` for a malformed escape (§7 reserves that class for an accepted raster whose
    // decode fails, which is Task 4's world), or decoding `%zz` into something the author never wrote.
    let ws = Workspace::new();
    let refusal =
        resolve_image(ws.root(), NOTE, b"kafka%zz.png").expect_err("no file is named this");
    assert_eq!(refusal, MediaRefusal::MissingTarget);

    // Failing closed means passing the bytes through: a file literally called `kafka%zz.png` still resolves, because
    // nothing was rewritten out of the name.
    ws.write("notes/deep/kafka%zz.png", b"png");
    let resolved = resolve_image(ws.root(), NOTE, b"kafka%zz.png")
        .expect("a literal %zz in a file name is a legal name");
    assert_eq!(resolved.rel_bytes, b"notes/deep/kafka%zz.png".to_vec());
}

#[test]
fn an_encoded_slash_still_escapes() {
    // Mutation: decoding after normalisation, which would let `%2e%2e%2f` walk out of the workspace unnoticed. The
    // decode happens first, so an encoded traversal is refused exactly like a written one.
    let ws = Workspace::new();
    ws.write_outside("outside.png");
    assert_eq!(
        resolve_image(ws.root(), NOTE, b"%2e%2e/%2e%2e/outside.png"),
        Err(MediaRefusal::OutsideWorkspace),
        "an encoded parent step escapes the workspace just the same"
    );
}

// --------------------------------------------------------------- containment

#[test]
fn a_parent_step_out_of_the_workspace_is_refused_and_never_opened() {
    // Mutation: containment checked after the extension or after the existence probe, so an escaping path is opened
    // (or reported as merely missing) instead of refused. Both files exist above the root on purpose.
    let ws = Workspace::new();
    ws.write_outside("outside.png");
    assert_eq!(
        resolve_image(ws.root(), NOTE, b"../../../outside.png"),
        Err(MediaRefusal::OutsideWorkspace)
    );

    // The same escape with a refused extension: containment wins, so the class names the escape and not the suffix.
    ws.write_outside("outside.gif");
    assert_eq!(
        resolve_image(ws.root(), NOTE, b"../../../outside.gif"),
        Err(MediaRefusal::OutsideWorkspace),
        "containment is decided before the extension is looked at"
    );
}

#[test]
fn an_absolute_target_is_refused_whatever_its_suffix() {
    // Mutation: treating a leading `/` as workspace-root-relative, which is `crate::links`' rule for note targets.
    // §7 refuses absolute paths, and this one names a file that really exists — so reaching it would embed it.
    let ws = Workspace::new();
    let outside = ws.write_outside("absolute.png");
    assert_eq!(
        resolve_image(
            ws.root(),
            NOTE,
            &outside.to_string_lossy().into_owned().into_bytes()
        ),
        Err(MediaRefusal::OutsideWorkspace)
    );
    // The plan's example, with nothing behind it: same class, because containment comes before existence too.
    assert_eq!(
        resolve_image(ws.root(), NOTE, b"/etc/x.png"),
        Err(MediaRefusal::OutsideWorkspace)
    );
}

#[test]
fn a_parent_step_that_stays_inside_the_workspace_resolves() {
    // Mutation: refusing any `..` at all. A note that steps out and back is how `../shared/arch.png` gets written,
    // and the result is inside the workspace.
    let ws = Workspace::new();
    ws.write("notes/shared/arch.png", b"png");
    let resolved = resolve_image(ws.root(), NOTE, b"../shared/arch.png")
        .expect("the result is inside the workspace");
    assert_eq!(resolved.rel_bytes, b"notes/shared/arch.png".to_vec());
}

#[test]
fn two_spellings_of_one_path_agree() {
    // Mutation: keeping `.` and `..` in the joined path instead of walking components, so `./a/../b.png` and `b.png`
    // become two keys for one file — which is one media row per spelling.
    let ws = Workspace::new();
    ws.write("notes/deep/a/b.png", b"png");
    let walked = resolve_image(ws.root(), NOTE, b"./a/../b.png").expect("./ then .. then the name");
    let direct = resolve_image(ws.root(), NOTE, b"b.png").expect("the same file spelled directly");
    assert_eq!(walked.rel_bytes, direct.rel_bytes);
    assert_eq!(walked.rel_bytes, b"notes/deep/b.png".to_vec());
    assert_eq!(walked.abs, direct.abs);
}

#[test]
fn the_note_directory_is_the_base() {
    // Mutation: anchoring a relative target on the workspace root. `x.png` in `notes/deep/a.md` is
    // `notes/deep/x.png`, and both files exist here so a wrong anchor picks the other one silently.
    let ws = Workspace::new();
    ws.write("notes/deep/x.png", b"deep");
    ws.write("x.png", b"root");
    let resolved =
        resolve_image(ws.root(), NOTE, b"x.png").expect("the note's own directory holds the file");
    assert_eq!(resolved.rel_bytes, b"notes/deep/x.png".to_vec());
    assert_eq!(
        fs::read(&resolved.abs).expect("read the resolved file"),
        b"deep"
    );

    // A root-level note has the workspace itself as its base.
    let top =
        resolve_image(ws.root(), b"top.md", b"x.png").expect("a root note resolves at the root");
    assert_eq!(top.rel_bytes, b"x.png".to_vec());
}

// ------------------------------------------------------------------- formats

#[test]
fn an_accepted_extension_names_its_mime() {
    // Mutation: `.jpg` mapped to `image/jpg` (not what §6 stores), `.jpeg` left out of the accepted set, or the
    // extension compared case-sensitively so `diagram.PNG` is refused. The fold here judges a format, not a path.
    let ws = Workspace::new();
    for (name, mime) in [
        ("a.png", "image/png"),
        ("b.jpg", "image/jpeg"),
        ("c.jpeg", "image/jpeg"),
        ("d.PNG", "image/png"),
        ("e.Jpeg", "image/jpeg"),
    ] {
        ws.write(&format!("notes/deep/{name}"), b"png");
        let resolved = resolve_image(ws.root(), NOTE, name.as_bytes())
            .unwrap_or_else(|refusal| panic!("{name} is an accepted format, refused: {refusal:?}"));
        assert_eq!(resolved.mime, mime);
    }
}

#[test]
fn a_refused_extension_is_refused_present_and_absent() {
    // Mutation: accepting one of these because the pinned `mtmd` could probably decode it (§8: the claim that
    // matters is "this build does"), or checking existence before the extension — which is why half of these files
    // deliberately do not exist.
    let ws = Workspace::new();
    for extension in ["gif", "webp", "svg", "bmp"] {
        let absent = format!("diagram.{extension}");
        assert_eq!(
            resolve_image(ws.root(), NOTE, absent.as_bytes()),
            Err(MediaRefusal::UnsupportedFormat),
            "{extension} is not an accepted format whether or not the file is there"
        );
        ws.write(&format!("notes/deep/diagram.{extension}"), b"bytes");
        assert_eq!(
            resolve_image(ws.root(), NOTE, absent.as_bytes()),
            Err(MediaRefusal::UnsupportedFormat),
            "{extension} exists on disk and is still refused"
        );
    }
}

#[test]
fn a_path_with_no_extension_is_not_a_format() {
    // Mutation: reading the extension as "after the last dot anywhere in the target", which finds `png` in
    // `archive.png/README`, or treating a dot-only name (`.png`, a hidden file) as if it had a format.
    let ws = Workspace::new();
    for target in ["README", ".png", "no-extension", "a.png/README"] {
        assert_eq!(
            resolve_image(ws.root(), NOTE, target.as_bytes()),
            Err(MediaRefusal::UnsupportedFormat),
            "{target} names no accepted raster"
        );
    }
}

// ------------------------------------------------------------------ existence

#[test]
fn a_missing_file_refuses_missing_target() {
    // Mutation: reporting `unsupported-format` for anything absent, or recording a media row for a path that is not
    // there. The directory exists; the file does not.
    let ws = Workspace::new();
    fs::create_dir_all(ws.join("notes/deep")).expect("test setup: note directory");
    assert_eq!(
        resolve_image(ws.root(), NOTE, b"gone.png"),
        Err(MediaRefusal::MissingTarget)
    );
}

#[test]
fn a_target_that_names_no_file_refuses_missing_target() {
    // Mutation: an empty or self-only target being treated as the workspace root's own entry — or its `mime` read
    // off some other component of the path.
    let ws = Workspace::new();
    ws.write("notes/deep/keep.png", b"png");
    for target in ["", ".", "./", "a/../.."] {
        assert_eq!(
            resolve_image(ws.root(), NOTE, target.as_bytes()),
            Err(MediaRefusal::MissingTarget),
            "{target:?} names no file"
        );
    }
}

#[test]
fn a_directory_is_not_an_image() {
    // Mutation: an existence test that accepts any entry, which would have the digest of a directory read recorded
    // as an image's identity.
    let ws = Workspace::new();
    fs::create_dir_all(ws.join("notes/deep/folder.png"))
        .expect("test setup: a directory with an image suffix");
    assert_eq!(
        resolve_image(ws.root(), NOTE, b"folder.png"),
        Err(MediaRefusal::MissingTarget)
    );
}

// -------------------------------------------------------------- byte identity

/// `caf\xe9.png` — not valid UTF-8, and the name F13 exists for.
#[cfg(unix)]
const RAW_NAME: &[u8] = b"caf\xe9.png";

#[cfg(unix)]
#[test]
fn a_non_utf8_file_name_resolves_with_its_raw_bytes() {
    // Mutation: any lossy decode between the link and the lookup (`String::from_utf8_lossy`, or `Path::to_str()`),
    // which turns `caf\xe9.png` into `caf\u{FFFD}.png` — a name that exists nowhere, so the image is silently
    // refused while an author watches it fail with `missing-target`.
    use std::ffi::OsStr;
    use std::os::unix::ffi::OsStrExt;

    let ws = Workspace::new();
    let mut abs = ws.join("notes/deep");
    fs::create_dir_all(&abs).expect("test setup: note directory");
    abs.push(OsStr::from_bytes(RAW_NAME));
    fs::write(&abs, b"png").expect("test setup: write a file whose name is not valid UTF-8");

    // Written as raw bytes in the link ...
    let written =
        resolve_image(ws.root(), NOTE, RAW_NAME).expect("a non-UTF-8 name is a legal name");
    // ... and written as an escape: one byte each way, one file, one key.
    let escaped = resolve_image(ws.root(), NOTE, b"caf%E9.png")
        .expect("%E9 decodes to the byte, not to a character");
    assert_eq!(written.rel_bytes, b"notes/deep/caf\xe9.png".to_vec());
    assert_eq!(escaped.rel_bytes, written.rel_bytes);
    assert_eq!(escaped.abs, written.abs);
    assert!(
        written.abs.is_file(),
        "the OS path kept the byte it was built from"
    );
}

// --------------------------------------------------------------------- labels

#[test]
fn every_refusal_class_carries_its_spec_label() {
    // Mutation: a label spelled `outside_workspace` or `missing target`. §8's kebab-case strings are what the embed
    // report and its JSON print, and Tasks 5, 6 and 8 all reuse these same constants.
    assert_eq!(MediaRefusal::MissingTarget.label(), "missing-target");
    assert_eq!(MediaRefusal::OutsideWorkspace.label(), "outside-workspace");
    assert_eq!(
        MediaRefusal::UnsupportedFormat.label(),
        "unsupported-format"
    );
    assert_eq!(MediaRefusal::Undecodable.label(), "undecodable");
    assert_eq!(MediaRefusal::OverBudget.label(), "over-budget");
    assert_eq!(
        MediaRefusal::VisionUnavailable.label(),
        "vision-unavailable"
    );
}

#[test]
fn a_refusal_label_never_names_a_file() {
    // Mutation: a label that grows the path it refused (`unsupported-format: notes/x.png`). §8 rule 8 keeps names out
    // of refusal text so the report layer decides what to show, and a separator or a space is the fingerprint of one.
    for class in [
        MediaRefusal::MissingTarget,
        MediaRefusal::OutsideWorkspace,
        MediaRefusal::UnsupportedFormat,
        MediaRefusal::Undecodable,
        MediaRefusal::OverBudget,
        MediaRefusal::VisionUnavailable,
    ] {
        let label = class.label();
        assert!(
            !label.contains('/') && !label.contains('\\') && !label.contains(char::is_whitespace),
            "a refusal names a class, not a path: {label}"
        );
    }
}

// ---------------------------------------------------------------- sha256_file

#[test]
fn sha256_file_reports_the_digest_and_the_byte_count() {
    // Mutation: reporting `metadata` size instead of the bytes actually read, or re-spelling the digest here instead
    // of delegating to `hash::Sha256Stream` — hence comparing against the crate's own definition.
    let ws = Workspace::new();
    let content: &[u8] = b"a diagram, in bytes, of some length\n";
    let abs = ws.write("notes/deep/x.png", content);
    let (digest, size) = sha256_file(&abs).expect("read the file this test just wrote");
    assert_eq!(digest, sha256_bytes(content));
    assert_eq!(size, content.len() as u64);

    // An empty file has a real digest and no bytes; treating it as a failure would drop zero-byte attachments.
    let empty = ws.write("notes/deep/empty.png", b"");
    let (digest, size) = sha256_file(&empty).expect("an empty file still digests");
    assert_eq!(digest, sha256_bytes(b""));
    assert_eq!(size, 0);
}

#[test]
fn sha256_file_refuses_a_directory_and_a_missing_path() {
    // Mutation: swallowing the read error and returning the empty-input digest, which would then be recorded as the
    // identity of every file this call could not read.
    let ws = Workspace::new();
    fs::create_dir_all(ws.join("notes/deep/folder")).expect("test setup: a directory");
    assert!(
        sha256_file(&ws.join("notes/deep/folder")).is_err(),
        "a directory is not a file"
    );
    assert!(
        sha256_file(&ws.join("notes/deep/gone.png")).is_err(),
        "a path that is not there has no digest"
    );
}
