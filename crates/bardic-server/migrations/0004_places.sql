-- M2: places, their history, and a stored chapter length for progress.
ALTER TABLE chapters ADD COLUMN char_len INTEGER NOT NULL DEFAULT 0;
UPDATE chapters SET char_len = length(text);

CREATE TABLE places (
  listener_id    TEXT NOT NULL REFERENCES listeners(id) ON DELETE CASCADE,
  book_id        TEXT NOT NULL REFERENCES books(id) ON DELETE CASCADE,
  chapter_id     TEXT NOT NULL,
  char_offset    INTEGER NOT NULL,          -- code points into the chapter text
  progress       REAL NOT NULL,             -- share of story text before the place
  mode           TEXT NOT NULL CHECK (mode IN ('listening','reading')),
  audiobook_id   TEXT,
  device_id      TEXT NOT NULL,
  device_name    TEXT NOT NULL,
  revision       INTEGER NOT NULL,
  updated_at     TEXT NOT NULL,
  marked_at      TEXT,                      -- set when marked finished; cleared by any change of place
  PRIMARY KEY (listener_id, book_id)
);

-- Earlier places, newest first by id. At most 10 per listener and book.
CREATE TABLE place_history (
  id             INTEGER PRIMARY KEY AUTOINCREMENT,
  listener_id    TEXT NOT NULL REFERENCES listeners(id) ON DELETE CASCADE,
  book_id        TEXT NOT NULL REFERENCES books(id) ON DELETE CASCADE,
  chapter_id     TEXT NOT NULL,
  char_offset    INTEGER NOT NULL,
  progress       REAL NOT NULL,
  mode           TEXT NOT NULL,
  audiobook_id   TEXT,
  device_id      TEXT NOT NULL,
  device_name    TEXT NOT NULL,
  revision       INTEGER NOT NULL,
  updated_at     TEXT NOT NULL
);
CREATE INDEX place_history_lookup ON place_history(listener_id, book_id, id DESC);
