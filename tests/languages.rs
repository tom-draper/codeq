//! Language support as a contract, checked against one fixture per language in
//! `tests/fixtures/languages/`.
//!
//! Two checks share the fixtures:
//!
//! - **The contract.** `capabilities.language_support` is derived from the query
//!   text, so it says what a query *mentions*, not what extraction *produces*.
//!   Every fact it advertises for a language must actually come out of that
//!   language's fixture.
//! - **The golden facts.** Everything extracted from the fixtures is recorded
//!   under `golden/`, with the `SCHEMA_VERSION` it was recorded at. Freshness is
//!   content hash plus that number, so an extraction change without a bump
//!   leaves every existing index serving the old answer. Here that is a failing
//!   test rather than a remembered check. It sees only what the fixtures
//!   exercise, so a rule no fixture reaches can still slip through.
//!
//! Regenerate the golden files with `CODEQ_BLESS=1 cargo test --test languages`.

use assert_cmd::cargo::CommandCargoExt;
use rusqlite::Connection;
use serde_json::Value;
use std::collections::BTreeMap;
use std::fmt::Write as _;
use std::path::{Path, PathBuf};
use std::process::Command;
use tempfile::TempDir;

fn fixtures() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/fixtures/languages")
}

/// The fixtures, copied out of the checkout and indexed.
struct Indexed {
    dir: TempDir,
    conn: Connection,
    capabilities: Value,
}

impl Indexed {
    fn new() -> Self {
        let dir = TempDir::new().expect("tempdir");
        for entry in std::fs::read_dir(fixtures()).expect("fixture directory") {
            let entry = entry.unwrap();
            if entry.file_type().unwrap().is_file() {
                std::fs::copy(entry.path(), dir.path().join(entry.file_name())).unwrap();
            }
        }
        let run = |args: &[&str]| {
            let out = Command::cargo_bin("codeq")
                .unwrap()
                .current_dir(dir.path())
                .args(["--root", ".", "--json"])
                .args(args)
                .output()
                .expect("run codeq");
            assert!(
                out.status.success(),
                "codeq {args:?}: {}",
                String::from_utf8_lossy(&out.stderr)
            );
            serde_json::from_slice::<Value>(&out.stdout).expect("json")
        };
        run(&["index"]);
        let capabilities = run(&["capabilities"]);
        let conn = Connection::open(dir.path().join(".codeq/index.db")).expect("open index");
        Indexed {
            dir,
            conn,
            capabilities,
        }
    }

    fn schema_version(&self) -> i64 {
        self.capabilities["data"]["index_schema"].as_i64().unwrap()
    }

    /// `(id, path)` of every fixture in `language`, in path order.
    fn files(&self, language: &str) -> Vec<(i64, String)> {
        self.rows(
            "SELECT id, path FROM files WHERE language = ?1 ORDER BY path",
            language,
            |r| Ok((r.get(0)?, r.get(1)?)),
        )
    }

    fn rows<T>(
        &self,
        sql: &str,
        param: impl rusqlite::ToSql,
        map: impl FnMut(&rusqlite::Row) -> rusqlite::Result<T>,
    ) -> Vec<T> {
        let mut stmt = self.conn.prepare(sql).unwrap();
        let rows = stmt.query_map([param], map).unwrap();
        rows.map(Result::unwrap).collect()
    }

    fn count(&self, sql: &str, param: impl rusqlite::ToSql) -> i64 {
        self.conn.query_row(sql, [param], |r| r.get(0)).unwrap()
    }
}

#[test]
fn each_fixture_yields_every_fact_capabilities_advertises() {
    let index = Indexed::new();
    let mut broken = Vec::new();
    for support in index.capabilities["data"]["language_support"]
        .as_array()
        .unwrap()
    {
        let language = support["language"].as_str().unwrap();
        let files = index.files(language);
        if files.is_empty() {
            broken.push(format!(
                "{language}: no fixture in tests/fixtures/languages/"
            ));
            continue;
        }
        let mut found: BTreeMap<&str, i64> = BTreeMap::new();
        let mut relation_kinds = Vec::new();
        for (id, path) in &files {
            // A fixture the parser has to recover from is testing recovery,
            // not the language.
            let errors: Option<i64> = index
                .conn
                .query_row("SELECT parse_errors FROM files WHERE id = ?1", [id], |r| {
                    r.get(0)
                })
                .unwrap();
            if errors.unwrap_or(0) > 0 {
                broken.push(format!("{language}: {path} has parse errors"));
            }
            let facts = "SELECT COUNT(*) FROM facts WHERE path = ?1 AND is_derived = 0 AND kind";
            for (what, sql) in [
                (
                    "definitions",
                    format!("{facts} NOT IN ('import', 'field', 'variant')"),
                ),
                ("imports", format!("{facts} = 'import'")),
                ("members", format!("{facts} IN ('field', 'variant')")),
                (
                    "relations",
                    "SELECT COUNT(*) FROM impls WHERE path = ?1".into(),
                ),
            ] {
                *found.entry(what).or_default() += index.count(&sql, path);
            }
            for (what, table) in [("spans", "spans"), ("calls", "calls")] {
                let sql = format!("SELECT COUNT(*) FROM {table} WHERE file_id = ?1");
                *found.entry(what).or_default() += index.count(&sql, id);
            }
            relation_kinds.extend(index.rows(
                "SELECT DISTINCT relation FROM impls WHERE path = ?1",
                path,
                |r| r.get::<_, String>(0),
            ));
        }
        for (what, count) in &found {
            if support[*what] == true && *count == 0 {
                broken.push(format!(
                    "{language}: advertises {what}, the fixture yields none"
                ));
            }
        }
        for kind in support["relation_kinds"].as_array().unwrap() {
            let kind = kind.as_str().unwrap();
            if !relation_kinds.iter().any(|k| k == kind) {
                broken.push(format!(
                    "{language}: advertises `{kind}` relations, the fixture yields none"
                ));
            }
        }
    }
    assert!(
        broken.is_empty(),
        "capabilities and extraction disagree:\n  {}",
        broken.join("\n  ")
    );
}

/// Everything extraction stored for one language's fixtures, one row per line.
fn dump(index: &Indexed, language: &str) -> String {
    let text = |v: Option<String>| v.unwrap_or_else(|| "-".into());
    let mut out = String::new();
    for (id, path) in index.files(language) {
        // The fixture's own hash, so an edited fixture is told apart from
        // changed extraction: only the second needs a version bump.
        let hash = blake3::hash(&std::fs::read(index.dir.path().join(&path)).unwrap());
        writeln!(out, "file {path} {}", &hash.to_hex()[..16]).unwrap();
        for line in index.rows(
            "SELECT kind, name, start_line, start_col, end_line, end_col, start_byte, end_byte,
                    visibility, owner, is_derived, is_test, decorator, contract, shape,
                    shape_nodes, source, confidence, signature, doc
             FROM facts WHERE path = ?1 ORDER BY start_byte, end_byte, kind, name",
            &path,
            |r| {
                Ok(format!(
                    "fact {} {} {}:{}-{}:{} bytes={}-{} vis={} owner={} derived={} test={} \
                     decorator={} contract={} shape={}/{} source={} confidence={} sig={} doc={}",
                    r.get::<_, String>(0)?,
                    text(r.get(1)?),
                    r.get::<_, i64>(2)?,
                    r.get::<_, i64>(3)?,
                    r.get::<_, i64>(4)?,
                    r.get::<_, i64>(5)?,
                    r.get::<_, i64>(6)?,
                    r.get::<_, i64>(7)?,
                    text(r.get(8)?),
                    text(r.get(9)?),
                    r.get::<_, i64>(10)?,
                    r.get::<_, i64>(11)?,
                    text(r.get(12)?),
                    text(r.get(13)?),
                    text(r.get(14)?),
                    r.get::<_, Option<i64>>(15)?.unwrap_or(0),
                    r.get::<_, String>(16)?,
                    r.get::<_, f64>(17)?,
                    text(r.get(18)?).escape_debug(),
                    text(r.get(19)?).escape_debug(),
                ))
            },
        ) {
            writeln!(out, "{line}").unwrap();
        }
        for line in index.rows(
            "SELECT line, subtype, relation, supertype FROM impls WHERE path = ?1
             ORDER BY line, subtype, relation, supertype",
            &path,
            |r| {
                Ok(format!(
                    "relation {} {} {} {}",
                    r.get::<_, i64>(0)?,
                    r.get::<_, String>(1)?,
                    r.get::<_, String>(2)?,
                    r.get::<_, String>(3)?,
                ))
            },
        ) {
            writeln!(out, "{line}").unwrap();
        }
        for line in index.rows(
            "SELECT start_byte, name, receiver FROM calls WHERE file_id = ?1 ORDER BY start_byte",
            id,
            |r| {
                Ok(format!(
                    "call {} {} on={}",
                    r.get::<_, i64>(0)?,
                    r.get::<_, String>(1)?,
                    text(r.get(2)?).escape_debug(),
                ))
            },
        ) {
            writeln!(out, "{line}").unwrap();
        }
        for line in index.rows(
            "SELECT start_byte, end_byte, kind FROM spans WHERE file_id = ?1
             ORDER BY start_byte, end_byte, kind",
            id,
            |r| {
                Ok(format!(
                    "span {}-{} kind={}",
                    r.get::<_, i64>(0)?,
                    r.get::<_, i64>(1)?,
                    r.get::<_, i64>(2)?,
                ))
            },
        ) {
            writeln!(out, "{line}").unwrap();
        }
    }
    out
}

/// The `file` lines of a dump: which fixtures it was taken from, at what content.
fn sources(dump: &str) -> Vec<&str> {
    dump.lines().filter(|l| l.starts_with("file ")).collect()
}

#[test]
fn extraction_matches_the_golden_facts_for_this_schema_version() {
    let index = Indexed::new();
    let golden = fixtures().join("golden");
    let version = index.schema_version();
    let recorded: Option<i64> = std::fs::read_to_string(golden.join("SCHEMA_VERSION"))
        .ok()
        .and_then(|s| s.trim().parse().ok());

    // Per language: the fresh dump, and what is wrong with the recorded one.
    let mut dumps = BTreeMap::new();
    let mut extraction_changed = Vec::new();
    let mut fixture_changed = Vec::new();
    for support in index.capabilities["data"]["language_support"]
        .as_array()
        .unwrap()
    {
        let language = support["language"].as_str().unwrap().to_string();
        let fresh = dump(&index, &language);
        let old = std::fs::read_to_string(golden.join(format!("{language}.txt"))).ok();
        match old {
            Some(old) if old == fresh => {}
            Some(old) if sources(&old) == sources(&fresh) => {
                let line = fresh
                    .lines()
                    .zip(old.lines())
                    .position(|(a, b)| a != b)
                    .unwrap_or_else(|| fresh.lines().count().min(old.lines().count()));
                extraction_changed.push(format!(
                    "{language}: first difference at golden/{language}.txt:{}\n      was: {}\n      now: {}",
                    line + 1,
                    old.lines().nth(line).unwrap_or("(end of file)"),
                    fresh.lines().nth(line).unwrap_or("(end of file)"),
                ));
            }
            _ => fixture_changed.push(language.clone()),
        }
        dumps.insert(language, fresh);
    }

    if std::env::var_os("CODEQ_BLESS").is_some() {
        assert!(
            extraction_changed.is_empty() || recorded != Some(version),
            "refusing to bless: extraction changed for unchanged fixtures while SCHEMA_VERSION \
             is still {version}. Existing indexes would keep the old facts. Bump SCHEMA_VERSION \
             in src/db.rs, then bless again.\n  {}",
            extraction_changed.join("\n  ")
        );
        std::fs::create_dir_all(&golden).unwrap();
        for entry in std::fs::read_dir(&golden).unwrap() {
            std::fs::remove_file(entry.unwrap().path()).unwrap();
        }
        for (language, fresh) in &dumps {
            std::fs::write(golden.join(format!("{language}.txt")), fresh).unwrap();
        }
        std::fs::write(golden.join("SCHEMA_VERSION"), format!("{version}\n")).unwrap();
        return;
    }

    const BLESS: &str = "CODEQ_BLESS=1 cargo test --test languages";
    assert!(
        extraction_changed.is_empty() || recorded != Some(version),
        "extraction changed for unchanged fixtures, and SCHEMA_VERSION is still {version}. \
         Existing indexes would keep serving the old facts. Bump SCHEMA_VERSION in src/db.rs, \
         then run `{BLESS}` and commit the golden files.\n  {}",
        extraction_changed.join("\n  ")
    );
    assert_eq!(
        recorded,
        Some(version),
        "the golden facts were recorded at another SCHEMA_VERSION. Run `{BLESS}` and commit \
         the result, so the next extraction change is measured against this version."
    );
    assert!(
        fixture_changed.is_empty(),
        "fixtures changed or were added without their golden facts ({}). Run `{BLESS}` and \
         commit the result.",
        fixture_changed.join(", ")
    );
}
