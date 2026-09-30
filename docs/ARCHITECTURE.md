# Server architecture (proposal)

Status: proposal for review. It turns the product spec into a structure; none of it is built. Choices marked **decide** need an owner decision before the milestone that depends on them.

## 1. Shape

One process, one data folder, one writer.

```
clients (web) ──HTTP/SSE──▶ api ──▶ core rules ──▶ store (SQLite + audio files)
                                     │
                                     ├──▶ jobs (queue, pacing, recovery)
                                     │        └──▶ voice sources ──▶ Breeze / Gemini / local
                                     └──▶ prices (daily + on usage)
```

Suggested crates in a Cargo workspace (start as modules in one crate and split when a boundary proves useful):

| Crate | Owns |
|---|---|
| `bardic-core` | Domain types and rules, no I/O: places and history, the finished clock, plan state machine, cost ranges, money, scope. Takes an injectable clock. |
| `bardic-store` | SQLite schema and migrations, the content-addressed audio store, atomic writes. |
| `bardic-import` | EPUB and text to book: safe unzip, chapter and line detection, cover and colour sample, file fingerprint. |
| `bardic-voices` | The `VoiceSource` trait and adapters (`breeze`, `gemini`, `local`), chunking, pacing, usage and refusal mapping. |
| `bardic-jobs` | Persistent queue, scheduling, per-provider pacing and quotas, restart recovery, plan gating. |
| `bardic-prices` | Price interfaces per provider, manual tables, daily refresh. |
| `bardic-api` | HTTP routes, headers (`X-Bardic-Listener`, `X-Bardic-Device`), error mapping, Server-Sent Events. |
| `bardic-server` | The binary: configuration, wiring, startup checks (single instance, migrations). |

## 2. Stack (decide)

| Concern | Proposal | Why / alternatives |
|---|---|---|
| Runtime and HTTP | `tokio` + `axum` | Mature, good streaming and range support. |
| Storage | SQLite in WAL mode, `sqlx` with migrations | One file to back up, strong transactions. `rusqlite` behind a writer thread is the alternative. |
| Serialization | `serde` | |
| Contract conformance | Validate every test response against `openapi.yaml` with a JSON Schema validator; generate serde models from the schemas (for example `typify`) and review drift | Rust has no mature contract-first server generator, so the contract stays normative and is enforced by tests. |
| Audio encoding | **decide**: AAC in MP4 (plays everywhere, including iOS) versus Opus | Providers return PCM. Needs an encoder: a bundled library or an external `ffmpeg`. |
| EPUB | `zip` + an XML parser, own safety limits | Bound expanded size and file count. |
| Logging | `tracing`, structured, secrets redacted | |

## 3. Data model (sketch)

- **Identifiers** are opaque, stable and sortable (ULID is a good fit). Chapters and lines keep their ids for the life of a book's text.
- **Text** is stored once per chapter with `text_sha256`; lines are `(id, start, end)` code point spans into it.
- **Audio** is a file named by the hash of what it was made from (chapter text hash, voice revision, settings) and never modified. An audiobook chapter points at an audio id. Making a chapter again creates a new file; the old one stays until *free up space*.
- **Places** are one current row per `(listener, book)` with a `revision`, plus a bounded history table (10). The finished state is computed on read from `progress` and `updated_at`.
- **Plans, jobs, spend** are append-friendly: spend entries are immutable rows (`known` micros or `unknown`), and plan totals are derived. Audit is append-only.
- **Schema versions** and a backup point precede every migration.

## 4. Invariants the structure must protect

1. **P1 text unchanged.** No code path writes chapter text after import.
2. **P2 plan gate.** One function decides whether a premium request may run (plan approved and running, inside its limit and the monthly limit, key valid, scope covers the chapter). It is the only caller of premium `synthesize`. A test enumerates callers.
3. **P3 durability.** Write audio to a temporary file, flush, rename into place, then record it, then mark the chapter ready. Startup repairs any chapter that claims ready without a file.
4. **Short transactions.** Never hold a transaction or lock across a provider call.
5. **Restart safety.** Jobs and plans are persisted state machines; on start, running work becomes `waiting` or `queued` and resumes without re-approval, inside the original limit.
6. **Money is integers.** Micros everywhere; no floats.
7. **Unknown stays unknown.** A request with no reported usage is stored as unknown and counted.

## 5. Voice sources

A `VoiceSource` provides: `list_voices`, `sample`, `synthesize(chunk) -> audio + usage + limits`, and `check`. Adapters map provider behaviour to the contract: rate limits and daily quotas to `waiting`, refusals to `provider_refused`, a rejected key to `key_rejected`, transport failure to `source_unreachable`.

- **Gemini:** chunk text to provider limits, pace by per-minute and daily limits, read token usage from each response, treat "success with no audio" as a failed item with recorded usage (**decide** how it is counted).
- **Breeze:** user-run server; base URL in settings; voices are whatever it reports; free. Usage reporting unknown.
- **Local:** voices already on the computer, optional.

## 6. Jobs and pacing

A single scheduler owns the queue. Priorities: chapter requested by a listener now, then chapters ahead of a listener's place, then background make-ready. One job per audiobook chapter at a time; a second request joins it. Per-provider concurrency and rate pacing are configuration. Progress is emitted as change notices on the event stream.

## 7. Prices

Read from the provider's price interface where one exists (for Google, the Cloud Billing Catalog, which may need a key different from the Gemini key); otherwise a manual table. Refresh daily and whenever usage is fetched; keep the last good price and show its age on failure.

## 8. Security

Bind to loopback by default; binding to the network is an explicit setting. There are no passwords in this version; the protections that remain are the origin check on browser writes, write-only keys stored with file permissions or the OS keychain, and the audit log. Do not expose the server to the public internet.

## 9. Testing

| Kind | What |
|---|---|
| Unit | Core rules with a fake clock: finished after 24 h, place history rules, plan state machine, cost range arithmetic. |
| Conformance | Every operation returns its 2xx and validates against `openapi.yaml`; undeclared fields and undocumented statuses fail. |
| Integration | A fake voice source with programmable delays, quotas, refusals and missing usage. |
| Crash | Kill the process mid-job; assert P3 and restart behaviour. |
| Property | Place history and sync revisions under reordered, duplicated and stale writes. |
| Soak | A few hundred books; library and search latency targets from the spec. |

## 10. Risks
1. Audio encoding and codec support across browsers, especially iOS.
2. Latency: how fast each source makes a first chapter.
3. Provider behaviour changes (quotas, usage fields, refusals).
4. Price interfaces missing or keyed differently; manual tables drift.
5. No authentication on a shared network.
