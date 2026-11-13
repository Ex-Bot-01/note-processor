//! The pin table's two promises, made executable (`docs/design-m2-local-embeddings.md` §12 rule 7).
//!
//! First: **a pin is a digest, not a hint.** A row whose `sha256` is short, uppercase or blank would let a fetch
//! "verify" bytes against something nobody can check, so the shape of every entry is asserted here rather than left
//! to the reviewer reading a `const`.
//!
//! Second: **a pin row says what to accept and never where from.** That half of rule 7 is the one a well-meaning
//! reader is most likely to "improve" — putting a host in a row would make the table self-contained and break the
//! mirror case, plus §12 rule 8's guarantee that no refusal text can carry a URL. So it is a test, not only a doc
//! comment: every field of every row is scanned for scheme- and host-shaped substrings.
//!
//! **What changed on 2026-11-04, and what did not.** Rule 7 was amended so that one origin is compiled in
//! ([`np_core::pins::DEFAULT_ORIGIN`]) — an install with no config file has to be able to finish. That does not soften
//! the scan above: the amendment put the default beside the table, not inside a row, so a row still cannot name a
//! place, and rule 8's other half (no URL in any refusal or report line) is untouched. The three cases at the end of
//! this file are the amendment's own fence: the baked value, and the member set a fetch of *this* build plans.
//!
//! The gap in the table gets the same treatment as the data ([`MMPROJ_DIGEST_PREFIX_ONLY`]'s own words explain why
//! the second member of the pair is absent), so the missing row stays a decision instead of becoming a forgotten one.

use std::fs;
use std::io::Write;
use std::path::PathBuf;

use np_core::hash::sha256_file;
use np_core::pins::{
    identity, members_for_build, pinned, pinned_member, total_size, PinnedIdentity, PinnedModel,
    DEFAULT_ORIGIN, PINNED, PINNED_IDENTITIES,
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
/// Host-shaped substrings a pin row must never contain (§12 rule 7: the table says **what** to accept, config says where
/// from). `.co` joined the list on 2026-11-04: the same change that put an origin in the binary made `huggingface.co` a value
/// this file must keep out of every row, and without `.co` the scan passed it.
const HOST_SHAPED: [&str; 8] = ["http", "://", ".com", ".org", ".net", ".io", ".dev", ".co"];

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

    // Provenance strings are part of the sweep above (`fields()` covers `read_on`), which matters now that they
    // describe where a value was read: "downloaded from <host>" would be a URL smuggled into the table that exists
    // to hold none.
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
    assert_eq!(
        pinned("embeddinggemma-2").count(),
        2,
        "the pair, not two models"
    );

    // A member this build does not pin answers `None`, and that is a refusal rather than a shrug: an unpinned member
    // has no digest to check. This is also the arm §12 rule 7's guarantee rests on, which is why it is asserted for
    // the id that exists (a name inside a pinned id) rather than only for one that does not.
    assert!(pinned_member("embeddinggemma-2", "audio").is_none());
    assert!(pinned_member("some-other-model", "main").is_none());
}

#[test]
fn both_members_of_the_pair_are_pinned_and_their_total_is_the_number_the_design_quotes() {
    // The pair, not two models: same id, two members. This is the case that used to fence the *absence* of the
    // mmproj row; it now fences its presence, because the reason to add that row was a digest somebody could
    // reproduce — and §12's recorded prefix turned out to match no file in the repository at all. If a member goes
    // missing again, `total_size` quietly drops below what rule 6's preflight must reserve, and this is what says so.
    let members: Vec<&str> = pinned("embeddinggemma-2")
        .map(|entry| entry.member)
        .collect();
    assert_eq!(members, ["main", "mmproj"]);
    let main = pinned_member("embeddinggemma-2", "main").expect("main is pinned");
    let mmproj = pinned_member("embeddinggemma-2", "mmproj").expect("mmproj is pinned");
    assert_eq!(main.filename, "embeddinggemma-2-Q8_0.gguf");
    assert_eq!(mmproj.filename, "mmproj-embeddinggemma-2-Q8_0.gguf");
    // Distinct files, distinct digests: two members with one digest would mean the table accepts either half for
    // both slots.
    assert_ne!(main.sha256, mmproj.sha256);
    assert_eq!(mmproj.size_bytes, 554_821_056);

    // §5's "864.7 MB", quoted in bytes and by name: guessing one file understates the transfer by a factor of 2.8,
    // which is exactly what rule 6 exists to prevent.
    assert_eq!(total_size("embeddinggemma-2"), 864_676_512);
    assert_eq!(
        total_size("embeddinggemma-2"),
        main.size_bytes + mmproj.size_bytes,
        "the sum must be the members, not a constant someone typed"
    );

    // Every pinned member has to carry provenance: pin rot is invisible until first start refuses, so a refusal has
    // to be able to say where and when the expectation was read.
    for entry in PINNED {
        assert!(
            !entry.read_on.is_empty(),
            "{} has no provenance",
            entry.member
        );
    }
}

#[test]
fn every_fetchable_id_declares_an_identity_and_every_pinned_id_has_one() {
    // A fetch writes a `models` row, and the row needs `dim` plus both prefixes. The table that pins bytes therefore
    // has to cover every id it can transfer — an id with members but no identity would download 865 MB and then be
    // unable to say what it is.
    for entry in PINNED {
        let found =
            identity(entry.id).unwrap_or_else(|| panic!("{} has no pinned identity", entry.id));
        assert_eq!(found.id, entry.id);
        assert!(
            found.dim > 0,
            "a zero width admits a vector of nothing (migration 7's CHECK)"
        );
    }
    // And no identity without members: an id in the identity table that pins no bytes is a fetch that would refuse
    // with `NothingPinned` after a caller had already promised work.
    for decl in PINNED_IDENTITIES {
        assert!(
            pinned(decl.id).next().is_some(),
            "{} declares an identity but pins no artifact",
            decl.id
        );
    }

    let gemma = identity("embeddinggemma-2").expect("the shipped id");
    assert_eq!(gemma.dim, 768);
    // Empty prefixes are a claim about evidence: every embedding run recorded in this repository used none, and a
    // prefix changes vector bytes, so adopting a convention later means a **new id** (`@n`), not an edit here.
    assert_eq!(
        gemma,
        &PinnedIdentity {
            id: "embeddinggemma-2",
            dim: 768,
            prefix_doc: "",
            prefix_query: "",
        }
    );
    // An id nobody declared answers `None` rather than a plausible default.
    assert!(identity("some-other-model").is_none());
}

#[test]
fn members_for_build_is_the_member_set_this_binary_can_use() {
    // `docs/design-m2-image-embeddings.md` §13: fetch what this build can consume. One case covering both feature
    // rows, because the row that runs here can only prove its own half unless the other half is written as an
    // `else` — a vision build plans the projector too, and nothing on this default row can execute that.
    let names: Vec<&str> = members_for_build().to_vec();
    if cfg!(feature = "vision") {
        assert_eq!(
            names,
            ["main", "mmproj"],
            "a vision build's default set must carry the projector it can load"
        );
    } else {
        assert_eq!(
            names,
            ["main"],
            "a build without image support must not plan 554 MB of projector it cannot load"
        );
    }

    // The primary member leads on every row: a default set that planned nothing would arrive at `fetch` as
    // `NothingPinned`, and an install step that fetched no weights installs nothing.
    assert_eq!(names.first().copied(), Some("main"));
}

#[test]
fn every_default_member_is_pinned_for_the_id_a_fetch_will_ask_about() {
    // A set naming a member no row pins would pass the CLI's plan and refuse inside the engine with
    // `MemberUnpinned` — after a caller had already treated it as one. The id is named rather than derived because
    // that is what today's single-id table means; a second id has to decide its own default set.
    for name in members_for_build() {
        assert!(
            pinned_member("embeddinggemma-2", name).is_some(),
            "`{name}` is in this build's default member set, but no row pins its digest"
        );
    }
}

#[test]
fn the_baked_default_origin_is_the_repository_the_pinned_digests_came_from() {
    // Rule 7 as amended 2026-11-04 (the human's R7c scope decision, `docs/design-m2-image-embeddings.md` §13): a
    // machine with no config file makes one outbound request during installation. Spelled literally rather than
    // derived, so moving the default is a change made twice — here and in `pins.rs`.
    assert_eq!(
        DEFAULT_ORIGIN,
        "https://huggingface.co/ggml-org/embeddinggemma-2-GGUF/resolve/main"
    );

    // The three shape facts the transport relies on: a scheme it will accept, no credentials in the authority
    // (`config::validate_base_url` refuses those in a configured value, and a baked one should not be sloppier),
    // and no trailing slash — `HttpTransport` joins `origin` + `/` + filename, so a slash here would double up.
    assert!(DEFAULT_ORIGIN.starts_with("https://"), "{DEFAULT_ORIGIN}");
    assert!(!DEFAULT_ORIGIN.contains('@'), "{DEFAULT_ORIGIN}");
    assert!(
        !DEFAULT_ORIGIN.ends_with('/'),
        "a trailing slash would reach the transport as `//`"
    );
}

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

/// **What a fetch would cost on *this* build** (issue **#3**).
///
/// [`total_size`] answers §12 rule 6's preflight — every member the table holds for an id, because the disk has to be
/// able to hold whatever *could* be asked for. The number a person needs before agreeing to a download is the other
/// one: what this binary volunteers, which is [`members_for_build`] — the primary artifact alone on a text-only
/// build, both halves on a `vision` build, because 554 MB of projector nobody can load is not a cost to ask about.
///
/// The two numbers differ by design and both are stated here: today they differ by 554,821,056 bytes on every row in
/// the table, and a test that pinned only one of them would let the other drift unnoticed.
#[test]
fn fetch_plan_is_what_this_build_volunteers_and_sums_to_its_own_number() {
    let plan = np_core::pins::fetch_plan("embeddinggemma-2");
    let names: Vec<&str> = plan.iter().map(|entry| entry.member).collect();
    assert_eq!(
        names,
        members_for_build(),
        "the plan is the build's member set, in table order: {names:?}"
    );

    let sum: u64 = plan.iter().map(|entry| entry.size_bytes).sum();
    assert_eq!(
        np_core::pins::plan_size("embeddinggemma-2"),
        sum,
        "the size is the plan's own members summed, never a constant beside them"
    );

    // The two questions stay two answers. A `vision` build volunteers both halves, so there the plan *is* the whole
    // table and the sums agree; a text build pays 309,855,456 of the 864,676,512 the preflight reserves.
    if members_for_build().contains(&np_core::pins::MMPROJ_MEMBER) {
        assert_eq!(sum, total_size("embeddinggemma-2"), "{names:?}");
    } else {
        assert_eq!(
            (sum, total_size("embeddinggemma-2")),
            (309_855_456, 864_676_512),
            "the size a text-only build quotes is its own transfer, not §5's 864.7 MB pair"
        );
    }

    // An id this build does not pin plans nothing and costs nothing — the same `None`-shaped answer [`pinned`] gives,
    // spelled as an empty plan so a caller never has to branch on an option to print a size.
    assert!(np_core::pins::fetch_plan("some-other-model").is_empty());
    assert_eq!(np_core::pins::plan_size("some-other-model"), 0);
}
