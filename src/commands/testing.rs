//! Test mapping: `tests`, `untested`, and the test-scaffolding predicates.

use super::*;

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
pub(super) struct TestScopes {
    by_path: HashMap<String, Vec<(i64, i64)>>,
    callbacks: HashMap<String, Vec<CallbackTest>>,
}

#[derive(Clone)]
struct CallbackTest {
    name: String,
    line: i64,
    start_line: i64,
    end_line: i64,
}

impl TestScopes {
    pub(super) fn load(conn: &rusqlite::Connection, root: &Path) -> Result<TestScopes> {
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
        let mut callbacks = HashMap::new();
        let mut stmt = conn.prepare(
            "SELECT path, language FROM files WHERE language IN ('javascript', 'typescript', 'tsx')",
        )?;
        let files = stmt
            .query_map([], |r| Ok((r.get::<_, String>(0)?, r.get::<_, String>(1)?)))?
            .collect::<std::result::Result<Vec<_>, _>>()?;
        for (path, language) in files {
            if !is_test_path(&path) {
                continue;
            }
            if let Some(source) = crate::index::load_body(root, &path) {
                let found = callback_tests(&language, &source);
                if !found.is_empty() {
                    by_path
                        .entry(path.clone())
                        .or_default()
                        .extend(found.iter().map(|t| (t.start_line, t.end_line)));
                    callbacks.insert(path, found);
                }
            }
        }
        Ok(TestScopes { by_path, callbacks })
    }

    /// Whether `line` of `path` falls inside a test-like container.
    fn contains(&self, path: &str, line: i64) -> bool {
        self.by_path.get(path).is_some_and(|spans| {
            spans
                .iter()
                .any(|&(start, end)| line >= start && line <= end)
        })
    }

    fn callback_at(&self, path: &str, line: i64) -> Option<&CallbackTest> {
        self.callbacks
            .get(path)?
            .iter()
            .find(|test| line >= test.start_line && line <= test.end_line)
    }
}

fn callback_tests(language: &str, source: &str) -> Vec<CallbackTest> {
    let Some(language) = crate::astgrep::ts_language(language) else {
        return Vec::new();
    };
    let mut parser = tree_sitter::Parser::new();
    if parser.set_language(&language).is_err() {
        return Vec::new();
    }
    let Some(tree) = parser.parse(source, None) else {
        return Vec::new();
    };
    let src = source.as_bytes();
    let mut suites = Vec::new();
    let mut tests = Vec::new();
    let mut stack = vec![tree.root_node()];
    while let Some(node) = stack.pop() {
        if node.kind() == "call_expression" {
            if let Some((kind, title, callback)) = test_callback(node, src) {
                let entry = CallbackTest {
                    name: title,
                    line: node.start_position().row as i64 + 1,
                    start_line: callback.start_position().row as i64 + 1,
                    end_line: callback.end_position().row as i64 + 1,
                };
                if kind == "suite" {
                    suites.push((callback.start_byte(), callback.end_byte(), entry.name));
                } else {
                    tests.push((callback.start_byte(), callback.end_byte(), entry));
                }
            }
        }
        let mut cursor = node.walk();
        stack.extend(node.named_children(&mut cursor));
    }
    tests
        .into_iter()
        .map(|(start, end, mut test)| {
            let mut parents: Vec<_> = suites
                .iter()
                .filter(|(suite_start, suite_end, _)| *suite_start <= start && end <= *suite_end)
                .collect();
            parents.sort_by_key(|(suite_start, _, _)| *suite_start);
            if !parents.is_empty() {
                test.name = parents
                    .iter()
                    .map(|(_, _, name)| name.as_str())
                    .chain(std::iter::once(test.name.as_str()))
                    .collect::<Vec<_>>()
                    .join(" > ");
            }
            test
        })
        .collect()
}

fn test_callback<'tree>(
    call: tree_sitter::Node<'tree>,
    src: &[u8],
) -> Option<(&'static str, String, tree_sitter::Node<'tree>)> {
    let function = call.child_by_field_name("function")?;
    let expression = function.utf8_text(src).ok()?;
    let base = expression.split(['.', '(']).next()?;
    let kind = match base {
        "describe" | "suite" => "suite",
        "it" | "test" | "specify" => "test",
        _ => return None,
    };
    let args = call.child_by_field_name("arguments")?;
    let mut cursor = args.walk();
    let children: Vec<_> = args.named_children(&mut cursor).collect();
    let title = children.iter().find_map(|node| {
        matches!(node.kind(), "string" | "template_string")
            .then(|| node.utf8_text(src).ok())
            .flatten()
            .map(|text| {
                text.get(1..text.len().saturating_sub(1))
                    .unwrap_or(text)
                    .to_string()
            })
    })?;
    let callback = children
        .into_iter()
        .find(|node| matches!(node.kind(), "arrow_function" | "function_expression"))?;
    Some((kind, title, callback))
}

/// Whether a definition site is a test — its file, its own name, or the container
/// it sits in says so. The three signals are always weighed together, so `tests`,
/// `callers` and `unused` agree on what a test is instead of each carrying its own
/// (differently wrong) notion.
pub(super) fn is_test_site(
    path: &str,
    symbol: Option<&str>,
    line: i64,
    scopes: &TestScopes,
) -> bool {
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
pub(super) fn test_sites(
    s: &Session,
    name: &str,
    path: Option<&str>,
) -> Result<std::collections::BTreeSet<(String, String)>> {
    Ok(test_sites_for(s, &[name], path, false)?.remove(0))
}

/// [`test_sites`] for several names in one pass over the files, so a file that
/// mentions many of them is read once rather than once per name. One set per
/// name, in the order given.
///
/// With `first_only` a name stops being searched at its first test site: the
/// set is then "empty or not", which is all a coverage check needs, and the
/// rest of the repository is never read for a name that is already covered.
pub(super) fn test_sites_for(
    s: &Session,
    names: &[&str],
    path: Option<&str>,
    first_only: bool,
) -> Result<Vec<std::collections::BTreeSet<(String, String)>>> {
    let scopes = s.test_scopes()?;
    let mut sites = vec![std::collections::BTreeSet::new(); names.len()];
    scan_names(
        &s.db.conn,
        &s.repo.root,
        names,
        path,
        InFilter::Any,
        &mut |i, hits| {
            for h in &hits {
                if let Some(callback) = scopes.callback_at(&h.path, h.line as i64) {
                    if callback.name != names[i] {
                        sites[i].insert((h.path.clone(), callback.name.clone()));
                    }
                }
                let sym = match &h.symbol {
                    Some(sy) if sy != names[i] => sy,
                    _ => continue,
                };
                let is_fn = matches!(h.kind.as_deref(), Some("function") | Some("method"));
                if is_fn && is_test_site(&h.path, Some(sym), h.line as i64, &scopes) {
                    sites[i].insert((h.path.clone(), sym.clone()));
                }
            }
            first_only && !sites[i].is_empty()
        },
    )?;
    Ok(sites)
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
    for (path, name) in tests {
        if let Some(callback) = s
            .test_scopes()?
            .callbacks
            .get(path)
            .and_then(|callbacks| callbacks.iter().find(|test| test.name == *name))
        {
            rows.push(TestRow {
                path: path.clone(),
                line: callback.line,
                name: callback.name.clone(),
                kind: "test".into(),
            });
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

/// Names whose coverage `untested` checks in one pass over the files. Each name
/// adds its trigram clauses to the candidate query, so this bounds that query as
/// much as the work a pass does.
pub(super) const NAMES_PER_SCAN: usize = 128;

#[derive(Serialize, Clone)]
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

    let scopes = s.test_scopes()?;
    // Test scaffolding is not production code that needs covering. Same rule
    // `unused` applies, via the same predicate: this used to be a path-only
    // check, so a `pub fn` inside a `#[cfg(test)] mod tests` in a production
    // file was reported as untested public API — which `unused`, asking the
    // identical question one screen away, already knew better than to do.
    let candidates: Vec<UntestedRow> = candidates
        .into_iter()
        .filter(|c| !is_test_site(&c.path, Some(&c.name), c.line, &scopes))
        .collect();

    // One lookup per distinct name, not per definition, and a batch of names
    // per pass over the files rather than a pass each. The batch is the next
    // names in report order. Under a `--limit` it starts at the number of rows
    // wanted and doubles, so a small limit does not look far past the rows it
    // can stop at.
    let mut batch = if limit > 0 {
        limit.clamp(8, NAMES_PER_SCAN)
    } else {
        NAMES_PER_SCAN
    };
    let mut stopped_at_limit = false;
    let mut covered: HashMap<String, bool> = HashMap::new();
    let mut data: Vec<UntestedRow> = Vec::new();
    let mut checked = 0usize;
    for (i, cand) in candidates.iter().enumerate() {
        checked += 1;
        let has_test = match covered.get(&cand.name) {
            Some(&c) => c,
            None => {
                let mut ahead: Vec<&str> = Vec::with_capacity(batch);
                for c in &candidates[i..] {
                    let n = c.name.as_str();
                    if !covered.contains_key(n) && !ahead.contains(&n) {
                        ahead.push(n);
                        if ahead.len() == batch {
                            break;
                        }
                    }
                }
                let found = test_sites_for(&s, &ahead, None, true)?;
                for (n, sites) in ahead.iter().zip(found) {
                    covered.insert((*n).to_string(), !sites.is_empty());
                }
                batch = (batch * 2).min(NAMES_PER_SCAN);
                covered[&cand.name]
            }
        };
        if !has_test {
            data.push(cand.clone());
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
        if checked == 0 {
            Some(match path {
                Some(p) => format!("no eligible public definitions to check in '{p}'"),
                None => "no eligible public definitions to check".to_string(),
            })
        } else {
            match path {
                Some(p) => Some(format!(
                    "every eligible public definition in '{p}' is referenced by a test"
                )),
                None => {
                    Some("every eligible public definition is referenced by a test".to_string())
                }
            }
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
