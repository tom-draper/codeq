//! Command implementations. This file holds the machinery every command shares
//! (`Out`, `Session`, the filter/limit helpers and empty-result diagnosis); each
//! topic lives in its own submodule and is re-exported here so callers keep
//! writing `commands::cmd_x`.

use std::cell::OnceCell;
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
    self, innermost, load_file_defs, prefilter_of, regex_prefilter, run_search, run_search_acc,
    scan_stdin, scan_unindexed, FileDef, Hit, InFilter, OutMode, ScanOpts, ScanScope, SearchData,
};

mod analysis;
mod callgraph;
mod explore;
mod find;
mod imports;
mod map;
mod meta;
mod project;
mod read;
mod symbols;
mod testing;
mod types;

pub use analysis::*;
pub use callgraph::*;
pub use explore::*;
pub use find::*;
pub use imports::*;
pub use map::*;
pub use meta::*;
pub use project::*;
pub use read::*;
pub use symbols::*;
pub use testing::*;
pub use types::*;

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
    // Whole-table loads, kept for the life of the query. A walk that visits
    // many symbols (`callers --depth`, `untested`, `affected`) asks for each of
    // these once per symbol, and each is a full scan of the import or
    // definition rows: on a 1,500-file repo that was most of a 3.8s
    // `callers --depth 3`.
    dep_graph: OnceCell<deps::Graph>,
    test_scopes: OnceCell<testing::TestScopes>,
    import_lines: OnceCell<HashMap<String, HashSet<i64>>>,
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
            db.reset_fts()?;
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
            dep_graph: OnceCell::new(),
            test_scopes: OnceCell::new(),
            import_lines: OnceCell::new(),
        })
    }

    /// The internal import graph, built on first use.
    fn dep_graph(&self) -> Result<&deps::Graph> {
        if self.dep_graph.get().is_none() {
            let _ = self.dep_graph.set(imports::load_dep_graph(&self.db)?);
        }
        Ok(self.dep_graph.get().expect("set above"))
    }

    /// The repo's test-like containers, loaded on first use.
    fn test_scopes(&self) -> Result<&testing::TestScopes> {
        if self.test_scopes.get().is_none() {
            let _ = self
                .test_scopes
                .set(testing::TestScopes::load(&self.db.conn)?);
        }
        Ok(self.test_scopes.get().expect("set above"))
    }

    /// Whether `line` of `path` is an import statement.
    fn is_import_line(&self, path: &str, line: i64) -> Result<bool> {
        if self.import_lines.get().is_none() {
            let mut lines: HashMap<String, HashSet<i64>> = HashMap::new();
            let mut st = self
                .db
                .conn
                .prepare("SELECT path, start_line FROM facts WHERE kind = 'import'")?;
            let rows = st.query_map([], |r| Ok((r.get::<_, String>(0)?, r.get::<_, i64>(1)?)))?;
            for row in rows {
                let (p, line) = row?;
                lines.entry(p).or_default().insert(line);
            }
            let _ = self.import_lines.set(lines);
        }
        let lines = self.import_lines.get().expect("set above");
        Ok(lines.get(path).is_some_and(|l| l.contains(&line)))
    }

    /// Build the envelope and print it honoring the caller's render options.
    fn emit<T: Serialize>(&self, data: T) -> Result<()> {
        self.emit_noted(data, None)
    }

    /// Emit with a note explaining an empty result. The note is only ever
    /// diagnostic: it never changes `data`, so a parser can keep ignoring it.
    fn emit_noted<T: Serialize>(&self, data: T, note: Option<String>) -> Result<()> {
        let index = IndexMeta {
            schema: SCHEMA_VERSION,
            fresh: true, // we just refreshed before answering
            reindexed_files: self.reindexed,
            rebuilt_from_schema: self.db.rebuilt_from.clone(),
            partial_failures: self.partial_failures,
            elapsed_ms: self.started.elapsed().as_millis() as u64,
        };
        let render = Render {
            json: self.out.json,
            meta: self.out.meta,
            pretty: self.out.pretty,
            fields: self.out.fields.as_deref(),
            group: self.out.group,
            max_per_file: self.out.max_per_file,
        };
        // HEAD and the dirty flag each cost a `git` process, and only the repo
        // block reports them. Text output and a quiet JSON answer never print
        // that block, so asking anyway was two spawns on every query: about a
        // third of a warm lookup, and `git status` grows with the tree.
        let shown = render.includes_meta(&index);
        let repo = RepoMeta {
            root: self.repo.root.to_string_lossy().into_owned(),
            vcs: self.repo.vcs(),
            head: if shown { self.repo.head() } else { None },
            dirty: if shown { self.repo.dirty() } else { None },
        };
        Envelope::new(repo, index, data).note(note).print(render)
    }
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
        "SELECT COUNT(*) FROM files WHERE path LIKE ?1 ESCAPE '\\'",
        [format!("%{}%", escape_like_literal(p))],
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

/// Optional `AND …` filter clauses shared by the list commands, built with
/// positional `?` params (bound in append order, after any base params the query
/// already has). Centralizes the repeated path-substring / exact-column-match
/// pattern so a new filtered command can't misthread its parameters.
#[derive(Default)]
struct Filters {
    sql: String,
    args: Vec<String>,
}

fn escape_like_literal(value: &str) -> String {
    value
        .replace('\\', "\\\\")
        .replace('%', "\\%")
        .replace('_', "\\_")
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
            let escaped = escape_like_literal(n);
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
