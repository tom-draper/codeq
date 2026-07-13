# codeq — working in this repo

codeq is a deterministic repository index for AI agents (this repo builds it).
**Dogfood it:** when navigating this codebase, prefer codeq over raw whole-file
reads and grep, and log any friction in [DOGFOOD.md](./DOGFOOD.md).

Build with `cargo build` → run `./target/debug/codeq` from the repo root
(`--release` for benchmarking). Output is JSON unless noted.

## Prefer codeq for navigation

- **Orient:** `codeq map`
- **Understand a file cheaply:** `codeq read <file>` — folds long function bodies
  to signatures; read this instead of ingesting a whole large file.
- **Read one definition / range (verbatim, line-numbered):**
  `codeq read <file> --symbol NAME` · `codeq read <file> --lines A-B`
- **Understand a symbol before editing:** `codeq about NAME` (definition +
  callers + callees, one call)
- **Who uses it / what breaks:** `codeq callers NAME` · `codeq references NAME`
- **Search (a superset of grep):** `codeq search PATTERN [-C N] [--in code|comments|strings]`
- **Trim tokens on list output:** append `--fields a,b`

## Known limitations (found by dogfooding — don't be misled)

- **Symbol commands match by NAME across the whole repo.** `about`/`callers`/
  `callees`/`definitions` conflate all same-named symbols. For a common name
  (`new`, `build`, `open`) `about build` mixes every `build` in the repo. Run
  `codeq definitions NAME` first to see if there are several; there is not yet a
  `--path` scope for the graph commands.
- **References/callers are textual (name-based), not type-resolved.**

## Build / test

- `cargo build --release` · `cargo test` (integration tests in `tests/cli.rs`)
- Benchmarks: `bench/bench.py` (single commands), `bench/tasks.py` (task-level).
