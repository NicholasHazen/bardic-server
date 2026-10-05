-- Contract 0.4.0: chapter_count counts every chapter; story_chapter_count keeps the old meaning.
-- Generated covers are flagged so a refresh knows to regenerate rather than re-read the original.
ALTER TABLE books ADD COLUMN story_chapter_count INTEGER NOT NULL DEFAULT 0;
ALTER TABLE books ADD COLUMN cover_generated INTEGER NOT NULL DEFAULT 0;
UPDATE books SET story_chapter_count = chapter_count;
UPDATE books SET chapter_count = (SELECT COUNT(*) FROM chapters WHERE chapters.book_id = books.id);
