# codeq

`codeq` helps AI coding agents query a codebase returning only what it needs with
fewer tool calls. It finds definitions, traces callers, and reads complete
functions without guessing line ranges.

It runs as a CLI or an MCP server. Tree-sitter extracts code structure into a
local SQLite index, which `codeq` keeps up to date as files change. Queries run
locally, completely daemonless, with no model or remote service.

## Install

Release archives contain the executable, matching `GUIDE.md`, and license. Each
archive has a SHA-256 checksum. After the `v0.1.0` release is published, install a
pinned Linux or macOS binary without Rust:

```sh
curl -fsSLo install-codeq.sh https://raw.githubusercontent.com/tom-draper/codeq/v0.1.0/scripts/install.sh
sh install-codeq.sh v0.1.0
```

Windows users can extract the matching ZIP from [releases](https://github.com/tom-draper/codeq/releases).
Run `codeq --version` for the build revision, `codeq capabilities --json` for the
bundled guide digest, and `codeq doctor` after agent setup. Release checks install
the packaged executable in a fresh agent environment and run a navigation smoke
test on Linux, macOS, and Windows. Linux archives use musl.

Build from source with a Rust toolchain and `git` on your PATH:

```sh
cargo install --git https://github.com/tom-draper/codeq --locked
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
The full response has its own `--max-response-tokens` budget. If
`response.next_cursor` is present, pass it with `--cursor` to fetch the next
page. A changed result invalidates the cursor. If `response.minimum_next_tokens`
is present, raise the response budget before continuing. Source omissions list
their path and name for a direct `read --symbol` follow-up.

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
| Find definitions from ordinary words | `codeq discover "refresh auth token" --source` |
| Browse names you only partly know | `codeq symbols QUERY` |
| Read a symbol and its usage | `codeq explore NAME` |
| Find callers at call sites | `codeq callers NAME --calls` |
| See what a function calls | `codeq callees NAME` |
| Find tests that refer to a symbol | `codeq tests NAME` |
| Inspect a type's fields and methods | `codeq members NAME` |
| Trace imports to local files | `codeq deps FILE` |
| Inspect the impact of a change | `codeq affected --compare-to main` |

Use `discover "refresh auth token"` when the name is unknown. Ranking splits
camelCase and snake_case, then combines exact word matches in names, signatures,
docs and paths. Candidates report scores and match reasons. The agent workflow
suite includes deterministic discovery tasks; embeddings have not been evaluated.

Scope text or references to one selected definition:

```sh
codeq search 'refresh' --within cmd_search --path src/commands/find.rs
codeq references definition_scope --within cmd_search --path src/commands/find.rs --kind call
```

`--within` must match exactly one definition. `--within-at PATH:LINE` uses its
exact byte range, including the declaration. Scope applies before limits and
counts. References return each occurrence with `use_kind` and syntax `evidence`.
Read/write classification currently covers Rust, Python and JavaScript/TypeScript.
`--kind read` and `--kind write` include `read-write`. Comments, strings,
unsupported syntax and unsupported languages retain `unknown`; filtered queries
report how many unknown occurrences were excluded. References are textual uses,
not a type-resolved identity analysis.

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

Resolve several locations in one call with `codeq at src/main.rs:20 src/repo.rs:37:5`.
To resolve a stack trace or compiler output, use `codeq at --stdin < trace.txt`.
Python, Rust, Node and Java or Kotlin frame locations are recognized. Columns are
accepted, but definitions are selected by line. Non-location lines are counted as
ignored. Input with no recognized locations is an error.

A single positional location retains the existing definition list. Multiple
locations, `--stdin`, or `--source` return `data.results` in input order, including
repeated frames. Each entry has the original input in `name`, its innermost
definitions in `data`, and a `status` of `resolved`, `unresolved`, or `ambiguous`.
Unresolved frames carry a diagnostic. A Java basename that matches several indexed
files remains ambiguous. Absolute paths outside the repository are unresolved.

Add `--source --max-tokens 4000` for a shared, deduplicated source bundle under
`data.source`. The budget covers source across all frames, at approximately four
bytes per token. Definitions that do not fit are listed as omitted. MCP
`codeq_at.location` accepts either one string or an ordered array of locations.

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

To reclaim disk space after large deletions or branch switches, run
`codeq index --compact`. This rewrites the database and may wait for active
readers. Ordinary queries reuse freed pages without compacting the index.

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

## Dependency views

Use `deps src/util.ts --reverse --depth 3 --limit 50` for incoming file dependencies.
`deps --by dir --depth 2 --limit 50` groups direct imports by directory and labels
internal edges separately from external or unresolved imports. In directory mode,
`depth` is the number of directory components, rather than graph hops.

Add `--mermaid` to either view, or to forward file dependencies, to print a diagram.
Arrows always point from importer to dependency, including in reverse queries.
Directory edges show counts and retain external or unresolved labels. `--limit`
caps file results or directory edges and adds a truncation comment to the diagram.

TypeScript path aliases use the importing file's project, including nested configs,
local inheritance, and project references. JSONC comments and trailing commas work.
Invalid configs produce an error naming the file; ambiguous or ignored project
configs retain an unresolved reason. See [project resolution](docs/project-resolution.md).
Workspace packages honor npm or pnpm membership and explicit package exports,
including private subpath exclusions. Unsupported export conditions stay unresolved.

`packages express` joins declarations to importing files. npm, Cargo, Python
`project.dependencies` and optional groups, and Go `require` declarations are read
from indexed manifests. Imports belong to the nearest manifest in that ecosystem.
Use `--undeclared` or `--unimported` for mismatches. A missing declaration may be a
standard library or local module, so it remains uncertain. Python name matching
is heuristic. Package rows and each row's importer list share `--limit`; total
and truncation fields report withheld importers. `--limit 0` removes the caps.

## Language support and limits

Codeq extracts definitions from Rust, Python, JavaScript, TypeScript, Go, Java,
Ruby, C, C++, C#, PHP, Bash, Lua, R, Julia, Scala, Kotlin, Swift, Dart, Zig, Elixir,
Solidity, Svelte, Vue, CSS, PowerShell, Protobuf, GraphQL, HCL, SQL, Markdown,
reStructuredText, and Jinja templates. Other text files remain searchable.
JSON, YAML, and TOML keys are definitions named by their full path.
Use `outline package.json`, `read pyproject.toml --symbol tool.ruff`, or
`at .github/workflows/ci.yml:12`. Arrays use `[0]` indices, and literal dots or
brackets in keys use JSON-quoted brackets, such as `["a.b"].value`.
Configuration keys never create call edges. YAML aliases are not expanded.
Extraction stops at syntax depth 64 and skips files over 1 MiB or 10,000 keys and common
lockfiles. These files remain searchable; `capabilities` lists the limits.

HTML and XML, including SVG, expose elements as definitions. HTML uses IDs or
`name` attributes when present and tag names otherwise. XML uses IDs or indexed
paths such as `/root[1]/item[2]`. Literal local `href` and `src` values become file
dependencies. HTML also retains Jinja blocks, macros and template imports.
Embedded scripts remain opaque to the HTML extractor.

INI sections and keys, dotenv assignments and Java properties are definitions.
INI keys are qualified by section. Literal dots use the same bracket notation
as JSON keys. Outlines show key names without values. `read --symbol` returns
the original assignment, including multiline values. Properties escapes in keys
are decoded. Values are not evaluated and do not create call edges.

```sh
codeq outline page.html
codeq read settings.ini --symbol server.port
codeq outline messages.properties
```

These extractors skip files over 1 MiB and stop at 10,000 facts. Markup nesting
is limited to 64 levels. Existing ignore rules still apply. Dotfiles such as
`.env.local` need an explicit inclusion such as `!.env.local` in `.ignore`.
Ignored files stay excluded. Existing indexes rebuild automatically when the index schema changes.

AsciiDoc and Org headings define sections through the next heading of the same
or higher level. Named delimited blocks are definitions too. Literal examples
and comment blocks do not produce headings. Local AsciiDoc `include::` directives
and Org file links, `#+INCLUDE` and `#+SETUPFILE` directives become dependencies.
Attribute substitution and document preprocessing are not evaluated.

Dockerfiles and Containerfiles expose named build stages, numeric names for
unnamed stages, and ARG/ENV keys qualified by stage. `graph --language dockerfile`
shows `depends-on` edges for earlier stages named in FROM, COPY `--from` and
RUN `--mount=from=...`. Image names and build contexts are not resolved to files.
Heredoc bodies are skipped. Makefiles expose literal targets, variables and
`define` blocks. Literal includes become file dependencies, and
`graph --language make` connects targets to their declared target prerequisites,
including order-only prerequisites. Recipes remain part of their target's source.
Variables, pattern rules and target-specific assignments are not expanded.
Neither extractor runs commands. These formats share the 1 MiB and 10,000-fact
limits and do not generate call or dead-code candidates.

```sh
codeq outline notes.org
codeq read guide.adoc --symbol Setup
codeq read Dockerfile --symbol build
codeq graph --language make --relation depends-on
```

CMakeLists.txt and `.cmake` files expose functions, macros, literal targets and
`set`/`option` keys. Functions and macros include their bodies in `read --symbol`.
Literal `.cmake` paths in `include` and `add_subdirectory` paths become file
dependencies. CMake module lookup, variables, escaped arguments and generator
expressions are not evaluated. `graph --language cmake` connects targets declared
in the same file through `add_dependencies`, `target_link_libraries` and aliases.
Comments and quoted or bracket arguments support `search --in`. Extraction skips
files over 1 MiB, facts and relations over 10,000, and nesting below 64 levels.
Commands are never executed, and definitions do not become dead-code candidates.

```sh
codeq outline CMakeLists.txt
codeq read helpers.cmake --symbol configure_app
codeq graph --language cmake --relation depends-on
```

Justfiles and `.just` files expose recipes and aliases as targets, assignments
as keys, and module and function declarations. Recipe source includes attributes
and the command body. Literal single-line imports and explicit module paths
become file dependencies. Implicit module lookup, escaped paths and expression evaluation are
not supported. `graph --language just` connects recipes and aliases to targets
declared in the same file, including parameterized and subsequent dependencies.
Recipe commands remain opaque. Comments and strings outside recipe bodies support
`search --in`. Files over 1 MiB or 10,000 facts or relations are skipped, and span
extraction stops at depth 64. Just is never executed, and definitions do not
become dead-code candidates.

```sh
codeq outline justfile
codeq read justfile --symbol build
codeq graph --language just --relation depends-on
```

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

R files (`.r` and `.R`) expose named functions assigned with `<-`, `=`, `<<-`,
`->`, or `->>`, including backtick names and `\(x)` functions. Literal
`library`, `require`, `requireNamespace`, and `loadNamespace` calls expose package
imports. Literal `source` and `sys.source` paths become file dependencies.
Computed paths, escaped strings, quoted code, export visibility, and call
relationships are not resolved. Comment and string filters use the R syntax tree.

Julia files (`.jl`) expose long and short function definitions, modules, macros,
structs, abstract types, and primitive types. `using` and `import` record module
paths, including selective and aliased imports. A literal `include("helpers.jl")`
becomes a file dependency. Quoted code, macro expansion, computed or escaped
include paths, triple-quoted include paths, export visibility, members, and call
relationships are not resolved. Comment and string filters use the Julia syntax tree.

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
      <td align="right"><b>3 ms</b></td>
      <td align="right">9 ms</td>
    </tr>
    <tr>
      <td>Read a function (<code>read --symbol</code>)</td>
      <td>find it, read 60 lines from it</td>
      <td align="right"><b>6.9k</b></td>
      <td align="right">31k</td>
      <td align="right"><b>31</b></td>
      <td align="right">72</td>
      <td align="right"><b>3 ms</b></td>
      <td align="right">10 ms</td>
    </tr>
    <tr>
      <td>Definition enclosing a line (<code>at</code>)</td>
      <td>read the 50 lines above it</td>
      <td align="right"><b>1.5k</b></td>
      <td align="right">15k</td>
      <td align="right">31</td>
      <td align="right">31</td>
      <td align="right">3 ms</td>
      <td align="right"><b>1 ms</b></td>
    </tr>
    <tr>
      <td>A type's members (<code>members</code>)</td>
      <td>find the type, read 60 lines from it</td>
      <td align="right"><b>5.6k</b></td>
      <td align="right">15k</td>
      <td align="right"><b>29</b></td>
      <td align="right">58</td>
      <td align="right"><b>3 ms</b></td>
      <td align="right">8 ms</td>
    </tr>
    <tr>
      <td>Understand before editing (<code>about</code>)</td>
      <td><code>rg</code> the definition and call sites, read both</td>
      <td align="right"><b>22k</b></td>
      <td align="right">62k</td>
      <td align="right"><b>31</b></td>
      <td align="right">62</td>
      <td align="right"><b>9 ms</b></td>
      <td align="right">15 ms</td>
    </tr>
    <tr>
      <td>Explore with source (<code>explore</code>)</td>
      <td>read the definition and 3 call sites, then 3 callees</td>
      <td align="right"><b>124k</b></td>
      <td align="right">159k</td>
      <td align="right"><b>31</b></td>
      <td align="right">126</td>
      <td align="right"><b>9 ms</b></td>
      <td align="right">28 ms</td>
    </tr>
    <tr>
      <td>Who calls it (<code>callers</code>)</td>
      <td><code>rg 'NAME('</code>, read around 5 call sites</td>
      <td align="right"><b>3.7k</b></td>
      <td align="right">78k</td>
      <td align="right"><b>31</b></td>
      <td align="right">59</td>
      <td align="right"><b>6 ms</b></td>
      <td align="right">10 ms</td>
    </tr>
    <tr>
      <td>What it calls (<code>callees</code>)</td>
      <td>find the definition, read 60 lines from it</td>
      <td align="right"><b>6.9k</b></td>
      <td align="right">33k</td>
      <td align="right"><b>31</b></td>
      <td align="right">72</td>
      <td align="right"><b>6 ms</b></td>
      <td align="right">10 ms</td>
    </tr>
    <tr>
      <td>2-hop impact (<code>callers --depth 2</code>)</td>
      <td>as above, then <code>rg</code> calls to those callers</td>
      <td align="right"><b>21k</b></td>
      <td align="right">79k</td>
      <td align="right"><b>31</b></td>
      <td align="right">59</td>
      <td align="right">11 ms</td>
      <td align="right"><b>10 ms</b></td>
    </tr>
    <tr>
      <td>Which tests use it (<code>tests</code>)</td>
      <td><code>rg -w</code> in test files, read around 5 hits</td>
      <td align="right"><b>2.2k</b></td>
      <td align="right">39k</td>
      <td align="right"><b>31</b></td>
      <td align="right">45</td>
      <td align="right"><b>5 ms</b></td>
      <td align="right">8 ms</td>
    </tr>
    <tr>
      <td>Who extends or implements a type (<code>implementors</code>)</td>
      <td><code>rg</code> declaration headers that name the type</td>
      <td align="right"><b>1.1k</b></td>
      <td align="right">6.5k</td>
      <td align="right">24</td>
      <td align="right">24</td>
      <td align="right"><b>2 ms</b></td>
      <td align="right">6 ms</td>
    </tr>
    <tr>
      <td>All references (<code>references</code>)</td>
      <td><code>rg -w</code></td>
      <td align="right">29k</td>
      <td align="right"><b>28k</b></td>
      <td align="right">31</td>
      <td align="right">31</td>
      <td align="right"><b>6 ms</b></td>
      <td align="right">7 ms</td>
    </tr>
    <tr>
      <td>A file's structure (<code>outline</code>)</td>
      <td>read the whole file</td>
      <td align="right"><b>60k</b></td>
      <td align="right">482k</td>
      <td align="right">31</td>
      <td align="right">31</td>
      <td align="right">4 ms</td>
      <td align="right"><b>3 ms</b></td>
    </tr>
    <tr>
      <td>A file's imports (<code>imports</code>)</td>
      <td><code>rg</code> import lines, read grouped blocks</td>
      <td align="right"><b>6.2k</b></td>
      <td align="right">6.3k</td>
      <td align="right"><b>31</b></td>
      <td align="right">40</td>
      <td align="right">4 ms</td>
      <td align="right">4 ms</td>
    </tr>
    <tr>
      <td>Who imports a file (<code>importers</code>)</td>
      <td><code>rg</code> import lines naming the file</td>
      <td align="right"><b>6.5k</b></td>
      <td align="right">9.1k</td>
      <td align="right">31</td>
      <td align="right">31</td>
      <td align="right"><b>4 ms</b></td>
      <td align="right">7 ms</td>
    </tr>
    <tr>
      <td>A file's untested API (<code>untested</code>)</td>
      <td><code>rg</code> declaration lines, then <code>rg -w</code> per name in test files</td>
      <td align="right"><b>19k</b></td>
      <td align="right">30k</td>
      <td align="right"><b>31</b></td>
      <td align="right">62</td>
      <td align="right"><b>17 ms</b></td>
      <td align="right">525 ms</td>
    </tr>
    <tr>
      <td>A file's dead code (<code>unused</code>)</td>
      <td><code>rg -w NAME | wc -l</code> per definition, in one loop</td>
      <td align="right"><b>0.8k</b></td>
      <td align="right">30k</td>
      <td align="right"><b>31</b></td>
      <td align="right">62</td>
      <td align="right"><b>20 ms</b></td>
      <td align="right">584 ms</td>
    </tr>
    <tr>
      <td colspan="2"><b>All 17 tasks</b></td>
      <td align="right"><b>324k</b></td>
      <td align="right">1,121k</td>
      <td align="right"><b>518</b></td>
      <td align="right">906</td>
      <td align="right"><b>115 ms</b></td>
      <td align="right">1,244 ms</td>
    </tr>
    <tr>
      <td colspan="2"></td>
      <td colspan="2" align="center"><b>3.45x</b></td>
      <td colspan="2" align="center"><b>1.75x</b></td>
      <td colspan="2" align="center"><b>10.8x</b></td>
    </tr>
  </tbody>
</table>

`implementors` ran on the 24 repositories with extends or implements edges.
`importers` matches a file's stem as a whole name in each import, and drops an
import the dependency graph resolves to a different file.
An earlier benchmark run found no JavaScript or TypeScript tests that used the
queried name when those tests were written as bare `it(...)` callbacks. `tests`
now recognizes `it`, `test`, `specify`, and nested `describe` callbacks, including
their `.each(...)` forms. Times are warm queries and exclude the agent's own turn.
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
The workflow correctness CI job runs this check and the MCP session tests on
every pull request and push to `main`, using local fixtures and no model calls.

Licensed under [MIT](LICENSE).
