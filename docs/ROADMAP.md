# Server roadmap

Milestones end with exit criteria that can be checked against the spec's acceptance tests. Operation ids are from `docs/contract/openapi.yaml`.

| # | Milestone | Scope | Exit criteria |
|---|---|---|---|
| M0 (done) | Skeleton and harness | Workspace, config, single-instance lock, migrations, conformance harness, `getHealth`, `getServer`, devices, audit log, event stream skeleton. | Harness fails on an undeclared field and on an undocumented status. A second instance refuses to start. |
| M1 (done) | Listeners, library, import | Listeners and settings; imports (EPUB, text), duplicate check, sample book, series, chapters, text, cover and colour sample, search, remove and restore. | P1 tested byte for byte. A1 to A10 and L1 to L7 acceptance tests pass with original fixtures. |
| M2 (done) | Places | `putPlace` with revisions and conflicts, history, finished (marked and automatic), place notices. | P5 and P7, C1 to C7, D1 and C6 tests pass, including the fake clock boundary. |
| M3 (done) | Free audio | Voice sources `breeze` and `local`; audiobooks; jobs, pacing, recovery; audio delivery with ranges; timings; `requestChapterAudio`, `makeAudiobookReady`. | P3 crash test passes. First audio and resume targets met on the reference machine. |
| M4 | Premium audio and money | `gemini` source; prices; `previewPlan`, `createPlan`, plan state machine, Allowance, spend with unknowns, quotas as `waiting`. | P2 and P6, D9 and PL1 to PL11 tests pass against a fake provider; one bounded, authorised live check. |
| M5 | Offline, space, deletion, export | Manifest, `checkDownloads`, newer audio, free up space, scheduled deletion with undo, M4B export, backup. | O1 to O8 server-side tests, D6 and G2 to G4 tests pass. |
| M6 | Hardening | Content refusals, provider edge cases, restart and quota soak, security review, performance targets at 500 books. | Spec section 10 targets met; open items in spec section 15 closed or scheduled. |

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
