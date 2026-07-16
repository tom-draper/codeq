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

    // Native, git-free fast path (our own index cache): stream the directory and
    // file manifests, stat each entry, and bail on the first mismatch — no
    // HashMap, and without loading the heavy content_hash column. Fresh iff:
    //   * every cached directory's mtime is unchanged — a file added, removed or
    //     renamed bumps its parent dir's mtime, so this catches all *structural*
    //     changes (the untracked-cache trick git uses), and
    //   * every cached file's (size, mtime) is unchanged — catches content edits.
    // Only a real change short-circuits it into the full walk below.
    if is_fresh(db, repo_root)? {
        return Ok(RefreshStats {
            reindexed: 0,
            partial_failures: 0,
        });
    }

    // Not fresh: load the full manifest (with hashes, to tell a real content
    // change from a mere mtime bump) and walk the tree to reconcile.
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
    let mut dirs_seen: HashMap<String, i64> = HashMap::new();
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
                signature, doc, source, confidence, visibility)
             VALUES(?1,?2,?3,?4,?5,?6,?7,?8,?9,?10,?11,?12,?13,?14,?15,?16)",
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
        let mut del_impls = tx.prepare("DELETE FROM impls WHERE path = ?1")?;
        let mut ins_impl = tx.prepare(
            "INSERT INTO impls(path, line, subtype, supertype, relation, language)
             VALUES(?1, ?2, ?3, ?4, ?5, ?6)",
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
            let ftype = match dent.file_type() {
                Some(t) => t,
                None => continue,
            };
            let abs = dent.path();
            let rel = match abs.strip_prefix(repo_root) {
                Ok(r) => r.to_string_lossy().replace('\\', "/"),
                Err(_) => continue,
            };
            // Cache every walked directory's mtime for the freshness fast path.
            if ftype.is_dir() {
                if let Some(m) = dent.metadata().ok().as_ref().and_then(mtime_ns_of) {
                    dirs_seen.insert(rel.clone(), m);
                }
                // Ignore files change which paths get indexed, but they're hidden
                // (never walked), so an in-place `.gitignore`/`.ignore` edit bumps
                // no cached mtime — track them here or freshness would miss it.
                for ig in [".gitignore", ".ignore"] {
                    if let Some(m) = std::fs::metadata(abs.join(ig)).ok().as_ref().and_then(mtime_ns_of) {
                        let key = if rel.is_empty() { ig.to_string() } else { format!("{rel}/{ig}") };
                        dirs_seen.insert(key, m);
                    }
                }
                continue;
            }
            if !ftype.is_file() {
                continue;
            }

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
            let mut bytes = match std::fs::read(abs) {
                Ok(b) => b,
                Err(_) => {
                    partial_failures += 1;
                    continue; // unreadable; leave prior row untouched
                }
            };
            // Strip a leading UTF-8 BOM so line-1 `^`/`$` anchors match and the
            // stored byte offsets align with the logical content — ripgrep and
            // editors do the same. A BOM before the first char otherwise hides
            // line 1 from anchored search (common in C#/Windows-authored files).
            if bytes.starts_with(&[0xEF, 0xBB, 0xBF]) {
                bytes.drain(..3);
            }
            let hash = blake3::hash(&bytes).to_hex().to_string();
            let is_binary = looks_binary(&bytes);
            let language = if is_binary { None } else { language_for(&rel) };

            let prev = existing.get(&rel);
            let content_changed = prev
                .map(|p| p.hash.as_deref() != Some(hash.as_str()))
                .unwrap_or(true);
            let was_indexed = prev.is_some();

            upsert.execute((&rel, language, size, mtime_ns, &hash, now))?;

            if content_changed {
                reindexed += 1;
                // Drop the file's stale derived rows — but only if it was already
                // indexed. A brand-new file (every file during --rebuild / a cold
                // index) has nothing to delete, and `DELETE FROM content_fts WHERE
                // path=?` scans the whole growing FTS table (path is UNINDEXED),
                // so running it per new file made cold indexing O(files²).
                if was_indexed {
                    del_fts.execute([&rel])?;
                    del_facts.execute([&rel])?;
                    del_spans.execute([&rel])?;
                    del_impls.execute([&rel])?;
                }
                // FTS first (covers text files even when we can't classify them).
                if !is_binary {
                    let body = String::from_utf8_lossy(&bytes);
                    ins_fts.execute((&rel, language, body.as_ref()))?;
                }
                if let Some(lang) = language {
                    let (facts, spans, relations) = extractor.extract(lang, &bytes);
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
                            &f.visibility,
                        ))?;
                    }
                    for (sb, eb, sp) in spans {
                        ins_span.execute((&rel, sb as i64, eb as i64, sp.as_i64()))?;
                    }
                    for r in relations {
                        ins_impl.execute((
                            &rel,
                            r.start_line as i64,
                            &r.subtype,
                            &r.supertype,
                            r.relation,
                            lang,
                        ))?;
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

    // Replace the directory-mtime cache with the tree we just walked.
    {
        tx.execute("DELETE FROM dirs", [])?;
        let mut ins_dir = tx.prepare("INSERT INTO dirs(path, mtime_ns) VALUES(?1, ?2)")?;
        for (p, m) in &dirs_seen {
            ins_dir.execute((p, m))?;
        }
    }

    tx.commit()?;
    db.set_meta("last_indexed_at", &now_ns().to_string())?;
    Ok(RefreshStats {
        reindexed,
        partial_failures,
    })
}

/// Whether the index is fresh, checked against codeq's own cache without a walk:
/// stream the cached directory and file stats, stat each on disk, and stop at the
/// first mismatch. Streams (no HashMap) and skips the content_hash column, so the
/// common "nothing changed" path is just syscalls. An empty dir cache (no index
/// yet, or a pre-v6 one) is never fresh — the caller must do a full walk.
fn is_fresh(db: &Db, root: &std::path::Path) -> Result<bool> {
    let mut any_dir = false;
    {
        let mut stmt = db.conn.prepare("SELECT path, mtime_ns FROM dirs")?;
        let mut rows = stmt.query([])?;
        while let Some(r) = rows.next()? {
            any_dir = true;
            let path: String = r.get(0)?;
            let mtime: i64 = r.get(1)?;
            if path_mtime(&root.join(&path)) != Some(mtime) {
                return Ok(false);
            }
        }
    }
    if !any_dir {
        return Ok(false);
    }
    let mut stmt = db
        .conn
        .prepare("SELECT path, size_bytes, mtime_ns FROM files")?;
    let mut rows = stmt.query([])?;
    while let Some(r) = rows.next()? {
        let path: String = r.get(0)?;
        let size: i64 = r.get(1)?;
        let mtime: i64 = r.get::<_, Option<i64>>(2)?.unwrap_or(0);
        if file_size_mtime(&root.join(&path)) != Some((size, mtime)) {
            return Ok(false);
        }
    }
    Ok(true)
}

/// Nanosecond mtime of a stat result, if representable.
fn mtime_ns_of(md: &std::fs::Metadata) -> Option<i64> {
    md.modified()
        .ok()
        .and_then(|t| t.duration_since(std::time::UNIX_EPOCH).ok())
        .map(|d| d.as_nanos() as i64)
}

/// Mtime of a watched path for the freshness check: a directory (structural
/// changes bump it) or an ignore file (edits change what gets indexed).
fn path_mtime(path: &std::path::Path) -> Option<i64> {
    mtime_ns_of(&std::fs::metadata(path).ok()?)
}

/// File `(size, mtime_ns)` for the freshness check, or `None` if not a file.
fn file_size_mtime(path: &std::path::Path) -> Option<(i64, i64)> {
    let md = std::fs::metadata(path).ok()?;
    if !md.is_file() {
        return None;
    }
    Some((md.len() as i64, mtime_ns_of(&md).unwrap_or(0)))
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
        "cc" | "cpp" | "cxx" | "hpp" | "hh" => "cpp",
        "cs" => "csharp",
        "php" => "php",
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
