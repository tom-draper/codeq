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

```
codeq index (rebuild, cold): ~1.5 s   # one-time; queries below reuse it

task                              tool     ms    bytes  ~tok  results
find definition of a symbol       codeq  60.0      279    69       1
                                  rg      6.7      154    38       2
find all references to a symbol   codeq  59.8      391    97       2
                                  rg      6.6      154    38       2
outline a file's structure        codeq  60.3      224    56       2
                                  rg      2.0      153    38       2
who imports a module              codeq  60.3    20775  5193     179
                                  rg      7.7    16759  4189     179
TODO comments only                codeq 604.9      680   170       4
                                  rg      6.7      378    94       4
list all classes                  codeq  59.6     8386  2096      62
                                  rg     10.7    13415  3353     112
text search with context          codeq  88.3      653   163       2
                                  rg      6.7      628   157      11
```

## Honest reading

**rg wins raw speed. codeq wins precision and structure.** We should not
pretend otherwise — the value proposition is not "faster grep".

- **Speed:** rg answers a single query in ~7 ms; codeq in ~60 ms. codeq's cost
  is dominated by the per-query **freshness re-walk** of the tree (it re-checks
  every file so the answer reflects the current workspace). This is the single
  biggest thing to optimise — a git/mtime-based freshness shortcut would cut it
  sharply. `--in` is far slower (~600 ms) because it re-parses candidate files
  with tree-sitter.

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

- The per-query freshness walk is the dominant query cost → motivates the
  git/mtime freshness optimisation.
- `--in` needs the comment/string spans cached in the index rather than
  re-parsed per query.
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
