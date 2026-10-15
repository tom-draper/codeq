# codeq

`codeq` helps AI coding agents query a codebase returning only what it needs with
fewer tool calls. It finds definitions, traces callers, and reads complete
functions without guessing line ranges.

It runs as a CLI or an MCP server. Tree-sitter extracts code structure into a
local SQLite index, which `codeq` keeps up to date as files change. Queries run
locally, completely daemonless, with no model or remote service.

## Install

Build from source with a Rust toolchain and `git` on your PATH:

```sh
git clone https://github.com/tom-draper/codeq.git
cd codeq
cargo install --path .
```

Make sure your Cargo bin directory is on your PATH, usually `~/.cargo/bin`.
You can now use `codeq` in any repository.

To connect it to your coding agents:

```sh
codeq install --dry-run
codeq install
```

The installer registers the MCP server and copies a bundled guide for detected
agents. It supports Claude Code, Cursor, Windsurf, Gemini CLI, and Codex.
Restart your agent after setup. See [agent setup](docs/agent-setup.md) for
configuration paths, updates, and removal.

## Try it on this repository

Start with a summary, then inspect a file's public definitions:

```sh
codeq map --totals
codeq api src/repo.rs
```

Explore a function and its usage in one call:

```sh
codeq explore language_for --path src/index.rs
```

The result includes its definition, callers, callees, and source, plus imports
from the definition's file. Source has an approximate token budget, with a
list of anything omitted. Use `--max-tokens 8000` when you need more context.

To read just that function:

```sh
codeq read src/index.rs --symbol language_for
```

No initial indexing command is needed. The first query creates the index;
later queries refresh changed files before answering. Nothing runs in the background.

## Choose a command by task

These are useful starting points. Replace `NAME`, `FILE`, and `PATH` with names
and paths from your repository.

| You want to... | Command |
| --- | --- |
| Find where the code lives | `codeq tree src --min-symbols 1` |
| Understand a file's structure | `codeq outline FILE` |
| Inspect public definitions | `codeq api PATH` |
| Find an exact definition | `codeq definitions NAME` |
| Browse names you only partly know | `codeq symbols QUERY` |
| Read a symbol and its usage | `codeq explore NAME` |
| Find callers at call sites | `codeq callers NAME --calls` |
| See what a function calls | `codeq callees NAME` |
| Find tests that refer to a symbol | `codeq tests NAME` |
| Inspect a type's fields and methods | `codeq members NAME` |
| Trace imports to local files | `codeq deps FILE` |
| Inspect the impact of a change | `codeq affected --compare-to main` |

For common names such as `new` or `open`, add `--path` to narrow the lookup.
Name lookups are exact and case-sensitive; `symbols` searches name substrings
without regard to case.

Use `codeq --help` for the full command list and `codeq <command> --help` for
flags and defaults. The [bundled agent guide](.agents/skills/codeq/SKILL.md)
covers more workflows, including call chains, type relations, and cleanup queries.

Look up several exact names in one call:

```sh
codeq definitions language_for refresh
codeq about language_for refresh
```

Results follow argument order. Single-name JSON output keeps its existing shape;
batches put `{name, data, note?}` entries in `data.results`. Each entry's `data`
has the same shape as a single-name query. A missing name has its own note, even
when other names resolve. `--path` and `about --resolved` apply to every name;
`definitions --limit` applies separately to each name. Output flags apply to
each entry's rows. With `--max-per-file`, a capped entry includes
`truncated_per_file` counts. The MCP tools accept arrays through `symbol` for
`codeq_definitions` and `name` for `codeq_about`; scalar names still work.

## Read only the source you need

`codeq read FILE` folds long bodies, keeping imports, signatures, and surrounding
source visible. Expand a definition, a position, or a known range:

```sh
codeq read src/repo.rs
codeq read src/repo.rs --symbol changed_since
codeq read --at src/index.rs:808
codeq read src/repo.rs:30-57 src/index.rs:808-818
```

`read --at` returns the innermost definition enclosing a line. You can repeat
`--at` to read several positions in one call. Add `--raw` to a single selection
for exact source without line-number prefixes, or `--full` to unfold a file.

Markdown headings are definitions too:

```sh
codeq outline README.md
codeq read README.md --symbol 'Install'
```

## Search and read matching definitions

Text search annotates each match with its enclosing definition. Add `--source`
to get matching definitions' bodies in the same call:

```sh
codeq search 'retry|backoff' --path src
codeq search 'retry|backoff' --source --path src --max-tokens 3000
codeq search TODO --in comments
```

Use `--symbols` instead of `--source` for a list of matching definitions.
Both modes rank definitions by matching-line count, with deterministic ties.
Matches outside indexed definitions appear only in ordinary search.

For a syntax pattern, use `astgrep`:

```sh
codeq astgrep '$X.unwrap()' --language rust --source --max-tokens 3000
```

Use `rg` for a flat text lookup when you don't need code structure. Codeq's
savings come from selecting relevant definitions and avoiding follow-up reads;
reading the same exact range with codeq and `sed` returns the same source bytes.

## Scope queries

Codeq honors `.gitignore`, `.ignore`, and `.codeqignore`. To inspect an ignored
dependency, set its directory as the root:

```sh
codeq --root node_modules/lodash api
```

The index lives at `.codeq/index.db` under the selected root. It is ignored by
Git and safe to delete. Codeq needs no background indexing process.

## Output contract

Most commands return compact text. Add `--json` for the `codeq.result.v2`
envelope, or `--meta` to include repository and index metadata on every result:

```sh
codeq definitions language_for --json
codeq map --totals --json --meta
```

`read` emits source text even with `--json`. Index refresh messages and diagnostic
notes go to stderr in text mode. JSON results include diagnostic notes in the
envelope. Check notes and truncation markers before treating an empty or limited
result as a complete answer.

Before 1.0, releases may change commands, flags, and text output. For scripts,
check the JSON `schema` and use `rows` to locate the result array when present.
`codeq capabilities` reports the CLI version, result schema, and index schema.
When the index schema changes, codeq rebuilds its cache on the next query;
upgrades and downgrades need no manual index migration.

## Language support and limits

Codeq extracts definitions from Rust, Python, JavaScript, TypeScript, Go, Java,
Ruby, C, C++, C#, PHP, Bash, Lua, Scala, Kotlin, Swift, Dart, Zig, Elixir,
Solidity, Svelte, Vue, CSS, PowerShell, Protobuf, GraphQL, HCL, SQL, Markdown,
reStructuredText, and Jinja templates. Other text files remain searchable.
JSON, YAML, and TOML have no extracted definitions. `outline` and `at` report
that limitation in stderr or the JSON `note`; `read --symbol`, `read --at`, and
`explore --at` explain it too. Use `search` or `read` with a line range to inspect
these files.

Support varies by language and query. Run `codeq capabilities` for the support
matrix bundled with your executable.

Call relationships use syntax and name matching, without full type checking.
Same-named methods can produce multiple candidates. `--resolved` uses import
reachability to narrow candidates, but unresolved imports can hide real calls.
Callees report `qualified`, `name-only`, or `ambiguous` resolution; `explore`
lists ambiguous candidates and omits their bodies unless you pass
`--include-ambiguous`.

Treat `unused`, `untested`, and `affected` as investigation aids. They cannot
prove that code is dead, that tests cover behavior, or that every runtime effect
of a change has been found.

## Benchmarks and development

The benchmarks compare output size and tool calls for complete navigation tasks,
and check correctness on fixtures and pinned repositories. Results depend on
the task, source budget, and repository. See [benchmark methodology](bench/README.md)
and [real workflow results](bench/REAL_WORKFLOWS.md).

### Evaluation

[`bench/completion.py`](bench/completion.py) runs each task with codeq and with
the commands Claude Code or Codex would run to reach the same answer, then
records output tokens (bytes / 4), tool calls, and average time per repository.
A tool call is one agent turn: independent commands share a call, as parallel
tool calls or one shell invocation do. Searches find a definition with a
declaration pattern and read a 60-line window, not the whole file. Where the
baseline needs to know which names to check, it gets them from codeq for free.
Totals across 31 pinned open-source repositories in every supported language,
with the lower value in bold:

<table>
  <thead>
    <tr>
      <th rowspan="2" align="left">Task</th>
      <th rowspan="2" align="left">Generic approach</th>
      <th colspan="2">Tokens</th>
      <th colspan="2">Tool calls</th>
      <th colspan="2">Avg time per repo</th>
    </tr>
    <tr>
      <th align="right">codeq</th>
      <th align="right">generic</th>
      <th align="right">codeq</th>
      <th align="right">generic</th>
      <th align="right">codeq</th>
      <th align="right">generic</th>
    </tr>
  </thead>
  <tbody>
    <tr>
      <td>Find a definition (<code>definitions</code>)</td>
      <td>declaration <code>rg</code>, then <code>rg -w</code> if it misses</td>
      <td align="right"><b>7.6k</b></td>
      <td align="right">18k</td>
      <td align="right"><b>31</b></td>
      <td align="right">41</td>
      <td align="right"><b>4 ms</b></td>
      <td align="right">12 ms</td>
    </tr>
    <tr>
      <td>Read a function (<code>read --symbol</code>)</td>
      <td>find it, read 60 lines from it</td>
      <td align="right"><b>6.9k</b></td>
      <td align="right">33k</td>
      <td align="right"><b>31</b></td>
      <td align="right">72</td>
      <td align="right"><b>5 ms</b></td>
      <td align="right">15 ms</td>
    </tr>
    <tr>
      <td>Definition enclosing a line (<code>at</code>)</td>
      <td>read the 50 lines above it</td>
      <td align="right"><b>1.5k</b></td>
      <td align="right">15k</td>
      <td align="right">31</td>
      <td align="right">31</td>
      <td align="right">4 ms</td>
      <td align="right"><b>2 ms</b></td>
    </tr>
    <tr>
      <td>A type's members (<code>members</code>)</td>
      <td>find the type, read 60 lines from it</td>
      <td align="right"><b>5.6k</b></td>
      <td align="right">15k</td>
      <td align="right"><b>29</b></td>
      <td align="right">58</td>
      <td align="right"><b>5 ms</b></td>
      <td align="right">11 ms</td>
    </tr>
    <tr>
      <td>Understand before editing (<code>about</code>)</td>
      <td><code>rg</code> the definition and call sites, read both</td>
      <td align="right"><b>22k</b></td>
      <td align="right">62k</td>
      <td align="right"><b>31</b></td>
      <td align="right">62</td>
      <td align="right"><b>14 ms</b></td>
      <td align="right">21 ms</td>
    </tr>
    <tr>
      <td>Explore with source (<code>explore</code>)</td>
      <td>read the definition and 3 call sites, then 3 callees</td>
      <td align="right"><b>124k</b></td>
      <td align="right">158k</td>
      <td align="right"><b>31</b></td>
      <td align="right">126</td>
      <td align="right"><b>13 ms</b></td>
      <td align="right">39 ms</td>
    </tr>
    <tr>
      <td>Who calls it (<code>callers</code>)</td>
      <td><code>rg 'NAME('</code>, read around 5 call sites</td>
      <td align="right"><b>3.7k</b></td>
      <td align="right">79k</td>
      <td align="right"><b>31</b></td>
      <td align="right">59</td>
      <td align="right"><b>9 ms</b></td>
      <td align="right">14 ms</td>
    </tr>
    <tr>
      <td>2-hop impact (<code>callers --depth 2</code>)</td>
      <td>as above, then <code>rg</code> calls to those callers</td>
      <td align="right"><b>21k</b></td>
      <td align="right">79k</td>
      <td align="right"><b>31</b></td>
      <td align="right">59</td>
      <td align="right">16 ms</td>
      <td align="right"><b>13 ms</b></td>
    </tr>
    <tr>
      <td>Which tests use it (<code>tests</code>)</td>
      <td><code>rg -w</code> in test files, read around 5 hits</td>
      <td align="right"><b>2.2k</b></td>
      <td align="right">43k</td>
      <td align="right"><b>31</b></td>
      <td align="right">45</td>
      <td align="right"><b>8 ms</b></td>
      <td align="right">11 ms</td>
    </tr>
    <tr>
      <td>All references (<code>references</code>)</td>
      <td><code>rg -w</code></td>
      <td align="right">29k</td>
      <td align="right"><b>28k</b></td>
      <td align="right">31</td>
      <td align="right">31</td>
      <td align="right">8 ms</td>
      <td align="right">8 ms</td>
    </tr>
    <tr>
      <td>A file's structure (<code>outline</code>)</td>
      <td>read the whole file</td>
      <td align="right"><b>60k</b></td>
      <td align="right">482k</td>
      <td align="right">31</td>
      <td align="right">31</td>
      <td align="right">5 ms</td>
      <td align="right"><b>3 ms</b></td>
    </tr>
    <tr>
      <td>A file's public API (<code>api FILE</code>)</td>
      <td><code>rg</code> declaration lines in the file</td>
      <td align="right">56k</td>
      <td align="right"><b>20k</b></td>
      <td align="right">31</td>
      <td align="right">31</td>
      <td align="right">5 ms</td>
      <td align="right">5 ms</td>
    </tr>
    <tr>
      <td>A package's public API (<code>api DIR</code>)</td>
      <td><code>rg</code> declaration lines in the directory</td>
      <td align="right">613k</td>
      <td align="right"><b>333k</b></td>
      <td align="right">31</td>
      <td align="right">31</td>
      <td align="right">9 ms</td>
      <td align="right"><b>8 ms</b></td>
    </tr>
    <tr>
      <td>A file's imports (<code>imports</code>)</td>
      <td><code>rg</code> import lines, read grouped blocks</td>
      <td align="right"><b>6.2k</b></td>
      <td align="right">6.3k</td>
      <td align="right"><b>31</b></td>
      <td align="right">40</td>
      <td align="right">5 ms</td>
      <td align="right">5 ms</td>
    </tr>
    <tr>
      <td>Who imports a file (<code>importers</code>)</td>
      <td><code>rg</code> import lines naming the file</td>
      <td align="right">22k</td>
      <td align="right"><b>9.1k</b></td>
      <td align="right">31</td>
      <td align="right">31</td>
      <td align="right"><b>4 ms</b></td>
      <td align="right">9 ms</td>
    </tr>
    <tr>
      <td>A file's dead code (<code>unused</code>)</td>
      <td><code>rg -w NAME | wc -l</code> per definition, in one loop</td>
      <td align="right"><b>0.8k</b></td>
      <td align="right">30k</td>
      <td align="right"><b>31</b></td>
      <td align="right">62</td>
      <td align="right"><b>24 ms</b></td>
      <td align="right">696 ms</td>
    </tr>
    <tr>
      <td><b>All 16 tasks</b></td>
      <td></td>
      <td align="right"><b>982k</b></td>
      <td align="right">1,411k</td>
      <td align="right"><b>494</b></td>
      <td align="right">810</td>
      <td align="right"><b>138 ms</b></td>
      <td align="right">871 ms</td>
    </tr>
  </tbody>
</table>

codeq returns more on the API tasks, because it gives complete signatures where
a grep gives first lines. `importers` matches a file's stem as a substring of
each import string, so a short stem over-matches: `sr.js` also matches `../src`.
On 2 JavaScript and TypeScript repositories `tests` found nothing where tests do
use the name, because tests written as bare `it(...)` calls have no named test
to report. Times are warm queries and exclude the agent's own turn.
Building the index took under 1 s per repository. Token counts are approximate
and targets are picked automatically, so treat these as indicative rather than a
model-agent evaluation. Rerun with `python3 bench/completion.py` after
`cargo build --release`.

### Development

For local development:

```sh
cargo build
cargo test
python3 bench/agent_workflows.py --codeq target/debug/codeq
```

The workflow check verifies complete source answers before comparing output
bytes and call counts. See [benchmark commands](bench/README.md) for the wider
correctness and performance checks.

Licensed under [MIT](LICENSE).
