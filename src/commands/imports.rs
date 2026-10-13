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

/// `importers <query>` — files that import a module matching `query`. This is a
/// raw substring match against the import as written. A module token (`db`,
/// `db::Store`) matches verbatim; a file-path query (`src/db.rs`) is reduced to
/// its module stem first (see `importer_needle`) so it mirrors `imports <file>`.
pub fn cmd_importers(cwd: &Path, query: &str, limit: usize, out: Out) -> Result<()> {
    let s = Session::open(cwd, false, out)?;

    let mut sql = String::from(
        "SELECT path, name, start_line, language
         FROM facts WHERE kind = 'import' AND name LIKE ?1
         ORDER BY path, start_line",
    );
    if limit > 0 {
        sql.push_str(&format!(" LIMIT {limit}"));
    }

    // Accept a file path symmetrically with `imports <file>`: import module
    // strings never contain the on-disk path (Rust `crate::db`, JS `./db`), so
    // a path query like `src/db.rs` would silently match nothing. When the query
    // looks like a path (has a separator), fall back to its module stem — the
    // file's base name without extension — matched as a substring. Still purely
    // textual and a safe superset; a bare module token is used verbatim.
    let needle = importer_needle(query);
    let pattern = format!("%{needle}%");
    let mut stmt = s.db.conn.prepare(&sql)?;
    let rows = stmt.query_map([&pattern], |r| {
        Ok(ImportRow {
            path: r.get(0)?,
            module: r.get(1)?,
            start_line: r.get(2)?,
            language: r.get(3)?,
        })
    })?;
    let data: Vec<ImportRow> = rows.collect::<std::result::Result<_, _>>()?;
    s.emit(data)
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
    // A Go import names a package directory relative to the module path, not a
    // file stem. Keep this small piece of build metadata separate from the
    // syntax facts so dependency resolution can use exact internal Go edges
    // without becoming a general build-system resolver.
    let go_module: Option<String> = db
        .dir
        .parent()
        .and_then(|root| std::fs::read_to_string(root.join("go.mod")).ok())
        .and_then(|body| {
            body.lines().find_map(|line| {
                line.trim()
                    .strip_prefix("module ")
                    .and_then(|rest| rest.split_whitespace().next())
                    .map(str::to_string)
            })
        });
    Ok(deps::Graph::build(files, imports, go_module.as_deref()))
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
