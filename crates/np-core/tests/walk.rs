//! Workspace discovery: deterministic order, exclusions, long-path helper.
//!
//! Every tree is built inside a `tempfile::TempDir`, so no test touches a real
//! workspace; the only absolute paths are the ones `walk` itself produces.

use std::fs;
use std::path::{Path, PathBuf};

use np_core::walk::{long_prefix, to_open_path, walk, DiscoveredFile, WalkOptions};
use tempfile::TempDir;

/// Build a workspace holding `files` as `(relative path, contents)` and return it.
/// `/` in a relative path means a directory, so the same tree builds on every OS.
fn tree(files: &[(&str, &str)]) -> TempDir {
    let tmp = TempDir::new().expect("test setup: tempdir");
    for (rel, body) in files {
        let path = tmp.path().join(rel);
        if let Some(parent) = path.parent() {
            fs::create_dir_all(parent).expect("test setup: create parent directory");
        }
        fs::write(&path, body).expect("test setup: write file");
    }
    tmp
}

/// `walk` that must succeed, mapped to its `rel` list.
fn rels(root: &Path) -> Vec<String> {
    rels_from(&walk_files(root))
}

/// `walk` with default options that must succeed, returning the files it found.
fn walk_files(root: &Path) -> Vec<DiscoveredFile> {
    walk(root, &WalkOptions::default())
        .expect("walk must succeed")
        .files
}

fn rels_from(found: &[DiscoveredFile]) -> Vec<String> {
    found.iter().map(|file| file.rel.clone()).collect()
}

#[test]
fn sorted_and_normalized() {
    let tmp = tree(&[
        ("Zeta/b.md", "b"),
        ("alpha/c.md", "c"),
        ("a/Notes/d.md", "dddd"),
        ("top.md", "top"),
    ]);

    let found = walk_files(tmp.path());
    let keys: Vec<&str> = found.iter().map(|file| file.case_key.as_str()).collect();
    // `/` (0x2F) sorts before every letter, so `a/notes/…` precedes `alpha/…`.
    assert_eq!(
        keys,
        ["a/notes/d.md", "alpha/c.md", "top.md", "zeta/b.md"],
        "case_key must come back ascending"
    );

    for file in &found {
        assert!(!file.rel.is_empty(), "every entry has a rel: {file:?}");
        assert!(
            !file.rel.starts_with("./") && !file.rel.starts_with('/'),
            "rel must have no leading ./ or /: {:?}",
            file.rel
        );
        assert!(!file.rel.contains('\\'), "rel must use /: {:?}", file.rel);
        assert_eq!(file.case_key, file.rel.to_lowercase());
        assert!(
            file.abs.is_file(),
            "abs must be the real path: {:?}",
            file.abs
        );
        assert!(file.is_markdown, "{:?} is markdown", file.rel);
    }

    // `rel` keeps the written spelling; only `case_key` is folded.
    let deep = found
        .iter()
        .find(|file| file.rel == "a/Notes/d.md")
        .expect("nested file is found");
    assert_eq!(deep.size, 4, "size is bytes on disk");
    assert!(deep.mtime > 0, "mtime is epoch seconds: {}", deep.mtime);
    assert!(deep.abs.starts_with(tmp.path()));
}

#[test]
fn excludes_derived_and_hidden() {
    let tmp = tree(&[
        ("INDEX.md", "catalog"),
        ("INDEX-a-m.md", "shard"),
        ("processed/x.md", "derived"),
        (".np/state.sqlite", "db"),
        (".git/config", "cfg"),
        ("notes/INDEX-old.md", "a real note"),
        ("notes/.draft.md", "hidden"),
        ("notes/processed/y.md", "derived again"),
        ("notes/a.md", "a"),
    ]);

    let found = rels(tmp.path());

    for excluded in [
        "INDEX.md",
        "INDEX-a-m.md",
        "processed/x.md",
        ".np/state.sqlite",
        ".git/config",
        "notes/.draft.md",
        "notes/processed/y.md",
    ] {
        assert!(
            !found.iter().any(|rel| rel == excluded),
            "{excluded} must not be tracked, got {found:?}"
        );
    }
    assert!(
        found.contains(&"notes/INDEX-old.md".to_string()),
        "only the root-level catalog names are excluded, got {found:?}"
    );
    // `a.md` precedes `INDEX-old.md` once both are lowercased.
    assert_eq!(found, ["notes/a.md", "notes/INDEX-old.md"]);
}

#[test]
fn symlink_loop_does_not_hang() {
    let tmp = tree(&[("a/b.md", "b"), ("a/c.md", "c")]);
    let dir = tmp.path().join("a");
    #[cfg(unix)]
    std::os::unix::fs::symlink(&dir, dir.join("loop")).expect("test setup: symlink to own parent");
    #[cfg(windows)]
    match std::os::windows::fs::symlink_dir(&dir, dir.join("loop")) {
        Ok(()) => {}
        // Without the SeCreateSymbolicLink privilege there is no loop to build.
        Err(err) => {
            eprintln!("skipping: cannot create symlink on this machine: {err}");
            return;
        }
    }

    assert_eq!(rels(tmp.path()), ["a/b.md", "a/c.md"]);
}

#[cfg(unix)]
#[test]
fn non_utf8_filename_recorded() {
    use std::ffi::OsString;
    use std::os::unix::ffi::{OsStrExt, OsStringExt};

    let tmp = TempDir::new().expect("test setup: tempdir");
    let mut raw = b"we".to_vec();
    raw.push(0xffu8);
    raw.extend_from_slice(b".md");
    let name = OsString::from_vec(raw);
    let path = tmp.path().join(&name);
    fs::write(&path, b"body").expect("test setup: write non-UTF-8 named file");

    let found = walk_files(tmp.path());

    assert_eq!(found.len(), 1, "expected one entry, got {found:?}");
    let file = &found[0];
    assert!(!file.rel.is_empty(), "rel must still be recorded: {file:?}");
    assert!(file.is_markdown);
    // `abs` keeps the raw bytes, which is what lets a later open() read the file.
    assert_eq!(file.abs.as_os_str().as_bytes(), path.as_os_str().as_bytes());
    assert_eq!(fs::read(&file.abs).expect("open via abs"), b"body");
}

#[test]
fn long_path_gets_prefix_only_when_needed() {
    let long = format!("C:\\notes\\{}", "a".repeat(300));
    let prefixed = long_prefix(&long, true);
    assert!(prefixed.starts_with(r"\\?\"), "got {prefixed}");
    assert_eq!(
        prefixed.matches(r"\\?\").count(),
        1,
        "the prefix must be added exactly once"
    );
    assert_eq!(&prefixed[4..], long);

    // The rule is "260 or more", measured in characters.
    let at = "y".repeat(260);
    assert!(long_prefix(&at, true).starts_with(r"\\?\"));
    let below = "y".repeat(259);
    assert_eq!(long_prefix(&below, true), below);

    let short = "C:\\notes\\daily\\xy.md";
    assert_eq!(short.chars().count(), 20);
    assert_eq!(long_prefix(short, true), short, "a short path is unchanged");

    let already = format!(r"\\?\{long}");
    assert_eq!(
        long_prefix(&already, true),
        already,
        "already prefixed stays put"
    );

    // Off Windows the prefix would be wrong, so the flag alone decides.
    assert_eq!(long_prefix(&long, false), long);
}

#[test]
fn to_open_path_follows_the_prefix_rule_for_this_platform() {
    let long = "x".repeat(300);
    let got = to_open_path(Path::new(&long));
    if cfg!(windows) {
        assert_eq!(got, PathBuf::from(format!(r"\\?\{long}")));
    } else {
        assert_eq!(got, PathBuf::from(&long), "identity off Windows");
    }
}

#[test]
fn non_markdown_files_are_tracked_too() {
    // Non-markdown files get a registry row later and can be link targets, so the
    // walk must hand them over with `is_markdown` false rather than drop them.
    let tmp = tree(&[
        ("notes/photo.png", "binary-ish"),
        ("notes/Makefile", "make"),
        ("notes/UPPER.MD", "a note"),
    ]);

    let found = walk_files(tmp.path());

    assert_eq!(
        rels_from(&found),
        ["notes/Makefile", "notes/photo.png", "notes/UPPER.MD"]
    );
    let markdown: Vec<&str> = found
        .iter()
        .filter(|file| file.is_markdown)
        .map(|file| file.rel.as_str())
        .collect();
    assert_eq!(
        markdown,
        ["notes/UPPER.MD"],
        "is_markdown marks the .md names, case-insensitively"
    );
}

#[test]
fn a_root_that_cannot_be_read_is_an_error() {
    let tmp = TempDir::new().expect("test setup: tempdir");

    let err = walk(&tmp.path().join("nope"), &WalkOptions::default())
        .expect_err("a missing root must fail, not report an empty workspace");
    assert!(
        err.to_string().contains("nope"),
        "the error must name the root: {err}"
    );

    // A file is not a readable directory either. An empty list here would read as
    // "the workspace lost every note", which is exactly the mistake Task 10 must not
    // make, so nothing about a bad root may come back as `Ok`.
    let file = tmp.path().join("note.md");
    fs::write(&file, "x").expect("test setup: write file");
    let err =
        walk(&file, &WalkOptions::default()).expect_err("a root that is a file must fail too");
    assert!(
        err.to_string().contains("note.md"),
        "the error must name the root: {err}"
    );
}

/// A subdirectory nobody may read must be named, not quietly skipped: an absent
/// subtree and an unreadable one look identical in `files` alone.
#[cfg(unix)]
#[test]
fn an_unreadable_directory_is_reported_not_silently_missing() {
    use std::os::unix::fs::PermissionsExt;

    let tmp = tree(&[
        ("keep.md", "k"),
        ("secret/a.md", "a"),
        ("secret/deep/b.md", "b"),
    ]);
    let secret = tmp.path().join("secret");
    // The mode the directory actually arrived with. `TempDir` creates it as `0o777 & !umask`, so a
    // hardcoded 0o755 would *widen* permissions on a runner with a restrictive umask (0o077 gives
    // 0o700); whatever was there is what the guard puts back.
    let found_mode = fs::metadata(&secret)
        .expect("test setup: read the directory mode")
        .permissions()
        .mode()
        & 0o7777; // only the permission bits travel back; the file-type bits are not ours to set
    fs::set_permissions(&secret, fs::Permissions::from_mode(0o0))
        .expect("test setup: chmod 0 the subdirectory");
    // Declared before any assertion so the mode goes back on every path out of this test,
    // panic included; `TempDir` cleanup cannot remove a tree holding a mode-0 directory.
    let _readable_again = ReadableAgain {
        dir: secret.clone(),
        mode: found_mode,
    };
    if fs::read_dir(&secret).is_ok() {
        // Running as root (common in containers) reads a mode-0 directory anyway.
        eprintln!("skipping: this machine can read a mode-0 directory");
        return;
    }

    let walked = walk(tmp.path(), &WalkOptions::default())
        .expect("one unreadable directory must not fail the walk");

    assert_eq!(rels_from(&walked.files), ["keep.md"]);
    assert_eq!(
        walked.unreadable,
        ["secret"],
        "the unreadable directory is named by its rel-style path"
    );

    // Restoration asserted rather than assumed: dropping the guard here runs it early, and what comes
    // back has to be the mode this machine made — `0o700` under a `0o077` umask, where restoring a fixed
    // `0o755` would have handed group and other read+execute on a directory nobody asked for. The mode of
    // a sibling made by the same umask is the premise, so a hardcoded value cannot pass this quietly.
    drop(_readable_again);
    let restored = fs::metadata(&secret)
        .expect("test: re-read the directory mode")
        .permissions()
        .mode()
        & 0o7777;
    assert_eq!(
        restored, found_mode,
        "the guard restores the mode it captured"
    );
    let plain = tmp.path().join("plain");
    fs::create_dir(&plain).expect("test setup: a directory made by this umask");
    assert_eq!(
        restored,
        fs::metadata(&plain)
            .expect("test: read the sibling mode")
            .permissions()
            .mode()
            & 0o7777,
        "and that mode is this machine's own, not an assumed one"
    );
}

/// Restores `dir` to the mode it carried when the guard was created.
///
/// A mode-0 directory makes `TempDir::drop` fail, and a leaked one stays unwritable on the
/// machine forever, so the restoration may not depend on reaching the end of a test body.
/// The mode travels in rather than being assumed: restoring a fixed 0o755 would grant read and
/// execute to group and other on a runner whose umask made the directory narrower to begin with.
#[cfg(unix)]
struct ReadableAgain {
    dir: PathBuf,
    /// The permission bits `fs::metadata` reported before the test chmod'd the directory.
    mode: u32,
}

#[cfg(unix)]
impl Drop for ReadableAgain {
    fn drop(&mut self) {
        use std::os::unix::fs::PermissionsExt;

        // Best effort: `Drop` must not panic, and a failure here only repeats the leak the
        // guard exists to prevent.
        let _ = fs::set_permissions(&self.dir, fs::Permissions::from_mode(self.mode));
    }
}

/// Sibling of `symlink_loop_does_not_hang`, which cannot see `follow_links`: walkdir's
/// own same-file check drops a directory loop even when links are followed. A link to
/// a *file* is the case that changes — following it would record the link as a second
/// copy of its target, so it gets hashed twice and becomes a duplicate note.
#[cfg(unix)]
#[test]
fn symlink_to_a_file_is_not_recorded() {
    let tmp = tree(&[("target.md", "the real note")]);
    std::os::unix::fs::symlink(tmp.path().join("target.md"), tmp.path().join("link.md"))
        .expect("test setup: symlink to a file");

    let found = walk_files(tmp.path());

    assert_eq!(rels_from(&found), ["target.md"]);
    for file in &found {
        assert!(
            !file.abs.is_symlink(),
            "a tracked path must be a real file, not a link: {:?}",
            file.abs
        );
    }
}

#[test]
fn extra_excludes_are_honoured_at_every_depth() {
    let tmp = tree(&[
        ("draft.md", "d"),
        ("keep.md", "k"),
        ("notes/draft.md", "nested d"),
        ("notes/Draft.MD", "same name, other case"),
    ]);

    // The configured spelling is upper case on purpose: both sides fold before they
    // are compared, because Windows path comparison does (spec §11).
    let opts = WalkOptions {
        extra_excludes: vec!["DRAFT.md".to_string()],
    };
    let walked = walk(tmp.path(), &opts).expect("walk");

    assert_eq!(rels_from(&walked.files), ["keep.md"]);
    assert!(walked.unreadable.is_empty(), "nothing was unreadable");
}

/// Nothing a normal tree can name is refused: [`Walked::unrepresentable`] stays empty, and every file on
/// disk reaches [`Walked::files`].
///
/// The F18 refusal has to be inert for names the platform spells exactly, so this runs on both platforms CI
/// builds — and on Unix it includes the raw-byte population F13 exists for (`caf\xe9.md`), because a
/// mitigation that silently skipped a real Unix note would cost more than the Windows overwrite it prevents.
/// The other direction — a name that *is* refused — needs a lone surrogate, which only Windows can put in a
/// file name; that is [`an_unpaired_surrogate_name_is_refused`], and its absence from this machine's run is
/// exactly why `path_keys::has_unpaired_surrogate`'s units are the coverage of the rule.
#[test]
fn nothing_a_normal_tree_can_name_is_refused() {
    let tmp = tree(&[
        ("notes/a.md", "a"),
        ("caf\u{e9}.md", "cafe, spelled in UTF-8"),
        ("UPPER.MD", "u"),
    ]);
    // The same name as raw bytes rather than as text: on Unix that is a *second*, distinct file, and the
    // walk must key it exactly like every other one.
    #[cfg(unix)]
    {
        use std::ffi::OsString;
        use std::os::unix::ffi::OsStringExt;
        let raw = OsString::from_vec(b"caf\xe9.md".to_vec());
        fs::write(tmp.path().join(&raw), b"latin-1").expect("test setup: write raw-byte name");
    }

    let walked = walk(tmp.path(), &WalkOptions::default()).expect("walk must succeed");

    assert!(
        walked.unrepresentable.is_empty(),
        "no ordinary name may be refused: {:?}",
        walked.unrepresentable
    );
    // Three names from the tree, plus the raw-byte one only Unix can build.
    let expected = if cfg!(unix) { 4 } else { 3 };
    assert_eq!(
        walked.files.len(),
        expected,
        "every file on disk is indexed: {:?}",
        rels_from(&walked.files)
    );
}

/// F18's refusal at the level of a real file — **written for a Windows machine to run, not for Linux CI.**
///
/// This cannot execute here: the module is compiled out on Linux, and no Linux syscall can hand `create()` an
/// unpaired surrogate. So do not count this test as coverage of the rule — `path_keys`' predicate units are
/// what CI runs and what F18's design doc names as the part that carries the weight. On a Windows box
/// (`cargo test --test walk`) it is the end-to-end check: two live names differing only in an unpaired
/// surrogate, which `to_string_lossy` spells identically, must both stay out of the catalog rather than one
/// overwriting the other's row.
///
/// If even Windows refuses to *create* such a name, the test says so on stderr and returns without asserting:
/// a silent pass here would be indistinguishable from the coverage this test cannot provide on Linux.
#[cfg(windows)]
#[test]
fn an_unpaired_surrogate_name_is_refused() {
    use std::ffi::OsString;
    use std::os::windows::ffi::OsStringExt;

    let tmp = TempDir::new().expect("test setup: tempdir");
    // The overwrite pair: `caf<U+D800>.md` and `caf<U+D801>.md` are two names, one lossy spelling.
    let twin = |surrogate: u16| -> OsString {
        OsString::from_wide(
            &"caf"
                .encode_utf16()
                .chain([surrogate])
                .chain(".md".encode_utf16())
                .collect::<Vec<u16>>(),
        )
    };
    let twins = [twin(0xD800), twin(0xD801)];
    for (index, name) in twins.iter().enumerate() {
        if let Err(err) = fs::write(tmp.path().join(name), format!("note {index}")) {
            eprintln!(
                "NOT COVERAGE: this Windows refused to create a name holding an unpaired surrogate \
                 ({err}); run this test by hand and rely on the path_keys units meanwhile"
            );
            return;
        }
    }
    // A *complete* pair is U+10000 — one ordinary character, and an ordinary name.
    fs::write(tmp.path().join("\u{10000}.md"), "kept").expect("test setup: write valid-pair name");

    let walked = walk(tmp.path(), &WalkOptions::default()).expect("walk must succeed");

    for name in &twins {
        assert!(
            fs::metadata(tmp.path().join(name)).is_ok(),
            "the refused file is on disk, so this is a refusal and not a missing entry"
        );
    }
    assert_eq!(
        rels_from(&walked.files),
        ["\u{10000}.md"],
        "neither twin is indexed; the valid pair is"
    );
    // One line for two files: the report names display spellings, and both twins share this one.
    assert_eq!(walked.unrepresentable, ["caf\u{fffd}.md"]);
}
