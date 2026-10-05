# Contract changelog

Newest first. While the version is 0.x: additive changes bump the patch; breaking changes (a required field becoming nullable, removing or renaming anything, tightening validation) bump the minor and say what clients must change.

## 0.5.1 (chapter metadata and optional matter selection; additive)
- EPUB imports recover names from EPUB 3 navigation or EPUB 2 NCX, then headings/document titles, with structural metadata and conservative title fallbacks for matter kinds. Source text stays intact.
- New plain-text imports retain heading paragraphs in canonical text instead of dropping them; matter headings can form chapters. Old stored text stays unchanged, and refresh refuses a mismatch with the corrected importer.
- `listChapters` accepts `include_matter=false` to hide front/back matter while preserving ids and indices. The default remains all chapters.
- `Scope.include_matter=false` excludes matter from free make-ready and premium previews/approved plans, including explicit chapter scopes. Omitted defaults to true for existing clients and stored plans. Selection is independent of list visibility.
- `requestChapterAudio.include_matter=false` skips matter in free ahead-of-playback generation; the explicitly requested chapter is still honored. Premium generation stays inside the approved selection.
- `refreshBookChapters` re-reads the saved original to repair chapter names/kinds and story counts. It refuses any text/order/boundary mismatch and retains all ids, text, audio, places and existing plan selections; no provider work starts.

## 0.5.0 (sample safeguards and Ready-file recovery; tightens browser origin checks)
- `getVoiceSample` (including implicit HEAD) now refuses foreign browser provenance before it can generate audio or spend. Clients serving the web app from a separate origin must configure `--allow-origin`, as for writes. Requests with no Origin use Referer and Fetch Metadata when present; direct navigation with Fetch Metadata remains allowed. **Scripts must send `X-Bardic-Device` when every browser provenance header is absent.** The web client already sends it. This closes no-referrer media/no-cors requests on plain HTTP LAN addresses where Fetch Metadata may be omitted. The sample operation documents 403 `origin_not_allowed`.
- Concurrent sample cache misses for one voice revision share one generation and one spending record. A client disconnect does not cancel provider settlement or cause a duplicate request; a failed attempt permits a later deliberate retry.
- `getVoiceSample` documents the existing 416 response for an unsatisfiable byte range.
- Ready audio is reconciled at startup and when relevant resources are accessed. Missing, non-file or wrong-sized backing files become unavailable, retaining metadata, timings, spending and device copies. Reading state or audio never starts replacement generation. An explicit free request can regenerate; premium replacement still requires a running approved plan with that chapter queued, and completed plans stay completed.

## 0.4.0 (chapter counts and generated covers; changes the meaning of a field)
- **`Book.chapter_count` now counts every chapter**, front and back matter included (it counted story chapters only). It is the number an audiobook covers, so "N of M chapters ready" and a header count agree. **Clients that want the story-only number must read the new `Book.story_chapter_count`** (required; the old meaning; plain text has no matter, so the two are equal there). `word_count` is unchanged (story chapters).
- **Every book now has a cover.** A book with no cover image (text, the sample, an EPUB without one or with one that cannot be read) gets a generated cover: a deterministic 240 x 360 JPEG (2:3) with a gradient and soft blobs and **no text**, so the client overlays the title. Its hue is derived from the normalised title and author, and its `sample.vivid` is true. `Book.cover` is therefore null only while a book is still being added. Books already in a data folder get one when the server starts.
- **`Cover.generated`** (boolean, required) is true for these and false for covers from the file. Clients may show the title only when it is true.
- `refreshBookCover` on a generated cover draws it again from the current title and author (an edited title or author may change the colour; the URL changes with the hash). On a real cover it behaves as before. A generated cover never replaces a real one.
- `getBookCover` answers 404 `cover_not_found` only for a book that is still being added.

## 0.3.3 (description only)
- `getVoiceSample`: removed the stale line saying premium samples arrive with premium audio. A Gemini sample works whenever a key is accepted (since M4).

## 0.3.2 (M6: deletion and audio; behaviour only)
- `requestChapterAudio`, `makeAudiobookReady`, `resumeJob` and `resumePlan` answer 409 `deletion_pending` while the book is scheduled for deletion. A deletion that comes due stops any job still open for the book, and audio folders that belong to no audiobook are swept at startup and shortly after a deletion.

## 0.3.1 (M6: limits on work a client can start; backups; additive)
- `Backup.path` is now relative to the data folder (`backups/<id>`) instead of absolute, and the backup's database copy has the API keys removed.
- `streamEvents` documents `429 too_many_streams` (at most 64 open streams).
- `createExport` and `createBackup`: a second request while one runs for the same audiobook (backup: at all) returns the one under way. Imports are worked through three at a time; the rest stay `queued`. Encoding runs one ffmpeg at a time.

## 0.3.0 (M6: hardening; tightens validation)
- **`searchBook`: `q` is at most 200 characters** (400 `invalid_request` otherwise). A client that sends longer search text must shorten it.
- Spending rules: for limits, a request with unknown cost counts at its held-back amount; `resumePlan` and the worker therefore refuse to send it again past the limit (`limit_exceeded`). `resumePlan` also answers `deletion_pending` while the book is scheduled for deletion.
- `stopPlan` takes effect at the next request instead of at the end of the chapter in hand; the request already sent finishes and its audio is kept. `pausePlan` is unchanged (chapter boundary).
- Not in the contract: every request is refused with 403 `host_not_allowed` unless its `Host` is an IP address, `localhost`, a single-word or private-network name, or listed with `--allow-host` (DNS-rebinding guard).

## 0.2.6 (M5: space, offline, deletion, export, backup; additive)
- `downloadExport` documents `206`.
- Behaviour documented for `freeAudiobookSpace`, `checkDownloads`, `scheduleBookDeletion`, `createExport` and `createBackup`. New codes: `encoder_missing`, `export_not_ready`, `export_not_found`, `export_failed` (job detail), `deletion_pending`, `job_running` on deletion.
- `Book.audiobook_count` is now the real count (it was always 0 before).

## 0.2.5 (M4b/c: plans and paid audio; descriptions only)
- Plan operations, spending rules and their codes are documented: `plan_not_needed`, `estimate_not_found`, `estimate_used`, `nothing_to_make`, `plan_active`, `plan_not_resumable`, `provider_uncertain`, `provider_quota`.
- A plan's `state` mirrors its job: queued and running are both `running`; `approved` is never returned.
- Needs-you codes on plans and jobs: `limit_exceeded`, `allowance_exceeded`, `key_rejected`, `provider_refused`, `repeated_failure`; waiting: `waiting_quota` (`until` set).

## 0.2.4 (M4a: Gemini source, prices, Allowance; descriptions only)
- `configureVoiceSource` works for `gemini` (30 prebuilt voices, key checked against the free model list); `source_unsupported` is no longer returned.
- `refreshPrices` and `Price` document that prices are a dated manual table until a provider price interface is connected. Only `USD` is accepted for money.

## 0.2.3 (M3b: making audio; additive)
- `getVoiceSample` documents `206` (Range), `voice_changed` (409) and `rate_limited` (429).
- Descriptions of `requestChapterAudio` (joining and urgency) and `pauseJob`.
- New error codes in use: `audiobook_not_found`, `audio_not_found`, `job_not_resumable` (409), `voice_unavailable`, `voice_changed`, `source_unsupported`.
- Audio is 24 kHz 16-bit mono WAV (`content_type: audio/wav`) for now; clients must read `content_type`, not assume it.

## 0.2.2 (M3a: voice sources and audiobooks; descriptions only)
- `configureVoiceSource`: Breeze accepts an optional write-only `api_key` (an empty string clears it); `gemini` answers 400 `source_unsupported` until premium audio exists; source ids equal their kind.
- `VoiceSource.has_key` applies to Breeze as well as Gemini.
- `createAudiobook` documents `voice_unavailable` (409) and that an audiobook covers every chapter.

## 0.2.1 (found while implementing M2)
- `PlaceConflict` is a flat object (the `Error` fields plus `server_place`) instead of `allOf` with `Error`, so strict validators can check it. The wire format is unchanged.
- `updateBook`, `refreshBookCover`, `removeBook` and `restoreBook` accept an optional `X-Bardic-Listener`; with it, `Book.place` is that listener's summary, without it null.
- `putPlace`: `chapter_not_found` is 404 (the description said 400).

## 0.2.0 (BREAKING, found while implementing M0)
- `Actor.listener_id` and `Actor.listener_name` are now nullable. Renaming the server and creating the first listener happen when no listener applies, so the audit record could not be written honestly with a required listener. Clients that read `Actor` must accept null.
- `Notice.type` documents `server.updated` and `device.updated`.
- Every mutating operation now declares the `X-Bardic-Device` header (it was missing on `updateDevice`, `cancelImport`, `refreshVoiceSource`, `testVoiceSource`, `refreshPrices` and similar). Clients must send it on every POST, PUT, PATCH and DELETE.
- Every operation documents `400` (malformed header or parameter) and `500` (unexpected failure, `internal_error`). Found by the conformance harness.
- Every operation that takes `X-Bardic-Listener` documents `404` (`listener_not_found`). Found by the conformance harness in M1.
- README: error code `device_required` (400) for a mutating request without `X-Bardic-Device`.

## 0.1.0
First draft, from `docs/PRODUCT-SPEC.md`.
