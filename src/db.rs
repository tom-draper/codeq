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
// `tests/languages.rs` checks this mechanically: it records every fact extracted
// from one fixture per language, with the version it was recorded at, and fails
// when the facts change while this number does not. Hashing the query constants
// was the obvious form and would have covered only the query-driven half; a
// visibility rule or an attribute reader is ordinary Rust. Comparing output
// covers both, but only as far as the fixtures reach, so a rule no fixture
// exercises still depends on remembering.
// v46 records Rust impl owners for qualifier-aware callee selection.
// v47 stops tagging wrapper decorators (`pass_context`, `setupmethod`) as registrations.
// v48 records Bash commands and SQL trigger functions as call sites.
pub const SCHEMA_VERSION: i64 = 48;

/// Below this many free pages (1 MiB at the default page size) compaction is
/// not worth a rewrite, whatever share of a small index they are.
const COMPACT_MIN_FREE_PAGES: i64 = 256;

pub struct Db {
    pub conn: Connection,
    /// The `.codeq` directory; reserved for status/cleanup commands.
    #[allow(dead_code)]
    pub dir: PathBuf,
    /// The schema version of the index this open replaced, when it found one
    /// written by a different codeq. Everything derived was dropped and is
    /// about to be rebuilt, which the caller should say rather than leave an
    /// upgrade looking like an unexplained slow query.
    pub rebuilt_from: Option<String>,
}

/// How long a query waits for another process's index write before giving up.
///
/// Fifteen seconds covers a refresh after any ordinary edit. A cold build of a
/// very large repository can hold the write lock for longer, and a query that
/// arrives during one would rather wait than fail, so `CODEQ_BUSY_TIMEOUT_MS`
/// overrides it.
pub fn busy_timeout() -> std::time::Duration {
    let ms = std::env::var("CODEQ_BUSY_TIMEOUT_MS")
        .ok()
        .and_then(|v| v.parse().ok())
        .unwrap_or(15_000);
    std::time::Duration::from_millis(ms)
}

/// Whether `err` is SQLite giving up on a lock another process held past the
/// busy timeout, as against anything wrong with the index itself.
pub fn is_busy(err: &anyhow::Error) -> bool {
    err.chain().any(|cause| {
        matches!(
            cause.downcast_ref::<rusqlite::Error>(),
            Some(rusqlite::Error::SqliteFailure(e, _))
                if matches!(
                    e.code,
                    rusqlite::ErrorCode::DatabaseBusy | rusqlite::ErrorCode::DatabaseLocked
                )
        )
    })
}

/// Switch a new database to WAL, waiting out a competing switch.
///
/// Changing the journal mode needs the database to itself, and the busy
/// timeout does not reliably cover it: on a cold start several processes all
/// find a non-WAL database together, and a loser could get `SQLITE_BUSY` back
/// and fail with "database is locked" — about one invocation in a hundred
/// when twelve began at the same moment, none in 1,200 with the retry. The
/// switch is idempotent, so retry until someone has done it.
fn set_wal(conn: &Connection) -> Result<()> {
    let deadline = std::time::Instant::now() + busy_timeout();
    loop {
        let busy = match conn.pragma_update(None, "journal_mode", "WAL") {
            Ok(()) => return Ok(()),
            Err(rusqlite::Error::SqliteFailure(e, _))
                if e.code == rusqlite::ErrorCode::DatabaseBusy =>
            {
                e
            }
            Err(e) => return Err(e.into()),
        };
        if std::time::Instant::now() >= deadline {
            return Err(rusqlite::Error::SqliteFailure(busy, None).into());
        }
        std::thread::sleep(std::time::Duration::from_millis(5));
    }
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
        conn.busy_timeout(busy_timeout())?;
        // `PRAGMA journal_mode = WAL` takes an exclusive lock even when the
        // database is already in WAL, so setting it unconditionally made every
        // concurrent *reader* contend on open for no reason. Read it first and
        // only write when it differs.
        let mode: String = conn
            .query_row("PRAGMA journal_mode", [], |r| r.get(0))
            .unwrap_or_default();
        if !mode.eq_ignore_ascii_case("wal") {
            set_wal(&conn)?;
        }
        conn.pragma_update(None, "synchronous", "NORMAL")?;
        conn.pragma_update(None, "foreign_keys", "ON")?;

        let mut db = Db {
            conn,
            dir,
            rebuilt_from: None,
        };
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
            db.rebuilt_from = db.migrate()?;
        }
        Ok(db)
    }

    /// Returns the version it replaced: `None` on a first build, or when
    /// another process migrated while this one waited.
    fn migrate(&self) -> Result<Option<String>> {
        // One migration at a time, and whoever waited re-checks. A cold start
        // is routinely several processes at once, all of which saw no schema;
        // unserialised, the second would drop the tables the first had just
        // built while a third was already querying them.
        let tx = rusqlite::Transaction::new_unchecked(
            &self.conn,
            rusqlite::TransactionBehavior::Immediate,
        )?;
        self.conn.execute_batch(
            "CREATE TABLE IF NOT EXISTS meta (
                key   TEXT PRIMARY KEY,
                value TEXT NOT NULL
            );",
        )?;
        let prev = self.get_meta("schema_version")?;
        if prev.as_deref() == Some(SCHEMA_VERSION.to_string().as_str()) {
            tx.commit()?;
            return Ok(None);
        }
        // Everything but `meta` is derived, and a version change rebuilds it
        // all — so drop the old tables rather than empty them. `CREATE … IF
        // NOT EXISTS` would otherwise keep a table's previous shape, and a
        // drop hands the pages back at once where FTS5's row-by-row delete
        // writes tombstones first. Children before `files`, which they
        // reference.
        self.conn.execute_batch(
            "DROP TABLE IF EXISTS content_fts;
             DROP TABLE IF EXISTS facts;
             DROP TABLE IF EXISTS spans;
             DROP TABLE IF EXISTS impls;
             DROP TABLE IF EXISTS calls;
             DROP TABLE IF EXISTS dirs;
             DROP TABLE IF EXISTS files;",
        )?;
        self.set_meta("last_indexed_at", "0")?;

        self.conn.execute_batch(
            r#"
            CREATE TABLE IF NOT EXISTS files (
                -- Explicit, so it is stable: an implicit rowid may be
                -- renumbered by the VACUUM that compaction runs, and `spans`
                -- and `calls` key on it. A path is repeated in every row of a
                -- table that stores it and again in each of that table's
                -- indexes; an integer is not.
                id           INTEGER PRIMARY KEY,
                path         TEXT NOT NULL UNIQUE,
                -- Text, so searchable: its trigrams are in `content_fts` under
                -- this row's id. A file with a NUL in its first 8 KiB is not.
                is_text      INTEGER NOT NULL DEFAULT 0,
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

            -- Clustered on the file, so a file's spans are one range scan
            -- already in `start_byte` order, with no separate index to keep.
            CREATE TABLE IF NOT EXISTS spans (
                file_id    INTEGER NOT NULL,
                start_byte INTEGER NOT NULL,
                end_byte   INTEGER NOT NULL,
                kind       INTEGER NOT NULL,
                PRIMARY KEY(file_id, start_byte, end_byte, kind),
                FOREIGN KEY(file_id) REFERENCES files(id) ON DELETE CASCADE
            ) WITHOUT ROWID;

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
                file_id    INTEGER NOT NULL,
                start_byte INTEGER NOT NULL,
                name       TEXT NOT NULL,
                receiver   TEXT,
                PRIMARY KEY(file_id, start_byte),
                FOREIGN KEY(file_id) REFERENCES files(id) ON DELETE CASCADE
            ) WITHOUT ROWID;

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
            CREATE INDEX IF NOT EXISTS impls_supertype ON impls(supertype);
            CREATE INDEX IF NOT EXISTS impls_subtype ON impls(subtype);
            CREATE INDEX IF NOT EXISTS impls_path ON impls(path);
            CREATE INDEX IF NOT EXISTS calls_name ON calls(name);
            "#,
        )?;

        self.create_fts()?;

        self.set_meta("schema_version", &SCHEMA_VERSION.to_string())?;
        tx.commit()?;
        Ok(prev)
    }

    /// The trigram index over file contents: which files hold each trigram,
    /// keyed by `files.id`, and nothing else. A search intersects the trigrams
    /// of its required literals to get candidate files (Zoekt/csearch style),
    /// then reads those from disk for the real matcher to confirm.
    /// Case-insensitive so the candidates are a safe superset for both
    /// case-sensitive and -i searches.
    ///
    /// Two things it deliberately does not hold, each of which used to be most
    /// of the database:
    ///   * trigram *positions* (`detail=none`) — 30.7 MB of a 66 MB index on
    ///     redis, 1.5 GB on a repo of committed geodata, against 2.5 MB and
    ///     5.4 MB without them;
    ///   * the text itself (`content=''`). Every query refreshes first, so the
    ///     file on disk is the text that was indexed, and a second copy of it
    ///     was 702 MB of what the geodata repo had left.
    fn create_fts(&self) -> Result<()> {
        self.conn.execute_batch(
            "CREATE VIRTUAL TABLE IF NOT EXISTS content_fts USING fts5(
                 body, content = '', contentless_delete = 1,
                 tokenize = 'trigram case_sensitive 0', detail = none
             );",
        )?;
        Ok(())
    }

    /// Empty the content index for a full rebuild. Dropped and recreated, not
    /// `DELETE`d: FTS5 deletes row by row, writing a tombstone per row into an
    /// index that is about to be thrown away, where a drop frees its pages.
    pub fn reset_fts(&self) -> Result<()> {
        self.conn
            .execute_batch("DROP TABLE IF EXISTS content_fts;")?;
        self.create_fts()
    }

    /// Give free pages back to the filesystem once they are a real share of the
    /// file. SQLite never shrinks a database on its own — deleted rows leave
    /// pages on a free list — so an index only ever grew: one measured 5.3 GB
    /// held 3.1 GB of nothing. Thresholded because `VACUUM` rewrites every live
    /// page: at a quarter free it reclaims at least a third of what it copies,
    /// and a steady trickle of edits never reaches it.
    ///
    /// Best-effort throughout. A failure here (another process mid-read, a full
    /// disk) leaves a correct index that is merely larger than it need be.
    pub fn compact_if_bloated(&self) {
        let pragma = |name: &str| -> i64 {
            self.conn
                .query_row(&format!("PRAGMA {name}"), [], |r| r.get(0))
                .unwrap_or(0)
        };
        let (pages, free) = (pragma("page_count"), pragma("freelist_count"));
        if free < COMPACT_MIN_FREE_PAGES || free * 4 < pages {
            return;
        }
        if self.conn.execute_batch("VACUUM;").is_ok() {
            // In WAL mode the rewrite lands in the log; the main file only
            // shrinks, and the log only empties, at a checkpoint.
            let _ = self
                .conn
                .query_row("PRAGMA wal_checkpoint(TRUNCATE)", [], |_| Ok(()));
        }
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
