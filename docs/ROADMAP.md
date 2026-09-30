# Server roadmap

Milestones end with exit criteria that can be checked against the spec's acceptance tests. Operation ids are from `docs/contract/openapi.yaml`.

| # | Milestone | Scope | Exit criteria |
|---|---|---|---|
| M0 (done) | Skeleton and harness | Workspace, config, single-instance lock, migrations, conformance harness, `getHealth`, `getServer`, devices, audit log, event stream skeleton. | Harness fails on an undeclared field and on an undocumented status. A second instance refuses to start. |
| M1 (done) | Listeners, library, import | Listeners and settings; imports (EPUB, text), duplicate check, sample book, series, chapters, text, cover and colour sample, search, remove and restore. | P1 tested byte for byte. A1 to A10 and L1 to L7 acceptance tests pass with original fixtures. |
| M2 | Places | `putPlace` with revisions and conflicts, history, finished (marked and automatic), place notices. | P5 and P7, C1 to C7, D1 and C6 tests pass, including the fake clock boundary. |
| M3 | Free audio | Voice sources `breeze` and `local`; audiobooks; jobs, pacing, recovery; audio delivery with ranges; timings; `requestChapterAudio`, `makeAudiobookReady`. | P3 crash test passes. First audio and resume targets met on the reference machine. |
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
- `listBooks` filters `in_progress` and `finished` return nothing and `recent` means newest added, until places arrive in M2.
- `Book.place` is null and `audiobook_count` is 0 until M2 and M3.
- CORS: `--allow-origin` (repeatable, or `BARDIC_ALLOW_ORIGINS`). Browser writes from any other origin get 403 `origin_not_allowed`; requests with no `Origin` (curl, scripts) are unaffected.
- The import runs as a background task with stages `reading`, `finding_chapters`, `preparing_text`, `done`; cancellation is honoured between stages.
- Known limits: search scans chapter text (fine for a household library); `listBooks` uses offset cursors.
