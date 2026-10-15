//! Repo-shape commands: `index`, `files`, `changed`, `tree`.

use super::*;

#[derive(Serialize)]
struct IndexReport {
    indexed_files: i64,
    reindexed: u64,
}

pub fn cmd_index(cwd: &Path, rebuild: bool, compact: bool, out: Out) -> Result<()> {
    let s = Session::open(cwd, rebuild, out)?;
    if compact {
        s.db.compact()?;
    }
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
pub(super) struct ChangedSymbolRow {
    pub(super) name: String,
    pub(super) kind: String,
    pub(super) path: String,
    #[serde(rename = "line")]
    pub(super) start_line: i64,
    pub(super) end_line: i64,
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

    let mut paths = s.repo.changed_since(compare_to)?.paths;
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
pub(super) fn touched_defs(s: &Session, compare_to: &str) -> Result<Vec<ChangedSymbolRow>> {
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

/// Arguments of `tree`.
pub struct TreeArgs<'a> {
    pub path: Option<&'a str>,
    pub language: Option<&'a str>,
    pub depth: usize,
    pub min_symbols: i64,
    pub files_too: bool,
    pub limit: usize,
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
pub fn cmd_tree(cwd: &Path, args: TreeArgs, out: Out) -> Result<()> {
    let TreeArgs {
        path,
        language,
        depth,
        min_symbols,
        files_too,
        limit,
    } = args;
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
