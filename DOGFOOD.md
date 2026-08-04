# Dogfooding log

Honest friction, limitations, and failures found while using codeq to develop
codeq. New entries at the top. The point is candour — a frustration recorded
here is a feature request or a bug report.

## 2026-07-14 — session 19: tests-linkage (`codeq tests NAME`)

The 3rd capability wish: "which tests exercise X". New `codeq tests <name>` reuses
the `references` machinery (word-boundary matches carry their enclosing symbol),
keeps the hits whose enclosing function is a test — by test-file path
(`tests/`, `*_test.*`, `*.spec.*`, `src/test/`, …) or a test-like name
(`test_*`, `Test*`, `*Test`) — and reports the unique test defs. For the
edit→find-the-test→run-it loop. Wired into capabilities + MCP (`codeq_tests`).
- Works well where tests call the symbol by name: flask `Blueprint` → 54 tests,
  the `field_set` helper → 36. Name-based like references, so codeq's *own* tests
  (which drive the CLI via `f.run([...])` rather than referencing internal fn
  names) return little for internal symbols — expected, and honest about the
  heuristic. Can't see attribute-only tests in non-test paths (`#[cfg(test)]`).

## 2026-07-14 — session 18: built structural (AST) search + registered the MCP server

Acted on the top capability wish from the "what do you wish codeq had" reflection:
structural search. New `codeq astgrep '<pattern>' --language <lang>` — an ast-grep
-style matcher: the pattern (with `$NAME`/`$_` metavariables) is parsed with the
language's own grammar and matched on the syntax tree, so `$X.unwrap()` finds every
`.unwrap()` call whatever the receiver, `$X == $X` finds only self-comparisons
(metavariables bind consistently), and `return Ok($X)` stays anchored to `return`.
Deterministic, all 12 languages (reuses the tree-sitter grammars). 5 tests; wired
into `capabilities` and the MCP server (`codeq_astgrep`).
- **`$$$` variadic metavariables** (follow-up): `foo($$$)` matches any argument
  count, `fn $N($$$)` any parameters — a run of zero-or-more siblings in a list
  position. Also detects a variadic wrapped in a statement node. Reliable in
  argument/parameter lists; block/statement-body variadics are grammar-sensitive
  (Rust tail-expr works; Python one-line-vs-indented body differs) — documented.
- **Enclosing-`[symbol]` annotation** (follow-up): each astgrep match now shows the
  definition it sits inside, like `search`.
- **Honest limitations (documented):** (1) it can't match inside macro invocations
  — tree-sitter parses macro bodies as opaque token-trees, so `.unwrap()` inside
  `assert_eq!(…)` isn't an AST call node (this explained a 92-vs-132 count gap vs rg,
  and is inherent to all AST tools); (2) matching is structurally exact, so a
  pattern must include optional syntax the code has (a no-return-type `fn` pattern
  won't match a fn *with* one).
- **Process win — codeq is now a native tool for future sessions.** The recurring
  reason I kept slipping back to grep/Read: codeq was invoked via **Bash**
  (second-class), while Read/Grep are native tools with reflex-level gravity. Added
  `.mcp.json` registering `codeq mcp`, so next session `codeq_search`/`codeq_read`/…
  are first-class tools that compete on equal footing — and win for navigation
  because they're better. This was flagged back in session 1; finally done.

## Process note — the "index-timing ghost" was NOT a codeq bug (false alarm)

For a stretch, dogfood scripts intermittently saw an empty `--json` result on the
first query after a fresh index, and it looked like a cold-query race. It wasn't
codeq at all: the scripts set `CQ=./target/release/codeq` (a **relative** path)
then `cd`'d into a `/tmp` fixture dir, so the shell couldn't find the binary and
exited 127 — and the commands' `2>/dev/null` swallowed the "no such file" error,
leaving only empty stdout. Deterministic, not a race; "worked on retry" only
because retries used the absolute path. Verified: with an absolute path, cold
first-query never fails (20/20, 8/8 large-repo, 30/30 fresh-repo, 3/3
build+query). **Lessons: don't blanket-`2>/dev/null` while dogfooding — stderr is
the diagnosis; use absolute binary paths in scripts that `cd`.**

## 2026-07-14 — session 16: writing tests surfaced a JS/TS `require()` import gap

While adding extraction-depth unit tests, a fixture check showed `imports a.js`
captured the ES `import` but *not* `const p = require("path")`. CommonJS
`require()` is a huge share of real JS/TS imports, so `imports`/`importers` were
incomplete for those languages.

- **FIXED: JS/TS/TSX `require("x")` now captured as an import.** Same shape as the
  earlier Ruby `require` fix — it's a call, not a syntactic import, so the shared
  `JS_QUERY` now matches `(call_expression function: (identifier) @guard
  arguments: (arguments . (string ...)))` and the existing `@guard` keyword filter
  (`require`/…) accepts it. Verified on express: 16 static `require()` imports
  captured from one file (the remaining `require(expr)` dynamic forms have no
  static module string, correctly skipped); `console.log(...)` is not an import.
  Fixes JS, TS, and TSX in one query (they share JS_QUERY). Regression test added.
- Meta-note: this is the *fourth* finding of the "a construct that's a plain call,
  not a keyword, is missed" class (Ruby require, now JS require). Ran the audit
  across all languages (below).

**Call-style-import audit (all 12 languages):**

- **FIXED: PHP `require`/`require_once`/`include`/`include_once`** were missed —
  only `use` (namespace) was captured. These are PHP's file-inclusion mechanism,
  used throughout legacy code and entry points. Added patterns for all four (they
  are dedicated node types in tree-sitter-php), capturing the static string path
  in *both* quote styles — single-quoted `(string)` and double-quoted
  `(encapsed_string)` (the double-quoted form was a second, easy-to-miss gap).
  Dynamic paths (`require_once __DIR__ . '/x.php'`) are correctly skipped — no
  static module. Regression test added.
- **Complete (no gap): Java** (`import` + `import static`), **C#** (`using`),
  **Go** (`import ( … )` blocks), **C/C++** (`#include`) — all keyword/preprocessor
  based and fully captured.
- **Skipped (rare/dynamic): Python `importlib.import_module`/`__import__`,
  Ruby `autoload` (symbol-first arg, so the string isn't the first child).** These
  are uncommon and genuinely dynamic; capturing them risks noise for little gain.
  Documented rather than chased.

## 2026-07-14 — session 15: git-free freshness missed `.gitignore` edits (staleness)

With the text renderer now guarded by `bench/textmode.py`, moved to correctness
edge cases of the git-free freshness cache. Stress-testing ignore-rule changes
found a real staleness bug.

- **REAL BUG, FIXED: editing `.gitignore` didn't invalidate freshness.** Adding
  `build/` to `.gitignore` should drop `build/gen.rs` from the index, but its
  symbols stayed searchable. Cause: `.gitignore`/`.ignore` are *hidden* files —
  never indexed (so not in the file manifest) and never walked — and an in-place
  edit bumps no *directory* mtime, so neither half of the freshness cache noticed.
  Ignore-rule changes were silently missed until some unrelated change forced a
  walk. Fixed: the walk now records the mtime of each directory's
  `.gitignore`/`.ignore` alongside the dir mtimes (generalised `dir_mtime` →
  `path_mtime`), so an ignore edit invalidates freshness and re-evaluates. Verified
  ignore→drop and un-ignore→restore; regression test added.
  - Known residual: changes to `.git/info/exclude` or the global excludesFile
    aren't auto-detected (they're outside the tree) — `index --rebuild` picks them
    up. Acceptable; git's own fsmonitor has similar caveats.
- **Validated (no bug): rename / touch / edit-revert** all handled correctly (from
  the prior round). The dir-mtime + manifest cache is robust once ignore files are
  tracked.
- **Validated (no bug): weird files.** Empty, binary (null bytes), invalid-UTF-8,
  a 200 KB single line, and a symlink — index without crashing; binary files are
  excluded from search; the symlink is skipped (ignore-crate default).

- **FIXED (context UX/correctness): a starved budget dropped the requested seed.**
  `context --symbols refresh --max-tokens 10` returned **zero** chunks — the very
  symbol you asked for, silently gone, because the seed exceeded the budget. Worse,
  with `--expand` a tiny budget would return the seed's *neighbours* but not the
  seed itself. Fixed so explicitly-requested seeds are always included; the budget
  caps only automatic `--expand` growth. An agent asking for a big function with a
  modest budget now gets the function. Test updated to the new contract.

## 2026-07-13 — session 14: dogfooding caught `index --rebuild` leaving an empty index

Prompted to actually dogfood (I'd drifted to grep/rg/sed), used `codeq search
'fresh_token|freshness_token'` to check the git-free change left no dangling
refs. The text output's `[enclosing-symbol]` annotation pointed straight at a
`DELETE FROM meta WHERE key='fresh_token'` inside `Session::open`'s `--rebuild`
path — `codeq read --lines` then showed the whole block.

- **REAL BUG, FIXED: `codeq index --rebuild` produced an *empty* index.** The
  rebuild wiped `files`/`content_fts` but not the new `dirs` cache; the next
  refresh's `is_fresh` then saw an unchanged `dirs` cache + an empty `files`
  table, judged the index fresh, and skipped the walk. Verified: 355 symbols → 0
  after `--rebuild`. Same latent bug in `migrate()`'s schema-version wipe. Fixed
  both to clear `dirs` whenever they wipe `files`; regression test added
  (`rebuild_repopulates_the_index`).
- **Why the harness missed it:** `differential.py`/`completion.py` force a clean
  index with `rm -rf .codeq`, never exercising `--rebuild`. A whole command path
  was untested — only *using* the tool (and reading its own output) surfaced it.
  The `[enclosing-symbol]` annotation restored earlier this day is what made the
  stray statement legible at a glance.

Kept dogfooding the *text output* (the harness only exercises `--json`), which
found two more:

- **FIXED: `codeq map` dropped its counts in text mode.** The generic text
  renderer only understood path/line/name-style fields, so map's rows
  (`definitions_by_file`, `directories`, `imports_by_module`, `languages`)
  rendered with empty payloads — `src/commands.rs: ` with the count gone. Added a
  `key=value` fallback for unrecognised scalar fields; tabular commands stay lean.
- **FIXED: `codeq capabilities` ignored the text default and always dumped JSON.**
  It's session-less, so it hand-rolled its envelope and printed JSON directly,
  never touching the text renderer. Exposed `envelope::render_data_text` and made
  it honor `--json`/`--meta`/`--pretty` like everything else.
- Checked (no bug): empty results print nothing at exit 0; `read`/`about` of a
  missing symbol/file error to stderr with a non-zero exit. Error/empty paths are
  sound.

Kept going — two more from the text layer, plus a freshness stress-test:

- **FIXED: `search -A/-B/-C` context lines were silently dropped in text mode.**
  The renderer only printed the match line, so the context flags (present in the
  JSON as `before`/`after`) were a no-op in the default output. Now rendered
  grep-style (`path-line- content`) with computed line numbers. Regression test
  added.
- **IMPROVED: `context --expand` text now labels chunks `(seed)`/`(referenced)`**
  — the whole point of `--expand` (which chunk you asked for vs its neighbours)
  was invisible in text mode. Surfaced the `reason` in the chunk header.
- **Validated (no bug): git-free freshness handles rename / touch / edit-revert.**
  Renaming `a.rs`→`renamed.rs` moves the symbol and drops the stale path; a bare
  `touch` (mtime bump, same content) doesn't spuriously reindex; edit-then-revert
  restores cleanly. The dir-mtime + file-manifest cache is robust across the hard
  cases, not just add/delete.

Tally: five text/command-path fixes this day, none of which the `--json`-only
harness could reach. Standing lesson — a new command or output mode needs a
dogfood pass; the harness guards correctness-at-scale, not the surface an agent
actually reads.

## 2026-07-13 — session 13: git-free freshness via codeq's own index cache

Profiling showed ~58% of every command's latency was the git freshness check
(`rev-parse HEAD` + `git status --porcelain`, ~5.5ms, two process spawns). First
attempt — just remove git and always walk — regressed *badly* (8→218ms on redis):
the git "fast-path" existed for a reason, because codeq's native `ignore`-crate
walk with .gitignore matching is slow, and `git status` is fast thanks to git's
index cache. The honest fix was to **maintain our own index cache** (git's actual
trick):

- codeq already stored a per-file `(size, mtime, hash)` manifest. Added a per-
  **directory** mtime cache (`dirs` table, schema v6) — git's untracked-cache
  idea. Freshness is now native: a file added/removed/renamed bumps its parent
  dir's mtime (caught by the dir cache); a content edit changes the file's mtime
  (caught by the manifest). If both are unchanged → fresh, no walk, no gitignore,
  no git. Only a real change falls back to the full walk.
- Result: **git removed from the freshness hot path**; ~8.5→7.7ms on redis (no
  regression, and no `git` binary needed). Correctness verified — edit / add /
  delete / add-in-new-subdir all detected (3 new unit tests), 630/630 differential
  assertions PASS. Git remains only for the inherently-git `changed` command and
  optional `--meta` HEAD/dirty.
- Lesson: the naive "remove the dependency" regressed 27x; the dependency was
  hiding a real optimization. Reimplementing that optimization natively (the
  dir-mtime cache) was the right move — measure before ripping things out.

## 2026-07-13 — session 12: token-to-completion showed JSON was tax → text default

Built `bench/completion.py` (task-level token-to-completion: tokens + tool calls
to finish realistic tasks, codeq vs chained rg/sed/cat). It showed codeq *losing*
to rg on `references` (0.7x) and `imports` (0.5x) — the JSON envelope is pure
token tax where the answer is essentially `path:line: text`. Also measured that
compact text is 2–6x smaller than JSON per command (definitions 5.9x, symbols
2.7x).

- **CHANGE: default output is now compact grep-style text; `--json` is opt-in.**
  One central renderer in `envelope.rs` (`path:line:col: payload` — `signature`
  for defs, `kind name` for symbols, sectioned for `about`/`map`, `key: value`
  for `stats`), so zero per-command churn. `--meta`/`--pretty` imply `--json`;
  noteworthy meta goes to stderr so stdout stays parseable. MCP passes `--json`
  (structured, as programmatic consumers expect). Tests kept green by injecting
  `--json` in the harness; added a text-default test. Payoff (completion, 25
  repos): total token saving **3.1x → 6.2x**, and the two losing tasks flipped to
  wins (references 0.7→1.1x, imports 0.5→1.0x). Differential still 5224/5224.
- **Also found (harness scaling): `symbols` default-caps at 200 and returns
  alphabetically**, so samplers that forgot `--limit 0` saw a biased slice — it
  silently skipped 5 repos in completion until fixed. Same class as search/
  references capping at 100.
- **Resolved earlier this session: Ruby `require`/`require_relative` now captured
  as imports** (guarded call match + Rust-side keyword filter, since the query
  engine doesn't evaluate `#eq?`). sinatra 0→85, rack 0→122.

## 2026-07-13 — session 11: differential harness at scale → found the UTF-8 BOM bug

Built out the multi-repo differential harness into a verification suite: 25 real
repos (one+ per language), ~200 high-quality assertions each (~5,000/run) across
SEARCH (superset vs rg), FUZZ (60 real identifiers vs `rg -F`), INVARIANT (~6
per-symbol checks of definitions/references/at/read vs ground truth), and NAV
(benefit vs the grep/cat pipeline), plus a per-command report of correctness +
speed/token ratios so it's obvious where to improve.

- **REAL BUG, FIXED (schema v5): index kept a leading UTF-8 BOM.** The first big
  run flagged `newtonsoft/anchor-hash` (`^#`) missing **549** lines rg found — all
  on line 1 of BOM-prefixed C# files (`EF BB BF` before `#region`/`#nullable`).
  The BOM sat before the first char, so `^#` couldn't match line 1 and byte
  offsets were shifted. Fixed by stripping a leading BOM at index time (and in
  `read`), like ripgrep/editors do; schema bumped 4→5 so stale indexes rebuild.
  Invisible to the unit suite; only a BOM-heavy real repo surfaced it.
- **Harness-quality lesson: triage every FAIL.** The expanded run threw 15 FAILs;
  all 15 were *harness-assertion* bugs, not codeq bugs — `ref-includes-def` was
  too strict about annotation/attribute `start_line`s (fixed to span membership),
  and `read` sampled a vendored *minified* JS blob (now excluded via a VENDORED
  path filter). Also learned: `references` (like `search`) default-caps at 100, so
  invariants must pass `--limit 0`. Distinguishing harness bugs from real bugs is
  the discipline that keeps a 5,000-assertion suite trustworthy.
- **Where-to-improve signal, quantified:** `read --symbol` is ~1% of `cat`'s bytes
  (the ~80x token win) but ~10x slower than `cat`; `definitions`/`imports`/
  `outline` emit several× the bytes of the equivalent grep heuristic. These are
  now tracked per-command, per-run, over time.

## 2026-07-13 — session 10: multi-language extraction sweep → Ruby/C++ gaps

Differentially tested symbol/import extraction across all 12 languages with
fixtures (Python, JS, TS, Go, Java, Ruby, C, C++, C#, PHP). Most are correct
(free functions, classes, methods, interfaces, namespaces, structs, and imports
all extracted as expected). Two real gaps and one re-confirmed limitation:

- **RESOLVED: Ruby imports were entirely missing.** `require`/`require_relative`
  yielded zero import facts, so `imports`/`importers` were dead for every Ruby
  file. `require "x"` parses as a generic `(call)` and the extractor doesn't
  evaluate tree-sitter predicates, so the query now captures the call and its
  method identifier as `@guard`, and `extract` filters to the recognised keywords
  (`require`/`require_relative`/`load`/`autoload`) in Rust — `puts "x"` etc. are
  not imports. sinatra 0→85, rack 0→122 imports; regression test added.
- **RESOLVED (session 17): C++ pure method *declarations* now captured.** `void
  m();` inside a class was yielding no symbol (only definitions/inline methods
  were), so a header-only class exposed none of its API. The query matched only
  `function_definition` (which requires a body); added patterns for
  `field_declaration`/`declaration` with a `function_declarator` — class-member
  prototypes (methods, constructors) and free-function prototypes. Verified on a
  header fixture (declared_only/compute/pure_virtual/free_prototype now found, no
  double-count of inline methods) and on real repos (fmt/json/spdlog: 617/617
  differential assertions still PASS). Minor known residual: an all-caps macro
  invocation at declaration scope (e.g. `FMT_ENUM_UNDERLYING_TYPE(...)`) can be
  mislabeled a function — tree-sitter can't expand macros; ~3% of a header, and
  the alternative (dropping declarations) loses far more. `typedef struct {…}
  Named;` anonymous-struct-typedef is still not captured (separate, minor).
- **Re-confirmed (`--path` substring): `symbols --path s.c` returned symbols from
  `s.cpp` and `S.cs`** — `s.c` is a substring of both (and matching is
  case-insensitive). Documented behaviour, but for a query that looks like a full
  filename it's surprising; an exact-path mode (or `--file`) would help.

These are natural test cases for the multi-repo differential harness (below).

## 2026-07-13 — session 9: hardened differential vs rg → no bug, `search` validated

Widened the ripgrep differential to ~20 patterns plus the separate output modes
(`-l`, `-c`) and the `--in` partition. **No new bug** — after sessions 4 and 8,
`search` now matches rg exactly at line granularity across literals, anchors,
`\b`, alternation with a shared required literal (`(Db|Repo)::new`), char
classes, optional/repetition, unicode (`→`, `—`), `-i`, short sub-3-char
literals (full-scan path), `-l` (set-equal), and `-c` (per-file counts equal).

One **false alarm worth recording**: a `--in` cross-check summed by *column* gave
code+comments+strings = 239 vs unfiltered 237. Chased it to two lines each
containing the word `other` twice (which embeds "the"), with the two hits in
different syntactic classes. Root cause: `search` emits **one row per matching
line** (grep default), and the `--in` accept() filter runs *before* that
per-line dedup — so unfiltered reports a line at its first hit's column, while
`--in strings` reports the same line at the first *string* hit's column. At line
granularity everything partitions exactly (any == code∪comments∪strings == rg,
237 = 237). Not a bug; the column is legitimately filter-dependent. Documented
the subtlety and added a regression test locking the accept-before-dedup order.

Takeaway: differential testing also produces valuable *negative* results — a
clean diff after a fix is evidence the fix holds, and a surprising diff is worth
chasing even when it turns out correct (it surfaced a real, subtle, previously
untested behaviour). Worth promoting to a standing `bench/` rg-diff harness.

## 2026-07-13 — session 8: differential test vs ripgrep → `^`/`$` anchors were broken

Put the flagship claim ("`search` is a strict superset of grep, never reach for
rg") under a differential test: ran ~15 patterns through both `codeq search
--limit 0` and `rg`, comparing match counts. Most matched exactly (literals,
`\b`, alternation, char classes, `-i`, escaped metachars). Two diverged.

- **CRITICAL, FIXED: `^` and `$` anchors matched the whole file buffer, not each
  line.** `codeq search '^use'` returned **0** while `rg '^use'` found 52 — the
  regex was built without multiline mode, so `^`/`$` anchored to the start/end of
  the entire file body (which `run_search` scans in one pass), not line
  boundaries. Every line-anchored search — `^import`, `^def `, `^func `,
  `^pub fn`, `^class ` — silently returned nothing. That's a severe false
  negative for one of the most common code-search idioms, and it directly breaks
  the "superset of grep" promise. **Fixed:** build the pattern with
  `.multi_line(true)`. After the fix, `^use`/`^pub fn`/`^\s*fn `/`^#`/`Result>$`
  all match rg **exactly**. Regression test added (`^use`, `x;$`).
- **Known minor divergence (safe direction): `^$` over-counts empty lines by
  exactly one per file** (1147 vs rg's 1124; diff == the 23 scanned files).
  codeq counts the zero-width empty "line" after a file's trailing `\n`, which
  grep omits. It's an *over*-count, so the strict-superset property still holds
  (codeq finds every match rg does, plus these), and it only affects degenerate
  empty-line patterns — real content patterns matched rg exactly. Documented
  rather than special-cased, to keep the hot match loops simple.
- Technique: **differential testing against the tool we claim to replace.** The
  first four sessions found bugs by *using* codeq; this one found a worse bug by
  *comparing* codeq to its baseline across many inputs. Both belong in the kit —
  differential vs rg should be a standing check, ideally a bench/ harness.

## 2026-07-13 — session 7: `at`/`outline` are sound — and caught my own stale docs

Stress-tested the location commands. `at <file>:<line>` correctly returns the
enclosing definition (method interior → the method; struct field line → the
struct; blank gap / import region → `[]`). `outline` is correct too, and its
`depth` field is real (0 for flat defs, 1 for Python class methods and nested
Rust fns — verified it reaches 1 in bench/*.py, commands.rs, mcp.rs). No bug in
either command.

The find this round was **doc drift caught by using codeq to read the code**:

- **FIXED (stale doc): `cmd_importers`'s doc comment still said it "deliberately
  does not resolve a file path to the module spelling."** That was true until
  session 6, when I made it do exactly that — but I updated the CLI help and
  missed the doc comment. `at src/commands.rs:930` printed the stale doc back at
  me. Corrected.
- **FIXED (stale doc): the `content_fts` schema comment in db.rs still described
  querying by trigram `MATCH`.** Leftover from session 4, which replaced `MATCH`
  with `LIKE` precisely because MATCH drops matches. Found via `search 'MATCH'
  --in comments`. Corrected to describe the LIKE prefilter.
- Lesson: rapid fix-commits leave doc/comment drift that the test suite can't
  see. `at` (prints a def's doc) and `search --in comments` are effective drift
  detectors — worth a sweep after a run of behavioural changes. Both stale docs
  were introduced by *my own* prior sessions; codeq caught them.

## 2026-07-13 — session 6: module-graph task → `importers <file>` silently returned nothing

Task: understand codeq's own module structure — "what imports `db`?" `imports
src/commands.rs` worked (lists module strings). But `importers src/db.rs` — the
natural mirror of `imports <file>` — returned `[]`.

- **FIXED (false negative): `importers` matched the query as a substring of
  import *module strings*, which never contain the file path.** Rust imports are
  `crate::db::Db`, JS is `./db`; none contain `src/db.rs`, so a path query
  matched nothing and looked like "db.rs is imported by no one" — a wrong,
  trust-eroding answer (an agent would treat db.rs as dead). The two sibling
  commands were also asymmetric: `imports` takes a *file*, `importers` wanted a
  *module token*. **Fixed:** a path-shaped query (contains a separator) is now
  reduced to its module stem, so `importers src/db.rs` → `%db%` and finds all 3
  importers. Plain module tokens (`db`, `db::Store`) and dotted modules
  (`foo.bar`) are unchanged — the separator check avoids touching them. Still
  purely textual / a safe superset. Help text corrected (it wrongly promised
  "path substring"). Regression test asserts the path form resolves.
- Same failure *class* as sessions 4 and 5: a silent false result surfaced by
  actually running the workflow. Here it was reaching for the obvious symmetric
  call (`imports <file>` ⇒ `importers <file>`) and getting an empty, misleading
  answer instead of an error or a result.

## 2026-07-13 — session 5: reviewing my own diff → `context --changed` was too coarse

Used codeq to review what my recent commit touched: `changed --symbols
--compare-to HEAD~1` correctly listed the 12 functions I edited (hunk∩def
ranges — spot-on). But cross-checking against `context --changed` exposed an
inconsistency.

- **FIXED (relevance/tokens): `context --changed` seeded every def in a changed
  *file*, not the defs the change *touched*.** For the same HEAD~1 diff it
  seeded **86** defs (≈ all of commands.rs's 89) when only **12** were actually
  changed. That's the opposite of what `context` is for — it bloats the window
  with untouched code and dilutes relevance. `changed --symbols` already had the
  right logic (`changed_line_ranges` hunk overlap); `context`'s changed-mode was
  still using `changed_since` (file granularity). **Fixed:** changed-mode now
  seeds via hunk∩def overlap, matching `changed --symbols`. Result: 86 → 13
  seeds, exactly equal to the ground-truth touched-def count. `--expand` still
  pulls in referenced neighbours, so you get *touched defs + their dependencies*
  — the right context for reviewing or modifying a change. Regression test
  strengthened to assert an untouched def in a changed file is not seeded.
- Note: this was found purely by *using* the review workflow (diff → symbols →
  context) and noticing the two commands disagreed on scope. The tool was never
  wrong per se, just wasteful — the kind of thing a test suite won't flag but a
  real "gather context for my change" task makes obvious.

## 2026-07-13 — session 4: MCP-coverage audit → found a CRITICAL search bug

Set out to audit whether every command has an MCP tool. The audit query
(`search 'codeq_' --in strings`) returned nothing, which surfaced a serious bug.

- **CRITICAL, FIXED: `search`/`references`/`callers` dropped matches
  (false negatives).** The content prefilter used FTS5 trigram `MATCH`, which
  silently fails for many substrings: `search 'ode'` returned 0 though `decode`
  contains it; raw `MATCH "ode"` → 0 while `LIKE '%ode%'` → 20 (ground truth).
  Even worse, `MATCH "codeq_"` matched *fewer* docs than `MATCH "codeq_map"` —
  impossible for correct substring search. Trigram `MATCH` is just unreliable
  for arbitrary substrings; the tokenizer's intended substring interface is
  `LIKE`/`GLOB`. **Fixed:** the prefilter now emits trigram-accelerated `LIKE
  '%literal%'` clauses (OR-ed for regex literal sets) instead of `MATCH`. Still
  a safe superset (`_`/`%` act as wildcards → broader; the regex confirms).
  Regression test added; `search ode/cod/codeq_` now match ground truth. This is
  the most important dogfood find — a correctness bug that erodes trust in the
  whole tool, invisible to the test suite until real use hit it.

- **Minor, FIXED: `stats` had no MCP tool.** The audit's actual answer: 17 of 18
  query commands were exposed; `stats` was missing. Added `codeq_stats`.

- **Usage insight: `--in` classifies by the match's START position.** A pattern
  like `tool("codeq_` starts in code and overlaps a string, so `--in strings`
  excludes it. To find string-embedded content, the pattern must *begin* inside
  the string (`search 'codeq_' --in strings`). Non-obvious; reader beware.

## 2026-07-13 — session 3: perf-smell audit (via `search` + `read`)

Audited for other full-scan queries like the callees_of one, using
`codeq search 'query_map\(\[\]'` and `codeq search 'FROM facts' --in strings`.
The tooling shone: `search` returned every no-param query **with its enclosing
function** (grep gives only file:line), and alternation regex + `--path`
prefiltering worked. Findings:

- **RESOLVED (perf): `context::load_defs` full-scanned all defs on every
  `context` call** — same pattern as the callees_of smell. Because
  `context::build` is index-based over the all-defs Vec, the fix was a proper
  two-phase restructure (done as a dedicated task, not rushed): seeds loaded via
  `defs_by_names`/`defs_by_paths` (facts name/path indexes), then expansion
  neighbours via `defs_by_names` over the referenced idents. `defs` now holds
  only seeds + neighbours, not the whole repo. All 6 context tests still green;
  `EXPLAIN QUERY PLAN` shows `SEARCH facts USING INDEX facts_name`.
- The other no-param scans (`cmd_stats`, `cmd_map`, `refresh`) are legitimate:
  they are whole-repo aggregations or the refresh file-comparison, which need
  every row. `fetch_bodies`'s no-MATCH scan is the intended regex fallback.

- **Usage insight (not a bug): `--in code` vs `--in strings` matters and can
  mislead.** codeq's SQL lives in string literals, so `search 'FROM facts'
  --in code` returns **0** and `--in strings` returns **22**. An agent auditing
  string-embedded content (SQL, config, templates) must pick `--in strings`;
  the wrong choice silently finds nothing. Added a note to CLAUDE.md.

## 2026-07-13 — session 2: investigating callees_of (found via `about`/`read`)

Used `codeq about callees_of --path commands` then `codeq read src/commands.rs
--symbol callees_of` to study the function without loading the 1900-line file —
that flow worked well (orient → read one symbol). Two findings:

- **RESOLVED (perf): `callees_of` full-scanned every fact.** It ran
  `SELECT … FROM facts WHERE kind<>'import' AND name IS NOT NULL` and filtered in
  Rust — O(all facts) per `about`/`callees` call, ignoring the `facts_name`
  index. Fixed: query `WHERE name IN (<referenced idents>)` in chunks;
  `EXPLAIN QUERY PLAN` now shows `SEARCH facts USING INDEX facts_name`.

- **Known limitation (noise): callees over-matches.** The callees list included
  `db`, `path`, `remove`, `new` — these are field accesses (`s.db`, `.path`),
  std method calls (`.remove`), and a name that collides with an unrelated def
  (`Fixture::remove` in tests). Name-based analysis can't tell `x.remove()`
  (std) from a call to a defined `remove`.
  - **PARTIALLY RESOLVED (session 17): callees is now call-aware.** `collect_idents`
    only collects a name when it's immediately followed by `(` — an actual call —
    so field accesses (`s.db`, `.path`), type references, and variable uses that
    merely share a name with a definition are dropped, while real calls
    (`foo()`, `obj.method()`, `Type::new()`) are kept. Strict improvement: no true
    callee is lost (real callees are always called). What *remains* is genuine
    same-name **call** collision (a std `.remove()` vs a local `remove`, `X::new`),
    which needs type resolution — still deliberately out of scope. Reader beware:
    callees is still a superset, just a much tighter one.

## 2026-07-13 — RESOLVED: symbol commands need path scoping (highest priority)

**Fixed** the same day: `definitions`/`callees`/`about` now take `--path <substr>`
to scope the target definition, and `callers` `--path` scopes caller locations.
`about build --path context` now shows only `context::build` and its callees;
`--path commands` shows the other one. First dogfood-driven fix — the loop works.

Original report:

`codeq about build` matched **two** `build` functions — `GlobFilter::build`
(src/commands.rs) and `context::build` (src/context.rs). `about`/`callers`/
`callees` **aggregate across all same-named symbols**, so the dossier merged two
unrelated functions (callees mixed `normalize`/`GlobFilter` from the wrong one).
For common names (`new`, `build`, `open`) these commands are actively
misleading. `definitions` correctly returns both, but the graph commands don't
disambiguate, and there's no way to scope them.

**Fix:** add `--path <substr>` (and maybe `--file`) to `definitions`, `callers`,
`callees`, `about`; when a name resolves to multiple definitions, either scope
or clearly separate the per-definition graphs. Also consider showing each
callee's path (a bare `new` × 4 is ambiguous).

## 2026-07-13 — read --symbol is a modest but real win

`codeq read src/context.rs --symbol build` returned 7.3k bytes vs 11.1k to read
the whole file (~34%). The win is small here because `build` is most of the
file; it grows when the target is a small part of a large file. Good enough that
reading a specific symbol beats ingesting the file.

## 2026-07-13 — invocation is via Bash, not a native tool

In this session codeq is called through Bash (`./target/debug/codeq …`), not as
a first-class tool, so its output arrives as shell stdout. Registering the MCP
server (`.mcp.json` → `codeq mcp`) would make `codeq_*` native tools in future
sessions — worth doing to make codeq the default reach.

## 2026-07-14 — multi-hop navigation: callers/callees --depth

Added transitive walking to `callers`/`callees` via `--depth N` (default 1 =
today's direct-only behavior). A BFS up (callers) or down (callees) the call
tree, each result tagged with its hop `depth`; text output indents by hop, so
`callers is_fresh --depth 3` reads as a reachability tree at a glance. Capped at
`HOP_CAP=2000` results so a hot symbol can't explode. Because the graph is
name-based (not type-resolved), a hop can over-reach on common names — scope with
`--path` when needed, same caveat as depth-1.

**Consistency bug caught by a test:** the shared `Hop` struct serialized the
callee name under `symbol`, but depth-1 callees use `name` — so `callees X` and
`callees X --depth 2` disagreed on field name. Split out a `CalleeHop` mirroring
`Callee` (`name` at every depth). Lesson: reusing one row type across two
commands leaked the wrong field name; the depth-1 shape is the contract the
deeper walk must match.

## 2026-07-14 — module-dependency graph: deps + deps --cycles

Second wishlist item. `deps <file> [--depth N]` resolves a file's raw imports to
internal files and walks the graph; `deps --cycles` finds every import cycle.
The hard part is resolution: imports are raw strings (`crate::db`, `foo.bar`,
`./x`), so `deps` matches an import's path *segments* against repo file stems,
scoring candidates by how many path components the import names — `foo.bar.util`
picks `foo/bar/util.py` over an unrelated `util.py`. No real module resolver, so
it's honest-but-fuzzy: unresolved imports are treated as external (no edge), and
genuine ambiguity is reported as multiple edges rather than a guess.

Dogfooded on this repo: `deps src/commands.rs` cleanly resolved all 7 `crate::`
imports to their files; `deps --cycles` correctly found none (the crate is a
DAG). Caught by writing a test: Python `from pkg import x` stores the module as
just `pkg` (the from-clause, not `pkg.x`), so with no `pkg.py` it can't resolve —
`import pkg.x` carries both segments and does. A real limitation of resolving
from the stored string; documented rather than papered over.

Also a text-rendering gap: a cycle is a *list* field (`cycle: [a, b]`), which the
generic row renderer dropped (it only prints scalar fields), leaving a bare
`length=2`. Added a dedicated branch so cycles render as `a -> b -> a` — the loop
closed back to its first member. Lesson (again): every new row shape needs a look
at the text renderer, not just the JSON.

## 2026-07-14 — proved the premise, then made the benchmark measure what we built

Ran the task-level token-to-completion harness (bench/completion.py) across the
full 25-repo / 12-language corpus: completing a realistic task costs a generic
agent (rg/sed/cat) ~6x the tokens of codeq. Premise holds.

But the harness predated the session's new capabilities, so it undersold exactly
where codeq wins hardest — the multi-round work. Added two tasks with fair
(conservative) baselines:

- **blast-radius** = `callers S --depth 2` vs the honest generic loop: grep S's
  call sites, read a window at each to learn the enclosing callers (hop 1), then
  grep each of those (hop 2). Result: 155 baseline tool calls vs 25 — a 6x
  call-count explosion codeq collapses into one command. (Baseline under-charges,
  not over: it skips the hop-2 reads, so the real gap is wider.)
- **which-tests** = `tests S` vs grep-in-test-files + read-to-confirm: **15x**
  fewer tokens. The single sharpest win in the suite.

Kept the one task codeq *loses* (file-imports, 0.8x — annotated output is heavier
than raw `rg` on import lines) rather than cherry-pick it out; a benchmark you
only run to win isn't a benchmark. New totals: 8 tasks, 5.7x tokens, 2.6x tool
calls across 25 repos.

## 2026-07-14 — the file-imports "loss" was an unfair baseline, not a weakness

Chasing the one task codeq lost (file-imports, 0.8x) turned up a flaw in the
baseline, not in codeq. `rg '^\s*(import|from|use|...)'` is line-anchored, so on a
Go block import

    import (
        "fmt"
        "strings"
    )

it matches only `import (` and misses every member. Measured directly: codeq
finds 4-5x more import modules than the rg grep on Go (cobra 188 vs 35, mux 66 vs
17), because it parses the block; Rust grouped `use a::{...}` a smaller edge;
Python near-parity (rg sometimes higher — textual matches in comments/strings).
So codeq isn't a blanket superset, but on multi-line/block imports it's decisively
more complete.

The benchmark was charging codeq *for being complete* — comparing its full answer
to rg's partial one. Fixed the baseline to reach the SAME module list: when a
keyword line is a block opener (`(`/`{`), read the block. file-imports moved 0.8x
-> 1.0x (true parity on bytes) and 25 vs 39 tool calls — codeq's one call still
wins, honestly now. Lesson: when codeq "loses" a bench task, check the baseline is
solving the same problem before believing it.

## 2026-07-14 — wishlist #1: `api` (public API surface) + visibility extraction

Built the highest-value net-new capability: `codeq api <path>` returns only the
*exported* definitions of a module, with signatures — the "how do I call this
unfamiliar module" task that made agents `cat` whole files. Needed real parser
work: a normalized `visibility` (public/private/restricted) computed per language
and stored on every definition fact (schema v6 -> v7).

Visibility is each language's own notion: Rust `pub` (node), JS/TS `export`
(ancestor node), Go capitalization, Python leading-underscore, a `public`
modifier in Java/C#/PHP, non-`static` in C/C++. `pub(crate)`/`protected` map to
`restricted` (excluded from `api`, since they're not external API). Verified the
discrimination on a 5-language probe and at scale (cobra: 450 public / 572 total).

Honest limits, documented not hidden: Ruby `private`/`protected` are stateful
section markers this doesn't track (all reported public); Go test functions are
capitalized so they count as public and show up (scope with a path or `--kind`).
Dogfooded immediately: `api src/deps.rs` cleanly listed the pub structs + methods
and excluded every private helper — the intended replacement for reading the file.

## 2026-07-14 — benchmarking `api` exposed (and fixed) a signature-bloat defect

Added a `public-api` task to the completion harness (`api F` vs grepping
declaration lines and filtering public by eye). It immediately showed codeq
*losing* — 0.3x tokens. Two honest findings:

1. **Real defect, fixed:** `api` printed struct/enum/class *bodies* (every field),
   because the stored signature is the whole node for those kinds. For an API
   *surface* you want the header — truncated the signature at the body opener
   (`pub struct Edge { … }` -> `pub struct Edge`). Functions were already headers.
2. **Honest non-win, kept:** even after the fix `api` is ~3x heavier than a decl
   grep on a single file — same shape as file-imports. codeq returns *complete,
   visibility-filtered* signatures; the grep is cheaper because it returns
   incomplete first-lines and can't tell public from private. api's value is the
   accurate cross-language public filter and the *package-scale* surface (`api
   src/` = a whole package's API vs reading N files), not single-file tokens.

Left the 0.3x in the suite rather than tuning the baseline until codeq won — two
non-wins (file-imports, public-api) are what makes the other seven credible.

## 2026-07-14 — package-scale `api` is where the win is (benchmark confirms)

Followed the single-file `public-api` non-win with a `package-api` task: `api
<dir>` (a whole package's exported surface, one call) vs the generic path of
reading the files that hold that API. Result across 25 repos: 4.2x fewer tokens
and 284 -> 25 tool calls (11x fewer), even with the baseline capped at 15 files
per package (which under-charges it).

The two api tasks now sit side by side and tell the honest truth about scope:
single-file `public-api` *loses* 0.3x (its complete signatures outweigh a decl
grep on one file), package-scale `package-api` *wins* 4.2x (the alternative is
reading N files). api's value is the package surface, exactly as predicted — the
benchmark now shows both the ceiling and the floor.

## 2026-07-14 — wishlist: `unused` (dead-code candidates), and it found some

Built `codeq unused` — definitions with no reference anywhere outside their own
definition site. No git (an agent uses git itself for history); this is a pure
aggregate over the reference machinery: for each distinct definition name, scan
the repo for `\b name \b` and keep the ones whose only occurrences are their def
sites. Conservative by design (precision > recall for a command that might drive
deletion): a mention in a *comment or string* counts as a use (`InFilter::Any`),
and it's name-based, so it under-reports rather than flag live code.

Dogfooded immediately and it found real dead code in codeq itself: `pub fn
has_been_indexed` in src/db.rs — nothing calls it. Also surfaced the expected
signal problem: every `#[test] fn` looks unused (the runner invokes them by
discovery, not by name), flooding the output. Fixed by excluding test-file defs
by default (`is_test_path`, reused from `tests`), `--include-tests` to opt in —
which collapsed the codeq repo's output from 121 rows to the 1 that matters.

Fast enough to not need optimizing: ~0.4s over the whole codeq repo (each name's
scan is trigram-narrowed to files containing the substring; results cached per
name so a name defined N times is scanned once).

## 2026-07-14 — `unused` benchmark: 64x, the suite's sharpest win

Added a `dead-code` task: `unused <file>` (one call) vs the generic hunt — list
the file's definitions, then grep each name across the repo to see if anything
uses it. Result across 25 repos: 64.2x fewer tokens (17.8k vs 1.14M) and 436 vs
25 tool calls. It's the biggest win in the suite because it's a whole-repo
aggregate generic tools can only approximate with a per-symbol grep loop (each
grep dumps every occurrence of a used name). Baseline capped at 20 symbols/file,
so it *under*-charges. First acted on the feature's own finding — removed the
dead `has_been_indexed` it flagged — then benchmarked it. Totals: 11 tasks, 5.8x
tokens, 4.6x tool calls.

## 2026-07-14 — `implementors` (type hierarchy) — the one real graph gap

Every graph command so far (callers/callees/about) is *call*-based; nothing
answered "who implements this trait / extends this class." Built `implementors
<type>` on a new relations pass: a third tree-sitter query per language captures
`@sub` (the deriving type) and `@implements`/`@extends` (the supertype), stored
in a dedicated `impls` table (mirroring `spans`; schema v7 -> v8) rather than
overloaded onto facts — 20 `kind <> 'import'` sites showed how error-prone
overloading would be.

Covers 9 languages: Rust `impl T for X`, Java/TS/PHP `extends`+`implements`
(distinctly), Python `class X(Base)`, Ruby `class X < Y`, C++/C# base lists, JS
`extends`. Go (structural typing) and C have none — reported honestly, not faked.
All 9 grammar queries matched on the first try; validated at scale on the corpus
(serde: 323 relations, `implementors Serialize` → str/String/Option<T>…;
newtonsoft: 700; gson `implementors TypeAdapterFactory` → every factory class).

Known limit, documented: C#'s grammar can't distinguish a base class from an
interface in the base list, so C# reports everything as `extends`.

## 2026-07-15 — `path` (shortest call chain between two symbols)

Rounded out the graph family: `callers/callees --depth` give reachability (does X
reach Y, at what depth), but not *how*. `path <from> <to>` does a BFS down the
callee graph and reconstructs the shortest chain — from calls … calls to — shown
step by step, indented by position. Pure index, no git (matches the "agents use
git themselves" stance). Reuses `callees_of`; empty when unreachable within
`--max-depth`.

Dogfooded on codeq: `path cmd_search innermost` traced cmd_search -> run_search
-> innermost exactly. A test using single-letter fn names (a/b/c/d) surfaced (not
a bug) the deliberate `collect_idents` >=2-char filter that drops single-char
call-noise — fixed the fixture to realistic names. Name-based caveat, as with all
graph commands: the chain-by-name is right, but an ambiguous name's displayed
*location* may resolve to the wrong same-named definition.

## 2026-07-15 — import-scoped resolution: `--resolved` for callers/references

Took on the biggest standing limitation — the name-based graph's false positives.
Full type resolution is out of scope, but the module-dependency graph (already
built for `deps`) gives a principled, deterministic filter: a reference resolves
to a definition when its file *is* the definition site (`same-file`) or *reaches*
one through imports (`imported`), several (`ambiguous`), or none (`unresolved`).
`--resolved` drops the unresolved ones and tags the rest; the default output is
unchanged (safe superset), so it's opt-in precision, not a silent behavior change.

Dogfooded on codeq itself: `callers new` 190 -> 174 (conservative), `references
build` 53 -> 12 — and the 41 pruned `build`s were exactly the noise name-matching
wrongly includes: "cargo build" in markdown, "build release" in Python strings,
prose in docs — none of them codeq's `build` functions. Correct pruning.

Honest limits (documented, opt-in for this reason): import resolution is
best-effort, so `--resolved` can over-prune. Notably Rust `mod x;` declarations
aren't captured as import edges, so intra-crate uses reached only via `mod` +
`crate::` paths may read as `unresolved` — a future win is treating `mod` as a
reachability edge. Safe superset stays the default; a name with no definition to
anchor to leaves the result untouched (never silently emptied).

## 2026-07-15 — Rust `mod` as a reachability edge (improves --resolved recall)

Followed up the resolution work's own documented gap: Rust files that pull in
submodules via `mod x;` (not `use crate::x`) had no import edges, so their
references read as `unresolved` and `--resolved` over-pruned them. Classic case:
`main.rs`/`lib.rs`, which are almost all `mod` declarations — they were graph
sinks, so every reference in them got dropped.

Fix: capture an external `mod x;` (bodyless — `!body` in the query excludes inline
`mod x { … }`) as an import fact, so it feeds the dependency graph. Now `deps
src/main.rs` reaches the whole crate, and `callers cmd_map --resolved` keeps the
`main.rs` dispatcher as `via=imported` (previously pruned). Schema v8 -> v9 to
rebuild with the new edges.

The fix I flagged when shipping `--resolved`, closed. Remaining honest gap: stem
resolution still can't follow Python `from pkg import x` (module string is just
`pkg`). Verified: 135 tests, differential 5223/5223, text-mode pass.

## 2026-07-15 — closed the Python `from pkg import x` resolution gap

The gap flagged when `--resolved` shipped: `from pkg import x` stored only the
module `pkg`, so the imported name was lost and — worse — a bare `pkg` couldn't
even stem-match a package directory (`pkg/__init__.py`, stem `__init__`), so
package imports resolved to nothing and got over-pruned.

Fix: capture `from MODULE import a, b [as c]` as one qualified `MODULE.a` import
per name (the extractor combines a `@from_module` + `@from_name` capture; aliases
keep the original name, which is what picks the source file). Now `from pkg
import target` → `pkg.target`, which stem-matches `pkg/target.py`. Verified on the
actual gap case (package + submodule): `deps user.py` reaches `pkg/target.py` and
`callers make --resolved` keeps the importer as `via=imported` — both previously
unresolved. Dogfooded on flask: from-imports now show qualified (`datetime.
timedelta`, `collections.abc`) and app.py's internal edges resolve.

Also a genuine improvement to `imports`/`deps` granularity, not just resolution:
`from x import a, b` is now two precise edges instead of one coarse `x`. Remaining
coarse forms (documented): relative `from . import x` and wildcard `import *`.
Schema v9 -> v10. Suite 136 green, differential 5223/5223, text-mode pass.

## 2026-07-15 — extended qualified-import capture to JS/TS named imports

The `@from_module`/`@from_name` mechanism built for Python from-imports is
language-agnostic, so extending it to JS/TS was one query pattern (shared across
JS/TS/TSX via JS_QUERY): `import { a, b [as c] } from './x'` now emits qualified
`./x.a` per named binding, capturing the *source* name (not the alias, which is
the local binding — the source name is what picks the exporting file). Default and
namespace imports keep the coarse module string. `callers make --resolved` on a JS
fixture importing `{ make }` correctly resolves the importer as `via=imported`.

With this, named-binding reachability works across the three languages people
actually hit it in — Rust (`use`/`mod`), Python (`from`), JS/TS (`import {}`).
Star re-exports (`export * from`) and wildcard/relative forms stay coarse by
design (no specific name to qualify). Schema v10 -> v11. Suite 137 green,
differential 5223/5223, text-mode pass.

## 2026-07-15 — resolution refinements: `--calls` and `--on` (scan-time, no extraction)

The two deferred refinements, both done at *scan time* from the match position —
no new index extraction. `--calls` keeps only call sites (`NAME(` after the
match), excluding a bare identifier / field / variable of the same name; `--on
RECV` keeps only `RECV.NAME` / `RECV::NAME` / `RECV->NAME`, disambiguating a
common method by its receiver. Both imply code-only (a call in a doc comment
isn't a call site — caught when `references build --calls` surfaced a `//! fn
build()` prose example, since references defaults to matching everything).

Dogfooded: `references build` 56 → 15 with `--calls` (drops "cargo build" prose,
comments), `--calls --resolved` → 9 (composes), and `references build --on Graph`
→ exactly the one `deps::Graph::build(...)` call. `--calls` shows its value on
`references` (every occurrence) more than `callers` (which already dedups to
enclosing defs, so a def that both calls and mentions still counts once).

With this the false-positive story is complete along two axes: `--resolved`
(which definition) and `--calls`/`--on` (which occurrence). Suite 139 green,
differential 5223/5223, text-mode pass.

## 2026-07-15 — `read --raw`: serving the edit loop, not just reading

Reflection on my own workflow: codeq owned *orient* and ceded the edit handoff —
after it told me *where* code was, I re-fetched it with grep+Read to get the exact
`old_string` for an edit, because codeq's output is comprehension-optimized
(folded, `[symbol]`-annotated, `cat -n` line prefixes) — the opposite of what an
editor's search text needs.

`read --raw` closes that: exact bytes of a symbol/range, no prefixes, no folding,
addressed by name. `codeq read foo.rs --symbol bar --raw` now yields bar's source
verbatim, ready to paste as an Edit `old_string` — collapsing grep-for-line →
Read-region → strip-prefixes into one call. With `--symbol` it refuses an
ambiguous multi-match (an edit anchor must be unique) and points to `--lines`.

Deliberately kept read-only (the safe half of the "serve the edit loop" fork);
structural edit primitives (`replace`/`insert` by symbol) are the mutation half,
left for later. Default read output unchanged. Suite 141 green, differential
5223/5223 (read 500/500), text-mode pass.

## 2026-07-16 — scale test found an O(files²) cold-index bug

Indexed a synthetic 9k-file repo (the whole bench corpus, hardlink-multiplied in
tmpfs) — cold index took **233s** (~25ms/file), yet the same repos indexed
*separately* summed to ~40s. Classic superlinearity. Cause: the per-file refresh
did `DELETE FROM content_fts WHERE path=?` unconditionally, and `content_fts.path`
is UNINDEXED (FTS5), so each delete scans the whole *growing* FTS table — O(n²)
over a cold index / `--rebuild`, where every file is "changed".

Fix: only run the stale-row deletes when the file was *already* indexed (`prev =
existing.get(path)`); a brand-new file has nothing to delete. Also added the
missing `impls(path)` index (same scan for its per-file delete). Cold index of 9k
files: **233s → 50s (4.7x)**, and now linear (9k/18k/27k → 50/78/153s, ~5ms/file).

Scale profile after the fix (27k files, 1.2GB index): cold index linear ~5ms/file;
warm queries linear — freshness sweep 37→70→102ms (the per-query "always fresh"
tax, stat over the manifest, ~3.7ms/1k files), search fast (25→62ms), `callers`
the slowest-growing (0.5→1.4s, name-based candidate scan). DB ≈ source size
(trigram FTS dominates). Correctness held: 141 tests, differential 5223/5223,
incremental edits still detected. Remaining (linear, not bugs): the freshness
sweep and `callers` cost at 100k+ files — noted, not yet optimized.

## 2026-07-16 — parallel cold index (2.7x more, on top of the O(n²) fix)

Profiling the (now-linear) cold index showed it CPU-bound single-threaded — 1.00
cores on a 4-core box, 3 idle. Split the cost: ~67% parse/extract (tree-sitter),
~33% FTS trigram insert (serial, single SQLite writer). So parsing is
embarrassingly parallel with a serial-writer ceiling.

Parallelized it safely: worker threads each own an Extractor and do
read+hash+parse; results flow over a **bounded** channel to a single writer
thread that owns the transaction. Only *parsing* is concurrent — every SQLite
write stays on one thread, so index content is deterministic regardless of which
worker finishes first (facts are content-addressed, queries ORDER BY). Confirmed:
differential 5223/5223 on two consecutive runs (no nondeterminism), text-mode
pass, incremental edits/deletes still correct.

Result: 9k-file cold index 50s -> 18.3s (**2.7x**, 2.2 cores busy — the serial FTS
writer is the ceiling, exactly the profile). Combined with the O(n²) fix this
session: **233s -> 18.3s (12.7x)**. Extrapolated 100k files: ~40min -> ~3min.

## 2026-07-16 — chasing the per-query cost: what's worth it, what isn't

Two more perf leads after the cold-index wins. Kept a daemonless single binary
throughout (no watcher/fsmonitor).

**Freshness sweep (`is_fresh`, runs on every query).** Measured: pure stats of 27k
files = 52ms, but parallelizing them is *6x slower* — warm-inode stat contends
across threads (the kernel VFS path isn't embarrassingly parallel). So the sweep
is at its stat-floor within a daemonless binary. Shipped only the honest part:
borrow the path from the SQLite row and reuse one `root/…` buffer instead of a
String + PathBuf alloc per file (~200k allocations saved at 100k files). Small on
a warm cache; cleaner, and matters more cold.

**Query scan (`callers`/`references` at scale).** Profiled callers `new`: it
looked like the 739ms was regex scan, so I parallelized `run_search` (workers with
their own read connections). Correct (differential 5223/5223 twice) but only
~1.3x. Then measured pure-regex Count mode parallel: 799 vs 739ms — *no* speedup.
The real cost is upstream in `fetch_bodies`, which serially loads the full body of
every file containing the substring (MBs for a common name); the scan is a small
fraction. So the change optimized the wrong target for a modest win on a *rare*
worst case (common-name queries), at the cost of per-query multi-connection
complexity. **Reverted it** — shipping complexity that misses the actual
bottleneck isn't worth it. The honest remaining cost is body-loading, which is
I/O/memory-bound and the pathological case is uncommon.

## 2026-07-16 — streaming search: bound memory, stop copying bodies

Chased the real query bottleneck (fetch_bodies) after the parallel-scan revert.
Measured it: `callers new` on 27k files matches 9,903 files = **174 MB of bodies**,
and `fetch_bodies` loaded them all into a Vec at once — 226 MB peak RSS. The scan
itself is memory-bandwidth-bound (why parallelizing it did nothing), so the fix is
to *move less data*, not add cores.

Merged fetch_bodies into run_search as a streaming pass: iterate the FTS cursor
one row at a time and **borrow** each body from the row (`get_ref`/`as_str`,
never copied into a String), scan it, drop it. Peak memory is now one body, not
all matching bodies — the scale-robustness win (a common-name query on a huge repo
no longer risks loading GBs / OOM). Also kills the 174 MB of String allocation.

Results (27k, worst case `new`): peak RSS 226 → **109 MB (2.1x)**; `references
new -l 0` 2368 → **1177 ms (2x)** (it had materialized all bodies *and* matches);
`callers`/`search` ~1.1x (the residual is the bandwidth-bound regex scan — a token
index would be the only way further, a much larger change). Correct: differential
5223/5223, text-mode pass, 141 tests. The glob filter (search --glob) became a
per-row path predicate.

## 2026-07-16 — token (inverted position) index: measured, declined

Considered a Zoekt-style token index — store every identifier occurrence's
position (`name -> [(file, line, col), …]`) so `references`/`callers` become an
indexed `WHERE name=?` lookup instead of loading + regex-scanning bodies. It's the
only way past the residual bandwidth-bound scan cost on common-name queries.

Measured the real cost on the 27k corpus before building: **23.8M** identifier
occurrences (~910 MB, ~doubles the 1.2 GB DB); **11.9M** if restricted to
symbol-name occurrences (~450 MB, but needs two-pass indexing). Plus a real
index-time hit (tokenize 237 MB + insert millions of rows) on the cold index we
just optimized.

Declined. The trade is poor for codeq's workload: a *specific* symbol matches few
files, so today's scan is already ~tens of ms — the token index only rescues the
*rare* common-name query, at a *permanent* 450–910 MB + slower-indexing cost on
every repo. Zoekt pays this because it serves interactive any-name search over
giant repos; codeq doesn't. And the streaming search fix already removed the
actual scale risk (the memory blowup). Revisit only if reference latency on huge
repos becomes a real, reported pain — the design and numbers are here.

## 2026-07-16 — SQLite pragma tuning: net-negative here, not shipped

Tried the classic bulk-load / read pragmas (cache_size -256MB, mmap_size 256MB,
temp_store MEMORY) as a low-risk broad win. Measured net-negative on the test env
(tmpfs / RAM-backed): cold index 18.3 -> 26s, warm_noop 99 -> 128ms; only callers
improved slightly. Reason: mmap and a big cache pay off by avoiding disk I/O, but
on a RAM-backed FS there's no I/O to avoid — they just add setup overhead. They'd
likely help on a real SSD, but shipping a change that regresses the measurable
case for an unmeasurable one is the wrong call. Reverted. (If ever revisited,
gate the pragmas on index-vs-query and benchmark on real disk.)

## 2026-07-16 — efficiency audit: astgrep streaming + lazy scan work

Audited the hot paths for inefficiencies. Most are already tight (batched IN-clause
lookups in callees_of, indexed queries, the streaming search). Found and fixed two:

1. **astgrep materialized all language bodies** into a Vec before matching — the
   same anti-pattern the search streaming fix removed. Since astgrep::search takes
   an iterator, streaming the FTS rows straight in (no Vec) bounds it to one body
   at a time: csharp peak RSS 35 -> 12 MB. Matters on a big single-language repo.
2. **run_search computed line-starts (a full-body newline scan) for every
   candidate** even in Files mode (never used) and for candidates that match the
   substring prefilter but not `\bword\b`. Made line-starts *and* the per-file
   defs load lazy — computed only when a match actually survives the filters. Win
   is modest for common names (most candidates match) but real when the substring
   over-matches: `references newer` (trigram "new" hits thousands, "\bnewer\b" few)
   scans in ~109 ms. Correct: differential 5223/5223, text-mode, 141 tests.

Left alone (measured/considered, not worth it): callees_of loads each target's
full body to slice one function (fine — targets are few and files usually small;
a substr(blob) would help only huge files, with UTF-8 byte/char subtlety).

## 2026-07-16 — skill-file loop: cold subagents as a doc-and-tool fuzzer

Developed the codeq usage skill (`.claude/skills/codeq/SKILL.md`) by iterating it
against fresh subagents, and it worked well enough to write down as a repeatable
method.

**The loop.** Spawn a *cold* general-purpose subagent (no session context) given
ONLY the skill file, a handful of concrete navigation tasks with checkable answers,
and instructions to (a) prefer codeq, (b) note every fallback to grep/reading
files and why, and (c) end with a blunt FRICTION REPORT. Read its report, fix the
friction, re-run with a fresh agent and slightly different tasks. Stop when an
agent says "it was clear" and lands every task first try.

**Four rounds converged it** (1: 4/5, 2: 5/5, 3: 5/5, 4: 4/4, zero grep fallback in
any round). Two findings worth remembering:

1. **It's a fuzzer for the tool, not just the docs.** A fresh agent with no
   assumptions found a real bug I'd missed — `codeq path main run_search` resolved
   the *Python* `main` and `path` had no `--path` to disambiguate (fixed in code) —
   and pinned the exact semantics my own doc got wrong: `--on RECV` matches the
   *literal receiver token*, so it fits `Type::method` (`Session::open`) but not
   `var.method` (`s.emit()`, token is `s`). Testing the docs surfaced tool gaps.
2. **Navigation tasks overcome the grep reflex.** Every agent stayed entirely in
   codeq. (Contrast: I reach for grep/Read in *edit*-dominated work, because the
   harness gates Edit on a prior Read — see the earlier honest-reflection notes.)
   So the skill is effective for its real job (explore/understand), and that's the
   job to optimize the doc for.

**Re-run it whenever codeq gains a command or flag**: add a task that exercises the
new thing, run one cold subagent, read the friction report. Cheap regression test
for both the tool's UX and the skill file.

## 2026-07-17 — FTS size-cap: built, correct, but breaks search⊇grep — reverted

Built a size/minified cap: skip trigram-FTS-indexing files over 2 MB or with a
huge average line length (minified bundles, SVG blobs, one-line JSON), still
recording them in `files`. It worked exactly as designed — real large source
(json.hpp 900KB avgline 37, lodash.js, module.c) stayed indexed; large.json (6.7MB)
and the SVGs skipped; a synthetic minified-JS repo indexed to 96 KB instead of
~6.7 MB.

But the **differential caught it: 5223 -> 5210**. All 13 failures are codeq missing
content rg finds in the skipped files — the SVGs, and **20,000+ lines in
large.json**. The cap breaks codeq's core "search is a superset of grep on text"
guarantee: any file skipped for FTS is silently unsearchable, so `search`/
`references`/`callers` miss real content. And *any* skipping violates it — there's
no threshold that both saves space and preserves the invariant.

So a **default** cap is wrong, full stop. The only correct form is **opt-in** (a
persisted per-repo setting, default off, so the default keeps the guarantee and a
minified-heavy repo can trade searchability for a smaller/faster index). That's
real plumbing (thread a max-bytes setting through refresh -> workers, persist it in
`meta`, apply on every auto-refresh) for a niche benefit. Reverted to the clean
5223/5223 state; the design + the invariant tradeoff are captured here for if it's
ever worth the opt-in build. Lesson: the differential is the guardrail that stops
a plausible-looking index optimization from silently making search lie.

## `about` section ordering — definitions should lead (fixed)

A cold subagent testing the skill file noted `codeq about NAME` printed sections
alphabetically (`# callees`, `# callers`, `# definitions`) — so *definitions*, the
thing you most want first, came last. Root cause was not in the renderer: serde_json
defaults its `Value` object to a sorted `BTreeMap`, so the `About` struct's declared
field order (`definitions, callers, callees`) was lost before `render_value` ever saw
it. Fix: enable serde_json's `preserve_order` feature (one Cargo line) so objects keep
declaration order everywhere — a strict improvement (declared order is intentional and
meaningful) rather than a per-command special-case in the text renderer. `about` now
leads with definitions; 142 tests, differential 5223/5223, textmode ALL PASS unchanged.
Lesson: "alphabetical output" bugs can live in the serialization layer, not the printer.

## callees precision parity — `--resolved` (and why NOT `--calls`)

`callers`/`references` grew three precision filters (`--calls`, `--on`, `--resolved`),
but `callees` had none — an agent who learns `--calls` on callers would reach for it
on callees and find nothing. Investigating the actual over-match (DOGFOOD logged
`callees` pulling in a `Fixture::remove` the target never calls) showed the honest
shape of the gap:

- **`--calls` would be a no-op.** `collect_idents` already requires a `(` after the
  name, so `callees` is call-position only by construction. Adding a `--calls` flag
  that does nothing would be worse than the inconsistency. Documented instead.
- **`--resolved` is the real fix**, and here it can be *more* precise than the
  name-based Resolver used for callers: the anchor is fixed (NAME's own definition
  file), so we prune by each callee's *specific* def-file reachability through the
  import graph, not just "some def of this name is reachable." `callees run
  --resolved` drops the unrelated `work` in a file `run`'s file can't import, tagging
  the rest `via` same-file / imported. Same documented heuristic limit as everywhere:
  if several same-named defs sit in reachable files it keeps them all (can't tell
  which you meant) — a superset-narrowing, not type resolution.

Wired through CLI + MCP (schema + arm) + docs; +1 test (143 total), differential
5223/5223, textmode ALL PASS. Lesson: "parity" isn't "add every flag" — it's matching
the *useful* precision, and being honest in the docs about the one flag that would be
dead weight.

## CLI↔MCP parity audit — flag drift found and closed

After adding `callees --resolved` to both CLI and MCP, ran a full parity audit
(agents-via-MCP are a primary consumer, so drift there is silent breakage). Method:
scripted diff of the clap command/flag surface against the MCP arms + schemas, then a
*definitive* check — extract every CLI flag each MCP arm actually pushes and confirm
the CLI subcommand accepts it (`--help` grep).

- **Command level: clean.** Every subcommand has an MCP tool except `help`/`index`/
  `mcp` (correctly excluded). No rename drift.
- **Flag level: real drift.** `--limit` was exposed on some MCP tools (callers,
  search, symbols, …) but silently missing from 6 others (`definitions`, `changed`,
  `tests`, `imports`, `importers`, `astgrep`) — an MCP agent couldn't cap results
  there. And `codeq_context` was missing `--path` (scope seeds) and `--no-text`
  (provenance-only). Wired all of them through arm + schema.
- **The scripted heuristic itself over-reported** `context --limit`: the earlier
  regex saw `limit` in the variant body (from `max_tokens`) and flagged it, but
  `context` has no such flag. I nearly wired a phantom `--limit` into the context arm
  — the *definitive* arm-flag-vs-`--help` check (and a runtime `context --limit` →
  "unexpected argument") caught it before commit. Reverted just that one; the other
  six were verified real at runtime first.

Locked in with an MCP-protocol regression test (`mcp_context_forwards_path_and_no_text_flags`)
driving the real JSON-RPC server — the `no_text` assertion fails if the flag stops
being forwarded. 144 tests. Lesson: a scripted surface-diff is a good *finder* but a
bad *authority* — confirm each hit against the real CLI before wiring it, or you trade
one drift for a worse one (a schema that promises a flag the CLI rejects).

## `callers --examples` — usage examples, reusing what was already there

Goal: answer "how is this API actually called?" in one command. Investigating first
paid off — two facts changed the design from "new feature" to "surface what's already
computed":

1. **The source line was already in hand.** `callers_of` runs the same search as
   `references`, whose `Hit` already carries the matched line `text` — `callers` just
   *discarded* it, keeping only the enclosing symbol. So an agent had to run `callers`
   (who) then `references --calls` (how) separately.
2. **The renderer already formats it.** `render_row` turns any row with a `text` field
   into a grep-style match line (`path:line:col: <text>  [symbol]`). So no renderer
   change: `--examples` just stops dropping the hit's `col`/`text`, and the output
   falls into the existing match-line path automatically.

Result: `callers NAME --examples` → `path:line:col:` the real call expression, one
representative site per caller (callers already dedupes by enclosing symbol);
`references NAME --calls` stays the exhaustive per-site form. Composes with
`--calls`/`--on`/`--resolved`. Wired CLI + MCP + docs, +1 test (145), textmode ALL
PASS. Lesson: the best "new capability" is often plumbing an existing value (the hit
text) to an existing sink (the match-line renderer) — look before building.

## `explore` — the one-call answer (learning from codegraph)

codegraph's whole thesis is a single `explore` tool that returns "relevant symbols'
verbatim source grouped by file, plus the call paths between them," and that one design
choice is where its headline "58% fewer tool calls" comes from. We had the graph
(`about`) and the budgeted source assembler (`context`) as separate commands but nothing
that fused them, so an agent ran `definitions` -> `callers` -> `read` -> `read`. `explore`
composes what already existed: the graph functions for the edges, plus context's chunking
and budget fill for the source.

Two things the "look before building" habit caught:

1. **A common-name blowup.** Seeding `context::build` with bare neighbour *names* would
   re-expand a common callee (`new`, `open`) to every same-named definition in the repo,
   and since seeds are force-included, blow the budget with noise. Fix: a targeted
   assembler (`context::source_for`) keyed on the precise `(name, path)` we already
   resolved, not on names. Never balloons.
2. **No renderer work.** Structuring the result as a flat object of arrays
   (`focus`/`callers`/`callees`/`source`) let the existing envelope renderer print it as
   `# section` blocks, and tagging each source chunk with a role reason ("focus"/"caller"/
   "callee") made `render_row`'s multi-line-text branch emit `path:line (caller)` headers
   for free. The focus is `forced` so it survives any budget; overflow is reported in
   `omitted`.

Wired CLI + MCP (registered first, as the primary tool, mirroring codegraph) + docs.
+2 tests (147), textmode ALL PASS. Lesson (again): the biggest capability of the session
was mostly *fusing* two things we already had, with one precise-targeting fix so it
doesn't inherit the name-based blowup.

## `affected` — "what to re-run for this diff" (borrowed from codegraph)

codegraph ships `affected` (test files impacted by a change); we had every piece
(`changed --symbols`, a caller BFS, `tests`) but never composed them. `affected
--compare-to <ref>` returns the diff's touched definitions, their transitive callers
(the blast radius, depth-bounded), and the test files exercising any of them. Built by
extracting three shared helpers (`touched_defs`, `test_sites`, `resolve_test_rows`) from
the existing commands so nothing is duplicated.

Dogfooding it on its own (huge, core-touching) commit immediately exposed a UX problem
the fixture tests would not have: at *function* granularity the tests list was 153 rows,
including test-file *helpers* (`git`, `run`, `field_set`) that our name/path test
heuristic can't tell from real tests. Fix, matching codegraph's own choice: report test
*files* with a per-file match count (`tests/cli.rs: tests=153`) — that's what you act on
("run this file"), and it sidesteps the helper-vs-test ambiguity entirely. The blast
radius stays name-based and depth-bounded (default 3, HOP_CAP guard); for a foundational
change "everything is affected" is the correct answer, not noise. +1 test (148), MCP
parity clean, textmode ALL PASS. Lesson: dogfooding a command on a *real* messy diff
surfaces output-shape problems that a clean 3-symbol fixture never will.

## `install` — one-command MCP setup (borrowed from codegraph)

codegraph's `install` auto-writes its MCP config into each detected agent; manual MCP
registration is real adoption friction. `codeq install` detects installed agents (Claude
Code, Cursor, Windsurf, Gemini — all share the `{ "mcpServers": {...} }` JSON shape) and
merges a `codeq` entry into each one's config, pointing at the absolute path of the running
binary (`current_exe`).

This is the one codeq command that writes outside the index, so it's the most conservative:
- **Read-only-index rule still holds** — it touches *agent config* in `$HOME`, never the
  repository codeq indexes. That distinction is why it's allowed at all.
- Only edits *detected* agents unless `--agent` names one; merges without dropping other
  keys (verified: a pre-existing server and unrelated keys survive); idempotent
  (`already-current`); `--dry-run` previews; `--uninstall` removes only the codeq key.
- Pure in the home dir (`apply(home, opts)`), so it's unit-tested against a temp dir and
  **never touches a real config in tests** — the one feature where testing against the real
  side effect would be actively harmful.
- CLI-only, not an MCP tool: an agent has no reason to install codeq into other agents.

3 unit tests + docs; 148 integration tests still green. Lesson: for a command with an
irreversible side effect on the user's machine, make the core a pure function of an
injected root so it can be exercised safely, and gate the effect behind detection +
dry-run + uninstall.

## Head-to-head benchmark — published numbers (borrowed from codegraph)

codegraph wins credibility partly by *showing* numbers (58% fewer tool calls, 22%
faster). We already had `bench/completion.py` (token-to-completion vs a fair generic-tool
baseline across 25 pinned real repos) but it predated the flagship `explore`. Added an
`explore` task with an honest baseline: to assemble the same def + callers + callees WITH
source the generic way is a locate+read loop (read the def, read a window at up to 3 call
sites, locate+read up to 3 callees) — codeq does it in one call.

Result across 25 repos / 12 tasks: **5.2x fewer tokens, 5x fewer tool calls** (300 vs
1519). `explore` itself: 25 vs 243 calls (~10x fewer) — the codegraph "fewer tool calls"
story, only sharper. Crucially the harness also reports where codeq *doesn't* win: `api`
0.3x (returns complete signatures vs a decl grep's first-lines), imports 0.9x, references
1.1x (near parity with rg). Publishing the losses is the credibility move codegraph's
all-green table skips. Numbers went into the README Benchmarks table; reproduce with
`python3 bench/completion.py`. Lesson: a benchmark that admits its near-parity and losing
cases is more persuasive than one that doesn't.

## Five more languages: Bash, Lua, Scala, Kotlin, Swift

Extended extraction from 12 to 17 languages, closing part of the breadth gap vs
codegraph. Each needs a grammar crate + three tree-sitter queries (defs, comment/string
spans, relations) + wiring (Extractor field, extract arm, has_spans, extension map,
astgrep grammar, capabilities) + a schema bump (11 -> 12) so indexes rebuild.

The queries were the real work, and writing them blind was a trap. Two bugs only a
parse-tree dump caught (I wrote a throwaway `tsdump` bin, used it, deleted it):
- **Swift double-matched** every function: `function_declaration` exposes BOTH its name
  AND its return type under a field literally named `name`, so `name: (_)` bound twice.
  Fix: pin to `name: (simple_identifier)`. Protocol methods are also a distinct node
  (`protocol_function_declaration`), not `function_declaration`.
- **Kotlin looked broken** on a hand-written single-line class after `package`+`import`
  (whole class became an ERROR node) but parses real multi-line code with zero errors —
  the pathology was my test snippet, not the grammar. Lesson: validate a new grammar on
  realistic multi-line code, not minimal one-liners, and dump the actual tree before
  trusting node-types.json (Kotlin's name field is `identifier`, not the `type_identifier`
  the schema suggested).

Bash/Lua/Scala worked first try. v1 scope: defs + spans + imports (Scala/Kotlin/Swift
have real import statements; Bash/Lua use call-style includes, skipped). Relations
(implementors/supertypes) deferred — empty like Go/C. Verified end to end on all five:
outline, callers/callees, read-fold, search + enclosing symbol, `--in` span classification,
explore. 149 tests (+1), differential search 2250/2250 (invariant unaffected), textmode
ALL PASS.

## Four more languages: Dart, Zig, Elixir, Solidity (21 total)

Extraction now spans 21 languages. Dart and Solidity were routine (clean `class`/
`contract` + `function` nodes with name fields). Two were interesting:

- **Zig** binds types as constants — `const Point = struct {…}` is a `variable_declaration`
  whose value is a `struct_declaration`, not a named type node. Captured via
  `(variable_declaration (identifier) @name (struct_declaration)) @struct` (+ enum/union).
  Functions are clean. Imports (`@import` builtin calls) skipped, like Bash/Lua.
- **Elixir has no def syntax** — `defmodule`/`def`/`defp` are all macro `call` nodes. Reused
  the `@guard` mechanism built for Ruby's `require` (a call, not a keyword) and made its
  keyword allow-list language-aware: for Elixir it accepts the `def*` family, so
  `defmodule Foo`/`def bar` extract as module/function while an ordinary `import Foo` call is
  rejected. The call graph resolves across it (`callees` on an Elixir function finds what it
  calls). Def bodies aren't folded (no body field on the macro call), like Ruby.

Following the last batch's lesson, I dumped real parse trees first (throwaway `tsdump` bin,
deleted after) rather than trusting node-types.json. Honest v1 limits: relations
(implementors/supertypes) empty for all four; Elixir imports (`alias`/`import`/`use`)
skipped; astgrep on Zig is finicky because a bare call isn't valid top-level Zig so the
metavar pattern won't parse (inherent astgrep limit, grammar is wired). Verified end to end:
outline, callers/callees, read, search + enclosing symbol, `--in` spans, imports (Dart/
Solidity). Schema 12 -> 13. 150 tests (+1), differential search 2250/2250, textmode ALL PASS.

## Svelte: the first embedded-language case (22 total)

Svelte broke the shape every previous language fitted. The other 21 are "one file, one
grammar"; a `.svelte` file is a *component* — markup, with the real code sealed inside
`<script>`, which the Svelte grammar hands back as a single opaque `raw_text` token. Wiring
the grammar alone would have indexed nothing an agent wants.

- **Blank, don't slice.** Each `<script>` body is re-parsed with the JS or TS extractor
  (chosen from the block's `lang` attribute) over a *copy of the whole file* with every byte
  outside the block replaced by a space, newlines preserved. Byte offsets, rows and columns
  therefore come back already in file coordinates — no offset arithmetic, so no class of
  off-by-one bugs, and multiple blocks (`<script module>` + `<script>`) just work. Verified
  the hard case: a definition sharing a line with the `<script>` tag lands at the right
  column. `LangSpec::extract` was split into parse + `harvest` so the one Svelte parse feeds
  both the markup queries and the script-region walk.
- **The file is the definition.** A component has no declaration to capture — `Button.svelte`
  is referenced as `<Button/>` after importing it. So the file itself yields a `component`
  fact named for its stem, which is what makes `explore Button` return the component plus
  everything it uses. This needed `Extractor::extract` to learn the path. Guard rail:
  SvelteKit routes (`+page.svelte`) are skipped — convention-loaded, never named, so a fact
  there could only ever surface as false dead code in `unused`.
- **Spans are half-covered, on purpose.** Markup comments and quoted attribute values are
  classified, script spans come from the nested pass — but `<style>` is another opaque token,
  so CSS comments read as *code* to `search --in`. Documented rather than papered over.

**Follow-up, found by dogfooding on a real Svelte app:** `callers FormTiles` pointed at
`CurrentForm.svelte:2` (the import) instead of `:116` (the `<FormTiles/>`) — right file,
wrong line. The dedup keeps one row per (file, enclosing symbol) and the first hit wins;
imports had always sat *outside* every definition, so this never bit. A component
definition spans its whole file and therefore encloses its own imports, so the import won
the tie and *shadowed* the usage — the line was silently lost.

The instructive part was the fix I nearly shipped. Excluding import lines from `callers`
outright fixed the symptom and read cleanly ("an import is a dependency edge, that's what
`importers` is for") — but it quietly narrowed the default, and this repo's rule is the
opposite: the default is a safe textual superset, with `--calls`/`--resolved`/`--on` as the
opt-in narrowing filters. It would also have dropped every import-only file from `callers`
across all 22 languages to fix a Svelte anchoring bug. Corrected to *rank* anchors instead
of dropping rows: the representative site is the first real usage, falling back to the
import when the file names the symbol nowhere else. Lesson: when a fix changes a default,
check it against the documented contract before the tests — 152 green said nothing about
this, because no test pinned the superset property.

Honest v1 limits: `astgrep --language svelte` matches markup only (a `<script>` body is one
token to that grammar, so JS patterns can't reach inside); relations are empty; Svelte 5
runes (`let x = $state(0)`) aren't definitions, consistent with how plain JS treats a
`variable_declarator` with a call value. Schema 13 -> 14. 152 tests (+2), full suite green.

## Vue: the abstraction holds (23 total)

Vue was the test of whether the Svelte work produced an abstraction or a one-off. It held:
`tree-sitter-vue-next` names its nodes exactly as the Svelte grammar does (`script_element`,
`raw_text`, `start_tag`, `comment`), so `script_regions`, `script_is_ts`, `blank_outside` and
`component_fact` all applied unchanged. `extract_svelte` became `extract_component`, taking
the markup `LangSpec` plus the JS and TS ones as three disjoint `&mut` field borrows — no
`RefCell`, no cloning, and Svelte and Vue are now two arms of one path. `<script>` and
`<script setup>` in the same file need no special case; they are simply two regions.
`VUE_QUERY` is empty: a Vue `<template>` declares nothing nameable, so every definition comes
from the nested script pass and the file's own component fact.

**The one real divergence, and it was a bug I shipped into the working tree before catching
it.** Vue puts JavaScript in *quoted* directive values — `@click="handleClick"`,
`:disabled="d"`, `v-if="c"` — where Svelte uses braces (`on:click={handleClick}`), which are
never quoted values. Copying `(quoted_attribute_value) @string` across therefore classified
every template expression as a *string*, and `callers`/`callees` read code only: `callers
handleClick` returned nothing at all for a handler the template plainly binds. The grammar
does distinguish them (plain `attribute` vs `directive_attribute`, each with its own
`quoted_attribute_value`), so scoping the span to `(attribute (quoted_attribute_value))`
fixes it. Worth noting how it surfaced: not from a test, but from reading `callers
handleClick` on a realistic fixture and asking why an obviously-bound handler had no callers.
An empty result is the easiest kind of wrong answer to miss — nothing looks broken.

Also confirmed the earlier anchor-ranking fix generalises for free: `callers Button` points at
the `<Button …/>` in the template, not the `import` line above it. Honest v1 limits, same as
Svelte: `astgrep --language vue` is markup-only; relations empty; `const x = computed(…)` and
`ref(…)` aren't definitions, consistent with how plain JS treats a `variable_declarator`
whose value is a call. Nuxt `pages/` files get a component fact named for the stem even
though they are convention-routed (no `+` marker to detect, unlike SvelteKit). Schema
14 -> 15. 153 tests (+1).

## explore's budget was advice, not a limit

Dogfooding the new component languages on a real Svelte app turned up a bug that
had nothing to do with Svelte — it just made it impossible to miss.

`explore App --max-tokens 200` returned **14,299 tokens**: 71x the budget asked for.
The focus definition is never dropped (rightly — dropping the symbol you named is
worse than overshooting), but "never dropped" had quietly come to mean "never
measured", so any definition bigger than the whole budget sailed straight through
it. Pre-existing and language-agnostic: codeq's own `explore refresh --max-tokens
200` returned 2,911 tokens (14.6x). Components merely turned "occasionally
overruns on a pathological function" into "always overruns by one whole file",
because a component's definition *is* its file. Every component in that app cost
~15k tokens to explore — `explore` degenerating into `cat`, which is the exact
thing it exists to avoid.

First fix was folding the focus (reuse `read`'s folded view). On a script-heavy
component that works beautifully. On the real one it recovered **6%** — App.svelte
is mostly template, and folding only collapses function/method bodies. Worth
recording: the obvious fix was right in principle and nearly useless in practice,
and only measuring it on real input showed that. So: fold first, then clip the
tail if folding didn't get there. Both paths emit a `⋮` marker naming the exact
hidden line range and tag the chunk `reduced: folded|clipped`, so nothing vanishes
silently and `read --lines` recovers it. Budget is now 0.98–1.00x of whatever is
asked, never over. `context` force-includes its seeds on identical reasoning and
got the same treatment.

A second, smaller find from the same session: `explore` emitted source it had
already emitted. Exploring `App` produced the whole file *plus* 14 of its own
functions again, each one a literal substring of the first chunk. Also
language-agnostic — a Python class and a Rust trait both did it, and codeq's own
`AstRow` wasted 13%. Now a chunk is skipped when an already-emitted *verbatim*
chunk contains it; `App` went 17 blocks -> 3. The "verbatim" qualifier is
load-bearing: a *folded* container has holes where its bodies were, so its members
must still be emitted in full — you get the folded shape plus the specific
functions you asked about, not a folded shape with gaps.

Left deliberately: a small definition declared *inside* a larger one (codeq's own
`AstData`, a struct inside `cmd_astgrep`) still appears twice, once as the focus
and once inside the caller's body. That is the inverse direction — the focus has
to stay identifiable and the caller carries other content — and it is bounded by
the focus size. Also confirmed clean and left alone: `search` vs `rg` across 18
patterns, the `--in` partition (both apparent mismatches were legitimate), and
component byte ranges under CRLF, BOM, multibyte markup and `<script src=…>`.
