//! Command implementations for the v1 proving spine:
//! `index`, `files`, `changed`, `stats`.

use std::collections::{BTreeMap, HashMap, HashSet};
use std::path::Path;
use std::time::Instant;

use anyhow::{anyhow, Result};
use serde::Serialize;

use crate::context::{self, Request};
use crate::db::{Db, SCHEMA_VERSION};
use crate::deps;
use crate::envelope::{Envelope, IndexMeta, Render, RepoMeta, RESULT_SCHEMA};
use crate::index::{language_for, refresh};
use crate::repo::Repo;
use crate::search::{
    self, innermost, like_of, load_file_defs, regex_prefilter, run_search, scan_stdin,
    FileDef, Hit, InFilter, OutMode, ScanOpts, SearchData,
};

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
        self.emit_noted(data, None)
    }

    /// Emit with a note explaining an empty result. The note is only ever
    /// diagnostic: it never changes `data`, so a parser can keep ignoring it.
    fn emit_noted<T: Serialize>(&self, data: T, note: Option<String>) -> Result<()> {
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
        Envelope::new(repo, index, data).note(note).print(Render {
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

// --- empty-result diagnosis --------------------------------------------------
//
// An empty result is ambiguous: the name may not exist, it may exist with none
// of the relation asked for, or a `--path` filter may have removed everything.
// Left unexplained, all three look like "no" — and a typo looks like a fact.
// These helpers turn the empty case into one line saying which it was.

/// Levenshtein distance, bailing out once it cannot beat `best`.
fn edit_distance(a: &str, b: &str, best: usize) -> usize {
    if a.len().abs_diff(b.len()) > best {
        return best + 1;
    }
    let (a, b): (Vec<char>, Vec<char>) = (a.chars().collect(), b.chars().collect());
    let mut prev: Vec<usize> = (0..=b.len()).collect();
    let mut cur = vec![0usize; b.len() + 1];
    for i in 1..=a.len() {
        cur[0] = i;
        let mut row_best = cur[0];
        for j in 1..=b.len() {
            let cost = usize::from(a[i - 1] != b[j - 1]);
            cur[j] = (prev[j] + 1).min(cur[j - 1] + 1).min(prev[j - 1] + cost);
            row_best = row_best.min(cur[j]);
        }
        if row_best > best {
            return best + 1; // no cell in this row can still win
        }
        std::mem::swap(&mut prev, &mut cur);
    }
    prev[b.len()]
}

/// The closest defined symbol names to `name`, nearest first. Case-insensitive,
/// and tolerant in proportion to the name's length.
fn nearest_names(s: &Session, name: &str) -> Result<Vec<String>> {
    // Deliberately tight: a suggestion has to be plausibly the same name
    // mistyped. Two edits on a long name already reaches unrelated words, and a
    // bad suggestion is worse than none because it invites a second wrong query.
    let budget = (name.chars().count() / 4).clamp(1, 2);
    let needle = name.to_lowercase();
    let mut stmt = s
        .db
        .conn
        .prepare("SELECT DISTINCT name FROM facts WHERE kind <> 'import'")?;
    let mut scored: Vec<(usize, String)> = stmt
        .query_map([], |r| r.get::<_, String>(0))?
        .filter_map(|n| n.ok())
        .filter_map(|n| {
            let d = edit_distance(&needle, &n.to_lowercase(), budget);
            (d <= budget).then_some((d, n))
        })
        .collect();
    scored.sort_by(|a, b| {
        let affix = |n: &str| {
            let n = n.to_lowercase();
            !(n.starts_with(&needle) || needle.starts_with(&n))
        };
        affix(&a.1)
            .cmp(&affix(&b.1))
            .then(a.0.cmp(&b.0))
            .then(a.1.cmp(&b.1))
    });
    scored.dedup_by(|a, b| a.1 == b.1);
    Ok(scored.into_iter().take(3).map(|(_, n)| n).collect())
}

/// Where a symbol is defined: `(path, line)` per definition.
fn definition_sites(s: &Session, name: &str) -> Result<Vec<(String, i64)>> {
    let mut stmt = s.db.conn.prepare(
        "SELECT path, start_line FROM facts
         WHERE name = ?1 AND kind <> 'import' ORDER BY path, start_line",
    )?;
    let sites = stmt
        .query_map([name], |r| Ok((r.get(0)?, r.get(1)?)))?
        .collect::<std::result::Result<_, _>>()?;
    Ok(sites)
}

/// Explain an empty result for a command keyed on a symbol name.
///
/// `relation` names what was being looked for ("callers", "members", …) and is
/// used verbatim in the message. Returns `None` when the result was not empty.
fn note_if_empty<T>(
    s: &Session,
    rows: &[T],
    name: &str,
    relation: &str,
    path: Option<&str>,
) -> Result<Option<String>> {
    if !rows.is_empty() {
        return Ok(None);
    }
    let sites = definition_sites(s, name)?;
    if sites.is_empty() {
        let near = nearest_names(s, name)?;
        let hint = if near.is_empty() {
            String::new()
        } else {
            format!("; closest: {}", near.join(", "))
        };
        return Ok(Some(format!("no symbol named '{name}'{hint}")));
    }
    if let Some(filter) = path {
        if !sites.iter().any(|(p, _)| p.contains(filter)) {
            let where_ = sites
                .iter()
                .map(|(p, l)| format!("{p}:{l}"))
                .take(3)
                .collect::<Vec<_>>()
                .join(", ");
            return Ok(Some(format!(
                "'{name}' is defined at {where_} but nothing matches --path {filter}"
            )));
        }
    }
    let (p, l) = &sites[0];
    let more = if sites.len() > 1 {
        format!(" (+{} more definitions)", sites.len() - 1)
    } else {
        String::new()
    };
    Ok(Some(format!(
        "'{name}' is defined at {p}:{l}{more} but has no {relation}"
    )))
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

/// Optional `AND …` filter clauses shared by the list commands, built with
/// positional `?` params (bound in append order, after any base params the query
/// already has). Centralizes the repeated path-substring / exact-column-match
/// pattern so a new filtered command can't misthread its parameters.
#[derive(Default)]
struct Filters {
    sql: String,
    args: Vec<String>,
}

impl Filters {
    fn new() -> Self {
        Self::default()
    }

    /// `AND {col} LIKE '%needle%'` when `needle` is Some.
    fn like(mut self, col: &str, needle: Option<&str>) -> Self {
        if let Some(n) = needle {
            self.sql.push_str(&format!(" AND {col} LIKE ?"));
            self.args.push(format!("%{n}%"));
        }
        self
    }

    /// `AND {col} = value` when `value` is Some.
    fn eq(mut self, col: &str, value: Option<&str>) -> Self {
        if let Some(v) = value {
            self.sql.push_str(&format!(" AND {col} = ?"));
            self.args.push(v.to_string());
        }
        self
    }

    fn sql(&self) -> &str {
        &self.sql
    }

    fn args(&self) -> &[String] {
        &self.args
    }
}

/// A `LIMIT n` clause, or empty when `limit` is 0 (unlimited).
fn limit_clause(limit: usize) -> String {
    if limit > 0 {
        format!(" LIMIT {limit}")
    } else {
        String::new()
    }
}

/// Filter args as `ToSql` refs, appended after any `base` params the query binds
/// before its optional filters.
fn params_with<'a>(
    base: &[&'a dyn rusqlite::ToSql],
    filters: &'a Filters,
) -> Vec<&'a dyn rusqlite::ToSql> {
    let mut p: Vec<&dyn rusqlite::ToSql> = base.to_vec();
    p.extend(filters.args().iter().map(|a| a as &dyn rusqlite::ToSql));
    p
}

pub fn cmd_files(
    cwd: &Path,
    pattern: Option<&str>,
    language: Option<&str>,
    limit: usize,
    out: Out,
) -> Result<()> {
    let s = Session::open(cwd, false, out)?;

    let filters = Filters::new().like("path", pattern).eq("language", language);
    let sql = format!(
        "SELECT path, language, size_bytes FROM files WHERE 1=1{} ORDER BY path{}",
        filters.sql(),
        limit_clause(limit),
    );
    let mut stmt = s.db.conn.prepare(&sql)?;
    let rows = stmt.query_map(params_with(&[], &filters).as_slice(), |r| {
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
    /// still exists in the tree). True for almost every row, so it is only
    /// emitted when false, that being the case that carries information.
    #[serde(skip_serializing_if = "is_true")]
    indexed: bool,
}

fn is_true(b: &bool) -> bool {
    *b
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
/// Definitions whose body overlaps a changed hunk vs `compare_to` — the diff's
/// touched symbols. Shared by `changed --symbols` and `affected`.
fn touched_defs(s: &Session, compare_to: &str) -> Result<Vec<ChangedSymbolRow>> {
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
    Ok(data)
}

fn changed_symbols(s: &Session, compare_to: &str, limit: usize) -> Result<()> {
    let mut data = touched_defs(s, compare_to)?;
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
        let kind: String = r.get(1)?;
        Ok(SymbolRow {
            name: r.get(0)?,
            path: r.get(2)?,
            language: r.get(3)?,
            start_line: r.get(4)?,
            end_line: r.get(5)?,
            signature: if signatures {
                declaration_header(&kind, r.get(6)?)
            } else {
                None
            },
            kind,
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
    let filters = Filters::new().like("path", path);
    let sql = format!(
        "SELECT name, kind, path, language,
                start_line, start_col, end_line, end_col, start_byte, end_byte,
                signature, doc
         FROM facts WHERE name = ? AND kind <> 'import'{} ORDER BY path, start_line{}",
        filters.sql(),
        limit_clause(limit),
    );
    let mut stmt = s.db.conn.prepare(&sql)?;
    let rows = stmt.query_map(params_with(&[&symbol], &filters).as_slice(), |r| {
        let kind: String = r.get(1)?;
        Ok(DefinitionRow {
            name: r.get(0)?,
            path: r.get(2)?,
            language: r.get(3)?,
            start_line: r.get(4)?,
            start_col: r.get(5)?,
            end_line: r.get(6)?,
            end_col: r.get(7)?,
            start_byte: r.get(8)?,
            end_byte: r.get(9)?,
            signature: declaration_header(&kind, r.get(10)?),
            doc: r.get(11)?,
            kind,
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
    let note = note_if_empty(&s, &data, symbol, "definition", path)?;
    s.emit_noted(data, note)
}

#[derive(Serialize)]
struct ApiRow {
    name: String,
    kind: String,
    path: String,
    line: i64,
    #[serde(skip_serializing_if = "Option::is_none")]
    signature: Option<String>,
}

/// `api [<path>]` — the public API surface: definitions the code exports for
/// outside use, with signatures. "Public" is each language's own notion
/// (Rust `pub`, JS/TS `export`, Go capitalization, Python non-underscore, a
/// `public` modifier in Java/C#/PHP, non-`static` in C/C++); `pub(crate)` and
/// `protected` count as `restricted`, not public, so they're excluded. `path`
/// scopes to a file or directory substring; `--kind` filters to one kind.
pub fn cmd_api(
    cwd: &Path,
    path: Option<&str>,
    kind: Option<&str>,
    limit: usize,
    out: Out,
) -> Result<()> {
    let s = Session::open(cwd, false, out)?;
    let filters = Filters::new().like("path", path).eq("kind", kind);
    let sql = format!(
        "SELECT name, kind, path, start_line, signature
         FROM facts WHERE visibility = 'public' AND kind <> 'import'{} ORDER BY path, start_line{}",
        filters.sql(),
        limit_clause(limit),
    );
    let mut stmt = s.db.conn.prepare(&sql)?;
    let rows = stmt.query_map(params_with(&[], &filters).as_slice(), |r| {
        let kind: String = r.get(1)?;
        Ok(ApiRow {
            name: r.get(0)?,
            path: r.get(2)?,
            line: r.get(3)?,
            signature: declaration_header(&kind, r.get(4)?),
            kind,
        })
    })?;
    let data: Vec<ApiRow> = rows.collect::<std::result::Result<_, _>>()?;
    // Empty almost always means "the definitions are there, none are public" —
    // Rust `pub(crate)` and `protected` count as restricted, not public. Say so
    // with the counts, so it can't be mistaken for an empty or wrong path.
    let note = if data.is_empty() {
        let scope = path.unwrap_or("");
        let mut counts = s.db.conn.prepare(
            "SELECT COALESCE(visibility, 'unknown'), COUNT(*) FROM facts
             WHERE kind <> 'import' AND path LIKE ?1 GROUP BY 1 ORDER BY 1",
        )?;
        let like = format!("%{scope}%");
        let rows: Vec<(String, i64)> = counts
            .query_map([&like], |r| Ok((r.get(0)?, r.get(1)?)))?
            .collect::<std::result::Result<_, _>>()?;
        let total: i64 = rows.iter().map(|(_, n)| n).sum();
        let where_ = if scope.is_empty() {
            "this repo".to_string()
        } else {
            format!("'{scope}'")
        };
        Some(if total == 0 {
            format!("no definitions at all in {where_}")
        } else {
            let breakdown = rows
                .iter()
                .map(|(v, n)| format!("{n} {v}"))
                .collect::<Vec<_>>()
                .join(", ");
            format!("no public definitions in {where_} ({breakdown})")
        })
    } else {
        None
    };
    s.emit_noted(data, note)
}

/// Container kinds whose extracted `signature` is the whole declaration, body
/// and all, because the grammar has no separate body capture for them.
const CONTAINER_KINDS: &[&str] = &[
    "struct", "enum", "union", "trait", "class", "interface", "record",
    "namespace", "module", "type", "typealias",
];

/// The declaration header of a definition: for a container kind the stored
/// signature is the entire body (every field, doc comment and attribute), but a
/// listing only needs the header, so it is cut at the body opener:
/// `pub struct Edge { pub to: String, … }` becomes `pub struct Edge`. Functions
/// and methods already stop before their body, and a constant's value *is* its
/// content, so both are left alone.
fn declaration_header(kind: &str, sig: Option<String>) -> Option<String> {
    let sig = sig?;
    if !CONTAINER_KINDS.contains(&kind) {
        return Some(sig);
    }
    Some(match sig.split_once('{') {
        Some((head, _)) => head.trim_end().to_string(),
        None => sig,
    })
}

#[derive(Serialize)]
struct Caller {
    #[serde(skip_serializing_if = "Option::is_none")]
    symbol: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    kind: Option<String>,
    path: String,
    line: usize,
    /// The call site's column and source line — populated only under `--examples`,
    /// which turns a caller row into a usage example: `path:line:col:` the actual
    /// call expression (one representative site per caller).
    #[serde(skip_serializing_if = "Option::is_none")]
    col: Option<usize>,
    #[serde(skip_serializing_if = "Option::is_none")]
    text: Option<String>,
    /// Best-effort: the caller looks like a test (path or symbol name).
    #[serde(skip_serializing_if = "is_false")]
    test: bool,
    /// How this caller's file connects to a definition of the name via the import
    /// graph (only set under `--resolved`): same-file / imported / ambiguous.
    #[serde(skip_serializing_if = "Option::is_none")]
    via: Option<String>,
}

/// Distinct definitions that contain a word-boundary reference to `name`,
/// excluding `name`'s own declaration. Textual (name-based), not type-resolved.
/// `path` restricts to callers *located* in matching files.
fn callers_of(
    s: &Session,
    name: &str,
    path: Option<&str>,
    call_only: bool,
    qualifier: Option<&str>,
    examples: bool,
) -> Result<Vec<Caller>> {
    let re = regex::Regex::new(&format!(r"\b{}\b", regex::escape(name)))?;
    // Callers means references in *code* — not comments, strings, or prose in
    // unparseable files (which are excluded). Uses the cached comment/string spans.
    let opts = ScanOpts {
        before: 0,
        after: 0,
        out: OutMode::Matches,
        in_filter: InFilter::Code,
        limit: 0,
        call_only,
        qualifier: qualifier.map(str::to_string),
    };
    let hits = match run_search(&s.db.conn, &re, like_of(name).as_deref(), path, None, None, &opts)?
    {
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
    for mut h in hits {
        if defsites.contains(&(h.path.clone(), h.line as i64)) {
            continue; // the declaration itself
        }
        if !seen.insert((h.path.clone(), h.symbol.clone())) {
            continue; // one row per (file, enclosing symbol) — first call site wins
        }
        let test = h.path.contains("test")
            || h.path.contains("spec")
            || h.symbol
                .as_deref()
                .map(|s| s.starts_with("test") || s.starts_with("Test"))
                .unwrap_or(false);
        // Under --examples, surface the call site's line/column and source text so
        // the row reads as a usage example (rendered grep-style by the envelope).
        let (col, text) = if examples {
            (Some(h.col), Some(std::mem::take(&mut h.text)))
        } else {
            (None, None)
        };
        out.push(Caller {
            symbol: h.symbol,
            kind: h.kind,
            path: h.path,
            line: h.line,
            col,
            text,
            test,
            via: None,
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
    /// How this callee's definition file connects to `name`'s file via the import
    /// graph (only set under `--resolved`): same-file / imported.
    #[serde(skip_serializing_if = "Option::is_none")]
    via: Option<String>,
}

/// Definitions referenced by name inside `name`'s body — its (textual) callees.
/// Same one-hop, name-based rule as `context --expand`. `path` scopes which
/// definition(s) of `name` to analyse (so a common name like `build` doesn't
/// merge unrelated functions).
fn callees_of(
    s: &Session,
    name: &str,
    path: Option<&str>,
    resolved: bool,
) -> Result<Vec<Callee>> {
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
        let mut span_stmt = s.db.conn.prepare(
            "SELECT start_byte, end_byte FROM spans WHERE path = ?1 ORDER BY start_byte",
        )?;
        for (path, sb, eb) in &targets {
            let body: Option<String> = body_stmt.query_row([path], |r| r.get(0)).ok();
            let Some(b) = body else { continue };
            let Some(slice) = b.get(*sb..(*eb).min(b.len())) else {
                continue;
            };
            // `helper()` written inside a string literal or a comment is not a
            // call, so the identifier scan must skip those regions. Spans are
            // already in the index; a language without them yields none and the
            // whole body counts, as before.
            let prose: Vec<(usize, usize)> = span_stmt
                .query_map([path], |r| {
                    Ok((r.get::<_, i64>(0)? as usize, r.get::<_, i64>(1)? as usize))
                })?
                .collect::<std::result::Result<_, _>>()?;
            collect_idents(slice, *sb, &prose, &mut idents);
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
                    via: None,
                });
            }
        }
    }
    // Import-scoped resolution: a genuine callee's definition should live in a
    // file `name`'s own file can reach through imports (or the same file). Unlike
    // the name-based Resolver used for callers, here the anchor is fixed — `name`'s
    // definition file(s) — so we can prune by each callee's *specific* def-file
    // reachability, dropping unrelated same-named definitions in unreachable files
    // (the documented callees over-match). Heuristic, like all import resolution:
    // it can't tell which same-named def you meant if several sit in reachable
    // files, so it narrows a superset rather than pinpointing.
    if resolved {
        let target_files: HashSet<String> = targets.iter().map(|(p, _, _)| p.clone()).collect();
        let graph = load_dep_graph(s)?;
        let mut reach: HashSet<String> = target_files.clone();
        for tf in &target_files {
            for r in graph.reachable_from(tf) {
                reach.insert(r.to_string());
            }
        }
        out.retain(|c| reach.contains(&c.path));
        for c in &mut out {
            c.via = Some(if target_files.contains(&c.path) {
                "same-file".to_string()
            } else {
                "imported".to_string()
            });
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
/// Identifiers in call position within `text`, skipping any that fall inside a
/// comment or string. `base` is `text`'s byte offset within its file, so the
/// sorted `prose` ranges (which are file-absolute) line up.
fn collect_idents(
    text: &str,
    base: usize,
    prose: &[(usize, usize)],
    out: &mut HashSet<String>,
) {
    let in_prose = |off: usize| {
        let abs = base + off;
        let idx = prose.partition_point(|(s, _)| *s <= abs);
        idx > 0 && prose[idx - 1].1 > abs
    };

    let bytes = text.as_bytes();
    let is_word = |b: u8| b == b'_' || b.is_ascii_alphanumeric();
    let n = bytes.len();
    let mut i = 0;
    while i < n {
        // Non-ASCII bytes are never part of an identifier codeq reports, and
        // stepping by byte keeps offsets aligned with the span ranges.
        if is_word(bytes[i]) {
            let start = i;
            while i < n && is_word(bytes[i]) {
                i += 1;
            }
            // Peek past whitespace for a '(' — the mark of a call.
            let mut j = i;
            while j < n && bytes[j].is_ascii_whitespace() {
                j += 1;
            }
            if j < n && bytes[j] == b'(' && i - start >= 2 && !in_prose(start) {
                out.insert(text[start..i].to_string());
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

pub fn cmd_callers(
    cwd: &Path,
    name: &str,
    path: Option<&str>,
    depth: usize,
    resolved: bool,
    call_only: bool,
    qualifier: Option<&str>,
    examples: bool,
    out: Out,
) -> Result<()> {
    let s = Session::open(cwd, false, out)?;
    if depth <= 1 {
        let callers = callers_of(&s, name, path, call_only, qualifier, examples)?;
        let rows = resolve_rows(&s, name, resolved, callers, |c| c.path.as_str(), |c, v| {
            c.via = v;
        })?;
        let note = note_if_empty(&s, &rows, name, "callers", path)?;
        return s.emit_noted(rows, note);
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
            for c in callers_of(&s, sym, path, false, None, false)? {
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
    let note = note_if_empty(&s, &hops, name, "callers", path)?;
    s.emit_noted(hops, note)
}

/// `callees <name>` — what this symbol references (by name).
pub fn cmd_callees(
    cwd: &Path,
    name: &str,
    path: Option<&str>,
    depth: usize,
    resolved: bool,
    out: Out,
) -> Result<()> {
    let s = Session::open(cwd, false, out)?;
    if depth <= 1 {
        let rows = callees_of(&s, name, path, resolved)?;
        let note = note_if_empty(&s, &rows, name, "callees", path)?;
        return s.emit_noted(rows, note);
    }
    // Breadth-first down the call tree by callee name.
    let mut reported: HashSet<(String, String)> = HashSet::new();
    let mut expanded: HashSet<String> = HashSet::from([name.to_string()]);
    let mut frontier: Vec<String> = vec![name.to_string()];
    let mut hops: Vec<CalleeHop> = Vec::new();
    'walk: for d in 1..=depth {
        let mut next: Vec<String> = Vec::new();
        for sym in &frontier {
            for c in callees_of(&s, sym, path, false)? {
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
    let note = note_if_empty(&s, &hops, name, "callees", path)?;
    s.emit_noted(hops, note)
}

#[derive(Serialize)]
struct PathStep {
    symbol: String,
    kind: String,
    path: String,
    line: i64,
    /// Position in the chain (0 = the `from` symbol). Text output indents by it.
    depth: usize,
}

/// `path <from> <to>` — the shortest call chain from `from` to `to`, if one
/// exists: `from` calls … calls `to`. A BFS down the callee graph (so the chain
/// is shortest by hops), bounded by `max_depth`. Reachability with the route
/// shown, complementing `callees --depth` (which only says whether/at what depth
/// something is reached). Name-based, like the other graph commands; emits an
/// empty result when `to` isn't reachable from `from` within `max_depth`.
pub fn cmd_path(
    cwd: &Path,
    from: &str,
    to: &str,
    max_depth: usize,
    path: Option<&str>,
    out: Out,
) -> Result<()> {
    let s = Session::open(cwd, false, out)?;

    // Location + initial expansion of `from` are scoped by `path`, so an ambiguous
    // start name (e.g. `main` in several files/languages) resolves to the intended
    // definition. Only `from` is scoped; the chain then follows callees anywhere.
    let loc_of = |name: &str| -> Result<Option<(String, String, i64)>> {
        let defs = definitions_of(&s, name, path, 1)?;
        Ok(defs
            .into_iter()
            .next()
            .map(|d| (d.kind, d.path, d.start_line)))
    };

    // Trivial path: a symbol reaches itself in zero hops.
    if from == to {
        let step = loc_of(from)?.map(|(kind, path, line)| PathStep {
            symbol: from.to_string(),
            kind,
            path,
            line,
            depth: 0,
        });
        return s.emit(step.into_iter().collect::<Vec<_>>());
    }

    // BFS down the callee graph, recording each name's discoverer so the chain
    // can be rebuilt. Callees are sorted per node so the found path is stable.
    let mut pred: HashMap<String, String> = HashMap::new();
    let mut loc: HashMap<String, (String, String, i64)> = HashMap::new();
    let mut visited: HashSet<String> = HashSet::from([from.to_string()]);
    let mut frontier: Vec<String> = vec![from.to_string()];
    let mut found = false;
    'walk: for _ in 1..=max_depth {
        let mut next: Vec<String> = Vec::new();
        for node in &frontier {
            // Scope only the start symbol's callees; hops after it follow anywhere.
            let scope = if node.as_str() == from { path } else { None };
            let mut callees = callees_of(&s, node, scope, false)?;
            callees.sort_by(|a, b| a.name.cmp(&b.name).then(a.path.cmp(&b.path)).then(a.line.cmp(&b.line)));
            for c in callees {
                if !visited.insert(c.name.clone()) {
                    continue;
                }
                pred.insert(c.name.clone(), node.clone());
                loc.insert(c.name.clone(), (c.kind, c.path, c.line));
                if c.name == to {
                    found = true;
                    break 'walk;
                }
                next.push(c.name);
                if visited.len() >= HOP_CAP {
                    break 'walk;
                }
            }
        }
        frontier = next;
        if frontier.is_empty() {
            break;
        }
    }

    if !found {
        return s.emit(Vec::<PathStep>::new());
    }

    // Rebuild the chain from `to` back to `from`, then reverse.
    let mut chain: Vec<String> = vec![to.to_string()];
    let mut cur = to.to_string();
    while let Some(p) = pred.get(&cur) {
        chain.push(p.clone());
        cur = p.clone();
    }
    chain.reverse();

    let mut steps: Vec<PathStep> = Vec::with_capacity(chain.len());
    for (depth, name) in chain.into_iter().enumerate() {
        let l = if name == from {
            loc_of(&name)?
        } else {
            loc.get(&name).cloned()
        };
        let (kind, path, line) = l.unwrap_or_else(|| (String::new(), String::new(), 0));
        steps.push(PathStep { symbol: name, kind, path, line, depth });
    }
    s.emit(steps)
}

#[derive(Serialize)]
struct About {
    definitions: Vec<DefinitionRow>,
    /// What the symbol contains (fields, variants, methods). Omitted entirely
    /// for a function or anything else with no members, so the dossier does not
    /// grow an empty section for the common case.
    #[serde(skip_serializing_if = "Vec::is_empty")]
    members: Vec<MemberRow>,
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
        members: members_of(&s, name, path, None, 0)?,
        callers: callers_of(&s, name, None, false, None, false)?,
        callees: callees_of(&s, name, path, false)?,
    };
    // A typo is the dangerous case here: every section comes back empty under
    // valid headers, which reads like a real answer about a real symbol.
    let note = if data.definitions.is_empty() {
        note_if_empty(&s, &data.definitions, name, "definition", path)?
    } else {
        None
    };
    s.emit_noted(data, note)
}

/// A node in an `explore` graph: a definition (focus) or one of its immediate
/// neighbours (a caller or callee), with just enough to locate and identify it.
#[derive(serde::Serialize)]
struct ExploreNode {
    name: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    kind: Option<String>,
    path: String,
    line: i64,
    #[serde(skip_serializing_if = "Option::is_none")]
    signature: Option<String>,
    /// Only carried on the focus: what the symbol says it does, which is the
    /// point of exploring it. Callers and callees stay bare so the neighbourhood
    /// listing does not turn into prose.
    #[serde(skip_serializing_if = "Option::is_none")]
    doc: Option<String>,
}

/// The one-call answer: the symbol's definition, who calls it, what it calls, and
/// the verbatim source of that whole neighbourhood, budgeted and grouped by file.
#[derive(serde::Serialize)]
struct ExploreResult {
    focus: Vec<ExploreNode>,
    #[serde(skip_serializing_if = "Vec::is_empty")]
    callers: Vec<ExploreNode>,
    #[serde(skip_serializing_if = "Vec::is_empty")]
    callees: Vec<ExploreNode>,
    source: Vec<crate::context::Chunk>,
    budget: crate::context::Budget,
    #[serde(skip_serializing_if = "Vec::is_empty")]
    omitted: Vec<crate::context::Omitted>,
}

/// `explore <name>` — a single call that answers "show me this symbol and how it
/// fits". It composes the graph commands (definition + callers + callees) with
/// the context assembler (verbatim source of that neighbourhood, under a token
/// budget, grouped by file), so an agent gets structure *and* code in one shot
/// instead of a `definitions` → `callers` → `read` → `read` sequence.
pub fn cmd_explore(
    cwd: &Path,
    name: &str,
    path: Option<&str>,
    max_tokens: u64,
    out: Out,
) -> Result<()> {
    let s = Session::open(cwd, false, out)?;

    // The graph around `name`: its definition(s), who calls it, what it calls.
    // Callers stay repo-wide (textual "who uses this"); focus and callees honour
    // `--path` so a common name doesn't merge unrelated symbols.
    let defs = definitions_of(&s, name, path, 0)?;
    let callers = callers_of(&s, name, None, false, None, false)?;
    let callees = callees_of(&s, name, path, false)?;

    let focus: Vec<ExploreNode> = defs
        .iter()
        .map(|d| ExploreNode {
            name: d.name.clone(),
            kind: Some(d.kind.clone()),
            path: d.path.clone(),
            line: d.start_line,
            signature: d.signature.clone(),
            doc: d.doc.clone(),
        })
        .collect();
    let caller_nodes: Vec<ExploreNode> = callers
        .iter()
        .filter_map(|c| {
            c.symbol.as_ref().map(|sym| ExploreNode {
                name: sym.clone(),
                kind: c.kind.clone(),
                path: c.path.clone(),
                line: c.line as i64,
                signature: None,
                doc: None,
            })
        })
        .collect();
    let callee_nodes: Vec<ExploreNode> = callees
        .iter()
        .map(|c| ExploreNode {
            name: c.name.clone(),
            kind: Some(c.kind.clone()),
            path: c.path.clone(),
            line: c.line,
            signature: None,
            doc: None,
        })
        .collect();

    // Verbatim source for the neighbourhood, located precisely by (name, path) so
    // a common callee name is not re-expanded to every same-named definition. The
    // focus is forced (always included); callers/callees fill the rest of the
    // budget. Focus seeds first so the focus definition keeps the "focus" role.
    let mut seeds: Vec<crate::context::Seed> = Vec::new();
    for d in &defs {
        seeds.push(crate::context::Seed {
            name: d.name.clone(),
            path: d.path.clone(),
            role: "focus",
            forced: true,
        });
    }
    for c in &callers {
        if let Some(sym) = &c.symbol {
            seeds.push(crate::context::Seed {
                name: sym.clone(),
                path: c.path.clone(),
                role: "caller",
                forced: false,
            });
        }
    }
    for c in &callees {
        seeds.push(crate::context::Seed {
            name: c.name.clone(),
            path: c.path.clone(),
            role: "callee",
            forced: false,
        });
    }

    let (budget, source, omitted) =
        crate::context::source_for(&s.db, &s.repo, &seeds, max_tokens)?.into_source();

    let note = note_if_empty(&s, &focus, name, "definition", path)?;
    s.emit_noted(
        ExploreResult {
            focus,
            callers: caller_nodes,
            callees: callee_nodes,
            source,
            budget,
            omitted,
        },
        note,
    )
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
    // One file, resolved like `read`/`outline`: a bare `README.md` otherwise
    // matched every README in the repo and mixed their definitions together.
    let rel = resolve_file(&s, path)?;

    let mut stmt = s.db.conn.prepare(
        "SELECT name, kind, path, language,
                start_line, start_col, end_line, end_col, start_byte, end_byte,
                signature, doc
         FROM facts
         WHERE kind <> 'import' AND path = ?1
           AND start_line <= ?2 AND end_line >= ?2
         ORDER BY (end_byte - start_byte) ASC, path",
    )?;
    let rows = stmt.query_map((&rel, line), |r| {
        let kind: String = r.get(1)?;
        Ok(DefinitionRow {
            name: r.get(0)?,
            path: r.get(2)?,
            language: r.get(3)?,
            start_line: r.get(4)?,
            start_col: r.get(5)?,
            end_line: r.get(6)?,
            end_col: r.get(7)?,
            start_byte: r.get(8)?,
            end_byte: r.get(9)?,
            signature: declaration_header(&kind, r.get(10)?),
            doc: r.get(11)?,
            kind,
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

/// One definition in an owner's file, as loaded for containment resolution.
struct FileFact {
    name: String,
    kind: String,
    start_byte: i64,
    end_byte: i64,
    line: i64,
    signature: Option<String>,
    doc: Option<String>,
}

#[derive(Serialize)]
struct MemberRow {
    name: String,
    kind: String,
    path: String,
    line: i64,
    #[serde(skip_serializing_if = "Option::is_none")]
    signature: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    doc: Option<String>,
    /// Which definition of NAME this member belongs to. Only emitted when NAME
    /// resolves to more than one definition, where it is the disambiguator.
    #[serde(skip_serializing_if = "Option::is_none")]
    owner: Option<String>,
}

/// `members <name>` — what a type contains: its fields, enum variants, and
/// methods, in source order.
///
/// Containment is structural, not name-based: a member is a definition whose
/// byte range sits inside NAME's, with no other definition in between (so a
/// method's locals are not members of the class). That makes this the one graph
/// command with no name-matching caveat — the relationship comes from the parse
/// tree.
///
/// Languages that do not declare members (Ruby's `attr_accessor`, Python's
/// `self.x = …` in `__init__`, Go interface methods) yield nothing rather than
/// a guess.
pub fn cmd_members(
    cwd: &Path,
    name: &str,
    path: Option<&str>,
    kind: Option<&str>,
    limit: usize,
    out: Out,
) -> Result<()> {
    let s = Session::open(cwd, false, out)?;
    let data = members_of(&s, name, path, kind, limit)?;
    let relation = match kind {
        Some(k) => format!("members of kind '{k}'"),
        None => "members (fields, variants or methods)".to_string(),
    };
    let note = note_if_empty(&s, &data, name, &relation, path)?;
    s.emit_noted(data, note)
}

fn members_of(
    s: &Session,
    name: &str,
    path: Option<&str>,
    kind: Option<&str>,
    limit: usize,
) -> Result<Vec<MemberRow>> {
    // The owners: every definition of NAME (scoped by --path), each of which
    // contributes the members inside its own byte range.
    let mut owner_stmt = s.db.conn.prepare(
        "SELECT path, start_byte, end_byte FROM facts
         WHERE name = ?1 AND kind <> 'import'
         ORDER BY path, start_byte",
    )?;
    let owners: Vec<(String, i64, i64)> = owner_stmt
        .query_map([name], |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?)))?
        .collect::<std::result::Result<Vec<_>, _>>()?
        .into_iter()
        .filter(|(p, _, _): &(String, i64, i64)| path.is_none_or(|f| p.contains(f)))
        .collect();
    let multiple_owners = owners.len() > 1;

    // Every definition in an owner's file, so containment can be resolved
    // without a second query per candidate.
    let mut in_file = s.db.conn.prepare(
        "SELECT name, kind, start_byte, end_byte, start_line, signature, doc
         FROM facts WHERE path = ?1 AND kind <> 'import'
         ORDER BY start_byte, end_byte DESC",
    )?;

    let mut data: Vec<MemberRow> = Vec::new();
    for (owner_path, owner_start, owner_end) in &owners {
        let defs: Vec<FileFact> = in_file
            .query_map([owner_path], |r| {
                Ok(FileFact {
                    name: r.get(0)?,
                    kind: r.get(1)?,
                    start_byte: r.get(2)?,
                    end_byte: r.get(3)?,
                    line: r.get(4)?,
                    signature: r.get(5)?,
                    doc: r.get(6)?,
                })
            })?
            .collect::<std::result::Result<_, _>>()?;

        // Strictly inside the owner, and not the owner itself.
        let inside = |start: i64, end: i64| {
            start >= *owner_start
                && end <= *owner_end
                && (start != *owner_start || end != *owner_end)
        };

        for d in &defs {
            if !inside(d.start_byte, d.end_byte) {
                continue;
            }
            // Direct members only: skip anything nested inside another definition
            // that is itself inside the owner (a method's locals, the fields of
            // an inline struct).
            let nested = defs.iter().any(|o| {
                inside(o.start_byte, o.end_byte)
                    && o.start_byte <= d.start_byte
                    && o.end_byte >= d.end_byte
                    && (o.start_byte != d.start_byte || o.end_byte != d.end_byte)
            });
            if nested {
                continue;
            }
            if let Some(k) = kind {
                if d.kind != k {
                    continue;
                }
            }
            data.push(MemberRow {
                name: d.name.clone(),
                kind: d.kind.clone(),
                path: owner_path.clone(),
                line: d.line,
                signature: declaration_header(&d.kind, d.signature.clone()),
                doc: d.doc.clone(),
                owner: multiple_owners.then(|| format!("{owner_path}:{owner_start}")),
            });
            if limit > 0 && data.len() >= limit {
                break;
            }
        }
        if limit > 0 && data.len() >= limit {
            break;
        }
    }

    Ok(data)
}

/// `outline <file>` — the structure of a file top to bottom: every definition
/// in source order, with its signature and a nesting depth so containment
/// (methods within a class, nested functions) is visible.
pub fn cmd_outline(cwd: &Path, file: &str, docs: bool, out: Out) -> Result<()> {
    let s = Session::open(cwd, false, out)?;

    // Resolve to one file the same way `read` does: exact path wins, else a
    // unique suffix match. Matching `path LIKE '%/README.md'` as well as the
    // exact path merged every README in the repo into one interleaved outline.
    let rel = resolve_file(&s, file)?;
    let mut stmt = s.db.conn.prepare(
        "SELECT name, kind, start_byte, end_byte, start_line, end_line, signature, doc
         FROM facts
         WHERE kind <> 'import' AND path = ?1
         ORDER BY start_byte, end_byte DESC",
    )?;
    let defs: Vec<OutlineDef> = stmt
        .query_map([&rel], |r| {
            let kind: String = r.get(1)?;
            Ok(OutlineDef {
                name: r.get(0)?,
                start_byte: r.get(2)?,
                end_byte: r.get(3)?,
                start_line: r.get(4)?,
                end_line: r.get(5)?,
                signature: declaration_header(&kind, r.get(6)?),
                kind,
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
    raw: bool,
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
    let show = |a: usize, b: usize| {
        if raw {
            print_range_raw(&file_lines, a, b)
        } else {
            print_range(&file_lines, a, b)
        }
    };

    // Verbatim line range.
    if let Some(spec) = lines {
        let (a, b) = parse_range(spec)?;
        show(a, b);
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
        // `--raw` yields exact bytes for an editor's old_string, so a single
        // clean block is what's wanted — refuse an ambiguous multi-match rather
        // than emit two concatenated definitions.
        if raw && ranges.len() > 1 {
            return Err(anyhow!(
                "{} definitions named '{name}' in {rel}; --raw needs one — use --lines A-B",
                ranges.len()
            ));
        }
        for (a, b) in &ranges {
            show(*a as usize, *b as usize);
            if !raw {
                println!(); // blank between/after defs, as the folded view does
            }
        }
        return Ok(());
    }

    // Whole file when asked, or when we can't parse it to know what to fold.
    if full || raw || !crate::facts::has_spans(language_for(&rel).unwrap_or("")) {
        show(1, n);
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

        // A markdown heading owns everything down to the next heading of the
        // same or higher level, so folding its whole range would collapse the
        // document to its title. Fold the prose *between* consecutive headings
        // instead, which leaves the full heading tree visible: a table of
        // contents with each section's length, and `--symbol` to open one.
        let mut heads = s.db.conn.prepare(
            "SELECT start_line FROM facts
             WHERE path = ?1 AND kind = 'heading' ORDER BY start_line",
        )?;
        let starts: Vec<usize> = heads
            .query_map([&rel], |r| r.get::<_, i64>(0))?
            .filter_map(|r| r.ok())
            .map(|l| l.max(1) as usize)
            .collect();
        for (i, &sl) in starts.iter().enumerate() {
            let el = starts.get(i + 1).map(|&s| s - 1).unwrap_or(n);
            if el > sl && el - sl >= MIN_FOLD {
                for ln in (sl + 1)..=el.min(n) {
                    folded[ln - 1] = true;
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

/// Verbatim lines with no line-number prefix — exact source, ready to drop into
/// an editor's search text (the `--raw` view).
fn print_range_raw(lines: &[&str], a: usize, b: usize) {
    for i in a.max(1)..=b {
        if let Some(l) = lines.get(i - 1) {
            println!("{l}");
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
            // Range printed in the exact syntax `--lines` accepts, so the marker
            // is a copy-pasteable next command rather than something to retype.
            println!("        ... {count} lines ({}-{})", start + 1, i);
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
/// an `(importing_file, raw_module)` pair, resolved to file→file edges.
fn load_dep_graph(s: &Session) -> Result<deps::Graph> {
    let files: Vec<String> = {
        let mut stmt = s.db.conn.prepare("SELECT path FROM files ORDER BY path")?;
        let rows = stmt.query_map([], |r| r.get::<_, String>(0))?;
        rows.collect::<std::result::Result<_, _>>()?
    };
    let imports: Vec<(String, String)> = {
        let mut stmt = s
            .db
            .conn
            .prepare("SELECT path, name FROM facts WHERE kind = 'import'")?;
        let rows = stmt.query_map([], |r| Ok((r.get::<_, String>(0)?, r.get::<_, String>(1)?)))?;
        rows.collect::<std::result::Result<_, _>>()?
    };
    Ok(deps::Graph::build(files, imports))
}

/// Import-scoped resolution for the name-based graph commands: attributes a
/// reference in some file to a *definition* of the name via the module-dependency
/// graph. A reference resolves when its file is the definition site (`same-file`)
/// or reaches one through imports (`imported`), several (`ambiguous`), or none
/// (`unresolved` — the strongest false-positive signal, since the file can't see
/// any definition). Heuristic — import resolution is itself best-effort — so it's
/// opt-in (`--resolved`); the default output stays a safe superset. Caches per
/// file. See [[differential-bench-harness]] for how precision is measured.
struct Resolver {
    graph: deps::Graph,
    defs: std::collections::HashSet<String>,
    cache: HashMap<String, &'static str>,
}

impl Resolver {
    fn new(s: &Session, name: &str) -> Result<Resolver> {
        let mut defs = HashSet::new();
        {
            let mut stmt = s.db.conn.prepare(
                "SELECT DISTINCT path FROM facts WHERE name = ?1 AND kind <> 'import'",
            )?;
            let rows = stmt.query_map([name], |r| r.get::<_, String>(0))?;
            for row in rows {
                defs.insert(row?);
            }
        }
        Ok(Resolver {
            graph: load_dep_graph(s)?,
            defs,
            cache: HashMap::new(),
        })
    }

    /// Whether resolution can apply at all — there's a definition to anchor to.
    fn has_defs(&self) -> bool {
        !self.defs.is_empty()
    }

    /// How `file`'s reference to the name connects to a definition of it.
    fn via(&mut self, file: &str) -> &'static str {
        if let Some(v) = self.cache.get(file) {
            return v;
        }
        let v = if self.defs.contains(file) {
            "same-file"
        } else {
            let hits = {
                let reach = self.graph.reachable_from(file);
                self.defs.iter().filter(|d| reach.contains(d.as_str())).count()
            };
            match hits {
                0 => "unresolved",
                1 => "imported",
                _ => "ambiguous",
            }
        };
        self.cache.insert(file.to_string(), v);
        v
    }
}

/// Apply import-scoped resolution to a list of reference-bearing rows: when
/// `resolved`, tag each with its `via` and drop the ones whose file can't reach
/// any definition (`unresolved`). A no-op when `resolved` is false, or when the
/// name has no definition to anchor to (so it never silently empties a result).
fn resolve_rows<T>(
    s: &Session,
    name: &str,
    resolved: bool,
    rows: Vec<T>,
    path_of: impl Fn(&T) -> &str,
    set_via: impl Fn(&mut T, Option<String>),
) -> Result<Vec<T>> {
    if !resolved {
        return Ok(rows);
    }
    let mut r = Resolver::new(s, name)?;
    if !r.has_defs() {
        return Ok(rows); // nothing to anchor resolution to — leave untouched
    }
    let mut out = Vec::with_capacity(rows.len());
    for mut row in rows {
        let v = r.via(path_of(&row));
        if v == "unresolved" {
            continue;
        }
        set_via(&mut row, Some(v.to_string()));
        out.push(row);
    }
    Ok(out)
}

/// `deps <file> [--depth N]` — the internal files a file depends on, resolved
/// from its imports; `--cycles` reports every import cycle in the repo instead.
pub fn cmd_deps(
    cwd: &Path,
    file: Option<&str>,
    depth: usize,
    cycles: bool,
    out: Out,
) -> Result<()> {
    let s = Session::open(cwd, false, out)?;
    let graph = load_dep_graph(&s)?;

    if cycles {
        let data: Vec<CycleRow> = graph
            .cycles()
            .into_iter()
            .map(|c| CycleRow { length: c.len(), cycle: c })
            .collect();
        return s.emit(data);
    }

    let file = file.ok_or_else(|| anyhow!("deps needs a <file> (or --cycles)"))?;
    // Resolve the query to an indexed file path: exact, else unique suffix match.
    let target = resolve_dep_target(&graph, &s, file)?;
    let hops = graph.deps(&target, depth.max(1));
    let data: Vec<DepRow> = hops
        .into_iter()
        .map(|h| DepRow { path: h.path, module: h.module, depth: h.depth })
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
    let mut stmt = s.db.conn.prepare("SELECT path FROM files ORDER BY path")?;
    let paths: Vec<String> = stmt
        .query_map([], |r| r.get::<_, String>(0))?
        .collect::<std::result::Result<_, _>>()?;
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
    // Stream the bodies into the matcher one at a time (it re-parses each and
    // keeps only matches), so a big single-language repo isn't all held in memory
    // at once — the astgrep matcher takes an iterator, so no Vec is needed.
    let mut stmt = s.db.conn.prepare(&sql)?;
    let files = stmt
        .query_map(params.as_slice(), |r| {
            Ok((r.get::<_, String>(0)?, r.get::<_, String>(1)?))
        })?
        .filter_map(|r| r.ok());

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
    /// Search a buffer piped in on stdin instead of the repository index.
    /// Explicit only, never inferred from whether stdin is a TTY.
    pub stdin: bool,
}

/// `search <pattern>` — content search over indexed, non-binary files, with each
/// match annotated by its enclosing definition. Regex by default (linear-time,
/// no catastrophic backtracking); `--fixed` for a literal string. A strict
/// superset of grep: same regex engine, plus structure grep cannot see.
pub fn cmd_search(cwd: &Path, args: SearchArgs, out: Out) -> Result<()> {
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
    // A `--glob` set becomes a per-row path predicate for the streaming scan.
    let glob = if args.globs.is_empty() {
        None
    } else {
        Some(GlobFilter::build(args.globs)?)
    };
    let glob_accept = glob.as_ref().map(|gf| move |p: &str| gf.accepts(p));
    let path_accept = glob_accept
        .as_ref()
        .map(|f| f as &dyn Fn(&str) -> bool);

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
        call_only: false,
        qualifier: None,
    };

    // Piped input never touches the index: no repo discovery, no refresh, no
    // database. That keeps codeq usable as a plain filter outside a repository,
    // and keeps this branch entirely off the indexed path the MCP server uses.
    if args.stdin {
        let data = scan_stdin(&re, &opts, args.language, search::STDIN_LABEL)?;
        return emit_sessionless(&data, out);
    }

    let s = Session::open(cwd, false, out)?;
    let data = run_search(
        &s.db.conn,
        &re,
        match_query.as_deref(),
        args.path,
        args.language,
        path_accept,
        &opts,
    )?;
    s.emit(data)
}

#[derive(Serialize)]
struct ReferenceRow {
    #[serde(flatten)]
    hit: Hit,
    /// True when this line is the definition site of the symbol (not a use).
    #[serde(skip_serializing_if = "is_false")]
    definition: bool,
    /// Import-scoped attribution (only under `--resolved`): same-file / imported
    /// / ambiguous.
    #[serde(skip_serializing_if = "Option::is_none")]
    via: Option<String>,
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
    resolved: bool,
    call_only: bool,
    qualifier: Option<&str>,
    out: Out,
) -> Result<()> {
    let s = Session::open(cwd, false, out)?;

    let expr = format!(r"\b{}\b", regex::escape(name));
    let re = regex::Regex::new(&expr).map_err(|e| anyhow!("invalid name: {e}"))?;
    // The name is a substring of every reference, so the trigram index narrows
    // to files that contain it (the \b matcher then confirms word boundaries).
    let match_query = like_of(name);
    // A call site or qualified access is *code* — a doc-comment example like
    // `fn build()` is not one — so the position filters imply code-only.
    let in_filter = if call_only || qualifier.is_some() {
        InFilter::Code
    } else {
        InFilter::Any
    };
    let opts = ScanOpts {
        before: 0,
        after: 0,
        out: OutMode::Matches,
        in_filter,
        limit,
        call_only,
        qualifier: qualifier.map(str::to_string),
    };
    let (hits, truncated) =
        match run_search(&s.db.conn, &re, match_query.as_deref(), path, language, None, &opts)? {
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

    let references: Vec<ReferenceRow> = hits
        .into_iter()
        .map(|h| {
            let definition = defsites.contains(&(h.path.clone(), h.line as i64));
            ReferenceRow { hit: h, definition, via: None }
        })
        .collect();
    let references = resolve_rows(
        &s,
        name,
        resolved,
        references,
        |r| r.hit.path.as_str(),
        |r, v| r.via = v,
    )?;

    let note = note_if_empty(&s, &references, name, "references", path)?;
    s.emit_noted(ReferencesResult { references, truncated }, note)
}

#[derive(Serialize)]
struct ImplementorRow {
    /// The implementing/deriving type.
    subtype: String,
    /// How it relates: "implements" or "extends".
    relation: String,
    path: String,
    line: i64,
    #[serde(skip_serializing_if = "Option::is_none")]
    language: Option<String>,
}

/// `implementors <type>` — the types that implement or extend a trait /
/// interface / base class. Covers the inheritance edges the call graph doesn't:
/// `impl T for X` (Rust), `class X extends Y` / `implements I` (Java/TS/PHP/…),
/// `class X(Base)` (Python), `class X < Y` (Ruby), base lists (C++/C#). Go
/// (structural typing) and C have none. Name-based, like the other graph
/// commands.
pub fn cmd_implementors(
    cwd: &Path,
    name: &str,
    relation: Option<&str>,
    path: Option<&str>,
    limit: usize,
    out: Out,
) -> Result<()> {
    let s = Session::open(cwd, false, out)?;
    let filters = Filters::new().eq("relation", relation).like("path", path);
    let sql = format!(
        "SELECT subtype, relation, path, line, language
         FROM impls WHERE supertype = ?{} ORDER BY path, line{}",
        filters.sql(),
        limit_clause(limit),
    );
    let mut stmt = s.db.conn.prepare(&sql)?;
    let rows = stmt.query_map(params_with(&[&name], &filters).as_slice(), |r| {
        Ok(ImplementorRow {
            subtype: r.get(0)?,
            relation: r.get(1)?,
            path: r.get(2)?,
            line: r.get(3)?,
            language: r.get(4)?,
        })
    })?;
    let data: Vec<ImplementorRow> = rows.collect::<std::result::Result<_, _>>()?;
    let note = note_if_empty(&s, &data, name, "implementors or subtypes", path)?;
    s.emit_noted(data, note)
}

#[derive(Serialize)]
struct SupertypeRow {
    /// The trait / interface / base class this type extends or implements.
    supertype: String,
    /// How it relates: "implements" or "extends".
    relation: String,
    path: String,
    line: i64,
    #[serde(skip_serializing_if = "Option::is_none")]
    language: Option<String>,
}

/// `supertypes <type>` — the traits / interfaces / base classes a type extends
/// or implements (the reverse of `implementors`): `impl S for X` shows `S` for
/// `supertypes X`, `class X extends Y implements I` shows `Y` and `I`. The
/// supertype names, as written at the declaration — not transitively resolved.
pub fn cmd_supertypes(
    cwd: &Path,
    name: &str,
    relation: Option<&str>,
    path: Option<&str>,
    limit: usize,
    out: Out,
) -> Result<()> {
    let s = Session::open(cwd, false, out)?;
    let filters = Filters::new().eq("relation", relation).like("path", path);
    let sql = format!(
        "SELECT supertype, relation, path, line, language
         FROM impls WHERE subtype = ?{} ORDER BY path, line{}",
        filters.sql(),
        limit_clause(limit),
    );
    let mut stmt = s.db.conn.prepare(&sql)?;
    let rows = stmt.query_map(params_with(&[&name], &filters).as_slice(), |r| {
        Ok(SupertypeRow {
            supertype: r.get(0)?,
            relation: r.get(1)?,
            path: r.get(2)?,
            line: r.get(3)?,
            language: r.get(4)?,
        })
    })?;
    let data: Vec<SupertypeRow> = rows.collect::<std::result::Result<_, _>>()?;
    let note = note_if_empty(&s, &data, name, "supertypes (it extends nothing)", path)?;
    s.emit_noted(data, note)
}

#[derive(Serialize)]
struct UnusedRow {
    name: String,
    kind: String,
    path: String,
    line: i64,
    #[serde(skip_serializing_if = "Option::is_none")]
    visibility: Option<String>,
}

/// Whether `name` occurs anywhere in the repo outside its own definition
/// site(s). Conservative: a word-boundary mention in code, a comment, or a
/// string all count as a use, so `unused` only ever flags a name with *zero*
/// occurrences elsewhere. Name-based (not type-resolved), like `references`.
fn name_is_used(
    s: &Session,
    name: &str,
    defsites: Option<&HashSet<(String, i64)>>,
) -> Result<bool> {
    let re = regex::Regex::new(&format!(r"\b{}\b", regex::escape(name)))
        .map_err(|e| anyhow!("invalid name: {e}"))?;
    let like = like_of(name);
    let opts = ScanOpts {
        before: 0,
        after: 0,
        out: OutMode::Matches,
        // A name mentioned only in a comment ("TODO: wire up foo later") used to
        // count as a use and mask genuinely dead code. A comment is never a use.
        // String mentions still count: reflection, serde renames, config keys and
        // SQL identifiers are real uses that live in string literals.
        in_filter: InFilter::NotComment,
        limit: 0,
        call_only: false,
        qualifier: None,
    };
    let hits = match run_search(&s.db.conn, &re, like.as_deref(), None, None, None, &opts)? {
        SearchData::Matches { matches, .. } => matches,
        _ => unreachable!("unused always uses Matches mode"),
    };
    let empty = HashSet::new();
    let ds = defsites.unwrap_or(&empty);
    Ok(hits
        .iter()
        .any(|h| !ds.contains(&(h.path.clone(), h.line as i64))))
}

/// `unused [<path>]` — definitions with no reference anywhere outside their own
/// definition site (candidate dead code). Reports `visibility` so a public
/// symbol (possibly part of an external API, not dead) is distinguishable from a
/// private one (safe to remove). Name-based and conservative: a mention in a
/// comment or string counts as a use, and a same-named symbol elsewhere masks
/// it, so this under-reports rather than misleads. `--path`/`--kind` scope what
/// is *reported*; the usage check is always repo-wide. Test-file definitions are
/// excluded by default (framework entry points look unused) unless `include_tests`.
pub fn cmd_unused(
    cwd: &Path,
    path: Option<&str>,
    kind: Option<&str>,
    include_tests: bool,
    limit: usize,
    out: Out,
) -> Result<()> {
    let s = Session::open(cwd, false, out)?;

    // Every definition site keyed by name (repo-wide) so a use can be told from
    // a definition, and a name defined in >1 place is handled.
    let mut defsites: HashMap<String, HashSet<(String, i64)>> = HashMap::new();
    {
        let mut stmt = s.db.conn.prepare(
            "SELECT name, path, start_line FROM facts
             WHERE kind <> 'import' AND name IS NOT NULL",
        )?;
        let rows = stmt.query_map([], |r| {
            Ok((r.get::<_, String>(0)?, r.get::<_, String>(1)?, r.get::<_, i64>(2)?))
        })?;
        for row in rows {
            let (n, p, l) = row?;
            defsites.entry(n).or_default().insert((p, l));
        }
    }

    // The definitions eligible to be reported (scoped by path/kind). A markdown
    // heading is prose, not an API: nothing ever "calls" it, so every section in
    // the repo would be reported as dead. Excluded unless asked for by --kind.
    let filters = Filters::new().like("path", path).eq("kind", kind);
    let exclude_headings = if kind == Some("heading") {
        ""
    } else {
        " AND kind <> 'heading'"
    };
    let sql = format!(
        "SELECT name, kind, path, start_line, visibility FROM facts
         WHERE kind <> 'import' AND name IS NOT NULL{exclude_headings}{} ORDER BY path, start_line",
        filters.sql(),
    );
    let candidates: Vec<UnusedRow> = {
        let mut stmt = s.db.conn.prepare(&sql)?;
        let rows = stmt.query_map(params_with(&[], &filters).as_slice(), |r| {
            Ok(UnusedRow {
                name: r.get(0)?,
                kind: r.get(1)?,
                path: r.get(2)?,
                line: r.get(3)?,
                visibility: r.get(4)?,
            })
        })?;
        rows.collect::<std::result::Result<_, _>>()?
    };

    // Scan each distinct name once; keep the defs whose name is never used.
    let mut used: HashMap<String, bool> = HashMap::new();
    let mut data: Vec<UnusedRow> = Vec::new();
    for cand in candidates {
        // Test functions are invoked by the test runner (by discovery/attribute),
        // not referenced by name, so they always look unused — exclude them by
        // default so real dead code isn't buried under every test.
        if !include_tests && is_test_path(&cand.path) {
            continue;
        }
        let is_used = match used.get(&cand.name) {
            Some(&u) => u,
            None => {
                let u = name_is_used(&s, &cand.name, defsites.get(&cand.name))?;
                used.insert(cand.name.clone(), u);
                u
            }
        };
        if !is_used {
            data.push(cand);
            if limit > 0 && data.len() >= limit {
                break;
            }
        }
    }
    // Nothing unused is a real answer, not a miss: say so, so an empty result is
    // not read as "the command didn't work".
    let note = data.is_empty().then(|| match path {
        Some(p) => format!("no unused definitions in '{p}' (every one is referenced somewhere)"),
        None => "no unused definitions (every one is referenced somewhere)".to_string(),
    });
    s.emit_noted(data, note)
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
#[derive(Serialize)]
struct TestRow {
    path: String,
    line: i64,
    name: String,
    kind: String,
}

/// The enclosing test functions (path, test-name) that reference `name`,
/// excluding `name`'s own definition. Shared by `tests` and `affected`.
fn test_sites(
    s: &Session,
    name: &str,
    path: Option<&str>,
) -> Result<std::collections::BTreeSet<(String, String)>> {
    let expr = format!(r"\b{}\b", regex::escape(name));
    let re = regex::Regex::new(&expr).map_err(|e| anyhow!("invalid name: {e}"))?;
    let opts = ScanOpts {
        before: 0,
        after: 0,
        out: OutMode::Matches,
        in_filter: InFilter::Any,
        limit: 0,
        call_only: false,
        qualifier: None,
    };
    let hits = match run_search(&s.db.conn, &re, like_of(name).as_deref(), path, None, None, &opts)? {
        SearchData::Matches { matches, .. } => matches,
        _ => unreachable!("tests always uses Matches mode"),
    };

    let mut tests: std::collections::BTreeSet<(String, String)> = std::collections::BTreeSet::new();
    for h in &hits {
        let sym = match &h.symbol {
            Some(sy) if sy != name => sy,
            _ => continue,
        };
        let is_fn = matches!(h.kind.as_deref(), Some("function") | Some("method"));
        if is_fn && (is_test_path(&h.path) || is_test_name(sym)) {
            tests.insert((h.path.clone(), sym.clone()));
        }
    }
    Ok(tests)
}

/// Resolve (path, test-name) sites to their definition rows (line, kind), sorted
/// and de-duped; returns (rows, truncated).
fn resolve_test_rows(
    s: &Session,
    tests: &std::collections::BTreeSet<(String, String)>,
    limit: usize,
) -> Result<(Vec<TestRow>, bool)> {
    let mut rows: Vec<TestRow> = Vec::new();
    let names: Vec<String> = tests
        .iter()
        .map(|(_, n)| n.clone())
        .collect::<HashSet<_>>()
        .into_iter()
        .collect();
    for chunk in names.chunks(500) {
        if chunk.is_empty() {
            continue;
        }
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
    Ok((rows, truncated))
}

pub fn cmd_tests(cwd: &Path, name: &str, path: Option<&str>, limit: usize, out: Out) -> Result<()> {
    let s = Session::open(cwd, false, out)?;
    let tests = test_sites(&s, name, path)?;
    let (rows, truncated) = resolve_test_rows(&s, &tests, limit)?;

    #[derive(Serialize)]
    struct TestsData {
        tests: Vec<TestRow>,
        #[serde(skip_serializing_if = "std::ops::Not::not")]
        truncated: bool,
    }
    let note = note_if_empty(&s, &rows, name, "tests referencing it", path)?;
    s.emit_noted(
        TestsData {
            tests: rows,
            truncated,
        },
        note,
    )
}

/// A definition pulled into the blast radius: a transitive caller of a changed
/// symbol, tagged with its hop distance from the change.
#[derive(serde::Serialize)]
struct ImpactNode {
    name: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    kind: Option<String>,
    path: String,
    line: i64,
    depth: usize,
}

/// A test file to re-run, with how many of its test-like functions reference
/// something in scope. File granularity is what you act on ("run these files")
/// and avoids listing every individual test function (and test-file helper).
#[derive(serde::Serialize)]
struct AffectedTestFile {
    path: String,
    tests: usize,
}

#[derive(serde::Serialize)]
struct AffectedResult {
    changed: Vec<ChangedSymbolRow>,
    #[serde(skip_serializing_if = "Vec::is_empty")]
    impacted: Vec<ImpactNode>,
    tests: Vec<AffectedTestFile>,
    #[serde(skip_serializing_if = "std::ops::Not::not")]
    truncated: bool,
}

/// `affected --compare-to <ref>` — for a diff: the definitions it touched, their
/// transitive callers (the blast radius), and the tests exercising any of them —
/// "what should I re-check and re-run for this change". Composes `changed
/// --symbols` + a caller BFS + `tests`; name-based like the rest of the graph.
pub fn cmd_affected(
    cwd: &Path,
    compare_to: &str,
    depth: usize,
    limit: usize,
    out: Out,
) -> Result<()> {
    let s = Session::open(cwd, false, out)?;

    let changed = touched_defs(&s, compare_to)?;

    // Blast radius: BFS up the call tree from the changed definition names. Each
    // caller is reported once at its shortest hop distance.
    let mut impacted: Vec<ImpactNode> = Vec::new();
    let mut in_scope: HashSet<String> = changed.iter().map(|c| c.name.clone()).collect();
    let mut reported: HashSet<(String, String)> = HashSet::new();
    let mut frontier: Vec<String> = in_scope.iter().cloned().collect();
    frontier.sort();
    'walk: for d in 1..=depth {
        let mut next: Vec<String> = Vec::new();
        for sym in &frontier {
            for c in callers_of(&s, sym, None, false, None, false)? {
                let Some(cname) = c.symbol.clone() else { continue };
                if !reported.insert((c.path.clone(), cname.clone())) {
                    continue;
                }
                impacted.push(ImpactNode {
                    name: cname.clone(),
                    kind: c.kind,
                    path: c.path,
                    line: c.line as i64,
                    depth: d,
                });
                if in_scope.insert(cname.clone()) {
                    next.push(cname);
                }
                if impacted.len() >= HOP_CAP {
                    break 'walk;
                }
            }
        }
        frontier = next;
        frontier.sort();
        if frontier.is_empty() {
            break;
        }
    }
    impacted.sort_by(|a, b| {
        a.depth
            .cmp(&b.depth)
            .then(a.path.cmp(&b.path))
            .then(a.line.cmp(&b.line))
    });

    // Test files exercising anything in scope (changed symbols ∪ blast radius),
    // with a per-file count of matching test-like functions — the actionable
    // "what to run", without one row per individual test.
    let mut sites: std::collections::BTreeSet<(String, String)> = std::collections::BTreeSet::new();
    for name in &in_scope {
        for site in test_sites(&s, name, None)? {
            sites.insert(site);
        }
    }
    let mut by_file: BTreeMap<String, usize> = BTreeMap::new();
    for (path, _test) in &sites {
        *by_file.entry(path.clone()).or_insert(0) += 1;
    }
    let mut tests: Vec<AffectedTestFile> = by_file
        .into_iter()
        .map(|(path, tests)| AffectedTestFile { path, tests })
        .collect();
    let truncated = limit > 0 && tests.len() > limit;
    if truncated {
        tests.truncate(limit);
    }

    s.emit(AffectedResult {
        changed,
        impacted,
        tests,
        truncated,
    })
}

/// `install` — register codeq's MCP server with detected AI coding agents. Writes
/// to each agent's own config (in the home directory), never to a repository, so
/// it needs no index/Session. Honours `--json`/`--pretty` for its own summary.
pub fn cmd_install(
    dry_run: bool,
    uninstall: bool,
    agent: Option<&str>,
    out: Out,
) -> Result<()> {
    let home = crate::install::home_dir()?;
    let bin = std::env::current_exe()
        .ok()
        .map(|p| p.to_string_lossy().into_owned())
        .unwrap_or_else(|| "codeq".to_string());
    let opts = crate::install::Options {
        dry_run,
        uninstall,
        only: agent.map(String::from),
        bin,
    };
    let actions = crate::install::apply(&home, &opts)?;

    if out.json {
        let data = serde_json::json!({
            "schema": crate::envelope::RESULT_SCHEMA,
            "data": actions,
        });
        let s = if out.pretty {
            serde_json::to_string_pretty(&data)?
        } else {
            serde_json::to_string(&data)?
        };
        println!("{s}");
    } else {
        for a in &actions {
            let note = a
                .note
                .as_deref()
                .map(|n| format!("  ({n})"))
                .unwrap_or_default();
            println!("{:<12}  {:<15}  {}{}", a.agent, a.action, a.config, note);
        }
        let touched = actions
            .iter()
            .filter(|a| !matches!(a.action, "skipped" | "already-current" | "already-absent"))
            .count();
        if touched == 0 && !uninstall {
            println!(
                "\nNo agents updated. Detected agents are configured automatically; \
                 target one explicitly with `--agent <name>` (claude-code, cursor, \
                 windsurf, gemini)."
            );
        } else if !dry_run && !uninstall && touched > 0 {
            println!("\nRestart the agent(s) above to pick up the codeq MCP server.");
        }
    }
    Ok(())
}

fn is_false(b: &bool) -> bool {
    !*b
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
        // Kept in step with the CLI's own command list: a stale entry here is
        // worse than none, because an agent takes it as the contract.
        "commands": [
            "index", "files", "changed", "affected", "stats", "map",
            "symbols", "definitions", "api", "members", "outline", "at", "read",
            "callers", "callees", "path", "about", "explore", "references",
            "implementors", "supertypes", "unused", "tests",
            "imports", "importers", "deps", "astgrep", "search", "context",
            "capabilities", "mcp", "install",
        ],
        // Languages with symbol + import extraction.
        "extraction_languages": ["rust", "python", "javascript", "typescript", "tsx", "go", "java", "ruby", "c", "cpp", "csharp", "php", "bash", "lua", "scala", "kotlin", "swift", "dart", "zig", "elixir", "solidity"],
        // Languages recognised for file classification (extraction may be absent).
        "classified_languages": [
            "rust", "javascript", "typescript", "tsx", "python", "go", "java",
            "json", "yaml", "toml", "markdown", "sql", "bash", "lua", "scala",
            "kotlin", "swift", "dart", "zig", "elixir", "solidity", "c", "cpp",
            "csharp", "php", "ruby",
        ],
        "fact_kinds": [
            "function", "method", "struct", "enum", "union", "trait", "module",
            "constant", "typealias", "macro", "class", "interface", "type", "import",
            "field", "variant",
            "field", "variant",
        ],
        "notes": {
            "imports": "raw syntactic module strings; not resolved",
            "context_tokenizer": "approx:4-bytes-per-token",
            "freshness": "every command refreshes changed files before answering",
        },
    });
    emit_sessionless(&data, out)
}

/// Emit a result that has no session behind it — `capabilities`, and anything
/// scanning piped input rather than the index. Honors the global output mode
/// like every other command: JSON on --json/--meta/--pretty, otherwise codeq's
/// compact text. There is no `repo`/`index` block because no index was read.
fn emit_sessionless<T: Serialize>(data: &T, out: Out) -> Result<()> {
    if out.json || out.meta || out.pretty {
        let value = serde_json::json!({ "schema": RESULT_SCHEMA, "data": data });
        if out.pretty {
            println!("{}", serde_json::to_string_pretty(&value)?);
        } else {
            println!("{}", serde_json::to_string(&value)?);
        }
    } else {
        print!(
            "{}",
            crate::envelope::render_data_text(&serde_json::to_value(data)?)
        );
    }
    Ok(())
}
