# Contract changelog

Newest first. While the version is 0.x: additive changes bump the patch; breaking changes (a required field becoming nullable, removing or renaming anything, tightening validation) bump the minor and say what clients must change.

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
