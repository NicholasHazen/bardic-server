-- M1a: listeners and their settings.
CREATE TABLE listeners (
  id               TEXT PRIMARY KEY,
  name             TEXT NOT NULL,
  name_key         TEXT NOT NULL UNIQUE,   -- lower-cased, for case-insensitive uniqueness
  created_at       TEXT NOT NULL,
  last_listened_at TEXT
);

CREATE TABLE listener_settings (
  listener_id                TEXT PRIMARY KEY REFERENCES listeners(id) ON DELETE CASCADE,
  default_voice_id           TEXT,
  place_conflict             TEXT NOT NULL DEFAULT 'ask' CHECK (place_conflict IN ('ask','newest','this_device')),
  continue_into_next_chapter INTEGER NOT NULL DEFAULT 1
);
