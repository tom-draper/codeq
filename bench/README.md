# Benchmark: codeq vs grep/rg/find

Harnesses, increasing in scope:

| script | scope | measures |
| --- | --- | --- |
| `bench.py` | single commands, one repo | speed, bytes, result count |
| `tasks.py` | multi-step agent tasks, one repo | total tool calls, bytes, time |
| `differential.py` | **many repos, correctness-first** | ~5,000 correctness assertions (superset-of-rg + per-symbol invariants) + per-command speed/bytes, over time |
| `completion.py` | **many repos, token-to-completion** | tokens + tool calls an agent burns to finish realistic tasks, codeq vs chained rg/sed/cat |
| `textmode.py` | **default (text) output faithfulness** | text output carries the same facts as `--json` (locators, counts, `-C` context, no raw-JSON dumps) |
| `real_workflows.py` | **complete bodies on pinned real repositories** | command invocations, input/stdout/stderr bytes, complete-body checks, optional binary comparison |

See [real workflow results and methodology](REAL_WORKFLOWS.md) for 19 fixed tasks
across Flask, Requests, Express, Gin, and ripgrep, including exact-range controls
and counted follow-up reads when source budgets are too small.

## Refresh query latency (`refresh_perf.py`)

Compare two release binaries with seven alternating runs per side and one discarded
warmup. The script verifies complete definition rows and the number of refreshed
files. It exits on command failures or incorrect results.

```sh
python3 bench/refresh_perf.py /tmp/codeq-before target/release/codeq --repo /path/to/codeq
```

The fixtures contain one or 128 Python files. `--repo` also measures an edit to
`bench/bench.py` in two copies of the repository's tracked files. Timings include
process startup, refresh, database writes, and definition output. Cold means a
missing index with warm filesystem caches. The repository scenario measures an
existing index after a single-file edit, then an unchanged query.

[The lazy-language results](results/refresh_lazy_languages_20261011.json) compare
main at `e87a446` with parsers and queries initialized on first use. On the
210-file source snapshot, the edited query fell from a median of 1094.83 ms to
27.34 ms, about 40 times faster. The seven-run ranges were 1066.23 to 1112.46 ms
before and 26.22 to 29.78 ms after. Unchanged queries stayed near 3 to 4 ms.

A separate baseline profile attributed 94.65% of 131 CPU samples to Tree-sitter
query compilation functions. Each worker previously compiled all 29 parser
variants. The new extractor compiles languages as it encounters them and reuses
their parsers. The improvement depends on the languages needed and the amount of
source to parse. This is not a general estimate for large repository rebuilds.

## Warm query latency (`query_latency.py`)

Compare two release binaries on complete repository queries with seven alternating
runs per side. The harness checks identical stdout and stderr for every invocation
and exits on failed commands or changed answers. It records every sample, medians,
ranges, binary hashes, corpus revisions, and machine load.

```sh
python3 bench/query_latency.py --before /tmp/codeq-before --after target/release/codeq
```

The corpus must already be cached. `--corpus PATH` selects another copy. Run this
without concurrent builds or corpus verification, since those compete for CPU and
may refresh the same indexes. Timings include process startup, repository discovery,
freshness checks, graph construction, queries, and output capture. Both sides use
warm indexes and filesystem caches. Cold indexing, concurrent queries, and model
reasoning are outside this measurement.

[The exact-candidate results](results/query_latency_20261012.json) compare release
binaries before and after indexing import candidates by their final component.
Rust Book's position-based `explore` query fell from a median of 8340.42 ms to
67.58 ms, a 123.4-fold speedup across seven alternating runs per binary. The ranges
were 7681.00 to 12011.12 ms before and 50.34 to 90.03 ms after. Redis's `explore`
query showed a similar reduction; Flask's smaller graph improved less. The three
control queries had overlapping ranges and showed no clear speedup.

[A separate baseline CPU profile](results/query_latency_profile_20261012.txt)
shows path splitting, character iteration, and `exact_module_match` dominating
CPU work. The old graph builder tested every import against every file, allocating
and normalizing paths repeatedly. The new candidate index narrows the files by
filename stem or entry-directory name before applying the same matching and
ranking rules. It preserves Python import prefixes, Jinja ancestor lookup,
relative paths, and ambiguity. The profiling binary uses the original release
configuration with symbols retained; profiling time is excluded from the samples.

[The cached-call results](results/call_query_latency_20261013.json) compare
baseline `43f820d` with indexed call-site reads and shared-tree extraction.
Across seven alternating runs per binary, Redis's `callers dictFind --calls`
fell from a median of 1305.86 ms to 49.23 ms. Flask's `references get --calls`
fell from 148.38 ms to 20.78 ms. The repository's `references prepare --on conn`
fell from 353.49 ms to 17.52 ms. All nine query scenarios returned identical
stdout and stderr on every timed invocation. The call-filter scenarios also
check that their answers contain rows.

[The refresh results](results/call_refresh_latency_20261013.json) check cold,
edited, and unchanged queries on small Python fixtures and a tracked repository
snapshot. Their timings remain close, so these samples do not establish an
indexing speedup. Normal indexing now extracts calls from the existing tree,
including component script trees. Rust call sites retain the unrepaired tree,
and SQL identifier repair still parses the original source for calls. The
indexed facts and schema version remain unchanged.

## Callee example latency (`callee_examples.py`)

Compare two release binaries on complete warm `callees --examples` queries.
The script alternates seven runs per side and checks exact stdout and stderr.
Synthetic JSON checks also verify every callee name, expression, and first call
line. Fixtures include a UTF-8 BOM, Unicode comments, and CRLF line endings.

```sh
python3 bench/callee_examples.py --before /tmp/codeq-before --after target/release/codeq --before-revision <commit>
```

The cases cover repeated calls, distinct callees, an early sparse call, a small
file, multiple definitions in one file, and `scan_called_names` in this
repository. Dense fixtures also run without examples as controls. Timings
include process startup, freshness checking,
callee resolution, and rendering. They exclude cold indexing and compilation.
The JSON report records raw samples, binary hashes, source hashes, fixture sizes,
and the byte counts for the old prefix scans and the new incremental line index.

[The callee example results](results/callee_examples_20261014.json) compare
baseline `da40fcb` with line offsets cached by file and extended only for retained
examples. The 5,000-call fixture fell from a median of 1371.56 ms to 5.79 ms,
with seven-run ranges of 1295.96 to 1480.01 ms before and 4.90 to 10.49 ms after.
Its prefix scans totalled 2.94 GB before and 548,908 bytes after. The same query
without examples stayed near 5 ms. The 1,000-distinct-callee fixture fell from
192.17 ms to 6.78 ms. Every timed invocation returned identical stdout and stderr.

[The separate baseline profile](results/callee_examples_profile_20261014.txt)
placed 97.97% of CPU samples in the newline-count loop. The remaining work
includes CLI startup, freshness checks, SQLite call reads, and callee resolution.
Sparse and small-file sample ranges overlap, and the repository case does not
establish a general query speedup. Another differential run was active in a
separate checkout during measurement. Both binaries use the same release
configuration, and alternating samples share that background load.

## Context source reads (`context_reads.py`)

Compare two release binaries on complete warm context queries. Seven samples per
side alternate execution order and must return identical stdout and stderr.
JSON checks verify seed counts, text presence, omitted neighbors, and oversized
seed reduction. Optional Linux traces count source opens and bytes read in
separate untimed invocations, whose output must also match.

```sh
python3 bench/context_reads.py --before /tmp/codeq-before --after target/release/codeq --before-revision <commit> --trace
```

[The context results](results/context_reads_20261014.json) compare baseline
`e3a23d9` with source loaded only for expansion, returned text, or seed reduction.
The 128-seed metadata case fell from a median of 47.80 ms to 3.59 ms. Seven-run
ranges were 46.89 to 52.61 ms before and 3.44 to 4.64 ms after. The separate
traces recorded 128 source opens and 67,112,448 bytes before, and zero after.
Freshness checks, definition lookups, and rendering remain in the timed queries.

Expansion with 64 oversized neighbors retained the seed and the same omission
report, while source opens fell from 65 to one. The text case fell from 17.48 ms
to 4.17 ms. Metadata for an oversized class still opens its source to preserve
the folded token estimate. Small-seed, folded-seed, ordinary text, and real
repository sample ranges overlap, so those cases establish no speedup. These
measurements use warm filesystem caches and do not estimate cold-disk latency.

## Historical API blobs (`api_git_blobs.py`)

Compare two release binaries on complete warm `api --compare-to HEAD` queries.
Seven samples per side alternate execution order and require identical stdout
and stderr. JSON assertions check the changed export count and both signatures.
Optional Linux traces count merge-base, show, and cat-file launches separately
from timing. Single-file, scoped-file, and empty-scope cases act as controls.

```sh
python3 bench/api_git_blobs.py --before /tmp/codeq-before --after target/release/codeq --before-revision <commit> --trace
```

[The API results](results/api_git_blobs_20261015.json) compare baseline `f8e15db`
with historical blobs streamed through one Git process. Across 128 changed Rust
files, median latency fell from 316.76 ms to 39.22 ms. Seven-run ranges were
306.86 to 324.09 ms before and 37.82 to 41.59 ms after. The separate traces show
129 merge-base calls and 128 show calls before, versus one merge-base call and
one cat-file process after. Removing repeated Git launches explains the gain;
Git diff discovery, historical extraction, freshness checks, and rendering still
run inside every timed query.

Single-file, scoped-file, and empty-scope ranges overlap, so those cases establish
no speedup. Empty scopes start no blob reader. These are sequential synthetic
queries with warm indexes and filesystem caches, and do not estimate cold-index
or cold-disk latency. Load averages were about 2.0 to 2.2 on 12 CPUs.

## Text-mode faithfulness (`textmode.py`)

`differential.py`/`completion.py` only exercise `--json`, so the *text renderer* —
the surface an agent actually reads by default — went untested, and a run of bugs
slipped through it: `-C` context lines silently dropped, `map`'s counts vanishing,
`capabilities` dumping raw JSON. `textmode.py` runs each command in **both** modes
and asserts the text output represents the same facts as the JSON: every result
row's `path:line` locator appears, metric counts appear, `-C` context lines
appear, and no command emits raw JSON in text mode. It's a rendering-layer check,
so one repo suffices — it defaults to the codeq repo itself (fast, no corpus).

```bash
python3 bench/textmode.py            # -> "ALL PASS" or lists FAILs, non-zero exit
```

This closes the structural gap that let the text-rendering bugs through: the other
harnesses guard correctness-at-scale, this one guards the default output surface.

## Token-to-completion harness (`completion.py`)

The metric that matters most: an agent pays for every byte it reads into context
(~4 B/tok). For each realistic task, `completion.py` runs the tool sequence a
competent agent would actually use to reach the *same answer* — with codeq, and
with generic tools (rg to locate, `sed` to read a window, `cat` to learn a file)
— and sums the tokens ingested + tool calls, across the corpus. Baselines are
deliberately fair (a ~60-line read window, not the whole file), and tasks where
codeq is near-parity (references, imports) are reported honestly.

```bash
python3 bench/completion.py            # writes results/completion_<ts>.json
```

Baseline (25 repos, compact-text default output): completing the 12 tasks costs
an agent **~5.4x the tokens and ~5.1x the tool calls** with generic tools vs codeq.
Spread is wide and the harness reports the losses too: `dead-code` 114.8x,
`file-structure` 9.8x, `read-impl` 6.5x, `callsites` 6.2x, down to `usages` 1.0x,
`file-imports` 0.9x and `public-api` 0.3x.

Switching the default output from JSON to grep-style text (`--json` still opt-in)
**doubled** the total saving — 3.1x → 6.2x — and flipped the two tasks codeq used
to *lose* on (`references` 0.7x→1.1x, `imports` 0.5x→1.0x), because the JSON
envelope was pure token tax where the answer is essentially `path:line: text`.

## Agent search/read workflows (`agent_workflows.py`)

```sh
python3 bench/agent_workflows.py --codeq target/debug/codeq
```

A deterministic Python fixture checks complete answers before comparing stdout
bytes and subprocess calls. The baseline merges overlapping windows and batches
all sed ranges into one invocation. No corpus download is needed.

| Task | codeq bytes / calls | rg + sed bytes / calls |
| --- | --- | --- |
| Already-known range, exact bytes | 70 / 1 | 70 / 1 |
| Complete body from a search hit | 70 / 1 | 4050 / 1 |
| Search and read all matching implementations | 487 / 1 | 12052 / 2 |
| Structural search and read matching implementations | 487 / 1 | 12052 / 2 |
| Batch several search hits, source once | 249 / 1 | 12165 / 1 |

These are mechanism checks on short definitions surrounded by unrelated source,
not general savings estimates. Sed reads windows of up to 60 lines when the end
of the definition is unknown. If boundaries are already known, raw codeq and sed
are byte-identical. Bytes/4 is an approximate token estimate; model reasoning,
tool-call syntax, stderr and latency are not included. Use the corpus completion
harness above for broader task comparisons.

## Differential harness (multi-repo, correctness + trend)

`differential.py` is the regression net. It clones a fixed corpus of real
projects (`corpus.py`, pinned tags — one per language codeq supports:
flask/gin/express/ripgrep/zod/sinatra/gson/jq/nlohmann-json/Newtonsoft/guzzle)
and runs two batteries against each:

**SEARCH** — the same query through `codeq search` and an equivalent `rg`,
checking the property codeq actually promises:

> codeq's matches must be a **superset** of ripgrep's — never fewer.

Any line rg finds that codeq misses is a **FAIL**; a codeq crash or bad flag is an
**ERROR** (kept distinct from a miss).

One exemption, and it is stated as a rule rather than a repo name. `rg -w` anchors on
`\b`, where `$` is a non-word character, so `rg -w locale` matches inside
`this.$locale()`; codeq treats `$` as identifier syntax where the lexer does, so
`$locale` is a different name. A `-w` line is exempt only when **every** occurrence on
it is `$`-adjacent, and only in the extensions that bind `$` — one free-standing hit and
the line still has to be answered. Exempt lines are counted into `sigil_exempt` in the
summary and printed on every run, so the exemption can be watched rather than trusted.
Today it is 22 lines, all of them dayjs's `$locale`. This is the check that would have caught
the `^`/`$` anchor regression and the trigram-`MATCH` substring drops — both of
which passed the unit suite.

**FUZZ** — 60 real identifiers auto-sampled from each repo's symbol table,
superset-checked against `rg -F`. Auto-scales the assertion count with the corpus
and is where a substring/prefilter regression lights up.

**INVARIANT** — ~6 checks per sampled symbol verifying `definitions`,
`references`, `at`, and `read` against ground truth (not against a grep baseline):
the returned def actually sits at that line; `at` round-trips and its body-line
resolves to an enclosing def; `references` includes the def site and is a superset
of `rg -nw`; `read --symbol` returns that symbol's source, bounded by the file.

**NAV** — the realistic navigation an agent does (definitions, references,
who-calls, read one symbol, outline, imports), each against the grep/`cat`
pipeline it would otherwise chain. These aren't a clean single-tool superset (the
grep pipeline is a lossy approximation, not ground truth), so they're scored on
speed + output bytes, not PASS/FAIL. `read --symbol` vs `cat file` is the headline
token comparison.

Each run force-rebuilds every index with the current binary, writes a timestamped
JSON to `bench/results/` (committed — history lives in git), and prints a
**per-command report** (correctness pass-rate + median codeq/alt time and byte
ratios) so it's obvious where codeq wins and where to improve. `trend.py` shows
movement across runs.

```bash
cargo build --release
python3 bench/corpus.py sync        # clone the 25-repo corpus (once; cached, gitignored)
python3 bench/differential.py       # ~5,000 assertions; writes results/<ts>.json
python3 bench/trend.py              # correctness + ratios across all runs
```

Baseline — 25 repos, all 11 languages, ~200 assertions each:

```
5015 / 5015 correctness assertions PASS   (search 750, fuzz 1456, invariant 2809; 0 FAIL/ERROR)

per-command    correctness   time x   bytes x   (<1 = codeq wins)
search          2206/2206      3.3      1.69     raw grep: rg wins on speed & bytes
definitions      475/475       1.6      5.46     structured defs cost bytes vs a grep heuristic
references       950/950       2.4      2.08
at               909/909        —         —
read             475/475      10.0      0.012    ~80x FEWER tokens than `cat`... but 10x slower
callers             —          2.5      1.12
outline             —          3.4      3.48
imports             —          3.6      4.36
```

The honest read: on *plain search/grep in isolation* codeq is slower and more
verbose than rg. Its wins are (1) **correctness** — one tool, right answers, never
fewer than rg, verified across ~5,000 assertions; and (2) **tokens on real
navigation** — reading a single symbol costs ~1% of `cat`-ing the file, the
operation agents do constantly. The per-command byte/time columns are the
where-to-improve list: `read` is a huge token win but ~10x slower than `cat`;
`definitions`/`imports`/`outline` emit several× the bytes of the grep heuristic.

## Single-command harness

`bench.py` runs agent-style exploration tasks with codeq and with the tool an
agent would otherwise reach for (`rg`), measuring **wall time** (best of 5),
**output bytes** (a token proxy at ~4 bytes/token), and **result count** (how
many hits the agent has to read). Targets (a class, a file, a module) are picked
from the index so it works on any repo.

```bash
git clone --depth 1 https://github.com/colinhacks/zod /tmp/zod
cargo build --release
python3 bench/bench.py /tmp/zod
```

## Results (zod, 374 TS/JS files)

With the git freshness fast-path (queries skip the full tree walk when a
HEAD+status+untracked fingerprint is unchanged), warm queries dropped from
~60 ms to ~12 ms — within ~2x of ripgrep:

```
codeq index (rebuild, cold): ~1.4 s   # one-time; queries below reuse it

task                              tool     ms    bytes  ~tok  results
find definition of a symbol       codeq  11.4      279    69       1
                                  rg      6.7      154    38       2
find all references to a symbol   codeq  11.8      391    97       2
                                  rg      6.6      154    38       2
outline a file's structure        codeq  12.7      224    56       2
                                  rg      1.9      153    38       2
who imports a module              codeq  12.3    20775  5193     179
                                  rg      7.5    16759  4189     179
TODO comments only                codeq  14.8      680   170       4   # --in cached (was ~570ms)
                                  rg      6.5      378    94       4
list all classes                  codeq  11.6     8386  2096      62
                                  rg     10.9    13415  3353     112
text search with context          codeq  17.8      653   163       2   # trigram-prefiltered (was ~43ms)
                                  rg      8.3      628   157      11
```

## vs grep — the ignore-awareness win

We won't beat ripgrep on raw engine speed (it's parallel + SIMD and also
ignore-aware). But plain `grep -r` is neither indexed nor ignore-aware, and it's
the tool an agent types naively. On a clean checkout grep is fast because
there's little to scan; add an installed dependency tree and it inverts.

Same repo + a 16 MB gitignored `node_modules` (4000 files), searching `ZodString`:

```
codeq search  21 ms   # ignores node_modules — flat vs the clean repo
rg             7 ms   # ignores node_modules
grep -rnI     26 ms   # scans node_modules — now slower than codeq
```

That's only 4000 extra files. A real `node_modules` is 100k+ files, where
`grep -r` runs into seconds while codeq (and rg) stay flat. So: **codeq can't
beat ripgrep on speed, but on a realistic repo it beats grep** — because it is
ignore-aware and indexed — *and* returns structure grep can't.

## Honest reading

**rg wins raw speed. codeq wins precision and structure.** We should not
pretend otherwise — the value proposition is not "faster grep".

- **Speed:** rg answers a single query in ~7 ms; codeq in ~12 ms. codeq used to
  be ~60 ms — dominated by a per-query freshness re-walk of the tree — until the
  **git freshness fast-path** landed: an unchanged working tree (same
  HEAD + `git status` + untracked stats) skips the walk entirely, so a warm
  query is now git-status (~3 ms) plus an indexed lookup. It is no longer "10x
  slower than rg", it is "~2x slower and returns structured results". `--in`
  used to be slow (~570 ms) because it re-parsed candidate files per query; it
  now reads comment/string spans **cached in the index** (extracted in the same
  parse as symbols), bringing it to ~15 ms. And raw text/regex search used to
  scan every body (~43 ms); the content index is now a **trigram** index, so a
  regex is prefiltered by its required literals (Zoekt/csearch style) down to
  the candidate files — ~18 ms, and always a true superset (never drops a
  match; when no >=3-char literal exists it safely full-scans).

- **Precision (the real win):** "list all classes" — codeq returns **62 actual
  class definitions**; `rg 'class \w+'` returns **112 lines**, padded with
  matches in comments, strings, and `className=`. codeq is both more correct and
  **fewer tokens** (2096 vs 3353). "find definition" returns one precise answer
  (with signature) versus every textual mention.

- **Structure grep can't give:** codeq answers carry signatures, the enclosing
  symbol of each match, byte/line ranges, and a definition-vs-use flag. That
  routinely saves a *follow-up* read — `definitions X` returns the signature
  without opening the file. This compounding, multi-call saving is **not**
  captured by the single-command timings here, and it is where an agent's real
  token budget is spent.

- **Tokens are mixed, not a blanket win:** when codeq returns the same large,
  low-structure result set (e.g. all 179 importers of a ubiquitous module), its
  JSON is *heavier* than rg's plain lines. codeq saves tokens when structure lets
  it return **fewer, precise** results; it does not when the answer is just a big
  flat list.

## Findings fed back into the tool

- The per-query freshness walk was the dominant query cost → **fixed** with the
  git freshness fast-path (~60 ms → ~12 ms warm).
- `--in` re-parsed per query → **fixed** by caching comment/string spans in the
  index (extracted in the same parse as symbols): ~570 ms → ~15 ms.
- Regex text search scanned every body → **fixed** with a trigram content index
  + required-literal prefiltering: ~43 ms → ~18 ms, and it beats `grep -r` on any
  repo with a gitignored dependency tree.
- JS identifiers containing `$` (e.g. `$ZodError`) break both `rg`'s regex and
  codeq's `\b` word-boundary in `references` — word boundaries assume `\w`
  identifiers. Worth a JS-aware boundary.

## Caveats

Single-command microbenchmarks understate codeq's multi-step advantage (fewer
follow-up reads) and overstate rg on tasks where the agent must then read files
to get what codeq already returned. "Quality of results" is reported here only
as result count and structure; a task-level eval (bug-fix / feature-add with vs
without codeq, measuring total tool calls and tokens to completion) is the
honest next step.
