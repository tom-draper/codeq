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
    /// which turns a caller row into a usage example: `path:line:` the actual
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
    let opts = caller_scan_opts(call_only, qualifier);
    let mut hits = match run_search(
        &s.db.conn,
        &s.repo.root,
        &re,
        prefilter_of(name).as_deref(),
        ScanScope {
            path,
            ..Default::default()
        },
        &opts,
    )? {
        SearchData::Matches { matches, .. } => matches,
        _ => unreachable!(),
    };
    hits.extend(alias_hits(s, name, path, None, &opts)?);

    caller_rows(s, name, hits, call_only, qualifier, examples)
}

fn callers_for(s: &Session, names: &[String], path: Option<&str>) -> Result<Vec<Vec<Caller>>> {
    if let [name] = names {
        return Ok(vec![callers_of(s, name, path, false, None, false)?]);
    }
    let refs: Vec<&str> = names.iter().map(String::as_str).collect();
    let mut hits: Vec<Vec<Hit>> = (0..names.len()).map(|_| Vec::new()).collect();
    crate::search::scan_names(
        &s.db.conn,
        &s.repo.root,
        &refs,
        path,
        InFilter::Code,
        &mut |i, found| {
            hits[i].extend(found);
            false
        },
    )?;
    names
        .iter()
        .zip(hits)
        .map(|(name, hits)| {
            let mut hits = hits;
            hits.extend(alias_hits(
                s,
                name,
                path,
                None,
                &caller_scan_opts(false, None),
            )?);
            caller_rows(s, name, hits, false, None, false)
        })
        .collect()
}

fn caller_scan_opts(call_only: bool, qualifier: Option<&str>) -> ScanOpts {
    ScanOpts {
        before: 0,
        after: 0,
        out: OutMode::Matches,
        in_filter: InFilter::Code,
        limit: 0,
        call_only,
        qualifier: qualifier.map(str::to_string),
        identifier_boundary: true,
        word_regex: None,
        per_file: None,
        only_matching: false,
        invert: false,
    }
}

fn caller_rows(
    s: &Session,
    name: &str,
    hits: Vec<Hit>,
    call_only: bool,
    qualifier: Option<&str>,
    examples: bool,
) -> Result<Vec<Caller>> {
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
    // One row per (file, enclosing symbol). The representative site is the first
    // real usage; an import site is only the anchor when the file names the
    // symbol nowhere else, so a file that imports without using it is still
    // reported (just anchored at its import).
    let mut pick: HashMap<(String, Option<String>), (Hit, bool)> = HashMap::new();
    let mut order: Vec<(String, Option<String>)> = Vec::new();
    for h in hits {
        // Syntax filters already exclude declarations. Claiming their first
        // remaining hit would discard a recursive call.
        if !call_only && qualifier.is_none() && defsites.is_declaration(&h.path, h.line as i64) {
            continue; // the declaration itself
        }
        let is_import = s.is_import_line(&h.path, h.line as i64)?;
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

    let scopes = s.test_scopes()?;
    let mut out = Vec::new();
    for key in order {
        let (mut h, _) = match pick.remove(&key) {
            Some(v) => v,
            None => continue,
        };
        let test = is_test_site(&h.path, h.symbol.as_deref(), h.line as i64, scopes);
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
    if !s.may_be_aliased(name)? {
        return Ok(Vec::new());
    }
    let graph = s.dep_graph()?;
    let mut hits = Vec::new();
    for (file, binding) in graph.bindings_for_source(name) {
        if path.is_some_and(|scope| !file.contains(scope)) {
            continue;
        }
        let re = regex::Regex::new(&regex::escape(&binding))?;
        let like = prefilter_of(&binding);
        let data = run_search(
            &s.db.conn,
            &s.repo.root,
            &re,
            like.as_deref(),
            ScanScope {
                path: Some(&file),
                language,
                ..Default::default()
            },
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
    #[serde(skip)]
    pub(super) start_byte: usize,
    #[serde(skip)]
    owner: Option<String>,
    pub(super) resolution: &'static str,
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
    let candidates = callee_candidates(s, &called.idents)?;
    let mut out = select_callee_definitions(candidates, name, &called);
    if resolved {
        keep_reachable(s, &targets, &mut out)?;
    }
    classify_callees(&called, &mut out);
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
    module_targets: HashMap<String, Vec<CallConstraint>>,
}

/// Evidence from one call. Alternatives are unioned, so an unresolved call to
/// the same name preserves the conservative candidate set.
struct CallConstraint {
    files: Option<HashSet<String>>,
    owner: Option<String>,
}

impl CallConstraint {
    fn allows(&self, path: &str, owner: Option<&str>) -> bool {
        self.files.as_ref().is_none_or(|files| files.contains(path))
            && self
                .owner
                .as_deref()
                .is_none_or(|wanted| owner == Some(wanted))
    }
}

fn classify_callees(called: &CalledNames, out: &mut [Callee]) {
    let mut groups: HashMap<&str, Vec<&Callee>> = HashMap::new();
    for candidate in out.iter() {
        groups.entry(&candidate.name).or_default().push(candidate);
    }
    let mut labels = Vec::with_capacity(out.len());
    for candidate in out.iter() {
        let peers = &groups[candidate.name.as_str()];
        let calls = called.module_targets.get(&candidate.name);
        let ambiguous = calls.map_or(peers.len() > 1, |calls| {
            calls.iter().any(|call| {
                call.allows(&candidate.path, candidate.owner.as_deref())
                    && peers
                        .iter()
                        .filter(|peer| call.allows(&peer.path, peer.owner.as_deref()))
                        .count()
                        > 1
            })
        });
        let qualified = calls.is_some_and(|calls| {
            calls
                .iter()
                .filter(|call| call.allows(&candidate.path, candidate.owner.as_deref()))
                .all(|call| call.files.is_some() || call.owner.is_some())
        });
        labels.push(if ambiguous {
            "ambiguous"
        } else if qualified {
            "qualified"
        } else {
            "name-only"
        });
    }
    for (candidate, label) in out.iter_mut().zip(labels) {
        candidate.resolution = label;
    }
}

fn rust_call_owner(
    s: &Session,
    graph: &deps::Graph,
    path: &str,
    receiver: Option<&str>,
    files: Option<&HashSet<String>>,
    method: &str,
) -> Result<Option<String>> {
    let Some(receiver) = receiver else {
        return Ok(None);
    };
    if !receiver
        .bytes()
        .all(|b| b == b'_' || b.is_ascii_alphanumeric())
    {
        return Ok(None);
    }
    let owner = graph
        .source_name_for_binding(path, receiver)
        .unwrap_or(receiver);
    let mut stmt =
        s.db.conn
            .prepare("SELECT path FROM facts WHERE name = ?1 AND kind IN ('struct', 'enum')")?;
    let candidates = stmt.query_map([owner], |r| r.get::<_, String>(0))?;
    for candidate in candidates {
        let candidate = candidate?;
        if files.map_or(candidate == path, |files| files.contains(&candidate)) {
            // A known type alone does not locate an implementation. Trait
            // defaults and blanket impls can provide methods under other owners.
            let mut methods = s.db.conn.prepare(
                "SELECT path FROM facts WHERE name = ?1 AND owner = ?2 AND kind IN ('function', 'method')",
            )?;
            let paths =
                methods.query_map(rusqlite::params![method, owner], |r| r.get::<_, String>(0))?;
            for method_path in paths {
                let method_path = method_path?;
                if files.is_none_or(|files| files.contains(&method_path)) {
                    return Ok(Some(owner.to_string()));
                }
            }
        }
    }
    Ok(None)
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
        graph
            .source_name_for_binding(path, word)
            .unwrap_or(word)
            .to_string()
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
    let graph = s.dep_graph()?;
    let mut calls_stmt = s.db.conn.prepare(
        "SELECT start_byte, name, receiver FROM calls
         WHERE file_id = (SELECT id FROM files WHERE path = ?1)
           AND start_byte >= ?2 AND start_byte < ?3
         ORDER BY start_byte",
    )?;
    let mut span_stmt = s.db.conn.prepare(
        "SELECT start_byte, end_byte FROM spans
         WHERE file_id = (SELECT id FROM files WHERE path = ?1) ORDER BY start_byte",
    )?;
    let mut line_starts = HashMap::new();
    'targets: for (path, sb, eb) in targets {
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
            let source = OnceCell::new();
            for row in rows {
                let (off, word, recv) = row?;
                // Cached calls need no source except for Rust's `::` evidence
                // and example line numbers. Load only once the first call needs it.
                let body = if lang == "rust" || examples {
                    let Some(body) = source.get_or_init(|| s.source_body(path)) else {
                        continue 'targets;
                    };
                    Some(body.as_str())
                } else {
                    None
                };
                let (lookup, pinned) = resolve_syntax_call(graph, path, &word, recv.as_deref());
                // An empty target set is no evidence: it may be an external
                // module or a value receiver. `pinned` is `None` then, so
                // candidate filtering remains conservative.
                let owner = if lang == "rust"
                    && body.is_some_and(|b| b[..off.min(b.len())].trim_end().ends_with("::"))
                {
                    rust_call_owner(s, graph, path, recv.as_deref(), pinned.as_ref(), &lookup)?
                } else {
                    None
                };
                called
                    .module_targets
                    .entry(lookup.clone())
                    .or_default()
                    .push(CallConstraint {
                        files: pinned,
                        owner,
                    });
                if let Some(b) = body.filter(|_| examples) {
                    called.sites.entry(lookup.clone()).or_insert_with(|| {
                        let expr = match &recv {
                            Some(r) => format!("{r}.{word}"),
                            None => word.clone(),
                        };
                        let (starts, scanned) = line_starts
                            .entry(path.as_str())
                            .or_insert_with(|| (vec![0], 0));
                        let end = off.min(b.len());
                        if *scanned < end {
                            let base = *scanned;
                            starts.extend(
                                memchr::memchr_iter(b'\n', &b.as_bytes()[base..end])
                                    .map(|i| base + i + 1),
                            );
                            *scanned = end;
                        }
                        let line = starts.partition_point(|&start| start <= end) as i64;
                        (expr, line)
                    });
                }
                called.idents.insert(lookup);
            }
            continue;
        }
        let Some(b) = s.source_body(path) else {
            continue;
        };
        let Some(slice) = b.get(*sb..(*eb).min(b.len())) else {
            continue;
        };
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

fn callee_candidates(s: &Session, idents: &HashSet<String>) -> Result<Vec<Callee>> {
    let ident_list: Vec<&String> = idents.iter().collect();
    let mut out = Vec::new();
    for chunk in ident_list.chunks(500) {
        let placeholders = vec!["?"; chunk.len()].join(",");
        let sql = format!(
            "SELECT name, kind, path, start_line, owner, start_byte FROM facts
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
                r.get::<_, Option<String>>(4)?,
                r.get::<_, i64>(5)? as usize,
            ))
        })?;
        for row in rows {
            let (n, k, p, l, owner, start_byte) = row?;
            out.push(Callee {
                name: n,
                kind: k,
                path: p,
                line: l,
                start_byte,
                owner,
                resolution: "name-only",
                via: None,
                call: None,
                call_line: None,
            });
        }
    }
    Ok(out)
}

fn callee_allowed(candidate: &Callee, name: &str, called: &CalledNames) -> bool {
    candidate.name != name
        && called
            .module_targets
            .get(&candidate.name)
            .is_none_or(|calls| {
                calls
                    .iter()
                    .any(|call| call.allows(&candidate.path, candidate.owner.as_deref()))
            })
}

fn select_callee_definitions(
    candidates: impl IntoIterator<Item = Callee>,
    name: &str,
    called: &CalledNames,
) -> Vec<Callee> {
    let mut seen = HashSet::new();
    candidates
        .into_iter()
        .filter_map(|mut candidate| {
            if !callee_allowed(&candidate, name, called)
                || !seen.insert((
                    candidate.name.clone(),
                    candidate.path.clone(),
                    candidate.line,
                ))
            {
                return None;
            }
            let site = called.sites.get(&candidate.name);
            candidate.call = site.map(|(expression, _)| expression.clone());
            candidate.call_line = site.map(|(_, line)| *line);
            Some(candidate)
        })
        .collect()
}

#[derive(Default)]
struct CalleeResolver {
    candidates: HashMap<String, Vec<Callee>>,
}

impl CalleeResolver {
    fn resolve<'a>(
        &'a mut self,
        s: &Session,
        name: &str,
        path: Option<&str>,
    ) -> Result<Vec<&'a Callee>> {
        let targets = definition_ranges(s, name, path)?;
        if targets.is_empty() {
            return Ok(Vec::new());
        }
        let mut called = scan_called_names(s, &targets, false)?;
        called.idents.remove(name);
        let missing: HashSet<String> = called
            .idents
            .iter()
            .filter(|ident| !self.candidates.contains_key(*ident))
            .cloned()
            .collect();
        let loaded = callee_candidates(s, &missing)?;
        for ident in missing {
            self.candidates.insert(ident, Vec::new());
        }
        for candidate in loaded {
            self.candidates
                .get_mut(&candidate.name)
                .unwrap()
                .push(candidate);
        }
        let mut out: Vec<&Callee> = called
            .idents
            .iter()
            .flat_map(|ident| self.candidates[ident].iter())
            .filter(|candidate| callee_allowed(candidate, name, &called))
            .collect();
        out.sort_by(|a, b| {
            (a.name.as_str(), a.path.as_str(), a.line).cmp(&(
                b.name.as_str(),
                b.path.as_str(),
                b.line,
            ))
        });
        Ok(out)
    }
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
    let graph = s.dep_graph()?;
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
    let CallersArgs {
        name,
        path,
        depth,
        resolved,
        call_only,
        qualifier,
        examples,
        limit,
    } = args;
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
        for names in frontier.chunks(32) {
            for callers in callers_for(&s, names, path)? {
                for c in callers {
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
    let CalleesArgs {
        name,
        path,
        depth,
        resolved,
        examples,
        limit,
    } = args;
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
    let mut resolver = CalleeResolver::default();
    'walk: for d in 1..=depth {
        let mut next: Vec<String> = Vec::new();
        for sym in &frontier {
            for c in resolver.resolve(&s, sym, path)? {
                if !reported.insert((c.path.clone(), c.name.clone())) {
                    continue;
                }
                if expanded.insert(c.name.clone()) {
                    next.push(c.name.clone());
                }
                hops.push(CalleeHop {
                    name: c.name.clone(),
                    kind: c.kind.clone(),
                    path: c.path.clone(),
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
    let mut resolver = CalleeResolver::default();
    'walk: for _ in 1..=max_depth {
        let mut next: Vec<String> = Vec::new();
        for node in &frontier {
            // Scope only the start symbol's callees; hops after it follow anywhere.
            let scope = if node.as_str() == from { path } else { None };
            let mut callees = resolver.resolve(&s, node, scope)?;
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
                loc.insert(c.name.clone(), (c.kind.clone(), c.path.clone(), c.line));
                if c.name == to {
                    found = true;
                    break 'walk;
                }
                next.push(c.name.clone());
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

    // The walk is by name, so `from` may stand for several definitions
    // (`main` in two languages). Place the chain on the one that actually calls
    // its first hop, not on whichever sorts first by path.
    let start = match chain.get(1) {
        Some(hop) => {
            let mut found = None;
            for d in definitions_of(&s, from, path, 0)? {
                let calls = callees_of(&s, from, Some(&d.path), false, false)?;
                if calls.iter().any(|c| c.name == *hop) {
                    found = Some((d.kind, d.path, d.start_line));
                    break;
                }
            }
            match found {
                Some(site) => Some(site),
                None => loc_of(from)?,
            }
        }
        None => loc_of(from)?,
    };

    let mut steps: Vec<PathStep> = Vec::with_capacity(chain.len());
    for (depth, name) in chain.into_iter().enumerate() {
        let l = if name == from {
            start.clone()
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
struct Resolver<'a> {
    graph: &'a deps::Graph,
    defs: std::collections::HashSet<String>,
    cache: HashMap<String, &'static str>,
}

impl<'a> Resolver<'a> {
    fn new(s: &'a Session, name: &str) -> Result<Resolver<'a>> {
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
            graph: s.dep_graph()?,
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

fn resolve_impact_lines(s: &Session, impacted: &mut [ImpactNode]) -> Result<()> {
    let mut by_file: BTreeMap<&str, HashMap<&str, Vec<usize>>> = BTreeMap::new();
    for (i, node) in impacted.iter().enumerate() {
        by_file
            .entry(&node.path)
            .or_default()
            .entry(&node.name)
            .or_default()
            .push(i);
    }
    let paths: Vec<_> = by_file.keys().copied().collect();
    let mut lines = vec![None; impacted.len()];
    for chunk in paths.chunks(64) {
        let placeholders = vec!["?"; chunk.len()].join(",");
        let sql = format!(
            "SELECT path, name, MIN(start_line) FROM facts
             WHERE path IN ({placeholders}) AND kind <> 'import' GROUP BY path, name"
        );
        let mut stmt = s.db.conn.prepare(&sql)?;
        let mut rows = stmt.query(rusqlite::params_from_iter(chunk))?;
        while let Some(row) = rows.next()? {
            let path = row.get_ref(0)?.as_str()?;
            let Some(name) = row.get_ref(1)?.as_str_or_null()? else {
                continue;
            };
            if let Some(indices) = by_file[path].get(name) {
                let line: i64 = row.get(2)?;
                for &i in indices {
                    lines[i] = Some(line);
                }
            }
        }
    }
    for (node, line) in impacted.iter_mut().zip(lines) {
        node.line = line.or(node.call_line).unwrap_or(0);
        if node.call_line == Some(node.line) {
            node.call_line = None;
        }
    }
    Ok(())
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
        for names in frontier.chunks(32) {
            for callers in callers_for(&s, names, None)? {
                for c in callers {
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
        }
        frontier = next;
        frontier.sort();
        if frontier.is_empty() {
            break;
        }
    }
    resolve_impact_lines(&s, &mut impacted)?;
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
    let scoped: Vec<&str> = in_scope.iter().map(String::as_str).collect();
    for chunk in scoped.chunks(super::testing::NAMES_PER_SCAN) {
        for found in test_sites_for(&s, chunk, None, false)? {
            sites.extend(found);
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

#[cfg(test)]
mod resolver_tests {
    use super::*;
    use std::sync::atomic::{AtomicUsize, Ordering};

    static LOOKUPS: AtomicUsize = AtomicUsize::new(0);

    fn count_lookup(sql: &str) {
        if sql.contains("SELECT name, kind, path, start_line, owner, start_byte FROM facts") {
            LOOKUPS.fetch_add(1, Ordering::Relaxed);
        }
    }

    #[test]
    fn shared_and_missing_callees_are_queried_once_per_walk() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(
            dir.path().join("source.py"),
            "def leaf(): pass\ndef left(): leaf(); external()\ndef right(): leaf(); external()\n",
        )
        .unwrap();
        let mut session = Session::open(dir.path(), false, Out::default()).unwrap();
        session.db.conn.trace(Some(count_lookup));
        LOOKUPS.store(0, Ordering::Relaxed);
        let mut resolver = CalleeResolver::default();
        let left = resolver.resolve(&session, "left", None).unwrap();
        assert_eq!(left.len(), 1);
        assert_eq!(left[0].name, "leaf");
        assert_eq!(LOOKUPS.load(Ordering::Relaxed), 1);
        let right = resolver.resolve(&session, "right", None).unwrap();
        assert_eq!(right.len(), 1);
        assert_eq!(right[0].name, "leaf");
        assert_eq!(LOOKUPS.load(Ordering::Relaxed), 1);
    }
}

#[cfg(test)]
mod impact_line_tests {
    use super::*;
    use std::sync::atomic::{AtomicUsize, Ordering};

    static LOOKUPS: AtomicUsize = AtomicUsize::new(0);

    fn count_lookup(sql: &str) {
        if sql.contains("SELECT path, name, MIN(start_line)") {
            LOOKUPS.fetch_add(1, Ordering::Relaxed);
        }
    }

    #[test]
    fn resolves_each_file_once_and_preserves_missing_and_duplicate_names() {
        let dir = tempfile::tempdir().unwrap();
        let mut nodes = Vec::new();
        for i in 0..130 {
            let path = format!("source{i:03}.py");
            std::fs::write(
                dir.path().join(&path),
                "from external import run\ndef run(): pass\ndef run(): pass\n",
            )
            .unwrap();
            nodes.push(ImpactNode {
                name: "run".into(),
                kind: None,
                path,
                line: 0,
                call_line: Some(if i % 2 == 0 { 2 } else { 3 }),
                depth: 1,
            });
        }
        nodes.push(ImpactNode {
            name: "missing".into(),
            kind: None,
            path: "source000.py".into(),
            line: 0,
            call_line: Some(9),
            depth: 1,
        });
        nodes.push(ImpactNode {
            name: "missing".into(),
            kind: None,
            path: "source000.py".into(),
            line: 0,
            call_line: None,
            depth: 1,
        });
        let mut session = Session::open(dir.path(), false, Out::default()).unwrap();
        session.db.conn.trace(Some(count_lookup));
        LOOKUPS.store(0, Ordering::Relaxed);
        resolve_impact_lines(&session, &mut nodes).unwrap();
        assert_eq!(LOOKUPS.load(Ordering::Relaxed), 3);
        for (i, node) in nodes[..130].iter().enumerate() {
            assert_eq!(node.line, 2);
            assert_eq!(node.call_line, (i % 2 == 1).then_some(3));
        }
        assert_eq!((nodes[130].line, nodes[130].call_line), (9, None));
        assert_eq!((nodes[131].line, nodes[131].call_line), (0, None));
        resolve_impact_lines(&session, &mut []).unwrap();
        assert_eq!(LOOKUPS.load(Ordering::Relaxed), 3);
    }
}
