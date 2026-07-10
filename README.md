# codeq

`codeq` is a persistent, deterministic single source of truth for repository intelligence, built for AI coding agents.

AI agents repeatedly spend tokens, wall time, and money rediscovering facts a computer can compute exactly: file lists, symbols, imports, changed functions, routes, configs, tests, SQL strings, and ownership boundaries. `codeq` aims to index those facts once and expose precise, structured queries that return exactly the context an agent needs.

The goal is not to build another AI assistant, editor, LSP, or code generator. The goal is to let agents understand repositories faster and more cheaply than they do today, with a better primitive than repeatedly composing `find`, `rg`, `git`, `cat`, `sed`, and ad hoc scripts.

## Design Target

- Deterministic answers, no model calls.
- Agent-readable output by default, especially JSON and JSONL.
- Precise context retrieval to minimize prompt size, token usage, and agent cost.
- Daemonless-first operation so one-off agent runs work reliably.
- Incremental indexing so unchanged files are not reparsed.
- Language-aware facts through Tree-sitter and declarative queries.
- Useful for humans, optimized for automated agents.

## Example

```bash
codeq index
codeq symbols authenticate --json
codeq definitions UserRepository --json
codeq changed --compare-to main --symbols --jsonl
codeq routes --json
```

## v1 Scope

The first useful version should be deliberately narrow:

- File inventory respecting ignore rules.
- Symbol extraction for a small set of common languages.
- Definitions, exports, imports, and simple dependency edges.
- Changed files and changed symbols relative to Git refs.
- JSON/JSONL output contracts stable enough for agents to consume.
- Basic fact extraction for routes, env vars, configs, tests, and SQL where patterns are reliable.

Features that require whole-program semantic analysis, type resolution, or framework-specific execution should be treated as later work unless they can be answered deterministically from indexed syntax.

## Status

This repository currently contains the product and architecture draft. See [DRAFT.md](./DRAFT.md) for the working specification, [GEMINI.md](./GEMINI.md) for a critical companion review, and [GPT.md](./GPT.md) for agent-facing product notes.
