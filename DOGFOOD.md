# Dogfooding log

Honest friction, limitations, and failures found while using codeq to develop
codeq. New entries at the top. The point is candour — a frustration recorded
here is a feature request or a bug report.

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
