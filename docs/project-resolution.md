# Project import resolution

Codeq uses project configuration to resolve imports to indexed source files.
This supports `deps` and the import-reachability filters on call queries.
It does not run TypeScript or infer runtime types.

## Select the importing project

Indexed `tsconfig.json` and `jsconfig.json` files define projects. TypeScript
configs take precedence over JavaScript configs in the same directory. Project
references discover named configs such as `tsconfig.app.json`, without sharing
the solution project's aliases with every referenced project.

`files`, `include`, and `exclude` select initial project files. Relative imports
and configured aliases bring dependencies into that project's membership even
when they fall outside `include`. Default output-directory exclusion is respected.
Patterns give only `*` and `?` wildcard meaning; braces and brackets are literal.

Explicit `files` membership wins, followed by the deepest applicable project.
Equally specific projects remain ambiguous. These are bounded source-index rules,
not a complete implementation of TypeScript's project service or module resolver.

## Inherit configuration and resolve aliases

Local `extends` accepts one config or an ordered list. Later parents override
earlier ones, and the child overrides parents. A child `paths` map replaces the
inherited map. References are not inherited.

Relative settings retain their declaring directory. Paths use the effective
`baseUrl` when present, including a child's override. Otherwise they use the
directory that declared the paths map. Exact aliases beat wildcard aliases;
wildcards use the longest matching prefix and retain declaration order on ties.
Targets within an alias are tried in order.

Configs accept comments and trailing commas. Missing parents, inheritance cycles,
invalid settings, and unreadable configs produce errors naming the config.
Package-based inheritance and paths outside the repository are unsupported.

The relevant TypeScript rules are documented under
[extends](https://www.typescriptlang.org/tsconfig/extends.html) and
[paths](https://www.typescriptlang.org/tsconfig/paths.html).

## Keep cached results current

Only indexed, repository-local configs are interpreted. Indexed config edits
invalidate graph caches through the index generation, including in persistent
MCP sessions. An ignored ancestor config blocks parent-alias fallback and
produces an unresolved reason. Codeq checks for the creation or removal of those
ignored configs before reusing a graph, without reading their settings.

The graph owns edges. The private project resolver owns configuration, source
membership, and alias interpretation. Keeping those decisions together lets
imported helpers retain their project context without adding policy to commands.
Uncertain ownership stays explicit rather than selecting an arbitrary project.

## Resolve workspace packages

Workspace membership comes from `package.json` workspaces or, when present,
`pnpm-workspace.yaml`. Patterns include nested directories and exclusions starting
with `!`. The root package is always included. YAML is parsed with the existing
tree-sitter grammar; aliases, multiline patterns, and unsupported quoted escapes
produce errors rather than partial membership.

Package `exports` controls root and subpath imports, including wildcard maps and
`null` exclusions. When exports exists, Codeq never falls back to `main` or a
private file path. An export must name an exact indexed file. Conditional exports
resolve only when every potentially selected branch before the default names the
same target. Other conditional exports and export arrays remain unresolved with
a reason. Codeq does not guess runtime conditions or emitted build outputs.

Packages without exports retain the `source`, `module`, `types`, and `main` entry
lookup, followed by conventional source/index files. Duplicate package names
remain unresolved. Workspace manifest edits invalidate persistent MCP graphs.

The supported export rules follow the Node.js documentation for
[package entry points](https://nodejs.org/api/packages.html#package-entry-points).
