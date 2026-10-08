//! Type structure: `members`, `graph`, `implementors`, `supertypes`.

use super::*;

#[derive(Serialize)]
pub(super) struct MemberRow {
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

pub(super) fn members_of(
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
