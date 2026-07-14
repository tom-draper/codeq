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
