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

**Understand one symbol before editing it**
- `codeq about <NAME>` — its definition(s) + who calls it + what it calls, in one call.
  Also the fastest way to find *which file* a symbol lives in.
- `codeq read <file> --symbol <NAME>` — that definition's source, line-numbered. Add
  `--raw` for exact bytes with no line-number prefix (to copy it verbatim).

**Who uses it / what breaks if I change it**
- `codeq callers <NAME>` — definitions that call/reference NAME (test sites flagged).
- `codeq references <NAME>` — every occurrence, with the enclosing symbol.
- `codeq tests <NAME>` — which tests exercise NAME *by name* (empty if only exercised
  indirectly, e.g. an internal fn tested through a CLI/public entry point).
- Transitive: `codeq callers <NAME> --depth 3` (up the call tree),
  `codeq callees <NAME> --depth 3` (down).

**Trace control flow**
- `codeq path <FROM> <TO>` — the shortest call chain from FROM to TO. Names resolve
  across ALL languages, so a common name like `main` may land in a Python/JS file —
  scope the start with `--path`: `codeq path main write --path src/main.rs`.

**What a symbol / file depends on**
- `codeq callees <NAME>` — what NAME calls.
- `codeq deps <file>` — internal files a file imports (`codeq deps --cycles` for cycles).

**API surface / cleanup**
- `codeq api <path>` — the public/exported definitions of a file or directory.
- `codeq unused [<path>]` — definitions with no references (candidate dead code; empty
  output = none found). Conservative and name-based, so it under-reports.
- `codeq implementors <TYPE>` / `codeq supertypes <TYPE>` — who implements/extends a
  trait/interface/class, and what a type derives from.

**Search**
- `codeq search <REGEX>` — content search; each match annotated with its enclosing
  symbol. `--in code|comments|strings` to scope. A superset of grep.
- `codeq astgrep '<PATTERN>' --language <lang>` — structural (AST) search, e.g.
  `codeq astgrep '$X.unwrap()' --language rust`. Scope to files with `--path <substr>`.

## Gotchas
- **Symbol lookups match by NAME across the whole repo, in every language.** For a
  common/ambiguous name (`new`, `build`, `main`), scope with `--path <substr>` —
  supported on `about`, `callers`, `callees`, `definitions`, `references`, `path`,
  `implementors`, `unused`, `astgrep`, and more. Don't know a symbol's file? Run
  `codeq about <NAME>` — it shows the definition file(s).
- **callers/references are textual (name-based), not type-resolved.** A row with a
  blank symbol is a module-level mention (e.g. an `import`/`use`), not a real call —
  ignore it. Three flags cut false positives:
  - `--calls` — only genuine call sites (`NAME(`), dropping bare mentions. If a
    method name has a single definition, this alone finds all its callers.
  - `--on <RECV>` — only calls written `RECV.NAME` / `RECV::NAME` / `RECV->NAME`,
    matching the *literal token* before the call. Ideal for associated/static calls
    (`codeq callers open --on Session` → `Session::open(...)`). It will NOT match an
    instance-method call on a variable — `s.emit()` where `s: Session` has token
    `s`, not `Session` — so use `--calls` there instead.
  - `--resolved` — drops occurrences whose file can't reach a definition of NAME via imports.
- **Trim output tokens** with `--fields name,path,line`.
- Default output is text; use `--json` only when you need to parse it programmatically.
