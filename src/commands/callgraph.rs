//! Call-graph commands: `callers`, `callees`, `callpath`, `affected`, and the
//! import-scoped `Resolver` they share.

use super::*;

#[derive(Serialize)]
pub(super) struct Caller {
    #[serde(skip_serializing_if = "Option::is_none")]
    // This row *is* a definition, so its identity is `name` (ROW_KEY_RULE).
    #[serde(rename = "name")]
    pub(super) symbol: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub(super) kind: Option<String>,
    pub(super) path: String,
    pub(super) line: usize,
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
    pub(super) via: Option<String>,
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
pub(super) fn truncation_note(shown: usize, total: usize) -> Option<String> {
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

pub(super) fn callers_of(
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
pub(super) fn alias_hits(
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
pub(super) struct Callee {
    pub(super) name: String,
    pub(super) kind: String,
    pub(super) path: String,
    pub(super) line: i64,
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
pub(super) fn callees_of(
    s: &Session,
    name: &str,
    path: Option<&str>,
    resolved: bool,
    examples: bool,
) -> Result<Vec<Callee>> {
    let targets = definition_ranges(s, name, path)?;
    if targets.is_empty() {
        return Ok(Vec::new());
    }
    let mut called = scan_called_names(s, &targets, examples)?;
    called.idents.remove(name);
    if called.idents.is_empty() {
        return Ok(Vec::new());
    }
    let mut out = callee_definitions(s, name, &called)?;
    if resolved {
        keep_reachable(s, &targets, &mut out)?;
    }
    out.sort_by(|a, b| {
        (a.name.as_str(), a.path.as_str(), a.line).cmp(&(b.name.as_str(), b.path.as_str(), b.line))
    });
    Ok(out)
}

/// `(path, start_byte, end_byte)` of each definition of `name`, optionally
/// scoped to files whose path contains `path`.
fn definition_ranges(
    s: &Session,
    name: &str,
    path: Option<&str>,
) -> Result<Vec<(String, usize, usize)>> {
    let mut sql = String::from(
        "SELECT path, start_byte, end_byte FROM facts WHERE name = ?1 AND kind <> 'import'",
    );
    let like = path.map(|p| format!("%{}%", escape_like_literal(p)));
    let mut params: Vec<&dyn rusqlite::ToSql> = vec![&name];
    if let Some(l) = &like {
        sql.push_str(" AND path LIKE ?2 ESCAPE '\\'");
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
    Ok(rows.collect::<std::result::Result<Vec<_>, _>>()?)
}

/// What the bodies of a symbol call.
struct CalledNames {
    /// Names called inside the target body/bodies.
    idents: HashSet<String>,
    /// Under `--examples`, each name's call as written and the line it sits on,
    /// recorded while the body is in hand.
    sites: HashMap<String, (String, i64)>,
    /// For a call such as `context::build()`, the target file of the imported
    /// `context` module. A receiver is only trusted when it exactly names a
    /// direct import; ordinary value receivers and unknown modules keep the
    /// established name-based superset.
    module_targets: HashMap<String, Vec<Option<HashSet<String>>>>,
}

/// The name a syntax call is looked up under, and the files it is pinned to when
/// its receiver (or, for a bare call, the called name itself) is a direct import.
fn resolve_syntax_call(
    graph: &deps::Graph,
    path: &str,
    word: &str,
    receiver: Option<&str>,
) -> (String, Option<HashSet<String>>) {
    let lookup = if receiver.is_none() {
        graph.source_name_for_binding(path, word).unwrap_or(word).to_string()
    } else {
        word.to_string()
    };
    let pinned = receiver
        .map(|receiver| {
            graph
                .direct_targets_for_receiver(path, receiver)
                .into_iter()
                .map(str::to_string)
                .collect::<HashSet<_>>()
        })
        .filter(|targets| !targets.is_empty());
    let pinned = pinned.or_else(|| {
        let targets = graph
            .direct_targets_for_receiver(path, word)
            .into_iter()
            .map(str::to_string)
            .collect::<HashSet<_>>();
        (!targets.is_empty()).then_some(targets)
    });
    (lookup, pinned)
}

/// Names called inside the target body/bodies. The receiver comes from the parse
/// tree rather than from walking bytes backwards: `calls_in` already resolved
/// `subprocess.run` to callee `run` on receiver `subprocess`, which is both
/// cheaper and correct for chained or whitespace-separated receivers.
fn scan_called_names(
    s: &Session,
    targets: &[(String, usize, usize)],
    examples: bool,
) -> Result<CalledNames> {
    let mut called = CalledNames {
        idents: HashSet::new(),
        sites: HashMap::new(),
        module_targets: HashMap::new(),
    };
    let graph = load_dep_graph(&s.db)?;
    let mut body_stmt = s.db.conn.prepare("SELECT body FROM content_fts WHERE path = ?1")?;
    let mut calls_stmt = s.db.conn.prepare(
        "SELECT start_byte, name, receiver FROM calls
         WHERE path = ?1 AND start_byte >= ?2 AND start_byte < ?3
         ORDER BY start_byte",
    )?;
    let mut span_stmt = s.db.conn.prepare(
        "SELECT start_byte, end_byte FROM spans WHERE path = ?1 ORDER BY start_byte",
    )?;
    for (path, sb, eb) in targets {
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
            let rows = calls_stmt.query_map(rusqlite::params![path, *sb as i64, *eb as i64], |r| {
                Ok((
                    r.get::<_, i64>(0)? as usize,
                    r.get::<_, String>(1)?,
                    r.get::<_, Option<String>>(2)?,
                ))
            })?;
            for row in rows {
                let (off, word, recv) = row?;
                let (lookup, pinned) = resolve_syntax_call(&graph, path, &word, recv.as_deref());
                // An empty target set is no evidence: it may be an external
                // module or a value receiver. `pinned` is `None` then, so
                // candidate filtering remains conservative.
                called.module_targets.entry(lookup.clone()).or_default().push(pinned);
                if examples {
                    let expr = match &recv {
                        Some(r) => format!("{r}.{word}"),
                        None => word.clone(),
                    };
                    let line = b[..off.min(b.len())].bytes().filter(|c| *c == b'\n').count() as i64 + 1;
                    called.sites.entry(lookup.clone()).or_insert((expr, line));
                }
                called.idents.insert(lookup);
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
        collect_idents(slice, *sb, &prose, &mut called.idents);
    }
    Ok(called)
}

/// The definitions of the called names, other than `name` itself, that the call
/// evidence allows. Looks up only the referenced names via the facts(name)
/// index, in chunks to stay under SQLite's bound-parameter limit — instead of
/// scanning every fact.
fn callee_definitions(s: &Session, name: &str, called: &CalledNames) -> Result<Vec<Callee>> {
    let ident_list: Vec<&String> = called.idents.iter().collect();
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
            let allowed = called.module_targets.get(&n).is_none_or(|calls| {
                calls
                    .iter()
                    .any(|targets| targets.as_ref().is_none_or(|targets| targets.contains(&p)))
            });
            if n != name && allowed && seen.insert((n.clone(), p.clone(), l)) {
                let site = called.sites.get(&n);
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
    Ok(out)
}

/// Import-scoped resolution: a genuine callee's definition should live in a
/// file `name`'s own file can reach through imports (or the same file). Unlike
/// the name-based Resolver used for callers, here the anchor is fixed — `name`'s
/// definition file(s) — so we can prune by each callee's *specific* def-file
/// reachability, dropping unrelated same-named definitions in unreachable files
/// (the documented callees over-match). Heuristic, like all import resolution:
/// it can't tell which same-named def you meant if several sit in reachable
/// files, so it narrows a superset rather than pinpointing.
fn keep_reachable(
    s: &Session,
    targets: &[(String, usize, usize)],
    out: &mut Vec<Callee>,
) -> Result<()> {
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
    for c in out.iter() {
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
    Ok(())
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

/// Arguments of `callers`.
pub struct CallersArgs<'a> {
    pub name: &'a str,
    pub path: Option<&'a str>,
    pub depth: usize,
    pub resolved: bool,
    pub call_only: bool,
    pub qualifier: Option<&'a str>,
    pub examples: bool,
    pub limit: usize,
}

pub fn cmd_callers(cwd: &Path, args: CallersArgs, out: Out) -> Result<()> {
    let CallersArgs { name, path, depth, resolved, call_only, qualifier, examples, limit } = args;
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

/// Arguments of `callees`.
pub struct CalleesArgs<'a> {
    pub name: &'a str,
    pub path: Option<&'a str>,
    pub depth: usize,
    pub resolved: bool,
    pub examples: bool,
    pub limit: usize,
}

/// `callees <name>` — what this symbol references (by name).
pub fn cmd_callees(cwd: &Path, args: CalleesArgs, out: Out) -> Result<()> {
    let CalleesArgs { name, path, depth, resolved, examples, limit } = args;
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
        if let Some((kind, path, line)) = loc_of(from)? {
            return s.emit(vec![PathStep {
                symbol: from.to_string(),
                kind,
                path,
                line,
                depth: 0,
            }]);
        }
        let sites = definition_sites(&s, from)?;
        let note = if sites.is_empty() {
            format!("no symbol named '{from}' is defined in this repository")
        } else {
            format!(
                "'{from}' is defined at {} but nothing matches --path {}",
                sites
                    .iter()
                    .take(2)
                    .map(|(p, l)| format!("{p}:{l}"))
                    .collect::<Vec<_>>()
                    .join(", "),
                path.unwrap_or_default(),
            )
        };
        return s.emit_noted(Vec::<PathStep>::new(), Some(note));
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
pub(super) fn resolve_rows<T>(
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
