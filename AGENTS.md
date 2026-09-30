# Working on the Bardic server

Entry point for coding agents. User instructions for the current task take precedence over this file.

## Read before changing code
1. `docs/PRODUCT-SPEC.md`: promises P1 to P7, requirement ids, acceptance tests.
2. `docs/contract/README.md` and `openapi.yaml`: the normative API. Change the contract **first** (same commit as the behaviour), bump its version, and keep operation ids permanent.
3. `docs/ARCHITECTURE.md` for structure and invariants; `docs/ROADMAP.md` for what is in scope now.

## Invariants (break one and a promise breaks)
- **Text is immutable.** Stored once; offsets are zero-based Unicode code points, end exclusive. Never bytes or UTF-16 units.
- **Nothing paid starts without an approved plan.** Every premium request goes through the plan gate: plan approved, running, inside its limit and the optional monthly limit, key valid. Retries and repairs spend from the same limit. Samples are the only exception and are counted.
- **Finished audio is kept.** Audio is immutable and content-addressed; a failure, stop, limit or restart never deletes it. A chapter is `ready` only after its file is durable.
- **Unknown is unknown.** Money is integer micros. Missing usage becomes an `unknown_items` count, never zero.
- **Places are a projection with history.** Revisions increase on every change; conflicts return 409 with the server's place; the client chooses.
- **Short transactions.** Never hold a database transaction or lock across a provider request. One writer per data folder; refuse a second instance.
- **Secrets stay secret.** Keys are write-only over the API, never logged, never returned.
- **Audit.** Every change to a listener, plan, Allowance, key, source, price, deletion or export records listener and device.

## Cost and work boundaries
- Development is offline by default: tests use fake voice sources and mocked provider responses. Do not make a paid request without the user's explicit authorisation for that scope; a configured key is not authorisation.
- Use original synthetic text in fixtures. Never use a user's books, audio or keys as fixtures, and never commit them.
- Do not add a licence, CI service or paid dependency as part of an unrelated task.

## Testing
- Core rules (finished clock, place history, plan state machine, cost ranges) take an injectable clock and are unit tested.
- Every operation has a conformance test: it receives its 2xx, and the response is validated against `openapi.yaml`. Undeclared fields and undocumented statuses fail the test; fix the code or the contract, never the check.
- Crash tests kill the process mid-job and assert P3.

## Workflow
Inspect `git status`, keep changes scoped, update the spec or contract when behaviour changes, run `cargo fmt`, `cargo clippy` and `cargo test`, and report what was actually verified (mock versus live).
