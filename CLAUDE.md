# codeq — working in this repo

codeq is a deterministic repository index for AI agents (this repo builds it).
**Dogfood it:** when navigating this codebase, prefer codeq over raw whole-file
reads and grep, and log any friction in [DOGFOOD.md](./DOGFOOD.md).

Build with `cargo build` → run `./target/debug/codeq` from the repo root
(`--release` for benchmarking). Output defaults to compact grep-style text
(`path:line:col: payload`); pass `--json` for the `codeq.result.v1` envelope
(what the MCP server and programmatic consumers use).

**Never run `codeq index` for correctness — every query auto-refreshes first.**
Each command opens the index and reruns the git-free freshness check (changed/
added/removed files get reparsed, schema-version bumps trigger a rebuild) *before*
answering, so a query always reflects the current tree — even right after an edit.
The refresh count is only shown under `--json`/`--meta` (the `index` block's
`reindexed_files`), which is why it *looks* like nothing happened. `index` exists
only to force a full `--rebuild` or to pre-warm the cache for timing-sensitive
benchmarks — not as a step you run before querying.

## Prefer codeq for navigation

- **Understand or change one symbol (start here):** `codeq explore NAME` — one call
  returns its definition + callers + callees *and* the verbatim source of that whole
  neighbourhood, budgeted and grouped by file (each source block tagged focus/caller/
  callee). Replaces the `about` → `read` → `read` sequence; `--max-tokens` caps it (the
  definition is always kept, the rest fill the budget, drops are listed under `omitted`).
- **Orient:** `codeq map`
- **Docs are indexed too.** A markdown heading is a definition, so `outline
  README.md` is a table of contents, `read README.md` folds to that ToC with each
  section's line range, `read README.md --symbol "Output contract"` returns just
  that section (read this instead of the whole file), and `at README.md:230` says
  which section a line is in. Search hits carry their `[Section]`. `--in strings`
  searches fenced examples, `--in comments` searches prose (`--in code` is
  correctly empty: markdown has no code). Local links and images are file
  references, so `deps`/`importers` work across docs and assets.
- **Understand a file cheaply:** `codeq read <file>` — folds long function bodies
  to signatures; read this instead of ingesting a whole large file.
- **Read one definition / range (verbatim, line-numbered):**
  `codeq read <file> --symbol NAME` · `codeq read <file> --lines A-B`
- **Grab a symbol's source to edit it:** add `--raw` — exact bytes, no
  line-number prefixes, no folding, addressed by name. Its output drops straight
  into the Edit tool's `old_string`, so you skip the grep-for-the-line →
  read-the-region → strip-prefixes dance. With `--symbol` it requires a single
  match (a clean anchor); scope with `--lines A-B` if a name is ambiguous.
- **Understand a symbol before editing:** `codeq about NAME` (definition +
  callers + callees + members, one call). Definition rows carry the symbol's own
  doc comment as `  // <doc>`, so `about`/`definitions`/`explore` answer "what
  does this do" without opening the file; `outline --docs` adds them per file.
- **What a type contains:** `codeq members NAME` — its fields, enum variants and
  methods, in source order (`--kind field|variant|function` to narrow). Read this
  instead of opening the file to see a struct's shape. Unlike the other graph
  commands it is **structural, not name-based**: a member is a definition nested
  directly inside NAME's byte range, so there are no same-name false positives.
  `about NAME` already includes it, so a struct's dossier is one call. Limits:
  in Rust a struct's methods live in a separate `impl` block, so only fields come
  back; Ruby (`attr_accessor`) and Python instance attributes (`self.x = …` in
  `__init__`) aren't declarations, so they yield nothing rather than a guess.
- **Learn a module's public API (how to call it):** `codeq api <path>` — only the
  *exported* definitions, with signatures, instead of reading whole files. "Public"
  is each language's own notion (Rust `pub`, JS/TS `export`, Go capitalization,
  Python non-underscore, a `public` modifier, non-`static` in C/C++); `pub(crate)`
  and `protected` are `restricted`, not public. `--kind function` to narrow.
- **Who uses it / what breaks:** `codeq callers NAME` · `codeq references NAME`
  - **Cut false positives.** Three opt-in, composable filters on `callers`/
    `references` (the default stays a safe textual superset):
    - `--resolved` — import-scoped: drops occurrences whose file can't reach a
      definition of NAME through imports; tags the rest `via` same-file /
      imported / ambiguous. Heuristic (best-effort import resolution — a
      stem-matched edge, so Python `from . import x` relative / `import *`
      wildcard forms stay coarse). Rust `mod x;` and named imports (Python `from
      pkg import x` → `pkg.x`, JS/TS `import {a} from './x'` → `./x.a`) all count
      as edges, so intra-crate, package-submodule, and named-binding reachability
      work.
    - `--calls` — keep only call sites. Answered from the **parse tree**, not the
      surrounding bytes, so a declaration (`fn NAME(`) and a mention in a string or
      comment are all excluded, and formatting never matters.
    - `--on RECV` — keep only calls on that receiver, matched against the
      **receiver expression as written**. So `--on db` gets `db.query()` however
      it is wrapped, and *not* `mydb.query()` or `a.inner.query()`; for those use
      `--on a.inner`, and a call-valued receiver works too (`--on 'conn()'`).
      Receivers are as the language writes them — PHP needs `--on '$db'`.
    - `--examples` (callers) — surface the actual call expression at each site (one
      representative per caller): the row becomes `path:line:col:` the source line,
      so one command answers "who calls it *and how*". For every call site rather
      than one per caller, use `references NAME --calls`. Composes with the filters.
  - **`callees` is already call-position only** (a bare mention of a name isn't a
    callee, and `helper()` written inside a string or comment isn't either), so it needs no `--calls`; it does take `--resolved`, which keeps only
    callees whose definition file NAME's *own* file can reach through imports (tags
    each `via` same-file / imported) — pruning the name-based over-match where a
    common callee name (`new`, `open`) matches unrelated same-named defs in
    unreachable files.
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
- **What a diff affects:** `codeq affected --compare-to <ref>` — the definitions the
  diff touched, their transitive callers (blast radius, `--depth N`), and the test
  *files* to re-run. Composes `changed --symbols` + a caller BFS + `tests`; name-based,
  so it over-includes rather than misses.
- **Find dead code:** `codeq unused [<path>]` — definitions with no reference
  anywhere outside their own definition site. A mention in a **comment** is not a
  use (a stale TODO can't mask dead code), but a mention in a **string** still is,
  because reflection, serde renames, config keys and SQL identifiers are real uses
  that live in string literals. Still name-based, so a same-named symbol elsewhere
  masks it; it under-reports rather than mislead. Shows `visibility` (a `public` hit may
  be external API, not dead); test-file defs excluded unless `--include-tests`.
- **Search (a superset of grep):** `codeq search PATTERN [-C N] [--in code|comments|strings]`
  - `--in strings` (not `--in code`) to find string-embedded content — SQL, config,
    templates. codeq's own SQL lives in string literals, so `--in code` finds none.
  - **Piped input:** `CMD | codeq search PAT --stdin` scans the pipe instead of the
    index, so codeq replaces grep in a pipeline too (no repo or index needed). Add
    `--language rust` to also get `--in` and the enclosing `[symbol]` on each match.
    `--stdin` is required — it is never inferred, so an open pipe can't hijack a
    normal query.
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
  to file stems); named imports are captured qualified — Python `from pkg import
  x` → `pkg.x`, JS/TS `import {a} from './x'` → `./x.a` — so the imported symbol
  resolves like a direct module import; relative (`from . import x`) and wildcard
  forms stay coarse. Verify via the edge's module string.
- **Trim tokens on list output:** append `--fields a,b`

## When a query comes back empty

codeq tells you why, on stderr (or in the `note` field under `--json`):

- `no symbol named 'ScanOpt'; closest: ScanOpts` — a typo, with the fix.
- `'X' is defined at a.rs:2 but has no callers` — the symbol is real, the
  relation is genuinely empty.
- `'X' is defined at a.rs:2 but nothing matches --path zzz` — your filter, not
  the code.
- `no public definitions in 'src/search.rs' (34 private, 40 restricted)` — the
  defs exist, none are public (`pub(crate)` is restricted, not public).
- `no unused definitions ...` — a clean result, not a failed one.

So read stderr before assuming a command doesn't work or a symbol doesn't exist.

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
