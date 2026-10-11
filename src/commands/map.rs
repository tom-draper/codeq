//! `map` — the factual aggregation of the index.

use super::*;

pub(super) fn is_false(b: &bool) -> bool {
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
/// A tree of `.json` backups *is* text, so it is searched. On one dashboard
/// repo, 14.5 MB of `updater/backups/*.json` against 700 KB of actual code
/// produced a **129 MB** `index.db`, and `map` said nothing at all. The index
/// no longer stores text or trigram positions, so that cost is gone; what is
/// left is that every search without a selective literal reads the tree, and
/// its hits crowd out the code's. The advice is the same either way
/// (`.codeqignore`), so the warning should be too.
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
         searched, so it slows and crowds every broad search — `.codeqignore` \
         it if it is data rather than code",
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

    let totals = map_totals(conn)?;
    let mut languages = map_languages(conn)?;
    let data_note = committed_data(conn, &languages, &totals)?;
    languages.truncate(cap);

    let note = unindexed_note(conn, &languages, &totals);
    // The `(unknown)` note cannot see the other way a repo misleads: a
    // committed vendor bundle *is* source, *is* parsed, and *does* produce
    // symbols — so it costs nothing in index size and quietly owns every
    // symbol-ranking command. On one dashboard repo three files held 5,507 of
    // 7,558 symbols, and `hotspots` — the command for meeting an unfamiliar
    // codebase — answered `function t refs=45993`.
    let note = append_note(note, generated_files(conn, totals.symbols)?);
    let note = append_note(note, data_note);

    // Everything above is aggregate queries; everything below scans rows. Stop
    // here when only the totals were asked for.
    if totals_only {
        return s.emit_noted(MapSummary { totals, languages }, note);
    }

    let (directories, manifests) = directories_and_manifests(conn, cap)?;
    let data = RepoMap {
        totals,
        languages,
        directories,
        manifests,
        imports_by_module: imports_by_module(conn, cap)?,
        definitions_by_file: definitions_by_file(conn, cap)?,
    };
    s.emit_noted(data, note)
}

fn map_totals(conn: &rusqlite::Connection) -> Result<MapTotals> {
    Ok(MapTotals {
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
    })
}

/// Languages: file/byte counts merged with symbol counts, most files first.
fn map_languages(conn: &rusqlite::Connection) -> Result<Vec<MapLang>> {
    let mut langs: BTreeMap<String, (i64, i64, i64)> = BTreeMap::new();
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
    Ok(languages)
}

/// A repo can be mostly things codeq cannot index — 2,108 flag PNGs against 12
/// Python files, in the case that prompted this. The `(unknown)` language row
/// states that accurately and still reads like a gap in coverage rather than
/// "this repo is mostly images", so say it in words when it dominates. A note,
/// not a new field: it is guidance for reading the number, which is exactly
/// what the note channel is for.
fn unindexed_note(
    conn: &rusqlite::Connection,
    languages: &[MapLang],
    totals: &MapTotals,
) -> Option<String> {
    let (files, bytes) = languages
        .iter()
        .find(|l| l.language == "(unknown)")
        .map(|l| (l.files, l.bytes))?;
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
}

/// Add a further line to a note, if there is one.
fn append_note(note: Option<String>, extra: Option<String>) -> Option<String> {
    match extra {
        Some(extra) => Some(match note {
            Some(n) => format!("{n}\n{extra}"),
            None => extra,
        }),
        None => note,
    }
}

/// Imports grouped by module (raw string), counted by distinct importing file.
fn imports_by_module(conn: &rusqlite::Connection, cap: usize) -> Result<Vec<ModuleImports>> {
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
    Ok(rows.collect::<std::result::Result<Vec<_>, _>>()?)
}

/// Definitions grouped by file.
fn definitions_by_file(conn: &rusqlite::Connection, cap: usize) -> Result<Vec<FileDefs>> {
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
    Ok(rows.collect::<std::result::Result<Vec<_>, _>>()?)
}

/// Top-level directories with their file and symbol counts, and the project
/// manifests: one scan of file paths.
fn directories_and_manifests(
    conn: &rusqlite::Connection,
    cap: usize,
) -> Result<(Vec<DirStat>, Vec<String>)> {
    let mut dir_files: BTreeMap<String, (i64, i64)> = BTreeMap::new();
    let mut manifests: Vec<String> = Vec::new();
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
    let mut dir_syms: BTreeMap<String, i64> = BTreeMap::new();
    let mut st = conn.prepare("SELECT path FROM facts WHERE kind <> 'import'")?;
    let rows = st.query_map([], |r| r.get::<_, String>(0))?;
    for row in rows {
        *dir_syms.entry(top_dir(&row?)).or_default() += 1;
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
    Ok((directories, manifests))
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
