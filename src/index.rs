//! Ignore-aware walk, hashing, and incremental refresh.
//!
//! This is the daemonless freshness path: every query calls [`refresh`], which
//! walks the tree, reparses only files whose size/mtime changed, and reports
//! how many files it had to touch. A no-op refresh on an unchanged tree writes
//! nothing.

use std::collections::HashMap;
use std::time::UNIX_EPOCH;

use anyhow::{Context, Result};
use ignore::WalkBuilder;

use crate::db::Db;
use crate::facts::Extractor;
use crate::repo::Repo;

/// Result of a refresh pass.
pub struct RefreshStats {
    /// Files newly indexed or whose content hash changed.
    pub reindexed: u64,
    /// Files we could not read/stat and therefore skipped.
    pub partial_failures: u64,
}

struct Stored {
    size: i64,
    mtime_ns: i64,
    hash: Option<String>,
}

/// Bring the index in line with the working tree. Returns how many files were
/// (re)parsed. Incremental: unchanged files (same size + mtime) are skipped
/// without hashing. For git repos, a matching freshness token skips the walk
/// entirely.
pub fn refresh(db: &Db, repo: &Repo) -> Result<RefreshStats> {
    let repo_root = repo.root.as_path();

    // Fast path: if the git freshness token matches the last full index, the
    // working tree is unchanged and no walk is needed.
    let token = repo.freshness_token()?;
    if let Some(t) = &token {
        if db.get_meta("fresh_token")?.as_deref() == Some(t.as_str()) {
            return Ok(RefreshStats {
                reindexed: 0,
                partial_failures: 0,
            });
        }
    }

    // Load the current index state once.
    let mut existing: HashMap<String, Stored> = HashMap::new();
    {
        let mut stmt = db
            .conn
            .prepare("SELECT path, size_bytes, mtime_ns, content_hash FROM files")?;
        let rows = stmt.query_map([], |r| {
            Ok((
                r.get::<_, String>(0)?,
                Stored {
                    size: r.get::<_, i64>(1)?,
                    mtime_ns: r.get::<_, i64>(2).unwrap_or(0),
                    hash: r.get::<_, Option<String>>(3)?,
                },
            ))
        })?;
        for row in rows {
            let (path, stored) = row?;
            existing.insert(path, stored);
        }
    }

    let mut seen: HashMap<String, ()> = HashMap::new();
    let mut reindexed: u64 = 0;
    let mut partial_failures: u64 = 0;
    let mut extractor = Extractor::new()?;

    let tx = db.conn.unchecked_transaction()?;
    {
        let mut upsert = tx.prepare(
            "INSERT INTO files(path, language, size_bytes, mtime_ns, content_hash, indexed_at)
             VALUES(?1, ?2, ?3, ?4, ?5, ?6)
             ON CONFLICT(path) DO UPDATE SET
                language = excluded.language,
                size_bytes = excluded.size_bytes,
                mtime_ns = excluded.mtime_ns,
                content_hash = excluded.content_hash,
                indexed_at = excluded.indexed_at",
        )?;
        let mut del_facts = tx.prepare("DELETE FROM facts WHERE path = ?1")?;
        let mut ins_fact = tx.prepare(
            "INSERT OR REPLACE INTO facts(
                id, kind, name, path, language,
                start_byte, end_byte, start_line, start_col, end_line, end_col,
                signature, doc, source, confidence)
             VALUES(?1,?2,?3,?4,?5,?6,?7,?8,?9,?10,?11,?12,?13,?14,?15)",
        )?;
        // FTS5 has no UPSERT; update is delete-then-insert keyed by path.
        let mut del_fts = tx.prepare("DELETE FROM content_fts WHERE path = ?1")?;
        let mut ins_fts = tx.prepare(
            "INSERT INTO content_fts(path, language, body) VALUES(?1, ?2, ?3)",
        )?;
        let mut del_spans = tx.prepare("DELETE FROM spans WHERE path = ?1")?;
        let mut ins_span = tx.prepare(
            "INSERT INTO spans(path, start_byte, end_byte, kind) VALUES(?1, ?2, ?3, ?4)",
        )?;

        let now = now_ns();

        let walker = WalkBuilder::new(repo_root)
            .standard_filters(true) // .gitignore, .ignore, hidden, parents
            .build();

        for dent in walker {
            let dent = match dent {
                Ok(d) => d,
                Err(_) => continue, // unreadable entry: skip, don't crash
            };
            if !dent.file_type().map(|t| t.is_file()).unwrap_or(false) {
                continue;
            }
            let abs = dent.path();
            let rel = match abs.strip_prefix(repo_root) {
                Ok(r) => r.to_string_lossy().replace('\\', "/"),
                Err(_) => continue,
            };

            let meta = match dent.metadata() {
                Ok(m) => m,
                Err(_) => {
                    partial_failures += 1;
                    continue;
                }
            };
            let size = meta.len() as i64;
            let mtime_ns = meta
                .modified()
                .ok()
                .and_then(|t| t.duration_since(UNIX_EPOCH).ok())
                .map(|d| d.as_nanos() as i64)
                .unwrap_or(0);

            seen.insert(rel.clone(), ());

            // Fast path: size + mtime unchanged => trust the stored hash.
            if let Some(prev) = existing.get(&rel) {
                if prev.size == size && prev.mtime_ns == mtime_ns {
                    continue;
                }
            }

            // Something moved: read + hash to decide whether content actually
            // changed. Read once, reuse the bytes for fact extraction.
            let bytes = match std::fs::read(abs) {
                Ok(b) => b,
                Err(_) => {
                    partial_failures += 1;
                    continue; // unreadable; leave prior row untouched
                }
            };
            let hash = blake3::hash(&bytes).to_hex().to_string();
            let is_binary = looks_binary(&bytes);
            let language = if is_binary { None } else { language_for(&rel) };

            let content_changed = existing
                .get(&rel)
                .map(|p| p.hash.as_deref() != Some(hash.as_str()))
                .unwrap_or(true);

            upsert.execute((&rel, language, size, mtime_ns, &hash, now))?;

            if content_changed {
                reindexed += 1;
                // Content-derived data: drop stale, rebuild. FTS first (covers
                // text files even when we can't classify their language).
                del_fts.execute([&rel])?;
                if !is_binary {
                    let body = String::from_utf8_lossy(&bytes);
                    ins_fts.execute((&rel, language, body.as_ref()))?;
                }
                // Facts + spans are derived from content: drop stale, re-extract.
                del_facts.execute([&rel])?;
                del_spans.execute([&rel])?;
                if let Some(lang) = language {
                    let (facts, spans) = extractor.extract(lang, &bytes);
                    for f in facts {
                        ins_fact.execute((
                            f.id(&rel),
                            &f.kind,
                            &f.name,
                            &rel,
                            lang,
                            f.start_byte as i64,
                            f.end_byte as i64,
                            f.start_line as i64,
                            f.start_col as i64,
                            f.end_line as i64,
                            f.end_col as i64,
                            &f.signature,
                            &f.doc,
                            &f.source,
                            f.confidence,
                        ))?;
                    }
                    for (sb, eb, sp) in spans {
                        ins_span.execute((&rel, sb as i64, eb as i64, sp.as_i64()))?;
                    }
                }
            }
        }
    }

    // Drop files that vanished from the tree.
    {
        let mut del = tx.prepare("DELETE FROM files WHERE path = ?1")?;
        // FTS is not FK-linked to files, so remove it explicitly.
        let mut del_fts = tx.prepare("DELETE FROM content_fts WHERE path = ?1")?;
        let gone: Vec<String> = existing
            .keys()
            .filter(|p| !seen.contains_key(*p))
            .cloned()
            .collect();
        for p in gone {
            del.execute([&p])?;
            del_fts.execute([&p])?;
        }
    }

    tx.commit()?;
    db.set_meta("last_indexed_at", &now_ns().to_string())?;
    // Record the state we just indexed so the next query can skip the walk.
    if let Some(t) = token {
        db.set_meta("fresh_token", &t)?;
    }
    Ok(RefreshStats {
        reindexed,
        partial_failures,
    })
}

fn looks_binary(bytes: &[u8]) -> bool {
    let n = bytes.len().min(8192);
    bytes[..n].contains(&0)
}

/// Extension-based language classification. Deliberately syntactic and cheap;
/// this is not content sniffing.
pub fn language_for(path: &str) -> Option<&'static str> {
    let ext = path.rsplit('.').next()?;
    let lang = match ext {
        "rs" => "rust",
        "js" | "cjs" | "mjs" => "javascript",
        "jsx" => "javascript",
        "ts" | "cts" | "mts" => "typescript",
        "tsx" => "tsx",
        "py" | "pyi" => "python",
        "go" => "go",
        "java" => "java",
        "json" => "json",
        "yaml" | "yml" => "yaml",
        "toml" => "toml",
        "md" | "markdown" => "markdown",
        "sql" => "sql",
        "sh" | "bash" => "shell",
        "c" | "h" => "c",
        "cc" | "cpp" | "cxx" | "hpp" => "cpp",
        "rb" => "ruby",
        _ => return None,
    };
    Some(lang)
}

fn now_ns() -> i64 {
    std::time::SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_nanos() as i64)
        .unwrap_or(0)
}

impl Db {
    /// Best-effort placeholder so callers can `use` the module cohesively.
    pub fn file_count(&self) -> Result<i64> {
        self.conn
            .query_row("SELECT COUNT(*) FROM files", [], |r| r.get(0))
            .context("counting files")
    }
}
