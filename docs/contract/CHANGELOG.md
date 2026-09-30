# Contract changelog

Newest first. While the version is 0.x: additive changes bump the patch; breaking changes (a required field becoming nullable, removing or renaming anything, tightening validation) bump the minor and say what clients must change.

## 0.2.0 (BREAKING, found while implementing M0)
- `Actor.listener_id` and `Actor.listener_name` are now nullable. Renaming the server and creating the first listener happen when no listener applies, so the audit record could not be written honestly with a required listener. Clients that read `Actor` must accept null.
- `Notice.type` documents `server.updated` and `device.updated`.
- Every mutating operation now declares the `X-Bardic-Device` header (it was missing on `updateDevice`, `cancelImport`, `refreshVoiceSource`, `testVoiceSource`, `refreshPrices` and similar). Clients must send it on every POST, PUT, PATCH and DELETE.
- Every operation documents `400` (malformed header or parameter) and `500` (unexpected failure, `internal_error`). Found by the conformance harness.
- Every operation that takes `X-Bardic-Listener` documents `404` (`listener_not_found`). Found by the conformance harness in M1.
- README: error code `device_required` (400) for a mutating request without `X-Bardic-Device`.

## 0.1.0
First draft, from `docs/PRODUCT-SPEC.md`.
