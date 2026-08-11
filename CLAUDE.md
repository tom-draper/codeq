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
  It also reports the focus file's `imports` — what is already in scope at the edit
  site, so a proposed call isn't to something the file can't see.
  - **From a position, not a name:** `codeq explore --at <file>:<line>` explores
    whatever definition encloses that line (innermost wins). For turning a stack
    frame, a diff hunk or a search hit straight into its neighbourhood, instead of
    `at` to get the name and then `explore`. The position scopes the lookup too, so
    a common name can't drag in same-named definitions from other files. A line
    between definitions is an empty result with a note, not an error.
- **A type's members:** fields, properties and enum variants are facts too, tagged with
  the `owner` type — `codeq definitions retries` says which type declares it, and
  `codeq outline <file>` nests members under their type. Rust, Go, Java, TS/TSX,
  C#, Python, C, C++, PHP, Kotlin and Swift; the remaining languages return just
  the type. Python has no declaration site, so a member is its earliest class-body
  or `self.x =` assignment (`other.x =` is not a declaration).
- **JS/TS: a module-scope binding is a definition.** So much of JavaScript that is
  conceptually a definition is a `const` bound to a *call* — `memo(…)`,
  `forwardRef(…)`, `styled.div\`…\``, `createContext(…)`, `express.Router()` — and
  a plain `export const VERSION` is public API. All of these are `constant` facts,
  so `symbols`/`outline`/`api` see React components however they are wrapped.
  Module scope only: locals and loop counters are not definitions, and
  destructuring (`const { a } = …`) declares nothing at that site. A CommonJS
  `var x = require('y')` is an **import**, not a definition — it is already an
  import fact, so look for it with `imports`/`deps`, not `symbols`. When the value
  spans several lines the signature stops after the wrapper — `const Card = memo`
  — so it says what the binding *is*.
- **Orient:** `codeq map` (grouped totals) · `codeq tree` (directory *shape*, folded
  to `--depth`, each directory carrying the file and symbol counts of everything
  below it). `map` collapses to top-level directories, so on a nested repo it hides
  where the code is; `tree --min-symbols 1` goes straight to the directories that
  hold any. Ignore-aware, so no `node_modules`/`.git` noise — on a 2,700-file data
  repo it answers in 8 lines what `find -type d` needs 44 KB for.
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
- **`--kind method` vs `--kind function` is meaningful.** A function declared
  inside a type body is a `method` (it can be called on a value); a nested
  function or closure is not, however deep the class above it. Rust needs a
  `self` parameter, so `S::assoc()` stays a `function` while `s.meth()` is a
  `method`. Elixir `def`s are module functions, so they stay `function`.
- **Understand a symbol before editing, without pulling in source:** `codeq about
  NAME` — the same neighbourhood `explore` walks, minus the code and plus fuller
  metadata per node (doc comment, `owner`, which callers are tests). Source is the
  only axis the two differ on: `explore` to change code, `about` for the metadata
  or a cheap "where does this live".
- **What's at this line?** `codeq at <file>:<line>` — the definition enclosing a
  position. For turning a stack frame, a diff hunk or a grep hit into a symbol.
- **Assemble a token-budgeted bundle:** `codeq context --symbols a,b` — verbatim
  source for several named symbols under one budget, or `--changed --compare-to
  <ref>` to seed from a diff. `explore` is this plus the graph for one symbol;
  reach for `context` when you already know the set you want.
  - `--expand` adds one hop of definitions referenced by name in the seeds — and
    because that hop is *by name*, a seed mentioning `new`/`open`/`build` drags in
    every same-named definition in the repo. `--expand --resolved` keeps only the
    ones living in a file the seeds reach through imports (the same heuristic as
    `--resolved` elsewhere). On a seed referencing common names this cut the
    bundle from 34 chunks to 6. When candidates exceed the neighbour cap it
    changes *which* survive rather than how many, so don't read the count alone.
- **List files / imports:** `codeq files [PATTERN]` (indexed files; `--language`)
  · `codeq imports [PATH]` (raw, unresolved module strings, filtered to importing
  files whose path contains PATH) · `codeq importers <QUERY>` (files importing a
  module; a path like `src/db.rs` is reduced to its stem `db`).
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
    - With either filter, each row also carries **`on=<receiver>`**, the thing the
      call was made on. codeq shows it rather than filtering on it: a receiver may
      be a value (`db.stem()`) or a module path (`helpers::stem()`), and telling
      those apart needs name resolution codeq doesn't do. So when a name is both a
      method and a free function, read `on=` and decide — don't expect codeq to.
    - `--examples` (callers) — surface the actual call expression at each site (one
      representative per caller): the row becomes `path:line:col:` the source line,
      so one command answers "who calls it *and how*". For every call site rather
      than one per caller, use `references NAME --calls`. Composes with the filters.
  - **`callees` is already call-position only** (a bare mention of a name isn't a
    callee, and `helper()` written inside a string or comment isn't either), so it needs no `--calls`; it does take `--resolved`, which keeps only
    callee), so it needs no `--calls`. `--examples` adds `call` — the call as
    *written*, receiver included (`subprocess.run`, not `run`) — plus `call_line`.
    That is usually the fastest way to spot a name collision: a module-qualified
    call is attributed to every definition sharing the bare name, so
    `callees codeq_commit` lists four unrelated `run`s, and `--examples` shows all
    four came from one `subprocess.run`. It also takes `--resolved`, which keeps only
    callees whose definition file NAME's *own* file can reach through imports (tags
    each `via` same-file / imported) — pruning the name-based over-match where a
    common callee name (`new`, `open`) matches unrelated same-named defs in
    unreachable files.
  - **Transitive call tree:** `codeq callers NAME --depth N` (up) ·
    `codeq callees NAME --depth N` (down) — BFS to N hops, each result tagged
    with its `depth`; text output indents by hop. For impact / reachability.
  - **Call chain between two symbols:** `codeq callpath FROM TO` — the shortest chain
    where FROM calls … calls TO, shown step by step (empty if unreachable within
    `--max-depth`, default 8). Reachability *with the route*, vs `callees --depth`
    which only says whether/at what depth.
- **Who implements / extends a type:** `codeq implementors NAME` — the inheritance
  edges the call graph misses, across 18 languages (`impl T for X` in Rust,
  `extends`/`implements` in Java/TS/PHP/C#/C++/Dart, `class X : B` in
  Kotlin/Swift, `extends ... with` in Scala, `contract A is B` in Solidity,
  `class X(Base)` in Python, `class X < Y` in Ruby, `implements` in GraphQL,
  and the `<script>` body of a Svelte/Vue component). `--relation
  implements|extends` to filter. Go (structural typing), C and Zig have no
  inheritance at all — `capabilities.language_support` says which, and an empty
  result there says so rather than implying none exist. Name-based.
  - **A namespaced base class is stored dotted, but matched either way.**
    `class Legacy extends React.Component` records the supertype as
    `React.Component`, and `implementors` matches the last segment too — so both
    `implementors Component` and `implementors React.Component` find it.
  - **The reverse — what a type derives from:** `codeq supertypes NAME` — the
    traits / interfaces / base classes NAME extends or implements (as written at
    the declaration, not transitively resolved).
- **Check what codeq supports before trusting an empty answer:** `codeq
  capabilities` includes `language_support`, a per-language matrix of which
  extractions exist. Go and C have **no** inheritance edges, Ruby declares no
  fields, Bash/Lua/Zig/Elixir have no imports — for those, "none found" is not
  evidence of absence, and the empty-result note says so.
- **Public API with no tests:** `codeq untested [<path>]` — `api` composed with
  `tests` in one call instead of one per symbol. Public only; over-reports
  coverage (a test that never names the symbol is invisible).
- **Did this change the public surface:** `codeq api <path> --compare-to <ref>` —
  exported declarations added, removed, or changed shape, grouped and with the
  previous signature. The review question a raw diff can't answer.
- **Which tests exercise it:** `codeq tests NAME` — the test functions that
  reference NAME. A test is recognised by test-file path, test-like name, *or* a
  test-like enclosing module/class — so Rust's `#[cfg(test)] mod tests` inside the
  file under test counts, as do Python `class TestFoo` / Java `class FooTest`.
  codeq reads no attributes, so a `#[test]` fn with none of those three still
  slips through. For the edit→find-the-test loop.
- **What a diff affects:** `codeq affected --compare-to <ref>` — the definitions the
  diff touched, their transitive callers (impacted, `--depth N`), and the test
  *files* to re-run. Composes `changed --symbols` + a caller BFS + `tests`; name-based,
  so it over-includes rather than misses.
- **Is this already implemented?** `codeq clones` — definitions with an identical
  syntax shape (identifiers and literals normalised away). Exact structural
  equality, never a similarity score; `--min-nodes N` raises the floor on how
  much code counts. Costs ~20% on a cold index, nothing on a warm one.
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
  anywhere outside their own definition site. Conservative (a comment/string
  mention counts as a use; name-based, so a same-named symbol elsewhere masks it)
  so it under-reports rather than mislead. Shows `visibility` (a `public` hit may
  be external API, not dead); tests excluded unless `--include-tests` — same
  recognition as `codeq tests`, so a Rust `mod tests` unit test is not dead code.
- **What's load-bearing here?** `codeq hotspots [<path>]` — definitions ranked by
  how often their name is referenced in code; the inverse of `unused`, for meeting
  an unfamiliar file. One row per *name* (the count is a property of the name),
  with `defs` showing how many definitions share it. Add `--calls` to count only
  call sites — without it a function whose name is also a common variable (`s`,
  `new`) scores on every mention. Read a row as "this identifier carries weight",
  then `callers NAME --calls` for the precise question.
- **Search (a superset of grep):** `codeq search PATTERN [-C N] [--in code|comments|strings]`
  - `--in strings` (not `--in code`) to find string-embedded content — SQL, config,
    templates. codeq's own SQL lives in string literals, so `--in code` finds none.
  - **It has grep's flags too, so you never need to shell out:** `--fixed` (literal,
    not regex), `-i`, `-w`, `-g '*.rs' -g '!tests/'` (repeatable include/exclude
    globs), `-l` (paths only), `-c` (counts), `-B`/`-A`/`-C`, `--path`, `--language`,
    `--limit` (default 100 — raise it or pass 0 before concluding "no more hits").
  - `--path` / `--language` / `--limit` are on `references` and most list commands
    as well; narrow with them instead of post-filtering the output.
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
- **What a type holds:** `codeq symbols --owner Config` — the members a type
  declares, without dumping every field in the repo.
- **Trim tokens on list output:** append `--fields a,b`
- **`--group`** hoists the repeated path onto a `path (count)` header instead of
  prefixing every row with it, and the count saves counting lines. **Opt-in on
  purpose, and worth understanding before reaching for it:** the default flat
  `path:line:col:` form is *self-describing*, so any single row still names its
  file after being piped, grepped, or quoted on its own — grouping trades that
  away. Use it when you'll read the whole result; keep the default when you'll
  slice it. Never inferred from whether stdout is a terminal, and it has no
  effect under `--json`.
  - Measured on real repos: **22–37% fewer bytes** when hits cluster (many per
    file). But it *costs* bytes when they don't — a one-hit-per-file result went
    **−4%**, because a header per single-row group is worse than the prefix it
    replaces. Group dense results, not sparse ones.
- **`--max-per-file N`** keeps at most N rows per file. This is where the real
  saving is: `--group --max-per-file 3` cut a broad search by **65–91%** across
  three repos. Unlike `--group` it is **lossy**, so it changes `data` itself and
  applies to `--json` too — text and JSON never disagree about which rows the
  answer holds. Every capped file reports its true count (`a.rs: +237 more (of
  240)` in text, a `path (3 of 240)` header under `--group`,
  `notes.truncated_per_file` in JSON), so a truncated answer cannot read as a
  complete one. Reach for it when you want the *shape* of a result ("which files
  mention this, roughly how often") rather than every occurrence.
  - It surveys the **whole** result, not the first `--limit` rows: matches past a
    file's cap are still counted, they just don't spend the limit. So `--limit`
    bounds the rows you get back while the `of N` totals stay true.

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

It also says when an answer is **incomplete**: `result stopped at --limit` means
you are looking at the first `--limit` (default 100) rows of a longer result.

## Excluding files

codeq honours `.gitignore`/`.ignore`, plus a **`.codeqignore`** for trees that are
*committed* but not code (vendored data, fixtures, generated assets) — gitignore
cannot help there by definition. Gitignore syntax and precedence-over-`.gitignore`,
with git's own sharp edge: `!data/keep.json` will not resurrect a file under an
excluded `data/`, because the walk never descends into it. Exclude `data/*.json`
instead. Editing it takes effect on the next query — no reindex step.

## Known limitations (found by dogfooding — don't be misled)

- **Symbol commands match by NAME across the whole repo.** For a common name
  (`new`, `build`, `open`), scope with `--path <substr>`:
  `codeq about build --path context` targets the `build` in that file. Without a
  scope, `definitions`/`callees`/`about` cover every same-named symbol. (`about`
  keeps callers repo-wide even when scoped.)
- **References/callers are textual (name-based), not type-resolved.** `--resolved`
  narrows them using the import graph (see above), but it's a heuristic filter,
  not real type resolution.

## Reading a dependency

Dependency trees are gitignored, so `node_modules`, `vendor` and friends are
invisible to the normal index (correctly: they are not this repo's code). To read
one, point codeq at it: `codeq --root node_modules/lodash api`. `--root` takes the
directory literally — no walking up to a Git toplevel, no parent ignore rules — so
every command works there. It writes a `.codeq` index inside that directory.

## Build / test

- `cargo build --release` · `cargo test` (integration tests in `tests/cli.rs`)

**Before committing, run the correctness harnesses too — `cargo test` alone is not
enough.** It uses small synthetic fixtures; the harnesses run against real code and
catch what fixtures cannot.

```bash
python3 bench/textmode.py      # seconds. Text output carries the same facts as --json
python3 bench/differential.py  # ~10 min (clones 20 repos once, then cached)
```

`differential.py` is the important one: ~5,200 assertions over 25 pinned
open-source repos, checking that `search` is a superset of ripgrep and that
per-symbol invariants hold in every supported language. It writes a result to
`bench/results/` (committed, so the trend lives in git). Expect
`N PASS, 0 FAIL, 0 ERROR`.

Run these whenever a change touches extraction, the search engine, the renderer,
or the schema — which is most changes. Skipping them is how six commits once went
in unverified: everything passed in the end, but nobody knew that at the time.

- Token/benchmark numbers: `bench/bench.py` (single commands), `bench/tasks.py`
  (task-level), `bench/completion.py` (the token-to-completion table in the
  README — rerun it if a change moves those numbers, since they are published as
  current).
