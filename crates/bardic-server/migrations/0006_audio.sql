-- M3b: made audio, jobs that make it, and cached voice samples.
-- One audio row per audiobook chapter. The file lives at data/audio/<audiobook>/<audio id>.wav;
-- the id names exactly those bytes and is never reused.
CREATE TABLE audio (
  id             TEXT PRIMARY KEY,
  audiobook_id   TEXT NOT NULL REFERENCES audiobooks(id) ON DELETE CASCADE,
  chapter_id     TEXT NOT NULL REFERENCES chapters(id) ON DELETE CASCADE,
  voice_revision TEXT NOT NULL,
  path           TEXT NOT NULL,              -- relative to the data directory
  bytes          INTEGER NOT NULL,
  sha256         TEXT NOT NULL,
  duration_ms    INTEGER NOT NULL,
  content_type   TEXT NOT NULL,
  timings        TEXT NOT NULL,              -- JSON array of {line_id,start_ms,end_ms}
  created_at     TEXT NOT NULL,
  UNIQUE (audiobook_id, chapter_id)
);

CREATE TABLE jobs (
  id                 TEXT PRIMARY KEY,
  kind               TEXT NOT NULL,
  state              TEXT NOT NULL CHECK (state IN ('queued','running','waiting','paused','needs_you','completed','stopped','failed')),
  audiobook_id       TEXT REFERENCES audiobooks(id) ON DELETE CASCADE,
  book_id            TEXT,
  plan_id            TEXT,
  urgent             INTEGER NOT NULL DEFAULT 0,   -- someone is waiting for it (pressed play)
  chapters_total     INTEGER NOT NULL DEFAULT 0,
  chapters_done      INTEGER NOT NULL DEFAULT 0,
  current_chapter_id TEXT,
  waiting            TEXT,                         -- JSON Detail
  needs_you          TEXT,                         -- JSON Detail
  started_by         TEXT NOT NULL,                -- JSON Actor
  idempotency_key    TEXT,
  created_at         TEXT NOT NULL,
  updated_at         TEXT NOT NULL
);
CREATE UNIQUE INDEX jobs_idempotency ON jobs(idempotency_key) WHERE idempotency_key IS NOT NULL;
CREATE INDEX jobs_by_audiobook ON jobs(audiobook_id, state);

CREATE TABLE job_items (
  job_id     TEXT NOT NULL REFERENCES jobs(id) ON DELETE CASCADE,
  chapter_id TEXT NOT NULL,
  position   INTEGER NOT NULL,
  state      TEXT NOT NULL CHECK (state IN ('queued','done','failed','skipped')),
  detail     TEXT,
  PRIMARY KEY (job_id, chapter_id)
);

CREATE TABLE voice_samples (
  voice_id     TEXT NOT NULL REFERENCES voices(id) ON DELETE CASCADE,
  revision     TEXT NOT NULL,
  path         TEXT NOT NULL,
  bytes        INTEGER NOT NULL,
  content_type TEXT NOT NULL,
  created_at   TEXT NOT NULL,
  PRIMARY KEY (voice_id, revision)
);
