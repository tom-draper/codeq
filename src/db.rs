//! SQLite index store.
//!
//! The index lives at `.codeq/index.db` under the repo root. It is safe to
//! delete and should be git-ignored. WAL mode is enabled so that a reindex by
//! one process does not block reads by concurrent agent invocations.

use std::path::{Path, PathBuf};

use anyhow::{Context, Result};
use rusqlite::Connection;

pub const SCHEMA_VERSION: i64 = 2;

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

        conn.pragma_update(None, "journal_mode", "WAL")?;
        conn.pragma_update(None, "synchronous", "NORMAL")?;
        conn.pragma_update(None, "foreign_keys", "ON")?;

        let db = Db { conn, dir };
        db.migrate()?;
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
                FOREIGN KEY(path) REFERENCES files(path) ON DELETE CASCADE
            );

            CREATE INDEX IF NOT EXISTS files_language ON files(language);
            CREATE INDEX IF NOT EXISTS facts_kind_name ON facts(kind, name);
            CREATE INDEX IF NOT EXISTS facts_name ON facts(name);
            CREATE INDEX IF NOT EXISTS facts_path ON facts(path);
            "#,
        )?;

        // Full-text content index. `tokenchars '_'` keeps identifiers whole
        // (foo_bar is one token), which aligns FTS tokens with word boundaries.
        self.conn.execute_batch(
            "CREATE VIRTUAL TABLE IF NOT EXISTS content_fts USING fts5(
                 path UNINDEXED, language UNINDEXED, body,
                 tokenize = \"unicode61 tokenchars '_'\"
             );",
        )?;

        // Best-effort column additions for indexes created before these fields
        // existed. Errors (column already present) are expected and ignored.
        let _ = self
            .conn
            .execute("ALTER TABLE facts ADD COLUMN signature TEXT", []);
        let _ = self.conn.execute("ALTER TABLE facts ADD COLUMN doc TEXT", []);

        // On a schema version change, wipe derived data so the next refresh
        // rebuilds everything (including the FTS index) from scratch.
        let prev = self.get_meta("schema_version")?;
        if prev.as_deref() != Some(SCHEMA_VERSION.to_string().as_str()) {
            self.conn.execute("DELETE FROM files", [])?; // cascades to facts
            let _ = self.conn.execute("DELETE FROM content_fts", []);
            let _ = self.conn.execute("DELETE FROM meta WHERE key = 'fresh_token'", []);
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

    /// Whether the index has ever been built.
    #[allow(dead_code)]
    pub fn has_been_indexed(&self) -> bool {
        matches!(self.get_meta("last_indexed_at"), Ok(Some(_)))
    }
}
