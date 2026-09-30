# Contract changelog

Newest first. While the version is 0.x: additive changes bump the patch; breaking changes (a required field becoming nullable, removing or renaming anything, tightening validation) bump the minor and say what clients must change.

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
