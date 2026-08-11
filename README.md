# codeq

codeq is a deterministic index of repository facts, built for AI coding agents.

Agents burn tokens and time rediscovering things a computer can just compute: where a
symbol is defined, who calls it, what a file contains, what changed since a commit. codeq
parses the repo once into SQLite and answers precise, structured queries, so an agent can
navigate a codebase without re-deriving it with `find`, `rg`, `git`, and `cat` on every
task.

The rule it follows: codeq reports facts, it does not guess. No relevance ranking, no
"did you mean", no intent inference. The same query returns the same answer every time. An
agent that can trust the output has no reason to fall back to ad-hoc shell pipelines.

> Status: working prototype. Symbol and import extraction covers Rust, Python, JavaScript,
> TypeScript/TSX, Go, Java, Ruby, C, C++, C#, PHP, Bash, Lua, Scala, Kotlin, Swift, Dart, Zig,
> Elixir, Solidity, Svelte, Vue, CSS, PowerShell, Protobuf, GraphQL, HCL/Terraform, and
> Markdown. Other
> languages are indexed as files and stay fully searchable. `codeq capabilities` is the
> authoritative list. See [DRAFT.md](./DRAFT.md) for the design rationale.

## Install

```bash
cargo build --release
# binary at ./target/release/codeq
```

You need a Rust toolchain and `git` on PATH. The index lives at `.codeq/index.db` under the
repo root. It is git-ignored and safe to delete.

## Why not grep and find?

grep and find work on bytes and lines. codeq works on a parsed model of the code, which
buys a few things a byte scan cannot:

- Structure. `grep authenticate` mixes the definition, every call, and comment mentions
  together. `codeq definitions authenticate` returns just the definition, with its
  signature and doc. Every `codeq search` match is tagged with the definition that encloses
  it, so you get `fn cmd_search` instead of `file:625`.
- Relationships. Who calls this, what does it call, who implements this trait, who imports
  this module, what breaks if I change it. `callers`, `callees`, `implementors`,
  `importers`, and `about` answer these directly.
- Less to read. `outline` and `api` show a file or module from signatures and docstrings
  alone, no bodies, so an agent grasps the shape for a fraction of the tokens. `members`
  gives a type's fields and variants without opening the file, and is structural rather
  than name-based, so it has none of the same-name ambiguity the call graph does.
- Computed once, kept fresh. Each file is parsed once; only changed files (by size and
  mtime) are reparsed. The cost is amortized across a session of many queries against a
  mostly stable tree, which is the agent workload. For a single one-off text search a cold
  `rg` will often win, and that is fine: codeq is not trying to replace it there.

search itself is a strict superset of grep. It uses the same regex engine as ripgrep (the
Rust `regex` crate), adds the flags you would reach for `rg` to get (`-A`/`-B`/`-C`, `-l`,
`-c`, `-w`, `-g`), and does one thing grep cannot: `--in code|comments|strings` restricts
matches by syntactic class, so you can find a TODO only in comments, or a literal only in
real code.

That extends to a pipeline. `--stdin` scans text piped in instead of the index, so codeq
also stands in for grep where there is no repository to index:

```bash
git log --oneline -20 | codeq search '^fix' --stdin
curl -s $URL | codeq search 'apiKey' --stdin --language javascript --in strings
```

Add `--language` and the buffer is parsed once, which gives the two things grep still
cannot do on a stream: `--in code|comments|strings`, and the enclosing `[symbol]` on every
match. Without it the scan is exact but has neither, and `--in` is rejected rather than
silently ignored. A piped scan opens no index and needs no repo.

`--stdin` is explicit and never inferred from whether stdin is a TTY. That is deliberate:
an agent's shell usually hands the process an open pipe, so auto-detection would silently
turn every ordinary `codeq search` into a scan of an empty buffer.

Where structure is available, codeq uses it instead of matching characters. The
`--calls` and `--on` precision filters parse the candidate files and read call sites
off the tree, so a declaration is not a call, formatting is irrelevant, and the
receiver is the expression as written (`--on a.inner`, or even `--on 'conn()'`)
rather than whichever identifier happens to sit before the dot. That parse happens
only when those filters are used, so ordinary queries pay nothing and the index does
not grow. A name
mentioned in a comment is not a use, so a stale `// TODO: wire up foo later` cannot
hide dead code from `unused`; `helper()` written inside a string or a comment is not
a call, so it is not a callee. A mention in a *string* still counts as a use, because
reflection, serde renames, config keys and SQL identifiers are real uses that live in
string literals, and a file codeq cannot parse keeps all of its occurrences rather
than losing them.

A function declared inside a type body is recorded as a `method` rather than a
`function`, which several grammars do not distinguish for you: Rust's `function_item`
and Python's `function_definition` are the same node whether or not they sit in an
`impl` or a `class`. Rust additionally requires a `self` parameter, so an associated
function called as `S::assoc()` stays a `function`. A nested function or closure is
never a method, however deep the class above it.

Support is uneven across the 29 languages, so `capabilities` reports a
`language_support` matrix — which of definitions, imports, members, relations, spans
and calls each language actually has, derived from the extraction queries rather than
a hand-kept list. Go and C have no inheritance edges at all; Ruby declares no fields.
When a command comes back empty for that reason it says so, rather than letting an
unsupported language read as evidence of absence.

The honest boundary: codeq's facts are syntactic, not deeply semantic. It knows an
identifier is a function definition. It does not resolve a call to its definition across
module aliases, generics, or build systems. The graph commands match by name, with opt-in
heuristics (`--resolved`, `--calls`, `--on`) to cut false positives. Half-right resolution
would produce the untrustworthy results that send an agent back to grep, so codeq stops at
facts it can stand behind.

## Benchmarks

How much does codeq actually save an agent? `bench/completion.py` runs, for each of a
dozen realistic tasks, the exact tool sequence a competent agent would use to reach the
same answer with generic tools (ripgrep to locate, then read a ~60-line window, never a
strawman), and sums what the agent pays: tokens ingested (output bytes over 4) and tool
calls. Across a corpus of 20 real open-source repositories, pinned by tag and spanning
every supported language:

| Task | codeq tokens | generic tokens | saving | codeq calls | generic calls |
| --- | --: | --: | --: | --: | --: |
| explore a symbol (def + callers + callees, with source) | 107k | 161k | 1.5x | 25 | 243 |
| understand a symbol before editing (`about`) | 16k | 70k | 4.3x | 25 | 99 |
| read a function's body (`read --symbol`) | 9k | 35k | 3.9x | 25 | 50 |
| who calls it, with context (`callers`) | 3k | 43k | 13.9x | 25 | 66 |
| 2-hop impacted set (`callers --depth 2`) | 53k | 194k | 3.7x | 25 | 155 |
| which tests exercise it (`tests`) | 2k | 27k | 15.0x | 25 | 72 |
| a file's dead code (`unused`) | 18k | 1144k | 64.2x | 25 | 436 |
| learn a file's structure (`outline`) | 75k | 751k | 10.0x | 25 | 25 |
| a file's public API (`api`) | 80k | 25k | 0.3x | 25 | 25 |
| a package's public API (`api <dir>`) | 378k | 1581k | 4.2x | 25 | 284 |
| all references (`references`) | 38k | 40k | 1.1x | 25 | 25 |
| a file's imports (`imports`) | 9k | 8k | 0.9x | 25 | 39 |
| **all 12 tasks** | **788k** | **4.08M** | **5.2x** | **300** | **1519** |

> **These numbers predate the merge of the component/clones and markdown/relations
> lines and have not been re-measured on the combined code.** They came from a
> 25-repo run; a parallel 20-repo run on the other line reported different figures
> for the same tasks. Re-run `python3 bench/completion.py` before quoting them.

Completing these tasks costs a generic-tool agent **5.5x the tokens and 5x the tool calls**.
The `explore` win is mostly in round-trips: one call instead of about ten. codeq is not
ahead everywhere, and the harness is built to say so: on plain reference search and
single-line imports it is near parity with ripgrep, and `api` deliberately "loses" on
tokens because it returns complete signatures with visibility where a declaration grep
returns incomplete first-lines. Reproduce with `python3 bench/completion.py`; each run is
written to `bench/results/` (committed, so the trend lives in git).

## Commands

Run any command inside a repo. Default output is compact text (`path:line:col: payload`,
grep style); add `--json` for the structured envelope.

Understand a symbol in one call:
- `codeq explore <name>` returns the symbol's definition, its callers, its callees, and the
  verbatim source of that whole neighbourhood, budgeted and grouped by file. It is the
  fastest way to understand or change a function: structure and code in one call, instead of
  a `definitions` then `callers` then `read` then `read` sequence. `--max-tokens` caps the
  source (the definition is always included; callers and callees fill the rest, and anything
  dropped is listed). A definition too big for the budget is *reduced*, never dropped and
  never allowed to overrun: its nested bodies fold to signatures first, and its tail is
  clipped only if folding isn't enough. Either way a `⋮` marker names the exact hidden line
  range and the chunk is tagged `reduced`, so `read --lines` recovers the rest.

Orient and read:
- `codeq map` groups counts (languages, directories, imports, definitions) to get your bearings.
- `codeq read <file>` returns the file with long bodies folded to signatures (see below).
- `codeq outline <file>` lists definitions top to bottom with nesting.
- `codeq api <path>` shows only the exported definitions of a file or directory, with signatures.

Find and understand a symbol:
- `codeq definitions <name>` gives the definition(s), with signature, doc, and ranges.
  Struct fields, class properties and enum variants are definitions too, each carrying the
  type that declares it — so `definitions retries` answers *which* type's field it found
  rather than leaving a bare name. `outline` nests them under their type. Python has no
  declaration site for an attribute, so a member is its earliest class-body or `self.x =`
  assignment, deduplicated across the methods that assign it.
- `codeq about <name>` is a one-call dossier: definition plus callers plus callees.
- `codeq at <path>:<line>` names the definition enclosing a location, e.g. from a stack trace.

Trace relationships (matched by name):
- `codeq callers <name>` and `codeq callees <name>` show who calls it and what it calls.
  `--depth N` walks the tree; `callers --examples` prints the actual call line at each site.
- `codeq references <name>` lists every use, each tagged with its enclosing definition.
- `codeq callpath <from> <to>` finds the shortest call chain between two symbols.
- `codeq implementors <type>` and `codeq supertypes <type>` show who implements or extends a
  type, and what it derives from.
- `codeq tests <name>` finds the tests that exercise a symbol.
- `codeq unused [path]` lists definitions with no reference anywhere (candidate dead code).
- `codeq tree [path]` shows the directory shape, folded to `--depth`, with each
  directory's file and symbol counts aggregated from everything below it.
- `codeq hotspots [path]` is its inverse: definitions ranked by how often they are
  referenced, for finding what carries an unfamiliar module. `--calls` counts only
  call sites.
- `codeq clones` groups definitions whose syntax shape is identical up to renaming — the
  same code written twice. Exact, not fuzzy: shapes match or they don't, so there is no
  similarity score to interpret. `--min-nodes` sets how much code a shared shape must
  cover. Ask it before writing a helper, or to find the copy-paste that needs one fix in
  four places.

Imports and dependencies:
- `codeq imports [path]` and `codeq importers <query>` list import statements, and who
  imports a module.
- `codeq deps <file>` shows the internal files a file depends on (`--cycles` for import cycles).

Search:
- `codeq search <pattern>` is content search, each match tagged with its enclosing definition.
- `codeq astgrep '<pattern>' --language <lang>` matches over the syntax tree, e.g. `'$X.unwrap()'`.

Repo state:
- `codeq changed --compare-to <ref>` lists files changed vs a git ref (`--symbols` for the
  definitions touched).
- `codeq affected --compare-to <ref>` takes a diff and reports the definitions it touched,
  their transitive callers (the impacted set), and the test files that exercise any of them,
  so you know what to re-check and re-run.
- `codeq files`, `codeq symbols`, and `codeq stats` list and count.
- `codeq capabilities` self-describes: commands, languages, fact kinds, schema.

Cross-cutting flags: `--path <substr>` scopes a name-based lookup to matching files,
`--fields a,b` trims output to the fields you want, and `--json` switches to the structured
envelope.

## Markdown is indexed like code

A heading is a definition: it has a name, a byte range (down to the next heading of
the same or higher level) and a nesting depth, which is exactly the model the code
extractors produce. So the existing commands work on documentation with nothing new
to learn:

```
$ codeq outline README.md          # the table of contents
$ codeq read README.md             # folded to that ToC, with each section's range
$ codeq read README.md --symbol "Output contract"   # just that section, verbatim
$ codeq at README.md:230           # which section is this line in
```

Reading one section instead of a whole README is a large saving on exactly the files
that are pure prose and do not compress.

Fenced code blocks are `strings` and prose is `comments`, so `--in strings` searches
the documented examples and `--in comments` searches the surrounding text (`--in code`
is empty for markdown, which has none). Search hits carry their enclosing `[Section]`.

Local links and images (`[g](./guide.md)`, `![logo](assets/logo.png)`) are file
references, so `deps` and `importers` cover documentation and its assets; external
URLs and bare `#anchor` links name no file and are skipped, and link syntax inside
inline code is an example rather than a link. `codeq deps --external` lists every
import that resolves to nothing: the third-party dependency list, with broken local
references flagged. Headings are excluded
from `unused`, since nothing ever references prose by name.

Parsed line-wise rather than with a grammar: headings are unambiguous once fenced
blocks are tracked, so a `# comment` inside a shell example is not mistaken for a
heading. The `source` field records this (`lines/markdown` vs `treesitter/rust`).

## read, a folding file reader

`codeq read <file>` is built for a token budget. It returns the file with long function
bodies folded to their signature plus a marker with the hidden line range, so you see the
whole structure (imports, signatures, docstrings) cheaply, then open only what you need:

```
  20  impl Repo {
  23      pub fn discover(start: &Path) -> Result<Repo> {
          ... 8 lines (24-31)
  33      pub fn vcs(&self) -> &'static str {
          ... 6 lines (34-39)
```

- `codeq read <file> --lines 24-31` for a verbatim, line-numbered slice.
- `codeq read <file> --symbol discover` for one named definition, verbatim.
- `codeq read <file> --raw --symbol discover` for exact bytes with no line prefixes, ready
  to drop into an edit.
- `codeq read <file> --full` for the whole file, unfolded.

## context

`codeq context` assembles the smallest useful slice of code for a task under a token budget,
from explicit seeds only (it never infers what you are working on):

- Seed with `--symbols name,name` and/or `--changed`.
- Each chunk is a definition's source with provenance and an approximate token estimate.
- `--expand` adds one deterministic hop to definitions referenced by name in the seeds.
- When the budget truncates, the response says what was left out, so the agent can ask for more.

## MCP server

`codeq mcp` runs a Model Context Protocol server over stdio, so hosts like Claude Code and
Cursor can call codeq natively instead of shelling out. It has no async runtime and bridges
each tool call to the CLI in its working directory.

`codeq install` wires it up for you: it detects which agents are installed (Claude Code,
Cursor, Windsurf, Gemini) and adds the codeq server to each one's MCP config, merging into
whatever is already there. Use `--dry-run` to preview, `--uninstall` to remove, and
`--agent <name>` to target one specifically.

Or register it by hand:

```json
{
  "mcpServers": {
    "codeq": { "command": "codeq", "args": ["mcp"] }
  }
}
```

Each query command above is exposed as a `codeq_*` tool that returns codeq's JSON as the
result. Start with `codeq_map` to orient.

## Output contract

The default output is compact grep-style text; `--json` gives the envelope.

**Text.** One result per line, `path:line:col: payload`, and nothing else. The rules,
which apply to every command:

- **No decoration.** No box drawing, dividers, banners, colour, or arrow glyphs. The
  only structural marker is `# <key>` above each list when a result has several
  (`about`, `map`, `affected`); a single-list result has no header at all.
- **ASCII only.** Any range codeq prints is written in the syntax codeq accepts
  (`44-64`, not `44–64`), so output is a copy-pasteable next command.
- **No prose.** No explanation of how results were ordered or produced. Ordering
  guarantees live in `--help`; provenance lives in `--json`.
- **A field earns its place.** A column that is the same on every row (or derivable
  from the path) is omitted; booleans are emitted only in the informative case
  (`indexed=false` shows up, `indexed=true` does not).
- **A flag must change the text output.** Anything that only shows up under
  `--json` is a no-op from the default's point of view. A doc comment rides on its
  definition's row as `  // <doc>`, so `--docs` visibly does something.
- **Signatures are declaration headers**, not bodies: `pub struct Edge`, never the
  field list.
- **One row is one line.** Multi-line facts are collapsed to a single space.
  Verbatim source (`read`, `explore`, `context`) is the one exception and is always
  introduced by a locator line.
- **Indentation only means depth**, two spaces per hop, relative to the shallowest
  row shown.
- **Stdout is pure data.** Index/staleness notes go to stderr, so a pipe stays clean.
- **An empty result explains itself.** "No results" is ambiguous between no such
  symbol, no such relation, and a filter that removed everything, so codeq says
  which, with a near-miss suggestion when the name looks mistyped. The note goes to
  stderr in text mode and into the envelope's `note` field under `--json` (the MCP
  server captures only stdout). It is diagnostic, never data: `data` stays empty and
  the exit status stays 0.

**JSON.** Every result is `{"schema": "codeq.result.v1", "data": ...}`. Facts carry byte
ranges, 1-based line and column ranges, an extraction source (e.g. `treesitter/rust`),
and a confidence.

codeq refreshes on read, so answers are almost always fresh and the metadata envelope is
omitted unless it carries signal. Pass `--meta` to always include the `repo` and `index`
blocks; they also appear on their own when something is noteworthy, like files being
reparsed or a parse partially failing.

## Reading a dependency

A dependency tree is gitignored, so it stays out of the repo's own index — but that
also made "how do I call this library function" unanswerable. `--root` points codeq
at any directory and takes it literally: no walking up to a Git toplevel, and no
consulting the parent repository's ignore rules.

```
$ codeq --root node_modules/lodash api
$ codeq --root node_modules/lodash read chunk.js --symbol chunk
```

Every command works there, and the index lives in a `.codeq` inside that directory.

## How it works

- Daemonless. Every command refreshes the index first: unchanged files (same size and
  mtime) are skipped, changed files are reparsed. There is no background process.
- SQLite store. Files, definitions, and imports (with ranges, signatures, docstrings) live
  in `.codeq/index.db` in WAL mode, so one invocation can refresh while others read.
- Tree-sitter extraction. Symbols and imports come from Tree-sitter queries, so a definition
  is distinguished from a mention. Import strings are kept raw; codeq does not resolve
  aliases or package roots.
- Single-file components (Svelte, Vue) are component files, not flat source files, so they
  are read that way: each `<script>` block is re-parsed with the JavaScript or TypeScript
  extractor (per its `lang` attribute) and reported at the *file's* own lines and columns,
  the markup contributes what it declares (Svelte's `{#snippet}`), and the file itself
  becomes a `component` definition named after its stem — so `explore Button` returns the
  component and everything it uses. Template expressions count as code, so `callers` sees a
  handler bound by `@click="handleClick"` or `on:click={handleClick}`. SvelteKit route files
  (`+page.svelte`) get no component fact; they are loaded by convention, never by name.
  `<style>` contents stay unclassified, so CSS comments read as code to `search --in`.
- Index-backed search. File contents sit in an FTS5 trigram index. A query is narrowed by
  the index, then confirmed with a precise regex, so search never re-reads the filesystem
  and never drops a match grep would find.

## Development

```bash
cargo test                     # integration tests, driving the binary against fixtures
python3 bench/textmode.py      # text output carries the same facts as --json
python3 bench/differential.py  # assertions over pinned real repos
```

The tests cover the JSON contract and adversarial inputs (malformed source, symlink cycles,
binary, unicode, and large files, non-git repos, deleted-file cascades), assert that `map`
is byte-for-byte reproducible, and check the standing invariant that search always returns a
superset of grep.

`cargo test` uses small synthetic fixtures, so the two harnesses are part of the loop rather
than an extra: `differential.py` runs the same invariants over 20 pinned open-source repos
spanning every supported language, and writes each run to `bench/results/` so the trend lives
in git.
