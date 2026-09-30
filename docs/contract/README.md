# Bardic v2 API contract

`openapi.yaml` (OpenAPI 3.1) is the normative interface between the Bardic server and its clients. It is written from [`docs/PRODUCT-SPEC.md`](../PRODUCT-SPEC.md), not from the prototype. Clients are generated from it; a server is correct when it satisfies it.

**Status:** draft 0.1.0, pre-release. Until 1.0.0, additive changes bump the patch and breaking changes bump the minor.

**Validate:** `uv run --with openapi-spec-validator --with pyyaml python -c "from openapi_spec_validator import validate; from openapi_spec_validator.readers import read_from_filename as r; validate(r('openapi.yaml')[0])"`

## 1. Conventions

### Identity and acting
There are no passwords. Two headers say who is acting:

| Header | Meaning |
|---|---|
| `X-Bardic-Listener` | The listener acting. Required on operations that read or write per-listener state (places, settings, plans, approvals). Unknown id: 404 `listener_not_found`. |
| `X-Bardic-Device` | A client-generated device id (UUID kept in the browser). Registers the device on first use; recorded on every change. Required on mutating operations. |

The server has no "current listener": two devices can act as different people at once. Every mutation of a listener, plan, Allowance, key, source, price, deletion or export is written to the append-only audit log with its `Actor` (listener and device). This is the hook for controls later.

### Identifiers, times, text
- Ids are opaque strings; never parse them. Timestamps are RFC 3339 UTC.
- Text offsets are **zero-based Unicode code points**, end exclusive. They are not bytes and not UTF-16 units. A place is `(chapter_id, offset)`.
- Chapter text is immutable. `text_sha256` identifies it for offline caches.

### Money
`Money = {micros, currency}`, millionths of a unit as an integer; never a float. Costs are always shown as ranges (`CostEstimate`: `low`, `likely`, `high`, with `prices_as_of` and `basis`). Spending is `Spend = {known, unknown_items}`: an item whose cost could not be determined is counted in `unknown_items` and is **never** folded into `known` as zero.

### Cost classes (`x-bardic-cost`)
| Value | Meaning |
|---|---|
| `none` | Never contacts a provider or spends. |
| `network` | Contacts a provider or a price source for reading (voice lists, prices); spends nothing. |
| `may_charge` | Can cause premium spending, and only when the rules in section 4 allow it. |

### Pagination
`limit` (1 to 200, default 50) and `after` (opaque cursor). Pages are `{items, next}`; `next` is null on the last page. Small fixed lists return `{items}` only.

### Idempotency
`Idempotency-Key` on operations that create things (imports, plans, make-ready). A retry with the same key and body returns the first result.

### Errors
Every error body is `Error {code, detail, retryable?, retry_after_seconds?, context?}`. `code` is stable and machine-readable (an open enumeration: clients treat unknown codes by status). `detail` is safe to show. Status classes: 400 invalid, 404 not found, 409 conflict with state, 413 too large, 429 limited.

Codes used (not exhaustive):
| Area | Codes |
|---|---|
| Listeners | `listener_not_found`, `name_invalid`, `name_taken`, `last_listener` |
| Library | `book_not_found`, `book_removed`, `chapter_not_found`, `duplicate_book` (informational), `deletion_not_found`, `deletion_done` |
| Import | `import_drm_protected`, `import_unreadable`, `import_unsupported_encoding`, `import_no_text`, `import_too_large` |
| Places | `place_not_found`, `place_conflict`, `offset_out_of_range` |
| Voices | `voice_not_found`, `source_not_set_up`, `source_unreachable`, `key_rejected`, `provider_quota`, `provider_refused` |
| Plans | `plan_required`, `estimate_expired`, `estimate_changed`, `limit_below_estimate`, `limit_exceeded`, `allowance_exceeded` |
| Jobs | `job_not_found`, `job_running`, `job_not_pausable` |
| Audio | `nothing_ready` |
| General | `invalid_request`, `rate_limited`, `storage_full` |

### Routing
Literal segments win over parameters: `/api/books/duplicates` and `/api/books/sample` are not book ids.

### Audio delivery
`getAudio` supports `Range` and is immutable: the id names the exact bytes, so `Cache-Control` is immutable and the ETag is the id. Timings for read-along are a separate immutable resource (`getAudioTimings`).

## 2. Live updates
`streamEvents` is one Server-Sent Events stream. Events carry ids only (`Notice`); clients re-read the resource. Listener-scoped notices (places) carry `listener_id` so other listeners' clients ignore them. `Last-Event-ID` resumes; if the server cannot replay it sends `resync`, and clients reload what is on screen. Clients must also work without the stream (poll `getJob`, `getPlan`).

## 3. Place sync protocol
Goal: P5 and P7 (places follow the listener; the listener chooses when two disagree).

1. The server keeps per `(listener, book)` the current `Place` with a `revision` that increases on every change, and up to 10 earlier places (kept when at least 30 minutes old, or when the new place is a jump of 2% or more of the book).
2. A client keeps its last known `revision` locally. It writes with `putPlace(..., base_revision)`.
3. If `base_revision` equals the server's, the write succeeds and the response carries the new `revision`.
4. If the server's place is newer and was written by another device, the response is `409 place_conflict` with `server_place`. An identical write never conflicts.
5. The client applies the listener's `place_conflict` setting: `ask` shows both places (device, chapter, progress, time); `newest` keeps the later `updated_at`; `this_device` keeps the local one. It then writes again with the server's `revision` as `base_revision`. The place not chosen stays in history.
6. Offline, the client queues writes (coalescing to the latest) and replays them on reconnect through steps 2 to 5.
7. Finished: `finished.reason` is `marked` (by `setFinished`) or `reached_end` (progress at least 98% and unchanged for more than 24 hours; any change restarts the clock). The server evaluates it when it reads; there is no background job. A change of place clears a marked finish.

## 4. Paying for audio
Goal: P2 (nothing paid without an approved plan), PL1 to PL11.

```
previewPlan  ->  estimate_id + range + suggested_limit + allowance position + blocked?
createPlan   ->  (estimate_id, limit)  ==  the approval  ->  Plan(running) + Job
              ... chapters finish ...  ->  Plan(completed)
```

- `previewPlan` spends nothing and is valid for 15 minutes. If prices moved outside the stated range by approval time, `createPlan` returns `estimate_changed`; the client previews again.
- The plan **limit** is checked before each provider request; an item that could pass it is not started. The optional **monthly limit** (Allowance) is checked the same way. Retries and repairs spend from the same limit.
- Free voices never need a plan. For a premium audiobook, `requestChapterAudio` and `makeAudiobookReady` spend only if the chapter is covered by an approved, running plan for that audiobook; otherwise `409 plan_required`.
- Plan states: `approved`, `running`, `waiting`, `paused`, `needs_you`, `completed`, `stopped`, `failed`.
  - `waiting`: a provider quota or pacing. Resumes by itself after the reset, inside the **original limit**, with no new approval. `Plan.waiting` has the reason and `until`.
  - `needs_you`: limit or Allowance reached, key rejected, content refused, or repeated failure. `Plan.needs_you` says what is kept and what is needed. Continuing inside the limit uses `resumePlan`; raising it uses `resumePlan` with `new_limit`, which is an approval and is audited.
- `stopPlan`, `pausePlan` and `cancelJob` keep all finished chapters (P3).
- Premium voice samples are real but tiny requests; they are counted and cached per voice revision.
- Prices come from the provider's price interface where one exists (for Google, the Cloud Billing Catalog), are refreshed daily and whenever usage is fetched (`refreshPrices` does it on demand), and carry their date. Where none exists the owner sets a manual table (`putPriceTable`) and estimates are labelled `basis: manual`. Actual spend comes from provider-reported usage (for Gemini, per-request token counts); missing usage becomes `unknown_items`.

## 5. Offline
The server does not know what a device holds. A device downloads with:

1. `getAudiobookManifest` to list ready chapters with `audio.id`, size, hash, duration, and `text_sha256`.
2. `getAudio`, `getAudioTimings` and `getChapterText` for the chosen chapters.
3. Later, `checkDownloads` (send what it has) to learn which chapters were made again with a newer voice revision, with a comparison (`changes`). Nothing is replaced until the listener chooses; kept copies keep playing.

Removing a book from the library does not delete downloads; the client offers to remove them next time it connects.

## 6. Deletion levels (D5, D11)
| Level | Operations | Result |
|---|---|---|
| Remove | `removeBook`, `restoreBook` | Hidden; audio, places and downloads kept; reversible. |
| Free up space | `getAudiobookSpace`, `freeAudiobookSpace` | Deletes regenerable audio only; shows the cost to make it again. |
| Delete permanently | `scheduleBookDeletion`, `cancelBookDeletion`, `getBookDeletion` | Hidden immediately; irreversible deletion after at least 60 seconds; the schedule is stored and survives restarts. |

## 7. Traceability (spec requirement to operations)
| Spec | Operations |
|---|---|
| L1 to L7 Listeners | `listListeners`, `getListener`, `createListener`, `renameListener`, `deleteListener`, `getListenerImpact`, `getListenerSettings`, `putListenerSettings` |
| A2 to A6 Add | `findDuplicateBooks`, `createImport`, `getImport`, `cancelImport`, `createSampleBook`, `getBookCover` |
| A7 to A10 Library | `listBooks`, `listSeries`, `getBook`, `updateBook`, `refreshBookCover`, `removeBook`, `restoreBook` |
| B1 to B6 Book and audiobooks | `listAudiobooks`, `createAudiobook`, `getAudiobook`, `listAudiobookChapters`, `listChapters` |
| V1 to V10 Voices | `listVoiceSources`, `getVoiceSource`, `configureVoiceSource`, `testVoiceSource`, `refreshVoiceSource`, `removeVoiceSource`, `listVoices`, `getVoiceSample`, `putListenerSettings` (default voice) |
| S1 to S9 Listening | `getChapterText`, `getAudio`, `getAudioTimings`, `requestChapterAudio`, `searchBook`, `putPlace` |
| C1 to C7 Places | `getPlace`, `putPlace`, `setFinished`, `listPlaceHistory`, `clearPlace`, `streamEvents` |
| M1 to M8 Making audio | `makeAudiobookReady`, `requestChapterAudio`, `listJobs`, `getJob`, `pauseJob`, `resumeJob`, `cancelJob` |
| PL1 to PL11 Plans and Allowance | `previewPlan`, `createPlan`, `getPlan`, `listPlans`, `pausePlan`, `resumePlan`, `stopPlan`, `getAllowance`, `putAllowance`, `listPrices`, `refreshPrices`, `putPriceTable`, `listAudit` |
| O1 to O8 Offline | `getAudiobookManifest`, `checkDownloads`, `getAudio`, `getChapterText`, `getAudioTimings` |
| G1 to G7 Settings and recovery | `getServer`, `updateServer`, `listDevices`, `updateDevice`, `getAudiobookSpace`, `freeAudiobookSpace`, `scheduleBookDeletion`, `cancelBookDeletion`, `createExport`, `getExport`, `downloadExport`, `createBackup`, `listBackups`, `getHealth` |

## 8. Design notes and open items
- **Why place conflicts return 409 with the server's place:** it gives the client everything it needs to show the choice in one round trip, and it keeps the server free of any policy: the conflict policy is a listener setting applied by the client.
- **Why no `Audiobook.mixed`:** one voice per audiobook (D2). Adding mixed voices later means a new field, not a change.
- **Not in the contract (client-only state):** playback speed, sleep timer, reader appearance, the downloaded-chapter index, the queued place writes, the chosen listener on a device.
- **Open:** the exact set of Gemini and Breeze fields a `Voice` exposes beyond those shown; whether Breeze reports usage; the price-catalogue key for Google (see the spec, section 15); how a Gemini "success with no audio" is represented in `Detail` and `Spend`.
- **Not yet specified here:** authentication beyond the trusted network; rate limits for clients; a `DELETE` for individual plans or audit records (audit is append-only by design).
