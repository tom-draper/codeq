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
