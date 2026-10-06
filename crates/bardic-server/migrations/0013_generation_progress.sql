-- Internal current-chapter progress and same-job request timing samples. The API
-- exposes only documented fields; restart clears any in-flight request timer.
ALTER TABLE jobs ADD COLUMN generation TEXT;
