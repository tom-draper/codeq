# codeq — working in this repo

codeq is a deterministic repository index for AI agents (this repo builds it).
**Dogfood it:** when navigating this codebase, prefer codeq over raw whole-file
reads and grep, and log any friction in [DOGFOOD.md](./DOGFOOD.md).

Build with `cargo build` → run `./target/debug/codeq` from the repo root
(`--release` for benchmarking). Output defaults to compact grep-style text
(`path:line:col: payload`); pass `--json` for the `codeq.result.v1` envelope
(what the MCP server and programmatic consumers use).

## Prefer codeq for navigation

- **Orient:** `codeq map`
- **Understand a file cheaply:** `codeq read <file>` — folds long function bodies
  to signatures; read this instead of ingesting a whole large file.
- **Read one definition / range (verbatim, line-numbered):**
  `codeq read <file> --symbol NAME` · `codeq read <file> --lines A-B`
- **Understand a symbol before editing:** `codeq about NAME` (definition +
  callers + callees, one call)
- **Learn a module's public API (how to call it):** `codeq api <path>` — only the
  *exported* definitions, with signatures, instead of reading whole files. "Public"
  is each language's own notion (Rust `pub`, JS/TS `export`, Go capitalization,
  Python non-underscore, a `public` modifier, non-`static` in C/C++); `pub(crate)`
  and `protected` are `restricted`, not public. `--kind function` to narrow.
- **Who uses it / what breaks:** `codeq callers NAME` · `codeq references NAME`
  - **Cut false positives (import-scoped):** add `--resolved` to `callers`/
    `references` — drops occurrences whose file can't reach a definition of NAME
    through imports, and tags the rest `via` same-file / imported / ambiguous.
    Opt-in and heuristic (import resolution is best-effort — a stem-matched edge,
    so e.g. Python `from . import x` relative/`import *` wildcard forms stay
    coarse), so it can over-prune; the default stays a safe textual superset. Rust
    `mod x;` declarations and Python `from pkg import x` (captured as `pkg.x`) both
    count as edges, so intra-crate and package-submodule reachability work.
  - **Transitive call tree:** `codeq callers NAME --depth N` (up) ·
    `codeq callees NAME --depth N` (down) — BFS to N hops, each result tagged
    with its `depth`; text output indents by hop. For blast-radius / reachability.
  - **Call chain between two symbols:** `codeq path FROM TO` — the shortest chain
    where FROM calls … calls TO, shown step by step (empty if unreachable within
    `--max-depth`, default 8). Reachability *with the route*, vs `callees --depth`
    which only says whether/at what depth.
- **Who implements / extends a type:** `codeq implementors NAME` — the types that
  `impl T for X` (Rust), `extends`/`implements` (Java/TS/PHP/C#/C++), subclass
  (`class X(Base)` Python, `class X < Y` Ruby). The inheritance edges the call
  graph misses; `--relation implements|extends` to filter. Go (structural) / C
  have none; name-based like the other graph commands.
  - **The reverse — what a type derives from:** `codeq supertypes NAME` — the
    traits / interfaces / base classes NAME extends or implements (as written at
    the declaration, not transitively resolved).
- **Which tests exercise it:** `codeq tests NAME` — the test functions that
  reference NAME (by test-file path or test-like name). For the edit→find-the-test loop.
- **Find dead code:** `codeq unused [<path>]` — definitions with no reference
  anywhere outside their own definition site. Conservative (a comment/string
  mention counts as a use; name-based, so a same-named symbol elsewhere masks it)
  so it under-reports rather than mislead. Shows `visibility` (a `public` hit may
  be external API, not dead); test-file defs excluded unless `--include-tests`.
- **Search (a superset of grep):** `codeq search PATTERN [-C N] [--in code|comments|strings]`
  - `--in strings` (not `--in code`) to find string-embedded content — SQL, config,
    templates. codeq's own SQL lives in string literals, so `--in code` finds none.
- **Structural (AST) search:** `codeq astgrep '$X.unwrap()' --language rust` — match
  syntax, not text. `$VAR`/`$_` = one node (repeated `$VAR` binds the same);
  `$$$` = any run of siblings, so `foo($$$)` matches any arg count. For "all calls
  to X", "all `Ok($X)` returns", etc. Each match is annotated with its enclosing
  `[symbol]`. Limits: it's structurally exact (include optional syntax the code
  has) and can't see inside macro invocations.
- **Module-dependency graph:** `codeq deps <file> [--depth N]` — the internal
  files a file depends on, resolved from its imports (each edge shows the raw
  import string); `--depth N` walks transitively. `codeq deps --cycles` lists
  every import cycle in the repo. Resolution is heuristic (import segments matched
  to file stems); Python `from pkg import x` is captured as `pkg.x` so it resolves
  like `import pkg.x`, but relative (`from . import x`) and wildcard forms stay
  coarse — verify via the edge's module string.
- **Trim tokens on list output:** append `--fields a,b`

## Known limitations (found by dogfooding — don't be misled)

- **Symbol commands match by NAME across the whole repo.** For a common name
  (`new`, `build`, `open`), scope with `--path <substr>`:
  `codeq about build --path context` targets the `build` in that file. Without a
  scope, `definitions`/`callees`/`about` cover every same-named symbol. (`about`
  keeps callers repo-wide even when scoped.)
- **References/callers are textual (name-based), not type-resolved.** `--resolved`
  narrows them using the import graph (see above), but it's a heuristic filter,
  not real type resolution.

## Build / test

- `cargo build --release` · `cargo test` (integration tests in `tests/cli.rs`)
- Benchmarks: `bench/bench.py` (single commands), `bench/tasks.py` (task-level).
