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

    // Load all definitions once; we need them both as seed candidates and as
    // the target set for name-based expansion.
    let defs = load_defs(db)?;
    let mut by_name: HashMap<&str, Vec<usize>> = HashMap::new();
    for (i, d) in defs.iter().enumerate() {
        by_name.entry(d.name.as_str()).or_default().push(i);
    }

    let mut modes: Vec<&'static str> = Vec::new();
    let mut seed_idx: Vec<usize> = Vec::new();
    let mut seen: HashSet<usize> = HashSet::new();

    if !req.symbols.is_empty() {
        modes.push("symbols");
        for name in &req.symbols {
            if let Some(ixs) = by_name.get(name.as_str()) {
                for &i in ixs {
                    if seen.insert(i) {
                        seed_idx.push(i);
                    }
                }
            }
        }
    }
    if req.changed {
        modes.push("changed");
        let changed: HashSet<String> =
            repo.changed_since(&req.compare_to)?.into_iter().collect();
        for (i, d) in defs.iter().enumerate() {
            if changed.contains(&d.path) && seen.insert(i) {
                seed_idx.push(i);
            }
        }
    }

    // Optional path filter narrows the seed set.
    if let Some(prefix) = &req.path {
        seed_idx.retain(|&i| defs[i].path.contains(prefix.as_str()));
    }

    // Deterministic seed order.
    seed_idx.sort_by(|&a, &b| {
        (defs[a].path.as_str(), defs[a].start_line)
            .cmp(&(defs[b].path.as_str(), defs[b].start_line))
    });

    let mut file_cache: HashMap<String, Option<Vec<u8>>> = HashMap::new();

    // Read seed chunk texts (needed for expansion regardless of include_text).
    let mut seed_text: HashMap<usize, String> = HashMap::new();
    for &i in &seed_idx {
        if let Some(t) = chunk_text(&repo.root, &defs[i], &mut file_cache) {
            seed_text.insert(i, t);
        }
    }

    // One-hop expansion: identifiers referenced inside seed chunks that name a
    // definition we hold and that is not already a seed.
    let mut neighbor_idx: Vec<usize> = Vec::new();
    if req.expand {
        let mut refs: HashSet<String> = HashSet::new();
        for t in seed_text.values() {
            collect_identifiers(t, &mut refs);
        }
        let seed_names: HashSet<&str> =
            seed_idx.iter().map(|&i| defs[i].name.as_str()).collect();
        let mut added: HashSet<usize> = HashSet::new();
        let mut names: Vec<&String> = refs.iter().collect();
        names.sort();
        for name in names {
            if seed_names.contains(name.as_str()) {
                continue;
            }
            if let Some(ixs) = by_name.get(name.as_str()) {
                for &i in ixs {
                    if !seen.contains(&i) && added.insert(i) {
                        neighbor_idx.push(i);
                    }
                }
            }
        }
        neighbor_idx.sort_by(|&a, &b| {
            (defs[a].name.as_str(), defs[a].path.as_str(), defs[a].start_line).cmp(&(
                defs[b].name.as_str(),
                defs[b].path.as_str(),
                defs[b].start_line,
            ))
        });
        neighbor_idx.truncate(MAX_NEIGHBORS);
    }

    // Candidate list in rank order: seeds, then referenced neighbours.
    let candidates: Vec<(usize, &'static str)> = seed_idx
        .iter()
        .map(|&i| (i, "seed"))
        .chain(neighbor_idx.iter().map(|&i| (i, "referenced")))
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
        let mut s: Vec<String> = seed_idx.iter().map(|&i| defs[i].name.clone()).collect();
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

fn load_defs(db: &Db) -> Result<Vec<Def>> {
    let mut stmt = db.conn.prepare(
        "SELECT name, kind, path, language, start_byte, end_byte, start_line, end_line
         FROM facts WHERE kind <> 'import' AND name IS NOT NULL",
    )?;
    let rows = stmt.query_map([], |r| {
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
    })?;
    Ok(rows.collect::<std::result::Result<_, _>>()?)
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
