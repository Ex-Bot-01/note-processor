//! Content hashing: lowercase hex, streamed reads, honest failure.
//!
//! `tests/change.rs` stays filesystem-free; this file is where `sha256_file` gets its
//! coverage, because reading in chunks is exactly the kind of loop that passes on a
//! one-line fixture and breaks on a real note.

use std::fs;
use std::path::Path;

use np_core::hash::{sha256_bytes, sha256_file};
use tempfile::TempDir;

#[test]
fn sha256_is_lowercase_hex_of_a_known_vector() {
    // Published digests, so the formatting (lowercase, 64 chars, no separators) is
    // pinned by something other than this crate's own output.
    assert_eq!(
        sha256_bytes(b"abc"),
        "ba7816bf8f01cfea414140de5dae2223b00361a396177a9cb410ff61f20015ad"
    );
    assert_eq!(
        sha256_bytes(b""),
        "e3b0c44298fc1c149afbf4c8996fb92427ae41e4649b934ca495991b7852b855"
    );
}

#[test]
fn sha256_file_matches_sha256_bytes_over_many_chunks() {
    // Sizes around a plausible 64 KiB read boundary, plus an empty file and one far past
    // it: a loop that stops after the first chunk or drops a tail fails here.
    let sizes = [0usize, 1, 65_535, 65_536, 65_537, 200_000];
    let tmp = TempDir::new().expect("test setup: tempdir");

    for (index, size) in sizes.iter().enumerate() {
        let body: Vec<u8> = (0..*size).map(|byte| (byte % 251) as u8).collect();
        let path = tmp.path().join(format!("note-{index}.md"));
        fs::write(&path, &body).expect("test setup: write file");

        assert_eq!(
            sha256_file(&path).expect("file was just written"),
            sha256_bytes(&body),
            "{size} bytes must hash the same as the same bytes in memory"
        );
    }
}

#[test]
fn a_file_that_cannot_be_read_is_an_error_naming_the_path() {
    let missing = Path::new("/definitely/not/here/note.md");

    let err = sha256_file(missing).expect_err("a missing file cannot hash");
    let message = err.to_string();
    assert!(
        message.contains("note.md"),
        "the error has to name the file: {message}"
    );
}
