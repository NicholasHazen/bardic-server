# Server roadmap

Milestones end with exit criteria that can be checked against the spec's acceptance tests. Operation ids are from `docs/contract/openapi.yaml`.

| # | Milestone | Scope | Exit criteria |
|---|---|---|---|
| M0 | Skeleton and harness | Workspace, config, single-instance lock, migrations, conformance harness, `getHealth`, `getServer`, devices, audit log, event stream skeleton. | Harness fails on an undeclared field and on an undocumented status. A second instance refuses to start. |
| M1 | Listeners, library, import | Listeners and settings; imports (EPUB, text), duplicate check, sample book, series, chapters, text, cover and colour sample, search, remove and restore. | P1 tested byte for byte. A1 to A10 and L1 to L7 acceptance tests pass with original fixtures. |
| M2 | Places | `putPlace` with revisions and conflicts, history, finished (marked and automatic), place notices. | P5 and P7, C1 to C7, D1 and C6 tests pass, including the fake clock boundary. |
| M3 | Free audio | Voice sources `breeze` and `local`; audiobooks; jobs, pacing, recovery; audio delivery with ranges; timings; `requestChapterAudio`, `makeAudiobookReady`. | P3 crash test passes. First audio and resume targets met on the reference machine. |
| M4 | Premium audio and money | `gemini` source; prices; `previewPlan`, `createPlan`, plan state machine, Allowance, spend with unknowns, quotas as `waiting`. | P2 and P6, D9 and PL1 to PL11 tests pass against a fake provider; one bounded, authorised live check. |
| M5 | Offline, space, deletion, export | Manifest, `checkDownloads`, newer audio, free up space, scheduled deletion with undo, M4B export, backup. | O1 to O8 server-side tests, D6 and G2 to G4 tests pass. |
| M6 | Hardening | Content refusals, provider edge cases, restart and quota soak, security review, performance targets at 500 books. | Spec section 10 targets met; open items in spec section 15 closed or scheduled. |

Out of scope until the spec changes: characters, casting, performances, voice design and cloning, roles and passwords, notifications.
