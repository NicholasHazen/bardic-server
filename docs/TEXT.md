# How book text becomes chapters and lines

What the importer does, so clients and later milestones can rely on it. The text is stored once and never changed (a database trigger refuses updates). The original file is kept under `originals/{book_id}/source.{ext}`.

## Offsets
Every offset and span is a **zero-based Unicode code point** count (Rust `char`), end exclusive. Not bytes, not UTF-16 units. JavaScript clients must count code points (`Array.from(text)` or an iterator), not `String.length`.

## Canonical chapter text
A chapter's `text` is its paragraphs joined by one blank line (`"\n\n"`). Inside a paragraph, runs of whitespace (including hard-wrapped line breaks from plain text files) are collapsed to one space. Nothing else is altered: no quote changes, no spelling changes.

## Lines
A *line* is the smallest unit that is spoken and highlighted.
- One paragraph is one line.
- A paragraph longer than 600 characters is cut into lines at sentence ends (`.`, `!`, `?`, `…`, with any closing quotes or brackets), each at least about 250 characters where possible. With no sentence end, it is cut at the last space in the window, else hard at 600.
- Lines never overlap and appear in order; the gaps between them are only the `"\n\n"` separators and the single spaces dropped at cut points.

## Chapters
- **EPUB:** the spine order. Navigation documents are skipped. A chapter's title is its first `h1` to `h3`, else the document title, else `Section N`. The heading text stays in the chapter text, so it is read aloud like any other line.
- **Text file:** a chapter starts at a short paragraph (80 characters or fewer) beginning `Chapter`, `Part`, `Book`, `Section`, `Prologue`, `Epilogue`, `Interlude`, `Preface` or `Introduction`, when at least two exist; otherwise at lone roman numerals when at least two exist. Text before the first heading becomes a chapter titled `Beginning`. With no headings the whole file is one chapter, titled like the book.
- **Kind:** `front_matter` for titles such as Copyright, Contents, Dedication, Epigraph; `back_matter` for Acknowledgements, About the Author, Colophon; otherwise `story`. Only story chapters count in `story_chapter_count` and `word_count`; `chapter_count` counts every chapter.

## Markup handling (EPUB)
A forgiving reader, not a browser: scripts, styles, headers, SVG and navigation are dropped; block elements (`p`, `div`, headings, list items, quotes, table rows…) become paragraphs; `<br>` becomes a space; common named and all numeric entities are decoded; malformed markup never fails an import.

## Safety and failure
- EPUBs may expand to at most 100 MB and 5,000 files; links cannot escape the archive.
- DRM is detected from `META-INF/encryption.xml` (font obfuscation is not DRM) and refused with `import_drm_protected`.
- Text files must be UTF-8 (a BOM is ignored) and contain no NUL bytes.
- A failed or cancelled import leaves no book, no original and no upload. An import that was running when the server stopped is failed as `import_interrupted` at the next start, and its half-made book is removed.

## Cover
The cover becomes a JPEG of at most 240 x 360 and a colour sample (`hex`, `hue`, `saturation`, `lightness`, `vivid`, `version`) measured by a deterministic algorithm documented in `src/cover.rs`. A cover that cannot be read does not fail the import.

## Search
Case-insensitive over stored text, folding each character to lower case one-to-one so offsets stay valid. Hits carry the chapter, line, code point span, and up to 40 characters of context either side.
