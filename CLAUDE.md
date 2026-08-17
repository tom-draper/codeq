# codeq — working in this repo

codeq is a deterministic repository index for AI agents (this repo builds it).
**Dogfood it:** when navigating this codebase, prefer codeq over raw whole-file
reads and grep, and log any friction in [DOGFOOD.md](./DOGFOOD.md).

Build with `cargo build` → run `./target/debug/codeq` from the repo root
(`--release` for benchmarking). Output defaults to compact grep-style text
(`path:line:col: payload`); pass `--json` for the `codeq.result.v2` envelope
(what the MCP server and programmatic consumers use).

**Never run `codeq index` for correctness — every query auto-refreshes first.**
Each command opens the index and reruns the git-free freshness check (changed/
added/removed files get reparsed, schema-version bumps trigger a rebuild) *before*
answering, so a query always reflects the current tree — even right after an edit.
The refresh count appears as `reindexed_files` in the `index` block under
`--json`/`--meta`. In text mode a real reparse prints `# codeq: reindexed N
file(s)` on **stderr** — so stdout stays pure `path:line:col:` rows and a pipe
never sees it, but the terminal is not silent. Freshness is keyed on the content
hash, so a bare `touch` reparses nothing and prints nothing; only a real edit
does. `index` exists
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
- **`__all__` and `__path__` are private, not API.** Metadata the interpreter
  reads, declared once and referenced never, so they stay out of `api` and out of
  `unused`. Not a blanket dunder rule — `__version__` *does* get referenced.
- **Python: a module-scope binding is a definition too**, on the same rule as
  JS/TS below. `DATA_DIR = Path(...)`, a 52-entry `alias` dict, every settings
  constant — all `constant` facts, so `definitions`/`outline`/`api` see them.
  Module scope only: locals are not definitions, and destructuring (`a, b = …`)
  declares nothing at that site. A bare annotation (`X: int`, no value) declares
  a type, not a binding, and is not indexed.
- **A member of a private type is private**, whatever its own modifier says: the
  methods and fields of a Python `class _Draw` are not public API, so they stay
  out of `api` and `untested`. Checked up the whole chain of enclosing types, so
  a public inner class inside a private outer one is private too.
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
  - **A module-scope property assignment is a method.** `proto.use = function
    use(fn){}`, `Thing.prototype.render = …`, `exports.helper = …` — how most of
    Node is written, and it used to be invisible: `outline` on Express's entire
    router file returned nothing. The name is the *property* (`use`), so common
    names collide across files — scope with `--path`. `exports.x`/
    `module.exports.x` are `public`; anything else is `private`, because knowing
    that `proto` escapes via `module.exports` needs data flow codeq doesn't do.
- **The whole relation graph:** `codeq graph [--relation references|implements|
  extends] [--language L]` — every type edge in the repo as `from -> to`, not one
  node's neighbours. `--mermaid` emits a renderable `graph LR` document (a
  document, not rows — it leaves the `path:line:col:` contract on purpose, like
  `read --raw`). Relations only, and **deliberately not calls**: a relation edge
  is read off the parse tree (`impl T for X`, a SQL `REFERENCES`) so the graph is
  exact, whereas the call graph is name-based and every hop compounds the
  over-match — a whole-repo call graph would be mostly plausible-looking noise.
  Use `callers`/`callees --depth N` there, rooted at a symbol you can check.
  TypeScript/TSX has explicit `extends`/`implements` syntax, so it contributes edges;
  Go does not. Go interfaces are structural, and inferring their implementors would
  require type checking rather than Codeq's syntax extraction.
- **Orient:** `codeq map` (grouped totals; `--totals` for just the headline
  numbers and the per-language table, which skips the row scans and stays flat as
  the repo grows — and it says so on stderr when most of the repo is assets codeq
  cannot index, so the byte total isn't misread as coverage) · `codeq tree` (directory *shape*, folded
  to `--depth`, each directory carrying the file and symbol counts of everything
  below it). `map` collapses to top-level directories, so on a nested repo it hides
  where the code is; `tree --min-symbols 1` goes straight to the directories that
  hold any. Ignore-aware, so no `node_modules`/`.git` noise — on a 2,700-file data
  repo it answers in 8 lines what `find -type d` needs 44 KB for.
  - **`--min-symbols` alone is not enough in an asset-heavy repo**, because a
    markdown heading is a definition: 281 per-country READMEs outranked the source
    68 to 1 on a real one. Use **`tree --language python`** (or `files
    --language`) when you want where the *code* is; `--min-symbols` is for when
    prose and code are both wanted.
- **Docs are indexed too.** A markdown heading is a definition, so `outline
  README.md` is a table of contents, `read README.md` folds to that ToC with each
  section's line range, `read README.md --symbol "Output contract"` (or
  `"## Output contract"`, the form `outline` prints) returns just
  that section (read this instead of the whole file), and `at README.md:230` says
  which section a line is in. Search hits carry their `[Section]`. `--in strings`
  searches fenced examples, `--in comments` searches prose (`--in code` is
  correctly empty: markdown has no code). Local links and images are file
  references, so `deps`/`importers` work across docs and assets.
- **The schema is indexed too.** A `.sql` file yields `table`, `view`, `index`,
  `trigger`, `function`, `type` and `module` (schema) definitions, with columns as
  `field`s owned by their table — so `definitions users` finds the table and
  `members users` lists its columns. The payoff is cross-language: reference
  matching is textual, so `references users` finds the table named inside the SQL
  *string* in the Python or Go that queries it, and `callers touch_updated_at`
  finds the `CREATE TRIGGER` that fires it. Details worth knowing:
  - A column added by a later `ALTER TABLE` still belongs to its table — but only
    when the table name has one definition repo-wide, since that link is by name.
  - `CREATE TABLE analytics.events` is stored as `events`; the qualified form is
    in the signature.
  - Enum variants lose their quotes: `members mood` says `sad`, not `'sad'`.
  - `--in strings` is quoted literals only; `42` and `NULL` stay `--in code`.
  - No imports (SQL has none). A **foreign key is a `references` relation** — its
    own kind, not `implements`/`extends`, because it is an edge but not an is-a.
    So `codeq supertypes orders` is what `orders` points at and `codeq
    implementors orgs` is what points at `orgs`; add `--relation references` to
    ask for only those. All three syntactic forms count: an inline column
    `REFERENCES`, a table-level `CONSTRAINT … FOREIGN KEY`, and a later
    `ALTER TABLE … ADD CONSTRAINT`.
  - `unused` skips SQL indexes, triggers and enum variants — each is named exactly
    once by construction, so reporting them is noise, not a finding. Tables,
    views, columns and functions are reported normally.
  - `CREATE PROCEDURE` is not in the grammar and yields nothing.
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
  - A caller row with **`scope=file`** and no `name` is a reference at module
    scope, with no definition around it — a Python module-level call, a JS
    property assignment. Real, but not a definition, which is why it is labelled
    rather than left blank. Both take `--limit` (default 0 = unlimited).
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
    each `via` same-file / imported / **ambiguous**) — pruning the name-based
    over-match where a common callee name (`new`, `open`) matches unrelated
    same-named defs in unreachable files. `ambiguous` is the residual it can
    narrow but not resolve: several reachable files define that name, so read the
    rows as "one of these" rather than "this one". Same vocabulary as `callers
    --resolved`, so the two agree about the same situation.
  - **Transitive call tree:** `codeq callers NAME --depth N` (up) ·
    `codeq callees NAME --depth N` (down) — BFS to N hops, each result tagged
    with its `depth`; text output indents by hop. For impact / reachability.
  - **Call chain between two symbols:** `codeq callpath FROM TO` — the shortest chain
    where FROM calls … calls TO, shown step by step. An empty result says *which*
    of the four causes it was: a misspelled FROM, a misspelled TO, both real but
    unconnected, or a chain past `--max-depth` (default 8). Reachability *with the route*, vs `callees --depth`
    which only says whether/at what depth.
- **Who implements / extends a type:** `codeq implementors NAME` — the inheritance
  edges the call graph misses, in languages with explicit relation syntax (`impl T for X` in Rust,
  `extends`/`implements` in Java/TS/PHP/C#/C++/Dart, `class X : B` in
  Kotlin/Swift, `extends ... with` in Scala, `contract A is B` in Solidity,
  `class X(Base)` in Python, `class X < Y` in Ruby, `implements` in GraphQL,
  and the `<script>` body of a Svelte/Vue component). `--relation
  implements|extends` to filter. Go (structural typing), C and Zig have no relation
  extraction — `capabilities.language_support` says which, and an empty
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
  extractions exist. Go, C and Zig have **no** relation extraction, Ruby declares no
  fields, Bash/Lua/Zig/Elixir have no imports — for those, "none found" is not
  evidence of absence, and the empty-result note says so.
- **Public API with no tests:** `codeq untested [<path>]` — `api` composed with
  `tests` in one call instead of one per symbol. Public only; over-reports
  coverage (a test that never names the symbol is invisible). Test scaffolding is
  excluded by the same rule `unused` uses — path, name, *or* enclosing test
  module — so a `pub fn` helper inside `#[cfg(test)] mod tests` is not "untested
  public API".
- **Did this change the public surface:** `codeq api <path> --compare-to <ref>` —
  exported declarations added, removed, or changed shape, grouped and with the
  previous signature. The review question a raw diff can't answer. A definition
  **moved out and imported back** is not a removal: the name is still bound and
  still importable, so it is reported in a note ("still exported, now via an
  import") rather than as a breaking change.
- **Which tests exercise it:** `codeq tests NAME` — the test functions that
  reference NAME. A test is recognised **structurally first**: a test *attribute*
  the grammar already parses — `#[test]`, `#[tokio::test]` (any `::test` path),
  `#[bench]`, `rstest`/`proptest`/`quickcheck`, `@pytest.mark.…`/`@pytest.fixture`,
  `@Test`/`@BeforeEach` (Java/Kotlin/Scala), `[Fact]`/`[Theory]`/`[Test]`
  (C#), `#[Test]` (PHP). Failing that, the label signals still apply: test-file
  path, test-like name, or a test-like enclosing module/class (so Rust's
  `#[cfg(test)] mod tests` and Python `class TestFoo` count). The marker list is
  fixed, not a "contains test" pattern — `#[inline]` and `#[derive(Debug)]` mark
  nothing, and neither does `@TestOnly`. Languages whose frameworks use bare
  calls rather than attributes (JS/TS `it(…)`, Go `func TestX`) still rely on the
  name and path. `unused` and `untested` share the same predicate, so all three
  agree on what scaffolding is. For the edit→find-the-test loop.
- **What a diff affects:** `codeq affected --compare-to <ref>` — the definitions the
  diff touched, their transitive callers (impacted, `--depth N`), and the test
  *files* to re-run. An impacted row's `line` is the definition's, per the row
  rule; **`call_line`** is where the impact enters it. Composes `changed --symbols` + a caller BFS + `tests`; name-based,
  so it over-includes rather than misses.
- **Is this already implemented?** `codeq clones [<path>]` — definitions with an
  identical syntax shape (identifiers and literals normalised away). Exact
  structural equality, never a similarity score; `--min-nodes N` raises the floor
  on how much code counts. Costs ~20% on a cold index, nothing on a warm one.
  - **Tests are excluded by default** (`--include-tests` to keep them). Two
    assertions in sequence share a shape however different the assertions are, so
    a suite is a field of true positives that drowns the duplication worth
    finding — measured on a real repo, *every* row it returned was test
    boilerplate.
  - **`pair`** on each row names the directories its group spans, the same value
    across the group — so a monorepo's deliberate vendoring (`core <-> laravel`)
    is one thing to dismiss with `--fields pair,name`, not one per row.
  - **When `clones` is empty, try `astgrep`.** Two implementations of one
    algorithm that differ only in their accessor expressions have different
    trees, so `clones` cannot see them — but a structural pattern can:
    `astgrep '$A.emitted and $B > 1'` found a duplicated confirm-window
    implementation that `clones` returned nothing for, because the shape is
    stable where the naming is not.
  - **Know what it cannot see.** It finds copy-paste that was never edited
    afterwards. Two implementations of one algorithm over different data
    representations — same names, same argument order, same call structure,
    differing only in the accessor expressions — have *different* trees and will
    not be reported. On a real refactor it found none of five such pairs. That is
    the price of having no similarity threshold to tune or distrust; read an empty
    result as "no exact copies", not "no duplication".
- **Find dead code:** `codeq unused [<path>]` — definitions with no reference
  anywhere outside their own definition site. A mention in a **comment** is not a
  use (a stale TODO can't mask dead code), but a mention in a **string** still is,
  because reflection, serde renames, config keys and SQL identifiers are real uses
  that live in string literals. Still name-based, so a same-named symbol elsewhere
  masks it; it under-reports rather than mislead. Shows `visibility` (a `public` hit may
  be external API, not dead); tests excluded unless `--include-tests` — recognised
  the same way `codeq tests` does, so a Rust `#[cfg(test)] mod tests` unit test is
  not reported as dead code.
  - **A row with `decorator=…` is probably not dead.** A framework entry point —
    `@app.get("/x")`, `@click.command()`, a plugin `@register` — is referenced by
    the decorator above it and by nothing else, so it always looked dead. It is
    now tagged rather than suppressed: suppressing anything decorated would hide
    real dead code, which is the one thing `unused` must not do. Inert decorators
    (`@property`, `@staticmethod`, `#[derive]`, `#[inline]`) are not tagged, so
    the tag means something when it appears.
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
  - **And grep's calling convention:** `codeq search TODO src/a.rs docs` — paths
    positionally, after the pattern. Each is a path *substring* (same rule as
    `--path`), so a directory scopes everything under it and a bare filename finds
    that file wherever it lives; several are OR'd, which is what a shell glob
    expands to. Composes with `--path`/`-g` by AND.
  - **Piped input:** `CMD | codeq search PAT --stdin` scans the pipe instead of the
    index, so codeq replaces grep in a pipeline too (no repo or index needed). Add
    `--language rust` to also get `--in` and the enclosing `[symbol]` on each match.
    `--stdin` is required — it is never inferred, so an open pipe can't hijack a
    normal query.
  - `--path` / `--language` / `--limit` are on `references` and most list commands
    as well; narrow with them instead of post-filtering the output.
- **Structural (AST) search:** `codeq astgrep '$X.unwrap()' --language rust` — match
  syntax, not text. `$VAR`/`$_` = one node (repeated `$VAR` binds the same);
  `$$$` = any run of siblings, so `foo($$$)` matches any arg count. For "all calls
  to X", "all `Ok($X)` returns", etc. Each match is annotated with its enclosing
  `[symbol]`. Limits: it's structurally exact (include optional syntax the code
  has) and can't see inside macro invocations.
  - `--path` scopes it (exact file, directory, or bare substring); a `--path`
    matching no indexed file of that language says so on stderr rather than
    returning an empty success.
  - **Don't guess the node kinds — ask.** `codeq astgrep --explain <file>:<line>`
    prints the parse tree covering that line: every node's `kind`, the `field` it
    fills in its parent, and its source. Write the pattern from that. Without it a
    wrong kind returns an empty result that reads exactly like "no matches", so
    run `--explain` on a line you *know* should match before concluding a pattern
    found nothing. Takes no `--language` (the file already knows) and no pattern.
    Named nodes only — punctuation is omitted, since `$$$` covers it.
- **Module-dependency graph:** `codeq deps <file> [--depth N]` — the internal
  files a file depends on, resolved from its imports (each edge shows the raw
  import string); `--depth N` walks transitively. `codeq deps --cycles` lists
  every import cycle in the repo. Go imports under the module declared in `go.mod`
  resolve to their package directory exactly (but `replace` directives are not read).
  Other resolution is heuristic (import segments matched to file stems); named imports are captured qualified — Python `from pkg import
  x` → `pkg.x`, JS/TS `import {a} from './x'` → `./x.a` — so the imported symbol
  resolves like a direct module import; relative (`from . import x`) and wildcard
  forms stay coarse. Verify via the edge's module string.
- **What a type holds:** `codeq members Config` — its fields, enum variants *and*
  methods, **grouped by kind** (methods, then fields), each group in line order.
  So the line numbers restart partway down; that is the grouping, not a sort bug.
  Keep `kind` in `--fields` if you project the output, or the break is invisible.
  Resolved by parse-tree containment. Not `symbols --owner Config`: that
  filters on the stored `owner` tag, which only fields and variants carry, so it
  silently answers the same question minus every method. `--owner` is for browsing
  fields (`--owner X --kind field`), not for "what does this type have".
  - **One exception to containment, for members declared outside their type:** a
    SQL `ALTER TABLE … ADD COLUMN` in a later migration is a column of that table
    even though the `CREATE TABLE` doesn't span it. Those are found via the
    `owner` tag — but *only* when the type name has a single definition in the
    repo, because the tag is a name and two same-named types would otherwise
    trade members. Two `CREATE TABLE jobs` and the later column is left out.
- **Where are the rows?** Under `--json` the envelope carries **`rows`** — the
  dotted path to the row array (`"data"`, `"data.matches"`, `"data.files"`). Row
  *keys* are uniform (below), but the container is not and cannot be: `search -l`
  returns `files` where plain `search` returns `matches`, and those are different
  facts. Read `rows` instead of memorising which commands nest. Absent when a
  result holds several lists (`about`, `map`) — there is no single answer there.
- **Trim tokens on list output:** append `--fields a,b`. Row keys are the same on
  every command: **`name`** is the definition a row *is*, **`symbol`** is the
  definition it sits *inside* (`search`/`references`/`astgrep` matches only), and
  position is **`line`/`col`/`byte`** with **`end_line`/`end_col`/`end_byte`** for
  the extent. So `--fields path,line` works everywhere — there is no `start_line`.
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

- `no symbol named 'ScanOpt'; closest: ScanOpts` — a typo, with the fix. Only
  when the name occurs *nowhere*: a name that is referenced but declared outside
  the repo (`Path`, `HTTPException`) says so instead, because reading "no symbol
  named 'Path'" as "that import is gone" is a mistake that has actually been made.
- `'X' is defined at a.rs:2 but has no callers` — the symbol is real, the
  relation is genuinely empty.
- `'X' is defined at a.rs:2 but nothing matches --path zzz` — your filter, not
  the code.
- `no public definitions in 'src/search.rs' (34 private, 40 restricted)` — the
  defs exist, none are public (`pub(crate)` is restricted, not public).
- `no unused definitions ...` — a clean result, not a failed one.

- `no indexed file matches 'zzz' — the filter selected nothing` vs `74 indexed
  file(s) match 'bench/results', but none has any import` — a scope that selected
  no file and a scope that genuinely holds none of the thing are *different
  answers*, and every path-scoped command now distinguishes them.
- `no symbol named zzznotreal — 1 of 2 seeds contributed nothing` (`context`) —
  a bad seed beside good ones is the dangerous case: the bundle looks full and
  the name you got wrong is the one missing from it.
- `pattern ran over 15 rust file(s) and matched nothing` (`astgrep`) — the
  pattern parsed and ran, so this is a real miss rather than a wrong node kind;
  `--explain` tells the two apart when you're unsure.

A **bare pattern miss stays silent on purpose**: an empty `search zzz` is
unambiguous, and a note on every one of those would be the noise these exist to
avoid. `symbols`/`files`/`importers` are likewise quiet — "nothing matched that
substring" needs no gloss.

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
  - **`definitions` is exact and case-sensitive; `symbols` is substring and
    case-insensitive.** That difference decides which to reach for in a
    polyglot repo, where one concept is spelled per language: Go `GetIPAddress`,
    JS `getIPAddress`, Python `get_ip_address`. `symbols ipaddress` finds all of
    them in one call; `definitions` needs the exact spelling each time (it does
    suggest `closest:` on a near miss, so a wrong case is a hint, not a dead
    end). Use `definitions` when you know the name and want to jump to it,
    `symbols` when you are looking for a *concept* across languages.
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

`differential.py` is the important one: ~7,000 assertions over 25 pinned
open-source repos, checking that `search` is a superset of ripgrep and that
per-symbol invariants hold in every supported language. Symbols are sampled
**stratified by kind**, so a change to how fields, constants or SQL declarations
are extracted is actually exercised — before that the sampler only looked at
functions, methods, classes and structs, and a day of extraction work went
unchecked by it. It writes a result to
`bench/results/` (committed, so the trend lives in git). Expect
`N PASS, 0 FAIL, 0 ERROR`.

Run these whenever a change touches extraction, the search engine, the renderer,
or the schema — which is most changes. Skipping them is how six commits once went
in unverified: everything passed in the end, but nobody knew that at the time.

- Token/benchmark numbers: `bench/bench.py` (single commands), `bench/tasks.py`
  (task-level), `bench/completion.py` (the token-to-completion table in the
  README — rerun it if a change moves those numbers, since they are published as
  current).
