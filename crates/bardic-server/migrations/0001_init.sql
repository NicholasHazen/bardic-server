-- M0: server identity, devices, audit.
CREATE TABLE meta (
  key   TEXT PRIMARY KEY,
  value TEXT NOT NULL
);

CREATE TABLE devices (
  id            TEXT PRIMARY KEY,
  name          TEXT NOT NULL,
  first_seen_at TEXT NOT NULL,
  last_seen_at  TEXT NOT NULL
);
CREATE INDEX devices_last_seen ON devices(last_seen_at DESC);

-- Append-only. Never updated or deleted by the server.
CREATE TABLE audit (
  id            TEXT PRIMARY KEY,
  at            TEXT NOT NULL,
  action        TEXT NOT NULL,
  listener_id   TEXT,
  listener_name TEXT,
  device_id     TEXT NOT NULL,
  device_name   TEXT NOT NULL,
  target        TEXT NOT NULL
);
CREATE INDEX audit_action ON audit(action, id DESC);
CREATE INDEX audit_listener ON audit(listener_id, id DESC);
