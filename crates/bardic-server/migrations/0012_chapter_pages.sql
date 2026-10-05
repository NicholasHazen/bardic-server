-- Source pagination is optional metadata. Existing chapters stay unknown until
-- the original can be checked by an explicit, metadata-only refresh.
ALTER TABLE chapters ADD COLUMN page_count INTEGER CHECK (page_count IS NULL OR page_count >= 1);
