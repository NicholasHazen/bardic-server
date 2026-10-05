-- Measured against the live API on 2026-09-30 (two short requests): about 32 audio tokens per second
-- of speech and 2.3 to 3.0 audio tokens per spoken character, plus about 0.25 text tokens per
-- character, at $9.00 and $0.50 per million. That is $22 to $27 per million characters; the seeded
-- $16.30 assumed 1.8 audio tokens per character. $25.00 puts that span inside the 75% to 140% range.
-- Only an unedited seeded price is changed.
UPDATE prices SET per_unit = 25000000, as_of = '2026-09-30T00:00:00.000Z'
WHERE provider = 'gemini' AND unit = 'million_characters' AND per_unit = 16300000 AND basis = 'manual';
