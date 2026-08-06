//! SQLite index store.
//!
//! The index lives at `.codeq/index.db` under the repo root. It is safe to
//! delete and should be git-ignored. WAL mode is enabled so that a reindex by
//! one process does not block reads by concurrent agent invocations.

use std::path::{Path, PathBuf};

use anyhow::{Context, Result};
use rusqlite::Connection;

// v5: index strips a leading UTF-8 BOM, changing stored bodies/offsets for
// BOM-prefixed files — bump so pre-fix indexes rebuild on upgrade.
// v6: git-free freshness via a native dir-mtime cache (the `dirs` table); old
// indexes lack it, so rebuild to populate it.
// v12: added Bash/Lua/Scala/Kotlin/Swift extraction (and remapped .sh from the
// non-extracted "shell" label to "bash"), so existing indexes must rebuild to
// populate facts/spans for these files.
// v13: added Dart/Zig/Elixir/Solidity extraction — rebuild to populate them.
// v14: added Svelte extraction (nested `<script>` facts, component facts) —
// rebuild to populate them.
// v15: added Vue extraction — rebuild to populate them.
// v16: freshness also compares inode ctime, so a size- and mtime-preserving edit
// is no longer invisible; old rows have no ctime to compare, so rebuild.
// v17: struct fields / class properties / enum variants are extracted as
// `field` and `variant` facts carrying their owning type — rebuild to populate.
// v18: added CSS/PowerShell/Protobuf/GraphQL extraction, and members for six more
// languages — rebuild to populate them.
// v19: added HCL/Terraform extraction — rebuild to populate it.
pub const SCHEMA_VERSION: i64 = 19;

pub struct Db {
    pub conn: Connection,
    /// The `.codeq` directory; reserved for status/cleanup commands.
    #[allow(dead_code)]
    pub dir: PathBuf,
}

impl Db {
    /// Open (creating if needed) the index for `repo_root`.
    pub fn open(repo_root: &Path) -> Result<Db> {
        let dir = repo_root.join(".codeq");
        std::fs::create_dir_all(&dir)
            .with_context(|| format!("creating {}", dir.display()))?;
        // Make git ignore our whole index dir, so it never shows up as untracked
        // (which would also destabilise the freshness token).
        let gitignore = dir.join(".gitignore");
        if !gitignore.exists() {
            let _ = std::fs::write(&gitignore, "*\n");
        }
        let path = dir.join("index.db");
        let conn = Connection::open(&path)
            .with_context(|| format!("opening {}", path.display()))?;

        // Must come first: without it SQLite gives up the instant it meets a
        // lock, and every pragma below can contend. Agents run codeq
        // concurrently — several tool calls at once, or several agents on one
        // checkout — and each invocation refreshes, so contention is the normal
        // case, not an edge one. Waiting briefly is right where failing is not:
        // the work being waited on is another process finishing the very refresh
        // this one would otherwise do.
        conn.busy_timeout(std::time::Duration::from_secs(15))?;
        // `PRAGMA journal_mode = WAL` takes an exclusive lock even when the
        // database is already in WAL, so setting it unconditionally made every
        // concurrent *reader* contend on open for no reason. Read it first and
        // only write when it differs.
        let mode: String = conn
            .query_row("PRAGMA journal_mode", [], |r| r.get(0))
            .unwrap_or_default();
        if !mode.eq_ignore_ascii_case("wal") {
            conn.pragma_update(None, "journal_mode", "WAL")?;
        }
        conn.pragma_update(None, "synchronous", "NORMAL")?;
        conn.pragma_update(None, "foreign_keys", "ON")?;

        let db = Db { conn, dir };
        // The DDL batch (CREATE TABLE/INDEX/VIRTUAL TABLE + best-effort ALTERs)
        // only needs to run when the schema is missing or out of date. On the
        // common path it's already current, so skip it — re-parsing ~a dozen
        // statements on every command is wasted latency.
        let stored = db
            .conn
            .query_row(
                "SELECT value FROM meta WHERE key = 'schema_version'",
                [],
                |r| r.get::<_, String>(0),
            )
            .ok();
        if stored.as_deref() != Some(SCHEMA_VERSION.to_string().as_str()) {
            db.migrate()?;
        }
        Ok(db)
    }

    fn migrate(&self) -> Result<()> {
        self.conn.execute_batch(
            r#"
            CREATE TABLE IF NOT EXISTS files (
                path         TEXT PRIMARY KEY,
                language     TEXT,
                size_bytes   INTEGER NOT NULL,
                mtime_ns     INTEGER,
                -- Inode change time. mtime is settable (`touch -d`, `cp -p`,
                -- `rsync -a`, tar extraction), so size+mtime alone can miss a
                -- real edit; ctime moves on any write and cannot be forged from
                -- userspace, which closes that hole.
                ctime_ns     INTEGER,
                content_hash TEXT,
                indexed_at   INTEGER NOT NULL
            );

            CREATE TABLE IF NOT EXISTS meta (
                key   TEXT PRIMARY KEY,
                value TEXT NOT NULL
            );

            CREATE TABLE IF NOT EXISTS facts (
                id         TEXT PRIMARY KEY,
                kind       TEXT NOT NULL,
                name       TEXT,
                path       TEXT NOT NULL,
                language   TEXT,
                start_byte INTEGER NOT NULL,
                end_byte   INTEGER NOT NULL,
                start_line INTEGER NOT NULL,
                start_col  INTEGER NOT NULL,
                end_line   INTEGER NOT NULL,
                end_col    INTEGER NOT NULL,
                signature  TEXT,
                doc        TEXT,
                source     TEXT NOT NULL,
                confidence REAL NOT NULL,
                visibility TEXT,
                -- The type a `field`/`variant` belongs to; NULL otherwise.
                owner      TEXT,
                FOREIGN KEY(path) REFERENCES files(path) ON DELETE CASCADE
            );

            CREATE TABLE IF NOT EXISTS spans (
                path       TEXT NOT NULL,
                start_byte INTEGER NOT NULL,
                end_byte   INTEGER NOT NULL,
                kind       INTEGER NOT NULL,
                FOREIGN KEY(path) REFERENCES files(path) ON DELETE CASCADE
            );

            -- Type relations: `subtype` implements/extends `supertype`
            -- (impl/extends/implements edges the call graph doesn't cover).
            CREATE TABLE IF NOT EXISTS impls (
                path      TEXT NOT NULL,
                line      INTEGER NOT NULL,
                subtype   TEXT NOT NULL,
                supertype TEXT NOT NULL,
                relation  TEXT NOT NULL,
                language  TEXT,
                FOREIGN KEY(path) REFERENCES files(path) ON DELETE CASCADE
            );

            -- Directory mtimes: codeq's own index cache for cheap, git-free
            -- freshness. A file added/removed/renamed bumps its parent dir's
            -- mtime, so checking these detects structural changes without a walk.
            CREATE TABLE IF NOT EXISTS dirs (
                path     TEXT PRIMARY KEY,
                mtime_ns INTEGER NOT NULL
            );

            CREATE INDEX IF NOT EXISTS files_language ON files(language);
            CREATE INDEX IF NOT EXISTS facts_kind_name ON facts(kind, name);
            CREATE INDEX IF NOT EXISTS facts_name ON facts(name);
            CREATE INDEX IF NOT EXISTS facts_path ON facts(path);
            CREATE INDEX IF NOT EXISTS spans_path ON spans(path);
            CREATE INDEX IF NOT EXISTS impls_supertype ON impls(supertype);
            CREATE INDEX IF NOT EXISTS impls_subtype ON impls(subtype);
            CREATE INDEX IF NOT EXISTS impls_path ON impls(path);
            "#,
        )?;

        // Full-text content index using the trigram tokenizer, which lets
        // SQLite accelerate `LIKE '%literal%'` on any substring of >= 3 chars.
        // We prefilter with LIKE (not FTS `MATCH`, which silently drops matches
        // for many substrings) by a regex's required literals (Zoekt/csearch
        // style), not just token/word queries. Case-insensitive so it's a safe
        // superset for both case-sensitive and -i searches (the real matcher
        // confirms).
        self.conn.execute_batch(
            "CREATE VIRTUAL TABLE IF NOT EXISTS content_fts USING fts5(
                 path UNINDEXED, language UNINDEXED, body,
                 tokenize = 'trigram case_sensitive 0'
             );",
        )?;

        // Best-effort column additions for indexes created before these fields
        // existed. Errors (column already present) are expected and ignored.
        let _ = self
            .conn
            .execute("ALTER TABLE facts ADD COLUMN signature TEXT", []);
        let _ = self.conn.execute("ALTER TABLE facts ADD COLUMN doc TEXT", []);
        let _ = self
            .conn
            .execute("ALTER TABLE facts ADD COLUMN visibility TEXT", []);
        let _ = self
            .conn
            .execute("ALTER TABLE files ADD COLUMN ctime_ns INTEGER", []);
        let _ = self
            .conn
            .execute("ALTER TABLE facts ADD COLUMN owner TEXT", []);

        // On a schema version change, wipe derived data so the next refresh
        // rebuilds everything (including the FTS index) from scratch.
        let prev = self.get_meta("schema_version")?;
        if prev.as_deref() != Some(SCHEMA_VERSION.to_string().as_str()) {
            self.conn.execute("DELETE FROM files", [])?; // cascades to facts
            let _ = self.conn.execute("DELETE FROM content_fts", []);
            // Clear the dir cache too: an empty `files` table against a populated
            // `dirs` cache would look "fresh" and skip the rebuild walk.
            let _ = self.conn.execute("DELETE FROM dirs", []);
            let _ = self.set_meta("last_indexed_at", "0");
        }
        self.set_meta("schema_version", &SCHEMA_VERSION.to_string())?;
        Ok(())
    }

    pub fn set_meta(&self, key: &str, value: &str) -> Result<()> {
        self.conn.execute(
            "INSERT INTO meta(key, value) VALUES(?1, ?2)
             ON CONFLICT(key) DO UPDATE SET value = excluded.value",
            (key, value),
        )?;
        Ok(())
    }

    pub fn get_meta(&self, key: &str) -> Result<Option<String>> {
        let v = self
            .conn
            .query_row("SELECT value FROM meta WHERE key = ?1", [key], |r| {
                r.get::<_, String>(0)
            })
            .ok();
        Ok(v)
    }
}
