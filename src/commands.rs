//! Command implementations for the v1 proving spine:
//! `index`, `files`, `changed`, `stats`.

use std::collections::{BTreeMap, HashSet};
use std::path::Path;
use std::time::Instant;

use anyhow::{anyhow, Result};
use regex_syntax::hir::literal::{ExtractKind, Extractor as LiteralExtractor};
use serde::Serialize;

use crate::context::{self, Request};
use crate::db::{Db, SCHEMA_VERSION};
use crate::envelope::{Envelope, IndexMeta, Render, RepoMeta, RESULT_SCHEMA};
use crate::facts::{has_spans, Span};
use crate::index::{language_for, refresh};
use crate::repo::Repo;

/// Output rendering options parsed from global CLI flags.
#[derive(Clone, Default)]
pub struct Out {
    pub json: bool,
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
            db.conn.execute("DELETE FROM files", [])?; // cascades to facts
            db.conn.execute("DELETE FROM content_fts", [])?;
            // Also clear the directory cache: otherwise the next refresh sees an
            // empty `files` table against an unchanged `dirs` cache, judges the
            // index "fresh", and skips the walk — leaving it empty.
            db.conn.execute("DELETE FROM dirs", [])?;
        }
        let stats = refresh(&db, &repo)?;
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
            json: self.out.json,
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

fn definitions_of(
    s: &Session,
    symbol: &str,
    path: Option<&str>,
    limit: usize,
) -> Result<Vec<DefinitionRow>> {
    let mut sql = String::from(
        "SELECT name, kind, path, language,
                start_line, start_col, end_line, end_col, start_byte, end_byte,
                signature, doc
         FROM facts WHERE name = ?1 AND kind <> 'import'",
    );
    let like = path.map(|p| format!("%{p}%"));
    let mut params: Vec<&dyn rusqlite::ToSql> = vec![&symbol];
    if let Some(l) = &like {
        sql.push_str(" AND path LIKE ?2");
        params.push(l);
    }
    sql.push_str(" ORDER BY path, start_line");
    if limit > 0 {
        sql.push_str(&format!(" LIMIT {limit}"));
    }
    let mut stmt = s.db.conn.prepare(&sql)?;
    let rows = stmt.query_map(params.as_slice(), |r| {
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
    Ok(rows.collect::<std::result::Result<_, _>>()?)
}

pub fn cmd_definitions(
    cwd: &Path,
    symbol: &str,
    path: Option<&str>,
    limit: usize,
    out: Out,
) -> Result<()> {
    let s = Session::open(cwd, false, out)?;
    let data = definitions_of(&s, symbol, path, limit)?;
    s.emit(data)
}

#[derive(Serialize)]
struct Caller {
    #[serde(skip_serializing_if = "Option::is_none")]
    symbol: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    kind: Option<String>,
    path: String,
    line: usize,
    /// Best-effort: the caller looks like a test (path or symbol name).
    #[serde(skip_serializing_if = "is_false")]
    test: bool,
}

/// Distinct definitions that contain a word-boundary reference to `name`,
/// excluding `name`'s own declaration. Textual (name-based), not type-resolved.
/// `path` restricts to callers *located* in matching files.
fn callers_of(s: &Session, name: &str, path: Option<&str>) -> Result<Vec<Caller>> {
    let re = regex::Regex::new(&format!(r"\b{}\b", regex::escape(name)))?;
    let candidates = fetch_bodies(s, like_of(name).as_deref(), path, None)?;
    // Callers means references in *code* — not comments, strings, or prose in
    // unparseable files (which are excluded). Uses the cached comment/string spans.
    let opts = ScanOpts {
        before: 0,
        after: 0,
        out: OutMode::Matches,
        in_filter: InFilter::Code,
        limit: 0,
    };
    let hits = match run_search(s, &re, candidates, &opts)? {
        SearchData::Matches { matches, .. } => matches,
        _ => unreachable!(),
    };

    // Declaration lines of `name` — a reference on one of these is the def, not a call.
    let mut defsites: HashSet<(String, i64)> = HashSet::new();
    {
        let mut st = s.db.conn.prepare(
            "SELECT path, start_line FROM facts WHERE name = ?1 AND kind <> 'import'",
        )?;
        for row in st.query_map([name], |r| Ok((r.get::<_, String>(0)?, r.get::<_, i64>(1)?)))? {
            defsites.insert(row?);
        }
    }

    let mut seen: HashSet<(String, Option<String>)> = HashSet::new();
    let mut out = Vec::new();
    for h in hits {
        if defsites.contains(&(h.path.clone(), h.line as i64)) {
            continue; // the declaration itself
        }
        if !seen.insert((h.path.clone(), h.symbol.clone())) {
            continue; // one row per (file, enclosing symbol)
        }
        let test = h.path.contains("test")
            || h.path.contains("spec")
            || h.symbol
                .as_deref()
                .map(|s| s.starts_with("test") || s.starts_with("Test"))
                .unwrap_or(false);
        out.push(Caller {
            symbol: h.symbol,
            kind: h.kind,
            path: h.path,
            line: h.line,
            test,
        });
    }
    Ok(out)
}

#[derive(Serialize)]
struct Callee {
    name: String,
    kind: String,
    path: String,
    line: i64,
}

/// Definitions referenced by name inside `name`'s body — its (textual) callees.
/// Same one-hop, name-based rule as `context --expand`. `path` scopes which
/// definition(s) of `name` to analyse (so a common name like `build` doesn't
/// merge unrelated functions).
fn callees_of(s: &Session, name: &str, path: Option<&str>) -> Result<Vec<Callee>> {
    let targets: Vec<(String, usize, usize)> = {
        let mut sql = String::from(
            "SELECT path, start_byte, end_byte FROM facts WHERE name = ?1 AND kind <> 'import'",
        );
        let like = path.map(|p| format!("%{p}%"));
        let mut params: Vec<&dyn rusqlite::ToSql> = vec![&name];
        if let Some(l) = &like {
            sql.push_str(" AND path LIKE ?2");
            params.push(l);
        }
        let mut st = s.db.conn.prepare(&sql)?;
        let rows = st.query_map(params.as_slice(), |r| {
            Ok((
                r.get::<_, String>(0)?,
                r.get::<_, i64>(1)? as usize,
                r.get::<_, i64>(2)? as usize,
            ))
        })?;
        rows.collect::<std::result::Result<Vec<_>, _>>()?
    };
    if targets.is_empty() {
        return Ok(Vec::new());
    }

    let mut idents: HashSet<String> = HashSet::new();
    {
        let mut body_stmt = s.db.conn.prepare("SELECT body FROM content_fts WHERE path = ?1")?;
        for (path, sb, eb) in &targets {
            let body: Option<String> = body_stmt.query_row([path], |r| r.get(0)).ok();
            if let Some(b) = body {
                if let Some(slice) = b.get(*sb..(*eb).min(b.len())) {
                    collect_idents(slice, &mut idents);
                }
            }
        }
    }
    idents.remove(name);
    if idents.is_empty() {
        return Ok(Vec::new());
    }

    // Look up only the referenced names via the facts(name) index, in chunks to
    // stay under SQLite's bound-parameter limit — instead of scanning every fact.
    let ident_list: Vec<&String> = idents.iter().collect();
    let mut seen: HashSet<(String, String, i64)> = HashSet::new();
    let mut out = Vec::new();
    for chunk in ident_list.chunks(500) {
        let placeholders = vec!["?"; chunk.len()].join(",");
        let sql = format!(
            "SELECT name, kind, path, start_line FROM facts
             WHERE kind <> 'import' AND name IN ({placeholders})"
        );
        let mut stmt = s.db.conn.prepare(&sql)?;
        let params: Vec<&dyn rusqlite::ToSql> =
            chunk.iter().map(|n| *n as &dyn rusqlite::ToSql).collect();
        let rows = stmt.query_map(params.as_slice(), |r| {
            Ok((
                r.get::<_, String>(0)?,
                r.get::<_, String>(1)?,
                r.get::<_, String>(2)?,
                r.get::<_, i64>(3)?,
            ))
        })?;
        for row in rows {
            let (n, k, p, l) = row?;
            if n != name && seen.insert((n.clone(), p.clone(), l)) {
                out.push(Callee {
                    name: n,
                    kind: k,
                    path: p,
                    line: l,
                });
            }
        }
    }
    out.sort_by(|a, b| {
        (a.name.as_str(), a.path.as_str(), a.line).cmp(&(b.name.as_str(), b.path.as_str(), b.line))
    });
    Ok(out)
}

/// Collect the identifiers that are *called* in `text`: a name (>= 2 chars)
/// immediately followed by `(`, ignoring whitespace. Keeping only call sites
/// (`foo()`, `obj.method()`, `Type::new()`) drops the field accesses, variable
/// uses and type references that merely share a name with some definition — the
/// main source of callee noise. Real callees are always called, so nothing that
/// is genuinely a callee is lost. (Still name-based, not type-resolved: a std
/// `.remove()` can't be told apart from a local `remove`.)
fn collect_idents(text: &str, out: &mut HashSet<String>) {
    let chars: Vec<char> = text.chars().collect();
    let n = chars.len();
    let mut i = 0;
    while i < n {
        if chars[i] == '_' || chars[i].is_alphanumeric() {
            let start = i;
            while i < n && (chars[i] == '_' || chars[i].is_alphanumeric()) {
                i += 1;
            }
            // Peek past whitespace for a '(' — the mark of a call.
            let mut j = i;
            while j < n && chars[j].is_whitespace() {
                j += 1;
            }
            if j < n && chars[j] == '(' && i - start >= 2 {
                out.insert(chars[start..i].iter().collect());
            }
        } else {
            i += 1;
        }
    }
}

/// `callers <name>` — who references this symbol (by enclosing definition).
/// A node in a transitive `callers`/`callees` walk, tagged with the hop distance.
#[derive(Serialize)]
struct Hop {
    #[serde(skip_serializing_if = "Option::is_none")]
    symbol: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    kind: Option<String>,
    path: String,
    line: usize,
    /// Hops from the queried symbol (1 = direct).
    depth: usize,
    #[serde(skip_serializing_if = "is_false")]
    test: bool,
}

/// A transitive callee, mirroring `Callee` (so `name` is the field at every
/// depth) plus its hop distance.
#[derive(Serialize)]
struct CalleeHop {
    name: String,
    kind: String,
    path: String,
    line: i64,
    /// Hops from the queried symbol (1 = direct).
    depth: usize,
}

/// Safety bound on a transitive walk so a hot symbol can't blow up.
const HOP_CAP: usize = 2000;

pub fn cmd_callers(cwd: &Path, name: &str, path: Option<&str>, depth: usize, out: Out) -> Result<()> {
    let s = Session::open(cwd, false, out)?;
    if depth <= 1 {
        return s.emit(callers_of(&s, name, path)?);
    }
    // Breadth-first up the call tree: each hop expands the newly-found callers by
    // their enclosing symbol name. Deterministic; each caller reported once at its
    // shortest depth, each name expanded once.
    let mut reported: HashSet<(String, Option<String>)> = HashSet::new();
    let mut expanded: HashSet<String> = HashSet::from([name.to_string()]);
    let mut frontier: Vec<String> = vec![name.to_string()];
    let mut hops: Vec<Hop> = Vec::new();
    'walk: for d in 1..=depth {
        let mut next: Vec<String> = Vec::new();
        for sym in &frontier {
            for c in callers_of(&s, sym, path)? {
                if !reported.insert((c.path.clone(), c.symbol.clone())) {
                    continue;
                }
                if let Some(cn) = &c.symbol {
                    if expanded.insert(cn.clone()) {
                        next.push(cn.clone());
                    }
                }
                hops.push(Hop {
                    symbol: c.symbol,
                    kind: c.kind,
                    path: c.path,
                    line: c.line,
                    depth: d,
                    test: c.test,
                });
                if hops.len() >= HOP_CAP {
                    break 'walk;
                }
            }
        }
        frontier = next;
        if frontier.is_empty() {
            break;
        }
    }
    hops.sort_by(|a, b| {
        a.depth
            .cmp(&b.depth)
            .then(a.path.cmp(&b.path))
            .then(a.line.cmp(&b.line))
    });
    s.emit(hops)
}

/// `callees <name>` — what this symbol references (by name).
pub fn cmd_callees(cwd: &Path, name: &str, path: Option<&str>, depth: usize, out: Out) -> Result<()> {
    let s = Session::open(cwd, false, out)?;
    if depth <= 1 {
        return s.emit(callees_of(&s, name, path)?);
    }
    // Breadth-first down the call tree by callee name.
    let mut reported: HashSet<(String, String)> = HashSet::new();
    let mut expanded: HashSet<String> = HashSet::from([name.to_string()]);
    let mut frontier: Vec<String> = vec![name.to_string()];
    let mut hops: Vec<CalleeHop> = Vec::new();
    'walk: for d in 1..=depth {
        let mut next: Vec<String> = Vec::new();
        for sym in &frontier {
            for c in callees_of(&s, sym, path)? {
                if !reported.insert((c.path.clone(), c.name.clone())) {
                    continue;
                }
                if expanded.insert(c.name.clone()) {
                    next.push(c.name.clone());
                }
                hops.push(CalleeHop {
                    name: c.name,
                    kind: c.kind,
                    path: c.path,
                    line: c.line,
                    depth: d,
                });
                if hops.len() >= HOP_CAP {
                    break 'walk;
                }
            }
        }
        frontier = next;
        if frontier.is_empty() {
            break;
        }
    }
    hops.sort_by(|a, b| {
        a.depth
            .cmp(&b.depth)
            .then(a.path.cmp(&b.path))
            .then(a.line.cmp(&b.line))
    });
    s.emit(hops)
}

#[derive(Serialize)]
struct About {
    definitions: Vec<DefinitionRow>,
    callers: Vec<Caller>,
    callees: Vec<Callee>,
}

/// `about <name>` — the pre-edit dossier: definition(s) with signature/doc, who
/// calls it, and what it calls — in one call instead of five. `path` scopes the
/// definition and its callees to the matching file(s), so a common name doesn't
/// merge unrelated symbols. Callers stay repo-wide (textual references to the
/// name), which is the useful "who uses this".
pub fn cmd_about(cwd: &Path, name: &str, path: Option<&str>, out: Out) -> Result<()> {
    let s = Session::open(cwd, false, out)?;
    let data = About {
        definitions: definitions_of(&s, name, path, 0)?,
        callers: callers_of(&s, name, None)?,
        callees: callees_of(&s, name, path)?,
    };
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

/// Bodies of function/method definitions this many lines or longer are folded.
const MIN_FOLD: usize = 4;

/// `read <file>` — a folding file reader (text output, not JSON). By default it
/// shows the file with long function/method bodies collapsed to their signature
/// plus a marker with the exact hidden line range, so an agent grasps a large
/// file's whole shape cheaply and expands only what it needs. `--lines A-B` and
/// `--symbol NAME` return verbatim, line-numbered slices to read or edit.
pub fn cmd_read(
    cwd: &Path,
    file: &str,
    lines: Option<&str>,
    symbol: Option<&str>,
    full: bool,
    out: Out,
) -> Result<()> {
    let s = Session::open(cwd, false, out)?;
    let rel = resolve_file(&s, file)?;
    let bytes = std::fs::read(s.repo.root.join(&rel))
        .map_err(|e| anyhow!("cannot read {rel}: {e}"))?;
    // Match the index: drop a leading UTF-8 BOM so line/column numbering here
    // agrees with what search and the symbol facts report.
    let text = String::from_utf8_lossy(bytes.strip_prefix(&[0xEF, 0xBB, 0xBF]).unwrap_or(&bytes));
    let file_lines: Vec<&str> = text.lines().collect();
    let n = file_lines.len();

    // Verbatim line range.
    if let Some(spec) = lines {
        let (a, b) = parse_range(spec)?;
        print_range(&file_lines, a, b);
        return Ok(());
    }

    // Verbatim definition(s) by name.
    if let Some(name) = symbol {
        let mut stmt = s.db.conn.prepare(
            "SELECT start_line, end_line FROM facts
             WHERE path = ?1 AND name = ?2 AND kind <> 'import' ORDER BY start_line",
        )?;
        let ranges: Vec<(i64, i64)> = stmt
            .query_map((&rel, name), |r| Ok((r.get(0)?, r.get(1)?)))?
            .collect::<std::result::Result<_, _>>()?;
        if ranges.is_empty() {
            return Err(anyhow!("no definition named '{name}' in {rel}"));
        }
        for (a, b) in ranges {
            print_range(&file_lines, a as usize, b as usize);
            println!();
        }
        return Ok(());
    }

    // Whole file when asked, or when we can't parse it to know what to fold.
    if full || !crate::facts::has_spans(language_for(&rel).unwrap_or("")) {
        print_range(&file_lines, 1, n);
        return Ok(());
    }

    // Folded view: hide the bodies of function/method definitions.
    let mut folded = vec![false; n];
    {
        let mut stmt = s.db.conn.prepare(
            "SELECT start_line, end_line FROM facts
             WHERE path = ?1 AND kind IN ('function', 'method')",
        )?;
        let rows = stmt.query_map([&rel], |r| {
            Ok((r.get::<_, i64>(0)?, r.get::<_, i64>(1)?))
        })?;
        for row in rows {
            let (sl, el) = row?;
            let (sl, el) = (sl.max(1) as usize, el.max(1) as usize);
            if el >= sl && el - sl >= MIN_FOLD {
                for ln in (sl + 1)..=el {
                    if ln - 1 < n {
                        folded[ln - 1] = true;
                    }
                }
            }
        }
    }
    print_folded(&file_lines, &folded);
    Ok(())
}

fn parse_range(spec: &str) -> Result<(usize, usize)> {
    let (a, b) = spec
        .split_once('-')
        .ok_or_else(|| anyhow!("--lines must be A-B, e.g. 40-80"))?;
    let a: usize = a.trim().parse().map_err(|_| anyhow!("invalid line range"))?;
    let b: usize = b.trim().parse().map_err(|_| anyhow!("invalid line range"))?;
    Ok((a.max(1), b.max(a)))
}

fn print_range(lines: &[&str], a: usize, b: usize) {
    for i in a.max(1)..=b {
        if let Some(l) = lines.get(i - 1) {
            println!("{i:6}  {l}");
        }
    }
}

fn print_folded(lines: &[&str], folded: &[bool]) {
    let n = lines.len();
    let mut i = 0;
    while i < n {
        if folded[i] {
            let start = i;
            while i < n && folded[i] {
                i += 1;
            }
            let count = i - start;
            println!("        ⋮  {count} lines ({}–{})", start + 1, i);
        } else {
            println!("{:6}  {}", i + 1, lines[i]);
            i += 1;
        }
    }
}

/// Resolve a file argument (repo-relative path or basename) to an indexed path.
fn resolve_file(s: &Session, arg: &str) -> Result<String> {
    if s
        .db
        .conn
        .query_row("SELECT 1 FROM files WHERE path = ?1", [arg], |_| Ok(()))
        .is_ok()
    {
        return Ok(arg.to_string());
    }
    let suffix = format!("%/{arg}");
    let mut stmt = s
        .db
        .conn
        .prepare("SELECT path FROM files WHERE path LIKE ?1 ORDER BY path")?;
    let matches: Vec<String> = stmt
        .query_map([suffix], |r| r.get(0))?
        .collect::<std::result::Result<_, _>>()?;
    match matches.len() {
        1 => Ok(matches.into_iter().next().unwrap()),
        0 => Err(anyhow!("file not found: {arg}")),
        _ => Err(anyhow!("'{arg}' is ambiguous: {}", matches.join(", "))),
    }
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

pub fn cmd_astgrep(
    cwd: &Path,
    pattern: &str,
    language: &str,
    path: Option<String>,
    limit: usize,
    out: Out,
) -> Result<()> {
    let s = Session::open(cwd, false, out)?;

    // Gather the indexed source of every file in the requested language (the
    // stored FTS body is the whole file). Structural search re-parses each.
    let mut sql = String::from("SELECT path, body FROM content_fts WHERE language = ?1");
    let like = path.map(|p| format!("%{p}%"));
    let mut params: Vec<&dyn rusqlite::ToSql> = vec![&language];
    if let Some(l) = &like {
        sql.push_str(" AND path LIKE ?2");
        params.push(l);
    }
    sql.push_str(" ORDER BY path");
    let files: Vec<(String, String)> = {
        let mut stmt = s.db.conn.prepare(&sql)?;
        let rows = stmt.query_map(params.as_slice(), |r| {
            Ok((r.get::<_, String>(0)?, r.get::<_, String>(1)?))
        })?;
        rows.collect::<std::result::Result<_, _>>()?
    };

    let mut matches = crate::astgrep::search(language, pattern, files)?;
    let truncated = limit > 0 && matches.len() > limit;
    if truncated {
        matches.truncate(limit);
    }

    // Annotate each match with the definition it sits inside (like `search`), so
    // the reader needn't a follow-up `at`. Matches are already sorted by path, so
    // load each file's defs once.
    let mut def_stmt = s.db.conn.prepare(
        "SELECT name, kind, start_byte, end_byte FROM facts
         WHERE path = ?1 AND kind <> 'import'",
    )?;
    let mut cur_path: Option<String> = None;
    let mut defs: Vec<FileDef> = Vec::new();

    #[derive(Serialize)]
    struct AstRow {
        path: String,
        line: usize,
        col: usize,
        end_line: usize,
        text: String,
        #[serde(skip_serializing_if = "Option::is_none")]
        symbol: Option<String>,
        #[serde(skip_serializing_if = "serde_json::Map::is_empty")]
        binds: serde_json::Map<String, serde_json::Value>,
    }
    #[derive(Serialize)]
    struct AstData {
        matches: Vec<AstRow>,
        #[serde(skip_serializing_if = "std::ops::Not::not")]
        truncated: bool,
    }
    let mut rows: Vec<AstRow> = Vec::with_capacity(matches.len());
    for m in matches {
        if cur_path.as_deref() != Some(m.path.as_str()) {
            defs = load_file_defs(&mut def_stmt, &m.path)?;
            cur_path = Some(m.path.clone());
        }
        let symbol = innermost(&defs, m.start_byte).map(|d| d.name.clone());
        let binds = m
            .binds
            .into_iter()
            .map(|(k, v)| (k, serde_json::Value::from(v)))
            .collect();
        rows.push(AstRow {
            path: m.path,
            line: m.start_line,
            col: m.start_col,
            end_line: m.end_line,
            text: m.text,
            symbol,
            binds,
        });
    }
    s.emit(AstData {
        matches: rows,
        truncated,
    })
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
    /// Context lines before the match (with -B/-C), nearest last.
    #[serde(skip_serializing_if = "Vec::is_empty")]
    before: Vec<String>,
    /// Context lines after the match (with -A/-C).
    #[serde(skip_serializing_if = "Vec::is_empty")]
    after: Vec<String>,
}

#[derive(Serialize)]
struct FileCount {
    path: String,
    count: usize,
}

/// Output shape depends on the mode: full matches, matching file paths, or
/// per-file counts. `truncated` is true when `--limit` cut it off.
#[derive(Serialize)]
#[serde(untagged)]
enum SearchData {
    Matches { matches: Vec<Hit>, truncated: bool },
    Files { files: Vec<String>, truncated: bool },
    Counts { counts: Vec<FileCount>, truncated: bool },
}

#[derive(Clone, Copy, PartialEq)]
enum OutMode {
    Matches,
    Files,
    Count,
}

/// Restrict matches by the syntactic class of their position.
#[derive(Clone, Copy, PartialEq)]
enum InFilter {
    Any,
    Code,
    Comments,
    Strings,
}

impl InFilter {
    fn parse(s: &str) -> Result<InFilter> {
        Ok(match s {
            "any" => InFilter::Any,
            "code" => InFilter::Code,
            "comment" | "comments" => InFilter::Comments,
            "string" | "strings" => InFilter::Strings,
            other => return Err(anyhow!("--in must be code|comments|strings, got '{other}'")),
        })
    }

    fn accepts(self, cls: Option<Span>) -> bool {
        match self {
            InFilter::Any => true,
            InFilter::Code => cls.is_none(),
            InFilter::Comments => cls == Some(Span::Comment),
            InFilter::Strings => cls == Some(Span::Str),
        }
    }
}

/// The class of byte offset `off` given sorted, non-overlapping `spans`.
fn class_at(spans: &[(usize, usize, Span)], off: usize) -> Option<Span> {
    let idx = spans.partition_point(|(s, _, _)| *s <= off);
    if idx == 0 {
        return None;
    }
    let (s, e, c) = spans[idx - 1];
    if s <= off && off < e {
        Some(c)
    } else {
        None
    }
}

struct ScanOpts {
    before: usize,
    after: usize,
    out: OutMode,
    in_filter: InFilter,
    limit: usize,
}

struct FileDef {
    name: String,
    kind: String,
    start_byte: usize,
    end_byte: usize,
}

const MAX_LINE: usize = 240;

/// Candidate file bodies pulled from the FTS index — no filesystem reads.
/// When `like` holds one or more `%literal%` patterns, the content index (a
/// trigram FTS5 table) narrows to files whose body matches ANY of them, via
/// trigram-accelerated `LIKE` — a safe superset (`_`/`%` in the literal act as
/// wildcards, only broadening; the precise regex confirms). `LIKE` is used
/// rather than FTS `MATCH`, which silently drops substring matches for many
/// short/underscore terms. With no patterns, every text file is a candidate
/// (regex with no extractable literal). `path`/`language` filters apply after.
fn fetch_bodies(
    s: &Session,
    like: Option<&[String]>,
    path: Option<&str>,
    language: Option<&str>,
) -> Result<Vec<(String, Option<String>, String)>> {
    let patterns = like.filter(|l| !l.is_empty());

    let mut sql = String::from("SELECT path, language, body FROM content_fts");
    if let Some(pats) = patterns {
        let clause = (1..=pats.len())
            .map(|i| format!("body LIKE ?{i}"))
            .collect::<Vec<_>>()
            .join(" OR ");
        sql.push_str(&format!(" WHERE {clause}"));
    }
    sql.push_str(" ORDER BY path");

    let raw: Vec<(String, Option<String>, String)> = {
        let mut stmt = s.db.conn.prepare(&sql)?;
        if let Some(pats) = patterns {
            let params: Vec<&dyn rusqlite::ToSql> =
                pats.iter().map(|p| p as &dyn rusqlite::ToSql).collect();
            let rows = stmt.query_map(params.as_slice(), row_to_body)?;
            rows.collect::<std::result::Result<Vec<_>, _>>()?
        } else {
            let rows = stmt.query_map([], row_to_body)?;
            rows.collect::<std::result::Result<Vec<_>, _>>()?
        }
    };

    let mut out = Vec::new();
    for (p, lang, body) in raw {
        if let Some(pf) = path {
            if !p.contains(pf) {
                continue;
            }
        }
        if let Some(lf) = language {
            if lang.as_deref() != Some(lf) {
                continue;
            }
        }
        out.push((p, lang, body));
    }
    Ok(out)
}

fn row_to_body(r: &rusqlite::Row) -> rusqlite::Result<(String, Option<String>, String)> {
    Ok((r.get(0)?, r.get(1)?, r.get(2)?))
}

/// Byte offset of the start of each line.
fn line_starts_of(text: &str) -> Vec<usize> {
    let mut v = vec![0usize];
    for (i, b) in text.as_bytes().iter().enumerate() {
        if *b == b'\n' {
            v.push(i + 1);
        }
    }
    v
}

/// The text of line `idx` (0-based), trailing whitespace trimmed and capped.
fn line_text(text: &str, line_starts: &[usize], idx: usize) -> String {
    let start = line_starts[idx];
    let end = line_starts.get(idx + 1).map(|&s| s - 1).unwrap_or(text.len());
    cap_str(text[start..end.min(text.len())].trim_end(), MAX_LINE)
}

/// Context lines for the indices in `range` that exist.
fn context_lines(
    text: &str,
    line_starts: &[usize],
    range: std::ops::Range<usize>,
) -> Vec<String> {
    range
        .filter(|&i| i < line_starts.len())
        .map(|i| line_text(text, line_starts, i))
        .collect()
}

fn load_file_defs(stmt: &mut rusqlite::Statement, path: &str) -> Result<Vec<FileDef>> {
    let defs = stmt
        .query_map([path], |r| {
            Ok(FileDef {
                name: r.get(0)?,
                kind: r.get(1)?,
                start_byte: r.get::<_, i64>(2)? as usize,
                end_byte: r.get::<_, i64>(3)? as usize,
            })
        })?
        .collect::<std::result::Result<_, _>>()?;
    Ok(defs)
}

/// Run `re` over candidate bodies and shape the output per `opts.out`.
/// Deterministic order (path, then line). When `opts.in_filter` is not `Any`,
/// matches are classified via `classifier`; files in a language we cannot parse
/// are excluded (never guessed).
fn run_search(
    s: &Session,
    re: &regex::Regex,
    candidates: Vec<(String, Option<String>, String)>,
    opts: &ScanOpts,
) -> Result<SearchData> {
    let cap = if opts.limit == 0 { usize::MAX } else { opts.limit };
    let filtering = opts.in_filter != InFilter::Any;

    let mut def_stmt = s.db.conn.prepare(
        "SELECT name, kind, start_byte, end_byte FROM facts
         WHERE path = ?1 AND kind <> 'import'",
    )?;
    // Comment/string spans are cached in the index (extracted once), so --in is
    // a lookup, not a per-query re-parse.
    let mut span_stmt = s.db.conn.prepare(
        "SELECT start_byte, end_byte, kind FROM spans WHERE path = ?1 ORDER BY start_byte",
    )?;

    let mut files: Vec<String> = Vec::new();
    let mut counts: Vec<FileCount> = Vec::new();
    let mut hits: Vec<Hit> = Vec::new();
    let mut truncated = false;

    'outer: for (rel, lang, body) in &candidates {
        // Load cached spans if filtering; skip files in a language we can't parse.
        let spans = if filtering {
            if !has_spans(lang.as_deref().unwrap_or("")) {
                continue;
            }
            let rows = span_stmt.query_map([rel], |r| {
                Ok((
                    r.get::<_, i64>(0)? as usize,
                    r.get::<_, i64>(1)? as usize,
                    Span::from_i64(r.get::<_, i64>(2)?).unwrap_or(Span::Str),
                ))
            })?;
            rows.collect::<std::result::Result<Vec<_>, _>>()?
        } else {
            Vec::new()
        };
        let accept = |off: usize| !filtering || opts.in_filter.accepts(class_at(&spans, off));

        let starts = line_starts_of(body);

        match opts.out {
            OutMode::Files => {
                if re.find_iter(body).any(|m| accept(m.start())) {
                    files.push(rel.clone());
                    if files.len() >= cap {
                        truncated = true;
                        break 'outer;
                    }
                }
            }
            OutMode::Count => {
                let mut last = usize::MAX;
                let mut n = 0usize;
                for m in re.find_iter(body) {
                    if !accept(m.start()) {
                        continue;
                    }
                    let li = starts.partition_point(|&s| s <= m.start()) - 1;
                    if li != last {
                        last = li;
                        n += 1;
                    }
                }
                if n > 0 {
                    counts.push(FileCount {
                        path: rel.clone(),
                        count: n,
                    });
                    if counts.len() >= cap {
                        truncated = true;
                        break 'outer;
                    }
                }
            }
            OutMode::Matches => {
                let defs = load_file_defs(&mut def_stmt, rel)?;
                let mut last_line = usize::MAX;
                for m in re.find_iter(body) {
                    let off = m.start();
                    if !accept(off) {
                        continue;
                    }
                    let line_idx = starts.partition_point(|&s| s <= off) - 1;
                    if line_idx == last_line {
                        continue; // one row per line
                    }
                    last_line = line_idx;

                    let enclosing = innermost(&defs, off);
                    hits.push(Hit {
                        path: rel.clone(),
                        line: line_idx + 1,
                        col: off - starts[line_idx],
                        symbol: enclosing.map(|d| d.name.clone()),
                        kind: enclosing.map(|d| d.kind.clone()),
                        text: line_text(body, &starts, line_idx),
                        before: context_lines(
                            body,
                            &starts,
                            line_idx.saturating_sub(opts.before)..line_idx,
                        ),
                        after: context_lines(
                            body,
                            &starts,
                            (line_idx + 1)..(line_idx + 1 + opts.after),
                        ),
                    });
                    if hits.len() >= cap {
                        truncated = true;
                        break 'outer;
                    }
                }
            }
        }
    }

    Ok(match opts.out {
        OutMode::Files => SearchData::Files { files, truncated },
        OutMode::Count => SearchData::Counts { counts, truncated },
        OutMode::Matches => SearchData::Matches {
            matches: hits,
            truncated,
        },
    })
}

/// A `%literal%` LIKE pattern for the content index — trigram needs >= 3 chars,
/// so shorter literals can't be prefiltered (caller full-scans). `_`/`%` in the
/// literal are left as LIKE wildcards: only broadens the candidate set (a safe
/// superset), and the precise regex confirms.
fn like_pattern(lit: &str) -> Option<String> {
    if lit.chars().count() < 3 {
        return None;
    }
    Some(format!("%{lit}%"))
}

/// A single-literal prefilter (for `--fixed` search and `references`).
fn like_of(lit: &str) -> Option<Vec<String>> {
    like_pattern(lit).map(|p| vec![p])
}

/// Build a prefilter for a regex: a set of literals such that *every* match must
/// contain at least one of them (as a substring), OR-ed as LIKE patterns. Uses
/// regex-syntax prefix/suffix literal extraction, then an inner-literal fallback.
/// Returns `None` when no safe prefilter exists — the caller then full-scans,
/// which is always correct.
///
/// Correctness: a finite prefix (or suffix) literal set means every match starts
/// (or ends) with one of the literals, so it contains one as a substring; the
/// LIKE OR is a true superset and never drops a match.
fn regex_prefilter(pattern: &str) -> Option<Vec<String>> {
    let hir = regex_syntax::parse(pattern).ok()?;
    usable_literals(ExtractKind::Prefix, &hir)
        .or_else(|| usable_literals(ExtractKind::Suffix, &hir))
        .or_else(|| inner_required_literal(&hir))
}

/// The longest literal that must appear in *every* match, found by walking the
/// HIR. This catches a literal buried in the middle (e.g. `.*needle.*`) that
/// prefix/suffix extraction misses. A required literal is a safe single-term
/// prefilter; when there isn't one >= 3 chars we return None and full-scan.
fn inner_required_literal(hir: &regex_syntax::hir::Hir) -> Option<Vec<String>> {
    fn flush(cur: &mut Vec<u8>, best: &mut Vec<u8>) {
        if cur.len() > best.len() {
            *best = cur.clone();
        }
        cur.clear();
    }
    // Accumulate contiguous literals; anything that isn't an unconditional
    // literal (a class, an optional/zero repetition, an alternation, a look)
    // breaks the run. Required containers (concat, capture, min>=1 repetition)
    // are walked so their literals count too.
    fn walk(hir: &regex_syntax::hir::Hir, cur: &mut Vec<u8>, best: &mut Vec<u8>) {
        use regex_syntax::hir::HirKind;
        match hir.kind() {
            HirKind::Literal(lit) => cur.extend_from_slice(&lit.0[..]),
            HirKind::Concat(subs) => {
                for s in subs {
                    walk(s, cur, best);
                }
            }
            HirKind::Capture(c) => walk(&c.sub, cur, best),
            HirKind::Repetition(r) if r.min >= 1 => {
                flush(cur, best);
                walk(&r.sub, cur, best);
                flush(cur, best);
            }
            _ => flush(cur, best),
        }
    }
    let mut best = Vec::new();
    let mut cur = Vec::new();
    walk(hir, &mut cur, &mut best);
    flush(&mut cur, &mut best);
    let s = std::str::from_utf8(&best).ok()?;
    like_pattern(s).map(|p| vec![p])
}

fn usable_literals(kind: ExtractKind, hir: &regex_syntax::hir::Hir) -> Option<Vec<String>> {
    let mut ex = LiteralExtractor::new();
    ex.kind(kind);
    let seq = ex.extract(hir);
    let lits = seq.literals()?;
    if lits.is_empty() || lits.len() > 64 {
        return None;
    }
    let mut terms = Vec::with_capacity(lits.len());
    for lit in lits {
        let s = std::str::from_utf8(lit.as_bytes()).ok()?;
        // Any alternative shorter than a trigram is unfilterable, which would
        // make the whole OR unsafe — bail to a full scan instead.
        terms.push(like_pattern(s)?);
    }
    Some(terms)
}

/// Include/exclude glob filter over repo-relative paths, ripgrep-style: a `!`
/// prefix excludes; a pattern with no `/` matches by basename in any directory;
/// a trailing `/` matches a whole directory.
struct GlobFilter {
    includes: globset::GlobSet,
    excludes: globset::GlobSet,
    has_includes: bool,
}

impl GlobFilter {
    fn build(globs: &[String]) -> Result<GlobFilter> {
        let mut inc = globset::GlobSetBuilder::new();
        let mut exc = globset::GlobSetBuilder::new();
        let mut has_includes = false;
        for g in globs {
            let (neg, pat) = match g.strip_prefix('!') {
                Some(rest) => (true, rest),
                None => (false, g.as_str()),
            };
            let normalized = Self::normalize(pat);
            let glob = globset::GlobBuilder::new(&normalized)
                .literal_separator(true)
                .build()
                .map_err(|e| anyhow!("invalid glob '{g}': {e}"))?;
            if neg {
                exc.add(glob);
            } else {
                inc.add(glob);
                has_includes = true;
            }
        }
        Ok(GlobFilter {
            includes: inc.build()?,
            excludes: exc.build()?,
            has_includes,
        })
    }

    /// A bare name (no `/`) matches in any directory; a trailing `/` means a
    /// whole directory subtree.
    fn normalize(pat: &str) -> String {
        let base = if let Some(dir) = pat.strip_suffix('/') {
            format!("{dir}/**")
        } else {
            pat.to_string()
        };
        if base.contains('/') {
            base
        } else {
            format!("**/{base}")
        }
    }

    fn accepts(&self, path: &str) -> bool {
        if self.excludes.is_match(path) {
            return false;
        }
        !self.has_includes || self.includes.is_match(path)
    }
}

/// Options for the `search` command surface, kept in a struct so the CLI can
/// grow without a huge argument list.
pub struct SearchArgs<'a> {
    pub pattern: &'a str,
    pub fixed: bool,
    pub ignore_case: bool,
    pub word: bool,
    pub path: Option<&'a str>,
    pub language: Option<&'a str>,
    pub globs: &'a [String],
    pub in_kind: Option<&'a str>,
    pub before: usize,
    pub after: usize,
    pub files_with_matches: bool,
    pub count: bool,
    pub limit: usize,
}

/// `search <pattern>` — content search over indexed, non-binary files, with each
/// match annotated by its enclosing definition. Regex by default (linear-time,
/// no catastrophic backtracking); `--fixed` for a literal string. A strict
/// superset of grep: same regex engine, plus structure grep cannot see.
pub fn cmd_search(cwd: &Path, args: SearchArgs, out: Out) -> Result<()> {
    let s = Session::open(cwd, false, out)?;

    let mut expr = if args.fixed {
        regex::escape(args.pattern)
    } else {
        args.pattern.to_string()
    };
    if args.word {
        expr = format!(r"\b(?:{expr})\b");
    }
    // Match line-by-line semantics like grep/ripgrep: the scan runs over each
    // whole file body, so `^`/`$` must anchor to line boundaries, not the start
    // and end of the buffer. Without this, `^use` matches only a file that
    // begins with `use` (near-zero) instead of every line starting with it.
    let re = regex::RegexBuilder::new(&expr)
        .case_insensitive(args.ignore_case)
        .multi_line(true)
        .build()
        .map_err(|e| anyhow!("invalid pattern: {e}"))?;

    // Prefilter candidate files via the trigram index: a fixed search by its
    // whole literal, a regex by its required literals. Either may be None (no
    // usable >=3-char literal), in which case we scan every body — still from
    // the index, never the filesystem, and always correct.
    let match_query = if args.fixed {
        like_of(args.pattern)
    } else {
        regex_prefilter(args.pattern)
    };
    let mut candidates = fetch_bodies(&s, match_query.as_deref(), args.path, args.language)?;

    if !args.globs.is_empty() {
        let gf = GlobFilter::build(args.globs)?;
        candidates.retain(|(path, _, _)| gf.accepts(path));
    }

    let out_mode = if args.files_with_matches {
        OutMode::Files
    } else if args.count {
        OutMode::Count
    } else {
        OutMode::Matches
    };
    let in_filter = match args.in_kind {
        Some(k) => InFilter::parse(k)?,
        None => InFilter::Any,
    };

    let opts = ScanOpts {
        before: args.before,
        after: args.after,
        out: out_mode,
        in_filter,
        limit: args.limit,
    };
    let data = run_search(&s, &re, candidates, &opts)?;
    s.emit(data)
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
    // The name is a substring of every reference, so the trigram index narrows
    // to files that contain it (the \b matcher then confirms word boundaries).
    let match_query = like_of(name);
    let candidates = fetch_bodies(&s, match_query.as_deref(), path, language)?;
    let opts = ScanOpts {
        before: 0,
        after: 0,
        out: OutMode::Matches,
        in_filter: InFilter::Any,
        limit,
    };
    let (hits, truncated) = match run_search(&s, &re, candidates, &opts)? {
        SearchData::Matches { matches, truncated } => (matches, truncated),
        _ => unreachable!("references always uses Matches mode"),
    };

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

/// Whether a path looks like a test file, across common frameworks. Heuristic —
/// codeq can't see `#[test]`/`@Test` attributes, so it leans on path and name.
fn is_test_path(path: &str) -> bool {
    let p = path.to_ascii_lowercase();
    for seg in ["/test/", "/tests/", "/__tests__/", "/spec/", "/specs/"] {
        if p.contains(seg) {
            return true;
        }
    }
    if p.starts_with("test/") || p.starts_with("tests/") || p.starts_with("spec/") {
        return true;
    }
    let fname = p.rsplit('/').next().unwrap_or(&p);
    fname.starts_with("test_")
        || fname.ends_with("_test.rs")
        || fname.ends_with("_test.py")
        || fname.ends_with("_test.go")
        || fname.ends_with("_test.rb")
        || fname.ends_with("_spec.rb")
        || fname.contains(".test.")
        || fname.contains(".spec.")
        || fname.ends_with("test.java")
        || fname.ends_with("tests.java")
        || fname.ends_with("test.cs")
        || fname.ends_with("tests.cs")
}

/// Whether a definition name looks like a test (test_foo, TestBar, fooTest, …).
fn is_test_name(name: &str) -> bool {
    name.starts_with("test")
        || name.starts_with("Test")
        || name.ends_with("Test")
        || name.ends_with("Tests")
        || name.ends_with("Spec")
        || name.ends_with("_spec")
}

/// `tests <name>` — the test functions that reference `name`. A reference counts
/// as a test when its file looks like a test file or its enclosing function name
/// looks like a test. Name-based, like `references`; can't see attribute-only
/// tests (Rust `#[cfg(test)]` unit fns in a non-test path may be missed).
pub fn cmd_tests(cwd: &Path, name: &str, path: Option<&str>, limit: usize, out: Out) -> Result<()> {
    let s = Session::open(cwd, false, out)?;

    let expr = format!(r"\b{}\b", regex::escape(name));
    let re = regex::Regex::new(&expr).map_err(|e| anyhow!("invalid name: {e}"))?;
    let candidates = fetch_bodies(&s, like_of(name).as_deref(), path, None)?;
    let opts = ScanOpts {
        before: 0,
        after: 0,
        out: OutMode::Matches,
        in_filter: InFilter::Any,
        limit: 0,
    };
    let hits = match run_search(&s, &re, candidates, &opts)? {
        SearchData::Matches { matches, .. } => matches,
        _ => unreachable!("tests always uses Matches mode"),
    };

    // Unique enclosing tests (path, test-name) that reference `name` — excluding
    // `name`'s own definition (enclosing == name).
    let mut tests: std::collections::BTreeSet<(String, String)> = std::collections::BTreeSet::new();
    for h in &hits {
        let sym = match &h.symbol {
            Some(s) if s != name => s,
            _ => continue,
        };
        let is_fn = matches!(h.kind.as_deref(), Some("function") | Some("method"));
        if is_fn && (is_test_path(&h.path) || is_test_name(sym)) {
            tests.insert((h.path.clone(), sym.clone()));
        }
    }

    // Resolve each test's definition line/kind via the facts(name) index.
    #[derive(Serialize)]
    struct TestRow {
        path: String,
        line: i64,
        name: String,
        kind: String,
    }
    let mut rows: Vec<TestRow> = Vec::new();
    let names: Vec<String> = tests
        .iter()
        .map(|(_, n)| n.clone())
        .collect::<HashSet<_>>()
        .into_iter()
        .collect();
    for chunk in names.chunks(500) {
        let placeholders = vec!["?"; chunk.len()].join(",");
        let sql = format!(
            "SELECT path, name, start_line, kind FROM facts
             WHERE kind <> 'import' AND name IN ({placeholders})"
        );
        let mut stmt = s.db.conn.prepare(&sql)?;
        let params: Vec<&dyn rusqlite::ToSql> =
            chunk.iter().map(|n| n as &dyn rusqlite::ToSql).collect();
        let frows = stmt.query_map(params.as_slice(), |r| {
            Ok((
                r.get::<_, String>(0)?,
                r.get::<_, String>(1)?,
                r.get::<_, i64>(2)?,
                r.get::<_, String>(3)?,
            ))
        })?;
        for fr in frows {
            let (p, n, ln, k) = fr?;
            if tests.contains(&(p.clone(), n.clone())) {
                rows.push(TestRow {
                    path: p,
                    line: ln,
                    name: n,
                    kind: k,
                });
            }
        }
    }
    rows.sort_by(|a, b| a.path.cmp(&b.path).then(a.line.cmp(&b.line)));
    rows.dedup_by(|a, b| a.path == b.path && a.name == b.name && a.line == b.line);
    let truncated = limit > 0 && rows.len() > limit;
    if truncated {
        rows.truncate(limit);
    }

    #[derive(Serialize)]
    struct TestsData {
        tests: Vec<TestRow>,
        #[serde(skip_serializing_if = "std::ops::Not::not")]
        truncated: bool,
    }
    s.emit(TestsData {
        tests: rows,
        truncated,
    })
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
            "symbols", "definitions", "callers", "callees", "about",
            "references", "outline", "at", "read", "search",
            "imports", "importers", "astgrep", "tests", "context", "capabilities", "mcp",
        ],
        // Languages with symbol + import extraction.
        "extraction_languages": ["rust", "python", "javascript", "typescript", "tsx", "go", "java", "ruby", "c", "cpp", "csharp", "php"],
        // Languages recognised for file classification (extraction may be absent).
        "classified_languages": [
            "rust", "javascript", "typescript", "tsx", "python", "go", "java",
            "json", "yaml", "toml", "markdown", "sql", "shell", "c", "cpp", "ruby",
        ],
        "fact_kinds": [
            "function", "method", "struct", "enum", "union", "trait", "module",
            "constant", "typealias", "macro", "class", "interface", "type", "import",
        ],
        "notes": {
            "imports": "raw syntactic module strings; not resolved",
            "context_tokenizer": "approx:4-bytes-per-token",
            "freshness": "every command refreshes changed files before answering",
        },
    });
    // Honor the global output mode like every other command: JSON on
    // --json/--meta/--pretty, otherwise codeq's compact text.
    if out.json || out.meta || out.pretty {
        let value = serde_json::json!({ "schema": RESULT_SCHEMA, "data": data });
        if out.pretty {
            println!("{}", serde_json::to_string_pretty(&value)?);
        } else {
            println!("{}", serde_json::to_string(&value)?);
        }
    } else {
        print!("{}", crate::envelope::render_data_text(&data));
    }
    Ok(())
}
