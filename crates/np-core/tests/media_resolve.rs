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
//! The digest half ([`np_core::hash::sha256_file_with_size`], which `media.rs` does not re-implement) is compared
//! against [`np_core::hash::sha256_bytes`], so a second
//! SHA-256 spelling in `media.rs` fails here rather than drifting quietly (local-embeddings §12 rule 9).

use std::fs;
use std::io::Read;
use std::path::{Path, PathBuf};

use np_core::hash::{sha256_bytes, sha256_file_with_size, sha256_reader_with_size};
use np_core::media::{
    image_kind_of_prefix, image_links, link_target_rel, resolve_image, resolve_image_in,
    MediaRefusal, PathStyle, PNG_SIGNATURE,
};
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
fn a_tilde_block_is_not_a_fence_because_the_chunker_does_not_treat_it_as_one() {
    // Mutation: recognising `~~~` here while `parse::prose_lines` keeps its column-0 backtick rule, which gives the
    // program two answers to "is this inside a code block" — the image skipped and its caption indexed as prose. This
    // module follows §5's published rule wherever that leads, so a tilde block's image is extracted like its words.
    let body = "~~~\n![example](example.png)\n~~~\n";
    assert_eq!(extracted(body), vec![(b"example.png".to_vec(), 2)]);
}

#[test]
fn an_indented_backtick_fence_is_not_a_fence_for_the_same_reason() {
    // Mutation: skipping leading whitespace here (CommonMark allows up to three spaces) while `parse` requires column
    // 0. The divergence is the defect: a fence that the chunker treats as prose has to be treated as prose here too.
    let body = "    ```\n![example](example.png)\n    ```\n";
    assert_eq!(extracted(body), vec![(b"example.png".to_vec(), 2)]);

    // Column 0 is a fence, which the case above cannot show on its own.
    let body = "```\n![hidden](hidden.png)\n```\n";
    assert_eq!(extracted(body), Vec::<(Vec<u8>, usize)>::new());
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
        resolve_image(ws.root(), NOTE, b"%2e%2e/%2e%2e/%2e%2e/outside.png"),
        Err(MediaRefusal::OutsideWorkspace),
        "an encoded parent step escapes the workspace just the same"
    );
}

// --------------------------------------------------------------- containment

#[test]
fn a_parent_step_out_of_the_workspace_is_refused() {
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
    // Normalisation is a walk over components, not a walk down the tree: `a/` is created so the spelling looks like
    // what an author would write, and the file lives where the two spellings meet.
    fs::create_dir_all(ws.join("notes/deep/a"))
        .expect("test setup: the directory the link steps into");
    ws.write("notes/deep/b.png", b"png");
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
    // `notes/deep` is a real directory: a name with no extension is refused for having no extension, before anything
    // asks whether it exists — the order §8's table implies for `.gif` applies to a bare name too. The note sits at
    // the root so every target below is spelled against the workspace itself.
    fs::create_dir_all(ws.join("notes/deep")).expect("test setup: a directory to name");
    for target in [
        "README",
        ".png",
        "no-extension",
        "a.png/README",
        "notes/deep",
    ] {
        assert_eq!(
            resolve_image(ws.root(), b"top.md", target.as_bytes()),
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
    // Mutation: an empty or self-only target being walked into the workspace root's own entry and then having its
    // `mime` read off some other component of the path.
    let ws = Workspace::new();
    ws.write("notes/deep/keep.png", b"png");
    for target in ["", ".", "./", ".//."] {
        assert_eq!(
            resolve_image(ws.root(), NOTE, target.as_bytes()),
            Err(MediaRefusal::MissingTarget),
            "{target:?} names no file"
        );
    }

    // The same shape reached by walking: `a/..` from a root-level note is the workspace root, a directory.
    assert_eq!(
        resolve_image(ws.root(), b"top.md", b"a/.."),
        Err(MediaRefusal::MissingTarget)
    );

    // A destination written empty is still a link the author wrote, and `image_links` documents producing it — dropping
    // it here (as `links.rs` drops `[x]()`) would hide the link from the report instead of counting it as a refusal.
    assert_eq!(extracted("![a]()\n"), vec![(b"".to_vec(), 1)]);

    // A NUL byte cannot name a file anywhere. The class is what is pinned, not the mechanism: on Unix the OS call would
    // refuse the same path anyway, so this case survives a mutant that deletes the explicit check (measured) and exists to
    // keep the answer stable on the platform where the conversion could report something else.
    assert_eq!(
        resolve_image(ws.root(), NOTE, b"shot%00.png"),
        Err(MediaRefusal::MissingTarget)
    );
    assert_eq!(
        resolve_image(ws.root(), NOTE, b"shot\0.png"),
        Err(MediaRefusal::MissingTarget)
    );

    // A trailing separator names a directory. `assets.png` exists as a file, and the link did not point at it.
    assert_eq!(
        resolve_image(ws.root(), NOTE, b"keep.png/"),
        Err(MediaRefusal::MissingTarget),
        "the spelling with a trailing slash names no file"
    );

    // A base that itself pops past the root is not a workspace path either — nothing escapes through `note_rel`.
    assert_eq!(
        resolve_image(ws.root(), b"../a.md", b"keep.png"),
        Err(MediaRefusal::OutsideWorkspace)
    );
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
fn a_refusal_produced_by_a_real_link_names_a_class_and_not_a_path() {
    // Mutation: a label that grows the path it refused (`unsupported-format: notes/deep/x.png`). The refusals here are
    // the ones this module actually returns, from real calls — pinning six hand-written `label()` strings instead would
    // test nothing that the previous case has not already pinned.
    let ws = Workspace::new();
    ws.write("notes/deep/clip.gif", b"gif");
    let produced = [
        resolve_image(ws.root(), NOTE, b"gone.png"),
        resolve_image(ws.root(), NOTE, b"../../../outside.png"),
        resolve_image(ws.root(), NOTE, b"clip.gif"),
    ];
    let labels: Vec<&str> = produced
        .iter()
        .map(|outcome| outcome.as_ref().expect_err("each target refuses").label())
        .collect();
    assert_eq!(
        labels,
        vec!["missing-target", "outside-workspace", "unsupported-format"]
    );
    for label in labels {
        assert!(
            !label.contains('/') && !label.contains('\\') && !label.contains(char::is_whitespace),
            "a refusal names a class, not a path: {label}"
        );
    }
}

/// The Windows spelling rules, exercised on a Linux host.
///
/// These are the escapes that only exist on a shipped Windows build: `PathBuf::push` there discards the base path when
/// the argument carries a drive or a UNC prefix, so a component that Unix reads as one odd filename silently moves the
/// whole result outside the workspace. Refusing them is logic, not an OS call, so it is testable here — which is the
/// only honest way to make a Windows claim from this machine (F18).
#[test]
fn a_windows_prefixed_target_is_refused_before_anything_is_pushed() {
    let ws = Workspace::new();
    ws.write("notes/deep/keep.png", b"png");
    for target in [
        "C:\\Users\\me\\shot.png",
        "\\\\srv\\share\\a.png",
        // Three steps up from a note two directories deep: past the root, so it escapes however it is spelled.
        "..\\..\\..\\outside.png",
        "/etc/x.png",
    ] {
        assert_eq!(
            resolve_image_in(PathStyle::Windows, ws.root(), NOTE, target.as_bytes()),
            Err(MediaRefusal::OutsideWorkspace),
            "{target:?} is rooted or escapes under Windows spelling rules"
        );
    }

    // Containment still wins over the extension: a `.gif` that escapes reports the escape.
    assert_eq!(
        resolve_image_in(
            PathStyle::Windows,
            ws.root(),
            NOTE,
            b"..\\..\\..\\outside.gif"
        ),
        Err(MediaRefusal::OutsideWorkspace)
    );

    // Two steps up from the same note lands *on* the root, which is inside: writing the file there proves the walk is
    // arithmetic and not a blanket refusal of anything containing `..`.
    ws.write("stays-inside.png", b"png");
    let resolved = resolve_image_in(
        PathStyle::Windows,
        ws.root(),
        NOTE,
        b"..\\..\\stays-inside.png",
    )
    .expect("two pops from depth two is the root, not an escape");
    assert_eq!(resolved.rel_bytes, b"stays-inside.png".to_vec());

    // And on Windows a backslash is a separator, so `a\b.png` from a note in `notes/deep/` means `notes/a/b.png`.
    ws.write("notes/a/b.png", b"png");
    let resolved = resolve_image_in(PathStyle::Windows, ws.root(), NOTE, b"..\\a\\b.png")
        .expect("the same path spelled with separators this platform uses");
    assert_eq!(resolved.rel_bytes, b"notes/a/b.png".to_vec());
}

#[test]
fn a_backslash_is_an_ordinary_byte_in_a_unix_name() {
    // Mutation: applying the Windows separator rule everywhere, which would refuse — or worse, split — a legal Unix
    // filename whose bytes include a backslash.
    let ws = Workspace::new();
    let abs = ws.write("notes/deep/we\\ird.png", b"png");
    let resolved = resolve_image_in(PathStyle::Unix, ws.root(), NOTE, b"we\\ird.png")
        .expect("a backslash is part of a name here, not a separator");
    assert_eq!(resolved.rel_bytes, b"notes/deep/we\\ird.png".to_vec());
    assert_eq!(resolved.abs, abs);
}

/// A link is not a workspace path. §7's containment promise is about the bytes a note can reach, and a directory
/// inside the vault that points at `/elsewhere` reaches them — so any component of the walk being a symlink refuses as
/// `outside-workspace`, whether it is the directory holding the file or the file itself.
#[cfg(unix)]
#[test]
fn a_symlinked_directory_or_file_is_outside_the_workspace() {
    use std::os::unix::fs::symlink;

    let ws = Workspace::new();
    ws.write("notes/deep/keep.png", b"png");
    let outside = ws.write_outside("through-a-link.png");

    // A directory link inside the workspace pointing above the root.
    let link_dir = ws.join("notes/deep/elsewhere");
    symlink(
        outside.parent().expect("test setup: parent exists"),
        &link_dir,
    )
    .expect("test setup: directory symlink");
    assert_eq!(
        resolve_image(ws.root(), NOTE, b"elsewhere/through-a-link.png"),
        Err(MediaRefusal::OutsideWorkspace),
        "the components are all inside; the bytes are not"
    );

    // A file link pointing at a real image outside the workspace.
    symlink(&outside, ws.join("notes/deep/looked-local.png")).expect("test setup: file symlink");
    assert_eq!(
        resolve_image(ws.root(), NOTE, b"looked-local.png"),
        Err(MediaRefusal::OutsideWorkspace)
    );

    // Ordinary directories still resolve — the guard is not "no symlinks anywhere on the host".
    let resolved =
        resolve_image(ws.root(), NOTE, b"keep.png").expect("a plain file inside resolves");
    assert_eq!(resolved.rel_bytes, b"notes/deep/keep.png".to_vec());
}

/// The Windows name conversion, on the platform that needs it.
///
/// `component_name`'s non-unix arm is the only spelling path Windows takes, and a claim about what it does with bytes
/// that are not valid UTF-16 would otherwise rest on nobody having run it. Writing the **lossy** name to disk is the
/// point: if the resolver substituted U+FFFD instead of refusing, this file would be found and the case would fail.
#[cfg(not(unix))]
#[test]
fn a_name_this_platform_cannot_spell_refuses_instead_of_substituting() {
    let ws = Workspace::new();
    // The replacement-character spelling of `caf\xe9.png` — what a lossy decode would have looked for.
    ws.write("notes/deep/caf\u{FFFD}.png", b"png");
    assert_eq!(
        resolve_image(ws.root(), NOTE, b"caf\xe9.png"),
        Err(MediaRefusal::MissingTarget),
        "the raw bytes name no file here, and the lossy spelling must not be tried in their place"
    );
}

/// A destination with a title is one reference, not a filename that happens to end in quotes.
#[test]
fn a_title_after_the_destination_is_not_part_of_the_name() {
    // Mutation: taking everything to `)` as the name, which turns the extension into `png "Kafka ingest"` and refuses
    // a file that exists — with `unsupported-format`, a class §8 reserves for extensions outside the accepted set.
    let ws = Workspace::new();
    ws.write("notes/deep/kafka.png", b"png");
    // The cut belongs to the syntax reader, so each case is written as the note spells it and then resolved — the pair
    // is what a run does, and resolving the raw bytes with a title in them would be testing something no caller sends.
    for body in [
        "![d](kafka.png \"Kafka ingest\")",
        "![d](kafka.png 'one two')",
        "![d](kafka.png (parenthesised))",
    ] {
        let links = image_links(body.as_bytes());
        assert_eq!(links.len(), 1, "{body:?}");
        assert_eq!(
            links[0].target,
            b"kafka.png".to_vec(),
            "the title is not part of the destination"
        );
        let resolved = resolve_image(ws.root(), NOTE, &links[0].target)
            .unwrap_or_else(|err| panic!("{body:?} names the file that exists: {err:?}"));
        assert_eq!(resolved.rel_bytes, b"notes/deep/kafka.png".to_vec());
    }

    // A name with a real space still needs the angle form: an unescaped space ends the destination, so the note below
    // names `my`, which has no extension at all. That is a syntax fact, visible in what extraction hands on — and
    // `resolve_image` itself never cuts at spaces, because by then it has a name rather than a destination: handing it
    // the raw bytes finds no file with that spelling and refuses `missing-target`.
    let links = image_links("![d](my diagram.png)\n".as_bytes());
    assert_eq!(links[0].target, b"my".to_vec(), "the space ended the name");
    assert_eq!(
        resolve_image(ws.root(), NOTE, &links[0].target),
        Err(MediaRefusal::UnsupportedFormat)
    );
    assert_eq!(
        resolve_image(ws.root(), NOTE, b"my diagram.png"),
        Err(MediaRefusal::MissingTarget),
        "resolution takes the name as given; only the reader splits destinations"
    );
    // Spelled the way markdown wants, the same file resolves — and the brackets are gone by the time the target is a
    // name, because `image_links` strips them at syntax level. `resolve_image` never sees `<…>`: handing it those bytes
    // literally would look for a file whose name starts with `<`, which is what it should do.
    ws.write("notes/deep/my diagram.png", b"png");
    let links = image_links(b"![d](<my diagram.png>)\n");
    assert_eq!(links.len(), 1, "the angle form yields one link");
    assert_eq!(links[0].target, b"my diagram.png".to_vec());
    assert_eq!(
        resolve_image(ws.root(), NOTE, &links[0].target)
            .expect("the extracted target carries the space")
            .rel_bytes,
        b"notes/deep/my diagram.png".to_vec()
    );
}

// ---------------------------------------------------------------- digest and count

#[test]
fn sha256_file_with_size_reports_the_digest_and_the_byte_count() {
    // Mutation: re-spelling the digest here instead of going through `hash`, which is why the expected value comes from
    // the crate's own definition rather than a constant. What this case does **not** prove is that the count is the
    // bytes read rather than `metadata` length — both fixtures are healthy regular files where those numbers are equal,
    // so nothing here would notice that swap. The claim worth making is narrower: the number is what was digested.
    let ws = Workspace::new();
    let content: &[u8] = b"a diagram, in bytes, of some length\n";
    let abs = ws.write("notes/deep/x.png", content);
    let (digest, size) = sha256_file_with_size(&abs).expect("read the file this test just wrote");
    assert_eq!(digest, sha256_bytes(content));
    assert_eq!(size, content.len() as u64);

    // An empty file has a real digest and no bytes; treating it as a failure would drop zero-byte attachments.
    let empty = ws.write("notes/deep/empty.png", b"");
    let (digest, size) = sha256_file_with_size(&empty).expect("an empty file still digests");
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
        sha256_file_with_size(&ws.join("notes/deep/folder")).is_err(),
        "a directory is not a file"
    );
    assert!(
        sha256_file_with_size(&ws.join("notes/deep/gone.png")).is_err(),
        "a path that is not there has no digest"
    );
}

// ---------------------------------------------------------------- content, not filename

/// §7's promise is "png/jpg/jpeg only", and the extension gate alone does not keep it: measured 2026-11-03, a BMP payload
/// under a `.png` name reached the pinned decoder and produced a vector at 80 image tokens. This function is what closes
/// that gap (backlog F26), so its table is the whole promise in one place.
#[test]
fn the_content_check_accepts_png_and_jpeg_signatures_only() {
    let mut png = Vec::new();
    png.extend_from_slice(PNG_SIGNATURE);
    png.extend_from_slice(b"IHDR and the rest of a file");
    let mut jpeg = Vec::new();
    jpeg.extend_from_slice(&[0xff, 0xd8, 0xff, 0xe0]);
    jpeg.extend_from_slice(b"JFIF and the rest");

    assert_eq!(image_kind_of_prefix(&png), Some("image/png"));
    assert_eq!(image_kind_of_prefix(&jpeg), Some("image/jpeg"));

    // Everything else the decoder would happily have eaten, plus the shapes that are not formats at all. Each of these
    // resolving to `Some` would store a vector whose row came from a format §7 never promised.
    let bmp = b"BM\x36\x00\x00\x00\x00\x00";
    let gif = b"GIF89a\x01\x00";
    let mut webp = Vec::new();
    webp.extend_from_slice(b"RIFF\x10\x00\x00\x00WEBPVP8 ");
    for (name, prefix) in [
        ("bmp", bmp.as_slice()),
        ("gif", gif.as_slice()),
        ("webp", webp.as_slice()),
        ("tga", b"\x10\x00\x02\x00\x00\x00"),
        (
            "a truncated jpeg (two of three marker bytes)",
            &[0xff, 0xd8][..],
        ),
        ("an empty file", b"".as_slice()),
        (
            "a png signature with its last byte off",
            b"\x89PNG\r\n\x1b\x0a",
        ),
    ] {
        assert_eq!(
            image_kind_of_prefix(prefix),
            None,
            "{name} is not one of the two formats §7 promises, so it must answer nothing"
        );
    }
}

/// The division of labour between the resolver and the content check, pinned: resolution answers *where* bytes live and
/// succeeds here; the content check answers *what* they are and refuses. Collapsing the two (checking bytes inside
/// `resolve_image`) would report a real file as `missing-target`, which is a different statement about the vault.
#[test]
fn a_mislabelled_file_resolves_and_then_fails_the_content_check() {
    let ws = Workspace::new();
    // A BMP, named `.png`, inside the workspace, linked from a note next to it — exactly the F26 shape.
    let mut bmp = b"BM\x36\x00\x00\x00\x00\x00".to_vec();
    bmp.extend_from_slice(&[0u8; 64]);
    ws.write("notes/deep/diagram.png", &bmp);

    let resolved = resolve_image(
        ws.root(),
        b"notes/deep/note.md",
        b"diagram.png",
    )
    .expect("the path is inside the workspace and the file exists, so resolution is not the thing that fails here");
    assert_eq!(resolved.rel_bytes, b"notes/deep/diagram.png".to_vec());

    let mut file = fs::File::open(&resolved.abs).expect("test setup: read what was just written");
    let mut prefix = [0u8; 8];
    std::io::Read::read_exact(&mut file, &mut prefix)
        .expect("the fixture is longer than the prefix");
    assert_eq!(
        image_kind_of_prefix(&prefix),
        None,
        "a BMP named .png must be refused by content — accepting it is the defect F26 records as measured"
    );
}

/// The digest and the peek have to describe **one read**: a caller that sniffed eight bytes, then re-opened for
/// [`sha256_file_with_size`], could store a size from one moment and a signature from another. Chaining is what avoids
/// that, and this pins that chaining neither drops the prefix nor counts it twice.
#[test]
fn a_chained_prefix_digests_like_the_whole_file() {
    let ws = Workspace::new();
    let content: &[u8] = b"\x89PNG\r\n\x1a\nthen a great many bytes of diagram follow this line\n";
    let abs = ws.write("notes/deep/real.png", content);

    // The caller's shape: read a prefix, then digest prefix-then-rest from the same handle.
    let mut file = fs::File::open(&abs).expect("test setup: open the fixture");
    let mut prefix = vec![0u8; 8];
    std::io::Read::read_exact(&mut file, &mut prefix)
        .expect("the fixture is longer than the prefix");
    let mut chained = prefix.as_slice().chain(file);
    let (digest, size) =
        sha256_reader_with_size(&mut chained).expect("digesting a chained reader cannot fail here");

    assert_eq!(
        digest,
        sha256_bytes(content),
        "the digest must cover every byte once"
    );
    assert_eq!(
        size,
        content.len() as u64,
        "the count must be the whole file, prefix included"
    );
    assert_eq!(
        image_kind_of_prefix(&prefix),
        Some("image/png"),
        "and the same bytes that were digested are the bytes that were classified"
    );

    // Same function, no prefix: identical to the file helper, so there is still one digest definition (rule 9).
    let mut again = fs::File::open(&abs).expect("test setup: reopen");
    let (via_reader, reader_size) =
        sha256_reader_with_size(&mut again).expect("read it again from the start");
    let (via_file, file_size) =
        sha256_file_with_size(&abs).expect("digest through the path helper");
    assert_eq!(via_reader, via_file);
    assert_eq!(reader_size, file_size);
}

// ---------------------------------------------------------------- link placement (no filesystem)

/// [`np_core::media::link_target_rel`] answers §7 rule 4's question — "does this note link this path?" — from the text
/// alone, and its whole job is to refuse **exactly** what the resolver refuses before and during the walk. Review caught
/// the first version omitting the rooted test: `![d](/assets/d.png)` refuses as `outside-workspace`, yet the helper returned
/// `assets/d.png` and kept that row and its vector alive indefinitely, long after the author deleted the real link.
#[test]
fn link_placement_agrees_with_the_resolver_about_what_counts_as_a_link() {
    let note = b"notes/deep/note.md";
    let placed = |target: &[u8]| link_target_rel(PathStyle::Unix, note, target);

    // Targets that do name a workspace figure path, whether or not a file is there.
    assert_eq!(
        placed(b"diagram.png").as_deref(),
        Some(&b"notes/deep/diagram.png"[..])
    );
    assert_eq!(
        placed(b"%64iagram.png").as_deref(),
        Some(&b"notes/deep/diagram.png"[..]),
        "`%64` is `d`: one escape, decoded exactly once, the same way the resolver decodes it"
    );
    assert_eq!(
        placed(b"../shared/arch.jpeg").as_deref(),
        Some(&b"notes/shared/arch.jpeg"[..]),
        "a parent step that stays inside the vault is a link, not an escape"
    );
    assert_eq!(
        placed(b"../../outside.png").as_deref(),
        Some(&b"outside.png".to_vec()[..]),
        "two steps up from `notes/deep/` is the root, which is still inside the vault"
    );
    assert_eq!(
        placed(b"my%20diagram.PNG").as_deref(),
        Some(&b"notes/deep/my diagram.PNG".to_vec()[..]),
        "byte-exact: no case fold on the path this protects"
    );

    // And the refusals, each of which must protect nothing.
    for (name, target) in [
        ("an absolute destination", b"/assets/diagram.png".as_slice()),
        ("a NUL byte", b"diag\x00ram.png".as_slice()),
        ("the note's own directory", b".".as_slice()),
        ("a directory", b"sub/".as_slice()),
        // Two parent steps from `notes/deep/` land on the workspace root, which is inside; the third pops past it.
        (
            "an escape past the root",
            b"../../../outside.png".as_slice(),
        ),
        ("an unsupported extension", b"clip.gif".as_slice()),
        ("no extension at all", b"LICENCE".as_slice()),
        (
            "an inline payload",
            b"data:image/png;base64,AAAA".as_slice(),
        ),
    ] {
        assert_eq!(
            placed(target),
            None,
            "{name} never linked a workspace figure path, so it must not keep one alive"
        );
    }

    // Windows rules are the same conversation with different separators: a drive prefix or a UNC share is rooted there, and
    // `..` that pops past the root still refuses. Written and executed here; the platform half stays unmeasured (F18).
    assert_eq!(
        link_target_rel(PathStyle::Windows, b"notes\\a.md", b"C:\\shots\\a.png"),
        None,
        "a drive-prefixed target names no workspace path"
    );
    assert_eq!(
        link_target_rel(PathStyle::Windows, b"notes\\a.md", b"\\\\srv\\share\\a.png"),
        None
    );
    assert_eq!(
        link_target_rel(
            PathStyle::Windows,
            b"notes\\deep\\a.md",
            b"..\\arch.jpeg".as_slice()
        )
        .as_deref(),
        Some(&b"notes/arch.jpeg"[..]),
        "a backslash parent step inside the vault is a link under the Windows rules"
    );
}
/// The same target judged by both questions, in one place: wherever the resolver **succeeds**, placement agrees about the
/// path it succeeded on — so the sweep can never protect a different path from the one the pass just wrote.
#[test]
fn where_resolution_succeeds_placement_agrees_on_the_path() {
    let ws = Workspace::new();
    ws.write("notes/deep/diagram.png", b"png");
    ws.write("notes/shared/arch.jpeg", b"jpeg");
    ws.write("top.png", b"png");
    ws.write("notes/deep/my diagram.png", b"png");
    ws.write("notes/deep/a.md", b"body\n");
    ws.write("a.md", b"body\n");

    for (note, target) in [
        ("notes/deep/a.md", "diagram.png"),
        ("notes/deep/a.md", "../shared/arch.jpeg"),
        ("notes/deep/a.md", "my%20diagram.png"),
        ("a.md", "top.png"),
    ] {
        let image = resolve_image(ws.root(), note.as_bytes(), target.as_bytes())
            .expect("the fixture created this figure");
        assert_eq!(
            Some(image.rel_bytes),
            link_target_rel(PathStyle::Unix, note.as_bytes(), target.as_bytes()),
            "placement and resolution disagree for `{note}` -> `{target}`: the sweep would guard one path while the pass \
             wrote another"
        );
    }
}
