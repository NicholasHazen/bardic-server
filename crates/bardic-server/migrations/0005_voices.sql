-- M3a: voice sources, the voices they report, and audiobooks (one per book and voice revision).
CREATE TABLE voice_sources (
  id          TEXT PRIMARY KEY,                 -- the kind: breeze, gemini or local
  config      TEXT NOT NULL DEFAULT '{}',       -- JSON: base_url, api_key, enabled. Never returned.
  state       TEXT NOT NULL DEFAULT 'not_set_up',
  detail      TEXT,
  checked_at  TEXT,
  updated_at  TEXT NOT NULL
);

CREATE TABLE voices (
  id           TEXT PRIMARY KEY,
  source_id    TEXT NOT NULL REFERENCES voice_sources(id),
  external_id  TEXT NOT NULL,                   -- the id the source uses
  name         TEXT NOT NULL,
  tier         TEXT NOT NULL CHECK (tier IN ('free','premium')),
  language     TEXT NOT NULL,
  description  TEXT NOT NULL DEFAULT '',
  revision     TEXT NOT NULL,
  available    INTEGER NOT NULL DEFAULT 1,
  updated_at   TEXT NOT NULL,
  UNIQUE (source_id, external_id)
);

-- The voice's name and revision are copied so the audiobook stays true when the voice changes or its source is removed.
CREATE TABLE audiobooks (
  id             TEXT PRIMARY KEY,
  book_id        TEXT NOT NULL REFERENCES books(id) ON DELETE CASCADE,
  voice_id       TEXT NOT NULL REFERENCES voices(id),
  voice_name     TEXT NOT NULL,
  voice_revision TEXT NOT NULL,
  created_at     TEXT NOT NULL,
  UNIQUE (book_id, voice_id, voice_revision)
);

INSERT INTO voice_sources(id, updated_at) VALUES
  ('breeze', strftime('%Y-%m-%dT%H:%M:%fZ','now')),
  ('gemini', strftime('%Y-%m-%dT%H:%M:%fZ','now')),
  ('local',  strftime('%Y-%m-%dT%H:%M:%fZ','now'));
UPDATE voice_sources SET state='unavailable', detail='This server has no voices of its own.' WHERE id='local';
