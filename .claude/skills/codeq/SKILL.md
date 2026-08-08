---
name: codeq
description: >
  How to use the codeq CLI — a fast, deterministic code index — to navigate and
  understand a codebase: find a symbol's callers/callees/tests, read one definition,
  trace call chains, list a module's public API or dead code, and search (textual or
  structural). Use whenever exploring or understanding code; prefer it over grep and
  over reading whole files.
---

# Using codeq — a code index for navigating repositories

codeq is a fast, deterministic index of a codebase. **Prefer it over `grep`/`ripgrep`
and over reading whole files** when you need to navigate or understand code — it is
built for exactly that and uses far fewer tokens.

## Invoking
Run `codeq <command> [args]` from anywhere in the repo. Output is compact
`path:line:col: text` (grep-style). **Empty output means "no results"** — it's a valid
answer, not an error. Add `--json` for structured output. You never need to build or
refresh the index — every command auto-refreshes changed files first.

## What to run for what

**Orient in an unfamiliar repo**
- `codeq map` — high-level structure (languages, file/symbol counts, directories).

**Understand a file without reading all of it**
- `codeq read <file>` — the file with long function bodies folded to signatures.
- `codeq outline <file>` — just the definitions and their nesting.

**Understand or change one symbol (reach for this first)**
- `codeq explore <NAME>` — one call returns the definition + callers + callees *and* the
  verbatim source of that whole neighbourhood, budgeted and grouped by file (each source
  block tagged focus/caller/callee). Use it instead of a `definitions` → `callers` → `read`
  → `read` sequence when you need to understand or edit a function/type. `--max-tokens`
  caps the source; the definition is always kept, and anything dropped is listed.
- `codeq about <NAME>` — the same neighbourhood with **no source**, but fuller metadata
  per node (doc comment, `owner`, which callers are tests). Source is the only axis the
  two differ on: `explore` to change code, `about` for metadata or a cheap "which file
  does this live in".
- `codeq read <file> --symbol <NAME>` — that definition's source, line-numbered. Add
  `--raw` for exact bytes with no line-number prefix (to copy it verbatim).

**Who uses it / what breaks if I change it**
- `codeq callers <NAME>` — definitions that call/reference NAME (test sites flagged).
  Add `--examples` to show the actual call expression at each site (how NAME is really
  invoked, one per caller); `references <NAME> --calls` for every call site.
- `codeq references <NAME>` — every occurrence, with the enclosing symbol.
- `codeq tests <NAME>` — which tests exercise NAME *by name*. A test is recognised by
  its file path, its own name, or a test-like enclosing module/class (so Rust's
  `#[cfg(test)] mod tests` in the file under test counts). Empty means only exercised
  indirectly, e.g. an internal fn tested through a CLI/public entry point.
- `codeq affected --compare-to <ref>` — for a diff: the definitions it touched, their
  transitive callers (impacted), and the test *files* to re-run. Use after making
  changes to know what to re-check. Name-based, so it over-includes rather than misses.
- Transitive: `codeq callers <NAME> --depth 3` (up the call tree),
  `codeq callees <NAME> --depth 3` (down).

**Trace control flow**
- `codeq path <FROM> <TO>` — the shortest call chain from FROM to TO. Names resolve
  across ALL languages, so a common name like `main` may land in a Python/JS file —
  scope the start with `--path`: `codeq path main write --path src/main.rs`.

**What a symbol / file depends on**
- `codeq callees <NAME>` — what NAME calls.
- `codeq deps <file>` — internal files a file imports (`codeq deps --cycles` for cycles).
- `codeq imports [PATH]` — raw, unresolved module strings. `codeq importers <QUERY>` —
  files importing a module (a path like `src/db.rs` is reduced to its stem `db`).

**Position and bundles**
- `codeq at <file>:<line>` — the definition enclosing a position. Turns a stack frame,
  diff hunk or grep hit into a symbol.
- `codeq context --symbols a,b` — verbatim source for several symbols under one token
  budget (`--changed --compare-to <ref>` seeds from a diff instead).
- `codeq files [PATTERN]` — indexed files, with `--language`.

**API surface / cleanup**
- `codeq api <path>` — the public/exported definitions of a file or directory.
- `codeq unused [<path>]` — definitions with no references (candidate dead code; empty
  output = none found). Conservative and name-based, so it under-reports.
- `codeq implementors <TYPE>` / `codeq supertypes <TYPE>` — who implements/extends a
  trait/interface/class, and what a type derives from.

**Search**
- `codeq search <REGEX>` — content search; each match annotated with its enclosing
  symbol. `--in code|comments|strings` to scope. A superset of grep — it also has
  `--fixed`, `-i`, `-w`, `-g '*.rs' -g '!tests/'` (repeatable globs), `-l`, `-c`,
  `-B`/`-A`/`-C`, `--path`, `--language`, `--limit`. **`--limit` defaults to 100**:
  raise it (or pass 0) before concluding there are no further hits.
- `codeq astgrep '<PATTERN>' --language <lang>` — structural (AST) search, e.g.
  `codeq astgrep '$X.unwrap()' --language rust`. Scope to files with `--path <substr>`.
  Each match is annotated with the `[symbol]` it sits inside.

## Gotchas
- **Symbol lookups match by NAME across the whole repo, in every language.** For a
  common/ambiguous name (`new`, `build`, `main`), scope with `--path <substr>` —
  supported on `about`, `callers`, `callees`, `definitions`, `references`, `path`,
  `implementors`, `unused`, `astgrep`, and more. Don't know a symbol's file? Run
  `codeq about <NAME>` — it shows the definition file(s).
- **callers/references are textual (name-based), not type-resolved.** A row with a
  blank symbol is a module-level mention (e.g. an `import`/`use`), not a real call —
  ignore it. Three flags cut false positives:
  - `--calls` — only genuine call sites (`NAME(`), dropping bare mentions. Best
    for an instance method invoked on a variable (`codeq callers emit --calls`);
    if the method name has a single definition, this alone finds all its callers.
  - `--on <RECV>` — only calls written `RECV.NAME` / `RECV::NAME` / `RECV->NAME`,
    matching the *literal token* before the call. Ideal for associated/static calls
    (`codeq callers open --on Session` → `Session::open(...)`). It will NOT match an
    instance-method call on a variable — `s.emit()` where `s: Session` has token
    `s`, not `Session` — so use `--calls` there instead.
  - `--resolved` — drops occurrences whose file can't reach a definition of NAME via imports.
- **`callees` is already call-only** (a bare mention isn't a callee), so it needs no
  `--calls`. It does take `--resolved`, which keeps only callees whose definition file
  the symbol's own file can reach via imports — pruning unrelated same-named defs (a
  common `new`/`open` matching every same-named definition repo-wide).
- **Trim output tokens** with `--fields name,path,line`.
- Default output is text; use `--json` only when you need to parse it programmatically.
