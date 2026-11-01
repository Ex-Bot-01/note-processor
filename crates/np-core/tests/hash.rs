//! Content hashing: lowercase hex, streamed reads, honest failure.
//!
//! `tests/change.rs` stays filesystem-free; this file is where `sha256_file` gets its
//! coverage, because reading in chunks is exactly the kind of loop that passes on a
//! one-line fixture and breaks on a real note.
//!
//! [`Sha256Stream`] is covered here too, and the two sets of cases are the same claim from two sides: the stream is
//! the program's one digest definition (§12 rule 9) and the file/bytes helpers are its callers, so "the caller and
//! the stream agree" is what keeps that from being a slogan. If they ever disagree, one of them grew a second
//! hasher.

use std::fs;
use std::io::{self, Cursor, Write};
use std::path::Path;

use np_core::hash::{sha256_bytes, sha256_file, Sha256Stream};
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
fn a_stream_split_at_any_point_digests_like_one_call() {
    // The property the fetcher depends on: bytes arriving in pieces hash to what the same bytes hash to whole,
    // or a streamed download could never be checked against a pin computed from a file.
    let body: Vec<u8> = (0..200_000usize).map(|byte| (byte % 251) as u8).collect();
    for split in [1usize, 7, 4096, 65_536, 64 * 1024, body.len()] {
        let mut stream = Sha256Stream::new();
        for piece in body.chunks(split) {
            stream.update(piece);
        }
        assert_eq!(
            stream.finish_hex(),
            sha256_bytes(&body),
            "chunks of {split} bytes must digest as one"
        );
    }

    // Mutation this catches: `sha256_file` or `sha256_bytes` growing their own hasher again and drifting from the
    // stream — e.g. hex uppercased in one path, which fails here and in the file case above.
}

#[test]
fn writing_to_the_stream_hashes_every_byte_it_is_given() {
    // `impl Write` exists so a transfer can hash inside `io::copy` and count in the same pass, so what is asserted
    // is both halves: the digest matches, **and** the byte count `copy` reports is the length that was hashed.
    let body: Vec<u8> = (0..100_000usize).map(|byte| (byte % 97) as u8).collect();
    let mut stream = Sha256Stream::new();
    let copied =
        io::copy(&mut Cursor::new(&body), &mut stream).expect("writing to a digest cannot fail");

    assert_eq!(
        copied as usize,
        body.len(),
        "the count is the same pass's other answer"
    );
    assert_eq!(stream.finish_hex(), sha256_bytes(&body));

    // `write_all` reaches the same place through the same trait, which is what a caller with one buffer in hand uses.
    let mut again = Sha256Stream::new();
    again
        .write_all(&body)
        .expect("writing to a digest cannot fail");
    assert_eq!(again.finish_hex(), sha256_bytes(&body));
}

#[test]
fn an_empty_stream_digests_like_empty_bytes() {
    // "Nothing fed yet" is a real state with a published digest, and the fetcher's zero-length response has to be
    // distinguishable from a pin rather than reported as one.
    assert_eq!(Sha256Stream::new().finish_hex(), sha256_bytes(b""));
    assert_eq!(
        Sha256Stream::new().finish_hex(),
        "e3b0c44298fc1c149afbf4c8996fb92427ae41e4649b934ca495991b7852b855"
    );
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
