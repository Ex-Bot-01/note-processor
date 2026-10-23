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
    rels_from(&walk(root, &WalkOptions::default()).expect("walk must succeed"))
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

    let found = walk(tmp.path(), &WalkOptions::default()).expect("walk");
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

    let found = walk(tmp.path(), &WalkOptions::default()).expect("a raw byte name is not an error");

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

    let found = walk(tmp.path(), &WalkOptions::default()).expect("walk");

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
fn an_unreadable_root_is_the_only_error() {
    let tmp = TempDir::new().expect("test setup: tempdir");

    let err = walk(&tmp.path().join("nope"), &WalkOptions::default())
        .expect_err("a root that cannot be read must fail");
    assert!(
        err.to_string().contains("nope"),
        "the error must name the root: {err}"
    );

    // A root that is a file holds no notes, which is not an error.
    let file = tmp.path().join("note.md");
    fs::write(&file, "x").expect("test setup: write file");
    assert!(walk(&file, &WalkOptions::default())
        .expect("a file root is walkable")
        .is_empty());
}

#[test]
fn extra_excludes_are_honoured_at_every_depth() {
    let tmp = tree(&[
        ("draft.md", "d"),
        ("keep.md", "k"),
        ("notes/draft.md", "nested d"),
    ]);

    let opts = WalkOptions {
        extra_excludes: vec!["draft.md".to_string()],
    };
    let found = walk(tmp.path(), &opts).expect("walk");

    assert_eq!(rels_from(&found), ["keep.md"]);
}
