//! Single-symbol neighbourhoods: `about`, `explore`, `at`, `context`.

use super::*;

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
