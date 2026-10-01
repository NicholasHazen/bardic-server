# Server roadmap

Milestones end with exit criteria that can be checked against the spec's acceptance tests. Operation ids are from `docs/contract/openapi.yaml`.

| # | Milestone | Scope | Exit criteria |
|---|---|---|---|
| M0 (done) | Skeleton and harness | Workspace, config, single-instance lock, migrations, conformance harness, `getHealth`, `getServer`, devices, audit log, event stream skeleton. | Harness fails on an undeclared field and on an undocumented status. A second instance refuses to start. |
| M1 (done) | Listeners, library, import | Listeners and settings; imports (EPUB, text), duplicate check, sample book, series, chapters, text, cover and colour sample, search, remove and restore. | P1 tested byte for byte. A1 to A10 and L1 to L7 acceptance tests pass with original fixtures. |
| M2 (done) | Places | `putPlace` with revisions and conflicts, history, finished (marked and automatic), place notices. | P5 and P7, C1 to C7, D1 and C6 tests pass, including the fake clock boundary. |
| M3 (done) | Free audio | Voice sources `breeze` and `local`; audiobooks; jobs, pacing, recovery; audio delivery with ranges; timings; `requestChapterAudio`, `makeAudiobookReady`. | P3 crash test passes. First audio and resume targets met on the reference machine. |
| M4 (done) | Premium audio and money | `gemini` source; prices; `previewPlan`, `createPlan`, plan state machine, Allowance, spend with unknowns, quotas as `waiting`. | P2 and P6, D9 and PL1 to PL11 tests pass against a fake provider; one bounded, authorised live check. |
| M5 (done) | Offline, space, deletion, export | Manifest, `checkDownloads`, newer audio, free up space, scheduled deletion with undo, M4B export, backup. | O1 to O8 server-side tests, D6 and G2 to G4 tests pass. |
| M6 (mostly done; open items below) | Hardening | Content refusals, provider edge cases, restart and quota soak, security review, performance targets at 500 books. | Spec section 10 targets met; open items in spec section 15 closed or scheduled. |

Out of scope until the spec changes: characters, casting, performances, voice design and cloning, roles and passwords, notifications.

## Notes from M0
- The conformance harness found three contract gaps on its first run (nullable `Actor.listener_*`, the `X-Bardic-Device` header missing on some mutating operations, undocumented 400 and 500). They are fixed in contract 0.2.0; see `docs/contract/CHANGELOG.md`. This is the intended loop: implement, let the harness object, change the contract first.
- **Browser clients on another origin need CORS.** Not built yet: add an explicit allow-list (`--allow-origin`) in M1, since the spec requires foreign-origin browser writes to be refused by default.
- Device `last_seen_at` is written at most every 30 seconds per device.
- Coverage is tracked in `tests/coverage.rs`: move operation ids into `IMPLEMENTED` as they land.

## Notes from M1
- Contract 0.2.0 grew two more fixes found by the harness: operations taking `X-Bardic-Listener` document `404 listener_not_found`, and `cancelImport` was changed in the server so an import reports `cancelled` only after it cleaned up (a race the test exposed).
- `audiobook_count` is 0 until M3.
- CORS: `--allow-origin` (repeatable, or `BARDIC_ALLOW_ORIGINS`). Browser writes from any other origin get 403 `origin_not_allowed`; requests with no `Origin` (curl, scripts) are unaffected.
- The import runs as a background task with stages `reading`, `finding_chapters`, `preparing_text`, `done`; cancellation is honoured between stages.
- Known limits: search scans chapter text (fine for a household library); `listBooks` uses offset cursors.

## Notes from M2
- Contract 0.2.1: `PlaceConflict` is flat (strict validators cannot check `allOf` with a strict `Error`); `updateBook`, `refreshBookCover`, `removeBook` and `restoreBook` take an optional `X-Bardic-Listener` so the returned `Book.place` can be filled.
- A write from the device that wrote the server's place is never a conflict, whatever its `base_revision` (a lost response must not lock a device out). Identical writes change nothing, including the clock.
- Finished is computed when read, from the stored `marked_at`, progress and `updated_at`; there is no background job. Marking or reopening bumps the revision and `updated_at`.
- `listBooks` applies the place filters after reading the viewer's places, so `in_progress`/`finished` use the same clock rule as `getPlace`. `recent` orders by latest place, then newest added. Cursors are still offsets.
- History entries always report `finished` false: finished describes the current place only.
- `audiobook_id` on a place is stored as sent; it is validated when audiobooks exist (M3).
- `place.updated` notices carry `listener_id`; the event-stream test for them arrives with M3's stream work.

## Notes from M3a
- Breeze is read through `/health`, `/v1/voices` and `/v1/voices/{id}/reference`. Only `cloned` voices are offered; a voice's revision hashes settings, instruction, reference text and the reference clip's SHA-256, not its label.
- Voice ids are stable across refreshes (source id plus the source's own id). A refresh that cannot reach the server keeps the last voices and marks them `available: false`; removing a source does the same, so audio made with them stays playable.
- `local` has no engine in this server and stays `unavailable`; `gemini` is refused until M4.
- Keys are stored as plain text in the database (personal use, same trust as the data directory) and never returned or written to the audit log.
- Not yet: `getVoiceSample` and the daily refresh (M3b, with audio generation and background tasks), audio states in `listAudiobookChapters` (always `not_yet`), `chapters_ready`, `bytes` and `active_job_id` (always zero or null).

## Notes from M3b
- **One worker, one request at a time** (a voice server is one GPU). Order: urgent jobs (created by `requestChapterAudio`, or joined by it) first, then background jobs, each oldest first, chapters by position. A chapter is written only when complete (temp file, then rename, then the row), so stop, crash or an unreachable server never leaves a half-made chapter; finished chapters are kept.
- A chapter is split into requests of whole lines, at most `BARDIC_AUDIO_CHUNK_CHARS` (2500) characters each; each request is the exact chapter text from its first line's start to its last line's end. Line timings come from the server's sentence segments when they validate, otherwise audio is spread over lines by length.
- Failures: unreachable (3 retries with doubling waits), busy (waits `Retry-After`, job shows `waiting`), then `needs_you` with a code: `source_unreachable`, `key_rejected`, `voice_not_found`, `voice_changed` (the voice's revision differs from the audiobook's, so nothing is spoken), `source_busy`. A refused chapter fails alone (`provider_refused`); the job goes on and ends `needs_you`. `resumeJob` retries failed chapters. The server's own error text is never passed on, only its fixed code.
- Pause and cancel drop the in-flight request (closing the stream stops the server's work). A running job at startup goes back to `queued`.
- Voices are refreshed daily by the worker loop (relative to server start).
- **Open: audio size.** WAV is 172 MB per hour. Fine for a household server, heavy for offline downloads. Options: an encoder dependency (Opus or AAC) chosen together with the M4B decision in M5, or ffmpeg if present. Clients read `content_type`, so switching is not a contract break.
- Not yet: `getAudiobookSpace`/`freeAudiobookSpace` (M5), plans (M4), `Retry-After` on queue-full.

## Notes from M4a
- Gemini: key checked against `GET /v1beta/models` (free); 30 prebuilt voices with Google's one-word descriptions; revision is `<model>/1`. The base URL is configurable (`BARDIC_GEMINI_URL`) for tests and proxies.
- Prices are per million characters spoken, in USD only. The seeded Gemini price ($16.30 per million characters) is derived from token rates ($0.50 per million text tokens, $9.00 per million audio tokens) and about 1.8 audio tokens and 0.25 text tokens per spoken character. It is labelled manual with its date; estimates will show a range around it. **No provider price interface is connected** (the Google Cloud Billing Catalog needs a different key and a SKU mapping that has to be checked live), so `refreshPrices` reports that honestly.
- Allowance: one row, default no monthly limit and a $10 default plan limit; the period is the calendar month in UTC. The `spend` ledger exists (reserved, known, unknown); plans and paid requests (M4b, M4c) write to it. Unknown items are counted, never added as zero.

## Notes from M4b/c
- **Money safety.** Every paid request is reserved (at its high estimate) before it is sent and settled from the usage Gemini reports: input tokens at $0.50 and output audio tokens at $9.00 per million (dated 2026-09-27, in `voices/gemini.rs`), rounded up. Incomplete usage, timeouts and a stop mid-request are recorded as unknown; a reservation found at startup becomes unknown. Refused, rate-limited, unreachable and rejected-key requests cost nothing.
- **Limits.** A chapter starts only if the rest of it fits under the plan's limit and the month's Allowance at its high estimate; each request reserves again. A plan can still pass its limit by the gap between estimate and real cost on the request in flight.
- **Estimates use the editable price table** (per million characters); **actuals use token rates** in code. If the owner edits the table, estimates move and actuals do not.
- **Quotas.** A 429 makes the plan `waiting` with `until` (Retry-After, at least 1 s); the worker re-queues it when the time comes, same limit, no approval. There is no local rate pacing yet (Gemini limits vary per project), so the first 429 is how a limit is learned.
- **Pause and stop on a plan** never discard paid work: the request in flight finishes and the chapter in hand is kept; pause takes effect at the chapter boundary. Finished requests of a chapter are kept in `chapter_parts` (for both providers), so a wait, stop or crash costs at most the request in flight.
- **Gemini has no sentence timings**, so read-along timings for premium audio are spread by line length and less exact than Breeze's.
- **Decisions on the spec's open items:** "success with no audio" counts as spent (known if usage is reported, else unknown) and fails the chapter without an automatic retry; three failed chapters in a row stop a plan (`repeated_failure`). No provider price interface is connected (see M4a).
- Not built: the local request pacer, invoice reconciliation, `getAudiobookSpace` remake estimates (M5).

## Notes from M5
- **Freed audio keeps its record** (`audio.deleted_at`, file and path gone), so `checkDownloads` can compare what a device holds with what replaced it. A copy the server freed but did not remake is reported up to date.
- **Deletion** is a stored schedule (`deletions`), run by the worker loop (checked every second while one is pending; also at startup). It refuses to schedule while audio is being made for the book. A finished deletion keeps its row so `deletion_done` can be answered. Rows go first, files after: a crash leaves orphan files, never a book without files. The spend ledger and audit log are kept.
- **Backup** is `VACUUM INTO` (one consistent snapshot; it holds the database connection for the duration, which is short for a household library) plus hard links of originals, audio and samples. It is on the same disk: a copy elsewhere is the owner's job. There is no restore operation; to restore, stop the server and replace `bardic.db` and the media folders from the backup folder.
- **Export** needs `ffmpeg` (`BARDIC_FFMPEG` to point at it): concat of the WAV chapters, FFMETADATA chapter markers, AAC 64 kb/s mono in an `ipod` (M4B) container. Not run against real ffmpeg yet; tests use a stub that records its arguments. No cover art in the file yet. This is also the answer to the WAV size problem for downloads of a whole book, but not for per-chapter offline downloads, which are still WAV.
- `tests/live_breeze.rs` is a bounded check for a real Breeze server (ignored by default): see the file header.

## Live checks (2026-09-30)
- **Breeze** (`tests/live_breeze.rs`, a Spark host over Tailscale, free): source connected, 30+ cloned voices, a sample of 6.8 s in 11.3 s, a three-line chapter of 10.2 s in 10.8 s, line timings ordered and inside the audio. Passed.
- **Gemini** (`tests/live_gemini.rs`, paid, authorised: one sample, one plan limited to $0.05, plus two reruns of the same, about $0.01 in all). Audio was made, kept and played; the key check, voice list and plan state machine behaved. It found three things the fake could not:
  1. **Every request reports about 200 audio input tokens** (`input_tokens_by_modality` has `audio`). Google's price page lists only text input and audio output for this model, so audio input is treated as unbilled. The parser had (correctly) refused to price it and recorded the request as unknown. It now accepts text and audio input, bills text only, and the note on an unknown cost carries the token counts (numbers only). **Confirm against the Google billing report that audio input is not charged.**
  2. **The seeded price was low.** Real speech is about 32 audio tokens per second and 2.3 to 3.0 audio tokens per spoken character (two short requests), not 1.8. A 118-character chapter cost about $0.0032 against a high estimate of $0.0027. Migration 0010 moves an unedited seed to $25.00 per million characters (range $18.75 to $35.00). Two data points are not a distribution: re-measure on a long chapter before trusting the range.
  3. **Google's rates double on 2027-01-01** ($1.00 text input, $18.00 audio output per million). Actuals use the date (`voices/gemini.rs::rates`); **estimates and reservations come from the price table, which does not change by itself.** Owner action or a dated price row is needed before then: open item for M6.
- The fake Gemini now reports the audio input tokens too.

## Notes from M6
- **Performance at 500 books** (`tests/perf.rs`, ignored, release build; 500 synthetic books, 147 MB of text, 24 chapters each, database 219 MB): import 31 ms per book; `listBooks` page p95 1.4 ms (all sorts and filters; the whole library in 10 pages in 24 ms); `searchBook` p95 about 2 ms; `getChapterText` and `getPlace` 0.3 ms. Spec 10.1 targets for the library, search and resume are met by three orders of magnitude on this machine. The free-voice targets were measured in M3 and again live (above).
- **Real ffmpeg export** (`a_real_ffmpeg_makes_a_playable_m4b_with_chapter_markers`, skipped when ffmpeg or ffprobe is missing): the M4B is a valid `mov,mp4` file with one mono AAC stream, one marker per exported chapter, ordered and ending where the audio ends. Closes the M5 "not run against real ffmpeg" gap. Cover art is still not embedded.
- **Provider edge cases** (fake Gemini now also answers 4xx/5xx, a non-JSON body and undecodable audio): a content refusal (400) costs nothing and fails only that chapter; 5xx, 503, garbage and bad audio are one unknown item each and are never retried by themselves; the provider's own error text is never passed on.
- **Soak** (`a_soak_of_faults_and_restarts_keeps_the_ledger_and_the_audio_honest`, four seeds): random faults and up to three restarts per run on a whole-book plan. Invariants: the ledger has a row for every request the provider received (and at most one more per restart), nothing stays reserved, held spending stays inside the limit, every chapter marked ready has its file at the recorded size, no `.part` file is left.
- **Security review** (red-team agent, then each finding verified in code before acting). Fixed: (1) **DNS rebinding**: a page on a public name pointed at this machine passed the same-origin rule because `Origin` and `Host` agreed; every request is now refused with 403 `host_not_allowed` unless `Host` is an IP, `localhost`, a single-word or private-network (`.local`, `.lan`, `.home.arpa`, `.ts.net`) name, or listed with `--allow-host` / `--allow-origin`. (2) **Stop** let the whole chapter in hand be made, and a stop could race ahead of the first request; a reservation now requires the plan's job not to be stopped, so stop takes effect at the next request (pause is unchanged: chapter boundary, as documented). (3) **Unknown spend did not count toward limits**, so a billed-but-unreadable request made room to be sent again; for limits it now counts at its held amount (shown as unknown, not spent). Also: `searchBook` `q` at most 200 characters; an EPUB spine entry is read once and a book's text is capped at 100 MB; a cover's size is read from its header before it is decoded; the data folder is mode 0700 (keys, backups and exports live there); `resumePlan` refuses while the book is scheduled for deletion.
- **Contract 0.3.0** (tightening: the `q` limit). See the changelog.
- **Still open (M6):**
  1. **Gemini rates double on 2027-01-01.** Actuals follow the date; the estimate table and reservations do not. Before then: a dated price row or an edit of the table, and check that reservations (high estimate) still cover a request.
  2. **Estimate range from two measurements.** Re-measure on a long chapter; also whether audio input is billed (Google's page says no).
  3. **Concurrency caps.** No limit on concurrent imports, SSE connections, exports (one ffmpeg per request, plus `ffmpeg -version` each time) or backups; add a global semaphore or one-active-per-audiobook.
  4. **Search holds the database lock for the scan** (the 200-character cap bounds the worst case; copying text out first would remove it).
  5. **Keys at rest.** Plain text in `bardic.db`, and so in backups and the `pre-migration` copies; the folder is now private, but a backup copied elsewhere carries the keys. Redact in backups or document it. `GET /api/backups` returns the absolute server path.
  6. **Breeze `base_url` is client-controlled** (deliberate for a LAN voice server): any client can aim it at any host, and book text then goes there; omitting `api_key` re-sends the stored key. The Host guard narrows who can do this; an `--allow-voice-host` list would close it.
  7. Deleting a book while a paid chapter is mid-flight can leave an orphan `.part` file (resume is now refused, but the in-flight chapter is not interrupted); backups keep hard links to deleted media.
- Spec section 15: items 2 (Gemini no-audio) and 3 (export encoder: ffmpeg) were decided in M4 and M5; item 1 (price interfaces) is still a manual table; 4 and 5 are unchanged.
