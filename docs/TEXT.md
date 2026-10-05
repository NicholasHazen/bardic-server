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
- **EPUB:** the spine order. Navigation documents are skipped as spoken text. Names come from EPUB 3 TOC navigation, then EPUB 2 NCX, then the first `h1` to `h3`, then the document title. Unnamed story units become `Chapter N`; unnamed matter gets `Front matter N` or `Back matter N`. Navigation paths are relative to their own documents, percent-decoded and matched to archive entries. Page lists, broken links and malformed optional metadata are ignored. Navigation never reorders or splits spine units. Multiple distinct chapter anchors inside one source unit remain one unit, using its heading/fallback rather than selecting one sibling's name for all of it.
- **Text file:** a chapter starts at a short paragraph (80 characters or fewer) beginning `Chapter`, `Part`, `Book`, `Section`, `Prologue`, `Epilogue`, `Interlude` or a recognized matter title, when at least two exist; otherwise at lone roman numerals when at least two exist. Text before the first heading becomes a chapter titled `Beginning`. With no headings the whole file is one chapter, titled like the book. Heading paragraphs stay in canonical text, as they do in EPUBs.
- **Kind:** whole-document EPUB structural types/roles, landmarks and OPF guide references take precedence over title and filename fallbacks. Titles such as Copyright, Contents, Dedication, Epigraph, Preface and Foreword identify `front_matter`; Acknowledgements, About the Author, Afterword, Colophon, Bibliography and Index identify `back_matter`; otherwise `story`. Fallbacks match label boundaries rather than arbitrary substrings. Explicit chapters, prologues and epilogues are story; nested notes or matter blocks cannot exclude an entire mixed story document. Unrecognized content remains story. Only story chapters count in `story_chapter_count` and `word_count`; `chapter_count` counts every chapter.

## Hiding and audio selection
Matter is retained with its exact text and can be read or played explicitly. `listChapters?include_matter=false` returns story metadata only, preserving original indices and ids. It does not remove text or audio and does not change audio selection.

Free make-ready and premium plan preview accept `scope.include_matter=false` for any scope kind. This filters the selected chapter ids before estimating or queuing work. Approval keeps those ids, so refreshing matter labels later cannot broaden an existing paid plan. `requestChapterAudio` also accepts `include_matter=false` to skip matter in free ahead generation while honoring the explicitly requested chapter. Omitting these flags keeps historical all-chapter behavior; new client flows explicitly send false by default.

## Refreshing existing chapter metadata
`POST /api/books/{book_id}/chapters/refresh` parses the saved original outside the database lock, then checks every ordered chapter's text byte for byte in one short transaction before updating titles, kinds and story totals. Chapter and line ids, text, hashes, audio, places, jobs and approved plan selections are untouched. A metadata change emits `book.updated` and is audited; repeating it unchanged is a no-op. Missing/unreadable originals return `source_unavailable`; changed text or boundaries return `chapter_structure_changed`, with no partial changes.

Older text imports omitted heading paragraphs. Those books keep their stored text and fail the refresh match if reparsing would restore headings; adding a new copy is required to use the corrected text import. Existing EPUB text is compatible with metadata-only refresh. No automatic rewrite or provider request occurs.

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
