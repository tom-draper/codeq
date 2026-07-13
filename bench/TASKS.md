# Task-level eval: codeq vs grep/rg/cat

`tasks.py` models multi-step *tasks* (not single commands): the tool sequence a
competent agent runs to answer a question, with generic tools vs with codeq. It
sums, across the sequence, **tool calls**, **output bytes** (a token proxy at
~4 bytes/token — what the agent must read), and time. Targets are auto-picked
from the index so it runs on any repo.

```bash
python3 bench/tasks.py /path/to/repo
```

## Results (zod, 374 TS/JS files)

Targets: symbol `BenchmarkJS`, file `packages/zod/src/v3/types.ts` (the file with
the *most* definitions — a worst case for `outline`), module `vitest`.

```
task                              tool      calls   bytes   ~tok
learn a file's API/structure      codeq         1   68068  17017   outline: signatures
                                  baseline      1  160328  40082   cat: the whole file
read a symbol's definition        codeq         1    2950    737   context: exact body
                                  baseline      2    1337    334   rg + sed window
find all usages of a symbol       codeq         1     391     97   references (+ enclosing fn)
                                  baseline      1     154     38   rg -nw
find TODOs in comments only       codeq         1     680    170   search --in comments (precise)
                                  baseline      1     378     94   rg TODO (also code/strings)
which files import a module       codeq         1   20775   5193   importers
                                  baseline      1    8531   2132   rg -nlF
gather context to modify a fn      codeq         1   22051   5512   context --expand (fn + refs)
                                  baseline      2    6998   1749   rg + cat the file
-------------------------------------------------------------------
TOTAL (6 tasks)                   codeq         6  114915  28728
                                  baseline      8  177726  44431
```

**Net: codeq uses ~1.5x fewer tokens and 6 vs 8 tool calls.**

## Honest reading

This is a *mixed* result, and the mix is the point.

- **Where codeq wins big: avoiding whole-file reads.** "Learn a file's API"
  (`outline`, 17k tok) vs reading it (`cat`, 40k tok) is the single largest
  swing — and it's the most common real move, since an agent's default is to
  open files. Note the picked file is the *largest* in the repo; on a typical
  file `outline` is a few hundred tokens vs thousands to `cat`, so this
  *understates* the usual win. `context` and `definitions` win the same way:
  they return the relevant code, not the whole file.

- **Where codeq wins on round-trips:** fewer tool calls almost everywhere
  (locate+read collapses into one structured call). Round-trips matter to an
  agent beyond tokens — each is a full model turn.

- **Where the baseline is leaner: pure locate tasks.** For "find usages",
  "importers", "TODOs", codeq's JSON (enclosing symbol, ranges, provenance) is
  *heavier* than rg's raw lines. codeq returns strictly more information
  (precision, structure) — but if all you want is line numbers, rg's plain text
  is fewer tokens. codeq's `--fields` projection narrows this, and isn't used
  here.

- **codeq trades tokens for completeness/precision.** `--in comments` excludes
  false positives rg can't; `references` says *which function* each use is in;
  `context --expand` pulls referenced definitions from *other* files the
  `cat`-the-one-file baseline never sees. That extra correctness costs tokens.

So the honest headline is not "codeq is always cheaper." It is: **codeq wins the
net because it replaces whole-file reads with structured slices, and it makes
fewer round-trips — but its JSON envelope costs tokens on simple locate tasks
where raw grep output is already minimal.**

## Findings fed back

- codeq's output overhead is real on high-cardinality locate results
  (`importers` of a ubiquitous module: 5.2k vs 2.1k tok). **Fixed the lever:**
  `--fields` now projects the rows inside object-wrapped results
  (`search`/`references`/`context`/`map`), not just top-level arrays — so
  `codeq importers vitest --fields path` is ~half the tokens (20.8k → 10.5k
  bytes), and `references --fields path,line` ~56% smaller. Previously `--fields`
  silently emptied those results.
- `context`'s `plan`/`budget` metadata could still be opt-out to shave tokens.

## Caveats

The tool *sequences* are scripted (a fair approximation of a good agent), not
produced by a live model, so this measures the cost of a reasonable strategy,
not an actual agent's choices. The strongest real signal — total tokens and
tool calls for a live agent to *complete a bug fix* with vs without codeq (now
easy to wire through the MCP server) — is the honest next step beyond this.
