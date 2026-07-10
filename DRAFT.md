# codeq

> A persistent, deterministic single source of truth for repository intelligence, built for AI agents.

## Mission

AI coding agents repeatedly do the same expensive work:

- walk repositories
- respect ignore files
- inspect changed files
- search symbols
- infer imports and exports
- locate routes, tests, configs, migrations, SQL, and environment variables
- build context under a token budget

That repeated work costs latency, tokens, and money. It also makes agents less reliable because every run reconstructs a partial understanding of the repository from scratch.

Most of that work is computation, not reasoning. It should not consume model tokens every time an agent starts a task.

`codeq` exists to index repository facts once, keep them fresh, and expose precise structured queries that agents can call instead of rediscovering the same information through shell pipelines.

The product promise is that an agent should be able to ask for exactly the repository intelligence it needs, receive only the relevant facts or code chunks, and spend fewer model tokens reaching a correct plan.

The ambition is simple: for repository intelligence, `codeq` should become the single source of truth that lets agents understand a codebase faster than reading and searching files manually.

## Core Thesis

LLMs should reason over relevant context.

Computers should compute deterministic repository facts.

Good `codeq` questions:

- Which files changed since `main`?
- Which symbols are defined in this package?
- Which files import `auth/session`?
- Which tests reference this module?
- Which routes are declared in this service?
- Which SQL strings exist in this repository?
- Which environment variables are read?
- Which chunks fit in a 12k-token context budget?
- What is the smallest set of files and symbols needed to answer this task?

These are not open-ended language-model tasks. They are indexing, parsing, and query tasks.

## Product Principle

Before adding a feature, ask three questions:

1. Does an AI coding agent repeatedly compute this today?
2. Can `codeq` answer it deterministically, precisely, and faster than the shell workflow it replaces?
3. Does the answer reduce token usage or prevent the agent from reading irrelevant files?

If any answer is no, do not build it yet.

## What codeq Is

`codeq` is:

- a local repository index
- a CLI designed for agents
- a deterministic fact database
- a language-aware parser pipeline
- a precise context retrieval layer
- a source of structured JSON/JSONL outputs
- a faster alternative to repeated `find | rg | cat | grep | sed | awk` workflows

`codeq` is not:

- an IDE
- an LSP replacement
- an AI assistant
- a code generator
- a vector database first
- a whole-program semantic analyzer in v1

## Critical Design Corrections

The original product idea is strong, but several details need sharper boundaries.

### 1. Daemonless First

A required background daemon is a poor fit for many AI-agent environments. Agents often run in short-lived sandboxes, CI jobs, containers, or restricted workspaces where background services are fragile.

`codeq` should be daemonless-first:

- every query may perform a cheap freshness check
- unchanged repositories return immediately
- changed files are incrementally reparsed before answering
- `codeq watch` can exist later as an optimization, not a requirement

This keeps the tool reliable for one-off automated invocations.

### 2. Framework Facts Must Be Declarative

Facts like routes, SQL queries, migrations, tests, and environment variables are not universal AST concepts.

Examples:

- Express routes use calls such as `app.get("/users", handler)`.
- FastAPI routes often use decorators such as `@app.get("/users")`.
- Rails routes may live in a DSL file.
- SQL may be raw strings, query builders, migration files, or generated code.

Hardcoding every framework into the core binary will not scale. The core should provide:

- language parsers
- Tree-sitter query execution
- a normalized fact schema
- bundled query packs for common ecosystems
- project-local query packs for teams and agents to extend

Framework intelligence should live in declarative query packs where possible, not in opaque core logic.

### 3. Agent Output Is a Contract

Human-readable terminal output is useful, but agents need stable machine-readable data. Every command should support JSON, and streaming or large-result commands should support JSONL.

Output schemas should be versioned and documented. A fast tool with unstable output is hard for agents to trust.

Precision matters as much as parseability. Commands should return bounded, relevant results by default and expose enough metadata for agents to decide whether more context is needed.

### 4. Index Freshness Must Be Explicit

Agents need to know whether an answer reflects the current workspace. Each structured response should include index metadata:

- repository root
- index schema version
- indexed commit, when available
- dirty state, when available
- freshness status
- files reindexed for this query

This avoids false confidence when a repository changes under the agent.

### 5. v1 Must Avoid Semantic Overreach

Call graphs, references, implementations, duplicate detection, and type-aware resolution are valuable, but they are not cheap v1 features. They require language-specific semantics, build-system knowledge, type resolution, or runtime framework understanding.

v1 should win by being reliable and fast on syntactic and filesystem facts.

## v1 Goals

The first useful version should optimize for a narrow but dependable agent workflow:

- install one binary
- run inside an arbitrary Git repository
- build or refresh an index
- answer common repository questions with structured output
- return the smallest useful context for a task
- reduce prompt tokens and model calls
- avoid reparsing unchanged files
- stay useful even without a persistent daemon

### Performance Targets

Targets should be measured on warm and cold runs.

| Operation | Target |
| --- | ---: |
| Cold CLI startup | <100ms |
| No-op freshness check | <20ms |
| Simple indexed query | <10ms |
| Complex indexed query | <20ms |
| Incremental parse of a small edited file | <100ms |
| Peak memory on large repositories | <200MB |

Initial full-index targets should be treated as benchmark goals, not promises:

| Repository | Target |
| --- | ---: |
| Kubernetes | <20s |
| Linux kernel | <90s |
| large monorepo | <30s after ignore filtering |

## v1 Commands

The CLI should be small enough that agents can discover and use it reliably.

### Repository

```bash
codeq index
codeq files [query] --json
codeq tree --json
codeq stats --json
codeq changed --compare-to main --json
```

### Symbols

```bash
codeq symbols [query] --json
codeq definitions <symbol> --json
codeq exports --json
```

### Imports and Dependencies

```bash
codeq imports [path] --json
codeq importers <module-or-path> --json
codeq dependencies --json
```

### Project Facts

```bash
codeq env --json
codeq routes --json
codeq sql --json
codeq tests --json
codeq configs --json
codeq docker --json
codeq migrations --json
```

These commands should expose confidence and extraction source. For example, a route found by a bundled Express query is stronger than a route guessed from a string literal.

### Context Construction

```bash
codeq chunk <path> --max-tokens 12000 --json
codeq context --changed --compare-to main --max-tokens 12000 --json
```

Context construction is central to the product. Agents do not only need search results; they need bounded, relevant text that fits the current model budget. Chunks should respect syntax boundaries where possible and include enough provenance for the agent to request adjacent context only when needed.

Good context output should minimize:

- irrelevant files
- duplicate code
- oversized chunks
- arbitrary line slicing
- repeated model calls for follow-up lookup

## Output Contract

Every JSON response should have a predictable envelope:

```json
{
  "schema": "codeq.result.v1",
  "repo": {
    "root": "/workspace/project",
    "vcs": "git",
    "head": "abc123",
    "dirty": true
  },
  "index": {
    "schema": 1,
    "fresh": true,
    "reindexed_files": 1,
    "elapsed_ms": 8
  },
  "data": []
}
```

JSONL commands may emit one fact per line, but the schema still needs a documented version.

## Fact Model

The index should normalize facts without pretending all languages are the same.

Minimum useful entities:

- `files`
- `symbols`
- `definitions`
- `imports`
- `exports`
- `references` only where syntactically reliable
- `project_facts` for routes, SQL, env vars, configs, tests, migrations, Dockerfiles
- `chunks`

Each fact should include:

- stable id
- kind
- path
- byte range
- line and column range
- language
- extraction source
- confidence
- indexed timestamp or content hash
- enough location data to fetch the exact surrounding code without rereading the file blindly

## Architecture

```text
Filesystem / Git
      |
      v
Change Detector
      |
      v
Parser Registry
      |
      v
Tree-sitter AST + Query Packs
      |
      v
Fact Extractor
      |
      v
SQLite Index
      |
      v
CLI / JSON / JSONL
```

### Change Detection

Use layered change detection:

1. Respect `.gitignore`, `.ignore`, and common generated/vendor directories.
2. Use Git metadata when available.
3. Store file size, mtime, and content hash.
4. Hash only when cheaper metadata indicates a possible change.
5. Reparse only changed files.

Git should be an optimization, not a hard requirement. Non-Git repositories should still work.

### Parser Strategy

Use Tree-sitter for syntax-aware indexing.

Recommended v1 parser set:

- JavaScript
- TypeScript / TSX
- Python
- Go
- Rust
- Java
- JSON
- YAML
- TOML
- Markdown
- SQL where practical

Parser distribution is a real product decision. A reasonable path:

- bundle common parsers in the binary
- keep query packs as data files or embedded assets
- allow optional external parsers or query packs later
- avoid requiring users to compile parsers for v1

### Query Packs

Query packs should define how facts are extracted for a language or framework.

Example pack types:

- generic TypeScript symbols
- Node Express routes
- Python FastAPI routes
- Rust module imports
- Go test files
- SQL strings in common host languages

Project-local packs could live under:

```text
.codeq/queries/
```

This lets teams teach `codeq` local conventions without changing the core binary.

### Storage

SQLite is the preferred default index store:

- embedded
- cross-platform
- inspectable
- transactional
- fast enough for indexed repository facts
- supports FTS5 for text lookup

Default location:

```text
.codeq/index.db
```

The index directory should be safe to delete and should normally be ignored by Git.

## Suggested Schema Sketch

```sql
CREATE TABLE files (
  path TEXT PRIMARY KEY,
  language TEXT,
  size_bytes INTEGER NOT NULL,
  mtime_ns INTEGER,
  content_hash TEXT,
  ignored INTEGER NOT NULL DEFAULT 0,
  indexed_at INTEGER NOT NULL
);

CREATE TABLE facts (
  id TEXT PRIMARY KEY,
  kind TEXT NOT NULL,
  name TEXT,
  path TEXT NOT NULL,
  language TEXT,
  start_byte INTEGER NOT NULL,
  end_byte INTEGER NOT NULL,
  start_line INTEGER NOT NULL,
  start_col INTEGER NOT NULL,
  end_line INTEGER NOT NULL,
  end_col INTEGER NOT NULL,
  source TEXT NOT NULL,
  confidence REAL NOT NULL,
  payload_json TEXT,
  FOREIGN KEY(path) REFERENCES files(path)
);

CREATE INDEX facts_kind_name ON facts(kind, name);
CREATE INDEX facts_path ON facts(path);
```

This is intentionally generic. Specialized tables can be added when query performance or schema clarity demands them.

## Agent UX Requirements

Agents should be able to use `codeq` without brittle prompt gymnastics.

Requirements:

- `--json` on every command
- `--jsonl` for large streams
- stable non-zero exit codes
- clear `--help` examples
- no interactive prompts in default commands
- bounded output by default, with `--limit` and pagination
- `--max-tokens` for context-producing commands
- predictable relevance ordering for context-producing commands
- documented schema versions
- deterministic sorting unless explicitly ranked

Useful agent affordances:

```bash
codeq explain-command symbols
codeq capabilities --json
codeq schema --json
```

These let agents discover supported languages, fact kinds, and output schemas.

## Testing Strategy

Every feature needs correctness and performance tests.

Correctness cases:

- ignored files are ignored
- symlinks do not create cycles
- Unicode paths work
- binary files are skipped safely
- huge files are bounded
- malformed source does not crash indexing
- nested repositories are handled deliberately
- Git worktrees work
- generated/vendor files can be excluded
- partial framework patterns report lower confidence

Performance cases:

- cold startup
- no-op query
- initial indexing
- incremental indexing
- query latency
- peak memory
- large JSON/JSONL output streaming

Benchmark against:

- `ripgrep`
- `fd`
- `git`
- `find`
- direct Tree-sitter parsing

Real repositories matter more than synthetic fixtures:

- Linux kernel
- Kubernetes
- Rust
- LLVM
- VSCode
- Home Assistant
- React
- TypeScript
- PostgreSQL

## Roadmap

### Milestone 1: Useful Core

- Rust CLI
- SQLite index
- ignore-aware file inventory
- Git-aware changed files
- JSON envelope
- freshness checks
- tests for filesystem edge cases

### Milestone 2: Symbols and Imports

- Tree-sitter parser registry
- bundled parsers for 3 to 5 languages
- symbol extraction
- definitions and exports
- imports and importers
- schema documentation

### Milestone 3: Agent Context

- syntax-aware chunking
- token counting
- changed-symbol context
- `codeq context` command
- relevance and budget controls
- JSONL streaming for large outputs

### Milestone 4: Project Facts

- declarative query packs
- env var extraction
- tests/configs/docker/migrations
- initial route extraction for a few common frameworks
- SQL extraction with confidence reporting

## Later Work

These are valuable, but should not distract from v1:

```bash
codeq callers <symbol>
codeq references <symbol>
codeq implementations <interface>
codeq duplicates
codeq impact <path>
codeq query "kind = 'function' AND path LIKE 'src/auth/%'"
```

Potential future directions:

- type-aware analysis for selected languages
- optional LSP integration
- optional watch daemon
- plugin API
- remote cache
- vector embeddings as an optional layer over deterministic facts

## How We Know We Are Winning

Not by feature count.

By replacement rate, token savings, and time-to-understanding.

If an AI agent naturally chooses:

```bash
codeq changed --compare-to main --symbols --json
codeq definitions authenticate --json
codeq context --changed --max-tokens 12000 --json
```

instead of:

```bash
find . | rg ... | xargs cat | sed ... | awk ...
```

then `codeq` is doing its job.

The fastest and cheapest way for an agent to understand a codebase should be querying a fresh deterministic index, not repeatedly rereading the repository from scratch.
