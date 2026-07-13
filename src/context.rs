//! Token-budgeted context assembly.
//!
//! This is the command that has to justify the whole project: return the
//! *smallest useful* slice of code for a task, not just search results. The
//! design follows the constraints argued in DRAFT.md:
//!
//! - **Seeds are explicit.** codeq does not guess task intent. The agent names
//!   symbols, or asks for changed code; we assemble from that.
//! - **Ranking is deterministic and explainable.** Seeds first, then a single
//!   name-based expansion hop (definitions referenced by name inside the seed
//!   chunks), then a stable path+line tiebreak. No hidden semantic scoring.
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
    chunks: Vec<Chunk>,
    /// Candidates dropped by the token budget — a trust signal, so omitted only
    /// when there actually were omissions.
    #[serde(skip_serializing_if = "Vec::is_empty")]
    omitted: Vec<Omitted>,
    #[serde(skip_serializing_if = "std::ops::Not::not")]
    omitted_truncated: bool,
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
struct Budget {
    max_tokens: u64,
    used_tokens: u64,
    /// The estimate is approximate (byte-based); labeled so an agent never
    /// treats it as an exact per-model count.
    tokenizer: &'static str,
}

#[derive(Serialize)]
struct Chunk {
    path: String,
    symbol: String,
    kind: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    language: Option<String>,
    start_line: i64,
    end_line: i64,
    start_byte: usize,
    end_byte: usize,
    est_tokens: u64,
    /// Why this chunk is here: "seed" or "referenced".
    reason: &'static str,
    #[serde(skip_serializing_if = "Option::is_none")]
    text: Option<String>,
}

#[derive(Serialize)]
struct Omitted {
    path: String,
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

    // Load only the definitions we actually need, via the facts(name)/facts(path)
    // indexes — never a full scan of every definition. `defs` holds the seeds
    // first (indices 0..seed_count), then any expansion neighbours appended.
    let mut modes: Vec<&'static str> = Vec::new();
    let mut defs: Vec<Def> = Vec::new();
    let mut seen: HashSet<(String, usize)> = HashSet::new(); // (path, start_byte)

    if !req.symbols.is_empty() {
        modes.push("symbols");
        for d in defs_by_names(db, &req.symbols)? {
            if seen.insert((d.path.clone(), d.start_byte)) {
                defs.push(d);
            }
        }
    }
    if req.changed {
        modes.push("changed");
        let changed = repo.changed_since(&req.compare_to)?;
        for d in defs_by_paths(db, &changed)? {
            if seen.insert((d.path.clone(), d.start_byte)) {
                defs.push(d);
            }
        }
    }

    // Optional path filter narrows the seed set.
    if let Some(prefix) = &req.path {
        defs.retain(|d| d.path.contains(prefix.as_str()));
    }

    // Everything loaded so far is a seed. Deterministic order.
    defs.sort_by(|a, b| (a.path.as_str(), a.start_line).cmp(&(b.path.as_str(), b.start_line)));
    let seed_count = defs.len();

    let mut file_cache: HashMap<String, Option<Vec<u8>>> = HashMap::new();

    // Read seed chunk texts (needed for expansion regardless of include_text).
    let mut seed_text: HashMap<usize, String> = HashMap::new();
    for i in 0..seed_count {
        if let Some(t) = chunk_text(&repo.root, &defs[i], &mut file_cache) {
            seed_text.insert(i, t);
        }
    }

    // One-hop expansion: definitions referenced by name inside the seed chunks,
    // that are not already seeds. Looked up by name (indexed), never a full scan.
    if req.expand {
        let mut refs: HashSet<String> = HashSet::new();
        for t in seed_text.values() {
            collect_identifiers(t, &mut refs);
        }
        let seed_names: HashSet<&str> = defs[..seed_count].iter().map(|d| d.name.as_str()).collect();
        refs.retain(|r| !seed_names.contains(r.as_str()));
        let seed_keys: HashSet<(String, usize)> = defs[..seed_count]
            .iter()
            .map(|d| (d.path.clone(), d.start_byte))
            .collect();

        let names: Vec<String> = refs.into_iter().collect();
        let mut neighbors: Vec<Def> = Vec::new();
        let mut added: HashSet<(String, usize)> = HashSet::new();
        for d in defs_by_names(db, &names)? {
            let key = (d.path.clone(), d.start_byte);
            if !seed_keys.contains(&key) && added.insert(key) {
                neighbors.push(d);
            }
        }
        neighbors.sort_by(|a, b| {
            (a.name.as_str(), a.path.as_str(), a.start_line)
                .cmp(&(b.name.as_str(), b.path.as_str(), b.start_line))
        });
        neighbors.truncate(MAX_NEIGHBORS);
        defs.extend(neighbors);
    }

    // Candidate list in rank order: seeds, then referenced neighbours.
    let candidates: Vec<(usize, &'static str)> = (0..seed_count)
        .map(|i| (i, "seed"))
        .chain((seed_count..defs.len()).map(|i| (i, "referenced")))
        .collect();

    // Budget fill: include in rank order while it fits; a chunk that overflows
    // is recorded as omitted, and we keep trying smaller later chunks.
    let mut chunks = Vec::new();
    let mut omitted = Vec::new();
    let mut used: u64 = 0;
    for (i, reason) in candidates {
        let d = &defs[i];
        let text = seed_text
            .remove(&i)
            .or_else(|| chunk_text(&repo.root, d, &mut file_cache));
        let bytes = d.end_byte.saturating_sub(d.start_byte) as u64;
        let est = bytes.div_ceil(4).max(1);

        if used + est > req.max_tokens {
            if omitted.len() < MAX_OMITTED {
                omitted.push(Omitted {
                    path: d.path.clone(),
                    symbol: d.name.clone(),
                    est_tokens: est,
                    reason,
                });
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
        });
    }

    let omitted_truncated = omitted.len() >= MAX_OMITTED;
    let seeds: Vec<String> = {
        let mut s: Vec<String> = defs[..seed_count].iter().map(|d| d.name.clone()).collect();
        s.sort();
        s.dedup();
        s
    };

    let plan = req.plan.then(|| Plan {
        modes,
        seeds,
        expansion: if req.expand {
            "one-hop: definitions referenced by name in seed chunks"
        } else {
            "disabled"
        },
        order: "seeds first, then referenced defs; each group by path+line; a chunk over budget is skipped, not reordered",
    });

    Ok(ContextResult {
        plan,
        budget: Budget {
            max_tokens: req.max_tokens,
            used_tokens: used,
            tokenizer: TOKENIZER,
        },
        chunks,
        omitted,
        omitted_truncated,
    })
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

const DEF_COLUMNS: &str =
    "name, kind, path, language, start_byte, end_byte, start_line, end_line";

/// Load definitions whose name is in `names`, via the facts(name) index, in
/// chunks to stay under SQLite's bound-parameter limit.
fn defs_by_names(db: &Db, names: &[String]) -> Result<Vec<Def>> {
    let mut out = Vec::new();
    for chunk in names.chunks(500) {
        if chunk.is_empty() {
            continue;
        }
        let ph = vec!["?"; chunk.len()].join(",");
        let sql = format!(
            "SELECT {DEF_COLUMNS} FROM facts WHERE kind <> 'import' AND name IN ({ph})"
        );
        let mut stmt = db.conn.prepare(&sql)?;
        let params: Vec<&dyn rusqlite::ToSql> =
            chunk.iter().map(|n| n as &dyn rusqlite::ToSql).collect();
        let rows = stmt.query_map(params.as_slice(), row_to_def)?;
        for r in rows {
            out.push(r?);
        }
    }
    Ok(out)
}

/// Load definitions in files whose path is in `paths`, via the facts(path) index.
fn defs_by_paths(db: &Db, paths: &[String]) -> Result<Vec<Def>> {
    let mut out = Vec::new();
    for chunk in paths.chunks(500) {
        if chunk.is_empty() {
            continue;
        }
        let ph = vec!["?"; chunk.len()].join(",");
        let sql = format!(
            "SELECT {DEF_COLUMNS} FROM facts WHERE kind <> 'import' AND path IN ({ph})"
        );
        let mut stmt = db.conn.prepare(&sql)?;
        let params: Vec<&dyn rusqlite::ToSql> =
            chunk.iter().map(|p| p as &dyn rusqlite::ToSql).collect();
        let rows = stmt.query_map(params.as_slice(), row_to_def)?;
        for r in rows {
            out.push(r?);
        }
    }
    Ok(out)
}

/// Slice a definition's source out of its file, caching file reads.
fn chunk_text(
    root: &Path,
    d: &Def,
    cache: &mut HashMap<String, Option<Vec<u8>>>,
) -> Option<String> {
    let bytes = cache
        .entry(d.path.clone())
        .or_insert_with(|| std::fs::read(root.join(&d.path)).ok());
    let bytes = bytes.as_ref()?;
    let end = d.end_byte.min(bytes.len());
    if d.start_byte >= end {
        return None;
    }
    Some(String::from_utf8_lossy(&bytes[d.start_byte..end]).into_owned())
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
