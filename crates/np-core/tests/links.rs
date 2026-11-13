//! Link extraction, resolution, and the `links` rows Task 9 counts and Task 10 reports.
//!
//! Extraction is asserted on inline text and on the committed fixtures; resolution on a corpus map
//! spelled exactly the way a scan builds it (`case_key` → every rel carrying that fold); storing on SQL
//! against an in-memory database, for the same reason `tests/db.rs` reads rows back — what later tasks
//! rely on is the state of the rows, not the return value of a helper.

use std::collections::BTreeMap;
use std::fs;
use std::path::Path;
use std::time::{Duration, Instant};

use np_core::db::{note_counts, upsert_file, Db, DbError};
use np_core::hash::Snapshot;
use np_core::links::{extract_links, resolve, store_links, LinkKind, RawLink, ResolvedLink};
use np_core::parse::parse_markdown;
use np_core::path_keys::RelPath;

/// Contents of one fixture note, byte for byte as stored. `german-note.md` is kept with `\r\n`
/// and the others with `\n`, which is what makes [`line_numbers_stable_across_endings`] a check
/// against real files rather than only against strings built here.
fn fixture(name: &str) -> String {
    let path = Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("../../tests/fixtures/notes")
        .join(name);
    fs::read_to_string(&path).unwrap_or_else(|err| panic!("cannot read {}: {err}", path.display()))
}

/// The same text with `\r\n` on every line — the CRLF twin of an LF string.
fn crlf(text: &str) -> String {
    text.replace('\n', "\r\n")
}

/// The same text with `\r\n` on every other line, as a note edited by two different editors
/// would be stored.
fn mixed_endings(text: &str) -> String {
    let mut mixed = String::with_capacity(text.len() + text.len() / 10);
    for (index, line) in text.split('\n').enumerate() {
        if index > 0 {
            mixed.push_str(if index % 2 == 0 { "\r\n" } else { "\n" });
        }
        mixed.push_str(line);
    }
    mixed
}

/// The same text with its final line terminator removed — how most editors save the last line.
fn no_final_newline(text: &str) -> String {
    text.strip_suffix('\n').unwrap_or(text).to_string()
}

/// The comparable shape of a link list, so an assertion failure prints every link found.
fn found(links: &[RawLink]) -> Vec<(&str, LinkKind, usize)> {
    links
        .iter()
        .map(|link| (link.target.as_str(), link.kind, link.line))
        .collect()
}

/// A corpus map in the shape a scan builds it: `case_key` → every live row carrying that fold. Two rels
/// that fold onto one key are therefore two candidates rather than one overwriting the other (F13 §3). Each
/// candidate carries its byte key beside the display text, because that is what a placed target writes.
fn corpus(rels: &[&str]) -> BTreeMap<String, Vec<RelPath>> {
    let mut map: BTreeMap<String, Vec<RelPath>> = BTreeMap::new();
    for rel in rels {
        map.entry(rel.to_lowercase()).or_default().push(path(rel));
    }
    map
}

/// One path in both spellings, as a walk would record them for an ASCII or valid-UTF-8 name.
fn path(rel: &str) -> RelPath {
    RelPath::from_display(rel)
}

/// One `[[target]]` as the resolver receives it.
fn wiki(target: &str, line: usize) -> RawLink {
    RawLink {
        target: target.to_string(),
        kind: LinkKind::Wiki,
        line,
    }
}

/// One `[text](target)` as the resolver receives it.
fn md(target: &str, line: usize) -> RawLink {
    RawLink {
        target: target.to_string(),
        kind: LinkKind::Md,
        line,
    }
}

/// The resolved links as `(src, target, dst)` triples, with `-` for a destination that stayed
/// unresolved — so one assertion shows the whole placement table.
fn placed(links: &[ResolvedLink]) -> Vec<(&str, &str, &str)> {
    links
        .iter()
        .map(|link| {
            (
                link.src.rel.as_str(),
                link.target.as_str(),
                link.dst.as_ref().map(|dst| dst.rel.as_str()).unwrap_or("-"),
            )
        })
        .collect()
}

/// A registry row plus the note row Task 10 would write for it, as the note's id.
fn add_note(db: &Db, rel: &str) -> i64 {
    add_file(db, rel);
    // The byte key is written here as well, because this fixture is a writer: migration 4 made the identity
    // a BLOB column, and a note whose key is missing has no identity for any reader to find.
    db.0.execute(
        "INSERT INTO notes (rel_bytes, rel, title, title_source, encoding, word_count, body)
         VALUES (?1, ?2, ?2, 'filename', 'utf-8', 3, 'body')",
        rusqlite::params![rel.as_bytes(), rel],
    )
    .expect("test setup: note row");
    db.0.last_insert_rowid()
}

/// A registry row with **no** note row — what a tracked non-markdown file looks like, which is
/// why a link sourced from it has nowhere to hang.
fn add_file(db: &Db, rel: &str) {
    upsert_file(
        db,
        &Snapshot {
            rel_bytes: rel.as_bytes().to_vec(),
            case_bytes: np_core::path_keys::case_fold(rel.as_bytes()),
            rel: rel.to_string(),
            case_key: rel.to_lowercase(),
            hash: "h".to_string(),
            size: 10,
            mtime: 1_000,
            file_id: None,
        },
        &Path::new("/ws").join(rel),
        1,
    )
    .expect("test setup: registry row");
}

/// Every `links` row in insert order.
fn rows(db: &Db) -> Vec<(i64, Option<i64>, String, String, i64, String)> {
    let mut statement =
        db.0.prepare(
            "SELECT src_note_id, dst_note_id, kind, target, line, origin
             FROM links ORDER BY rowid",
        )
        .expect("links is readable");
    let rows = statement
        .query_map([], |row| {
            Ok((
                row.get(0)?,
                row.get(1)?,
                row.get(2)?,
                row.get(3)?,
                row.get(4)?,
                row.get(5)?,
            ))
        })
        .expect("link rows");
    rows.collect::<Result<Vec<_>, _>>().expect("link values")
}

#[test]
fn wikilink_forms_normalised() {
    // Alias, heading and block references are stripped; the author's own case is kept, because
    // `[[note a]]` and `[[Note A]]` are two spellings the resolver still has to tell apart.
    let links = extract_links(
        "[[Note A|alias]] [[Note A#section]] [[note a]] [[Note A#section|alias]] \
         [[Note A^sec12]] ![[Note A|600]]\n",
    );
    assert_eq!(
        found(&links),
        [
            ("Note A", LinkKind::Wiki, 1),
            ("Note A", LinkKind::Wiki, 1),
            ("note a", LinkKind::Wiki, 1),
            ("Note A", LinkKind::Wiki, 1),
            ("Note A", LinkKind::Wiki, 1),
            ("Note A", LinkKind::Wiki, 1),
        ]
    );

    // Nothing usable left after the strips is not a link to anywhere: an empty target, an alias
    // with no target, and an in-page anchor all yield no row rather than an unresolved one.
    assert_eq!(
        found(&extract_links(
            "[[]] [[|alias]] [[#heading]] [[   ]] [[^sec12]]"
        )),
        []
    );
}

#[test]
fn attachment_vs_md_by_extension() {
    // `.md` is a note, an attachment extension is an attachment, anything else with an extension
    // is the non-note bucket too, and an extension-less destination is not a file at all — so it
    // produces no row. The target keeps its own spelling; only the comparison is case-folded.
    let links = extract_links(
        "[a](notes/x.md) ![b](assets/pic.PNG) [c](doc.pdf) [d](photo.jpg) [e](relative-path) \
         [f](archive.zip) [[report.pdf]] ![[clip.gif]] ![[x.jpeg]] ![[y.webp]] ![[z.bmp]] \
         [[Note A]] [g](.gitignore)\n",
    );
    assert_eq!(
        found(&links),
        [
            ("notes/x.md", LinkKind::Md, 1),
            ("assets/pic.PNG", LinkKind::Attachment, 1),
            ("doc.pdf", LinkKind::Attachment, 1),
            ("photo.jpg", LinkKind::Attachment, 1),
            ("archive.zip", LinkKind::Attachment, 1),
            ("report.pdf", LinkKind::Attachment, 1),
            ("clip.gif", LinkKind::Attachment, 1),
            ("x.jpeg", LinkKind::Attachment, 1),
            ("y.webp", LinkKind::Attachment, 1),
            ("z.bmp", LinkKind::Attachment, 1),
            ("Note A", LinkKind::Wiki, 1),
        ],
        "extension-less `relative-path` and the dot-file `.gitignore` contribute nothing"
    );
}

#[test]
fn resolution_order() {
    // `notes/a.md` and `other/a.md` make the basename `a` ambiguous on purpose: an exact path or
    // a case-insensitive one still has to win over that ambiguity.
    let corpus = corpus(&[
        "notes/a.md",
        "other/a.md",
        "Notes/Alpha.md",
        "deep/nested/Beta.md",
        "assets/gamma.png",
    ]);
    let links = vec![(
        path("index.md"),
        vec![
            md("notes/a.md", 1), // exact rel, despite the ambiguous basename
            md("NOTES/A.MD", 2), // case-insensitive rel
            wiki("alpha", 3),    // unique basename, different directory
            md("Beta.md", 4),    // unique basename, spelled as a path
            RawLink {
                target: "gamma.png".to_string(),
                kind: LinkKind::Attachment,
                line: 5,
            }, // an attachment is corpus content too
            wiki("Missing", 6),  // nowhere in the corpus
            wiki("a", 7),        // ambiguous
        ],
    )];

    let resolved = resolve(&corpus, &links);
    assert_eq!(
        placed(&resolved),
        [
            ("index.md", "notes/a.md", "notes/a.md"),
            ("index.md", "NOTES/A.MD", "notes/a.md"),
            ("index.md", "alpha", "Notes/Alpha.md"),
            ("index.md", "Beta.md", "deep/nested/Beta.md"),
            ("index.md", "gamma.png", "assets/gamma.png"),
            ("index.md", "Missing", "-"),
            ("index.md", "a", "-"),
        ]
    );
    // `resolve` places targets; it does not rewrite the link itself.
    assert_eq!(
        (
            resolved[0].kind,
            resolved[0].line,
            resolved[0].src.rel.as_str()
        ),
        (LinkKind::Md, 1, "index.md")
    );
}

#[test]
fn ambiguous_basename_is_unresolved() {
    let corpus = corpus(&["x/a.md", "y/a.md"]);
    let resolved = resolve(
        &corpus,
        &[(path("index.md"), extract_links("link to [[a]] here\n"))],
    );
    assert_eq!(
        placed(&resolved),
        [("index.md", "a", "-")],
        "two candidates named `a` is a decision to make by hand, not a coin flip"
    );

    let db = Db::in_memory().expect("in-memory database");
    add_note(&db, "index.md");
    store_links(&db, &resolved, "prose").expect("store one unresolved link");

    let counts = note_counts(&db).expect("counts");
    assert_eq!(counts.links, 1);
    assert_eq!(
        counts.unresolved_links, 1,
        "the ambiguous link is the missing-note signal Task 10 reports"
    );
    assert_eq!(rows(&db)[0].2, "wiki", "kind stays inside the CHECK domain");
}

#[test]
fn line_numbers_stable_across_endings() {
    let lf = "---\ntitle: X\n---\n\nSee [[Alpha]] and [b](notes/b.md).\n\n```\n[[Fenced]]\n```\n\nLast [[Gamma]].\n";
    assert_eq!(
        found(&extract_links(&crlf(lf))),
        found(&extract_links(lf)),
        "a CRLF copy reports the same lines"
    );
    assert_eq!(
        found(&extract_links(&mixed_endings(lf))),
        found(&extract_links(lf)),
        "mixed endings report the same lines"
    );
    assert_eq!(
        found(&extract_links(&no_final_newline(lf))),
        found(&extract_links(lf)),
        "a note saved without a trailing newline reports the same lines"
    );
    // The equality above would also hold for two readings that both found nothing, so the
    // expected lines are pinned as well.
    assert_eq!(
        found(&extract_links(lf)),
        [
            ("Alpha", LinkKind::Wiki, 5),
            ("notes/b.md", LinkKind::Md, 5),
            ("Gamma", LinkKind::Wiki, 11),
        ]
    );

    // The same over a committed fixture. Both spellings are rebuilt from the stored bytes instead
    // of comparing the file against a normalisation of itself: on a checkout that stores LF, the
    // file and `german.replace("\r\n", "\n")` are the same string and the assertion proves nothing.
    let lf = fixture("german-note.md").replace("\r\n", "\n");
    let dos = crlf(&lf);
    assert!(dos.contains("\r\n"), "the CRLF twin has to exist");
    assert_eq!(
        found(&extract_links(&dos)),
        found(&extract_links(&lf)),
        "the fixture reports the same lines in either spelling"
    );
    assert_eq!(
        found(&extract_links(&dos)),
        [("notizen/andere.md", LinkKind::Md, 13)],
        "the in-page anchor `[siehe](#anker)` on that line adds no row"
    );
}

#[test]
fn fenced_code_holds_no_links() {
    let links = extract_links(
        "[[Before]]\n```\n[[Inside]] and ![plot](assets/a.png)\n```\n[[Between]]\n~~~\n[[Tilde]]\n~~~\n[[After]]\n",
    );
    assert_eq!(
        found(&links),
        [
            ("Before", LinkKind::Wiki, 1),
            ("Between", LinkKind::Wiki, 5),
            // `~~~` is not a fence in M1 — the same documented limit `parse` has, so a tilde
            // region still yields its links.
            ("Tilde", LinkKind::Wiki, 7),
            ("After", LinkKind::Wiki, 9),
        ]
    );

    // An info string does not stop a fence from opening, and an unterminated fence keeps the
    // rest of the note out of the link list — both as in `parse`.
    assert_eq!(
        found(&extract_links("```json\n[[A]]\n```\n[[B]]\n")),
        [("B", LinkKind::Wiki, 4)]
    );
    assert_eq!(
        found(&extract_links("[[A]]\n```\n[[B]]\n")),
        [("A", LinkKind::Wiki, 1)]
    );
}

#[test]
fn destinations_that_leave_the_corpus_are_skipped() {
    // Neither resolved nor counted as unresolved: `unresolved_links` means "a note I could not
    // place", and an external URL or an in-page anchor is nothing of the kind.
    let links = extract_links(
        "[a](https://x.dev/y.md) ![b](http://cdn/i.png) [c](mailto:me@x.de) [d](#section) \
         [e]() [f](//cdn/y.md) [g](file:///C:/x/y.md) [h](C:\\notes\\x.md) [i](notes/x.md)\n",
    );
    assert_eq!(found(&links), [("notes/x.md", LinkKind::Md, 1)]);

    // An empty or whitespace-only destination is the same case.
    assert_eq!(
        found(&extract_links("[a]() [b](   ) [[c]](x.md)")),
        [("c", LinkKind::Wiki, 1)]
    );
}

#[test]
fn markdown_destinations_are_normalised() {
    // `\` is a path separator, `./` and a leading `/` are workspace-root noise, and a fragment or
    // query belongs to the note rather than naming another file. Percent-escapes are deliberately
    // left alone: no decoder exists in M1, so `%20` stays part of the target and such a link
    // resolves only if the file really is named that way.
    let links = extract_links(
        "[a](.\\notes\\x.md) [b](/abs/x.md) [c](notes/x%20name.md) [d](notes/y.md#section) \
         [e](notes/z.md?v=2) [f](   )\n",
    );
    assert_eq!(
        found(&links),
        [
            ("notes/x.md", LinkKind::Md, 1),
            ("abs/x.md", LinkKind::Md, 1),
            ("notes/x%20name.md", LinkKind::Md, 1),
            ("notes/y.md", LinkKind::Md, 1),
            ("notes/z.md", LinkKind::Md, 1),
        ]
    );

    // A title attribute is pandoc/GitHub/editor noise, not part of the path: `[x](a.md "Title")`
    // is a link to `a.md`. Left attached, the file name ends in `"Title"`, so the extension test
    // reads `md "Title"` — the note link gets filed as an attachment whose target nothing can
    // place, and every such link inflates `unresolved_links`.
    assert_eq!(
        found(&extract_links(
            "[x](notes/a.md \"Release\") ![alt](img/x.png \"The title\") [y](my photo.png)\n"
        )),
        [
            ("notes/a.md", LinkKind::Md, 1),
            ("img/x.png", LinkKind::Attachment, 1),
            // A path that merely contains a space keeps it: the cut needs whitespace *and* a
            // quote, which is what separates a title from a file name.
            ("my photo.png", LinkKind::Attachment, 1),
        ],
        "a title must not change a link's kind or leave its target holding the tooltip"
    );

    // A fragment and a title on one destination, and a destination that is nothing but a quoted
    // string — which names no file and stays skipped.
    assert_eq!(
        found(&extract_links(
            "[x](notes/b.md#stelle \"Tipp\") [y](\"a b\")\n"
        )),
        [("notes/b.md", LinkKind::Md, 1)]
    );
}

#[test]
fn several_links_per_line_including_one_nested_in_link_text() {
    // The badge is a real link inside the text part of a link whose destination leaves the
    // corpus; dropping it would lose evidence.
    let links = extract_links("[![badge](img/i.png)](https://shields.io) and [[Alpha]] [[Beta]]\n");
    assert_eq!(
        found(&links),
        [
            ("img/i.png", LinkKind::Attachment, 1),
            ("Alpha", LinkKind::Wiki, 1),
            ("Beta", LinkKind::Wiki, 1),
        ]
    );
}

#[test]
fn brackets_that_are_not_links() {
    // Square brackets alone are not a link: no `]]`, no `(destination)`, and an unterminated `[[`
    // all stay prose, so nothing here may produce a row — or panic on the way to deciding that.
    assert_eq!(
        found(&extract_links(
            "a [bare] reference and [a [b] c with [[unterminated\n"
        )),
        []
    );
    assert_eq!(found(&extract_links("] x ] (y.md) and [[]]\n")), []);

    // An unterminated `[[` steps one character, not two, so its own second bracket gets its turn
    // at the markdown syntax — which is why the link after the junk below is still found.
    assert_eq!(
        found(&extract_links("]] then [[x](notes/a.md)\n")),
        [("notes/a.md", LinkKind::Md, 1)],
        "the failed wikilink must not skip the bracket that could open a markdown link"
    );

    // A target is sliced by byte index, so multi-byte text next to the brackets — `\u{00e4}` is
    // the `ä` of `nächste.md` — is the case that would fail on a character boundary.
    assert_eq!(
        found(&extract_links("🙂 [[ä]] [x](n\u{00e4}chste.md)\n")),
        [("ä", LinkKind::Wiki, 1), ("nächste.md", LinkKind::Md, 1),]
    );

    // Malformed bracket soup is not an error either. Two shapes are pinned rather than merely
    // exercised, so what the scanner decides about them cannot drift silently: `[[![](]` yields no
    // row at all — the text between `[[` and the first `]]` is `![](`, which names no file — while
    // `[[a]]]](` yields the one row that is genuinely there, a wikilink to `a`.
    assert_eq!(found(&extract_links("[[![](]")), []);
    assert_eq!(
        found(&extract_links("[[a]]]](")),
        [("a", LinkKind::Wiki, 1)],
        "the trailing `]](` is junk; the wikilink before it is real"
    );

    // The rest only have to survive: no panic, however the brackets are stacked next to
    // multi-byte text.
    for junk in [
        "[[[",
        "]]](",
        "[]](",
        "[[|]](((",
        "[\\",
        "\u{1f600}[[\u{e9}\u{1f600}]](",
    ] {
        let _ = extract_links(junk);
    }
}

/// The same shapes at the size of a real accident.
///
/// An unfenced paste of minified JSON or JS is one line holding tens of thousands of `[`, and
/// `parse_markdown` now extracts links from every note, so a scanner that asks each `[` whether
/// anything closes it spends the rest of the afternoon there. `scan_line` answers `]]` and the
/// bracket pairs once per line instead, which is the property this measures.
///
/// Ignored and release-only, in the style of Task 8's huge-input test: run it with
/// `cargo test --release -- --ignored`, which CI does from Task 10 on. The bound is deliberately
/// generous — orders of magnitude above what the linear scan measures — so it fails on a quadratic
/// shape rather than on a busy machine.
#[test]
#[ignore = "huge single-line input; release-only (`cargo test --release -- --ignored`)"]
fn bracket_heavy_line_is_linear() {
    // One line of a million openers with no `]]` anywhere: every `[` is a wikilink attempt nothing
    // closes, followed by a markdown attempt whose brackets never pair either.
    let unmatched = "[".repeat(1_000_000);
    // A second line whose brackets *do* pair and which carries both `](` and `)`, so the markdown
    // branch is reached on every opener and the one pairing pass does the whole job.
    let paired = format!("{}(a)", "[x]".repeat(250_000));
    let text = format!("{unmatched}\n{paired}\n");

    let start = Instant::now();
    let links = extract_links(&text);
    let elapsed = start.elapsed();

    assert!(links.is_empty(), "brackets alone are not links: {links:?}");
    assert!(
        elapsed < Duration::from_secs(2),
        "1.25M brackets over two lines took {elapsed:?}; the scan is quadratic in line length",
    );
}

#[test]
fn unknown_origin_is_refused() {
    // `links.origin` has a CHECK domain, so a wrong spelling at the call site has to fail loudly
    // rather than write rows nothing later recognises. Modelled on the CHECK probes of
    // `tests/db.rs`: assert the constraint violation itself, then that nothing was written.
    let db = Db::in_memory().expect("in-memory database");
    add_note(&db, "index.md");
    add_note(&db, "notes/a.md");
    let links = [ResolvedLink {
        src: path("index.md"),
        target: "notes/a.md".to_string(),
        kind: LinkKind::Md,
        line: 1,
        dst: Some(path("notes/a.md")),
    }];

    let err = store_links(&db, &links, "notes")
        .expect_err("an origin outside the CHECK domain has to be refused");
    assert!(
        matches!(
            err,
            DbError::Sqlite(rusqlite::Error::SqliteFailure(fault, _))
                if fault.code == rusqlite::ErrorCode::ConstraintViolation
        ),
        "expected a CHECK violation on origin, got {err}"
    );
    assert!(rows(&db).is_empty(), "a refused link writes no row");

    // The same list goes in under a spelling the schema knows, so the refusal above was about
    // `origin` and not about the row.
    store_links(&db, &links, "prose").expect("the same link under a known origin");
    assert_eq!(rows(&db).len(), 1);
}

#[test]
fn store_links_maps_rels_to_note_ids() {
    let db = Db::in_memory().expect("in-memory database");
    let src = add_note(&db, "index.md");
    let dst = add_note(&db, "notes/a.md");
    // A tracked non-markdown file: corpus content, resolvable as a path, but no note row.
    add_file(&db, "assets/i.png");

    let links = [
        ResolvedLink {
            src: path("index.md"),
            target: "notes/a.md".to_string(),
            kind: LinkKind::Md,
            line: 3,
            dst: Some(path("notes/a.md")),
        },
        ResolvedLink {
            src: path("index.md"),
            target: "Missing".to_string(),
            kind: LinkKind::Wiki,
            line: 4,
            dst: None,
        },
        ResolvedLink {
            src: path("index.md"),
            target: "assets/i.png".to_string(),
            kind: LinkKind::Attachment,
            line: 5,
            dst: Some(path("assets/i.png")),
        },
        ResolvedLink {
            // Sourced from a file with no note row: an orphan link row would be garbage.
            src: path("assets/i.png"),
            target: "Missing2".to_string(),
            kind: LinkKind::Wiki,
            line: 1,
            dst: None,
        },
    ];
    store_links(&db, &links, "prose").expect("store the links of one note");

    assert_eq!(
        rows(&db),
        vec![
            (
                src,
                Some(dst),
                "md".to_string(),
                "notes/a.md".to_string(),
                3,
                "prose".to_string()
            ),
            (
                src,
                None,
                "wiki".to_string(),
                "Missing".to_string(),
                4,
                "prose".to_string()
            ),
            (
                src,
                None,
                "attachment".to_string(),
                "assets/i.png".to_string(),
                5,
                "prose".to_string()
            ),
        ],
        "the fourth link has no note to belong to and is skipped"
    );

    // Two of the three rows have no note on the other end: the missing target, and the
    // attachment that is tracked as a file but has no note row.
    let counts = note_counts(&db).expect("counts");
    assert_eq!((counts.links, counts.unresolved_links), (3, 2));

    // Storing appends and deletes nothing: replacing a note's `prose` rows is the caller's job,
    // which is why Task 10 clears them in the same transaction before re-storing.
    store_links(&db, &links[..1], "approved").expect("append an approved link");
    assert_eq!(rows(&db).len(), 4);
    assert_eq!(rows(&db)[3].5, "approved");
}

#[test]
fn note_doc_carries_its_links() {
    // `NoteDoc` is built at one place, so its links are exactly what extraction finds in the
    // same raw text — including the exclusion of fenced code.
    let raw = "# T\n\nSee [[Alpha]] and ![plot](assets/p.png).\n\n```\n[[Fenced]]\n```\n";
    let doc = parse_markdown(raw, "note");
    assert_eq!(found(&doc.links), found(&extract_links(raw)));
    assert_eq!(
        found(&doc.links),
        [
            ("Alpha", LinkKind::Wiki, 3),
            ("assets/p.png", LinkKind::Attachment, 3),
        ]
    );

    // The committed note has one real link on line 4; the `[see](#anchor)` beside it is an
    // in-page anchor and the fenced heading above it is not a link.
    let release = parse_markdown(&fixture("release-note.md"), "release-note");
    assert_eq!(found(&release.links), [("notes/other.md", LinkKind::Md, 4)]);
}

// ---------------------------------------------------------------------------
// Reading the graph back: `links::listing` (issue #2)
//
// The table holds every edge `np` computed and no method on it existed, so an application that wanted a backlinks pane
// had two options: re-parse every note with its own link parser — whose idea of a wikilink drifts from np's in ways no
// test catches — or read `<data_dir>/workspaces/<key>.sqlite` directly and build a shipping product on an internal
// schema that migrations reshape without warning. Both are worse than one query here, which is why the row shape below
// is spelled in display paths and carries the line: it is meant to be the answer a GUI renders, not a dump of `links`.
// ---------------------------------------------------------------------------

/// The three edges of the fixture corpus this module reads: one resolved, one missing-note, one attachment.
fn graph_fixture() -> Db {
    let db = Db::in_memory().expect("in-memory database");
    add_note(&db, "index.md");
    add_note(&db, "notes/a.md");
    add_file(&db, "assets/i.png");
    store_links(
        &db,
        &[
            ResolvedLink {
                src: path("index.md"),
                target: "notes/a.md".to_string(),
                kind: LinkKind::Md,
                line: 3,
                dst: Some(path("notes/a.md")),
            },
            ResolvedLink {
                src: path("index.md"),
                target: "Missing".to_string(),
                kind: LinkKind::Wiki,
                line: 4,
                dst: None,
            },
            ResolvedLink {
                src: path("index.md"),
                target: "assets/i.png".to_string(),
                kind: LinkKind::Attachment,
                line: 5,
                dst: Some(path("assets/i.png")),
            },
        ],
        "prose",
    )
    .expect("store the graph");
    db
}

/// Edges as this module's assertions read them: `(src, dst, kind, target, line)`.
fn edge_tuples(listing: &np_core::links::LinkListing) -> Vec<(&str, &str, &str, &str, i64)> {
    listing
        .edges
        .iter()
        .map(|edge| {
            (
                edge.src.as_str(),
                edge.dst.as_str(),
                edge.kind.as_str(),
                edge.target.as_str(),
                edge.line,
            )
        })
        .collect()
}

/// Unresolved edges as `(src, kind, target, line)`.
fn unresolved_tuples(listing: &np_core::links::LinkListing) -> Vec<(&str, &str, &str, i64)> {
    listing
        .unresolved
        .iter()
        .map(|edge| {
            (
                edge.src.as_str(),
                edge.kind.as_str(),
                edge.target.as_str(),
                edge.line,
            )
        })
        .collect()
}

#[test]
fn listing_names_both_ends_and_the_line_each_edge_was_written_on() {
    let db = graph_fixture();
    let listing =
        np_core::links::listing(&db, np_core::links::LinkScope::All).expect("read the graph");

    // **A display path and a line, not a row id.** The three questions this answers — "who links here", "where is this
    // dead target written", "what must I edit when I rename that note" — are all answered by a place a person can open.
    assert_eq!(
        edge_tuples(&listing),
        vec![("index.md", "notes/a.md", "md", "notes/a.md", 3)]
    );
    assert_eq!(
        unresolved_tuples(&listing),
        vec![("index.md", "wiki", "Missing", 4)],
        "the target is spelled as its author wrote it, because that is the text a fix replaces"
    );
}

#[test]
fn listing_leaves_out_attachments_and_edges_no_reader_can_explain() {
    let db = graph_fixture();
    // An attachment edge has no note on either end of the question this answers: `links.target` for a figure is relative
    // to the linking note's directory and cannot survive a trip through TEXT unchanged (§6, and the reason search
    // re-resolves figure targets itself rather than trusting this column). So it is in neither list — **and that is a
    // gap, not a cleanup**: "which notes embed this figure" is answered today by `search`'s `linked_notes`, which runs
    // the one resolution rule that can. A rename tool that needs figure edges must ask there, or this list has to grow a
    // third bucket built on that rule; what it must not do is guess from `links.target`.
    let listing =
        np_core::links::listing(&db, np_core::links::LinkScope::All).expect("read the graph");
    assert_eq!(listing.edges.len(), 1, "the resolved note edge only");
    assert_eq!(
        listing.unresolved.len(),
        1,
        "the missing note, not the figure"
    );

    // An `approved` edge is a claim a human made about a note that may no longer exist, and every other reader of this
    // table — the linker set behind an image hit, a scan's unresolved list — reads prose only. One rule for all of them
    // means `np scan --json`'s number and this list can be read side by side.
    store_links(
        &db,
        &[ResolvedLink {
            src: path("notes/a.md"),
            target: "index.md".to_string(),
            kind: LinkKind::Wiki,
            line: 1,
            dst: Some(path("index.md")),
        }],
        "approved",
    )
    .expect("store an approved edge");
    let listing = np_core::links::listing(&db, np_core::links::LinkScope::All).expect("re-read");
    assert_eq!(listing.edges.len(), 1, "the approved edge stays out");
}

#[test]
fn listing_scopes_to_one_note_at_either_end() {
    let db = graph_fixture();
    store_links(
        &db,
        &[ResolvedLink {
            src: path("notes/a.md"),
            target: "index.md".to_string(),
            kind: LinkKind::Wiki,
            line: 7,
            dst: Some(path("index.md")),
        }],
        "prose",
    )
    .expect("link back the other way");

    // **`rel` names a subject, not a direction.** The three features that asked for this method need two different
    // filters — a backlinks pane wants the edges arriving at a note, fix-at-source wants the dead links written in it —
    // and a parameter that silently meant "incoming" for one list and "outgoing" for the other is how a GUI ends up
    // showing its own note as its own backlink. So one rule covers both: an edge appears when the named note is **either
    // end** of it, and every entry says which end by naming both. A client that wants only the incoming ones compares
    // `dst` to the path it asked about; nothing new has to be invented to say it.
    let scoped = np_core::links::listing(&db, np_core::links::LinkScope::Note(&path("index.md")))
        .expect("scope to one note");
    assert_eq!(
        edge_tuples(&scoped),
        vec![
            ("index.md", "notes/a.md", "md", "notes/a.md", 3),
            ("notes/a.md", "index.md", "wiki", "index.md", 7),
        ],
        "the outgoing edge and the backlink, in reading order"
    );
    assert_eq!(
        unresolved_tuples(&scoped),
        vec![("index.md", "wiki", "Missing", 4)],
        "an unresolved edge has no destination, so it can only belong to the note that wrote it"
    );

    // Scoping to the *other* note answers a different question and must not return the same document: `notes/a.md` holds
    // no unresolved link of its own. A filter that quietly ignored `rel` would pass an assertion on one of these two.
    let other = np_core::links::listing(&db, np_core::links::LinkScope::Note(&path("notes/a.md")))
        .expect("scope to the target");
    assert_eq!(other.edges.len(), 2, "one edge each way");
    assert!(
        other.unresolved.is_empty(),
        "the scope reached the unresolved list too: {:?}",
        other.unresolved
    );

    // A note that is not in the database scopes to nothing rather than failing: the caller asked about a path this build
    // has never seen, which is a fact about the folder, not a malformed request.
    let stranger = np_core::links::listing(&db, np_core::links::LinkScope::Note(&path("nope.md")))
        .expect("scope to an unknown note");
    assert!(stranger.edges.is_empty() && stranger.unresolved.is_empty());
}

#[test]
fn listing_reads_the_graph_a_reader_can_act_on_today() {
    let db = graph_fixture();

    // **A source whose file is gone contributes nothing.** `files.state = 'stale'` is history np keeps on purpose — the
    // note may come back, and a scan's unresolved count deliberately still counts those rows — but this list is rendered
    // into a pane a person clicks. Naming a deleted file as a backlink offers a click on nothing, so the predicate here
    // is the one search's linker set uses (`state = 'ok'`) rather than the one an author-facing count uses. The two
    // numbers can therefore disagree when a corpus has churn in it, and that is the correct disagreement: one counts
    // what was written, this lists what can be opened.
    np_core::db::mark_stale(&db, b"index.md").expect("age out the source");
    let listing = np_core::links::listing(&db, np_core::links::LinkScope::All)
        .expect("read after the delete");
    assert!(
        listing.edges.is_empty() && listing.unresolved.is_empty(),
        "a deleted note's edges are not a reader's to follow: {listing:?}"
    );

    // **A destination whose file is gone is unresolved, not resolved-onto-nothing.** This is the same `LIVE_EDGE`
    // predicate a scan counts its unresolved links with, so the count in `np scan --json` and the list here are answers
    // to one question rather than two.
    let db = graph_fixture();
    np_core::db::mark_stale(&db, b"notes/a.md").expect("age out the target");
    let listing = np_core::links::listing(&db, np_core::links::LinkScope::All)
        .expect("read after the delete");
    assert!(
        listing.edges.is_empty(),
        "the edge no longer lands anywhere"
    );
    assert_eq!(
        unresolved_tuples(&listing),
        vec![
            ("index.md", "md", "notes/a.md", 3),
            ("index.md", "wiki", "Missing", 4),
        ],
        "the edge that lost its destination joins the target that never had one, and both are named by the text a rename has to fix"
    );
}

#[test]
fn listing_orders_by_source_then_line() {
    let db = Db::in_memory().expect("in-memory database");
    add_note(&db, "b.md");
    add_note(&db, "a.md");
    // Stored in an order no reader would read: `b.md`'s rows first, and within it the later line first.
    store_links(
        &db,
        &[
            ResolvedLink {
                src: path("b.md"),
                target: "a.md".to_string(),
                kind: LinkKind::Wiki,
                line: 9,
                dst: Some(path("a.md")),
            },
            ResolvedLink {
                src: path("b.md"),
                target: "gone-1".to_string(),
                kind: LinkKind::Wiki,
                line: 2,
                dst: None,
            },
            ResolvedLink {
                src: path("a.md"),
                target: "b.md".to_string(),
                kind: LinkKind::Wiki,
                line: 1,
                dst: Some(path("b.md")),
            },
            ResolvedLink {
                src: path("b.md"),
                target: "gone-2".to_string(),
                kind: LinkKind::Wiki,
                line: 2,
                dst: None,
            },
        ],
        "prose",
    )
    .expect("store four edges out of order");

    let listing =
        np_core::links::listing(&db, np_core::links::LinkScope::All).expect("read the graph");
    assert_eq!(
        edge_tuples(&listing),
        vec![
            ("a.md", "b.md", "wiki", "b.md", 1),
            ("b.md", "a.md", "wiki", "a.md", 9),
        ],
        "source path first, then the line it was written on"
    );
    assert_eq!(
        unresolved_tuples(&listing),
        vec![
            ("b.md", "wiki", "gone-1", 2),
            ("b.md", "wiki", "gone-2", 2),
        ],
        "two dead targets on one line keep their written order, so the list does not shuffle between runs"
    );
}

#[test]
fn listing_an_empty_or_absent_graph_is_two_empty_lists() {
    let db = Db::in_memory().expect("in-memory database");
    let listing =
        np_core::links::listing(&db, np_core::links::LinkScope::All).expect("read an empty graph");
    assert!(listing.edges.is_empty() && listing.unresolved.is_empty());

    // A workspace scanned before links existed, or one whose notes hold none: the same answer, and no error to parse.
    add_note(&db, "lonely.md");
    let listing = np_core::links::listing(&db, np_core::links::LinkScope::All)
        .expect("read a linkless graph");
    assert!(listing.edges.is_empty() && listing.unresolved.is_empty());
}
