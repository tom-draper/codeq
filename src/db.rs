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
// v14-v21 below are a *merge* of two parallel lines that both bumped from v13,
// so the same numbers meant different migrations on each side. They are recorded
// together for history only — no index can have followed both sequences, and the
// numbers are not individually meaningful any more. What matters is v22: it is
// above every version either line issued, so every existing index rebuilds once
// and lands on the union of both feature sets. The version is a cache-invalidation
// token, not a schema identity, which is what makes this safe to collapse.
//
// From the component/clones line:
// v14: added Svelte extraction (nested `<script>` facts, component facts).
// v15: added Vue extraction.
// v16: freshness also compares inode ctime, so a size- and mtime-preserving edit
// is no longer invisible.
// v18: added CSS/PowerShell/Protobuf/GraphQL extraction, and members for six more
// languages.
// v19: added HCL/Terraform extraction.
// v20: members for Zig/Solidity/Dart/Scala, and GraphQL type relations.
// v21: definitions carry a syntax-shape hash, for structural clone detection.
//
// From the markdown/relations line:
// v15: added markdown extraction (`heading` facts, code-fence/prose spans, and
// local link/image targets as imports).
// v16: relations for Scala/Kotlin/Swift/Dart/Solidity, imports for Zig/Elixir/
// Lua/Bash, and Ruby attr_* fields.
// v17: functions declared inside a type body are now `method`, not `function`.
//
// Both lines independently added member extraction (`field`/`variant` facts with
// their owning type) — mine at v17, theirs at v14.
//
// v22: the merge of both lines — rebuild once to populate everything above.
// v23: JS/TS module-scope bindings are definitions (a React component wrapped in
// `memo`/`forwardRef`, a `styled.div`, an exported `const`), and a namespaced
// base class (`extends React.Component`) now yields a relation edge — rebuild to
// populate both.
// v24: SQL is extracted rather than treated as a data format — tables, views,
// indexes, triggers, functions, types, schemas and columns. Existing indexes
// hold `.sql` files parsed under the old rule (searchable, no facts), and
// nothing about those files has changed on disk for the freshness check to
// notice, so only a version bump reparses them.
// v25: JS/TS property-assignment methods (`proto.use = function use(){}`,
// `Thing.prototype.render = …`, `exports.helper = …`) are definitions. Same
// reason as v24: the files have not changed on disk, so only a version bump
// reparses them — an extraction change without one ships to nobody.
// v26: SQL foreign keys are relation edges (`references`) — again, existing
// `.sql` files are unchanged on disk, so only a bump reparses them.
// v27: definitions record whether a test *attribute* marks them, so `tests`,
// `unused` and `untested` stop relying on the name and the path alone.
// v28: definitions record a registering decorator (`@app.get`), so `unused` can
// tag a framework entry point instead of reporting it as dead code.
// v29: Python module-scope bindings are definitions, and a member of a private
// type is private — both change what is extracted, so existing indexes must
// reparse to see it.
// v30: Python `__all__` and friends are private (metadata the interpreter reads,
// not surface), and a folded multi-line constant's signature carries its shape.
// v31: syntax-derived callsites are cached for structural navigation commands.
// v32: import facts retain their local alias binding in the otherwise-empty
// signature field, so qualified/bare alias calls can resolve to direct imports.
// v33: Svelte/Vue script blocks contribute syntax-derived calls at component
// file offsets; cached component call rows must be rebuilt.
// v34: indexed files retain parser recovery diagnostics, so agents can detect
// partial syntax extraction instead of treating every index row as equally sound.
// v35: facts record the trait a definition implements, so `unused` can tell a
// trait method (never dead — deleting it breaks the build) from dead code.
// v38: derived import facts are marked so user-facing import listings and
// counts report source statements while resolution retains named bindings.
// v39: Python dotted from-imports resolve to the deepest matching module, and
// SQL field facts capture and normalize quoted column identifiers.
// v40: Rust/C# escaped identifiers use semantic names in facts and callsites;
// SQL's doubled and bracketed identifier escapes are parsed without recovery.
// v41: clone shapes include anonymous syntax tokens such as operators, so cached
// fingerprints must be regenerated.
//
// **This is an extraction version, not just a storage version.** Freshness is
// content hash plus this number, so changing *how* a fact is extracted — a
// tree-sitter query, a visibility rule, an attribute reader — leaves every
// existing index serving the old answer indefinitely, with nothing to indicate
// it. That has already cost one debugging session: a fixed decorator rule
// appeared to do nothing until `codeq index --rebuild`. Bump this whenever
// extraction changes, even when no column does.
//
// A mechanical version of that check would be better than a remembered one, and
// hashing the query constants is the obvious form — but it would cover only the
// query-driven half. A visibility rule or an attribute reader is ordinary Rust
// and would slip through, while the mechanism looked complete. Not yet solved.
pub const SCHEMA_VERSION: i64 = 42;

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
        std::fs::create_dir_all(&dir).with_context(|| format!("creating {}", dir.display()))?;
        // Make git ignore our whole index dir, so it never shows up as untracked
        // (which would also destabilise the freshness token).
        let gitignore = dir.join(".gitignore");
        if !gitignore.exists() {
            let _ = std::fs::write(&gitignore, "*\n");
        }
        let path = dir.join("index.db");
        let conn =
            Connection::open(&path).with_context(|| format!("opening {}", path.display()))?;

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
                -- Count of parser ERROR nodes in the tree used for extraction.
                -- NULL means this format is intentionally not parser-backed.
                parse_errors INTEGER,
                -- The extractor selected a safer recovery parse (currently Rust).
                parse_recovered INTEGER NOT NULL DEFAULT 0,
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
                -- Supporting facts used internally but not shown as source
                -- declarations (currently qualified named imports).
                is_derived INTEGER NOT NULL DEFAULT 0,
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
                -- Hash of the definition's syntax shape (identifiers and
                -- literals normalised away) and how many nodes it covers.
                shape       TEXT,
                shape_nodes INTEGER,
                -- The definition carries a test attribute (`#[test]`,
                -- `@pytest.mark.…`, `@Test`, `[Fact]`) — a structural fact, as
                -- against the path/name guesses test recognition used before.
                is_test     INTEGER NOT NULL DEFAULT 0,
                -- A decorator that may register the definition somewhere codeq
                -- cannot see (`@app.get("/x")`), so `unused` can tag rather than
                -- silently report a framework entry point as dead.
                decorator   TEXT,
                -- The trait this definition implements, when it exists to
                -- satisfy one. A Rust `impl Trait for X` method is called
                -- through the trait and named by nothing, so `unused` reported
                -- every actix handler and `Display::fmt` as dead code.
                contract    TEXT,
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

            -- Syntax-derived invocations. `name` is the final called identifier;
            -- `receiver` preserves how a member call was written for later
            -- import-aware resolution without reparsing the source at query time.
            CREATE TABLE IF NOT EXISTS calls (
                path       TEXT NOT NULL,
                start_byte INTEGER NOT NULL,
                name       TEXT NOT NULL,
                receiver   TEXT,
                PRIMARY KEY(path, start_byte),
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
            CREATE INDEX IF NOT EXISTS calls_name ON calls(name);
            CREATE INDEX IF NOT EXISTS calls_path ON calls(path);
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
        let _ = self
            .conn
            .execute("ALTER TABLE facts ADD COLUMN doc TEXT", []);
        let _ = self
            .conn
            .execute("ALTER TABLE facts ADD COLUMN visibility TEXT", []);
        let _ = self
            .conn
            .execute("ALTER TABLE files ADD COLUMN ctime_ns INTEGER", []);
        let _ = self
            .conn
            .execute("ALTER TABLE files ADD COLUMN parse_errors INTEGER", []);
        let _ = self.conn.execute(
            "ALTER TABLE files ADD COLUMN parse_recovered INTEGER NOT NULL DEFAULT 0",
            [],
        );
        let _ = self
            .conn
            .execute("ALTER TABLE facts ADD COLUMN owner TEXT", []);
        let _ = self
            .conn
            .execute("ALTER TABLE facts ADD COLUMN shape TEXT", []);
        let _ = self
            .conn
            .execute("ALTER TABLE facts ADD COLUMN shape_nodes INTEGER", []);
        let _ = self.conn.execute(
            "ALTER TABLE facts ADD COLUMN is_test INTEGER NOT NULL DEFAULT 0",
            [],
        );
        let _ = self
            .conn
            .execute("ALTER TABLE facts ADD COLUMN decorator TEXT", []);
        let _ = self
            .conn
            .execute("ALTER TABLE facts ADD COLUMN contract TEXT", []);
        let _ = self.conn.execute(
            "ALTER TABLE facts ADD COLUMN is_derived INTEGER NOT NULL DEFAULT 0",
            [],
        );

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
