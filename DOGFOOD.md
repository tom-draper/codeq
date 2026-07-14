# Dogfooding log

Honest friction, limitations, and failures found while using codeq to develop
codeq. New entries at the top. The point is candour — a frustration recorded
here is a feature request or a bug report.

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
  sound. Three text/command paths the automated suite structurally can't reach,
  all found by *using* the tool.

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
- **GAP: C++ pure method *declarations* aren't captured.** `void m();` inside a
  class yields no symbol; only out-of-line definitions (`Widget::m`) and inline
  ones are. A header-only class shows no methods — bad for C/C++ header
  navigation. Also `typedef struct {…} Named;` (anonymous struct typedef) isn't
  captured.
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
  (std) from a call to a defined `remove`. Skipping `.`-preceded identifiers
  would drop the field/std noise but also lose real method-call callees
  (`s.emit`, `extractor.extract`). Left as-is and documented; a proper fix needs
  resolution, which is deliberately out of scope. Reader beware: callees is a
  superset.

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
