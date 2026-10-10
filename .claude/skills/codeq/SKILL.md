---
name: codeq
description: >
  How to use the codeq CLI — a fast, deterministic code index — to navigate and
  understand a codebase: find a symbol's callers/callees/tests, read one definition,
  trace call chains, list a module's public API or dead code, and search (textual or
  structural). Use whenever exploring or understanding code; always prefer it over
  reading a whole file. For flat text lookup and exact edit anchors, grep is fine.
---

# Using codeq — a code index for navigating repositories

codeq is a fast, deterministic index of a codebase. **Never read a whole source file —
use `api`/`read`/`explore` instead.** That is where the savings are, and they are large.

**When grep is fine:** flat text lookup that returns a set of lines, and finding an
exact anchor before an `Edit`. Measured on a Python repo 2026-08-16: against a full read
of a 1,636-line file, `codeq read` cut output 77% and `codeq api` cut it 86% — but on
lookups returning the *identical* lines, codeq ran 17–50% larger, because `path:line:col:`
prefixes and `[symbol]` suffixes cost more than they save. Reaching for codeq there is a
net loss. (Numbers are from one repo with long files; the direction holds, the magnitude
shrinks on many-small-files codebases.)

Still reach for `codeq search` over grep when *which definition contains this hit* is
part of the question — the `[symbol]` annotation is something grep cannot produce.

## Invoking
Run `codeq <command> [args]` from anywhere in the repo. Output is compact
`path:line:col: text` (grep-style). **Empty output means "no results"** — it's a valid
answer, not an error. Add `--json` for structured output. You never need to build or
refresh the index — every command auto-refreshes changed files first.

## What to run for what

**Orient in an unfamiliar repo**
- `codeq map` — high-level structure (languages, file/symbol counts, top-level directories).
  `--totals` for just the headline numbers and the language table: it skips the sections
  that scan rows, so it stays fast on a big repo.
- `codeq graph [--relation references|implements|extends]` — the whole type-relation
  graph as `from -> to` edges (`--mermaid` to render it). Relations only, not calls:
  relation edges come off the parse tree so the graph is exact, while a name-based
  call graph compounds its over-match at every hop — use `callers/callees --depth N`
  there instead. TypeScript/TSX contributes explicit `extends`/`implements` edges;
  Go does not, because its structural interfaces need type checking to determine
  implementations.
- `codeq tree [<path>]` — the directory shape with nesting kept, folded at `--depth`
  (default 3); each directory reports the files and symbols of everything below it.
  `--min-symbols 1` jumps straight to where the code is. In a repo full of data,
  prefer `--language python` (markdown headings are definitions, so per-directory
  READMEs can outrank the source). Ignore-aware, so no node_modules/.git noise.

**Understand a file without reading all of it**
- `codeq read <file>` — the file with long function bodies folded to signatures.
  Slices: `codeq read a.rs:40-80 b.rs:12,90-95` reads several ranges of several
  files in one call (the `sed -n 'A,Bp'` you would otherwise run per slice).
  For every source byte without display prefixes, use `codeq read <file> --raw`;
  `--full` unfolds but keeps line numbers.
- `codeq outline <file>` — just the definitions and their nesting.

**Understand or change one symbol (reach for this first)**
- `codeq explore <NAME>` — one call returns the definition + callers + callees *and* the
  verbatim source of that whole neighbourhood, budgeted and grouped by file (each source
  block tagged focus/caller/callee). Use it instead of a `definitions` → `callers` → `read`
  → `read` sequence when you need to understand or edit a function/type. `--max-tokens`
  caps the source; the definition is always kept and emitted **first**, so
  `explore … | head -N` samples the definition rather than a neighbour. It
  also returns the focus file's imports — what is in scope where you're about to edit.
  Add `--resolved` on a polyglot repo: callees are matched by name, so a Go
  function otherwise pulls in same-named TypeScript and spends the token budget
  on it (391 lines to 109 on a real repo). `about` takes it too.
- `codeq explore --at <file>:<line>` — the same, addressed by position instead of name:
  explores whatever definition encloses that line (innermost wins). Use it on a stack
  frame, a diff hunk or a search hit rather than running `at` and then `explore`.
- `codeq about <NAME>` — the same neighbourhood with **no source**, but fuller metadata
  per node (doc comment, `owner`, which callers are tests). Source is the only axis the
  two differ on: `explore` to change code, `about` for metadata or a cheap "which file
  does this live in".
- `codeq read <file> --symbol <NAME>` — that definition's source, line-numbered. Add
  `--raw` for exact bytes with no line-number prefix (to copy it verbatim).

**Who uses it / what breaks if I change it**
- `codeq callers <NAME>` — definitions that call/reference NAME (test sites flagged).
  Add `--examples` to show the actual call expression at each site (how NAME is really
  invoked, one per caller); `references <NAME> --calls` for every call line.
- `codeq references <NAME>` — every *line* holding an occurrence, with the
  enclosing symbol (three calls on one line are one row, anchored at the first).
- `codeq tests <NAME>` — which tests exercise NAME *by name*. A test is recognised
  structurally where a framework marks one — `#[test]`/`#[tokio::test]`,
  `@pytest.mark.…`, `@Test`, `[Fact]` — and otherwise by file path, its own name, or a
  test-like enclosing module/class (so Rust's `#[cfg(test)] mod tests` counts). JS/TS
  `it(…)` and Go `TestX` have no attribute, so those still lean on path and name.
  Empty means only exercised indirectly, e.g. an internal fn tested through a CLI.
- `codeq affected --compare-to <ref>` — for a diff: the definitions it touched, their
  transitive callers (impacted), and the test *files* to re-run. Use after making
  changes to know what to re-check. Name-based, so it over-includes rather than misses.
- Transitive: `codeq callers <NAME> --depth 3` (up the call tree),
  `codeq callees <NAME> --depth 3` (down).

**Trace control flow**
- `codeq callpath <FROM> <TO>` — the shortest call chain from FROM to TO. Both
  arguments are *symbols*, not files: this has nothing to do with `--path`. Names resolve
  across ALL languages, so a common name like `main` may land in a Python/JS file —
  scope the start with `--path`: `codeq callpath main write --path src/main.rs`.

**What a symbol / file depends on**
- `codeq callees <NAME>` — what NAME calls.
- `codeq deps <file>` — internal files a file imports (`codeq deps --cycles` for cycles).
- `codeq imports [PATH]` — raw, unresolved module strings. `codeq importers <QUERY>` —
  files importing a module (a path like `src/db.rs` is reduced to its stem `db`).

**Position and bundles**
- `codeq at <file>:<line>` — the definition enclosing a position. Turns a stack frame,
  diff hunk or grep hit into a symbol.
- `codeq context --symbols a,b` — verbatim source for several symbols under one token
  budget (`--changed --compare-to <ref>` seeds from a diff instead). `--expand` adds one
  hop of referenced definitions, matched *by name* — add `--resolved` to keep only those
  the seed files reach through imports, or a common name spends the budget on unrelated
  same-named code.
- `codeq files [PATTERN]` — indexed files, with `--language`.

**API surface / cleanup**
- `codeq api <path>` — the public/exported definitions of a file or directory.
- **Empty result?** codeq says why on stderr (`no symbol named 'X'; closest: Y`,
  `'X' ... has no callers`, `nothing matches --path Z`), or in `note` under
  `--json`. Read it before concluding the symbol or the command is wrong.
- **Markdown**: headings are definitions. `outline doc.md` = table of contents;
  `read doc.md --symbol "Section"` = one section (not the whole file); `--in
  strings` = fenced examples, `--in comments` = prose; local links/images are
  file references for `deps`/`importers`.
- **SQL**: `.sql` yields `table`, `view`, `index`, `trigger`, `function`, `type`
  and `module` (schema) definitions, with columns as `field`s of their table — so
  `definitions users` finds the table, `members users` lists its columns
  (including ones a later `ALTER TABLE` added), and `api schema.sql` is the
  schema's surface. Because reference matching is textual, `references users`
  also finds the table named inside the SQL *string* in the app code that queries
  it — the schema is usually the one artefact every language in the repo shares.
  A schema-qualified name is stored bare (`analytics.events` → `events`); enum
  variants lose their quotes; `--in strings` is quoted literals only. No imports.
  A **foreign key is a `references` relation** (its own kind — an edge, not an
  is-a), so `supertypes orders` is what it points at and `implementors orgs` is
  what points at it; inline, table-level and `ALTER TABLE` forms all count.
- `codeq members NAME` — a type's fields, enum variants and methods (`--kind` to
  narrow). Containment is structural (a member is nested inside NAME's byte range),
  though NAME itself still resolves by name — two same-named types both answer, so
  scope with `--path`. Already included in `about`. Rust struct methods live in a
  separate `impl`, so a Rust struct returns fields only. Prefer this over
  `symbols --owner NAME`, which only sees fields/variants — never methods.
- `codeq unused [<path>]` — definitions with no references (candidate dead code; empty
  output = none found). Conservative and name-based, so it under-reports. Two tags mark
  a row a framework reaches without naming it — shown rather than hidden, so discount
  them yourself: `decorator=@app.get(...)` is registration codeq can see, and
  `entrypoint=next/page` is a file convention it can't (Next.js App Router and Pages
  Router exports, `react/lifecycle` methods). A Rust `impl Trait for X` method is
  *not* shown: it is called through the trait and deleting it breaks the build, so it
  is never dead (an inherent `impl X` method with no callers still is).
- `codeq entrypoints [<path>]` — the definitions those two tags identify, on their own:
  what calls into this repo from outside it. `--include-tests` adds test-runner entry
  points.
- `codeq untested [<path>]` — public definitions no test names. Fields, variants
  and constants are excluded unless `--kind` asks: none has behaviour to test,
  and they crowd out the functions that do.
- `codeq clones [<path>]` — definitions with an identical syntax shape. Tests excluded
  by default. Exact equality, so it finds unedited copy-paste and **not** two
  implementations of one algorithm that differ in their accessor expressions; an empty
  result means "no exact copies", not "no duplication".
- `codeq implementors <TYPE>` / `codeq supertypes <TYPE>` — who implements/extends a
  trait/interface/class, and what a type derives from.
- `codeq hotspots [<path>]` — the inverse of `unused`: definitions ranked by how often
  their name is referenced. One row per name; `--calls` counts only call sites (use it
  for functions — otherwise a name that is also a common variable dominates).

**Search**
- `codeq search <REGEX>` — content search; each match annotated with its enclosing
  symbol. `--in code|comments|strings` to scope. A superset of grep — it also has
  `--fixed`, `-i`, `-w`, `-g '*.rs' -g '!tests/'` (repeatable globs), `-l`, `-c`,
  `-B`/`-A`/`-C`, `--path`, `--language`, `--limit` — and grep's calling
  convention, `codeq search TODO src/a.rs docs`, where each positional path is a
  substring (a directory scopes under it) and several are OR'd. grep's other flags work too: `-e A -e B`, `-o`, `-v`,
  `-L`, `-F`, `-t py`. A path the index
  does not hold (gitignored output, a log, a file outside the repo) is searched
  from disk, with a note and without `[symbol]`. **`--limit` defaults to 100**,
  and a result that hit it says `stopped at --limit` on stderr — raise it (or
  pass 0) for the rest. Other commands cap too, and the defaults differ:
  `files`/`symbols`/`members`/`imports`/`untested` at 200, `hotspots` at 30. All
  of them now say when they cut.
  `-w` is an *identifier* boundary rather than `\b`: `$` continues a name only in
  JS/TS-family and Java-family **code** (not inside a string, where it is a
  template or a JVM binary name), and Unicode punctuation ends a name where a
  Unicode letter does not.
  `CMD | codeq search PAT --stdin [--language L]` scans piped text instead of the
  index, for grep-in-a-pipeline over output that isn't in the repo.
- `codeq astgrep '<PATTERN>' --language <lang>` — structural (AST) search, e.g.
  `codeq astgrep '$X.unwrap()' --language rust`. Scope to files with `--path <substr>`
  (a `--path` matching no indexed file of that language says so, rather than looking
  like "no matches"). **Reach for this when `clones` comes back empty**: two
  implementations that differ only in their accessor expressions have different trees,
  so `clones` cannot see them and a pattern can.
  Each match is annotated with the `[symbol]` it sits inside.
- `codeq astgrep --explain <file>:<line>` — the parse tree covering that line:
  each node's `kind`, the `field` it fills in its parent, and its source. **Write
  patterns from this rather than guessing kinds**, and run it on a line you know
  should match before believing an empty `astgrep` result — a wrong kind and no
  matches look identical. No `--language` needed.

## Gotchas
- **Symbol lookups match by NAME across the whole repo, in every language.** For a
  common/ambiguous name (`new`, `build`, `main`), scope with `--path <substr>` —
  supported on `about`, `callers`, `callees`, `definitions`, `references`,
  `implementors`, `unused`, `astgrep`, `callpath`, and more. Don't know a symbol's file? Run
  `codeq about <NAME>` — it shows the definition file(s).
- **`definitions` is exact + case-sensitive; `symbols` is substring + case-insensitive.**
  In a polyglot repo one concept is spelled per language (Go `GetIPAddress`, JS
  `getIPAddress`, Python `get_ip_address`) — `symbols ipaddress` finds them all in one
  call, where `definitions` needs each exact spelling. `definitions` to jump to a name
  you know; `symbols` to find a concept across languages.
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
  `--calls`. Add `--examples` to see the call as written with its receiver
  (`subprocess.run`, not `run`) — the quickest way to tell a real callee from a
  name collision. It does take `--resolved`, which keeps only callees whose definition file
  the symbol's own file can reach via imports — pruning unrelated same-named defs (a
  common `new`/`open` matching every same-named definition repo-wide). Rows tagged
  `via=ambiguous` mean several reachable files define that name: "one of these",
  not "this one". Same vocabulary as `callers --resolved`.
- **JS/TS: `proto.use = function use(){}` is a method.** Module-scope property
  assignment — `X.y = fn`, `X.prototype.y = fn`, `exports.y = fn` — is how most
  of Node is written and it is now indexed under the *property* name (`use`), so
  scope common names with `--path`. `exports.x` is public; other objects are
  private (codeq doesn't trace whether they escape).
- **JS/TS: a module-scope `const` is a definition**, whatever it's bound to. That's
  how React components are usually written — `memo(…)`, `forwardRef(…)`,
  `styled.div\`…\``, `createContext(…)` — so `symbols`/`outline`/`api` find them, and
  they show up as kind `constant`, not `function`. A CommonJS
  `var x = require('y')` is an import — find it with `imports`/`deps`.
- **A `const` bound to an arrow function is a `function`, at any nesting.** A
  component's local handlers are definitions of their own, indented under it in
  `outline`, so `explore handleClick` works — a local `const` bound to anything
  else is not. They read as signatures (`withCDN(path: string)`,
  `async load(u: string): Promise<void>`), not as the assignment they're written
  as, and the single-parameter form gets its parentheses back (`x => x` is
  `bare(x)`).
- **A namespaced base class is stored dotted** (`extends React.Component` →
  `React.Component`), but `implementors` matches the last segment too, so
  `implementors Component` finds it.
- **Where the rows are:** under `--json` the envelope carries `rows`, the dotted
  path to the row array (`"data"`, `"data.matches"`, `"data.files"`). Containers
  differ by command on purpose (`search -l` returns files, not matches); read
  `rows` rather than memorising them. Absent when a result holds several lists.
- **Trim output tokens** with `--fields name,path,line`. The keys are identical on
  every command: `name` = the definition this row *is*; `symbol` = the definition it
  sits *inside* (only on `search`/`references`/`astgrep` matches); `line`/`col`/`byte`
  for position, `end_line`/`end_col`/`end_byte` for extent. No command uses
  `start_line`.
- **`--group`** replaces the repeated path prefix with a `path (count)` header. Opt-in,
  and the default is flat for a reason: a flat row names its own file, so it survives
  being piped, grepped or quoted in isolation, and a grouped row does not. Group when
  you'll read the whole result; stay flat when you'll slice it. No effect under `--json`.
  Worth 22–37% on results that cluster in a few files; it *costs* ~4% when hits are
  spread one-per-file, so it isn't a free default.
- **`--max-per-file N`** keeps at most N rows per file — the big saving (`--group
  --max-per-file 3` cut broad searches by 65–91%). **Lossy**, so it changes the data,
  `--json` included. Capped files always state their true count (`a.rs: +237 more (of
  240)`, or a `path (3 of 240)` header, or `notes.truncated_per_file`), so a truncated
  answer can't be mistaken for a whole one. Use it for the *shape* of a result — which
  files mention something and roughly how often — then rerun uncapped for detail. It
  surveys every file, not just the first `--limit` rows' worth.
- Default output is text; use `--json` only when you need to parse it programmatically.

## Excluding files
codeq honours `.gitignore`/`.ignore` and a `.codeqignore` for committed-but-not-code
trees (data, fixtures, generated assets). Gitignore syntax; note that `!keep.json`
cannot re-include a file under an excluded `data/` — exclude `data/*.json` instead.
