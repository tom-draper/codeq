# Benchmark: codeq vs grep/rg/find

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
