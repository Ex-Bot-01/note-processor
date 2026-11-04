//! Migration 10: `media`, `media_path`, `media_embeddings`, and the two projector stamps.
//!
//! The schema is the place R7c's whole promise is kept or lost — one vector per distinct content hash, byte-exact
//! image paths, vectors that die with the media row (§6 of `docs/design-m2-image-embeddings.md`) — so a case that
//! asserts a return value also reads back the row behind it, and every case names the production mutation it catches.
//! Where a case can only assert a refusal (`None` for an unverified stamp, an `Ok` for stamping a row that is gone) that
//! is stated in its own comment rather than glossed.
//! The two CHECK families are migration 4's and migration 7's discipline re-applied to new tables (`typeof` beside
//! declared affinity, `length(vector) = 4 * dim`), and the stamp pairs are migration 8's D12 semantics carried to
//! `media` and `model_files`.

use std::path::Path;

use np_core::db::{
    insert_model, link_media_path, media_by_hash, media_embeddings, media_path_check, media_stamp,
    member_stamp, prune_unlinked_media, record_media, record_media_embedding, record_member_stamp,
    record_model_file, set_media_path_check, unlink_media_path, Db, DbError, Migration, MIGRATIONS,
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

/// The same extended-code assertion `rejects_check` makes, for refusals that reach the caller as [`DbError`] instead of a
/// statement error. Matching `err.to_string().contains("CHECK")` would accept any message that happens to contain the
/// word — including one this file's own CHECK raises for a *different* column of the same table.
fn refused_by_check(err: &DbError) -> bool {
    matches!(
        err,
        DbError::Sqlite(rusqlite::Error::SqliteFailure(fault, _))
            if fault.extended_code == rusqlite::ffi::SQLITE_CONSTRAINT_CHECK
    )
}

/// A `models` row, because `media_embeddings.model` is a real foreign key — an insert naming no such row would
/// die on the key and pass a vector test for the wrong reason.
fn install_model(db: &Db, id: &str) {
    insert_model(db, id, 2, Some(&hash('d')), "", "", 1_730_000_000)
        .expect("test setup: model row");
}

/// One stored media vector, read back column by column.
fn read_embedding(db: &Db, media_id: i64, model: &str) -> (i64, Vec<u8>, String, i64) {
    db.0.query_row(
        "SELECT dim, vector, source_hash, embedded_at
           FROM media_embeddings WHERE media_id = ?1 AND model = ?2",
        rusqlite::params![media_id, model],
        |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?, row.get(3)?)),
    )
    .expect("read the embedding row")
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
    assert!(
        refused_by_check(&err),
        "expected a CHECK violation, got {err}"
    );

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

    link_media_path(&db, &raw, Path::new("/ws/caf\u{fffd}.png"), media)
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
        "INSERT INTO media_path (rel_bytes, abs, media_id) VALUES (?1, '/ws/x.png', ?2)",
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
    // The row the accessor wrote, read back column by column. Without this the case proves only that the function
    // returns `Ok`: a body that did nothing at all would satisfy an assertion on its return value.
    assert_eq!(
        read_embedding(&db, media, "m"),
        (2, vector_bytes(2, 7), hash('a'), 1),
        "dim, payload, source hash and clock are what was passed in"
    );

    // The boundary itself, through the accessor rather than a hand-written INSERT: one byte over is refused, and by the
    // CHECK rather than by anything upstream of it.
    let err = record_media_embedding(&db, media, "m", 2, &[7u8; 9], &hash('a'), 1)
        .expect_err("nine bytes is not two f32 components");
    assert!(
        refused_by_check(&err),
        "expected a CHECK violation, got {err}"
    );

    let err = record_media_embedding(&db, media, "m", 2, &[7u8; 7], &hash('a'), 1)
        .expect_err("seven bytes is not two f32 components either");
    assert!(
        refused_by_check(&err),
        "expected a CHECK violation, got {err}"
    );

    // And a refused write left nothing behind: an upsert that wrote the payload before discovering the length was wrong
    // would have replaced the good row with garbage the CHECK then refused to describe.
    assert_eq!(
        read_embedding(&db, media, "m"),
        (2, vector_bytes(2, 7), hash('a'), 1),
        "the accepted row is untouched by the two refusals"
    );
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

    link_media_path(&db, rel, Path::new("/ws/diagrams/kafka.png"), old_bytes).expect("first link");
    link_media_path(&db, rel, Path::new("/ws/diagrams/kafka.png"), new_bytes)
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
    link_media_path(&db, b"notes/a.png", Path::new("/ws/notes/a.png"), media)
        .expect("link one note");
    link_media_path(&db, b"notes/b.png", Path::new("/ws/notes/b.png"), media)
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
    // Named by rowid rather than by content, because the assertion below is about rowid order: `low` was created first
    // and so holds the smaller id.
    let low = media_row(&db, 'b');
    let high = media_row(&db, 'a');
    assert!(
        low < high,
        "test setup: rowids ascend with insertion — if that stops being true this case stops testing what it says"
    );
    // `high`'s vector is written **first** on purpose. An unordered scan of `media_embeddings` comes back in rowid order,
    // so writing these in id order would make the assertion below pass with or without the reader's `ORDER BY`; this way
    // a reader that drops it returns [high, low] and the case fails.
    record_media_embedding(&db, high, "m1", 2, &vector_bytes(2, 2), &hash('a'), 1)
        .expect("vector for the higher row, written first");
    record_media_embedding(&db, low, "m1", 2, &vector_bytes(2, 1), &hash('b'), 1)
        .expect("vector for the lower row");
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

/// `dim` has to be a real width; zero is not "a vector with no components".
///
/// Catches: leaving `dim` unconstrained. `length(vector) = 4 * dim` alone accepts `dim = 0` beside an empty BLOB, and a
/// zero-component row reads back as a unit vector of nothing — the garbage-cosine class the length arm exists to stop.
#[test]
fn a_media_vector_of_zero_dimensions_is_refused() {
    let db = Db::in_memory().expect("in-memory database");
    install_model(&db, "m");
    let media = media_row(&db, 'a');

    let err = record_media_embedding(&db, media, "m", 0, &[], &hash('a'), 1)
        .expect_err("dim 0 is not a vector width");
    assert!(
        refused_by_check(&err),
        "expected a CHECK violation, got {err}"
    );

    // The accepting arm at the smallest honest width, so the refusal above is not a writer that always fails.
    record_media_embedding(&db, media, "m", 1, &vector_bytes(1, 3), &hash('a'), 1)
        .expect("one component is a vector");
    assert_eq!(
        read_embedding(&db, media, "m"),
        (1, vector_bytes(1, 3), hash('a'), 1)
    );
}

/// A media row that no path holds goes on the sweep — including one that never had a path.
///
/// Catches: treating `unlink_media_path` as the whole lifecycle rule. It works from a path, so bytes that were recorded
/// and then refused by the encoder (undecodable file, provider that would not load) leave a row no later unlink can find,
/// and §6's "an orphaned diagram cannot haunt `np search`" quietly stops being true.
#[test]
fn a_media_row_with_no_paths_is_swept_and_its_vectors_go_with_it() {
    let db = Db::in_memory().expect("in-memory database");
    install_model(&db, "m");
    let linked = media_row(&db, 'a');
    let stranded = media_row(&db, 'b');
    record_media_embedding(&db, linked, "m", 2, &vector_bytes(2, 1), &hash('a'), 1)
        .expect("vector for the linked row");
    record_media_embedding(&db, stranded, "m", 2, &vector_bytes(2, 2), &hash('b'), 1)
        .expect("vector for the stranded row");
    link_media_path(&db, b"notes/a.png", Path::new("/ws/notes/a.png"), linked)
        .expect("link one path");

    assert_eq!(
        prune_unlinked_media(&db).expect("sweep"),
        1,
        "exactly the row nobody holds"
    );
    assert_eq!(
        media_embeddings(&db, "m")
            .expect("read the surviving vectors")
            .into_iter()
            .map(|(id, _)| id)
            .collect::<Vec<_>>(),
        vec![linked],
        "the swept row's vector went by cascade, not by luck"
    );

    // Idempotent, which is what lets the embed pass run it unconditionally instead of only when it thinks something moved.
    assert_eq!(prune_unlinked_media(&db).expect("sweep again"), 0);
    assert_eq!(scalar::<i64>(&db, "SELECT COUNT(*) FROM media"), 1);
}

/// Re-pointing a path at different bytes does not leave the old figure behind.
///
/// Catches: an upsert that moves the `media_id` and nothing else — the displaced row keeps its vectors, still answers
/// `np search`, and is unreachable by both lifecycle accessors (no path names it, so unlink never sees it).
#[test]
fn re_pointing_a_path_sweeps_the_figure_it_displaced() {
    let db = Db::in_memory().expect("in-memory database");
    install_model(&db, "m");
    let old_bytes = media_row(&db, 'a');
    let new_bytes = media_row(&db, 'b');
    record_media_embedding(&db, old_bytes, "m", 2, &vector_bytes(2, 1), &hash('a'), 1)
        .expect("vector for the old bytes");
    link_media_path(
        &db,
        b"diagrams/kafka.png",
        Path::new("/ws/diagrams/kafka.png"),
        old_bytes,
    )
    .expect("first link");

    link_media_path(
        &db,
        b"diagrams/kafka.png",
        Path::new("/ws/diagrams/kafka.png"),
        new_bytes,
    )
    .expect("the same path now holds different bytes");

    assert_eq!(
        scalar::<i64>(&db, "SELECT COUNT(*) FROM media"),
        1,
        "the displaced row goes with the last path that held it"
    );
    assert_eq!(
        media_embeddings(&db, "m")
            .expect("vectors")
            .into_iter()
            .map(|(id, _)| id)
            .collect::<Vec<_>>(),
        Vec::<i64>::new(),
        "and its vector went by cascade"
    );

    // A re-point to a figure that other paths still hold sweeps nothing.
    let shared = media_row(&db, 'c');
    link_media_path(
        &db,
        b"notes/one.png",
        Path::new("/ws/notes/one.png"),
        shared,
    )
    .expect("first holder");
    link_media_path(
        &db,
        b"notes/two.png",
        Path::new("/ws/notes/two.png"),
        shared,
    )
    .expect("second holder");
    link_media_path(
        &db,
        b"notes/one.png",
        Path::new("/ws/notes/one.png"),
        new_bytes,
    )
    .expect("re-point one of the two holders");
    assert_eq!(
        scalar::<i64>(
            &db,
            &format!("SELECT COUNT(*) FROM media WHERE id = {shared}")
        ),
        1,
        "the figure that still has a path standing survives its sibling being re-pointed"
    );
}

/// Two spellings that differ only in case are two paths; identical bytes are still one figure.
///
/// **Catches: dedup keyed on something other than content.** Each spelling goes through `record_media` with the same digest,
/// the way the pass does it — an earlier version handed one already-recorded media id to both links, which made the
/// "one row" assertion true no matter how dedup was keyed, because nothing was left to deduplicate. `media_path` has no
/// case-fold uniqueness on purpose (migration 4 moved that guard away from `case_key`), so what is pinned here is the rule
/// that matters: one content, one row, one vector.
#[test]
fn case_twin_paths_holding_one_file_answer_once() {
    let db = Db::in_memory().expect("in-memory database");
    install_model(&db, "m");
    let mut figure = None;
    for rel in [b"Diagram.PNG".as_slice(), b"diagram.png".as_slice()] {
        let abs =
            Path::new("/ws").join(std::str::from_utf8(rel).expect("test setup: ascii spelling"));
        // Two calls with the same digest, as two notes linking two spellings would do.
        let row = record_media(&db, &hash('a'), 4096, "image/png").expect("record these bytes");
        link_media_path(&db, rel, &abs, row.id).expect("link a case twin");
        figure = Some(row.id);
    }
    let figure = figure.expect("test setup: both spellings linked");
    assert_eq!(
        scalar::<i64>(&db, "SELECT COUNT(*) FROM media_path"),
        2,
        "two spellings are two paths (§6: identity is the hash, not the name)"
    );
    record_media_embedding(&db, figure, "m", 2, &vector_bytes(2, 4), &hash('a'), 1)
        .expect("one vector for the figure");

    assert_eq!(
        scalar::<i64>(&db, "SELECT COUNT(*) FROM media"),
        1,
        "identity is content, not spelling (§7)"
    );
    assert_eq!(
        media_embeddings(&db, "m").expect("vectors").len(),
        1,
        "so a search answers once, whatever the case of the link that found it"
    );
}

/// `abs` is a display spelling and says so; `rel_bytes` stays the byte-exact identity.
///
/// Catches: a comment claiming `link_media_path` binds bytes "and nothing else" while its third column goes through
/// `to_string_lossy`. This pins which of the two columns is allowed to lose information, so nobody "fixes" the wrong one.
#[cfg(unix)]
#[test]
fn an_absolute_path_that_is_not_valid_utf8_stays_a_display_spelling() {
    use std::ffi::OsStr;
    use std::os::unix::ffi::OsStrExt;

    let db = Db::in_memory().expect("in-memory database");
    let media = media_row(&db, 'a');
    let raw: &[u8] = b"caf\xe9.png";
    let abs = Path::new(OsStr::from_bytes(b"/ws/caf\xe9.png"));

    link_media_path(&db, raw, abs, media).expect("link a path no UTF-8 decoder can spell");

    let stored_abs: String =
        db.0.query_row("SELECT abs FROM media_path", [], |row| row.get(0))
            .expect("read the display spelling");
    assert_eq!(
        stored_abs, "/ws/caf\u{fffd}.png",
        "the report column is lossy by design (F20)"
    );
    let stored_rel: Vec<u8> =
        db.0.query_row("SELECT rel_bytes FROM media_path", [], |row| row.get(0))
            .expect("read the identity");
    assert_eq!(stored_rel, raw, "and the identity column is not");
}

/// A media **path** check round-trips, and a half-written pair reads as no check at all.
///
/// Catches: a reader that treats one column present as verified (migration 8's rule — a half stamp would let an embed
/// reuse a hash for bytes it never checked), a writer that appends history instead of replacing the pair, and the
/// placement itself: the pair sits on `media_path` because reading it has to precede reading the file.
#[test]
fn a_media_path_check_round_trips_and_a_half_pair_reads_as_no_check() {
    let db = Db::in_memory().expect("in-memory database");
    let media = media_row(&db, 'a');
    link_media_path(&db, b"fig.png", Path::new("/ws/fig.png"), media).expect("link the path");

    let read = |rel: &[u8]| {
        media_path_check(&db, rel)
            .expect("read the check")
            .map(|check| (check.size, check.mtime, check.hash))
    };

    assert_eq!(
        read(b"fig.png"),
        None,
        "never checked is not checked at size zero"
    );

    set_media_path_check(&db, b"fig.png", 40_960, 1_730_000_500).expect("record the check");
    assert_eq!(
        read(b"fig.png"),
        Some((40_960, 1_730_000_500, hash('a'))),
        "the pair answers with the hash it vouches for, which is the whole reason it sits on the path"
    );

    set_media_path_check(&db, b"fig.png", 40_960, 1_730_000_900).expect("re-check");
    assert_eq!(
        read(b"fig.png"),
        Some((40_960, 1_730_000_900, hash('a'))),
        "the pair is replaced, not accumulated"
    );

    // A half pair is reachable only by hand-editing the file; D12's rule says that is "unverified".
    db.0.execute(
        "UPDATE media_path SET verified_mtime = NULL WHERE rel_bytes = ?1",
        rusqlite::params![b"fig.png".as_slice()],
    )
    .expect("test setup: half-stamp the row");
    assert_eq!(read(b"fig.png"), None, "one column present is not a check");

    // A check for a path that is gone is a no-op, exactly as `record_artifact_stamp` promises for `models`.
    set_media_path_check(&db, b"gone.png", 1, 1).expect("stamping an absent path is not an error");
    assert_eq!(read(b"gone.png"), None);

    // A check does not outlive its path: unlinking removes the row that held the pair, so there is nothing to read back.
    // (Stated plainly because the earlier wording — "a swept media row cannot be revived from its stamp" — described a
    // state this schema cannot reach: with `ON DELETE CASCADE` and enforcement on, a path row whose media row is gone does
    // not exist, so the JOIN was never what made this answer `None`.)
    let orphan = media_row(&db, 'b');
    link_media_path(&db, b"orphan.png", Path::new("/ws/orphan.png"), orphan).expect("link");
    set_media_path_check(&db, b"orphan.png", 8, 8).expect("check it");
    assert_eq!(
        read(b"orphan.png")
            .expect("a check exists before the unlink")
            .2,
        hash('b')
    );
    unlink_media_path(&db, b"orphan.png").expect("unlink the last referrer");
    assert_eq!(
        read(b"orphan.png"),
        None,
        "the pair went with the path row, so no figure can be re-verified from a stamp nobody owns any more"
    );
}

/// The **embedding** currency is a per-`(media, model)` digest, and it is written only by the vector itself.
///
/// Catches: reading one model's stamp as another's (which would call a fresh model's work current), and a second writer
/// for the same fact — there is deliberately no `set_media_stamp`: `record_media_embedding` takes the digest, so a
/// vector and the bytes it was made from cannot be stored apart.
#[test]
fn embedding_currency_is_one_digest_per_media_and_model() {
    let db = Db::in_memory().expect("in-memory database");
    install_model(&db, "alpha");
    install_model(&db, "beta");
    let media = media_row(&db, 'a');

    assert_eq!(
        media_stamp(&db, media, "alpha").expect("read before any vector"),
        None,
        "no vector is not a vector made from nothing"
    );

    record_media_embedding(&db, media, "alpha", 2, &vector_bytes(2, 7), &hash('a'), 1)
        .expect("store");
    assert_eq!(
        media_stamp(&db, media, "alpha")
            .expect("read the currency")
            .as_deref(),
        Some(hash('a').as_str()),
        "the stored digest is the currency"
    );
    assert_eq!(
        media_stamp(&db, media, "beta").expect("ask as another model"),
        None,
        "one model's vector never answers for another's"
    );

    // A re-embed replaces the digest with the bytes it just read, which is how a changed figure becomes owed again.
    record_media_embedding(&db, media, "alpha", 2, &vector_bytes(2, 9), &hash('b'), 2)
        .expect("re-store");
    assert_eq!(
        media_stamp(&db, media, "alpha")
            .expect("read the re-stamped currency")
            .as_deref(),
        Some(hash('b').as_str())
    );

    // The **media** row is still keyed by the bytes on disk, which is what `media_by_hash` reads back without opening
    // the file — re-embedding changes the vector's digest, not the figure's identity.
    assert_eq!(
        media_by_hash(&db, &hash('a'))
            .expect("read by content")
            .map(|row| row.id),
        Some(media),
        "a stamped path resolves to its row without a read"
    );
    assert_eq!(
        media_by_hash(&db, &hash('z')).expect("ask for absent bytes"),
        None,
        "absent is an answer, not an error"
    );
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

/// Re-recording a member's digest invalidates its stamp.
///
/// Catches: `record_model_file`'s update arm rewriting `sha256`/`size` and leaving migration 10's stamp columns alone —
/// after a fetch that found new bytes, `member_stamp` would still report "verified at the old size", and D12's
/// verify-on-change would skip hashing a projector it has never checked. This is the 554,821,056-byte case.
#[test]
fn re_recording_a_member_with_different_bytes_invalidates_its_stamp() {
    let db = Db::in_memory().expect("in-memory database");
    insert_model(&db, "alpha", 768, Some(&hash('a')), "", "", 1_730_000_000).expect("model row");
    record_model_file(&db, "alpha", "mmproj", &hash('c'), 554_821_056).expect("first record");
    record_member_stamp(&db, "alpha", "mmproj", 554_821_056, 1_730_000_500)
        .expect("stamp the bytes we hashed");
    assert!(member_stamp(&db, "alpha", "mmproj")
        .expect("read the stamp")
        .is_some());

    record_model_file(&db, "alpha", "mmproj", &hash('d'), 600_000_000)
        .expect("a fetch found different bytes");
    assert_eq!(
        member_stamp(&db, "alpha", "mmproj").expect("read after re-recording"),
        None,
        "the stamp described bytes that are no longer on disk, so it describes nothing"
    );

    // Re-recording the *same* declaration is the ordinary repeat (a second `np models fetch`, an install retry) and must
    // not cost a re-hash either: same digest, same size, stamp kept.
    record_model_file(&db, "alpha", "mmproj", &hash('d'), 600_000_000)
        .expect("repeat the same record");
    record_member_stamp(&db, "alpha", "mmproj", 600_000_000, 1_730_000_900)
        .expect("stamp the new bytes");
    record_model_file(&db, "alpha", "mmproj", &hash('d'), 600_000_000).expect("repeat again");
    assert_eq!(
        member_stamp(&db, "alpha", "mmproj").expect("read after a repeat"),
        Some((600_000_000, 1_730_000_900)),
        "an unchanged re-record is not a reason to hash 554 MB again"
    );
}

/// A stamp column holds only an integer, because affinity converts on write.
///
/// This case exists because a review asked for a `typeof` guard on the stamp pair, and it does not need one: these
/// columns have INTEGER affinity, so `'40960'` is stored as the number 40960 and there is no text value to misread.
/// That is a property of declared affinity, not of our writers — `rel_bytes` and `vector` are BLOB-affinity, which is
/// exactly why *they* need the `typeof` arm (a TEXT bind stays TEXT there). Pinning the difference here stops the next
/// reader from "fixing" the stamp readers to match them, and from assuming affinity protects a BLOB column.
#[test]
fn a_stamp_column_holds_only_an_integer() {
    let db = Db::in_memory().expect("in-memory database");
    let media = media_row(&db, 'a');
    link_media_path(&db, b"fig.png", Path::new("/ws/fig.png"), media).expect("link the path");
    db.0.execute(
        "UPDATE media_path SET verified_size = '40960', verified_mtime = '1730000500' WHERE rel_bytes = ?1",
        rusqlite::params![b"fig.png".as_slice()],
    )
    .expect("test setup: write the stamp as text");

    // Affinity did the converting, at write time: what comes back is an integer pair, not a string that reads like one.
    let check = media_path_check(&db, b"fig.png")
        .expect("read the coerced stamp")
        .expect("affinity made a complete pair");
    assert_eq!(
        (check.size, check.mtime),
        (40_960, 1_730_000_500),
        "INTEGER affinity converts on write, so no reader-side typeof guard is needed here"
    );
    assert_eq!(
        scalar::<String>(&db, "SELECT typeof(verified_size) FROM media_path"),
        "integer",
        "the stored value is an integer even though the bind was text — the opposite of a BLOB-affinity column"
    );
}

/// **Re-pointing a path at different content clears its check.** The pair was measured against the bytes this path used to
/// hold; leaving it intact after `media_id` changes hands means the next pass matches a size-and-mtime record that describes
/// other pixels, reads nothing, and answers search with a stale vector forever. `record_model_file` established this rule
/// for `model_files` (nulling the pair on a digest change is "the load-bearing half"); this is its analogue, and it is the
/// database's own guarantee — not something the embed pass happens to re-write afterwards.
#[test]
fn re_pointing_a_path_at_other_content_clears_its_check() {
    let db = Db::in_memory().expect("in-memory database");
    install_model(&db, "m");
    let first = record_media(&db, &hash('a'), 4096, "image/png").expect("first content");
    let second = record_media(&db, &hash('b'), 4096, "image/jpeg").expect("other content");

    link_media_path(&db, b"fig.png", Path::new("/ws/fig.png"), first.id).expect("link");
    set_media_path_check(&db, b"fig.png", 4096, 1_730_000_000).expect("check the old bytes");
    assert!(
        media_path_check(&db, b"fig.png")
            .expect("read the check")
            .is_some(),
        "the pair is a real check before anything moves"
    );

    link_media_path(&db, b"fig.png", Path::new("/ws/fig.png"), second.id).expect("re-point");
    assert_eq!(
        media_path_check(&db, b"fig.png").expect("read the check"),
        None,
        "a path that now holds other content has no check, whatever its size and mtime happen to be"
    );

    // The ordinary case must stay cheap: re-linking the **same** content keeps the pair, or §7 rule 2 would cost a read.
    set_media_path_check(&db, b"fig.png", 4096, 1_730_000_001).expect("check the new bytes");
    link_media_path(&db, b"fig.png", Path::new("/ws/fig.png"), second.id)
        .expect("re-link same content");
    let kept = media_path_check(&db, b"fig.png")
        .expect("read the check")
        .expect("same content keeps its check");
    assert_eq!((kept.size, kept.mtime), (4096, 1_730_000_001));
    assert_eq!(kept.hash, hash('b'));
}
