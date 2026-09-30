-- M4a: prices, the allowance, and the spending ledger.
CREATE TABLE prices (
  provider     TEXT PRIMARY KEY,
  unit         TEXT NOT NULL,
  per_unit     INTEGER NOT NULL,          -- micros of the currency per unit
  currency     TEXT NOT NULL,
  as_of        TEXT NOT NULL,
  basis        TEXT NOT NULL CHECK (basis IN ('provider','manual')),
  refresh_error TEXT
);
-- Gemini speech: about 1.8 audio tokens and 0.25 text tokens per spoken character at
-- $9.00 and $0.50 per million tokens, so about $16.30 per million characters.
INSERT INTO prices(provider,unit,per_unit,currency,as_of,basis,refresh_error)
VALUES ('gemini','million_characters',16300000,'USD','2026-09-27T00:00:00.000Z','manual',NULL);

-- One row. monthly_limit NULL means no monthly limit (the default).
CREATE TABLE allowance (
  id                 INTEGER PRIMARY KEY CHECK (id = 1),
  monthly_limit      INTEGER,
  default_plan_limit INTEGER NOT NULL,
  currency           TEXT NOT NULL
);
INSERT INTO allowance(id,monthly_limit,default_plan_limit,currency) VALUES (1,NULL,10000000,'USD');

-- Every paid request, reserved before it is sent and settled when it ends.
-- known_micros NULL with status 'unknown' is money that may have been spent and cannot be stated: never zero.
CREATE TABLE spend (
  id           TEXT PRIMARY KEY,
  plan_id      TEXT,
  audiobook_id TEXT,
  chapter_id   TEXT,
  at           TEXT NOT NULL,
  status       TEXT NOT NULL CHECK (status IN ('reserved','known','unknown','none')),
  reserved     INTEGER NOT NULL DEFAULT 0, -- micros held back before sending
  known_micros INTEGER,
  input_tokens INTEGER,
  output_tokens INTEGER,
  note         TEXT
);
CREATE INDEX spend_by_plan ON spend(plan_id);
CREATE INDEX spend_by_time ON spend(at);
