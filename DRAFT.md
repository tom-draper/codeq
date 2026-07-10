# codeq

> A persistent, language-aware code intelligence engine for AI agents.

## Mission

Modern AI coding assistants repeatedly perform the same expensive work:

- Walking repositories
- Reading thousands of files
- Respecting `.gitignore`
- Parsing source code
- Searching for symbols
- Finding imports
- Building prompts
- Counting tokens

Every prompt starts almost from scratch.

**codeq exists to eliminate this repeated work.**

Instead of asking an AI model to rediscover information about a repository over and over, codeq continuously indexes the repository and exposes deterministic, millisecond-latency queries that any AI agent can call.

It should become for AI agents what `ripgrep` became for text search.

---

# Philosophy

LLMs should perform reasoning.

Computers should perform computation.

Anything that can be answered deterministically should never consume model tokens.

Good examples:

- Which files import this module?
- Which endpoints exist?
- Which files changed?
- Which functions are exported?
- Which tests reference this file?
- Which SQL queries exist?
- Which environment variables are used?

These are not AI problems.

They are indexing problems.

---

# Goals

## Primary Goal

Provide deterministic answers to repository questions in under 20ms.

---

## Secondary Goals

- Zero configuration
- One static binary
- Extremely low memory usage
- Incremental indexing
- Cross-platform
- Language agnostic
- AI-first CLI
- Useful without AI

---

# Non-goals

This project is NOT:

- an IDE
- an LSP
- another editor
- another AI assistant
- another code generator

We are infrastructure.

---

# Design Principles

## 1. Persistent

The repository should be indexed once.

Never repeatedly scan the repository if nothing changed.

---

## 2. Incremental

Changing one file should only require reparsing one file.

Never rebuild the entire index.

---

## 3. Deterministic

The same query should always return the same result.

No AI.

No guessing.

No probabilistic output.

---

## 4. Fast

Speed is the feature.

Every design decision should optimize for:

- startup
- indexing
- querying
- memory usage

---

## 5. AI-native

Every command should be designed around questions AI agents naturally ask.

Example:

Instead of

```
rg Authenticate
```

the agent should call

```
codeq symbols Authenticate
```

Instead of

```
find . -name '*.sql'
```

```
codeq sql
```

Instead of

```
rg TODO
```

```
codeq todos
```

---

# Initial Features

## Repository

```
codeq files
```

```
codeq changed
```

```
codeq tree
```

```
codeq stats
```

---

## Symbols

```
codeq symbols
```

```
codeq definitions Foo
```

```
codeq exports
```

---

## Imports

```
codeq imports
```

```
codeq dependencies
```

---

## Project Facts

```
codeq env
```

Lists environment variables.

```
codeq routes
```

Lists API endpoints.

```
codeq sql
```

Lists SQL queries.

```
codeq docker
```

Lists Dockerfiles.

```
codeq migrations
```

Lists database migrations.

```
codeq configs
```

Lists configuration files.

```
codeq tests
```

Lists test files.

---

## Search

```
codeq files auth
```

```
codeq symbols authenticate
```

```
codeq paths user
```

---

# Stretch Goals

Eventually:

```
codeq callers Foo
```

```
codeq references Foo
```

```
codeq implementations Interface
```

```
codeq duplicates
```

```
codeq chunk --tokens 12000
```

These require deeper semantic analysis and are intentionally out of scope for v1.

---

# Architecture

```
Filesystem

        │

        ▼

File Watcher

        │

        ▼

Tree-sitter Parser

        │

        ▼

Extract Facts

        │

        ▼

Persistent Index

        │

        ▼

CLI Queries
```

Only changed files should ever be reparsed.

---

# Success Criteria

A user should be able to clone a repository and immediately run

```
codeq index
```

After that, every command should feel instant.

---

# Performance Targets

Repository indexing:

- Linux kernel (<90 seconds)
- Kubernetes (<20 seconds)
- Large monorepo (<30 seconds)

Incremental update:

- <100ms

Simple query:

- <10ms

Complex query:

- <20ms

Memory:

- <200MB on very large repositories

Cold start:

- <100ms

---

# Testing

Every feature should have both correctness tests and performance tests.

Example:

```
tests/

    correctness/

    benchmarks/

    integration/

    stress/

    regression/
```

---

## Correctness

Verify:

- ignored files ignored
- symlinks handled
- Unicode paths
- huge files
- binary files
- nested repositories
- git worktrees
- malformed source code
- generated code

---

## Performance

Continuously benchmark against:

- ripgrep
- fd
- tree-sitter
- find
- git

Measure:

- startup
- indexing
- incremental updates
- query latency
- peak memory

---

# Repository Benchmarks

Always test against real projects.

Examples:

- Linux kernel
- Kubernetes
- Rust
- LLVM
- VSCode
- Home Assistant
- React
- TypeScript
- PostgreSQL

Synthetic benchmarks are insufficient.

---

# How We Know We're Winning

Not by feature count.

By replacing shell pipelines.

Instead of:

```
find

↓

rg

↓

cat

↓

grep

↓

sed

↓

awk

↓

python
```

an AI agent should naturally prefer:

```
codeq ...
```

because it is:

- faster
- deterministic
- simpler
- richer

---

# Guiding Question

Before adding any feature, ask:

> "Is this something an AI agent repeatedly computes today?"

If the answer is no,

don't build it.

If the answer is yes,

ask:

> "Can we answer it deterministically in under 20ms?"

If not,

don't build it yet.

---

# Long-term Vision

The long-term goal is for codeq to become a standard dependency of AI coding agents.

Today:

```
Agent

↓

find

↓

rg

↓

git

↓

cat

↓

tree
```

Tomorrow:

```
Agent

↓

codeq
```

A single, persistent source of truth for repository intelligence.

The fastest way to understand a codebase shouldn't be reading files—it should be querying an index.
