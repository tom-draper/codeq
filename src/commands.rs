//! Command implementations for the v1 proving spine:
//! `index`, `files`, `changed`, `stats`.

use std::collections::BTreeMap;
use std::path::Path;
use std::time::Instant;

use anyhow::Result;
use serde::Serialize;

use crate::context::{self, Request};
use crate::db::{Db, SCHEMA_VERSION};
use crate::envelope::{Envelope, IndexMeta, Render, RepoMeta, RESULT_SCHEMA};
use crate::index::{language_for, refresh};
use crate::repo::Repo;

/// Output rendering options parsed from global CLI flags.
#[derive(Clone, Default)]
pub struct Out {
    pub meta: bool,
    pub pretty: bool,
    pub fields: Option<Vec<String>>,
}

/// Shared setup: discover the repo, open the index, and bring it fresh.
/// Returns the pieces every command needs plus how many files were reparsed.
struct Session {
    repo: Repo,
    db: Db,
    reindexed: u64,
    partial_failures: u64,
    started: Instant,
    out: Out,
}

impl Session {
    fn open(cwd: &Path, rebuild: bool, out: Out) -> Result<Session> {
        let started = Instant::now();
        let repo = Repo::discover(cwd)?;
        let db = Db::open(&repo.root)?;
        if rebuild {
            db.conn.execute("DELETE FROM files", [])?;
        }
        let stats = refresh(&db, &repo.root)?;
        Ok(Session {
            repo,
            db,
            reindexed: stats.reindexed,
            partial_failures: stats.partial_failures,
            started,
            out,
        })
    }

    /// Build the envelope and print it honoring the caller's render options.
    fn emit<T: Serialize>(&self, data: T) -> Result<()> {
        let repo = RepoMeta {
            root: self.repo.root.to_string_lossy().into_owned(),
            vcs: self.repo.vcs(),
            head: self.repo.head(),
            dirty: self.repo.dirty(),
        };
        let index = IndexMeta {
            schema: SCHEMA_VERSION,
            fresh: true, // we just refreshed before answering
            reindexed_files: self.reindexed,
            partial_failures: self.partial_failures,
            elapsed_ms: self.started.elapsed().as_millis() as u64,
        };
        Envelope::new(repo, index, data).print(Render {
            meta: self.out.meta,
            pretty: self.out.pretty,
            fields: self.out.fields.as_deref(),
        })
    }
}

#[derive(Serialize)]
struct IndexReport {
    indexed_files: i64,
    reindexed: u64,
}

pub fn cmd_index(cwd: &Path, rebuild: bool, out: Out) -> Result<()> {
    let s = Session::open(cwd, rebuild, out)?;
    let total = s.db.file_count()?;
    let data = IndexReport {
        indexed_files: total,
        reindexed: s.reindexed,
    };
    s.emit(data)
}

#[derive(Serialize)]
struct FileRow {
    path: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    language: Option<String>,
    size_bytes: i64,
}

pub fn cmd_files(
    cwd: &Path,
    pattern: Option<&str>,
    language: Option<&str>,
    limit: usize,
    out: Out,
) -> Result<()> {
    let s = Session::open(cwd, false, out)?;

    let mut sql = String::from(
        "SELECT path, language, size_bytes FROM files WHERE 1=1",
    );
    let mut args: Vec<String> = Vec::new();
    if let Some(pat) = pattern {
        sql.push_str(" AND path LIKE ?");
        args.push(format!("%{pat}%"));
    }
    if let Some(lang) = language {
        sql.push_str(" AND language = ?");
        args.push(lang.to_string());
    }
    sql.push_str(" ORDER BY path");
    if limit > 0 {
        sql.push_str(&format!(" LIMIT {limit}"));
    }

    let mut stmt = s.db.conn.prepare(&sql)?;
    let param_refs: Vec<&dyn rusqlite::ToSql> =
        args.iter().map(|a| a as &dyn rusqlite::ToSql).collect();
    let rows = stmt.query_map(param_refs.as_slice(), |r| {
        Ok(FileRow {
            path: r.get(0)?,
            language: r.get(1)?,
            size_bytes: r.get(2)?,
        })
    })?;
    let data: Vec<FileRow> = rows.collect::<std::result::Result<_, _>>()?;
    s.emit(data)
}

#[derive(Serialize)]
struct ChangedRow {
    path: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    language: Option<String>,
    /// Whether this path is present in the current index (i.e. not ignored /
    /// still exists in the tree).
    indexed: bool,
}

pub fn cmd_changed(cwd: &Path, compare_to: &str, limit: usize, out: Out) -> Result<()> {
    let s = Session::open(cwd, false, out)?;
    let mut paths = s.repo.changed_since(compare_to)?;
    if limit > 0 && paths.len() > limit {
        paths.truncate(limit);
    }

    let mut data = Vec::with_capacity(paths.len());
    for p in paths {
        let indexed: bool = s
            .db
            .conn
            .query_row(
                "SELECT 1 FROM files WHERE path = ?1",
                [&p],
                |_| Ok(true),
            )
            .unwrap_or(false);
        let language = language_for(&p).map(|l| l.to_string());
        data.push(ChangedRow {
            path: p,
            language,
            indexed,
        });
    }
    s.emit(data)
}

#[derive(Serialize)]
struct SymbolRow {
    name: String,
    kind: String,
    path: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    language: Option<String>,
    start_line: i64,
    end_line: i64,
    /// Only populated when the caller passes --signatures.
    #[serde(skip_serializing_if = "Option::is_none")]
    signature: Option<String>,
}

pub fn cmd_symbols(
    cwd: &Path,
    pattern: Option<&str>,
    kind: Option<&str>,
    language: Option<&str>,
    path: Option<&str>,
    signatures: bool,
    limit: usize,
    out: Out,
) -> Result<()> {
    let s = Session::open(cwd, false, out)?;

    let mut sql = String::from(
        "SELECT name, kind, path, language, start_line, end_line, signature
         FROM facts WHERE name IS NOT NULL AND kind <> 'import'",
    );
    let mut args: Vec<String> = Vec::new();
    if let Some(p) = pattern {
        sql.push_str(" AND name LIKE ?");
        args.push(format!("%{p}%"));
    }
    if let Some(k) = kind {
        sql.push_str(" AND kind = ?");
        args.push(k.to_string());
    }
    if let Some(l) = language {
        sql.push_str(" AND language = ?");
        args.push(l.to_string());
    }
    if let Some(p) = path {
        sql.push_str(" AND path LIKE ?");
        args.push(format!("%{p}%"));
    }
    // Deterministic ordering: name, then location.
    sql.push_str(" ORDER BY name, path, start_line");
    if limit > 0 {
        sql.push_str(&format!(" LIMIT {limit}"));
    }

    let mut stmt = s.db.conn.prepare(&sql)?;
    let param_refs: Vec<&dyn rusqlite::ToSql> =
        args.iter().map(|a| a as &dyn rusqlite::ToSql).collect();
    let rows = stmt.query_map(param_refs.as_slice(), |r| {
        Ok(SymbolRow {
            name: r.get(0)?,
            kind: r.get(1)?,
            path: r.get(2)?,
            language: r.get(3)?,
            start_line: r.get(4)?,
            end_line: r.get(5)?,
            signature: if signatures { r.get(6)? } else { None },
        })
    })?;
    let data: Vec<SymbolRow> = rows.collect::<std::result::Result<_, _>>()?;
    s.emit(data)
}

#[derive(Serialize)]
struct DefinitionRow {
    name: String,
    kind: String,
    path: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    language: Option<String>,
    start_line: i64,
    start_col: i64,
    end_line: i64,
    end_col: i64,
    start_byte: i64,
    end_byte: i64,
    #[serde(skip_serializing_if = "Option::is_none")]
    signature: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    doc: Option<String>,
}

pub fn cmd_definitions(cwd: &Path, symbol: &str, limit: usize, out: Out) -> Result<()> {
    let s = Session::open(cwd, false, out)?;

    let mut sql = String::from(
        "SELECT name, kind, path, language,
                start_line, start_col, end_line, end_col, start_byte, end_byte,
                signature, doc
         FROM facts WHERE name = ?1 AND kind <> 'import'
         ORDER BY path, start_line",
    );
    if limit > 0 {
        sql.push_str(&format!(" LIMIT {limit}"));
    }

    let mut stmt = s.db.conn.prepare(&sql)?;
    let rows = stmt.query_map([symbol], |r| {
        Ok(DefinitionRow {
            name: r.get(0)?,
            kind: r.get(1)?,
            path: r.get(2)?,
            language: r.get(3)?,
            start_line: r.get(4)?,
            start_col: r.get(5)?,
            end_line: r.get(6)?,
            end_col: r.get(7)?,
            start_byte: r.get(8)?,
            end_byte: r.get(9)?,
            signature: r.get(10)?,
            doc: r.get(11)?,
        })
    })?;
    let data: Vec<DefinitionRow> = rows.collect::<std::result::Result<_, _>>()?;
    s.emit(data)
}

#[derive(Serialize)]
struct OutlineRow {
    name: String,
    kind: String,
    /// Nesting level derived from byte containment (methods inside a class, …).
    depth: usize,
    start_line: i64,
    end_line: i64,
    #[serde(skip_serializing_if = "Option::is_none")]
    signature: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    doc: Option<String>,
}

struct OutlineDef {
    name: String,
    kind: String,
    start_byte: i64,
    end_byte: i64,
    start_line: i64,
    end_line: i64,
    signature: Option<String>,
    doc: Option<String>,
}

/// `outline <file>` — the structure of a file top to bottom: every definition
/// in source order, with its signature and a nesting depth so containment
/// (methods within a class, nested functions) is visible.
pub fn cmd_outline(cwd: &Path, file: &str, docs: bool, out: Out) -> Result<()> {
    let s = Session::open(cwd, false, out)?;

    let mut stmt = s.db.conn.prepare(
        "SELECT name, kind, start_byte, end_byte, start_line, end_line, signature, doc
         FROM facts
         WHERE kind <> 'import' AND (path = ?1 OR path LIKE ?2)
         ORDER BY start_byte, end_byte DESC",
    )?;
    let suffix = format!("%/{file}");
    let defs: Vec<OutlineDef> = stmt
        .query_map((file, &suffix), |r| {
            Ok(OutlineDef {
                name: r.get(0)?,
                kind: r.get(1)?,
                start_byte: r.get(2)?,
                end_byte: r.get(3)?,
                start_line: r.get(4)?,
                end_line: r.get(5)?,
                signature: r.get(6)?,
                doc: r.get(7)?,
            })
        })?
        .collect::<std::result::Result<_, _>>()?;

    // Depth = number of other defs whose byte range strictly encloses this one.
    let data: Vec<OutlineRow> = defs
        .iter()
        .map(|d| {
            let depth = defs
                .iter()
                .filter(|o| {
                    !std::ptr::eq(*o, d)
                        && o.start_byte <= d.start_byte
                        && o.end_byte >= d.end_byte
                        && (o.start_byte != d.start_byte || o.end_byte != d.end_byte)
                })
                .count();
            OutlineRow {
                name: d.name.clone(),
                kind: d.kind.clone(),
                depth,
                start_line: d.start_line,
                end_line: d.end_line,
                signature: d.signature.clone(),
                doc: if docs { d.doc.clone() } else { None },
            }
        })
        .collect();

    s.emit(data)
}

#[derive(Serialize)]
struct ImportRow {
    /// The file that contains the import.
    path: String,
    /// The imported module, as written in source (raw / unresolved).
    module: String,
    start_line: i64,
    #[serde(skip_serializing_if = "Option::is_none")]
    language: Option<String>,
}

/// `imports [path]` — list import statements, optionally scoped to importing
/// files whose path matches. Modules are raw syntactic strings; codeq does not
/// resolve path aliases, package roots, or module systems in v1.
pub fn cmd_imports(cwd: &Path, path: Option<&str>, limit: usize, out: Out) -> Result<()> {
    let s = Session::open(cwd, false, out)?;

    let mut sql = String::from(
        "SELECT path, name, start_line, language
         FROM facts WHERE kind = 'import'",
    );
    let mut args: Vec<String> = Vec::new();
    if let Some(p) = path {
        sql.push_str(" AND path LIKE ?");
        args.push(format!("%{p}%"));
    }
    sql.push_str(" ORDER BY path, start_line");
    if limit > 0 {
        sql.push_str(&format!(" LIMIT {limit}"));
    }

    let mut stmt = s.db.conn.prepare(&sql)?;
    let param_refs: Vec<&dyn rusqlite::ToSql> =
        args.iter().map(|a| a as &dyn rusqlite::ToSql).collect();
    let rows = stmt.query_map(param_refs.as_slice(), |r| {
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

/// `importers <query>` — files that import a module matching `query`. This is a
/// raw string match against the import as written; it deliberately does not
/// resolve `query` (a file path) to the module spelling used at the import site.
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

    let pattern = format!("%{query}%");
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

pub fn cmd_context(cwd: &Path, req: Request, out: Out) -> Result<()> {
    let s = Session::open(cwd, false, out)?;
    let result = context::build(&s.db, &s.repo, &req)?;
    s.emit(result)
}

#[derive(Serialize)]
struct Stats {
    files: i64,
    bytes: i64,
    by_language: BTreeMap<String, i64>,
}

pub fn cmd_stats(cwd: &Path, out: Out) -> Result<()> {
    let s = Session::open(cwd, false, out)?;
    let files: i64 = s.db.file_count()?;
    let bytes: i64 = s
        .db
        .conn
        .query_row("SELECT COALESCE(SUM(size_bytes),0) FROM files", [], |r| {
            r.get(0)
        })?;

    let mut by_language = BTreeMap::new();
    let mut stmt = s.db.conn.prepare(
        "SELECT COALESCE(language,'(unknown)') AS lang, COUNT(*)
         FROM files GROUP BY lang ORDER BY lang",
    )?;
    let rows = stmt.query_map([], |r| {
        Ok((r.get::<_, String>(0)?, r.get::<_, i64>(1)?))
    })?;
    for row in rows {
        let (lang, count) = row?;
        by_language.insert(lang, count);
    }

    let data = Stats {
        files,
        bytes,
        by_language,
    };
    s.emit(data)
}

/// Self-description so an agent can discover what codeq supports without
/// guessing. Deliberately does not require (or refresh) an index.
pub fn cmd_capabilities(out: Out) -> Result<()> {
    let data = serde_json::json!({
        "result_schema": RESULT_SCHEMA,
        "index_schema": SCHEMA_VERSION,
        "commands": [
            "index", "files", "changed", "stats",
            "symbols", "definitions", "outline", "imports", "importers",
            "context", "capabilities",
        ],
        // Languages with symbol + import extraction.
        "extraction_languages": ["rust", "python"],
        // Languages recognised for file classification (extraction may be absent).
        "classified_languages": [
            "rust", "javascript", "typescript", "tsx", "python", "go", "java",
            "json", "yaml", "toml", "markdown", "sql", "shell", "c", "cpp", "ruby",
        ],
        "fact_kinds": [
            "function", "struct", "enum", "union", "trait", "module",
            "constant", "typealias", "macro", "class", "import",
        ],
        "notes": {
            "imports": "raw syntactic module strings; not resolved",
            "context_tokenizer": "approx:4-bytes-per-token",
            "freshness": "every command refreshes changed files before answering",
        },
    });
    let value = serde_json::json!({ "schema": RESULT_SCHEMA, "data": data });
    if out.pretty {
        println!("{}", serde_json::to_string_pretty(&value)?);
    } else {
        println!("{}", serde_json::to_string(&value)?);
    }
    Ok(())
}
