# Dogfooding log

Honest friction, limitations, and failures found while using codeq to develop
codeq. New entries at the top. The point is candour — a frustration recorded
here is a feature request or a bug report.

## 2026-09-25 — Search called exact-limit results truncated

Exploring `scan_doc` showed it set `truncated` as soon as the number of rows
reached `--limit`. A one-result search with `--limit 1` therefore claimed there
were more results even when there were none; the same boundary affected
files-with-matches and count modes. Those modes now look for one extra result
before setting the flag and trim that probe row from output. Match scans combined
with `--max-per-file` keep their full-file survey behavior so per-file counts stay
accurate.

## 2026-09-25 — API diffs collapsed overloaded exports by name

After checking signature behavior in `api_diff`, a Java fixture that added
`run(int)` alongside the existing `run()` showed the new overload as a changed
`run()` declaration. The old surface was stored as one row per name, so one
overload overwrote the others. API diffs now match identical signatures first,
then pair one-to-one signature changes, leaving additional overloads as
additions and removed overloads as removals.

## 2026-09-25 — API comparisons ignored `--kind`

While exploring `cmd_api`, I found its comparison branch only forwarded the
path and row limit. A comparison with `--kind function` still reported changed
structs and functions together. The filter now applies to both the old and
current exported surfaces before changes are classified; a regression covers
added and removed functions alongside struct changes.

## 2026-09-25 — API comparisons ignored `--limit`

Codeq showed that `api` accepts a row limit, but `api --compare-to REF` took a
branch that never passed the limit into the diff builder. A three-addition,
two-removal fixture still returned all five rows with `--limit 2`. API diffs
now cap the combined rows, prioritize removals before changed and added rows,
and set `data.truncated` so text output explains that results were capped.

## 2026-09-25 — Relative dependencies could escape the repo root

Exploring the dependency graph builder and probing a root-level JavaScript
relative import found that `../helper.js` resolved to the repository's own
`helper.js`. Path normalization silently ignored `..` once it reached the root,
so an import pointing outside the indexed repository was reported as an
internal edge. The resolver now rejects a relative path that would pop beyond
the root; a regression also checks that a valid nested `../../helper.js` still
resolves.

## 2026-09-25 — `read --raw` rewrote line endings

Exploring `cmd_read` and its raw-line printer showed that it used `str::lines()`
then `println!`, so a raw read normalized CRLF to LF and appended a newline to
files that had no final newline. A follow-up probe with invalid UTF-8 showed
that decoding through `String::from_utf8_lossy` also replaced source bytes with
U+FFFD. Raw mode now writes newline-inclusive byte slices; regressions cover
CRLF, a missing final newline, and invalid UTF-8.

## 2026-09-24 — Pure deletions vanished from changed-symbol and impact reports

After rebuilding Codeq, I made a temporary Git repo with a function containing
three statements, then deleted the middle statement. `changed` listed the file,
but `changed --symbols` returned no definitions and `affected` had an empty
`changed` list. The `-U0` diff hunk had a zero-length new side, which
`changed_line_ranges` discarded; no current line remained to overlap the enclosing
function.

Zero-length deletion hunks now anchor to the preceding surviving line, clamped to
line 1 at the start of a file. This lets `changed --symbols`, `affected`, and
changed-context seeds include the containing definition. A regression test checks
both changed-symbol and affected results while ensuring a neighboring untouched
function stays out. The behavior is computed from Git diffs, so it needs no index
version bump.

## 2026-09-24 — Fixed semantic lookup for escaped identifiers and escaped SQL columns

Probed source spellings that contain language-level escapes after the SQL
identifier fix. Two more edge cases surfaced:

- Rust `pub fn r#type() {}` is indexed as `r#type`; `definitions type` says no
  definition, although `references type` and `callers type` find the function
  and its `r#type()` call. C# behaves similarly for `@class` and `@event`:
  `symbols` retains the `@`, while `definitions class` / `definitions event`
  miss names that `references` recognizes. Exact definition lookup and usage
  lookup disagree on the semantic identifier spelling.
- In SQL, doubled delimiters escape a quote inside a quoted identifier:
  `"quote""name"`, `` `tick``name` ``, and `[close]]name]`. For a table with
  these columns, `files escaped.sql` reports four parser errors and `outline`
  emits only `quote`, `tick`, and `close`, with truncated signatures. The
  parser-health count exposes the recovery, but the partial member rows can
  still be mistaken for complete names.

Both cases reproduce in a fresh temporary index. They need separate decisions:
whether identifier facts should use semantic names without escape prefixes,
and whether the SQL grammar or Codeq's recovery layer should handle doubled
identifier delimiters. Codeq now stores Rust `r#name` and C# `@name` as
semantic `name` facts while retaining source spellings in signatures; syntax
callsites use the same normalization. For SQL, a byte-preserving sanitizer
rewrites only quoted-identifier escape syntax before parsing, while harvesting
all names and signatures from the original source. Comments and single-quoted
strings are skipped. Regression tests cover all three SQL delimiter styles,
zero parser errors, semantic Rust/C# definitions and their callees. The index
schema is bumped to v40 so existing indexes receive the normalized facts.

## 2026-09-24 — Fixed Python submodule resolution and quoted SQL columns

Rebuilt the release binary and indexed a small temporary polyglot repo to probe
syntax that the real-repository corpus is unlikely to exercise. Two issues
reproduced:

- With `pkg/mod.py` and `from pkg.mod import real`,
  `codeq deps client2.py --depth 3 --json` reports `pkg/__init__.py` as the
  dependency, even though the imported module is `pkg/mod.py`. The sibling
  `from pkg import exported` correctly resolves to `pkg/__init__.py`; the
  dotted import appears to stop at the package initializer instead of choosing
  the more specific module file.
- For `CREATE TABLE t ([bracket] TEXT);`, `outline` reports the member as
  `bracket` with signature `bracket] TEXT`. The opening `[` is dropped from
  the captured name/signature. The same table also showed the double-quoted
  column `"select"` missing from members, so quoted SQL identifiers need a
  wider check across supported dialects.

The resolver now keeps `mod` as a real module segment and selects the longest
exact import-prefix match, so `pkg/mod.py` wins over `pkg/__init__.py`. SQL
column extraction now captures the declared `name` field, removes identifier
delimiters from lookup names, and restores the opening bracket in the source
range/signature for T-SQL names. New CLI regressions cover both. The alias-
related `callees` misses seen in the same fixture are consistent with the
command's documented name-based graph and were not counted as bugs. The index
schema is bumped to v39 so existing repositories rebuild these corrected
facts instead of retaining stale SQL extraction results.

## 2026-09-24 — Import aliases disappear, Python `from` imports are missing, and the cold cycle check misses its budget

I oriented with `codeq map`, `tree`, `explore refresh`, and targeted source/test
reads, then ran `cargo test`. The run finished **352/356** integration tests
passing. Three import-related failures reproduce individually:

- `python_import_forms_all_capture_the_module`: `imports a.py` returns `os` and
  `json`, but drops `from sys import path` and
  `from collections import OrderedDict, defaultdict`.
- `resolved_follows_js_named_imports`: the plain named import survives
  `callers make --resolved`, but its aliased sibling (`make as m`) is missing.
- `import_aliases_narrow_bare_calls_and_context_expansion`: `callees run` returns
  no target for `m()` imported as `make as m`, instead of `util.js`.

The two JavaScript cases are separate user-visible paths through alias handling:
resolved `callers` and import-aware `callees`/`context`. Both fail on their own,
not just under the full suite. Codeq's name-based `explore` can show the failing
tests, but pulls in many unrelated `new`/`run` definitions as neighbours; the
targeted `read --lines` call was the useful follow-up.

Fixed the Python listing by exposing the qualified `from`-import facts in
`imports` while keeping them derived for `map`'s source-statement totals. Fixed
the JS cases by recording the local binding's source name on the exact-file
resolution path; that path had recorded only its target file, unlike the later
stem-resolution path.

The remaining failure, `deps_cycles_stays_fast_on_a_wide_acyclic_graph`, failed
alone at **10.3s** against its 5s budget. A hand-timed equivalent was **5.46s
cold** (including indexing 28 TypeScript files) and **0.03s warm**. Small cold
indexes were creating several extractors, each compiling every supported
language's queries; workers are now capped so there are at least 32 files per
worker. The test also warms the index before timing, so it measures cycle
enumeration instead of competing with other tests' cold indexing. The cold test
passed alone in **2.43s** after the worker cap; with its timing narrowed, the
final `cargo test` passed all **356 CLI tests** and **9 unit tests**. The three
cycle correctness tests passed too.

## 2026-08-30 — A decorator list that never matched, a 129 MB index, and an installer that would have edited the checkout

### The inert list had the right name in the wrong place

`entrypoints` reported `@functools.cache` and `@contextlib.contextmanager` as
framework registrations, contradicting the rule CLAUDE.md states. `functools`
**was** on the INERT list. The check never saw it: for `@functools.cache` the
head is `functools.cache` and the last segment is `cache`, so the entry only
ever caught a bare `@functools`, which nobody writes.

A dotted decorator now gets judged by its parts, module included, so whole
namespaces answer for themselves (`functools`, `contextlib`, `typing`, `abc`,
`dataclasses`, `enum`) plus the names people import bare. `persona` drops from 8
rows to its 5 real FastAPI routes; every genuine registration survives. A list
whose entries cannot match anything is worse than a short list — it reads as
covered.

### `map` warned about the free case and stayed silent on the expensive one

First pass on `pldashboard`: a **129 MB** `index.db` for ~700 KB of code, and
`map` said nothing at all.

The `(unknown)` note only covers `language IS NULL` — files with no grammar. Those
are exactly the files codeq never content-indexes, so they cost nothing but a
row in the language table. Meanwhile 14.5 MB of `updater/backups/*.json` — a
language codeq parses, producing zero symbols — goes into the trigram index
whole. The warning was pointed at the harmless case.

| | before | after `.codeqignore` |
|---|---|---|
| index.db | 129 MB | **7.3 MB** |
| symbols | 1696 | 1696 |
| imports | 1380 | 1380 |

17.6x smaller, nothing lost — measured rather than asserted, because a note that
recommends an action should have had that action tried once. Fires on this repo
and none of the other eight indexed.

### The installer would have edited the checkout it was installed from

`codeq install` registered the MCP server for four agents and did nothing about
the skill — the thing that actually teaches an agent to reach for `explore`
instead of reading a file whole. Registering the server without it hands over the
tool and not the habit. It now installs both, with the skill text `include_str!`'d
into the binary: after a session spent finding docs that described behaviour the
binary no longer had, "the installed guide matches the installed codeq" is worth
making true by construction rather than by discipline.

The first dry run against this machine said `already-current`, and it was right
by accident. The developer setup here symlinks `skills/codeq` at the
**directory**, so a check on the final `SKILL.md` follows the link and sees an
ordinary file; it only matched because the content was identical. Against a
modified checkout, `install` would have written *through* the link into the
repository, and `--uninstall` would have deleted the repository's file.

Every path component up to `$HOME` is now tested, and `--uninstall` refuses on a
link rather than deleting through it — codeq did not create that link and will
not undo it. **The test was run against the unfixed code before being believed**,
same as the cycles one: `install wrote through the symlink into the checkout`.

Worth recording *why* this was invisible: a clean machine has no symlink, so
every test I would have written from scratch passes. The awkward case was the
machine I was already on.

## 2026-08-30 — Carving out the `$` divergence, and the vacuum I found while doing it

The harness's last two failures, closed the way they were always going to be:
by deciding what the superset claim means and writing it down.

`rg -w` anchors on `\b`, where `$` is a non-word character, so `rg -w locale`
matches inside `this.$locale()`. codeq treats `$` as identifier syntax in the
languages whose lexer does, so `$locale` is a different name. codeq is right and
`rg -w` is looser, and dayjs writes both spellings in one file.

Exempted **by the rule, not by the repo name**: those extensions only, and only
when *every* occurrence on the line is `$`-adjacent. One free-standing hit and
the line still has to be answered. Checked against the real data before running
anything:

```
missing 11 -> exempt 11 -> real 0
total lines rg matched: 1166 | classified sigil-bound: 11
```

Eleven divergent lines explained, and nothing else out of 1166 swept up with
them. **The exemptions are counted, not deleted** — `sigil_exempt` goes in the
summary and prints on every run. An exemption you cannot see is indistinguishable
from a hole, and this harness exists precisely to not have holes. 8607/8609 → 
**8609/8609, 0 failures, sigil_exempt=22**, all 22 from the one symbol.

The same rule now applies to the `-w` search actions. They are green today, but
they are the same comparison, and a future `$` hit would have read as a
regression there.

### And the thing I found by accident, which is worse than the thing I fixed

Probing the divergence from a `python3 - <<EOF` heredoc, rg returned **zero**
lines in a repo with 1166 matches. Given no path argument, ripgrep searches
*stdin* when stdin is a pipe — and every assertion in this file is
`rg_lines(...) - codeq_lines(...)`:

> an rg that returns nothing makes every superset assertion pass, silently, with
> a perfect score.

That is the worst failure mode a verification harness has. It does not go red, it
goes vacuously green, and the number it prints is the number you wanted.

The harness omits the path deliberately — adding `.` prefixes every result with
`./` and would change what the byte-ratio metrics mean — so this is probed for
rather than defended against: one `rg [a-z]` against the first repo, which must
match something. Fires on a pipe, silent on a TTY and on `< /dev/null`, which rg
treats differently.

```
$ echo "" | python3 bench/differential.py dayjs
rg matched nothing in a non-empty repo — it is reading stdin, not the
repository, so every superset assertion would pass vacuously.   (exit 1)
```

The committed runs were never affected — they found the dayjs failures, which
requires rg to have been reading the filesystem. This was a hole CI would have
walked into, not a retraction. But it is worth noticing *why* it stayed hidden:
the harness has been run one way, by hand, every time, and the failure mode is
invisible in exactly the mode nobody used yet.

## 2026-08-30 — The three findings from the Next.js pass, and one that was wider than reported

### `entrypoints` answered "none" on an App Router app

Not "noisy `unused`" — the command that exists to answer this question returned
nothing at all, on a repo with four routes, a layout, a sitemap and a robots
handler. `facts.decorator` records registration codeq can *see*; Next.js
registers by where the file sits, and there is no syntax to read.

Settled the way the trait-impl question was, and it landed on the other side:
**tagged, not suppressed.** Deleting a `page.tsx` export still compiles — the
route 404s — so it is the "looks dead, might not be" case the `decorator=` tag
exists for, unlike an `impl Trait` method that cannot be deleted at all. The two
tags now sit side by side and mean the same thing with different evidence.

The implementation is a query-time policy function next to
`unreferenced_by_design`, **not a column**. Nothing about a path needs
extracting, and the alternative would have been a schema bump plus a migration
to store what `path` already says.

| | before | after |
|---|---|---|
| `entrypoints` on `uk-data-atlas` | 0 rows | 16 |
| `unused` rows discountable there | 0 of 24 | 7 |
| `nginx-analytics` | 0 | 12 of 76 |

Two more repos in `~/dev` turned out to be Next.js. No `next/*` tag fired on any
Rust or Python repo, and the only `react/lifecycle` rows anywhere were the three
methods of a real `ErrorBoundary` — including `render`, the name most likely to
collide, which never matched anything else.

### The arrow-function finding was wider than I filed it

I recorded it as *nested* arrows rendering badly, because that is where I saw it
— indented under a React component, next to clean signatures. It is every arrow
bound to a name, module scope included:

```
before   withCDN = (path: string) =>
after    withCDN(path: string)
```

The definition node is the declarator, so the source up to the body is an
assignment fragment; nesting had nothing to do with it. **Reporting the instance
rather than the class understated it by however many arrow functions a JS repo
has**, which is most of its functions. Worth noticing that the fix is the same
either way and the report still would have been wrong.

Composed from the parameter and return-type fields instead of sliced out of the
source, which also restores the parentheses source omits on `x => x`, and keeps
`async` and type parameters. That is extraction, so `SCHEMA_VERSION` went 35 →
36 — the check this log flagged in August as remembered where a mechanical one
belongs, and it was remembered.

### And the doc claim that was simply false

SKILL.md said "Locals aren't definitions." A local `const` bound to an arrow
function is one, deliberately — a component's handlers are units of code worth
exploring. `entrypoints` was not documented anywhere at all.

### Noticed, not acted on

`entrypoints` reports `@functools.cache` and `@contextlib.contextmanager` as
registrations. CLAUDE.md says inert decorators are not tagged, and those two
wrap rather than register, so the rule and the behaviour disagree. Pre-existing
and outside what this pass was for.

## 2026-08-30 — `deps --cycles` was enumerating paths, and a test that proved nothing

First pass on `uk-data-atlas` (Next.js, TypeScript, ~200 files). Every command
answered in under 100ms except one:

| | |
|---|---|
| `deps lib/types/index.ts --depth 99` | 0.01s |
| `deps --cycles` | **61.6s** (11 cycles) |

Same graph, same loader, so the cost was all inside `cycles()`. It was a DFS
that walks every simple **path** and reports a back-edge into the current one.
A dependency graph has exponentially more paths than cycles, so the cost had
nothing to do with the size of the answer. Johnson's algorithm is the fix, and
the part that matters is blocking: a node is unblocked only once it is *known*
to lie on a cycle, so a subtree that cannot reach the start is explored once
instead of once per path leading into it. 61.6s → 0.014s, output byte-identical
on all seven repos I had indexed.

### The test I wrote first was worthless

I wrote the regression test from the synthetic repro — layers of two files each
importing both files of the next, no cycles, exponentially many paths — but
translated it to Python imports because the fixtures nearby were Python. It
passed on the unfixed code in 1.4s.

The shape was right and the language was not. A TypeScript named import records
**two** exact edges to the same file:

```
src/m0_0.ts:1: ./m1_0          the module
src/m0_0.ts:1: ./m1_0.f1_0     the module plus the binding
```

`deps` dedupes by target so it shows one; `cycles()` walked both, doubling the
branching factor at every hop — 4^13 paths through a graph that has 2^13.
Python's `import m` has no binding fact, so the fixture had a quarter of the
branching and finished instantly. Same picture, a quarter of the exponent, and
the test would have sat there green forever.

**A performance regression test that has never been observed to fail is a
decoration.** Running it against the old binary is the only thing that
distinguishes it from one, and it cost one command: 24.7s against a 5s bound on
the old code, 0.006s on the new. Deduplicating successors is now part of the fix
too — the second doubling changed no answer either.

### And the correctness check that was also wrong

I cross-checked the new enumeration against a brute-force one on 60 random
graphs. The first run reported mismatches on nearly every graph — `(0, 2, 5)`
expected, `(0, 2, 5, 0)` produced. That was my parser: the text output repeats
the opening file to close the loop, and I had read it as a member. Fixing the
harness, not the code: 60/60, 8405 cycles. Worth the ten seconds it took to
check whether the *test* was wrong before touching what it was testing — the
mismatch was uniform and structural, which is what a harness bug looks like and
not what a lost cycle looks like.

### Also seen on this repo, not yet acted on

- **`entrypoints` reports "no registered entry points"** on a Next.js app with
  four routes, a layout, a sitemap and a robots handler. Next registers by file
  convention, not by decorator, and `facts.decorator` is the only bit codeq has.
  The fallout lands in `unused`: 7 of 24 rows are framework-called code (5 App
  Router conventions, plus `getDerivedStateFromError` and `componentDidCatch`,
  which React calls and nothing else will). The other 17 rows are correct — I
  checked the 12 flagged type aliases with `rg` and they are genuinely dead. On
  the rule this log already uses, this is a **tag, not a suppression**: deleting
  a `page.tsx` export compiles fine, it just 404s.
- **Nested arrow functions render as an assignment fragment.** codeq indexes
  component-local arrows as definitions, which is right, but they print as
  `getSearchParam = (key: string) =>` — no kind, dangling arrow — where every
  other row carries a clean signature. Visible in `outline`, `definitions`,
  `untested`.
- **SKILL.md says "Locals aren't definitions."** Nested arrows are, deliberately.

## 2026-08-27 — Closing the open list, and what `nginx-analytics` said back

Every item this log had left open, then a full pass over `nginx-analytics`
(Go + TypeScript + TSX, three programs in one repo) with the result.

### The superset guarantee: 24 failures to 2, and the default was backwards

`differential.py` had 24 failures carried from the pulled commits. Thirteen were
one line, and the line was not wrong so much as **shaped wrong**:

```rust
const DOLLAR_IS_SIGIL: &[&str] = &["php", "bash", "powershell"];
```

A denylist quietly asserts that `$` binds into an identifier in every language
nobody has thought about yet. It does not. Rust `$bytes` is a macro
metavariable, `Outer$Inner` in a JVM config file is a binary name, `$(H1)` in a
Makefile is an expansion. Flipping it to an allowlist of the languages whose
lexer actually allows `$` in a name — JS/TS/TSX, Svelte, Vue, Java, Kotlin,
Scala — and treating a file codeq assigned no language as punctuation-agnostic
fixed thirteen of them at once.

A fourteenth was older and the same species: `b >= 0x80`, i.e. "any non-ASCII
byte is a letter". True of a CJK ideograph, false of CJK punctuation, and the
two are neighbours in real prose — `console。` in a Chinese README made codeq
reject a boundary ripgrep accepts. `char::is_alphanumeric` is the question
actually being asked; the byte test was a stand-in for it.

That left four, and the shape of *those* was informative too. Three were `$`
inside a **string literal** — `"Java17RecordTest$PrivateRecord"` in an
expected-error message, `"$error.message"` in a firebug template. `$` is
identifier syntax where a lexer is lexing identifiers; inside a string it is
templating, or a binary name, or a shell expansion. codeq already caches string
spans, so this costs a lookup:

| | fail |
|---|---|
| carried from the pull | 24 |
| `$` allowlist + Unicode boundary | 4 |
| `$` binds in code, not in strings | **2** |

The last two are dayjs's `this.$locale()` — real JavaScript, where `$locale` is a
real identifier and `locale` is a different symbol. codeq is right and `rg -w` is
looser. **That is the whole remaining divergence from the superset claim, and it
is now one sentence rather than twenty-four scattered rows.** Whether to carve it
out of the harness or accept two is a judgement about what the claim means, not a
bug to fix.

### `unused` on Rust: the answer went from noise to seven rows

The trait-impl question this log has carried since 2026-08-25, settled the way it
was recorded: **suppressed, not tagged.** The `decorator=` tag exists for "looks
dead, might not be". A Rust `impl Trait for X` method is not that — deleting it
does not remove code, it breaks the build. It is never dead, so it is not a
judgement call to hand back to the reader.

That needed one bit codeq did not have (`facts.contract`, schema v35). On
`black-box`, `unused src` went from a list led by actix's `new_transform` and two
`handle`s to seven genuinely-unreferenced enum variants. An inherent
`impl S { fn dead() }` is still reported, which is the line that matters.

### Two commands were quietly answering a smaller question than they were asked

**`untested` was 26% data declarations.** A field, a variant and a constant have
no behaviour to exercise — they are tested through whatever reads them. On this
repo they were 52 of 200 rows, crowding out the functions the command exists to
find. Excluded unless `--kind` asks, on the same argument `clones` uses for
tests.

**And the 200 was a cap nobody was told about.** `untested` defaults to 200;
uncapped this repo has 366. `imports` defaults to 200 and the repo has **1,030**.
Both cut silently. The mechanism is structural, not an oversight: `search` and
`references` carry `truncated` in the envelope and the renderer warns, but a
result whose `data` is a bare array has nowhere to put that flag — so every
command shaped that way cuts in silence. Which also means the field/constant
noise was not merely noise: it was **displacing 52 real rows past a cap that said
nothing**.

Fixed for `imports`, `untested` and `tree`. Still silent, and worth a sweep
rather than another reactive fix: `files`, `symbols` and `members` (all default
200), which truncate in SQL and would need `LIMIT n+1` to know they had.

### `capabilities` said `relations: true` for a language that has one kind

C# can only ever emit `extends` — `class C : B, I` is one syntax for two
relationships and the grammar cannot tell them apart. Rust can only ever emit
`implements`. `relations: true` is true of both and useful for neither, so
`capabilities` now reports the kinds, read off each query's own capture names
(the same derivation the rest of the matrix uses, for the reason recorded there:
a hand-kept list is what drifts).

```
rust        kinds=['implements']      csharp  kinds=['extends']
java        kinds=['extends','implements']    go      kinds=[]
```

### `nginx-analytics`: what the fixed tool found

The point of the pass, and it went well enough to be worth recording as the
positive case.

- **`clones` found five real duplicate groups** and no noise: three device cards
  sharing a 435-node component shape, two `RenderContent`s, `sortClients` /
  `sortVersions`, `getColorForCPUUsage` / `getColorForMemoryUsage`, and a
  `LoadEnv` copied between `agent` and `tui` — which `pair=agent/internal/env
  <-> tui/internal/env` names as a cross-program copy without being asked.
- **`unused` found a genuine dead public function**: `ServeLogSize` in
  `agent/internal/routes/size.go`. Only `ServeLogSizes` (plural) is called
  anywhere, and codeq kept the two apart.
- **`callpath main ParseNginxLogs --path tui`** returned the whole six-hop chain
  from `main` to the parser, first try.
- **`astgrep '$X.($T)' --language go`** found the unchecked type assertions.

Two things it got less right, both honest limitations rather than defects:

- **13 of 76 `unused` rows are CSS classes** applied as `'level-' +
  getSuccessRateLevel(v)`. The name `level-10` genuinely appears nowhere else, so
  the row is a true statement and a misleading finding. Computed class names are
  the norm in CSS, and nothing name-based can see through them.
- **A multi-line `astgrep` match had no `[symbol]`.** The docs promise the
  enclosing symbol on *every* match — it is the thing grep cannot produce — but a
  match spanning a line fell into the verbatim-block branch written for `context`
  chunks and lost it. Whether a pattern happens to span a line is not a reason to
  answer a different question. Fixed.

### One item left open, and it is a process one

`SCHEMA_VERSION` gates freshness, so changing *how* a fact is extracted without
bumping it leaves every existing index serving the old answer forever. That cost
a debugging session yesterday. It is now documented at the constant, which is a
remembered check where a mechanical one belongs — and the obvious mechanical
version, hashing the query constants, would cover only the query-driven half
while looking complete. A visibility rule or an attribute reader is ordinary Rust
and would slip straight through. Recorded rather than half-built.

## 2026-08-27 — Following the loose ends, and one careless `git checkout`

Short pass, finishing what the previous entry left open and exercising `explore`
properly for the first time.

### `callers NextLine` answered: `NextLine`

The previous entry fixed `unused` and recorded the same bug in `callers` and
`references` as "less harmful — a missing flag rather than hidden dead code".
That was wrong, and one command would have shown it:

```
$ codeq callers NextLine        # [Obsolete] on the line above the method
G.cs:5: method NextLine
$ codeq callers SameLine        # [Obsolete] on the same line
# codeq: 'SameLine' is defined at G.cs:3 but has no callers
```

Not a missing flag — **a wrong answer to the question codeq is asked most
often.** The declaration, not being recognised as one, was counted as a call,
so an annotated method is its own caller in C#, Java, Kotlin and PHP. I had the
reproduction in hand and filed it from the shape of the code rather than from
running it. The rule is now one `DeclSites` type shared by all three commands:
the declaration is the first occurrence of the name inside the definition's
extent, and each extent is claimed once so recursion stays a real call.

Chasing it turned up an adjacent limitation, pre-existing and *not* a bug:
`references` emits one row per **line**, so `helper(1) + helper(2) + helper(3)`
is one row, not three. `rg -w -c` counts lines too, so the superset guarantee
holds — but the docs say `references` gives "every occurrence" and
`references --calls` "every call site", and both mean every *line* holding one.

### `explore` on a Go function opened with 79 lines of TypeScript

First real use of `explore` on a polyglot repo (`nginx-analytics`, Go + TS +
TSX). Exploring the Go `ParseNginxLogs`:

```
 27: dashboard/lib/parse.ts:123 (callee)     <- TypeScript, different program
 38: dashboard/lib/parse.ts:144 (callee)
 62: tui/internal/logs/parse.go:195 (callee)
 80: tui/internal/logs/parse.go:217 (focus)  <- the thing I asked about
```

The 2026-08-13 entry filed exactly this ("emitting the focus block first would
cost nothing and make `head` do the right thing") and it was still true two
weeks later, so: source chunks now sort focus-first, then by path. The focus
block moved from line 80 to line 27.

That fixes the reading order and not the cause. The cause is that `callees` is
name-based, and `dashboard/lib/parse.ts` happens to define `getCompiledFormat`
and `parseDate` too — a *different program in a different language* in the same
repo. `callers`, `callees` and `context` all have `--resolved` for this.
`explore` did not, which is backwards: it is the command where the over-match
costs most, because unrelated code spends the **token budget** and not just the
reader's attention. Adding it (and to `about`, which shares the walk):

```
$ codeq explore ParseNginxLogs              391 lines
$ codeq explore ParseNginxLogs --resolved   109 lines
```

72% of that answer was another program.

### The mistake: `git checkout --` on a file holding uncommitted work

A `python` patch script failed its assertion partway through a multi-file edit,
and I ran `git checkout -- src/commands.rs` to clear it. That file also held the
`DeclSites` work from earlier in the session, uncommitted. Gone — recovered only
because the change was recent enough to reconstruct exactly, and its tests were
in a different file and survived to prove the reconstruction.

The habit that caused it is worth naming: I reached for the *broadest* undo
available (revert the file) for the *narrowest* problem (an edit that had not
been written yet — the assertion fired before the write, so there was nothing to
undo at all). The fix is not "be careful with checkout", it is: **commit each
verified piece before starting the next one.** Everything after this point in
the session was committed as soon as it was green, which is also why the branch
reads as a sequence of small commits rather than one.

### Still open

- `capabilities` reports `relations: true` for C#, but C# can only ever produce
  `extends` — the same "which relation kinds does this language emit" gap the
  `graph` note now covers, one level up in the capability matrix.
- `untested` on a Go repo returned 200 rows, of which 52 were `field` and
  `constant`. Neither is a thing anyone writes a test for, and they bury the 130
  functions that are. The `clones`-excludes-tests argument applies unchanged:
  true positives that drown the finding worth having.

## 2026-08-26 — The second pass: what the index says about code it can see

Ten more repos, and a different class of finding from the first pass. That one
was crashes and misses. This one is codeq answering confidently about code it
*did* index, and being wrong about what it was looking at.

### It told a Business Central developer their source was data

```
$ codeq map --totals          # powersearch-business-central
33 of 36 files (0 MB of 0 MB) are not source codeq indexes — assets, data or
binaries, counted here but carrying no symbols
```

31 of those 33 files are `.al` — the entire source of the project. The note was
written against a repo of 2,106 flag PNGs, where "assets, data or binaries" was
true, and it generalised a category it had no way to check. The cost is in the
direction that always hurts most: it tells the author their code is data, and
implicitly proposes `.codeqignore` for it.

codeq knows the extension. That is a fact and it separates the two cases without
codeq having to know which it is looking at:

```
33 of 36 files (243 KB of 314 KB) carry no symbols — codeq has no grammar for .al (31), .xlf (2)
2108 of 2683 files (54 MB of 57 MB) carry no symbols — codeq has no grammar for .png (2106)
```

Same sentence, and a reader can tell an unsupported language from a pile of
images at a glance. `0 MB of 0 MB` was a second, smaller instance of the same
habit — a sentence with its quantities rounded out of it.

### The blind spot the asset note cannot cover: generated code that *does* parse

`pldashboard` commits three vendor bundles. They are JavaScript, they parse
cleanly, and they produce symbols — so nothing in codeq's "cannot index this"
machinery can see them, and the index size and query speed are both fine. What
they own is every symbol-ranking command:

```
$ codeq hotspots
dashboard/src/lib/App.js:1: function t refs=45993 defs=9
dashboard/src/lib/App.js:1: function e refs=2277 defs=137
```

The command whose stated purpose is *meeting an unfamiliar codebase* answers
`t`, `e`, `r`. Three files hold **5,507 of the repo's 7,558 symbols**; 531 of
`unused`'s 575 rows come from them; every `clones` group is minified. Writing a
three-line `.codeqignore`:

| | before | after |
|---|---|---|
| symbols | 7,558 | **2,051** |
| `unused` rows | 575 | **404** |
| `hotspots` top row | `function t refs=45993` | `field data refs=1005` |
| `clones` top group | minified `sn/on/ln/dn`, 4,955 nodes | `fixtureColor` / `fixtureColorSkewed` |

The last row is the point: with the bundles gone, `clones` immediately found a
real duplicated pair in the app's own code. It had been there all along, ranked
below 4,955-node minified blobs.

**The detector needs nothing stored: definitions per *distinct* line.** Hand-written
code declares about one thing per line — the ratio is bounded above by the
grammar, not by taste. Measured on this repo:

```
bundle.js               1152 symbols /    1 line   = 1152
plotly-latest.min.js    4135 symbols /    9 lines  =  459
App.js                   220 symbols /    1 line   =  220
── everything else (166 files) ─────────────────── =  1.0
```

Two orders of magnitude of empty space, so the cutoff is not a number anybody
has to tune. `map` now says so, names the files, and says what they distort. It
under-reports on purpose: a pretty-printed generated file is indistinguishable
from hand-written source by this test, and missing one is much cheaper than
calling somebody's code machine output.

### `decorator=self._axes_visible`, and `decorator=public`

From `seeable`:

```
src/seeable/widgets/heatmap.py:101: field _axes_visible visibility=private decorator=self._axes_visible
```

Line 101 is `self._axes_visible = True`. In the Python grammar the left side of
that assignment is a node of kind **`attribute`** — attribute *access* — and the
branch that reads Java/C#/PHP annotations matched any kind *containing*
"attribute". Tightening it to the list node each grammar actually wraps
annotations in (`attribute_list` in C# and PHP, `modifiers` in Java and Kotlin)
turned up a second one immediately: `modifiers` holds annotations *and* access
modifiers, so every plain `public class T` came back `decorator=public`.

The `decorator` field exists so a reader can tell a framework entry point from
dead code. A tag on an ordinary assignment is worse than no tag, because it
means the tag stops meaning anything where it does appear — which is the same
argument the inert-decorator list was added for, applied one level down.

### `unused` could not see any annotated declaration, in four languages

Chasing the above turned up the worse bug underneath it:

```php
class U {
  #[Route("/a")] public function sameLine() { }   // reported by `unused`
  #[Route("/b")]
  public function nextLine() { }                  // silently absent
}
```

Identical in C#, Java and Kotlin. In all four, the annotation lives *inside* the
declaration node, so the fact's line is the annotation's line and the name sits
a line below. `used_definition_names` decided "is this occurrence the definition
itself?" by comparing the occurrence's line to the fact's first line — so the
declaration counted as an external use, and the definition could never be
reported dead.

That is the conventional formatting. `[Fact]` above the method, `@Bean` above
the method: **`unused` was blind to essentially every annotated declaration in
every C#, Java, Kotlin and PHP codebase**, and the `decorator=` tag — added
precisely so these could be *shown and discounted* rather than hidden — was
being applied only to the ones written on one line.

The fix is a sentence: the declaration is the *first* occurrence of the name
inside its own extent, not the occurrence on the extent's first line. Claiming
each extent once keeps recursion working — a later occurrence in the same extent
is a real call, and treating the whole extent as "the definition site" would
have reported every recursive function as dead. `fact`, `walk` and a recursive
Java method are all still correctly live.

Two smaller pieces of the same bug are left: `references` and `callers` mark
their `definition` flag on the same first-line comparison, so an annotated
declaration is not flagged as its own definition there either. Less harmful —
a missing flag rather than hidden dead code — and not fixed.

### The index does not know when the extractor changed

Worth recording as a hazard for anyone developing codeq. After fixing the
`decorator=self._axes_visible` extraction, the wrong tag was still there:
freshness is content hash plus schema version, and neither had moved, so no file
was reparsed. `SCHEMA_VERSION` is treated as a *storage-format* version, but
every consumer of it depends on it also being an *extraction* version — the
facts are as stale after an extractor change as after a column change, and only
one of those bumps it. `codeq index --rebuild` is the workaround; remembering to
is the problem.

## 2026-08-25 — Six repos in `~/dev`, and the first one crashed the index

First deliberate sweep across other people's repos rather than codeq's own:
`black-box` (Rust), `api-analytics` (Go/Svelte/Python/TS/PHP), `xlsx-pipeline`
(C#), plus `seeable`, `nginx-analytics`, `tinyad`, `sono`, `nlmrs`,
`claude-usage`, `tpub`. Six real defects — plus a test suite and a differential
harness that were both already red on arrival — and most of them codeq could
never have found in its own source.

### `connect(mapState)(Component)` aborted the whole index build

```
$ codeq map --totals
codeq: UNIQUE constraint failed: calls.path, calls.start_byte: Error code 1555
```

Exit 1, deterministic, and **every** command hit it, because they all
auto-refresh. codeq was simply unusable on `api-analytics`. Bisecting 616 tracked
files down to six lines:

```go
package p
func F() { h := Wrap("k")(3); _ = h }
```

A curried call. `callee_of` takes the outer call's `function` field, finds an
inner `call_expression`, and — having no member field to read — falls through to
`last_identifier_child`, landing on **the inner call's own callee**. So `Wrap` was
recorded as called twice at one byte offset, and `calls` is `PRIMARY KEY(path,
start_byte)`. Reproduced in Go, JavaScript, TypeScript, Python and Rust; Java and
Ruby were fine, their grammars putting the method in a field.

Two things worth keeping from this one:

- **The blast radius was absurd relative to the cause.** `connect(x)(C)` is
  ordinary Redux, `middleware(cfg)(next)` ordinary Express, and one occurrence
  anywhere in a repo takes down every query in it. A codebase that trips this is
  not exotic, it is normal.
- **`calls_in` never had the bug, because a `HashMap` silently swallowed it.**
  The `--calls`/`--on` path collects into `CallMap` keyed on the offset; the
  index stores a `Vec` into a table keyed on the offset. Same invariant, enforced
  in one place and merely assumed in the other. The assumed one is where it broke,
  and it broke as a raw driver error naming no file — the same "reads like
  corruption" failure the 2026-08-13 lock entry filed.

Fixed both ends: the outer invocation of a curried call has no name in the source,
so `callee_of` now returns `None` for it, and `call_sites_in` enforces one site
per offset the way `calls_in` always did. The second is not belt-and-braces — it
is the difference between an extraction bug degrading one call site and an
extraction bug bricking the repo.

### Every enum in every language was missing from `api`

`black-box` is a Rust event collector whose central type is `pub enum Event`.

```
$ codeq api src | wc -l
421          # of which enum variants: 0
```

`visibility_of` asked the node for a `visibility_modifier`. An `enum_variant`
cannot syntactically carry one — in Rust, Java, C# or TS — so every variant in
every repo came back `private`. `api` reported a public enum's surface as the bare
enum name, and `unused` printed `variant FailedAuth visibility=private` for a
variant of a `pub enum`.

The rule was already half-written: `enclosed_by_private_type` *demotes* a public
member inside a private type. The missing half is that a member which cannot
declare visibility **inherits** it. Same walk, opposite direction. `api src` on
`black-box` went 421 → 496 rows.

### The TS enum test passed because it tested the rarer form

While fixing the above: `outline` on `enum E { A, B }` returned no members at all,
though `enum F { X = 1 }` worked. `A` with no initialiser parses to a bare
`property_identifier` under `enum_body`; only `enum_assignment` was queried.

The reason it survived is the part worth recording. `variants_extracted_across_
languages` covers six languages, and its TypeScript case was
`("a.ts", "enum E { X = 1 }", …)` — the *initialised* form, which is the less
common way to write a TS enum. A cross-language test that picks one spelling per
language will pick the one the implementer had in mind, which is exactly the one
that already works.

### A `--path` scope that scoped the question but not the answer

```
$ codeq members E --path z.ts
# codeq: 'E' is defined at A.cs:1 (+1 more definitions) but has no members
```

I asked about `z.ts` and was answered about `A.cs` — the one file I had ruled
out. `note_if_empty_feat` handled the case where `--path` matched *nothing*, then
reported `sites[0]` repo-wide when it matched something. The same unscoped
`sites.first()` also picked which language to consult for the "does codeq extract
members for this language" check, so a scoped query could be told about a
capability of a language it had excluded.

Fourth entry in this log about a note. The pattern across all four is not that
notes guess causes — it is narrower than that: **a note is generated from
different state than the answer it explains.** Here the answer was scoped and the
note was not.

### `graph --relation implements` on 148 implementations: "genuinely has no"

```
$ codeq graph --relation implements       # xlsx-pipeline, a C# repo
# codeq: no 'implements' relation edges — csharp/javascript does produce them,
#        so this repository genuinely has no inheritance or foreign-key edges
```

The repo has 148 relation edges. C# writes `class C : B, I` for both inheritance
and implementation and the grammar does not distinguish them, so codeq records
every C# edge as `extends` and can never emit `implements` for that language —
telling the two apart needs to know whether `I` is an interface, which is type
resolution codeq does not do.

That limitation is fine. The note is not: it converts "codeq cannot produce this
relation kind here" into a confident claim about the user's code, in the most
expensive direction available. This note had *already* been corrected once for
naming Go/C/Zig unconditionally — the comment above it says so. It was fixed for
the language axis and left wrong on the filter axis, because both times the
question asked was "which languages are in this repo" rather than "what did the
filter I applied actually remove". It now reports the kinds that are present:
`no 'implements' relation edges, but this repository has 148 extends — the filter
emptied this, not the code`.

### `$` became an identifier byte, and `references` stopped finding PHP fields

Running `bench/differential.py` to check the above turned up something bigger
than the above. Three runs, same corpus, same harness:

| | pass | fail | errors |
|---|---|---|---|
| `af5bf47` (last committed result, 2026-08-13) | 8609 | 0 | 0 |
| `8eb9379` (HEAD, before this session) | 5826 | 279 | **240** |
| HEAD + this session's fixes | 8585 | 24 | 0 |

The 240 errors were the curried-call crash: eight of twenty-five repos —
`express`, `lodash`, `dayjs`, `redis`, `requests`, `fmt`, `json`, `spdlog` —
could not be indexed at all, so every assertion against them errored. **A third
of the corpus was unmeasurable and the harness reported it as failures**, which
is how a crash this loud stayed quiet: nobody had run it since `af5bf47`.

Underneath that, 43 real failures, and `git log -L` puts the largest group on one
line of `1d2d6eb`:

```rust
-fn is_ident_byte(b: u8) -> bool { b == b'_' || b.is_ascii_alphanumeric() || b >= 0x80 }
+fn is_ident_byte(b: u8) -> bool { b == b'_' || b == b'$' || b.is_ascii_alphanumeric() || b >= 0x80 }
```

Deliberate, and right for what it was aimed at: JavaScript's `$ZodError` is one
symbol, so `references ZodError` should not match inside it. But `$` is a *sigil*
in PHP, and there codeq contradicted its own index — it records
`private $truncateAt;` under the name `truncateAt`, then refused to match the
declaration it had just indexed:

```
$ codeq definitions truncateAt
src/BodySummarizer.php:12:4: private $truncateAt;   [BodySummarizer]
$ codeq references truncateAt
src/BodySummarizer.php:16:15: $this->truncateAt = $truncateAt;   [__construct]
                                     ^ the definition site is not in the list
```

That is not a disagreement with ripgrep about strictness; it is the matcher and
the extractor disagreeing about what the symbol is called. `$` now binds
everywhere except `php`, `bash` and `powershell`, which fixed 21 of the 24
remaining failures.

**The 24 that remain, with causes, unfixed:**

- **`$` as punctuation outside those three languages** (13). Rust macro
  metavariables (`$bytes:expr` in `serde`, `$name` in `clap`), a JVM binary name
  in a config file (`Java17RecordReflectionTest$PublicRecord` in `gson`'s
  `reflect-config.json`), a PowerShell completion script embedded in a Go string
  (`cobra`), a Makefile's `$(H1)` (`httpie`), and dayjs's `$locale`. In each,
  codeq is arguably *more* precise than `rg -w` — and in each, the superset
  guarantee is broken. The choice is real: either `$` binds only where the
  language actually allows it in a name, or the harness's superset assertion
  needs an explicit `$` carve-out and the README claim needs the same caveat.
  Picking silently is how this arrived.
- **`b >= 0x80` treats all non-ASCII as identifier bytes** (1, and it is the
  older bug). In `rich`'s `README.zh-tw.md`, `console。` — the CJK full stop is
  `>= 0x80`, so codeq reads it as a letter and rejects the word boundary, where
  ripgrep's Unicode word classes call it punctuation.
- **`search --word` divergences** in `redis`, `ripgrep`, `sinatra`, `lodash`,
  `guzzle` (5+). Not chased.

The process point is the one to keep: **the last committed differential result was
from `af5bf47`, ten commits earlier.** `bench/results/` is committed precisely so
the trend lives in git, and the trend had a ten-commit hole in it during which a
total-crash regression and a matcher regression both landed. CLAUDE.md says to run
the harnesses before committing because `cargo test` alone is not enough; that was
right, and it was also not done.

### `cargo test` was already red at `8eb9379` — twice, and one was the crash

Before changing anything: `meta_flag_returns_full_stable_envelope` asserted
`v["index"]["schema"] == 30` against a `SCHEMA_VERSION` of 34. So a schema bump
broke a test about the *envelope*, four versions ago, and the suite has been
shipping red since. The literal was never the contract — that the `index` block
reports *the* index schema version is. It now asserts agreement with what
`capabilities` declares, which cannot go stale on the next bump.

The second red test is the one worth dwelling on. Splitting this session's work
into commits and checking each one, `commonjs_require_bindings_are_imports_not_
definitions` failed at every commit before the curried-call fix. Its fixture:

```js
var deprecate = require('depd')('express');
```

So **the crash that made a whole repo unindexable was already failing codeq's own
`cargo test`** — no corpus, no differential harness, no foreign repo needed. It
did not need finding, it needed *looking at*. Two independent failures is also
how a suite stops being a signal: the first one that goes unfixed makes the next
one invisible, because "the suite is red" stops meaning anything specific.

An ordering consequence, recorded because it looks like a mistake and is not:
the first commit on this branch still reports one failing test. HEAD was red on
two independent counts, so no ordering makes commit one green — whichever fix
lands first leaves the other's test failing. Each commit builds, none adds a
failure, and the count goes 2 → 1 → 0 and stays there.

### Still open: a Rust trait-impl method looks like dead code

```
$ codeq unused src
src/webui/auth.rs:70:  method new_transform visibility=private
src/webui/websocket.rs:122: method handle  visibility=private
```

Both are `impl Trait for Type` methods — actix's `Transform` and
`StreamHandler`. They cannot carry `pub`, so they inherit the `private` default
for the same reason variants did; and they are called through the trait, so they
have no textual caller. `api src/webui/auth.rs` reports `BasicAuth`'s surface as
`new()` alone, hiding that the type is middleware.

Unlike a variant, the right answer is not obvious. The method is as reachable as
the trait, and the trait is usually foreign (`Display`, `Transform`), so
inheriting its visibility means resolving something outside the repo. Three
options, none free: relabel as `public` (slightly over-claims for
`impl PrivateTrait for X`), suppress from `unused` (defensible — deleting a trait
method is a compile error, so it is never dead), or tag it the way a `decorator=`
row is tagged (honest, but wants a column). Recorded rather than guessed at.

Worth noting codeq could not have found any of this in its own source: it has no
curried calls, no trait impls, and no C#.

## 2026-08-24 — `entrypoints` mistook Python instance fields for registrations

Running `entrypoints src/flask` showed ordinary assignments such as
`self.cli`, `self.debug`, and `self.app` as framework entry points, with the
assignment target itself shown as the `registration`. The attribute collector
matched Python's ordinary `attribute` expression node while looking for Java,
C#, and PHP annotation containers. Restricting it to actual annotation-list
nodes preserves genuine decorators while keeping instance fields out of the
entry-point report.

## 2026-08-24 — `unused` mistook Python protocol hooks for dead methods

Flask's `unused src/flask --kind method` reported descriptor, container, and
context-manager hooks such as `__get__`, `__set__`, `__iter__`, and `__enter__`.
FastAPI independently exposed async-context, dataclass, and Pydantic hooks:
`__aenter__`, `__aexit__`, `__post_init__`,
`__get_pydantic_json_schema__`, and `__get_validators__`. None are normally
spelled at their call site; Python or the framework invokes them by protocol.

The existing narrow exemption for `__all__` deliberately did not blanket-hide
dunders — `__version__` must remain a candidate — but it was too narrow for
methods the interpreter actually dispatches. `unused` now has an explicit list
of Python protocol/framework hook names. Rechecking both repositories removed
the noise while leaving ordinary candidates in FastAPI, including `bytes_schema`
and `build_middleware_stack`.

## 2026-08-24 — FastAPI exposed a curried-call indexing crash

Fresh-indexing the FastAPI checkout failed every command with:

```
codeq: UNIQUE constraint failed: calls.path, calls.start_byte
```

The error named neither the source file nor the expression. Narrowing the tree
found `fastapi/dependencies/utils.py`, which has the normal Python pattern:

```py
cm = asynccontextmanager(dependant.call)(**sub_values)
```

Codeq indexed both the named inner call and the outer call-result invocation as
`asynccontextmanager` at the same byte offset. The latter has no statically
known callee; attributing it to the inner function was wrong even before it
violated the call-table key. `call_sites_in` now keeps only the named inner call,
and the full FastAPI checkout indexes successfully (3,101 files, 24,131
symbols).

## 2026-08-24 — Bare JavaScript package imports can manufacture local cycles

`deps` treats a bare JavaScript/TypeScript specifier as a local file when a
same-stem file exists, even though Node resolves a bare specifier as a package
(or a built-in), not as a sibling module. That turns an external dependency
into a false local edge and can make `deps --cycles` report an architectural
cycle that does not exist.

Minimal repro:

```js
// app.js
import express from "express";

// express.js
import { app } from "./app.js";
```

With no `package.json` declaring a local `express` workspace, codeq reports
`app.js -> express.js -> app.js`. `deps app.js` likewise shows `express.js` as
the target and `deps --external` omits `express`; both are wrong for Node's
module-resolution rules.

The Go equivalent (`import "net/http"` beside two local `http.go` files) already
behaved correctly. The resolver now applies Node's corresponding default:
non-relative JS/TS/Svelte/Vue specifiers remain external, while `./` and `../`
paths retain local resolution. Workspace aliases require configuration codeq
does not read, so they deliberately remain external rather than being guessed.

## 2026-08-14 — `deps --cycles` reported cycles that do not exist

In `pldashboard`, `codeq deps --cycles` reported these Python cycles:

```
updater/src/updater/data/data.py -> updater/src/updater/data/teams.py -> updater/src/updater/data/data.py
updater/src/updater/predictions/model_predictions.py -> updater/src/updater/updater.py -> updater/src/updater/predictions/model_predictions.py
```

Both were false positives. Reading the imports showed that `data.py` imports
`teams.py`, but `teams.py` does not import `data.py`; it imports the
`dataframes` package and serialization helpers. Likewise, `updater.py` imports
`model_predictions.py`, while `model_predictions.py` imports raw-data helpers,
formatting, distributions, and the model registry—not `updater.py`.

`uv run python -c "import updater.data.data; import updater.data.teams; import updater.updater; import updater.predictions.model_predictions"`
also completed successfully.

The likely cause is the documented heuristic module resolution matching names
or path segments rather than verified import edges. For `deps --cycles`, that
is especially costly: it presents a high-confidence architectural defect and
can invite needless indirection to break a cycle that is not real. Cycle output
should either resolve only concrete import edges, or label heuristic edges and
show the import statement that created each edge.

## 2026-08-13 — I tried the body cap, the harness said no, and it was right

The Next.js entry below measures the problem exactly: `content_fts` stores every
indexed file's body *plus* a trigram index over it, ~2.7x, so 1.3 GB of committed
geodata became a 3.2 GB index and a four-minute cold build for 800 KB of source.
Its suggestion — skip the FTS body for large unextractable files — is the obvious
fix, and I implemented it. On a synthetic repro: **21 MB index → 108 KB**, same
file count, same symbols.

Then `bench/differential.py` failed six assertions:

```
newtonsoft/search/anchor-brace-eol: missed 20000 rg lines
  e.g. Src/Newtonsoft.Json.Tests/large.json:2, :31, :35
```

`large.json` is 6.6 MB and 225,002 lines — somebody's test fixture, in a real
repo, that ripgrep searches. The cap made `codeq search` stop being a strict
superset of `rg`, which is the loudest claim this tool makes and the harness's
primary assertion. Reverted.

The tempting move was to raise the threshold until `large.json` fit: 6.6 MB here,
45 MB there, so anything around 20 MB "works". That is not a principle, it is
fitting a constant to the two data points in front of me — and the next repo has
a 30 MB fixture. There is no size at which a file stops being something somebody
might grep.

What was actually missing is not a cap; it is that **codeq had the diagnosis and
kept it to itself**. It knows the bytes and symbol count per language on the
first walk. `.codeqignore` fixed the reporter's repo completely in one line — 3.2
GB to 6.9 MB, 240 s to 2.8 s, *identical* symbol and import counts — they just
had to work out for themselves that it was the answer. Surfacing that is the fix
this warrants, and it costs no guarantee.

Two other things fell out of the attempt:

- The cap broke `concurrent_invocations_all_succeed` — a longer write lock during
  the bigger build pushed a concurrent reader past the busy timeout, producing
  the exact `database is locked: Error code 5` the entry below reports from a real
  cold build. Reverting fixed the test, but **the underlying issue is real**: a
  four-minute build outlasts the 15 s `busy_timeout`, and a raw SQLite error for
  "a build is in progress" reads like corruption. Recorded, not fixed.
- `map`'s unindexable-bytes note **does** fire on plain `map`, contrary to the
  entry — verified after a shell-redirection mistake nearly had me agree with it.
  The reporter was likely on a build predating it. Their deeper point stands
  regardless: the note arrives after the four minutes are spent, and the moment
  to say it is during the build.

### The note that guessed wrong, fixed

`definitions areaBank` said "declared elsewhere (a dependency, or a built-in)".
`areaBank` is a *function parameter* two lines away. My note, added this morning,
guessing outward in the most expensive available direction. It now lists the
possibilities — local, parameter, destructured binding, dependency — and picks
none. The entry's framing is the one to keep: **notes that guess a cause are
worse than notes that state the fact.** Second time that has been filed; the
first was the note this one replaced.

## 2026-08-13 — First Next.js repo: the index ate the data directory

First run against a foreign codebase — `uk-data-atlas`, a Next.js 16 / React 19 /
TypeScript app. The language support was never in doubt. What nearly sank the
session was that the repo **commits 1.3 GB of geospatial data**, and codeq indexed
all of it.

### The headline: 96.5% of the index was files that produce zero symbols

`codeq map` was my first command. It ran for **four minutes** and left a **3.2 GB**
`index.db` for a repo whose actual source is 800 KB. The output says why, if you
know to look:

```
totals: files=398 bytes=1359946429 symbols=2363 imports=2236
typescript  files=149 bytes=395311    symbols=1486
(unknown)   files=142 bytes=1313000330 symbols=0      <-- 96.5% of bytes, no symbols
tsx         files=69  bytes=349437    symbols=800
```

142 `.geojson`/`.shp`/`.dbf` files, 1.31 GB, **zero symbols**. `CLAUDE.md` promises
a stderr warning "when most of the repo is assets codeq cannot index, so the byte
total isn't misread as coverage" — but that warning is on `map --totals`, and plain
`map` printed nothing. I got the unqualified byte total on the command a newcomer
actually runs first.

Worse, the warning fires on `map`, which is not where it is needed. The moment to
say this is **during the first index build**, while the user is watching a progress
bar and can still act. By the time `map` prints, the four minutes and the 3.2 GB are
already spent.

Writing a `.codeqignore` for `data/**` fixed it completely:

| | before | after |
|---|---|---|
| cold build | ~240 s | **2.8 s** |
| `index.db` | 3.2 GB | **6.9 MB** |
| symbols | 2363 | **2363** |
| imports | 2236 | **2236** |

**Identical code coverage, a 470× smaller index, 85× faster.** Which is the real
finding: the 1.3 GB bought *nothing*.

### Where the 3.2 GB actually went — `content_fts`, measured

Worth pinning down rather than hand-waving, because the mechanism is not the facts
table. `content_fts` is an FTS5 virtual table with `tokenize = 'trigram'` holding
**every indexed file's full body**, and `src/index.rs:350` populates it for any file
that survives the walk — a grammar is not required. That is a deliberate and good
design (it is what lets `search` replace grep over files codeq cannot parse), but it
means *symbol count is irrelevant to index size*. A file with zero symbols costs the
same as one with a hundred.

Controlled test — one 45 MB `.geojson`, one 52 MB `.shp`, one 20-byte `.ts`:

```
$ sqlite3 .codeq/index.db "SELECT path, language, LENGTH(body)/1048576.0 FROM content_fts;"
a.ts       | typescript | 0.00002
w.geojson  |            | 44.83        <-- no language, full body stored anyway
                                       <-- z.shp absent: binary files ARE excluded

$ sqlite3 .codeq/index.db "SELECT name, SUM(pgsize)/1048576.0 FROM dbstat GROUP BY name ..."
content_fts_data    | 76.32     <-- the trigram index
content_fts_content | 44.88     <-- FTS5's verbatim copy of the body
```

**45 MB of text in, 121 MB of index out — ~2.7×**, split roughly 1.0× for FTS5's
stored copy and 1.7× for the trigram postings. Scale that to the ~1.05 GB of
*textual* data files in this repo (870 MB geojson + 126 MB topojson + 44 MB json +
37 MB csv) and you land at ~2.8–3.2 GB. That is the whole of the observed 3.2 GB,
accounted for.

Two things fall out of this that are worth acting on:

1. **Binary exclusion already exists; a size/extension policy does not.** The `.shp`
   was correctly skipped, so the machinery for "do not FTS this" is in place — it
   just has no rule for "this is a 45 MB machine-generated JSON blob". Even a default
   body cap (skip FTS above N MB, still index symbols) would have prevented all of
   this, and would cost almost nothing in real repos.
2. **`map`'s `bytes=` counts files that were never FTS-indexed.** It reported
   `bytes=101419817` for the test above, including the 52 MB `.shp` that contributed
   nothing to the index. So the number is "bytes walked", not "bytes indexed", and
   as a proxy for index size it is wrong in both directions.

This also explains the differential slowness below exactly. `clones`/`tree`/`outline`
read the `facts` table (0.86 MB here) and stayed instant. `search`, `references`,
`unused` and `hotspots` issue `SELECT path, language, body FROM content_fts` —
`src/search.rs:357` and `src/commands.rs:4187` both do it with no `WHERE` — so every
one of those queries streams the entire body column out of SQLite. On the fat index
that is ~1.05 GB of text per query. The 17-minute `unused` hang was not a deadlock;
it was a full scan of a gigabyte of geojson, once.

codeq has everything it needs to work this out by itself — it knows the byte count and the symbol count per extension the first time
it walks the tree. A first-run prompt ("142 files / 1.3 GB yielded no symbols — add
to `.codeqignore`?"), or a `codeq ignore --suggest` that emits the file, would have
turned a 40-minute detour into one line. Right now the tool has the diagnosis and
keeps it to itself.

### The bloat is not a flat tax — it hits the reference-scanning commands

This is what made it hard to diagnose. On the fat index:

- `codeq clones lib` — **instant**, full correct output.
- `codeq tree` — **18 ms**.
- `codeq affected --compare-to HEAD` — **44 s wall** to reparse **one edited file**
  (2.2 s user, 0.99 s system, 7% CPU — 40 seconds of pure I/O).
- `codeq unused lib` — **hung for 17+ minutes**, zero bytes of output, killed.

After the `.codeqignore`, `unused lib` returned in **0.9 s**. Same query, same repo,
same code. The commands that only touch the parse tree stayed fast and told me
everything was fine, so the first slow command reads as "this command is broken",
not "your index is 470× too big". A `--meta`-style line on the slow path — indexed
bytes being scanned, or just "scanning 1.3 GB of file content" — would have pointed
at the cause immediately.

`unused` deserves a specific note: **17 minutes with no output and no progress
indication is indistinguishable from a deadlock.** I killed it. If a command is
going to scan the whole content table it should say so on stderr before it starts.

### `database is locked: Error code 5` leaked a raw SQLite error

I issued two independent queries in one batch (`map` and `tree`) — the normal way an
agent parallelises work with no data dependency. The second died instantly with:

```
codeq: database is locked: Error code 5: The database file is locked
```

A raw driver error for an ordinary situation: a build was in progress and the second
reader arrived early. It should block on the write lock, or fail with "an index build
is in progress — retry shortly". As written it reads like index corruption, and my
first instinct was to go delete `.codeq/`. Note this is *most* likely to happen on
exactly the first-run cold build that already takes four minutes, i.e. at the moment
the user knows least about what is going on.

### `explore` puts the focus block in the middle of its own output

`codeq explore updateGenericMap` on a 15-caller symbol emits ~460 lines. The `#
source` section is sorted **by path**, so:

```
 85: lib/data/gazetteer/gazetteer.ts:29     (callee)
 89: lib/helpers/mapManager/eventHandler.ts (callee)
105: lib/helpers/mapManager/featureBuilder.ts (callee)
113: lib/helpers/mapManager/layerManager.ts (callee)
133: lib/helpers/mapManager/mapManager.ts:396 (focus)   <-- the thing I asked about
193: ... 15 caller blocks ...
```

I ran `explore … | head -90` — the obvious way to sample a large result — and got
four callees and **not the definition**. The docs say "the definition is always
kept", which is true of the *budget* and not of the *reading order*, and the gap
between those two is invisible until you pipe the output. Emitting the focus block
first would cost nothing and make `head` do the right thing.

### A note that guessed the wrong cause, again

```
$ codeq definitions areaBank
# codeq: 'areaBank' has no definition in this repository —
#         referenced here but declared elsewhere (a dependency, or a built-in)
```

`lib/data/areaBank.ts` is sitting right there in the repo. The name resolves to a
*parameter* (`matchColumnAgainstBank(columnData, areaBank)`) and a destructured
local, neither of which is a definition — so the first clause is correct, and the
parenthetical is wrong in the most misleading available direction. It sent me
looking outward at dependencies when the answer was a local binding two lines away.
For a lowercase identifier, "it is a local or a parameter" is far and away the
likeliest cause and is not in the list. Same class as the note audit logged below —
the notes that guess a cause are worse than the notes that just state the fact.

### Minor: `--lines A,B`

`codeq read file.ts --lines 456,500` → `--lines must be A-B, e.g. 40-80`. Good
message, but comma-for-hyphen is the obvious slip and the intent is unambiguous.

---

### What actually worked, and it was most of the session

**`clones` found the target in one command.** On a repo I had never opened:

```
mapManager.ts:456,474,494,715,745,777,807,837,871,905,930,955,980,996,1030
  method updateMapFor{HousePrices,CrimeRate,Income,IMD,SIMD,…} shape=43ac98e5eed5918f group=15
```

Fifteen structurally identical methods, plus a group of 5 and a group of 4 in
`featureBuilder.ts`. That is the whole "where is the duplication" question answered
before I had read a single file.

It is also worth recording that **reading the groups talked me out of refactoring
four of them.** The `featureBuilder` clones turned out to be thin, correctly-typed
wrappers over a shared `buildColorFeatures` — identical in shape precisely because
they were already factored well, and collapsing them further would have traded type
safety for line count. `clones` reporting no similarity score is what made that
judgement mine to make. Good signal even when the verdict is "leave it alone".

**The dead-code removal set came out of the index, not out of reading the file.**
Four commands established the complete transitive set before I touched anything:
`references buildAreaBank` (dead — only its own definition, a comment and the design
doc), then `references BOUNDARY_META` → used only by it, then `search PROPERTY_KEYS`
and `search BoundaryData` scoped to the file → each used only by those two. So the
edit was one function, one const, and **both of the file's imports**, known up front.

Then the verification loop, which is the bit I would most want to keep:

```
$ codeq deps lib/data/areaBank.ts     # before
lib/data/boundaries/boundaries.ts: @lib/data/boundaries/boundaries
lib/types/boundaries.ts: @lib/types/boundaries

$ codeq deps lib/data/areaBank.ts     # after
(empty)
```

The module went from two internal edges to zero — it no longer depends on the eager
boundary loader at all. codeq proved the cleanup landed, in the same vocabulary it
proposed it in. `tsc --noEmit` clean, 111/111 tests pass.

**`importers` at named-import granularity was the decisive evidence.** `importers
areaBank` listed `@lib/data/areaBank.buildAreaBankFromIndex`,
`.matchColumnAgainstBank`, `.detectCoordinateColumns` — and no `.buildAreaBank`.
That distinction between "the module is imported" and "this export is imported" is
what let me delete a `pub`-equivalent export with confidence. On a TS codebase with
barrel files it is the single most useful thing in the import index.

**Path aliases resolved correctly.** `@lib/...`, `@/lib/...` and relative forms all
resolved in `imports`/`importers`/`deps`. `const X = memo(...)` components indexed as
expected. No Next.js-specific breakage anywhere — the app-router `app/` directory,
`"use client"` files and `.tsx` all behaved.

**`affected --compare-to HEAD`** gave the changed symbol and a correct three-hop
impact chain (`useMapUpdates` → `MapView` → `MapInterface` → `AtlasClient`) with an
honest empty `# tests` section, which was accurate — nothing covers that hook.

### The boundary: codeq got me to the bug, it did not find the bug

The actual defect this session — two React effects fighting over the same map layer,
one clearing the boundary source and the next repainting it — is a cross-effect
semantic bug in a hook. No index finds that. What codeq did was collapse the search:
`clones` → `mapManager` → `explore updateGenericMap` → the dispatch switch, in about
five commands and no whole-file reads, at which point the bug was visible by reading
40 lines. That is the right division of labour and I would not want it overstated in
either direction.

## 2026-08-13 — Auditing the empty-result notes instead of waiting to trip over them

The third `persona` entry said the wrong `references` note was "a class of bug
rather than one bug ... which suggests auditing every command that can return
empty rather than fixing them as they surface". Four had been fixed reactively by
then — `references`, `callpath`, `graph`, `astgrep --path` — each after somebody
lost time to it. So: a script that runs every command in a way that returns
nothing and checks whether it says why.

**Eleven of twenty-six returned empty with no note, no stderr and exit 0.** Now
three, and those three are deliberate.

The distinction that made this tractable is which empties are *ambiguous*. A bare
`search zzz` finding nothing is unambiguous — grep behaves identically and nobody
is confused; a note on every one would be the noise these notes exist to avoid.
What is ambiguous is a **filter** that selected no file, because then the query
never ran and "no matches" is a statement about the filter rather than the code.
Most of the eleven were that one sentence with a different noun, which is why
they are one helper and not eleven messages — and why the list should not regrow.

Ranked by what a misreading costs, which is not the same as by how odd it looks:

- **`context` was the worst**, and it did not look it. It hands source to an
  agent, so an empty bundle reads as *nothing more is needed here*. Worse, a bad
  seed *beside good ones* was invisible: the bundle looked full and the one name
  you got wrong was silently missing from it. That case is now reported whether
  or not the bundle is empty — I had written a comment claiming it was handled
  before the code did, and caught the gap only by testing the mixed case.
- **`at` vs `explore --at`** disagreed about explaining themselves on the
  identical position. Two commands, one question, one of them mute.
- **`hotspots`/`imports`/`clones`/`tree`** with a path matching nothing.
- **`search`/`astgrep`** when a *filter* rather than the pattern emptied it.
  `astgrep` gets a note on a genuine miss too — alone among these — because its
  own docs concede that a wrong node kind and a real miss look identical, so it
  points at `--explain`.

`clones`'s empty note now also says what it cannot see, which is the third time
that limitation has cost somebody an assumption.

### Two things this turned up on the way

`hotspots` had a **second, silent emit** in an early return for the commonest
empty case. The note added at the end of the function was unreachable from there,
so the fix tested green on one route (`--min-refs 999999`) while doing nothing on
the one anybody would hit (`--path nosuchdir`). Worth stating because it looked
applied: the code was present, the test I would have written first passed, and
the behaviour was unchanged.

And the harness now asserts the class rather than the instances: eighteen
commands, run in a way that empties them via a filter, each required to carry a
note. The twelfth instance cannot arrive quietly.

## 2026-08-13 — Five from the third `persona` pass, and the check that kept failing

### The process failure, third occurrence

The entry below sat unread at the top of this file for two commits. It came in
via `bb4a0ee` — mine — on the same `git add -A` habit that did it in `52019bb`,
and that I caught in `2c26519` only by counting headings in the diff first. Asked
to double-check, I looked at `git status` and mtime, found both clean, and said
so twice. They *were* clean: the unread content was already committed. Those
commands answer "has anything changed since my last commit", which is a different
question from "is there anything here I have not read". Only the headings answer
that, and the check costs one command.

### `callpath` explained nothing — the widest of the empty-note family

Four causes, one output: no stdout, no stderr, exit 0, and no `note` under
`--json` either. A misspelled FROM, a misspelled TO, a real pair that does not
connect, and a chain past `--max-depth` were indistinguishable, on the command
where an empty answer has the *most* plausible explanations. It now names which,
and the unreachable case reports how many symbols the walk explored so "raise
--max-depth" and "these genuinely do not connect" are separable.

The reporter's framing is the durable part: the wrong `references` note fixed
this morning is a **class**, not an instance, and they found two more siblings in
an hour of ordinary use.

### `graph`'s note was about somebody else's repo — and it was mine

Written today. On a Python-only tree it said "Go, C and Zig have no inheritance
at all", which reads as *your language is unsupported* when the truth is *your
code has no edges*. It now partitions the languages actually present into those
that can produce relation edges and those that cannot, and says which case this
is. Writing a helpful note about the general case is exactly how you produce a
misleading one about the specific case in front of the reader.

### `__all__` was public API and dead code, in every Python package that has one

Declared once, referenced never, by construction — the same sentence that already
excludes SQL indexes and triggers. Now private and excluded, via a three-name list
rather than a blanket dunder rule: `__version__` *is* referenced, and hiding it
would be the opposite mistake. The reporter drew that distinction themselves,
which is why the fix is a list and not a pattern.

### Copy-paste from `outline` into `--symbol` failed on markdown

`outline` renders a heading's *signature* — the source line, `## Heading` —
while `--symbol` stores the name bare. The obvious workflow was the broken one.
`--symbol` now accepts the copied form. Nothing in any supported language
declares a name starting with `#`, so this can only rescue the case it is for.

### And `alias =`

A 52-entry dict, the substance of its file, rendered as a bare `alias =`. The
fold is right; the rendering read like the renderer had given up. It now carries
the bracket the value opens with — `alias = {…}`, `ITEMS = […]` — which is three
characters for the one fact that was missing. Single-line values still show
whole.

Item #4 from that entry (`api --compare-to` on re-exported *constants*) needed no
work: the fix landed in `bb4a0ee` reads import facts by last segment, so it
covers constants and functions alike. Verified rather than assumed.

### Round four, which landed while this was being written

No defect in it — a boundary, stated as one: codeq indexes the working tree,
while the Go build linked a *published* sibling module resolved from the module
cache, so `read core.go --symbol Shutdown` was truthfully describing code the
adapters do not run. Forty minutes of deadlock hunting, and the runtime stack was
the only thing that revealed it.

The suggestion — annotate an intra-repo import that resolves to a published
version rather than the local module — is real, and I am not building it today.
It requires per-language dependency resolution (read `go.mod`, compare module
paths, notice a missing `replace`), which is the thing codeq deliberately does
not do; its import graph is stem-matching and says so. A Go-only, heuristic
version of "your build links something else" would be a confident answer about
the one question where being wrong costs the most, and the entry itself asks for
a thought rather than a feature. Recorded as the boundary it is.

The fourth entry in a row to arrive mid-task, and the second the heading check
caught before a commit. That check is now the only reason this file's contents
and my commit messages agree.

## 2026-08-12 — Third `persona` pass: the fixes hold, and the empty-note bug has siblings

Same external repo, third session. I re-ran every finding from the two entries
below against the 19:00 build before writing anything new, because two of them
were filed against a binary that has since been rebuilt twice.

**All four fixes hold on a third-party repo**, which is worth stating since they
were developed against codeq's own fixtures: module constants now resolve
through `definitions`, `outline`, `about`, `explore` and `symbols --kind
constant`; `untested` no longer lists private classes' members; `affected` rows
carry `call_line=` separately; and `unused` now prints
`decorator=@app.get("/v1/{location:path}/")` on the FastAPI route from the first
entry. The new `references` note went past what I asked for — it separates
"external name" from "your filter emptied it" in one line, where I had only
asked it to stop saying "no symbol named".

Everything below is new.

### The #1 fix has one false positive: `__all__`

It will fire on every Python package that declares one:

```
$ codeq unused src/persona
src/persona/__init__.py:22: constant __all__ visibility=public
$ codeq untested src/persona/__init__.py
src/persona/__init__.py:22: __all__ = ["gen_samples", "generate", …]
$ codeq api src/persona/__init__.py
src/persona/__init__.py:22: __all__ = [...]
```

Two separate things are wrong. codeq's stated Python rule is that public means
*non-underscore*, and `__all__` opens with two — so by the documented rule it is
private and should not be in `api` at all. And it is a declaration the
interpreter reads, never a name anything references, so `unused` will report it
forever in every package that has one.

The precedent for the fix is already in the docs, one section over: `unused`
skips SQL indexes and triggers because they are "named exactly once by
construction, so reporting them is noise, not a finding". `__all__` is that
sentence exactly. (`__version__` is not — that one does get referenced — so this
wants a small dunder list, not a blanket dunder rule.)

### #4 is wider than when it was filed

"Not done yet" is fine, but the surface grew when constants became definitions.
I had moved *two* things out of `api/handler.py` and imported them back:

```
$ codeq api src/persona/api/handler.py --compare-to HEAD
# removed
src/persona/api/handler.py:0: constant DATA_DIR was=DATA_DIR = Path(__file__).parent.parent / "data"
src/persona/api/handler.py:0: function resolve_key was=def resolve_key(name: str, data: dict) -> str | None:
```

Both still import from `persona.api.handler`. Before the constants fix only the
function reported as removed; now every re-exported constant does too. Whenever
the module-scope-imports decision gets made, it is worth knowing it covers more
ground than it did this morning.

### The empty-result note has two more siblings

The entry below calls the wrong `references` note "the one that could do real
damage" and fixed it first. That was right, and it is a *class* of bug rather
than one bug — here are two more found in an hour of normal use, which suggests
auditing every command that can return empty rather than fixing them as they
surface.

**`callpath` explains nothing at all.** Four distinct situations, one output —
no stdout, no stderr, exit 0:

```
$ codeq callpath zzznotreal gen_sample          → (nothing), exit 0
$ codeq callpath run zzznotreal                 → (nothing), exit 0
$ codeq callpath run preprocess_location_data   → (nothing), exit 0
$ codeq callpath run preprocess_location_data --json
{"schema":"codeq.result.v2","rows":"data","data":[]}
```

No `note` under `--json` either, where `supertypes` on the same repo returns
`"note":"'_ArrayDraw' is defined at … but has no supertypes (it extends
nothing)"`. A misspelled `FROM`, a misspelled `TO`, an unreachable pair, and a
chain past `--max-depth` are indistinguishable — on the one command where the
empty answer has four plausible causes. My answer here was correct (the CLI
genuinely does not reach `preprocess_location_data`, which is API-only), and I
only knew that because I had already read both files. Without that I would have
had to assume I typoed something.

**`graph`'s empty note names languages that aren't in the tree.**

```
$ codeq graph
# codeq: no relation edges — Go, C and Zig have no inheritance at all, and a
  language with no `REFERENCES` or `impl` has nothing to graph
```

This repo is Python, which *does* produce inheritance edges per `capabilities`.
It genuinely has no edges — two bare classes, no subclassing — so the answer is
right and the explanation is about somebody else's repo. It reads as "your
language is unsupported" when the truth is "your code has no edges", which is
the precise failure mode the "don't be misled" section exists to prevent.

### Markdown: the name `outline` prints is not the name `--symbol` takes

Reading one entry of this very file, I copied its heading straight out of
`outline` and it was rejected:

```
$ codeq outline DOGFOOD.md
  88: ## 2026-08-12 — Acting on the second `persona` pass, and a process failure of mine

$ codeq read DOGFOOD.md --symbol "### What it caught immediately"
codeq: no definition named '### What it caught immediately' in DOGFOOD.md
$ codeq read DOGFOOD.md --symbol "What it caught immediately"
    34  ### What it caught immediately        ← works
```

`outline` and `at` render heading names *with* their `##` markers; `--symbol`
and `definitions` want them *without*. The docs' own example
(`read README.md --symbol "Output contract"`) uses the bare form, so the
rendering is the odd one out. Copy-paste from `outline` into `--symbol` is the
obvious workflow and it is the one that fails.

### Cosmetic: a multi-line constant's signature stops at the `=`

```
$ codeq outline src/persona/lib/format.py
4: alias =
```

`alias` is a 52-entry dict spanning lines 4–55, and it is the substance of that
file. The JS rule of stopping after the wrapper reads well when something
follows it (`const Card = memo` says what the binding *is*); a bare `alias =`
reads like the renderer gave up. `alias = {…}`, or an element count, would carry
the one fact worth having.

## 2026-08-12 — The last two open findings: a re-export, and a monorepo's clones

### `api --compare-to` called a move a removal (persona #4)

`resolve_key` moved from `api/handler.py` down into `lib/generate.py` and was
imported back at module scope. Every `from persona.api.handler import
resolve_key` still resolved — nine call sites in a green test suite — and codeq
reported it as a **removed public export**. From the command pitched as "the
review question a raw diff can't answer", that is the answer that would block a
PR on a non-breaking change.

The comparison was definitions-only. It now also reads the file's import facts:
the bound local name is the import's last segment, and codeq already stores
`from pkg import x` as `pkg.x` and JS `import {a} from './x'` as `./x.a`, so one
rule reads both — and JS `export { x } from './y'` with them.

Reported as a note ("still exported, now via an import") rather than dropped:
*it moved* is a fact, and it is not the same fact as *nothing happened*. A
genuine deletion is still a removal, which is what the test pins.

I deliberately did **not** take the filer's broader suggestion of counting every
non-underscore module-scope import as public surface. It would fix this, and it
would also list `os` and `sys` as the public API of every Python file. The
narrow rule answers the question that was actually wrong.

### `clones` on a monorepo (api-analytics #1)

Rows now carry `pair`: the directories the group spans, the same value across
every row of the group. On the repo that prompted it, thirty-plus of the top
rows were a PHP `core` package vendored into `laravel` and `php` — with `pair`
that is one thing to dismiss (`--fields pair,name`), not twelve.

Presentational on purpose. It changes nothing about what counts as a clone, and
I still declined `--exclude-pair`/a pairing-ignore file: persistent config
encoding a judgement makes an empty `clones` result unfalsifiable without
reading the config, and "no duplication found" is exactly the answer that must
never be quietly configured into existence.

That closes every finding from the three outside-repo entries except the ones
explicitly recorded as declined.

## 2026-08-12 — The harness could not see a day's work, and found a bug the hour it could

`bench/differential.py` is described in CLAUDE.md as the important one — the
check that catches what small fixtures cannot. Its assertion count sat at
5,226–5,227 all day, across SQL extraction, JS property-assignment methods,
Python module bindings, test attributes, decorators, FK relations and a new
`graph` command. That flatness was the tell.

`sample_symbols` filtered candidates to `("function", "method", "class",
"struct")`. So the battery never touched a field, a variant, a module-scope
constant, or any SQL declaration. On flask alone that is **132 constants, 212
fields and 2 tables** it was structurally unable to reach. Six schema bumps
landed today on the strength of unit fixtures and a harness that could not
exercise the facts they added.

Sampling is now stratified: round-robin across kinds rather than a flat stride,
so a kind with three instances in a repo is still sampled instead of being
rounded away by a kind with a thousand. `INVARIANT_K` 20 → 30. New invariants for
the commands added today — `explore --at` must agree with `at`, `astgrep
--explain` must return a tree covering the line it was asked about — plus
whole-repo ones: `map --totals` is a view of `map`, `graph`'s `--relation` filter
is a real subset, and the envelope's `rows` pointer resolves.

**5,226 → 8,159 assertions**, and no new corpus repo was needed: flask already
ships `schema.sql`, so SQL extraction is now exercised against real code rather
than only fixtures.

### What it caught immediately

Fifteen failures, every one of them `explore --at` disagreeing with `at`, and
every one on a kind the sampler could not previously reach. Sinatra:

```
$ codeq at lib/sinatra/base.rb:977
977: field app · field env · field request · field response · field params
$ codeq explore --at lib/sinatra/base.rb:977      # focus
app
```

The line is `attr_accessor :app, :env, :request, :response, :params`. Five
definitions share a span, so they tie for innermost, and `definition_at` had
`LIMIT 1` — it answered about `app` when the question was about `response`, with
nothing to indicate it had chosen. `at`, asking the same question, returned all
five and was right to. Same shape in the C++ repos (enum variants on one line)
and jq (multi-name field declarations).

`explore --at` now refuses: a line that declares several things is a line, not a
symbol, and it says so with the candidates listed. That is the honest answer —
the alternative is picking one silently, which is what it was doing.

The first fix was **still wrong, and the harness caught that too.** I treated
"innermost" as smallest span, so definitions only counted as ambiguous when they
tied exactly. Ruby's `attr_accessor` ties; a C++ `enum Color { red, green, blue,
unknown }` on one line does not — the four enumerators have different spans, so
it picked `red`, *because `red` is the shortest name*. Ranking by span was
measuring characters and calling it nesting.

The rule is containment: the leaves are the covering definitions that do not
contain another covering definition. Four disjoint enumerators are all leaves;
the `enum` around them contains them and is an ancestor. That is what nesting
means, and span only approximates it when the definitions happen to be siblings
of equal width. Three rounds of the harness to get from 15 failures to 7 to 0,
each round a real defect rather than a flaky check.

### Two things worth keeping

The first run of the new `astgrep --explain` invariant reported three CSS
failures. Those were **my invariant being wrong**, not codeq: `_cq` returns
stdout, and the "no grammar for 'css'" refusal goes to stderr, so the check
tested an empty result and blamed the tool. Fixed to skip when the command
produced no JSON. A dry run on one repo before the full sweep is what caught it,
and is worth doing for any new invariant.

While tracing this I found a comment claiming `--at` "identifies one definition
exactly, so a common name can't drag in same-named definitions from elsewhere".
It scopes to the *file*, so a name declared repeatedly in one file still
contributes every occurrence to the focus — `explore --at base.rb:83` returns
four `params`. The behaviour is defensible and the comment was not; the comment
is now accurate. Whether `--at` should narrow to the one definition on the line
is a separate question, deliberately left open.

## 2026-08-12 — Acting on the second `persona` pass, and a process failure of mine

First, the process failure, because it is the more useful lesson. Commit
`52019bb` added **477 lines** to this file; my own entry was about sixty of them.
`git add -A` swept up two entries written by other agents while I was working —
the second `persona` pass below and the api-analytics one — and the commit
message describes neither. I then told the user I could not find any new entries,
having checked `git status` and the file's mtime: both clean, *because I had
already committed the content unread*. The working tree is not the only place to
look for something you might have absorbed; history is.

I can't amend it (pushed, and force-pushing is off the table), so this is the
record.

Four of the five findings below are now fixed. All five reproduced on the current
binary before I touched anything — worth saying, because two of them are the kind
that get filed from a stale build.

### The one that could do real damage: a wrong empty-result note (#2)

`references Path --path scripts` answered `no symbol named 'Path'`. The filer had
just deleted `from pathlib import Path`, read that as confirmation the name was
gone, and moved on. Ruff happened to agree, so the edit survived — but the belief
was formed on a message answering a different question.

The "your filter, not the code" branch requires an in-repo *definition*, and a
third-party name has none, so it fell through to "this name does not exist". Now
a name that is referenced but not defined here says exactly that. codeq sells its
empty-result notes as the reason to read stderr; that makes a confidently wrong
one worse than silence, and it is why this was fixed before the bigger gap below.

### The structural one: Python module-scope bindings (#1)

`DATA_DIR = Path(__file__).parent / "data"` was indexed as nothing. Not a
definition, not a constant — `definitions DATA_DIR` returned the *typo* message
for a name whose assignment `references` could see perfectly well.

What makes it an oversight rather than a decision, and the filer put this better
than I would have: both neighbouring cases were already handled. The docs argue
at length that a JS module-scope binding is a definition; Python class-body
assignments already extract as fields. Assignment extraction existed and simply
never ran at module scope — which is where every settings constant in every
Python project lives.

It also degraded quietly rather than failing: `explore` on such a name returned
an empty focus and *no source at all*, despite having a caller it could have
shown, against a documented contract that "the definition is always kept".

One grammar note for anyone extending this: Python's `assignment` names its
fields left/type/**right**, not `value`. The first draft used `value:` and
tree-sitter rejected the pattern as impossible — which is the good failure, and
`astgrep --explain` is now the way to check that before writing it.

### Two smaller ones, both cheap and both plainly wrong

**A member of a private type was public API (#5).** Visibility was computed per
definition without consulting the type around it, so the methods and fields of a
Python `class _Draw` landed in `api`, in `untested`, and in `api --compare-to` as
newly *added* public surface. Now checked up the whole chain of enclosing types,
so a public inner class inside a private outer one is private too.

**`affected` used `line` for two things (#3).** `changed` rows carried the
definition's line and `impacted` rows the call site's — one command, one key, two
meanings, and clicking an impacted row landed you mid-body. The call site is
worth reporting, so it kept it as `call_line`, the shape `callees --examples`
already uses. That is the second time this month the fix for a row-key problem
was "give the smuggled fact its own field".

### Not done yet

**#4, `api --compare-to` reporting a re-export as a removed export.** Real, and
the reasoning is sound — a module-scope `from x import y` binds a public name in
Python, and codeq already stores it as an import fact. It needs a decision about
counting module-scope imports as public surface, which touches JS `export { x }
from './y'` the same way, so it is its own change rather than a fifth item in
this one.

## 2026-08-12 — Second pass on `persona`: five bugs, one of them structural

Continuing the entry below. Same repo, deeper into the commands I hadn't
touched — `explore`, `affected`, `api --compare-to`, `astgrep`, `untested`,
`definitions`. Two more refactors landed (2,874 tests green). Findings ranked by
what I'd fix first.

(The `target/release` binary was rebuilt part-way through the session, so all
five repros below were re-run against the later build — 47,000,176 bytes, built
15:49 — and all five still reproduce. `src/commands.rs` has edits newer than
that binary, so anything already fixed there is not reflected here.)

### 1. A Python module-level constant is not a definition

This is the structural one. codeq indexes `DATA_DIR = Path(__file__).parent /
"data"` as *nothing*:

```
$ codeq definitions DATA_DIR
# codeq: no symbol named 'DATA_DIR'

$ codeq references DATA_DIR --group
src/persona/lib/generate.py (7)
10:0: DATA_DIR = Path(__file__).parent.parent / "data"     ← the definition
…
src/persona/scripts/gen_readme.py (2)
8:0: DATA_DIR = Path(__file__).parent.parent / "data"      ← and again
```

`references` can see the assignment. `definitions` denies the name exists — and
says so with the *typo* message, the one documented to mean "you misspelled it".

The concrete cost: this repo had **three** copies of that same `DATA_DIR`
expression across three files, and deduplicating them was the task I was on.
`definitions NAME` is precisely the query for "where is this defined, and is it
defined more than once" — and it returned "no such symbol". I found the third
copy by opening `gen_readme.py` for an unrelated reason.

It degrades the commands above it rather than failing loudly:

```
$ codeq about NAME_REFERENCE_YEAR
# definitions          ← blank
# callers
src/persona/lib/generate.py:280: function _name_cohort

$ codeq explore ADULT_ONLY_FEATURES
# focus                ← blank
# callers
src/persona/lib/generate.py:361: function gen_sample
# source               ← blank
```

`explore`'s documented contract is "the definition is always kept, the rest fill
the budget". Here the focus is empty and *no source is returned at all*, even
though there was a caller it could have shown.

And `outline src/persona/lib/format.py` returns three functions — omitting
`alias`, a 52-entry dict that is the entire point of the file and the thing
anyone opening it is looking for. `symbols --kind constant --language python` is
empty repo-wide.

What makes this feel like an oversight rather than a decision is that both
neighbouring cases are already handled. The docs argue *at length* that a JS
module-scope binding is a definition — "a plain `export const VERSION` is public
API" — and Python class-body assignments are already extracted as fields (my new
`_WeightDraw.age = staticmethod(gen_age)` was indexed as `field age`). Python
assignment extraction exists; it just doesn't run at module scope, which is
where `DATA_DIR`, `alias`, `ADULT_ONLY_FEATURES`, `CONDITIONAL_FEATURES` and
every settings constant in every Python project lives.

### 2. `references` gives the typo message when `--path` empties an external name

Minimal repro, three commands:

```
$ codeq references HTTPException
src/persona/api/app.py:4:29: from fastapi import FastAPI, HTTPException, …
src/persona/api/app.py:47:39: def _location_not_found(data: dict) -> HTTPException:
src/persona/api/app.py:48:11: return HTTPException(

$ codeq references HTTPException --path lib
# codeq: no symbol named 'HTTPException'          ← wrong branch

$ codeq references gen_sample --path scripts
# codeq: 'gen_sample' is defined at src/persona/lib/generate.py:345 but nothing
  matches --path scripts                          ← correct branch
```

The "your filter, not the code" branch requires an in-repo *definition*. A name
imported from a third-party package has none, so it falls through to "this name
does not exist" — for a name codeq had listed three references to one command
earlier.

I did not spot this abstractly; I acted on it. Having just deleted `from pathlib
import Path` from a file, I ran `codeq references Path --path scripts`, read
"no symbol named 'Path'" as confirmation the name was gone, and moved on. Ruff
later agreed, so the edit was fine — but I had believed a message that was
answering a different question. The empty-result notes are sold as the reason to
read stderr, which makes a wrong one costlier than no message at all.

### 3. `affected` uses `line` for two different things in one result

```
# changed
src/persona/api/handler.py:38: function resolve_path_key
src/persona/api/handler.py:72: function _ancestor_leaf_features
# impacted
src/persona/api/handler.py:46: function resolve_path_key
src/persona/api/handler.py:77: function _ancestor_leaf_features
```

Same function, same command, two line numbers. `resolve_path_key` is defined at
38 and spans to 54; line 46 is `key = resolve_key(segments[0], data)` inside it.
Line 77 is the `leaf_ancestor_keys(...)` call inside `_ancestor_leaf_features`.
So **`changed` rows carry the definition's line and `impacted` rows carry the
call site's**. Verified on four symbols (`gen_sample` 345 vs 349, `gen_samples`
543 vs 583).

CLAUDE.md states the row contract as "`name` is the definition a row *is* …
position is `line`/`col`". An impacted row breaks it silently: click the line
and you land mid-body. The entry point of the impact *is* worth reporting — but
as its own field, the way `callees --examples` already carries `call_line`.

### 4. `api --compare-to` reports a re-export as a removed export

I moved `resolve_key` from `api/handler.py` down into `lib/generate.py` and
imported it back at module scope in `handler.py`. Every existing
`from persona.api.handler import resolve_key` still resolves — nine call sites
in the test suite prove it, and the suite is green. codeq:

```
$ codeq api src/persona/api/handler.py --compare-to HEAD
# removed
src/persona/api/handler.py:0: function resolve_key was=def resolve_key(name: str, data: dict) -> str | None:
```

For the command pitched as "the review question a raw diff can't answer", a
non-breaking move reported as a removed public export is the wrong answer to the
only question it exists to answer — and it is the answer that would block a PR.
A module-scope `from x import y` binds a public name in Python, and codeq
already stores it as an import fact; JS `export { x } from './y'` is the same
shape. Counting non-underscore module-scope imports as surface would fix both.

### 5. Member visibility ignores the owner's visibility

`_WeightDraw` and `_ArrayDraw` are private by the exact rule codeq uses for
Python (leading underscore). Their members are reported as public API:

```
$ codeq untested src/persona            # documented: "Public only"
src/persona/lib/generate.py:320: value = staticmethod(gen_feature)
src/persona/lib/generate.py:332: def value(proc: dict, rng: np.random.Generator) -> str:
```

and `api --compare-to HEAD` announced `age`, `value` and `name` as newly *added*
public API. A member of a private type is not public API in any language codeq
supports. The visibility rule is being applied per-definition without consulting
the `owner` codeq already stores.

(Amusing corroboration of a documented limit, in the same output: `age` and
`name` were *not* listed as untested while `value` was — because `tests` found
test functions mentioning the words "age" and "name" somewhere. Over-matching on
common names, exactly as advertised.)

### What was genuinely good

`explore get_features --max-tokens 1200` returned, in one call and inside
budget: the definition, both production callers, all three callees, five test
functions, every source block verbatim, and the focus file's import list. That
is the entire review packet for a change to that function, and it is why I
trusted the refactor. Nothing else I have replaces it.

`astgrep 'rng.choice($$$)' --language python` was the right tool for *verifying*
a refactor rather than exploring one — "is the drawing primitive now confined to
the two adapters?" came back as eight sites, all in one file, three of them the
new adapter methods, each tagged with its enclosing symbol. Structural search
earning its keep on a question text search answers badly.

`--group` on `references DATA_DIR` (21 rows over 5 files) read distinctly better
than the flat form — the per-file counts were the answer, not the rows.

One want: five of the eight source blocks `explore` spent its budget on were
tests. Before an edit that is useful; when the budget is tight I would like
`--no-tests` to spend it on production callers instead.

## 2026-08-12 — Acting on the `persona` findings (three of four, and half of the fourth)

The four points below were filed by an agent using codeq on a repo that is not
this one — the first outside-in dogfooding this log has. That is worth more than
codeq-on-codeq: every finding was about a *default* being wrong for a codebase
that doesn't look like this one.

### Fixed as filed

**Decorator-registered entry points (#2).** A FastAPI route's only reference is
the `@app.get(...)` line above it, so `unused` reported every entry point in
every decorator-driven codebase — 100% of its output on that repo. Definitions
now carry a `decorator` and the row is **tagged, not suppressed**: suppressing
anything decorated would hide real dead code, which is the one thing `unused`
must not do. The filer proposed exactly this and was right. Inert decorators
(`@property`, `#[derive]`, `#[inline]`) are not tagged, so the tag means
something when it appears — that exclusion list is the whole judgement in the
change, and there is a test pinning it.

Cost: nothing. `attributes_of` already existed from the `#[test]` work earlier
the same day; this is a second reader over the same three grammar shapes.

**`tree --language` (#3).** `--min-symbols` is *documented* as the way into an
asset-heavy repo and cannot work in one, because a markdown heading is a
definition and 281 per-country READMEs outrank the source 68 to 1. Indexing docs
is a good decision and discounting headings inside `--min-symbols` would erode
it, so the fix is the axis that was missing: `tree --language python` now scopes
files and counts, the way `files --language` always has.

**The `(unknown)` headline (#4).** Accurate, and still reads like a coverage gap
rather than "this repo is mostly images". `map` now says it in words on stderr
when unindexable files are the majority — a note, because this is guidance for
reading a number, which is what the note channel is for. Nothing when the repo is
mostly code: a banner on every call would be its own noise.

`map --language` was asked for alongside `tree --language` and **not** added.
`map`'s whole output is a per-language breakdown; filtering it to one language
makes that section a single row while making the rest useful, which is an
incoherent command. `tree --language` and `files --language` cover the question.

### Half-fixed, and the half I refused

**`clones` (#1)**, filed as "the finding I'd act on first". Two real defects,
fixed: it was the only command in its family insisting on `--path` instead of a
positional path, and it did not exclude tests. That second one is most of the
complaint — every row it returned on that repo was test boilerplate, because two
assertions in sequence share a shape however different the assertions are. Tests
are now excluded by default, same predicate as `unused`/`untested`.

The proposed cure for the 0-of-5 miss — matching on naming adjacency, `X` next to
`_X_processed` — I declined. That is a similarity heuristic wearing a structural
costume, and it is the trade this project keeps refusing: it would find those
five pairs and confidently invent pairs in every repo with an `_impl`/`_async`
suffix convention. The filer's own write-up concedes the design argument is
sound.

The honest response to "exact shape equality missed five real clones" is to
**document what it cannot see**, which the help text now does: `clones` finds
copy-paste that was never edited afterwards; two implementations of one algorithm
over different data representations have different trees; read an empty result as
"no exact copies", not "no duplication". A limitation stated is worth more than a
threshold nobody can calibrate.

## 2026-08-12 — Using codeq on someone else's repo: a refactor of `persona`

Not codeq-on-codeq this time. I drove a real refactor of `persona` (a Python
demographic-persona generator: 12 source files, ~570 lines in the one module
that matters, sitting inside a 2,683-file tree that is otherwise flag PNGs and
JSON datasets) using codeq as the only navigation tool. The refactor landed —
one import cycle removed, four pairs of parallel implementations collapsed to
one, 2,874 tests still green. What follows is what codeq got right and the four
places it got in the way.

### `deps --cycles` was the best command of the session

The module had two *function-local* imports (`from persona.api.handler import
resolve_key` inside two functions in `lib/generate.py`) — the kind of thing you
read past. One call:

```
$ codeq deps --cycles
src/persona/api/handler.py -> src/persona/lib/generate.py -> src/persona/api/handler.py
```

That is the whole diagnosis: `lib` reaches up into `api`, so the import has to
be deferred, so the layering is inverted. It was also the *cause* of the
duplication I was there to fix — with `lib` unable to import from `api` at
module scope, the API path had grown its own copies of half the generator. One
command turned "why is this import indented?" into a root cause. Nothing else in
the toolbox would have shown me that in one step.

`references NAME` after each edit was the other repeat winner — a
did-I-miss-a-call-site check where the *empty* answer is the useful one:

```
$ codeq references _segments
# codeq: no symbol named '_segments'
```

grep prints nothing and means the same thing far less clearly. And `read <file>`
folded turned a 579-line module into ~70 lines of shape, which is how the
`X` / `X_api` / `_X_processed` naming pattern — five duplicated pairs — became
visible at all. That is the pitch, working exactly as written.

### 1. `clones` found nothing, on a file with five clone pairs

This is the finding I'd act on first. The module contained five deliberate
parallel implementations of one algorithm over two data representations: raw
`{option: weight}` JSON (CLI) and precomputed numpy arrays (API).
`_merge_leaves`/`_merge_processed`, `gen_name`/`_gen_name_processed`,
`resolve_location`/`resolve_api_location`,
`resolve_location_path`/`resolve_api_path`, plus a copy-pasted expression.
Collapsing them *was* the refactor (I merged four of the five; the
`resolve_*` pair genuinely differs — filesystem paths vs preloaded keys — and
was left alone deliberately).

```
$ codeq clones --path src --min-nodes 10
(empty)
$ codeq clones --min-nodes 20
tests/test_generate.py:515: function test_get_features_regular_location …
… 13 more rows, every one of them test boilerplate
```

Zero of five. Repo-wide, every row it did return was two-assert test functions —
true positives that no one wants. The docs are honest that this is exact
structural equality, and I'm not asking for a similarity score; the design
argument in the help text is sound. But the measured result on a real refactor
is that `clones` had a 0% hit rate on the duplication a human would name
immediately, and a 100% noise rate on what it did return. Exact shape equality
finds copy-paste that was never edited afterwards, and copy-paste that was never
edited afterwards is the *cheapest* kind to find by other means.

The pairs above share names, argument order, docstring ("Preprocessed-array
equivalent of X"), and call structure — they differ only in the accessor
expressions. If there's a cheap signal here it is probably naming adjacency
(`X` and `_X_processed` in one file) rather than tree shape.

Also: `codeq clones src/persona` → `error: unexpected argument 'src/persona'`.
`unused`, `untested`, `hotspots`, `tree` and `search` all take a positional
path; `clones` is the only one that insists on `--path`. Small, but I hit it on
my first try.

### 2. `unused` reports every decorator-registered entry point

```
$ codeq unused src/persona
src/persona/api/app.py:97: function gen_personas visibility=public
```

`gen_personas` is a FastAPI route. Its only "reference" is the `@app.get(...)`
line directly above it. This is not an exotic pattern — it is how every Python
web framework, every JS decorator-based framework, every plugin registry and
every CLI built on `click` names its entry points. On this repo it was 100% of
the `unused` output, so the signal-to-noise on the command was zero.

The fix looks cheap and it is the same mechanism the `#[test]` work already
landed: the decorator is *in the parse tree, attached to the definition*. Either
suppress a definition that carries any unrecognised decorator, or (better, since
suppression can hide real dead code) keep the row and tag it
`decorated=@app.get`, letting the reader discount it in one glance. Python's
`decorated_definition` node is already being read for pytest markers.

### 3. In a data-heavy repo, `tree --min-symbols` points at the data

The help for `--min-symbols` says it "jumps straight to where the code lives in
a data- or asset-heavy repo". This is a data-heavy repo — 2,106 PNGs, 280 JSON
datasets, 12 Python files — so I used exactly that, and:

```
$ codeq tree --min-symbols 5
    src/persona/api: files=3 symbols=14
    src/persona/data: files=2667 symbols=2667   ← zero lines of code
    src/persona/lib: files=3 symbols=39
```

The data directory outranks the source by 68x, because each of its 281 country
folders has a `README.md` and **a markdown heading is a definition**. Indexing
docs is a good decision and I don't want it reverted; the interaction is that
prose headings and functions land in one undifferentiated `symbols` count, so
the one knob offered for asset-heavy repos cannot separate them here. Same for
`map --totals`: `markdown symbols=2685, python symbols=161`.

What actually oriented me was `codeq files --language python` (12 rows, done).
`files` has `--language`; `map` and `tree` don't. Putting `--language` on both —
or having `--min-symbols` discount markdown — would make the documented
orientation path work on the repos its own help text names.

### 4. `map --totals` spends its headline number on binaries

```
totals: files=2683 bytes=60048758 symbols=2846
(unknown) files=2108 bytes=56855576 symbols=0
```

95% of those bytes are flag PNGs that codeq cannot index and shouldn't. The
`(unknown)` row is accurate but reads like a gap in coverage rather than "this
repo is mostly images". Saying so directly — *2,108 files (56 MB) are not
indexable source* — would turn a number I had to interpret into one I could use.

### The refactor itself, for the record

Four changes, all found through codeq: hoisted the duplicated location-string
split into `lib/format.py`; collapsed the two persona-drawing loops into one
`gen_sample` taking a draw strategy; replaced `_merge_processed` and
`_ancestor_leaf_features` with delegations to their twins; and moved
`resolve_key` down into `lib/` to break the cycle. The `resolve_*` pair stays
duplicated on purpose. Both code paths now produce
byte-identical personas for the same seed — which they always should have, and
which nothing was checking.

## 2026-08-12 — Reading the `#[test]` attribute instead of guessing from the name

The question that prompted this: what are we doing by *label* that could be done
by *structure*? Most of codeq's name-based commands have to be — cross-file name
resolution is a different project — but test recognition was a clear exception,
and the docs already confessed it: "codeq reads no attributes, so a `#[test]` fn
with none of those three still slips through."

The reproduction is embarrassing in its simplicity:

```rust
pub fn parse(s: &str) -> usize { s.len() }

#[test]
fn parses_an_empty_string() { assert_eq!(parse(""), 0); }
```

`codeq tests parse` → "has no tests referencing it". The test is two lines below
the function, and codeq was looking at the file path, the function's name, and
the name of the enclosing module — three spellings — while the answer sat in the
parse tree one node away. Same for `@pytest.mark.parametrize`, `@Test`, `[Fact]`.
Any framework that does not make you *name* your tests `test_*` was unsupported
by accident rather than by decision.

### What made it more work than it looks

Grammars disagree about where an annotation lives, and all three shapes are real:
Rust puts `attribute_item` *before* the item as a sibling, Python wraps the
definition in a `decorated_definition` *above* it, and Java/Kotlin/C#/PHP hang
`modifiers`/`attribute_list` *inside* the declaration. The detector checks all
three; a language with none yields nothing and the old label signals still run.

The marker list is fixed rather than a "contains the word test" pattern, which
would have swept up `#[cfg(test)]` on a module (that marks a *container*, already
handled) and `@TestOnly` on production code. There is a test for the negative
case, because widening what counts as scaffolding silently *hides* dead code —
`unused` and `untested` share this predicate.

`#[tokio::test]` is matched by any attribute path ending in `::test`, so every
async runtime's wrapper works without enumerating them.

### What stays name-based, and why

JS/TS `it(…)`/`describe(…)` and Go `func TestXxx` are not attributes — one is a
bare call, the other a naming convention the compiler itself relies on. Those
still lean on path and name, which is now a documented limit rather than a
silent one.

## 2026-08-12 — `stats` folded into `map --totals`, and a `graph` command

### `stats` was a strict subset — but not a free one

The instinct was to delete it outright. Measuring first changed the shape of the
fix: `stats` is 11 ms on redis where `map` is 30 ms, and on a 12,664-file tree
29 ms against 202 ms. The gap widens with the repo because `map`'s cost is not
its aggregates — those are single GROUP BY queries — but the two full row scans
it does for directories and manifests.

So the cheap path was real and worth keeping; what was not worth keeping was a
second command name to choose between. `map --totals` stops after the aggregate
sections. `totals` also gained `bytes`, the one fact `stats` stated that `map`
only left derivable by summing its language rows — without it the fold would have
lost something and been a removal dressed up as a refactor.

### The renderer bug it surfaced

`map --totals` first shipped printing its language table and *silently dropping
its own headline totals*. The text renderer collapses an object to just its rows
when it has one array and `len() <= 2` — a positional test, meant for `{matches,
truncated}`. `{totals, languages}` has the same shape and a completely different
meaning. The condition now names the metadata key it is allowed to hide, so the
rule is about what a field means rather than how many there are.

Worth noting the near-miss: I nearly worked around it by flattening the summary's
fields to the top level, which would have made `map` and `map --totals` disagree
about where totals live — reintroducing exactly the per-command schema variance
`ROW_KEY_RULE` exists to prevent. The renderer was the thing that was wrong.

### `graph`

Asked for after the FK work: can codeq draw the schema? It could not — relations
were only reachable one node at a time. `codeq graph` emits every relation edge
as `from -> to`, filterable by `--relation`/`--language`/`--path`, with
`--mermaid` for a renderable document.

**Relations only, and deliberately not calls.** A relation edge comes off the
parse tree (`impl T for X`, `REFERENCES orgs`), so the graph is exact. The call
graph is name-based, and a rooted `callees --depth 3` on flask already pulls in a
`field response_class` and an unrelated `update` from an examples directory — at
whole-repo scale that compounds at every hop into something mostly
plausible-looking and wrong. Drawing it would make codeq look more capable and be
less trustworthy, which is the trade this project keeps refusing.

An edge row renders as an arrow rather than through the generic `k=v` tail, which
had put the row's *language* first — the only bare field it had — and left
`from=`/`to=` as trailing metadata, burying the one thing an edge says.

## 2026-08-12 — SQL foreign keys, and a doc that had been broken for a while

### The edge, finally

Foreign keys were deferred twice on the grounds that a FK is not inheritance and
`--relation implements|extends` had no honest slot for it. The resolution was
simply to add the slot: `references` is its own relation kind, so `supertypes
orders` reads as "what orders points at" and `implementors orgs` as "what points
at orgs", with `--relation references` to ask for only those. No command claims
an is-a that does not exist, and no other language emits the kind.

Three syntactic homes, all of which real migrations use: an inline column
`REFERENCES`, a table-level `CONSTRAINT … FOREIGN KEY` inside the `CREATE`, and
an `ALTER TABLE … ADD CONSTRAINT` afterwards. The third is the one worth naming
— missing it would lose exactly the keys a migrations directory adds later,
which is the same trap `ALTER TABLE … ADD COLUMN` set for members a few hours
earlier. Having been caught by it once, I looked for it this time.

### The doc rot

`CLAUDE.md` had a bad merge in it: the `search` bullet appeared twice, and
stranded between the two copies was an orphaned tail of the `unused` bullet —
five lines of prose attached to nothing. I noticed it while editing the file for
the SQL work and left it, which was the wrong call; it had been there long enough
to be quietly re-read by every session.

Repairing it needed care rather than deletion. The duplicate `search` block was
not identical to the survivor: `--stdin` was documented *only* in the copy that
had to go, and the orphaned `unused` fragment carried one fact the real bullet
lacked (that test recognition matches `codeq tests`, so a Rust `mod tests` unit
test is not dead code). Both were folded into the surviving bullets before the
duplicates went. A merge conflict resolved by picking a side would have silently
dropped two documented behaviours.

## 2026-08-12 — A cross-check sweep over 26 repos, and the three things it found

Method, per the technique that keeps working: don't look for bugs, look for two
commands that *must* agree and check whether they do. A script asserted seven
such invariants across the codeq repo and all 25 corpus repos — `api` vs public
`symbols`, `search -c` vs per-file match counts, `search -l` vs match paths,
`callers` vs `references`, `tests` vs `callers`, `members` vs `symbols --owner`,
`deps` vs `imports`. The invariants themselves all held. What found bugs was the
script *crashing*, and one flag that turned out not to exist.

### 1. JS: `proto.use = function use(){}` was not a definition

The sweep died on `KeyError: 'name'` in express. Chasing it: `callers gettype`
returned a row with a path and a line and no name, because `at
lib/router/index.js:469` finds no definition there — and it finds none because
codeq did not index a single one of that file's five methods.

Express's entire router API — `proto.param`, `proto.handle`,
`proto.process_params`, `proto.use`, `proto.route` — was invisible. `outline` on
a 673-line core file of one of the most-used packages on npm returned nothing.
A minimal fixture: seven function definitions, codeq found two.

Module-scope property assignment is how essentially all pre-ES6 JavaScript is
written and how much of Node still is. It is now a `method`, named by the
property (`use`, not `proto.use`) — what a call site writes, and the same choice
made for a schema-qualified SQL table. `exports.x` / `module.exports.x` count as
public, since in CommonJS that assignment *is* the export; other objects stay
private, because knowing that express's `var proto = module.exports = …` escapes
needs data flow codeq doesn't do, and guessing would be worse than under-stating.

I verified the fix with `astgrep --explain router.js:5`, built earlier the same
day for exactly this — reading node kinds off the tree instead of guessing them.
That is the first time one of these fixes has paid for another.

### 2. `callers` returned rows that were not callers

Independent of the above, and confirmed in six corpus repos across Python, C++,
C, JS and PowerShell: a reference at module scope has no enclosing definition, so
`callers` emitted `{"path": …, "line": …}` with no `name`. Text rendered it as a
bare `a.py:5`, which reads like a rendering bug.

The row is a real fact — a Python module-level call runs at import — so dropping
it would lose something. But `callers` promises *definitions*, and the docs' only
guidance was "ignore it", which is a tool emitting output it tells you to
discard. It now carries `scope=file`. Two consumers had crashed on the missing
key rather than noticing the fact it was carrying, which is the argument for
labelling over blanking: a named absence is data, an unnamed one is a bug report.

### 3. `callers` and `callees` had no `--limit`

Found because the sweep passed `--limit 0` to `callers` and got `error:
unexpected argument`, which made the `tests ⊆ callers` invariant fail in five
repos — a false alarm that was still worth having. Fifteen of seventeen list
commands take `--limit`; these two did not, and they are the likeliest to return
a lot. `callers serverLog` in redis is 227 rows and 11 KB with no way to bound
it, while `references serverLog --limit 5` gives you five.

### The recurring hazard, now hit twice

Changing extraction without bumping `SCHEMA_VERSION` ships to nobody: the files
have not changed on disk, so the freshness check does not reparse them and the
new facts never appear. I hit this while testing the JS fix — symbols still
absent after a rebuild of the binary — and it is the same trap that left the SQL
commit red. Both bumps are recorded in `db.rs`; the pin test in `cli.rs` is what
catches a missing one, so it is worth keeping deliberately brittle.

## 2026-08-12 — Fixing the four things the SQL session complained about

The friction log below was written the same day; this is what came of it. Worth
recording that all four were found by *using* codeq to build codeq, not by
reading its code — none of them is visible from the inside.

### `astgrep --explain <file>:<line>`

The complaint was that writing an astgrep pattern required knowing node kinds
codeq would not tell you. It now prints the tree covering a line: each node's
`kind`, the `field` it fills in its parent, and its source.

Fields turned out to matter as much as kinds. `callsite.rs` is written against
field names precisely because they are the stable part of a grammar, and a walk
that showed only kinds would have hidden the more useful half.

The sharp edge, and the reason there is a test for it: a line past the end of the
file. The obvious implementation clamps the byte range to `src.len()`, and the
root node contains that, so you get the whole file's tree back — a confident
answer to a question with none. That is the exact failure mode `--explain` exists
to fix, reproduced inside it. It returns empty with a note now.

Rendering needed one small honesty: the row's source preview is `src`, not
`text`, because `text` means "this row *is* the matched source" to the renderer
and would have swallowed the `kind` that is the actual payload.

### `search PATTERN [PATH]...`

Positional paths, matched as substrings like `--path`, OR'd, composing with `-g`
by AND. `codeq search TODO src` now works, which the docs' "superset of grep"
claim had been quietly overstating.

### `rows` in the envelope

Row *keys* were unified in `codeq.result.v2`; containers were not, and can't be —
`search -l` returns `files` where plain `search` returns `matches`, and those are
different facts, not different spellings for one. So rather than force a shape,
the envelope now states which one it used: `"rows": "data.matches"`. Derived from
the value, so a new command is described without being registered, and absent
when a result holds several lists (`about`, `map`) — where guessing would be
worse than silence. Additive, so the schema stays `v2`.

### `unused`'s exclusions

Moved out of a `format!` in `cmd_unused` into `facts::UNREFERENCED_BY_DESIGN`, a
table of (language, optional kind). It had grown an entry per language with an
external contract — GraphQL, protobuf, Terraform outputs, SQL — which is a
category that keeps arriving, and it belonged next to the other things that
describe a language rather than inside a query string in a different file.

## 2026-08-12 — Friction from a session spent adding a language

Three things slowed the SQL work down. Recorded before they are forgotten,
because each is a gap in codeq rather than a gap in my knowledge of it.

### 1. `astgrep` has no way to learn the node kinds it requires

The dominant activity of this session was "what does this grammar call its
nodes?", and codeq answered none of it. I wrote a throwaway `examples/sqldump.rs`
to walk the tree and print kinds, then deleted it.

This is not a grammar-development complaint — it is the missing half of a shipped
feature. `astgrep '$X.unwrap()' --language rust` asks an agent to write syntax
against a grammar it has never seen, with no way to discover kind names, and the
failure mode is a *silent empty result* that reads exactly like "no matches". The
docs already concede the pattern must be "structurally exact"; discovering what
exact means currently requires leaving codeq.

Something like `codeq astgrep --explain <file>:<line>` (the parse tree at a
position, kinds and fields) would close it, and would be honest dogfooding: I
needed it, worked around it, and the workaround was a Rust file.

### 2. `search PATTERN FILE` is not accepted, despite "a superset of grep"

I typed `codeq search 'markdown' CLAUDE.md` — grep's most basic calling
convention — and got `unexpected argument 'CLAUDE.md'`. Twice, in one command,
because I did it again for README.md in the same breath. The fix is `--path`.

That claim of superset is load-bearing in the docs, and grep's positional file
argument is the first thing anyone tries. It is also internally inconsistent:
`imports [PATH]`, `outline <file>`, `api <path>`, `read <file>` and `tree [path]`
all take a positional path. `search` is the one that doesn't, and it is the one
most likely to be reached for out of muscle memory.

### 3. Row *keys* are unified; row *containers* are not

`codeq.result.v2` made `line` mean `line` everywhere. But `symbols`,
`definitions` and `members` put rows in `data` directly, while `search` nests
them under `data.matches` and `references` under `data.references`. I wrote a
test helper against the wrong shape and got `Option::unwrap()` on a `None` —
the same class of "learn it per command" tax v2 was meant to end, one level up
from where it was fixed.

Not obviously a bug: `search` carries `matches` alongside other keys, so the
container is doing work. But an agent cannot tell which commands nest without
trying, and nothing documents it.

### Also worth noting: `unused`'s exclusion policy is now a growing SQL string

`graphql`/`proto` wholesale, `hcl` + `output`, and now `sql` + three kinds — all
hand-written into one `format!` in `cmd_unused`. It has grown once per language
that has externally-consumed declarations, which is a category that will keep
appearing. It belongs next to the language's other properties (`facts::support`),
not in a query string three thousand lines away.

## 2026-08-12 — SQL is a language, and codeq was treating it as a data file

`.sql` was in `index::NO_EXTRACTION` next to JSON, YAML and TOML: indexed for
search, zero definitions. That was a category error. JSON has keys; SQL has
*declarations and uses*, which is the entire thing codeq indexes. `definitions
users` came back empty on repositories whose `users` table half the code reads.

### Why this one first

The audit that produced it asked which non-code formats deserve extraction, and
the honest test is: does the format have named things, and references to those
names? Generic JSON fails — nobody looks up `$.compilerOptions.paths` by name,
and turning every key into a definition would flood `symbols`, `unused` and
`hotspots`. SQL passes outright, and it pays off *across* languages, because
reference matching is textual: the schema is the one artefact every language in a
polyglot repo talks to, and `references users` finds the table named inside the
SQL string in the Python that queries it. `callers touch_updated_at` finds the
`CREATE TRIGGER` that fires it — a call-graph edge in a language with no calls.

Grammar: `tree-sitter-sequel` 0.3.11. `CREATE PROCEDURE` doesn't parse in it;
that is now a documented limitation rather than a silent empty answer.

### Four things a query could not do

**Anchors, or every declaration gets named several times.** `create_trigger` has
three `object_reference` children — the trigger, the table it fires on, the
function it executes — and a column with a `REFERENCES` clause has two
identifiers. The first draft of the query produced three facts named after one
trigger. `.` after the introducing keyword pins each capture to the declaration's
own name. Worth stating because the failure is *over*-reporting: the output looks
populated, and is wrong.

**Quotes.** A Postgres enum declares its members as quoted literals, so a
verbatim capture answers `members mood` with `'sad'` — a name no query for `sad`
ever matches.

**Strings vs numbers.** This grammar gives every scalar the same `literal` node,
so `'admin'`, `42` and `NULL` are one kind. Marking them all as strings would
push numeric literals out of `--in code`; the quote character decides instead.

**Schema qualification.** `CREATE TABLE analytics.events` is stored as `events`,
because the fact is stored under the name it is *looked up* by. The qualified
form survives in the signature.

### The bug that was actually interesting

Columns are members, and members with no owner are dropped. Owners resolve by
walking up the parse tree — which finds `CREATE TABLE` and misses `ALTER TABLE`.
In a migrations directory that is where most columns live, so the newest half of
a schema was the invisible half.

The fix — fall back to the stored `owner` tag when containment finds nothing —
broke `members_are_structural_not_name_based` immediately, and correctly. That
test exists because `members` is the one graph command that is *not* name-based,
and the `owner` tag is a name: with two `class C`s it hands each one the other's
members. So the fallback is gated on the type name having exactly one definition
in the repository — counted *before* `--path` narrows it, since a filter chooses
which owner you're asking about, it doesn't make an ambiguous name unambiguous.

I would have shipped the unguarded version. The existing test caught it in one
run, which is the second time this month a test written for an unrelated reason
has been the thing that stopped a regression — see the cross-walk invariant note
below.

### `unused` needed a kind-level exclusion, not a language-level one

The instinct was to exclude SQL wholesale, the way GraphQL and protobuf are
excluded as contracts consumed from outside. Wrong: a table, view, column or
function *is* named by whatever queries it, and codeq finds those names, so an
unreferenced one is a genuine finding. An index is named exactly once in the
world — at its own `CREATE` — and so is a trigger; an enum variant is referenced
by data, not code. Those three would appear in every repository with a schema,
always. Excluded by kind; the rest report normally.

### Not done, deliberately

Foreign keys. `REFERENCES orgs(id)` is the most structurally interesting fact in
a schema, and `implementors`/`supertypes` are right there — but a foreign key is
not inheritance, and `--relation implements|extends` has no honest slot for it.
Forcing it in would make `supertypes` lie to save writing a third relation kind.
Left out until it can be its own edge.

## 2026-08-12 — `codeq.result.v2`: one key per fact, across every command

With no downstream users yet, the row schema got the breaking change it needed.

### The bug the rename fixes

`--fields line` worked on `callers`, `callees`, `references`, `members`,
`untested`, `hotspots`, `api` and `tests` — and matched **nothing** on
`definitions`, `symbols`, `imports` and `outline`, which spelled it `start_line`.
The internal facts-table column name had leaked onto the wire for four commands
and not the other eight.

Two things had been hiding it. `render_row` carried
`int_of("line").or_else(|| int_of("start_line"))`, so text output looked
uniform — and `bench/textmode.py`, the harness whose whole job is catching text
that disagrees with `--json`, carried *the same fallback*. The check and the
thing being checked papered over the identical crack. Both are gone; the harness
now reads one key, so a future split fails it.

I hit this twice while dogfooding before recognising it as a bug rather than my
own bad memory, which is the tell: a schema you have to memorise per command is
one an agent gets wrong silently.

### The rule

Documented as `ROW_KEY_RULE` in `envelope.rs`, since a convention with no written
home is one that drifts back:

- **`name`** — the definition this row *is*.
- **`symbol`** — the definition this row sits *inside*. Only `search`,
  `references` and `astgrep` matches have one; never a row's own identity.
- **`line`, `col`, `byte`** — primary position. **`end_line`, `end_col`,
  `end_byte`** — extent.

Rust fields keep the `start_` prefix that mirrors the DB; `#[serde(rename)]`
governs the wire. That kept the diff to the schema instead of the query layer.

Renamed on the way: `callers`/`callpath`/`context`-chunk/`explore`-omitted rows
from `symbol` to `name` (each *is* a definition), and — a scope call beyond what
was approved — `implementors.subtype` / `supertypes.supertype` to `name` too. I
had argued a week ago that those two distinct spellings were a readability win.
They aren't: the command you ran and the row's own `relation` field already say
which side of the edge you are on, so the special-case key bought nothing and
cost the one thing the rule exists for. Shipping the rule with a silent exception
would have been worse than either choice.

`astgrep`, `references` and `search` keep `symbol` — correctly. Those rows are
matches sitting inside a definition, which is a different fact from being one.

### Cost

265 tests, 36 of which asserted the old keys; the failures were the change
working. `bench/differential.py`, `completion.py` and `textmode.py` all sampled
`start_line` and needed updating too — worth noting for anyone who thinks a row
key is an internal detail: four harnesses and a hundred assertions disagreed.

## 2026-08-12 — duplicated *operations*: two fixed, one refactor declined again

Asked whether codeq has operations that mean the same thing. `clones` — the tool
built for exactly this — found nothing worth acting on: `cmd_implementors`/
`cmd_supertypes` are structurally identical but already share `impl_edges` and
differ only in a field *name* the output is better for keeping (`subtype=` vs
`supertype=`); `mcp::opt`/`optn` and `print_range`/`print_range_raw` are 6-line
pairs that differ by type and would be worse behind a boolean parameter.

That is `clones` working as designed and also its limit: it matches syntax
shapes, and the duplication that actually bites is **semantic** — the same
question answered twice in different code, where the copies drift.

### `via` was classified twice, and the copies had already drifted

`--resolved` tags each row with how it reaches a definition. `Resolver::via`
(callers/references) emits same-file / imported / ambiguous / unresolved. An
inline branch in `callees_of` emitted same-file / imported — **no `ambiguous`
arm**. Same graph, same facts, and the identical situation reported two ways:

```
mod a; mod b;  fn run() { helper(); }   # a.rs and b.rs both define helper

callers helper --resolved  ->  main.rs  ambiguous
callees run --resolved     ->  a.rs  imported          # each reads as confident
                               b.rs  imported
```

The `ambiguous` case is precisely the over-match `callees --resolved` exists to
narrow, so the copy that lost the arm was the one that most needed it. Both now
go through one `via_of(same_file, reachable_defs)`.

The two anchors genuinely differ — Resolver asks "can this *reference's* file
reach a def?", `callees_of` asks "can NAME's *own* file reach this callee's def?"
— so this was never a candidate for `clones`. The shared thing is the
vocabulary, not the traversal.

### `unused` and `untested` disagreed about test scaffolding

Both ask "is this definition test scaffolding, and therefore not the thing I'm
reporting on?" `unused` used `is_test_site` (path **or** test-like name **or**
enclosing test module); `untested` used `is_test_path` alone. So:

```rust
pub fn real() {}
#[cfg(test)]
mod tests { pub fn fixture_builder() {} #[test] fn t() { super::real(); } }
```

`untested` reported `fixture_builder` as untested public API. `unused`, asking
the same question one screen away, already knew better. Now both use
`is_test_site`.

### Four hand-rolled BFS walks — left alone, deliberately

`cmd_callers`, `cmd_callees`, `cmd_affected` and `cmd_callpath` each hand-roll the
same breadth-first skeleton (~25 lines: seed a frontier, expand one hop, dedupe,
tag depth, stop at `HOP_CAP`). The obvious move is to extract a generic walk.

Checked for drift first, since that is the only thing that makes duplication
*cost* anything: depth 1 means a direct edge in all of them, every node is
reported at its shortest depth, no walk emits its own seed, and `HOP_CAP` is
applied in all four. They agree.

A generic version would need closures for the expand function, the dedupe key
and the row constructor, to serve three call sites with three row types and two
different answers to "what about a caller with no enclosing symbol" — five type
parameters to remove a loop that is correct in all four copies. Same verdict as
the `commands.rs` split, on the same grounds. What went in instead is a test
(`the_call_graph_walks_agree_on_depth_semantics`) that pins the invariants across
the walks, so a fifth one that gets them wrong fails loudly. Cheaper than the
abstraction and catches the same class of bug.

### Reported, not fixed

- **`callers` rows key the found definition as `symbol`; `callees` uses `name`.**
  Mirror commands, same concept, different key. `symbol` is faithful to how a
  caller is computed (the enclosing symbol of a call site) and matches
  `references`, so it isn't wrong — but an agent consuming both pays for it.
  Renaming breaks `codeq.result.v1` and every `--fields` invocation, so it wants
  a deliberate schema decision, not a drive-by.
- **`stats` is a strict subset of `map`** (`files`, `bytes`, `by_language` are
  all in `map`) and appears in neither CLAUDE.md nor SKILL.md — but it is an MCP
  tool, so removing it is a breaking change to that surface.

### The `path` alias is gone — and my defence of it was bad

Two turns after arguing the alias was "earning its keep", it was removed. Worth
recording *why the argument was wrong*, because it was a plausible-sounding one.

The evidence I cited: the agent session doing the review was holding an MCP tool
list that still named `codeq_path`, which looked like exactly the cached-list
case the compat arm existed for. But that session was **my own dev environment**,
not a user. With no downstream consumers, there was nothing to be compatible
with. I mistook an artifact of running the tool for a constituency.

The alias also wasn't neutral. `--path` scopes fifteen commands and `path` is a
key on every row, so in codeq the word means "file path" everywhere except here.
Keeping the alias made the natural mistake *harder* to diagnose:

```
$ codeq path src/db.rs          # before
error: the following required arguments were not provided:  <TO>

$ codeq path src/db.rs          # after
error: unrecognized subcommand 'path'
```

The first reads as "you called this command wrong"; only the second says "that
isn't the command". A compat shim that degrades the error message for a typo,
in exchange for compatibility nobody needs, is worse than no shim.

(clap's near-miss suggestion for `path` is `at`, not `callpath` — edit distance
doesn't favour a substring. Left as-is; a wrong suggestion under a correct error
beats a correct-looking error about the wrong thing.)

### `callpath` vs `path` — the rename was complete outside, unfinished inside

Checked the one pair that is a literal alias. Every *surface* is correct: the CLI
takes `callpath` with `path` as a clap alias, `capabilities` lists only
`callpath`, and MCP advertises only `codeq_callpath` while `build_args` still
accepts `codeq_path` for hosts holding a cached tool list. That last compat arm
is currently earning its keep — the agent session doing this review is itself
holding a tool list that still names `codeq_path`, exactly the case the comment
predicts. Keep it.

What hadn't followed was the code: `cmd_path` was the **only** handler of 36 whose
name didn't match its command, the clap variant was still `Path`, and the
function's doc comment opened `/// \`path <from> <to>\``, presenting the old name
as current. This cost a lookup during this very session — `codeq definitions
cmd_callpath` returned "no symbol named", so the obvious way to get from a
command to its handler failed on precisely the command whose name had moved.
Renamed; `cmd_*` is now 1:1 with the command list (`mcp` aside, which isn't a
query). Both CLI spellings still work.

## 2026-08-11 — a bug hunt on the output layer: four found, all in what we'd just shipped

Went looking for bugs rather than features. All four live in the two output flags
added last session plus the code they sit next to, and three of them share one
shape: **the answer was incomplete and didn't say so** — the exact failure
`--max-per-file` was designed to prevent.

### `search` never said it stopped at `--limit`

The worst one, and the oldest. `codeq search the` on this repo returns 100 rows
out of 4,006 and says nothing. No stderr note, no `note` field. The agent-facing
docs compensate by telling the agent to *remember* that `--limit` defaults to 100
— which is a documentation patch over a tool that won't tell you.

What makes it a real bug rather than a design choice: **`--json` had the fact all
along.** `SearchData` carries `truncated: bool`, and it lands in the JSON. The
text renderer drops it, in `render_value`:

```rust
} else if arrays == 1 && map.len() <= 2 {
    // A single result list (+ maybe a scalar like `truncated`): just rows.
```

The comment names the thing it's discarding. So the default output surface —
the one an agent actually reads — was strictly less informative than `--json`,
which is precisely the invariant `bench/textmode.py` exists to protect. textmode
missed it because it checks that locators/counts/context *appear*, not that
nothing was dropped. Fixed by reading `truncated` in `print()` and reporting it
on stderr, next to the `--max-per-file` note that was already doing this right.

### `--max-per-file` surveyed the first `--limit` rows, not the repo

`--max-per-file` is sold as "the *shape* of a result — which files mention this,
roughly how often". It wasn't answering that. `--limit` is applied during the
scan, the cap in the envelope afterwards, so the limit was spent on raw matches
including the ones the cap was about to throw away:

```
$ codeq search "the" --max-per-file 1 --group     # before
.claude/skills/codeq/SKILL.md (1 of 55)
CLAUDE.md (1 of 45)
```

Two files. The repo has 30, and CLAUDE.md's true count is 139, not 45 — the
100-row budget ran out inside it. Both numbers on screen were wrong, and the `of
N` form makes them look authoritative.

Fixed by making the two budgets stop fighting: `ScanOpts::per_file` mirrors the
flag, matches past a file's cap are still collected (the envelope needs them to
count) but don't consume `limit`. One residual case a test caught — when the
limit fires *mid-file*, that file's count is partial, so the scan now finishes
the current file before stopping. Now all 30 files appear with counts that match
`search -c` exactly.

### `--fields` silently cancelled `--max-per-file`

Both flags exist to cut tokens, so combining them is the natural move — and it
gave *less* reduction than either alone. `project()` ran first and removed the
`path` column `cap_per_file` groups by, so the cap found no paths and kept
everything: `--max-per-file 2 --fields symbol,line` returned 100 rows instead of
4, without a word. Fixed by capping before projecting.

### `--group` mangled every one-row-per-path command

```
$ codeq files --group        # before
CLAUDE.md (1)
: markdown size_bytes=23823
```

`render_row` suppresses the path under a group header but still emitted the `: `
separator, so a row with a path and no line — `files`, `tree` — came out led by a
bare colon, at double the line count. Cosmetic next to the other three, but it's
the kind of thing that makes output look untrustworthy.

### The pattern worth remembering

Every one of these was found by **making two commands that must agree disagree**:
`--max-per-file`'s counts against `search -c`, and text output against `--json`.
Neither the unit suite (fixtures too small to reach `--limit 100`) nor
differential (never passes these flags) can see any of it. A flag that reduces
output needs a test that reduces *a lot* of output.

## 2026-08-08 — merging two parallel lines: three bugs that neither side could have found

`main` had moved 14 commits while this line built 29, both from the same base.
Every source file conflicted, and the schema had been bumped independently to 21
on one side and 17 on the other. Resolving it produced the most interesting bugs
of the week, because **each one needed both halves to exist**: every side's tests
passed on its own branch, and three real defects lived only in the combination.

**1. Ruby fields silently vanished.** The other line added Ruby `attr_accessor`
extraction. This line added a rule that a `field`/`variant` fact with no resolvable
owner is dropped, because an owner-less member is exactly the bare name the fact
kind exists to avoid. Ruby's grammar names its nodes `class` and `module`, with no
`_declaration` suffix, so they were absent from `TYPE_KINDS` — every Ruby field
resolved to no owner and was discarded. Two correct changes cancelling out.

**2. `--calls` stopped seeing component usage.** The other line moved call
detection onto the parse tree, which is a clear improvement: a declaration, a macro
name and prose are all handled without guessing. But a Svelte component is *used*
by writing `<Btn/>`, which no grammar reports as a call node. Tree-sitter is right
that it isn't a call; codeq is right that it's a usage. The byte-level check that
knew this was now unreachable, so `--calls` returned nothing for a component that
was plainly used. Markup instantiation is now checked *in addition to* the parse
tree, so the tree keeps rejecting declarations and prose.

**3. `callees` went empty for every component.** Worse version of the same shape.
`calls_in` asks whether a grammar exists; for `.svelte` and `.vue` one does — but
it is a *markup* grammar, and the `<script>` body comes back as a single opaque
`raw_text` node. So parsing succeeded, found zero calls, and returned `Some(empty
map)`. Callers treat `Some` as authoritative, so "I cannot see this code" was
indistinguishable from "this code calls nothing". `calls_in` now returns `None` for
embedded-code languages, which routes them to the byte scan that can see inside.

The pattern in all three: an improvement that is correct in isolation, meeting a
different improvement that is also correct in isolation. No amount of testing
either branch would have surfaced them.

**A trade-off adopted knowingly, not a bug:** the parse-tree path cannot see inside
macro invocations, because tree-sitter treats a macro body as an opaque token tree.
`callees cmd_capabilities` no longer lists `support`, which is called as
`crate::facts::support(l)` inside a `json!{…}`. The old byte scan found it. This is
the same limitation `astgrep` already documents, now inherited by `callees`; the
precision gained elsewhere is worth it, but it is a real loss and belongs written
down rather than discovered.

**On the schema:** v14-v21 on one line and v14-v17 on the other described different
migrations under the same numbers. No index can have followed both, and the version
is a cache-invalidation token rather than a schema identity, so the two histories
are recorded together for provenance and the version set to 22 — above everything
either line issued, so every existing index rebuilds exactly once.

**On the benchmark table:** both lines had published numbers, measured on different
corpora (25 repos vs 20) and on different code. Neither describes the merged
binary. Rather than pick the flattering one, the README now carries the larger run
with an explicit note that it predates the merge and needs re-running.

## 2026-08-08 — `tree`, `callees --examples`, `.codeqignore`, and a refactor I talked myself out of

### `tree` — the orientation gap `map` couldn't reach

`map` groups by *top-level* directory. On a nested repo that is close to useless:
on `persona` it reports one row, `src files=2677 symbols=58`, which hides both
where the code is and where the bulk is. `stats` gives totals, `files` gives a flat
list. Nothing showed shape.

`codeq tree` reports the directory hierarchy folded to `--depth`, with each
directory's file and symbol counts aggregated from *everything below it*, so
folding summarises instead of truncating. On persona, eight lines:

```
src/persona/api:     files=3    symbols=15
src/persona/data:    files=2667 symbols=0   dirs=194
src/persona/lib:     files=3    symbols=30
src/persona/scripts: files=2    symbols=9
```

That is the whole story of the repo — all 54 symbols live in 8 files, and 2,667
files hold no code at all. `--min-symbols 1` drops the data tree entirely: 386
bytes against 44 KB for `find . -type d`, and unlike `find` it answers the question
rather than listing paths.

Deliberately not an ASCII `tree` clone. A plain hierarchy would be *worse* than
`ls -R` for an agent. The three things that make it worth having are the three
things `tree(1)` can't do: it is ignore-aware by construction (no `node_modules`
noise), it counts symbols rather than files, and its output is bounded by depth
rather than by repo size.

### `callees --examples`

The gap recorded last round. `callees` reports what a symbol calls, name-based, so
`subprocess.run(...)` is attributed to every definition named `run` in the repo —
four of them here. `--resolved` fixed it, but only if you already suspected the
answer was wrong. Now `--examples` shows the call *as written*, receiver included:

```
bench/bench.py:23: function run call=subprocess.run call_line=106
```

Four rows, one obvious cause. The over-match no longer needs to be suspected to be
seen — which is the difference between a limitation and a trap.

### `.codeqignore`

The external report asked for this and mis-diagnosed it (see below); the fix is
real regardless. `.gitignore` cannot exclude *committed* data, and data-heavy repos
commit their data. Custom ignore file, gitignore syntax, highest precedence,
picked up on the next query with no reindex step — the freshness tracker already
watched `.gitignore`/`.ignore` mtimes, so it was one entry in that array.

One sharp edge worth knowing, inherited from git and confirmed by a failing test I
had written wrong: `!data/keep.json` will *not* resurrect a file under an excluded
`data/`, because the walk never descends. Exclude `data/*.json` instead.

### The `commands.rs` refactor — measured, then declined

I had claimed twice that `commands.rs` needed splitting, and that the real leverage
was a query layer, citing "43 raw SQL statements here against 2 in `db.rs`". Before
doing the work I measured what that number actually means:

- **49 SELECT literals, but 37 distinct shapes.** Only about a dozen repeat, mostly
  twice. The commands ask genuinely different questions. A "query layer" would have
  been 37 pass-through functions — indirection wearing the costume of abstraction.
- **The file's size costs 0.9s** on an incremental rebuild. The compile argument I
  had implied does not exist.
- Navigating 3,600 lines was never the bottleneck this session: `read --symbol` and
  `outline` addressed it by name every time. codeq solving the problem is a reason
  not to solve it again structurally.

What *was* real: three query shapes duplicated verbatim — definition sites of one
name (`callers`/`references`), all definition sites (`unused`/`hotspots`), and the
file-path list (two dependency-graph helpers). Those are now `def_sites_of`,
`for_each_def_site` and `all_file_paths`; 49 SELECT literals down to 46, and more
importantly the pairs that could silently drift apart on a `kind <> 'import'`
clause now can't.

The 3,600-line split is left undone on purpose. It is a move-only diff that would
degrade `git blame` on the file I have been actively debugging, in exchange for a
benefit I could not measure. Recording it here as a decision rather than a backlog
item: the argument I originally made for it was wrong, and finding that out cost
one `python3` script.

## 2026-08-08 — closing the `persona` report: `hotspots`, `callpath`, and two naming pairs

The remaining three items from the external report. Two were naming, one was a new
command, and the new command needed two design corrections before it was worth
shipping — both caught by looking at its actual output rather than at the code.

**`hotspots` — the inverse of `unused`.** Definitions ranked by how often their name
is referenced in code. Counted in *one* pass over the indexed bodies (tokenize, look
each identifier up in the candidate set) rather than `unused`'s scan-per-name, so it
costs about a single `search` instead of ~700ms of repeated scans. Comment and string
spans don't count and unparseable files are skipped, reusing `search`'s own span
machinery so `hotspots` and `callers` can't disagree about what "in code" means.

Two things were wrong in the first working version:

- **One row per definition, when the count is per name.** The top twelve rows for
  `src/` were twelve different definitions of `path`, each reading `refs=481`. True,
  useless, and actively misleading — it looks like twelve findings. Now one row per
  name, with `defs` reporting how many definitions collapsed into it.
- **`--calls` scored *higher* than counting everything**, which is impossible for a
  subset. Two causes, and I nearly accepted a bad explanation for the first: a
  declaration `fn new(` puts the name in call position, so declarations were counted
  in that mode; and the two numbers I was comparing came from either side of a
  reindex, so part of the gap was the tree changing under me. Fixed by skipping the
  declaration *site* — the same thing `unused` does — and re-measured against a
  stable index, where `--calls` is now a strict subset for all 622 names.

A third bug the tests caught, not the eyeballing: I keyed definition sites by
`(path, line)`, copying `unused`. That works there because `unused` asks about one
name at a time, but `hotspots` counts every name at once, and one line routinely
declares one symbol while calling another — `fn a() { hot(); }`. Every such call site
was silently dropped. On this repo the counts still looked plausible; on the
three-line fixture the answer was empty. Small fixtures find what big ones hide.

`--calls` earns its place: it takes `s` (a one-character helper in `mcp.rs`) from
`refs=372` to `refs=48`, because without it every local variable named `s` in the
repo reads as a reference. The command is still name-based and says so loudly — this
is "which identifier carries weight", not "this definition has N callers".

**`path` -> `callpath`.** The report's strongest rename, and right: in a *code* CLI
"path" reads as a file path, so `codeq path a b` scans as "the path to file a". The
old name stays as a clap alias, and the MCP server still accepts `codeq_path` so a
host holding a cached tool list keeps working. Renaming cost nothing in
`capabilities`, which now derives from clap — the rename and the new command both
appeared there with no separate step. That is the drift fix paying for itself the
same day it shipped.

**`map`/`stats` and `definitions`/`symbols` — help text, not renames.** Both pairs
have defensible names; what was missing was any statement of *which to reach for*.
Each now leads with the distinction in caps (BROWSE many vs JUMP TO one; WHOLE-REPO
TOTALS vs REPO BROKEN DOWN) and names its counterpart. Renaming would have broken
scripts to fix a sentence.

## 2026-08-08 — reply to the `persona` report: three fixed, one claim corrected

Working through the external entry below. Two of its findings were right and are
fixed; one was right about the symptom and wrong about the cause; one rested on a
premise that turns out to be false, which changed what the fix should be.

**`capabilities` drift — fixed, and found twice independently.** The same bug
surfaced in a codeq-on-codeq session the same day, with the same nine missing
commands. It now derives from clap's subcommand list, exactly as the report
recommended, with `capabilities_lists_every_command_the_cli_accepts` asserting
set-equality against `--help`. The report's framing was sharper than the internal
one: `capabilities` is what an *MCP host* reads, so an agent discovering codeq
through MCP could never learn `explore` existed.

**Docs underselling the surface — confirmed and fixed.** Checked rather than
assumed: `CLAUDE.md` contained *zero* occurrences of `at`, `context`, `files`,
`imports`, `importers`, `--glob` and `--limit`. All are now documented in both
`CLAUDE.md` and the skill file, including that `search --limit` defaults to 100 —
the kind of default that silently turns "no more hits" into a wrong conclusion.

**`about` vs `explore` — the premise was wrong, so the fix is different.** The
report says "the *only* difference is whether verbatim source is included". It
isn't. `about` returns full `DefinitionRow`/`Caller`/`Callee` rows — doc comment,
`owner`, byte offsets, the `test` flag on callers — while `explore` trims each node
to name/kind/path/line/signature and spends the budget on source instead. They are
two different products, and the suggested fix (fold into one command with
`--source`, defaulting to today's `about`) would have quietly cost every `about`
consumer the doc comments and the test flags.

That a careful user reading real output concluded they were the same command twice
is the actual finding: the names hide a *bigger* difference than the report thought.
So the walk is now shared (`symbol_graph`, so the two can never disagree about who
is in the neighbourhood) while the presentations stay distinct, and every place an
agent discovers them — CLI help, MCP tool descriptions, `CLAUDE.md`, the skill file
— now states the axis explicitly and points at the other command.

**Ignoring data trees — right symptom, wrong cause.** The report proposes
"respecting `.gitignore` or a `.codeqignore`". codeq is *already* gitignore-aware:
`src/index.rs` builds its walker with `standard_filters(true)` (.gitignore, .ignore,
hidden, parents). Checked persona directly: **2,686 tracked files, 2,386 of them
`.json`/`.png`, and only 10 ignored**. The data is committed, so gitignore support
was never going to help. The real request is a `.codeqignore` (or an opt-out for
files of unrecognised language). Recording this because implementing the first half
of that suggestion would have shipped a no-op and closed the issue.

**Still open from the report:** rename `path` -> `callpath`/`chain`/`route` (their
strongest rename, and I agree — in a code CLI "path" reads as a file path first);
a "hotspots"/most-referenced view as the inverse of `unused`; and the `map`/`stats`,
`definitions`/`symbols` naming pairs.

**Process note:** this log says "new entries at the top", and the external report
followed that. My own recent entries went to the bottom, so the file now has two
conventions. Top is the documented one.

## 2026-08-08 — external dogfood: codeq on the `persona` repo (naming + a capabilities-drift bug)

First sustained use of codeq on a *different* project (a Python demographic
generator, ~12 code files buried in ~2,700 data/asset files) rather than on
codeq itself. It carried a real refactor: `clones --min-nodes 20` directly
surfaced a cross-module duplicate (`cli.format_location` ≡ `generate._fmt_label`)
I'd otherwise have missed, and `references` found every tested call site before I
touched anything. The exploration surface is genuinely strong — the friction was
naming and discoverability, not missing power. Findings, worst first:

- **BUG — `capabilities` under-reports the command set (discovery drift).**
  `codeq capabilities` lists 23 commands; `codeq --help` has ~32. Missing from
  `capabilities`: **`explore`, `path`, `api`, `affected`, `unused`,
  `implementors`, `supertypes`, `deps`, `install`**. Repro:
  ```
  comm -13 <(codeq capabilities | sed -n '/^# commands/,/^#/p' | grep -vE '^#' | sort) \
           <(codeq --help | awk '/^Commands:/{f=1;next} /^Options:/{f=0} f && $1 ~ /^[a-z]/ {print $1}' | sort)
  ```
  For a tool whose whole pitch is "built for AI agents," the machine-readable
  manifest is exactly what an MCP host reads to learn what exists — so an agent
  discovering via `capabilities` can never learn about `explore` or
  `implementors`. Highest-value fix: generate `capabilities` from the same
  command registry as the CLI so the two can't diverge (a test asserting
  set-equality would have caught this).

- **Rename `path` → `callpath` / `chain` / `route`.** In a *code* CLI, "path"
  parses as a file path first; `codeq path from to` is easy to misread as
  "the path to file `from`". Any of the alternatives is unambiguous. Strongest
  rename.

- **`about` vs `explore` are the same concept under two undifferentiated names.**
  Both are a symbol dossier; the *only* difference is whether verbatim source is
  included — and nothing in either name signals which. You must know the docs to
  choose. Suggest folding into one command with `--source/--no-source` (default
  no-source = today's `about`), or renaming so the code-vs-no-code axis is
  visible. Compounded by the bug above: `capabilities` lists `about` but not
  `explore`, so an agent only ever sees the sourceless one.

- **`map` vs `stats`** — both described as "counts"; the names don't convey that
  `map` is a grouped orientation breakdown and `stats` is summary totals.
  **`definitions` vs `symbols`** (minor) — `definitions NAME` is a single exact
  lookup, `symbols` is a listing; a name like `locate`/`def` for the singular
  case would separate them at a glance.

- **Capability wish — an inverse of `unused`: a "hotspots" / most-referenced
  view.** `unused` finds dead code and `map` ranks files by definition count, but
  there's no "which *symbols* are most referenced." Dropping into an unfamiliar
  file (`generate.py`) cold, that's what I wanted: show the load-bearing symbols
  first. Composes from data already in the index (reference counts per def).

- **Docs undersell the surface (discoverability).** The `CLAUDE.md` an agent
  actually reads omits `at`, `context`, `files`, `imports`, `importers`, and
  never mentions the `--path` / `--language` / `--glob` / `--limit` filters on
  `search`/`references`. Working from it I nearly proposed features that already
  exist (`at file:line` reverse lookup, `search --glob`, `--limit`) — the gap was
  discoverability, not capability. Once the drift bug is fixed, generating the
  doc's command inventory from `capabilities` keeps them honest.

- **Repo-specific friction — data-heavy trees inflate the index.** On persona,
  `map`/`stats` report ~2,108 files classified `(unknown)` (280 data JSONs + PNGs
  per country) and each query reindexes ~2,683 files. Respecting `.gitignore` or a
  `.codeqignore` to skip data/asset trees would cut cold-index cost and the
  `(unknown)` noise on non-code-dominated repos. (`.codeq/` self-ignores cleanly —
  verified it never touched the host repo's git status — so no complaint there.)

## 2026-07-22 - session 36: the prune I said was safe, isn't

Session 35 made method/function kinds accurate specifically so the receiver-based
prune could be built "on real facts rather than on a guess". Built the test case
first, and the premise collapsed:

    mod helpers { pub fn stem(s: &str) -> &str { s } }
    impl Db { fn stem(&self) -> u32 { 0 } }

    helpers::stem("x")   // receiver: helpers  -> a FREE FUNCTION
    db.stem()            // receiver: db       -> a METHOD

The rule was "a call with a receiver is not calling a free function". It is
wrong, because **a receiver can be a module path rather than a value**. Rust
`helpers::stem()`, Python `module.func()`, Go `pkg.Func()` and JS `mod.fn()` are
all qualified calls to free functions. Accurate kinds do not help: distinguishing
a module receiver from a value receiver *is* name resolution, which is the exact
boundary codeq declines to cross. Two sessions of work whose stated purpose was
to unblock this, and the honest outcome is that it stays unbuilt.

The kinds work was still worth doing on its own — `--kind method` is meaningful,
`members` is sharper, trait declarations are extracted — but I should record that
I asserted the prune was "now safe" before checking, and it took one fixture to
disprove.

What shipped instead is the version that adds information rather than removing
it: `--calls`/`--on` rows now carry **`on=<receiver>`**.

    a.rs:5:21: let a = helpers::stem("x");  [use_them]  on=helpers
    a.rs:6:15: let b = db.stem();           [use_them]  on=db

Same facts codeq was already computing (the call map is built for those filters
anyway, so this costs nothing new), presented so the reader can make the call
codeq cannot. When a name is both a method and a free function — precisely the
case where `callers` is noisiest — `on=` is what tells them apart. Unfiltered
queries parse nothing and report no receiver, so nobody pays for it who did not
ask.

This is the same lesson as `unused` counting string mentions, and as the
`--resolved` heuristics being opt-in: where resolution genuinely cannot be done,
surface the evidence instead of guessing, and say which it is.

204 tests (+1), textmode ALL PASS.

## 2026-07-22 - session 35: a method is not a function

Session 34 ended by declining to build receiver-based `--resolved` because the
prerequisite was missing: codeq could not tell a method from a free function.

    $ codeq definitions query --fields kind,name   # impl method + free fn
    function query
    function query

Rust's `function_item` and Python's `function_definition` are the same node
whether or not they sit inside an `impl` or a `class`, so the queries cannot tell
them apart and every method came back as `function`. Measured across the
languages first, rather than assumed: **8 of 16 were wrong** — Rust, Python,
Kotlin, Swift, Scala, Solidity, Zig (Java, Go, JS/TS, C++, C#, PHP, Ruby, Dart
already had distinct nodes and were right).

The rule is structural: walk outward from the definition and stop at the first
enclosing definition. A function ancestor means this is nested — a closure or an
inner `def` — and not a member, however deep the class above it. A type body
means it is a method.

Rust needs one more condition, and it is the interesting one. An `impl` block
holds methods *and* associated functions, and only a `self` parameter says which
can be called on a value. So `S::assoc()` stays a `function` while `s.meth()` is
a `method` — which is exactly the distinction the whole exercise exists to serve.
Elixir `def`s stay functions too: they are module functions, called qualified,
not on a value.

Poking at nesting surfaced a **pre-existing gap I had not gone looking for**:
`trait T { fn required(&self); }` was extracted as nothing at all. The Rust
pattern requires `body: (block)`, and a trait method declaration has no body — so
the one thing `implementors` sends you to look at was invisible. Added as
`function_signature_item`.

Two existing tests failed, both correctly: they asserted `kind == "function"` for
a Python method and filtered `members --kind function` to find one. Tests
encoding the old, wrong classification. That is twice in two sessions that a test
broke because the tool got better, which is the good failure mode.

Schema 16 -> 17. 203 tests (+4, 2 updated). differential **4188/4188**
(three more assertions than before, all passing), textmode ALL PASS.

The prune this unblocks — a call with a receiver is not calling a free function —
is now safe to build on real facts rather than on a guess, but is deliberately
left for its own change.

## 2026-07-22 - session 34: filling the coverage gaps the matrix exposed

Session 33's support matrix made the holes legible, which made them worth
closing. Relations went from 12 languages to 17, imports from 17 to 21, and Ruby
gained fields.

- **Relations for Scala, Kotlin, Swift, Dart, Solidity.** Each has inheritance
  syntax codeq simply never read. Dart is the only one of the five that
  distinguishes `extends` from `implements`; Kotlin and Swift do not separate a
  base class from an interface syntactically, so both report `extends`, which is
  what the source actually says.
- **Imports for Zig, Elixir, Lua, Bash.** All four express imports as *calls*
  (`@import("std")`, `alias Foo.Bar`, `require("mod")`, `source ./lib.sh`), which
  is exactly why they were missed: the existing patterns look for import syntax.
  Reused the `@guard` mechanism built for Ruby's `require`.
- **Ruby `attr_accessor :a, :b` as fields**, same mechanism again.

Two bugs worth recording, both caught in seconds rather than shipped:

**The guard allow-list was language-wide, not per-pattern.** In Elixir
`defmodule M` and `alias M` are the *same shape* of call — a macro call with an
alias argument — and only the keyword distinguishes them. With one list per
language, `defmodule M` was extracted as an import *of* M. The fix is to check the
keyword against what the match claims to be (`kind == "import"` or not), which is
the honest formulation anyway: the keyword is what makes it an import.

**I used the wrong capture convention for relations** and got silence rather than
an error — five queries that compiled fine and matched nothing. The existing
convention is `@sub` on the subtype and `@extends`/`@implements` on the
*supertype node*; I wrote `@subtype`/`@supertype` with the relation on the whole
match. Nothing failed, there were just no relations. A reminder that a query
compiling is not evidence it matches.

The up-front extractor validation from session 25 paid for itself again: a bad
field name in the Elixir pattern surfaced as `Invalid field name "arguments"`
instead of an index that silently came back empty.

Also of note: `capabilities_reports_per_language_support` failed, correctly. It
asserted `ruby.members == false` and `bash.imports == false`, which had just
stopped being true. A test that breaks when the tool gets better is the test
working.

Remaining gaps are genuine language facts, not omissions: Go (structural typing),
C and Zig have no inheritance; Bash, Lua and Elixir declare no fields. The matrix
reports those as absent and the empty-result note explains them.

Schema 15 -> 16. 199 tests (+4, 1 updated). differential 4185/4185, textmode ALL
PASS.

## 2026-07-22 - session 33: making codeq's own limits queryable

Three capabilities, and the first came out of a bug in the second-to-last commit.

`capabilities` listed the fact kind `field, variant` **twice** and omitted
`heading` — a hand-maintained list that drifted the moment two sessions edited
it. That is the same class of failure as everything else this week: a claim
maintained by hand next to the thing it claims to describe.

**`capabilities.language_support`** is therefore *derived*, not declared. For each
language it reads that language's own query text and reports which of
definitions / imports / members / relations / spans / calls exist. It cannot
drift, because the queries are the source of truth:

    language   defs imp mem rel spa cal
    go         y    y   y   .   y   y
    ruby       y    y   .   y   y   y
    bash       y    .   .   .   y   y
    markdown   y    y   .   .   y   .

This matters because support has always been uneven and flat lists implied it was
not. Go and C have no inheritance edges *at all*; Ruby declares no fields. An
agent asking `implementors` about a Go interface got "none", which is true and
completely misleading. Now:

    codeq extracts no inheritance edges for go; this is not evidence that 'Reader' has none

The empty-result work from session 27 answered "why is this empty" at the command
level; this answers it at the capability level, which is where the remaining
misreadings lived.

**`untested`** is `api` composed with `tests`. Measured on this repo before
building it: 99 of 129 public functions have no test naming them — and finding
that out took ~130 subprocess calls, one per symbol. Now one call. Public only,
since a private helper is usually covered through whatever exported thing calls
it, and it over-reports coverage (a test that reaches code without naming it is
invisible) rather than claiming coverage that is not there.

**`api --compare-to <ref>`** answers "does this break anyone", which a diff
cannot: a diff shows every edited line and most are internal. It extracts the
exported surface from the old blob via `git show` and compares with the current
index, reporting removed / changed / added. Diff-shaped, not history-shaped, so
it sits with `changed` and `affected` rather than opening the door to a history
tool.

One design correction while building it. The first version returned a flat list
with a `change` column, which text mode rendered *identically* for an added and a
removed row — because the renderer prints a signature instead of the other
fields, exactly as session 24's contract says it should. Grouping into
`# removed` / `# changed` / `# added` sections uses the existing convention and
makes the distinction visible. A reminder that output shape is part of the
feature, not a wrapper around it.

195 tests (+4). differential and textmode re-run: ALL PASS.

## 2026-07-22 - session 32: the loose ends, and reading a dependency

Five things, two of them overdue verification rather than features.

**The correctness harnesses had not been run.** Six commits touching the
renderer, the search engine, extraction and the schema, verified with `cargo
test` alone, while previous sessions record running the differential and
text-mode harnesses before committing. Run now: `textmode` ALL PASS,
`differential` **20 repos / 4185 assertions, 4185 PASS, 0 FAIL, 0 ERROR**
(search 600, fuzz 1200, invariant 2385). Nothing had broken. That is a relief
rather than a vindication — the gap was real and I would not have known.

**MCP tool descriptions had drifted out of date.** They still described `--on`
as `RECV.NAME / RECV::NAME` byte matching, still said `unused` counts comment
mentions as uses, and never mentioned markdown. MCP is the primary agent path,
so those strings *are* the contract; a stale one is worse than a missing feature.

**`callees` now reads the call map** (session 31's `callsite.rs`) instead of
scanning identifiers for a following `(`, closing the last textual gap in the
call graph.

**`deps --external`** lists imports that resolve to no file. For code that is the
third-party dependency list; for markdown, whose links are always local paths, an
unresolved one is a broken reference, flagged `broken=true` and sorted first.

Dogfooding earned its keep immediately: the first run reported **two dead links
in this repo's own README**, both in documentation written in session 28. They
were `` `[g](./guide.md)` `` — link syntax inside inline code, i.e. an *example*
of a link. Documentation about markdown is full of those. Inline code spans are
now excluded from link extraction. A feature finding a bug in the docs that
introduced the feature is a good argument for shipping the check.

**`--root DIR`** closes the dependency-source hole. A dependency tree is
gitignored, so it stays out of the repo's own index (correct — it is not this
repo's code), but that made "how do I call this library function" unanswerable.
`--root` takes a directory literally: no walking up to a Git toplevel, and
`parents(false)` on the walker so the *outer* repo's ignore rules do not erase
every file inside it. Then every command works there:

    codeq --root node_modules/lodash api
    codeq --root node_modules/lodash read chunk.js --symbol chunk

Also refreshed the published benchmark table, which predated the session's
changes and was stated as current. New: **5.5x tokens** over 20 repos (was 5.2x
over 25). The interesting movement is `unused`, 64x -> 160x, a direct consequence
of session 29: comments no longer mask dead code, so codeq reports more of it
while the generic-tool baseline stays where it was.

One test failed for an instructive reason: `.gitignore` is only honoured inside a
real Git repository, and the test fixture is a bare temp directory. Switched to
`.ignore`, which the walker honours anywhere and exercises the same filter path.

191 tests (+3).

## 2026-07-22 - session 31: `--calls` and `--on` read the parse tree

The structural version of the two filters sessions 29 and 30 patched textually.
New `src/callsite.rs`: parse the file, walk it, and build a map from the byte
offset of each called identifier to its receiver. `--calls` is then a lookup and
`--on` is a comparison against the receiver *expression*.

The design choice that made this cheap: **verify, don't index.** The obvious
implementation is a `call` fact kind emitted during indexing, but calls outnumber
definitions by an order of magnitude and every query would pay for an index that
only two opt-in filters read. Instead the textual pass narrows first (trigram
index plus word-boundary regex, fast and never misses), and only the surviving
candidate files are parsed. Measured on this repo: plain `references` 0.00s,
`references --calls` 0.03s. Ordinary queries pay nothing and the index does not
grow.

What this buys beyond fixing the old bugs: the receiver is now the expression as
written, so `--on a.inner` and even `--on 'conn()'` work, and `--on db` correctly
excludes `a.inner.query()`. Text could not express any of that.

Grammars disagree about node *kinds* but largely agree about **field names**, so
the walk reads fields (`function`, `object`, `receiver`, `target`, `name`,
`suffix`) rather than kind spellings. That got 14 of 17 languages working from
one implementation. The three that needed help, each found by dumping the real
tree:

- **Ruby** puts the method in `method` and the receiver on the *call node*, not
  on a member node, so a bare callee identifier can still be qualified. Also
  `db.query` has no argument list at all, so requiring one rejected every
  paren-less call.
- **Solidity** wraps the callee in a transparent `expression` node with no
  fields, so single-child wrappers need stepping through.
- **Swift** keeps its arguments in a `call_suffix` rather than an argument list,
  and names the method via `suffix: navigation_suffix > suffix: identifier`, one
  level deeper than everyone else. Its `call_suffix` kind also contains "call",
  so it had to be excluded from being treated as a call itself.

All 17 are covered by a table-driven test that asserts the same call resolves in
each, written against dumped trees rather than documentation.

One test I wrote and then deleted rather than keep: a "falls back to text when
there is no grammar" case. The fallback exists and is correct, but both filters
already imply code-only, which excludes files codeq cannot parse *before* the
fallback matters — so the test would have asserted a behaviour that cannot be
reached. Better to say so in the module doc than to keep a test that implies
otherwise.

PHP is worth knowing about: receivers are as the language writes them, so it is
`--on '$db'`. Documented rather than special-cased, since inventing a normalised
form would be a lie about the source.

188 tests (+2).

## 2026-07-22 - session 30: a declaration is not a call

The two textual heuristics left over from session 29, both in `references`.

**`--calls` matched the definition site.** `fn parse_range(` is followed by `(`
exactly as `parse_range(x)` is, so `is_call_position` — which peeks past
whitespace for a `(` — cannot tell a declaration from a call. Every
`references NAME --calls` therefore reported the declaration as a call site.

No cleverness needed: `references` already computes `defsites` and flags each row
`definition: true`, purely so the output can show it. The information was sitting
right there; the filter just never consulted it. Same for `--on`, since a
declaration is not a qualified use either. Plain `references` still reports the
definition and still flags it — only the position filters drop it.

**`--on RECV` was defeated by a line break.** `has_qualifier` compared the bytes
immediately before the match, so of these three identical calls:

    db.query();
    db
        .query();
    db . query();

only the first was found. Two false negatives caused purely by rustfmt. Skipping
whitespace on both sides of the separator fixes it, and the receiver test itself
stays exact: `other.query()`, `mydb.query()` (longer name ending in `db`) and
`db.inner.query()` are all still correctly excluded from `--on db`.

Honest about what this is: still textual, not a parsed receiver expression.
`a.b.query()` matches `--on b` because the nearest qualifier is what gets tested,
and a call-valued receiver like `conn().query()` matches nothing at all. Both are
now documented rather than discovered.

The pattern across sessions 29 and 30 is worth naming. Every one of these four
bugs was a byte-level test standing in for something codeq already knew: spans
for "is this a comment", the facts table for "is this a declaration". The
heuristic was not compensating for missing information, it was ignoring
information already in the index. Worth checking for that before reaching for a
parser — the structural answer may already be one query away.

186 tests (+3).

## 2026-07-22 - session 29: comments are not uses

Asked where codeq still matches characters for something the structure already
knows. Four places; two fixed here, both with span data the index already had.

**`unused` was masked by comment mentions.**

    fn never_called_anywhere() {}
    // TODO: wire up never_called_anywhere later

Not reported. Delete the comment and it is. So a stale TODO hid real dead code —
and stale TODOs are exactly the kind of thing that mentions a function nobody
calls any more. The docstring called this conservatism, but it was buying
nothing: codeq extracts comment/string spans and `--in code` already filters on
them. The conservatism was accidental, not chosen.

**`callees` reported mentions in strings and comments as calls.** A function
whose body contains only `"helper() in a string"` and `// helper() in a comment`
listed `helper` as a callee, despite `callees` being documented as call-position
only. `collect_idents` scanned the body's characters with no idea where the
literals were.

The interesting decision was **not** applying the same rule to both. A comment is
never a use, so excluding comments is free. A *string* often is: reflection,
`getattr`, serde renames, DI registration by name, SQL identifiers, config keys.
Excluding strings from `unused` would make it report live code as dead, inverting
the command's whole stance — and deleting live code is the expensive mistake,
where a missed dead function is cheap. So:

- `unused` excludes **comments only**; string mentions still count as uses.
- `callees` excludes **comments and strings**, because in the languages codeq
  parses, a call written inside a string literal is not a call.

That needed a new internal `InFilter::NotComment`, and a distinction that had not
existed before: a `--in` filter is a positive claim ("this match is in a
comment") and cannot be made without spans, so it skips unparsable files; an
*exclusion* is not, so it must keep them. Without that, a symbol named only in
`config.toml` would have been reported as dead the moment the filter was applied,
since TOML has no spans. There is a test for exactly that.

One existing test failed, correctly: `unused_is_conservative_about_comment_mentions`
asserted the old contract in its name. Rewritten as
`..._about_string_mentions`, which is the part of the conservatism actually worth
keeping.

Still textual, deliberately unfixed for now: `references --calls` matches the
definition site (`fn foo(` is followed by `(` too, though the row is already
flagged `definition: true`, so the fix is cheap), and `--on RECV` scans bytes
immediately before the match, so `db\n    .query()` is missed while `db.query()`
is found. Both want real call-expression structure; `astgrep` already exists and
could supply it as an opt-in precision mode.

183 tests (+4, 1 rewritten).

## 2026-07-22 - session 28: markdown is indexed like code

Docs were the first thing an agent reads in an unfamiliar repo and the one thing
codeq knew nothing about. Markdown was 9 files and 20% of this repo's bytes,
indexed for content search and nothing else: no symbols, no outline, no folding,
no `--in`, no enclosing-section annotation on hits. For a 291-line README, codeq
offered nothing over `cat`.

**A heading is a definition.** That single mapping is the whole feature. A
heading has a name, a byte range (down to the next heading of the same or higher
level) and a nesting depth, which is exactly the fact model the Tree-sitter
extractors produce, so `outline`, `at`, `read --symbol`, folded `read`, `search`
annotation, `symbols`, `context` and `changed --symbols` all started working on
prose with no new commands at all.

The `read --symbol "Output contract"` case is the real prize: 2,961 bytes instead
of 17,987 for the whole README, on exactly the files that are pure prose and
don't compress.

Deliberately **not** Tree-sitter. Headings are unambiguous line-wise provided
fenced blocks are tracked, so this avoids a grammar dependency for the one format
where the structure is trivial, and the `source` field (`lines/markdown` vs
`treesitter/rust`) already existed to record which was used.

Three things that needed care, all of which would have looked broken on first run:

- **Folding a document collapsed it to its title.** A level-1 heading owns the
  whole file, so the generic "fold the definition's body" rule hid all 290
  remaining lines behind one marker. Markdown folds the prose *between*
  consecutive headings instead, which leaves the whole heading tree visible: a
  table of contents where each entry carries its section's line range, ready to
  paste into `--lines`.
- **`unused` would have reported every section in every document.** Nothing ever
  references prose by name. Headings are excluded unless explicitly asked for
  with `--kind heading`, the same shape as the existing test-file exclusion.
- **A `#` inside a fenced block is a shell comment, not a heading.** Fence
  tracking handles it, as does frontmatter (`---` at the top is metadata, and
  must not read as a setext underline), and a `---` after a blank line is a
  thematic break rather than an underline.

The spans mapping is worth stating: a fenced block is `Str` and prose is
`Comment`, so `--in strings` searches the documented examples and `--in comments`
searches the surrounding text. `--in code` is correctly empty for markdown, which
has no code — consistent with the existing guidance that SQL-in-strings is found
with `--in strings`, not `--in code`.

Local links and images are file references (`import` facts), so `deps`,
`importers` and cycle detection now cover documentation and its assets. External
URLs and bare `#anchor` links name no file and carry no edge.

**A latent bug this exposed:** `outline README.md` matched `path = ?1 OR path LIKE
'%/README.md'`, so it merged the root README and `bench/README.md` into a single
interleaved outline with nonsensical nesting. `at` had the same bug. It had been
invisible because code files rarely share a basename, whereas every repo has
several READMEs. Both now resolve to one file the way `read` always did (exact
path wins, else a unique suffix).

Schema 14 -> 15. 179 tests (+7).

## 2026-07-22 - session 27: an empty result now says why it is empty

Not a new command. codeq already has 31, and the next-best thing was not a 32nd
but making the ones there fail informatively.

The problem, from this session's own transcript rather than theory. Seven
commands, five silent:

    api src/search.rs        -> (empty)
    tests declaration_header -> (empty)
    unused src/deps.rs       -> (empty)
    implementors Serialize   -> (empty)
    callers nonexistent_xyz  -> (empty)
    about ScanOpt            -> "# definitions" and nothing under it

Every one is ambiguous between four different things: the name doesn't exist,
the name exists but has none of that relation, the language has no such concept,
or a `--path` filter removed everything. I lost real time to `api src/search.rs`
before remembering `pub(crate)` is `restricted`, not `public`. And `about
ScanOpt` is the worst of the set, because a typo returns valid headers with
nothing under them, which reads like a fact about a real symbol.

Now each explains itself:

    no symbol named 'ScanOpt'; closest: ScanOpts
    'declaration_header' is defined at src/commands.rs:671 but has no tests referencing it
    'ScanOpts' is defined at src/search.rs:110 but nothing matches --path zzz
    no public definitions in 'src/search.rs' (34 private, 40 restricted)
    no unused definitions in 'src/deps.rs' (every one is referenced somewhere)

Three design points worth keeping:

- **The note is diagnostic, never data.** `data` stays empty, the exit status
  stays 0, and stdout stays pure in text mode (the note goes to stderr). A parser
  that ignored it before ignores it still.
- **It has to reach MCP.** `call_tool` shells out with `--json` and captures
  *stdout only*, so a stderr-only note would be invisible to exactly the consumer
  that needs it most. Hence a `note` field in the envelope as well.
- **Suggestions are deliberately tight.** The first cut allowed edit distance up
  to 3, which turned `Serialize` into "closest: normalize" — an unrelated word
  offered as a fix, which invites a second wrong query. Now the budget is
  `len/4`, capped at 2, and names sharing a prefix rank first, since a truncated
  or extended name is the commonest real typo. `Serialize` now correctly offers
  nothing; `renderrow` offers `render_row, render_rows`.

One test of mine was wrong rather than the code: I asserted `unused` on a file
with a `main` would be clean, forgetting that nothing calls the root, so `main`
is legitimately reported. Scoping the assertion to a file whose only definition
is called from elsewhere fixed it. Worth noting because the failure was the tool
being right.

172 tests (+5).

## 2026-07-22 - session 26: docs in text output (a flag that did nothing)

Asked "can an agent get a function's documentation", checked, and found the data
was already there and the default output threw it away. `doc` sat in the
renderer's `CONSUMED` list, so every doc comment codeq extracts was visible only
under `--json`.

The sharp end of that: **`outline --docs` was a silent no-op in text mode.** The
flag parsed, the query fetched docs, the JSON contained them, and the default
output was byte-identical with and without it. Same shape as the en-dash range
bug from session 24: a documented affordance that quietly doesn't work in the
mode everyone actually uses. Both were invisible because nothing asserted on text
output, which is why session 24's contract test now has company.

Docs are stored already collapsed to one line and capped at 240 chars, so they
render inline as `  // <doc>` on the definition's own row and the one-row-one-line
rule survives. `definitions`, `about`, `explore` and `members` fetch docs anyway,
so they now show them by default, which is the right default for symbol-focused
commands (a dossier that omits what the author wrote about the symbol is a poor
dossier). `explore`'s focus node gained a `doc` field; its callers and callees
deliberately stay bare so the neighbourhood listing doesn't turn into prose.

New rule in the output contract, generalising the bug: **a flag must change the
text output.** If it only shows up under `--json`, it is a no-op from the
default's point of view.

167 tests (+3), one of which asserts that `--docs` changes the text output and
does not add a line, and one that an undocumented field carries no empty marker.

## 2026-07-22 - session 25: `members` (a type's fields and variants)

The gap: `about ScanOpts` told you the struct existed and who used it, but not
what was in it. The extractor captured 16 kinds and **none of them were fields**,
so "what does this struct contain" had no answer short of `read --symbol`, which
is the whole-file-read habit codeq exists to replace. Session 24 made it more
visible by cutting container bodies out of signatures, which removed the
accidental way of half-seeing fields through a truncated body dump.

`codeq members NAME` returns fields, enum variants and methods in source order.
The property worth stating: it is the **one graph command with no name-matching
caveat**. Containment comes from the parse tree (a member is a definition nested
directly inside NAME's byte range), so a same-named type elsewhere cannot leak
in, and a local inside a method is not a member of the class. `about` now
includes it, omitting the section entirely when there are none, so a struct's
dossier is still one call.

Followed the dump-real-parse-trees rule again, and it earned its keep. Two things
no amount of reading docs would have given:

- **JS and TS disagree on the node name.** A class field is `field_definition`
  in the JavaScript grammar and `public_field_definition` in TypeScript's. The
  shared `JS_QUERY` compiles against both, and a node type absent from a grammar
  fails the *whole* query, so the JS capture had to move to a new `JS_EXTRA`,
  mirroring the existing `TS_EXTRA`.
- **Solidity's `enum_value` is the identifier**, a leaf, not a wrapper around
  one. `(enum_value (identifier) @name)` is an "impossible pattern"; the fix is
  two captures on one node, `(enum_value) @name @variant`.

The real find, though, was the failure mode those two bugs exposed. A query that
fails to compile makes `Extractor::new()` return Err, and each worker did
`Err(_) => return`. So a malformed query produced **an index with zero files, no
error, and exit 0**. `codeq stats` reported `files: 0` in a repo full of Rust.
That is indistinguishable from an empty directory, and it cost a while to spot
because the obvious suspects (a corrupt db, a bad walk, an ignore rule) all
present identically. Now the extractor is built once up front purely to validate
and the error propagates with context, so a malformed query says so:

    codeq: building the fact extractor (malformed query?): compiling query:
    Query error at 14:13. Impossible pattern: (enum_value (identifier) @name)

Worth generalising: an error swallowed in a worker is far more expensive than one
that crashes, because the empty result looks like a legitimate answer. There is
now a test that simply asserts indexing a normal repo yields facts.

Also noticed while adding the command: `capabilities` listed 22 commands and the
CLI had 31. `explore`, `api`, `unused`, `deps`, `path`, `implementors`,
`supertypes`, `affected` and `install` were all missing, some for many sessions.
A stale capabilities list is worse than none, since an agent reads it as the
contract. It is now cross-checked against `--help` in the test suite.

Honest limits, all documented rather than guessed around: in Rust a struct's
methods live in a separate `impl` block, so `members` on a Rust struct returns
fields only; Ruby's `attr_accessor` is a method call, not a declaration; Python
instance attributes (`self.x = ...` in `__init__`) are assignments, not
declarations, so only class-level attributes are covered. Verified across 16
languages for fields and 6 for variants.

Schema 13 -> 14. 164 tests (+13).

## 2026-07-22 - session 24: output-format audit + piped input (`--stdin`)

Two jobs. First, an audit of what codeq actually prints, on the theory that its
whole value is emitting only what an agent needs.

Found one outright bug: the `read` fold marker printed its range with an en dash,
`21 lines (44–64)`, but `--lines` only parses ASCII hyphens. So the marker
advertised a next command that codeq itself rejects. Copying it verbatim errors.
Fixed to `... 21 lines (44-64)`, and the assertion now pins the exact string.

The rest was accumulated noise, all of it per-row or per-call:

- Container signatures (`struct`/`enum`/`interface`) were stored as the whole
  declaration, so `symbols --signatures`, `definitions`, `outline`, `about` and
  `explore` each dumped the full field list plus doc comments and attributes,
  truncated mid-token at 180 chars. `api` already had `api_signature()` to cut
  this. It was applied in exactly one of six places. Generalised to
  `declaration_header(kind, sig)`, kind-aware so a constant's value (which *is*
  its content) survives, and applied everywhere.
- `map` printed a 25-token sentence explaining its own sort order on every call.
  Moved to `--help`, where it costs nothing per query.
- `changed` printed `indexed=true` on every row. Only `false` is information.
- `callers/callees --depth` and `deps` indented every row including depth 1, so
  the shallowest results carried indentation that meant nothing. The renderer now
  normalises depth against the minimum in the list, which fixes both without
  touching `outline` (already 0-based).
- Callers at file scope rendered as `src/commands.rs:14: ` , a locator, a
  separator, and then nothing.
- A multi-line `use a::{\n b,\n}` import spilled one row across four lines,
  quietly breaking the one-row-per-line contract that makes the output greppable.

21% fewer characters across a 7-command workload with nothing lost. The lesson is
that none of these were decisions anyone made; they accreted one command at a
time, which is exactly why the contract is now written down in the README and
enforced by `text_output_stays_plain_ascii_rows`.

Second job: `search --stdin`, so codeq covers the case where an agent would
otherwise fall back to grep, output that isn't in the repo at all (`git log |`,
`curl |`, a build log). Refactored the per-file match loop out of `run_search`
into `scan_doc`, so piped and indexed scans share one implementation and cannot
drift; only the source of spans and defs differs. With `--language` the buffer is
parsed once, which is what makes this more than a slower grep: `--in
code|comments|strings` and the enclosing `[symbol]` both work on a stream.

The design constraint that mattered: **`--stdin` is never inferred**. The obvious
implementation is "if stdin isn't a TTY, read it", and it would have been a
disaster. An agent's shell hands the process an open pipe, so every ordinary
`codeq search` would have silently become a scan of an empty buffer, and the MCP
server would have answered nothing for every query while looking perfectly
healthy. There is a test named for exactly this. A related discovery while
testing: an `IsTerminal` guard does *not* save you, because in that same agent
shell stdin is a pipe, not a terminal. `codeq search x --stdin` with nothing
piped in hangs, same as `grep x` would.

Also worth recording, because it cost real time twice in one session: a
`cargo build --release` that gets interrupted leaves a **0-byte
`target/release/codeq`**, and cargo then reports `Finished` without relinking. An
empty executable file runs as an empty script, so every command exits 0 printing
nothing. `stats` returning nothing looks exactly like a corrupt index. The same
artifact instability caused the MCP server to fail at session start with ENOENT,
because the binary was mid-rebuild when Claude Code spawned it. Pointing the MCP
config at a stable installed path rather than the cargo target dir avoids the
whole class.

Minor friction, not fixed: `codeq api src/search.rs` returns nothing because
every item there is `pub(crate)`, which is `restricted`, not `public`. That is
correct and documented, but an empty result is indistinguishable from a wrong
command, and there is no hint that N restricted definitions were filtered out.
Also `api` takes its scope as a positional while most commands take `--path`.

157 tests (+7).

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

## astgrep's own documented example matched nothing

Found by cross-checking, pre-existing, and returning a confident wrong answer with
exit 0 — the failure mode codeq exists to eliminate. `fn $N($$$) { $$$ }` —
the pattern written in `astgrep.rs`'s module doc — returned **0 matches against
369 functions**, and the same in Python, JavaScript and Go. Not an error: zero
results, exit 0, so an agent concludes the code doesn't exist.

The cause took bisecting a pattern one component at a time. `def $N($$$): return
$X` worked. `def $N(a, $$$): $$$` worked. `def $N($$$): $$$` did not. The rule
turned out to be: *two* list nodes each holding a single `$$$`. `variadic_name`
deliberately sees through single-child wrappers, because a `$$$` in statement
position parses as an `expression_statement` around the sentinel — but that also
makes a `parameters` node holding one `$$$` look like a variadic itself, when the
absorption really belongs one level down inside it. Params and body both hit that,
`match_children` saw two variadics, and its "more than one would need
backtracking" guard rejected the whole pattern. So the single most natural thing
anyone would write was the one shape that failed. Fixed by preferring a positional
match when the arities already line up and letting the recursion absorb inside
each child, falling back to absorbing at this level only when they don't (binds
are trialled on a clone so a failed attempt can't pollute them). `fn $N($$$) {
$$$ }` 0 -> 177 on codeq itself; `$X.unwrap()` unchanged at 198.

## A size- and mtime-preserving edit was invisible

Same shape as the astgrep bug above: a confident wrong answer, indefinitely. The
freshness fast path
trusted `(size, mtime)`, so an edit of identical length that restored mtime left
every later query answering from a stale index — the file said `bbbb`, codeq said
`aaaa`, indefinitely. mtime is settable from userspace (`touch -d`, `cp -p`,
`rsync -a`, tar extraction, restoring an older copy), so this is reachable without
anything exotic. Fixed by also comparing inode ctime, which moves on any write and
cannot be forged. Cost measured, not assumed: an unchanged tree still short-
circuits at 4-6 ms, and the one operation that moves ctime without changing
content (chmod) costs a single re-walk with **zero** reparses, because the content
hash still matches. Schema 15 -> 16; unix-only, falling back to size+mtime
elsewhere.

Also swept clean this round and left alone: incremental-vs-full-rebuild after
edit/add/delete/rename/move, `at` innermost-first ordering, `path` chain edges,
`callers --depth` monotonicity, `unused` false positives (zero), the
`imports`/`importers` inverse, `--resolved` on components (correctly tags
same-file/imported and prunes same-named functions in unreachable files), and
adversarial file shapes — no trailing newline, a 4000-statement single line,
CR-only endings.

## Byte offsets measured in one text, applied to another

Probing a corner nothing had touched — files that aren't valid UTF-8 — turned up a
wrong answer, and chasing it surfaced a second, likelier one with the same cause.

The indexer normalises a file before measuring anything: strip a leading BOM,
decode as UTF-8 with invalid bytes replaced. Fact offsets are positions in *that*
text. But two consumers were applying those offsets to the bytes on disk, which is
a different string whenever normalisation actually changed something.

- **Invalid UTF-8.** Each bad byte becomes a three-byte U+FFFD, so the searchable
  body runs two bytes ahead per bad byte. `search`, `references` and `callers`
  compare a match's body offset against a fact's raw range, so past the first bad
  byte the enclosing-symbol lookup drifts — first dropping the tag, then landing
  on the *next* definition. The output contradicted itself: `z.rs:2:13: fn alpha()
  { helper(); }  [bravo]`, and `callers helper` named `bravo` where `alpha` is the
  caller, while `at z.rs:2` correctly said `alpha`. Two commands disagreeing is
  what made it findable.
- **A leading BOM.** Same shape, three bytes, and far likelier to meet in the wild
  — any Windows-authored tree. `explore` on a BOM file returned source starting
  mid-expression: focus text `'﻿fn alpha() { helper()'` (truncated), callee
  `' }\nfn helper()'`. Garbled code handed straight to an agent.

Fixed by making the normalised text the single coordinate system: facts are now
extracted from it rather than the raw bytes, and source assembly normalises before
slicing instead of after. For valid UTF-8 without a BOM — virtually every file —
extraction is byte-identical and costs nothing, because that string is already
materialised for the FTS body. `read` was never affected: it is line-based, and
line numbers were right throughout.

Honest scoping note: I first claimed this was reachable in a real repo, having
found seven invalid-UTF-8 files in api-analytics. Checking them showed all seven
were `.git/objects` and a `.br` artifact — none indexed. The BOM half is the one
that actually reaches users; the UTF-8 half needs genuinely latin-1 source, which
older C/C++/PHP/Perl trees do have (no null bytes, so `looks_binary` passes them
through as source). Worth stating plainly rather than leaving the inflated claim.

## The MCP server turned caller values into flags

The MCP server had never been swept. Driving it over JSON-RPC and calling all 28
advertised tools found one failure and one real bug.

Caller-supplied values are passed positionally to the CLI, and the CLI's parser
reads a leading dash as a flag. An MCP client has no way to say "this is a
literal" — a shell user types `--` themselves, but the protocol offers no
equivalent. So `pattern: "--json"`, an entirely ordinary thing to search a
codebase for, came back as "required argument not provided", and `symbol:
"--help"` printed codeq's help page and exited 0 — `isError: false`, help text
handed to an agent as a successful answer. Fixed by accumulating flags first,
then `--`, then caller values. Also sharpened `codeq_deps`, whose schema said
`required: []` while calling it with no arguments is an error.

Two process notes, both about nearly-wrong conclusions:

- I first cross-checked the schemas against `build_args` with a regex over the
  match arms. It silently matched nothing and reported **103 properties never
  forwarded** — i.e. every parameter of every tool broken, which is not a
  plausible state for a tool in daily use. That implausibility was the tell, not
  anything about the regex. Replaced it with an empirical sweep: call each
  parameter, diff the output against the call without it. 20 exercised, 0 broken.
- The fix itself was a mechanical rewrite routing positionals into a separate
  vec, and it **broke `codeq_astgrep`** — `language` is a flag's *value*, not a
  positional, so `--language` lost its argument. The compiler was happy; only
  re-running the whole 28-tool sweep caught it. After a bulk edit, re-run the
  sweep that found the bug, not just the case you were fixing.

## Two follow-ups: one real bug, one thing I had mislabelled

**astgrep accepted patterns it had only partly parsed.** `if { }`, `match { }`
and `garbage $$$` each returned zero matches and exit 0 — indistinguishable from
"no such code exists". The existing `has_error()` guard could not catch them:
`significant()` descends *past* an ERROR node (a bare expression is not a valid
top-level item in several grammars and only survives via recovery), and whatever
fragment survived is itself error-free. So `if { }` quietly became "an empty
block" and was matched as such. Fixed by requiring the chosen node to span the
whole pattern — recovery is precisely the thing that discards input, so a partial
span is the signal. Verified the patterns that legitimately depend on recovery
still work, and confirmed against the HEAD binary on the same tree that match
counts are unchanged (an apparent 198 -> 216 drift in `$X.unwrap()` was the
repository growing from earlier commits in this session, not the guard).

**`--fields` dropping unknown names was not a bug.** I had listed it as one for
several rounds: `--fields path,nonsense` returns just `path`, exit 0. Two things
said otherwise. `via` appears only under `--resolved` and `col`/`text` only under
`--examples`, so `--fields path,via` is a *working* query whose field is
legitimately absent — erroring would break it. And `fields_ignores_unknown_columns`
is a pre-existing test asserting exactly this behaviour, so it is a deliberate
contract, not an oversight. What was genuinely missing is observability, so the
projection now reports which requested names matched no row and lists the ones
that were available (`line` -> "available: …, start_line, …", which is what the
caller almost certainly meant). A note on stderr in text mode, a `notes` block in
JSON; the projection itself is unchanged and the old test still passes.

Worth recording as a pattern: the reflex on finding surprising behaviour is to fix
it, and twice now the right move was to first ask whether it was intended. The
existing test was better evidence than my own reasoning.

## codeq could not be run concurrently at all

The header of db.rs says WAL is enabled "so that a reindex by one process does not
block reads by concurrent agent invocations". Twelve simultaneous invocations:

    cold starts              10/12 failed   "database is locked"
    warm, unchanged tree     11/12 failed   <- even pure readers
    warm, after one edit     11/12 failed

Concurrency is not an edge case here — it is the design target. Parallel tool
calls, or two agents on one checkout, and every invocation refreshes.

Three separate causes, each hiding the next:

1. No `busy_timeout`, so SQLite gave up the instant it met a lock.
2. `PRAGMA journal_mode = WAL` ran on every open and takes an exclusive lock even
   when the database is already in WAL. That is why *readers* failed: they
   contended before reading anything. Now the mode is read first and only written
   when it differs.
3. `BEGIN DEFERRED` (rusqlite's default). This was the one that mattered. A
   deferred transaction takes its write lock lazily, and SQLite returns
   SQLITE_BUSY **immediately, bypassing the busy handler**, because two readers
   both upgrading would deadlock. Adding the timeout changed nothing at all until
   the transaction became `BEGIN IMMEDIATE` — a good reminder that a fix which
   "should" work and doesn't means the diagnosis is incomplete, not that the fix
   needs to be bigger.

Then, because the lock is taken before the parse, twelve processes each held it
through their own copy of the same work. Re-checking freshness once the lock is
held lets the waiters skip it: measured properly (remove the check, rebuild,
re-run) that is 8.5s -> 0.9s for a one-file change across twelve processes.
Warm and unchanged is 32ms for twelve.

Two of my own measurements were wrong before they were right, both worth naming.
The first "9.3 seconds for a one-file change" was an artefact of the test writing
its output files *into the indexed repository*, so the tree really was changing
under every process; the true figure was ~0.9s. And I nearly kept the
double-checked refresh on the strength of it sounding obviously good — the
before/after comparison is the only reason I know it earns its place.

## Member-level facts: fields, properties, enum variants

The gap that prompted this: `definitions retries` returned **nothing** for a
struct field, and `callers retries` conflated a Rust field with an unrelated
Python attribute of the same name while listing the struct declaration itself as
a "caller". For an agent editing a type — a very ordinary task — "what fields does
this have" and "who reads this field" were answerable only by noisy text search,
with no definition for `--resolved` to resolve against.

Members are now `field` and `variant` facts, each carrying an `owner`: the type
that declares it, found by walking to the nearest *type* declaration rather than
the nearest named node. The separate column was the design decision worth
arguing. Qualifying the name as `Config.retries` would have been less code — no
schema change, no column threaded through three queries — but it breaks exactly
the question being asked: `definitions retries` would return nothing, and you
would have to already know the type in order to ask which type it was. So:
`name` stays the member name, `owner` says whose it is. Cost: a 17-column insert,
which outgrew rusqlite's tuple `Params` impl and needed `params!`.

Best side effect: `outline` now shows a type's shape, members nested under it.

**A bug the feature exposed.** `api` on an exported TypeScript class reported
`private secret` as public API. The JS/TS visibility rule only consulted
accessibility modifiers for `method_definition`, so a class field fell through to
"does the file export this?" — and since the class was exported, every field read
as public. Members now consult their own modifier; verified that TS `private`,
Java `private` and non-`pub` Rust fields are all excluded from `api`.

**Two things checked rather than assumed.** Thousands of new field facts could
have buried `unused` — it still reports exactly 3 on codeq itself, because fields
are read somewhere. And text mode needed its own change: the renderer returns
early on any row with a `signature`, so `owner` was invisible in the *default*
output, which is where it matters most (`pub retries: u32` does not say whose it
is). Now bracketed like `search`'s enclosing symbol.

Honest scope: 5 of 23 languages (Rust, Go, Java, TypeScript/TSX, C#). Python gets
nothing, and Python is plausibly the most common language here — its attributes
are assignments inside `__init__`, not declarations, so they need a different
extraction shape plus a judgement on class-level assignments and `@property`. C/C++
was attempted and backed out when the query anchor did not match cleanly; better
done deliberately than by a regex that half-worked. Five languages that are exactly
right beats twenty that are approximately right, for a tool whose whole claim is
that its output can be trusted — but `definitions retries` still returns nothing in
a Python codebase, which is the same failure that started this. Schema 16 -> 17.

## Members for the remaining languages, and what Python forced

Extending member facts from 5 languages to 11 (adding Python, C, C++, PHP, Kotlin,
Swift) was mechanical for five of them and a design problem for Python, which has
no declaration site at all: an attribute exists because something assigned it.

Three rules came out of that, and two are general improvements rather than Python
special-casing:

- **A member with no owner is dropped.** An owner-less `field` is precisely the
  bare, unaddressable name this fact kind exists to replace. It also disposes of
  `self.x = 1` inside a plain function and of anonymous C structs, for free.
- **Members are deduplicated per (name, owner), earliest wins.** Python assigns
  `self.retries` in `__init__` and again in `reset`; that is one member, not two
  declarations. Safe generally, because no language here permits two members of
  one name in one type.
- **`other.x = 1` is not a declaration** — guarded on the `self` receiver, reusing
  the mechanism built for Ruby's `require` and Elixir's `def`.

The owner rule immediately caught a mistake of my own: I had put `@field` on the
`class_definition` node, so `owner_of` walked up from the class looking for an
*enclosing* type, found none, and silently dropped every class-body attribute —
`DEFAULT = 3` and `timeout: float = 1.5` simply were not there. The capture belongs
on the assignment, so that the walk starts inside the class. Worth noting that the
new invariant is what surfaced it: before, those would have been emitted as fields
owned by nothing and looked approximately right.

Python's rule is a judgement, not a fact, so it is stated in `capabilities`, the
README and CLAUDE.md rather than left implicit: a member is its earliest class-body
or `self.x =` assignment.

## A malformed query silently emptied the whole index

Adding four grammars, one of my span queries named a node type that does not exist
(`verbatim_string_literal`, PowerShell). The result was not an error. It was this:

    $ codeq stats
    files: 0
    index: {schema: 18, fresh: true}

codeq's own repository, 78 files, indexed as *nothing* — and reporting itself
fresh. Every language, not just the broken one.

`Extractor::new()` compiles every language's queries, and the parse workers did
`Err(_) => return`. So each worker exited quietly, sent no results, the writer
stored nothing, and the refresh reported success. A programming error in one
grammar's query took out all 27 languages and looked exactly like an empty repo.

This is the worst failure shape in the tool: not slow, not partial, but confidently
wrong at full scale. Query compilation fails identically for every worker and every
file, so there is no partial-success story to preserve — it now prints which query
failed and exits 1.

Two process notes. I found it only because a fixture returned nothing and I checked
codeq's *own* repo rather than assuming the fixture was odd — the fixture looked
like the more likely culprit. And once the error was visible, I validated every node
type in all five new grammars programmatically against their `node-types.json`
rather than fixing errors one rebuild at a time; that found the single remaining
problem in one pass.

## Four more languages: CSS, PowerShell, Protobuf, GraphQL (27 total)

Chosen for coverage rather than count. CSS earns its place twice: `<style>` inside a
Svelte or Vue component was opaque, so CSS comments read as *code* to `search --in`
— a limitation I had to document when adding those languages, now closed. PowerShell
is the Bash counterpart that was conspicuously missing. Protobuf and GraphQL are the
contracts between services, so in a polyglot repo a message name is often the one
symbol that crosses language boundaries.

Two things needed work beyond writing queries. Protobuf spells names per construct
(`message_name`, `enum_name`, `service_name`) rather than using a `name` field, so
`owner_of` found no name for the enclosing type and the new "members must have an
owner" rule silently dropped every protobuf field — the rule working exactly as
designed, on a grammar it had not met. Broadened to accept a `_name` suffix. And the
new grammars' type nodes had to join `TYPE_KINDS` before their members resolved at
all.

**HCL/Terraform was cut after it shipped wrong.** A block's identity lives in its
labels, and which label is the name depends on how many there are: `variable
"region"` is named by its only label, `resource "aws_s3_bucket" "logs"` by its
*last*. Tree-sitter queries cannot express "last", so my first attempt produced
`kindword "aws_s3_bucket"` and `kindword "logs"` — the capture name leaking in as the
fact kind, quotes included, two facts per block. I had flagged exactly this risk
before starting and went ahead anyway. It needs a name-derivation rule in code, like
Svelte's component fact, so it is deferred rather than approximated: `kindword` facts
in a tool whose pitch is trustworthy output are worse than no Terraform support.

## HCL/Terraform, done the way the language actually names things

Cut from the previous batch after it shipped `kindword "aws_s3_bucket"` facts.
The reason it could not be a query: a block is `keyword "label"… { … }`, and the
labels are a *repeated, unnamed* child, so tree-sitter cannot select "the last
one" — and which label is the name depends on how many there are.

    terraform { }                       0 labels -> no identity, skipped
    variable "region"                   1 label  -> region
    resource "aws_s3_bucket" "logs"     2 labels -> logs, of type aws_s3_bucket

The model that fits is the one already built for struct members. Terraform refers
to things as `aws_s3_bucket.logs.arn`, `var.region`, `local.x` — so the name is the
last label and the type label becomes the `owner`, exactly the field/type split.
`definitions logs` then answers "the aws_s3_bucket one", and `references logs`
finds `aws_s3_bucket.logs.arn` because codeq's graph is name-based. `kind` is the
block keyword verbatim, which costs an open-ended kind vocabulary but keeps this
honest for non-Terraform HCL (Nomad's `job`, Packer's `source`) instead of
hardcoding one tool's nouns. `locals` contributes one constant per attribute, and a
`module`'s `source` is captured as an import.

Two honest limits, both stated in `capabilities` rather than left to be discovered:
the kind vocabulary is open-ended, and `deps` does *not* resolve a module source —
resolution matches import segments against file stems, and a Terraform module is a
directory. That is a general limitation (JS `./utils` -> `utils/index.js` does not
resolve either), not something specific to HCL, so `imports` and `importers` work
while `deps` stays empty.

Also worth recording: my first version of the test asserted a hard-coded line
number and failed on an off-by-one in my own fixture, not in the code. Rewritten to
assert on content — that the use site containing `aws_s3_bucket.logs.arn` is
attributed to the enclosing `output` and is not flagged as the declaration — which
is what the naming rule is actually for.

## Five capability gaps, found by asking the tool instead of guessing

Rather than brainstorm what might be missing, I probed: query the thing, look at
what comes back. Four of five suspicions were real, and one was already working.

**`deps` missed every directory-style import.** Not a Terraform quirk — general and
severe. Resolution matched import segments against file *stems*, so `./utils` never
reached `utils/index.js`, `from pkg import x` never reached `pkg/__init__.py`, and a
Terraform `source` never reached its module directory. That is the dominant idiom in
the two largest ecosystems here, silently absent from the dependency graph the whole
time. A file that stands in for its directory — a conventional entry file, or any
`.tf` — is now also filed under the directory's name.

**Data I had added but left unqueryable.** `owner` went onto members across 13
languages with no way to filter by it: `symbols --kind field --owner Config` was an
argument error. So "what does this type hold?" meant fetching every field in the
repository and filtering by hand. My own omission. It turns out to generalise nicely
to HCL — `--owner aws_s3_bucket` lists every bucket, `--owner local` every local.

**GraphQL `implements` was not a relation.** `type User implements Node` is real
inheritance and I shipped GraphQL with an empty relation query. The fix then failed
halfway for a reason worth recording: `implements_interfaces` nests *left-recursively*
rather than listing siblings, so `A & B & C` parses as `II(II(II(A), B), C)` and one
pattern binds only the outermost. One pattern per nesting level, bounded at five
interfaces — a stated limit rather than a silent truncation.

**`--calls` only knew one syntax.** It tested for `Name(`, which a component is never
spelled with, so `callers Btn --calls` returned nothing for a component plainly used
as `<Btn />`. Markup instantiation now counts; a bare mention, an import and
`const x = Btn` still do not, and call-syntax languages are unchanged.

**`unused` reported contracts as dead code.** Every GraphQL type, every protobuf
message, every Terraform `output` — all things that exist to be consumed from
*outside* the repository, so "nothing here references it" is their normal state.
They drowned the real hits. Now skipped, with the reasoning in `capabilities`.

The one that was already right: CSS. `.orphan` is reported and `.primary` is not,
because `page.html` uses it — genuine dead-stylesheet detection that fell out of
adding CSS spans, and worth checking rather than assuming it was noise too.

## Members for Zig, Solidity, Dart and Scala (18 languages)

Mechanical for three of them. Zig was not, and the reason generalises: some
languages *bind* a type to a name rather than declaring it with one. `const Point
= struct { x: i32 }` leaves the struct itself anonymous — `Point` lives on the
enclosing declaration — so the owner lookup found an unnamed type and the
"members must have an owner" rule dropped every Zig field silently. The lookup now
follows that binding before giving up. Same shape as the mistake I made in Python,
where the capture sat on the class instead of the assignment: the owner rule keeps
catching cases where a member's type is not where you would first look for it.

Zig reports enum members as `field` rather than `variant`, because the grammar
models both as `container_field` and the language genuinely does not distinguish
them. Reporting what the language says beats inventing a distinction it lacks.

Also worth recording: the loud-failure fix committed an hour earlier paid for
itself immediately. A wrong Dart pattern — `declaration` does not contain
`initialized_variable_definition` — would previously have emptied the entire index
while reporting `fresh: true`. Instead it printed `Impossible pattern:` with the
exact query, and the fix took one lookup.

## clones: structural duplication as a fact, not a score

The original suggestion was a *similarity score* between two code sections. I
argued against it and built the version that fits: hash a definition's syntax
shape — the tree of node kinds, with identifiers and literals normalised away —
and group by exact hash. Shapes match or they do not. No threshold to justify, no
number an agent cannot act on, and `diff` still covers "what exactly differs".

It found real duplication in codeq's own source on the first run, biggest first:

    cmd_implementors / cmd_supertypes      202 nodes
    defs_by_names    / defs_by_paths       148 nodes
    opt              / optn                 58 nodes
    print_range      / print_range_raw      49 nodes

Checked by eye rather than trusting the hash: `cmd_implementors` and
`cmd_supertypes` really are the same function with `subtype`/`supertype` swapped.

Two decisions worth stating. The shape walk uses an explicit stack, because
recursion depth here is attacker-controlled by source nesting. And a definition
under twelve nodes gets no shape at all: every empty function in a repository
shares a shape, truthfully and uselessly, so the floor belongs at extraction
rather than in the query. `--min-nodes` (default 24) then filters how much code a
group must cover — it filters, it never ranks.

Honest cost, measured rather than assumed: a cold index of 78 files goes from
~900ms to ~1100ms, about 20%, because every definition's subtree is walked once.
Warm queries are unchanged at 5ms. That seemed worth it for a capability that
answers "is this already implemented?" — but it is a real cost and belongs in the
docs, not hidden.

## Using clones on codeq itself — and what it means to *not* act on one

`clones` reported 13 groups in this repository. Three were worth fixing:

- `defs_by_names` / `defs_by_paths` (148 nodes) — the same loader written twice for
  two indexed columns. Merged into `defs_by_column`.
- `cmd_implementors` / `cmd_supertypes` (202 nodes) — letter for letter the same
  function with `subtype` and `supertype` swapped. The query is now shared; 202 -> 117.
- `codeq_commit` in two bench harnesses — moved to `corpus.py`, which both already
  imported.

The other ten are the interesting part, because the right response to a true report
is often "yes, and that is fine":

- **`pick` in bench.py and tasks.py** is structurally identical and *behaviourally
  different* — one rebuilds the index and picks a `clean_name`, the other warms it
  and picks a `clean_class`. Merging would have been a bug. This is the clearest
  argument for reporting structure as a fact rather than emitting a "duplication
  score" that implies an action.
- **CLI enum variants** (`Implementors`/`Supertypes`, `Files`/`Api`, …) are
  declarative clap definitions; identical shape is what a uniform CLI *is*.
- **`ImplementorRow` / `SupertypeRow`** differ only in a field name, and that field
  name is the output contract.
- **`opt` / `optn`** and **`print_range` / `print_range_raw`** would each collapse
  into one function plus a flag, which reads worse at every call site than two
  names that say what they do.
- The residual `cmd_implementors` / `cmd_supertypes` group is now just the mapping
  boilerplate that exists *because* the two commands present the same rows under
  different names. Collapsing it needs a generic over the row type — more code than
  it removes. Stopping here rather than forcing the number down is the point: a
  tool that makes you game its own metric is worse than one you can ignore.

Also settled: `changed --since-index` is dropped rather than built. Every query
auto-refreshes, so there is no retained baseline to diff against; making it work
needs a stored snapshot, and codeq stays stateless.

---

## Round: turning codeq on codeq for refactoring work

The brief was "use codeq to find refactoring work, and report anything you trip
over while doing it". Both halves paid out, and the bugs were worth more than the
refactoring.

### The cross-check that mattered: `tests` was answering "none" for covered code

`codeq unused src` reported exactly three definitions — all three of them test
functions in `src/install.rs`. That is a 100% false-positive rate on the command
whose entire premise is that it under-reports rather than misleads.

Pulling the thread with two commands that must agree:

```
codeq tests apply --path src/install.rs      -> empty
codeq references apply --path src/install.rs -> 7 call sites in 3 test functions
```

`tests` says nothing exercises `apply`. `references` says three test functions call
it seven times. Both cannot be right.

The cause: `is_test_path` knew test *directories and filenames*, and `is_test_name`
knew test-like *function names*. Rust's dominant unit-test idiom — `#[cfg(test)]
mod tests` inside the very file under test — has neither. The path is ordinary and
the function names are prose (`installs_into_a_detected_agent_and_is_idempotent`).

This one stings for two reasons. First, the limitation was *already written down*
in the doc comment on `tests` ("can't see attribute-only tests… may be missed"), so
it was a known gap that nobody had priced. Documenting a hole is not the same as
knowing how big it is: the honest measurement is that on codeq's own repository the
command was blind to every unit test in the crate. Second, an empty result from
`tests` doesn't read as "I can't see those" — the skill documentation explicitly
tells an agent that empty means "only exercised indirectly". So the failure mode
was a confident wrong answer on the exact edit-then-find-the-test loop the command
exists to serve.

The fix needed no extraction change, because codeq already indexes the enclosing
`mod tests` as a fact with a line span. A definition inside a test-like container
is now a test. It generalises for free: a Python `class TestFoo` or a Java
`class FooTest` in an ordinary path classifies its methods the same way.

While fixing it, a fourth copy of the notion turned up: `callers_of` carried its own
inline predicate (`path.contains("test")`, name starts with `test`/`Test`) which is
both weaker *and* wronger than the other two — `path.contains("test")` matches
`src/latest.rs`. Four notions of "is this a test", differently wrong, in one file.
There is now one, `is_test_site`, and `tests`, `callers` and `unused` agree.

### `capabilities` had gone nine commands stale

The command whose only job is to tell an agent what codeq can do listed 23 of 32
commands. Missing: `affected`, `api`, `deps`, `explore`, `implementors`, `install`,
`path`, `supertypes`, `unused` — including `explore`, the tool an agent should
reach for first.

It was a hand-maintained array. It is now derived from clap's own subcommand list,
so it cannot drift, and a test cross-checks it against the parser's help output.
The bug was found the same way as the last one: asking two sources that must agree
(`capabilities` vs `--help`) and diffing them.

### The refactoring findings themselves

Less dramatic than the bugs, and mostly one root cause:

- **`commands.rs` is 108 KB / 3,117 lines / 71 functions / 30 command handlers.**
  The obvious split is by domain, but splitting it is cosmetic without the next item.
- **There is no query layer.** `codeq search 'SELECT ' --in strings` counts 43 raw
  SQL statements in `commands.rs` against 2 in `db.rs`. `db.rs` is schema and
  migration only; every handler hand-writes its own SQL. This is *why* the same
  definition-loading shape kept recurring — the clone groups merged last round were
  symptoms, not the disease.
- **The command surface is specified three times** — the clap enum, `build_args` in
  the MCP server, and the `capabilities` list. This has now cost real bugs twice:
  `codeq_astgrep` broke in the MCP copy, and the `capabilities` copy rotted.
  Deriving `capabilities` from clap fixes one third of it; `build_args` remains.
- **Long functions:** `refresh` 317, `main` 278, `build_args` 238, `harvest` 224,
  `Extractor::new` 207.

### A clone group I got wrong last round

`run()` in `bench.py` and `tasks.py` was reported by `clones` and I left it, having
lumped it in with the `pick` pair. It was a genuine duplicate — identical but for a
local variable name (`t0` vs `t`). Lifted to `corpus.timed_run`, which also absorbed
`completion.py`'s near-copy (same body, returns milliseconds) and the `CODEQ` path
constant that five harnesses each computed for themselves in two different spellings.

Worth recording as a process point: `clones` reported this correctly and I dismissed
it. The tool's job is to hand you exact structural equality; deciding is still the
reader's, and the reader can be wrong in *both* directions.

### Feature gap: `callees` cannot show its own evidence

`codeq about codeq_commit` reports four `run` functions as callees. The body calls
no `run` — it calls `subprocess.run(...)`, and the name-based matcher attributes the
qualified call to every definition named `run` in the repo. `--resolved` correctly
returns empty, but you only reach for it if you already suspect the answer.

`callers` has `--examples` to surface the call expression, and `--on` to filter by
receiver. `callees` has neither. Showing `subprocess.run` in the row would make the
over-match self-evident without the reader needing to suspect anything, and
module-qualified calls are the norm in Python and JS, so this is a broad noise class
rather than a corner case. Not built yet; recorded here as the next obvious gap.

### codeq under-reported React by a lot, and `capabilities` hid seven languages

Pointed codeq at a small but realistic React/TSX tree to see how it fares on what is
probably the most common thing anyone will point it at. Three findings, in descending
order of how badly they mislead.

**1. Wrapped components were invisible.** `outline` on a file with four components
returned two type aliases and an enum. The definition query matches a `const` bound
to an arrow or a function expression — but the dominant React idiom binds it to a
*call*:

```
export const Card = memo(function Card(…) {…})
export const Input = forwardRef((props, ref) => …)
export const Wrapper = styled.div`…`
export const Ctx = React.createContext(null)
```

None of those is function-valued, so none was a definition. The file looked fully
indexed — no error, no empty-result note — while most of its surface was missing.
This is the failure mode codeq exists to avoid: not a wrong answer, a confidently
incomplete one. The same hole swallowed `export const VERSION = '1.0'`, so `api` was
under-reporting exported constants in *every* JS/TS repo, not just React ones.

Fixed by treating a module-scope binding as a definition. The scope anchor matters:
capturing every `variable_declarator` would have buried the index in loop counters,
so the pattern is anchored to `(program …)` and a test asserts a function-local
`const` does not leak.

**2. `class X extends React.Component` had no supertypes.** The TS relation query
matched `extends_clause value: (identifier)` only; a namespaced base is a
`member_expression`, so React class components produced no inheritance edges at all.
Fixed, plus the same gap for `implements ns.I` and `implements I<T>`. The supertype
is stored dotted, as written — and since nobody types `implementors React.Component`,
the lookup now matches the last dotted segment too.

**3. `capabilities` listed 22 languages; codeq extracts 29.** svelte, vue, css, hcl,
powershell, proto and graphql had working extraction and were absent from the matrix.
Cause: `facts::queries_for` is a second hand-maintained registry that only `support()`
reads, and the languages whose facts are built in code rather than from a query
(svelte, vue, hcl, markdown) fall straight through it to `None`.

That one is the worst of the three in kind, because CLAUDE.md tells agents to consult
`capabilities` before believing an empty result. A stale matrix does not merely fail
to help — it converts "codeq found nothing here" into "codeq does not support this",
which is precisely the wrong conclusion.

The fix worth keeping is not the seven added strings. `language_for` became a table
(`index::EXTENSIONS`) so it can be enumerated, and two unit tests now pin it against
`facts::LANGUAGES` in both directions: every extension's language must be documented
or explicitly listed as data-only, and nothing may be advertised that no file can be.
Adding an extension without deciding which it is now fails the build. This is the
third capabilities-drift bug in this repo (`codeq_astgrep` in the MCP copy, then the
command list, now languages) and the first one closed by making the drift impossible
rather than by correcting the copy.

**A postscript worth more than the fix.** The binding change passed a hand-written
React fixture cleanly, so I ran it against the cached corpus before believing it.
Express went from 148 definitions to 681 — because CommonJS writes every dependency
as a module-scope binding, and `var http = require('http')` was now being counted as
a definition *and* an import. Ten of the first twelve rows of `outline lib/utils.js`
were the file's own import block. 424 phantom definitions in one small repo, and they
would have propagated into `unused` and `hotspots` as declarations.

The fixture could not have caught it: I wrote the fixture from what I expected React
to look like, so it contained exactly the shapes I had already thought of. The corpus
contains the shapes I had not. Same lesson as the `clones` miss above, pointing the
other way — there, the tool was right and the reader dismissed it; here, the tests
were green and the code was wrong. Neither green tests nor a plausible diff is
evidence about real code; only real code is.

Excluding `require`-rooted values also confirmed the change pays for itself on the
repos where it changes nothing structural: zod gained `NEVER`, `INVALID`, `BRAND` and
`EMPTY_PATH` — documented, exported API that no symbol command could previously find.

---

## 2026-08-12 — dogfooding on an outside repo (api-analytics): a real bug, and where `clones` over-reports

First run against a repo codeq did not grow up on: a Go/Svelte/Rust/PHP/Python
monorepo (579 files, 3,222 symbols). The brief was "explore it with codeq instead
of grep/rg/find, fix something real, and log the friction." One bug fixed, two
notes worth keeping — one a friction, one a doc-precision nit — and a couple of
plain wins.

### The win chain: `unused` → a dead sibling → a live bug

`unused dashboard/src` returned eight exported-but-unreferenced helpers.
`references`/`search -w` confirmed each appears exactly once in the whole repo
(distinctive names, indexed Svelte templates included), so they are genuinely
dead — not a name-collision artefact. Low-value on their own. But reading the
*neighbourhood* of one of them is what paid out: `dateInPrevPeriod` (dead) sits
beside `dateInPeriod` (live) and both call `periodToDays`, whose `'6 months'`
case returned `30 * 7` — 210 days, i.e. seven months. `'month'` returns `30`, so
the intended value was plainly `30 * 6`.

`callers periodToDays --examples` then gave the entire blast radius in one call —
`aggregate.ts` (bucket sizing), `plotly.ts` (axis), `utils.ts` (a `count / (24 *
days)` per-hour rate), `periodFilter.ts`, and `dateInPeriod` (the filter window) —
each row carrying the actual call line. That single command is what turned "typo
in a helper" into "every 6-month view is inflated ~17%," and it replaced the
rg-plus-open-five-files dance I'd otherwise have done. Fixed to `30 * 6`;
`svelte-check` clean, 0/0. The dead-helper report was the map; the bug was the
treasure next to the X.

### Friction: `clones` on a multi-package monorepo is dominated by *intentional* mirrors

`clones --min-nodes 40` put its biggest groups on copies that are supposed to be
copies: the PHP `core` package mirrored verbatim into the `laravel` and `php`
publishable packages (`Client`/`Config`/`RequestData`/`Mappers`, 3 copies each),
and the Python `python/python` monolith re-shipping each framework adapter it also
distributes standalone (`tornado`, `fastapi`, `starlette` …). Thirty-plus of the
top rows were expected vendoring. codeq is *correct* — they are exact structural
clones — and the existing philosophy ("report structure as a fact, and the right
response is often 'yes, and that is fine'") holds. But on a repo whose release
model is "ship N copies of one package on purpose," the deliberate copies bury the
accidental ones, and there's no path-aware way to say so. The `pick`-in-two-bench-
files case from the codeq round was one judgement call; here it's the *shape* of
the whole result. Worth considering: group clone rows by directory-pair (so a
reviewer dismisses `php/core ↔ php/laravel` once, not twelve times), or a
`--exclude-pair a:b` / `.codeqignore`-style pairing so the residual is the
*unexpected* duplication. This is the first repo I've dogfooded where the answer
was right and still hard to read.

### Doc-precision nit: the reindex notice, and content-hash freshness

CLAUDE.md says the refresh count "is only shown under `--json`/`--meta`." In
plain-text mode a `# codeq: reindexed N file(s)` notice does print on a real
reparse — I chased where it went and it is on **stderr** (present with stdout
discarded, absent with stderr discarded), so stdout stays pure `path:line:col:`
rows. Good behaviour; the wording just implies text mode is silent, and for a
beat I thought a banner was contaminating stdout. One line in the doc that a
stderr notice appears would close it.

Verified in passing, and worth stating because it's the *right* behaviour: a bare
`touch` triggered **no** reparse (no notice, unchanged output) — freshness is keyed
on content hash, not mtime, so re-saving a file with no edits costs nothing. Only a
genuine content change reindexed (`1 file(s)`). That's exactly what you want from
an every-query-auto-refreshes design.

### Smaller note: `unused` can't know app-vs-package

The eight dead helpers are all `visibility=public`. In an *application* that means
dead; in a *publishable package* it would mean API. codeq surfaces the right signal
(`visibility=public`) and can't be expected to know which subtree ships to a
registry — but on a monorepo that mixes both (this one does: `dashboard/` app vs
`analytics/*` packages), that's a judgement the reader has to add. Not a flaw,
just the seam where the name-based report hands back to a human.

Net: codeq carried the whole session — `map`/`tree` to orient, `unused` to find the
lead, `read --lines`/`outline` to compare siblings without ingesting files, and
`callers --examples` to size the bug — with no shell-out to grep/find. The one bug
it surfaced was worth more than the eight dead functions it was actually asked
about, which is becoming the recurring shape of these rounds.

---

## 2026-08-12 — same repo, round two: a cross-file data-flow bug, and two report-noise findings

Asked to keep going. This round the win was the exact thing codeq's graph is *for*:
a bug that lives in the seam between two functions in two files, invisible to either
one read alone.

### The win: `callers` → `read --lines` walked me across a three-file data-flow bug

`callers aggregate --examples` had exactly one call site —
`dashboardWorker.ts:83`, where `current` comes from `getPeriodData(...)` in
`periodFilter.ts`. Reading just the two relevant ranges (`read --lines`, never
opening either whole file) put both halves on screen at once, and the bug is only
visible with both:

- `getPeriodData` takes `now₁ = Date.now()`, then loops over *every* cached row
  filtering `current` to `dateMs >= now₁ - periodMs`.
- `aggregate` — called after that loop — takes `now₂ = Date.now()` (`now₂ > now₁`)
  and buckets against `startMs = now₂ - periodMs`, with the index clamped on the
  top (`Math.min(…, 4)`) but **not** the bottom.

A row in the `[now₁ - periodMs, now₂ - periodMs)` window survives the filter and
indexes the buckets at `-1`: `requestBuckets[-1]++` silently corrupts, and
`userSets[-1].add()` throws `TypeError` and kills the aggregation worker. The window
is as wide as the `getPeriodData` loop is slow, so it grows with dataset size — not
theoretical. One-line fix (`Math.max(…, 0)`); `svelte-check` clean. The point for
codeq: neither function is wrong by itself, so `unused`/`clones`/a single `read`
would never surface it — the caller edge plus two cheap ranged reads is what made a
two-`Date.now()` race legible. Committed `852eb08a`.

### Friction: `deps --cycles` reports phantom cycles from stem collisions with stdlib/registry names

`deps --cycles` led with
`server/email/http.go → …/monitor/http.go → server/email/http.go`. It's a phantom:
`server/email/http.go` imports stdlib **`net/http`**, and the resolver matched the
stem `http` to the only other repo file with that stem. `deps server/email/http.go`
even prints the edge as `net/http` — the disqualifying module string is right there,
the cycle detector just doesn't consult it. The rest of the cycle list was the same
failure on `express`/`fastify`/`hono`/`koa` (npm names that are also the example/test
filenames) and Django's `admin`/`apps`/`models`/`urls`. CLAUDE.md does say deps
resolution is heuristic and stem-based — but `--cycles` escalates that hedge into a
confident, scary-looking claim a reader may act on, and *every* cycle it reported
here was false. Suggest: don't resolve an import segment to a local file when the
segment is a known stdlib/registry module (`net/http`, a bare npm name), or at least
drop such edges from `--cycles` and tag them low-confidence elsewhere. A real
intra-repo cycle is worth a lot; burying it under ten phantoms costs that value.

### Nit: `unused` reports constructors as dead code

`unused analytics/javascript` and `unused analytics/nodejs` both led with
`method constructor visibility=public`. Constructors are invoked as `new Analytics()`
and never named `constructor` textually, so the name-based reference scan finds zero
uses — truthfully and uselessly, the same shape the SQL-index carve-out already
handles ("named exactly once by construction, so reporting it is noise"). A
constructor is dead only if the class is, which `unused` reports separately.
Excluding constructor-kind methods (the way `unused` already excludes test
scaffolding) would clear the top of the report of guaranteed false positives.

---

## 2026-08-12 — `astgrep --path` matched nothing, and FTS5 was why

The tinyad entry below files a P1 I could reproduce in one command: `astgrep
--path` returns empty for *every* scope form — exact file, directory, bare
substring — with exit 0. On this repo, 608 hits unscoped and 0 with `--path src`.

The cause is a SQLite property worth knowing about. `content_fts` is an FTS5
table with `tokenize = 'trigram'`, and the trigram tokenizer **takes over `LIKE`
on that table**. `path` is declared `UNINDEXED`, so it has no trigram data, so
`WHERE path LIKE '%src%'` matches nothing — not "fewer rows", *no* rows. Verified
against the DB directly: `SELECT count(*) FROM content_fts WHERE path LIKE
'%facts%'` is 0 while the same table holds 15 rust rows.

`search` was never affected, and the contrast is the lesson: it `LIKE`s `body`,
which *is* the indexed column, and filters paths in Rust. `astgrep` was the only
place applying `LIKE` to `path` on the FTS table — every other `path LIKE` in the
codebase is against `facts` or `files`, ordinary tables where it behaves. The fix
is to filter in Rust, as `search` already did.

The reporter's severity argument is right and worth preserving: it fails
*silently*, which is the exact failure mode the skill doc warns about for
astgrep, and the documented defence does not help — `--explain` showed a perfect
parse tree while the scoped query stayed empty. Worse, the docs steer you into
it ("scope to files with `--path`"). They followed that advice, concluded astgrep
could not express their query, and stopped using the command. A broken filter
that returns *fewer* results gets noticed; one that returns none reads as an
answer.

So `--path` matching no indexed file of that language now says so, rather than
returning an empty success.

### The other three, and what I did with them

**`clones` missed the duplication astgrep found.** Consistent with the documented
"exact equality" limit, and the reporter says so — but they are right that this
is the case a user most wants `clones` for. I did not add `--fuzzy`: normalising
identifiers is the similarity heuristic declined twice already this week, and it
would find their two blocks by also inventing pairs everywhere. Took the other
half of their suggestion instead — `clones`'s docs now point at `astgrep` for
exactly this shape, with their example.

**Cross-repo paths said "file not found"** for a file that plainly exists. It
described the lookup that failed rather than the state that caused it; an
absolute path into another checkout now says it is not in *this* index and names
the root. A path that really does not exist keeps the plain message.

**`members` interleaves kinds without a visible break.** Real, and documented
rather than changed: the output *is* grouped by kind, so line numbers restart
partway down, and projecting with `--fields name,line` hides the column that
explains it. Reordering would cost the grouping, which is useful; saying so costs
nothing.

### And the one latent thing round three surfaced without filing

That entry reports no defects — two wins and a caveat its author labels
not-a-flaw. Worth recording plainly: a dogfooding log that only ever grows defect
lists stops being evidence about the tool. But it did show something. The author
ran `definitions getIPAddress` *and* `definitions GetIPAddress` as two calls,
because the concept is spelled per language. `definitions` is exact and
case-sensitive by design; `symbols` is substring *and* case-insensitive —
verified, `symbols CMD_SEA` finds `cmd_search` — so one call would have done.
Nothing was broken; the cheaper route simply was not written down. Now it is.

Not adding `-i` to `definitions`: it is documented as "jump to one symbol: where
an EXACT name is defined", `symbols` owns the fuzzy question, and the near-miss
hint already catches a wrong case. Two commands with two jobs beats one command
with a flag that blurs both.

## 2026-08-12 — round three: cross-language consistency, where a single `search` beats a whole grep tour

Turned codeq on the analytics client family — the same middleware written once per
framework across Go, JS/TS, PHP, Ruby, C#, Rust (thirty-plus packages). This is the
shape codeq is *most* differentiated on: a fact-level question that spans many
languages at once.

### The win: one `search` mapped an inconsistency across seven language trees

The question was "which clients honor the `CF-Connecting-IP` / forwarded-header
chain when resolving the client IP?" `search "cf-connecting-ip" -i --group` answered
it for the entire repo in one call — and because every hit carries its enclosing
`[symbol]`, I could see at a glance that the match was in `GetIPAddress` for chi but
in a *test* or a *doc comment* for others, without opening a file. Cross-checked
with `definitions getIPAddress` + `definitions GetIPAddress`, which enumerated all
twelve IP-mapper definitions across Go/JS/TS/C# in two calls. The finding: chi
parses the full `CF → X-Forwarded-For → X-Real-IP` chain, echo/gin defer to
`RealIP()`/`ClientIP()`, and **fiber calls `c.IP()`** — the raw socket IP, no
forwarded-header handling at all, so every fiber user behind a proxy logs the
proxy's address. Doing this with grep means knowing where seven client families
live and reading each mapper by hand; codeq made it one query plus two lookups.

### The second win: `search Shutdown analytics/go` proved a negative

`Shutdown()` exists on the Go core `Client` and drains the buffer on graceful exit —
but do the framework adapters ever expose it? `search "Shutdown" analytics/go` showed
it defined once and called *only* in the core's own tests, never in any of the four
adapters. That single query established a real gap — the adapters create the client
inside a closure and never return it, so no user of fiber/gin/echo/chi analytics can
flush on shutdown (up to 60s of buffered requests lost), and it's why those adapters'
tests have been red since the channel refactor (`ea25b021`): the test can't force a
flush it has no handle for. This is the query grep answers with noise — the string
`Shutdown` appears in comments and docs — whereas codeq tagged every hit with its
enclosing symbol, so "all callers are `Test*` in core" was readable directly.

### Honest caveat: codeq scoped the question, but not the judgement

codeq told me *four* adapters diverge on IP handling. It could not tell me that
fixing echo/gin would be wrong — that `RealIP()`/`ClientIP()` carry trusted-proxy
semantics whose manual-parse replacement would reintroduce X-Forwarded-For spoofing.
That is framework knowledge, not a fact in any index. So the tool correctly bounded
the divergence (a name/reference question it owns) and correctly handed back the
"which of these should actually change" decision (a semantics question it doesn't).
I fixed only fiber, to match chi. Same seam as the app-vs-package note two rounds
back, pointing the other way: there the public-but-unused helpers were all dead;
here the divergence was all real, but only a quarter of it should be "fixed". The
recurring lesson is that codeq's name-based reports are a precise *map*, and reading
a map still needs someone who knows the territory.

## 2026-08-12 — tinyad (Python): `astgrep --path` silently matches nothing, and it cost me the tool's best answer

Used codeq while committing two changes to tinyad (a Python sensor-anomaly library,
~150 files). One P1 bug, one win that the bug had hidden from me, and two nits.

### P1: `astgrep --path` returns empty for every scope form

Reproducible on tinyad at `codeq 0.0.1`. The same pattern, with and without `--path`:

```
$ codeq astgrep 'len($X)' --language python --limit 0 | grep -c 'src/tinyad/stream.py'
4
$ codeq astgrep 'len($X)' --language python --path src/tinyad/stream.py
                                                          # (empty)
$ codeq astgrep 'len($X)' --language python --path examples
                                                          # (empty)
$ codeq astgrep 'len($X)' --language python --path stream
                                                          # (empty)
```

The unscoped run finds exactly 4 hits in `stream.py` — matching `codeq search 'len\('
src/tinyad/stream.py -c` → `count=4` — and its top hits are all under `examples/`.
Every `--path` form (exact file, directory, bare substring) suppresses all of them.
So the filter isn't narrowing the result, it's emptying it.

Severity is higher than "a flag is broken", for two reasons. First it fails **silently**
with exit 0 — which is precisely the failure mode DOGFOOD and the skill doc already warn
about ("a wrong kind and no matches look identical"), so the documented defence
(`astgrep --explain` on a line you know matches) doesn't help: `--explain` showed me a
perfect parse tree while the scoped query kept returning nothing. Second, the docs
actively steer you into it — the skill text says "Scope to files with `--path <substr>`",
and the gotchas section tells you to reach for `--path` whenever a name might be
ambiguous. I followed that advice on every astgrep call and concluded, wrongly, that
astgrep couldn't express my query. I stopped using it and went back to reading files.

Worth checking whether `--path` on `astgrep` shares an implementation with `search`'s
positional paths, which work fine — `codeq search 'len\(' src/tinyad/stream.py` scopes
correctly. Suggestion regardless: when `--path` matches zero indexed files, say so on
stderr (`nothing matches --path Z` already exists for other commands) rather than
returning an empty success.

### The win the bug hid: one structural query found duplicated logic that no text search could

The actual task was a debounce change, and the question was "how many places implement
consecutive-window confirmation?" Text search can't answer this in one shot, because the
two copies spell their receiver differently — a local in one, an attribute in the other:

```
$ codeq search '_confirm_windows > 1' src/
src/tinyad/stream.py:1028:35: if result.emitted and self._confirm_windows > 1:  [update]
$ codeq search 'confirm > 1' src/
src/tinyad/stream.py:269:30: if result.emitted and confirm > 1:  [update]
```

Two queries, each finding one, and no way to know you needed the second. The structural
query gets both at once — but *only* without `--path`:

```
$ codeq astgrep '$A.emitted and $B > 1' --language python
src/tinyad/stream.py:269:11:  result.emitted and confirm > 1                [update]
src/tinyad/stream.py:1028:11: result.emitted and self._confirm_windows > 1  [update]
```

That is the single most valuable thing codeq did for me all session: it proved tinyad has
**two parallel confirm-window implementations** — `TinyADStream.update` and
`TinyADSensor.update` — which had silently diverged, since a new margin gate was added to
one and not the other. This is exactly astgrep's differentiated case (shape is stable,
naming is not), and `--path` is the flag most likely to be reached for on a known file.

### `clones` did not catch that duplication

`codeq clones src/tinyad/stream.py` → empty, though the two blocks above are ~14 lines
each with identical control-flow shape (same if/else, same `min(counter + 1, N)`, same
latch assignments), differing only in whether the bound is a local or `self._attr`. This
is consistent with the documented "exact equality, not two implementations that differ in
their accessor expressions" caveat, so it's not a bug — but this is the case a user most
wants `clones` for, and astgrep found it. Worth considering a `clones --fuzzy` that
normalises identifiers, or a doc pointer from `clones`'s empty result toward astgrep.

### Nit: `members` interleaves kinds without a visible break

`codeq members TinyADSensor --fields name,line` emits methods line-ordered (759→1677),
then fields line-ordered (787→811). Grouping by kind is sensible, but with the default
field set there's no `kind` column and no separator, so the output reads as one list
whose line numbers inexplicably jump backwards at row 41. A blank line, or including
`kind` whenever output is grouped by it, would remove the double-take. (The grouping is
genuinely useful otherwise — `about TinyADSensor` surfaced `self._alert_margin` and
`self._confirm_windows` without my opening `__init__` at all.)

### Nit: cross-repo paths report "file not found"

Running `codeq outline /home/tomdraper/dev/codeq/DOGFOOD.md` from inside the tinyad
checkout gives `codeq: file not found: ...` and exit 1, for a file that plainly exists.
The real cause is that it's outside the current index. "file not found" sends you
checking the path; "not in the current index (run from /home/tomdraper/dev/codeq)" sends
you to the fix. Same class as the malformed-query note further up — the message describes
the lookup that failed rather than the state that caused it.

---

## 2026-08-12 — round four: the index is the source tree, not the linked build

This round turned the round-three shutdown finding into a real fix across the Go
client family, and in doing so hit the sharpest codeq limitation I've found on this
repo — not a bug, a boundary worth stating.

### The win, continued: `search` found the gap, `read` scoped the fix

The lead was still `search "Shutdown" analytics/go` from last round (drain hook
defined once, never called by an adapter). Turning it into a fix meant exposing the
client from four adapters and making `core.Shutdown()` actually block until the
final upload — and codeq's per-symbol `read`/`about` across the five modules was how
I kept the refactor consistent without opening whole files. Standard, and it worked.

### The limitation: codeq indexes the working tree; the Go build links a *pinned module version*

After wiring the adapters to call `client.Shutdown()`, the tests **deadlocked**. codeq
had told me — correctly — that the local `core.go` at HEAD has a fixed, non-deadlocking
`Shutdown` (`read core.go --symbol Shutdown` showed it plainly). But the deadlock stack
pointed at `core@v0.0.0-20251222101928/core.go:139` — the adapters don't build against
the sibling `../core` I was reading; they pin a **published** module version resolved
from the gitignored module cache, and that published version predates the deadlock fix.

codeq could not have shown this, and shouldn't pretend to: the resolved dependency is a
cached artifact, not this repo's tracked code, so it is correctly outside the index. But
it means a symbol query answers *"what does the source in front of you say"* — not
*"what will this module actually link"* — and on a multi-module repo where one module
depends on a **published** sibling instead of the local one, those two questions have
different answers. Every other round, source-truth and build-truth coincided and the
distinction never surfaced; here `read`/`about` on `core` were describing code the
adapters don't run. The runtime stack (module-cache path in the frame) was the only
thing that revealed it. `--root <cache-dir> api` is the manual bridge for reading a
dependency, but nothing connects an *import* to the specific cached version it resolves
to, or flags "this sibling module is shadowed by a pinned release." Worth a thought:
a `deps`/`imports` annotation that marks an intra-repo import which resolves to a
*published* version rather than the local module would have turned a 40-minute deadlock
hunt into one line.

### Smaller win: a config convention audit in one grep

`replace` directives are how this repo's `server/` modules use local siblings; the
analytics adapters lacked them, which is *why* they linked the stale published core.
One repo-wide search over `go.mod` files established both the convention and its
absence — the kind of cross-cutting "is this consistent everywhere" question that is
tedious by hand and a single query here. codeq remains at its best when the question
is a fact spanning many files; it is only the dependency-resolution boundary above
where the map stops matching the territory.

## 2026-09-25 — path filters treated SQL wildcards as literal filenames

Using `codeq explore` on `Filters` showed that the shared path filter interpolated
the user's substring directly into a SQL `LIKE` pattern. An underscore in a
filename filter therefore matched any character (`files a_b` also returned
`acb`), and percent broadened it to any sequence. This was especially surprising
because the parameter is documented as a path substring, not a pattern.

Escaped `\\`, `%`, and `_` in the bound pattern and added the SQL `ESCAPE` clause.
The CLI regression covers underscore and percent filenames. The focused test
passes; the same helper scopes `files`, `tree`, `symbols`, `definitions`, `api`,
`graph`, and related list commands.

## 2026-09-25 — `callpath x x` hid missing or out-of-scope symbols

`codeq explore cmd_callpath` showed that the zero-hop shortcut returned an empty
result immediately when `from == to`, even if the name was undefined or excluded
by `--path`. That bypassed `callpath`'s existing empty-result explanation for
all other queries. It now reports a missing-definition or path-scope note while
preserving the one-row, depth-zero result for a defined symbol. Regression cases
cover both missing and out-of-scope names.
