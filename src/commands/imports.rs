//! Module dependencies: `imports`, `importers`, `deps`.

use super::*;

#[derive(Serialize)]
pub(super) struct ImportRow {
    /// The file that contains the import.
    pub(super) path: String,
    /// The imported module, as written in source (raw / unresolved).
    pub(super) module: String,
    #[serde(rename = "line")]
    pub(super) start_line: i64,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub(super) language: Option<String>,
}

/// `imports [path]` — list import statements, optionally scoped to importing
/// files whose path matches. Modules are raw syntactic strings; codeq does not
/// resolve path aliases, package roots, or module systems in v1.
pub fn cmd_imports(cwd: &Path, path: Option<&str>, limit: usize, out: Out) -> Result<()> {
    let s = Session::open(cwd, false, out)?;

    let filters = Filters::new().like("path", path);
    let mut sql = String::from(
        "SELECT path, name, start_line, language
         FROM facts WHERE kind = 'import'
           AND (is_derived = 0 OR language = 'python')",
    );
    sql.push_str(filters.sql());
    sql.push_str(" ORDER BY path, start_line");
    // One past the limit, so a full page can be told from a page that happens to
    // end exactly on it. `imports` defaults to 200 and a real repo had 1,030;
    // the answer read like the whole set and said nothing.
    if limit > 0 {
        sql.push_str(&format!(" LIMIT {}", limit + 1));
    }

    let mut stmt = s.db.conn.prepare(&sql)?;
    let rows = stmt.query_map(params_with(&[], &filters).as_slice(), |r| {
        Ok(ImportRow {
            path: r.get(0)?,
            module: r.get(1)?,
            start_line: r.get(2)?,
            language: r.get(3)?,
        })
    })?;
    let mut data: Vec<ImportRow> = rows.collect::<std::result::Result<_, _>>()?;
    let more = limit > 0 && data.len() > limit;
    if more {
        data.truncate(limit);
    }
    let note = if data.is_empty() {
        note_if_scope_empty(&s, path, "any import")?
    } else if more {
        Some(format!(
            "showing the first {limit} — stopped at --limit; rerun with a higher --limit (or 0) for the rest"
        ))
    } else {
        None
    };
    s.emit_noted(data, note)
}

/// `importers <query>` — files that import a module matching `query`. A module
/// token (`db`, `db::Store`) matches verbatim; a file-path query (`src/db.rs`) is
/// reduced to its module stem first (see `importer_needle`) so it mirrors
/// `imports <file>`.
///
/// The needle must stand as a whole name in the import as written, not inside a
/// longer one: `sr` is in `./sr` but not in `../src`, `Buffer` is in `okio.Buffer`
/// but not in `okio.BufferedSource`. And when the query names one indexed file,
/// an import the dependency graph resolves to some *other* file is not an
/// importer of it: `crate::de::Deserialize` is `serde/src/de`, not
/// `serde/src/private/de.rs`. An import that resolves to no file in the repo is
/// kept, since the graph is a heuristic and a miss there must not hide a real
/// importer.
pub fn cmd_importers(cwd: &Path, query: &str, limit: usize, out: Out) -> Result<()> {
    let s = Session::open(cwd, false, out)?;

    // The LIKE is a cheap, case-insensitive prefilter; the whole-name rule and the
    // limit apply after it, so the limit counts rows that survive.
    let needle = importer_needle(query);
    let pattern = format!("%{needle}%");
    let mut stmt = s.db.conn.prepare(
        "SELECT path, name, start_line, language, NULLIF(signature, ''), is_derived
         FROM facts WHERE kind = 'import' AND name LIKE ?1
         ORDER BY path, start_line",
    )?;
    let rows = stmt.query_map([&pattern], |r| {
        Ok((
            ImportRow {
                path: r.get(0)?,
                module: r.get(1)?,
                start_line: r.get(2)?,
                language: r.get(3)?,
            },
            r.get::<_, Option<String>>(4)?,
            r.get::<_, bool>(5)?,
        ))
    })?;
    let lowered = needle.to_ascii_lowercase();
    let mut found: Vec<(ImportRow, Option<String>, bool)> = Vec::new();
    for row in rows {
        let row = row?;
        if names_module(&row.0.module, &lowered) {
            found.push(row);
        }
    }

    // Only a query that is one indexed file can be told apart from a lookalike.
    // Each import resolves on its own, so a graph over just these candidates has
    // the edges the whole-repo graph would give them, without resolving every
    // other import in the repository.
    let files = all_file_paths(&s.db.conn)?;
    if let (false, Some(target)) = (found.is_empty(), indexed_file(&files, query)) {
        let graph = deps::Graph::build(
            files,
            found.iter().map(|(row, binding, derived)| {
                (
                    row.path.clone(),
                    row.module.clone(),
                    binding.clone(),
                    *derived,
                )
            }),
            go_module_path(&s.db).as_deref(),
        );
        found.retain(|(row, _, _)| {
            let to = graph.targets(&row.path, &row.module);
            to.is_empty() || to.contains(&target.as_str())
        });
    }
    let mut data: Vec<ImportRow> = found.into_iter().map(|(row, _, _)| row).collect();
    if limit > 0 {
        data.truncate(limit);
    }
    s.emit(data)
}

/// The one indexed file `query` names: the path itself, or a path it is a
/// trailing part of (`db.rs` for `src/db.rs`). `None` when it matches no file or
/// several, which a plain module token such as `db::Store` does.
fn indexed_file(paths: &[String], query: &str) -> Option<String> {
    let q = query.trim_start_matches("./");
    if q.is_empty() {
        return None;
    }
    if paths.iter().any(|p| p == q) {
        return Some(q.to_string());
    }
    let suffix = format!("/{q}");
    let mut hits = paths.iter().filter(|p| p.ends_with(&suffix));
    match (hits.next(), hits.next()) {
        (Some(p), None) => Some(p.clone()),
        _ => None,
    }
}

/// Whether `needle` (already ASCII-lowercased) occurs in `module` as a whole
/// name: the characters beside it, where it starts or ends with a name
/// character, are not themselves part of a name. `/`, `.`, `:`, quotes and the
/// ends of the string separate names; `_`, `-`, `$` and letters or digits do not.
fn names_module(module: &str, needle: &str) -> bool {
    let is_name = |b: u8| b.is_ascii_alphanumeric() || matches!(b, b'_' | b'-' | b'$') || b >= 0x80;
    let Some(first) = needle.bytes().next() else {
        return false;
    };
    let last = needle.as_bytes()[needle.len() - 1];
    let hay = module.to_ascii_lowercase();
    let bytes = hay.as_bytes();
    let mut from = 0;
    while let Some(i) = hay[from..].find(needle) {
        let at = from + i;
        let end = at + needle.len();
        let left = !is_name(first) || at == 0 || !is_name(bytes[at - 1]);
        let right = !is_name(last) || end == bytes.len() || !is_name(bytes[end]);
        if left && right {
            return true;
        }
        from = at + hay[at..].chars().next().map_or(1, char::len_utf8);
    }
    false
}

#[derive(Serialize)]
struct DepRow {
    /// The depended-upon file.
    path: String,
    /// The raw import string that produced this edge (for verification).
    module: String,
    /// Hops from the queried file (1 = direct).
    depth: usize,
}

#[derive(Serialize)]
struct CycleRow {
    /// Number of files in the cycle.
    length: usize,
    /// The files in dependency order; the last depends back on the first.
    cycle: Vec<String>,
}

/// Load the internal import graph: every indexed file, and every import fact as
/// an `(importing_file, raw_module, local_binding, is_derived)` tuple, resolved
/// to file→file edges. Derived Python from-imports may append an imported name to
/// their module string. Bindings are optional metadata for alias-aware calls.
pub(crate) fn load_dep_graph(db: &Db) -> Result<deps::Graph> {
    let files: Vec<String> = { all_file_paths(&db.conn)? };
    let imports: Vec<(String, String, Option<String>, bool)> = {
        let mut stmt = db.conn.prepare(
            "SELECT path, name, NULLIF(signature, ''), is_derived FROM facts WHERE kind = 'import'",
        )?;
        let rows = stmt.query_map([], |r| {
            Ok((
                r.get::<_, String>(0)?,
                r.get::<_, String>(1)?,
                r.get::<_, Option<String>>(2)?,
                r.get::<_, bool>(3)?,
            ))
        })?;
        rows.collect::<std::result::Result<_, _>>()?
    };
    Ok(deps::Graph::build(
        files,
        imports,
        go_module_path(db).as_deref(),
    ))
}

/// The module path declared in the repository's `go.mod`, if it has one.
///
/// A Go import names a package directory relative to the module path, not a
/// file stem. Keep this small piece of build metadata separate from the syntax
/// facts so dependency resolution can use exact internal Go edges without
/// becoming a general build-system resolver.
fn go_module_path(db: &Db) -> Option<String> {
    db.dir
        .parent()
        .and_then(|root| std::fs::read_to_string(root.join("go.mod")).ok())
        .and_then(|body| {
            body.lines().find_map(|line| {
                line.trim()
                    .strip_prefix("module ")
                    .and_then(|rest| rest.split_whitespace().next())
                    .map(str::to_string)
            })
        })
}

/// `deps <file> [--depth N]` — the internal files a file depends on, resolved
/// from its imports; `--cycles` reports every import cycle in the repo instead.
/// An import that resolved to no file in the repo.
#[derive(Serialize)]
struct ExternalRow {
    module: String,
    importers: usize,
    /// An example importing file, so the claim is checkable.
    path: String,
    line: i64,
    /// True when the module names a path that should have existed: a relative
    /// reference (`./guide.md`, `../src/x`) or any markdown link. Those are
    /// broken references, not third-party dependencies.
    #[serde(skip_serializing_if = "is_false")]
    broken: bool,
}

pub fn cmd_deps(
    cwd: &Path,
    file: Option<&str>,
    depth: usize,
    cycles: bool,
    external: bool,
    out: Out,
) -> Result<()> {
    let s = Session::open(cwd, false, out)?;
    let graph = s.dep_graph()?;

    // Imports that resolve to nothing inside the repo. For code that is the
    // third-party dependency list; for a markdown link, whose target is always a
    // local file, it is a broken reference.
    if external {
        let mut stmt = s.db.conn.prepare(
            "SELECT f.path, f.name, f.start_line, files.language
             FROM facts f JOIN files ON files.path = f.path
             WHERE f.kind = 'import' ORDER BY f.path, f.start_line",
        )?;
        let rows: Vec<(String, String, i64, Option<String>)> = stmt
            .query_map([], |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?, r.get(3)?)))?
            .collect::<std::result::Result<_, _>>()?;

        let mut by_module: BTreeMap<String, ExternalRow> = BTreeMap::new();
        for (path, module, line, lang) in rows {
            if graph.resolves(&path, &module) {
                continue;
            }
            let looks_like_a_path = module.starts_with("./")
                || module.starts_with("../")
                || lang.as_deref() == Some("markdown");
            let entry = by_module.entry(module.clone()).or_insert(ExternalRow {
                module,
                importers: 0,
                path,
                line,
                broken: looks_like_a_path,
            });
            entry.importers += 1;
        }
        let mut data: Vec<ExternalRow> = by_module.into_values().collect();
        // Broken references first: they are defects, third-party imports are not.
        data.sort_by(|a, b| {
            b.broken
                .cmp(&a.broken)
                .then(b.importers.cmp(&a.importers))
                .then(a.module.cmp(&b.module))
        });
        let note = data
            .is_empty()
            .then(|| "every import resolves to a file in this repo".to_string());
        return s.emit_noted(data, note);
    }

    if cycles {
        let data: Vec<CycleRow> = graph
            .cycles()
            .into_iter()
            .map(|c| CycleRow {
                length: c.len(),
                cycle: c,
            })
            .collect();
        return s.emit(data);
    }

    let file = file.ok_or_else(|| anyhow!("deps needs a <file> (or --cycles)"))?;
    // Resolve the query to an indexed file path: exact, else unique suffix match.
    let target = resolve_dep_target(&graph, &s, file)?;
    let hops = graph.deps(&target, depth.max(1));
    let data: Vec<DepRow> = hops
        .into_iter()
        .map(|h| DepRow {
            path: h.path,
            module: h.module,
            depth: h.depth,
        })
        .collect();
    s.emit(data)
}

/// Map a user-supplied file query to an indexed path: use it verbatim if it is
/// a node, else match indexed paths that end with the query (a unique suffix
/// wins; ambiguity is an error listing the candidates).
fn resolve_dep_target(graph: &deps::Graph, s: &Session, query: &str) -> Result<String> {
    if graph.contains(query) {
        return Ok(query.to_string());
    }
    let paths = all_file_paths(&s.db.conn)?;
    let q = query.trim_start_matches("./");
    let matches: Vec<String> = paths
        .into_iter()
        .filter(|p| p == q || p.ends_with(&format!("/{q}")))
        .collect();
    match matches.len() {
        1 => Ok(matches.into_iter().next().unwrap()),
        0 => Err(anyhow!("no indexed file matches '{query}'")),
        _ => Err(anyhow!(
            "'{query}' is ambiguous ({} files match); use a longer path",
            matches.len()
        )),
    }
}

/// The substring to match import module strings against. A path-shaped query
/// (contains `/` or `\`) is reduced to its file stem so `importers src/db.rs`
/// matches `crate::db` / `./db`; a plain module token is returned unchanged so
/// dotted modules like `foo.bar` keep working.
fn importer_needle(query: &str) -> &str {
    if query.contains('/') || query.contains('\\') {
        Path::new(query)
            .file_stem()
            .and_then(|s| s.to_str())
            .filter(|s| !s.is_empty())
            .unwrap_or(query)
    } else {
        query
    }
}
