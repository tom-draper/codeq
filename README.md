# codeq

`codeq` is a deterministic index of repository facts, built for AI coding agents.

Agents waste tokens, time, and money rediscovering things a computer can compute
exactly: file lists, symbols, imports, changed functions, a file's structure. `codeq`
indexes those facts once and answers precise, structured queries so an agent can
understand a repository without re-deriving it through `find`, `rg`, `git`, and `cat`
on every task.

Its guiding rule: **codeq reports facts, it does not guess.** Results are deterministic
and reproducible. There is no relevance ranking, no "did you mean", no intent inference.
Where a list is ordered by a count, the count is in every row so the ordering is
transparent. An agent that can trust the output has no reason to fall back to ad-hoc
shell pipelines.

> Status: working prototype. Symbol/import extraction currently covers **Rust** and
> **Python**; other languages are indexed as files only. See [DRAFT.md](./DRAFT.md) for
> the design rationale and [GEMINI.md](./GEMINI.md) / [GPT.md](./GPT.md) for the critical
> reviews that shaped it.

## Why not just `grep` and `find`?

`grep` and `find` operate on **bytes and lines**. `codeq` operates on a **parsed,
persistent model of the code**. That difference buys five things shell tools cannot give:

- **Structure.** Shell tools match text with no idea what it *is*. `grep authenticate`
  returns the definition, every call, comment mentions, and substrings all mixed together.
  `codeq definitions authenticate` returns *the definition*, with signature and doc.
  `codeq search` goes further: every match is annotated with the definition that encloses
  it — grep tells you `file:625`, codeq tells you you're inside `fn cmd_search`.

- **Relationships.** `importers` answers "who imports this?", `outline` gives a file's
  structure and nesting, `map` aggregates imports and definitions across the whole repo.
  None of these are reachable from a single byte-level scan.

- **Understand without reading.** `outline` and `definitions --signatures` let an agent
  grasp an API surface from signatures and docstrings alone — no function bodies, so far
  fewer tokens. `grep` can't strip a body; it doesn't know where the body is.

- **Compute once, keep fresh.** `codeq` parses each file once into SQLite; queries hit an
  index, and only changed files (by size + mtime) are re-parsed. Parsing is the expensive
  part `grep` never does — so the win is amortized across a *session* of many structured
  queries against a mostly-stable tree, which is exactly the agent workload. (For a single
  one-off text search, `ripgrep` will often beat a cold index — `codeq` is not trying to
  replace `rg` for that.)

- **A stable contract.** Versioned JSON with byte/line ranges, an extraction `source`, and
  a `confidence` — instead of unstructured text re-parsed with brittle regex every time.

**The honest boundary:** `codeq`'s facts are *syntactic*, not deeply semantic. It knows an
identifier is a function definition; it does **not** resolve a call to its definition across
module aliases, generics, or build systems. Half-right resolution would produce exactly the
untrustworthy results that send an agent back to `grep`, so `codeq` deliberately stops at
facts it can guarantee.

In one line: *grep and find answer "where do these bytes appear." `codeq` answers "what is
defined here, what is it, what's its shape, what depends on it, and give me just enough of
it to fit my budget" — computed once and kept fresh.*

## Install

```bash
cargo build --release
# binary at ./target/release/codeq
```

Requires a Rust toolchain and `git` on PATH. The index is stored at `.codeq/index.db`
under the repo root (git-ignored, safe to delete).

## How it works

- **Daemonless and incremental.** Every command refreshes the index before answering.
  Unchanged files (same size + mtime) are skipped; changed files are reparsed. There is
  no background process to manage.
- **SQLite fact store.** Files and definitions/imports (with byte + line ranges,
  signatures, and docstrings) live in `.codeq/index.db`, in WAL mode so parallel agent
  invocations can read while one refreshes.
- **Tree-sitter extraction.** Symbols and imports come from Tree-sitter queries, so they
  distinguish a definition from a mention. Import strings are **raw** — codeq does not
  resolve path aliases, package roots, or module systems.

## Commands

Run inside a repository. Output is compact JSON on stdout.

| Command | Answers |
| --- | --- |
| `codeq index [--rebuild]` | Build or refresh the index. |
| `codeq files [pattern] [--language L]` | Indexed files, optionally filtered. |
| `codeq changed --compare-to <ref>` | Files changed vs a git ref (merge-base with HEAD) + untracked. |
| `codeq stats` | Counts by language and total bytes. |
| `codeq map [--limit N]` | Factual aggregation for orientation (see below). |
| `codeq symbols [pattern] [--kind K] [--language L] [--path P] [--signatures]` | Defined symbols. |
| `codeq definitions <name>` | Exact-name definition(s) with signature, doc, and precise ranges. |
| `codeq outline <file> [--docs]` | A file's definitions top to bottom, with signatures and nesting depth. |
| `codeq imports [path]` | Import statements (raw module strings). |
| `codeq importers <query>` | Files whose imports match `query` (raw string match). |
| `codeq context --symbols a,b \| --changed [--expand]` | Token-budgeted context assembly (see below). |
| `codeq capabilities` | Self-description: commands, languages, fact kinds, schema. |

### Global options

- `--fields a,b,c` — keep only these fields on each row (pay for what you need).
- `--meta` — include the full `repo`/`index` metadata envelope (see below).
- `--pretty` — pretty-print for humans (implies `--meta`).

## Output contract

Every result is `{"schema": "codeq.result.v1", "data": ...}`. The `data` shape is
per-command (an array for list commands, an object for `map`/`stats`/`context`).

Because codeq refreshes on read, answers are almost always fresh — so the metadata
envelope is **omitted unless it carries signal**. The `repo` and `index` blocks appear
only when you pass `--meta`, or automatically when something is noteworthy (files were
reparsed, or a read/parse partially failed). With `--meta`:

```json
{
  "schema": "codeq.result.v1",
  "repo":  { "root": "/w/project", "vcs": "git", "head": "abc123", "dirty": true },
  "index": { "schema": 1, "fresh": true, "elapsed_ms": 8 },
  "data":  [ ... ]
}
```

Facts carry byte ranges, 1-based line/column ranges, an extraction `source`
(e.g. `treesitter/rust`), and a `confidence`.

## `map`

One call to orient in an unfamiliar repo. Every field is a grouped count from the index —
nothing is a judgement about importance:

- `totals` — files, symbols, imports.
- `languages` — files, bytes, and symbols per language.
- `directories` — files and symbols per top-level directory.
- `manifests` — recognised project files present (`Cargo.toml`, `package.json`, `README*`, …).
- `imports_by_module` — each raw import string and how many files import it.
- `definitions_by_file` — each file and its definition count.

Count-ordered lists include the count in every row and an `ordering` field stating the
sort rule, so the result is transparent and reproducible.

## `context`

Assembles the smallest useful slice of code for a task, under a token budget, from
**explicit seeds** — codeq never infers what you're working on:

- Seed with `--symbols name,name` and/or `--changed [--compare-to <ref>]`.
- Each chunk is a definition's source, with provenance (path, symbol, kind, ranges) and an
  approximate token estimate (`~4 bytes/token`, labeled as approximate).
- The default returns exactly the seeds. `--expand` adds one deterministic hop:
  definitions referenced *by name* inside the seed chunks.
- The result includes a `plan` (what was seeded, how it was ordered) and an `omitted` list
  when the budget truncates, so an agent knows what was left out and can request more.

## Examples

```bash
codeq map
codeq changed --compare-to main --fields path
codeq outline src/auth/session.rs
codeq definitions authenticate
codeq importers auth::session
codeq context --symbols authenticate --expand --max-tokens 8000
```

## Development

```bash
cargo test    # 29 integration tests drive the binary against fixture repos
```

Tests cover the JSON contract and adversarial inputs (malformed source, symlink cycles,
binary/unicode/large files, non-git repos, deleted-file cascade) and assert that `map` is
byte-for-byte reproducible.
