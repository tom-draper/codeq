# codeq Agent Notes

This file records product reasoning for future AI agents working on `codeq`. Treat [README.md](./README.md) as the public overview, [DRAFT.md](./DRAFT.md) as the working product spec, and [GEMINI.md](./GEMINI.md) as the critical review.

## North Star

`codeq` should be the single source of truth for repository intelligence.

The product should let agents get exactly the repository facts and code context they need, precisely and extremely quickly, while minimizing:

- prompt tokens
- model calls
- duplicated repository scans
- irrelevant file reads
- wall-clock latency
- agent operating cost

The goal is not simply to search faster. The goal is to help agents understand repositories faster than they currently do with `find`, `rg`, `git`, `cat`, and ad hoc shell pipelines.

## Why Shell Tools Are Not Enough

`find`, `rg`, `git`, and `cat` are excellent tools, but agents use them as low-level primitives with no persistent repository model.

Current disadvantages:

- Repeated discovery: agents rerun the same searches every task.
- No memory: facts discovered in one step are not persisted as repository intelligence.
- Unstructured output: agents parse terminal text instead of stable data.
- Weak precision: text matches do not distinguish definitions, references, comments, imports, tests, or docs.
- No relationship graph: shell pipelines do not naturally answer importers, affected tests, route handlers, or changed symbols.
- Token waste: agents often read whole files after vague matches.
- Brittle pipelines: `find | rg | xargs | sed | awk` chains are platform-sensitive and easy to get subtly wrong.
- No freshness contract: agents cannot easily know whether their view of the repo is current after edits.

`codeq` should improve on shell tools by making repository knowledge persistent, structured, queryable, precise, and fresh.

## Product Differentiators

`codeq` should win on:

- deterministic facts, not AI guesses
- indexed queries, not repeated scans
- JSON/JSONL contracts, not terminal scraping
- syntax-aware chunks, not arbitrary file slices
- symbol/import/test relationships, not plain text matches
- explicit index freshness, not hidden stale state
- token-budgeted context retrieval, not whole-file dumping
- confidence-scored project facts for framework-specific extraction

If a command only wraps a trivial `rg`, it probably does not deserve to exist.

## Agent-First Query Examples

Prefer precise questions:

```bash
codeq changed --compare-to main --symbols --json
codeq definitions authenticate --json
codeq importers src/auth/session.ts --json
codeq tests --for src/auth/session.ts --json
codeq context --symbols authenticate,Session --include definitions,importers,tests --max-tokens 12000 --json
codeq routes --handler authenticate --json
codeq status --json
```

Avoid vague commands unless they compile into an explicit query plan:

```bash
codeq understand
codeq analyze
codeq magic
```

Agents need predictable tools with inspectable scope.

## Context Retrieval Requirements

`codeq context` may become the most valuable command.

It should return the smallest useful context for a task, with:

- syntax-aware chunks
- byte ranges
- line and column ranges
- token estimates
- provenance for every chunk
- relevance ordering
- omitted-result metadata when token limits exclude likely useful chunks
- controls for changed-only, path prefix, symbol, imports, importers, tests, and max tokens

The output should let an agent ask for adjacent or next-best context without rereading broad files.

## Freshness Requirements

Every structured response should make freshness explicit:

- repository root
- VCS type
- current commit when available
- dirty state when available
- index schema version
- whether the index is fresh
- files reindexed for the query
- partial failures
- elapsed time

Freshness is part of the product contract. Agents should not have to guess whether answers reflect the current workspace.

## v1 Bias

Keep v1 narrow and excellent.

Must-have shape:

- daemonless-first indexing
- ignore-aware file inventory
- Git-aware changed files
- stable JSON output envelope
- stable exit codes
- symbols and definitions for a small language set
- imports/importers where reliable
- syntax-aware chunks
- token-budgeted context
- benchmark harness that measures time and token savings

Defer:

- full call graphs
- type-aware references
- implementations
- duplicate detection
- vector embeddings
- LSP integration
- daemon as the primary mode
- broad plugin ecosystem

## Architecture Bias

Prefer:

- Rust CLI
- SQLite index
- Tree-sitter parsers
- declarative query packs
- project-local query packs under `.codeq/queries/`
- bundled parsers for common languages
- daemonless freshness checks

Be careful with framework facts. Routes, SQL, env vars, migrations, and tests are often framework-specific. Expose extraction source and confidence rather than pretending every fact is equally certain.

## Success Metrics

`codeq` is winning when an agent using it:

- reads fewer files
- sends fewer prompt tokens
- performs fewer tool calls
- reaches relevant code faster
- makes fewer plans based on stale or noisy search results
- chooses `codeq` over shell pipelines for repository intelligence

Measure replacement rate, token savings, and time-to-understanding, not feature count.

## Failure Modes To Remember

This project can fail even if the implementation is technically good.

Important risks:

- Agents may keep using universal shell tools if `codeq` is only marginally better. It must produce a clear, measurable win over `rg`, `find`, `git`, and `cat`.
- "Single source of truth" may overpromise if facts are partial, inferred, stale, framework-specific, or unsupported. The tool must distinguish known, inferred, partial, unsupported, and stale facts.
- Framework extraction can become an endless maintenance burden. Routes, SQL, env vars, tests, migrations, and configs vary heavily by ecosystem and project style.
- Import resolution can quietly turn into a full language and build-system problem. Syntactic imports are easy; resolved imports require TypeScript paths, Python package roots, Go modules, Rust features, Java classpaths, and monorepo aliases.
- Context ranking may return plausible but incomplete context, causing agents to miss important files. `codeq context` must expose query plans, omitted candidates, and next-best chunks.
- Speed targets can undermine correctness if freshness checks, partial failures, and changed files are hidden. Fast stale answers are worse than slower honest answers.
- Index storage can annoy users if lifecycle, cleanup, worktrees, read-only repos, and `.gitignore` behavior are not designed carefully.
- CLI-only integration may not be enough for every agent platform long term. The CLI should come first, but the internals should not block MCP, library, or service interfaces later.
- Stable schemas can either lock in bad early choices or churn enough to break consumers. Version schemas, mark experimental commands, and add compatibility tests.
- Demos may look good while task-level agent outcomes do not improve. Do not optimize for impressive commands; optimize for better completed coding tasks.

Mitigations to preserve:

- Measure adoption value with task-level benchmarks, not command count.
- Keep v1 narrow: files, changed paths, symbols, definitions, imports, importers, chunks, and context.
- Make freshness and confidence explicit in every structured response.
- Separate raw syntactic facts from resolved semantic facts.
- Keep framework intelligence in query packs, not hardcoded core logic.
- Prefer explicit query constraints over vague "understand this repo" magic.
- Include omitted context metadata so agents know when the answer is incomplete.
- Treat JSON schema compatibility as part of the test suite.

The project should prove value with task-level measurements: fewer tool calls, fewer tokens, less wall time, fewer files read, and equal or better final patch correctness.
