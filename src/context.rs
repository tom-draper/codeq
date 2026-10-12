//! Token-budgeted context assembly.
//!
//! This is the command that has to justify the whole project: return the
//! *smallest useful* slice of code for a task, not just search results. The
//! design follows the constraints argued in DESIGN.md:
//!
//! - **Seeds are explicit.** codeq does not guess task intent. The agent names
//!   symbols, or asks for changed code; we assemble from that.
//! - **Ranking is deterministic and explainable.** Seeds first, then syntax
//!   calls, then lexical references from the seed chunks, each with a stable
//!   path+line tiebreak. No hidden semantic scoring.
//! - **Omissions are visible.** When the budget truncates, dropped candidates
//!   are reported so the agent can request the next slice instead of silently
//!   missing code.
//! - **Token counts are approximate and labeled.** We estimate ~4 bytes/token;
//!   an exact per-model tokenizer is future work, not a silent guess.

use std::collections::{HashMap, HashSet};
use std::path::Path;

use anyhow::{anyhow, Result};
use serde::Serialize;

use crate::db::Db;
use crate::repo::Repo;

const TOKENIZER: &str = "approx:4-bytes-per-token";
/// Cap on expansion neighbours considered, to keep output bounded.
const MAX_NEIGHBORS: usize = 100;
/// Cap on how many omitted candidates we report.
const MAX_OMITTED: usize = 50;

pub struct Request {
    pub symbols: Vec<String>,
    pub changed: bool,
    pub compare_to: String,
    pub path: Option<String>,
    pub max_tokens: u64,
    pub expand: bool,
    /// Narrow `--expand` to definitions the seed files can actually reach through
    /// imports. Same heuristic as `--resolved` on callers/callees/references.
    pub resolved: bool,
    pub include_text: bool,
    /// Include the explanatory `plan` (seeds, modes, ordering). Off by default
    /// to keep the payload lean; the chunks and omissions are what an agent acts on.
    pub plan: bool,
}

/// A definition loaded from the index.
struct Def {
    name: String,
    kind: String,
    path: String,
    language: Option<String>,
    start_byte: usize,
    end_byte: usize,
    start_line: i64,
    end_line: i64,
}

#[derive(Serialize)]
pub struct ContextResult {
    /// Present only with `--plan`.
    #[serde(skip_serializing_if = "Option::is_none")]
    plan: Option<Plan>,
    budget: Budget,
    /// Readable so `cmd_context` can tell an empty bundle from a full one and
    /// explain which — an empty `context` reads as "no source needed" otherwise.
    pub(crate) chunks: Vec<Chunk>,
    /// Candidates dropped by the token budget — a trust signal, so omitted only
    /// when there actually were omissions.
    #[serde(skip_serializing_if = "Vec::is_empty")]
    omitted: Vec<Omitted>,
    #[serde(skip_serializing_if = "std::ops::Not::not")]
    pub(crate) omitted_truncated: bool,
}

impl ContextResult {
    /// Destructure into (budget, chunks, omitted). `explore` reuses context's
    /// budgeted source assembly and re-presents these under its own graph sections.
    pub(crate) fn into_source(self) -> (Budget, Vec<Chunk>, Vec<Omitted>) {
        (self.budget, self.chunks, self.omitted)
    }
}

#[derive(Serialize)]
struct Plan {
    /// Which seed modes were active.
    modes: Vec<&'static str>,
    /// Distinct seed symbol names resolved.
    seeds: Vec<String>,
    expansion: &'static str,
    order: &'static str,
}

#[derive(Serialize)]
pub(crate) struct Budget {
    max_tokens: u64,
    used_tokens: u64,
    /// The estimate is approximate (byte-based); labeled so an agent never
    /// treats it as an exact per-model count.
    tokenizer: &'static str,
}

#[derive(Serialize)]
pub(crate) struct Chunk {
    path: String,
    // The chunk *is* one definition's source, so its identity is `name`
    // (ROW_KEY_RULE); `symbol` stays reserved for an enclosing definition.
    #[serde(rename = "name")]
    symbol: String,
    kind: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    language: Option<String>,
    #[serde(rename = "line")]
    start_line: i64,
    end_line: i64,
    #[serde(rename = "byte")]
    start_byte: usize,
    end_byte: usize,
    est_tokens: u64,
    /// Why this chunk is here: "seed" or "referenced".
    reason: &'static str,
    #[serde(skip_serializing_if = "Option::is_none")]
    text: Option<String>,
    /// How `text` was reduced to fit the budget, when it was: `"folded"` (inner
    /// bodies collapsed) or `"clipped"` (tail elided). Absent when the chunk is
    /// the definition's verbatim source, so its presence is the signal that this
    /// is *not* the whole thing — recover the rest with `read --lines`.
    #[serde(skip_serializing_if = "Option::is_none")]
    reduced: Option<&'static str>,
}

#[derive(Serialize)]
pub(crate) struct Omitted {
    path: String,
    // A dropped definition is still a definition (ROW_KEY_RULE) -- and it sits
    // beside `chunks` in the same payload, so the two must agree.
    #[serde(rename = "name")]
    symbol: String,
    est_tokens: u64,
    reason: &'static str,
}

pub fn build(db: &Db, repo: &Repo, req: &Request) -> Result<ContextResult> {
    if req.symbols.is_empty() && !req.changed {
        return Err(anyhow!(
            "context needs explicit seeds: pass --symbols <names> and/or --changed"
        ));
    }

    // `defs` holds the seeds first (indices 0..seed_count), then any expansion
    // neighbours appended.
    let (modes, mut defs) = load_seeds(db, repo, req)?;
    let seed_count = defs.len();

    let mut file_cache: HashMap<String, Option<String>> = HashMap::new();

    // Read seed chunk texts (needed for expansion regardless of include_text).
    let mut seed_text: HashMap<usize, String> = HashMap::new();
    for (i, def) in defs.iter().take(seed_count).enumerate() {
        if let Some(t) = chunk_text(&repo.root, def, &mut file_cache) {
            seed_text.insert(i, t);
        }
    }

    if req.expand {
        let neighbors = expansion_neighbors(db, req, &defs[..seed_count], &seed_text)?;
        defs.extend(neighbors);
    }

    let filled = fill_budget(db, repo, req, &defs, seed_count, seed_text, &mut file_cache);

    let seeds: Vec<String> = {
        let mut s: Vec<String> = defs[..seed_count].iter().map(|d| d.name.clone()).collect();
        s.sort();
        s.dedup();
        s
    };

    let plan = req.plan.then_some(Plan {
        modes,
        seeds,
        expansion: if req.expand {
            "one-hop: syntax calls first, then lexical references in seed chunks"
        } else {
            "disabled"
        },
        order: "seeds first, then referenced defs; each group by path+line; a chunk over budget is skipped, not reordered",
    });

    Ok(ContextResult {
        plan,
        budget: Budget {
            max_tokens: req.max_tokens,
            used_tokens: filled.used,
            tokenizer: TOKENIZER,
        },
        chunks: filled.chunks,
        omitted: filled.omitted,
        omitted_truncated: filled.omitted_truncated,
    })
}

/// The seed definitions the request names, in deterministic path+line order,
/// and which seed modes were active.
fn load_seeds(db: &Db, repo: &Repo, req: &Request) -> Result<(Vec<&'static str>, Vec<Def>)> {
    // Load only the definitions we actually need, via the facts(name)/facts(path)
    // indexes — never a full scan of every definition.
    let mut modes: Vec<&'static str> = Vec::new();
    let mut defs: Vec<Def> = Vec::new();
    let mut seen: HashSet<(String, usize)> = HashSet::new(); // (path, start_byte)

    if !req.symbols.is_empty() {
        modes.push("symbols");
        for d in defs_by_column(db, "name", &req.symbols)? {
            if seen.insert((d.path.clone(), d.start_byte)) {
                defs.push(d);
            }
        }
    }
    if req.changed {
        modes.push("changed");
        // Seed only the definitions the change actually *touched* (hunk overlap),
        // not every definition in a changed file — the same granularity as
        // `changed --symbols`. Otherwise a one-line edit to a big file would
        // seed the whole file.
        let ranges = repo.changed_line_ranges(&req.compare_to)?;
        let paths: Vec<String> = ranges.keys().cloned().collect();
        for d in defs_by_column(db, "path", &paths)? {
            let touched = ranges.get(&d.path).is_some_and(|hunks| {
                let ds = d.start_line.max(0) as usize;
                let de = d.end_line.max(0) as usize;
                hunks.iter().any(|&(hs, he)| he >= ds && hs <= de)
            });
            if touched && seen.insert((d.path.clone(), d.start_byte)) {
                defs.push(d);
            }
        }
    }

    // Optional path filter narrows the seed set.
    if let Some(prefix) = &req.path {
        defs.retain(|d| d.path.contains(prefix.as_str()));
    }

    defs.sort_by(|a, b| (a.path.as_str(), a.start_line).cmp(&(b.path.as_str(), b.start_line)));
    Ok((modes, defs))
}

/// One-hop expansion from the seeds: syntax-derived calls first, then the
/// existing lexical identifiers as a conservative supplement for types,
/// constants and opaque grammars. Both resolve through the facts(name) index,
/// never a full scan.
fn expansion_neighbors(
    db: &Db,
    req: &Request,
    seeds: &[Def],
    seed_text: &HashMap<usize, String>,
) -> Result<Vec<Def>> {
    let mut calls = called_names(db, seeds)?;
    let mut syntax_names = calls.names();
    let mut lexical: HashSet<String> = HashSet::new();
    for t in seed_text.values() {
        collect_identifiers(t, &mut lexical);
    }
    let seed_names: HashSet<&str> = seeds.iter().map(|d| d.name.as_str()).collect();
    calls.unscoped.retain(|r| !seed_names.contains(r.as_str()));
    calls.scoped.retain(|r, _| !seed_names.contains(r.as_str()));
    syntax_names.retain(|r| !seed_names.contains(r.as_str()));
    lexical.retain(|r| !seed_names.contains(r.as_str()) && !syntax_names.contains(r));
    let seed_keys: HashSet<(String, usize)> = seeds
        .iter()
        .map(|d| (d.path.clone(), d.start_byte))
        .collect();

    let mut neighbors: Vec<Def> = Vec::new();
    let mut added: HashSet<(String, usize)> = HashSet::new();
    // Expansion is by name, so a seed mentioning `new` or `open` pulls in
    // every same-named definition in the repo and spends the budget on code
    // the seed cannot even call. `--resolved` keeps only definitions living
    // in a file the seeds reach through imports — the same import-graph
    // heuristic `--resolved` already applies to callers/callees, reused
    // rather than reinvented. Still a heuristic: it narrows a superset.
    let reach: Option<HashSet<String>> = if req.resolved {
        let seed_files: HashSet<String> = seeds.iter().map(|d| d.path.clone()).collect();
        let graph = crate::commands::load_dep_graph(db)?;
        let mut r = seed_files.clone();
        for f in &seed_files {
            for reached in graph.reachable_from(f) {
                r.insert(reached.to_string());
            }
        }
        Some(r)
    } else {
        None
    };
    let mut add_group = |names: HashSet<String>,
                         only_in: Option<&HashMap<String, HashSet<String>>>|
     -> Result<()> {
        let mut names: Vec<String> = names.into_iter().collect();
        names.sort();
        let mut group = defs_by_column(db, "name", &names)?;
        group.sort_by(|a, b| {
            (a.name.as_str(), a.path.as_str(), a.start_line).cmp(&(
                b.name.as_str(),
                b.path.as_str(),
                b.start_line,
            ))
        });
        for d in group {
            if let Some(paths) = only_in.and_then(|targets| targets.get(&d.name)) {
                if !paths.contains(&d.path) {
                    continue;
                }
            }
            if let Some(r) = &reach {
                if !r.contains(&d.path) {
                    continue;
                }
            }
            let key = (d.path.clone(), d.start_byte);
            if !seed_keys.contains(&key) && added.insert(key) {
                neighbors.push(d);
            }
        }
        Ok(())
    };
    // A module-qualified syntax call is more specific than a bare name:
    // `context::build()` can be tied to the direct `context` import, while
    // an unqualified `build()` deliberately remains a conservative superset.
    let scoped_names = calls.scoped.keys().cloned().collect();
    add_group(scoped_names, Some(&calls.scoped))?;
    add_group(calls.unscoped, None)?;
    add_group(lexical, None)?;
    neighbors.truncate(MAX_NEIGHBORS);
    Ok(neighbors)
}

/// What the token budget kept and dropped.
struct Filled {
    chunks: Vec<Chunk>,
    omitted: Vec<Omitted>,
    omitted_truncated: bool,
    used: u64,
}

/// Budget fill over the candidates in rank order (seeds, then referenced
/// neighbours): include while it fits; a chunk that overflows is recorded as
/// omitted, and we keep trying smaller later chunks.
fn fill_budget(
    db: &Db,
    repo: &Repo,
    req: &Request,
    defs: &[Def],
    seed_count: usize,
    mut seed_text: HashMap<usize, String>,
    file_cache: &mut HashMap<String, Option<String>>,
) -> Filled {
    let candidates: Vec<(usize, &'static str)> = (0..seed_count)
        .map(|i| (i, "seed"))
        .chain((seed_count..defs.len()).map(|i| (i, "referenced")))
        .collect();

    let mut chunks = Vec::new();
    let mut omitted = Vec::new();
    let mut omitted_truncated = false;
    let mut used: u64 = 0;
    for (i, reason) in candidates {
        let d = &defs[i];
        let mut text = seed_text
            .remove(&i)
            .or_else(|| chunk_text(&repo.root, d, file_cache));
        let bytes = d.end_byte.saturating_sub(d.start_byte) as u64;
        let mut est = bytes.div_ceil(4).max(1);

        // Explicitly-requested seeds are always included, even if they exceed the
        // budget — you asked for them, and returning nothing (or dropping the very
        // symbol you named) is worse than overshooting. The budget caps expansion.
        // An oversized seed is reduced rather than dropped (see `reduce_to_fit`),
        // so "always included" doesn't turn `--max-tokens` into a suggestion.
        let reduced = if reason == "seed" {
            reduce_to_fit(db, d, &mut text, &mut est, used, req.max_tokens)
        } else {
            None
        };
        if reason != "seed" && used + est > req.max_tokens {
            if omitted.len() < MAX_OMITTED {
                omitted.push(Omitted {
                    path: d.path.clone(),
                    symbol: d.name.clone(),
                    est_tokens: est,
                    reason,
                });
            } else {
                omitted_truncated = true;
            }
            continue;
        }
        used += est;
        chunks.push(Chunk {
            path: d.path.clone(),
            symbol: d.name.clone(),
            kind: d.kind.clone(),
            language: d.language.clone(),
            start_line: d.start_line,
            end_line: d.end_line,
            start_byte: d.start_byte,
            end_byte: d.end_byte,
            est_tokens: est,
            reason,
            text: if req.include_text { text } else { None },
            reduced,
        });
    }
    Filled {
        chunks,
        omitted,
        omitted_truncated,
        used,
    }
}

/// A definition to include in an `explore` bundle, already located by (name,
/// path) so a common name is never re-expanded to every same-named definition in
/// the repo. `role` is surfaced on the chunk ("focus" / "caller" / "callee");
/// `forced` chunks (the focus symbol) are always included, even over budget.
pub struct Seed {
    pub name: String,
    pub path: String,
    pub role: &'static str,
    pub forced: bool,
    pub start_byte: Option<usize>,
}

/// Budgeted, file-grouped verbatim source for an explicit set of located
/// definitions. Mirrors `build`'s budget fill but keyed on precise (name, path)
/// targets, so `explore` never balloons on a common callee name; each chunk keeps
/// its seed's role. Forced seeds are always included; the rest fill the remaining
/// budget in a stable order and overflow is reported in `omitted`.
pub fn source_for(db: &Db, repo: &Repo, seeds: &[Seed], max_tokens: u64) -> Result<ContextResult> {
    source_for_impl(db, repo, seeds, max_tokens, false)
}

/// Fill and emit in seed rank order, keeping any forced definitions first.
/// Search uses this so its ranking survives both budgeting and rendering.
pub fn source_for_ranked(
    db: &Db,
    repo: &Repo,
    seeds: &[Seed],
    max_tokens: u64,
) -> Result<ContextResult> {
    source_for_impl(db, repo, seeds, max_tokens, true)
}

fn source_for_impl(
    db: &Db,
    repo: &Repo,
    seeds: &[Seed],
    max_tokens: u64,
    ranked: bool,
) -> Result<ContextResult> {
    // Resolve each (name, path) to its definition(s); the first seed to claim a
    // definition sets its role (pass focus seeds first).
    let mut cands: Vec<(Def, &'static str, bool)> = Vec::new();
    let mut seen: HashSet<(String, usize)> = HashSet::new();
    for seed in seeds {
        let defs = match seed.start_byte {
            Some(byte) => defs_enclosing_byte(db, &seed.path, byte)?,
            None => defs_by_name_in_path(db, &seed.name, &seed.path)?,
        };
        for d in defs {
            if seen.insert((d.path.clone(), d.start_byte)) {
                cands.push((d, seed.role, seed.forced));
            }
        }
    }

    // Stable sorting preserves seed rank within each forced/non-forced group.
    // Exploration instead budgets and groups neighbours by path and line.
    let mut order: Vec<usize> = (0..cands.len()).collect();
    order.sort_by(|&a, &b| {
        let (da, _, fa) = &cands[a];
        let (db_, _, fb) = &cands[b];
        if ranked {
            (!fa).cmp(&(!fb))
        } else {
            (!fa, da.path.as_str(), da.start_line).cmp(&(!fb, db_.path.as_str(), db_.start_line))
        }
    });

    let mut file_cache: HashMap<String, Option<String>> = HashMap::new();
    let mut chunks = Vec::new();
    let mut omitted = Vec::new();
    let mut omitted_truncated = false;
    let mut used: u64 = 0;
    // Byte ranges already emitted *verbatim*, per file. A later definition whose
    // range sits inside one of these is already on the page — a class's methods,
    // a component's functions — so re-emitting it would spend budget on a literal
    // duplicate. Folded chunks are deliberately not recorded here: folding hides
    // the inner bodies, so those members still need emitting in full.
    let mut verbatim: Vec<(&str, usize, usize)> = Vec::new();
    for i in order {
        let (d, role, forced) = &cands[i];
        if verbatim
            .iter()
            .any(|(p, s, e)| *p == d.path && *s <= d.start_byte && d.end_byte <= *e)
        {
            continue; // its source is already inside an emitted chunk
        }
        let bytes = d.end_byte.saturating_sub(d.start_byte) as u64;
        let mut est = bytes.div_ceil(4).max(1);
        if !forced && used + est > max_tokens {
            if omitted.len() < MAX_OMITTED {
                omitted.push(Omitted {
                    path: d.path.clone(),
                    symbol: d.name.clone(),
                    est_tokens: est,
                    reason: role,
                });
            } else {
                omitted_truncated = true;
            }
            continue;
        }
        let mut text = chunk_text(&repo.root, d, &mut file_cache);
        // A forced (focus) chunk is never dropped, so a definition bigger than the
        // whole budget used to blow straight through it — and a container
        // definition (a class, a module, a whole-file single-file component) is
        // routinely that big. Fold its inner bodies instead: the shape, the
        // signatures and the budget all survive, and `read --symbol` recovers any
        // body in full.
        let reduced = if *forced {
            reduce_to_fit(db, d, &mut text, &mut est, used, max_tokens)
        } else {
            None
        };
        if reduced.is_none() {
            verbatim.push((d.path.as_str(), d.start_byte, d.end_byte));
        }
        used += est;
        chunks.push(Chunk {
            reduced,
            path: d.path.clone(),
            symbol: d.name.clone(),
            kind: d.kind.clone(),
            language: d.language.clone(),
            start_line: d.start_line,
            end_line: d.end_line,
            start_byte: d.start_byte,
            end_byte: d.end_byte,
            est_tokens: est,
            reason: role,
            text,
        });
    }
    // Focus first, then path order. The budget already guarantees the definition
    // survives — but the *reading* order is a separate thing, and sorting purely
    // by path put the thing you asked about wherever its path happened to sort.
    // On a polyglot repo that is a real cost: `explore ParseNginxLogs` on a Go
    // function opened with 79 lines of same-named TypeScript from an unrelated
    // program, so `explore … | head -90` — the obvious way to sample a large
    // result — showed callees and not the definition. Hoisting one block costs
    // nothing and makes `head` do the right thing.
    if !ranked {
        chunks.sort_by(|a, b| {
            (a.reason != "focus", a.path.as_str(), a.start_line).cmp(&(
                b.reason != "focus",
                b.path.as_str(),
                b.start_line,
            ))
        });
    }

    Ok(ContextResult {
        plan: None,
        budget: Budget {
            max_tokens,
            used_tokens: used,
            tokenizer: TOKENIZER,
        },
        chunks,
        omitted,
        omitted_truncated,
    })
}

/// Definitions of an exact name within one file, via the facts(name) index.
fn defs_by_name_in_path(db: &Db, name: &str, path: &str) -> Result<Vec<Def>> {
    let sql =
        format!("SELECT {DEF_COLUMNS} FROM facts WHERE name = ? AND path = ? AND kind <> 'import'");
    let mut stmt = db.conn.prepare(&sql)?;
    let rows = stmt.query_map(rusqlite::params![name, path], row_to_def)?;
    Ok(rows.collect::<std::result::Result<_, _>>()?)
}

fn defs_enclosing_byte(db: &Db, path: &str, byte: usize) -> Result<Vec<Def>> {
    let sql = format!("SELECT {DEF_COLUMNS} FROM facts WHERE path = ? AND kind <> 'import' AND start_byte <= ? AND end_byte > ? ORDER BY end_byte - start_byte, start_byte DESC LIMIT 1");
    let mut stmt = db.conn.prepare(&sql)?;
    let rows = stmt.query_map(rusqlite::params![path, byte, byte], row_to_def)?;
    Ok(rows.collect::<std::result::Result<_, _>>()?)
}

fn row_to_def(r: &rusqlite::Row) -> rusqlite::Result<Def> {
    Ok(Def {
        name: r.get(0)?,
        kind: r.get(1)?,
        path: r.get(2)?,
        language: r.get(3)?,
        start_byte: r.get::<_, i64>(4)? as usize,
        end_byte: r.get::<_, i64>(5)? as usize,
        start_line: r.get(6)?,
        end_line: r.get(7)?,
    })
}

const DEF_COLUMNS: &str = "name, kind, path, language, start_byte, end_byte, start_line, end_line";

/// Load definitions matching a set of values in one indexed column, in chunks to
/// stay under SQLite's bound-parameter limit.
///
/// `column` is `name` or `path`; both are indexed, and the two call sites were
/// otherwise the same function written twice.
fn defs_by_column(db: &Db, column: &str, values: &[String]) -> Result<Vec<Def>> {
    debug_assert!(
        matches!(column, "name" | "path"),
        "column is not interpolable from input"
    );
    let mut out = Vec::new();
    for chunk in values.chunks(500) {
        if chunk.is_empty() {
            continue;
        }
        let ph = vec!["?"; chunk.len()].join(",");
        let sql = format!(
            "SELECT {DEF_COLUMNS} FROM facts WHERE kind <> 'import' AND {column} IN ({ph})"
        );
        let mut stmt = db.conn.prepare(&sql)?;
        let params: Vec<&dyn rusqlite::ToSql> =
            chunk.iter().map(|v| v as &dyn rusqlite::ToSql).collect();
        let rows = stmt.query_map(params.as_slice(), row_to_def)?;
        for r in rows {
            out.push(r?);
        }
    }
    Ok(out)
}

/// Names called within the seed definitions, read from the syntax-derived
/// callsite index. One statement is reused for every seed; the byte bounds keep
/// a class/module seed from accidentally borrowing calls outside its own span.
struct CalledNames {
    /// Bare calls, calls on values, and external/unknown receivers. They retain
    /// the historical name-based expansion because no repository file is a
    /// safe target to prefer.
    unscoped: HashSet<String>,
    /// Calls on a directly imported, single-segment module, keyed by callee
    /// name and narrowed to the module's resolved internal files.
    scoped: HashMap<String, HashSet<String>>,
}

impl CalledNames {
    fn names(&self) -> HashSet<String> {
        self.unscoped
            .iter()
            .chain(self.scoped.keys())
            .cloned()
            .collect()
    }
}

fn called_names(db: &Db, seeds: &[Def]) -> Result<CalledNames> {
    let mut stmt = db.conn.prepare(
        "SELECT name, receiver FROM calls
         WHERE file_id = (SELECT id FROM files WHERE path = ?1)
           AND start_byte >= ?2 AND start_byte < ?3",
    )?;
    let graph = crate::commands::load_dep_graph(db)?;
    let mut out = CalledNames {
        unscoped: HashSet::new(),
        scoped: HashMap::new(),
    };
    for seed in seeds {
        let rows = stmt.query_map(
            rusqlite::params![&seed.path, seed.start_byte as i64, seed.end_byte as i64],
            |r| Ok((r.get::<_, String>(0)?, r.get::<_, Option<String>>(1)?)),
        )?;
        for row in rows {
            let (name, receiver) = row?;
            let name = if receiver.is_none() {
                graph
                    .source_name_for_binding(&seed.path, &name)
                    .unwrap_or(&name)
                    .to_string()
            } else {
                name
            };
            let targets = receiver
                .as_deref()
                .map(|receiver| graph.direct_targets_for_receiver(&seed.path, receiver))
                .filter(|targets| !targets.is_empty())
                .unwrap_or_else(|| graph.direct_targets_for_receiver(&seed.path, &name));
            if targets.is_empty() {
                out.unscoped.insert(name);
            } else {
                out.scoped
                    .entry(name)
                    .or_default()
                    .extend(targets.into_iter().map(str::to_string));
            }
        }
    }
    Ok(out)
}

/// Slice a definition's source out of its file, caching file reads.
/// Shrink a force-included chunk until it fits the remaining budget, updating
/// `text`/`est` in place and returning how it was reduced (`None` = untouched).
///
/// A forced chunk is never dropped — dropping the very symbol you asked for is
/// worse than overshooting — but "never dropped" used to mean `--max-tokens`
/// wasn't a limit at all: a definition larger than the whole budget sailed
/// through it, and a container definition (a class, a module, a whole-file
/// single-file component) is routinely that big. So: fold nested bodies first,
/// which keeps the shape and every signature, and only clip the tail if the
/// definition is mostly markup or data that folding can't touch. Either way the
/// marker says exactly which lines are missing, and `read --lines` recovers them.
fn reduce_to_fit(
    db: &Db,
    d: &Def,
    text: &mut Option<String>,
    est: &mut u64,
    used: u64,
    max_tokens: u64,
) -> Option<&'static str> {
    if used + *est <= max_tokens {
        return None;
    }
    let mut how: Option<&'static str> = None;
    if let Some(f) = text.as_deref().and_then(|t| fold_bodies(db, d, t)) {
        let fest = (f.len() as u64).div_ceil(4).max(1);
        if fest < *est {
            *est = fest;
            *text = Some(f);
            how = Some("folded");
        }
    }
    if used + *est > max_tokens {
        let room = max_tokens.saturating_sub(used).saturating_mul(4).max(1);
        if let Some(c) = text
            .as_deref()
            .and_then(|t| clip_to_budget(t, room as usize, d.start_line))
        {
            *est = (c.len() as u64).div_ceil(4).max(1);
            *text = Some(c);
            how = Some("clipped");
        }
    }
    how
}

/// `text` cut to at most `room` bytes on a line boundary, with a marker naming
/// the line range that was dropped. `None` when it already fits or when not even
/// one line does — a caller that gets `None` keeps the full text rather than
/// emit a chunk with no content.
fn clip_to_budget(text: &str, room: usize, start_line: i64) -> Option<String> {
    if text.len() <= room {
        return None;
    }
    let mut kept = 0usize; // bytes
    let mut lines = 0usize;
    for l in text.lines() {
        let next = kept + l.len() + 1;
        // Leave headroom for the marker itself so the result really fits.
        if next + 64 > room {
            break;
        }
        kept = next;
        lines += 1;
    }
    if lines == 0 {
        return None;
    }
    let total = text.lines().count();
    let mut out: String = text.lines().take(lines).collect::<Vec<_>>().join("\n");
    out.push('\n');
    out.push_str(&format!(
        "        ⋮  {} lines ({}–{}) clipped to fit --max-tokens\n",
        total - lines,
        start_line + lines as i64,
        start_line + total as i64 - 1
    ));
    Some(out)
}

/// `text` with the bodies of function/method definitions *nested inside* `d`
/// collapsed to a marker naming the exact hidden line range — the same folded
/// view `read <file>` gives, applied to one definition's source. `None` when
/// there is nothing worth folding, so the caller keeps the verbatim text.
fn fold_bodies(db: &Db, d: &Def, text: &str) -> Option<String> {
    let mut stmt = db
        .conn
        .prepare(
            "SELECT start_line, end_line FROM facts
             WHERE path = ?1 AND kind IN ('function', 'method')
               AND start_line >= ?2 AND end_line <= ?3",
        )
        .ok()?;
    let rows = stmt
        .query_map(rusqlite::params![&d.path, d.start_line, d.end_line], |r| {
            Ok((r.get::<_, i64>(0)?, r.get::<_, i64>(1)?))
        })
        .ok()?;

    let lines: Vec<&str> = text.lines().collect();
    let mut hide = vec![false; lines.len()];
    let mut any = false;
    for row in rows {
        let (sl, el) = row.ok()?;
        if el < sl || (el - sl) < crate::commands::MIN_FOLD as i64 {
            continue;
        }
        // Never fold `d` itself away — only things nested within it.
        if sl <= d.start_line && el >= d.end_line {
            continue;
        }
        for ln in (sl + 1)..=el {
            let idx = (ln - d.start_line) as usize;
            if ln >= d.start_line && idx < hide.len() {
                hide[idx] = true;
                any = true;
            }
        }
    }
    if !any {
        return None;
    }

    let mut out = String::with_capacity(text.len() / 2);
    let mut i = 0;
    while i < lines.len() {
        if hide[i] {
            let start = i;
            while i < lines.len() && hide[i] {
                i += 1;
            }
            let (a, b) = (d.start_line + start as i64, d.start_line + i as i64 - 1);
            out.push_str(&format!("        ⋮  {} lines ({a}–{b})\n", i - start));
        } else {
            out.push_str(lines[i]);
            out.push('\n');
            i += 1;
        }
    }
    Some(out)
}

/// The source of one definition, sliced by its byte range.
///
/// The file is normalised exactly as the indexer normalised it before measuring
/// those offsets — leading BOM stripped, then decoded as UTF-8 with invalid bytes
/// replaced — because the offsets are positions in *that* text, not in the bytes
/// on disk. Slicing the raw bytes instead shifted every chunk by three on a
/// BOM-prefixed file, and by two per invalid byte on anything that wasn't valid
/// UTF-8, yielding source that started mid-expression.
fn chunk_text(root: &Path, d: &Def, cache: &mut HashMap<String, Option<String>>) -> Option<String> {
    let text = cache
        .entry(d.path.clone())
        .or_insert_with(|| crate::index::load_body(root, &d.path));
    let text = text.as_ref()?;
    let end = d.end_byte.min(text.len());
    if d.start_byte >= end {
        return None;
    }
    // Byte ranges come from tree-sitter, so they land on char boundaries; guard
    // anyway rather than risk a panic on a stale index.
    if !text.is_char_boundary(d.start_byte) || !text.is_char_boundary(end) {
        return None;
    }
    Some(text[d.start_byte..end].to_string())
}

/// Collect identifier-like tokens (len >= 2) from text.
fn collect_identifiers(text: &str, out: &mut HashSet<String>) {
    let mut cur = String::new();
    for ch in text.chars() {
        if ch == '_' || ch.is_alphanumeric() {
            cur.push(ch);
        } else if !cur.is_empty() {
            if cur.len() >= 2 {
                out.insert(std::mem::take(&mut cur));
            } else {
                cur.clear();
            }
        }
    }
    if cur.len() >= 2 {
        out.insert(cur);
    }
}
