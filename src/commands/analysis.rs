//! Whole-repo analysis: `unused`, `entrypoints`, `hotspots`, `clones`.

use super::*;

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
    let filters = Filters::new().like("path", path);
    let mut sql = String::from(
        "SELECT name, kind, path, start_line, decorator, is_test, language, visibility
         FROM facts WHERE kind <> 'import'",
    );
    if !include_tests {
        sql.push_str(" AND is_test = 0");
    }
    sql.push_str(filters.sql());
    sql.push_str(" ORDER BY path, start_line");
    let mut stmt = s.db.conn.prepare(&sql)?;
    let rows = stmt.query_map(params_with(&[], &filters).as_slice(), |r| {
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
pub(super) fn name_is_used(s: &Session, name: &str) -> Result<bool> {
    let names = HashSet::from([name.to_string()]);
    // No declaration sites to exclude: the only caller reaches here when the
    // name has no definition codeq indexes, so every occurrence is a use.
    let sites = HashMap::new();
    Ok(used_definition_names(s, &names, &sites)?.contains(name))
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

/// Arguments of `hotspots`.
pub struct HotspotsArgs<'a> {
    pub path: Option<&'a str>,
    pub kind: Option<&'a str>,
    pub language: Option<&'a str>,
    pub calls_only: bool,
    pub min_refs: i64,
    pub limit: usize,
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
pub fn cmd_hotspots(cwd: &Path, args: HotspotsArgs, out: Out) -> Result<()> {
    let HotspotsArgs { path, kind, language, calls_only, min_refs, limit } = args;
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
