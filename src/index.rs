//! Ignore-aware walk, hashing, and incremental refresh.
//!
//! This is the daemonless freshness path: every query calls [`refresh`], which
//! walks the tree, reparses only files whose size/mtime/ctime changed, and reports
//! how many files it had to touch. A no-op refresh on an unchanged tree writes
//! nothing.

use std::collections::{HashMap, HashSet};
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
    parse_errors: Option<i64>,
    parse_recovered: bool,
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
    /// Not binary, so its contents are searchable.
    is_text: bool,
    /// Body for the FTS index — `Some` for a changed text file, else `None`.
    body: Option<String>,
    facts: Vec<Fact>,
    spans: Vec<(usize, usize, Span)>,
    relations: Vec<Relation>,
    calls: Vec<crate::callsite::CallSite>,
    /// `None` when this format is deliberately not parsed (for example Markdown)
    /// or the file is binary.
    parse_errors: Option<i64>,
    parse_recovered: bool,
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
) -> Result<Option<Parsed>> {
    let Ok(mut bytes) = std::fs::read(abs) else {
        return Ok(None);
    };
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

    let (body, facts, spans, relations, calls, parse_errors, parse_recovered) =
        if content_changed && !is_binary {
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
            let ((f, s, r), health) = match language {
                Some(lang) => extractor.extract(lang, rel, text.as_bytes())?,
                None => ((Vec::new(), Vec::new(), Vec::new()), None),
            };
            let calls = language
                .and_then(|lang| crate::callsite::call_sites_in(lang, &text))
                .unwrap_or_default();
            let (parse_errors, parse_recovered) = health
                .map(|h| (Some(h.errors as i64), h.recovered))
                .unwrap_or((None, false));
            (Some(text), f, s, r, calls, parse_errors, parse_recovered)
        } else {
            let previous_health = prev.map(|p| (p.parse_errors, p.parse_recovered));
            (
                None,
                Vec::new(),
                Vec::new(),
                Vec::new(),
                Vec::new(),
                previous_health.and_then(|h| h.0),
                previous_health.is_some_and(|h| h.1),
            )
        };

    Ok(Some(Parsed {
        rel: rel.to_string(),
        size,
        mtime_ns,
        ctime_ns,
        hash,
        language,
        content_changed,
        was_indexed,
        is_text: !is_binary,
        body,
        facts,
        spans,
        relations,
        calls,
        parse_errors,
        parse_recovered,
    }))
}

/// A file the walk found changed (or new): relative path, absolute path, then
/// size, mtime and ctime.
type WorkItem = (String, PathBuf, i64, i64, i64);

/// What the serial directory walk saw.
struct Walked {
    /// Every file path present in the tree, to spot the ones that vanished.
    seen: HashSet<String>,
    /// Every walked directory's mtime, for the freshness cache.
    dirs_seen: HashMap<String, i64>,
    /// Files whose stat differs from the index, so need (re)processing.
    work: Vec<WorkItem>,
    partial_failures: u64,
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
    let existing = load_manifest(db)?;
    let now = now_ns();
    let Walked {
        seen,
        dirs_seen,
        work,
        mut partial_failures,
    } = walk_tree(repo, &existing);

    // BEGIN IMMEDIATE, not the default DEFERRED. A deferred transaction takes its
    // write lock lazily, on the first write — and SQLite refuses to make a reader
    // wait for that upgrade, returning SQLITE_BUSY *immediately* and bypassing the
    // busy timeout entirely, because two readers both upgrading would deadlock.
    // Taking the write lock up front is what lets the busy handler do its job, so
    // concurrent refreshes queue instead of failing.
    let tx =
        rusqlite::Transaction::new_unchecked(&db.conn, rusqlite::TransactionBehavior::Immediate)?;
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
    let reindexed = parse_and_write(&tx, &work, &existing, now, &parse_failures)?;
    partial_failures += parse_failures.load(Ordering::Relaxed);

    drop_vanished(&tx, &existing, &seen)?;
    replace_dirs(&tx, &dirs_seen)?;

    tx.commit()?;
    db.set_meta("last_indexed_at", &now_ns().to_string())?;
    // Only a refresh that wrote can have freed pages, so this is the one place
    // worth asking; the fresh fast path above never reaches it.
    db.compact_if_bloated();
    Ok(RefreshStats {
        reindexed,
        partial_failures,
    })
}

/// Every file row the index already holds, keyed by path.
fn load_manifest(db: &Db) -> Result<HashMap<String, Stored>> {
    let mut existing: HashMap<String, Stored> = HashMap::new();
    let mut stmt = db
        .conn
        .prepare("SELECT path, size_bytes, mtime_ns, ctime_ns, content_hash, parse_errors, parse_recovered FROM files")?;
    let rows = stmt.query_map([], |r| {
        Ok((
            r.get::<_, String>(0)?,
            Stored {
                size: r.get::<_, i64>(1)?,
                mtime_ns: r.get::<_, i64>(2).unwrap_or(0),
                ctime_ns: r.get::<_, i64>(3).unwrap_or(0),
                hash: r.get::<_, Option<String>>(4)?,
                parse_errors: r.get::<_, Option<i64>>(5)?,
                parse_recovered: r.get::<_, bool>(6).unwrap_or(false),
            },
        ))
    })?;
    for row in rows {
        let (path, stored) = row?;
        existing.insert(path, stored);
    }
    Ok(existing)
}

/// Serial walk (stat-bound): record directory mtimes for the freshness cache,
/// build the `seen` set, and collect the files that actually need
/// (re)processing — unchanged files short-circuit on the stat fast path.
fn walk_tree(repo: &Repo, existing: &HashMap<String, Stored>) -> Walked {
    let repo_root = repo.root.as_path();
    let mut seen: HashSet<String> = HashSet::new();
    let mut dirs_seen: HashMap<String, i64> = HashMap::new();
    let mut partial_failures: u64 = 0;
    let mut work: Vec<WorkItem> = Vec::new();
    let mut wb = WalkBuilder::new(repo_root);
    wb.standard_filters(true); // .gitignore, .ignore, hidden, parents
                               // `parents(false)` when the root was pinned with --root: a dependency
                               // directory is ignored *by its parent repo*, and consulting that would make
                               // every file in it invisible. Ignore files inside the root still apply.
    wb.parents(!repo.pinned);
    // `.codeqignore` — for trees that are *committed* but not code. Respecting
    // .gitignore is not enough on a data-heavy repo: an external user reported
    // ~2,100 files landing in the index as `(unknown)`, and every one of them was
    // tracked, so nothing gitignore-based could ever have excluded them. Takes
    // precedence over .gitignore (the `ignore` crate orders custom files highest).
    // Gitignore semantics otherwise, including the sharp edge: `!data/keep.json`
    // cannot resurrect a file under an excluded `data/`, because the walk never
    // descends — exclude `data/*.json` instead if you need to re-include one.
    wb.add_custom_ignore_filename(".codeqignore");
    let walker = wb.build();
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
            for ig in [".gitignore", ".ignore", ".codeqignore"] {
                if let Some(m) = std::fs::metadata(abs.join(ig))
                    .ok()
                    .as_ref()
                    .and_then(mtime_ns_of)
                {
                    let key = if rel.is_empty() {
                        ig.to_string()
                    } else {
                        format!("{rel}/{ig}")
                    };
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

        seen.insert(rel.clone());

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
    Walked {
        seen,
        dirs_seen,
        work,
        partial_failures,
    }
}

/// Parse the changed files in parallel (read + hash + tree-sitter extraction
/// is the CPU cost) and write the results serially on this thread — the
/// transaction never crosses threads, so index content stays deterministic
/// regardless of which worker finished first. A bounded channel bounds the
/// parsed bodies held in flight. Returns how many files had their content
/// rewritten; files that failed to parse are counted in `parse_failures`.
fn parse_and_write(
    tx: &rusqlite::Transaction,
    work: &[WorkItem],
    existing: &HashMap<String, Stored>,
    now: i64,
    parse_failures: &AtomicU64,
) -> Result<u64> {
    // Worker-local extractors compile only the languages they encounter.
    // A malformed query remains fatal, rolling back the refresh transaction.
    let workers = std::thread::available_parallelism()
        .map(|n| n.get())
        .unwrap_or(1)
        .clamp(1, 8)
        // Keep at least 32 files per worker to amortize parser/query setup.
        .min(work.len().div_ceil(32).max(1));
    let chunk = work.len().div_ceil(workers).max(1);
    let (res_tx, res_rx) = std::sync::mpsc::sync_channel::<Parsed>(256);
    std::thread::scope(|scope| -> Result<u64> {
        for c in work.chunks(chunk) {
            let sender = res_tx.clone();
            scope.spawn(move || {
                let mut ex = Extractor::default();
                for (rel, abs, size, mtime, ctime) in c {
                    match parse_one(existing, &mut ex, rel, abs, *size, *mtime, *ctime) {
                        Ok(Some(p)) => {
                            if sender.send(p).is_err() {
                                return; // writer went away (error) — stop
                            }
                        }
                        Ok(None) => {
                            parse_failures.fetch_add(1, Ordering::Relaxed);
                        }
                        Err(e) => {
                            eprintln!("codeq: fact extraction unavailable: {e:#}");
                            std::process::exit(1);
                        }
                    }
                }
            });
        }
        drop(res_tx); // close the channel once every worker's clone is gone

        let mut writer = RowWriter::new(tx, now)?;
        let mut reindexed: u64 = 0;
        for p in res_rx {
            if writer.write(&p)? {
                reindexed += 1;
            }
        }
        Ok(reindexed)
    })
}

/// The serial writer: owns the prepared statements that store one parsed file.
struct RowWriter<'tx> {
    now: i64,
    upsert: rusqlite::Statement<'tx>,
    del_facts: rusqlite::Statement<'tx>,
    ins_fact: rusqlite::Statement<'tx>,
    del_fts: rusqlite::Statement<'tx>,
    ins_fts: rusqlite::Statement<'tx>,
    del_spans: rusqlite::Statement<'tx>,
    ins_span: rusqlite::Statement<'tx>,
    del_impls: rusqlite::Statement<'tx>,
    ins_impl: rusqlite::Statement<'tx>,
    del_calls: rusqlite::Statement<'tx>,
    ins_call: rusqlite::Statement<'tx>,
}

impl<'tx> RowWriter<'tx> {
    fn new(tx: &'tx rusqlite::Transaction, now: i64) -> Result<Self> {
        Ok(RowWriter {
            now,
            upsert: tx.prepare(
                "INSERT INTO files(path, language, size_bytes, mtime_ns, ctime_ns, content_hash, indexed_at, parse_errors, parse_recovered, is_text)
                 VALUES(?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10)
                 ON CONFLICT(path) DO UPDATE SET
                    language = excluded.language,
                    size_bytes = excluded.size_bytes,
                    mtime_ns = excluded.mtime_ns,
                    ctime_ns = excluded.ctime_ns,
                    content_hash = excluded.content_hash,
                    indexed_at = excluded.indexed_at,
                    parse_errors = excluded.parse_errors,
                    parse_recovered = excluded.parse_recovered,
                    is_text = excluded.is_text
                 RETURNING id",
            )?,
            del_facts: tx.prepare("DELETE FROM facts WHERE path = ?1")?,
            ins_fact: tx.prepare(
                "INSERT OR REPLACE INTO facts(
                    id, kind, is_derived, name, path, language,
                    start_byte, end_byte, start_line, start_col, end_line, end_col,
                    signature, doc, source, confidence, visibility, owner, shape, shape_nodes,
                    is_test, decorator, contract)
                 VALUES(?1,?2,?3,?4,?5,?6,?7,?8,?9,?10,?11,?12,?13,?14,?15,?16,?17,?18,?19,?20,?21,?22,?23)",
            )?,
            del_fts: tx.prepare("DELETE FROM content_fts WHERE rowid = ?1")?,
            ins_fts: tx.prepare("INSERT INTO content_fts(rowid, body) VALUES(?1, ?2)")?,
            del_spans: tx.prepare("DELETE FROM spans WHERE file_id = ?1")?,
            // OR IGNORE: the span is the whole row, so a grammar that reports
            // one twice has said nothing new the second time.
            ins_span: tx.prepare(
                "INSERT OR IGNORE INTO spans(file_id, start_byte, end_byte, kind)
                 VALUES(?1, ?2, ?3, ?4)",
            )?,
            del_impls: tx.prepare("DELETE FROM impls WHERE path = ?1")?,
            ins_impl: tx.prepare(
                "INSERT INTO impls(path, line, subtype, supertype, relation, language)
                 VALUES(?1, ?2, ?3, ?4, ?5, ?6)",
            )?,
            del_calls: tx.prepare("DELETE FROM calls WHERE file_id = ?1")?,
            ins_call: tx.prepare(
                "INSERT INTO calls(file_id, start_byte, name, receiver) VALUES(?1, ?2, ?3, ?4)",
            )?,
        })
    }

    /// Store one parsed file. Returns whether its content was rewritten (false
    /// when only the stat columns changed).
    fn write(&mut self, p: &Parsed) -> Result<bool> {
        let file_id: i64 = self.upsert.query_row(
            (
                &p.rel,
                p.language,
                p.size,
                p.mtime_ns,
                p.ctime_ns,
                &p.hash,
                self.now,
                p.parse_errors,
                p.parse_recovered,
                p.is_text,
            ),
            |r| r.get(0),
        )?;
        if !p.content_changed {
            return Ok(false);
        }
        // Only a previously-indexed file has stale rows to drop; a new file
        // (every file on a cold index) has none.
        if p.was_indexed {
            self.del_fts.execute([file_id])?;
            self.del_facts.execute([&p.rel])?;
            self.del_spans.execute([file_id])?;
            self.del_impls.execute([&p.rel])?;
            self.del_calls.execute([file_id])?;
        }
        if let Some(body) = &p.body {
            // Every text file's trigrams go in, whatever its size. A cap
            // was tried and reverted: it made a data-heavy repo's index
            // 200x smaller, and it also made `search` stop being a strict
            // superset of ripgrep — the differential harness caught 20,000
            // lines of a 6.6 MB test fixture that `rg` finds and codeq no
            // longer would. That guarantee is the loudest thing this tool
            // claims, and no size threshold separates "somebody's test
            // fixture" from "committed geodata" on principle rather than
            // by fitting two data points. The size was fixed by storing
            // less per file instead: only the trigram set is kept, not the
            // text. `.codeqignore` is still the answer for data that should
            // not be searched at all.
            self.ins_fts.execute((file_id, body.as_str()))?;
        }
        if let Some(lang) = p.language {
            for f in &p.facts {
                // `params!` rather than a tuple: rusqlite only implements
                // `Params` for tuples up to 16, and this is 22 columns.
                self.ins_fact.execute(rusqlite::params![
                    f.id(&p.rel),
                    &f.kind,
                    f.is_derived,
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
                    &f.shape,
                    f.shape_nodes,
                    f.is_test,
                    &f.decorator,
                    f.contract.as_deref(),
                ])?;
            }
            for (sb, eb, sp) in &p.spans {
                self.ins_span
                    .execute((file_id, *sb as i64, *eb as i64, sp.as_i64()))?;
            }
            for r in &p.relations {
                self.ins_impl.execute((
                    &p.rel,
                    r.start_line as i64,
                    &r.subtype,
                    &r.supertype,
                    r.relation,
                    lang,
                ))?;
            }
            for call in &p.calls {
                self.ins_call.execute((
                    file_id,
                    call.start_byte as i64,
                    &call.name,
                    &call.receiver,
                ))?;
            }
        }
        Ok(true)
    }
}

/// Drop files that vanished from the tree.
fn drop_vanished(
    tx: &rusqlite::Transaction,
    existing: &HashMap<String, Stored>,
    seen: &HashSet<String>,
) -> Result<()> {
    let mut del = tx.prepare("DELETE FROM files WHERE path = ?1")?;
    // FTS is not FK-linked to files, so remove it explicitly — and first, while
    // the row that maps the path to its id is still there.
    let mut del_fts =
        tx.prepare("DELETE FROM content_fts WHERE rowid = (SELECT id FROM files WHERE path = ?1)")?;
    let gone: Vec<&String> = existing.keys().filter(|p| !seen.contains(*p)).collect();
    for p in gone {
        del_fts.execute([p])?;
        del.execute([p])?;
    }
    Ok(())
}

/// Replace the directory-mtime cache with the tree we just walked.
fn replace_dirs(tx: &rusqlite::Transaction, dirs_seen: &HashMap<String, i64>) -> Result<()> {
    tx.execute("DELETE FROM dirs", [])?;
    let mut ins_dir = tx.prepare("INSERT INTO dirs(path, mtime_ns) VALUES(?1, ?2)")?;
    for (p, m) in dirs_seen {
        ins_dir.execute((p, m))?;
    }
    Ok(())
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

/// The text of an indexed file, as the indexer saw it: leading BOM stripped,
/// then decoded as UTF-8 with invalid bytes replaced. Every stored offset —
/// fact extents, spans, call sites — is a position in *this* text, not in the
/// bytes on disk, so anything that slices by one must read through here.
/// `None` when the file cannot be read, which a caller treats as no matches:
/// it vanished since the refresh this query began with.
pub(crate) fn load_body(root: &Path, rel: &str) -> Option<String> {
    let bytes = std::fs::read(root.join(rel)).ok()?;
    let bytes = bytes.strip_prefix(&[0xEF, 0xBB, 0xBF]).unwrap_or(&bytes);
    Some(String::from_utf8_lossy(bytes).into_owned())
}

pub(crate) fn looks_binary(bytes: &[u8]) -> bool {
    let n = bytes.len().min(8192);
    bytes[..n].contains(&0)
}

/// File extension → language, as a table rather than a `match` so it can be
/// enumerated. `facts::LANGUAGES` (what `capabilities` reports) is pinned
/// against this by a test: adding an extension here without deciding whether
/// the language extracts fails the build rather than quietly shipping a
/// language that `capabilities` calls unsupported.
pub const EXTENSIONS: &[(&str, &str)] = &[
    ("rs", "rust"),
    ("js", "javascript"),
    ("cjs", "javascript"),
    ("mjs", "javascript"),
    ("jsx", "javascript"),
    ("ts", "typescript"),
    ("cts", "typescript"),
    ("mts", "typescript"),
    ("tsx", "tsx"),
    ("py", "python"),
    ("pyi", "python"),
    ("go", "go"),
    ("java", "java"),
    ("json", "json"),
    ("yaml", "yaml"),
    ("yml", "yaml"),
    ("toml", "toml"),
    ("md", "markdown"),
    ("markdown", "markdown"),
    ("rst", "rst"),
    ("html", "jinja"),
    ("jinja", "jinja"),
    ("jinja2", "jinja"),
    ("j2", "jinja"),
    ("sql", "sql"),
    ("sh", "bash"),
    ("bash", "bash"),
    ("c", "c"),
    ("h", "c"),
    ("cc", "cpp"),
    ("cpp", "cpp"),
    ("cxx", "cpp"),
    ("hpp", "cpp"),
    ("hh", "cpp"),
    ("cs", "csharp"),
    ("php", "php"),
    ("rb", "ruby"),
    ("lua", "lua"),
    ("scala", "scala"),
    ("sc", "scala"),
    ("kt", "kotlin"),
    ("kts", "kotlin"),
    ("swift", "swift"),
    ("dart", "dart"),
    ("zig", "zig"),
    ("ex", "elixir"),
    ("exs", "elixir"),
    ("sol", "solidity"),
    ("svelte", "svelte"),
    ("vue", "vue"),
    ("css", "css"),
    ("scss", "css"),
    ("tf", "hcl"),
    ("tfvars", "hcl"),
    ("hcl", "hcl"),
    ("nomad", "hcl"),
    ("ps1", "powershell"),
    ("psm1", "powershell"),
    ("psd1", "powershell"),
    ("proto", "proto"),
    ("graphql", "graphql"),
    ("gql", "graphql"),
];

/// Languages codeq indexes as files (so they are searchable) but does not
/// extract definitions from — no grammar is wired up for them. Kept explicit so
/// the drift test can tell "deliberately data-only" from "someone forgot".
pub const NO_EXTRACTION: &[&str] = &["json", "yaml", "toml"];

/// Extension-based language classification. Deliberately syntactic and cheap;
/// this is not content sniffing.
pub fn language_for(path: &str) -> Option<&'static str> {
    let ext = path.rsplit('.').next()?;
    EXTENSIONS
        .iter()
        .find(|(e, _)| e.eq_ignore_ascii_case(ext))
        .map(|(_, l)| *l)
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

#[cfg(test)]
mod tests {
    use super::*;

    /// Every language a file can be classified as must be either extractable
    /// (and therefore reported by `capabilities`) or explicitly data-only.
    ///
    /// This is the check that was missing: seven languages shipped with working
    /// extraction while `capabilities` listed them as unsupported, and since
    /// codeq tells agents to trust that matrix before concluding a language
    /// isn't handled, the gap actively misled them.
    #[test]
    fn every_classified_language_is_documented() {
        for (ext, lang) in EXTENSIONS {
            if NO_EXTRACTION.contains(lang) {
                assert!(
                    crate::facts::support(lang).is_none(),
                    ".{ext} is listed as data-only but `support({lang})` describes it"
                );
                continue;
            }
            assert!(
                crate::facts::LANGUAGES.contains(lang),
                ".{ext} maps to '{lang}', which `capabilities` does not list — add it \
                 to facts::LANGUAGES, or to index::NO_EXTRACTION if it is data-only"
            );
            assert!(
                crate::facts::support(lang).is_some(),
                "'{lang}' is in facts::LANGUAGES but `support` cannot describe it"
            );
        }
    }

    /// The other direction: nothing is advertised that no file can ever be.
    #[test]
    fn every_documented_language_is_reachable() {
        for lang in crate::facts::LANGUAGES {
            assert!(
                EXTENSIONS.iter().any(|(_, l)| l == lang),
                "`capabilities` lists '{lang}' but no extension maps to it"
            );
        }
    }

    #[test]
    fn language_detection_accepts_uppercase_extensions() {
        assert_eq!(language_for("Component.TSX"), Some("tsx"));
        assert_eq!(language_for("SCRIPT.PY"), Some("python"));
    }
}
