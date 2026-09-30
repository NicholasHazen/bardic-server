# Bardic server

The server half of Bardic v2: it turns books you own into audiobooks on your own computer. It stores books and text, talks to voice sources (Breeze, Gemini, voices on this computer), makes and keeps audio, runs plans with limits, and keeps each listener's place. Clients (see the `bardic-web` repository) talk to it over the HTTP contract in this repository.

**Status: M0, M1 and M2 done.** Implemented and tested against the contract: server, devices, audit, events, listeners and settings, the library (import EPUB and text, duplicates, sample book, series, chapters and text, cover and colour sample, search, edit, remove and restore) and places (revisions, conflicts, history, finished). Audio, voices, plans and offline are still to do (see the roadmap). Try it with [docs/CURL.md](docs/CURL.md).

## Start here

| Read | For |
|---|---|
| [docs/PRODUCT-SPEC.md](docs/PRODUCT-SPEC.md) | What Bardic does, the promises it makes, requirements with ids, acceptance tests. |
| [docs/contract/openapi.yaml](docs/contract/openapi.yaml) and [README](docs/contract/README.md) | The normative HTTP contract (OpenAPI 3.1) and its conventions. **The contract lives in this repository**; the web client keeps a synced copy. |
| [docs/ARCHITECTURE.md](docs/ARCHITECTURE.md) | Proposed structure, stack, invariants and risks. |
| [docs/ROADMAP.md](docs/ROADMAP.md) | Milestones with exit criteria. |
| [docs/TEXT.md](docs/TEXT.md) | How book text becomes chapters and lines; offsets are code points. |
| [docs/CURL.md](docs/CURL.md) | Exercise the running server with curl. |
| [docs/contract/CHANGELOG.md](docs/contract/CHANGELOG.md) | What changed in the contract and why. |
| [docs/design/BOARDS.md](docs/design/BOARDS.md) | The screens the contract serves. Images live in the web repository. |
| [AGENTS.md](AGENTS.md) | Rules for coding agents and contributors. |
| [docs/history/README.md](docs/history/README.md) | Where the prototype and earlier design notes live. |

## Layout

```
crates/bardic-server/   the server (library + binary), migrations, tests
docs/                   spec, contract, architecture, roadmap
```

## Run, test, lint

```sh
cargo run -p bardic-server -- --data-dir ./data     # http://127.0.0.1:8765
#   --bind 0.0.0.0:8765          listen on the network (trusted networks only)
#   --allow-origin http://localhost:5173   let a dev web page call the API
cargo test                                          # includes contract conformance
cargo clippy --all-targets -- -D warnings && cargo fmt --check
```

## Principles in one breath

Text is never changed. Nothing paid starts without a plan you approved. Finished audio is always kept. Unknown cost is shown as unknown. A listener's place follows them, and they choose when two places disagree.
