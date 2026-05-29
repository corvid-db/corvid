# Working instructions

Read `DESIGN.md` before any architectural change. It holds the what and why; this file holds only how to work.

## Never

- No SQL: no parser, no query strings, no string interpolation into queries. The fluent builder is the only entrypoint.
- No networking, no replication, no server/wire protocol, no multi-node, no cloud sync — **in the engine crate**. The MCP sidecar is a *separate* crate that embeds the engine and speaks MCP over stdio; networking-shaped code lives only there and never leaks into the engine.
- No tensor/model code. Users bring their own (candle/burn).
- No backward-compat or migration-shim code. Break the file format freely until v1.0 is declared; old files are skipped with a warning and require manual reimport.
- No "eventually consistent" or async-rebuilt index that the public API can observe. The builder's cross-modal consistency guarantee is non-negotiable.
- Do not wrap a library's own on-disk persistence for vector/FTS/graph state. Use the algorithm; store state as redb entries.

## Architecture constraints

- Every swappable component sits behind a trait (storage backend, JSON parser, index types). Wrapping is scaffolding — keep the seam clean so the wrapped part can be replaced without touching callers.
- The data model and the fluent builder API are the parts that can't be cheaply migrated. Get them right; treat everything below them as replaceable.
- The public API promises its final semantics from day one even when the implementation behind it is a v0.1 shortcut. Never let a shortcut leak into the contract.
- Performance is the last priority, not the first. Match incumbents within a small factor and move on. Do not micro-optimize a replaceable component before profiling proves it's a hot path.

## Making changes

- A decision determined by the project's goal: make it and state the reasoning. Only ask when the goal genuinely doesn't determine the answer.
- Record every non-obvious decision in the decision log in `DESIGN.md` (date, decision, one-line rationale). When the reasoning is long, add a note under `decisions/`.
- When you change the file format or data model, write the one-shot dump/load migration in the same change.

## Rust conventions

- Sync public API. Async, if offered, is a shallow wrapper — never the core.
- Errors: typed per layer via `thiserror`. Never panic on user input; panics are for violated internal invariants only.
- SIMD: portable `std::simd` by default; intrinsic-specialized paths behind `cfg(target_feature)`. Remember WASM has only SIMD128 — no AVX-512/NEON.
- Hot paths allocate from a query-scoped arena where possible; aim for zero allocation during execution.

## Build & test

- `cargo test` must pass before any change is considered done. Run it; report failures with output.
- **Commit every meaningful step on `master`** — small, self-contained commits so progress is never lost. Each commit builds and its tests pass.
- **Coverage ≥ 90% line coverage**, measured with `cargo llvm-cov`. A change that drops coverage below the bar is not done.
- **Meaningful tests only.** Cover real behavior and every error path; do not write assertions that exist only to inflate the coverage number. Over-test where correctness is subtle (storage, transactions, the invariant, query semantics); don't pad trivial getters.
- Performance-sensitive changes carry a `criterion` benchmark; regressions block the change.
- WASM build target: bundle under 2 MB gzipped. Native/mobile binary: keep lean (mobile target under ~5 MB stripped).
