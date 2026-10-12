---
name: codeq
description: >
  Use codeq for repository exploration: read definitions or the code around a
  search hit, understand files, find callers/callees/tests, trace dependencies,
  and search by text or syntax. Choose the command that answers the whole question
  with the least source and fewest follow-up reads.
---

# Code exploration with codeq

Use `codeq guide` to retrieve the instructions bundled with the executable you
are running. If a saved skill may be stale, run `codeq guide --check <skill-path>`.
A mismatch exits nonzero without changing the file. Run `codeq install` to update
managed skills, then restart the agent. Compare against the same executable you
use for queries, including its explicit path when several builds are installed.

Optimise **tokens and calls to a complete answer**, including follow-up reads.
Choose the smallest useful result from this table. Codeq refreshes automatically;
run commands from anywhere in the repo. Default text is for reading; `--json` is
for programmatic parsing.

| What you need | First command | What it saves |
| --- | --- | --- |
| Definition and its usage before editing | `codeq explore NAME --path src/file.rs` | Definition, imports, callers/callees and their source in one call |
| Only a definition's body | `codeq read file.rs --symbol NAME` | Exact boundaries, without a locating search or guessed window |
| Body enclosing a search hit or stack frame | `codeq read --at file.rs:42` | Resolves and reads that occurrence in one call |
| Neighbourhood around a position | `codeq explore --at file.rs:42` | Resolves the symbol and includes its usage |
| Find implementations mentioning a pattern | `codeq search PATTERN --source --path src --max-tokens 3000` | Matching definitions once, replacing search then repeated range reads |
| Identify matching definitions without bodies | `codeq search PATTERN --symbols --path src` | One row per definition, ranked by matching-line count |
| Understand an unfamiliar file | `codeq api file.rs` or `codeq read file.rs` | Public signatures or folded source instead of all bodies |
| Read known ranges in several files | `codeq read a.rs:40-80 b.rs:12,90-95` | Several slices in one call, with filenames and line numbers |
| Exact source for an edit | `codeq read file.rs --symbol NAME --raw` | Source bytes without display prefixes |
| Who uses it / what it calls | `codeq callers NAME --calls` / `codeq callees NAME` | Enclosing definitions and test sites rather than loose mentions |
| Repo orientation | `codeq map --totals`, then `codeq tree src --min-symbols 1` | Counts and directory shape without reading source |

## Replace search-then-read loops

When looking for code to understand or change, request definitions or source
**on the initial search**:

```sh
codeq search 'retry|backoff' --symbols --path src
codeq search 'retry|backoff' --source --path src --max-tokens 3000
codeq astgrep '$X.unwrap()' --language rust --source --max-tokens 3000
```

`search --symbols` and `search --source` rank by matching-line count, descending;
ties use path and position. `--limit` counts definitions in these modes. Source is budgeted in
that order; definitions that do not fit are listed under `omitted`. Hit frequency
is a navigation heuristic, not a claim of semantic relevance. Matches outside
indexed definitions are excluded from these modes; use ordinary `search` for
module-level code, imports and unsupported file types.

`astgrep --source` returns enclosing definitions ranked by structural match count,
with `--limit` counting definitions. Several matches in one definition produce one
source block. Matches outside indexed definitions are reported in a note; ordinary
`astgrep` returns those exact matches.

If you already have `file:line`, read its enclosing definition directly:

```sh
codeq read --at src/client.rs:83
codeq read --at src/client.rs:83 --at src/client.rs:91 --at src/server.rs:25
codeq read --at src/client.rs:83 --raw
```

`read --at` returns the innermost definition at each position, deduplicates
overlapping source and addresses the occurrence even when names repeat.
A position outside a definition explains that and suggests reading the line.
`explore --at` is for the wider neighbourhood; repeated names in the same file
can contribute several focus definitions there.

## Replace guessed sed windows

Use `read --symbol` or `read --at` when you want a complete definition. Its
boundaries come from the syntax tree, so you neither cut off the body nor read
unrelated surrounding code. For an unfamiliar file, `api` shows the public
surface, `outline` shows all definitions and nesting, and `read` folds long
bodies while keeping imports and surrounding source. Expand only what you need.

When the exact lines are already known, use `read file.rs:40-80`; for several
slices, put ranges on each path. `--raw` emits one exact block and preserves line
endings; it takes one file/range/position. For a single known range,
`codeq read file.rs:40-80 --raw` returns the same source bytes as
`sed -n '40,80p' file.rs`. Numbered output costs extra tokens because it adds
locations. The saving comes from smaller relevant selections and avoiding
follow-up calls, not from making identical source shorter.

## Flat text lookup

Use `codeq search PATTERN` when the enclosing definition matters. It annotates
matches with `[symbol]` and supports regex, `-F`, `-i`, `-w`, `-g`, `-l`, `-c`,
`-A`/`-B`/`-C`, `-e`, `-o`, `-v`, `-L`, `--path` and `--language`.
`--in code|comments|strings` excludes irrelevant syntax without manual filtering.
Positional paths work too: `codeq search TODO src docs`.

Use `rg`/`grep` for a flat list of lines or an exact edit anchor when no definition,
source expansion or structural filtering is needed. On identical line sets,
codeq's symbol annotations can cost extra tokens. Use `sed` for stream
transformations. For repository source reads, choose the `read` mode above.

## Keep results useful and bounded

- Scope broad searches with `--path`, `--language` or `-g` before fetching source.
  Name lookups are exact and case-sensitive across all languages; disambiguate
  common names (`new`, `main`, `build`) with `--path`. `symbols QUERY` finds name
  substrings, case-insensitively, when the spelling is unknown.
- `about NAME` is the neighbourhood metadata without source. Use it when you
  need locations and relationships; `explore` when you need the bodies too.
- `--max-tokens` budgets source in `explore`, `context` and `search --source`.
  Estimates are byte-based, not exact model token counts. Read `omitted` before
  assuming all requested source was returned.
- Broad line lists: `--group` avoids repeating paths; `--max-per-file 3` surveys
  every matching file and states the omitted counts. This is a lossy overview;
  rerun scoped and uncapped for detail. `--fields path,line,text` drops symbol
  annotations when only the lines are needed.
- `search` defaults to `--limit 100`. Truncation is reported; increase the limit
  or use `--limit 0` when completeness matters. Other command defaults differ:
  check `<command> --help` rather than assuming 100 everywhere.
- Empty data and error notes differ: read stderr (or JSON `note`) to distinguish
  no matching symbol, no callers, a mistyped name and a filter selecting no files.
- Call relationships are name-based, not type-resolved. `callers --calls` removes
  bare mentions; `--on RECEIVER` matches a literal receiver, and `--resolved`
  prunes files unreachable through imports. `callees` is already call-only;
  `--examples` shows how a call is written. On a polyglot repo,
  `explore NAME --resolved` can avoid unrelated same-named callees.
  Callees report `resolution=qualified|name-only|ambiguous`, describing syntax
  evidence rather than type checking. Ambiguous candidates stay listed in
  `explore`, with source deferred by default. Read a selected candidate with
  `read --at path:line`, or use `explore --include-ambiguous` for all candidate bodies.

## Other structural questions

Use these when the question needs their extra structure; consult command help
for flags rather than loading a complete command catalogue.

- Code shape: `astgrep '$X.unwrap()' --language rust`. Before guessing syntax
  kinds or trusting an empty result, `astgrep --explain file.rs:42` shows the tree.
- Change impact: `affected --compare-to main`; direct tests: `tests NAME`;
  multi-hop calls: `callers NAME --depth 3`; shortest chain: `callpath FROM TO`.
- Imports: `imports PATH`, `deps FILE`, `importers MODULE`; cycles: `deps --cycles`.
- Types: `members NAME`, `implementors TYPE`, `supertypes TYPE`, `graph`.
- Cleanup: `unused PATH`, `untested PATH`, `clones PATH`, `hotspots PATH --calls`.
  These are candidates: framework entrypoints, indirect tests and name collisions
  need judgement; an empty `clones` result means no identical syntax trees.
- Bundles: `context --symbols a,b --max-tokens 3000`; `--expand` adds references,
  `--resolved` prunes import-unreachable definitions.
- Markdown headings are definitions: `outline README.md` and
  `read README.md --symbol 'Section'` read documentation in slices too.
  SQL tables/views/functions are definitions; `members TABLE` returns columns.

Codeq honours `.gitignore`, `.ignore` and `.codeqignore`. An explicit search path
outside the index is searched from disk with a note; syntax filters cannot apply
there. To explore an ignored dependency as code, point the root at it:
`codeq --root node_modules/library api`.
