-- M5: freed audio keeps its record (so a device can be told what changed), deletions, exports, backups.
CREATE TABLE audio_new (
  id             TEXT PRIMARY KEY,
  audiobook_id   TEXT NOT NULL REFERENCES audiobooks(id) ON DELETE CASCADE,
  chapter_id     TEXT NOT NULL REFERENCES chapters(id) ON DELETE CASCADE,
  voice_revision TEXT NOT NULL,
  path           TEXT NOT NULL,
  bytes          INTEGER NOT NULL,
  sha256         TEXT NOT NULL,
  duration_ms    INTEGER NOT NULL,
  content_type   TEXT NOT NULL,
  timings        TEXT NOT NULL,
  created_at     TEXT NOT NULL,
  deleted_at     TEXT                          -- freed: the file is gone, the facts remain
);
INSERT INTO audio_new(id,audiobook_id,chapter_id,voice_revision,path,bytes,sha256,duration_ms,content_type,timings,created_at)
  SELECT id,audiobook_id,chapter_id,voice_revision,path,bytes,sha256,duration_ms,content_type,timings,created_at FROM audio;
DROP TABLE audio;
ALTER TABLE audio_new RENAME TO audio;
CREATE UNIQUE INDEX audio_live ON audio(audiobook_id, chapter_id) WHERE deleted_at IS NULL;

-- Permanent deletion. The row stays after it ran, so "deletion_done" can be answered.
CREATE TABLE deletions (
  book_id      TEXT PRIMARY KEY,               -- not a foreign key: the book is gone when it is done
  prior_state  TEXT NOT NULL,                  -- readable or removed: what cancelling returns it to
  state        TEXT NOT NULL CHECK (state IN ('pending','cancelled','done')),
  scheduled_at TEXT NOT NULL,
  executes_at  TEXT NOT NULL,
  scheduled_by TEXT NOT NULL,                  -- JSON Actor
  finished_at  TEXT
);

CREATE TABLE exports (
  id           TEXT PRIMARY KEY,
  audiobook_id TEXT NOT NULL REFERENCES audiobooks(id) ON DELETE CASCADE,
  state        TEXT NOT NULL CHECK (state IN ('queued','running','ready','failed')),
  format       TEXT NOT NULL,
  bytes        INTEGER,
  path         TEXT,
  job_id       TEXT NOT NULL,
  error        TEXT,
  created_at   TEXT NOT NULL
);

CREATE TABLE backups (
  id         TEXT PRIMARY KEY,
  state      TEXT NOT NULL CHECK (state IN ('running','done','failed')),
  created_at TEXT NOT NULL,
  bytes      INTEGER,
  path       TEXT,
  error      TEXT
);
