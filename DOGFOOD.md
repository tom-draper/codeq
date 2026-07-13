# Dogfooding log

Honest friction, limitations, and failures found while using codeq to develop
codeq. New entries at the top. The point is candour — a frustration recorded
here is a feature request or a bug report.

## 2026-07-13 — RESOLVED: symbol commands need path scoping (highest priority)

**Fixed** the same day: `definitions`/`callees`/`about` now take `--path <substr>`
to scope the target definition, and `callers` `--path` scopes caller locations.
`about build --path context` now shows only `context::build` and its callees;
`--path commands` shows the other one. First dogfood-driven fix — the loop works.

Original report:

`codeq about build` matched **two** `build` functions — `GlobFilter::build`
(src/commands.rs) and `context::build` (src/context.rs). `about`/`callers`/
`callees` **aggregate across all same-named symbols**, so the dossier merged two
unrelated functions (callees mixed `normalize`/`GlobFilter` from the wrong one).
For common names (`new`, `build`, `open`) these commands are actively
misleading. `definitions` correctly returns both, but the graph commands don't
disambiguate, and there's no way to scope them.

**Fix:** add `--path <substr>` (and maybe `--file`) to `definitions`, `callers`,
`callees`, `about`; when a name resolves to multiple definitions, either scope
or clearly separate the per-definition graphs. Also consider showing each
callee's path (a bare `new` × 4 is ambiguous).

## 2026-07-13 — read --symbol is a modest but real win

`codeq read src/context.rs --symbol build` returned 7.3k bytes vs 11.1k to read
the whole file (~34%). The win is small here because `build` is most of the
file; it grows when the target is a small part of a large file. Good enough that
reading a specific symbol beats ingesting the file.

## 2026-07-13 — invocation is via Bash, not a native tool

In this session codeq is called through Bash (`./target/debug/codeq …`), not as
a first-class tool, so its output arrives as shell stdout. Registering the MCP
server (`.mcp.json` → `codeq mcp`) would make `codeq_*` native tools in future
sessions — worth doing to make codeq the default reach.
