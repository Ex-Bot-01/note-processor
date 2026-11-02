//! Migration 10: `media`, `media_path`, `media_embeddings`, and the two projector stamps.
//!
//! The schema is the place R7c's whole promise is kept or lost — one vector per distinct content hash, byte-exact
//! image paths, vectors that die with the media row (§6 of `docs/design-m2-image-embeddings.md`) — so every case
//! below asserts a state of the rows rather than a return value, and each names the production mutation it catches.
//! The two CHECK families are migration 4's and migration 7's discipline re-applied to new tables (`typeof` beside
//! declared affinity, `length(vector) = 4 * dim`), and the stamp pairs are migration 8's D12 semantics carried to
//! `media` and `model_files`.

use std::path::Path;

use np_core::db::{
    insert_model, link_media_path, media_embeddings, media_stamp, member_stamp, record_media,
    record_media_embedding, record_member_stamp, set_media_stamp, unlink_media_path, Db, DbError,
    Migration, MIGRATIONS,
};
use tempfile::TempDir;

/// A 64-character digest-shaped value, distinguishable from its twins by its first letter.
fn hash(letter: char) -> String {
    letter.to_string().repeat(64)
}

/// A vector payload of exactly `4 * dim` bytes.
fn vector_bytes(dim: i64, fill: u8) -> Vec<u8> {
    vec![fill; (4 * dim) as usize]
}

/// The single value a probe query returns, with the query in the failure message.
fn scalar<T: rusqlite::types::FromSql>(db: &Db, sql: &str) -> T {
    db.0.query_row(sql, [], |row| row.get::<_, T>(0))
        .unwrap_or_else(|err| panic!("query failed: {sql}\n{err}"))
}

/// A statement that has to be refused by a CHECK constraint specifically.
///
/// The extended code is asserted rather than the generic `ConstraintViolation`: a foreign key reports the same
/// primary code, so matching only that would let an orphan row satisfy the test while proving nothing about the
/// CHECK (`crates/np-core/tests/db.rs`'s helper of the same name says why).
fn rejects_check(db: &Db, sql: &str, args: &[&dyn rusqlite::ToSql], column: &str) {
    let mut statement =
        db.0.prepare(sql)
            .unwrap_or_else(|err| panic!("statement setup failed: {sql}\n{err}"));
    // Bound positionally in the order given, which is what `?1`, `?2`, … stand for. The offending value goes in as
    // bytes and its neighbours as their real types, because SQLite reports a foreign key before a CHECK: an orphan
    // row here would "pass" by failing the wrong constraint — the same reason `install_model` exists below.
    let err = statement
        .execute(rusqlite::params_from_iter(args.iter().copied()))
        .expect_err("a CHECK constraint has to refuse this value");
    assert!(
        matches!(
            err,
            rusqlite::Error::SqliteFailure(fault, _)
                if fault.extended_code == rusqlite::ffi::SQLITE_CONSTRAINT_CHECK
        ),
        "expected a CHECK violation on {column}, got {err}"
    );
}

/// A `models` row, because `media_embeddings.model` is a real foreign key — an insert naming no such row would
/// die on the key and pass a vector test for the wrong reason.
fn install_model(db: &Db, id: &str) {
    insert_model(db, id, 2, Some(&hash('d')), "", "", 1_730_000_000)
        .expect("test setup: model row");
}

/// A media row for `letter`, returned as its id.
fn media_row(db: &Db, letter: char) -> i64 {
    record_media(db, &hash(letter), 10, "image/png")
        .unwrap_or_else(|err| panic!("test setup: media row for {letter}: {err}"))
        .id
}

/// A version-nine database — the schema as it stood before migration 10 — written by **replaying** the first nine
/// [`MIGRATIONS`] entries rather than copying their SQL out, which is the only faithful way to reach "the file an
/// older build left behind" while migrations 3, 4 and 5 are `Code` steps (see `seed_version_seven` in
/// `crates/np-core/tests/db.rs`). The count is spelled as `take`, so a reordering or an appended entry cannot turn
/// this fixture into "the current schema" without the assertion below noticing.
fn seed_version_nine(path: &Path) -> Result<(), DbError> {
    let conn = rusqlite::Connection::open(path).expect("test setup: open the new file");
    // The `Code` steps refuse to run with enforcement on, exactly as `migrate` hands them over.
    conn.execute_batch("PRAGMA foreign_keys = OFF")
        .expect("test setup: enforcement off");
    conn.execute_batch("CREATE TABLE schema_version (version INTEGER PRIMARY KEY);")
        .expect("test setup: version table");
    let nine = MIGRATIONS.len() - 1;
    for (index, step) in MIGRATIONS.iter().enumerate().take(nine) {
        match step {
            Migration::Sql(script) => conn
                .execute_batch(script)
                .unwrap_or_else(|e| panic!("migration {} failed: {e}", index + 1)),
            Migration::Code(func) => func(&conn)?,
        }
        conn.execute(
            "INSERT INTO schema_version (version) VALUES (?1)",
            rusqlite::params![index as i64 + 1],
        )
        .expect("test setup: record the version");
    }
    // One note, one installed model, and one recorded member: the three kinds of row migration 10 must leave alone.
    // Both digests are full length because migration 9's `length(sha256) = 64` is live in this schema — a
    // two-character stand-in is refused on insert, and the point of the fixture is a file an older build accepted.
    let main = "a".repeat(64);
    let mmproj = "c".repeat(64);
    conn.execute_batch(&format!(
        "INSERT INTO files (rel_bytes, case_bytes, rel, case_key, abs, hash, size, mtime, state, last_seen)
            VALUES (x'612e6d64', x'612e6d64', 'a.md', 'a.md', '/ws/a.md', 'ha', 1, 1, 'ok', 1);
         INSERT INTO notes (rel_bytes, rel, title) VALUES (x'612e6d64', 'a.md', 'A');
         INSERT INTO models (id, dim, sha256, prefix_doc, prefix_query, installed_at)
            VALUES ('embeddinggemma-2', 768, '{main}', 'doc|', 'query|', 1730000000);
         INSERT INTO model_files (model, name, sha256, size)
            VALUES ('embeddinggemma-2', 'mmproj', '{mmproj}', 554821056);",
    ))
    .expect("test setup: seed a model row and a member");
    assert_eq!(
        conn.query_row::<i64, _, _>("SELECT MAX(version) FROM schema_version", [], |row| row
            .get(0))
            .expect("test setup: read the fixture's version"),
        nine as i64,
        "test setup: this fixture has to stop one entry short of the list it is testing"
    );
    conn.close().expect("test setup: close the fixture");
    Ok(())
}

/// Migration 10 upgrades a version-nine file without touching what it finds, and both new stamps arrive empty.
///
/// Catches: an entry that rebuilds instead of appending (losing the note or model row), and an upgrade that invented
/// a verification stamp — which would let an old database skip a digest check it never performed.
#[test]
fn a_version_nine_database_upgrades_to_ten_and_keeps_its_rows() {
    let tmp = TempDir::new().expect("test setup: tempdir");
    let path = tmp.path().join("ws.sqlite");
    seed_version_nine(&path).expect("test setup: version-nine file");

    let db = Db::open(&path).expect("opening a version-nine file applies migration 10");
    assert_eq!(
        scalar::<i64>(&db, "SELECT COUNT(*) FROM schema_version"),
        MIGRATIONS.len() as i64,
        "no migration is recorded twice"
    );

    // The rows the fixture planted are still there: migration 10 is three creates and two `ADD COLUMN`s, not a rebuild.
    assert_eq!(scalar::<i64>(&db, "SELECT COUNT(*) FROM notes"), 1);
    assert_eq!(scalar::<i64>(&db, "SELECT COUNT(*) FROM model_files"), 1);
    assert_eq!(
        scalar::<String>(&db, "SELECT sha256 FROM model_files WHERE name = 'mmproj'"),
        "c".repeat(64),
        "the recorded projector digest survives the two added columns"
    );

    // The new tables exist and are empty — nothing is backfilled by hand (§6: "the next `np embed` fills them").
    assert_eq!(scalar::<i64>(&db, "SELECT COUNT(*) FROM media"), 0);
    assert_eq!(scalar::<i64>(&db, "SELECT COUNT(*) FROM media_path"), 0);
    assert_eq!(
        scalar::<i64>(&db, "SELECT COUNT(*) FROM media_embeddings"),
        0
    );

    // And both stamp pairs read as "never checked", which is the honest cost of upgrading into a check.
    assert_eq!(
        member_stamp(&db, "embeddinggemma-2", "mmproj").expect("read the member stamp"),
        None,
        "an upgrade may not invent a member stamp"
    );
}

/// `media.hash` is a digest or nothing.
///
/// Catches: dropping the length CHECK, which would store a truncated paste as content identity and then refuse the
/// real file forever, with no record of when it went wrong (the same argument migration 9 makes for
/// `model_files.sha256`).
#[test]
fn a_media_hash_that_is_not_64_characters_is_refused_by_the_schema() {
    let db = Db::in_memory().expect("in-memory database");
    let err = record_media(
        &db,
        &hash('a').chars().take(63).collect::<String>(),
        10,
        "image/png",
    )
    .expect_err("63 characters is not a sha256");
    assert!(err.to_string().contains("CHECK"), "{err}");

    // The accepting arm, so the refusal above cannot be a writer that always fails.
    let row = record_media(&db, &hash('a'), 10, "image/png").expect("64 characters is one");
    assert_eq!(row.hash, hash('a'));
}

/// A media path keeps raw bytes, and a TEXT bind is refused even though the column says `BLOB`.
///
/// Catches two mutations: storing paths through a lossy decode (F13's defect, re-opened on a new table), and dropping
/// the `typeof` arm — declared affinity alone accepts a `String` bind, and `length()` would then count characters.
#[test]
fn a_media_path_keeps_non_utf8_bytes_and_refuses_a_text_bind() {
    let db = Db::in_memory().expect("in-memory database");
    let media = media_row(&db, 'a');
    // `café.png` with the `é` as a lone 0xe9 byte: not valid UTF-8, and a name this program has to store.
    let raw: Vec<u8> = b"caf\xe9.png".to_vec();

    link_media_path(
        &db,
        &raw,
        "café.png",
        Path::new("/ws/caf\u{fffd}.png"),
        media,
    )
    .expect("link a path that is not valid UTF-8");
    let stored: Vec<u8> =
        db.0.query_row(
            "SELECT rel_bytes FROM media_path WHERE media_id = ?1",
            rusqlite::params![media],
            |row| row.get(0),
        )
        .expect("read the path back");
    assert_eq!(
        stored, raw,
        "the bytes round-trip; nothing decoded them on the way in"
    );
    assert_eq!(
        scalar::<String>(&db, "SELECT typeof(rel_bytes) FROM media_path"),
        "blob",
        "declared affinity is not enforcement — the CHECK is what keeps it a blob"
    );

    // The same column, spelled as text: refused by `typeof = 'blob'` rather than silently accepted.
    // The offending bind is a `String`, which rusqlite sends as TEXT — binding these bytes as `&[u8]` would make a
    // BLOB, and the column's `typeof` arm accepts that happily. Its neighbour binds its real type, because SQLite
    // reports a foreign key before a CHECK and an orphan row would prove nothing about this column.
    let text_bind = "x.png".to_string();
    rejects_check(
        &db,
        "INSERT INTO media_path (rel_bytes, case_key, abs, media_id) VALUES (?1, 'x.png', '/ws/x.png', ?2)",
        &[&text_bind, &media],
        "media_path.rel_bytes",
    );
}

/// A vector is `4 * dim` bytes or nothing — one byte either side is refused.
///
/// Catches: dropping the length CHECK, which is what lets a q8 payload or a half-written f32 row into a table whose
/// readers assume little-endian f32 (migration 7's argument, re-argued for `media_embeddings`).
#[test]
fn a_media_vector_whose_length_is_not_four_times_dim_is_refused() {
    let db = Db::in_memory().expect("in-memory database");
    install_model(&db, "m");
    let media = media_row(&db, 'a');

    record_media_embedding(&db, media, "m", 2, &vector_bytes(2, 7), &hash('a'), 1)
        .expect("exactly 4 * dim is stored");
    let too_long =
        record_media_embedding(&db, media, "m", 2, &vector_bytes(2, 7)[..], &hash('a'), 1)
            .map(|_| ());
    assert!(too_long.is_ok(), "the good case again: {too_long:?}");

    let err =
        db.0.execute(
            "INSERT INTO media_embeddings (media_id, model, dim, vector, source_hash, embedded_at)
             VALUES (?1, 'm', 2, ?2, 'h', 1)",
            rusqlite::params![media, vec![7u8; 9]],
        )
        .expect_err("nine bytes is not two f32 components");
    assert!(err.to_string().contains("CHECK"), "{err}");

    let err =
        db.0.execute(
            "INSERT INTO media_embeddings (media_id, model, dim, vector, source_hash, embedded_at)
             VALUES (?1, 'm', 2, ?2, 'h', 1)",
            rusqlite::params![media, vec![7u8; 7]],
        )
        .expect_err("seven bytes is not two f32 components either");
    assert!(err.to_string().contains("CHECK"), "{err}");
}

/// A vector bound as TEXT is refused at the right length.
///
/// Catches: dropping the `typeof(vector) = 'blob'` arm, whose failure mode is invisible here and expensive later —
/// eight characters would satisfy `length(vector) = 4 * dim` at dim=2 and reach a cosine as garbage.
#[test]
fn a_media_vector_bound_as_text_is_refused_at_the_right_length() {
    let db = Db::in_memory().expect("in-memory database");
    install_model(&db, "m");
    let media = media_row(&db, 'a');
    // Eight characters is exactly `4 * dim` at dim 2, so the length arm passes and only `typeof` can refuse it —
    // which is the whole point: without that arm a TEXT bind of eight characters reaches a cosine as garbage.
    let text_bind = "abcdefgh".to_string();
    rejects_check(
        &db,
        "INSERT INTO media_embeddings (media_id, model, dim, vector, source_hash, embedded_at)
         VALUES (?1, 'm', 2, ?2, 'h', 1)",
        &[&media, &text_bind],
        "media_embeddings.vector",
    );
}

/// One hash is one media row, however many times it is recorded.
///
/// Catches: an insert that fails on the UNIQUE (an embed run aborting because two notes link one diagram) and one
/// that inserts a second row for the same bytes (two vectors, two hits, and a stale duplicate that outlives the
/// original — §7's reason identity is content rather than path).
#[test]
fn record_media_is_insert_or_get_by_hash() {
    let db = Db::in_memory().expect("in-memory database");
    let first = record_media(&db, &hash('a'), 100, "image/png").expect("first spelling");
    let again = record_media(&db, &hash('a'), 100, "image/png").expect("the same bytes again");
    assert_eq!(first.id, again.id, "one hash, one row");
    assert_eq!(scalar::<i64>(&db, "SELECT COUNT(*) FROM media"), 1);

    // Different bytes, different row — the dedup must not be a blanket "always the first row".
    let other = record_media(&db, &hash('b'), 200, "image/jpeg").expect("other bytes");
    assert_ne!(other.id, first.id);
    assert_eq!(other.mime, "image/jpeg");
    assert_eq!(scalar::<i64>(&db, "SELECT COUNT(*) FROM media"), 2);
}

/// Re-linking a path re-points it instead of failing.
///
/// Catches: an insert where the upsert belongs — a diagram replaced in place is an ordinary next run, and a
/// `SQLITE_CONSTRAINT_PRIMARYKEY` out of the write pass would roll the whole embed pass back over one re-pointed row.
#[test]
fn linking_a_path_that_already_exists_re_points_it() {
    let db = Db::in_memory().expect("in-memory database");
    let old_bytes = media_row(&db, 'a');
    let new_bytes = media_row(&db, 'b');
    let rel = b"diagrams/kafka.png";

    link_media_path(
        &db,
        rel,
        "diagrams/kafka.png",
        Path::new("/ws/diagrams/kafka.png"),
        old_bytes,
    )
    .expect("first link");
    link_media_path(
        &db,
        rel,
        "diagrams/kafka.png",
        Path::new("/ws/diagrams/kafka.png"),
        new_bytes,
    )
    .expect("the same path now holds different bytes");

    assert_eq!(
        scalar::<i64>(&db, "SELECT COUNT(*) FROM media_path"),
        1,
        "one row per path, not one row per link event"
    );
    assert_eq!(
        scalar::<i64>(&db, "SELECT media_id FROM media_path"),
        new_bytes,
        "the path points at the bytes it holds now"
    );
}

/// Unlinking the last path removes the media row **and its vectors**; unlinking one of two leaves both alone.
///
/// Catches: a cleanup that deletes only the path row — an orphaned diagram would keep answering `np search` after
/// its last note is gone (§6's lifecycle) — and one that deletes media rows still referenced by another path.
#[test]
fn unlinking_the_last_path_removes_the_media_row_and_its_vectors() {
    let db = Db::in_memory().expect("in-memory database");
    install_model(&db, "m");
    let media = media_row(&db, 'a');
    record_media_embedding(&db, media, "m", 2, &vector_bytes(2, 3), &hash('a'), 1)
        .expect("one vector for the diagram");
    link_media_path(
        &db,
        b"notes/a.png",
        "notes/a.png",
        Path::new("/ws/notes/a.png"),
        media,
    )
    .expect("link one note");
    link_media_path(
        &db,
        b"notes/b.png",
        "notes/b.png",
        Path::new("/ws/notes/b.png"),
        media,
    )
    .expect("link a second note");

    assert_eq!(
        unlink_media_path(&db, b"notes/a.png").expect("unlink the first path"),
        0,
        "one path still holds those bytes, so no media row goes"
    );
    assert_eq!(scalar::<i64>(&db, "SELECT COUNT(*) FROM media"), 1);
    assert_eq!(
        scalar::<i64>(&db, "SELECT COUNT(*) FROM media_embeddings"),
        1,
        "and its vector is untouched"
    );

    assert_eq!(
        unlink_media_path(&db, b"notes/b.png").expect("unlink the last path"),
        1,
        "the return value is the count of media rows removed with it"
    );
    assert_eq!(scalar::<i64>(&db, "SELECT COUNT(*) FROM media"), 0);
    assert_eq!(
        scalar::<i64>(&db, "SELECT COUNT(*) FROM media_embeddings"),
        0,
        "`ON DELETE CASCADE` has to take the vectors with the row"
    );

    // A path nobody linked is a fact about a deleted file, not a corrupt database.
    assert_eq!(
        unlink_media_path(&db, b"notes/gone.png").expect("unlink an absent path"),
        0
    );
}

/// `media_embeddings` reads one model at a time, in media order.
///
/// Catches: a reader that forgets the `model` filter — `PRIMARY KEY (media_id, model)` lets two models hold a vector
/// for one diagram, and mixing them is how one model's query ranks another model's answer (`vectors_for`'s rule).
#[test]
fn media_embeddings_reads_one_model_at_a_time_in_media_order() {
    let db = Db::in_memory().expect("in-memory database");
    install_model(&db, "m1");
    install_model(&db, "m2");
    // Named by rowid rather than by content, because the assertion below is about rowid order: `low` was inserted
    // first and so holds the smaller id, and a reader missing its `ORDER BY` would hand them back the other way.
    let low = media_row(&db, 'b');
    let high = media_row(&db, 'a');
    assert!(low < high, "test setup: rowids ascend with insertion");
    record_media_embedding(&db, low, "m1", 2, &vector_bytes(2, 1), &hash('b'), 1)
        .expect("vector for the lower row");
    record_media_embedding(&db, high, "m1", 2, &vector_bytes(2, 2), &hash('a'), 1)
        .expect("vector for the higher row");
    record_media_embedding(&db, high, "m2", 2, &vector_bytes(2, 3), &hash('a'), 1)
        .expect("the same diagram under a second model");

    let m1 = media_embeddings(&db, "m1").expect("read m1");
    assert_eq!(
        m1.iter().map(|(id, _)| *id).collect::<Vec<_>>(),
        vec![low, high],
        "ascending by media id, which is what makes a resumed run's rows stable"
    );
    assert_eq!(
        m1[0].1,
        vector_bytes(2, 1),
        "the payload comes back as stored"
    );
    assert_eq!(
        media_embeddings(&db, "m2")
            .expect("read m2")
            .into_iter()
            .map(|(id, _)| id)
            .collect::<Vec<_>>(),
        vec![high],
        "one model's read cannot see another's vectors"
    );
}

/// `media_embeddings` keeps both models' rows for one diagram.
///
/// Catches: a primary key narrowed to `media_id`, which would let embedding under a second model destroy the first —
/// the guarantee migration 7 states for `embeddings` and this table inherits.
#[test]
fn two_models_hold_vectors_for_one_media_row() {
    let db = Db::in_memory().expect("in-memory database");
    install_model(&db, "m1");
    install_model(&db, "m2");
    let media = media_row(&db, 'a');
    record_media_embedding(&db, media, "m1", 2, &vector_bytes(2, 1), &hash('a'), 1)
        .expect("first model");
    record_media_embedding(&db, media, "m2", 2, &vector_bytes(2, 2), &hash('a'), 1)
        .expect("second model");

    assert_eq!(
        scalar::<i64>(&db, "SELECT COUNT(*) FROM media_embeddings"),
        2,
        "re-embedding under a second model must not disturb the first row"
    );

    // And re-embedding under one model corrects that row rather than appending history nobody reads.
    record_media_embedding(&db, media, "m1", 2, &vector_bytes(2, 9), &hash('e'), 5)
        .expect("re-embed the same pair");
    assert_eq!(
        scalar::<i64>(
            &db,
            "SELECT COUNT(*) FROM media_embeddings WHERE model = 'm1'"
        ),
        1
    );
    assert_eq!(
        scalar::<String>(
            &db,
            "SELECT source_hash FROM media_embeddings WHERE model = 'm1'"
        ),
        hash('e'),
        "the row states what it now holds"
    );
}

/// The clock a vector was written at is the clock the caller was given.
///
/// Catches: a body that ignores its `now` argument — passing `0`, or reading a clock of its own, compiles fine and
/// makes `embedded_at` unauditable, which is the column an operator reads to ask "when did this diagram last move".
#[test]
fn record_media_embedding_stores_the_clock_it_was_given() {
    let db = Db::in_memory().expect("in-memory database");
    install_model(&db, "m");
    let first = media_row(&db, 'a');
    let second = media_row(&db, 'b');

    record_media_embedding(
        &db,
        first,
        "m",
        2,
        &vector_bytes(2, 1),
        &hash('a'),
        1_730_000_111,
    )
    .expect("first write");
    record_media_embedding(
        &db,
        second,
        "m",
        2,
        &vector_bytes(2, 2),
        &hash('b'),
        1_730_000_999,
    )
    .expect("second write");

    let stamps: Vec<i64> =
        db.0.prepare("SELECT embedded_at FROM media_embeddings ORDER BY media_id")
            .expect("read the stamps")
            .query_map([], |row| row.get(0))
            .expect("stamp rows")
            .collect::<Result<Vec<_>, _>>()
            .expect("stamp values");
    assert_eq!(stamps, vec![1_730_000_111, 1_730_000_999]);
}

/// A media stamp round-trips, and a half-written pair reads as no stamp at all.
///
/// Catches: a reader that treats one column present as verified (migration 8's rule — a half stamp would let an embed
/// skip hashing bytes it never checked), and a writer that appends history instead of replacing the pair.
#[test]
fn a_media_stamp_round_trips_and_a_half_pair_reads_as_no_stamp() {
    let db = Db::in_memory().expect("in-memory database");
    let media = media_row(&db, 'a');

    assert_eq!(
        media_stamp(&db, media).expect("read before any stamp"),
        None,
        "never checked is not checked at size zero"
    );

    set_media_stamp(&db, media, 40_960, 1_730_000_500).expect("record the stamp");
    assert_eq!(
        media_stamp(&db, media).expect("read after stamping"),
        Some((40_960, 1_730_000_500))
    );

    set_media_stamp(&db, media, 40_960, 1_730_000_900).expect("re-stamp");
    assert_eq!(
        media_stamp(&db, media).expect("read after re-stamping"),
        Some((40_960, 1_730_000_900)),
        "the pair is replaced, not accumulated"
    );

    // A half pair is reachable only by hand-editing the file; D12's rule says that is "unverified".
    db.0.execute(
        "UPDATE media SET verified_mtime = NULL WHERE id = ?1",
        rusqlite::params![media],
    )
    .expect("test setup: half-stamp the row");
    assert_eq!(
        media_stamp(&db, media).expect("read a half pair"),
        None,
        "one column present is not a stamp"
    );

    // A stamp for a row that is gone is a no-op, exactly as `record_artifact_stamp` promises for `models`.
    set_media_stamp(&db, 999_999, 1, 1).expect("stamping an absent media row is not an error");
    assert_eq!(media_stamp(&db, 999_999).expect("read it back"), None);
}

/// `member_stamp` separates "no stamp recorded" from "a stamp of zeros", and both from "no member recorded".
///
/// Catches: a reader that coerces NULLs to `0` (every projector would look verified at size zero, so D12's
/// verify-on-change would silently stop hashing the 554 MB file), and a reader that cannot tell an unchecked member
/// from a model whose members were never recorded — §6's third change is precisely that distinction.
#[test]
fn member_stamp_separates_no_member_from_unchecked_from_a_stamp_of_zeros() {
    let db = Db::in_memory().expect("in-memory database");
    insert_model(&db, "alpha", 768, Some(&hash('a')), "", "", 1_730_000_000).expect("model row");

    // Nothing recorded at all.
    assert_eq!(
        member_stamp(&db, "alpha", "mmproj").expect("read an unrecorded member"),
        None
    );

    // Recorded, never verified: still `None`, but now distinguishable from the arm above because the member exists.
    np_core::db::record_model_file(&db, "alpha", "mmproj", &hash('c'), 554_821_056)
        .expect("record the projector");
    assert_eq!(
        member_stamp(&db, "alpha", "mmproj").expect("read an unchecked member"),
        None,
        "a row with no stamp is not a verified row"
    );
    assert_eq!(
        np_core::db::model_files(&db, "alpha")
            .expect("members")
            .len(),
        1,
        "and yet the member is recorded — which is the distinction migration 10 adds"
    );

    // A stamp whose values are both zero is a real stamp: `Some`, not the missing case.
    record_member_stamp(&db, "alpha", "mmproj", 0, 0).expect("stamp with zeros");
    assert_eq!(
        member_stamp(&db, "alpha", "mmproj").expect("read the zero stamp"),
        Some((0, 0)),
        "zeros are a size and an mtime, not an absent pair"
    );

    // Real values replace them, and a half pair falls back to "unverified" rather than half-trusting one column.
    record_member_stamp(&db, "alpha", "mmproj", 554_821_056, 1_730_000_500).expect("re-stamp");
    assert_eq!(
        member_stamp(&db, "alpha", "mmproj").expect("read the real stamp"),
        Some((554_821_056, 1_730_000_500))
    );
    db.0.execute(
        "UPDATE model_files SET verified_size = NULL WHERE model = 'alpha' AND name = 'mmproj'",
        [],
    )
    .expect("test setup: half-stamp the member");
    assert_eq!(
        member_stamp(&db, "alpha", "mmproj").expect("read a half pair"),
        None
    );

    // An absent member is a no-op to stamp, not an error — same promise `record_artifact_stamp` makes.
    record_member_stamp(&db, "alpha", "no-such-member", 1, 1).expect("stamping an absent member");
}
