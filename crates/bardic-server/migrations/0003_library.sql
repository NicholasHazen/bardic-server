-- M1b: books, chapters, lines, imports.
CREATE TABLE books (
  id            TEXT PRIMARY KEY,
  title         TEXT NOT NULL,
  author        TEXT NOT NULL DEFAULT '',
  state         TEXT NOT NULL CHECK (state IN ('adding','readable','removed','deleting')),
  added_at      TEXT NOT NULL,
  removed_at    TEXT,
  series_name   TEXT,
  series_order  REAL,
  source_sha256 TEXT,
  source_name   TEXT,
  source_ext    TEXT,
  word_count    INTEGER NOT NULL DEFAULT 0,
  chapter_count INTEGER NOT NULL DEFAULT 0,
  cover_sha256  TEXT,
  cover_width   INTEGER,
  cover_height  INTEGER,
  cover_jpeg    BLOB,
  cover_sample  TEXT
);
CREATE INDEX books_source ON books(source_sha256);
CREATE INDEX books_state_added ON books(state, added_at DESC);

CREATE TABLE chapters (
  id          TEXT PRIMARY KEY,
  book_id     TEXT NOT NULL REFERENCES books(id) ON DELETE CASCADE,
  idx         INTEGER NOT NULL,
  title       TEXT NOT NULL,
  kind        TEXT NOT NULL CHECK (kind IN ('story','front_matter','back_matter')),
  text        TEXT NOT NULL,
  text_sha256 TEXT NOT NULL,
  word_count  INTEGER NOT NULL,
  UNIQUE (book_id, idx)
);

-- Promise P1: the words of a book are never changed after import.
CREATE TRIGGER chapters_text_is_immutable
BEFORE UPDATE OF text, text_sha256 ON chapters
BEGIN
  SELECT RAISE(ABORT, 'chapter text is immutable');
END;

CREATE TABLE lines (
  id         TEXT PRIMARY KEY,
  chapter_id TEXT NOT NULL REFERENCES chapters(id) ON DELETE CASCADE,
  idx        INTEGER NOT NULL,
  start      INTEGER NOT NULL,   -- code point offset, inclusive
  end        INTEGER NOT NULL,   -- code point offset, exclusive
  UNIQUE (chapter_id, idx)
);

CREATE TABLE imports (
  id           TEXT PRIMARY KEY,
  state        TEXT NOT NULL,
  file_name    TEXT NOT NULL,
  created_at   TEXT NOT NULL,
  progress     REAL NOT NULL DEFAULT 0,
  book_id      TEXT,
  error_code   TEXT,
  error_detail TEXT,
  idem_key     TEXT UNIQUE
);
