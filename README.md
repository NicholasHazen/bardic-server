# Bardic server

The server half of Bardic v2: it turns books you own into audiobooks on your own computer. It stores books and text, talks to voice sources (Breeze, Gemini, voices on this computer), makes and keeps audio, runs plans with limits, and keeps each listener's place. Clients (see the `bardic-web` repository) talk to it over the HTTP contract in this repository.

**Status: pre-implementation.** The product spec, the API contract and the architecture proposal are written; the code is a skeleton. Nothing here has been built against the contract yet.

## Start here

| Read | For |
|---|---|
| [docs/PRODUCT-SPEC.md](docs/PRODUCT-SPEC.md) | What Bardic does, the promises it makes, requirements with ids, acceptance tests. |
| [docs/contract/openapi.yaml](docs/contract/openapi.yaml) and [README](docs/contract/README.md) | The normative HTTP contract (OpenAPI 3.1) and its conventions. **The contract lives in this repository**; the web client keeps a synced copy. |
| [docs/ARCHITECTURE.md](docs/ARCHITECTURE.md) | Proposed structure, stack, invariants and risks. |
| [docs/ROADMAP.md](docs/ROADMAP.md) | Milestones with exit criteria. |
| [docs/design/BOARDS.md](docs/design/BOARDS.md) | The screens the contract serves. Images live in the web repository. |
| [AGENTS.md](AGENTS.md) | Rules for coding agents and contributors. |
| [docs/history/README.md](docs/history/README.md) | Where the prototype and earlier design notes live. |

## Layout

```
crates/bardic-server/   the binary (skeleton)
docs/                   spec, contract, architecture, roadmap
```

## Run the skeleton

```sh
cargo run -p bardic-server
```

## Principles in one breath

Text is never changed. Nothing paid starts without a plan you approved. Finished audio is always kept. Unknown cost is shown as unknown. A listener's place follows them, and they choose when two places disagree.
