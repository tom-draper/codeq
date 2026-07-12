//! Command implementations for the v1 proving spine:
//! `index`, `files`, `changed`, `stats`.

use std::collections::{BTreeMap, HashSet};
use std::path::Path;
use std::time::Instant;

use anyhow::{anyhow, Result};
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

#[derive(Serialize)]
struct ChangedSymbolRow {
    name: String,
    kind: String,
    path: String,
    start_line: i64,
    end_line: i64,
}

pub fn cmd_changed(
    cwd: &Path,
    compare_to: &str,
    symbols: bool,
    limit: usize,
    out: Out,
) -> Result<()> {
    let s = Session::open(cwd, false, out)?;

    if symbols {
        return changed_symbols(&s, compare_to, limit);
    }

    let mut paths = s.repo.changed_since(compare_to)?;
    if limit > 0 && paths.len() > limit {
        paths.truncate(limit);
    }

    let mut data = Vec::with_capacity(paths.len());
    for p in paths {
        let indexed: bool = s
            .db
            .conn
            .query_row("SELECT 1 FROM files WHERE path = ?1", [&p], |_| Ok(true))
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

/// Definitions whose line range overlaps a changed hunk — "which functions did
/// this change touch". Deterministic (path, start_line).
fn changed_symbols(s: &Session, compare_to: &str, limit: usize) -> Result<()> {
    let ranges = s.repo.changed_line_ranges(compare_to)?;
    let mut paths: Vec<&String> = ranges.keys().collect();
    paths.sort();

    let mut stmt = s.db.conn.prepare(
        "SELECT name, kind, start_line, end_line FROM facts
         WHERE path = ?1 AND kind <> 'import' ORDER BY start_line",
    )?;

    let mut data: Vec<ChangedSymbolRow> = Vec::new();
    for p in paths {
        let hunks = &ranges[p];
        let defs = stmt.query_map([p], |r| {
            Ok((
                r.get::<_, String>(0)?,
                r.get::<_, String>(1)?,
                r.get::<_, i64>(2)?,
                r.get::<_, i64>(3)?,
            ))
        })?;
        for def in defs {
            let (name, kind, start_line, end_line) = def?;
            // Compare in usize so the untracked whole-file sentinel (usize::MAX)
            // doesn't wrap when cast.
            let ds = start_line.max(0) as usize;
            let de = end_line.max(0) as usize;
            let touched = hunks.iter().any(|&(hs, he)| he >= ds && hs <= de);
            if touched {
                data.push(ChangedSymbolRow {
                    name,
                    kind,
                    path: p.clone(),
                    start_line,
                    end_line,
                });
            }
        }
    }
    if limit > 0 && data.len() > limit {
        data.truncate(limit);
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

/// `at <path>:<line>` — the definition(s) enclosing a location, innermost first.
/// Turns a stack-trace or compiler-error line into "what code is this".
pub fn cmd_at(cwd: &Path, location: &str, out: Out) -> Result<()> {
    let s = Session::open(cwd, false, out)?;
    let (path, line_str) = location
        .rsplit_once(':')
        .ok_or_else(|| anyhow!("expected <path>:<line>, got '{location}'"))?;
    let line: i64 = line_str
        .parse()
        .map_err(|_| anyhow!("invalid line number in '{location}'"))?;
    let suffix = format!("%/{path}");

    let mut stmt = s.db.conn.prepare(
        "SELECT name, kind, path, language,
                start_line, start_col, end_line, end_col, start_byte, end_byte,
                signature, doc
         FROM facts
         WHERE kind <> 'import' AND (path = ?1 OR path LIKE ?2)
           AND start_line <= ?3 AND end_line >= ?3
         ORDER BY (end_byte - start_byte) ASC, path",
    )?;
    let rows = stmt.query_map((path, &suffix, line), |r| {
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

/// One matched line, shared by `search` and `references`.
#[derive(Serialize, Clone)]
struct Hit {
    path: String,
    line: usize,
    col: usize,
    /// The innermost definition enclosing this match, if any.
    #[serde(skip_serializing_if = "Option::is_none")]
    symbol: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    kind: Option<String>,
    /// The matched line, trailing whitespace trimmed and capped.
    text: String,
}

#[derive(Serialize)]
struct SearchResult {
    matches: Vec<Hit>,
    /// True when `--limit` cut the results off — the agent knows to narrow the query.
    truncated: bool,
}

struct FileDef {
    name: String,
    kind: String,
    start_byte: usize,
    end_byte: usize,
}

const MAX_LINE: usize = 240;

/// Scan indexed, non-binary files for `re`, one hit per matching line, each
/// annotated with its enclosing definition. Deterministic order (path, then
/// line). Returns `(hits, truncated)`.
fn scan_files(
    s: &Session,
    re: &regex::Regex,
    path: Option<&str>,
    language: Option<&str>,
    limit: usize,
) -> Result<(Vec<Hit>, bool)> {
    // Restrict the file set with the same filters, to read less from disk.
    let mut sql = String::from("SELECT path FROM files WHERE 1=1");
    let mut args: Vec<String> = Vec::new();
    if let Some(p) = path {
        sql.push_str(" AND path LIKE ?");
        args.push(format!("%{p}%"));
    }
    if let Some(l) = language {
        sql.push_str(" AND language = ?");
        args.push(l.to_string());
    }
    sql.push_str(" ORDER BY path");

    let files: Vec<String> = {
        let mut stmt = s.db.conn.prepare(&sql)?;
        let refs: Vec<&dyn rusqlite::ToSql> =
            args.iter().map(|a| a as &dyn rusqlite::ToSql).collect();
        let rows = stmt.query_map(refs.as_slice(), |r| r.get::<_, String>(0))?;
        rows.collect::<std::result::Result<_, _>>()?
    };

    let cap = if limit == 0 { usize::MAX } else { limit };
    let mut hits: Vec<Hit> = Vec::new();
    let mut truncated = false;

    let mut def_stmt = s.db.conn.prepare(
        "SELECT name, kind, start_byte, end_byte FROM facts
         WHERE path = ?1 AND kind <> 'import'",
    )?;

    'outer: for rel in files {
        let bytes = match std::fs::read(s.repo.root.join(&rel)) {
            Ok(b) => b,
            Err(_) => continue,
        };
        if bytes.iter().take(8192).any(|b| *b == 0) {
            continue; // binary
        }
        let text = String::from_utf8_lossy(&bytes);

        // Line-start offsets for O(log n) line lookup.
        let mut line_starts = vec![0usize];
        for (i, b) in text.as_bytes().iter().enumerate() {
            if *b == b'\n' {
                line_starts.push(i + 1);
            }
        }

        // Definitions in this file, for enclosing-symbol resolution.
        let defs: Vec<FileDef> = def_stmt
            .query_map([&rel], |r| {
                Ok(FileDef {
                    name: r.get(0)?,
                    kind: r.get(1)?,
                    start_byte: r.get::<_, i64>(2)? as usize,
                    end_byte: r.get::<_, i64>(3)? as usize,
                })
            })?
            .collect::<std::result::Result<_, _>>()?;

        let mut last_line = usize::MAX;
        for m in re.find_iter(&text) {
            let off = m.start();
            let line_idx = line_starts.partition_point(|&s| s <= off) - 1;
            if line_idx == last_line {
                continue; // one row per line
            }
            last_line = line_idx;

            let line_start = line_starts[line_idx];
            let line_end = line_starts
                .get(line_idx + 1)
                .map(|&s| s - 1)
                .unwrap_or(text.len());
            let raw = &text[line_start..line_end.min(text.len())];
            let enclosing = innermost(&defs, off);

            hits.push(Hit {
                path: rel.clone(),
                line: line_idx + 1,
                col: off - line_start,
                symbol: enclosing.map(|d| d.name.clone()),
                kind: enclosing.map(|d| d.kind.clone()),
                text: cap_str(raw.trim_end(), MAX_LINE),
            });

            if hits.len() >= cap {
                truncated = true;
                break 'outer;
            }
        }
    }

    Ok((hits, truncated))
}

/// `search <pattern>` — content search over indexed, non-binary files, with each
/// match annotated by its enclosing definition. Regex by default (linear-time,
/// no catastrophic backtracking); `--fixed` for a literal string. This is the
/// factual answer to "where does this text appear, and what is it inside".
#[allow(clippy::too_many_arguments)]
pub fn cmd_search(
    cwd: &Path,
    pattern: &str,
    fixed: bool,
    ignore_case: bool,
    path: Option<&str>,
    language: Option<&str>,
    limit: usize,
    out: Out,
) -> Result<()> {
    let s = Session::open(cwd, false, out)?;

    let expr = if fixed {
        regex::escape(pattern)
    } else {
        pattern.to_string()
    };
    let re = regex::RegexBuilder::new(&expr)
        .case_insensitive(ignore_case)
        .build()
        .map_err(|e| anyhow!("invalid pattern: {e}"))?;

    let (matches, truncated) = scan_files(&s, &re, path, language, limit)?;
    s.emit(SearchResult { matches, truncated })
}

#[derive(Serialize)]
struct ReferenceRow {
    #[serde(flatten)]
    hit: Hit,
    /// True when this line is the definition site of the symbol (not a use).
    #[serde(skip_serializing_if = "is_false")]
    definition: bool,
}

#[derive(Serialize)]
struct ReferencesResult {
    references: Vec<ReferenceRow>,
    truncated: bool,
}

/// `references <name>` — word-boundary occurrences of an exact symbol name,
/// each annotated with its enclosing definition, and definition sites flagged.
/// These are *textual* references (not type-resolved) — honest and deterministic,
/// but far more precise than a substring grep.
pub fn cmd_references(
    cwd: &Path,
    name: &str,
    path: Option<&str>,
    language: Option<&str>,
    limit: usize,
    out: Out,
) -> Result<()> {
    let s = Session::open(cwd, false, out)?;

    let expr = format!(r"\b{}\b", regex::escape(name));
    let re = regex::Regex::new(&expr).map_err(|e| anyhow!("invalid name: {e}"))?;
    let (hits, truncated) = scan_files(&s, &re, path, language, limit)?;

    // Definition sites of this exact name: (path, start_line).
    let mut defsites: HashSet<(String, i64)> = HashSet::new();
    {
        let mut stmt = s.db.conn.prepare(
            "SELECT path, start_line FROM facts WHERE name = ?1 AND kind <> 'import'",
        )?;
        let rows =
            stmt.query_map([name], |r| Ok((r.get::<_, String>(0)?, r.get::<_, i64>(1)?)))?;
        for row in rows {
            defsites.insert(row?);
        }
    }

    let references = hits
        .into_iter()
        .map(|h| {
            let definition = defsites.contains(&(h.path.clone(), h.line as i64));
            ReferenceRow { hit: h, definition }
        })
        .collect();

    s.emit(ReferencesResult { references, truncated })
}

/// The innermost definition whose byte range contains `off`.
fn innermost(defs: &[FileDef], off: usize) -> Option<&FileDef> {
    defs.iter()
        .filter(|d| d.start_byte <= off && off < d.end_byte)
        .max_by_key(|d| d.start_byte)
}

fn is_false(b: &bool) -> bool {
    !*b
}

fn cap_str(s: &str, max: usize) -> String {
    if s.chars().count() <= max {
        s.to_string()
    } else {
        let t: String = s.chars().take(max).collect();
        format!("{t}…")
    }
}

// `map` is a factual aggregation of the index — grouped counts, nothing more.
// It makes no judgement about importance or intent. Where a list is ordered by
// a count, that count is present in every row so the ordering is transparent
// and reproducible; ties break on name/path. Nothing here is a heuristic guess.

#[derive(Serialize)]
struct MapLang {
    language: String,
    files: i64,
    bytes: i64,
    symbols: i64,
}

#[derive(Serialize)]
struct MapTotals {
    files: i64,
    symbols: i64,
    imports: i64,
}

#[derive(Serialize)]
struct DirStat {
    dir: String,
    files: i64,
    symbols: i64,
}

#[derive(Serialize)]
struct ModuleImports {
    /// Raw import string, exactly as written (unresolved).
    module: String,
    /// Number of distinct files that import it.
    importers: i64,
}

#[derive(Serialize)]
struct FileDefs {
    path: String,
    definitions: i64,
    bytes: i64,
}

#[derive(Serialize)]
struct RepoMap {
    /// How each ordered list is sorted, stated plainly so results are predictable.
    ordering: &'static str,
    totals: MapTotals,
    languages: Vec<MapLang>,
    directories: Vec<DirStat>,
    manifests: Vec<String>,
    imports_by_module: Vec<ModuleImports>,
    definitions_by_file: Vec<FileDefs>,
}

/// `map` — one call that summarises the index for orientation. Deliberately
/// factual: counts grouped by language, directory, imported module, and file.
/// `--limit` bounds each list; `--limit 0` returns everything.
pub fn cmd_map(cwd: &Path, limit: usize, out: Out) -> Result<()> {
    let s = Session::open(cwd, false, out)?;
    let conn = &s.db.conn;
    let cap = if limit == 0 { 1_000_000 } else { limit };

    let totals = MapTotals {
        files: conn.query_row("SELECT COUNT(*) FROM files", [], |r| r.get(0))?,
        symbols: conn.query_row(
            "SELECT COUNT(*) FROM facts WHERE kind <> 'import'",
            [],
            |r| r.get(0),
        )?,
        imports: conn.query_row(
            "SELECT COUNT(*) FROM facts WHERE kind = 'import'",
            [],
            |r| r.get(0),
        )?,
    };

    // Languages: file/byte counts merged with symbol counts.
    let mut langs: BTreeMap<String, (i64, i64, i64)> = BTreeMap::new();
    {
        let mut st = conn.prepare(
            "SELECT COALESCE(language,'(unknown)'), COUNT(*), COALESCE(SUM(size_bytes),0)
             FROM files GROUP BY language",
        )?;
        let rows = st.query_map([], |r| {
            Ok((r.get::<_, String>(0)?, r.get::<_, i64>(1)?, r.get::<_, i64>(2)?))
        })?;
        for row in rows {
            let (l, f, b) = row?;
            let e = langs.entry(l).or_default();
            e.0 += f;
            e.1 += b;
        }
        let mut st = conn.prepare(
            "SELECT COALESCE(language,'(unknown)'), COUNT(*)
             FROM facts WHERE kind <> 'import' GROUP BY language",
        )?;
        let rows = st.query_map([], |r| Ok((r.get::<_, String>(0)?, r.get::<_, i64>(1)?)))?;
        for row in rows {
            let (l, c) = row?;
            langs.entry(l).or_default().2 += c;
        }
    }
    let mut languages: Vec<MapLang> = langs
        .into_iter()
        .map(|(language, (files, bytes, symbols))| MapLang {
            language,
            files,
            bytes,
            symbols,
        })
        .collect();
    languages.sort_by(|a, b| b.files.cmp(&a.files).then(a.language.cmp(&b.language)));
    languages.truncate(cap);

    // Imports grouped by module (raw string), counted by distinct importing file.
    let imports_by_module: Vec<ModuleImports> = {
        let mut st = conn.prepare(
            "SELECT name, COUNT(DISTINCT path) c FROM facts
             WHERE kind = 'import' AND name IS NOT NULL
             GROUP BY name ORDER BY c DESC, name LIMIT ?1",
        )?;
        let rows = st.query_map([cap as i64], |r| {
            Ok(ModuleImports {
                module: r.get(0)?,
                importers: r.get(1)?,
            })
        })?;
        rows.collect::<std::result::Result<Vec<_>, _>>()?
    };

    // Definitions grouped by file.
    let definitions_by_file: Vec<FileDefs> = {
        let mut st = conn.prepare(
            "SELECT facts.path, COUNT(*) c, files.size_bytes
             FROM facts JOIN files ON files.path = facts.path
             WHERE facts.kind <> 'import'
             GROUP BY facts.path ORDER BY c DESC, facts.path LIMIT ?1",
        )?;
        let rows = st.query_map([cap as i64], |r| {
            Ok(FileDefs {
                path: r.get(0)?,
                definitions: r.get(1)?,
                bytes: r.get(2)?,
            })
        })?;
        rows.collect::<std::result::Result<Vec<_>, _>>()?
    };

    // Directories + manifests: one scan of file paths.
    let mut dir_files: BTreeMap<String, (i64, i64)> = BTreeMap::new();
    let mut manifests: Vec<String> = Vec::new();
    {
        let mut st = conn.prepare("SELECT path, size_bytes FROM files")?;
        let rows = st.query_map([], |r| Ok((r.get::<_, String>(0)?, r.get::<_, i64>(1)?)))?;
        for row in rows {
            let (path, size) = row?;
            let e = dir_files.entry(top_dir(&path)).or_default();
            e.0 += 1;
            e.1 += size;
            if is_manifest(&path) {
                manifests.push(path);
            }
        }
    }
    let mut dir_syms: BTreeMap<String, i64> = BTreeMap::new();
    {
        let mut st = conn.prepare("SELECT path FROM facts WHERE kind <> 'import'")?;
        let rows = st.query_map([], |r| r.get::<_, String>(0))?;
        for row in rows {
            *dir_syms.entry(top_dir(&row?)).or_default() += 1;
        }
    }
    let mut directories: Vec<DirStat> = dir_files
        .into_iter()
        .map(|(dir, (files, _bytes))| {
            let symbols = dir_syms.get(&dir).copied().unwrap_or(0);
            DirStat { dir, files, symbols }
        })
        .collect();
    directories.sort_by(|a, b| b.files.cmp(&a.files).then(a.dir.cmp(&b.dir)));
    directories.truncate(cap);
    manifests.sort();
    manifests.truncate(cap);

    let data = RepoMap {
        ordering: "lists ordered by their visible count (files/importers/definitions) desc, ties by name/path",
        totals,
        languages,
        directories,
        manifests,
        imports_by_module,
        definitions_by_file,
    };
    s.emit(data)
}

/// The first path component, or "(root)" for top-level files.
fn top_dir(path: &str) -> String {
    match path.split_once('/') {
        Some((d, _)) => d.to_string(),
        None => "(root)".to_string(),
    }
}

/// Recognise common project manifest / metadata files by basename.
fn is_manifest(path: &str) -> bool {
    let base = path.rsplit('/').next().unwrap_or(path);
    if base.starts_with("README") {
        return true;
    }
    matches!(
        base,
        "Cargo.toml"
            | "Cargo.lock"
            | "package.json"
            | "pyproject.toml"
            | "setup.py"
            | "setup.cfg"
            | "requirements.txt"
            | "Pipfile"
            | "go.mod"
            | "go.sum"
            | "pom.xml"
            | "build.gradle"
            | "Gemfile"
            | "Makefile"
            | "Dockerfile"
            | "docker-compose.yml"
            | "tsconfig.json"
    )
}

/// Self-description so an agent can discover what codeq supports without
/// guessing. Deliberately does not require (or refresh) an index.
pub fn cmd_capabilities(out: Out) -> Result<()> {
    let data = serde_json::json!({
        "result_schema": RESULT_SCHEMA,
        "index_schema": SCHEMA_VERSION,
        "commands": [
            "index", "files", "changed", "stats", "map",
            "symbols", "definitions", "references", "outline", "at", "search",
            "imports", "importers", "context", "capabilities",
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
