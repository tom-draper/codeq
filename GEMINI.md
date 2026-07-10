# codeq Critique and Product Suggestions

This document is a critical companion to [DRAFT.md](./DRAFT.md). The draft describes what `codeq` should become; this file calls out the risks, tradeoffs, and product decisions that need pressure before implementation.

## Executive Critique

The idea is strong because it attacks a real waste pattern: AI agents repeatedly spend tokens, time, and money rediscovering deterministic repository facts. A fast local index can make agents cheaper, faster, and less error-prone.

The main risk is scope creep. If `codeq` tries to answer every "understand this codebase" question, it will drift into being a partial LSP, partial static analyzer, partial framework runtime, partial vector store, and partial AI assistant. That would dilute the project.

The winning version is narrower: a deterministic repository fact index with excellent agent ergonomics and precise context retrieval. `codeq` should help agents ask for exactly what they need, not just search faster.

## Hard Product Questions

### 1. What is the minimum agent workflow that proves value?

The project should avoid starting with a huge command surface. A credible v1 could prove itself with:

- `codeq files`
- `codeq changed`
- `codeq symbols`
- `codeq definitions`
- `codeq imports`
- `codeq context`

If those commands are fast, fresh, structured, and reliable, agents will already have a better primitive than shell pipelines.

The proof should include measurable token reduction: given the same coding task, an agent using `codeq` should read fewer files, send fewer prompt tokens, and reach relevant code faster.

### 2. What makes codeq better than `rg` plus conventions?

Raw speed is not enough. `ripgrep` is already very fast and universally installed in many agent environments.

`codeq` needs to win on:

- structured output
- syntax-aware boundaries
- indexed freshness
- import and symbol relationships
- changed-symbol context
- token-budgeted context retrieval
- stable contracts agents can call repeatedly

If a command only wraps a trivial `rg`, it may not deserve to exist.

### 3. How much framework intelligence belongs in core?

Very little. Routes, SQL, env vars, tests, and migrations are valuable only if the extraction is trustworthy. A wrong route list is worse than no route list because an agent may build a plan on false facts.

Recommendation:

- keep the core generic
- expose confidence and extraction source
- put ecosystem logic in query packs
- allow project-local query packs
- mark weak heuristics as low confidence

### 4. Is "one static binary" compatible with parser breadth?

Only if the parser set is deliberately limited. Bundling every Tree-sitter parser can bloat the binary and complicate releases.

Recommendation:

- bundle a small, high-value parser set first
- measure binary size as a release metric
- defer exotic languages to optional packs
- do not require users to compile parsers for v1

### 5. How does an agent know an answer is fresh?

Freshness must be part of the API, not an implementation detail. Agents need to know whether the index was refreshed, skipped, stale, or partially failed.

Recommendation:

- include freshness metadata in every JSON response
- make partial indexing failures visible
- return stable non-zero exit codes for unusable indexes
- support `codeq status --json`

### 6. What does "exactly what the agent needs" mean?

This phrase is powerful but dangerous. The tool cannot know a task's full intent unless the agent supplies constraints. `codeq` should therefore expose precise controls:

- symbol name
- path prefix
- changed-only scope
- dependency direction
- test-only or source-only scope
- max files
- max tokens
- include definitions/importers/tests flags

The product should avoid vague "understand this repo" commands unless they compile these controls into an explicit, inspectable query plan.

## Architectural Recommendations

### Prefer Daemonless-First

A required daemon creates lifecycle problems in CI, containers, sandboxes, remote workspaces, and short-lived agent sessions. Make each command capable of checking freshness and reparsing changed files.

`codeq watch` can be useful later, but it should be an optimization.

### Treat SQLite as a Product Feature

SQLite is not just convenient storage. It gives the project an inspectable, portable, transactional fact store. That matters for debugging agent behavior.

Suggested principles:

- schema migrations from day one
- documented schema versions
- indexes based on measured queries
- avoid storing derived data that cannot be invalidated clearly

### Build Around Query Packs

Declarative query packs are the escape hatch for framework diversity.

Good query packs should specify:

- supported language
- supported framework or convention
- Tree-sitter queries
- emitted fact kinds
- confidence level
- examples and fixtures

The pack system should be simple before it is powerful.

### Make Context Construction First-Class

Agents do not only need facts; they need context that fits a model window. `codeq context` may become the most valuable command if it can gather changed symbols, definitions, imports, and tests under a token budget.

Avoid naive file slicing. Prefer syntax-aware chunks with byte ranges, line ranges, and token estimates.

The output should make omissions clear. If relevant chunks were excluded because of a token limit, the agent needs to know what was left out and how to ask for the next slice.

## Suggested v1 Product Shape

### Must Have

- daemonless indexing
- ignore-aware file inventory
- Git-aware changed files
- JSON output envelope
- stable exit codes
- symbols and definitions for 3 to 5 languages
- imports/importers where syntactically reliable
- syntax-aware chunks
- token-budgeted context output
- benchmark harness

### Should Have

- JSONL streaming
- `codeq capabilities --json`
- `codeq schema --json`
- index status command
- project-local query packs
- env/config/test fact extraction
- query plans for context commands

### Defer

- full call graph
- type-aware references
- implementations
- duplicate detection
- vector embeddings
- LSP integration
- background daemon as the primary mode
- plugin marketplace

## API Suggestions

Prefer commands that map to agent questions:

```bash
codeq changed --compare-to main --symbols --json
codeq definitions authenticate --json
codeq importers src/auth/session.ts --json
codeq context --changed --compare-to main --max-tokens 12000 --json
codeq status --json
codeq capabilities --json
```

Avoid commands that are too vague:

```bash
codeq analyze
codeq understand
codeq magic
```

Agents need predictable tools, not broad verbs.

## Testing Suggestions

The test suite should include adversarial repositories:

- malformed syntax
- huge generated files
- vendored dependencies
- nested Git repositories
- Git worktrees
- symlink cycles
- Unicode paths
- files changing during indexing
- repositories without Git
- monorepos with multiple package roots

Performance tests should be part of CI, but with thresholds calibrated to CI noise. Keep strict local benchmarks for release validation.

## Open Risks

- Tree-sitter queries vary in quality across languages.
- Import resolution can quickly become language-specific and build-system-specific.
- Query packs may become a compatibility burden.
- Agent consumers will break if JSON schemas change casually.
- Full-index performance targets may be unrealistic without careful ignore defaults.
- Storing `.codeq/index.db` inside the repo may annoy users unless `.gitignore` guidance is automatic.
- Token counting depends on model tokenizer choice, so context commands need an explicit tokenizer option or documented approximation.
- "Repository intelligence" can become too broad unless every feature is tied to token savings, speed, or correctness.

## Reasons This Might Fail

### 1. Agents May Prefer Universal Tools Over a New Dependency

Agents already know `rg`, `find`, `git`, and `cat`. These tools exist almost everywhere, have predictable behavior, and do not require project-specific setup.

`codeq` must therefore be dramatically better, not marginally better. If the win is only "a nicer wrapper around search", agents and agent builders may not adopt it.

The adoption bar is:

- fewer tool calls
- fewer prompt tokens
- faster task completion
- more correct edits
- lower implementation complexity for agent authors

If those are not measured and demonstrated, the project may feel useful but fail to become necessary.

### 2. The "Single Source of Truth" Claim May Be Too Strong

Repository intelligence is not one thing. Some facts are cheap and deterministic, such as files, symbols, imports, and changed paths. Other facts are contextual or semantic, such as ownership, runtime routes, generated code, dependency injection, build outputs, and test coverage.

If `codeq` calls itself the single source of truth but returns incomplete or approximate answers, agents may overtrust it. That is worse than making agents do extra searches.

The product should distinguish:

- known facts
- inferred facts
- partial facts
- unsupported facts
- stale facts

Precision and honesty matter more than breadth.

### 3. Framework Extraction Could Become a Maintenance Trap

Routes, SQL, env vars, tests, migrations, and configs are exactly the kind of facts agents want. They are also where ecosystems differ most.

Failure mode:

- v1 ships a few impressive framework extractors
- users expect broad coverage
- every framework and coding style needs exceptions
- query packs grow brittle
- maintainers spend all their time chasing framework edge cases

Mitigation:

- keep framework packs separate from core
- publish confidence levels
- make unsupported cases explicit
- build fixtures from real repositories
- avoid implying complete coverage

### 4. Import Resolution Can Quietly Become a Full Language Toolchain

Syntactic imports are easy. Resolved imports are hard.

Examples:

- TypeScript path aliases
- Python virtualenv and package roots
- Go modules and replace directives
- Rust crates and feature flags
- Java classpaths
- monorepo workspace aliases

If `codeq importers` gives wrong answers, agents may edit the wrong files or miss affected code. The project should separate raw import facts from resolved dependency facts.

### 5. Context Ranking May Be the Hardest Product Problem

Returning chunks under a token budget is easy. Returning the right chunks is hard.

Ranking may require task intent, symbol relationships, changed files, tests, recency, dependency direction, and sometimes semantic understanding. A deterministic index can provide candidates, but choosing the best context is not always deterministic.

Failure mode:

- `codeq context` returns plausible but incomplete context
- agents trust it
- agents miss important files
- edits regress behavior

Mitigation:

- expose the query plan
- include omitted candidates
- let agents request next-best chunks
- avoid pretending relevance ranking is perfect
- support explicit constraints over magical task inference

### 6. Performance Targets May Conflict With Correctness

Sub-20ms queries are attractive, but correctness may require freshness checks, filesystem metadata reads, Git inspection, schema migrations, or incremental parsing.

If the tool optimizes for benchmark numbers over trustworthy answers, it will fail the agent use case.

The real target should be:

- fast when unchanged
- bounded when changed
- explicit when stale
- honest when partial

### 7. Index Location and Lifecycle May Create Friction

Storing `.codeq/index.db` inside repositories has tradeoffs:

- it may need `.gitignore` updates
- it may be large
- it may confuse cleanup workflows
- it may not work well in read-only checkouts
- it may behave poorly with multiple worktrees

An external cache also has tradeoffs:

- harder to inspect
- harder to map to repo state
- harder to clean
- possible permission issues

This needs an explicit lifecycle design before implementation.

### 8. Agent Integration May Need More Than a CLI

A CLI is universal, but tool-call overhead and JSON parsing may limit adoption in some agent systems. Some agent platforms may prefer MCP, a library API, or a long-running local service.

The CLI should be the first interface, but the internal design should not make other interfaces painful.

### 9. Schema Stability Could Slow Iteration

Agents need stable output contracts, but early product discovery needs rapid schema changes. If schemas stabilize too early, the project may lock in bad abstractions. If they churn too much, agents cannot trust the tool.

Mitigation:

- version every schema
- mark experimental commands clearly
- keep v1 fact types minimal
- add compatibility tests for JSON outputs

### 10. The Project May Optimize for Agent Builders, Not Agent Outcomes

It is easy to build impressive commands that look useful in demos. The hard question is whether agents actually complete tasks better.

The benchmark suite should include task-level evaluations:

- bug fix with and without `codeq`
- feature addition with and without `codeq`
- refactor with and without `codeq`
- test update with and without `codeq`

Measure:

- tool calls
- prompt tokens
- wall time
- files read
- correctness of final patch

Without task-level evidence, the project may become a technically interesting index that does not materially improve agent performance.

## Strong Recommendation

Build the first version as a boring, fast, deterministic index with a small command set and excellent JSON contracts.

Do not chase semantic understanding until `codeq` has proven it can replace repetitive shell work and reduce the context an agent needs to send to the model.
