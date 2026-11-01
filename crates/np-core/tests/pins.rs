//! The pin table's two promises, made executable (`docs/design-m2-local-embeddings.md` §12 rule 7).
//!
//! First: **a pin is a digest, not a hint.** A row whose `sha256` is short, uppercase or blank would let a fetch
//! "verify" bytes against something nobody can check, so the shape of every entry is asserted here rather than left
//! to the reviewer reading a `const`.
//!
//! Second: **a pin says what to accept and never where from.** That half of rule 7 is the one a well-meaning reader
//! is most likely to "improve" — adding a host would make the table self-contained and break the mirror case, plus
//! §12 rule 8's guarantee that no refusal text can carry a URL. So it is a test, not only a doc comment: every field
//! of every row is scanned for scheme- and host-shaped substrings.
//!
//! The gap in the table gets the same treatment as the data ([`MMPROJ_DIGEST_PREFIX_ONLY`]'s own words explain why
//! the second member of the pair is absent), so the missing row stays a decision instead of becoming a forgotten one.

use std::fs;
use std::io::Write;
use std::path::PathBuf;

use np_core::hash::sha256_file;
use np_core::pins::{
    pinned, pinned_member, total_size, PinnedModel, MMPROJ_DIGEST_PREFIX_ONLY, PINNED,
};

/// Substrings that would mean a field names a place rather than a thing.
///
/// `.com`/`.org`/`.net`/`.io`/`.dev` are here because a host has to end in something like them, while the one
/// extension this table genuinely carries — `.gguf`, on a filename — is not among them. That asymmetry is what makes
/// the test say something: it passes on names and fails on origins, rather than firing on every legal row.
///
/// A bare dot is deliberately **not** on the list — `models::validate_id` allows dots in an id, so "no dots anywhere"
/// would refuse a legal future row. If a genuinely legal value ever trips one of these endings, the fix is to narrow
/// the needle (say `.com/`, which no filename holds), not to delete the rule.
const HOST_SHAPED: [&str; 7] = ["http", "://", ".com", ".org", ".net", ".io", ".dev"];

/// Every field a row can hide a host in, in declaration order.
fn fields(entry: &PinnedModel) -> [(&str, &str); 5] {
    [
        ("id", entry.id),
        ("member", entry.member),
        ("filename", entry.filename),
        ("sha256", entry.sha256),
        ("read_on", entry.read_on),
    ]
}

#[test]
fn every_pinned_digest_is_lowercase_hex_of_exactly_64_characters() {
    // An empty table would satisfy the loop below, so the premise is asserted first: this build must pin something,
    // or "nothing unpinned is accepted" has no content.
    assert!(
        !PINNED.is_empty(),
        "an empty pin table fetches nothing and tests nothing"
    );

    for entry in PINNED {
        let chars: Vec<char> = entry.sha256.chars().collect();
        assert_eq!(
            chars.len(),
            64,
            "{} / {}: a sha256 is 64 hex characters, got {:?}",
            entry.id,
            entry.member,
            entry.sha256
        );
        assert!(
            chars
                .iter()
                .all(|c| c.is_ascii_digit() || ('a'..='f').contains(c)),
            "{} / {}: a pin is compared against lowercase hex, so it has to be spelled that way: {:?}",
            entry.id,
            entry.member,
            entry.sha256
        );

        // Mutation this catches: pasting an uppercase digest (every comparison against `hash::Sha256Stream` output
        // would then fail on a byte-perfect download), or truncating one to the short prefix design §12 quotes.
    }
}

#[test]
fn every_pinned_size_is_a_real_length() {
    for entry in PINNED {
        assert!(
            entry.size_bytes > 0,
            "{} / {}: a zero size makes §12 rule 6's preflight admit a fetch of nothing",
            entry.id,
            entry.member
        );
    }
}

#[test]
fn member_names_are_unique_within_an_id() {
    // `pinned_member` answers one row, and the child table §12 rule 10 sketches keys on `(model, name)`. Two rows
    // sharing a member would make "which digest does `main` mean" unanswerable — and the first duplicate is exactly
    // the edit a careless second commit makes, by copying a row to add a file.
    let mut seen: Vec<(&str, &str)> = Vec::new();
    for entry in PINNED {
        let key = (entry.id, entry.member);
        assert!(
            !seen.contains(&key),
            "duplicate member `{}` for id `{}`: a fetch could not pick one digest",
            entry.member,
            entry.id
        );
        seen.push(key);
    }
}

#[test]
fn no_pinned_field_names_a_host_or_a_url() {
    // §12 rule 7's boundary in one loop: the host is config (`[models] base_url`), and a table that grew one would
    // silently end the mirror case and start making refusal text carry origins.
    let mut checked = 0usize;
    for entry in PINNED {
        for (name, value) in fields(entry) {
            let lowered = value.to_ascii_lowercase();
            for needle in HOST_SHAPED {
                assert!(
                    !lowered.contains(needle),
                    "{} / {}: `{}` holds {needle:?}, which names a place; the pin table says what to accept \
                     and `[models] base_url` says where from — field value: {value:?}",
                    entry.id,
                    entry.member,
                    name
                );
            }
            // A separator would be a *path* smuggled into a filename, which is the same category error one level
            // down: `what to accept` must stay joinable onto whatever directory the caller chose.
            assert!(
                !value.contains('/') && !value.contains('\\'),
                "{} / {}: `{}` holds a path separator: {value:?}",
                entry.id,
                entry.member,
                name
            );
            checked += 1;
        }
    }
    assert!(
        checked > 0,
        "the loop above has to have looked at something"
    );

    // The gap constant is scanned too: it lives in this module, and a reader copying it into a row must not be
    // handed a host by accident.
    let lowered = MMPROJ_DIGEST_PREFIX_ONLY.to_ascii_lowercase();
    for needle in HOST_SHAPED {
        assert!(
            !lowered.contains(needle),
            "the recorded mmproj prefix holds {needle:?}: {MMPROJ_DIGEST_PREFIX_ONLY:?}"
        );
    }
}

#[test]
fn total_size_is_the_sum_of_that_ids_members() {
    for entry in PINNED {
        let sum: u64 = pinned(entry.id).map(|member| member.size_bytes).sum();
        assert_eq!(
            total_size(entry.id),
            sum,
            "the preflight number has to be the members themselves, not a constant beside them"
        );
    }

    // An id this build does not pin sums to zero — correct arithmetic for "nothing is pinned", and asserted so a
    // later reader cannot quietly turn it into a default size.
    assert_eq!(total_size("no-such-model"), 0);
    assert_eq!(pinned("no-such-model").count(), 0);
}

#[test]
fn lookup_answers_by_id_and_member_and_refuses_what_is_not_pinned() {
    // The two accessors a fetch calls, including the `None` that means "refuse this member" rather than "unknown".
    let main = pinned_member("embeddinggemma-2", "main").expect("the primary artifact is pinned");
    assert_eq!(main.filename, "embeddinggemma-2-Q8_0.gguf");
    assert_eq!(pinned("embeddinggemma-2").count(), 1);

    // A member this build does not pin answers `None`, and that is a refusal rather than a shrug: an unpinned member
    // has no digest to check. This is also the arm §12 rule 7's guarantee rests on, which is why it is asserted for
    // the id that exists rather than only for one that does not.
    assert!(pinned_member("embeddinggemma-2", "mmproj").is_none());
    assert!(pinned_member("some-other-model", "main").is_none());
}

#[test]
fn the_mmproj_half_of_the_pair_is_a_recorded_gap_and_not_a_pin() {
    // This is the fence that fails when the missing row arrives, which is the point: whoever lands the real digest
    // has to delete this case on purpose instead of watching it pass while `total_size` quietly doubles.
    assert!(
        pinned_member("embeddinggemma-2", "mmproj").is_none(),
        "the mmproj digest is not in this repository (design §12 rule 7 records eight characters only), so \
         pinning it here would mean inventing 56 of them — delete this test only when the full value has a source"
    );
    assert_eq!(
        MMPROJ_DIGEST_PREFIX_ONLY.chars().count(),
        8,
        "the constant carries §12's eight-character prefix and nothing more — reaching 64 would make it a digest, \
         which belongs in `PINNED` with a source, not here"
    );
    for entry in PINNED {
        assert_ne!(
            entry.sha256, MMPROJ_DIGEST_PREFIX_ONLY,
            "a prefix was compared as a digest somewhere"
        );
    }

    // The cost of the gap, stated as an assertion so it cannot be missed: `total_size` for this id covers the primary
    // artifact alone, which is less than §12 rule 6 wants a preflight to reserve.
    let both = pinned("embeddinggemma-2").count();
    assert_eq!(
        both, 1,
        "one member is pinned today; when the second lands, re-read `total_size`'s doc and this test"
    );
    assert_eq!(total_size("embeddinggemma-2"), PINNED[0].size_bytes);
}

/// The artifact-gated half: **the pinned digest against the bytes themselves.**
///
/// Gated on `NP_TEST_MODEL_PATH` like `tests/local_load.rs`, and for the same F18 reason — an absent 310 MB file
/// must not read as a failure, and must not read as coverage either. Unset (the ordinary CI state, and the state of
/// every gate run in this round) it prints `NOT COVERAGE` and passes; set, it hashes the real file with the same
/// [`np_core::hash`] path a fetch would use.
#[test]
fn the_pinned_digest_is_the_real_artifacts_digest_when_the_runner_has_one() {
    let Some(path) = std::env::var_os("NP_TEST_MODEL_PATH").map(PathBuf::from) else {
        println!(
            "NOT COVERAGE: NP_TEST_MODEL_PATH is unset, so no real artifact was hashed against the pin. \
             Re-run with it pointing at embeddinggemma-2-Q8_0.gguf (sha256 2188ac1d…) to check the pin's bytes."
        );
        return;
    };

    let main = pinned_member("embeddinggemma-2", "main").expect("the primary artifact is pinned");
    let found = sha256_file(&path).expect("test setup: the named artifact must be readable");
    assert_eq!(
        found,
        main.sha256,
        "{} hashes to {found}, which is not what this build pins",
        path.display()
    );

    // The size travels with the digest in the same row, so it is checked in the same pass: a pin whose length is
    // wrong would refuse a complete download at §12 rule 6's preflight.
    let bytes = fs::metadata(&path)
        .expect("test setup: stat the named artifact")
        .len();
    assert_eq!(
        bytes,
        main.size_bytes,
        "{} has another length",
        path.display()
    );

    // And the digest is reachable through the streaming API byte by byte, which is how a fetch feeds it: same
    // number, one `update` per chunk.
    let mut stream = np_core::hash::Sha256Stream::new();
    stream
        .write_all(&fs::read(&path).expect("test setup: read the named artifact"))
        .expect("writing to a digest cannot fail");
    assert_eq!(stream.finish_hex(), main.sha256);
}
