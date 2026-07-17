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
> TypeScript/TSX, Go, Java, Ruby, C, C++, C#, and PHP. Other languages are indexed as files
> and stay fully searchable. See [DRAFT.md](./DRAFT.md) for the design rationale, and
> [GEMINI.md](./GEMINI.md) / [GPT.md](./GPT.md) for the reviews that shaped it.

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
  alone, no bodies, so an agent grasps the shape for a fraction of the tokens.
- Computed once, kept fresh. Each file is parsed once; only changed files (by size and
  mtime) are reparsed. The cost is amortized across a session of many queries against a
  mostly stable tree, which is the agent workload. For a single one-off text search a cold
  `rg` will often win, and that is fine: codeq is not trying to replace it there.

search itself is a strict superset of grep. It uses the same regex engine as ripgrep (the
Rust `regex` crate), adds the flags you would reach for `rg` to get (`-A`/`-B`/`-C`, `-l`,
`-c`, `-w`, `-g`), and does one thing grep cannot: `--in code|comments|strings` restricts
matches by syntactic class, so you can find a TODO only in comments, or a literal only in
real code.

The honest boundary: codeq's facts are syntactic, not deeply semantic. It knows an
identifier is a function definition. It does not resolve a call to its definition across
module aliases, generics, or build systems. The graph commands match by name, with opt-in
heuristics (`--resolved`, `--calls`, `--on`) to cut false positives. Half-right resolution
would produce the untrustworthy results that send an agent back to grep, so codeq stops at
facts it can stand behind.

## Commands

Run any command inside a repo. Default output is compact text (`path:line:col: payload`,
grep style); add `--json` for the structured envelope.

Orient and read:
- `codeq map` groups counts (languages, directories, imports, definitions) to get your bearings.
- `codeq read <file>` returns the file with long bodies folded to signatures (see below).
- `codeq outline <file>` lists definitions top to bottom with nesting.
- `codeq api <path>` shows only the exported definitions of a file or directory, with signatures.

Find and understand a symbol:
- `codeq definitions <name>` gives the definition(s), with signature, doc, and ranges.
- `codeq about <name>` is a one-call dossier: definition plus callers plus callees.
- `codeq at <path>:<line>` names the definition enclosing a location, e.g. from a stack trace.

Trace relationships (matched by name):
- `codeq callers <name>` and `codeq callees <name>` show who calls it and what it calls.
  `--depth N` walks the tree; `callers --examples` prints the actual call line at each site.
- `codeq references <name>` lists every use, each tagged with its enclosing definition.
- `codeq path <from> <to>` finds the shortest call chain between two symbols.
- `codeq implementors <type>` and `codeq supertypes <type>` show who implements or extends a
  type, and what it derives from.
- `codeq tests <name>` finds the tests that exercise a symbol.
- `codeq unused [path]` lists definitions with no reference anywhere (candidate dead code).

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
- `codeq files`, `codeq symbols`, and `codeq stats` list and count.
- `codeq capabilities` self-describes: commands, languages, fact kinds, schema.

Cross-cutting flags: `--path <substr>` scopes a name-based lookup to matching files,
`--fields a,b` trims output to the fields you want, and `--json` switches to the structured
envelope.

## read, a folding file reader

`codeq read <file>` is built for a token budget. It returns the file with long function
bodies folded to their signature plus a marker with the hidden line range, so you see the
whole structure (imports, signatures, docstrings) cheaply, then open only what you need:

```
  20  impl Repo {
  23      pub fn discover(start: &Path) -> Result<Repo> {
          ⋮  8 lines (24-31)
  33      pub fn vcs(&self) -> &'static str {
          ⋮  6 lines (34-39)
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

Every result is `{"schema": "codeq.result.v1", "data": ...}`. Facts carry byte ranges,
1-based line and column ranges, an extraction source (e.g. `treesitter/rust`), and a
confidence.

codeq refreshes on read, so answers are almost always fresh and the metadata envelope is
omitted unless it carries signal. Pass `--meta` to always include the `repo` and `index`
blocks; they also appear on their own when something is noteworthy, like files being
reparsed or a parse partially failing.

## How it works

- Daemonless. Every command refreshes the index first: unchanged files (same size and
  mtime) are skipped, changed files are reparsed. There is no background process.
- SQLite store. Files, definitions, and imports (with ranges, signatures, docstrings) live
  in `.codeq/index.db` in WAL mode, so one invocation can refresh while others read.
- Tree-sitter extraction. Symbols and imports come from Tree-sitter queries, so a definition
  is distinguished from a mention. Import strings are kept raw; codeq does not resolve
  aliases or package roots.
- Index-backed search. File contents sit in an FTS5 trigram index. A query is narrowed by
  the index, then confirmed with a precise regex, so search never re-reads the filesystem
  and never drops a match grep would find.

## Development

```bash
cargo test    # 145 integration tests drive the binary against fixture repos
```

The tests cover the JSON contract and adversarial inputs (malformed source, symlink cycles,
binary, unicode, and large files, non-git repos, deleted-file cascades), assert that `map`
is byte-for-byte reproducible, and check the standing invariant that search always returns a
superset of grep.
