//! Definition listing: `symbols`, `definitions`, `api`.

use super::*;

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

/// Arguments of `symbols`.
pub struct SymbolsArgs<'a> {
    pub pattern: Option<&'a str>,
    pub kind: Option<&'a str>,
    pub owner: Option<&'a str>,
    pub language: Option<&'a str>,
    pub path: Option<&'a str>,
    pub signatures: bool,
    pub limit: usize,
}

pub fn cmd_symbols(cwd: &Path, args: SymbolsArgs, out: Out) -> Result<()> {
    let SymbolsArgs {
        pattern,
        kind,
        owner,
        language,
        path,
        signatures,
        limit,
    } = args;
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
        sql.push_str(" AND path LIKE ? ESCAPE '\\'");
        args.push(format!("%{}%", escape_like_literal(p)));
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
pub(super) struct DefinitionRow {
    pub(super) name: String,
    pub(super) kind: String,
    pub(super) path: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub(super) language: Option<String>,
    // A row's *primary position* is `line`/`col`/`byte`; its extent is
    // `end_line`/`end_col`/`end_byte`. The Rust fields keep the `start_` prefix
    // that mirrors the facts table, but the emitted key must not — see
    // ROW_KEY_RULE. `start_line` here and `line` on the next command over is how
    // `--fields line` came to work on some commands and not others.
    #[serde(rename = "line")]
    pub(super) start_line: i64,
    #[serde(rename = "col")]
    pub(super) start_col: i64,
    pub(super) end_line: i64,
    pub(super) end_col: i64,
    #[serde(rename = "byte")]
    pub(super) start_byte: i64,
    pub(super) end_byte: i64,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub(super) signature: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub(super) doc: Option<String>,
    /// For a `field`/`variant`, the type it belongs to. Absent otherwise — a
    /// member name alone (`id`, `name`) does not identify anything.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub(super) owner: Option<String>,
}

pub(super) fn definitions_of(
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
        let scope_filter = Filters::new().like("path", path);
        let sql = format!(
            "SELECT COALESCE(visibility, 'unknown'), COUNT(*) FROM facts
             WHERE kind <> 'import'{} GROUP BY 1 ORDER BY 1",
            scope_filter.sql(),
        );
        let mut counts = s.db.conn.prepare(&sql)?;
        let rows: Vec<(String, i64)> = counts
            .query_map(params_with(&[], &scope_filter).as_slice(), |r| {
                Ok((r.get(0)?, r.get(1)?))
            })?
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
    let mut ex = crate::facts::Extractor::default();
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
        // Each version also keeps the line it was declared on at `compare_to`: a
        // removed export has no line now, and the old one is the only true place.
        let mut old: HashMap<String, Vec<(String, String, i64)>> = HashMap::new();
        if let Some(bytes) = s.repo.file_at(compare_to, &file) {
            for fact in ex
                .extract(lang, &file, &bytes)?
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
                    fact.start_line as i64,
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
                    .position(|(kind, signature, _)| {
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
                    .position(|(old_kind, _, _)| old_kind == &now.kind)
                {
                    let (_, before, _) = old_versions.remove(position);
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
            for (mut now, (_, before, _)) in unmatched_current
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
            for (kind, before, was_line) in old_versions {
                data.push(ApiChangeRow {
                    change: "removed".into(),
                    name: name.clone(),
                    kind,
                    path: file.clone(),
                    line: was_line,
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
pub(super) fn declaration_header(kind: &str, sig: Option<String>) -> Option<String> {
    let sig = sig?;
    if !CONTAINER_KINDS.contains(&kind) {
        return Some(sig);
    }
    Some(match sig.split_once('{') {
        Some((head, _)) => head.trim_end().to_string(),
        None => sig,
    })
}
