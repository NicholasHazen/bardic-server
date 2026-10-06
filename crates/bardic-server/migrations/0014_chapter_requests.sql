-- Free requests can finish out of order. Immutable request files are durable
-- before their rows appear; chapter_parts remains the ordered contiguous prefix.
CREATE TABLE chapter_requests (
  audiobook_id  TEXT NOT NULL,
  chapter_id    TEXT NOT NULL,
  request_index INTEGER NOT NULL CHECK (request_index >= 0),
  audio_id      TEXT NOT NULL,
  pcm_bytes     INTEGER NOT NULL CHECK (pcm_bytes > 0),
  timings       TEXT NOT NULL, -- JSON per-line timings relative to this request
  sha256        TEXT NOT NULL,
  PRIMARY KEY (audiobook_id, chapter_id, request_index),
  FOREIGN KEY (audiobook_id, chapter_id)
    REFERENCES chapter_parts(audiobook_id, chapter_id) ON DELETE CASCADE
);
