-- M4b/c: estimates, plans, and chapters being made in parts so finished work survives a stop.
ALTER TABLE jobs ADD COLUMN wake_at TEXT;     -- a waiting job is queued again at this time

CREATE TABLE estimates (
  id            TEXT PRIMARY KEY,
  audiobook_id  TEXT NOT NULL REFERENCES audiobooks(id) ON DELETE CASCADE,
  scope         TEXT NOT NULL,                -- JSON Scope as asked
  chapter_ids   TEXT NOT NULL,                -- JSON array: the chapters that would be made
  chars         INTEGER NOT NULL,
  low           INTEGER NOT NULL,
  likely        INTEGER NOT NULL,
  high          INTEGER NOT NULL,
  per_unit      INTEGER NOT NULL,             -- the price used, micros per million characters
  prices_as_of  TEXT NOT NULL,
  basis         TEXT NOT NULL,
  suggested     INTEGER NOT NULL,
  expires_at    TEXT NOT NULL,
  created_at    TEXT NOT NULL,
  used_by       TEXT                          -- the plan that consumed it; an estimate is approved once
);

CREATE TABLE plans (
  id              TEXT PRIMARY KEY,
  audiobook_id    TEXT NOT NULL REFERENCES audiobooks(id) ON DELETE CASCADE,
  book_id         TEXT NOT NULL,
  scope           TEXT NOT NULL,
  est_low         INTEGER NOT NULL,
  est_likely      INTEGER NOT NULL,
  est_high        INTEGER NOT NULL,
  prices_as_of    TEXT NOT NULL,
  basis           TEXT NOT NULL,
  limit_micros    INTEGER NOT NULL,
  job_id          TEXT NOT NULL,
  approved_by     TEXT NOT NULL,              -- JSON Actor
  idempotency_key TEXT,
  created_at      TEXT NOT NULL,
  updated_at      TEXT NOT NULL
);
CREATE UNIQUE INDEX plans_idempotency ON plans(idempotency_key) WHERE idempotency_key IS NOT NULL;
CREATE INDEX plans_by_audiobook ON plans(audiobook_id);

-- A chapter made in requests. Each finished request is kept, so stopping, waiting for a quota or a crash
-- costs at most the request in flight, and paid audio is never thrown away.
CREATE TABLE chapter_parts (
  audiobook_id TEXT NOT NULL REFERENCES audiobooks(id) ON DELETE CASCADE,
  chapter_id   TEXT NOT NULL,
  audio_id     TEXT NOT NULL,
  chunk_chars  INTEGER NOT NULL,
  chunks_done  INTEGER NOT NULL,
  pcm_bytes    INTEGER NOT NULL,
  timings      TEXT NOT NULL,
  PRIMARY KEY (audiobook_id, chapter_id)
);
