//! Command implementations for the v1 proving spine:
//! `index`, `files`, `changed`, `map`.

use std::collections::{BTreeMap, HashMap, HashSet};
use std::io::Write;
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
    self, innermost, like_of, load_file_defs, regex_prefilter, run_search, scan_stdin, FileDef,
    Hit, InFilter, OutMode, ScanOpts, SearchData,
};

/// Output rendering options parsed from global CLI flags.
#[derive(Clone, Default)]
pub struct Out {
    pub json: bool,
    pub meta: bool,
    pub pretty: bool,
    pub fields: Option<Vec<String>>,
    pub group: bool,
    pub max_per_file: Option<usize>,
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

/// Set by `--root`: the tree was named explicitly, so discovery must take it
/// literally rather than resolving to an enclosing repository.
static PINNED_ROOT: std::sync::atomic::AtomicBool = std::sync::atomic::AtomicBool::new(false);

pub fn pin_root() {
    PINNED_ROOT.store(true, std::sync::atomic::Ordering::Relaxed);
}

impl Session {
    fn open(cwd: &Path, rebuild: bool, out: Out) -> Result<Session> {
        let started = Instant::now();
        let repo = Repo::at(cwd, PINNED_ROOT.load(std::sync::atomic::Ordering::Relaxed))?;
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
            group: self.out.group,
            max_per_file: self.out.max_per_file,
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
    let mut stmt =
        s.db.conn
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
/// Why a path-scoped listing came back empty: because the filter selected no
/// indexed file, or because the files it selected genuinely hold nothing.
///
/// Eleven of twenty-six commands returned empty with no note, no stderr and exit
/// 0. Most were this one sentence with a different noun, which is why they are
/// one helper rather than eleven bespoke messages — the shared form is also what
/// stops the list regrowing. A bare pattern miss stays silent on purpose: an
/// empty `search zzz` is unambiguous, and a note on every one would be noise.
fn note_if_scope_empty(s: &Session, path: Option<&str>, what: &str) -> Result<Option<String>> {
    let Some(p) = path else { return Ok(None) };
    let n: i64 = s.db.conn.query_row(
        "SELECT COUNT(*) FROM files WHERE path LIKE ?1",
        [format!("%{p}%")],
        |r| r.get(0),
    )?;
    Ok(Some(if n == 0 {
        format!(
            "no indexed file matches '{p}' — the filter selected nothing, so this says nothing about {what}"
        )
    } else {
        format!("{n} indexed file(s) match '{p}', but none has {what}")
    }))
}

fn note_if_empty<T>(
    s: &Session,
    rows: &[T],
    name: &str,
    relation: &str,
    path: Option<&str>,
) -> Result<Option<String>> {
    note_if_empty_feat(s, rows, name, relation, path, None)
}

/// As [`note_if_empty`], but `feature` names the extraction a command depends on
/// ("relations", "members"). When the symbol's own language does not support it,
/// the answer is "codeq cannot see this here", which is a different fact from
/// "there are none" — and the one an agent needs, because otherwise it takes an
/// unsupported language as evidence of absence.
fn note_if_empty_feat<T>(
    s: &Session,
    rows: &[T],
    name: &str,
    relation: &str,
    path: Option<&str>,
    feature: Option<&str>,
) -> Result<Option<String>> {
    if !rows.is_empty() {
        return Ok(None);
    }
    let sites = definition_sites(s, name)?;
    // `--path` scopes the question, so it has to scope the answer too. Without
    // this the note reported `sites[0]` repo-wide: asked about `E` in `z.ts` it
    // answered "'E' is defined at A.cs:1 (+1 more definitions)", naming the one
    // file the caller had explicitly ruled out — and then picked *that* file's
    // language for the support check below.
    let sites = match path {
        Some(filter) if !sites.is_empty() => {
            let scoped: Vec<(String, i64)> = sites
                .iter()
                .filter(|(p, _)| p.contains(filter))
                .cloned()
                .collect();
            if scoped.is_empty() {
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
            scoped
        }
        _ => sites,
    };
    if let (Some(feature), Some((p, _))) = (feature, sites.first()) {
        if let Some(lang) = language_for(p) {
            let unsupported = crate::facts::support(lang).is_some_and(|sup| match feature {
                "relations" => !sup.relations,
                "members" => !sup.members,
                _ => false,
            });
            if unsupported {
                let why = match feature {
                    "relations" => format!(
                        "codeq extracts no inheritance edges for {lang}; this is not evidence that '{name}' has none"
                    ),
                    _ => format!(
                        "codeq extracts no field or variant declarations for {lang}, so only methods would be reported for '{name}', and it has none"
                    ),
                };
                return Ok(Some(why));
            }
        }
    }
    if sites.is_empty() {
        // A name with no *definition* here is not necessarily a name that does
        // not exist: `HTTPException`, `Path`, every third-party import is
        // declared outside the repo — and so is every local, parameter and
        // destructured binding, none of which codeq records as definitions.
        //
        // The first version of this note guessed: "declared elsewhere (a
        // dependency, or a built-in)". For `areaBank`, a *function parameter*
        // sitting in a file two lines away, that sent the reader looking outward
        // at dependencies — wrong in the most expensive available direction. The
        // fix is to stop guessing which: list the possibilities and let the
        // reader pick. A note that states the fact beats a note that picks a
        // cause, and this is the second time that lesson has been filed.
        if name_is_used(s, name)? {
            let scope = path
                .map(|f| format!(" and nothing matches --path {f}"))
                .unwrap_or_default();
            return Ok(Some(format!(
                "'{name}' has no definition codeq indexes — referenced here, but declared somewhere codeq does not record it: a local, a parameter, a destructured binding, or a symbol from a dependency{scope}"
            )));
        }
        let near = nearest_names(s, name)?;
        let hint = if near.is_empty() {
            String::new()
        } else {
            format!("; closest: {}", near.join(", "))
        };
        return Ok(Some(format!("no symbol named '{name}'{hint}")));
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
    /// Parser recovery nodes in the syntax tree used for facts. Absent for
    /// intentionally non-parser-backed formats and binary files.
    #[serde(skip_serializing_if = "Option::is_none")]
    parse_errors: Option<i64>,
    #[serde(skip_serializing_if = "is_false")]
    parse_recovered: bool,
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

    /// `AND {col} LIKE '%needle%'` when `needle` is Some. Treat SQL LIKE
    /// metacharacters as path characters, not user-supplied wildcards.
    fn like(mut self, col: &str, needle: Option<&str>) -> Self {
        if let Some(n) = needle {
            self.sql.push_str(&format!(" AND {col} LIKE ? ESCAPE '\\'"));
            let escaped = n
                .replace('\\', "\\\\")
                .replace('%', "\\%")
                .replace('_', "\\_");
            self.args.push(format!("%{escaped}%"));
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

/// Definition sites of one exact name, as `(path, start_line)`.
///
/// Both `callers` and `references` need this to tell a declaration apart from a
/// use of the same name — the one place where two commands genuinely ask the
/// database the same question, so it is worth one function rather than two
/// copies that could drift on the `kind <> 'import'` clause.
/// Where one name is declared, and whether a given occurrence *is* the
/// declaration.
///
/// Not "the same line as the fact": a definition's recorded line is its node's
/// first line, and in C#, Java, Kotlin and PHP the annotation lives inside the
/// declaration node, so that line holds `[Fact]` and the name sits below it.
/// Comparing lines therefore missed the declaration in the formatting nearly
/// everyone writes, and `callers` reported an annotated method as its own
/// caller.
///
/// The declaration is the *first* occurrence of the name inside the definition's
/// extent. A later occurrence in the same extent is a recursive use and stays
/// one, so each extent is claimed only once. Callers must feed occurrences in
/// source order for that to hold.
struct DeclSites {
    extents: Vec<(String, i64, i64)>,
    claimed: HashSet<(String, i64)>,
}

impl DeclSites {
    fn load(conn: &rusqlite::Connection, name: &str) -> Result<Self> {
        let mut stmt = conn.prepare(
            "SELECT path, start_line, end_line FROM facts
             WHERE name = ?1 AND kind <> 'import'",
        )?;
        let rows = stmt.query_map([name], |r| {
            Ok((
                r.get::<_, String>(0)?,
                r.get::<_, i64>(1)?,
                r.get::<_, i64>(2)?,
            ))
        })?;
        Ok(Self {
            extents: rows.collect::<std::result::Result<Vec<_>, _>>()?,
            claimed: HashSet::new(),
        })
    }

    fn is_declaration(&mut self, path: &str, line: i64) -> bool {
        let hit = self
            .extents
            .iter()
            .find(|(p, start, end)| p == path && (*start..=*end).contains(&line));
        match hit {
            Some((p, start, _)) => {
                let key = (p.clone(), *start);
                self.claimed.insert(key)
            }
            None => false,
        }
    }
}

/// Every definition site in the repo, as `(name, path, start_line)`.
///
/// Yields rows instead of returning a built structure because the two callers
/// want different shapes of the same scan: `unused` groups sites by name to tell
/// a use from a declaration, `hotspots` needs both a per-name count and a flat
/// site set. Sharing the query keeps them agreeing on what counts as a definition.
fn for_each_def_site(
    conn: &rusqlite::Connection,
    mut f: impl FnMut(String, String, i64, i64),
) -> Result<()> {
    let mut stmt = conn.prepare(
        "SELECT name, path, start_line, end_line FROM facts
         WHERE kind <> 'import' AND name IS NOT NULL",
    )?;
    let rows = stmt.query_map([], |r| {
        Ok((
            r.get::<_, String>(0)?,
            r.get::<_, String>(1)?,
            r.get::<_, i64>(2)?,
            r.get::<_, i64>(3)?,
        ))
    })?;
    for row in rows {
        let (name, path, start, end) = row?;
        f(name, path, start, end);
    }
    Ok(())
}

/// Every indexed file path, sorted — the node set for the dependency graph.
fn all_file_paths(conn: &rusqlite::Connection) -> Result<Vec<String>> {
    let mut stmt = conn.prepare("SELECT path FROM files ORDER BY path")?;
    let rows = stmt.query_map([], |r| r.get::<_, String>(0))?;
    Ok(rows.collect::<std::result::Result<Vec<_>, _>>()?)
}

/// A `LIMIT n` clause, or empty when `limit` is 0 (unlimited).
fn limit_clause(limit: usize) -> String {
    if limit > 0 {
        format!(" LIMIT {limit}")
    } else {
        String::new()
    }
}

/// `LIMIT n+1`, so a page that fills exactly can be told from one that was cut.
///
/// A listing that truncates in SQL does not otherwise know it truncated, which
/// is why these commands cut in silence: `symbols` defaults to 200 and a real
/// repo had 1,228. One extra row is the whole cost of being able to say so.
fn limit_probe_clause(limit: usize) -> String {
    if limit > 0 {
        format!(" LIMIT {}", limit + 1)
    } else {
        String::new()
    }
}

/// Drop the probe row if it came back, and say the answer is partial.
fn take_limit<T>(rows: &mut Vec<T>, limit: usize) -> Option<String> {
    if limit == 0 || rows.len() <= limit {
        return None;
    }
    rows.truncate(limit);
    Some(format!(
        "showing the first {limit} — stopped at --limit; rerun with a higher --limit (or 0) for the rest"
    ))
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

    let filters = Filters::new()
        .like("path", pattern)
        .eq("language", language);
    let sql = format!(
        "SELECT path, language, size_bytes, parse_errors, parse_recovered FROM files WHERE 1=1{} ORDER BY path{}",
        filters.sql(),
        limit_probe_clause(limit),
    );
    let mut stmt = s.db.conn.prepare(&sql)?;
    let rows = stmt.query_map(params_with(&[], &filters).as_slice(), |r| {
        Ok(FileRow {
            path: r.get(0)?,
            language: r.get(1)?,
            size_bytes: r.get(2)?,
            parse_errors: r.get(3)?,
            parse_recovered: r.get(4)?,
        })
    })?;
    let mut data: Vec<FileRow> = rows.collect::<std::result::Result<_, _>>()?;
    let note = take_limit(&mut data, limit);
    s.emit_noted(data, note)
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
    #[serde(rename = "line")]
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
        let indexed: bool =
            s.db.conn
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
    #[serde(rename = "line")]
    start_line: i64,
    end_line: i64,
    /// Only populated when the caller passes --signatures.
    #[serde(skip_serializing_if = "Option::is_none")]
    signature: Option<String>,
    /// The type a `field`/`variant` belongs to; absent for everything else.
    #[serde(skip_serializing_if = "Option::is_none")]
    owner: Option<String>,
}

pub fn cmd_symbols(
    cwd: &Path,
    pattern: Option<&str>,
    kind: Option<&str>,
    owner: Option<&str>,
    language: Option<&str>,
    path: Option<&str>,
    signatures: bool,
    limit: usize,
    out: Out,
) -> Result<()> {
    let s = Session::open(cwd, false, out)?;

    let mut sql = String::from(
        "SELECT name, kind, path, language, start_line, end_line, signature, owner
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
    // Members are only addressable with their type — `id` and `name` recur
    // everywhere — so filtering by owner is how "what does this type hold?" gets
    // asked without dumping every field in the repository.
    if let Some(o) = owner {
        sql.push_str(" AND owner = ?");
        args.push(o.to_string());
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
    sql.push_str(&limit_probe_clause(limit));

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
            owner: r.get(7)?,
            kind,
        })
    })?;
    let mut data: Vec<SymbolRow> = rows.collect::<std::result::Result<_, _>>()?;
    let note = take_limit(&mut data, limit);
    s.emit_noted(data, note)
}

#[derive(Serialize)]
struct DefinitionRow {
    name: String,
    kind: String,
    path: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    language: Option<String>,
    // A row's *primary position* is `line`/`col`/`byte`; its extent is
    // `end_line`/`end_col`/`end_byte`. The Rust fields keep the `start_` prefix
    // that mirrors the facts table, but the emitted key must not — see
    // ROW_KEY_RULE. `start_line` here and `line` on the next command over is how
    // `--fields line` came to work on some commands and not others.
    #[serde(rename = "line")]
    start_line: i64,
    #[serde(rename = "col")]
    start_col: i64,
    end_line: i64,
    end_col: i64,
    #[serde(rename = "byte")]
    start_byte: i64,
    end_byte: i64,
    #[serde(skip_serializing_if = "Option::is_none")]
    signature: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    doc: Option<String>,
    /// For a `field`/`variant`, the type it belongs to. Absent otherwise — a
    /// member name alone (`id`, `name`) does not identify anything.
    #[serde(skip_serializing_if = "Option::is_none")]
    owner: Option<String>,
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
                signature, doc, owner
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
            owner: r.get(12)?,
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
    compare_to: Option<&str>,
    limit: usize,
    out: Out,
) -> Result<()> {
    let s = Session::open(cwd, false, out)?;
    if let Some(reference) = compare_to {
        return api_diff(&s, reference, path, kind, limit);
    }
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
    "struct",
    "enum",
    "union",
    "trait",
    "class",
    "interface",
    "record",
    "namespace",
    "module",
    "type",
    "typealias",
];

#[derive(Serialize)]
struct ApiChangeRow {
    /// "added", "removed", or "changed"; also the section this row appears in.
    #[serde(skip_serializing)]
    change: String,
    name: String,
    kind: String,
    path: String,
    line: i64,
    #[serde(skip_serializing_if = "Option::is_none")]
    signature: Option<String>,
    /// The previous signature, when it changed.
    #[serde(skip_serializing_if = "Option::is_none")]
    was: Option<String>,
}

/// `api --compare-to <ref>` — how the exported surface differs from `ref`.
///
/// "Does this change break anyone" is the question a review actually asks, and
/// it is not answerable from a diff: a diff shows every edited line, most of
/// which are internal. This reports only the exported declarations that appeared,
/// disappeared, or changed shape.
///
/// Diff-shaped, not history-shaped: it compares two trees, the same thing
/// `changed` and `affected` already do, and needs Git for exactly that reason.
fn api_diff(
    s: &Session,
    compare_to: &str,
    path: Option<&str>,
    kind_filter: Option<&str>,
    limit: usize,
) -> Result<()> {
    let changed = s.repo.changed_since(compare_to)?;
    let mut ex = crate::facts::Extractor::new()?;
    let mut data: Vec<ApiChangeRow> = Vec::new();
    // Names that left this file but are still exported from it via an import.
    // Reported as a note rather than dropped silently: "it moved" is the fact,
    // and it is not the same fact as "nothing happened".
    let mut moved: Vec<String> = Vec::new();

    for file in changed {
        if path.is_some_and(|p| !file.contains(p)) {
            continue;
        }
        let Some(lang) = language_for(&file) else {
            continue;
        };

        // The exported surface as of `compare_to`.
        let mut old: HashMap<String, Vec<(String, String)>> = HashMap::new();
        if let Some(bytes) = s.repo.file_at(compare_to, &file) {
            for fact in ex
                .extract(lang, &file, &bytes)
                .0
                 .0
                .into_iter()
                .filter(|f| {
                    f.visibility.as_deref() == Some("public")
                        && f.kind != "import"
                        && kind_filter.is_none_or(|kind| f.kind == kind)
                })
            {
                old.entry(fact.name).or_default().push((
                    fact.kind.clone(),
                    declaration_header(&fact.kind, Some(fact.signature)).unwrap_or_default(),
                ));
            }
        }

        // ...and as of now, from the index (already fresh).
        let mut stmt = s.db.conn.prepare(
            "SELECT name, kind, start_line, signature FROM facts
             WHERE path = ?1 AND visibility = 'public' AND kind <> 'import'
             ORDER BY start_line",
        )?;
        let now: Vec<(String, String, i64, Option<String>)> = stmt
            .query_map([&file], |r| {
                Ok((r.get(0)?, r.get(1)?, r.get(2)?, r.get(3)?))
            })?
            .collect::<std::result::Result<_, _>>()?;

        let mut seen: HashSet<String> = HashSet::new();
        let mut current: HashMap<String, Vec<ApiChangeRow>> = HashMap::new();
        for (name, current_kind, line, sig) in now {
            if kind_filter.is_some_and(|filter| current_kind != filter) {
                continue;
            }
            let sig = declaration_header(&current_kind, sig);
            seen.insert(name.clone());
            let before = old.get_mut(&name).and_then(|versions| {
                versions
                    .iter()
                    .position(|(kind, signature)| {
                        kind == &current_kind && Some(signature.as_str()) == sig.as_deref()
                    })
                    .map(|position| versions.remove(position))
            });
            if before.is_none() {
                current.entry(name.clone()).or_default().push(ApiChangeRow {
                    change: "added".into(),
                    name,
                    kind: current_kind,
                    path: file.clone(),
                    line,
                    signature: sig,
                    was: None,
                });
            }
        }
        // Names this file still binds through a module-scope import. Moving a
        // definition down a layer and importing it back keeps every
        // `from handler import resolve_key` working — the surface is unchanged —
        // but the definition is no longer *here*, so a definitions-only
        // comparison called it a removed public export. For the command pitched
        // as "the review question a raw diff can't answer", that is the answer
        // that would block a PR on a non-breaking change.
        //
        // The bound local name is the import's last segment: codeq stores
        // `from pkg import x` as `pkg.x` and JS `import {a} from './x'` as
        // `./x.a`, so the same rule reads both, and JS `export { x } from './y'`
        // with it.
        let rebound: HashSet<String> = {
            let mut st = s.db.conn.prepare(
                "SELECT name FROM facts WHERE path = ?1 AND kind = 'import' AND name IS NOT NULL",
            )?;
            let rows = st.query_map([&file], |r| r.get::<_, String>(0))?;
            rows.filter_map(|r| r.ok())
                .filter_map(|m| m.rsplit(['.', '/']).next().map(str::to_string))
                .collect()
        };
        let names: HashSet<String> = old.keys().chain(current.keys()).cloned().collect();
        for name in names {
            let mut old_versions = old.remove(&name).unwrap_or_default();
            let mut current_versions = current.remove(&name).unwrap_or_default();
            let had_current = seen.contains(&name);

            // Pair unmatched overloads by kind first, so a changed parameter
            // list remains a `changed` row while newly added overloads survive.
            let mut unmatched_current = Vec::new();
            for mut now in current_versions.drain(..) {
                if let Some(position) = old_versions
                    .iter()
                    .position(|(old_kind, _)| old_kind == &now.kind)
                {
                    let (_, before) = old_versions.remove(position);
                    now.change = "changed".into();
                    now.was = Some(before);
                    data.push(now);
                } else {
                    unmatched_current.push(now);
                }
            }

            // A declaration changing kind but keeping its name is still one
            // changed export once same-kind overload pairs have been handled.
            let paired = unmatched_current.len().min(old_versions.len());
            for (mut now, (_, before)) in unmatched_current
                .drain(..paired)
                .zip(old_versions.drain(..paired))
            {
                now.change = "changed".into();
                now.was = Some(before);
                data.push(now);
            }
            data.extend(unmatched_current);

            if !had_current && rebound.contains(&name) {
                if !old_versions.is_empty() {
                    moved.push(name);
                }
                continue;
            }
            for (kind, before) in old_versions {
                data.push(ApiChangeRow {
                    change: "removed".into(),
                    name: name.clone(),
                    kind,
                    path: file.clone(),
                    line: 0,
                    signature: None,
                    was: Some(before),
                });
            }
        }
    }

    data.sort_by(|a, b| a.path.cmp(&b.path).then(a.name.cmp(&b.name)));

    // Grouped rather than one flat list with a `change` column: the text
    // renderer shows a signature *instead of* the other fields, so a flat list
    // would print an added and a removed row identically. Removals first — they
    // are the breaking ones.
    #[derive(Serialize)]
    struct ApiDiff {
        #[serde(skip_serializing_if = "Option::is_none")]
        truncated: Option<bool>,
        removed: Vec<ApiChangeRow>,
        changed: Vec<ApiChangeRow>,
        added: Vec<ApiChangeRow>,
    }
    let truncated = limit > 0 && data.len() > limit;
    let mut remaining = if limit == 0 { usize::MAX } else { limit };
    let mut take = |data: &[ApiChangeRow], which: &str| -> Vec<ApiChangeRow> {
        let rows: Vec<ApiChangeRow> = data
            .iter()
            .filter(|r| r.change == which)
            .take(remaining)
            .map(|r| ApiChangeRow {
                change: r.change.clone(),
                name: r.name.clone(),
                kind: r.kind.clone(),
                path: r.path.clone(),
                line: r.line,
                signature: r.signature.clone(),
                was: r.was.clone(),
            })
            .collect();
        remaining -= rows.len();
        rows
    };
    let out = ApiDiff {
        truncated: truncated.then_some(true),
        // Breaking removals are the most useful rows in a capped review.
        removed: take(&data, "removed"),
        changed: take(&data, "changed"),
        added: take(&data, "added"),
    };
    if !moved.is_empty() {
        moved.sort();
        moved.dedup();
        let note = format!(
            "still exported, now via an import rather than a definition here: {} — the surface is unchanged, so these are not removals",
            moved.join(", "),
        );
        return s.emit_noted(out, Some(note));
    }
    let note = data
        .is_empty()
        .then(|| format!("the exported surface is unchanged since {compare_to}"));
    s.emit_noted(out, note)
}

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
    // This row *is* a definition, so its identity is `name` (ROW_KEY_RULE).
    #[serde(rename = "name")]
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
    /// `"file"` when the reference sits at module scope with no definition
    /// around it, which is why this row has no `name`.
    ///
    /// The row is real — a Python module-level call runs at import, and a JS
    /// `proto.use = …` at file scope is how most of Node is written — but
    /// `callers` promises *definitions*, so a nameless row read as a broken one.
    /// Two consumers of this output (including a dogfooding sweep of ours)
    /// crashed on the missing key rather than noticing the fact it carried. Say
    /// what it is instead of leaving it blank.
    #[serde(skip_serializing_if = "Option::is_none")]
    scope: Option<&'static str>,
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
/// Apply `--limit` to a finished row list. `0` means unlimited, matching every
/// other list command — which is why `callers`/`callees` finally have one: they
/// were the only two without, and the likeliest to return a lot (`callers
/// serverLog` in redis is 227 rows and 11 KB, unboundable).
///
/// Truncation happens after sorting so the rows kept are the first N of the
/// answer, not the first N found.
/// A note when `--limit` cut the answer, for results whose data is a bare array.
///
/// `search` and `references` carry `truncated` in their envelope and the
/// renderer turns it into a warning. A result whose `data` is a plain array has
/// nowhere to put that flag, so those commands cut in silence: `imports` on a
/// real repo showed 200 of 1,030 and said nothing, which reads exactly like
/// "this repo has 200 imports".
fn truncation_note(shown: usize, total: usize) -> Option<String> {
    (total > shown).then(|| {
        format!(
            "showing {shown} of {total} — stopped at --limit; rerun with a higher --limit (or 0) for the rest"
        )
    })
}

fn truncate_rows<T>(rows: &mut Vec<T>, limit: usize) {
    if limit > 0 && rows.len() > limit {
        rows.truncate(limit);
    }
}

fn callers_of(
    s: &Session,
    name: &str,
    path: Option<&str>,
    call_only: bool,
    qualifier: Option<&str>,
    examples: bool,
) -> Result<Vec<Caller>> {
    let re = regex::Regex::new(&regex::escape(name))?;
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
        identifier_boundary: true,
        per_file: None,
    };
    let mut hits = match run_search(
        &s.db.conn,
        &re,
        like_of(name).as_deref(),
        path,
        None,
        None,
        &opts,
    )? {
        SearchData::Matches { matches, .. } => matches,
        _ => unreachable!(),
    };
    hits.extend(alias_hits(s, name, path, None, &opts)?);

    // Where `name` is declared — an occurrence there is the def, not a call.
    let mut defsites = DeclSites::load(&s.db.conn, name)?;

    // Import lines in the files that matched. An import names the symbol without
    // using it, so it is a poor anchor for a caller row — it points at the
    // dependency, not the usage. It went unnoticed while imports sat outside
    // every definition (they surfaced as stray symbol-less rows), but a
    // definition spanning its whole file (a Svelte component) encloses its own
    // imports, and the dedup below then let the import line shadow the real
    // usage site. These are used to *rank* anchors, not to drop rows: the
    // default `callers` stays a textual superset, with `--calls` to narrow it.
    let hit_paths: HashSet<&str> = hits.iter().map(|h| h.path.as_str()).collect();
    let mut importsites: HashSet<(String, i64)> = HashSet::new();
    {
        let mut st =
            s.db.conn
                .prepare("SELECT path, start_line FROM facts WHERE kind = 'import'")?;
        let rows = st.query_map([], |r| Ok((r.get::<_, String>(0)?, r.get::<_, i64>(1)?)))?;
        for row in rows {
            let (p, line) = row?;
            if hit_paths.contains(p.as_str()) {
                importsites.insert((p, line));
            }
        }
    }

    // One row per (file, enclosing symbol). The representative site is the first
    // real usage; an import site is only the anchor when the file names the
    // symbol nowhere else, so a file that imports without using it is still
    // reported (just anchored at its import).
    let mut pick: HashMap<(String, Option<String>), (Hit, bool)> = HashMap::new();
    let mut order: Vec<(String, Option<String>)> = Vec::new();
    for h in hits {
        if defsites.is_declaration(&h.path, h.line as i64) {
            continue; // the declaration itself
        }
        let is_import = importsites.contains(&(h.path.clone(), h.line as i64));
        let key = (h.path.clone(), h.symbol.clone());
        match pick.get(&key) {
            None => {
                pick.insert(key.clone(), (h, is_import));
                order.push(key);
            }
            // Upgrade an import anchor to a real usage; otherwise the first wins.
            Some((_, prev_import)) if *prev_import && !is_import => {
                pick.insert(key, (h, is_import));
            }
            Some(_) => {}
        }
    }

    let scopes = TestScopes::load(&s.db.conn)?;
    let mut out = Vec::new();
    for key in order {
        let (mut h, _) = match pick.remove(&key) {
            Some(v) => v,
            None => continue,
        };
        let test = is_test_site(&h.path, h.symbol.as_deref(), h.line as i64, &scopes);
        // Under --examples, surface the call site's line/column and source text so
        // the row reads as a usage example (rendered grep-style by the envelope).
        let (col, text) = if examples {
            (Some(h.col), Some(std::mem::take(&mut h.text)))
        } else {
            (None, None)
        };
        let scope = h.symbol.is_none().then_some("file");
        out.push(Caller {
            symbol: h.symbol,
            kind: h.kind,
            path: h.path,
            line: h.line,
            col,
            text,
            scope,
            test,
            via: None,
        });
    }
    Ok(out)
}

/// Find local aliases of an imported source name. The dependency graph only
/// records these when the import resolves to an internal target, so `m()` is
/// included for `use crate::util::make as m` but an unrelated local `m` is not.
/// This supplements the ordinary source-name scan; it never replaces it.
fn alias_hits(
    s: &Session,
    name: &str,
    path: Option<&str>,
    language: Option<&str>,
    opts: &ScanOpts,
) -> Result<Vec<Hit>> {
    // A receiver-qualified request names the syntax the caller wants to see;
    // a bare alias has no such receiver and must not satisfy it.
    if opts.qualifier.is_some() {
        return Ok(Vec::new());
    }
    let graph = load_dep_graph(&s.db)?;
    let mut hits = Vec::new();
    for (file, binding) in graph.bindings_for_source(name) {
        if path.is_some_and(|scope| !file.contains(scope)) {
            continue;
        }
        let re = regex::Regex::new(&regex::escape(&binding))?;
        let like = like_of(&binding);
        let data = run_search(
            &s.db.conn,
            &re,
            like.as_deref(),
            Some(&file),
            language,
            None,
            opts,
        )?;
        if let SearchData::Matches { matches, .. } = data {
            hits.extend(matches);
        }
    }
    Ok(hits)
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
    /// The call as actually written inside the caller — `subprocess.run`, not
    /// `run` (only under `--examples`). The single most useful thing for spotting
    /// a name-based over-match: `about codeq_commit` listed four unrelated `run`
    /// definitions as callees, and the body calls `subprocess.run`. You had to
    /// already suspect the answer to reach for `--resolved`; this shows it.
    #[serde(skip_serializing_if = "Option::is_none")]
    call: Option<String>,
    /// Line of that call site, in the *caller's* file (only under `--examples`).
    /// `path`/`line` stay on the callee's definition — that is what `callees` is
    /// for; this is the evidence.
    #[serde(skip_serializing_if = "Option::is_none")]
    call_line: Option<i64>,
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
    examples: bool,
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

    // Names called inside the target body/bodies. Under `--examples` each also
    // gets the call as written and the line it sits on, recorded here while the
    // body is in hand. The receiver comes from the parse tree rather than from
    // walking bytes backwards: `calls_in` already resolved `subprocess.run` to
    // callee `run` on receiver `subprocess`, which is both cheaper and correct
    // for chained or whitespace-separated receivers.
    let mut idents: HashSet<String> = HashSet::new();
    let mut sites: HashMap<String, (String, i64)> = HashMap::new();
    // For a call such as `context::build()`, retain the target file of the
    // imported `context` module. A receiver is only trusted when it exactly
    // names a direct import; ordinary value receivers and unknown modules keep
    // the established name-based superset.
    let graph = load_dep_graph(&s.db)?;
    let mut module_targets: HashMap<String, Vec<Option<HashSet<String>>>> = HashMap::new();
    {
        let mut body_stmt =
            s.db.conn
                .prepare("SELECT body FROM content_fts WHERE path = ?1")?;
        let mut calls_stmt = s.db.conn.prepare(
            "SELECT start_byte, name, receiver FROM calls
             WHERE path = ?1 AND start_byte >= ?2 AND start_byte < ?3
             ORDER BY start_byte",
        )?;
        let mut span_stmt = s.db.conn.prepare(
            "SELECT start_byte, end_byte FROM spans WHERE path = ?1 ORDER BY start_byte",
        )?;
        for (path, sb, eb) in &targets {
            let body: Option<String> = body_stmt.query_row([path], |r| r.get(0)).ok();
            let Some(b) = body else { continue };
            let Some(slice) = b.get(*sb..(*eb).min(b.len())) else {
                continue;
            };
            // Prefer the syntax-derived callsites collected during indexing: they
            // know exactly which identifiers are called, without reparsing this
            // file for every `callees` query. Unsupported/opaque grammars keep
            // the existing textual fallback below.
            let lang = language_for(path).unwrap_or("");
            if crate::callsite::supports_syntax_calls(lang) {
                let rows =
                    calls_stmt.query_map(rusqlite::params![path, *sb as i64, *eb as i64], |r| {
                        Ok((
                            r.get::<_, i64>(0)? as usize,
                            r.get::<_, String>(1)?,
                            r.get::<_, Option<String>>(2)?,
                        ))
                    })?;
                for row in rows {
                    let (off, word, recv) = row?;
                    let lookup = if recv.is_none() {
                        graph
                            .source_name_for_binding(path, &word)
                            .unwrap_or(&word)
                            .to_string()
                    } else {
                        word.clone()
                    };
                    let targets = recv
                        .as_deref()
                        .map(|receiver| {
                            graph
                                .direct_targets_for_receiver(path, receiver)
                                .into_iter()
                                .map(str::to_string)
                                .collect::<HashSet<_>>()
                        })
                        .filter(|targets| !targets.is_empty());
                    let targets = targets.or_else(|| {
                        let targets = graph
                            .direct_targets_for_receiver(path, &word)
                            .into_iter()
                            .map(str::to_string)
                            .collect::<HashSet<_>>();
                        (!targets.is_empty()).then_some(targets)
                    });
                    // An empty target set is no evidence: it may be an external
                    // module or a value receiver. Represent that as `None` so
                    // candidate filtering remains conservative.
                    module_targets
                        .entry(lookup.clone())
                        .or_default()
                        .push(targets);
                    if examples {
                        let expr = match &recv {
                            Some(r) => format!("{r}.{word}"),
                            None => word.clone(),
                        };
                        let line = b[..off.min(b.len())]
                            .bytes()
                            .filter(|c| *c == b'\n')
                            .count() as i64
                            + 1;
                        sites.entry(lookup.clone()).or_insert((expr, line));
                    }
                    idents.insert(lookup);
                }
                continue;
            }
            // No grammar for this language: fall back to the identifier scan,
            // still skipping comment and string spans.
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
             WHERE kind NOT IN ('import', 'field', 'variant') AND name IN ({placeholders})"
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
            let allowed = module_targets.get(&n).is_none_or(|calls| {
                calls
                    .iter()
                    .any(|targets| targets.as_ref().is_none_or(|targets| targets.contains(&p)))
            });
            if n != name && allowed && seen.insert((n.clone(), p.clone(), l)) {
                let site = sites.get(&n);
                out.push(Callee {
                    name: n,
                    kind: k,
                    path: p,
                    line: l,
                    via: None,
                    call: site.map(|(e, _)| e.clone()),
                    call_line: site.map(|(_, l)| *l),
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
        let graph = load_dep_graph(&s.db)?;
        let mut reach: HashSet<String> = target_files.clone();
        for tf in &target_files {
            for r in graph.reachable_from(tf) {
                reach.insert(r.to_string());
            }
        }
        out.retain(|c| reach.contains(&c.path));
        // How many reachable files define each callee name: >1 is the residual
        // over-match this filter narrows but cannot resolve, and saying so is the
        // difference between "this is the definition" and "one of these is".
        let mut reachable_defs: HashMap<&str, usize> = HashMap::new();
        for c in &out {
            *reachable_defs.entry(c.name.as_str()).or_default() += 1;
        }
        let tags: Vec<&'static str> = out
            .iter()
            .map(|c| {
                via_of(
                    target_files.contains(&c.path),
                    reachable_defs.get(c.name.as_str()).copied().unwrap_or(0),
                )
            })
            .collect();
        for (c, tag) in out.iter_mut().zip(tags) {
            c.via = Some(tag.to_string());
        }
    }
    out.sort_by(|a, b| {
        (a.name.as_str(), a.path.as_str(), a.line).cmp(&(b.name.as_str(), b.path.as_str(), b.line))
    });
    Ok(out)
}

/// Identifiers in call position within `text`, skipping any that fall inside a
/// comment or string. `base` is `text`'s byte offset within its file, so the
/// sorted `prose` ranges (which are file-absolute) line up.
fn collect_idents(text: &str, base: usize, prose: &[(usize, usize)], out: &mut HashSet<String>) {
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
    // A caller row *is* a definition, so its identity is `name` like every
    // other definition row. `symbol` stays reserved for "the definition this
    // row sits inside" (search/references), which is a different fact.
    #[serde(rename = "name")]
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
    limit: usize,
    out: Out,
) -> Result<()> {
    let s = Session::open(cwd, false, out)?;
    if depth <= 1 {
        let mut callers = callers_of(&s, name, path, call_only, qualifier, examples)?;
        truncate_rows(&mut callers, limit);
        let rows = resolve_rows(
            &s,
            name,
            resolved,
            callers,
            |c| c.path.as_str(),
            |c, v| {
                c.via = v;
            },
        )?;
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
    truncate_rows(&mut hops, limit);
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
    examples: bool,
    limit: usize,
    out: Out,
) -> Result<()> {
    let s = Session::open(cwd, false, out)?;
    if depth <= 1 {
        let mut rows = callees_of(&s, name, path, resolved, examples)?;
        truncate_rows(&mut rows, limit);
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
            for c in callees_of(&s, sym, path, false, false)? {
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
    truncate_rows(&mut hops, limit);
    let note = note_if_empty(&s, &hops, name, "callees", path)?;
    s.emit_noted(hops, note)
}

#[derive(Serialize)]
struct PathStep {
    // This row *is* a definition, so its identity is `name` (ROW_KEY_RULE).
    #[serde(rename = "name")]
    symbol: String,
    kind: String,
    path: String,
    line: i64,
    /// Position in the chain (0 = the `from` symbol). Text output indents by it.
    depth: usize,
}

/// `callpath <from> <to>` — the shortest call chain from `from` to `to`, if one
/// exists: `from` calls … calls `to`. A BFS down the callee graph (so the chain
/// is shortest by hops), bounded by `max_depth`. Reachability with the route
/// shown, complementing `callees --depth` (which only says whether/at what depth
/// something is reached). Name-based, like the other graph commands; emits an
/// empty result when `to` isn't reachable from `from` within `max_depth`.
pub fn cmd_callpath(
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
            let mut callees = callees_of(&s, node, scope, false, false)?;
            callees.sort_by(|a, b| {
                a.name
                    .cmp(&b.name)
                    .then(a.path.cmp(&b.path))
                    .then(a.line.cmp(&b.line))
            });
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
        // Four causes, one output. `callpath` is the command where an empty
        // answer has the most plausible explanations — a misspelled FROM, a
        // misspelled TO, a real pair that simply does not connect, or a chain
        // longer than `--max-depth` — and it used to distinguish none of them:
        // no stdout, no stderr, exit 0, and no `note` under `--json` either.
        // Someone who had not already read both files could only assume a typo.
        let from_sites = definition_sites(&s, from)?;
        let to_sites = definition_sites(&s, to)?;
        let where_ = |sites: &[(String, i64)]| {
            sites
                .iter()
                .map(|(p, l)| format!("{p}:{l}"))
                .take(2)
                .collect::<Vec<_>>()
                .join(", ")
        };
        let note = match (from_sites.is_empty(), to_sites.is_empty()) {
            (true, true) => format!("neither '{from}' nor '{to}' is defined in this repository"),
            (true, false) => format!(
                "no symbol named '{from}' (the FROM end); '{to}' is defined at {}",
                where_(&to_sites)
            ),
            (false, true) => format!(
                "no symbol named '{to}' (the TO end); '{from}' is defined at {}",
                where_(&from_sites)
            ),
            (false, false) if path.is_some_and(|f| !from_sites.iter().any(|(p, _)| p.contains(f))) => {
                format!(
                    "'{from}' is defined at {} but nothing matches --path {}",
                    where_(&from_sites),
                    path.unwrap_or_default(),
                )
            }
            // Both real and both reachable-in-principle: say how far the walk got,
            // because "raise --max-depth" and "they genuinely do not connect" are
            // different next actions.
            (false, false) => format!(
                "both are defined ('{from}' at {}, '{to}' at {}) but no call chain reaches one from the other within --max-depth {max_depth} ({} symbols explored)",
                where_(&from_sites),
                where_(&to_sites),
                visited.len(),
            ),
        };
        return s.emit_noted(Vec::<PathStep>::new(), Some(note));
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
        steps.push(PathStep {
            symbol: name,
            kind,
            path,
            line,
            depth,
        });
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
pub fn cmd_about(
    cwd: &Path,
    name: &str,
    path: Option<&str>,
    resolved: bool,
    out: Out,
) -> Result<()> {
    let s = Session::open(cwd, false, out)?;
    // `members` is theirs, the shared walk is mine: `explore` needs the same
    // three lists, and letting the two commands gather them separately is how
    // they would drift about who is in the neighbourhood.
    let (definitions, callers, callees) = symbol_graph(&s, name, path, resolved)?;
    let data = About {
        definitions,
        members: members_of(&s, name, path, None, 0)?,
        callers,
        callees,
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

/// The graph around `name`: its definition(s), who calls it, what it calls.
///
/// Shared by `about` and `explore`, which walk the same neighbourhood and then
/// present it two different ways — `about` keeps every field of each node (doc
/// comments, the `test` flag on callers, byte offsets, `owner`), `explore` trims
/// the nodes and spends the budget on verbatim source instead. Reported from
/// outside as "the same command twice", which the names invite; keeping the walk
/// in one place at least means the two can't answer differently about *who* is in
/// the neighbourhood.
///
/// Callers stay repo-wide — a textual "who uses this" — while the focus and its
/// callees honour `path`, so a common name doesn't merge unrelated symbols.
fn symbol_graph(
    s: &Session,
    name: &str,
    path: Option<&str>,
    resolved: bool,
) -> Result<(Vec<DefinitionRow>, Vec<Caller>, Vec<Callee>)> {
    let callers = callers_of(s, name, None, false, None, false)?;
    let callers = resolve_rows(
        s,
        name,
        resolved,
        callers,
        |c| c.path.as_str(),
        |c, v| {
            c.via = v;
        },
    )?;
    Ok((
        definitions_of(s, name, path, 0)?,
        callers,
        callees_of(s, name, path, resolved, false)?,
    ))
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
    /// What is in scope in the focus file — the imports an edit can rely on.
    #[serde(skip_serializing_if = "Vec::is_empty")]
    imports: Vec<ImportRow>,
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
    name: Option<&str>,
    at: Option<&str>,
    path: Option<&str>,
    resolved: bool,
    max_tokens: u64,
    out: Out,
) -> Result<()> {
    let s = Session::open(cwd, false, out)?;

    // `--at` asks the same question from the other end. A stack frame, a diff
    // hunk and a search hit all name a *position*, not a symbol, so answering
    // "what is this and who uses it" meant `at` to get the name, then `explore`
    // with it. Resolving here collapses that to one call.
    //
    // The position scopes the lookup to that *file*, which keeps same-named
    // definitions in other files out. It does not narrow to the one definition
    // on that line: a name declared several times in one file (Ruby's repeated
    // `attr_accessor :params`) still contributes every occurrence to the focus.
    // Worth stating precisely — this comment used to claim the position
    // "identifies one definition exactly", which it does not.
    let (target, scope): (String, Option<String>) = match at {
        Some(loc) => {
            let found = definition_at(&s, loc)?;
            // A position between definitions is a real empty answer, not a bad
            // argument, so it gets a note rather than an error — as does a line
            // that declares several things and therefore names no single symbol
            // to explore. The budget comes from a real (empty) assembly so the
            // shape of the result is identical to any other explore.
            let refusal = match found.names.as_slice() {
                [] => Some(format!("no definition encloses {loc}")),
                [_one] => None,
                many => Some(format!(
                    "{}:{} declares {} definitions ({}) — a line, not a symbol; \
                     explore one by name",
                    found.rel,
                    found.line,
                    many.len(),
                    many.join(", "),
                )),
            };
            if let Some(note) = refusal {
                let (budget, source, omitted) =
                    crate::context::source_for(&s.db, &s.repo, &[], max_tokens)?.into_source();
                return s.emit_noted(
                    ExploreResult {
                        focus: Vec::new(),
                        callers: Vec::new(),
                        callees: Vec::new(),
                        imports: Vec::new(),
                        source,
                        budget,
                        omitted,
                    },
                    Some(note),
                );
            }
            (found.names[0].clone(), Some(found.rel))
        }
        // clap guarantees one of the two is present.
        None => (
            name.unwrap_or_default().to_string(),
            path.map(str::to_string),
        ),
    };
    let (name, path) = (target.as_str(), scope.as_deref());

    // The same neighbourhood `about` reports; explore trims the nodes and spends
    // the token budget on verbatim source instead.
    let (defs, callers, callees) = symbol_graph(&s, name, path, resolved)?;

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

    // The focus file's imports: what is already in scope at the edit site. An
    // agent about to change a definition needs to know whether the helper it
    // wants is imported, aliased, or absent — otherwise the first attempt at the
    // edit produces an unresolved name. Only the focus files, so a wide caller
    // set can't turn this into the repo's whole import table.
    let imports = focus_imports(&s, &defs)?;

    let note = note_if_empty(&s, &focus, name, "definition", path)?;
    s.emit_noted(
        ExploreResult {
            focus,
            callers: caller_nodes,
            callees: callee_nodes,
            imports,
            source,
            budget,
            omitted,
        },
        note,
    )
}

/// The innermost definition enclosing a `<path>:<line>` position.
///
/// Shared by `at` and `explore --at`: "which symbol is this line in" is one
/// question, and it should not have two implementations that can disagree.
/// Innermost wins (smallest byte range), so a line inside a method resolves to
/// the method rather than the type that contains it.
fn definition_at(s: &Session, location: &str) -> Result<AtPosition> {
    let (path, line_str) = location
        .rsplit_once(':')
        .ok_or_else(|| anyhow!("expected <path>:<line>, got '{location}'"))?;
    let line: i64 = line_str
        .parse()
        .map_err(|_| anyhow!("invalid line number in '{location}'"))?;
    let rel = resolve_file(s, path)?;
    // Every definition covering the line, innermost first. Not `LIMIT 1`: a
    // single line can *declare several things* — Ruby's `attr_accessor :app,
    // :env, :request`, a C++ enum's variants, a multi-name field declaration —
    // and those tie for innermost because they share a span. Taking the first
    // row silently answered about `app` when the question was about `response`,
    // which `at` (which returns all five) already knew better than to do.
    let mut stmt = s.db.conn.prepare(
        "SELECT name, start_byte, end_byte
         FROM facts
         WHERE kind <> 'import' AND path = ?1
           AND start_line <= ?2 AND end_line >= ?2
         ORDER BY start_byte ASC, name ASC",
    )?;
    let covering: Vec<(String, i64, i64)> = stmt
        .query_map((&rel, line), |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?)))?
        .collect::<std::result::Result<_, _>>()?;

    // The *leaves*: definitions covering the line that do not contain another
    // one. Containment, not span size — the first attempt ranked by span and
    // picked the shortest, so `enum Color { red, green, blue, unknown }` on one
    // line resolved to `red` because it has the fewest characters. Four disjoint
    // enumerators are equally innermost; only the enum around them is an
    // ancestor, and that is what containment says and span does not.
    let contains = |a: &(String, i64, i64), b: &(String, i64, i64)| {
        a.1 <= b.1 && a.2 >= b.2 && (a.1 != b.1 || a.2 != b.2)
    };
    let mut names: Vec<String> = covering
        .iter()
        .filter(|d| !covering.iter().any(|other| contains(d, other)))
        .map(|(n, _, _)| n.clone())
        .collect();
    names.dedup();
    Ok(AtPosition { rel, line, names })
}

/// What a `<file>:<line>` resolved to: the innermost definitions covering it.
/// More than one name means the line declares several things and does not
/// identify a single symbol.
struct AtPosition {
    rel: String,
    line: i64,
    names: Vec<String>,
}

/// The imports declared by the files the focus definitions live in.
fn focus_imports(s: &Session, defs: &[DefinitionRow]) -> Result<Vec<ImportRow>> {
    let mut files: Vec<&str> = defs.iter().map(|d| d.path.as_str()).collect();
    files.sort_unstable();
    files.dedup();
    let mut rows = Vec::new();
    for f in files {
        let mut stmt = s.db.conn.prepare(
            "SELECT path, name, start_line, language
             FROM facts
             WHERE kind = 'import' AND path = ?1
             ORDER BY start_line",
        )?;
        let got = stmt.query_map((f,), |r| {
            Ok(ImportRow {
                path: r.get(0)?,
                module: r.get(1)?,
                start_line: r.get(2)?,
                language: r.get(3)?,
            })
        })?;
        for row in got {
            rows.push(row?);
        }
    }
    Ok(rows)
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
                signature, doc, owner
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
            owner: r.get(12)?,
            kind,
        })
    })?;
    let data: Vec<DefinitionRow> = rows.collect::<std::result::Result<_, _>>()?;
    // `explore --at` on the identical position says "no definition encloses"; this
    // said nothing at all. Two commands answering one question, one of them mute.
    let note = data.is_empty().then(|| {
        format!("no definition encloses {rel}:{line} (between definitions, or past the end)")
    });
    s.emit_noted(data, note)
}

#[derive(Serialize)]
struct OutlineRow {
    name: String,
    kind: String,
    /// Nesting level derived from byte containment (methods inside a class, …).
    depth: usize,
    #[serde(rename = "line")]
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
    let mut data = members_of(&s, name, path, kind, if limit > 0 { limit + 1 } else { 0 })?;
    let limit_note = take_limit(&mut data, limit);
    let relation = match kind {
        Some(k) => format!("members of kind '{k}'"),
        None => "members (fields, variants or methods)".to_string(),
    };
    let note = match limit_note {
        Some(n) => Some(n),
        None => note_if_empty_feat(&s, &data, name, &relation, path, Some("members"))?,
    };
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
    let all_owners: Vec<(String, i64, i64)> = owner_stmt
        .query_map([name], |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?)))?
        .collect::<std::result::Result<Vec<_>, _>>()?;
    let owners: Vec<(String, i64, i64)> = all_owners
        .iter()
        .filter(|(p, _, _)| path.is_none_or(|f| p.contains(f)))
        .cloned()
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

    // A member belongs to its owner wherever it is *written*. Containment is how
    // an owner gets discovered; once a fact carries an `owner` tag, that tag is
    // the answer. The two come apart in SQL, where a migrations directory adds
    // columns with `ALTER TABLE` in a file the `CREATE TABLE` does not span — so
    // containment alone reported a schema's first columns and none of its later
    // ones. Every other language writes members inside their type, so those rows
    // are already present and the dedup drops them.
    //
    // Only when NAME has exactly one definition in the whole repository, which is
    // what keeps `members` structural. The `owner` tag is a *name*, so it cannot
    // tell two same-named types apart: with two `class C`s, trusting it would give
    // each one the other's members — the name-based false positive this command
    // exists to not have. One definition means there is nothing to confuse it
    // with. The count is deliberately taken before `--path` narrows it: a filter
    // selects which owner is asked about, it does not make an ambiguous name
    // unambiguous.
    if all_owners.len() == 1 && !owners.is_empty() && (limit == 0 || data.len() < limit) {
        let mut seen: HashSet<(String, i64, String)> = data
            .iter()
            .map(|r| (r.path.clone(), r.line, r.name.clone()))
            .collect();
        let mut tagged = s.db.conn.prepare(
            "SELECT name, kind, path, start_line, signature, doc FROM facts
             WHERE owner = ?1 AND kind <> 'import'
             ORDER BY path, start_line",
        )?;
        let rows = tagged.query_map([name], |r| {
            let kind: String = r.get(1)?;
            let signature: Option<String> = r.get(4)?;
            Ok(MemberRow {
                name: r.get(0)?,
                path: r.get(2)?,
                line: r.get(3)?,
                signature: declaration_header(&kind, signature),
                doc: r.get(5)?,
                kind,
                owner: None,
            })
        })?;
        for row in rows {
            let row = row?;
            if kind.is_some_and(|k| row.kind != k) {
                continue;
            }
            if !seen.insert((row.path.clone(), row.line, row.name.clone())) {
                continue;
            }
            data.push(row);
            if limit > 0 && data.len() >= limit {
                break;
            }
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
pub(crate) const MIN_FOLD: usize = 4;

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
    let bytes =
        std::fs::read(s.repo.root.join(&rel)).map_err(|e| anyhow!("cannot read {rel}: {e}"))?;
    // Match the index: drop a leading UTF-8 BOM so line/column numbering here
    // agrees with what search and the symbol facts report.
    let source_bytes = bytes.strip_prefix(&[0xEF, 0xBB, 0xBF]).unwrap_or(&bytes);
    let text = String::from_utf8_lossy(source_bytes);
    let file_lines: Vec<&str> = text.lines().collect();
    // Keep line terminators for --raw: splitting with `lines()` and printing
    // each line would silently rewrite CRLF, invalid UTF-8, or a missing final
    // newline.
    let raw_lines: Vec<&[u8]> = source_bytes
        .split_inclusive(|byte| *byte == b'\n')
        .collect();
    let n = file_lines.len();
    let show = |a: usize, b: usize| -> Result<()> {
        if raw {
            print_range_raw(&raw_lines, a, b)?;
        } else {
            print_range(&file_lines, a, b)
        }
        Ok(())
    };

    // Verbatim line range.
    if let Some(spec) = lines {
        let (a, b) = parse_range(spec)?;
        show(a, b)?;
        return Ok(());
    }

    // Verbatim definition(s) by name.
    if let Some(name) = symbol {
        let mut stmt = s.db.conn.prepare(
            "SELECT start_line, end_line FROM facts
             WHERE path = ?1 AND name = ?2 AND kind <> 'import' ORDER BY start_line",
        )?;
        let mut ranges: Vec<(i64, i64)> = stmt
            .query_map((&rel, name), |r| Ok((r.get(0)?, r.get(1)?)))?
            .collect::<std::result::Result<_, _>>()?;
        // A markdown heading is stored bare, but `outline` renders its signature —
        // the raw source line, `## Heading` — so copying a heading out of
        // `outline` and into `--symbol` is the obvious workflow and it failed.
        // Accept the copied form: strip the leading `#` run and retry. Nothing in
        // any supported language declares a name starting with `#`, so this can
        // only ever rescue the case it is for.
        if ranges.is_empty() {
            let bare = name.trim_start_matches('#').trim();
            if bare != name && !bare.is_empty() {
                ranges = stmt
                    .query_map((&rel, bare), |r| Ok((r.get(0)?, r.get(1)?)))?
                    .collect::<std::result::Result<_, _>>()?;
            }
        }
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
            show(*a as usize, *b as usize)?;
            if !raw {
                println!(); // blank between/after defs, as the folded view does
            }
        }
        return Ok(());
    }

    // Whole file when asked, or when we can't parse it to know what to fold.
    if full || raw || !crate::facts::has_spans(language_for(&rel).unwrap_or("")) {
        show(1, n)?;
        return Ok(());
    }

    // Folded view: hide the bodies of function/method definitions.
    let mut folded = vec![false; n];
    {
        let mut stmt = s.db.conn.prepare(
            "SELECT start_line, end_line FROM facts
             WHERE path = ?1 AND kind IN ('function', 'method')",
        )?;
        let rows = stmt.query_map([&rel], |r| Ok((r.get::<_, i64>(0)?, r.get::<_, i64>(1)?)))?;
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
    let a: usize = a
        .trim()
        .parse()
        .map_err(|_| anyhow!("invalid line range"))?;
    let b: usize = b
        .trim()
        .parse()
        .map_err(|_| anyhow!("invalid line range"))?;
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
fn print_range_raw(lines: &[&[u8]], a: usize, b: usize) -> Result<()> {
    let stdout = std::io::stdout();
    let mut stdout = stdout.lock();
    for i in a.max(1)..=b {
        if let Some(l) = lines.get(i - 1) {
            stdout.write_all(l)?;
        }
    }
    Ok(())
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
    if s.db
        .conn
        .query_row("SELECT 1 FROM files WHERE path = ?1", [arg], |_| Ok(()))
        .is_ok()
    {
        return Ok(arg.to_string());
    }
    let suffix = format!("%/{arg}");
    let mut stmt =
        s.db.conn
            .prepare("SELECT path FROM files WHERE path LIKE ?1 ORDER BY path")?;
    let matches: Vec<String> = stmt
        .query_map([suffix], |r| r.get(0))?
        .collect::<std::result::Result<_, _>>()?;
    match matches.len() {
        1 => Ok(matches.into_iter().next().unwrap()),
        // "file not found" describes the lookup that failed; the useful message
        // describes the state that caused it. An absolute path into a *different*
        // checkout exists on disk and is simply not in this index, and being told
        // it does not exist sends you checking the path instead of the repo. Same
        // class as the empty-result notes: the branch is known, so name it.
        0 => {
            let elsewhere =
                std::path::Path::new(arg).is_absolute() && std::path::Path::new(arg).exists();
            if elsewhere {
                Err(anyhow!(
                    "'{arg}' exists but is not in this index (rooted at {}); run codeq from \
                     that file's repository, or point at it with --root",
                    s.repo.root.display(),
                ))
            } else {
                Err(anyhow!("file not found in this index: {arg}"))
            }
        }
        _ => Err(anyhow!("'{arg}' is ambiguous: {}", matches.join(", "))),
    }
}

#[derive(Serialize)]
struct ImportRow {
    /// The file that contains the import.
    path: String,
    /// The imported module, as written in source (raw / unresolved).
    module: String,
    #[serde(rename = "line")]
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
         FROM facts WHERE kind = 'import'
           AND (is_derived = 0 OR language = 'python')",
    );
    let mut args: Vec<String> = Vec::new();
    if let Some(p) = path {
        sql.push_str(" AND path LIKE ?");
        args.push(format!("%{p}%"));
    }
    sql.push_str(" ORDER BY path, start_line");
    // One past the limit, so a full page can be told from a page that happens to
    // end exactly on it. `imports` defaults to 200 and a real repo had 1,030;
    // the answer read like the whole set and said nothing.
    if limit > 0 {
        sql.push_str(&format!(" LIMIT {}", limit + 1));
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
            let mut stmt = s
                .db
                .conn
                .prepare("SELECT DISTINCT path FROM facts WHERE name = ?1 AND kind <> 'import'")?;
            let rows = stmt.query_map([name], |r| r.get::<_, String>(0))?;
            for row in rows {
                defs.insert(row?);
            }
        }
        Ok(Resolver {
            graph: load_dep_graph(&s.db)?,
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
            let reach = self.graph.reachable_from(file);
            via_of(
                false,
                self.defs
                    .iter()
                    .filter(|d| reach.contains(d.as_str()))
                    .count(),
            )
        };
        self.cache.insert(file.to_string(), v);
        v
    }
}

/// Classify a `via` tag from how many definition files of one name the anchor can
/// reach. The single source of truth for that vocabulary.
///
/// It used to exist twice — here for `callers`/`references`, and as an inline
/// two-way branch in `callees_of` — anchored in opposite directions but answering
/// the same question. The copies had already drifted: `callees --resolved` had no
/// `ambiguous` arm, so two reachable same-named definitions came back as two rows
/// tagged `imported`, each reading like a confident resolution, while `callers`
/// called the identical situation `ambiguous`. Same graph, same facts, two answers.
fn via_of(same_file: bool, reachable_defs: usize) -> &'static str {
    if same_file {
        return "same-file";
    }
    match reachable_defs {
        0 => "unresolved",
        1 => "imported",
        _ => "ambiguous",
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
    let graph = load_dep_graph(&s.db)?;

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
    //
    // The path filter is applied in Rust, NOT as `AND path LIKE ?` in this query.
    // `content_fts` is an FTS5 table with `tokenize = 'trigram'`, and the trigram
    // tokenizer *takes over* `LIKE` on the table — so a `LIKE` against `path`,
    // which is `UNINDEXED` and therefore has no trigram data, matches nothing at
    // all. Every `--path` form returned an empty result with exit 0: not a
    // narrowed answer, an emptied one. It looked like "astgrep can't express
    // that", which is what the reporter concluded before giving up on the
    // command. `search` was never affected because it LIKEs `body` (which *is*
    // the indexed column) and filters paths in Rust, as here.
    let mut stmt =
        s.db.conn
            .prepare("SELECT path, body FROM content_fts WHERE language = ?1 ORDER BY path")?;
    let mut scanned = 0usize;
    // Stream the bodies into the matcher one at a time (it re-parses each and
    // keeps only matches), so a big single-language repo isn't all held in memory
    // at once — the astgrep matcher takes an iterator, so no Vec is needed.
    let files = stmt
        .query_map([&language], |r| {
            Ok((r.get::<_, String>(0)?, r.get::<_, String>(1)?))
        })?
        .filter_map(|r| r.ok())
        .filter(|(p, _): &(String, String)| path.as_deref().is_none_or(|q| p.contains(q)))
        .inspect(|_| scanned += 1);

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
    // An empty result has two very different causes, and they used to look
    // identical: the pattern matched nothing, or `--path` selected no files at
    // all. The second is the one that wastes an afternoon, so name it.
    let note = if rows.is_empty() {
        match path.as_deref() {
            Some(p) if scanned == 0 => Some(format!(
                "no indexed {language} file matches --path {p}; the pattern was never run"
            )),
            // The pattern parsed and ran and found nothing. Silence would be
            // right for `search`, where an empty grep is unambiguous — but
            // astgrep is structurally exact, and its own docs concede that a
            // wrong node kind and a genuine miss look identical. Point at the
            // command that tells them apart.
            _ => Some(format!(
                "pattern ran over {scanned} {language} file(s) and matched nothing; if it should have matched, `astgrep --explain <file>:<line>` shows the node kinds to write it against"
            )),
        }
    } else {
        None
    };
    s.emit_noted(
        AstData {
            matches: rows,
            truncated,
        },
        note,
    )
}

/// `astgrep --explain <file>:<line>` — the parse tree covering one line: the node
/// kinds and field names a pattern has to be written against.
///
/// The gap this closes: `astgrep` demands exact node kinds and codeq offered no
/// way to learn them, so writing a pattern meant leaving codeq for a tree-sitter
/// playground — and a wrong guess comes back as an empty result that reads
/// exactly like "no matches".
pub fn cmd_astgrep_explain(cwd: &Path, location: &str, out: Out) -> Result<()> {
    let s = Session::open(cwd, false, out)?;
    let (path, line_str) = location
        .rsplit_once(':')
        .ok_or_else(|| anyhow!("expected <path>:<line>, got '{location}'"))?;
    let line: usize = line_str
        .parse()
        .map_err(|_| anyhow!("invalid line number in '{location}'"))?;
    if line == 0 {
        return Err(anyhow!("line numbers start at 1"));
    }
    let rel = resolve_file(&s, path)?;

    // Language and body both come from the index, so `--explain` needs no
    // `--language`: the file already knows what it is.
    let (language, body): (Option<String>, String) = s.db.conn.query_row(
        "SELECT language, body FROM content_fts WHERE path = ?1",
        [&rel],
        |r| Ok((r.get(0)?, r.get(1)?)),
    )?;
    let language = language.ok_or_else(|| {
        anyhow!("'{rel}' has no language codeq classifies, so it has no parse tree")
    })?;
    if crate::astgrep::ts_language(&language).is_none() {
        return Err(anyhow!(
            "no grammar for '{language}', so `astgrep` cannot match in '{rel}' either \
             — see `codeq capabilities`"
        ));
    }

    #[derive(Serialize)]
    struct ExplainRow {
        path: String,
        line: usize,
        col: usize,
        depth: usize,
        kind: String,
        #[serde(skip_serializing_if = "Option::is_none")]
        field: Option<String>,
        /// The source this node covers. Deliberately not `text`: that key means
        /// "this row *is* the matched source" to the renderer, and here the
        /// payload is the node kind — the preview is context for it.
        src: String,
    }

    let nodes = crate::astgrep::explain(&language, &body, line)?;
    let data: Vec<ExplainRow> = nodes
        .into_iter()
        .map(|n| ExplainRow {
            path: rel.clone(),
            line: n.line,
            col: n.col,
            depth: n.depth,
            kind: n.kind,
            field: n.field,
            src: n.text,
        })
        .collect();
    let note = data.is_empty().then(|| {
        format!("'{rel}' has no parse tree covering line {line} (past the end of the file?)")
    });
    s.emit_noted(data, note)
}

pub fn cmd_context(cwd: &Path, req: Request, out: Out) -> Result<()> {
    let s = Session::open(cwd, false, out)?;
    let seeds: Vec<String> = req.symbols.clone();
    let result = context::build(&s.db, &s.repo, &req)?;
    // The worst of the silent empties. `context` exists to hand source to an
    // agent, so an empty bundle reads as "nothing more is needed here" rather
    // than "the symbol you named does not exist". Name the seeds that resolved
    // to nothing — with several seeds, one bad name is otherwise invisible
    // because the others still fill the bundle.
    let mut missing: Vec<&str> = Vec::new();
    for name in &seeds {
        if definition_sites(&s, name)?.is_empty() {
            missing.push(name);
        }
    }
    // Checked whether or not the bundle came back empty. A seed that resolves to
    // nothing is *more* dangerous when the others succeeded: the result looks
    // full, and the one name you got wrong is the one silently missing from it.
    let note = if !missing.is_empty() {
        Some(format!(
            "no symbol named {} — {} of {} seeds contributed nothing",
            missing.join(", "),
            missing.len(),
            seeds.len(),
        ))
    } else if result.chunks.is_empty() && !seeds.is_empty() {
        Some(
            "the seeds resolved but produced no source — a zero budget, or definitions with no readable body"
                .to_string(),
        )
    } else {
        None
    };
    s.emit_noted(result, note)
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
    /// Positional paths, as grep takes them: `codeq search TODO src`. Matched as
    /// path substrings (the same rule as `path`), OR'd together.
    pub paths: &'a [String],
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
    let expr = if args.fixed {
        regex::escape(args.pattern)
    } else {
        args.pattern.to_string()
    };
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
    // `--glob` and grep-style positional paths both become per-row path
    // predicates for the streaming scan, AND'd together: `-g` says which files
    // are eligible by shape, a positional says which by location.
    let glob = if args.globs.is_empty() {
        None
    } else {
        Some(GlobFilter::build(args.globs)?)
    };
    let accept = (glob.is_some() || !args.paths.is_empty()).then(|| {
        move |p: &str| {
            glob.as_ref().is_none_or(|gf| gf.accepts(p))
                && (args.paths.is_empty() || args.paths.iter().any(|q| p.contains(q.as_str())))
        }
    });
    let path_accept = accept.as_ref().map(|f| f as &dyn Fn(&str) -> bool);

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
        identifier_boundary: args.word,
        per_file: out.max_per_file,
    };

    // Piped input never touches the index: no repo discovery, no refresh, no
    // database. That keeps codeq usable as a plain filter outside a repository,
    // and keeps this branch entirely off the indexed path the MCP server uses.
    if args.stdin {
        let data = scan_stdin(&re, &opts, args.language, search::STDIN_LABEL)?;
        return emit_sessionless(&data, out);
    }

    let s = Session::open(cwd, false, out)?;
    let scoped = args.path.is_some() || !args.paths.is_empty() || args.language.is_some();
    let data = run_search(
        &s.db.conn,
        &re,
        match_query.as_deref(),
        args.path,
        args.language,
        path_accept,
        &opts,
    )?;
    // A bare pattern miss stays silent — an empty `search zzz` is unambiguous and
    // a note on every one would be noise. What is ambiguous is a *filter* that
    // selected no file: then the pattern never ran, and "no matches" is a
    // statement about the filter rather than about the code.
    let note = if scoped && data.is_empty() {
        let mut stmt = s.db.conn.prepare(
            "SELECT COUNT(*) FROM files WHERE (?1 IS NULL OR path LIKE ?2) AND (?3 IS NULL OR language = ?3)",
        )?;
        let like = args.path.map(|p| format!("%{p}%"));
        let n: i64 = stmt.query_row(rusqlite::params![args.path, like, args.language], |r| {
            r.get(0)
        })?;
        (n == 0).then(|| {
            let mut which: Vec<String> = Vec::new();
            if let Some(p) = args.path {
                which.push(format!("--path {p}"));
            }
            if let Some(l) = args.language {
                which.push(format!("--language {l}"));
            }
            format!(
                "no indexed file matches {} — the pattern was never run",
                which.join(" and ")
            )
        })
    } else {
        None
    };
    s.emit_noted(data, note)
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
    let per_file = out.max_per_file;
    let s = Session::open(cwd, false, out)?;

    let expr = regex::escape(name);
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
        // Include alias hits below, then apply one global cap to the combined
        // result. Per-alias caps would make --limit depend on import spelling.
        limit: 0,
        call_only,
        qualifier: qualifier.map(str::to_string),
        identifier_boundary: true,
        per_file,
    };
    let (mut hits, mut truncated) = match run_search(
        &s.db.conn,
        &re,
        match_query.as_deref(),
        path,
        language,
        None,
        &opts,
    )? {
        SearchData::Matches { matches, truncated } => (matches, truncated),
        _ => unreachable!("references always uses Matches mode"),
    };
    hits.extend(alias_hits(&s, name, path, language, &opts)?);
    hits.sort_by(|a, b| (a.path.as_str(), a.line, a.col).cmp(&(b.path.as_str(), b.line, b.col)));
    if limit > 0 && hits.len() > limit {
        hits.truncate(limit);
        truncated = true;
    }

    // Where this exact name is declared. Hits are sorted above, which is what
    // `DeclSites` needs to tell a declaration from a recursive use.
    let mut defsites = DeclSites::load(&s.db.conn, name)?;

    let references: Vec<ReferenceRow> = hits
        .into_iter()
        .map(|h| {
            let definition = defsites.is_declaration(&h.path, h.line as i64);
            ReferenceRow {
                hit: h,
                definition,
                via: None,
            }
        })
        // A definition is not a call. `fn foo(` and `def foo(` are followed by
        // `(` just as a call is, so the byte-level test cannot tell them apart —
        // but the definition sites are already known exactly, so drop them.
        // Same for `--on`: a declaration is not a qualified use.
        .filter(|r| !((call_only || qualifier.is_some()) && r.definition))
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
    s.emit_noted(
        ReferencesResult {
            references,
            truncated,
        },
        note,
    )
}

/// `graph` — every type relation in the repository as an edge list.
///
/// `implementors`/`supertypes` answer about *one* node; this is the whole graph,
/// which is what you need to draw a schema or an inheritance hierarchy. It is
/// deliberately limited to relations and not offered for calls: relation edges
/// come off the parse tree (`impl T for X`, `REFERENCES orgs`), so the graph is
/// exact, whereas the call graph is name-based and every hop compounds the
/// over-match — a whole-repo call graph would be mostly plausible-looking noise,
/// which is worse than no graph at all. `callers`/`callees --depth N` stay the
/// right tool there, rooted at a symbol you can sanity-check.
#[derive(Serialize)]
struct GraphEdge {
    from: String,
    to: String,
    relation: String,
    path: String,
    line: i64,
    #[serde(skip_serializing_if = "Option::is_none")]
    language: Option<String>,
}

pub fn cmd_graph(
    cwd: &Path,
    relation: Option<&str>,
    path: Option<&str>,
    language: Option<&str>,
    mermaid: bool,
    limit: usize,
    out: Out,
) -> Result<()> {
    let s = Session::open(cwd, false, out)?;
    let filters = Filters::new()
        .eq("relation", relation)
        .like("path", path)
        .eq("language", language);
    let sql = format!(
        "SELECT subtype, supertype, relation, path, line, language FROM impls
         WHERE 1=1{} ORDER BY path, line, subtype, supertype{}",
        filters.sql(),
        limit_clause(limit),
    );
    let edges: Vec<GraphEdge> = {
        let mut stmt = s.db.conn.prepare(&sql)?;
        let rows = stmt.query_map(params_with(&[], &filters).as_slice(), |r| {
            Ok(GraphEdge {
                from: r.get(0)?,
                to: r.get(1)?,
                relation: r.get(2)?,
                path: r.get(3)?,
                line: r.get(4)?,
                language: r.get(5)?,
            })
        })?;
        rows.collect::<std::result::Result<_, _>>()?
    };

    if mermaid {
        // A document, not rows — so it deliberately leaves the `path:line:col:`
        // contract rather than pretending to honour it, the way `read --raw`
        // does. Printed directly: the point is to paste it somewhere it renders.
        let mut ids: BTreeMap<&str, String> = BTreeMap::new();
        let mut lines = vec!["graph LR".to_string()];
        for e in &edges {
            let next = |n: usize| format!("n{n}");
            let len = ids.len();
            let a = ids
                .entry(e.from.as_str())
                .or_insert_with(|| next(len))
                .clone();
            let len = ids.len();
            let b = ids
                .entry(e.to.as_str())
                .or_insert_with(|| next(len))
                .clone();
            lines.push(format!(
                "  {a}[{}] -->|{}| {b}[{}]",
                e.from, e.relation, e.to
            ));
        }
        if edges.is_empty() {
            lines.push("  %% no relation edges".to_string());
        }
        println!("{}", lines.join("\n"));
        return Ok(());
    }

    // The note used to name Go, C and Zig unconditionally — so on a Python-only
    // tree it read as "your language is unsupported" when the truth was "your
    // code has no edges". That is the exact failure the don't-be-misled section
    // exists to prevent, and it was my own note. Answer about *this* repo:
    // separate the languages here that cannot produce edges from the ones that
    // can and did not.
    // A `--relation` filter emptying the result is a statement about the
    // filter, not the repository. C# writes `class C : B, I` for both kinds, so
    // codeq can only record `extends` there — and `graph --relation implements`
    // on a C# repo full of interface implementations then read "genuinely has no
    // inheritance edges to graph". Answer with the kinds that *are* present.
    let other_kinds: Vec<String> = if edges.is_empty() && relation.is_some() {
        let unfiltered = Filters::new().like("path", path).eq("language", language);
        let sql = format!(
            "SELECT relation, COUNT(*) FROM impls WHERE 1=1{} GROUP BY relation ORDER BY relation",
            unfiltered.sql(),
        );
        let mut stmt = s.db.conn.prepare(&sql)?;
        let rows = stmt.query_map(params_with(&[], &unfiltered).as_slice(), |r| {
            Ok(format!(
                "{} {}",
                r.get::<_, i64>(1)?,
                r.get::<_, String>(0)?
            ))
        })?;
        rows.filter_map(|r| r.ok()).collect()
    } else {
        Vec::new()
    };

    let note = if !other_kinds.is_empty() {
        let scope = path.map(|p| format!(" under '{p}'")).unwrap_or_default();
        Some(format!(
            "no '{}' relation edges{scope}, but this repository has {} — the filter emptied this, not the code",
            relation.unwrap_or_default(),
            other_kinds.join(", "),
        ))
    } else if edges.is_empty() {
        let mut stmt = s.db.conn.prepare(
            "SELECT DISTINCT language FROM files WHERE language IS NOT NULL ORDER BY language",
        )?;
        let langs: Vec<String> = stmt
            .query_map([], |r| r.get::<_, String>(0))?
            .filter_map(|r| r.ok())
            .collect();
        let (capable, incapable): (Vec<&String>, Vec<&String>) = langs
            .iter()
            .partition(|l| crate::facts::support(l).is_some_and(|sup| sup.relations));
        let what = relation.map(|r| format!("'{r}' ")).unwrap_or_default();
        let scope = path.map(|p| format!(" under '{p}'")).unwrap_or_default();
        Some(if capable.is_empty() && !incapable.is_empty() {
            format!(
                "no {what}relation edges{scope} — codeq extracts no inheritance edges for {}, which is what this repository is written in, so this is not evidence there are none",
                incapable.iter().map(|s| s.as_str()).collect::<Vec<_>>().join("/"),
            )
        } else {
            format!(
                "no {what}relation edges{scope} — {} does produce them, so this repository genuinely has no inheritance or foreign-key edges to graph",
                capable.iter().map(|s| s.as_str()).collect::<Vec<_>>().join("/"),
            )
        })
    } else {
        None
    };
    s.emit_noted(edges, note)
}

#[derive(Serialize)]
struct ImplementorRow {
    /// The implementing/deriving type.
    // ROW_KEY_RULE: the row *is* this type, so the key is `name`. The side of
    // the relation is already said by the command you ran and by `relation`;
    // spelling it `subtype` here only cost consumers a per-command special case.
    #[serde(rename = "name")]
    subtype: String,
    /// How it relates: "implements", "extends", or "references" (a SQL foreign key).
    relation: String,
    path: String,
    line: i64,
    #[serde(skip_serializing_if = "Option::is_none")]
    language: Option<String>,
}

#[derive(Serialize)]
struct SupertypeRow {
    /// The trait / interface / base class this type extends or implements.
    // ROW_KEY_RULE: the row *is* this type, so the key is `name`. The side of
    // the relation is already said by the command you ran and by `relation`;
    // spelling it `supertype` here only cost consumers a per-command special case.
    #[serde(rename = "name")]
    supertype: String,
    /// How it relates: "implements", "extends", or "references" (a SQL foreign key).
    relation: String,
    path: String,
    line: i64,
    #[serde(skip_serializing_if = "Option::is_none")]
    language: Option<String>,
}

/// One `impls` edge, before it is presented under either command's field names.
struct ImplEdge {
    other: String,
    relation: String,
    path: String,
    line: i64,
    language: Option<String>,
}

/// The `impls` edges touching `name`, read in one direction.
///
/// `implementors` and `supertypes` are the same query with `subtype` and
/// `supertype` swapped — which is why they were, letter for letter, the same
/// function written twice. They still present different field names (`subtype`
/// vs `supertype`), because that is the output contract, so only the query is
/// shared and each command names its own result.
fn impl_edges(
    s: &Session,
    select: &str,
    matched: &str,
    name: &str,
    relation: Option<&str>,
    path: Option<&str>,
    limit: usize,
) -> Result<Vec<ImplEdge>> {
    debug_assert!(
        matches!(select, "subtype" | "supertype") && matches!(matched, "subtype" | "supertype"),
        "columns are fixed, never caller input"
    );
    let filters = Filters::new().eq("relation", relation).like("path", path);
    // A supertype is stored as written, so a namespaced base (`React.Component`,
    // Python's `abc.ABC`) is stored dotted — and nobody asks for it that way. Match
    // the last dotted segment too, using `substr(x, -n)` rather than a LIKE so an
    // `_` in the name stays a literal underscore instead of a wildcard. Subtypes
    // are always bare declared names, so this is a no-op in that direction.
    let sql = format!(
        "SELECT {select}, relation, path, line, language
         FROM impls
         WHERE ({matched} = ?1 OR substr({matched}, -(length(?1) + 1)) = '.' || ?1){}
         ORDER BY path, line{}",
        filters.sql(),
        limit_clause(limit),
    );
    let mut stmt = s.db.conn.prepare(&sql)?;
    let rows = stmt.query_map(params_with(&[&name], &filters).as_slice(), |r| {
        Ok(ImplEdge {
            other: r.get(0)?,
            relation: r.get(1)?,
            path: r.get(2)?,
            line: r.get(3)?,
            language: r.get(4)?,
        })
    })?;
    Ok(rows.collect::<std::result::Result<_, _>>()?)
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
    let data: Vec<ImplementorRow> =
        impl_edges(&s, "subtype", "supertype", name, relation, path, limit)?
            .into_iter()
            .map(|e| ImplementorRow {
                subtype: e.other,
                relation: e.relation,
                path: e.path,
                line: e.line,
                language: e.language,
            })
            .collect();
    let note = note_if_empty_feat(
        &s,
        &data,
        name,
        "implementors or subtypes",
        path,
        Some("relations"),
    )?;
    s.emit_noted(data, note)
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
    let data: Vec<SupertypeRow> =
        impl_edges(&s, "supertype", "subtype", name, relation, path, limit)?
            .into_iter()
            .map(|e| SupertypeRow {
                supertype: e.other,
                relation: e.relation,
                path: e.path,
                line: e.line,
                language: e.language,
            })
            .collect();
    let note = note_if_empty_feat(
        &s,
        &data,
        name,
        "supertypes (it extends nothing)",
        path,
        Some("relations"),
    )?;
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
    /// A decorator that may be registering this definition with a framework —
    /// `@app.get("/x")`, `@click.command()`. Shown, not filtered on: a route's
    /// only reference is the line above it, so without this every entry point in
    /// a decorator-driven codebase read as dead code, and suppressing them
    /// instead would hide the real thing `unused` is for.
    #[serde(skip_serializing_if = "Option::is_none")]
    decorator: Option<String>,
    /// The framework that reaches this definition by file convention rather
    /// than by any call — `next/page`, `react/lifecycle`. Same purpose as
    /// `decorator`, different evidence: the path and the name, not a line of
    /// syntax above the definition.
    #[serde(skip_serializing_if = "Option::is_none")]
    entrypoint: Option<&'static str>,
}

/// A definition invoked by a framework or test runner rather than by an
/// ordinary source-level call. `registration` is the evidence: the exact
/// decorator syntax codeq observed, or the file convention that loads it.
#[derive(Serialize)]
struct EntrypointRow {
    name: String,
    kind: String,
    path: String,
    line: i64,
    #[serde(skip_serializing_if = "Option::is_none")]
    registration: Option<String>,
    #[serde(skip_serializing_if = "is_false")]
    test: bool,
}

/// `entrypoints` — definitions structurally registered with a framework, plus
/// optionally test-runner entry points. This intentionally does not call every
/// public export an entry point: export visibility is an API fact, whereas this
/// command reports concrete invocation registration.
pub fn cmd_entrypoints(
    cwd: &Path,
    path: Option<&str>,
    include_tests: bool,
    limit: usize,
    out: Out,
) -> Result<()> {
    let s = Session::open(cwd, false, out)?;
    // A file convention is not a column, so registration is decided in Rust and
    // the limit has to be applied after that rather than in SQL.
    let mut sql = String::from(
        "SELECT name, kind, path, start_line, decorator, is_test, language, visibility
         FROM facts WHERE kind <> 'import'",
    );
    if !include_tests {
        sql.push_str(" AND is_test = 0");
    }
    let like = path.map(|path| format!("%{path}%"));
    sql.push_str(" AND (?1 IS NULL OR path LIKE ?1)");
    sql.push_str(" ORDER BY path, start_line");
    let mut stmt = s.db.conn.prepare(&sql)?;
    let rows = stmt.query_map([&like], |r| {
        let name: String = r.get(0)?;
        let kind: String = r.get(1)?;
        let path: String = r.get(2)?;
        let language: Option<String> = r.get(6)?;
        let visibility: Option<String> = r.get(7)?;
        let registration: Option<String> = r.get(4)?;
        let registration = registration.or_else(|| {
            crate::facts::framework_registration(
                language.as_deref(),
                &kind,
                &name,
                &path,
                visibility.as_deref(),
            )
            .map(str::to_string)
        });
        Ok(EntrypointRow {
            name,
            kind,
            path,
            line: r.get(3)?,
            registration,
            test: r.get::<_, bool>(5)?,
        })
    })?;
    let mut data: Vec<EntrypointRow> = rows
        .collect::<std::result::Result<Vec<_>, _>>()?
        .into_iter()
        .filter(|r| r.registration.is_some() || (include_tests && r.test))
        .collect();
    let truncated = take_limit(&mut data, limit);
    let note = truncated.or_else(|| {
        data.is_empty().then(|| match path {
            Some(path) => format!("no registered entry points in '{path}'"),
            None => "no registered entry points".to_string(),
        })
    });
    s.emit_noted(data, note)
}

/// Every candidate name that occurs outside its own definition site. This is
/// deliberately one pass over the indexed bodies: the previous implementation
/// ran an FTS search once *per name*, making `unused` scale with definitions ×
/// repository size. The matching rules are unchanged — exact identifier
/// boundaries, comments excluded, strings included, and declaration sites
/// ignored — so this only changes cost, not findings.
fn used_definition_names(
    s: &Session,
    names: &HashSet<String>,
    defsites: &HashMap<String, HashSet<(String, i64, i64)>>,
) -> Result<HashSet<String>> {
    if names.is_empty() {
        return Ok(HashSet::new());
    }

    // Longest-first matters when one identifier is a prefix of another: regex
    // alternation otherwise picks the short one first, whose right boundary
    // fails, and never retries the longer alternative at that position.
    let mut alternatives: Vec<&String> = names.iter().collect();
    alternatives.sort_unstable_by(|a, b| b.len().cmp(&a.len()).then(a.cmp(b)));
    let expr = alternatives
        .iter()
        .map(|name| regex::escape(name))
        .collect::<Vec<_>>()
        .join("|");
    let re = regex::Regex::new(&format!("(?:{expr})"))
        .map_err(|e| anyhow!("building unused-name matcher: {e}"))?;

    let mut body_stmt =
        s.db.conn
            .prepare("SELECT path, language, body FROM content_fts")?;
    let mut span_stmt = s.db.conn.prepare(crate::search::SPANS_SQL)?;
    let mut used = HashSet::new();
    // Definition extents already accounted for, so a second occurrence inside
    // one (recursion, a self-referential default) still counts as a use.
    let mut claimed: HashSet<(String, String, i64)> = HashSet::new();
    let mut rows = body_stmt.query([])?;
    while let Some(row) = rows.next()? {
        let path = row.get_ref(0)?.as_str()?;
        let language: Option<String> = row.get(1)?;
        let body = row.get_ref(2)?.as_str()?;
        let spans = if crate::facts::has_spans(language.as_deref().unwrap_or("")) {
            span_stmt
                .query_map([path], crate::search::span_row)?
                .collect::<std::result::Result<Vec<_>, _>>()?
        } else {
            Vec::new()
        };
        let bytes = body.as_bytes();
        let mut line_starts = vec![0];
        line_starts.extend(
            bytes
                .iter()
                .enumerate()
                .filter(|(_, b)| **b == b'\n')
                .map(|(i, _)| i + 1),
        );
        for m in re.find_iter(body) {
            if !crate::search::has_identifier_boundaries(
                bytes,
                m.start(),
                m.end(),
                crate::index::language_for(path),
                crate::search::is_in_string(&spans, m.start()),
            ) || !crate::search::is_not_comment(&spans, m.start())
            {
                continue;
            }
            let line = line_starts.partition_point(|&start| start <= m.start()) as i64;
            // The declaration is the *first* occurrence of the name inside its
            // own extent, not the occurrence on the extent's first line. Those
            // are the same thing only when nothing precedes the name — and an
            // annotation on its own line, which is how C#, Java, Kotlin and PHP
            // are conventionally written, is exactly the case where they differ:
            // the fact starts at `[Fact]` and the name is a line below, so the
            // declaration read as an external use and the definition could never
            // be reported dead. Claiming each extent once keeps a recursive call
            // — a real use, later in the same extent — counted as one.
            let is_definition = defsites.get(m.as_str()).is_some_and(|sites| {
                sites.iter().any(|(p, start, end)| {
                    p == path
                        && (*start..=*end).contains(&line)
                        && claimed.insert((m.as_str().to_string(), p.clone(), *start))
                })
            });
            if !is_definition {
                used.insert(m.as_str().to_string());
                if used.len() == names.len() {
                    return Ok(used);
                }
            }
        }
    }
    Ok(used)
}

/// Single-name form used only for the explanatory empty-result note. `unused`
/// itself calls [`used_definition_names`] once for the complete candidate set.
fn name_is_used(s: &Session, name: &str) -> Result<bool> {
    let names = HashSet::from([name.to_string()]);
    // No declaration sites to exclude: the only caller reaches here when the
    // name has no definition codeq indexes, so every occurrence is a use.
    let sites = HashMap::new();
    Ok(used_definition_names(s, &names, &sites)?.contains(name))
}

/// One directory in the repo's shape, with what it contains.
#[derive(Serialize)]
struct TreeRow {
    path: String,
    /// Nesting level, so the text renderer indents it.
    depth: usize,
    /// Files at or below here (not just immediate children).
    files: i64,
    /// Definitions at or below here — where the *code* actually is.
    symbols: i64,
    /// Immediate subdirectories, so a folded branch still shows it has shape.
    dirs: i64,
    /// Languages present, most files first.
    #[serde(skip_serializing_if = "String::is_empty")]
    languages: String,
}

/// `tree [<path>]` — the repo's directory shape, annotated with what each
/// directory contains and folded to a depth.
///
/// The gap this fills: `map` groups by *top-level* directory only, so on a repo
/// like `src/pkg/{lib,api,data/<200 dirs>}` it reports one row — "src, 2677 files,
/// 58 symbols" — which hides both where the code is and where the bulk is. `stats`
/// gives totals, `files` gives a flat list; neither shows shape.
///
/// Deliberately not an ASCII `tree` clone. A plain hierarchy would be worse than
/// `ls -R` for an agent; the value is in what `tree` can't do:
///   * it is ignore-aware by construction (`.gitignore`/`.codeqignore`), so the
///     `node_modules`/`.git` noise that makes real `tree` unusable never appears;
///   * every directory carries its file and *symbol* counts, so "where is the
///     code" is answerable without opening anything;
///   * counts aggregate from everything below, so folding at `--depth` summarises
///     rather than truncates — the token budget is bounded by depth, not by repo
///     size.
pub fn cmd_tree(
    cwd: &Path,
    path: Option<&str>,
    language: Option<&str>,
    depth: usize,
    min_symbols: i64,
    files_too: bool,
    limit: usize,
    out: Out,
) -> Result<()> {
    let s = Session::open(cwd, false, out)?;

    // Definitions per file, so a directory can report the code it holds.
    let mut per_file: HashMap<String, i64> = HashMap::new();
    {
        let lang_filter = Filters::new().eq("language", language);
        let sql = format!(
            "SELECT path, COUNT(*) FROM facts
             WHERE kind <> 'import' AND name IS NOT NULL{} GROUP BY path",
            lang_filter.sql(),
        );
        let mut stmt = s.db.conn.prepare(&sql)?;
        let rows = stmt.query_map(params_with(&[], &lang_filter).as_slice(), |r| {
            Ok((r.get::<_, String>(0)?, r.get::<_, i64>(1)?))
        })?;
        for row in rows {
            let (p, n) = row?;
            per_file.insert(p, n);
        }
    }

    #[derive(Default)]
    struct Agg {
        files: i64,
        symbols: i64,
        children: HashSet<String>,
        langs: HashMap<String, i64>,
    }
    let mut dirs: HashMap<String, Agg> = HashMap::new();
    let mut file_rows: Vec<TreeRow> = Vec::new();

    // `--language` scopes the *files* as well as the counts, so an asset-heavy
    // repo can be asked "where is the Python" — which is what `--min-symbols`
    // claims to answer and cannot, because a markdown heading is a definition and
    // 281 country READMEs outrank the source 68 to 1.
    let filters = Filters::new().like("path", path).eq("language", language);
    let sql = format!(
        "SELECT path, language FROM files WHERE 1=1{} ORDER BY path",
        filters.sql()
    );
    let mut stmt = s.db.conn.prepare(&sql)?;
    let rows = stmt.query_map(params_with(&[], &filters).as_slice(), |r| {
        Ok((r.get::<_, String>(0)?, r.get::<_, Option<String>>(1)?))
    })?;
    for row in rows {
        let (fpath, lang) = row?;
        let syms = per_file.get(&fpath).copied().unwrap_or(0);
        // Credit every ancestor, so a folded directory summarises its whole subtree
        // instead of under-reporting what it holds.
        let segs: Vec<&str> = fpath.split('/').collect();
        for i in 0..segs.len().saturating_sub(1) {
            let dir = segs[..=i].join("/");
            let e = dirs.entry(dir.clone()).or_default();
            e.files += 1;
            e.symbols += syms;
            if let Some(l) = &lang {
                *e.langs.entry(l.clone()).or_insert(0) += 1;
            }
            if i + 2 < segs.len() {
                e.children.insert(segs[..=i + 1].join("/"));
            }
        }
        if files_too && segs.len() - 1 <= depth {
            let d = segs.len() - 1;
            file_rows.push(TreeRow {
                path: fpath.clone(),
                depth: d,
                files: 1,
                symbols: syms,
                dirs: 0,
                languages: lang.unwrap_or_default(),
            });
        }
    }

    let mut data: Vec<TreeRow> = dirs
        .into_iter()
        .filter(|(d, _)| d.split('/').count() <= depth)
        .filter(|(_, a)| a.symbols >= min_symbols)
        .map(|(d, a)| {
            let mut langs: Vec<(String, i64)> = a.langs.into_iter().collect();
            langs.sort_by(|x, y| y.1.cmp(&x.1).then(x.0.cmp(&y.0)));
            TreeRow {
                depth: d.split('/').count() - 1,
                path: d,
                files: a.files,
                symbols: a.symbols,
                dirs: a.children.len() as i64,
                languages: langs
                    .into_iter()
                    .map(|(l, _)| l)
                    .collect::<Vec<_>>()
                    .join(","),
            }
        })
        .collect();
    data.extend(file_rows);
    // Path order, so the listing reads as a tree rather than a ranking.
    data.sort_by(|a, b| a.path.cmp(&b.path));
    let total = data.len();
    if limit > 0 {
        data.truncate(limit);
    }
    let note = if data.is_empty() {
        note_if_scope_empty(&s, path, "any indexed content")?
    } else {
        truncation_note(data.len(), total)
    };
    s.emit_noted(data, note)
}

/// One *name* ranked by how often it is referenced in code.
///
/// Deliberately one row per name, not per definition: the count is a property of
/// the name (codeq's references are textual), so emitting a row per definition
/// printed the same number several times over — the first draft's top twelve rows
/// for this repo were twelve definitions of `path`, all reading `refs=481`.
/// `defs` carries what that collapsed, and `path`/`line` point at the first of
/// them so the row is still somewhere to go.
#[derive(Serialize)]
struct HotspotRow {
    name: String,
    kind: String,
    path: String,
    line: i64,
    refs: i64,
    defs: i64,
}

/// Call `f(offset, token)` for every identifier-like run in `body`.
///
/// A word byte is an ASCII alphanumeric, `_`, or *any* non-ASCII byte. Including
/// the high bytes matters: without them `café` would split into `caf` + the
/// accented tail, and a definition named `caf` would collect phantom references.
/// A run therefore only ever ends on an ASCII non-word byte, so the slice bounds
/// are always UTF-8 char boundaries.
fn for_each_ident(body: &str, mut f: impl FnMut(usize, &str)) {
    fn is_word(c: u8) -> bool {
        c == b'_' || c.is_ascii_alphanumeric() || c >= 0x80
    }
    let bytes = body.as_bytes();
    let mut i = 0;
    while i < bytes.len() {
        if is_word(bytes[i]) {
            let start = i;
            while i < bytes.len() && is_word(bytes[i]) {
                i += 1;
            }
            f(start, &body[start..i]);
        } else {
            i += 1;
        }
    }
}

/// `hotspots [<path>]` — definitions ranked by how many times their name appears
/// in code: the inverse of `unused`, and the "what is load-bearing here?" view for
/// a file or repo you've just met. Asked for by an external user who had `unused`
/// for dead code and `map` for file-level counts, but no way to find the symbols
/// that actually carry a module.
///
/// Counted in one pass over the indexed bodies rather than one scan per name, so
/// it costs about a single `search` instead of `unused`'s scan-per-definition.
/// Occurrences inside comment and string spans don't count, and files in a
/// language codeq can't parse are skipped — the same "references in code" rule
/// `callers` uses. Each definition's own declaration is subtracted, so `refs` is
/// uses *elsewhere*; a recursive call counts, because it is one.
///
/// Name-based like every graph command, and here that bites harder than usual:
/// counts are per *name*, so two definitions sharing a name report the same total,
/// and a short common name (`new`, `path`, `get`) ranks high because many unrelated
/// symbols spell it the same way. Read it as "this identifier is load-bearing",
/// not "this definition has N callers" — `callers NAME --calls` is the precise
/// question. `--kind`/`--path`/`--language` scope what is reported; the count is
/// always repo-wide.
pub fn cmd_hotspots(
    cwd: &Path,
    path: Option<&str>,
    kind: Option<&str>,
    language: Option<&str>,
    calls_only: bool,
    min_refs: i64,
    limit: usize,
    out: Out,
) -> Result<()> {
    let s = Session::open(cwd, false, out)?;

    // The definitions to rank. Scoped for *reporting* only — the count below is
    // repo-wide, because a reference from an unscoped file is still a reference.
    let filters = Filters::new()
        .like("path", path)
        .eq("kind", kind)
        .eq("language", language);
    let sql = format!(
        "SELECT name, kind, path, start_line FROM facts
         WHERE kind <> 'import' AND name IS NOT NULL{} ORDER BY path, start_line",
        filters.sql(),
    );
    let candidates: Vec<(String, String, String, i64)> = {
        let mut stmt = s.db.conn.prepare(&sql)?;
        let rows = stmt.query_map(params_with(&[], &filters).as_slice(), |r| {
            Ok((r.get(0)?, r.get(1)?, r.get(2)?, r.get(3)?))
        })?;
        rows.collect::<std::result::Result<_, _>>()?
    };
    if candidates.is_empty() {
        // The short-circuit had its own silent emit, so the note added at the end
        // of this function was unreachable for the commonest empty case — a
        // `--path` that selects nothing. Worth recording: the fix looked applied
        // and tested fine on one route while doing nothing on the other.
        let note = note_if_scope_empty(&s, path, "a referenced definition")?;
        return s.emit_noted(Vec::<HotspotRow>::new(), note);
    }
    let wanted: HashSet<&str> = candidates.iter().map(|(n, _, _, _)| n.as_str()).collect();

    // Every definition site of a wanted name, repo-wide. A declaration is not a
    // use of itself, and it can't be filtered out arithmetically: `fn new(` puts
    // the name in call position, so subtracting a per-name declaration count was
    // wrong in one direction for functions and in the other for structs. Skipping
    // the *site* is what `unused` already does, and it is right for both.
    // Keyed by name as well as position: `unused` can use (path, line) because it
    // asks about one name at a time, but here a single line often declares one
    // symbol and calls another — `fn a() { hot(); }` — and dropping the name
    // silently discarded every such call site.
    let mut defsites: HashSet<(String, String, i64)> = HashSet::new();
    let mut declarations: HashMap<String, i64> = HashMap::new();
    for_each_def_site(&s.db.conn, |name, path, line, _end| {
        if wanted.contains(name.as_str()) {
            *declarations.entry(name.clone()).or_insert(0) += 1;
            defsites.insert((name, path, line));
        }
    })?;

    // `--calls` can use the syntax-derived callsite index directly: declarations
    // never appear there, and this avoids reparsing or byte-scanning every body.
    // Svelte/Vue intentionally have no reliable stored calls (their script body
    // is opaque to the component parser), so those files retain the old textual
    // fallback in the pass below rather than silently losing coverage.
    let mut counts: HashMap<String, i64> = HashMap::new();
    if calls_only {
        let wanted_names: Vec<&str> = wanted.iter().copied().collect();
        for chunk in wanted_names.chunks(500) {
            let placeholders = vec!["?"; chunk.len()].join(",");
            let sql = format!(
                "SELECT c.name, COUNT(*) FROM calls c
                 JOIN files f ON f.path = c.path
                 WHERE c.name IN ({placeholders}) AND f.language NOT IN ('svelte', 'vue')
                 GROUP BY c.name"
            );
            let mut stmt = s.db.conn.prepare(&sql)?;
            let params: Vec<&dyn rusqlite::ToSql> = chunk
                .iter()
                .map(|name| name as &dyn rusqlite::ToSql)
                .collect();
            let rows = stmt.query_map(params.as_slice(), |r| {
                Ok((r.get::<_, String>(0)?, r.get::<_, i64>(1)?))
            })?;
            for row in rows {
                let (name, count) = row?;
                counts.insert(name, count);
            }
        }
    }

    // One pass over indexed bodies for normal hotspots, or only the opaque
    // grammar fallback in --calls mode.
    {
        let mut stmt =
            s.db.conn
                .prepare("SELECT path, language, body FROM content_fts")?;
        let mut span_stmt = s.db.conn.prepare(crate::search::SPANS_SQL)?;
        let mut rows = stmt.query([])?;
        while let Some(row) = rows.next()? {
            let rel = row.get_ref(0)?.as_str()?;
            let lang: Option<String> = row.get(1)?;
            if calls_only && crate::callsite::supports_syntax_calls(lang.as_deref().unwrap_or("")) {
                continue;
            }
            // Unparseable files have no comment/string spans, so every match in
            // them would look like code. `callers` skips them; so do we.
            if !crate::facts::has_spans(lang.as_deref().unwrap_or("")) {
                continue;
            }
            let spans: Vec<_> = span_stmt
                .query_map([rel], crate::search::span_row)?
                .collect::<std::result::Result<Vec<_>, _>>()?;
            let body = row.get_ref(2)?.as_str()?;
            let bytes = body.as_bytes();
            // Byte offset of each line start, so an occurrence can be matched
            // against the definition sites recorded above (which are line-based).
            let mut line_starts: Vec<usize> = vec![0];
            line_starts.extend(
                bytes
                    .iter()
                    .enumerate()
                    .filter(|(_, b)| **b == b'\n')
                    .map(|(i, _)| i + 1),
            );
            for_each_ident(body, |off, tok| {
                if let Some(name) = wanted.get(tok) {
                    if crate::search::class_at(&spans, off).is_some() {
                        return; // inside a comment or string literal
                    }
                    if calls_only && !crate::search::is_call_position(bytes, off, off + tok.len()) {
                        return;
                    }
                    let line = line_starts.partition_point(|&s| s <= off) as i64;
                    if defsites.contains(&((*name).to_string(), rel.to_string(), line)) {
                        return; // this name's own declaration
                    }
                    *counts.entry((*name).to_string()).or_insert(0) += 1;
                }
            });
        }
    }

    // Collapse to one row per name. `candidates` is ordered by (path, start_line),
    // so the first definition seen for a name is the representative.
    let mut seen_names: HashSet<&str> = HashSet::new();
    let mut data: Vec<HotspotRow> = Vec::new();
    for (name, kind, path, line) in &candidates {
        if !seen_names.insert(name.as_str()) {
            continue;
        }
        let declared = declarations.get(name).copied().unwrap_or(0);
        let refs = counts.get(name).copied().unwrap_or(0);
        if refs < min_refs {
            continue;
        }
        data.push(HotspotRow {
            name: name.clone(),
            kind: kind.clone(),
            path: path.clone(),
            line: *line,
            refs,
            defs: declared,
        });
    }
    // Most-referenced first; ties by location so the order is stable.
    data.sort_by(|a, b| {
        b.refs
            .cmp(&a.refs)
            .then(a.path.cmp(&b.path))
            .then(a.line.cmp(&b.line))
    });
    if limit > 0 {
        data.truncate(limit);
    }
    let note = if data.is_empty() {
        note_if_scope_empty(&s, path, "a referenced definition")?
    } else {
        None
    };
    s.emit_noted(data, note)
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
    let mut defsites: HashMap<String, HashSet<(String, i64, i64)>> = HashMap::new();
    for_each_def_site(&s.db.conn, |n, p, start, end| {
        defsites.entry(n).or_default().insert((p, start, end));
    })?;

    // The definitions eligible to be reported (scoped by path/kind). A markdown
    // heading is prose, not an API: nothing ever "calls" it, so every section in
    // the repo would be reported as dead. Excluded unless asked for by --kind.
    let filters = Filters::new().like("path", path).eq("kind", kind);
    // Markdown headings are prose, not definitions: nothing references a heading
    // by name, so every one of them would read as dead code.
    //
    // The other exclusion — declarations that exist to be consumed from outside
    // the repository — lives in `facts::UNREFERENCED_BY_DESIGN`, applied below.
    // It used to be three hand-written clauses in this query, which is the wrong
    // home for a policy that gains an entry every time a language with an
    // external contract is added.
    let exclude_headings = if kind == Some("heading") {
        ""
    } else {
        " AND kind <> 'heading'"
    };
    let sql = format!(
        // A definition that exists to satisfy a trait is called through the
        // trait and named by nothing, and deleting it does not remove code — it
        // breaks the build. That is not "looks dead but might not be", the case
        // the `decorator=` tag exists for; it is never dead, so it is excluded
        // rather than tagged.
        "SELECT name, kind, path, start_line, visibility, language, decorator FROM facts
         WHERE kind <> 'import' AND name IS NOT NULL AND contract IS NULL{exclude_headings}{}
         ORDER BY path, start_line",
        filters.sql(),
    );
    let candidates: Vec<UnusedRow> = {
        let mut stmt = s.db.conn.prepare(&sql)?;
        let rows = stmt.query_map(params_with(&[], &filters).as_slice(), |r| {
            let kind: String = r.get(1)?;
            let language: Option<String> = r.get(5)?;
            let name: String = r.get(0)?;
            let path: String = r.get(2)?;
            let visibility: Option<String> = r.get(4)?;
            let entrypoint = crate::facts::framework_registration(
                language.as_deref(),
                &kind,
                &name,
                &path,
                visibility.as_deref(),
            );
            Ok((
                UnusedRow {
                    name,
                    path,
                    line: r.get(3)?,
                    visibility,
                    decorator: r.get(6)?,
                    entrypoint,
                    kind,
                },
                language,
            ))
        })?;
        rows.collect::<std::result::Result<Vec<_>, _>>()?
            .into_iter()
            .filter(|(row, lang)| {
                !crate::facts::unreferenced_by_design(lang.as_deref(), &row.kind, &row.name)
            })
            .map(|(row, _)| row)
            .collect()
    };

    // Scan all candidate names in one pass; keep the defs whose name is never
    // used. This preserves the old per-name semantics without repeatedly
    // rescanning the same indexed bodies.
    let scopes = TestScopes::load(&s.db.conn)?;
    let names: HashSet<String> = candidates.iter().map(|c| c.name.clone()).collect();
    let used = used_definition_names(&s, &names, &defsites)?;
    let mut data: Vec<UnusedRow> = Vec::new();
    for cand in candidates {
        // Test functions are invoked by the test runner (by discovery/attribute),
        // not referenced by name, so they always look unused — exclude them by
        // default so real dead code isn't buried under every test. The path alone
        // is not enough: a Rust unit test lives in a `mod tests` inside the file
        // it tests, so every one of them used to be reported as dead.
        if !include_tests && is_test_site(&cand.path, Some(&cand.name), cand.line, &scopes) {
            continue;
        }
        if !used.contains(&cand.name) {
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

/// Test-like *containers* — a Rust `#[cfg(test)] mod tests`, a `class FooTest` —
/// as line spans, keyed by file.
///
/// Rust's dominant unit-test idiom puts tests in a module inside the very file
/// under test, where neither the path nor the function name says "test". Without
/// this, `tests <fn>` answered "none" for a function whose tests sit twenty lines
/// below it, and `unused` reported every one of those tests as dead code.
/// codeq already indexes the enclosing `mod` as a fact, so containment needs no
/// extraction change — and it generalises: a Python `class TestFoo` or a Java
/// `class FooTest` in an ordinary path classifies its methods the same way.
struct TestScopes {
    by_path: HashMap<String, Vec<(i64, i64)>>,
}

impl TestScopes {
    fn load(conn: &rusqlite::Connection) -> Result<TestScopes> {
        // Two sources, unioned. The name-based one covers test *containers* whose
        // only marking is what they are called (`mod tests`, `class FooTest`); the
        // attribute-based one covers everything a framework marks structurally,
        // whatever it is named. `is_test` alone is not enough — a `#[cfg(test)]
        // mod tests` carries no test attribute of its own, and its helper
        // functions carry none either — and names alone were never enough, which
        // is the gap this closes: a `#[test] fn parses_an_empty_string` sitting in
        // the file it tests matched neither the path, the name, nor the module.
        let mut stmt = conn.prepare(
            "SELECT path, name, start_line, end_line, is_test FROM facts
             WHERE (kind IN ('module', 'class') AND name IS NOT NULL) OR is_test = 1",
        )?;
        let rows = stmt.query_map([], |r| {
            Ok((
                r.get::<_, String>(0)?,
                r.get::<_, Option<String>>(1)?,
                r.get::<_, i64>(2)?,
                r.get::<_, Option<i64>>(3)?,
                r.get::<_, i64>(4)? == 1,
            ))
        })?;
        let mut by_path: HashMap<String, Vec<(i64, i64)>> = HashMap::new();
        for row in rows {
            let (path, name, start, end, is_test) = row?;
            let marked = is_test || name.as_deref().is_some_and(is_test_name);
            // A span with no recorded end can't bound anything — skip it rather
            // than guess and swallow the rest of the file.
            if let (true, Some(end)) = (marked, end) {
                by_path.entry(path).or_default().push((start, end));
            }
        }
        Ok(TestScopes { by_path })
    }

    /// Whether `line` of `path` falls inside a test-like container.
    fn contains(&self, path: &str, line: i64) -> bool {
        self.by_path.get(path).is_some_and(|spans| {
            spans
                .iter()
                .any(|&(start, end)| line >= start && line <= end)
        })
    }
}

/// Whether a definition site is a test — its file, its own name, or the container
/// it sits in says so. The three signals are always weighed together, so `tests`,
/// `callers` and `unused` agree on what a test is instead of each carrying its own
/// (differently wrong) notion.
fn is_test_site(path: &str, symbol: Option<&str>, line: i64, scopes: &TestScopes) -> bool {
    is_test_path(path) || symbol.is_some_and(is_test_name) || scopes.contains(path, line)
}

/// `tests <name>` — the test functions that reference `name`. A reference counts
/// as a test when its file looks like a test file, its enclosing function name
/// looks like a test, or it sits inside a test-like module/class (see
/// [`TestScopes`]). Name-based, like `references`: codeq reads no attributes, so
/// a `#[test]` fn outside a test path, name and module is still missed.
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
    let expr = regex::escape(name);
    let re = regex::Regex::new(&expr).map_err(|e| anyhow!("invalid name: {e}"))?;
    let opts = ScanOpts {
        before: 0,
        after: 0,
        out: OutMode::Matches,
        in_filter: InFilter::Any,
        limit: 0,
        call_only: false,
        qualifier: None,
        identifier_boundary: true,
        per_file: None,
    };
    let hits = match run_search(
        &s.db.conn,
        &re,
        like_of(name).as_deref(),
        path,
        None,
        None,
        &opts,
    )? {
        SearchData::Matches { matches, .. } => matches,
        _ => unreachable!("tests always uses Matches mode"),
    };

    let scopes = TestScopes::load(&s.db.conn)?;
    let mut tests: std::collections::BTreeSet<(String, String)> = std::collections::BTreeSet::new();
    for h in &hits {
        let sym = match &h.symbol {
            Some(sy) if sy != name => sy,
            _ => continue,
        };
        let is_fn = matches!(h.kind.as_deref(), Some("function") | Some("method"));
        if is_fn && is_test_site(&h.path, Some(sym), h.line as i64, &scopes) {
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

#[derive(Serialize)]
struct UntestedRow {
    name: String,
    kind: String,
    path: String,
    line: i64,
    #[serde(skip_serializing_if = "Option::is_none")]
    signature: Option<String>,
}

/// `untested [<path>]` — exported definitions no test refers to.
///
/// `api` composed with `tests`, which an agent otherwise has to do one symbol at
/// a time: a call per public definition just to learn which are uncovered.
///
/// Public only, because an unexported helper is usually covered through the
/// exported thing that calls it, and reporting every private function would bury
/// the answer. Heuristic in exactly the way `tests` is (a test is recognised by
/// test-like path or name) and name-based like the rest of the graph, so it
/// over-reports coverage rather than under-report it: a test that reaches the
/// code without naming it is invisible here.
pub fn cmd_untested(
    cwd: &Path,
    path: Option<&str>,
    kind: Option<&str>,
    limit: usize,
    out: Out,
) -> Result<()> {
    let s = Session::open(cwd, false, out)?;

    let filters = Filters::new().like("path", path).eq("kind", kind);
    let sql = format!(
        // Data declarations are excluded unless asked for by `--kind`. A field,
        // a variant and a constant have no behaviour to exercise — they are
        // tested through whatever reads them — so "untested" says nothing about
        // them, while they crowd out the functions it does say something about:
        // on a Go repo, 52 of 200 rows. Same argument as `clones` excluding
        // tests by default, and reversible the same way.
        "SELECT name, kind, path, start_line, signature FROM facts
         WHERE visibility = 'public' AND kind <> 'import' AND kind <> 'heading'
           AND ({} OR kind NOT IN ('field', 'variant', 'constant')){}
         ORDER BY path, start_line",
        if kind.is_some() { "1=1" } else { "1=0" },
        filters.sql(),
    );
    let candidates: Vec<UntestedRow> = {
        let mut stmt = s.db.conn.prepare(&sql)?;
        let rows = stmt.query_map(params_with(&[], &filters).as_slice(), |r| {
            let kind: String = r.get(1)?;
            Ok(UntestedRow {
                name: r.get(0)?,
                path: r.get(2)?,
                line: r.get(3)?,
                signature: declaration_header(&kind, r.get(4)?),
                kind,
            })
        })?;
        rows.collect::<std::result::Result<_, _>>()?
    };

    // One lookup per distinct name, not per definition.
    let scopes = TestScopes::load(&s.db.conn)?;
    let mut stopped_at_limit = false;
    let mut covered: HashMap<String, bool> = HashMap::new();
    let mut data: Vec<UntestedRow> = Vec::new();
    for cand in candidates {
        // Test scaffolding is not production code that needs covering. Same rule
        // `unused` applies, via the same predicate: this used to be a path-only
        // check, so a `pub fn` inside a `#[cfg(test)] mod tests` in a production
        // file was reported as untested public API — which `unused`, asking the
        // identical question one screen away, already knew better than to do.
        if is_test_site(&cand.path, Some(&cand.name), cand.line, &scopes) {
            continue;
        }
        let has_test = match covered.get(&cand.name) {
            Some(&c) => c,
            None => {
                let c = !test_sites(&s, &cand.name, None)?.is_empty();
                covered.insert(cand.name.clone(), c);
                c
            }
        };
        if !has_test {
            data.push(cand);
            if limit > 0 && data.len() >= limit {
                // Stop rather than count the rest: the per-candidate test lookup
                // is the expensive part, so the exact total is not worth a full
                // pass. Saying the answer is partial is.
                stopped_at_limit = true;
                break;
            }
        }
    }

    let note = if data.is_empty() {
        match path {
            Some(p) => Some(format!(
                "every public definition in '{p}' is referenced by a test"
            )),
            None => Some("every public definition is referenced by a test".to_string()),
        }
    } else if stopped_at_limit {
        Some(format!(
            "showing the first {} — stopped at --limit; rerun with a higher --limit (or 0) for the rest",
            data.len()
        ))
    } else {
        None
    };
    s.emit_noted(data, note)
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

/// A definition pulled in as impacted: a transitive caller of a changed
/// symbol, tagged with its hop distance from the change.
#[derive(serde::Serialize)]
struct ImpactNode {
    name: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    kind: Option<String>,
    path: String,
    /// The impacted definition's own line, per ROW_KEY_RULE. This used to be the
    /// *call site* — so `changed` and `impacted` rows in one result carried two
    /// different meanings for `line`, and clicking an impacted row landed you
    /// mid-body in the function rather than at it.
    line: i64,
    /// Where inside it the impact enters: the line referencing the changed
    /// symbol. The genuinely useful fact the old `line` was smuggling, kept as
    /// its own field — the same shape `callees --examples` already uses.
    #[serde(skip_serializing_if = "Option::is_none")]
    call_line: Option<i64>,
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
/// transitive callers (the impacted set), and the tests exercising any of them —
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
                let Some(cname) = c.symbol.clone() else {
                    continue;
                };
                if !reported.insert((c.path.clone(), cname.clone())) {
                    continue;
                }
                impacted.push(ImpactNode {
                    name: cname.clone(),
                    kind: c.kind,
                    path: c.path,
                    // `callers_of` anchors a row at the *call site*. Resolved to
                    // the definition's own line below; the call site keeps its
                    // own field rather than being smuggled through `line`.
                    line: 0,
                    call_line: Some(c.line as i64),
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
    // Resolve each impacted row to the *definition's* line. One statement reused
    // across the rows rather than a query per hop; a row whose definition cannot
    // be found in its own file falls back to the call site, which is still a real
    // position in the right file.
    {
        let mut stmt = s.db.conn.prepare(
            "SELECT MIN(start_line) FROM facts
             WHERE name = ?1 AND path = ?2 AND kind <> 'import'",
        )?;
        for node in &mut impacted {
            let def: Option<i64> = stmt
                .query_row((&node.name, &node.path), |r| r.get(0))
                .unwrap_or(None);
            node.line = def.or(node.call_line).unwrap_or(0);
            // Nothing to add when the impact enters at the declaration itself.
            if node.call_line == Some(node.line) {
                node.call_line = None;
            }
        }
    }
    impacted.sort_by(|a, b| {
        a.depth
            .cmp(&b.depth)
            .then(a.path.cmp(&b.path))
            .then(a.line.cmp(&b.line))
    });

    // Test files exercising anything in scope (changed symbols ∪ impacted),
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
    no_skill: bool,
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
        no_skill,
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
            println!(
                "{:<12}  {:<6}  {:<15}  {}{}",
                a.agent, a.what, a.action, a.config, note
            );
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
            println!("\nRestart the agent(s) above to pick up the codeq MCP server and skill.");
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
    /// Total indexed bytes. Here because `stats` used to be the only way to get
    /// it: `map` reported bytes per language but never the sum, which was the
    /// single fact keeping a second, strictly-weaker command alive.
    bytes: i64,
    symbols: i64,
    imports: i64,
}

/// `map --totals`: the two aggregate sections, without the four that cost.
///
/// `map`'s expense is not its aggregates — those are single GROUP BY queries —
/// but the two full row scans it does for directories and manifests, which grow
/// with the repo: 30 ms on redis, 200 ms on a 12k-file tree, against ~30 ms for
/// this. That gap is what `stats` was really buying, so it survives as a flag on
/// the command that already owns the concept rather than as a command of its own.
#[derive(Serialize)]
struct MapSummary {
    totals: MapTotals,
    languages: Vec<MapLang>,
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
/// Indexed source files whose symbols are stacked on a handful of lines, with a
/// note naming them — or `None` when there are none, or they are a rounding
/// error in the total.
///
/// Definitions per *distinct* line is the whole test, and it needs nothing
/// stored: hand-written code declares about one thing per line, so the ratio
/// sits at 1.0 and is bounded above by the language's grammar. Measured on a
/// real repo, 166 files sat below 2.0 and the three vendor bundles sat at 220,
/// 459 and 1152 — two orders of magnitude of empty space, so the cutoff is not
/// a number anybody has to tune. It under-reports by design: a pretty-printed
/// generated file is indistinguishable from hand-written source by this test,
/// and missing one is much cheaper than calling somebody's code machine output.
fn generated_files(conn: &rusqlite::Connection, total_symbols: i64) -> Result<Option<String>> {
    /// Definitions per distinct line. Anything hand-written is near 1.
    const DENSITY: f64 = 20.0;
    /// Below this a file is too small for the ratio to mean anything — a
    /// one-line SQL table with a dozen columns is not a bundle.
    const MIN_SYMBOLS: i64 = 50;

    let mut stmt = conn.prepare(
        "SELECT path, COUNT(*) AS syms, COUNT(DISTINCT start_line) AS lines
         FROM facts WHERE kind <> 'import'
         GROUP BY path
         HAVING syms >= ?1 AND syms >= ?2 * lines
         ORDER BY syms DESC",
    )?;
    let rows = stmt.query_map(rusqlite::params![MIN_SYMBOLS, DENSITY], |r| {
        Ok((
            r.get::<_, String>(0)?,
            r.get::<_, i64>(1)?,
            r.get::<_, i64>(2)?,
        ))
    })?;
    let hits: Vec<(String, i64, i64)> = rows.filter_map(|r| r.ok()).collect();
    if hits.is_empty() {
        return Ok(None);
    }
    let owned: i64 = hits.iter().map(|(_, syms, _)| syms).sum();
    // If they own a sliver of the repo they are not distorting anything, and a
    // note about them is the noise these notes exist to avoid.
    if owned * 4 < total_symbols {
        return Ok(None);
    }
    let listed = hits
        .iter()
        .take(3)
        .map(|(p, syms, lines)| format!("{p} ({syms} on {lines} line(s))"))
        .collect::<Vec<_>>()
        .join(", ");
    let more = if hits.len() > 3 {
        format!(", +{} more", hits.len() - 3)
    } else {
        String::new()
    };
    Ok(Some(format!(
        "{} of {total_symbols} symbols come from {} file(s) whose definitions are stacked on a few \
         lines, which is what generated or minified source looks like: {listed}{more}. They will \
         dominate `hotspots`, `unused` and `clones` — `.codeqignore` them if they are vendored",
        owned,
        hits.len(),
    )))
}

/// The commonest extensions among files codeq assigned no language, most files
/// first, as `.ext (N)`. Capped at three: the point is to say what the bulk is,
/// not to enumerate a tail.
/// A committed data tree: a language codeq *does* parse, holding a large share
/// of the indexed bytes and producing no symbols.
///
/// This is the expensive case, and it was the silent one. A `.png` has no
/// grammar, so it is never content-indexed and costs nothing but a line in the
/// language table — and that is the case the `(unknown)` note above warns about.
/// A tree of `.json` backups *is* text: it goes into the trigram index whole. On
/// one dashboard repo, 14.5 MB of `updater/backups/*.json` against 700 KB of
/// actual code produced a **129 MB** `index.db`, and `map` said nothing at all.
/// The advice is the same either way (`.codeqignore`), so the warning should be
/// too.
///
/// Thresholds are deliberately blunt — no symbols at all, a fifth of the bytes,
/// and at least a megabyte — because the note exists to catch the case that is
/// off by an order of magnitude, not to grade a repo's data-to-code ratio.
fn committed_data(
    conn: &rusqlite::Connection,
    languages: &[MapLang],
    totals: &MapTotals,
) -> Result<Option<String>> {
    let mut worst: Option<&MapLang> = None;
    for l in languages {
        if l.language == "(unknown)" || l.symbols > 0 {
            continue;
        }
        if l.bytes * 5 < totals.bytes || l.bytes < 1 << 20 {
            continue;
        }
        if worst.is_none_or(|w| l.bytes > w.bytes) {
            worst = Some(l);
        }
    }
    let Some(l) = worst else { return Ok(None) };

    // Name where it lives, because the note is only actionable if the reader
    // knows what to write in `.codeqignore`.
    let mut stmt = conn.prepare("SELECT path, size_bytes FROM files WHERE language = ?1")?;
    let mut by_dir: BTreeMap<String, (i64, i64)> = BTreeMap::new();
    for row in stmt.query_map([&l.language], |r| {
        Ok((r.get::<_, String>(0)?, r.get::<_, i64>(1)?))
    })? {
        let (path, bytes) = row?;
        let dir = path
            .rsplit_once('/')
            .map_or(String::new(), |(d, _)| d.to_string());
        let e = by_dir.entry(dir).or_default();
        e.0 += bytes;
        e.1 += 1;
    }
    let where_ = by_dir
        .iter()
        .max_by_key(|(_, (bytes, _))| *bytes)
        .filter(|(dir, _)| !dir.is_empty())
        .map(|(dir, (bytes, files))| {
            let s = if *files == 1 { "" } else { "s" };
            format!(" — {} in {dir}/ ({files} file{s})", human_bytes(*bytes))
        })
        .unwrap_or_default();

    Ok(Some(format!(
        "{} of {} is {} that defines nothing{where_}; unlike a binary it is \
         indexed for search, so it dominates the index — `.codeqignore` it if \
         it is data rather than code",
        human_bytes(l.bytes),
        human_bytes(totals.bytes),
        l.language,
    )))
}

fn unindexed_extensions(conn: &rusqlite::Connection) -> Result<Vec<String>> {
    let mut stmt = conn.prepare("SELECT path FROM files WHERE language IS NULL")?;
    let mut counts: BTreeMap<String, i64> = BTreeMap::new();
    for path in stmt.query_map([], |r| r.get::<_, String>(0))? {
        let path = path?;
        let name = path.rsplit('/').next().unwrap_or(&path);
        // A dotfile with no suffix (`.gitignore`) has no extension to report.
        let ext = match name.rsplit_once('.') {
            Some((stem, ext)) if !stem.is_empty() && !ext.is_empty() => format!(".{ext}"),
            _ => "(no extension)".to_string(),
        };
        *counts.entry(ext).or_default() += 1;
    }
    let mut rows: Vec<(String, i64)> = counts.into_iter().collect();
    rows.sort_by(|a, b| b.1.cmp(&a.1).then(a.0.cmp(&b.0)));
    rows.truncate(3);
    Ok(rows
        .into_iter()
        .map(|(e, n)| format!("{e} ({n})"))
        .collect())
}

/// Bytes at a unit that keeps the number meaningful. Fixing this at MB rendered
/// a 313 KB repo as "0 MB of 0 MB", which is a sentence with its quantities
/// removed.
fn human_bytes(n: i64) -> String {
    const KB: f64 = 1024.0;
    let b = n as f64;
    if b >= KB * KB * KB {
        format!("{:.1} GB", b / (KB * KB * KB))
    } else if b >= KB * KB {
        format!("{:.0} MB", b / (KB * KB))
    } else if b >= KB {
        format!("{:.0} KB", b / KB)
    } else {
        format!("{n} B")
    }
}

pub fn cmd_map(cwd: &Path, limit: usize, totals_only: bool, out: Out) -> Result<()> {
    let s = Session::open(cwd, false, out)?;
    let conn = &s.db.conn;
    let cap = if limit == 0 { 1_000_000 } else { limit };

    let totals = MapTotals {
        files: conn.query_row("SELECT COUNT(*) FROM files", [], |r| r.get(0))?,
        bytes: conn.query_row("SELECT COALESCE(SUM(size_bytes),0) FROM files", [], |r| {
            r.get(0)
        })?,
        symbols: conn.query_row(
            "SELECT COUNT(*) FROM facts WHERE kind <> 'import'",
            [],
            |r| r.get(0),
        )?,
        imports: conn.query_row(
            "SELECT COUNT(*) FROM facts WHERE kind = 'import' AND is_derived = 0",
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
            Ok((
                r.get::<_, String>(0)?,
                r.get::<_, i64>(1)?,
                r.get::<_, i64>(2)?,
            ))
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
    let data_note = committed_data(conn, &languages, &totals)?;
    languages.truncate(cap);

    // A repo can be mostly things codeq cannot index — 2,108 flag PNGs against 12
    // Python files, in the case that prompted this. The `(unknown)` language row
    // states that accurately and still reads like a gap in coverage rather than
    // "this repo is mostly images", so say it in words when it dominates. A note,
    // not a new field: it is guidance for reading the number, which is exactly
    // what the note channel is for.
    let unindexed = languages
        .iter()
        .find(|l| l.language == "(unknown)")
        .map(|l| (l.files, l.bytes));
    let note = unindexed.and_then(|(files, bytes)| {
        let majority = files * 2 > totals.files || bytes * 2 > totals.bytes;
        if !majority {
            return None;
        }
        // Name the extensions rather than the category. "Assets, data or
        // binaries" was a guess, and on a Business Central repo it was wrong in
        // the direction that costs most: 31 of the 33 files were `.al`, the
        // entire source of the project, described to its author as data and
        // implicitly proposed for `.codeqignore`. The extension is a fact, and
        // it tells a geospatial dump and an unsupported language apart without
        // codeq having to know which it is looking at.
        let exts = unindexed_extensions(conn).unwrap_or_default();
        let what = if exts.is_empty() {
            String::new()
        } else {
            format!(" — codeq has no grammar for {}", exts.join(", "))
        };
        Some(format!(
            "{files} of {} files ({} of {}) carry no symbols{what}; \
             `--language` on `tree`/`files` scopes to code",
            totals.files,
            human_bytes(bytes),
            human_bytes(totals.bytes),
        ))
    });

    // The `(unknown)` note above cannot see the other way a repo misleads: a
    // committed vendor bundle *is* source, *is* parsed, and *does* produce
    // symbols — so it costs nothing in index size and quietly owns every
    // symbol-ranking command. On one dashboard repo three files held 5,507 of
    // 7,558 symbols, and `hotspots` — the command for meeting an unfamiliar
    // codebase — answered `function t refs=45993`.
    let note = match generated_files(conn, totals.symbols)? {
        Some(extra) => Some(match note {
            Some(n) => format!("{n}\n{extra}"),
            None => extra,
        }),
        None => note,
    };
    let note = match data_note {
        Some(extra) => Some(match note {
            Some(n) => format!("{n}\n{extra}"),
            None => extra,
        }),
        None => note,
    };

    // Everything above is aggregate queries; everything below scans rows. Stop
    // here when only the totals were asked for.
    if totals_only {
        return s.emit_noted(MapSummary { totals, languages }, note);
    }

    // Imports grouped by module (raw string), counted by distinct importing file.
    let imports_by_module: Vec<ModuleImports> = {
        let mut st = conn.prepare(
            "SELECT name, COUNT(DISTINCT path) c FROM facts
             WHERE kind = 'import' AND is_derived = 0 AND name IS NOT NULL
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
            DirStat {
                dir,
                files,
                symbols,
            }
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
    s.emit_noted(data, note)
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
///
/// `commands` is passed in from the clap parser rather than written out here.
/// The hand-maintained list drifted: it had gone nine commands stale — including
/// `explore`, the one an agent should reach for first — so the command whose only
/// job is to say what codeq can do was under-selling it by a quarter.
pub fn cmd_capabilities(commands: &[&str], out: Out) -> Result<()> {
    // Distinct languages a file can be classified as, derived from the extension
    // table rather than restated (`json!` cannot host the turbofish inline).
    let mut classified: Vec<&str> = crate::index::EXTENSIONS.iter().map(|(_, l)| *l).collect();
    classified.sort_unstable();
    classified.dedup();
    let data = serde_json::json!({
        "result_schema": RESULT_SCHEMA,
        "index_schema": SCHEMA_VERSION,
        "commands": commands,
        // Per-language support, so "no results" can be told from "not supported".
        "language_support": crate::facts::LANGUAGES
            .iter()
            .filter_map(|l| crate::facts::support(l))
            .collect::<Vec<_>>(),
        // Languages with symbol + import extraction. Derived, not listed: this
        // was a third hand-kept copy, and it had already drifted out of step
        // with `language_support` above (it omitted markdown).
        "extraction_languages": crate::facts::LANGUAGES,
        // Languages recognised for file classification (extraction may be absent).
        "classified_languages": classified,
        // Classified but deliberately not parsed: indexed as files and fully
        // searchable, but they contribute no definitions. Stated so that an
        // empty `symbols` on a .json file reads as "by design", not "broken".
        "data_only_languages": crate::index::NO_EXTRACTION,
        "fact_kinds": [
            "function", "method", "struct", "enum", "union", "trait", "module",
            "constant", "typealias", "macro", "class", "interface", "type", "import",
            "component", "field", "variant", "heading",
        ],
        "notes": {
            "imports": "raw syntactic module strings; not resolved",
            "deps": "an import naming a directory resolves through that directory's conventional entry file (index.js, __init__.py, mod.rs) or any .tf",
            "clones": "structural equality of the syntax tree with identifiers and literals normalised away; exact, never a similarity score. min_nodes filters by how much code is involved",
            "unused": "skips GraphQL/protobuf definitions and Terraform outputs — they exist to be consumed outside the repo, so absence of local references says nothing",
            "members": "field/variant facts carry `owner` (the declaring type); extracted for Rust, Go, Java, TypeScript/TSX, C#, Python, C, C++, PHP, Kotlin, Swift, Protobuf, GraphQL, HCL, Zig, Solidity, Dart, Scala. Python attributes are assignments, so a member is its earliest `self.x =` or class-body assignment",
            "hcl": "block kinds are the block keyword verbatim (resource/variable/module/…), so the vocabulary is open-ended; a block is named by its last label and owned by its type label. A module `source` is an import, but `deps` resolves file stems and a module target is a directory, so it stays unresolved",
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

/// One member of a group of structurally identical definitions.
#[derive(Serialize)]
struct CloneRow {
    /// Shared shape hash — the group key.
    shape: String,
    /// How many definitions share it.
    group: i64,
    /// Nodes in the shared shape: bigger means more code duplicated.
    nodes: i64,
    name: String,
    kind: String,
    path: String,
    line: i64,
    /// The directories this group spans, sorted and joined — the same value on
    /// every row of the group.
    ///
    /// On a multi-package monorepo the biggest groups are *intentional* mirrors:
    /// a PHP `core` package vendored verbatim into the publishable `laravel` and
    /// `php` packages, a Python monolith re-shipping each adapter it also
    /// distributes standalone. codeq is right that they are clones, and the
    /// deliberate ones bury the accidental ones. This gives a reviewer one thing
    /// to dismiss per pair (`--fields pair,name`, or `--group`) instead of one
    /// per row, without a config file encoding the judgement — which would make
    /// an empty `clones` result unfalsifiable without reading it.
    pair: String,
}

/// `clones` — definitions whose syntax shape is identical up to renaming.
///
/// Exact, not fuzzy: shapes match or they do not, so there is no threshold and no
/// score. `--min-nodes` raises the floor on how much code has to be involved
/// before a group is worth reporting; it filters, it does not rank.
pub fn cmd_clones(
    cwd: &Path,
    path: Option<&str>,
    kind: Option<&str>,
    min_nodes: i64,
    include_tests: bool,
    limit: usize,
    out: Out,
) -> Result<()> {
    let s = Session::open(cwd, false, out)?;
    let filters = Filters::new().like("path", path).eq("kind", kind);
    // Group first, then re-join, so only shapes with more than one holder are read.
    let sql = format!(
        "SELECT shape, cnt, shape_nodes, name, kind, path, start_line FROM (
             SELECT shape, COUNT(*) OVER (PARTITION BY shape) AS cnt,
                    shape_nodes, name, kind, path, start_line
             FROM facts
             WHERE shape IS NOT NULL AND shape_nodes >= ?{}
         ) WHERE cnt > 1
         ORDER BY shape_nodes DESC, shape, path, start_line{}",
        filters.sql(),
        limit_clause(limit),
    );
    let mut stmt = s.db.conn.prepare(&sql)?;
    let mut params: Vec<&dyn rusqlite::ToSql> = vec![&min_nodes];
    for a in filters.args() {
        params.push(a);
    }
    let rows = stmt.query_map(params.as_slice(), |r| {
        Ok(CloneRow {
            shape: r.get(0)?,
            group: r.get(1)?,
            nodes: r.get(2)?,
            name: r.get(3)?,
            kind: r.get(4)?,
            path: r.get(5)?,
            line: r.get(6)?,
            pair: String::new(),
        })
    })?;
    let mut data: Vec<CloneRow> = rows.collect::<std::result::Result<_, _>>()?;

    // The directory span of each group, filled in once the whole group is known.
    {
        let dir_of = |p: &str| -> String {
            match p.rfind('/') {
                Some(i) => p[..i].to_string(),
                None => "(root)".to_string(),
            }
        };
        let mut dirs: BTreeMap<String, std::collections::BTreeSet<String>> = BTreeMap::new();
        for r in &data {
            dirs.entry(r.shape.clone())
                .or_default()
                .insert(dir_of(&r.path));
        }
        for r in &mut data {
            if let Some(d) = dirs.get(&r.shape) {
                r.pair = d.iter().cloned().collect::<Vec<_>>().join(" <-> ");
            }
        }
    }

    // Tests dominate this result and always will: two assertions in sequence is
    // the same shape however different the assertions are, so a suite of small
    // tests is a field of true positives nobody wants. On a real repo every row
    // `clones` returned was test boilerplate while it found none of the five
    // duplicated implementations the refactor was actually about. Excluded by
    // default, the same rule and the same predicate `unused` and `untested` use.
    if !include_tests {
        let scopes = TestScopes::load(&s.db.conn)?;
        data.retain(|r| !is_test_site(&r.path, Some(&r.name), r.line, &scopes));
        // Dropping one half of a pair leaves a "clone" with nothing to be a clone
        // of, so re-group and keep only shapes that still have more than one.
        let mut per_shape: HashMap<String, i64> = HashMap::new();
        for r in &data {
            *per_shape.entry(r.shape.clone()).or_default() += 1;
        }
        data.retain(|r| per_shape.get(&r.shape).copied().unwrap_or(0) > 1);
        for r in &mut data {
            r.group = per_shape.get(&r.shape).copied().unwrap_or(0);
        }
    }
    // An empty `clones` is the result most likely to be misread as "no
    // duplication", when it means "no *exact* structural copies" — so point at
    // the tool that finds the near-duplicates this one cannot see.
    let note = if data.is_empty() {
        match note_if_scope_empty(&s, path, "an exact structural clone")? {
            Some(n) => Some(n),
            None => Some(
                "no exact structural clones — two implementations differing only in their accessor expressions have different trees, so try `astgrep` for those".to_string(),
            ),
        }
    } else {
        None
    };
    s.emit_noted(data, note)
}
