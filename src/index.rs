//! Ignore-aware walk, hashing, and incremental refresh.
//!
//! This is the daemonless freshness path: every query calls [`refresh`], which
//! walks the tree, reparses only files whose size/mtime/ctime changed, and reports
//! how many files it had to touch. A no-op refresh on an unchanged tree writes
//! nothing.

use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::UNIX_EPOCH;

use anyhow::{Context, Result};
use ignore::WalkBuilder;

use crate::db::Db;
use crate::facts::{Extractor, Fact, Relation, Span};
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
    ctime_ns: i64,
    hash: Option<String>,
}

/// A file parsed by a worker thread, ready for the serial writer to store. The
/// parse (read + hash + tree-sitter extraction) is the CPU cost and runs in
/// parallel; all SQLite writes stay on one thread, so index *content* is
/// deterministic regardless of which worker finished first.
struct Parsed {
    rel: String,
    size: i64,
    mtime_ns: i64,
    ctime_ns: i64,
    hash: String,
    language: Option<&'static str>,
    content_changed: bool,
    was_indexed: bool,
    /// Body for the FTS index — `Some` for a changed text file, else `None`.
    body: Option<String>,
    facts: Vec<Fact>,
    spans: Vec<(usize, usize, Span)>,
    relations: Vec<Relation>,
}

/// Read, hash, and (if the content changed) parse one file. Pure and
/// thread-safe: workers call it in parallel. `None` on an unreadable file.
fn parse_one(
    existing: &HashMap<String, Stored>,
    extractor: &mut Extractor,
    rel: &str,
    abs: &Path,
    size: i64,
    mtime_ns: i64,
    ctime_ns: i64,
) -> Option<Parsed> {
    let mut bytes = std::fs::read(abs).ok()?;
    // Strip a leading UTF-8 BOM so line-1 anchors match and byte offsets align.
    if bytes.starts_with(&[0xEF, 0xBB, 0xBF]) {
        bytes.drain(..3);
    }
    let hash = blake3::hash(&bytes).to_hex().to_string();
    let is_binary = looks_binary(&bytes);
    let language = if is_binary { None } else { language_for(rel) };

    let prev = existing.get(rel);
    let content_changed = prev
        .map(|p| p.hash.as_deref() != Some(hash.as_str()))
        .unwrap_or(true);
    let was_indexed = prev.is_some();

    let (body, facts, spans, relations) = if content_changed && !is_binary {
        // Everything downstream shares one coordinate system: the file decoded as
        // UTF-8 with invalid bytes replaced (and the BOM already stripped above).
        // Extracting from the raw bytes instead would put fact offsets and the
        // searchable body on different scales for any file that is not valid
        // UTF-8 — each invalid byte becomes a three-byte U+FFFD — so every
        // enclosing-symbol lookup past the first bad byte would name the wrong
        // definition. For valid UTF-8, which is virtually every file, this is
        // byte-identical and free: the string is materialised for the FTS body
        // either way.
        let text = String::from_utf8_lossy(&bytes).into_owned();
        let (f, s, r) = match language {
            Some(lang) => extractor.extract(lang, rel, text.as_bytes()),
            None => (Vec::new(), Vec::new(), Vec::new()),
        };
        (Some(text), f, s, r)
    } else {
        (None, Vec::new(), Vec::new(), Vec::new())
    };

    Some(Parsed {
        rel: rel.to_string(),
        size,
        mtime_ns,
        ctime_ns,
        hash,
        language,
        content_changed,
        was_indexed,
        body,
        facts,
        spans,
        relations,
    })
}

/// Bring the index in line with the working tree. Returns how many files were
/// (re)parsed. Incremental: unchanged files (same size + mtime + ctime) are skipped
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
    //   * every cached file's (size, mtime, ctime) is unchanged — catches content
    //     edits, including ones that restore mtime after writing.
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
            .prepare("SELECT path, size_bytes, mtime_ns, ctime_ns, content_hash FROM files")?;
        let rows = stmt.query_map([], |r| {
            Ok((
                r.get::<_, String>(0)?,
                Stored {
                    size: r.get::<_, i64>(1)?,
                    mtime_ns: r.get::<_, i64>(2).unwrap_or(0),
                    ctime_ns: r.get::<_, i64>(3).unwrap_or(0),
                    hash: r.get::<_, Option<String>>(4)?,
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
    let mut partial_failures: u64 = 0;

    // Serial walk (stat-bound): record directory mtimes for the freshness cache,
    // build the `seen` set, and collect the files that actually need
    // (re)processing — unchanged files short-circuit on the stat fast path.
    let now = now_ns();
    let mut work: Vec<(String, PathBuf, i64, i64, i64)> = Vec::new();
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
        let ctime_ns = ctime_ns_of(&meta);

        seen.insert(rel.clone(), ());

        // Fast path: size + mtime + ctime unchanged => trust the stored hash, no
        // read. ctime is what makes this safe: mtime alone can be restored after
        // a write, so a same-size edit would otherwise slip through unnoticed.
        if let Some(prev) = existing.get(&rel) {
            if prev.size == size && prev.mtime_ns == mtime_ns && prev.ctime_ns == ctime_ns {
                continue;
            }
        }
        work.push((rel, abs.to_path_buf(), size, mtime_ns, ctime_ns));
    }

    // Parse the changed files in parallel (read + hash + tree-sitter extraction
    // is the CPU cost) and write the results serially on this thread — the
    // transaction never crosses threads, so index content stays deterministic
    // regardless of which worker finished first. A bounded channel bounds the
    // parsed bodies held in flight.
    // BEGIN IMMEDIATE, not the default DEFERRED. A deferred transaction takes its
    // write lock lazily, on the first write — and SQLite refuses to make a reader
    // wait for that upgrade, returning SQLITE_BUSY *immediately* and bypassing the
    // busy timeout entirely, because two readers both upgrading would deadlock.
    // Taking the write lock up front is what lets the busy handler do its job, so
    // concurrent refreshes queue instead of failing.
    let tx = rusqlite::Transaction::new_unchecked(
        &db.conn,
        rusqlite::TransactionBehavior::Immediate,
    )?;
    // Whoever held the lock may have just done exactly this work: several agent
    // invocations noticing the same edit at once all decide to refresh, then
    // queue here. Re-check now that the lock is ours, and skip the parse and the
    // writes rather than redo them. Dropping `tx` unwritten rolls back cleanly.
    if is_fresh(db, repo_root)? {
        return Ok(RefreshStats {
            reindexed: 0,
            partial_failures,
        });
    }
    let parse_failures = AtomicU64::new(0);
    let reindexed = {
        let workers = std::thread::available_parallelism()
            .map(|n| n.get())
            .unwrap_or(1)
            .clamp(1, 8);
        let chunk = work.len().div_ceil(workers).max(1);
        let (res_tx, res_rx) = std::sync::mpsc::sync_channel::<Parsed>(256);
        std::thread::scope(|scope| -> Result<u64> {
            for c in work.chunks(chunk) {
                let sender = res_tx.clone();
                let existing = &existing;
                let failures = &parse_failures;
                scope.spawn(move || {
                    // Building the extractor compiles every language's queries, so
                    // it fails only when one of them is malformed — a programming
                    // error, identical for every worker and every file. Returning
                    // quietly meant the workers sent nothing, the writer stored
                    // nothing, and the whole index came back *empty and reporting
                    // itself fresh*: a broken query in one language silently
                    // zeroed all 28. Make it loud.
                    let mut ex = match Extractor::new() {
                        Ok(e) => e,
                        Err(e) => {
                            eprintln!("codeq: fact extraction unavailable: {e:#}");
                            std::process::exit(1);
                        }
                    };
                    for (rel, abs, size, mtime, ctime) in c {
                        match parse_one(existing, &mut ex, rel, abs, *size, *mtime, *ctime) {
                            Some(p) => {
                                if sender.send(p).is_err() {
                                    return; // writer went away (error) — stop
                                }
                            }
                            None => {
                                failures.fetch_add(1, Ordering::Relaxed);
                            }
                        }
                    }
                });
            }
            drop(res_tx); // close the channel once every worker's clone is gone

            // Serial writer: owns the transaction and its prepared statements.
            let mut upsert = tx.prepare(
                "INSERT INTO files(path, language, size_bytes, mtime_ns, ctime_ns, content_hash, indexed_at)
                 VALUES(?1, ?2, ?3, ?4, ?5, ?6, ?7)
                 ON CONFLICT(path) DO UPDATE SET
                    language = excluded.language,
                    size_bytes = excluded.size_bytes,
                    mtime_ns = excluded.mtime_ns,
                    ctime_ns = excluded.ctime_ns,
                    content_hash = excluded.content_hash,
                    indexed_at = excluded.indexed_at",
            )?;
            let mut del_facts = tx.prepare("DELETE FROM facts WHERE path = ?1")?;
            let mut ins_fact = tx.prepare(
                "INSERT OR REPLACE INTO facts(
                    id, kind, name, path, language,
                    start_byte, end_byte, start_line, start_col, end_line, end_col,
                    signature, doc, source, confidence, visibility, owner)
                 VALUES(?1,?2,?3,?4,?5,?6,?7,?8,?9,?10,?11,?12,?13,?14,?15,?16,?17)",
            )?;
            let mut del_fts = tx.prepare("DELETE FROM content_fts WHERE path = ?1")?;
            let mut ins_fts =
                tx.prepare("INSERT INTO content_fts(path, language, body) VALUES(?1, ?2, ?3)")?;
            let mut del_spans = tx.prepare("DELETE FROM spans WHERE path = ?1")?;
            let mut ins_span = tx.prepare(
                "INSERT INTO spans(path, start_byte, end_byte, kind) VALUES(?1, ?2, ?3, ?4)",
            )?;
            let mut del_impls = tx.prepare("DELETE FROM impls WHERE path = ?1")?;
            let mut ins_impl = tx.prepare(
                "INSERT INTO impls(path, line, subtype, supertype, relation, language)
                 VALUES(?1, ?2, ?3, ?4, ?5, ?6)",
            )?;

            let mut reindexed: u64 = 0;
            for p in res_rx {
                upsert.execute((&p.rel, p.language, p.size, p.mtime_ns, p.ctime_ns, &p.hash, now))?;
                if !p.content_changed {
                    continue;
                }
                reindexed += 1;
                // Only a previously-indexed file has stale rows to drop; a new file
                // (every file on a cold index) has none, and `DELETE FROM
                // content_fts WHERE path=?` scans the UNINDEXED-path FTS table, so
                // skipping it for new files is what keeps cold indexing linear.
                if p.was_indexed {
                    del_fts.execute([&p.rel])?;
                    del_facts.execute([&p.rel])?;
                    del_spans.execute([&p.rel])?;
                    del_impls.execute([&p.rel])?;
                }
                if let Some(body) = &p.body {
                    ins_fts.execute((&p.rel, p.language, body.as_str()))?;
                }
                if let Some(lang) = p.language {
                    for f in &p.facts {
                        // `params!` rather than a tuple: rusqlite only implements
                        // `Params` for tuples up to 16, and this is 17 columns.
                        ins_fact.execute(rusqlite::params![
                            f.id(&p.rel),
                            &f.kind,
                            &f.name,
                            &p.rel,
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
                            &f.owner,
                        ])?;
                    }
                    for (sb, eb, sp) in &p.spans {
                        ins_span.execute((&p.rel, *sb as i64, *eb as i64, sp.as_i64()))?;
                    }
                    for r in &p.relations {
                        ins_impl.execute((
                            &p.rel,
                            r.start_line as i64,
                            &r.subtype,
                            &r.supertype,
                            r.relation,
                            lang,
                        ))?;
                    }
                }
            }
            Ok(reindexed)
        })?
    };
    partial_failures += parse_failures.load(Ordering::Relaxed);

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
///
/// This runs on *every* query, so it's allocation-conscious: it borrows each
/// cached path from the SQLite row (no per-row `String`) and reuses one path
/// buffer (`root/…`) instead of allocating a fresh `PathBuf` per file — which at
/// 100k files is ~200k allocations saved off the per-query cost.
fn is_fresh(db: &Db, root: &std::path::Path) -> Result<bool> {
    // Reused `root/<rel>` buffer: truncate back to `root/` and append each rel.
    let mut buf = root.to_string_lossy().into_owned();
    if !buf.ends_with(std::path::MAIN_SEPARATOR) {
        buf.push(std::path::MAIN_SEPARATOR);
    }
    let base = buf.len();

    let mut any_dir = false;
    {
        let mut stmt = db.conn.prepare("SELECT path, mtime_ns FROM dirs")?;
        let mut rows = stmt.query([])?;
        while let Some(r) = rows.next()? {
            any_dir = true;
            let rel = r.get_ref(0)?.as_str()?;
            let mtime: i64 = r.get(1)?;
            buf.truncate(base);
            buf.push_str(rel);
            if path_mtime(std::path::Path::new(&buf)) != Some(mtime) {
                return Ok(false);
            }
        }
    }
    if !any_dir {
        return Ok(false);
    }
    let mut stmt = db
        .conn
        .prepare("SELECT path, size_bytes, mtime_ns, ctime_ns FROM files")?;
    let mut rows = stmt.query([])?;
    while let Some(r) = rows.next()? {
        let rel = r.get_ref(0)?.as_str()?;
        let size: i64 = r.get(1)?;
        let mtime: i64 = r.get::<_, Option<i64>>(2)?.unwrap_or(0);
        let ctime: i64 = r.get::<_, Option<i64>>(3)?.unwrap_or(0);
        buf.truncate(base);
        buf.push_str(rel);
        if file_stat_key(std::path::Path::new(&buf)) != Some((size, mtime, ctime)) {
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

/// Nanosecond inode-change time, the part of a stat that userspace cannot set.
/// `mtime` can be restored after a write (`touch -d`, `cp -p`, `rsync -a`, tar
/// extraction), so a same-size edit could otherwise leave (size, mtime) intact
/// and the index silently stale; `ctime` moves on every write. Unix-only —
/// elsewhere this contributes nothing and freshness falls back to size + mtime.
#[cfg(unix)]
fn ctime_ns_of(md: &std::fs::Metadata) -> i64 {
    use std::os::unix::fs::MetadataExt;
    md.ctime()
        .saturating_mul(1_000_000_000)
        .saturating_add(md.ctime_nsec())
}

#[cfg(not(unix))]
fn ctime_ns_of(_md: &std::fs::Metadata) -> i64 {
    0
}

/// File `(size, mtime_ns, ctime_ns)` for the freshness check, or `None` if not
/// a file.
fn file_stat_key(path: &std::path::Path) -> Option<(i64, i64, i64)> {
    let md = std::fs::metadata(path).ok()?;
    if !md.is_file() {
        return None;
    }
    Some((
        md.len() as i64,
        mtime_ns_of(&md).unwrap_or(0),
        ctime_ns_of(&md),
    ))
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
        "sh" | "bash" => "bash",
        "c" | "h" => "c",
        "cc" | "cpp" | "cxx" | "hpp" | "hh" => "cpp",
        "cs" => "csharp",
        "php" => "php",
        "rb" => "ruby",
        "lua" => "lua",
        "scala" | "sc" => "scala",
        "kt" | "kts" => "kotlin",
        "swift" => "swift",
        "dart" => "dart",
        "zig" => "zig",
        "ex" | "exs" => "elixir",
        "sol" => "solidity",
        "svelte" => "svelte",
        "vue" => "vue",
        "css" | "scss" => "css",
        "tf" | "tfvars" | "hcl" | "nomad" => "hcl",
        "ps1" | "psm1" | "psd1" => "powershell",
        "proto" => "proto",
        "graphql" | "gql" => "graphql",
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
