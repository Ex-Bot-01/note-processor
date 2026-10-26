CREATE TABLE schema_version (version INTEGER PRIMARY KEY);

CREATE TABLE files (
    rel       TEXT    PRIMARY KEY,
    case_key  TEXT    NOT NULL UNIQUE,
    abs       TEXT    NOT NULL,
    hash      TEXT    NOT NULL,
    size      INTEGER NOT NULL,
    mtime     INTEGER NOT NULL,
    state     TEXT    NOT NULL CHECK (state IN ('ok','stale')),
    last_seen INTEGER NOT NULL
);
CREATE INDEX files_case_key ON files(case_key);

CREATE TABLE notes (
    id           INTEGER PRIMARY KEY,
    rel          TEXT REFERENCES files(rel) ON UPDATE CASCADE,
    title        TEXT,
    title_source TEXT,
    encoding     TEXT,
    word_count   INTEGER,
    body         TEXT
);
CREATE INDEX notes_rel ON notes(rel);

CREATE TABLE headings (
    note_id INTEGER NOT NULL,
    level   INTEGER NOT NULL,
    text    TEXT    NOT NULL,
    line    INTEGER NOT NULL
);

CREATE TABLE tags (
    note_id INTEGER NOT NULL,
    tag     TEXT    NOT NULL
);
CREATE INDEX tags_tag ON tags(tag);

CREATE TABLE links (
    src_note_id INTEGER NOT NULL,
    dst_note_id INTEGER,
    kind        TEXT    NOT NULL CHECK (kind IN ('wiki','md','attachment')),
    target      TEXT    NOT NULL,
    line        INTEGER NOT NULL,
    origin      TEXT    NOT NULL CHECK (origin IN ('prose','approved'))
);
CREATE INDEX links_dst_note_id ON links(dst_note_id);

CREATE TABLE items (
    note_id INTEGER NOT NULL,
    text    TEXT    NOT NULL,
    line    INTEGER NOT NULL,
    state   TEXT    NOT NULL CHECK (state IN ('open','done','paused')),
    marker  TEXT
);

CREATE TABLE meta (
    key   TEXT PRIMARY KEY,
    value TEXT
);

INSERT INTO schema_version (version) VALUES (1), (2);
