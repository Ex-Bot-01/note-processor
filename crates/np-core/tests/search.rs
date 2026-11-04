//! `np_core::search` over a fixture index (`docs/design-m2-image-embeddings.md` §12).
//!
//! Rows are written directly rather than through `np embed`: what is under test is how a reader ranks and names what
//! it finds, and a fake provider would add a dependency on the embed pass without making a single assertion sharper.
//! Widths are small and vectors are axis-aligned so an expected cosine is a number the reader can check by eye.

use np_core::db::{self, Db};
use np_core::embed::Vector;
use np_core::search::{self, HitKind, SearchError};
use tempfile::TempDir;

/// Every fixture model is two-dimensional: `[1, 0]` scores 1.0 against the query, `[0, 1]` scores 0.0.
const DIM: usize = 2;
const MODEL: &str = "fixture@1";

struct Fixture {
    _tmp: TempDir,
    db: Db,
}

impl Fixture {
    fn new() -> Self {
        let tmp = tempfile::tempdir().expect("test setup: tempdir");
        let db = Db::open(&tmp.path().join("index.sqlite")).expect("test setup: open db");
        db::insert_model(&db, MODEL, DIM as i64, None, "d:", "q:", 1_730_000_000)
            .expect("test setup: model row");
        Self { _tmp: tmp, db }
    }

    /// A live note with one chunk per vector in `vectors`, all under [`MODEL`].
    fn note(&self, rel: &str, vectors: Vec<Vec<f32>>) {
        let db = &self.db;
        db.0.execute(
            "INSERT INTO files (rel_bytes, case_bytes, rel, case_key, abs, hash, size, mtime, state, last_seen)
             VALUES (CAST(?1 AS BLOB), CAST(?1 AS BLOB), ?1, ?1, ?2, 'hh', 10, 1, 'ok', 1)",
            rusqlite::params![rel, format!("/ws/{rel}")],
        )
        .expect("test setup: file row");
        db.0.execute(
            "INSERT INTO notes (rel_bytes, rel, title, title_source, encoding, word_count, body)
             VALUES (?1, ?2, 't', 'h1', 'utf-8', 1, 'b')",
            rusqlite::params![rel.as_bytes(), rel],
        )
        .expect("test setup: note row");
        let note_id = db.0.last_insert_rowid();
        for (ordinal, vector) in vectors.iter().enumerate() {
            db.0.execute(
                "INSERT INTO chunks (note_id, ordinal, chunker, text, byte_start, byte_end, source_hash)
                 VALUES (?1, ?2, 1, CAST('text' AS BLOB), 0, 4, 'sh')",
                rusqlite::params![note_id, ordinal as i64],
            )
            .expect("test setup: chunk row");
            let chunk_id = db.0.last_insert_rowid();
            db::upsert_vector(
                db,
                chunk_id,
                MODEL,
                DIM,
                &bytes(vector),
                "sh",
                1_730_000_001,
            )
            .expect("test setup: vector row");
        }
    }

    /// One figure identified by content `seed` (a stand-in for its 64-hex-digit digest), seen at `paths`.
    fn image(&self, seed: u8, paths: &[&str], vector: Vec<f32>) {
        let hash = format!("{seed:0>64x}");
        let db = &self.db;
        let row = db::record_media(db, &hash, 100, "image/png").expect("test setup: media row");
        for path in paths {
            db::link_media_path(db, path.as_bytes(), std::path::Path::new(path), row.id)
                .expect("test setup: media path");
        }
        db::record_media_embedding(
            db,
            row.id,
            MODEL,
            DIM as i64,
            &bytes(&vector),
            &hash,
            1_730_000_002,
        )
        .expect("test setup: media vector");
    }

    /// The prose line `note` links an attachment at: what a scan would have recorded.
    fn link(&self, note: &str, target: &str) {
        let note_id: i64 = self
            .db
            .0
            .query_row(
                "SELECT id FROM notes WHERE rel_bytes = ?1",
                rusqlite::params![note.as_bytes()],
                |row| row.get(0),
            )
            .expect("test setup: note id");
        self.db
            .0
            .execute(
                "INSERT INTO links (src_note_id, dst_note_id, kind, target, line, origin)
                 VALUES (?1, NULL, 'attachment', ?2, 3, 'prose')",
                rusqlite::params![note_id, target],
            )
            .expect("test setup: link row");
    }

    /// §6's liveness fence: the file is gone, the rows stay.
    fn make_stale(&self, rel: &str) {
        self.db
            .0
            .execute(
                "UPDATE files SET state = 'stale' WHERE rel = ?1",
                rusqlite::params![rel],
            )
            .expect("test setup: mark stale");
    }

    fn search(
        &self,
        query: &[f32],
        limit: usize,
    ) -> Result<Vec<np_core::search::Hit>, SearchError> {
        search::search(&self.db, MODEL, &bytes(query), limit)
    }
}

/// Stored form: little-endian `f32`, exactly as [`Vector::to_bytes`] writes it.
fn bytes(components: &[f32]) -> Vec<u8> {
    Vector::new(components.to_vec()).to_bytes()
}

/// The query every case uses: `[1, 0]`, already unit length, so a score is a stored row's first component.
const QUERY: &[f32] = &[1.0, 0.0];

fn scores(hits: &[np_core::search::Hit]) -> Vec<String> {
    hits.iter()
        .map(|hit| format!("{}:{}", hit.kind.label(), hit.path))
        .collect()
}

#[test]
fn one_list_ranks_notes_and_images_together() {
    let f = Fixture::new();
    f.note("notes/b.md", vec![vec![0.0, 1.0]]);
    f.note("notes/a.md", vec![vec![1.0, 0.0]]);
    f.image(11, &["img/x.png"], vec![0.5, 0.5]);

    let hits = f.search(QUERY, 10).expect("search: ok");
    assert_eq!(
        scores(&hits),
        vec!["note:notes/a.md", "image:img/x.png", "note:notes/b.md"]
    );
    // The image's cosine is `0.5 / sqrt(0.5)` — not 0.5 — because the stored row is not unit length and §12 compares
    // rows **as stored** rather than silently rescaling them.
    assert!(
        (hits[1].score - std::f64::consts::FRAC_1_SQRT_2).abs() < 1e-5,
        "{}",
        hits[1].score
    );
    assert_eq!(hits[0].kind, HitKind::Note);
    assert_eq!(hits[1].kind, HitKind::Image);
    // A note hit names no source and no linkers (§12: those fields belong to an image).
    assert_eq!(hits[0].source_note, None);
    assert_eq!(hits[0].linked_from, 0);
}

#[test]
fn the_best_chunk_wins_and_a_note_answers_once() {
    let f = Fixture::new();
    // Three chunks, one of which is the answer. Without a per-note collapse this fixture would fill the list with
    // three hits for one file.
    f.note(
        "notes/long.md",
        vec![vec![0.0, 1.0], vec![1.0, 0.0], vec![0.5, 0.5]],
    );
    let hits = f.search(QUERY, 10).expect("search: ok");
    assert_eq!(scores(&hits), vec!["note:notes/long.md"]);
    assert!((hits[0].score - 1.0).abs() < 1e-9);
}

#[test]
fn ties_break_by_path_ascending_and_limit_truncates_that_order() {
    let f = Fixture::new();
    // Identical scores, inserted in the order a reader might expect rowid to decide: `z` first.
    f.note("notes/z.md", vec![vec![1.0, 0.0]]);
    f.note("notes/m.md", vec![vec![1.0, 0.0]]);
    f.note("notes/a.md", vec![vec![1.0, 0.0]]);

    // An image tied at the same score, whose path sorts **before** every note's. Notes are gathered first and images
    // appended, so without the explicit path tie-break this image lands after all three notes purely from assembly order —
    // which is exactly why the tie has to cross kinds to mean anything. Note-vs-note ties cannot test it: the note query
    // already reads rows in `rel_bytes` order, so removing the tie-break leaves that half of the list unchanged (measured —
    // the mutant survived until this case existed).
    f.image(9, ["a-image.png"].as_slice(), vec![1.0, 0.0]);

    let all = f.search(QUERY, 10).expect("search: ok");
    assert_eq!(
        scores(&all),
        vec![
            "image:a-image.png",
            "note:notes/a.md",
            "note:notes/m.md",
            "note:notes/z.md"
        ]
    );
    // Truncation takes the head of **that** order, so `--limit 2` cannot drop a tie's earlier path.
    let two = f.search(QUERY, 2).expect("search: limit");
    assert_eq!(
        scores(&two),
        vec!["image:a-image.png", "note:notes/a.md"],
        "truncation takes the head of the ranked list, ties included"
    );
}

#[test]
fn an_image_names_its_smallest_live_linker_and_counts_every_one() {
    let f = Fixture::new();
    f.note("notes/z.md", vec![vec![0.0, 1.0]]);
    f.note("notes/a.md", vec![vec![0.0, 1.0]]);
    f.note("notes/m.md", vec![vec![0.0, 1.0]]);
    // Insertion order is deliberately not the answer order: `z` linked first, and §6 forbids reporting it.
    f.link("notes/z.md", "diagrams/kafka.png");
    f.link("notes/m.md", "./diagrams/../diagrams/kafka.png");
    f.link("notes/a.md", "diagrams/kafka.png");
    f.image(22, &["notes/diagrams/kafka.png"], vec![1.0, 0.0]);

    let hits = f.search(QUERY, 10).expect("search: ok");
    let image = &hits[0];
    assert_eq!(image.kind, HitKind::Image);
    assert_eq!(image.source_note.as_deref(), Some("notes/a.md"));
    assert_eq!(image.linked_from, 3);
}

#[test]
fn every_path_of_one_figure_contributes_its_linkers() {
    let f = Fixture::new();
    f.note("notes/b.md", vec![vec![0.0, 1.0]]);
    f.note("notes/a.md", vec![vec![0.0, 1.0]]);
    // The same bytes at two paths (a copy), each linked by a different note: one hit, `linked_from: 2`, and the path
    // column names the smaller of the two spellings (§6's content identity).
    f.link("notes/b.md", "../img/copy.png");
    f.link("notes/a.md", "orig.png");
    // The paths are what §7's resolver makes of those two links — `notes/b.md` plus `../img/` lands on the workspace root,
    // and `orig.png` sits beside `notes/a.md` — because resolution, not the written spelling, is what links a note to a
    // media row. The first version of this fixture invented paths that no link resolves to, and the case passed by failing
    // to find any linker at all: `linked_from` came back 0 and nobody noticed it was answering a different question.
    f.image(3, &["img/copy.png", "notes/orig.png"], vec![1.0, 0.0]);

    let hits = f.search(QUERY, 10).expect("search: ok");
    // The image scores 1.0 and both notes score 0.0, so the image leads one blended list of three.
    assert_eq!(
        scores(&hits),
        vec!["image:img/copy.png", "note:notes/a.md", "note:notes/b.md"]
    );
    assert_eq!(hits[0].linked_from, 2);
    assert_eq!(hits[0].source_note.as_deref(), Some("notes/a.md"));
}

#[test]
fn a_deleted_note_is_neither_ranked_nor_a_linker() {
    let f = Fixture::new();
    f.note("notes/live.md", vec![vec![1.0, 0.0]]);
    f.note("notes/gone.md", vec![vec![1.0, 0.0]]);
    f.link("notes/gone.md", "diagrams/x.png");
    f.image(44, &["diagrams/x.png"], vec![1.0, 0.0]);

    f.make_stale("notes/gone.md");
    let hits = f.search(QUERY, 10).expect("search: ok");
    assert_eq!(
        scores(&hits),
        vec!["image:diagrams/x.png", "note:notes/live.md"]
    );
    // The image still answers — its own path row is untouched and the sweep has not run — but no live note names it.
    let image = &hits[0];
    assert_eq!(image.source_note, None);
    assert_eq!(image.linked_from, 0);
}

#[test]
fn a_model_with_no_vectors_answers_empty_rather_than_refusing() {
    let f = Fixture::new();
    let hits = f.search(QUERY, 10).expect("empty index is a fact");
    assert!(hits.is_empty(), "{hits:?}");
}

#[test]
fn a_note_only_index_answers_with_note_hits() {
    // §12 ranks whatever kinds are indexed. Search embeds nothing itself, so a corpus embedded by a text-only build
    // answers with notes — refusing here would make `np search` unusable wherever a vision build was not compiled.
    let f = Fixture::new();
    f.note("notes/a.md", vec![vec![1.0, 0.0]]);
    f.note("notes/b.md", vec![vec![0.5, 0.5]]);
    let hits = f
        .search(QUERY, 10)
        .expect("a note-only index is searchable");
    assert_eq!(scores(&hits), vec!["note:notes/a.md", "note:notes/b.md"]);
    assert!(hits.iter().all(|hit| hit.kind == HitKind::Note));
}

#[test]
fn an_image_only_index_answers_with_image_hits() {
    // A text query can match an image vector — that comparability is what Task 1 measured — and the row carries no
    // prefix of its own (§1.2), so it is comparable in the same space as a prefixed text row.
    let f = Fixture::new();
    f.image(2, &["img/x.png"], vec![1.0, 0.0]);
    let hits = f
        .search(QUERY, 10)
        .expect("an image-only index is searchable");
    assert_eq!(scores(&hits), vec!["image:img/x.png"]);
    assert_eq!(hits[0].kind, HitKind::Image);
}

#[test]
fn another_models_vectors_never_answer_for_this_one() {
    let f = Fixture::new();
    f.note("notes/mine.md", vec![vec![1.0, 0.0]]);
    // A second model id, embedded with a vector that would top the list if the `model` filter were ever dropped:
    // `PRIMARY KEY (chunk_id, model)` lets both rows exist for one chunk.
    db::insert_model(&f.db, "other@1", DIM as i64, None, "", "", 1)
        .expect("test setup: second model row");
    f.db.0
        .execute(
            "INSERT INTO embeddings (chunk_id, model, dim, vector, source_hash, embedded_at)
             SELECT c.id, 'other@1', ?1, ?2, 'sh', 1
               FROM chunks c",
            rusqlite::params![DIM as i64, bytes(&[1.0f32, 0.0])],
        )
        .expect("test setup: other model's vector");

    let hits = f.search(QUERY, 10).expect("search: ok");
    assert_eq!(scores(&hits), vec!["note:notes/mine.md"]);
}

#[test]
fn an_id_with_no_vectors_while_another_holds_them_refuses_by_id() {
    // The §12 exit-1 case: somebody embedded this workspace, just not under the id the config names (stale config,
    // typo, or an index built before a re-install). "Nothing indexed" would send the reader to `np embed` for nothing.
    let f = Fixture::new();
    db::insert_model(&f.db, "other@1", DIM as i64, None, "", "", 1)
        .expect("test setup: second model row");
    f.note("notes/a.md", vec![vec![1.0, 0.0]]); // stored under `other@1` below, not under MODEL

    let err = search::search(&f.db, "typo@9", &bytes(QUERY), 10)
        .expect_err("an id nobody embedded is a refusal");
    match &err {
        SearchError::NoVectorsForModel { model } => assert_eq!(model, "typo@9"),
        other => panic!("wrong refusal: {other:?}"),
    }
    // §8's rule 8: names what to change, and carries no path and no URL.
    let text = err.to_string();
    assert!(text.contains("typo@9"), "{text}");
    assert!(!text.contains('/'), "{text}");
    assert!(!text.contains("http"), "{text}");
}

#[test]
fn a_query_that_is_not_unit_length_refuses_instead_of_rescaling() {
    let f = Fixture::new();
    f.note("notes/a.md", vec![vec![1.0, 0.0]]);
    f.image(77, &["x.png"], vec![1.0, 0.0]);
    // `[2, 0]` points the right way and is twice as long: silently normalising it would produce a plausible list whose
    // scores came from a different query than the caller sent.
    let err = f.search(&[2.0, 0.0], 10).expect_err("length 2 is not unit");
    match &err {
        SearchError::NotUnitNorm { norm } => assert!((norm - 2.0).abs() < 1e-5, "{norm}"),
        other => panic!("wrong refusal: {other:?}"),
    }
    // A zero-length query is the same class of caller mistake.
    assert!(matches!(
        f.search(&[0.0, 0.0], 10),
        Err(SearchError::NotUnitNorm { .. })
    ));
}

#[test]
fn a_row_narrower_than_the_query_refuses_naming_both_widths() {
    let f = Fixture::new();
    f.note("notes/a.md", vec![vec![1.0, 0.0]]);
    f.image(8, &["x.png"], vec![1.0, 0.0]);
    // A three-wide query against a two-wide index: the rows are self-consistent (`length(vector) = 4 * dim`), so the
    // disagreement is between two models' widths and no cosine exists to compute.
    let err = f
        .search(&[1.0f32, 0.0, 0.0], 10)
        .expect_err("widths disagree");
    match &err {
        SearchError::DimensionMismatch { stored, query, .. } => {
            assert_eq!((stored, query), (&2usize, &3usize))
        }
        other => panic!("wrong refusal: {other:?}"),
    }
}

#[test]
fn searching_writes_nothing() {
    let f = Fixture::new();
    f.note("notes/a.md", vec![vec![1.0, 0.0]]);
    f.image(99, &["x.png"], vec![1.0, 0.0]);
    // `PRAGMA query_only` makes every write on this connection fail with SQLITE_READONLY, so a search that inserted,
    // stamped, or migrated anything would refuse here instead of returning hits. It is the module's headline claim,
    // checked rather than asserted in prose.
    f.db.0
        .execute_batch("PRAGMA query_only = ON")
        .expect("test setup: read-only pragma");
    let hits = f
        .search(QUERY, 10)
        .expect("a read-only connection must serve a search");
    assert_eq!(scores(&hits), vec!["note:notes/a.md", "image:x.png"]);
}
