//! Integration tests: drive the real `codeq` binary against programmatic
//! fixture repositories and assert on the JSON contract agents consume.
//!
//! These deliberately exercise the CLI end to end (not internals), because the
//! stable output envelope *is* the product surface.

use std::io::Write;
use std::path::Path;
use std::process::{Command, Output};

use assert_cmd::cargo::CommandCargoExt;
use serde_json::Value;
use tempfile::TempDir;

/// A throwaway repository under a temp dir.
struct Fixture {
    dir: TempDir,
}

impl Fixture {
    fn new() -> Self {
        Fixture {
            dir: TempDir::new().expect("tempdir"),
        }
    }

    fn path(&self) -> &Path {
        self.dir.path()
    }

    fn write(&self, rel: &str, contents: &str) -> &Self {
        self.write_bytes(rel, contents.as_bytes());
        self
    }

    fn write_bytes(&self, rel: &str, bytes: &[u8]) -> &Self {
        let p = self.path().join(rel);
        if let Some(parent) = p.parent() {
            std::fs::create_dir_all(parent).unwrap();
        }
        std::fs::write(p, bytes).unwrap();
        self
    }

    fn remove(&self, rel: &str) {
        std::fs::remove_file(self.path().join(rel)).unwrap();
    }

    /// Run git with deterministic identity/config injected.
    fn git(&self, args: &[&str]) {
        let out = Command::new("git")
            .arg("-C")
            .arg(self.path())
            .args([
                "-c",
                "user.email=codeq@test",
                "-c",
                "user.name=codeq",
                "-c",
                "commit.gpgsign=false",
            ])
            .args(args)
            .output()
            .expect("run git");
        assert!(
            out.status.success(),
            "git {:?} failed: {}",
            args,
            String::from_utf8_lossy(&out.stderr)
        );
    }

    fn git_init_commit(&self) {
        self.git(&["init", "-q", "-b", "main"]);
        self.git(&["add", "-A"]);
        self.git(&["commit", "-q", "-m", "init"]);
    }

    fn raw(&self, args: &[&str]) -> Output {
        Command::cargo_bin("codeq")
            .unwrap()
            .current_dir(self.path())
            .args(args)
            .output()
            .expect("run codeq")
    }

    /// Run codeq with `--json` (default output is now text), assert success, and
    /// parse stdout as JSON. Text-mode output is covered by dedicated tests.
    fn run(&self, args: &[&str]) -> Value {
        let mut with_json = vec!["--json"];
        with_json.extend_from_slice(args);
        let out = self.raw(&with_json);
        assert!(
            out.status.success(),
            "codeq {:?} exited {}: {}",
            args,
            out.status,
            String::from_utf8_lossy(&out.stderr)
        );
        serde_json::from_slice(&out.stdout).unwrap_or_else(|e| {
            panic!(
                "codeq {:?} produced invalid JSON: {e}\n{}",
                args,
                String::from_utf8_lossy(&out.stdout)
            )
        })
    }
}

/// Collect a string field from every element of `data`.
fn field_set(v: &Value, key: &str) -> Vec<String> {
    v["data"]
        .as_array()
        .unwrap()
        .iter()
        .filter_map(|r| r[key].as_str().map(|s| s.to_string()))
        .collect()
}

/// Collect a JSON array of strings into a Vec.
fn field_names(v: &Value) -> Vec<String> {
    v.as_array()
        .unwrap()
        .iter()
        .filter_map(|s| s.as_str().map(|s| s.to_string()))
        .collect()
}

fn at_stdin(f: &Fixture, input: &str, extra: &[&str]) -> Output {
    let mut child = Command::cargo_bin("codeq")
        .unwrap()
        .current_dir(f.path())
        .args(["--json", "at", "--stdin"])
        .args(extra)
        .stdin(std::process::Stdio::piped())
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::piped())
        .spawn()
        .unwrap();
    child
        .stdin
        .take()
        .unwrap()
        .write_all(input.as_bytes())
        .unwrap();
    child.wait_with_output().unwrap()
}

#[test]
fn at_batches_keep_order_duplicates_and_unresolved_locations() {
    let f = Fixture::new();
    f.write("a.py", "class A:\n    def run(self):\n        return 1\n");
    f.write("b.rs", "fn entry() {\n    let x = 1;\n}\n");
    let single = f.run(&["at", "a.py:3"]);
    assert_eq!(field_set(&single, "name"), ["run", "A"]);
    let column = f.run(&["at", "a.py:3:8"]);
    assert_eq!(column["data"], single["data"]);
    let batch = f.run(&[
        "at",
        "b.rs:2",
        "a.py:3:8",
        "missing.py:1",
        "a.py:3",
        "a.py:99",
    ]);
    let rows = batch["data"]["results"].as_array().unwrap();
    assert_eq!(rows.len(), 5);
    assert_eq!(rows[0]["data"][0]["name"], "entry");
    assert_eq!(rows[1]["data"].as_array().unwrap().len(), 1);
    assert_eq!(rows[1]["data"][0]["name"], "run");
    assert_eq!(rows[2]["status"], "unresolved");
    assert!(rows[2]["note"].as_str().unwrap().contains("not found"));
    assert_eq!(rows[3]["name"], "a.py:3");
    assert_eq!(rows[4]["status"], "unresolved");
    for location in [
        "a.py:0",
        "a.py:-1",
        "a.py:1:0",
        "a.py:9223372036854775808",
        "a.py:bad",
    ] {
        assert!(!f.raw(&["at", location]).status.success(), "{location}");
    }
    let text = f.raw(&["at", "a.py:3", "b.rs:2"]);
    assert!(text.status.success());
    let text = String::from_utf8(text.stdout).unwrap();
    assert!(text.find("# a.py:3").unwrap() < text.find("# b.rs:2").unwrap());
}

#[test]
fn at_stdin_resolves_stack_formats_without_dropping_unknown_frames() {
    let f = Fixture::new();
    f.write("src/a.py", "def first():\n    return 1\n");
    f.write("src/main.rs", "fn entry() {\n    let x = 1;\n}\n");
    f.write("src/view.ts", "function view() {\n    return 1;\n}\n");
    f.write("one/Cls.java", "class Cls {\n  void run() {}\n}\n");
    f.write("two/Cls.java", "class Cls {\n  void run() {}\n}\n");
    let trace = "Traceback (most recent call last):\n  File \"src/a.py\", line 2, in first\n    return 1\n   at src/main.rs:2:5\n   at view (src/view.ts:2:4)\n   at pkg.Cls.run(Cls.java:2)\nsrc/main.rs:2:5: error: example\n   at lib.Native.call(Native Method)\n   at foreign (/outside/src/view.ts:2:4)\n  File \"src/a.py\", line 2, in first\n";
    let output = at_stdin(&f, trace, &[]);
    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    let value: Value = serde_json::from_slice(&output.stdout).unwrap();
    let rows = value["data"]["results"].as_array().unwrap();
    assert_eq!(rows.len(), 8);
    for (i, name) in [
        (0, "first"),
        (1, "entry"),
        (2, "view"),
        (4, "entry"),
        (7, "first"),
    ] {
        assert_eq!(rows[i]["status"], "resolved");
        assert_eq!(rows[i]["data"][0]["name"], name);
    }
    assert_eq!(rows[3]["status"], "ambiguous");
    assert!(rows[3]["note"]
        .as_str()
        .unwrap()
        .contains("one/Cls.java, two/Cls.java"));
    assert_eq!(rows[5]["status"], "unresolved");
    assert_eq!(rows[6]["status"], "unresolved");
    assert_eq!(value["data"]["ignored_lines"], 2);
    assert!(!at_stdin(&f, "no frames here\n", &[]).status.success());
    assert!(!at_stdin(&f, "", &[]).status.success());
    assert!(!f.raw(&["at", "a.py:1", "--stdin"]).status.success());
    let one = at_stdin(&f, "src/a.py:2\n", &[]);
    let one: Value = serde_json::from_slice(&one.stdout).unwrap();
    assert_eq!(one["data"]["results"].as_array().unwrap().len(), 1);
}

#[test]
fn at_source_uses_one_deduplicated_budget_for_all_frames() {
    let f = Fixture::new();
    f.write(
        "a.py",
        "def first():\n    return 1\n\ndef second():\n    return 2\n",
    );
    let value = f.run(&[
        "at",
        "a.py:2",
        "a.py:5",
        "a.py:2",
        "--source",
        "--max-tokens",
        "100",
    ]);
    assert_eq!(value["data"]["results"].as_array().unwrap().len(), 3);
    let source = &value["data"]["source"];
    assert_eq!(source["chunks"].as_array().unwrap().len(), 2);
    assert_eq!(source["chunks"][0]["name"], "first");
    assert_eq!(source["chunks"][1]["name"], "second");
    assert!(source["chunks"][0]["text"]
        .as_str()
        .unwrap()
        .contains("return 1"));
    assert!(source["budget"]["used_tokens"].as_u64().unwrap() <= 100);
    let empty = f.run(&["at", "a.py:2", "--source", "--max-tokens", "0"]);
    assert_eq!(empty["data"]["source"]["chunks"], serde_json::json!([]));
    assert_eq!(empty["data"]["source"]["omitted"][0]["name"], "first");
    let text = f.raw(&["at", "a.py:2", "--source"]);
    assert!(text.status.success());
    assert!(String::from_utf8(text.stdout).unwrap().contains("return 1"));
    let projected = f.run(&["at", "a.py:2", "--source", "--fields", "name"]);
    assert_eq!(
        projected["data"]["source"]["chunks"][0],
        serde_json::json!({"name": "first"})
    );
    let capped = f.run(&["at", "a.py:2", "a.py:5", "--source", "--max-per-file", "1"]);
    assert_eq!(
        capped["data"]["source"]["chunks"].as_array().unwrap().len(),
        1
    );
    assert_eq!(capped["notes"]["truncated_per_file"]["a.py"], 2);
}

#[test]
fn at_stdin_handles_compiler_arrows_parentheses_and_file_urls() {
    let f = Fixture::new();
    f.write("src/(generated)/a.py", "def generated():\n    return 1\n");
    f.write("src/main.rs", "fn entry() {\n    let x = 1;\n}\n");
    f.write("src/space name.py", "def spaced():\n    return 1\n");
    let trace = format!(" --> src/main.rs:2:5\n ::: src/main.rs:2:5\nsrc/(generated)/a.py:2\n  at file://{}:2:4\nsrc/space name.py:2:1: diagnostic\n  at generated (src/(generated)/a.py:2)\n", f.path().join("src/main.rs").display());
    let output = at_stdin(&f, &trace, &[]);
    assert!(output.status.success());
    let value: Value = serde_json::from_slice(&output.stdout).unwrap();
    let frames = value["data"]["results"].as_array().unwrap();
    assert_eq!(frames.len(), 6);
    assert!(
        frames.iter().all(|frame| frame["status"] == "resolved"),
        "{value}"
    );
    assert_eq!(frames[2]["data"][0]["name"], "generated");
    assert_eq!(frames[4]["data"][0]["name"], "spaced");
}

/// Collect the `symbol` of every chunk in a `context` result's `data` object.
fn chunk_symbols(data: &Value) -> Vec<String> {
    data["chunks"]
        .as_array()
        .unwrap()
        .iter()
        .filter_map(|c| c["name"].as_str().map(|s| s.to_string()))
        .collect()
}

// ---------------------------------------------------------------------------
// Contract / schema lock
// ---------------------------------------------------------------------------

#[test]
fn default_output_is_lean_when_nothing_noteworthy() {
    // The token-tax fix: a fresh, clean call is just {schema, data} — no repo
    // or index meta blocks to pay for.
    let f = Fixture::new();
    f.write("a.py", "def x():\n    pass\n");
    f.run(&["index"]);
    let v = f.run(&["map", "--totals"]); // second call: nothing changed

    let mut keys: Vec<&str> = v.as_object().unwrap().keys().map(|s| s.as_str()).collect();
    keys.sort();
    // `rows` is contract, not noise: it names where the row array lives, which is
    // the alternative to a consumer knowing thirty commands' shapes in advance.
    // What "lean" pins is the absence of the repo/index meta blocks.
    assert_eq!(
        keys,
        ["data", "rows", "schema"],
        "clean call should carry no meta"
    );
    assert_eq!(v["schema"], "codeq.result.v2");
}

#[test]
fn meta_flag_returns_full_stable_envelope() {
    let f = Fixture::new();
    f.write("a.py", "def x():\n    pass\n");
    let v = f.run(&["map", "--totals", "--meta"]);

    let mut keys: Vec<&str> = v.as_object().unwrap().keys().map(|s| s.as_str()).collect();
    keys.sort();
    assert_eq!(keys, ["data", "index", "repo", "rows", "schema"]);
    assert_eq!(v["schema"], "codeq.result.v2");
    // The number itself is not the contract — that it is *the* index schema
    // version is. Pinning the literal made every schema bump break this
    // test instead of the thing it changed, and the suite shipped red.
    let declared = f.run(&["capabilities"])["data"]["index_schema"].clone();
    assert!(declared.as_u64().is_some_and(|n| n > 0), "{declared}");
    assert_eq!(v["index"]["schema"], declared);
    assert_eq!(v["index"]["fresh"], true);
    assert!(v["index"]["elapsed_ms"].is_u64());
    assert!(v["repo"]["root"].is_string());
    assert!(v["repo"]["vcs"].is_string());
}

#[test]
fn reindex_surfaces_index_block_without_meta() {
    // When work was actually done, the index block appears even in lean mode —
    // meta shows up precisely when it carries signal.
    let f = Fixture::new();
    f.write("a.py", "def x():\n    pass\n");
    let v = f.run(&["index"]); // first index: reparses the file
    assert_eq!(v["index"]["reindexed_files"], 1);
}

#[test]
fn fields_projection_keeps_only_requested_columns() {
    let f = Fixture::new();
    f.write("a.rs", "fn one() {}\nfn two() {}\n");
    let v = f.run(&["symbols", "--fields", "name,path"]);
    for row in v["data"].as_array().unwrap() {
        let mut keys: Vec<&str> = row
            .as_object()
            .unwrap()
            .keys()
            .map(|s| s.as_str())
            .collect();
        keys.sort();
        assert_eq!(
            keys,
            ["name", "path"],
            "projection should drop other fields"
        );
    }
}

#[test]
fn fields_projects_rows_inside_object_results() {
    // --fields must project the rows even when they're nested in a wrapper
    // object (references/search/context), the token-saving lever agents need.
    let f = Fixture::new();
    f.write("a.rs", "fn helper() {}\nfn caller() { helper(); }\n");
    let v = f.run(&["references", "helper", "--fields", "path,line"]);
    let refs = v["data"]["references"].as_array().unwrap();
    assert!(!refs.is_empty());
    for r in refs {
        let mut keys: Vec<&str> = r.as_object().unwrap().keys().map(|s| s.as_str()).collect();
        keys.sort();
        assert_eq!(keys, ["line", "path"], "row not projected: {r}");
    }
    // The wrapper's scalar field survives.
    assert!(v["data"].get("truncated").is_some());
}

#[test]
fn capabilities_describes_the_tool() {
    let f = Fixture::new();
    let v = f.run(&["capabilities"]);
    assert!(v["data"]["fact_kinds"]
        .as_array()
        .unwrap()
        .iter()
        .any(|kind| kind == "key"));
    assert_eq!(v["data"]["configuration_keys"]["max_keys_per_file"], 10000);
    let d = &v["data"];
    let cmds = field_names(&d["commands"]);
    assert!(cmds.contains(&"context".to_string()), "{cmds:?}");
    assert!(cmds.contains(&"capabilities".to_string()));
    let langs = field_names(&d["extraction_languages"]);
    assert!(langs.contains(&"rust".to_string()) && langs.contains(&"python".to_string()));
    assert_eq!(d["result_schema"], "codeq.result.v2");

    // Default output is text like every other command, not a raw JSON dump.
    let text = String::from_utf8_lossy(&f.raw(&["capabilities"]).stdout).into_owned();
    assert!(
        !text.contains("\"schema\""),
        "capabilities text mode dumped JSON: {text}"
    );
    assert!(
        text.contains("# commands"),
        "capabilities text missing sections: {text}"
    );
}

#[test]
fn capabilities_lists_every_command_the_cli_accepts() {
    // `commands` was hand-written and drifted nine commands behind the parser —
    // `explore` among them, so the one command whose job is to say what codeq can
    // do was hiding the tool an agent should reach for first. It is now derived
    // from clap; this cross-checks it against the parser's own help so any future
    // drift fails the build instead of quietly under-selling the tool.
    let f = Fixture::new();
    let listed = field_names(&f.run(&["capabilities"])["data"]["commands"]);

    // Subcommands are the `^  <name>` rows of the help's Commands: block; any
    // wrapped description line is indented further, so it can't be mistaken for one.
    let help = String::from_utf8_lossy(&f.raw(&["--help"]).stdout).into_owned();
    let accepted: Vec<String> = help
        .lines()
        .skip_while(|l| !l.starts_with("Commands:"))
        .skip(1)
        .take_while(|l| !l.starts_with("Options:"))
        .filter(|l| l.starts_with("  ") && !l.starts_with("   "))
        .filter_map(|l| l.split_whitespace().next())
        .filter(|n| *n != "help") // clap's own, not a codeq capability
        .map(str::to_string)
        .collect();

    assert!(
        accepted.len() > 20,
        "help parsing found too few commands: {accepted:?}"
    );
    for name in &accepted {
        assert!(
            listed.contains(name),
            "capabilities omits `{name}`: {listed:?}"
        );
    }
    assert_eq!(
        listed.len(),
        accepted.len(),
        "capabilities lists commands the CLI rejects"
    );
}

// ---------------------------------------------------------------------------
// Inventory + ignore handling
// ---------------------------------------------------------------------------

#[test]
fn indexes_files_and_respects_gitignore() {
    let f = Fixture::new();
    f.write("src/a.rs", "fn a() {}\n")
        .write("noise.log", "junk\n")
        .write(".gitignore", "*.log\n");
    f.git_init_commit();

    let v = f.run(&["files"]);
    let paths = field_set(&v, "path");
    assert!(paths.contains(&"src/a.rs".to_string()), "paths: {paths:?}");
    assert!(
        !paths.iter().any(|p| p.ends_with(".log")),
        "gitignored file leaked: {paths:?}"
    );
}

#[test]
fn binary_file_is_indexed_without_language() {
    let f = Fixture::new();
    f.write_bytes("blob.bin", &[0u8, 1, 2, 0, 255, 3]);
    f.write("real.py", "def y():\n    pass\n");
    let v = f.run(&["files"]);
    let blob = v["data"]
        .as_array()
        .unwrap()
        .iter()
        .find(|r| r["path"] == "blob.bin")
        .expect("binary file indexed");
    assert!(
        blob.get("language").is_none(),
        "binary file should have no language: {blob}"
    );
}

#[test]
fn files_surface_parser_recovery_health() {
    let f = Fixture::new();
    f.write("clean.rs", "fn clean() {}\n")
        .write("broken.rs", "fn broken( {\n")
        .write("README.md", "# prose\n");

    let rows = f.run(&["files"])["data"].as_array().unwrap().clone();
    let row = |path: &str| rows.iter().find(|r| r["path"] == path).unwrap();
    assert_eq!(row("clean.rs")["parse_errors"], 0);
    assert!(row("broken.rs")["parse_errors"].as_i64().unwrap() > 0);
    assert!(row("README.md").get("parse_errors").is_none());
}

#[test]
fn non_git_directory_works() {
    let f = Fixture::new();
    f.write("a.py", "def z():\n    pass\n");
    let v = f.run(&["map", "--totals", "--meta"]);
    assert_eq!(v["repo"]["vcs"], "none");
    assert!(v["repo"].get("head").is_none());
    assert_eq!(v["data"]["totals"]["files"], 1);
}

#[test]
fn unicode_paths_are_handled() {
    let f = Fixture::new();
    f.write("src/café/naïve.py", "def accented():\n    pass\n");
    let files = field_set(&f.run(&["files"]), "path");
    assert!(
        files.iter().any(|p| p.contains("café/na")),
        "unicode path missing: {files:?}"
    );
    let syms = field_set(&f.run(&["symbols"]), "name");
    assert!(syms.contains(&"accented".to_string()), "syms: {syms:?}");
}

// ---------------------------------------------------------------------------
// Robustness / adversarial
// ---------------------------------------------------------------------------

#[test]
fn malformed_source_does_not_crash() {
    let f = Fixture::new();
    f.write("broken.rs", "fn ( { struct )) impl for 123 use ;;;\n");
    // Indexing and querying must succeed even on unparseable input.
    f.run(&["index"]);
    f.run(&["symbols"]);
    f.run(&["imports"]);
}

#[test]
#[cfg(unix)]
fn symlink_cycle_terminates() {
    use std::os::unix::fs::symlink;
    let f = Fixture::new();
    f.write("dir/real.py", "def ok():\n    pass\n");
    symlink(f.path().join("dir"), f.path().join("dir/loop")).unwrap();
    // Must not infinitely recurse; default walk does not follow symlinks.
    let v = f.run(&["files"]);
    let paths = field_set(&v, "path");
    assert!(paths.contains(&"dir/real.py".to_string()), "{paths:?}");
}

#[test]
fn large_file_is_handled() {
    let f = Fixture::new();
    let big = "x".repeat(1_500_000);
    f.write("big.txt", &big);
    f.write("a.py", "def small():\n    pass\n");
    let v = f.run(&["map", "--totals"]);
    assert!(v["data"]["totals"]["files"].as_i64().unwrap() >= 2);
}

// ---------------------------------------------------------------------------
// Incremental lifecycle
// ---------------------------------------------------------------------------

#[test]
fn reindex_counts_only_content_changes() {
    let f = Fixture::new();
    f.write("a.rs", "fn one() {}\n");
    f.run(&["index"]);

    // Rewrite identical bytes: mtime may bump but content hash is unchanged, so
    // nothing is reparsed and the whole index block stays out of the envelope.
    f.write("a.rs", "fn one() {}\n");
    let v = f.run(&["map", "--totals"]);
    assert!(
        v.get("index").is_none(),
        "identical rewrite must not count as reindex: {v}"
    );

    // Real content change: exactly one file reparsed.
    f.write("a.rs", "fn one() {}\nfn two() {}\n");
    let v = f.run(&["map", "--totals"]);
    assert_eq!(v["index"]["reindexed_files"], 1);
}

#[test]
fn deleting_a_file_cascades_to_its_facts() {
    let f = Fixture::new();
    f.write("a.rs", "fn alpha() {}\n");
    assert_eq!(
        f.run(&["definitions", "alpha"])["data"]
            .as_array()
            .unwrap()
            .len(),
        1
    );

    f.remove("a.rs");
    let defs = f.run(&["definitions", "alpha"]);
    assert_eq!(defs["data"].as_array().unwrap().len(), 0, "fact not pruned");
    let files = field_set(&f.run(&["files"]), "path");
    assert!(!files.contains(&"a.rs".to_string()), "file row not pruned");
}

// ---------------------------------------------------------------------------
// Symbols / imports correctness
// ---------------------------------------------------------------------------

#[test]
fn extracts_rust_symbols_and_imports() {
    let f = Fixture::new();
    f.write(
        "lib.rs",
        "use crate::db::Store;\nstruct Widget;\nfn build() -> Widget { Widget }\n",
    );
    let syms = field_set(&f.run(&["symbols"]), "name");
    assert!(syms.contains(&"Widget".to_string()), "{syms:?}");
    assert!(syms.contains(&"build".to_string()), "{syms:?}");
    // Imports must not leak into symbols.
    assert!(!syms.iter().any(|s| s.contains("crate::db")), "{syms:?}");

    let mods = field_set(&f.run(&["imports"]), "module");
    assert!(mods.contains(&"crate::db::Store".to_string()), "{mods:?}");

    let importers = field_set(&f.run(&["importers", "db::Store"]), "path");
    assert_eq!(importers, vec!["lib.rs".to_string()]);

    // A file-path query is accepted symmetrically with `imports <file>`: the
    // module string is `crate::db::Store`, never the path, so `importers`
    // reduces `src/db.rs` to its stem `db` and still matches.
    let by_path = field_set(&f.run(&["importers", "src/db.rs"]), "path");
    assert_eq!(
        by_path,
        vec!["lib.rs".to_string()],
        "path query should resolve via stem"
    );
}

/// A short file stem must not match inside a longer name: `sr` is in `./locale/sr`
/// and not in `./src/util`, and `Buffer` is `okio.Buffer`, not `okio.BufferedSource`.
#[test]
fn importers_match_the_stem_as_a_whole_name() {
    let f = Fixture::new();
    f.write("locale/sr.js", "export default {};\n");
    f.write("src/util.js", "export const u = 1;\n");
    f.write(
        "app.js",
        "import sr from './locale/sr';\nimport u from './src/util';\n",
    );
    let rows = f.run(&["importers", "locale/sr.js"]);
    let modules = field_set(&rows, "module");
    assert_eq!(modules, vec!["./locale/sr".to_string()], "{rows}");

    f.write(
        "Main.java",
        "import okio.Buffer;\nimport okio.BufferedSource;\nclass Main {}\n",
    );
    let modules = field_set(&f.run(&["importers", "Buffer"]), "module");
    assert_eq!(modules, vec!["okio.Buffer".to_string()]);
}

/// A query that names one file drops an import the graph resolves to a different
/// file, even when the stem matches whole: `./de` in `a/` is `a/de.js`, not `b/de.js`.
#[test]
fn importers_drop_an_import_that_resolves_to_another_file() {
    let f = Fixture::new();
    f.write("a/de.js", "export const de = 1;\n");
    f.write("b/de.js", "export const de = 2;\n");
    f.write("a/main.js", "import { de } from './de';\n");
    f.write("b/main.js", "import { de } from './de';\n");
    // A named import also records the member it binds, so a file can appear twice.
    let importers = |q: &str| {
        let mut paths = field_set(&f.run(&["importers", q]), "path");
        paths.dedup();
        paths
    };
    assert_eq!(importers("a/de.js"), vec!["a/main.js".to_string()]);
    assert_eq!(importers("b/de.js"), vec!["b/main.js".to_string()]);

    // An import that resolves to nothing in the repo is kept: the graph is a
    // heuristic, so a miss there must not hide a real importer.
    f.write("c/main.js", "import { de } from 'de';\n");
    let paths = importers("a/de.js");
    assert!(paths.contains(&"c/main.js".to_string()), "{paths:?}");
}

#[test]
fn rust_parser_recovery_keeps_definitions_after_a_valid_trailing_closure_comma() {
    // Rust accepts a trailing comma in closure parameters. Older bundled
    // Tree-sitter Rust grammars recover by wrapping the rest of the file in an
    // ERROR node; dropping every definition after that makes the index lie.
    let f = Fixture::new();
    f.write(
        "lib.rs",
        "use std::collections::{HashMap, HashSet};\n\
         fn before() {}\n\
         fn uses_closure() {\n\
             let _f = |\n\
                 _value: HashSet<String>,\n\
                 _paths: Option<&HashMap<String, HashSet<String>>>,\n\
             | -> Result<(), ()> { Ok(()) };\n\
         }\n\
         pub fn after() {}\n",
    );
    let names = field_set(&f.run(&["definitions", "after"]), "name");
    assert_eq!(
        names,
        ["after"],
        "valid Rust after parser recovery disappeared: {names:?}"
    );
    let files = f.run(&["files"]);
    let row = files["data"]
        .as_array()
        .unwrap()
        .iter()
        .find(|r| r["path"] == "lib.rs")
        .unwrap();
    assert_eq!(
        row["parse_errors"], 0,
        "repaired parse still looks broken: {row}"
    );
    assert_eq!(
        row["parse_recovered"], true,
        "recovery was not surfaced: {row}"
    );
}

#[test]
fn ruby_require_is_an_import() {
    // Ruby has no import statement — `require`/`require_relative` are method
    // calls — but they are its import mechanism, so codeq captures them (guarded
    // by the method name so ordinary calls like `puts "x"` are not imports).
    let f = Fixture::new();
    f.write(
        "a.rb",
        "require \"set\"\nrequire_relative \"./foo\"\nputs \"nope\"\nraise \"no\"\ndef m; end\n",
    );
    let mods = field_set(&f.run(&["imports"]), "module");
    assert!(mods.contains(&"set".to_string()), "{mods:?}");
    assert!(mods.contains(&"./foo".to_string()), "{mods:?}");
    assert!(
        !mods.iter().any(|m| m == "nope" || m == "no"),
        "a non-require string leaked as an import: {mods:?}"
    );
    // Definitions must still be extracted alongside imports.
    assert!(field_set(&f.run(&["symbols"]), "name").contains(&"m".to_string()));
}

#[test]
fn definitions_returns_precise_ranges() {
    let f = Fixture::new();
    f.write(
        "m.py",
        "def first():\n    pass\n\ndef target():\n    return 1\n",
    );
    let v = f.run(&["definitions", "target"]);
    let rows = v["data"].as_array().unwrap();
    assert_eq!(rows.len(), 1);
    let r = &rows[0];
    assert_eq!(r["kind"], "function");
    assert_eq!(r["line"], 4); // 1-based
    assert!(r["end_byte"].as_i64().unwrap() > r["byte"].as_i64().unwrap());
}

// ---------------------------------------------------------------------------
// Git-aware changed + exit codes
// ---------------------------------------------------------------------------

#[test]
fn changed_handles_paths_git_would_quote() {
    // Git quotes non-ASCII names by default. Also exercise spaces and, on Unix,
    // a double quote that Windows filenames cannot contain.
    #[cfg(not(windows))]
    let names = ["ünï.rs", "sp ace.rs", "q\"uote.rs", " lead.rs"];
    #[cfg(windows)]
    let names = ["ünï.rs", "sp ace.rs", "quo té.rs", " lead.rs"];
    let f = Fixture::new();
    for n in names {
        f.write(n, "fn a() {}\n");
    }
    f.git_init_commit();
    for n in names {
        f.write(n, "fn a() {}\nfn added() {}\n");
    }
    f.write("new ünï.rs", "fn fresh() {}\n");
    // A user's diff config must not change what a `+++` line looks like.
    f.git(&["config", "diff.noprefix", "true"]);

    let paths = field_set(&f.run(&["changed", "--compare-to", "HEAD"]), "path");
    for n in names.iter().chain(&["new ünï.rs"]) {
        assert!(
            paths.contains(&n.to_string()),
            "{n:?} missing from {paths:?}"
        );
    }

    let v = f.run(&["changed", "--compare-to", "HEAD", "--symbols"]);
    let hit = |path: &str, name: &str| {
        v["data"]
            .as_array()
            .unwrap()
            .iter()
            .any(|r| r["path"] == path && r["name"] == name)
    };
    for n in names {
        assert!(hit(n, "added"), "{n:?} hunk not attributed: {v}");
    }
    assert!(hit("new ünï.rs", "fresh"), "{v}");
}

#[test]
fn changed_reports_modified_and_untracked() {
    let f = Fixture::new();
    f.write("a.rs", "fn a() {}\n").write("b.rs", "fn b() {}\n");
    f.git_init_commit();

    f.write("a.rs", "fn a() {}\n// edit\n"); // modify tracked
    f.write("c.rs", "fn c() {}\n"); // new untracked

    let paths = field_set(&f.run(&["changed", "--compare-to", "HEAD"]), "path");
    assert!(paths.contains(&"a.rs".to_string()), "{paths:?}");
    assert!(paths.contains(&"c.rs".to_string()), "{paths:?}");
    assert!(
        !paths.contains(&"b.rs".to_string()),
        "unchanged b leaked: {paths:?}"
    );
}

// ---------------------------------------------------------------------------
// TypeScript / JavaScript
// ---------------------------------------------------------------------------

#[test]
fn typescript_symbols_and_imports() {
    let f = Fixture::new();
    f.write(
        "svc.ts",
        "import { Client } from \"./client\";\n\
         interface User { id: number; }\n\
         type UserId = number;\n\
         enum Role { Admin }\n\
         export class Service {\n  find(id: UserId): User { return null as any; }\n}\n\
         export const auth = (t: string): boolean => t.length > 0;\n",
    );

    let syms = f.run(&["symbols"]);
    let by_kind = |k: &str| -> Vec<String> {
        syms["data"]
            .as_array()
            .unwrap()
            .iter()
            .filter(|r| r["kind"] == k)
            .map(|r| r["name"].as_str().unwrap().to_string())
            .collect()
    };
    assert!(by_kind("interface").contains(&"User".to_string()));
    assert!(by_kind("type").contains(&"UserId".to_string()));
    assert!(by_kind("enum").contains(&"Role".to_string()));
    assert!(by_kind("class").contains(&"Service".to_string()));
    assert!(
        by_kind("method").contains(&"find".to_string()),
        "method under class"
    );
    assert!(
        by_kind("function").contains(&"auth".to_string()),
        "arrow-const fn"
    );

    let mods = field_set(&f.run(&["imports"]), "module");
    assert!(
        mods.contains(&"./client".to_string()),
        "quotes stripped: {mods:?}"
    );
}

#[test]
fn javascript_symbols_and_in_filter() {
    let f = Fixture::new();
    f.write(
        "app.js",
        "const helper = (x) => x + 1;\n\
         function run() {\n  // process here\n  return helper(\"process\");\n}\n",
    );
    let syms = field_set(&f.run(&["symbols"]), "name");
    assert!(syms.contains(&"helper".to_string()) && syms.contains(&"run".to_string()));

    // 'process' appears in a comment and a string, never as code.
    let count = |kind: &str| -> i64 {
        f.run(&["search", "process", "--in", kind, "-c"])["data"]["counts"]
            .as_array()
            .unwrap()
            .iter()
            .map(|c| c["count"].as_i64().unwrap())
            .sum()
    };
    assert_eq!(count("code"), 0);
    assert_eq!(count("comments"), 1);
    assert_eq!(count("strings"), 1);
}

#[test]
fn go_symbols_and_imports() {
    let f = Fixture::new();
    f.write(
        "svc.go",
        "package main\n\n\
         import \"fmt\"\n\n\
         type Server struct { addr string }\n\n\
         type Handler interface { Serve() }\n\n\
         func New() *Server { return nil }\n\n\
         func (s *Server) Start() error { fmt.Println(\"x\"); return nil }\n",
    );
    let syms = f.run(&["symbols"]);
    let by_kind = |k: &str| -> Vec<String> {
        syms["data"]
            .as_array()
            .unwrap()
            .iter()
            .filter(|r| r["kind"] == k)
            .map(|r| r["name"].as_str().unwrap().to_string())
            .collect()
    };
    assert!(by_kind("struct").contains(&"Server".to_string()));
    assert!(by_kind("interface").contains(&"Handler".to_string()));
    assert!(by_kind("function").contains(&"New".to_string()));
    assert!(by_kind("method").contains(&"Start".to_string()));

    let mods = field_set(&f.run(&["imports"]), "module");
    assert!(
        mods.contains(&"fmt".to_string()),
        "go import quotes stripped: {mods:?}"
    );
}

#[test]
fn java_symbols_and_imports() {
    let f = Fixture::new();
    f.write(
        "S.java",
        "package p;\n\
         import java.util.List;\n\
         public interface H { void go(); }\n\
         public class S implements H {\n  public S() {}\n  public void go() {}\n}\n",
    );
    let syms = f.run(&["symbols"]);
    let by_kind = |k: &str| -> Vec<String> {
        syms["data"]
            .as_array()
            .unwrap()
            .iter()
            .filter(|r| r["kind"] == k)
            .map(|r| r["name"].as_str().unwrap().to_string())
            .collect()
    };
    assert!(by_kind("interface").contains(&"H".to_string()));
    assert!(by_kind("class").contains(&"S".to_string()));
    assert!(by_kind("method").contains(&"go".to_string()));
    assert!(field_set(&f.run(&["imports"]), "module").contains(&"java.util.List".to_string()));
}

#[test]
fn ruby_symbols_and_nesting() {
    let f = Fixture::new();
    f.write(
        "s.rb",
        "module A\n  class B\n    def m\n    end\n  end\nend\n\ndef top\nend\n",
    );
    let syms = field_set(&f.run(&["symbols"]), "name");
    for want in ["A", "B", "m", "top"] {
        assert!(syms.contains(&want.to_string()), "missing {want}: {syms:?}");
    }
    // Method m nests under class B under module A.
    let out = f.run(&["outline", "s.rb"]);
    let m = out["data"]
        .as_array()
        .unwrap()
        .iter()
        .find(|r| r["name"] == "m")
        .unwrap();
    assert_eq!(m["depth"], 2);
}

#[test]
fn c_symbols_and_includes() {
    let f = Fixture::new();
    f.write(
        "s.c",
        "#include <stdio.h>\n#include \"local.h\"\nstruct S { int x; };\nint f(void) { return 0; }\nchar *g(void) { return 0; }\n",
    );
    let syms = f.run(&["symbols"]);
    let names = field_set(&syms, "name");
    assert!(names.contains(&"S".to_string()) && names.contains(&"f".to_string()));
    assert!(
        names.contains(&"g".to_string()),
        "pointer-returning fn: {names:?}"
    );
    let mods = field_set(&f.run(&["imports"]), "module");
    assert!(
        mods.contains(&"stdio.h".to_string()),
        "angle include stripped: {mods:?}"
    );
    assert!(
        mods.contains(&"local.h".to_string()),
        "quoted include stripped: {mods:?}"
    );
}

#[test]
fn cpp_symbols_and_includes() {
    let f = Fixture::new();
    f.write(
        "a.cpp",
        "#include <vector>\nnamespace app {\nclass Widget { public: void render(); };\nvoid Widget::render() {}\nint helper(int x) { return x; }\n}\n",
    );
    let names = field_set(&f.run(&["symbols"]), "name");
    for want in ["Widget", "app", "helper", "Widget::render"] {
        assert!(
            names.contains(&want.to_string()),
            "missing {want}: {names:?}"
        );
    }
    assert!(field_set(&f.run(&["imports"]), "module").contains(&"vector".to_string()));
}

#[test]
fn csharp_symbols_and_usings() {
    let f = Fixture::new();
    f.write(
        "a.cs",
        "using System;\nnamespace App {\n  public interface IH { void H(); }\n  public class S : IH { public S() {} public void H() {} }\n}\n",
    );
    let syms = f.run(&["symbols"]);
    let by_kind = |k: &str| -> Vec<String> {
        syms["data"]
            .as_array()
            .unwrap()
            .iter()
            .filter(|r| r["kind"] == k)
            .map(|r| r["name"].as_str().unwrap().to_string())
            .collect()
    };
    assert!(by_kind("interface").contains(&"IH".to_string()));
    assert!(by_kind("class").contains(&"S".to_string()));
    assert!(by_kind("namespace").contains(&"App".to_string()));
    assert!(field_set(&f.run(&["imports"]), "module").contains(&"System".to_string()));
}

#[test]
fn php_symbols_and_uses() {
    let f = Fixture::new();
    f.write(
        "a.php",
        "<?php\nnamespace App;\nuse App\\Models\\User;\ninterface H { public function h(); }\nclass S implements H { public function h() { return 1; } }\nfunction top($x) { return $x; }\n",
    );
    let names = field_set(&f.run(&["symbols"]), "name");
    for want in ["H", "S", "h", "top"] {
        assert!(
            names.contains(&want.to_string()),
            "missing {want}: {names:?}"
        );
    }
    assert!(field_set(&f.run(&["imports"]), "module").contains(&"App\\Models\\User".to_string()));
}

// ---------------------------------------------------------------------------
// Signatures / docstrings / outline
// ---------------------------------------------------------------------------

#[test]
fn definitions_include_signature_and_rust_doc() {
    let f = Fixture::new();
    f.write(
        "a.rs",
        "/// Adds two numbers.\npub fn add(a: i32, b: i32) -> i32 {\n    a + b\n}\n",
    );
    let v = f.run(&["definitions", "add"]);
    let r = &v["data"][0];
    assert_eq!(r["signature"], "pub fn add(a: i32, b: i32) -> i32");
    assert_eq!(r["doc"], "Adds two numbers.");
}

#[test]
fn python_docstring_and_signature_extracted() {
    let f = Fixture::new();
    f.write(
        "m.py",
        "def greet():\n    \"\"\"Say hi.\"\"\"\n    return 1\n",
    );
    let v = f.run(&["definitions", "greet"]);
    let r = &v["data"][0];
    assert_eq!(r["signature"], "def greet():");
    assert_eq!(r["doc"], "Say hi.");
}

#[test]
fn api_lists_only_public_definitions_rust() {
    let f = Fixture::new();
    f.write(
        "a.rs",
        "pub fn exported() {}\n\
         fn internal() {}\n\
         pub(crate) fn crate_only() {}\n\
         pub struct Public;\n",
    );
    let names = field_set(&f.run(&["api", "a.rs"]), "name");
    assert!(
        names.contains(&"exported".to_string()),
        "pub fn missing: {names:?}"
    );
    assert!(
        names.contains(&"Public".to_string()),
        "pub struct missing: {names:?}"
    );
    assert!(
        !names.contains(&"internal".to_string()),
        "private fn leaked: {names:?}"
    );
    assert!(
        !names.contains(&"crate_only".to_string()),
        "pub(crate) is restricted, not public: {names:?}"
    );
}

#[test]
fn api_visibility_rules_per_language() {
    // Go: exported by capitalization.
    let g = Fixture::new();
    g.write(
        "m.go",
        "package m\nfunc Exported() {}\nfunc unexported() {}\n",
    );
    let gn = field_set(&g.run(&["api", "m.go"]), "name");
    assert!(
        gn.contains(&"Exported".to_string()) && !gn.contains(&"unexported".to_string()),
        "{gn:?}"
    );

    // Python: public unless leading underscore.
    let p = Fixture::new();
    p.write(
        "m.py",
        "def public():\n    pass\ndef _private():\n    pass\n",
    );
    let pn = field_set(&p.run(&["api", "m.py"]), "name");
    assert!(
        pn.contains(&"public".to_string()) && !pn.contains(&"_private".to_string()),
        "{pn:?}"
    );

    // JS: exported iff `export`.
    let j = Fixture::new();
    j.write("m.js", "export function shown() {}\nfunction hidden() {}\n");
    let jn = field_set(&j.run(&["api", "m.js"]), "name");
    assert!(
        jn.contains(&"shown".to_string()) && !jn.contains(&"hidden".to_string()),
        "{jn:?}"
    );
}

#[test]
fn api_kind_filter_and_signature() {
    let f = Fixture::new();
    f.write("a.rs", "/// A doc.\npub fn func() {}\npub struct S;\n");
    // --kind narrows to one kind.
    let v = f.run(&["api", "a.rs", "--kind", "function"]);
    let rows = v["data"].as_array().unwrap();
    assert_eq!(
        rows.len(),
        1,
        "kind filter should return only the function: {rows:?}"
    );
    assert_eq!(rows[0]["name"], "func");
    assert_eq!(
        rows[0]["signature"], "pub fn func()",
        "signature is carried"
    );
}

#[test]
fn symbols_signature_is_opt_in() {
    let f = Fixture::new();
    f.write("a.rs", "fn solo() {}\n");

    let plain = f.run(&["symbols"]);
    assert!(
        plain["data"][0].get("signature").is_none(),
        "should be off by default"
    );

    let with = f.run(&["symbols", "--signatures"]);
    assert_eq!(with["data"][0]["signature"], "fn solo()");
}

#[test]
fn outline_orders_by_position_and_nests() {
    let f = Fixture::new();
    f.write(
        "m.py",
        "class C:\n    def m(self):\n        pass\n\ndef f():\n    pass\n",
    );
    let v = f.run(&["outline", "m.py"]);
    let rows = v["data"].as_array().unwrap();
    let names: Vec<&str> = rows.iter().map(|r| r["name"].as_str().unwrap()).collect();
    assert_eq!(names, ["C", "m", "f"], "source order");

    let depth = |name: &str| {
        rows.iter().find(|r| r["name"] == name).unwrap()["depth"]
            .as_u64()
            .unwrap()
    };
    assert_eq!(depth("C"), 0);
    assert_eq!(depth("m"), 1, "method nested in class");
    assert_eq!(depth("f"), 0);
}

// ---------------------------------------------------------------------------
// references / at / changed --symbols
// ---------------------------------------------------------------------------

#[test]
fn references_flags_definition_vs_use_with_enclosing_symbol() {
    let f = Fixture::new();
    f.write(
        "m.py",
        "def helper():\n    return 1\n\ndef caller():\n    return helper()\n",
    );
    let v = f.run(&["references", "helper"]);
    let refs = v["data"]["references"].as_array().unwrap();

    let def = refs.iter().find(|r| r["line"] == 1).unwrap();
    assert_eq!(def["definition"], true, "line 1 is the definition site");

    let use_site = refs.iter().find(|r| r["line"] == 5).unwrap();
    assert!(
        use_site.get("definition").is_none(),
        "use site is not a definition"
    );
    assert_eq!(use_site["symbol"], "caller", "use is inside caller");
}

#[test]
fn callers_callees_and_about() {
    let f = Fixture::new();
    f.write(
        "a.rs",
        "fn helper() {}\n\
         fn caller() { helper(); }\n\
         fn other() { helper(); }\n\
         // helper mentioned in a comment\n",
    );

    // callers = distinct code callers, excluding the definition and comments.
    let callers = f.run(&["callers", "helper"]);
    let syms: Vec<String> = callers["data"]
        .as_array()
        .unwrap()
        .iter()
        .filter_map(|r| r["name"].as_str().map(String::from))
        .collect();
    assert!(
        syms.contains(&"caller".to_string()) && syms.contains(&"other".to_string()),
        "{syms:?}"
    );
    assert!(
        !syms.contains(&"helper".to_string()),
        "own definition is not a caller"
    );
    // The comment mention must not appear (code-only).
    assert_eq!(
        callers["data"].as_array().unwrap().len(),
        2,
        "comment mention leaked"
    );

    // callees = definitions referenced inside the body.
    let callees = field_set(&f.run(&["callees", "caller"]), "name");
    assert!(callees.contains(&"helper".to_string()), "{callees:?}");

    // about = one-call dossier.
    let about = f.run(&["about", "helper"]);
    let d = &about["data"];
    assert_eq!(d["definitions"][0]["name"], "helper");
    assert_eq!(d["callers"].as_array().unwrap().len(), 2);
}

#[test]
fn callers_prefer_a_usage_site_over_an_import_as_the_anchor() {
    let f = Fixture::new();
    // A Svelte component's definition spans its whole file, so it encloses its
    // own imports. Only one row survives per (file, enclosing symbol), so the
    // import at line 2 and the `<Card />` at line 6 compete — and the import
    // used to win by being first, hiding where the component is actually used.
    f.write("Card.svelte", "<div><slot /></div>\n");
    f.write(
        "Page.svelte",
        "<script>\n  import Card from './Card.svelte';\n</script>\n\n\
         <main>\n  <Card />\n</main>\n",
    );
    let v = f.run(&["callers", "Card"]);
    let rows = v["data"].as_array().unwrap();
    assert_eq!(rows.len(), 1, "{rows:?}");
    assert_eq!(rows[0]["path"], "Page.svelte");
    assert_eq!(
        rows[0]["line"], 6,
        "anchor must be <Card />, not the import"
    );

    // But an import is still a valid last-resort anchor: the default `callers`
    // is a textual superset (`--calls` is the way to narrow it), so a file that
    // imports the name and never uses it stays listed.
    f.write("lib.js", "export function helper() { return 1; }\n");
    f.write(
        "app.js",
        "import { helper } from './lib';\n\nfunction run() {\n  return helper();\n}\n",
    );
    f.write(
        "shelf.js",
        "import { helper } from './lib';\nexport const x = 1;\n",
    );

    let v = f.run(&["callers", "helper"]);
    let rows = v["data"].as_array().unwrap();
    let paths: Vec<&str> = rows.iter().map(|r| r["path"].as_str().unwrap()).collect();
    assert!(
        paths.contains(&"shelf.js"),
        "superset default lost a row: {paths:?}"
    );
    let run_row = rows
        .iter()
        .find(|r| r["name"] == "run")
        .expect("run caller");
    assert_eq!(run_row["line"], 4);
}

#[test]
fn callers_resolved_prunes_unreachable_and_tags_via() {
    let f = Fixture::new();
    f.write(
        "def.rs",
        "pub fn target() {}\nfn self_caller() { target(); }\n",
    );
    f.write(
        "user.rs",
        "use crate::def::target;\nfn user() { target(); }\n",
    );
    f.write(
        "stranger.rs",
        "fn stranger() { let target = 1; let _ = target; }\n",
    );

    // Default (name-based) includes the unrelated `target` in stranger.rs.
    let all = field_set(&f.run(&["callers", "target"]), "path");
    assert!(
        all.contains(&"stranger.rs".to_string()),
        "default should include stranger: {all:?}"
    );

    // --resolved drops the reference that can't reach a definition, and tags the
    // rest by how they connect.
    let res = f.run(&["callers", "target", "--resolved"]);
    let rows = res["data"].as_array().unwrap();
    let via = |sym: &str| {
        rows.iter()
            .find(|r| r["name"] == sym)
            .map(|r| r["via"].as_str().unwrap().to_string())
    };
    assert_eq!(via("self_caller"), Some("same-file".into()), "{rows:?}");
    assert_eq!(via("user"), Some("imported".into()), "{rows:?}");
    assert!(
        !rows.iter().any(|r| r["path"] == "stranger.rs"),
        "unreachable reference should be pruned: {rows:?}"
    );
}

#[test]
fn callees_resolved_prunes_unreachable_same_named_defs() {
    let f = Fixture::new();
    // `run` (in core.rs) calls `work`; core.rs defines its own `work`.
    f.write("core.rs", "pub fn work() {}\npub fn run() { work(); }\n");
    // An unrelated `work` in a file core.rs can't reach through imports.
    f.write("orphan.rs", "pub fn work() {}\n");

    // Default (name-based) over-matches: both `work` definitions show up.
    let all = field_set(&f.run(&["callees", "run"]), "path");
    assert!(all.contains(&"core.rs".to_string()), "{all:?}");
    assert!(
        all.contains(&"orphan.rs".to_string()),
        "default should over-match the unrelated same-named def: {all:?}"
    );

    // --resolved keeps only the reachable (same-file) callee, tags it via, and
    // prunes the unrelated same-named def in the unreachable file.
    let res = f.run(&["callees", "run", "--resolved"]);
    let rows = res["data"].as_array().unwrap();
    assert!(
        rows.iter()
            .any(|r| r["path"] == "core.rs" && r["via"] == "same-file"),
        "{rows:?}"
    );
    assert!(
        !rows.iter().any(|r| r["path"] == "orphan.rs"),
        "unreachable same-named callee should be pruned: {rows:?}"
    );
}

#[test]
fn resolved_callees_follow_typescript_aliases_reexports_and_workspaces() {
    let f = Fixture::new();
    f.write(
        "tsconfig.json",
        r#"{"compilerOptions":{"baseUrl":".","paths":{"@/*":["src/*"]}}}"#,
    );
    f.write(
        "package.json",
        r#"{"private":true,"workspaces":["packages/*"]}"#,
    );
    f.write(
        "app/main.ts",
        "import { aliasFn } from '@/alias';\nimport { reexportFn } from './barrel';\nimport { workspaceFn } from '@scope/workspace';\nexport function main() { aliasFn(); reexportFn(); workspaceFn(); }\n",
    );
    f.write("src/alias.ts", "export function aliasFn() {}\n");
    f.write(
        "app/barrel.ts",
        "export { reexportFn } from './reexport';\n",
    );
    f.write("app/reexport.ts", "export function reexportFn() {}\n");
    f.write(
        "packages/workspace/package.json",
        r#"{"name":"@scope/workspace","source":"src/index.ts"}"#,
    );
    f.write(
        "packages/workspace/src/index.ts",
        "export function workspaceFn() {}\n",
    );

    let rows = f.run(&["callees", "main", "--resolved"])["data"]
        .as_array()
        .unwrap()
        .clone();
    for expected in ["aliasFn", "reexportFn", "workspaceFn"] {
        let row = rows
            .iter()
            .find(|row| row["name"] == expected)
            .unwrap_or_else(|| panic!("missing {expected}: {rows:?}"));
        assert_ne!(
            row["via"], "unresolved-project-import",
            "resolved project import was marked uncertain: {row}"
        );
    }
    let dependencies = field_set(&f.run(&["deps", "app/main.ts", "--depth", "2"]), "path");
    for expected in [
        "src/alias.ts",
        "app/barrel.ts",
        "app/reexport.ts",
        "packages/workspace/src/index.ts",
    ] {
        assert!(
            dependencies.contains(&expected.to_string()),
            "dependency edge to {expected} is missing: {dependencies:?}"
        );
    }
}

#[test]
fn typescript_jsonc_config_preserves_alias_edges_and_resolved_callees() {
    let f = Fixture::new();
    f.write(
        "tsconfig.json",
        r#"{
            // URLs and comment markers inside strings must stay intact.
            "schema": "https://example.com/schema//config",
            "escaped": "a\"//b",
            "compilerOptions": {
                /* Project-local aliases. */
                "paths": {"@/*": ["src/*", /* fallback */],},
            },
        }"#,
    );
    f.write(
        "app/main.ts",
        "import { helper } from '@/lib';\nexport function main() { helper(); }\n",
    );
    f.write("src/lib.ts", "export function helper() {}\n");
    f.write("other/lib.ts", "export function helper() {}\n");

    assert_eq!(
        field_set(&f.run(&["deps", "app/main.ts"]), "path"),
        ["src/lib.ts"]
    );
    assert_eq!(
        field_set(&f.run(&["callees", "main", "--resolved"]), "path"),
        ["src/lib.ts"]
    );
}

#[test]
fn malformed_typescript_config_reports_the_file_and_parse_error() {
    let f = Fixture::new();
    f.write("main.ts", "import { helper } from '@/lib';\n");
    for config in [
        "{\n  \"compilerOptions\": invalid\n}",
        "{\"compilerOptions\": { /* never closed",
        "{\"compilerOptions\": {\"paths\": {\"@/*\": [,]}}}",
    ] {
        f.write("tsconfig.json", config);
        let out = f.raw(&["deps", "main.ts"]);
        let error = String::from_utf8_lossy(&out.stderr);
        assert!(!out.status.success(), "invalid config succeeded: {config}");
        assert!(error.contains("tsconfig.json"), "{error}");
        assert!(
            error.contains("line") && error.contains("column"),
            "{error}"
        );
    }
    f.write(
        "tsconfig.json",
        r#"{"compilerOptions":{"paths":{"@/*":["src/*"]}}}"#,
    );
    f.write("src/lib.ts", "export function helper() {}\n");
    assert_eq!(
        field_set(&f.run(&["deps", "main.ts"]), "path"),
        ["src/lib.ts"]
    );
}

#[test]
fn unresolved_typescript_aliases_keep_candidates_with_a_reason() {
    let f = Fixture::new();
    f.write(
        "tsconfig.json",
        r#"{"compilerOptions":{"baseUrl":".","paths":{"@/*":["src/*"]}}}"#,
    );
    f.write(
        "app/main.ts",
        "import { helper } from '@/missing';\nexport function main() { helper(); }\n",
    );
    f.write("elsewhere/helper.ts", "export function helper() {}\n");

    let callees = f.run(&["callees", "main", "--resolved"])["data"]
        .as_array()
        .unwrap()
        .clone();
    assert!(
        callees
            .iter()
            .any(|row| { row["name"] == "helper" && row["via"] == "unresolved-project-import" }),
        "an unresolved configured alias must not silently prune candidates: {callees:?}"
    );
    let external = f.run(&["deps", "--external"])["data"]
        .as_array()
        .unwrap()
        .clone();
    assert!(
        external.iter().any(|row| {
            row["module"] == "@/missing"
                && row["resolution_reason"]
                    .as_str()
                    .is_some_and(|reason| reason.contains("TypeScript path alias"))
        }),
        "expected an explanation for the unresolved alias: {external:?}"
    );
}

#[test]
fn project_aliases_belong_to_each_importers_nearest_config() {
    let f = Fixture::new();
    f.write(
        "tsconfig.json",
        r#"{"compilerOptions":{"paths":{"@/*":["decoy/*"]}}}"#,
    );
    f.write("decoy/lib.ts", "export function helper() {}\n");
    f.git_init_commit();
    for project in ["one", "two"] {
        f.write(
            &format!("{project}/tsconfig.json"),
            r#"{"compilerOptions":{"baseUrl":".","paths":{"@/*":["src/*"]}}}"#,
        );
        f.write(
            &format!("{project}/main.ts"),
            "import { helper } from '@/lib';\nexport function run() { helper(); }\n",
        );
        f.write(
            &format!("{project}/src/lib.ts"),
            "export function helper() {}\n",
        );
        assert_eq!(
            field_set(&f.run(&["deps", &format!("{project}/main.ts")]), "path"),
            [format!("{project}/src/lib.ts")]
        );
        assert_eq!(
            field_set(
                &f.run(&[
                    "callees",
                    "run",
                    "--path",
                    &format!("{project}/main.ts"),
                    "--resolved"
                ]),
                "path"
            ),
            [format!("{project}/src/lib.ts")]
        );
    }
}

#[test]
fn project_inheritance_preserves_path_origins_and_child_base_url_overrides() {
    let f = Fixture::new();
    f.write(
        "config/base.json",
        r#"{"compilerOptions":{"paths":{"@/*":["src/*"]}}}"#,
    );
    f.write("config/src/lib.ts", "export function helper() {}\n");
    f.write("app/tsconfig.json", r#"{"extends":"../config/base"}"#);
    f.write("app/main.ts", "import { helper } from '@/lib';\n");
    f.write("app/src/lib.ts", "export function helper() {}\n");
    assert_eq!(
        field_set(&f.run(&["deps", "app/main.ts"]), "path"),
        ["config/src/lib.ts"]
    );
    f.write(
        "app/tsconfig.json",
        r#"{"extends":"../config/base","compilerOptions":{"baseUrl":"."}}"#,
    );
    assert_eq!(
        field_set(&f.run(&["deps", "app/main.ts"]), "path"),
        ["app/src/lib.ts"]
    );
}

#[test]
fn project_references_select_named_configs_without_sharing_aliases() {
    let f = Fixture::new();
    f.write("tsconfig.json", r#"{"files":[],"references":[{"path":"./tsconfig.app.json"},{"path":"./tsconfig.test.json"}]}"#);
    f.write(
        "tsconfig.app.json",
        r#"{"include":["app/**/*"],"compilerOptions":{"paths":{"@/*":["app/*"]}}}"#,
    );
    f.write(
        "tsconfig.test.json",
        r#"{"include":["test/**/*"],"compilerOptions":{"paths":{"@/*":["test/*"]}}}"#,
    );
    for directory in ["app", "test"] {
        f.write(
            &format!("{directory}/main.ts"),
            "import { helper } from '@/lib';\n",
        );
        f.write(
            &format!("{directory}/lib.ts"),
            "export function helper() {}\n",
        );
        assert_eq!(
            field_set(&f.run(&["deps", &format!("{directory}/main.ts")]), "path"),
            [format!("{directory}/lib.ts")]
        );
    }
}

#[test]
fn project_alias_specificity_and_child_paths_replacement_are_respected() {
    let f = Fixture::new();
    f.write(
        "base.json",
        r#"{"compilerOptions":{"paths":{"old/*":["old/*"]}}}"#,
    );
    f.write("tsconfig.json", r#"{"extends":"./base.json","compilerOptions":{"paths":{"@/*":["generic/*"],"@/lib":["specific/lib"],"@/special/*":["special/*"]}}}"#);
    f.write(
        "main.ts",
        "import '@/lib';\nimport '@/special/lib';\nimport 'old/lib';\n",
    );
    for directory in ["generic", "generic/special", "specific", "special", "old"] {
        f.write(
            &format!("{directory}/lib.ts"),
            "export function helper() {}\n",
        );
    }
    assert_eq!(
        field_set(&f.run(&["deps", "main.ts"]), "path"),
        ["special/lib.ts", "specific/lib.ts"]
    );
}

#[test]
fn project_inheritance_errors_name_the_config_and_do_not_guess_edges() {
    let f = Fixture::new();
    f.write("main.ts", "import '@/lib';\n");
    f.write("lib.ts", "export function helper() {}\n");
    for (config, reason) in [
        (r#"{"extends":"./missing"}"#, "missing"),
        (r#"{"extends":"./tsconfig.json"}"#, "cycle"),
        (r#"{"extends":"../outside.json"}"#, "outside"),
    ] {
        f.write("tsconfig.json", config);
        let out = f.raw(&["deps", "main.ts"]);
        let error = String::from_utf8_lossy(&out.stderr);
        assert!(!out.status.success(), "{config} silently succeeded");
        assert!(
            error.contains("tsconfig.json") && error.contains(reason),
            "{error}"
        );
    }
}

#[test]
fn project_imports_keep_excluded_helpers_in_the_importing_program() {
    let f = Fixture::new();
    f.write(
        "tsconfig.json",
        r#"{"compilerOptions":{"paths":{"@/*":["root/*"]}}}"#,
    );
    f.write("root/value.ts", "export const value = 0;\n");
    f.write(
        "pkg/tsconfig.json",
        r#"{"include":["src/main.ts"],"compilerOptions":{"paths":{"@/*":["src/*"]}}}"#,
    );
    f.write("pkg/src/main.ts", "import './helper';\n");
    f.write("pkg/src/helper.ts", "import { value } from '@/value';\n");
    f.write("pkg/src/value.ts", "export const value = 1;\n");
    assert_eq!(
        field_set(&f.run(&["deps", "pkg/src/helper.ts"]), "path"),
        ["pkg/src/value.ts"]
    );
}

#[test]
fn project_ignored_configs_block_parent_aliases() {
    let f = Fixture::new();
    f.write(".ignore", "pkg/tsconfig.json\n");
    f.write(
        "tsconfig.json",
        r#"{"compilerOptions":{"paths":{"@/*":["root/*"]}}}"#,
    );
    f.write("root/value.ts", "export const value = 0;\n");
    f.write(
        "pkg/tsconfig.json",
        r#"{"compilerOptions":{"paths":{"@/*":["src/*"]}}}"#,
    );
    f.write("pkg/main.ts", "import { value } from '@/value';\n");
    f.write("pkg/src/value.ts", "export const value = 1;\n");
    let rows = f.run(&["deps", "pkg/main.ts"]);
    assert_eq!(field_set(&rows, "path"), Vec::<String>::new());
    let external = f.run(&["deps", "--external"]);
    assert!(
        external["data"]
            .as_array()
            .unwrap()
            .iter()
            .any(|row| row["module"] == "@/value"
                && row["resolution_reason"]
                    .as_str()
                    .is_some_and(|r| r.contains("not indexed"))),
        "{external}"
    );
}

#[test]
fn project_default_out_dir_exclusion_and_literal_glob_braces_match_typescript() {
    let f = Fixture::new();
    f.write(
        "tsconfig.json",
        r#"{"compilerOptions":{"paths":{"@/*":["root/*"]}}}"#,
    );
    f.write("root/value.ts", "export const value = 0;\n");
    f.write(
        "pkg/tsconfig.json",
        r#"{"compilerOptions":{"outDir":"dist","paths":{"@/*":["src/*"]}}}"#,
    );
    f.write("pkg/dist/main.ts", "import { value } from '@/value';\n");
    f.write("pkg/src/value.ts", "export const value = 1;\n");
    assert_eq!(
        field_set(&f.run(&["deps", "pkg/dist/main.ts"]), "path"),
        ["root/value.ts"]
    );
    f.write(
        "pkg/tsconfig.json",
        r#"{"include":["src/{one,two}.ts"],"compilerOptions":{"paths":{"@/*":["src/*"]}}}"#,
    );
    f.write("pkg/src/one.ts", "import { value } from '@/value';\n");
    assert_eq!(
        field_set(&f.run(&["deps", "pkg/src/one.ts"]), "path"),
        ["root/value.ts"]
    );
}

#[test]
fn project_typescript_config_takes_precedence_over_jsconfig() {
    let f = Fixture::new();
    f.write(
        "tsconfig.json",
        r#"{"compilerOptions":{"paths":{"@/*":["typescript/*"]}}}"#,
    );
    f.write(
        "jsconfig.json",
        r#"{"compilerOptions":{"paths":{"@/*":["javascript/*"]}}}"#,
    );
    f.write("main.ts", "import '@/lib';\n");
    f.write("typescript/lib.ts", "export const value = 1;\n");
    f.write("javascript/lib.ts", "export const value = 2;\n");
    assert_eq!(
        field_set(&f.run(&["deps", "main.ts"]), "path"),
        ["typescript/lib.ts"]
    );
}

#[test]
fn project_multiple_extends_and_ambiguous_ownership_remain_explicit() {
    let f = Fixture::new();
    f.write(
        "first.json",
        r#"{"compilerOptions":{"paths":{"@/*":["first/*"]}}}"#,
    );
    f.write(
        "second.json",
        r#"{"compilerOptions":{"paths":{"@/*":["second/*"]}}}"#,
    );
    f.write(
        "tsconfig.json",
        r#"{"extends":["./first.json","./second.json"]}"#,
    );
    f.write("first/lib.ts", "export const value = 1;\n");
    f.write("second/lib.ts", "export const value = 2;\n");
    f.write("main.ts", "import '@/lib';\n");
    assert_eq!(
        field_set(&f.run(&["deps", "main.ts"]), "path"),
        ["second/lib.ts"]
    );
    f.write(
        "tsconfig.json",
        r#"{"files":[],"references":[{"path":"./first.json"},{"path":"./second.json"}]}"#,
    );
    let external = f.run(&["deps", "--external"]);
    assert!(
        external["data"]
            .as_array()
            .unwrap()
            .iter()
            .any(|r| r["resolution_reason"]
                .as_str()
                .is_some_and(|s| s.contains("ambiguous project ownership"))),
        "{external}"
    );
}

#[test]
fn workspace_exports_control_root_subpaths_and_private_paths() {
    let f = Fixture::new();
    f.write("package.json", r#"{"workspaces":["packages/*"]}"#);
    f.write("packages/lib/package.json", r#"{"name":"@scope/lib","main":"src/legacy.ts","exports":{".":"./src/public.ts","./feature/*":"./src/feature/*.ts","./feature/private/*":null}}"#);
    for path in [
        "public",
        "legacy",
        "feature/one",
        "feature/private/secret",
        "internal",
    ] {
        f.write(
            &format!("packages/lib/src/{path}.ts"),
            "export const value = 1;\n",
        );
    }
    f.write("app.ts", "import '@scope/lib';\nimport '@scope/lib/feature/one';\nimport '@scope/lib/feature/private/secret';\nimport '@scope/lib/src/internal';\n");
    assert_eq!(
        field_set(&f.run(&["deps", "app.ts"]), "path"),
        [
            "packages/lib/src/feature/one.ts",
            "packages/lib/src/public.ts"
        ]
    );
    let external = f.run(&["deps", "--external"]);
    for module in [
        "@scope/lib/feature/private/secret",
        "@scope/lib/src/internal",
    ] {
        assert!(
            external["data"]
                .as_array()
                .unwrap()
                .iter()
                .any(|r| r["module"] == module
                    && r["resolution_reason"]
                        .as_str()
                        .is_some_and(|s| s.contains("exports"))),
            "{external}"
        );
    }
}

#[test]
fn workspace_conditional_exports_keep_uncertainty_instead_of_legacy_fallback() {
    let f = Fixture::new();
    f.write("package.json", r#"{"workspaces":["packages/*"]}"#);
    f.write(
        "packages/lib/package.json",
        r#"{"name":"lib","exports":{"import":"./src/esm.ts","require":"./src/cjs.ts"}}"#,
    );
    for file in ["index", "esm", "cjs"] {
        f.write(
            &format!("packages/lib/src/{file}.ts"),
            "export function helper() {}\n",
        );
    }
    f.write(
        "app.ts",
        "import { helper } from 'lib';\nexport function run() { helper(); }\n",
    );
    assert_eq!(
        field_set(&f.run(&["deps", "app.ts"]), "path"),
        Vec::<String>::new()
    );
    let external = f.run(&["deps", "--external"]);
    assert!(
        external["data"][0]["resolution_reason"]
            .as_str()
            .is_some_and(|s| s.contains("conditional exports")),
        "{external}"
    );
    f.write(
        "packages/lib/package.json",
        r#"{"name":"lib","exports":{"import":"./src/esm.ts","default":"./src/esm.ts"}}"#,
    );
    assert_eq!(
        field_set(&f.run(&["deps", "app.ts"]), "path"),
        ["packages/lib/src/esm.ts"]
    );
}

#[test]
fn workspace_pnpm_patterns_include_nested_packages_and_apply_exclusions() {
    let f = Fixture::new();
    f.write("package.json", r#"{"name":"root","private":true}"#);
    f.write(
        "pnpm-workspace.yaml",
        "# workspace packages\npackages:\n  - 'packages/**'\n  - '!packages/**/excluded'\n",
    );
    f.write(
        "packages/nested/lib/package.json",
        r#"{"name":"included","exports":"./src.ts"}"#,
    );
    f.write("packages/nested/lib/src.ts", "export const value = 1;\n");
    f.write(
        "packages/nested/excluded/package.json",
        r#"{"name":"excluded","exports":"./src.ts"}"#,
    );
    f.write(
        "packages/nested/excluded/src.ts",
        "export const value = 2;\n",
    );
    f.write("app.ts", "import 'included';\nimport 'excluded';\n");
    assert_eq!(
        field_set(&f.run(&["deps", "app.ts"]), "path"),
        ["packages/nested/lib/src.ts"]
    );
}

#[test]
fn workspace_invalid_separators_and_numeric_conditions_never_create_edges() {
    let f = Fixture::new();
    f.write("package.json", r#"{"workspaces":["packages/*"]}"#);
    f.write("packages/lib/src.ts", "export const value = 1;\n");
    f.write("packages/lib/other.ts", "export const value = 2;\n");
    f.write(
        "packages/lib/package.json",
        r#"{"name":"lib","exports":{"./x":"./src.ts"}}"#,
    );
    f.write("app.ts", "import 'lib//x';\n");
    assert_eq!(
        field_set(&f.run(&["deps", "app.ts"]), "path"),
        Vec::<String>::new()
    );
    f.write("app.ts", "import 'lib';\n");
    f.write(
        "packages/lib/package.json",
        r#"{"name":"lib","exports":{"0":"./src.ts","default":"./src.ts"}}"#,
    );
    assert_eq!(
        field_set(&f.run(&["deps", "app.ts"]), "path"),
        Vec::<String>::new()
    );
    f.write(
        "packages/lib/package.json",
        r#"{"name":"lib","exports":{"default":"./src.ts","import":"./other.ts"}}"#,
    );
    assert_eq!(
        field_set(&f.run(&["deps", "app.ts"]), "path"),
        ["packages/lib/src.ts"]
    );
}

#[test]
fn callees_use_an_imported_module_receiver_to_narrow_same_named_defs() {
    let f = Fixture::new();
    f.write("context.rs", "pub fn build() {}\n");
    f.write("other.rs", "pub fn build() {}\n");
    f.write(
        "app.rs",
        "use crate::context;\npub fn run() { context::build(); }\n",
    );

    let paths = field_set(&f.run(&["callees", "run"]), "path");
    assert_eq!(
        paths,
        ["context.rs"],
        "module-qualified call was not narrowed: {paths:?}"
    );
}

#[test]
fn callees_cache_keeps_each_callers_module_constraints_and_refreshes_after_edits() {
    let f = Fixture::new();
    for module in ["context", "other", "third"] {
        f.write(&format!("{module}.rs"), "pub fn build() {}\n");
    }
    let source = "use crate::context;\nuse crate::other;\nuse crate::third;\n                  fn left() { context::build(); }\n                  fn right() { other::build(); }\n                  fn entry() { left(); right(); }\n";
    f.write("app.rs", source);
    let paths = |v: &Value| {
        let mut paths: Vec<_> = v["data"]
            .as_array()
            .unwrap()
            .iter()
            .filter(|row| row["name"] == "build")
            .map(|row| row["path"].as_str().unwrap().to_string())
            .collect();
        paths.sort();
        paths
    };
    let first = f.run(&["callees", "entry", "--depth", "2", "--limit", "0"]);
    assert_eq!(paths(&first), ["context.rs", "other.rs"]);
    let scoped = f.run(&[
        "callees", "entry", "--depth", "2", "--path", "app.rs", "--limit", "0",
    ]);
    assert_eq!(scoped["data"], first["data"]);
    f.write("app.rs", &source.replace("context::build", "third::build"));
    let edited = f.run(&["callees", "entry", "--depth", "2", "--limit", "0"]);
    assert_eq!(paths(&edited), ["other.rs", "third.rs"]);
}

#[test]
fn callees_do_not_treat_fields_or_variants_as_callable_definitions() {
    let f = Fixture::new();
    f.write(
        "a.rs",
        "struct Worker { run: fn() }\n\
         enum State { run }\n\
         fn run() {}\n\
         fn caller() { run(); }\n",
    );

    let rows = f.run(&["callees", "caller"])["data"]
        .as_array()
        .unwrap()
        .clone();
    let runs: Vec<&Value> = rows.iter().filter(|r| r["name"] == "run").collect();
    assert_eq!(
        runs.len(),
        1,
        "a call should resolve only to callable definitions: {rows:?}"
    );
    assert_eq!(runs[0]["kind"], "function");
}

#[test]
fn a_word_boundary_follows_the_language_that_owns_the_bytes() {
    // `$` binds into an identifier only where a lexer allows it there, and only
    // where a lexer is running: a Rust macro metavariable, a JVM binary name in
    // a config file, and a template placeholder inside a string are all `$`
    // followed by a real occurrence. Treating `$` as an identifier byte
    // everywhere cost the superset-of-ripgrep guarantee in five languages.
    let f = Fixture::new();
    f.write(
        "m.rs",
        "macro_rules! m { ($bytes:expr) => { $bytes }; }\nfn bytes() {}\n",
    );
    f.write("cfg.json", "{ \"name\": \"Outer$PrivateRecord\" }\n");
    f.write(
        "s.java",
        "class T {\n  String m = \"Outer$PrivateRecord\";\n  int PrivateRecord = 1;\n}\n",
    );
    // In JavaScript code, though, `$locale` really is one identifier.
    f.write("a.js", "const $locale = 1;\nconst locale = 2;\n");

    let paths = |name: &str| -> Vec<String> {
        f.run(&["references", name, "--limit", "0"])["data"]["references"]
            .as_array()
            .unwrap()
            .iter()
            .filter_map(|r| r["path"].as_str().map(str::to_string))
            .collect()
    };
    assert!(
        paths("bytes").iter().any(|p| p == "m.rs"),
        "rust macro metavariable"
    );
    assert!(
        paths("PrivateRecord").iter().any(|p| p == "cfg.json"),
        "unclassified file"
    );
    assert!(
        paths("PrivateRecord").iter().any(|p| p == "s.java"),
        "inside a java string"
    );

    let v = f.run(&["references", "locale", "--limit", "0"]);
    let lines: Vec<i64> = v["data"]["references"]
        .as_array()
        .unwrap()
        .iter()
        .filter_map(|r| r["line"].as_i64())
        .collect();
    assert_eq!(
        lines,
        vec![2],
        "`$locale` is a different JS identifier: {v}"
    );
}

#[test]
fn a_word_boundary_reads_unicode_punctuation_as_punctuation() {
    // The boundary test asked `b >= 0x80`, i.e. "any non-ASCII byte is a
    // letter". True of a CJK ideograph, false of CJK punctuation, and the two
    // sit side by side in real prose.
    let f = Fixture::new();
    f.write("README.md", "Print it to console。See the example.\n");

    let v = f.run(&["search", "console", "-w"]);
    let hits = v["data"]["matches"].as_array().unwrap();
    assert!(!hits.is_empty(), "boundary rejected by a full stop: {v}");
}

#[test]
fn a_php_sigil_is_not_part_of_the_name_it_prefixes() {
    // The `$` boundary rule that keeps JavaScript's `$ZodError` whole made codeq
    // contradict its own index in PHP: it records `private $truncateAt;` under
    // the name `truncateAt`, then would not match the declaration it had just
    // indexed — so `references` could not find the field's own definition site.
    let f = Fixture::new();
    f.write(
        "a.php",
        "<?php\nclass S {\n    private $truncateAt;\n    public function get() { return $this->truncateAt; }\n}\n",
    );

    let v = f.run(&["references", "truncateAt"]);
    let rows = v["data"]["references"].as_array().unwrap();
    assert_eq!(rows.len(), 2, "declaration and use: {v}");
    assert!(
        rows.iter().any(|r| r["definition"] == true),
        "references must include the definition it indexed: {v}"
    );
}

#[test]
fn dollar_prefixed_javascript_identifiers_have_exact_boundaries() {
    let f = Fixture::new();
    f.write(
        "app.js",
        "export const $ZodError = () => {};\n\
         function caller() { $ZodError(); }\n\
         const almost$ZodError = 1;\n",
    );

    let refs = f.run(&["references", "$ZodError"]);
    let rows = refs["data"]["references"].as_array().unwrap();
    assert_eq!(rows.len(), 2, "definition and call only: {refs}");
    assert!(rows.iter().any(|r| r["definition"] == true));

    let callers = f.run(&["callers", "$ZodError", "--calls"]);
    assert_eq!(
        field_set(&callers, "name"),
        vec!["caller".to_string()],
        "{callers}"
    );

    let word = f.run(&["search", "$ZodError", "--fixed", "--word"]);
    assert_eq!(
        word["data"]["matches"].as_array().unwrap().len(),
        2,
        "{word}"
    );
}

#[test]
fn callers_examples_surface_the_call_expression() {
    let f = Fixture::new();
    f.write(
        "a.rs",
        "fn target(x: i32) {}\nfn caller() { target(41); }\n",
    );

    // Plain callers: the enclosing definition, no source text.
    let plain = f.run(&["callers", "target"]);
    let row = &plain["data"].as_array().unwrap()[0];
    assert_eq!(row["name"], "caller");
    assert!(
        row.get("text").is_none(),
        "plain callers should carry no source text: {row:?}"
    );

    // --examples: the row becomes a usage example with the actual call line + column.
    let ex = f.run(&["callers", "target", "--examples"]);
    let row = &ex["data"].as_array().unwrap()[0];
    assert_eq!(row["name"], "caller");
    assert!(
        row["text"].as_str().unwrap().contains("target(41)"),
        "example should show the call expression: {row:?}"
    );
    assert!(
        row["col"].as_i64().is_some(),
        "example row should carry a column: {row:?}"
    );
}

#[test]
fn explore_bundles_definition_callers_callees_and_source() {
    let f = Fixture::new();
    f.write(
        "a.rs",
        "fn helper(x: i32) -> i32 { x + 1 }\n\
         fn caller() { let _ = helper(41); }\n",
    );

    let r = f.run(&["explore", "helper"]);
    let d = &r["data"];

    // focus = the definition, carrying its signature.
    assert_eq!(d["focus"][0]["name"], "helper");
    assert!(
        d["focus"][0]["signature"]
            .as_str()
            .unwrap()
            .contains("helper"),
        "{d}"
    );

    // callers include the enclosing caller definition.
    let callers: Vec<&str> = d["callers"]
        .as_array()
        .unwrap()
        .iter()
        .filter_map(|c| c["name"].as_str())
        .collect();
    assert!(callers.contains(&"caller"), "{callers:?}");

    // source carries verbatim bodies, each tagged by role; the focus body is present.
    let src = d["source"].as_array().unwrap();
    let focus_chunk = src
        .iter()
        .find(|c| c["reason"] == "focus")
        .expect("focus source chunk");
    assert!(
        focus_chunk["text"].as_str().unwrap().contains("x + 1"),
        "{focus_chunk}"
    );
    assert!(
        src.iter()
            .any(|c| c["reason"] == "caller" && c["name"] == "caller"),
        "caller source missing: {src:?}"
    );
}

#[test]
fn explore_response_budget_pages_lists_and_rejects_stale_cursors() {
    let f = Fixture::new();
    let mut source = String::new();
    for index in 0..24 {
        source.push_str(&format!("use std::fmt::Debug as Debug{index};\n"));
    }
    source.push_str("fn target() {}\n");
    for index in 0..24 {
        source.push_str(&format!("fn caller_{index}() {{ target(); }}\n"));
    }
    f.write("a.rs", &source);

    let mut cursor: Option<String> = None;
    let mut callers = std::collections::HashSet::new();
    let mut imports = std::collections::HashSet::new();
    let mut first_cursor = None;
    let mut pages = 0;
    let mut response_limit = 256u64;
    let mut raised_limit = false;
    loop {
        let mut args = vec![
            "--json".to_string(),
            "explore".to_string(),
            "target".to_string(),
            "--max-response-tokens".to_string(),
            response_limit.to_string(),
        ];
        if let Some(value) = &cursor {
            args.extend(["--cursor".to_string(), value.clone()]);
        }
        let args: Vec<&str> = args.iter().map(String::as_str).collect();
        let output = f.raw(&args);
        assert!(
            output.status.success(),
            "{}",
            String::from_utf8_lossy(&output.stderr)
        );
        assert!(
            output.stdout.len() <= response_limit as usize * 4,
            "{} bytes",
            output.stdout.len()
        );
        let value: Value = serde_json::from_slice(&output.stdout).unwrap();
        let data = &value["data"];
        assert!(data["response"]["used_tokens"].as_u64().unwrap() <= response_limit);
        if let Some(rows) = data["callers"].as_array() {
            for caller in rows {
                let name = caller["name"].as_str().unwrap().to_string();
                assert!(
                    callers.insert(name.clone()),
                    "caller repeated across pages: {name}"
                );
            }
        }
        if let Some(rows) = data["imports"].as_array() {
            for import in rows {
                let key = format!("{}:{}", import["line"], import["module"]);
                assert!(
                    imports.insert(key.clone()),
                    "import repeated across pages: {key}"
                );
            }
        }
        let next = data["response"]["next_cursor"].as_str().map(str::to_string);
        if first_cursor.is_none() {
            first_cursor = next.clone();
        }
        pages += 1;
        if next.is_none() {
            assert!(data["response"]["complete"].as_bool().unwrap());
            break;
        }
        assert!(!data["response"]["complete"].as_bool().unwrap());
        if let Some(minimum) = data["response"]["minimum_next_tokens"].as_u64() {
            raised_limit = true;
            response_limit = response_limit.max(minimum);
        }
        cursor = next;
        assert!(
            pages < 100,
            "pagination did not finish at cursor {cursor:?}"
        );
    }
    assert_eq!(callers.len(), 24);
    assert_eq!(imports.len(), 24);
    assert!(pages > 1);
    assert!(
        raised_limit,
        "the next detail should report a larger minimum budget"
    );

    f.write("a.rs", &format!("{source}fn added() {{ target(); }}\n"));
    let stale = f.raw(&[
        "--json",
        "explore",
        "target",
        "--max-response-tokens",
        "256",
        "--cursor",
        first_cursor.as_deref().unwrap(),
    ]);
    assert!(!stale.status.success());
    assert!(String::from_utf8_lossy(&stale.stderr).contains("stale or invalid explore cursor"));
}

#[test]
fn explore_marks_source_budget_omissions_incomplete_and_directs_a_followup_read() {
    let f = Fixture::new();
    f.write(
        "a.rs",
        "fn target() { helper(); }\nfn helper() { let value = 41; }\nfn caller() { target(); }\n",
    );

    let result = f.run(&["explore", "target", "--max-tokens", "1"]);
    let data = &result["data"];
    assert!(!data["response"]["complete"].as_bool().unwrap());
    assert!(data["omitted"]
        .as_array()
        .unwrap()
        .iter()
        .any(|item| { item["name"] == "caller" && item["path"] == "a.rs" }));
    assert!(result["note"]
        .as_str()
        .unwrap()
        .contains("read --symbol <name> -- <path>"));
}

#[test]
fn explore_resolved_prunes_same_named_code_from_another_program() {
    // `callers`, `callees` and `context` all had `--resolved`; `explore` did
    // not, and it is the command that pays most for the name-based over-match,
    // because unrelated same-named code spends the *token budget* as well as
    // the reader's attention. On a Go/TypeScript repo, exploring a Go function
    // pulled in three same-named TypeScript functions from the web app and grew
    // the answer from 109 lines to 391.
    let f = Fixture::new();
    f.write(
        "go/main.go",
        "package main\n\nfunc parseDate() {}\n\nfunc Parse() { parseDate() }\n",
    );
    f.write("web/util.ts", "export function parseDate() {}\n");

    let callee_paths = |v: &Value| -> Vec<String> {
        v["data"]["callees"]
            .as_array()
            .unwrap()
            .iter()
            .filter_map(|c| c["path"].as_str().map(str::to_string))
            .collect()
    };

    let all = callee_paths(&f.run(&["explore", "Parse"]));
    assert!(
        all.iter().any(|p| p == "web/util.ts"),
        "expected the over-match: {all:?}"
    );

    let kept = callee_paths(&f.run(&["explore", "Parse", "--resolved"]));
    assert!(
        kept.iter().any(|p| p == "go/main.go"),
        "real callee dropped: {kept:?}"
    );
    assert!(
        !kept.iter().any(|p| p == "web/util.ts"),
        "unreachable callee kept: {kept:?}"
    );
}

#[test]
fn explore_emits_the_focus_source_before_the_neighbourhood() {
    // Source blocks were sorted purely by path, so the definition you asked
    // about landed wherever its path happened to sort. On a polyglot repo that
    // put unrelated same-named code from another language first, and
    // `explore … | head -N` — the obvious way to sample a large result — showed
    // callees instead of the definition.
    let f = Fixture::new();
    f.write(
        "zzz/focus.rs",
        "fn helper() {}\nfn target() { helper(); }\n",
    );
    f.write("aaa/caller.rs", "fn outer() { target(); }\n");

    // `zzz/` sorts last, so path order alone would bury the definition.
    let out = f.raw(&["explore", "target"]);
    let text = String::from_utf8_lossy(&out.stdout);
    let source = text.split("# source").nth(1).expect("no source section");
    let first = source
        .lines()
        .find(|l| l.contains(".rs:"))
        .expect("no source block");
    assert!(
        first.starts_with("zzz/focus.rs:2"),
        "the definition asked about should lead the source:\n{text}"
    );
}

#[test]
fn explore_forces_focus_source_under_a_tight_budget() {
    let f = Fixture::new();
    // A big caller body and a small focus, with a budget too small for both.
    let big = "fn caller() {\n".to_string() + &"    let _ = focus();\n".repeat(80) + "}\n";
    f.write("a.rs", &format!("fn focus() {{}}\n{big}"));

    let d = f.run(&["explore", "focus", "--max-tokens", "5"]);
    let src = d["data"]["source"].as_array().unwrap();
    // The focus is forced in even over budget; the oversized caller is omitted.
    assert!(
        src.iter().any(|c| c["reason"] == "focus"),
        "focus must survive the budget: {src:?}"
    );
    let omitted: Vec<&str> = d["data"]["omitted"]
        .as_array()
        .unwrap()
        .iter()
        .filter_map(|o| o["name"].as_str())
        .collect();
    assert!(
        omitted.contains(&"caller"),
        "oversized caller should be reported omitted: {omitted:?}"
    );
}

#[test]
fn members_are_definitions_and_name_the_type_that_declares_them() {
    // A member name alone identifies nothing — `id`, `name` and `retries` recur
    // across unrelated types — so a field fact is only useful with its owner.
    let f = Fixture::new();
    f.write(
        "a.rs",
        "pub struct Config { pub retries: u32, name: String }\npub enum Mode { Fast, Slow }\n",
    );
    f.write("g.go", "type Server struct { Host string }\n");
    f.write(
        "t.ts",
        "export class Widget { public width: number = 1; private secret: string = \"s\"; }\n",
    );
    // `public` on the class matters now: a member of a non-public type is not
    // public API either (covered by `a_member_of_a_private_type_is_not_public_api`).
    // This test is about the member's *own* modifier, so the class is public.
    f.write(
        "J.java",
        "public class Account { private int balance; public String holder; }\n",
    );

    // The case that used to return nothing at all.
    let v = f.run(&["definitions", "retries"]);
    let rows = v["data"].as_array().unwrap();
    assert_eq!(rows.len(), 1, "{v}");
    assert_eq!(rows[0]["kind"], "field");
    assert_eq!(
        rows[0]["owner"], "Config",
        "a field must name its type: {v}"
    );

    // Enum variants likewise.
    let v = f.run(&["definitions", "Fast"]);
    assert_eq!(v["data"][0]["kind"], "variant", "{v}");
    assert_eq!(v["data"][0]["owner"], "Mode", "{v}");

    // Extraction reaches the other grammars, and each owner is the *nearest*
    // type rather than whatever else encloses it.
    for (name, owner) in [
        ("Host", "Server"),
        ("width", "Widget"),
        ("balance", "Account"),
    ] {
        let v = f.run(&["definitions", name]);
        assert_eq!(v["data"][0]["owner"], owner, "{name}: {v}");
    }

    // A member's own modifier decides its visibility. Before, a private field in
    // an *exported* class inherited the class's visibility and showed up as API.
    let api: Vec<String> = field_set(&f.run(&["api", "t.ts"]), "name");
    assert!(api.contains(&"width".to_string()), "{api:?}");
    assert!(
        !api.contains(&"secret".to_string()),
        "private field leaked into api: {api:?}"
    );
    let api: Vec<String> = field_set(&f.run(&["api", "J.java"]), "name");
    assert!(
        api.contains(&"holder".to_string()) && !api.contains(&"balance".to_string()),
        "{api:?}"
    );

    // Non-members must not sprout an owner.
    let v = f.run(&["definitions", "Config"]);
    assert!(v["data"][0]["owner"].is_null(), "a type has no owner: {v}");
}

#[test]
fn clones_are_exact_structural_matches_not_a_similarity_score() {
    let f = Fixture::new();
    // Same shape, different names *and* different literals: a type-2 clone.
    f.write(
        "a.rs",
        "pub fn total(items: &[i32]) -> i32 {\n    let mut acc = 0;\n    for it in items {\n        acc += it * 2;\n    }\n    acc\n}\n\
         pub fn weight(rows: &[i32]) -> i32 {\n    let mut sum = 0;\n    for r in rows {\n        sum += r * 3;\n    }\n    sum\n}\n",
    );
    // Structurally different: an `if` where the others have none.
    f.write(
        "b.rs",
        "pub fn other(xs: &[i32]) -> i32 {\n    let mut acc = 0;\n    for x in xs {\n        if *x > 0 {\n            acc += x;\n        }\n    }\n    acc\n}\n",
    );
    // Operators are syntax too: otherwise these bodies collapse to the same
    // shape after named identifiers and literals are normalised away.
    f.write(
        "operators.rs",
        "fn different(a: i32) -> bool {\n    let v0 = a + 1;\n    let v1 = v0 * 2;\n    let v2 = v1 - 3;\n    let v3 = v2 / 4;\n    let v4 = v3 % 5;\n    v0 != v4\n}\n\nfn same(a: i32) -> bool {\n    let v0 = a + 1;\n    let v1 = v0 * 2;\n    let v2 = v1 - 3;\n    let v3 = v2 / 4;\n    let v4 = v3 % 5;\n    v0 == v4\n}\n",
    );

    let v = f.run(&["clones", "--min-nodes", "10"]);
    let rows = v["data"].as_array().unwrap();
    let names: Vec<&str> = rows.iter().map(|r| r["name"].as_str().unwrap()).collect();
    assert!(names.contains(&"total") && names.contains(&"weight"), "{v}");
    assert!(
        !names.contains(&"other"),
        "a different shape is not a clone: {v}"
    );
    assert!(
        !names.contains(&"different") && !names.contains(&"same"),
        "different comparison operators are not exact clones: {v}"
    );
    // Every row in a group carries the same shape and the group size.
    assert_eq!(rows[0]["shape"], rows[1]["shape"]);
    assert_eq!(rows[0]["group"], 2);

    // `min_nodes` filters by how much code is involved; it does not rank.
    let big = f.run(&["clones", "--min-nodes", "10000"]);
    assert!(big["data"].as_array().unwrap().is_empty(), "{big}");

    // Trivial definitions share a shape truthfully but uselessly, so they carry
    // none at all and can never form a group.
    f.write("c.rs", "pub fn a() {}\npub fn b() {}\n");
    let v = f.run(&["clones", "--min-nodes", "1"]);
    let names: Vec<&str> = v["data"]
        .as_array()
        .unwrap()
        .iter()
        .map(|r| r["name"].as_str().unwrap())
        .collect();
    assert!(
        !names.contains(&"a"),
        "empty fns must not be reported: {names:?}"
    );
}

#[test]
fn members_extract_for_zig_solidity_dart_and_scala() {
    let f = Fixture::new();
    // Zig binds a type to a name rather than declaring it with one, so the
    // struct is anonymous and `Point` sits on the enclosing declaration — every
    // field was ownerless, and therefore dropped, until the owner lookup
    // followed that binding.
    f.write("z.zig", "const Point = struct { x: i32, y: i32 };\n");
    f.write(
        "W.sol",
        "contract Wallet {\n  uint256 public balance;\n  enum State { Open, Closed }\n}\n",
    );
    f.write(
        "d.dart",
        "class Widget {\n  int size = 1;\n}\nenum Mode { fast, slow }\n",
    );
    f.write(
        "s.scala",
        "class Server {\n  val port: Int = 80\n  var live: Boolean = true\n}\n",
    );

    for (name, owner) in [
        ("x", "Point"),
        ("balance", "Wallet"),
        ("size", "Widget"),
        ("port", "Server"),
        ("live", "Server"),
    ] {
        let v = f.run(&["definitions", name]);
        assert_eq!(v["data"][0]["owner"], owner, "{name}: {v}");
    }
    for (name, owner) in [("Open", "State"), ("fast", "Mode")] {
        let v = f.run(&["definitions", name]);
        assert_eq!(v["data"][0]["owner"], owner, "{name}: {v}");
        assert_eq!(v["data"][0]["kind"], "variant", "{name}: {v}");
    }
}

#[test]
fn deps_resolves_an_import_that_names_a_directory() {
    // Most imports name a *directory*, not a file — `./utils` means
    // `utils/index.js`, `from pkg import x` means `pkg/__init__.py` — and stem
    // matching alone never resolved any of them, so the dependency graph silently
    // missed the dominant idiom in its two largest ecosystems.
    let f = Fixture::new();
    f.write(
        "app.js",
        "import { helper } from './utils';\nexport function run() { return helper(); }\n",
    );
    f.write("utils/index.js", "export function helper() { return 1 }\n");
    f.write("main.py", "from pkg import thing\n");
    f.write("pkg/__init__.py", "def thing(): pass\n");
    // An ordinary file import must keep working.
    f.write("b.js", "import { x } from './plain';\n");
    f.write("plain.js", "export const x = 1;\n");

    let deps = field_set(&f.run(&["deps", "app.js"]), "path");
    assert!(deps.contains(&"utils/index.js".to_string()), "{deps:?}");
    let deps = field_set(&f.run(&["deps", "main.py"]), "path");
    assert!(deps.contains(&"pkg/__init__.py".to_string()), "{deps:?}");
    let deps = field_set(&f.run(&["deps", "b.js"]), "path");
    assert!(
        deps.contains(&"plain.js".to_string()),
        "direct file import broke: {deps:?}"
    );
}

#[test]
fn go_module_imports_resolve_to_their_package_not_a_matching_file_stem() {
    // Go import paths are module-relative package directories. A stem matcher
    // would see only `worker` and connect this import to both files; go.mod lets
    // the resolver pick the package exactly and reject an external lookalike.
    let f = Fixture::new();
    f.write("go.mod", "module example.com/acme/service\n\ngo 1.22\n");
    f.write(
        "cmd/app/main.go",
        "package main\n\nimport \"example.com/acme/service/internal/worker\"\n\nfunc run() { worker.Build() }\n",
    );
    f.write(
        "internal/worker/worker.go",
        "package worker\n\nfunc Build() {}\n",
    );
    f.write("other/worker.go", "package other\n\nfunc Build() {}\n");
    f.write(
        "external.go",
        "package main\n\nimport \"example.net/lib/worker\"\n",
    );

    let deps = field_set(&f.run(&["deps", "cmd/app/main.go"]), "path");
    assert_eq!(deps, ["internal/worker/worker.go"], "{deps:?}");

    let external = field_set(&f.run(&["deps", "external.go"]), "path");
    assert!(
        external.is_empty(),
        "an external Go module borrowed local code: {external:?}"
    );

    let callers = f.run(&["callers", "Build", "--resolved"]);
    let row = callers["data"]
        .as_array()
        .unwrap()
        .iter()
        .find(|row| row["name"] == "run")
        .expect("the internal Go package call should resolve");
    assert_eq!(row["via"], "imported", "{callers}");
}

#[test]
fn a_go_package_is_every_non_test_file_in_exactly_its_directory() {
    // An import names a directory, so it reaches each file of that package:
    // not its tests, not a nested package, and not a non-Go file beside them.
    // The module path itself is the package at the repository root.
    let f = Fixture::new();
    f.write("go.mod", "module example.com/acme/service\n\ngo 1.22\n");
    f.write(
        "cmd/app/main.go",
        "package main\n\nimport (\n\t\"example.com/acme/service\"\n\t\"example.com/acme/service/internal/store\"\n)\n\nfunc run() { service.Start(); store.Open() }\n",
    );
    f.write("service.go", "package service\n\nfunc Start() {}\n");
    f.write(
        "service_test.go",
        "package service\n\nfunc TestStart() {}\n",
    );
    f.write(
        "internal/store/open.go",
        "package store\n\nfunc Open() {}\n",
    );
    f.write(
        "internal/store/close.go",
        "package store\n\nfunc Close() {}\n",
    );
    f.write(
        "internal/store/open_test.go",
        "package store\n\nfunc TestOpen() {}\n",
    );
    f.write("internal/store/schema.sql", "CREATE TABLE t (id INT);\n");
    f.write(
        "internal/store/cache/cache.go",
        "package cache\n\nfunc Get() {}\n",
    );

    assert_eq!(
        field_set(&f.run(&["deps", "cmd/app/main.go"]), "path"),
        [
            "internal/store/close.go",
            "internal/store/open.go",
            "service.go"
        ]
    );
}

#[test]
fn symbols_can_be_filtered_by_the_type_that_owns_them() {
    // Members are only addressable together with their type, so the owner has to
    // be queryable — otherwise "what does this type hold?" means fetching every
    // field in the repository and filtering by hand.
    let f = Fixture::new();
    f.write("a.rs", "pub struct Config { pub retries: u32, pub name: String }\npub struct Other { pub retries: u32 }\n");

    let mine = field_set(
        &f.run(&["symbols", "--owner", "Config", "--limit", "0"]),
        "name",
    );
    assert_eq!(mine.len(), 2, "{mine:?}");
    assert!(
        mine.contains(&"retries".to_string()) && mine.contains(&"name".to_string()),
        "{mine:?}"
    );

    // The same name under a different type is a different member.
    let v = f.run(&["symbols", "--owner", "Other", "--limit", "0"]);
    let rows = v["data"].as_array().unwrap();
    assert_eq!(rows.len(), 1, "{v}");
    assert_eq!(rows[0]["name"], "retries");
}

#[test]
fn graphql_implements_is_a_type_relation() {
    // `implements_interfaces` nests left-recursively rather than listing siblings,
    // so a single pattern bound only the outermost interface.
    let f = Fixture::new();
    f.write(
        "s.graphql",
        "interface A { id: ID! }\ninterface B { id: ID! }\ninterface C { id: ID! }\n\
         type User implements A & B & C { id: ID! }\ntype Post implements A { id: ID! }\n",
    );
    let supers = field_set(&f.run(&["supertypes", "User"]), "name");
    assert_eq!(
        supers,
        vec!["A", "B", "C"],
        "every interface, not just the last: {supers:?}"
    );
    let impls = field_set(&f.run(&["implementors", "A"]), "name");
    assert!(
        impls.contains(&"User".to_string()) && impls.contains(&"Post".to_string()),
        "{impls:?}"
    );
}

#[test]
fn calls_filter_recognises_markup_instantiation() {
    // `--calls` tested for `Name(`, which a component is never spelled with — so
    // it returned nothing for a component that is plainly used.
    let f = Fixture::new();
    f.write(
        "Btn.svelte",
        "<script>export let label = 'x'</script>\n<button>{label}</button>\n",
    );
    f.write(
        "Page.svelte",
        "<script>\n  import Btn from './Btn.svelte';\n  // Btn is mentioned here\n  const x = Btn;\n</script>\n<Btn />\n",
    );
    let v = f.run(&["references", "Btn", "--calls"]);
    let refs = v["data"]["references"].as_array().unwrap();
    assert_eq!(refs.len(), 1, "only the instantiation counts: {v}");
    assert_eq!(refs[0]["line"], 6, "{v}");
    // The import, the comment and the assignment are all mentions, not uses.
    assert!(
        f.run(&["callers", "Btn", "--calls"])["data"]
            .as_array()
            .unwrap()
            .len()
            == 1
    );
}

#[test]
fn unused_ignores_definitions_that_exist_to_be_consumed_elsewhere() {
    // A GraphQL type or protobuf message is a contract for another codebase, so
    // "nothing here references it" is its normal state, not a finding — reporting
    // them buries the real hits.
    let f = Fixture::new();
    f.write("s.graphql", "type User { id: ID! }\n");
    f.write(
        "u.proto",
        "syntax = \"proto3\";\nmessage Order { string id = 1; }\n",
    );
    f.write(
        "main.tf",
        "variable \"region\" { default = \"eu\" }\n\
                        output \"url\" { value = var.region }\n\
                        variable \"unused_var\" {}\n",
    );
    f.write(
        "a.css",
        ".primary { color: red }\n.orphan { color: blue }\n",
    );
    f.write("page.html", "<div class=\"primary\">x</div>\n");

    let dead = field_set(&f.run(&["unused"]), "name");
    // Real dead code survives: nothing references these.
    assert!(
        dead.contains(&"orphan".to_string()),
        "dead CSS class: {dead:?}"
    );
    assert!(
        dead.contains(&"unused_var".to_string()),
        "dead variable: {dead:?}"
    );
    // Contracts and externally-read values do not.
    for quiet in ["User", "Order", "url", "primary", "region"] {
        assert!(
            !dead.contains(&quiet.to_string()),
            "{quiet} should not be reported: {dead:?}"
        );
    }
}

#[test]
fn hcl_blocks_are_named_by_their_last_label_and_owned_by_their_type() {
    // A block's identity lives in its labels, and which label is the name depends
    // on how many there are — so this cannot be a query, and a first attempt that
    // tried produced two facts per resource and named the wrong one.
    let f = Fixture::new();
    f.write(
        "main.tf",
        "# infra\nterraform { required_version = \">= 1.0\" }\n\
         \nvariable \"region\" { default = \"eu-west-1\" }\n\
         \nlocals {\n  bucket_prefix = \"acme\"\n}\n\
         \nresource \"aws_s3_bucket\" \"logs\" {\n  lifecycle { prevent_destroy = true }\n}\n\
         \ndata \"aws_ami\" \"ubuntu\" { most_recent = true }\n\
         \nmodule \"vpc\" {\n  source = \"./modules/vpc\"\n}\n\
         \noutput \"arn\" { value = aws_s3_bucket.logs.arn }\n",
    );

    let by_name = |n: &str| -> Value {
        let v = f.run(&["definitions", n]);
        v["data"][0].clone()
    };

    // One label: the label names it, the keyword owns it.
    assert_eq!(by_name("region")["kind"], "variable");
    assert_eq!(by_name("region")["owner"], "variable");
    // Two labels: the *last* names it, the first is its type.
    let logs = by_name("logs");
    assert_eq!(logs["kind"], "resource");
    assert_eq!(
        logs["owner"], "aws_s3_bucket",
        "named by last label, owned by type: {logs}"
    );
    assert_eq!(by_name("ubuntu")["owner"], "aws_ami");
    // `locals` declares one name per attribute, referenced as `local.x`.
    assert_eq!(by_name("bucket_prefix")["owner"], "local");

    // Unlabelled blocks have no identity: `terraform` and the nested `lifecycle`
    // must not become definitions.
    let syms = field_set(&f.run(&["symbols", "--limit", "0"]), "name");
    assert!(!syms.contains(&"terraform".to_string()), "{syms:?}");
    assert!(!syms.contains(&"lifecycle".to_string()), "{syms:?}");

    // A module's `source` is an import, so `importers` finds it.
    let imports = field_set(&f.run(&["imports", "main.tf"]), "module");
    assert_eq!(
        imports,
        vec!["./modules/vpc".to_string()],
        "only the source: {imports:?}"
    );

    // The naming rule exists so that reference sites resolve: Terraform writes
    // `aws_s3_bucket.logs.arn`, and naming the block `logs` is what makes that
    // findable — and it lands inside the `output` that uses it.
    let v = f.run(&["references", "logs"]);
    let refs = v["data"]["references"].as_array().unwrap();
    let use_site = refs
        .iter()
        .find(|r| {
            r["text"]
                .as_str()
                .unwrap_or_default()
                .contains("aws_s3_bucket.logs.arn")
        })
        .unwrap_or_else(|| panic!("no use site for the resource: {v}"));
    assert_eq!(
        use_site["symbol"], "arn",
        "attributed to the enclosing output: {v}"
    );
    assert!(
        use_site["definition"].is_null(),
        "a use is not the declaration: {v}"
    );
}

#[test]
fn extracts_css_powershell_protobuf_and_graphql() {
    let f = Fixture::new();
    f.write("a.css", "/* theme */\n.primary { color: red }\n#hero { top: 0 }\n@keyframes fade { from { opacity: 0 } }\n");
    f.write(
        "s.ps1",
        "function Get-Thing {\n  return 1\n}\nclass Widget { [int] $Size }\n",
    );
    f.write(
        "u.proto",
        "syntax = \"proto3\";\nimport \"other.proto\";\nmessage User { string email = 1; }\n\
         enum Role { ADMIN = 0; }\nservice Users { rpc GetUser (User) returns (User); }\n",
    );
    f.write(
        "s.graphql",
        "type User { id: ID!\n  email: String }\nenum Role { ADMIN }\ninterface Node { id: ID! }\n",
    );

    let syms = field_set(&f.run(&["symbols", "--limit", "0"]), "name");
    for want in [
        "primary",
        "hero",
        "fade", // css selectors and keyframes
        "Get-Thing",
        "Widget", // powershell
        "User",
        "Role",
        "Users",
        "GetUser", // protobuf message/enum/service/rpc
        "Node",    // graphql interface
    ] {
        assert!(syms.contains(&want.to_string()), "missing {want}: {syms:?}");
    }

    // Members of a protobuf message and a GraphQL type carry their owner, so the
    // grammars that spell the name out per construct (`message_name`) resolve too.
    let v = f.run(&["definitions", "email"]);
    let owners: Vec<&str> = v["data"]
        .as_array()
        .unwrap()
        .iter()
        .filter_map(|r| r["owner"].as_str())
        .collect();
    assert_eq!(
        owners,
        ["User", "User"],
        "proto and graphql fields need owners: {v}"
    );

    // A protobuf import is a module string like any other.
    let imports = field_set(&f.run(&["imports", "u.proto"]), "module");
    assert!(imports.contains(&"other.proto".to_string()), "{imports:?}");

    // Spans: CSS comments are classified, which is what closes the `<style>` hole
    // in Svelte and Vue components.
    let count = |needle: &str, kind: &str| -> i64 {
        f.run(&["search", needle, "--in", kind, "-c"])["data"]["counts"]
            .as_array()
            .unwrap()
            .iter()
            .map(|c| c["count"].as_i64().unwrap())
            .sum()
    };
    assert_eq!(count("theme", "comments"), 1);
    assert_eq!(count("theme", "code"), 0);
}

#[test]
fn python_attributes_are_members_despite_having_no_declaration_site() {
    // Python declares an attribute by assigning it, so there is no single
    // declaration to capture: the same attribute is assigned in whichever methods
    // touch it, and an assignment through some *other* object is not a
    // declaration at all.
    let f = Fixture::new();
    f.write(
        "p.py",
        "class Config:\n    DEFAULT = 3\n    timeout: float = 1.5\n\
         \n    def __init__(self):\n        self.retries = 3\n\
         \n    def reset(self):\n        self.retries = 0\n\
         \ndef helper(other):\n    other.retries = 99\n",
    );

    let members = field_set(
        &f.run(&["symbols", "--kind", "field", "--limit", "0"]),
        "name",
    );
    for want in ["DEFAULT", "timeout", "retries"] {
        assert!(
            members.contains(&want.to_string()),
            "missing {want}: {members:?}"
        );
    }

    // Assigned in two methods, but it is one member — reported at the earliest.
    let v = f.run(&["definitions", "retries"]);
    let rows = v["data"].as_array().unwrap();
    assert_eq!(rows.len(), 1, "one member per (name, owner): {v}");
    assert_eq!(rows[0]["line"], 6, "earliest declaration wins: {v}");
    assert_eq!(rows[0]["owner"], "Config");

    // `other.retries = 99` sits in a plain function: not self-qualified, and no
    // enclosing type, so it is not a member.
    assert!(
        rows.iter().all(|r| r["line"] != 11),
        "an assignment through another object is not a declaration: {v}"
    );
}

#[test]
fn an_unresolvable_compare_to_ref_says_so_plainly() {
    // `--compare-to` defaults to `main`, so a repo whose trunk is `master` hits
    // this on its first `changed`. Git's own wording for an unknown ref advises
    // using `--` to separate paths from revisions, which points at the wrong
    // problem entirely.
    let f = Fixture::new();
    f.write("a.rs", "fn target() {}\n");
    f.git_init_commit();

    let out = f.raw(&["changed", "--compare-to", "definitely-not-a-ref"]);
    assert!(!out.status.success());
    let err = String::from_utf8_lossy(&out.stderr);
    assert!(
        err.contains("no such git ref: 'definitely-not-a-ref'"),
        "{err}"
    );
    assert!(
        !err.contains("separate paths from revisions"),
        "git's misleading advice leaked: {err}"
    );

    // A ref that exists still works.
    let ok = f.raw(&["changed", "--compare-to", "HEAD"]);
    assert!(
        ok.status.success(),
        "{}",
        String::from_utf8_lossy(&ok.stderr)
    );
}

#[test]
fn concurrent_invocations_all_succeed() {
    // Agents run codeq concurrently — parallel tool calls, or several agents on
    // one checkout — and every invocation refreshes, so contention is the normal
    // case. This used to fail outright: 10 of 12 returned "database is locked".
    use std::process::{Command as PCommand, Stdio};

    let f = Fixture::new();
    for i in 0..12 {
        f.write(
            &format!("src/m{i}.rs"),
            &format!("pub fn f{i}() {{ let v = {i}; }}\n"),
        );
    }

    // Cold: no index yet, so all twelve want to build one at the same time.
    let spawn_all = |f: &Fixture| -> Vec<String> {
        let kids: Vec<_> = (0..12)
            .map(|_| {
                PCommand::cargo_bin("codeq")
                    .unwrap()
                    .args(["--json", "--meta", "symbols", "--limit", "0"])
                    .current_dir(f.path())
                    .stdout(Stdio::piped())
                    .stderr(Stdio::piped())
                    .spawn()
                    .unwrap()
            })
            .collect();
        kids.into_iter()
            .map(|k| {
                let out = k.wait_with_output().unwrap();
                assert!(
                    out.status.success(),
                    "concurrent invocation failed: {}",
                    String::from_utf8_lossy(&out.stderr)
                );
                String::from_utf8(out.stdout).unwrap()
            })
            .collect()
    };

    let assert_round = |expected_rows, expected_reindexed| {
        let mut reindexed = 0;
        for body in spawn_all(&f) {
            let v: Value = serde_json::from_str(&body).unwrap();
            assert_eq!(v["data"].as_array().unwrap().len(), expected_rows, "{body}");
            reindexed += v["index"]["reindexed_files"].as_u64().unwrap_or(0);
        }
        assert_eq!(reindexed, expected_reindexed);
    };
    assert_round(12, 12);
    assert_round(12, 0);
    f.write("src/extra.rs", "pub fn extra() {}\n");
    assert_round(13, 1);
}

#[test]
fn fields_reports_names_that_matched_no_row() {
    // Projecting an unknown name silently yielded a narrower row, which reads as
    // "this record has no such value" rather than "that isn't a field".
    //
    // The wrong guess this used to test for was `line` vs `start_line` -- one
    // spelling on `search`, another on `definitions`. ROW_KEY_RULE removed that
    // trap, so the guess here is simply a name codeq has never had.
    let f = Fixture::new();
    f.write("a.rs", "fn target() {}\n");

    let v = f.run(&["definitions", "target", "--fields", "path,lineno"]);
    let notes = &v["notes"];
    assert_eq!(notes["unmatched_fields"][0], "lineno", "{v}");
    let avail: Vec<&str> = notes["available_fields"]
        .as_array()
        .unwrap()
        .iter()
        .map(|s| s.as_str().unwrap())
        .collect();
    assert!(
        avail.contains(&"line"),
        "must name the real field: {avail:?}"
    );
    // The projection still happens — this is a note, not a failure.
    assert_eq!(v["data"][0]["path"], "a.rs");

    // A field that exists produces no note at all -- and `line` is now that
    // field on every command, which is the point of the rule.
    let ok = f.run(&["definitions", "target", "--fields", "path,line"]);
    assert!(
        ok["notes"].is_null(),
        "clean projection must not be annotated: {ok}"
    );
}

#[test]
fn astgrep_rejects_a_pattern_it_could_only_partly_parse() {
    // Error recovery discards what it can't use, and the descent past the ERROR
    // lands on whatever fragment survived — so `if { }` silently became "an empty
    // block". Reporting zero matches for that is indistinguishable from "no such
    // code exists", which is the wrong answer to give an agent.
    let f = Fixture::new();
    f.write("a.rs", "fn one() { let a = 1; }\nfn two() { one(); }\n");

    for pat in ["if { }", "match { }", "garbage $$$", "let $X = ;"] {
        let out = f.raw(&["astgrep", pat, "--language", "rust"]);
        assert!(
            !out.status.success(),
            "malformed pattern {pat:?} reported success: {}",
            String::from_utf8_lossy(&out.stdout)
        );
    }
    // Patterns that legitimately rely on error recovery still work: a bare
    // expression is not a valid top-level item in Rust.
    for (pat, want) in [
        ("$X.unwrap()", 0usize),
        ("one($$$)", 1),
        ("fn $N($$$) { $$$ }", 2),
    ] {
        let v = f.run(&["astgrep", pat, "--language", "rust"]);
        assert_eq!(
            v["data"]["matches"].as_array().unwrap().len(),
            want,
            "pattern {pat:?}: {v}"
        );
    }
}

#[test]
fn offsets_stay_aligned_when_a_file_is_not_valid_utf8() {
    // Fact offsets and the searchable body have to be measured in the same text.
    // The body is decoded with invalid bytes replaced, and each replacement is
    // three bytes where the original was one — so extracting facts from the raw
    // bytes instead shifts every later offset and the enclosing-symbol lookup
    // names the wrong definition (or none).
    let f = Fixture::new();
    let mut src: Vec<u8> = b"// ".to_vec();
    src.extend(std::iter::repeat_n(0xE9u8, 16)); // latin-1 'e-acute': invalid UTF-8
    src.extend_from_slice(
        b"\nfn alpha() { helper(); }\nfn bravo() { let b = 2; }\n\
          fn charlie() { let c = 3; }\nfn helper() {}\n",
    );
    f.write_bytes("z.rs", &src);

    // alpha is the caller; bravo merely sits after it in the file.
    let callers = f.run(&["callers", "helper"]);
    let rows = callers["data"].as_array().unwrap();
    assert_eq!(rows.len(), 1, "{rows:?}");
    assert_eq!(
        rows[0]["name"], "alpha",
        "wrong enclosing definition: {rows:?}"
    );

    // `search` and `at` must agree about which definition line 2 belongs to.
    let v = f.run(&["search", "helper"]);
    let m = v["data"]["matches"].as_array().unwrap();
    let call = m.iter().find(|r| r["line"] == 2).expect("match on line 2");
    assert_eq!(call["symbol"], "alpha", "{call}");
    let at = f.run(&["at", "z.rs:2"]);
    assert_eq!(at["data"][0]["name"], "alpha", "{at}");
}

#[test]
fn chunk_text_slices_the_same_text_the_offsets_were_measured_in() {
    // The indexer strips a leading BOM before measuring offsets, so source
    // assembly has to strip it too — slicing the raw bytes shifted every chunk
    // by three and returned code starting mid-expression.
    let f = Fixture::new();
    f.write_bytes(
        "b.rs",
        "\u{feff}fn alpha() { helper(); }\nfn helper() {}\n".as_bytes(),
    );
    let d = f.run(&["explore", "alpha"]);
    let src = d["data"]["source"].as_array().unwrap();
    let focus = src.iter().find(|c| c["reason"] == "focus").expect("focus");
    assert_eq!(
        focus["text"].as_str().unwrap(),
        "fn alpha() { helper(); }",
        "BOM-shifted chunk: {focus}"
    );
    let callee = src
        .iter()
        .find(|c| c["reason"] == "callee")
        .expect("callee");
    assert_eq!(
        callee["text"].as_str().unwrap(),
        "fn helper() {}",
        "{callee}"
    );
}

#[test]
fn large_file_batches_keep_all_symbols_through_edits_and_deletions() {
    let f = Fixture::new();
    let padding = "x".repeat(600_000);
    f.write("a.rs", &format!("//{padding}\npub fn rust_target() {{}}\n"));
    f.write("b.py", &format!("#{padding}\ndef python_target(): pass\n"));
    f.write(
        "c.js",
        &format!("//{padding}\nexport function js_target() {{}}\n"),
    );
    assert_eq!(
        field_set(&f.run(&["symbols", "--limit", "0"]), "name"),
        vec!["js_target", "python_target", "rust_target"]
    );
    f.write(
        "a.rs",
        &format!("//{padding}\npub fn edited_target() {{}}\n"),
    );
    f.remove("b.py");
    f.write("d.go", "package sample\nfunc AddedTarget() {}\n");
    assert_eq!(
        field_set(&f.run(&["symbols", "--limit", "0"]), "name"),
        vec!["AddedTarget", "edited_target", "js_target"]
    );
    let v = f.run(&["search", "rust_target|python_target", "--limit", "0"]);
    assert!(v["data"]["matches"].as_array().unwrap().is_empty());
}

#[test]
fn refresh_sees_an_edit_that_preserves_size_and_mtime() {
    // The freshness fast path skips a file whose stat looks unchanged. mtime is
    // settable from userspace (`touch -d`, `cp -p`, `rsync -a`, tar extraction),
    // so an edit of identical length could restore it and stay invisible —
    // every later query would then answer from a stale index. ctime moves on any
    // write and cannot be set, which is what makes the skip safe.
    let f = Fixture::new();
    f.write("x.rs", "pub fn aaaa() {}\n");
    let syms = field_set(&f.run(&["symbols", "--limit", "0"]), "name");
    assert!(syms.contains(&"aaaa".to_string()), "{syms:?}");

    let p = f.path().join("x.rs");
    let before = std::fs::metadata(&p).unwrap();
    let (atime, mtime) = (before.accessed().unwrap(), before.modified().unwrap());

    // Same byte length, so only the content differs.
    f.write("x.rs", "pub fn bbbb() {}\n");
    assert_eq!(std::fs::metadata(&p).unwrap().len(), before.len());

    // Put mtime back exactly as it was.
    let h = std::fs::File::options().write(true).open(&p).unwrap();
    h.set_times(
        std::fs::FileTimes::new()
            .set_accessed(atime)
            .set_modified(mtime),
    )
    .unwrap();
    assert_eq!(std::fs::metadata(&p).unwrap().modified().unwrap(), mtime);

    let syms = field_set(&f.run(&["symbols", "--limit", "0"]), "name");
    assert!(
        syms.contains(&"bbbb".to_string()),
        "stale index: mtime-preserving edit went unnoticed, got {syms:?}"
    );
    assert!(
        !syms.contains(&"aaaa".to_string()),
        "old symbol lingered: {syms:?}"
    );
}

#[test]
fn explore_reduces_an_oversized_focus_instead_of_overrunning_the_budget() {
    let f = Fixture::new();
    // A focus far bigger than any budget we'll ask for. Its body is one long
    // run of statements, so folding alone can't shrink it enough and the clip
    // path has to engage.
    let body = (0..400)
        .map(|i| format!("    let v{i} = {i};\n"))
        .collect::<String>();
    f.write("a.rs", &format!("fn focus() {{\n{body}}}\n"));

    for max in [20u64, 100, 400] {
        let d = f.run(&["explore", "focus", "--max-tokens", &max.to_string()]);
        let b = &d["data"]["budget"];
        let used = b["used_tokens"].as_u64().unwrap();
        assert!(
            used <= max,
            "--max-tokens {max} overrun: used {used} (budget must be a cap, not advice)"
        );
        // Reduced, not dropped: the focus is still there, tagged so a reader
        // knows it is not the whole definition.
        let src = d["data"]["source"].as_array().unwrap();
        let focus = src
            .iter()
            .find(|c| c["reason"] == "focus")
            .expect("focus kept");
        assert!(
            focus["reduced"].is_string(),
            "reduction must be declared: {focus}"
        );
        assert!(
            focus["text"].as_str().unwrap().contains('⋮'),
            "hidden lines must be marked: {focus}"
        );
    }

    // A focus that fits is untouched — no marker, no `reduced` tag.
    f.write("b.rs", "fn small() { let x = 1; }\n");
    let d = f.run(&["explore", "small", "--max-tokens", "2000"]);
    let src = d["data"]["source"].as_array().unwrap();
    let focus = src.iter().find(|c| c["reason"] == "focus").unwrap();
    assert!(
        focus["reduced"].is_null(),
        "a fitting focus must stay verbatim: {focus}"
    );
}

#[test]
fn explore_does_not_emit_source_already_inside_another_chunk() {
    let f = Fixture::new();
    // A class whose definition spans its methods: exploring it used to emit the
    // whole class *and* each method again, paying twice for the same bytes.
    f.write(
        "svc.py",
        "class Service:\n    def run(self):\n        return self.helper() + 1\n\n\
             def helper(self):\n        return 41\n",
    );
    let d = f.run(&["explore", "Service", "--max-tokens", "4000"]);
    let src = d["data"]["source"].as_array().unwrap();

    for (i, b) in src.iter().enumerate() {
        for (j, o) in src.iter().enumerate() {
            if i == j || b["path"] != o["path"] || !o["reduced"].is_null() {
                continue;
            }
            let (bs, be) = (b["byte"].as_u64().unwrap(), b["end_byte"].as_u64().unwrap());
            let (os, oe) = (o["byte"].as_u64().unwrap(), o["end_byte"].as_u64().unwrap());
            assert!(
                !(os <= bs && be <= oe && (oe - os) > (be - bs)),
                "{} is already inside {}, emitting both wastes budget",
                b["name"],
                o["name"]
            );
        }
    }
}

#[test]
fn affected_reports_changed_blast_radius_and_test_files() {
    let f = Fixture::new();
    // core <- mid <- a test that only reaches core transitively (through mid).
    f.write("src.rs", "fn core() { let _ = 1; }\nfn mid() { core(); }\n");
    f.write("t.rs", "fn test_core_behaviour() { mid(); }\n");
    f.git_init_commit();
    // Change only core()'s body.
    f.write("src.rs", "fn core() { let _ = 2; }\nfn mid() { core(); }\n");

    let d = f.run(&["affected", "--compare-to", "HEAD"]);
    let d = &d["data"];

    // changed = the touched definition.
    let changed: Vec<&str> = d["changed"]
        .as_array()
        .unwrap()
        .iter()
        .filter_map(|c| c["name"].as_str())
        .collect();
    assert!(changed.contains(&"core"), "{changed:?}");

    // blast radius includes the direct caller (transitively, the test too).
    let impacted: Vec<&str> = d["impacted"]
        .as_array()
        .unwrap()
        .iter()
        .filter_map(|c| c["name"].as_str())
        .collect();
    assert!(
        impacted.contains(&"mid"),
        "blast radius missing caller: {impacted:?}"
    );

    // the test that exercises the change only via mid is reported, at file level.
    let test_files: Vec<&str> = d["tests"]
        .as_array()
        .unwrap()
        .iter()
        .filter_map(|t| t["path"].as_str())
        .collect();
    assert!(
        test_files.contains(&"t.rs"),
        "transitively-affected test file missing: {test_files:?}"
    );
}

#[test]
fn extracts_definitions_for_added_languages() {
    let f = Fixture::new();
    f.write("s.sh", "greet() { echo hi; }\nfunction bye { echo bye; }\n");
    f.write(
        "m.lua",
        "local function helper(x) return x end\nfunction process(n) return helper(n) end\n",
    );
    f.write(
        "a.scala",
        "class Foo { def bar(): Int = 1 }\ntrait Greeter { def greet(): String }\n",
    );
    f.write(
        "K.kt",
        "package p\nclass Widget {\n    fun render(): String = \"w\"\n}\nfun topLevel(): Int = 1\n",
    );
    f.write("V.swift", "class Vehicle { func drive() -> String { return \"v\" } }\nprotocol Named { func name() -> String }\n");

    // Each new language's definitions land in the symbol table.
    let syms = field_set(&f.run(&["symbols", "--limit", "0"]), "name");
    for want in [
        "greet", "bye", "helper", "process", "Foo", "bar", "Greeter", "Widget", "render",
        "topLevel", "Vehicle", "drive", "Named",
    ] {
        assert!(syms.contains(&want.to_string()), "missing {want}: {syms:?}");
    }

    // The call graph works across a new language: Lua `process` calls `helper`.
    let callees = field_set(&f.run(&["callees", "process"]), "name");
    assert!(callees.contains(&"helper".to_string()), "{callees:?}");

    // Imports are captured where the language has a real import statement.
    f.write("b.swift", "import Foundation\nfunc go() {}\n");
    let imports = field_set(&f.run(&["imports", "b.swift"]), "module");
    assert!(imports.contains(&"Foundation".to_string()), "{imports:?}");
}

#[test]
fn extracts_definitions_for_dart_zig_elixir_solidity() {
    let f = Fixture::new();
    f.write(
        "w.dart",
        "class Widget {\n  int render(String s) => s.length;\n}\nint topLevel(int n) => n * 2;\n",
    );
    f.write(
        "m.zig",
        "pub fn compute(a: i32, b: i32) i32 { return a + b; }\nconst Point = struct { x: i32 };\n",
    );
    f.write("app.ex", "defmodule Math do\n  def run(a, b) do\n    helper(a) + b\n  end\n  defp helper(x), do: x\nend\n");
    f.write(
        "W.sol",
        "contract Wallet {\n  function deposit(uint amt) public {}\n}\n",
    );

    let syms = field_set(&f.run(&["symbols", "--limit", "0"]), "name");
    for want in [
        "Widget", "render", "topLevel", "compute", "Point", "Math", "run", "helper", "Wallet",
        "deposit",
    ] {
        assert!(syms.contains(&want.to_string()), "missing {want}: {syms:?}");
    }

    // Elixir's def is a macro call, yet the call graph still resolves: Math.run
    // calls helper.
    let callees = field_set(&f.run(&["callees", "run", "--path", "app.ex"]), "name");
    assert!(callees.contains(&"helper".to_string()), "{callees:?}");

    // Solidity imports are real statements and are captured.
    f.write("I.sol", "import \"./Token.sol\";\ncontract C {}\n");
    let imports = field_set(&f.run(&["imports", "I.sol"]), "module");
    assert!(imports.contains(&"./Token.sol".to_string()), "{imports:?}");
}

#[test]
fn extracts_svelte_components_scripts_and_snippets() {
    let f = Fixture::new();
    f.write(
        "src/Button.svelte",
        "<script lang=\"ts\">\n  import { formatLabel } from './labels';\n\n  \
         interface Payload { at: number }\n\n  \
         // fire upward\n  \
         function handleClick(): Payload {\n    return { at: Date.now() };\n  }\n\n  \
         const pretty = (s: string) => formatLabel(s);\n</script>\n\n\
         <!-- markup comment -->\n\
         <button class=\"primary\" on:click={handleClick}>{pretty('x')}</button>\n\n\
         {#snippet badge(count)}\n  <span>{count}</span>\n{/snippet}\n\n\
         <style>\n  .primary { color: red; }\n</style>\n",
    );
    f.write(
        "src/labels.js",
        "export function formatLabel(s) {\n  return s;\n}\n",
    );
    // A SvelteKit route: convention-loaded, so it gets no component fact — but
    // its script is still extracted.
    f.write(
        "src/routes/+page.svelte",
        "<script>\n  import Button from '../Button.svelte';\n  function bump() {}\n</script>\n\n\
         <Button on:click={bump} />\n",
    );

    let syms = field_set(&f.run(&["symbols", "--limit", "0"]), "name");
    // The file itself is the component; `lang=\"ts\"` routes the script to the
    // TypeScript grammar (hence `interface`); snippets come from the markup.
    for want in [
        "Button",
        "handleClick",
        "pretty",
        "Payload",
        "badge",
        "bump",
    ] {
        assert!(syms.contains(&want.to_string()), "missing {want}: {syms:?}");
    }
    assert!(!syms.contains(&"+page".to_string()), "route file: {syms:?}");

    // Positions are the *file's*, not the script block's: line 7 of the file.
    let def = &f.run(&["definitions", "handleClick"])["data"][0];
    assert_eq!(def["line"], 7, "{def}");
    assert_eq!(def["path"], "src/Button.svelte");

    // The call graph crosses the component boundary in both directions.
    let callees = field_set(&f.run(&["callees", "pretty"]), "path");
    assert!(
        callees.contains(&"src/labels.js".to_string()),
        "{callees:?}"
    );
    let structural = f.run(&["callees", "pretty", "--examples"]);
    let fmt = structural["data"]
        .as_array()
        .unwrap()
        .iter()
        .find(|row| row["name"] == "formatLabel")
        .expect("formatLabel call");
    assert_eq!(
        fmt["call"], "formatLabel",
        "script call lost its syntax evidence: {structural}"
    );
    let v = f.run(&["references", "Button"]);
    let refs = v["data"]["references"].as_array().unwrap();
    // Import identifier, module string and `<Button …/>` markup occurrence.
    assert_eq!(refs.len(), 3, "{refs:?}");
    assert!(
        refs.iter().all(|r| r["path"] == "src/routes/+page.svelte"),
        "{refs:?}"
    );

    // Imports are captured, so the dependency graph resolves `.svelte` files.
    let imports = field_set(&f.run(&["imports", "src/Button.svelte"]), "module");
    assert!(imports.contains(&"./labels".to_string()), "{imports:?}");
    let deps = field_set(&f.run(&["deps", "src/routes/+page.svelte"]), "path");
    assert!(deps.contains(&"src/Button.svelte".to_string()), "{deps:?}");

    // Spans: markup comments and script comments are both classified, and a
    // literal attribute value counts as a string.
    let count = |needle: &str, kind: &str| -> i64 {
        f.run(&["search", needle, "--in", kind, "-c"])["data"]["counts"]
            .as_array()
            .unwrap()
            .iter()
            .map(|c| c["count"].as_i64().unwrap())
            .sum()
    };
    assert_eq!(count("markup comment", "comments"), 1);
    assert_eq!(count("fire upward", "comments"), 1); // from the nested TS pass
    assert_eq!(count("fire upward", "code"), 0);
    assert_eq!(count("primary", "strings"), 1);
}

#[test]
fn extracts_vue_single_file_components() {
    let f = Fixture::new();
    f.write(
        "lib/labels.ts",
        "export function fmt(s: string) {\n  return s\n}\n",
    );
    // `<script setup lang="ts">` plus a template that binds a handler.
    f.write(
        "Button.vue",
        "<script setup lang=\"ts\">\nimport { fmt } from './lib/labels'\n\n\
         interface Props { label: string }\n\n\
         // bump it\n\
         function handleClick() {\n  return fmt('x')\n}\n</script>\n\n\
         <template>\n  <!-- markup note -->\n  \
         <button class=\"primary\" @click=\"handleClick\">go</button>\n</template>\n\n\
         <style scoped>\n.primary { color: red }\n</style>\n",
    );
    // Two script blocks (`<script>` + `<script setup>`) are both extracted.
    f.write(
        "App.vue",
        "<script>\nexport default { name: 'App' }\n</script>\n\n\
         <script setup>\nimport Button from './Button.vue'\n\
         function onChange(n) { return n }\n</script>\n\n\
         <template>\n  <Button @change=\"onChange\" />\n</template>\n",
    );

    let syms = field_set(&f.run(&["symbols", "--limit", "0"]), "name");
    for want in ["Button", "App", "handleClick", "Props", "onChange"] {
        assert!(syms.contains(&want.to_string()), "missing {want}: {syms:?}");
    }

    // Positions are the file's: handleClick sits on line 7, not line 4 of the block.
    let def = &f.run(&["definitions", "handleClick"])["data"][0];
    assert_eq!(def["line"], 7, "{def}");

    // A template binding is *code*, so the call graph sees it. Vue quotes its
    // directive values (`@click="handleClick"`), which must not be classified as
    // strings or callers/callees go blind to every handler a template binds.
    let v = f.run(&["callers", "handleClick"]);
    let rows = v["data"].as_array().unwrap();
    assert_eq!(rows.len(), 1, "template binding not seen as code: {rows:?}");
    assert_eq!(rows[0]["path"], "Button.vue");
    // A plain attribute value stays a string, though.
    let count = |needle: &str, kind: &str| -> i64 {
        f.run(&["search", needle, "--in", kind, "-c"])["data"]["counts"]
            .as_array()
            .unwrap()
            .iter()
            .map(|c| c["count"].as_i64().unwrap())
            .sum()
    };
    assert_eq!(
        count("primary", "strings"),
        1,
        "class=\"primary\" is a string"
    );
    assert_eq!(
        count("handleClick", "strings"),
        0,
        "directive value is not a string"
    );
    assert_eq!(count("markup note", "comments"), 1);
    assert_eq!(count("bump it", "comments"), 1); // from the nested TS pass

    // Component usage anchors at the `<Button …/>`, not the import above it.
    let v = f.run(&["callers", "Button"]);
    let rows = v["data"].as_array().unwrap();
    assert_eq!(rows.len(), 1, "{rows:?}");
    assert_eq!(
        rows[0]["line"], 11,
        "anchor must be <Button />, not the import"
    );

    // Cross-file graph and dependency resolution both work through the SFC.
    let callees = field_set(&f.run(&["callees", "handleClick"]), "path");
    assert!(
        callees.contains(&"lib/labels.ts".to_string()),
        "{callees:?}"
    );
    let structural = f.run(&["callees", "handleClick", "--examples"]);
    let fmt = structural["data"]
        .as_array()
        .unwrap()
        .iter()
        .find(|row| row["name"] == "fmt")
        .expect("fmt call");
    assert_eq!(
        fmt["call"], "fmt",
        "script call lost its syntax evidence: {structural}"
    );
    let deps = field_set(&f.run(&["deps", "App.vue"]), "path");
    assert!(deps.contains(&"Button.vue".to_string()), "{deps:?}");
}

#[test]
fn resolved_follows_rust_mod_declarations_as_edges() {
    // A Rust file that pulls in another via `mod thing;` (not `use crate::…`)
    // can still reach that file's definitions — the `mod` edge must count.
    let f = Fixture::new();
    f.write("lib.rs", "mod thing;\nfn root() { make(); }\n");
    f.write("thing.rs", "pub fn make() {}\n");

    let res = f.run(&["callers", "make", "--resolved"]);
    let row = res["data"]
        .as_array()
        .unwrap()
        .iter()
        .find(|r| r["name"] == "root")
        .expect("root should survive resolution");
    assert_eq!(
        row["via"], "imported",
        "mod-declared dependency should resolve: {res}"
    );
}

#[test]
fn resolved_follows_python_from_import_of_a_submodule() {
    // `from pkg import target` where target is the submodule pkg/target.py — the
    // bare module string `pkg` can't stem-match the package dir, so before the
    // qualified-import fix this reference was unresolved. Now `pkg.target` reaches
    // pkg/target.py and the reference resolves.
    let f = Fixture::new();
    f.write("pkg/__init__.py", "\n");
    f.write("pkg/target.py", "def make():\n    return 1\n");
    f.write(
        "user.py",
        "from pkg import target\ndef use():\n    return target.make()\n",
    );

    let res = f.run(&["callers", "make", "--resolved"]);
    let row = res["data"]
        .as_array()
        .unwrap()
        .iter()
        .find(|r| r["name"] == "use")
        .expect("use() should survive resolution");
    assert_eq!(
        row["via"], "imported",
        "from-import of a submodule should resolve: {res}"
    );
}

#[test]
fn resolved_follows_js_named_imports() {
    // `import { make } from './util'` — the named binding is captured qualified
    // (./util.make) so the importer resolves to the file exporting it.
    let f = Fixture::new();
    f.write("util.js", "export function make() { return 1; }\n");
    f.write(
        "user.js",
        "import { make } from './util';\nexport function use() { return make(); }\n",
    );
    f.write(
        "alias.js",
        "import { make as m } from './util';\nexport function viaAlias() { return m(); }\n",
    );

    let mods = field_set(&f.run(&["imports", "user.js"]), "module");
    assert_eq!(
        mods,
        ["./util"],
        "imports must show source modules, not internal bindings"
    );
    let amods = field_set(&f.run(&["imports", "alias.js"]), "module");
    assert_eq!(amods, ["./util"], "aliases must not add an import row");

    // user.js calls make() directly → resolves as imported.
    let res = f.run(&["callers", "make", "--resolved"]);
    let row = res["data"]
        .as_array()
        .unwrap()
        .iter()
        .find(|r| r["name"] == "use")
        .expect("use() should survive resolution");
    assert_eq!(row["via"], "imported", "{res}");

    // Resolution still needs its internal `./util.make` fact, so hiding it from
    // `imports` must not weaken named-import resolution.
    let alias = res["data"]
        .as_array()
        .unwrap()
        .iter()
        .find(|r| r["name"] == "viaAlias")
        .expect("viaAlias() should survive resolution");
    assert_eq!(alias["via"], "imported", "{res}");

    let map = f.run(&["map"]);
    assert_eq!(
        map["data"]["totals"]["imports"], 2,
        "one row per source import: {map}"
    );
    let modules = map["data"]["imports_by_module"].as_array().unwrap();
    assert_eq!(
        modules.len(),
        1,
        "internal bindings must stay out of the module summary: {map}"
    );
    assert_eq!(modules[0]["module"], "./util");
    assert_eq!(modules[0]["importers"], 2);
}

#[test]
fn import_aliases_narrow_bare_calls_and_context_expansion() {
    let f = Fixture::new();
    f.write("util.js", "export function make() { return 1; }\n");
    f.write("other.js", "export function make() { return 2; }\n");
    f.write(
        "app.js",
        "import { make as m } from './util';\nexport function run() { return m(); }\n",
    );

    let paths = field_set(&f.run(&["callees", "run"]), "path");
    assert_eq!(
        paths,
        ["util.js"],
        "bare alias call was not narrowed: {paths:?}"
    );

    let context = f.run(&["context", "--symbols", "run", "--expand", "--no-text"]);
    let paths: Vec<&str> = context["data"]["chunks"]
        .as_array()
        .unwrap()
        .iter()
        .filter(|chunk| chunk["name"] == "make")
        .filter_map(|chunk| chunk["path"].as_str())
        .collect();
    assert_eq!(
        paths,
        ["util.js"],
        "alias expansion was not narrowed: {context}"
    );
}

#[test]
fn rust_use_alias_narrows_bare_calls_and_context_expansion() {
    let f = Fixture::new();
    f.write("helper.rs", "pub fn make() { }\n");
    f.write("other.rs", "pub fn make() { }\n");
    f.write(
        "app.rs",
        "use crate::helper::make as m;\npub fn run() { m(); }\n",
    );

    let imports = f.run(&["imports", "app.rs"]);
    assert_eq!(
        imports["data"][0]["module"], "crate::helper::make",
        "{imports}"
    );

    let paths = field_set(&f.run(&["callees", "run"]), "path");
    assert_eq!(
        paths,
        ["helper.rs"],
        "Rust alias call was not narrowed: {paths:?}"
    );

    let context = f.run(&["context", "--symbols", "run", "--expand", "--no-text"]);
    let paths: Vec<&str> = context["data"]["chunks"]
        .as_array()
        .unwrap()
        .iter()
        .filter(|chunk| chunk["name"] == "make")
        .filter_map(|chunk| chunk["path"].as_str())
        .collect();
    assert_eq!(
        paths,
        ["helper.rs"],
        "Rust alias expansion was not narrowed: {context}"
    );

    let callers = field_set(
        &f.run(&["callers", "make", "--calls", "--resolved"]),
        "name",
    );
    assert_eq!(
        callers,
        ["run"],
        "Rust alias call was not attributed to make: {callers:?}"
    );

    let references = f.run(&["references", "make", "--calls", "--resolved"]);
    let calls = references["data"]["references"].as_array().unwrap();
    assert!(
        calls
            .iter()
            .any(|row| row["path"] == "app.rs" && row["via"] == "imported"),
        "Rust alias reference was not attributed to make: {references}"
    );
}

#[test]
fn python_from_import_alias_narrows_a_bare_call() {
    let f = Fixture::new();
    f.write("helper.py", "def make():\n    return 1\n");
    f.write("other.py", "def make():\n    return 2\n");
    f.write(
        "app.py",
        "from helper import make as m\ndef run():\n    return m()\n",
    );

    let paths = field_set(&f.run(&["callees", "run"]), "path");
    assert_eq!(
        paths,
        ["helper.py"],
        "Python alias call was not narrowed: {paths:?}"
    );
}

#[test]
fn rust_module_alias_narrows_a_qualified_call() {
    let f = Fixture::new();
    f.write("helper.rs", "pub fn make() {}\n");
    f.write("other.rs", "pub fn make() {}\n");
    f.write(
        "app.rs",
        "use crate::helper as h;\npub fn run() { h::make(); }\n",
    );

    let paths = field_set(&f.run(&["callees", "run"]), "path");
    assert_eq!(
        paths,
        ["helper.rs"],
        "Rust module alias was not narrowed: {paths:?}"
    );
}

#[test]
fn a_curried_call_is_attributed_only_to_its_inner_callee() {
    // `connect(mapState)(Component)` invokes what the inner call returned, and
    // the source gives that no name. Attributing the outer invocation to
    // `connect` as well put two call sites at one byte offset — the offset that
    // *identifies* a call site — which aborted the whole index build.
    let f = Fixture::new();
    f.write("a.js", "const C = connect(mapState)(Component);\n");
    f.write(
        "b.go",
        "package p\n\nfunc F() { h := Wrap(\"k\")(3); _ = h }\n",
    );
    f.write("c.py", "y = outer(1)(2)\n");
    f.write("d.rs", "fn main() { let _ = outer(1)(2); }\n");

    // The build completes, and each inner call is still found exactly once.
    for (name, want) in [("connect", "a.js"), ("Wrap", "b.go"), ("outer", "c.py")] {
        let v = f.run(&["references", name, "--calls", "--path", want]);
        let refs = v["data"]["references"].as_array().unwrap();
        assert_eq!(
            refs.len(),
            1,
            "inner call of a curried call was lost: {refs:?}"
        );
        assert_eq!(refs[0]["path"].as_str().unwrap(), want, "{refs:?}");
    }
}

#[test]
fn calls_filter_excludes_non_call_references() {
    let f = Fixture::new();
    f.write(
        "a.rs",
        "fn make() {}\n\
         fn caller() { make(); }\n\
         fn mentioner() { let make = 1; let _ = make; }\n",
    );
    // Default (name-based) counts the unrelated local `make` in mentioner.
    let all = field_set(&f.run(&["callers", "make"]), "name");
    assert!(
        all.contains(&"mentioner".to_string()),
        "default should include the mention: {all:?}"
    );

    // --calls keeps only the real call site.
    let calls = field_set(&f.run(&["callers", "make", "--calls"]), "name");
    assert!(
        calls.contains(&"caller".to_string()),
        "real caller dropped: {calls:?}"
    );
    assert!(
        !calls.contains(&"mentioner".to_string()),
        "bare mention not excluded: {calls:?}"
    );
}

#[test]
fn on_filter_disambiguates_by_receiver() {
    let f = Fixture::new();
    f.write(
        "a.rs",
        "fn f() {\n    Foo::make();\n    Bar::make();\n    thing.make();\n}\n",
    );
    let lines = |recv: &str| -> Vec<i64> {
        f.run(&["references", "make", "--on", recv])["data"]["references"]
            .as_array()
            .unwrap()
            .iter()
            .map(|r| r["line"].as_i64().unwrap())
            .collect()
    };
    assert_eq!(lines("Foo"), [2], "only Foo::make()");
    assert_eq!(lines("Bar"), [3], "only Bar::make()");
    assert_eq!(lines("thing"), [4], "method-call receiver");
}

#[test]
fn references_limit_applies_after_resolution() {
    let f = Fixture::new();
    f.write("z.py", "def target():\n    return 1\n");
    f.write("a.py", "def unrelated():\n    return object().target()\n");
    f.write(
        "b.py",
        "from z import target\n\ndef caller():\n    return target()\n",
    );

    let limited = f.run(&[
        "references",
        "target",
        "--calls",
        "--resolved",
        "--limit",
        "1",
    ]);
    let rows = limited["data"]["references"].as_array().unwrap();
    assert_eq!(rows.len(), 1, "{limited}");
    assert_eq!(rows[0]["path"], "b.py");
    assert_eq!(rows[0]["line"], 4);
    assert_eq!(rows[0]["via"], "imported");
    assert_eq!(limited["data"]["truncated"], false);

    f.write(
        "c.py",
        "from z import target\n\ndef second():\n    return target()\n",
    );
    let limited = f.run(&[
        "references",
        "target",
        "--calls",
        "--resolved",
        "--limit",
        "1",
    ]);
    let rows = limited["data"]["references"].as_array().unwrap();
    assert_eq!(rows.len(), 1, "{limited}");
    assert_eq!(rows[0]["path"], "b.py");
    assert_eq!(limited["data"]["truncated"], true);

    let unlimited = f.run(&[
        "references",
        "target",
        "--calls",
        "--resolved",
        "--limit",
        "0",
    ]);
    let rows = unlimited["data"]["references"].as_array().unwrap();
    assert_eq!(rows.len(), 2, "{unlimited}");
    assert_eq!(rows[0]["path"], "b.py");
    assert_eq!(rows[1]["path"], "c.py");
    assert_eq!(unlimited["data"]["truncated"], false);
}

#[test]
fn resolved_is_a_noop_without_a_definition_to_anchor() {
    // A name with no definition in the repo (an external symbol) can't be
    // resolved against anything, so --resolved must not silently empty the result.
    let f = Fixture::new();
    f.write(
        "a.rs",
        "fn c1() { external_thing(); }\nfn c2() { external_thing(); }\n",
    );
    let plain = f.run(&["callers", "external_thing"]);
    let resolved = f.run(&["callers", "external_thing", "--resolved"]);
    assert_eq!(
        plain["data"].as_array().unwrap().len(),
        resolved["data"].as_array().unwrap().len(),
        "no definition to anchor → --resolved should leave the result unchanged",
    );
    // And it must not have tagged anything.
    assert!(resolved["data"]
        .as_array()
        .unwrap()
        .iter()
        .all(|r| r.get("via").is_none()));
}

#[test]
fn callers_default_has_no_via_field() {
    // Resolution is opt-in: the default output shape is unchanged.
    let f = Fixture::new();
    f.write("a.rs", "fn helper() {}\nfn caller() { helper(); }\n");
    let v = f.run(&["callers", "helper"]);
    assert!(
        v["data"]
            .as_array()
            .unwrap()
            .iter()
            .all(|r| r.get("via").is_none()),
        "default callers must not carry a via field"
    );
}

#[test]
fn caller_frontiers_preserve_aliases_filters_and_affected_tests_across_batches() {
    let f = Fixture::new();
    let source = format!(
        "def leaf():\n    return 1\n{}",
        (0..70)
            .map(|i| format!("def middle{i:02}():\n    leaf()\n"))
            .collect::<String>()
    );
    f.write("pipeline.py", &source);
    f.write("notes.txt", "middle00 middle01\n");
    f.write(
        "decoy.py",
        "# middle00 middle01\ntext = 'middle02 middle03'\n",
    );
    f.write(
        "test_flow.py",
        &format!(
            "from pipeline import middle00 as chosen\ndef test_flow():\n    chosen()\n{}",
            (1..70)
                .map(|i| format!("    middle{i:02}()\n"))
                .collect::<String>()
        ),
    );
    f.git_init_commit();
    let callers = f.run(&["callers", "leaf", "--depth", "3"]);
    let rows = callers["data"].as_array().unwrap();
    assert_eq!(rows.len(), 72, "{callers}");
    assert_eq!(rows.iter().filter(|r| r["depth"] == 1).count(), 70);
    let top = rows.iter().find(|r| r["name"] == "test_flow").unwrap();
    assert_eq!(top["depth"], 2);
    assert_eq!(top["test"], true);
    assert!(rows
        .iter()
        .all(|r| r["path"] != "notes.txt" && r["path"] != "decoy.py"));
    f.write("pipeline.py", &source.replacen("return 1", "return 2", 1));
    let affected = f.run(&["affected", "--compare-to", "HEAD", "--depth", "3"]);
    assert_eq!(
        affected["data"]["impacted"].as_array().unwrap().len(),
        71,
        "{affected}"
    );
    assert_eq!(affected["data"]["tests"][0]["path"], "test_flow.py");
    assert_eq!(affected["data"]["tests"][0]["tests"], 1);
}

#[test]
fn callers_depth_walks_transitively() {
    // leaf <- mid <- top : a call chain three deep.
    let f = Fixture::new();
    f.write(
        "a.rs",
        "fn leaf() {}\n\
         fn mid() { leaf(); }\n\
         fn top() { mid(); }\n",
    );

    // depth 1: only direct callers of leaf.
    let d1 = f.run(&["callers", "leaf"]);
    let syms1: Vec<String> = d1["data"]
        .as_array()
        .unwrap()
        .iter()
        .filter_map(|r| r["name"].as_str().map(String::from))
        .collect();
    assert_eq!(syms1, ["mid"], "depth 1 is direct only: {syms1:?}");

    // depth 2: reaches top through mid, each tagged with its hop distance.
    let d2 = f.run(&["callers", "leaf", "--depth", "2"]);
    let rows = d2["data"].as_array().unwrap();
    let depth_of = |sym: &str| {
        rows.iter().find(|r| r["name"] == sym).unwrap()["depth"]
            .as_u64()
            .unwrap()
    };
    assert_eq!(depth_of("mid"), 1, "direct caller is hop 1");
    assert_eq!(depth_of("top"), 2, "transitive caller is hop 2");
}

#[test]
fn implementors_finds_rust_trait_impls() {
    let f = Fixture::new();
    f.write(
        "a.rs",
        "trait Shape {}\n\
         struct Circle;\n\
         struct Square;\n\
         impl Shape for Circle {}\n\
         impl Shape for Square {}\n\
         impl Circle { fn area(&self) {} }\n",
    );
    let v = f.run(&["implementors", "Shape"]);
    let rows = v["data"].as_array().unwrap();
    let subs: Vec<&str> = rows.iter().map(|r| r["name"].as_str().unwrap()).collect();
    assert!(
        subs.contains(&"Circle") && subs.contains(&"Square"),
        "{subs:?}"
    );
    // The inherent `impl Circle {}` is not a relation to Shape.
    assert_eq!(rows.len(), 2, "inherent impl leaked: {rows:?}");
    assert_eq!(rows[0]["relation"], "implements");
}

#[test]
fn implementors_distinguishes_extends_from_implements() {
    // Java: extends a class, implements an interface — reported distinctly.
    let f = Fixture::new();
    f.write(
        "A.java",
        "class Animal {}\n\
         interface Runner {}\n\
         class Dog extends Animal implements Runner {}\n",
    );
    let animal = f.run(&["implementors", "Animal"]);
    assert_eq!(animal["data"][0]["name"], "Dog");
    assert_eq!(animal["data"][0]["relation"], "extends");

    let runner = f.run(&["implementors", "Runner"]);
    assert_eq!(runner["data"][0]["name"], "Dog");
    assert_eq!(runner["data"][0]["relation"], "implements");

    // --relation filters.
    let only_ext = f.run(&["implementors", "Animal", "--relation", "implements"]);
    assert_eq!(
        only_ext["data"].as_array().unwrap().len(),
        0,
        "Animal is extended, not implemented"
    );
}

#[test]
fn supertypes_is_the_inverse_of_implementors() {
    let f = Fixture::new();
    f.write(
        "A.java",
        "class Animal {}\n\
         interface Runner {}\n\
         class Dog extends Animal implements Runner {}\n",
    );
    // supertypes Dog = what Dog derives from, with each relation.
    let sup = f.run(&["supertypes", "Dog"]);
    let rows = sup["data"].as_array().unwrap();
    let by: std::collections::HashMap<&str, &str> = rows
        .iter()
        .map(|r| (r["name"].as_str().unwrap(), r["relation"].as_str().unwrap()))
        .collect();
    assert_eq!(by.get("Animal"), Some(&"extends"), "{by:?}");
    assert_eq!(by.get("Runner"), Some(&"implements"), "{by:?}");

    // Inverse: Dog appears in implementors of each of its supertypes.
    for (sup_name, rel) in [("Animal", "extends"), ("Runner", "implements")] {
        let imp = f.run(&["implementors", sup_name]);
        let row = imp["data"]
            .as_array()
            .unwrap()
            .iter()
            .find(|r| r["name"] == "Dog")
            .expect("Dog missing from implementors");
        assert_eq!(row["relation"], rel);
    }

    // --relation filters supertypes too.
    let ext = f.run(&["supertypes", "Dog", "--relation", "extends"]);
    let names = field_set(&ext, "name");
    assert!(
        names.contains(&"Animal".to_string()) && !names.contains(&"Runner".to_string()),
        "{names:?}"
    );
}

#[test]
fn implementors_covers_python_and_ruby_inheritance() {
    let f = Fixture::new();
    f.write(
        "m.py",
        "class Animal:\n    pass\nclass Dog(Animal):\n    pass\n",
    );
    f.write("m.rb", "class Base\nend\nclass Sub < Base\nend\n");

    let py = f.run(&["implementors", "Animal"]);
    assert_eq!(py["data"][0]["name"], "Dog");
    let rb = f.run(&["implementors", "Base"]);
    assert_eq!(rb["data"][0]["name"], "Sub");
}

#[test]
fn unused_reports_only_definitions_with_no_use() {
    let f = Fixture::new();
    f.write(
        "a.rs",
        "fn used() {}\n\
         fn dead() {}\n\
         fn caller() { used(); }\n",
    );
    let names = field_set(&f.run(&["unused"]), "name");
    assert!(
        names.contains(&"dead".to_string()),
        "dead fn not reported: {names:?}"
    );
    assert!(
        !names.contains(&"used".to_string()),
        "used fn wrongly reported: {names:?}"
    );
    // `caller` references `used` but nothing references `caller` → also unused.
    assert!(names.contains(&"caller".to_string()), "{names:?}");
}

#[test]
fn unused_excludes_javascript_constructors() {
    let f = Fixture::new();
    f.write(
        "service.js",
        "class Service {\n  constructor() {}\n  used() {}\n  dead() {}\n}\n\nnew Service().used();\n",
    );

    let names = field_set(&f.run(&["unused"]), "name");
    assert!(
        !names.contains(&"constructor".to_string()),
        "constructor is invoked through new: {names:?}"
    );
    assert!(
        !names.contains(&"used".to_string()),
        "called method: {names:?}"
    );
    assert!(
        names.contains(&"dead".to_string()),
        "ordinary unused method: {names:?}"
    );
}

#[test]
fn unused_is_conservative_about_string_mentions() {
    // `unused` under-reports rather than risk flagging something an agent might
    // delete, so a mention in a *string* counts as a use: it may be reflection,
    // a serde rename, a config key or a SQL identifier. (A mention in a comment
    // does not count — see `a_comment_mention_no_longer_masks_dead_code`.)
    let f = Fixture::new();
    f.write(
        "a.rs",
        "fn ghost() {}\nfn main() { log(\"ghost is named here\"); }\nfn log(_: &str) {}\n",
    );
    let names = field_set(&f.run(&["unused"]), "name");
    assert!(
        !names.contains(&"ghost".to_string()),
        "a string mention should count as a use: {names:?}"
    );
}

#[test]
fn unused_excludes_test_files_by_default() {
    let f = Fixture::new();
    f.write("src/lib.rs", "fn dead() {}\n");
    f.write("tests/it.rs", "fn a_test_case() {}\n");

    // Default: the test-file definition is not reported.
    let def = field_set(&f.run(&["unused"]), "name");
    assert!(
        def.contains(&"dead".to_string()),
        "real dead code missing: {def:?}"
    );
    assert!(
        !def.contains(&"a_test_case".to_string()),
        "test-file def leaked by default: {def:?}"
    );

    // --include-tests brings it in.
    let inc = field_set(&f.run(&["unused", "--include-tests"]), "name");
    assert!(inc.contains(&"a_test_case".to_string()), "{inc:?}");
}

#[test]
fn tree_reports_shape_with_counts_aggregated_from_below() {
    let f = Fixture::new();
    f.write("src/app/main.rs", "fn a() {}\nfn b() {}\n");
    f.write("src/app/deep/util.rs", "fn c() {}\n");
    f.write("data/x.json", "{}\n");
    f.write("data/y.json", "{}\n");

    let rows = f.run(&["tree"])["data"].as_array().unwrap().clone();
    let row = |p: &str| {
        rows.iter()
            .find(|r| r["path"] == p)
            .unwrap_or_else(|| panic!("no {p}: {rows:?}"))
            .clone()
    };

    // A parent counts everything beneath it, not just its own children.
    assert_eq!(row("src")["files"].as_i64(), Some(2), "{rows:?}");
    assert_eq!(row("src")["symbols"].as_i64(), Some(3), "{rows:?}");
    assert_eq!(row("src")["depth"].as_i64(), Some(0));
    assert_eq!(row("src/app")["depth"].as_i64(), Some(1));
    assert_eq!(
        row("src/app")["dirs"].as_i64(),
        Some(1),
        "immediate subdirs"
    );
    assert_eq!(
        row("data")["symbols"].as_i64(),
        Some(0),
        "json holds no definitions"
    );

    // --min-symbols skips the data tree entirely.
    let coded = field_set(&f.run(&["tree", "--min-symbols", "1"]), "path");
    assert!(coded.contains(&"src".to_string()), "{coded:?}");
    assert!(
        !coded.contains(&"data".to_string()),
        "data should be hidden: {coded:?}"
    );

    // Folding summarises rather than truncates: `src` still reports all 3 symbols.
    let shallow = f.run(&["tree", "--depth", "1"])["data"]
        .as_array()
        .unwrap()
        .clone();
    let paths: Vec<&str> = shallow
        .iter()
        .map(|r| r["path"].as_str().unwrap())
        .collect();
    assert!(paths.contains(&"src"), "{paths:?}");
    assert!(
        !paths.contains(&"src/app"),
        "should be folded away: {paths:?}"
    );
    let src = shallow.iter().find(|r| r["path"] == "src").unwrap();
    assert_eq!(
        src["symbols"].as_i64(),
        Some(3),
        "folded row must keep the total"
    );
}

#[test]
fn callees_examples_show_the_qualified_call_expression() {
    // The motivating case: a module-qualified call is attributed by name, so
    // `subprocess.run(...)` lists every definition named `run`. The row now says
    // which expression produced it, so the over-match is visible without having
    // to already suspect it and reach for --resolved.
    let f = Fixture::new();
    f.write("lib.py", "def run():\n    pass\n");
    f.write(
        "app.py",
        "\u{feff}import subprocess\r\n# café 🐍\r\ndef go():\r\n    subprocess.run(['ls'])\r\n    subprocess.run(['pwd'])\r\n",
    );

    let plain = f.run(&["callees", "go"])["data"]
        .as_array()
        .unwrap()
        .clone();
    let run_row = plain
        .iter()
        .find(|r| r["name"] == "run")
        .expect("run is a callee");
    assert!(
        run_row.get("call").is_none(),
        "call must be opt-in: {run_row:?}"
    );

    let ex = f.run(&["callees", "go", "--examples"])["data"]
        .as_array()
        .unwrap()
        .clone();
    let run_row = ex
        .iter()
        .find(|r| r["name"] == "run")
        .expect("run is a callee");
    assert_eq!(
        run_row["call"].as_str(),
        Some("subprocess.run"),
        "{run_row:?}"
    );
    assert_eq!(
        run_row["call_line"].as_i64(),
        Some(4),
        "line in the caller's file"
    );
}

#[test]
fn codeqignore_excludes_committed_trees_gitignore_cannot() {
    // Data committed to the repo is invisible to .gitignore by definition, so a
    // data-heavy repo had no way to keep it out of the index.
    let f = Fixture::new();
    f.write("src.rs", "fn keep() {}\n");
    f.write("data/a.json", "{\"a\":1}\n");
    f.write("data/b.json", "{\"b\":2}\n");
    assert_eq!(f.run(&["files"])["data"].as_array().unwrap().len(), 3);

    f.write(".codeqignore", "data/\n");
    let after = field_set(&f.run(&["files"]), "path");
    assert!(
        after.contains(&"src.rs".to_string()),
        "code must survive: {after:?}"
    );
    assert!(
        !after.iter().any(|p| p.starts_with("data/")),
        "data not excluded: {after:?}"
    );

    // Negation works against a file pattern. It does NOT resurrect a file whose
    // parent directory was excluded (`data/`) — the walker never descends, exactly
    // as git behaves — so the pattern has to exclude files, not the directory.
    f.write(".codeqignore", "data/*.json\n!data/b.json\n");
    let back = field_set(&f.run(&["files"]), "path");
    assert!(
        back.contains(&"data/b.json".to_string()),
        "negation ignored: {back:?}"
    );
    assert!(!back.contains(&"data/a.json".to_string()), "{back:?}");
}

#[test]
fn hotspots_ranks_by_reference_count_and_is_one_row_per_name() {
    let f = Fixture::new();
    f.write(
        "src/lib.rs",
        "pub fn hot() {}\npub fn cold() {}\npub fn unref() {}\n\
         pub fn a() { hot(); hot(); cold(); }\n\
         pub fn b() { hot(); }\n",
    );
    let rows = f.run(&["hotspots"])["data"].as_array().unwrap().clone();
    let by: std::collections::HashMap<String, i64> = rows
        .iter()
        .map(|r| {
            (
                r["name"].as_str().unwrap().to_string(),
                r["refs"].as_i64().unwrap(),
            )
        })
        .collect();

    assert_eq!(by.get("hot"), Some(&3), "three call sites: {by:?}");
    assert_eq!(by.get("cold"), Some(&1), "{by:?}");
    // The declaration is not a use of itself, so a never-used fn drops out at the
    // default --min-refs 1.
    assert!(
        !by.contains_key("unref"),
        "unreferenced fn should not rank: {by:?}"
    );
    // Most-referenced first.
    assert_eq!(
        rows[0]["name"].as_str().unwrap(),
        "hot",
        "not sorted by refs: {rows:?}"
    );
}

#[test]
fn hotspots_collapses_same_named_definitions_into_one_row() {
    // The count is a property of the *name*, so two definitions spelling it the
    // same must not print the same number twice.
    let f = Fixture::new();
    f.write("a.rs", "fn shared() {}\nfn ca() { shared(); }\n");
    f.write("b.rs", "fn shared() {}\nfn cb() { shared(); }\n");

    let rows = f.run(&["hotspots"])["data"].as_array().unwrap().clone();
    let shared: Vec<_> = rows.iter().filter(|r| r["name"] == "shared").collect();
    assert_eq!(shared.len(), 1, "one row per name expected: {rows:?}");
    assert_eq!(
        shared[0]["defs"].as_i64(),
        Some(2),
        "defs should report the collapse"
    );
    assert_eq!(
        shared[0]["refs"].as_i64(),
        Some(2),
        "both call sites counted"
    );
}

#[test]
fn hotspots_calls_mode_excludes_bare_mentions_and_declarations() {
    let f = Fixture::new();
    f.write(
        "src/lib.rs",
        "fn target() {}\nfn use_it() { target(); let f = target; }\n\
         // target in a comment\nfn s() -> &'static str { \"target in a string\" }\n",
    );
    // Default counts the bare mention too; comment and string never count.
    let all = field_set(&f.run(&["hotspots"]), "name");
    assert!(all.contains(&"target".to_string()), "{all:?}");

    let refs = |args: &[&str]| -> i64 {
        f.run(args)["data"]
            .as_array()
            .unwrap()
            .iter()
            .find(|r| r["name"] == "target")
            .map(|r| r["refs"].as_i64().unwrap())
            .unwrap_or(0)
    };
    assert_eq!(
        refs(&["hotspots"]),
        2,
        "call site + bare mention, not comment/string"
    );
    assert_eq!(refs(&["hotspots", "--calls"]), 1, "only the call site");
}

#[test]
fn callpath_is_the_only_name_for_it() {
    // `path` was the original name and briefly survived as an alias. It is gone:
    // `--path` scopes fifteen commands and `path` is a key on every row, so the
    // word means "file path" everywhere else in codeq. Keeping the alias meant
    // `codeq path src/db.rs` -- the natural thing to type -- failed with
    // "required argument <TO>", diagnosing a typo as a missing symbol.
    let f = Fixture::new();
    f.write(
        "a.rs",
        "fn top() { mid(); }\nfn mid() { bottom(); }\nfn bottom() {}\n",
    );

    let steps = f.run(&["callpath", "top", "bottom"]);
    let steps = steps["data"].as_array().unwrap();
    let names: Vec<&str> = steps.iter().map(|s| s["name"].as_str().unwrap()).collect();
    assert_eq!(names, ["top", "mid", "bottom"], "{steps:?}");

    let out = f.raw(&["path", "top", "bottom"]);
    assert!(
        !out.status.success(),
        "`path` must not resolve to a command"
    );
    let err = String::from_utf8(out.stderr).unwrap();
    assert!(
        err.contains("unrecognized subcommand"),
        "should read as a typo, not a bad call: {err}"
    );
}

#[test]
fn unused_excludes_rust_unit_tests_in_a_cfg_test_module() {
    // Rust's dominant idiom puts unit tests in a `mod tests` inside the file
    // under test: the path is ordinary and the fn names are prose, so neither
    // the path nor the name heuristic sees them. They used to be reported as
    // dead code — on codeq itself, every row `unused` produced was one of these.
    let f = Fixture::new();
    f.write(
        "src/lib.rs",
        "fn dead() {}\n\n#[cfg(test)]\nmod tests {\n    #[test]\n    fn returns_one() {}\n}\n",
    );

    let def = field_set(&f.run(&["unused"]), "name");
    assert!(
        def.contains(&"dead".to_string()),
        "real dead code missing: {def:?}"
    );
    assert!(
        !def.contains(&"returns_one".to_string()),
        "unit test reported as dead: {def:?}"
    );

    // Still reachable when you ask for tests explicitly.
    let inc = field_set(&f.run(&["unused", "--include-tests"]), "name");
    assert!(inc.contains(&"returns_one".to_string()), "{inc:?}");
}

#[test]
fn path_finds_shortest_call_chain() {
    let f = Fixture::new();
    f.write(
        "a.rs",
        "fn leaf() {}\n\
         fn mid() { leaf(); }\n\
         fn top() { mid(); }\n\
         fn other() {}\n",
    );
    let v = f.run(&["callpath", "top", "leaf"]);
    let steps: Vec<&str> = v["data"]
        .as_array()
        .unwrap()
        .iter()
        .map(|r| r["name"].as_str().unwrap())
        .collect();
    assert_eq!(steps, ["top", "mid", "leaf"], "chain wrong: {steps:?}");
    // Depth indexes the position in the chain.
    let depths: Vec<u64> = v["data"]
        .as_array()
        .unwrap()
        .iter()
        .map(|r| r["depth"].as_u64().unwrap())
        .collect();
    assert_eq!(depths, [0, 1, 2]);
}

#[test]
fn path_scopes_the_start_symbol_by_path() {
    // Two `entry` functions in different files; only one reaches `leaf`.
    let f = Fixture::new();
    f.write("a.rs", "fn leaf() {}\nfn entry() { leaf(); }\n");
    f.write("b.rs", "fn entry() { unrelated(); }\nfn unrelated() {}\n");

    // Scoped to b.rs, `entry` can't reach leaf.
    let none = f.run(&["callpath", "entry", "leaf", "--path", "b.rs"]);
    assert_eq!(
        none["data"].as_array().unwrap().len(),
        0,
        "b.rs entry shouldn't reach leaf"
    );

    // Scoped to a.rs, it does — and starts from the a.rs definition.
    let hit = f.run(&["callpath", "entry", "leaf", "--path", "a.rs"]);
    let rows = hit["data"].as_array().unwrap();
    assert_eq!(rows[0]["name"], "entry");
    assert_eq!(
        rows[0]["path"], "a.rs",
        "start location should be the scoped def"
    );
    let syms: Vec<&str> = rows.iter().map(|r| r["name"].as_str().unwrap()).collect();
    assert_eq!(syms, ["entry", "leaf"]);
}

#[test]
fn path_empty_when_unreachable_and_single_when_self() {
    let f = Fixture::new();
    f.write("a.rs", "fn leaf() {}\nfn top() { leaf(); }\n");

    // leaf does not reach top.
    let none = f.run(&["callpath", "leaf", "top"]);
    assert_eq!(
        none["data"].as_array().unwrap().len(),
        0,
        "should be unreachable"
    );

    // a symbol reaches itself in zero hops.
    let self_path = f.run(&["callpath", "top", "top"]);
    let rows = self_path["data"].as_array().unwrap();
    assert_eq!(rows.len(), 1);
    assert_eq!(rows[0]["name"], "top");
    assert_eq!(rows[0]["depth"], 0);

    let missing = f.run(&["callpath", "missing", "missing"]);
    assert_eq!(missing["data"].as_array().unwrap().len(), 0);
    assert!(
        missing["note"]
            .as_str()
            .unwrap()
            .contains("no symbol named 'missing'"),
        "{missing}"
    );

    let out_of_scope = f.run(&["callpath", "top", "top", "--path", "absent.rs"]);
    assert_eq!(out_of_scope["data"].as_array().unwrap().len(), 0);
    assert!(
        out_of_scope["note"]
            .as_str()
            .unwrap()
            .contains("nothing matches --path absent.rs"),
        "{out_of_scope}"
    );
}

#[test]
fn path_respects_max_depth() {
    let f = Fixture::new();
    f.write(
        "a.rs",
        "fn d4() {}\nfn c3() { d4(); }\nfn b2() { c3(); }\nfn a1() { b2(); }\n",
    );
    // a1 -> b2 -> c3 -> d4 is 3 hops; a max-depth of 2 can't reach d4.
    let shallow = f.run(&["callpath", "a1", "d4", "--max-depth", "2"]);
    assert_eq!(
        shallow["data"].as_array().unwrap().len(),
        0,
        "3-hop chain found at depth 2"
    );
    let deep = f.run(&["callpath", "a1", "d4", "--max-depth", "3"]);
    let steps: Vec<&str> = deep["data"]
        .as_array()
        .unwrap()
        .iter()
        .map(|r| r["name"].as_str().unwrap())
        .collect();
    assert_eq!(steps, ["a1", "b2", "c3", "d4"]);
}

#[test]
fn callees_depth_walks_transitively() {
    let f = Fixture::new();
    f.write(
        "a.rs",
        "fn leaf() {}\n\
         fn mid() { leaf(); }\n\
         fn top() { mid(); }\n",
    );

    let d1 = field_set(&f.run(&["callees", "top"]), "name");
    assert!(d1.contains(&"mid".to_string()), "{d1:?}");
    assert!(
        !d1.contains(&"leaf".to_string()),
        "leaf is two hops down: {d1:?}"
    );

    let d2 = f.run(&["callees", "top", "--depth", "2"]);
    let rows = d2["data"].as_array().unwrap();
    let names: Vec<&str> = rows.iter().map(|r| r["name"].as_str().unwrap()).collect();
    assert!(
        names.contains(&"mid") && names.contains(&"leaf"),
        "{names:?}"
    );
    let depth_of = |n: &str| {
        rows.iter().find(|r| r["name"] == n).unwrap()["depth"]
            .as_u64()
            .unwrap()
    };
    assert_eq!(depth_of("mid"), 1);
    assert_eq!(depth_of("leaf"), 2);
}

#[test]
fn about_path_scopes_to_one_definition() {
    // Two same-named `thing`s in different files must not be merged.
    let f = Fixture::new();
    f.write("a.rs", "fn thing() { helper_a(); }\nfn helper_a() {}\n");
    f.write("b.rs", "fn thing() { helper_b(); }\nfn helper_b() {}\n");

    let all = f.run(&["about", "thing"]);
    assert_eq!(
        all["data"]["definitions"].as_array().unwrap().len(),
        2,
        "unscoped sees both"
    );

    let scoped = f.run(&["about", "thing", "--path", "a.rs"]);
    let d = &scoped["data"];
    assert_eq!(d["definitions"].as_array().unwrap().len(), 1);
    let callees: Vec<&str> = d["callees"]
        .as_array()
        .unwrap()
        .iter()
        .map(|c| c["name"].as_str().unwrap())
        .collect();
    assert!(callees.contains(&"helper_a"), "{callees:?}");
    assert!(
        !callees.contains(&"helper_b"),
        "scoped callees leaked the other def: {callees:?}"
    );
}

#[test]
fn references_uses_word_boundaries() {
    let f = Fixture::new();
    // `find` must not match `finder`.
    f.write("a.rs", "fn find() {}\nfn finder() {}\nfn c() { find(); }\n");
    let v = f.run(&["references", "find"]);
    let lines: Vec<i64> = v["data"]["references"]
        .as_array()
        .unwrap()
        .iter()
        .map(|r| r["line"].as_i64().unwrap())
        .collect();
    assert!(lines.contains(&1), "definition line");
    assert!(lines.contains(&3), "call site");
    assert!(!lines.contains(&2), "must not match finder");
}

#[test]
fn at_resolves_innermost_enclosing_definition() {
    let f = Fixture::new();
    f.write("m.py", "class C:\n    def meth(self):\n        return 1\n");
    // Line 3 is inside both C and meth; innermost (meth) comes first.
    let v = f.run(&["at", "m.py:3"]);
    let rows = v["data"].as_array().unwrap();
    assert_eq!(rows[0]["name"], "meth");
    assert_eq!(rows[0]["kind"], "method", "declared inside a class");
}

#[test]
fn at_rejects_bad_location() {
    let f = Fixture::new();
    f.write("m.py", "def x():\n    pass\n");
    assert!(!f.raw(&["at", "m.py"]).status.success(), "missing :line");
    assert!(
        !f.raw(&["at", "m.py:abc"]).status.success(),
        "non-numeric line"
    );
}

#[test]
fn changed_symbols_reports_only_touched_definitions() {
    let f = Fixture::new();
    f.write(
        "a.rs",
        "fn keep() {\n    1;\n}\n\nfn change() {\n    1;\n}\n",
    );
    f.git_init_commit();
    // Edit only change()'s body.
    f.write(
        "a.rs",
        "fn keep() {\n    1;\n}\n\nfn change() {\n    2;\n}\n",
    );

    let v = f.run(&["changed", "--symbols", "--compare-to", "HEAD"]);
    let names = field_set(&v, "name");
    assert!(names.contains(&"change".to_string()), "{names:?}");
    assert!(
        !names.contains(&"keep".to_string()),
        "untouched fn leaked: {names:?}"
    );
}

#[test]
fn changed_symbols_and_affected_include_definitions_with_deleted_lines() {
    let f = Fixture::new();
    f.write(
        "a.rs",
        "fn keep() {\n    let value = 1;\n}\n\nfn change() {\n    let before = 1;\n    let removed = 2;\n    let after = before + 1;\n}\n",
    );
    f.git_init_commit();
    f.write(
        "a.rs",
        "fn keep() {\n    let value = 1;\n}\n\nfn change() {\n    let before = 1;\n    let after = before + 1;\n}\n",
    );

    let changed = f.run(&["changed", "--symbols", "--compare-to", "HEAD"]);
    let names = field_set(&changed, "name");
    assert!(names.contains(&"change".to_string()), "{changed}");
    assert!(
        !names.contains(&"keep".to_string()),
        "untouched definition leaked: {changed}"
    );

    let affected = f.run(&["affected", "--compare-to", "HEAD"]);
    let names: Vec<&str> = affected["data"]["changed"]
        .as_array()
        .unwrap()
        .iter()
        .filter_map(|row| row["name"].as_str())
        .collect();
    assert!(names.contains(&"change"), "{affected}");
}

// ---------------------------------------------------------------------------
// Search (content + enclosing symbol)
// ---------------------------------------------------------------------------

#[test]
fn search_annotates_matches_with_enclosing_symbol() {
    let f = Fixture::new();
    f.write(
        "a.rs",
        "fn add(a: i32) -> i32 {\n    let magic = 42;\n    a + magic\n}\n",
    );
    let v = f.run(&["search", "magic"]);
    let m = v["data"]["matches"].as_array().unwrap();
    assert!(!m.is_empty());
    for row in m {
        assert_eq!(row["symbol"], "add");
        assert_eq!(row["kind"], "function");
    }
    assert_eq!(v["data"]["truncated"], false);
}

#[test]
fn search_top_level_match_has_no_symbol() {
    let f = Fixture::new();
    f.write("a.rs", "// TODO cleanup\nfn f() {}\n");
    let v = f.run(&["search", "TODO"]);
    let row = &v["data"]["matches"][0];
    assert_eq!(row["line"], 1);
    assert!(
        row.get("symbol").is_none(),
        "comment is inside no definition"
    );
}

#[test]
fn search_fixed_is_literal_not_regex() {
    let f = Fixture::new();
    f.write("a.py", "x = a.b\ny = aXb\n");
    // Regex `a.b` matches both `a.b` and `aXb` (. is any char).
    let re = f.run(&["search", "a.b"]);
    assert_eq!(re["data"]["matches"].as_array().unwrap().len(), 2);
    // Literal `a.b` matches only the first line.
    let fixed = f.run(&["search", "a.b", "--fixed"]);
    let m = fixed["data"]["matches"].as_array().unwrap();
    assert_eq!(m.len(), 1);
    assert_eq!(m[0]["line"], 1);
}

#[test]
fn search_language_filter() {
    let f = Fixture::new();
    f.write("a.rs", "fn foo() { bar(); }\n");
    f.write("b.py", "def foo():\n    bar()\n");
    let v = f.run(&["search", "bar", "--language", "python"]);
    let m = v["data"]["matches"].as_array().unwrap();
    assert!(m.iter().all(|r| r["path"] == "b.py"), "{m:?}");
    assert!(!m.is_empty());
}

#[test]
fn search_reports_truncation() {
    let f = Fixture::new();
    f.write("a.rs", "fn a() {}\nfn b() {}\nfn c() {}\n");
    let v = f.run(&["search", "fn", "--limit", "1"]);
    assert_eq!(v["data"]["matches"].as_array().unwrap().len(), 1);
    assert_eq!(v["data"]["truncated"], true, "truncation must be visible");
}

#[test]
fn search_reflects_deleted_files() {
    // Content lives in the FTS index; a removed file must not leave phantom
    // matches behind.
    let f = Fixture::new();
    f.write("a.rs", "fn zzunique() {}\n");
    assert_eq!(
        f.run(&["search", "zzunique"])["data"]["matches"]
            .as_array()
            .unwrap()
            .len(),
        1
    );
    f.remove("a.rs");
    assert_eq!(
        f.run(&["search", "zzunique"])["data"]["matches"]
            .as_array()
            .unwrap()
            .len(),
        0,
        "deleted file still searchable — FTS not pruned"
    );
}

#[test]
fn search_context_lines() {
    let f = Fixture::new();
    f.write("a.rs", "fn a() {}\nlet target = 1;\nfn b() {}\n");
    let v = f.run(&["search", "target", "-C", "1"]);
    let m = &v["data"]["matches"][0];
    assert_eq!(m["before"].as_array().unwrap().len(), 1);
    assert_eq!(m["after"].as_array().unwrap().len(), 1);
    assert!(m["before"][0].as_str().unwrap().contains("fn a"));
    assert!(m["after"][0].as_str().unwrap().contains("fn b"));

    // Text mode must render the context lines too (grep-style `path-line-`), not
    // silently drop the -C flag. Match on line 2, context on 1 and 3.
    let text =
        String::from_utf8_lossy(&f.raw(&["search", "target", "-C", "1"]).stdout).into_owned();
    assert!(
        text.contains("a.rs-1- fn a() {}"),
        "before-context line missing: {text}"
    );
    assert!(
        text.contains("a.rs:2:") && text.contains("let target = 1;"),
        "match line missing: {text}"
    );
    assert!(
        text.contains("a.rs-3- fn b() {}"),
        "after-context line missing: {text}"
    );
}

#[test]
fn search_context_windows_merge_instead_of_repeating_lines() {
    // Hits on lines 2, 3 and 5 with -C 1: grep prints each line once, and a hit
    // is always a `:` row, never first shown as `-` context for the hit before.
    let f = Fixture::new();
    f.write("a.txt", "one\nhit a\nhit b\nfour\nhit c\nsix\n");
    let text = String::from_utf8_lossy(&f.raw(&["search", "hit", "-C", "1"]).stdout).into_owned();
    let rows: Vec<&str> = text.lines().filter(|l| l.starts_with("a.txt")).collect();
    assert_eq!(
        rows,
        vec![
            "a.txt-1- one",
            "a.txt:2: hit a",
            "a.txt:3: hit b",
            "a.txt-4- four",
            "a.txt:5: hit c",
            "a.txt-6- six",
        ],
        "{text}"
    );
}

#[test]
fn search_context_does_not_reprint_a_hit_on_the_first_line() {
    // A hit on line 1 has no window of its own, so nothing marked it printed and
    // the next hit's `-B` repeated it as `a.txt-1-` right under `a.txt:1:`.
    let f = Fixture::new();
    f.write("a.txt", "hit a\nhit b\nthree\n");
    let text = String::from_utf8_lossy(&f.raw(&["search", "hit", "-B", "1"]).stdout).into_owned();
    let rows: Vec<&str> = text.lines().filter(|l| l.starts_with("a.txt")).collect();
    assert_eq!(rows, vec!["a.txt:1: hit a", "a.txt:2: hit b"], "{text}");
}

#[test]
fn search_invert_carries_its_context_window() {
    // `rg -v -C1 hit` prints the matching lines beside each non-matching one as
    // `-` context. `-v` hits were built with empty windows, so -A/-B/-C did nothing.
    let f = Fixture::new();
    f.write("a.txt", "hit a\nplain\nhit b\nhit c\nplain two\n");
    let text =
        String::from_utf8_lossy(&f.raw(&["search", "hit", "-v", "-C", "1"]).stdout).into_owned();
    let rows: Vec<&str> = text.lines().filter(|l| l.starts_with("a.txt")).collect();
    assert_eq!(
        rows,
        vec![
            "a.txt-1- hit a",
            "a.txt:2: plain",
            "a.txt-3- hit b",
            "a.txt-4- hit c",
            "a.txt:5: plain two",
        ],
        "{text}"
    );
}

#[test]
fn graph_walks_refuse_a_zero_hop_depth() {
    // `--depth 1` is the direct neighbours, so 0 names no walk. `callers` and
    // `callees` quietly ran it as 1 and `deps` answered with nothing.
    let f = Fixture::new();
    f.write("a.rs", "fn a() { b() }\nfn b() {}\n");
    for args in [
        ["callers", "b", "--depth", "0"],
        ["callees", "a", "--depth", "0"],
        ["deps", "a.rs", "--depth", "0"],
    ] {
        let out = f.raw(&args);
        assert!(!out.status.success(), "{args:?} should be refused");
        let err = String::from_utf8_lossy(&out.stderr);
        assert!(err.contains("at least 1"), "{args:?}: {err}");
    }
    let v = f.run(&["callers", "b", "--depth", "1"]);
    assert_eq!(v["data"][0]["name"], "a", "{v}");
}

#[test]
fn search_has_no_line_after_the_trailing_newline() {
    // `a\n\nb\n` is three lines. The empty string after the final newline is not
    // a fourth, so `^$` must find the one blank line and `-A` must not invent
    // a trailing context row. This is what `rg -c '^$'` reports.
    let f = Fixture::new();
    f.write("a.txt", "a\n\nb\n");
    let v = f.run(&["search", "^$", "-c"]);
    assert_eq!(v["data"]["counts"][0]["count"], 1, "{v}");

    let v = f.run(&["search", "^$"]);
    let lines: Vec<i64> = v["data"]["matches"]
        .as_array()
        .unwrap()
        .iter()
        .map(|m| m["line"].as_i64().unwrap())
        .collect();
    assert_eq!(lines, vec![2]);

    let v = f.run(&["search", "b", "-A", "2"]);
    let after = &v["data"]["matches"][0]["after"];
    assert!(
        after.is_null() || after.as_array().is_some_and(|a| a.is_empty()),
        "{v}"
    );

    // -l / -L ask the same question as the rows do.
    f.write("b.txt", "only\n");
    let v = f.run(&["search", "^$", "-l"]);
    assert_eq!(field_names(&v["data"]["files"]), vec!["a.txt".to_string()]);
}

#[test]
fn search_files_with_matches() {
    let f = Fixture::new();
    f.write("a.rs", "fn needle() {}\n");
    f.write("b.rs", "fn other() {}\n");
    let v = f.run(&["search", "needle", "-l"]);
    let files = field_names(&v["data"]["files"]);
    assert_eq!(files, vec!["a.rs".to_string()]);
    assert!(
        v["data"].get("matches").is_none(),
        "-l returns paths, not matches"
    );
}

#[test]
fn search_counts_match_start_lines_across_line_endings_and_empty_matches() {
    let f = Fixture::new();
    for body in ["", "\n", "x x\r\n\néx tail", "x x\r\n\néx tail\n"] {
        f.write("a.txt", body);
        for pattern in [
            "x",
            "x|tail",
            "é|x",
            "^",
            "$",
            "",
            r"\n",
            r"x\r?\n\néx",
            ".*",
        ] {
            let matches = f.run(&["search", pattern, "--limit", "0"]);
            let lines: std::collections::HashSet<u64> = matches["data"]["matches"]
                .as_array()
                .unwrap()
                .iter()
                .map(|row| row["line"].as_u64().unwrap())
                .collect();
            let counts = f.run(&["search", pattern, "-c", "--limit", "0"]);
            let total: u64 = counts["data"]["counts"]
                .as_array()
                .unwrap()
                .iter()
                .map(|row| row["count"].as_u64().unwrap())
                .sum();
            assert_eq!(total as usize, lines.len(), "{body:?}, {pattern:?}");
        }
    }
}

#[test]
fn search_counts_only_lines_with_a_surviving_syntax_match() {
    let f = Fixture::new();
    f.write(
        "a.py",
        "# needle needle\nvalue = 'needle'; needle()\nneedle() # needle\n",
    );
    for (class, expected) in [("code", 2), ("comments", 2), ("strings", 1)] {
        let result = f.run(&["search", "needle", "-c", "-w", "--in", class]);
        assert_eq!(result["data"]["counts"][0]["count"], expected, "{result}");
    }
}

#[test]
fn broad_unlimited_searches_preserve_order_filters_counts_and_file_caps() {
    let f = Fixture::new();
    let padding = format!("# {}\n", "x".repeat(1_050_000));
    for i in (0..8).rev() {
        f.write(
            &format!("file{i}.py"),
            &format!(
                "{padding}# NEEDLE\nvalue = 'NEEDLE'\ndef sample{i}():\n    NEEDLE(); NEEDLE()\n"
            ),
        );
    }
    for (filter, expected) in [("code", 1), ("comments", 1), ("strings", 1), ("any", 3)] {
        let mut args = vec!["search", "N.E.LE", "--limit", "0"];
        if filter != "any" {
            args.extend(["--in", filter]);
        }
        let matches = f.run(&args);
        let rows = matches["data"]["matches"].as_array().unwrap();
        assert_eq!(rows.len(), expected * 8, "{filter}: {matches}");
        for (i, chunk) in rows.chunks(expected).enumerate() {
            assert!(chunk.iter().all(|r| r["path"] == format!("file{i}.py")));
        }
        args.push("-c");
        let counts = f.run(&args);
        let counts = counts["data"]["counts"].as_array().unwrap();
        assert_eq!(counts.len(), 8);
        assert!(counts.iter().all(|r| r["count"] == expected));
    }
    let capped = f.run(&["search", "N.E.LE", "--limit", "0", "--max-per-file", "1"]);
    assert_eq!(capped["data"]["matches"].as_array().unwrap().len(), 8);
    assert_eq!(capped["notes"]["truncated_per_file"]["file0.py"], 3);
    let limited = f.run(&["search", "N.E.LE", "--limit", "2"]);
    assert_eq!(limited["data"]["matches"].as_array().unwrap().len(), 2);
    assert_eq!(limited["data"]["truncated"], true);
    let occurrences = f.run(&["search", "N.E.LE", "--limit", "0", "-o", "--in", "code"]);
    assert_eq!(occurrences["data"]["matches"].as_array().unwrap().len(), 16);
}

#[test]
fn search_count_per_file() {
    let f = Fixture::new();
    f.write("a.rs", "x x\nx\n"); // two lines contain x
    let v = f.run(&["search", "x", "-c"]);
    let c = &v["data"]["counts"][0];
    assert_eq!(c["path"], "a.rs");
    assert_eq!(c["count"], 2, "counts matching lines, not occurrences");
}

#[test]
fn span_batches_preserve_filters_across_batch_boundaries_and_edits() {
    let f = Fixture::new();
    for count in [31, 32, 33, 64, 65, 129, 1] {
        let source: String = (0..count)
            .map(|i| format!("value_{i} = \"needle\" # needle\n"))
            .collect();
        f.write("source.py", &source);
        for class in ["comments", "strings"] {
            let result = f.run(&["search", "needle", "--in", class, "--limit", "0"]);
            let rows = result["data"]["matches"].as_array().unwrap();
            assert_eq!(rows.len(), count, "{class}: {result}");
            for (i, row) in rows.iter().enumerate() {
                assert_eq!(row["line"], i + 1, "{result}");
            }
        }
        let code = f.run(&["search", "needle", "--in", "code", "--limit", "0"]);
        assert!(
            code["data"]["matches"].as_array().unwrap().is_empty(),
            "{code}"
        );
    }
}

#[test]
fn search_in_filters_by_syntactic_class() {
    let f = Fixture::new();
    // 'needle' in a comment (l1), a string (l2), and code (l3).
    f.write(
        "a.rs",
        "// needle here\nlet s = \"needle\";\nfn needle_fn() {}\n",
    );
    let matches = |v: &serde_json::Value| v["data"]["matches"].as_array().unwrap().len();

    assert_eq!(matches(&f.run(&["search", "needle"])), 3, "all classes");

    let com = f.run(&["search", "needle", "--in", "comments"]);
    assert_eq!(matches(&com), 1);
    assert_eq!(com["data"]["matches"][0]["line"], 1);

    let strg = f.run(&["search", "needle", "--in", "strings"]);
    assert_eq!(matches(&strg), 1);
    assert_eq!(strg["data"]["matches"][0]["line"], 2);

    let code = f.run(&["search", "needle", "--in", "code"]);
    assert_eq!(matches(&code), 1);
    assert_eq!(code["data"]["matches"][0]["line"], 3);
}

#[test]
fn search_reports_one_row_per_line_and_in_partitions_it() {
    // A line with a code hit AND a string hit: unfiltered reports the line once
    // (grep default: one row per line, at the FIRST match's column), while `--in`
    // reports the same line at the first *in-class* match's column. So `--in`
    // can change the reported column, but the set of matching lines partitions
    // exactly — this guards the accept()-before-per-line-dedup interaction.
    let f = Fixture::new();
    f.write("a.rs", "let msg = value; // value comment\n");
    let matches = |v: &serde_json::Value| v["data"]["matches"].as_array().unwrap().clone();

    let any = matches(&f.run(&["search", "value"]));
    assert_eq!(any.len(), 1, "one row per line");
    let any_col = any[0]["col"].as_u64().unwrap();

    let code = matches(&f.run(&["search", "value", "--in", "code"]));
    assert_eq!(code.len(), 1);
    assert_eq!(code[0]["line"], 1);
    assert_eq!(
        code[0]["col"].as_u64().unwrap(),
        any_col,
        "unfiltered col == first (code) hit"
    );

    let com = matches(&f.run(&["search", "value", "--in", "comments"]));
    assert_eq!(com.len(), 1);
    assert_eq!(com[0]["line"], 1);
    assert!(
        com[0]["col"].as_u64().unwrap() > any_col,
        "comment hit is later on the line"
    );
}

#[test]
fn search_in_excludes_unparseable_languages() {
    let f = Fixture::new();
    f.write("notes.md", "needle in markdown\n"); // markdown: no parser
                                                 // Without --in it's found; with --in we can't classify, so it's excluded
                                                 // (never guessed) rather than returned as code.
    assert_eq!(
        f.run(&["search", "needle", "--path", "notes.md"])["data"]["matches"]
            .as_array()
            .unwrap()
            .len(),
        1
    );
    assert_eq!(
        f.run(&["search", "needle", "--path", "notes.md", "--in", "code"])["data"]["matches"]
            .as_array()
            .unwrap()
            .len(),
        0,
        "unparseable file must be excluded from --in, not guessed"
    );
}

#[test]
fn search_regex_prefilter_never_drops_matches() {
    // The trigram prefilter must be a true superset: every one of these regexes
    // has to find its match, even when the required literal is mid-token, in an
    // alternation, or reached only via prefix/suffix.
    let f = Fixture::new();
    f.write(
        "a.rs",
        "fn reauthenticate() {}\n\
         let widget = 1;\n\
         const gadget = 2;\n\
         fn handle_needle_case() {}\n",
    );
    let count = |pat: &str| -> usize {
        f.run(&["search", pat])["data"]["matches"]
            .as_array()
            .unwrap()
            .len()
    };

    // Literal is mid-token ("authenticate" inside "reauthenticate").
    assert_eq!(count("authenticate"), 1, "mid-token literal");
    // Alternation: both literals >=3.
    assert_eq!(count("widget|gadget"), 2, "alternation");
    // Prefix literal, then wildcard.
    assert_eq!(count("reauth\\w*"), 1, "prefix literal");
    // Suffix only (no usable prefix): matches must still be found.
    assert_eq!(count(".*needle.*"), 1, "middle/suffix literal");
    // Short literal (<3): no prefilter possible -> full scan still correct.
    assert_eq!(count("ge"), 2, "sub-trigram literal falls back to scan");
    // Dotted pattern where neither prefix nor suffix reaches 3 chars.
    assert_eq!(count("w.dget"), 1, "dot pattern");
}

#[test]
fn search_inner_literal_prefilter() {
    // The only usable literal is in the middle (no prefix/suffix literal),
    // so the inner-literal walk must find it — and must not drop the match.
    let f = Fixture::new();
    f.write("a.rs", "let x = MARKER_HERE + 1;\n");
    f.write("b.rs", "let y = 2;\n"); // decoy, no marker
    let v = f.run(&["search", "=.*MARKER_HERE.*1"]);
    let m = v["data"]["matches"].as_array().unwrap();
    assert_eq!(m.len(), 1);
    assert_eq!(m[0]["path"], "a.rs");
}

#[test]
fn search_finds_mid_token_and_underscore_substrings() {
    // Regression: the trigram MATCH prefilter silently dropped substring matches
    // for many short/underscore terms (e.g. `search ode` returned nothing though
    // `decode` contains it). The LIKE prefilter must find them.
    let f = Fixture::new();
    f.write("a.rs", "fn decode() {}\nlet my_var = 1;\n");
    let count = |pat: &str| {
        f.run(&["search", pat])["data"]["matches"]
            .as_array()
            .unwrap()
            .len()
    };
    assert_eq!(count("ode"), 1, "mid-token substring of decode");
    assert_eq!(count("cod"), 1, "3-char mid-token substring");
    assert_eq!(count("y_va"), 1, "underscore substring of my_var");
}

#[test]
fn search_glob_include_and_exclude() {
    let f = Fixture::new();
    f.write("src/a.rs", "fn hit() {}\n");
    f.write("src/b.py", "def hit():\n    pass\n");
    f.write("tests/c.rs", "fn hit() {}\n");

    // Include only .rs, by basename across directories.
    let rs = f.run(&["search", "hit", "-g", "*.rs", "-l"]);
    let mut files = field_names(&rs["data"]["files"]);
    files.sort();
    assert_eq!(
        files,
        vec!["src/a.rs".to_string(), "tests/c.rs".to_string()]
    );

    // Exclude a subtree.
    let no_tests = f.run(&["search", "hit", "-g", "!tests/", "-l"]);
    let files = field_names(&no_tests["data"]["files"]);
    assert!(!files.iter().any(|p| p.starts_with("tests/")), "{files:?}");
    assert!(files.contains(&"src/a.rs".to_string()));
}

#[test]
fn search_globs_use_the_last_matching_rule() {
    let f = Fixture::new();
    f.write("a.txt", "alpha\n");
    f.write("b.txt", "alpha\n");
    f.write("nested/a.txt", "alpha\n");
    f.write("other.rs", "// alpha\n");

    let cases: &[(&[&str], &[&str])] = &[
        (&["!*.txt", "a.txt"], &["a.txt", "nested/a.txt"]),
        (&["a.txt", "!*.txt"], &[]),
        (
            &["*.txt", "!a.txt", "nested/a.txt"],
            &["b.txt", "nested/a.txt"],
        ),
        (&["!nested/", "nested/a.txt"], &["nested/a.txt"]),
        (&["nested/a.txt", "!nested/"], &[]),
        (&["!*.txt", "!other.rs"], &[]),
        (&["!b.txt"], &["a.txt", "nested/a.txt", "other.rs"]),
        (&["a.txt", "b.txt"], &["a.txt", "b.txt", "nested/a.txt"]),
    ];
    for (globs, expected) in cases {
        let mut args = vec!["search", "alpha", "-l"];
        for glob in *globs {
            args.extend(["-g", glob]);
        }
        let result = f.run(&args);
        assert_eq!(
            field_names(&result["data"]["files"]),
            *expected,
            "{globs:?}"
        );
    }
}

#[test]
fn search_globs_filter_unindexed_paths() {
    let f = Fixture::new();
    f.write(".gitignore", "logs/\n");
    f.write("src/a.py", "needle\n");
    f.write("logs/a.py", "needle\n");
    f.write("logs/a.txt", "needle\n");
    f.write("logs/deep/b.py", "needle\n");
    f.git_init_commit();

    let cases: &[(&[&str], &[&str])] = &[
        (&["*.py"], &["logs/a.py", "logs/deep/b.py"]),
        (&["!*.txt"], &["logs/a.py", "logs/deep/b.py"]),
        (&["!logs/deep/", "*.py"], &["logs/a.py", "logs/deep/b.py"]),
        (&["*.py", "!logs/deep/"], &["logs/a.py"]),
        (&["logs/a.py"], &["logs/a.py"]),
        (&["!logs/"], &[]),
    ];
    for (globs, expected) in cases {
        let mut args = vec!["search", "needle", "logs", "-l"];
        for glob in *globs {
            args.extend(["-g", glob]);
        }
        assert_eq!(
            field_names(&f.run(&args)["data"]["files"]),
            *expected,
            "{globs:?}"
        );
    }

    for target in ["logs", "./logs", f.path().join("logs").to_str().unwrap()] {
        let result = f.run(&["search", "needle", target, "-g", "logs/a.py"]);
        let matches = result["data"]["matches"].as_array().unwrap();
        assert_eq!(matches.len(), 1, "{target}");
        assert_eq!(matches[0]["path"], format!("{target}/a.py"));
    }
    assert!(
        f.run(&["search", "needle", "logs/a.txt", "-g", "!*.txt"])["data"]["matches"]
            .as_array()
            .unwrap()
            .is_empty()
    );
    assert_eq!(
        field_names(
            &f.run(&[
                "search",
                "needle",
                "src",
                "logs",
                "-l",
                "-g",
                "*.py",
                "-g",
                "!logs/deep/"
            ])["data"]["files"]
        ),
        ["src/a.py", "logs/a.py"]
    );

    let external = Fixture::new();
    external.write("a.py", "needle\n");
    external.write("a.txt", "needle\n");
    for glob in ["*.py", "!*.txt"] {
        let result = f.run(&[
            "search",
            "needle",
            external.path().to_str().unwrap(),
            "-g",
            glob,
        ]);
        let matches = result["data"]["matches"].as_array().unwrap();
        assert_eq!(matches.len(), 1, "{glob}");
        assert_eq!(
            matches[0]["path"],
            format!("{}/a.py", external.path().display())
        );
    }
}

#[test]
fn search_word_matches_whole_words_only() {
    let f = Fixture::new();
    f.write("a.rs", "let count = 1;\nlet counter = 2;\n");
    // Plain matches both lines (substring); -w only the exact word.
    assert_eq!(
        f.run(&["search", "count"])["data"]["matches"]
            .as_array()
            .unwrap()
            .len(),
        2
    );
    let w = f.run(&["search", "count", "-w"]);
    let m = w["data"]["matches"].as_array().unwrap();
    assert_eq!(m.len(), 1);
    assert_eq!(m[0]["line"], 1);
}

#[test]
fn search_anchors_are_line_oriented_like_grep() {
    // `^`/`$` must anchor to line boundaries, not the whole file buffer. The
    // scan runs over each file body, so without multiline mode `^use` would
    // only match a file that *starts* with `use` — here that's zero.
    let f = Fixture::new();
    f.write("a.rs", "mod x;\nuse crate::x;\nuse crate::y;\nfn f() {}\n");

    let start = f.run(&["search", "^use"]);
    let sm = start["data"]["matches"].as_array().unwrap();
    assert_eq!(sm.len(), 2, "^use should match both use-lines: {sm:?}");
    assert_eq!(sm[0]["line"], 2);
    assert_eq!(sm[1]["line"], 3);

    // End anchor is line-oriented too.
    let end = f.run(&["search", "x;$"]);
    let em = end["data"]["matches"].as_array().unwrap();
    assert_eq!(
        em.len(),
        2,
        "x;$ should match `mod x;` and `use crate::x;`: {em:?}"
    );
}

#[test]
fn search_strips_leading_utf8_bom() {
    // A UTF-8 BOM before the first char must not hide line 1 from `^` anchors or
    // shift offsets — the index strips it, like ripgrep and editors do.
    let f = Fixture::new();
    f.write_bytes("a.cs", b"\xEF\xBB\xBF#region Foo\nint x = 1;\n#endregion\n");

    let hits = f.run(&["search", "^#"]);
    let m = hits["data"]["matches"].as_array().unwrap();
    assert_eq!(
        m.len(),
        2,
        "both #region (line 1) and #endregion must match: {m:?}"
    );
    assert_eq!(
        m[0]["line"], 1,
        "line-1 #region must be found despite the BOM"
    );
    // The BOM must not leak into the matched line text either.
    assert!(
        m[0]["text"].as_str().unwrap().starts_with("#region"),
        "BOM leaked into text: {:?}",
        m[0]["text"]
    );
}

#[test]
fn text_is_the_default_output_json_is_opt_in() {
    let f = Fixture::new();
    f.write("a.rs", "fn alpha() {}\nfn beta() {}\n");

    // Default: compact grep-style text, no JSON envelope.
    let out = f.raw(&["symbols"]);
    assert!(out.status.success());
    let text = String::from_utf8_lossy(&out.stdout);
    assert!(
        !text.contains("\"schema\""),
        "default output must not be JSON: {text}"
    );
    assert!(
        text.contains("a.rs:1: function alpha"),
        "grep-style row missing: {text}"
    );
    assert!(
        text.contains("a.rs:2: function beta"),
        "grep-style row missing: {text}"
    );

    // Search text is `path:line: text`, like `grep -n`, plus codeq's
    // differentiator: the enclosing symbol annotated as `[name]`.
    f.write("b.rs", "fn gamma() {\n    delta();\n}\n");
    let so = f.raw(&["search", "delta"]);
    assert_eq!(
        String::from_utf8_lossy(&so.stdout),
        "b.rs:2: delta();  [gamma]\n"
    );
    // The line that declares the symbol already names it, so the tag would
    // only repeat the line.
    let so = f.raw(&["search", "beta"]);
    assert_eq!(
        String::from_utf8_lossy(&so.stdout),
        "a.rs:2: fn beta() {}\n"
    );
    // The column is still a fact: asked for, it is there, and JSON always has it.
    let so = f.raw(&["search", "delta", "--fields", "path,line,col,text"]);
    assert_eq!(String::from_utf8_lossy(&so.stdout), "b.rs:2:4: delta();\n");
    let so = f.raw(&["--json", "search", "beta"]);
    let v: Value = serde_json::from_slice(&so.stdout).unwrap();
    let hit = &v["data"]["matches"][0];
    assert_eq!(hit["col"], 3);
    assert_eq!(hit["symbol"], "beta");
    assert_eq!(hit["decl"], true);

    // --json restores the structured envelope.
    let jo = f.raw(&["--json", "symbols"]);
    let j = String::from_utf8_lossy(&jo.stdout);
    assert!(
        j.contains("\"schema\""),
        "--json must emit the envelope: {j}"
    );
}

#[test]
fn search_invalid_regex_fails() {
    let f = Fixture::new();
    f.write("a.rs", "fn f() {}\n");
    let out = f.raw(&["search", "("]);
    assert!(!out.status.success());
    assert!(String::from_utf8_lossy(&out.stderr).contains("invalid pattern"));
}

// ---------------------------------------------------------------------------
// read (folding file reader, text output)
// ---------------------------------------------------------------------------

#[test]
fn read_folds_bodies_and_expands_verbatim() {
    let f = Fixture::new();
    let body: String = (0..10).map(|i| format!("    let v{i} = {i};\n")).collect();
    f.write("a.rs", &format!("fn small() {{}}\nfn big() {{\n{body}}}\n"));

    // Default: big's body folded, small shown, signatures visible.
    let out = String::from_utf8(f.raw(&["read", "a.rs"]).stdout).unwrap();
    assert!(out.contains("fn big()"), "signature shown: {out}");
    // The marker states the hidden range in the exact syntax `--lines` accepts,
    // so it doubles as the next command rather than something to retype.
    assert!(out.contains("... 11 lines (3-13)"), "body folded: {out}");
    assert!(!out.contains("let v5"), "folded body must be hidden: {out}");
    assert!(out.contains("fn small()"), "short fn stays inline");

    // --lines: verbatim, line-numbered slice.
    let slice = String::from_utf8(f.raw(&["read", "a.rs", "--lines", "3-4"]).stdout).unwrap();
    assert!(slice.contains("let v0"), "verbatim slice: {slice}");

    // --symbol: the definition's real body.
    let sym = String::from_utf8(f.raw(&["read", "a.rs", "--symbol", "big"]).stdout).unwrap();
    assert!(sym.contains("let v5"), "symbol body verbatim: {sym}");
    assert!(
        sym.contains("2\tfn big()"),
        "line-numbered from the def start: {sym}"
    );
}

/// The `sed -n 'A,Bp'` an agent runs once per slice is one `read`: a range rides
/// on the path, several ranges on one path, and several paths in one call.
#[test]
fn read_takes_ranges_on_the_path_and_several_files() {
    let f = Fixture::new();
    let lines = |tag: &str| (1..=9).map(|i| format!("{tag}{i}\n")).collect::<String>();
    f.write("a.txt", &lines("a"));
    f.write("b.txt", &lines("b"));
    let read = |args: &[&str]| {
        let mut argv = vec!["read"];
        argv.extend(args);
        let out = f.raw(&argv);
        (
            String::from_utf8(out.stdout).unwrap(),
            String::from_utf8(out.stderr).unwrap(),
        )
    };

    // `path:A-B`, and `path:N` as a search row prints it.
    assert_eq!(read(&["a.txt:2-3"]).0, "2\ta2\n3\ta3\n");
    assert_eq!(read(&["a.txt:4"]).0, "4\ta4\n");
    // Several ranges, however they are written, with grep's `--` between.
    let two = "2\ta2\n--\n8\ta8\n9\ta9\n";
    assert_eq!(read(&["a.txt:2,8-9"]).0, two);
    assert_eq!(read(&["a.txt", "--lines", "2,8-9"]).0, two);
    assert_eq!(read(&["a.txt", "--lines", "2", "--lines", "8-9"]).0, two);
    // Several files: each named, each with its own range (or none).
    assert_eq!(
        read(&["a.txt:1", "b.txt:9"]).0,
        "# a.txt\n1\ta1\n\n# b.txt\n9\tb9\n"
    );

    // `--raw` is one exact block, so it refuses to concatenate two.
    assert_eq!(read(&["a.txt:2-3", "--raw"]).0, "a2\na3\n");
    let (out, err) = read(&["a.txt:2,8", "--raw"]);
    assert!(out.is_empty() && err.contains("one range"), "{out}{err}");
    let (out, err) = read(&["a.txt", "b.txt", "--raw"]);
    assert!(out.is_empty() && err.contains("one file"), "{out}{err}");
    // A flag range cannot say which of several files it means.
    let (out, err) = read(&["a.txt", "b.txt", "--lines", "1-2"]);
    assert!(out.is_empty() && err.contains("a.rs:10-40"), "{out}{err}");
    // A missing file fails the call rather than printing the files that exist.
    let (out, err) = read(&["a.txt:1", "nope.txt:1"]);
    assert!(out.is_empty() && err.contains("nope.txt"), "{out}{err}");
}

#[test]
fn read_at_reads_exact_enclosing_definitions_and_deduplicates_positions() {
    let f = Fixture::new();
    f.write(
        "a.py",
        "def same():\n    return 1\n\ndef same():\n    return 2\n",
    )
    .write(
        "b.py",
        "class Wrapper:\n    def inner(self):\n        return 3\n",
    );
    let out = f.raw(&["read", "--at", "a.py:5", "--raw"]);
    assert!(out.status.success(), "{:?}", out);
    assert_eq!(out.stdout, b"def same():\n    return 2\n");

    let out = f.raw(&["read", "--at", "a.py:4", "--at", "a.py:5", "--at", "b.py:3"]);
    assert!(out.status.success(), "{:?}", out);
    assert_eq!(String::from_utf8(out.stdout).unwrap(),
        "# a.py\n4\tdef same():\n5\t    return 2\n\n# b.py\n2\t    def inner(self):\n3\t        return 3\n");

    // Asking for a container and a member emits their overlapping lines once.
    let out = f.raw(&["read", "--at", "b.py:1", "--at", "b.py:3"]);
    assert!(out.status.success());
    assert_eq!(
        String::from_utf8(out.stdout).unwrap(),
        "1\tclass Wrapper:\n2\t    def inner(self):\n3\t        return 3\n"
    );
}

#[test]
fn read_at_validates_all_positions_before_emitting_source() {
    let f = Fixture::new();
    f.write("a.rs", "fn one() {}\n\n");
    for bad in ["a.rs:2", "missing.rs:1", "a.rs:nope", "a.rs"] {
        let out = f.raw(&["read", "--at", "a.rs:1", "--at", bad]);
        assert!(!out.status.success(), "{bad}");
        assert!(out.stdout.is_empty(), "partial answer for {bad}");
    }
    for conflicting in ["--full", "--symbol", "--lines"] {
        let out = f.raw(&["read", "--at", "a.rs:1", conflicting, "1"]);
        assert!(!out.status.success(), "{conflicting}");
    }
    let out = f.raw(&["read", "--at", "a.rs:1", "--at", "a.rs:1", "--raw"]);
    assert!(!out.status.success());
    assert!(out.stdout.is_empty());
}

#[test]
fn read_at_preserves_raw_bytes_and_handles_shared_declaration_lines() {
    let f = Fixture::new();
    f.write("a.rs", "fn one() {\r\n    call();\r\n}")
        .write("b.rs", "enum Color { Red, Blue }\n");
    let out = f.raw(&["read", "--at", "a.rs:2", "--raw"]);
    assert!(out.status.success());
    assert_eq!(out.stdout, b"fn one() {\r\n    call();\r\n}");
    let out = f.raw(&["read", "--at", "b.rs:1"]);
    assert!(out.status.success());
    assert_eq!(out.stdout, b"1\tenum Color { Red, Blue }\n");
}

/// grep's flags mean in `search` what they mean in grep, so the command an
/// agent already knows is not an exit-2 round trip.
#[test]
fn search_takes_greps_flags() {
    let f = Fixture::new();
    f.write(
        "a.rs",
        "fn alpha() {}\nfn beta() { alpha(); alpha2(); }\n// a.b\n",
    );
    f.write("b.py", "def gamma():\n    pass\n");
    let search = |args: &[&str]| {
        let mut argv = vec!["search"];
        argv.extend(args);
        let out = f.raw(&argv);
        assert!(
            out.status.success(),
            "{args:?}: {}",
            String::from_utf8_lossy(&out.stderr)
        );
        String::from_utf8(out.stdout).unwrap()
    };

    // -e: several patterns OR'd, every positional then a path.
    let out = search(&["-e", "gamma", "-e", "beta"]);
    assert!(out.contains("a.rs:2:") && out.contains("b.py:1:"), "{out}");
    assert_eq!(
        search(&["-e", "gamma", "-e", "beta", "a.rs"])
            .lines()
            .count(),
        1
    );
    // An alternation inside one -e stays inside it.
    assert_eq!(
        search(&["-e", "^fn alpha|^def", "-e", "pass$"])
            .lines()
            .count(),
        3
    );

    // -o: the match alone, a row per occurrence.
    assert_eq!(
        search(&["-o", "alpha\\w*", "a.rs", "--fields", "line,text"]),
        "1: alpha\n2: alpha\n2: alpha2\n"
    );

    // -v: the lines that do not match; -c counts those lines.
    assert_eq!(
        search(&["-v", "alpha", "a.rs", "--fields", "line,text"]),
        "3: // a.b\n"
    );
    assert_eq!(search(&["-v", "-c", "alpha", "a.rs"]), "a.rs: count=1\n");

    // -L: files with no match — including the ones a literal prefilter skips.
    assert_eq!(search(&["-L", "alpha"]), "b.py\n");
    assert_eq!(search(&["-l", "alpha"]), "a.rs\n");

    // -F is --fixed: `a.b` literally, not `a` any `b`.
    assert_eq!(search(&["-F", "a.b"]).lines().count(), 1);
    assert_eq!(search(&["a.b"]).lines().count(), 1);
    assert_eq!(search(&["-F", "a.a"]).lines().count(), 0);

    // -t takes ripgrep's short type names; --type is the same flag.
    assert_eq!(search(&["-t", "py", "-l", "."]), "b.py\n");
    assert_eq!(search(&["--type", "rust", "-l", "."]), "a.rs\n");

    // -U is accepted: a pattern already matches across lines.
    assert!(search(&["-U", "\\{\\}\\s+fn beta"]).starts_with("a.rs:1:"));

    // `codeq grep` is `codeq search`.
    let out = f.raw(&["grep", "-l", "gamma"]);
    assert_eq!(String::from_utf8(out.stdout).unwrap(), "b.py\n");

    // -o has no line to give context around.
    let out = f.raw(&["search", "-o", "alpha", "-C", "1"]);
    assert!(!out.status.success());
}

/// The text output contract (README "Output contract"): plain ASCII rows, no
/// decoration, no prose, one row per line. Guards against decorative drift.
#[test]
fn text_output_stays_plain_ascii_rows() {
    let f = Fixture::new();
    f.write(
        "a.rs",
        "use std::collections::{\n    HashMap,\n    HashSet,\n};\n\
         pub struct Edge {\n    pub to: String,\n    pub module: String,\n}\n\
         pub fn go() -> Edge { Edge { to: String::new(), module: String::new() } }\n",
    );

    for args in [
        vec!["map"],
        vec!["symbols", "--signatures"],
        vec!["definitions", "Edge"],
        vec!["outline", "a.rs"],
        vec!["about", "Edge"],
        vec!["api"],
        vec!["imports"],
        vec!["callers", "Edge", "--depth", "2"],
    ] {
        let out = String::from_utf8(f.raw(&args).stdout).unwrap();
        assert!(
            out.is_ascii(),
            "{args:?} must emit ASCII only (no arrows, ellipses, dashes): {out}"
        );
        for line in out.lines() {
            assert!(
                !line.trim_end().ends_with(':'),
                "{args:?} emitted a locator with an empty payload: {line:?}"
            );
        }
        // Indentation is depth, and the shallowest row shown is flush left.
        if out.lines().any(|l| !l.is_empty()) {
            assert!(
                out.lines().any(|l| !l.starts_with(' ') && !l.is_empty()),
                "{args:?} indented every row: {out}"
            );
        }
    }

    // A container's signature is its header, never its field list.
    let defs = String::from_utf8(f.raw(&["definitions", "Edge"]).stdout).unwrap();
    assert!(defs.contains("pub struct Edge"), "header shown: {defs}");
    assert!(!defs.contains("pub to:"), "body must be cut: {defs}");

    // A multi-line import fact still occupies exactly one row.
    let imports = String::from_utf8(f.raw(&["imports"]).stdout).unwrap();
    assert_eq!(imports.lines().count(), 1, "one row per line: {imports}");
    assert!(imports.contains("HashMap, HashSet"), "collapsed: {imports}");
}

// ---------------------------------------------------------------------------
// MCP server
// ---------------------------------------------------------------------------

#[test]
fn mcp_server_handshake_and_tool_call() {
    use std::io::Write;
    use std::process::{Command as PCommand, Stdio};

    let f = Fixture::new();
    f.write("a.rs", "fn target() {}\n");

    let mut child = PCommand::cargo_bin("codeq")
        .unwrap()
        .arg("mcp")
        .current_dir(f.path())
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        .spawn()
        .unwrap();

    let reqs = [
        r#"{"jsonrpc":"2.0","id":1,"method":"initialize","params":{"protocolVersion":"2025-06-18"}}"#,
        r#"{"jsonrpc":"2.0","method":"notifications/initialized"}"#,
        r#"{"jsonrpc":"2.0","id":2,"method":"tools/list"}"#,
        r#"{"jsonrpc":"2.0","id":3,"method":"tools/call","params":{"name":"codeq_definitions","arguments":{"symbol":"target"}}}"#,
    ];
    {
        let mut stdin = child.stdin.take().unwrap();
        for r in reqs {
            writeln!(stdin, "{r}").unwrap();
        }
    } // drop stdin -> EOF, server exits after processing

    let out = child.wait_with_output().unwrap();
    let msgs: Vec<Value> = String::from_utf8(out.stdout)
        .unwrap()
        .lines()
        .map(|l| serde_json::from_str(l).unwrap())
        .collect();

    // The notification produced no response: 3 requests -> 3 responses.
    assert_eq!(msgs.len(), 3, "{msgs:?}");
    assert_eq!(msgs[0]["result"]["serverInfo"]["name"], "codeq");
    assert_eq!(msgs[0]["result"]["protocolVersion"], "2025-06-18");
    assert!(msgs[1]["result"]["tools"].as_array().unwrap().len() >= 10);

    // The tool result carries codeq's compact text, not the JSON envelope: the
    // caller is a model paying per token.
    let text = msgs[2]["result"]["content"][0]["text"].as_str().unwrap();
    assert!(text.starts_with("a.rs:1:"), "{text}");
    assert!(text.contains("fn target()"), "{text}");
    assert!(
        !text.contains("\"schema\""),
        "JSON envelope returned: {text}"
    );
}

#[test]
fn mcp_answers_lines_it_cannot_use_and_survives_invalid_utf8() {
    use std::io::Write;
    use std::process::{Command as PCommand, Stdio};

    let f = Fixture::new();
    let mut child = PCommand::cargo_bin("codeq")
        .unwrap()
        .arg("mcp")
        .current_dir(f.path())
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        .spawn()
        .unwrap();
    {
        let mut stdin = child.stdin.take().unwrap();
        stdin.write_all(b"{not json\n").unwrap();
        stdin.write_all(b"\xff\xfe\n").unwrap();
        stdin
            .write_all(b"[{\"jsonrpc\":\"2.0\",\"id\":7,\"method\":\"ping\"}]\n")
            .unwrap();
        stdin.write_all(b"[]\n").unwrap();
        // Still alive after all of the above.
        stdin
            .write_all(b"{\"jsonrpc\":\"2.0\",\"id\":8,\"method\":\"ping\"}\n")
            .unwrap();
    }
    let out = child.wait_with_output().unwrap();
    let msgs: Vec<Value> = String::from_utf8(out.stdout)
        .unwrap()
        .lines()
        .map(|l| serde_json::from_str(l).unwrap())
        .collect();

    assert_eq!(msgs.len(), 5, "every line is answered: {msgs:?}");
    assert_eq!(msgs[0]["error"]["code"], -32700);
    assert_eq!(
        msgs[1]["error"]["code"], -32700,
        "bad bytes are a parse error"
    );
    assert_eq!(msgs[2][0]["id"], 7, "a batch is answered as a batch");
    assert_eq!(msgs[3]["error"]["code"], -32600);
    assert_eq!(msgs[4]["id"], 8);
}

#[test]
fn mcp_exposes_every_agent_query_command_and_its_schema_flags() {
    use std::io::Write;
    use std::process::{Command as PCommand, Stdio};

    let f = Fixture::new();
    let expected: std::collections::BTreeSet<String> =
        field_names(&f.run(&["capabilities"])["data"]["commands"])
            .into_iter()
            // These commands manage Codeq itself or start the MCP server; they are not
            // repository-query tools an MCP client should recursively expose.
            .filter(|name| {
                !matches!(
                    name.as_str(),
                    "index" | "install" | "uninstall" | "doctor" | "mcp"
                )
            })
            .map(|name| format!("codeq_{name}"))
            .collect();

    let mut child = PCommand::cargo_bin("codeq")
        .unwrap()
        .arg("mcp")
        .current_dir(f.path())
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        .spawn()
        .unwrap();
    {
        let mut stdin = child.stdin.take().unwrap();
        writeln!(stdin, r#"{{"jsonrpc":"2.0","id":1,"method":"tools/list"}}"#).unwrap();
    }
    let out = child.wait_with_output().unwrap();
    let reply: Value = serde_json::from_slice(&out.stdout).unwrap();
    let tools = reply["result"]["tools"]
        .as_array()
        .expect("tools/list response");
    let actual: std::collections::BTreeSet<String> = tools
        .iter()
        .filter_map(|tool| tool["name"].as_str().map(str::to_string))
        .collect();
    assert_eq!(actual, expected, "MCP and CLI command surfaces drifted");

    let properties = |name: &str| -> &Value {
        &tools
            .iter()
            .find(|tool| tool["name"] == name)
            .unwrap_or_else(|| panic!("missing MCP tool {name}"))["inputSchema"]["properties"]
    };
    for (tool, key) in [("codeq_definitions", "symbol"), ("codeq_about", "name")] {
        assert_eq!(properties(tool)[key]["type"], "array");
        assert_eq!(properties(tool)[key]["items"]["type"], "string");
    }
    for (tool, flag) in [
        ("codeq_context", "resolved"),
        ("codeq_api", "compare_to"),
        ("codeq_tree", "language"),
        ("codeq_symbols", "owner"),
        ("codeq_map", "totals"),
        ("codeq_clones", "include_tests"),
        ("codeq_search", "before"),
        ("codeq_search", "after"),
        ("codeq_graph", "mermaid"),
    ] {
        assert!(
            properties(tool).get(flag).is_some(),
            "{tool} does not expose its CLI {flag} flag"
        );
    }
}

/// An MCP caller has no way to say "this value is a literal, not a flag" — so
/// the server must say it, by putting `--` ahead of every caller-supplied
/// positional. Without it, searching a codebase for `--json` (an entirely
/// ordinary thing to look for) failed as "required argument not provided", and
/// `symbol: "--help"` printed codeq's help page and exited 0, handing an agent
/// the help text as a successful answer.
#[test]
fn mcp_treats_flag_like_arguments_as_literals() {
    use std::io::Write;
    use std::process::{Command as PCommand, Stdio};

    let f = Fixture::new();
    f.write("a.rs", "// mentions --json in a comment\nfn target() {}\n");

    let mut child = PCommand::cargo_bin("codeq")
        .unwrap()
        .arg("mcp")
        .current_dir(f.path())
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        .spawn()
        .unwrap();
    let reqs = [
        r#"{"jsonrpc":"2.0","id":1,"method":"initialize","params":{}}"#,
        r#"{"jsonrpc":"2.0","id":2,"method":"tools/call","params":{"name":"codeq_search","arguments":{"pattern":"--json"}}}"#,
        r#"{"jsonrpc":"2.0","id":3,"method":"tools/call","params":{"name":"codeq_definitions","arguments":{"symbol":"--help"}}}"#,
        // A flag's *value* must stay attached to its flag, ahead of the `--`.
        r#"{"jsonrpc":"2.0","id":4,"method":"tools/call","params":{"name":"codeq_astgrep","arguments":{"pattern":"fn $N() {}","language":"rust"}}}"#,
    ];
    {
        let mut stdin = child.stdin.take().unwrap();
        for r in reqs {
            writeln!(stdin, "{r}").unwrap();
        }
    }
    let out = child.wait_with_output().unwrap();
    let mut msgs: Vec<Value> = String::from_utf8(out.stdout)
        .unwrap()
        .lines()
        .map(|l| serde_json::from_str(l).unwrap())
        .collect();
    msgs.sort_by_key(|message| message["id"].as_u64());
    assert_eq!(msgs.len(), 4, "{msgs:?}");

    // `--json` is searched for literally, and finds the comment.
    let r = &msgs[1]["result"];
    assert!(
        r["isError"].as_bool() != Some(true),
        "search treated --json as a flag: {r}"
    );
    let text = r["content"][0]["text"].as_str().unwrap();
    assert_eq!(
        text.lines().filter(|l| l.starts_with("a.rs:1:")).count(),
        1,
        "{text}"
    );

    // `--help` is a symbol name with no definition, not a request for help.
    let r = &msgs[2]["result"];
    let text = r["content"][0]["text"].as_str().unwrap();
    assert!(
        !text.contains("Usage:"),
        "help text returned as a result: {text}"
    );
    assert!(
        !text.contains("a.rs:"),
        "a row for a name with no definition: {text}"
    );
    // An empty result's explanation is on stderr in the CLI; over MCP it has to
    // arrive in the one channel there is.
    assert!(text.contains("no symbol named '--help'"), "{text}");

    // astgrep still works: its `language` is a flag value, not a positional.
    let r = &msgs[3]["result"];
    assert!(
        r["isError"].as_bool() != Some(true),
        "astgrep language broke: {r}"
    );
}

/// Guards CLI↔MCP flag parity: `codeq_context` must forward the `path` and
/// `no_text` arguments to the CLI (they were once absent from the MCP arm). The
/// `no_text` check is the real regression catch — if the flag isn't forwarded,
/// chunks still carry their source `text`.
#[test]
fn mcp_context_forwards_path_and_no_text_flags() {
    use std::io::Write;
    use std::process::{Command as PCommand, Stdio};

    let f = Fixture::new();
    f.write(
        "a.rs",
        "fn target() {\n    let marker_body = 1;\n}\nfn caller() { target(); }\n",
    );

    let mut child = PCommand::cargo_bin("codeq")
        .unwrap()
        .arg("mcp")
        .current_dir(f.path())
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        .spawn()
        .unwrap();

    let reqs = [
        r#"{"jsonrpc":"2.0","id":1,"method":"initialize","params":{"protocolVersion":"2025-06-18"}}"#,
        r#"{"jsonrpc":"2.0","method":"notifications/initialized"}"#,
        r#"{"jsonrpc":"2.0","id":2,"method":"tools/call","params":{"name":"codeq_context","arguments":{"symbols":["target"],"path":"a.rs","no_text":true}}}"#,
    ];
    {
        let mut stdin = child.stdin.take().unwrap();
        for r in reqs {
            writeln!(stdin, "{r}").unwrap();
        }
    }

    let out = child.wait_with_output().unwrap();
    let msgs: Vec<Value> = String::from_utf8(out.stdout)
        .unwrap()
        .lines()
        .map(|l| serde_json::from_str(l).unwrap())
        .collect();

    let call = msgs
        .iter()
        .find(|m| m["id"] == 2)
        .expect("tool result for id 2");
    assert!(
        call["error"].is_null(),
        "unexpected JSON-RPC error: {call:?}"
    );
    assert_ne!(
        call["result"]["isError"].as_bool(),
        Some(true),
        "tool errored: {call:?}"
    );
    let text = call["result"]["content"][0]["text"].as_str().unwrap();
    // --path a.rs kept the seed; --no-text dropped its source text.
    assert!(
        text.contains("a.rs:1: function target"),
        "seed missing: {text}"
    );
    assert!(
        !text.contains("marker_body"),
        "no_text was not forwarded — source text leaked: {text}"
    );
}

#[test]
fn mcp_context_forwards_resolved_expansion() {
    use std::io::Write;
    use std::process::{Command as PCommand, Stdio};

    let f = Fixture::new();
    f.write("seed.rs", "mod helper;\nfn seed() { build(); }\n");
    f.write("helper.rs", "pub fn build() {}\n");
    f.write("other.rs", "pub fn build() {}\n");

    let mut child = PCommand::cargo_bin("codeq")
        .unwrap()
        .arg("mcp")
        .current_dir(f.path())
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        .spawn()
        .unwrap();
    {
        let mut stdin = child.stdin.take().unwrap();
        writeln!(stdin, r#"{{"jsonrpc":"2.0","id":1,"method":"tools/call","params":{{"name":"codeq_context","arguments":{{"symbols":["seed"],"expand":true,"resolved":true,"no_text":true}}}}}}"#).unwrap();
    }
    let out = child.wait_with_output().unwrap();
    let reply: Value = serde_json::from_slice(&out.stdout).unwrap();
    let text = reply["result"]["content"][0]["text"]
        .as_str()
        .expect("tool text");
    assert!(
        text.contains("helper.rs:"),
        "reachable definition missing: {text}"
    );
    assert!(
        !text.contains("other.rs:"),
        "resolved was not forwarded: {text}"
    );
}

#[test]
fn mcp_refreshes_cached_session_between_tool_calls() {
    use std::io::Write;
    use std::process::{Command as PCommand, Stdio};

    let f = Fixture::new();
    f.write("src.rs", "pub fn before() {}\n");
    let mut child = PCommand::cargo_bin("codeq")
        .unwrap()
        .arg("mcp")
        .current_dir(f.path())
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        .spawn()
        .unwrap();
    let mut input = child.stdin.take().unwrap();
    let mut output = std::io::BufReader::new(child.stdout.take().unwrap());
    writeln!(input, r#"{{"jsonrpc":"2.0","id":1,"method":"tools/call","params":{{"name":"codeq_search","arguments":{{"pattern":"before"}}}}}}"#).unwrap();
    let mut line = String::new();
    std::io::BufRead::read_line(&mut output, &mut line).unwrap();
    assert!(line.contains("before"), "first response: {line}");

    f.write("src.rs", "pub fn after() {}\n");
    writeln!(input, r#"{{"jsonrpc":"2.0","id":2,"method":"tools/call","params":{{"name":"codeq_search","arguments":{{"pattern":"after"}}}}}}"#).unwrap();
    line.clear();
    std::io::BufRead::read_line(&mut output, &mut line).unwrap();
    assert!(line.contains("after"), "updated response: {line}");
    assert!(!line.contains("before"), "stale response: {line}");
    drop(input);
    child.wait().unwrap();
}

/// One MCP tool call against a fixture: `(is_error, text)`.
fn mcp_call(f: &Fixture, tool: &str, arguments: Value) -> (bool, String) {
    use std::io::Write;
    use std::process::{Command as PCommand, Stdio};

    let mut child = PCommand::cargo_bin("codeq")
        .unwrap()
        .arg("mcp")
        .current_dir(f.path())
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        .spawn()
        .unwrap();
    let req = serde_json::json!({
        "jsonrpc": "2.0", "id": 1, "method": "tools/call",
        "params": { "name": tool, "arguments": arguments }
    });
    writeln!(child.stdin.take().unwrap(), "{req}").unwrap();
    let out = child.wait_with_output().unwrap();
    let reply: Value = serde_json::from_slice(&out.stdout).unwrap();
    let result = &reply["result"];
    (
        result["isError"].as_bool() == Some(true),
        result["content"][0]["text"]
            .as_str()
            .expect("tool text")
            .to_string(),
    )
}

/// An argument the tool does not declare is an error, not a no-op. `paths` on a
/// tool without it used to be dropped, so a scoped question was answered with a
/// repo-wide search and nothing said the scope had not applied.
#[test]
fn mcp_rejects_arguments_a_tool_does_not_declare() {
    let f = Fixture::new();
    f.write("a.rs", "fn target() {}\n");
    f.write("b.rs", "fn other() { target(); }\n");

    let (err, text) = mcp_call(
        &f,
        "codeq_search",
        serde_json::json!({ "pattern": "target", "pathz": ["a.rs"] }),
    );
    assert!(err, "unknown argument accepted: {text}");
    assert!(text.contains("pathz") && text.contains("paths"), "{text}");

    let (err, text) = mcp_call(
        &f,
        "codeq_search",
        serde_json::json!({ "pattern": "target", "limit": true }),
    );
    assert!(err && text.contains("limit must be an integer"), "{text}");

    let (err, text) = mcp_call(&f, "codeq_search", serde_json::json!({}));
    assert!(
        err && text.contains("missing required argument: pattern"),
        "{text}"
    );

    // A number and its string spelling are the same request.
    for limit in [serde_json::json!(1), serde_json::json!("1")] {
        let (err, text) = mcp_call(
            &f,
            "codeq_search",
            serde_json::json!({ "pattern": "target", "limit": limit }),
        );
        assert!(!err, "{text}");
        assert_eq!(
            text.lines().filter(|l| !l.starts_with('#')).count(),
            1,
            "{text}"
        );
        // The truncation note is on stderr in the CLI; MCP has one channel.
        assert!(text.contains("stopped at --limit"), "{text}");
    }
}

/// The scope and the token-saving output flags are reachable over MCP.
#[test]
fn mcp_search_takes_paths_and_row_flags() {
    let f = Fixture::new();
    f.write(
        "a.rs",
        "fn target() {}\nfn again() { target(); target2(); }\n",
    );
    f.write("b.rs", "fn other() { target(); }\n");

    let (err, text) = mcp_call(
        &f,
        "codeq_search",
        serde_json::json!({ "pattern": "target", "paths": ["a.rs"] }),
    );
    assert!(
        !err && text.contains("a.rs:1:") && !text.contains("b.rs"),
        "{text}"
    );

    let (err, text) = mcp_call(
        &f,
        "codeq_search",
        serde_json::json!({ "pattern": "target", "group": true, "max_per_file": 1 }),
    );
    assert!(!err, "{text}");
    assert!(text.contains("a.rs (1 of 2)"), "grouped and capped: {text}");

    let (_, text) = mcp_call(
        &f,
        "codeq_search",
        serde_json::json!({ "pattern": "zzz_absent" }),
    );
    assert_eq!(text, "(no results)");
}

/// The schema is built from clap's argument types, and clap only keeps a type's
/// *name* in debug builds — so this is checked on type ids. In the release
/// binary every integer was advertised as a string.
#[test]
fn mcp_schema_types_integers() {
    use std::io::Write;
    use std::process::{Command as PCommand, Stdio};

    let f = Fixture::new();
    let mut child = PCommand::cargo_bin("codeq")
        .unwrap()
        .arg("mcp")
        .current_dir(f.path())
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        .spawn()
        .unwrap();
    writeln!(
        child.stdin.take().unwrap(),
        r#"{{"jsonrpc":"2.0","id":1,"method":"tools/list"}}"#
    )
    .unwrap();
    let out = child.wait_with_output().unwrap();
    let reply: Value = serde_json::from_slice(&out.stdout).unwrap();
    let search = reply["result"]["tools"]
        .as_array()
        .unwrap()
        .iter()
        .find(|t| t["name"] == "codeq_search")
        .unwrap();
    let props = &search["inputSchema"]["properties"];
    assert_eq!(props["limit"]["type"], "integer");
    assert_eq!(props["max_per_file"]["type"], "integer");
    assert_eq!(props["paths"]["type"], "array");
}

/// A positional path the index does not hold is searched from disk rather than
/// answered with a silent nothing, which read as "no match" for a pattern that
/// had never been run.
#[test]
fn search_reads_a_named_path_the_index_does_not_hold() {
    let f = Fixture::new();
    f.write(".codeqignore", "build/\n");
    f.write("src/a.rs", "fn target() {}\n");
    f.write("build/out.log", "first\nerror: target missing\n");
    f.write("build/deep/more.log", "target again\n");

    // Ignored, so not indexed and not in an unscoped search.
    let all = String::from_utf8(f.raw(&["search", "target"]).stdout).unwrap();
    assert!(!all.contains("build/"), "{all}");

    let out = f.raw(&["search", "target", "build/out.log"]);
    let (text, note) = (
        String::from_utf8(out.stdout).unwrap(),
        String::from_utf8(out.stderr).unwrap(),
    );
    assert_eq!(text, "build/out.log:2: error: target missing\n");
    assert!(note.contains("'build/out.log' not in the index"), "{note}");

    // A directory is walked, and composes with an indexed scope.
    let out = f.raw(&["search", "-l", "target", "build", "src"]);
    let text = String::from_utf8(out.stdout).unwrap();
    assert_eq!(text, "src/a.rs\nbuild/deep/more.log\nbuild/out.log\n");

    // Nothing was parsed, so `--in` cannot classify the file: said, not guessed.
    let out = f.raw(&["search", "target", "build/out.log", "--in", "code"]);
    assert!(out.stdout.is_empty());
    assert!(String::from_utf8(out.stderr)
        .unwrap()
        .contains("not searched"));

    // A path that is nowhere is a dead filter, and is named as one.
    let out = f.raw(&["search", "target", "nowhere"]);
    assert!(out.stdout.is_empty());
    let note = String::from_utf8(out.stderr).unwrap();
    assert!(
        note.contains("'nowhere' matches no indexed file and no path on disk"),
        "{note}"
    );

    // A path inside the repo keeps its symbols however it is spelled.
    let abs = f.path().join("src/a.rs");
    for spelled in ["./src/a.rs", abs.to_str().unwrap()] {
        let text = String::from_utf8(f.raw(&["search", "target", spelled]).stdout).unwrap();
        assert_eq!(text, "src/a.rs:1: fn target() {}\n", "{spelled}");
    }
}

// ---------------------------------------------------------------------------
// Map (factual aggregation, no opinion)
// ---------------------------------------------------------------------------

#[test]
fn map_is_factual_and_reproducible() {
    let f = Fixture::new();
    f.write(
        "src/a.rs",
        "use crate::db::Store;\nfn one() {}\nfn two() {}\n",
    );
    f.write("src/b.rs", "use crate::db::Store;\nstruct S;\n");
    f.write("Cargo.toml", "[package]\nname = \"x\"\n");
    f.write("README.md", "# x\n");
    f.run(&["index"]);

    let v = f.run(&["map"]);
    let d = &v["data"];

    assert_eq!(d["totals"]["files"], 4);

    let manifests = field_names(&d["manifests"]);
    assert!(manifests.contains(&"Cargo.toml".to_string()));
    assert!(manifests.contains(&"README.md".to_string()));

    // Import counts are plain facts, and the count is in the row.
    let store = d["imports_by_module"]
        .as_array()
        .unwrap()
        .iter()
        .find(|m| m["module"] == "crate::db::Store")
        .unwrap();
    assert_eq!(store["importers"], 2);

    // definitions_by_file is ordered by its visible count; a.rs (2) before b.rs (1).
    let df = d["definitions_by_file"].as_array().unwrap();
    let rust: Vec<_> = df
        .iter()
        .filter(|r| r["path"].as_str().unwrap().ends_with(".rs"))
        .collect();
    assert_eq!(rust[0]["path"], "src/a.rs");
    assert_eq!(rust[0]["definitions"], 2);

    // Same inputs, identical output — no hidden state, no ranking drift.
    let v2 = f.run(&["map"]);
    assert_eq!(v, v2, "map must be byte-for-byte reproducible");

    // Default (text) output must render the per-row metric counts, not drop them
    // into empty payloads (the generic renderer's key=value fallback).
    let text = String::from_utf8_lossy(&f.raw(&["map"]).stdout).into_owned();
    assert!(
        text.contains("src/a.rs: ") && text.contains("definitions=2"),
        "map text output lost its counts: {text}"
    );
    assert!(
        text.contains("importers=2"),
        "import counts missing from map text: {text}"
    );
}

// ---------------------------------------------------------------------------
// Context assembly
// ---------------------------------------------------------------------------

#[test]
fn context_requires_explicit_seeds() {
    let f = Fixture::new();
    f.write("m.py", "def solo():\n    return 42\n");
    let out = f.raw(&["context"]);
    assert!(!out.status.success(), "context with no seeds should fail");
    assert!(String::from_utf8_lossy(&out.stderr).contains("seeds"));
}

#[test]
fn context_symbol_seed_returns_chunk_with_text_and_range() {
    let f = Fixture::new();
    f.write("m.py", "def solo():\n    return 42\n");
    let v = f.run(&["context", "--symbols", "solo"]);
    let d = &v["data"];
    let chunks = d["chunks"].as_array().unwrap();
    assert_eq!(chunks.len(), 1, "only the seed, no references: {chunks:?}");
    let c = &chunks[0];
    assert_eq!(c["name"], "solo");
    assert_eq!(c["reason"], "seed");
    assert_eq!(c["line"], 1);
    assert!(c["text"].as_str().unwrap().contains("return 42"));
    assert!(d["budget"]["used_tokens"].as_u64().unwrap() > 0);
    assert_eq!(d["budget"]["tokenizer"], "approx:4-bytes-per-token");
}

#[test]
fn context_expands_to_referenced_definitions() {
    let f = Fixture::new();
    f.write(
        "m.py",
        "def helper():\n    return 1\n\ndef caller():\n    return helper()\n",
    );
    // Expansion is opt-in.
    let v = f.run(&["context", "--symbols", "caller", "--expand", "--no-text"]);
    let chunks = v["data"]["chunks"].as_array().unwrap();
    let seed = chunks.iter().find(|c| c["name"] == "caller").unwrap();
    let neigh = chunks.iter().find(|c| c["name"] == "helper").unwrap();
    assert_eq!(seed["reason"], "seed");
    assert_eq!(neigh["reason"], "referenced");
    // --no-text means provenance only.
    assert!(seed.get("text").is_none());
}

#[test]
fn context_expand_prioritizes_syntax_calls_over_lexical_mentions() {
    let f = Fixture::new();
    f.write(
        "a.rs",
        "fn seed() { z_called(); let a_mentioned = 1; }\n\
         fn z_called() { let payload = \"xxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxx\"; }\n\
         fn a_mentioned() { let payload = \"yyyyyyyyyyyyyyyyyyyyyyyyyyyyyyyyyyyyyyyy\"; }\n",
    );

    // Both neighbours fit individually, but not together. The direct call must
    // take the expansion slot ahead of the lexical-only mention, even though it
    // sorts later by name.
    let v = f.run(&[
        "context",
        "--symbols",
        "seed",
        "--expand",
        "--max-tokens",
        "45",
    ]);
    let names = chunk_symbols(&v["data"]);
    assert!(
        names.contains(&"z_called".to_string()),
        "called neighbour missing: {v}"
    );
    assert!(
        !names.contains(&"a_mentioned".to_string()),
        "lexical neighbour outranked call: {v}"
    );
}

#[test]
fn context_expand_uses_a_module_qualified_call_to_rank_the_right_definition() {
    let f = Fixture::new();
    f.write("context.rs", "pub fn build() { let data = \"right\"; }\n");
    f.write("other.rs", "pub fn build() { let data = \"wrong\"; }\n");
    f.write(
        "app.rs",
        "use crate::context;\nfn seed() { context::build(); }\n",
    );

    let v = f.run(&["context", "--symbols", "seed", "--expand", "--no-text"]);
    let paths: Vec<&str> = v["data"]["chunks"]
        .as_array()
        .unwrap()
        .iter()
        .filter(|chunk| chunk["name"] == "build")
        .filter_map(|chunk| chunk["path"].as_str())
        .collect();
    assert_eq!(
        paths,
        ["context.rs"],
        "qualified expansion was not narrowed: {v}"
    );
}

#[test]
fn context_default_returns_only_the_seeds() {
    // No expansion, no surprises: the default is exactly what was asked for.
    let f = Fixture::new();
    f.write(
        "m.py",
        "def helper():\n    return 1\n\ndef caller():\n    return helper()\n",
    );
    let v = f.run(&["context", "--symbols", "caller", "--no-text"]);
    let names = chunk_symbols(&v["data"]);
    assert_eq!(names, vec!["caller".to_string()], "default must not expand");
}

#[test]
fn context_respects_token_budget() {
    let f = Fixture::new();
    f.write("m.py", "def aa():\n    pass\n\ndef bb():\n    return 123\n");

    // Ample budget: both seeds fit, used == sum of est, nothing omitted.
    // (Default is no expansion, so only the two seeds are candidates.)
    let v = f.run(&["context", "--symbols", "aa,bb", "--no-text"]);
    let d = &v["data"];
    let chunks = d["chunks"].as_array().unwrap();
    let sum: u64 = chunks
        .iter()
        .map(|c| c["est_tokens"].as_u64().unwrap())
        .sum();
    let used = d["budget"]["used_tokens"].as_u64().unwrap();
    assert_eq!(sum, used);
    assert!(used <= 12000);
    // omitted is skipped when empty.
    assert!(d
        .get("omitted")
        .is_none_or(|o| o.as_array().unwrap().is_empty()));

    // Starve the budget: the explicitly-requested seeds are still returned — you
    // asked for them, and dropping the very symbol you named is worse than
    // overshooting. The budget caps automatic --expand growth, not the seeds.
    let v = f.run(&[
        "context",
        "--symbols",
        "aa,bb",
        "--no-text",
        "--max-tokens",
        "1",
    ]);
    let names = chunk_symbols(&v["data"]);
    assert!(
        names.contains(&"aa".to_string()) && names.contains(&"bb".to_string()),
        "seeds dropped by a starved budget: {names:?}"
    );
}

#[test]
fn context_no_text_preserves_seed_reduction_and_budget() {
    let f = Fixture::new();
    let source = format!(
        "\u{feff}class Seed:\n    def work(self):\n{}        return 1\n",
        "        # café 🐍\n".repeat(128)
    );
    f.write("seed.py", &source);
    let full = f.run(&["context", "--symbols", "Seed", "--max-tokens", "80"]);
    let lean = f.run(&[
        "context",
        "--symbols",
        "Seed",
        "--max-tokens",
        "80",
        "--no-text",
    ]);
    assert_eq!(lean["data"]["chunks"][0]["name"], "Seed");
    assert_eq!(lean["data"]["chunks"][0]["reduced"], "folded");
    assert!(lean["data"]["chunks"][0].get("text").is_none());
    assert!(lean["data"]["budget"]["used_tokens"].as_u64().unwrap() <= 80);
    let mut expected = full["data"].clone();
    expected["chunks"][0]
        .as_object_mut()
        .unwrap()
        .remove("text");
    assert_eq!(lean["data"], expected);
}

#[test]
fn context_marks_omissions_truncated_only_when_one_is_hidden() {
    let f = Fixture::new();
    let source = |count: usize| {
        let calls = (0..count)
            .map(|i| format!("n{i}();"))
            .collect::<Vec<_>>()
            .join(" ");
        let defs = (0..count)
            .map(|i| format!("fn n{i}() {{ let _ = {i}; }}\n"))
            .collect::<String>();
        format!("fn seed() {{ {calls} }}\n{defs}")
    };

    // The seed is force-included (and reduced); every referenced neighbour is
    // over budget. Exactly filling the visible omission list is not truncation.
    f.write("m.rs", &source(50));
    let complete = f.run(&[
        "context",
        "--symbols",
        "seed",
        "--expand",
        "--no-text",
        "--max-tokens",
        "1",
    ]);
    assert_eq!(
        complete["data"]["omitted"].as_array().unwrap().len(),
        50,
        "{complete}"
    );
    assert!(
        complete["data"].get("omitted_truncated").is_none(),
        "a full report was mislabelled as truncated: {complete}"
    );

    f.write("m.rs", &source(51));
    let truncated = f.run(&[
        "context",
        "--symbols",
        "seed",
        "--expand",
        "--no-text",
        "--max-tokens",
        "1",
    ]);
    assert_eq!(
        truncated["data"]["omitted"].as_array().unwrap().len(),
        50,
        "{truncated}"
    );
    assert_eq!(truncated["data"]["omitted_truncated"], true, "{truncated}");
}

#[test]
fn context_changed_mode_seeds_from_git() {
    let f = Fixture::new();
    f.write("a.py", "def old():\n    pass\n");
    f.git_init_commit();
    f.write("a.py", "def old():\n    pass\n\ndef fresh():\n    pass\n");

    // --plan opts into the explanatory plan (omitted by default to stay lean).
    let v = f.run(&[
        "context",
        "--changed",
        "--compare-to",
        "HEAD",
        "--no-text",
        "--plan",
    ]);
    let d = &v["data"];
    assert!(d["plan"]["modes"]
        .as_array()
        .unwrap()
        .iter()
        .any(|m| m == "changed"));
    let names = chunk_symbols(d);
    assert!(
        names.contains(&"fresh".to_string()),
        "changed seed missing: {names:?}"
    );
    // Seed granularity is the touched *hunk*, not the changed *file*: `old()` is
    // in the same file but untouched, so it must not be seeded.
    assert!(
        !names.contains(&"old".to_string()),
        "untouched def leaked as seed: {names:?}"
    );

    // Default (no --plan) omits the plan block.
    let lean = f.run(&["context", "--changed", "--compare-to", "HEAD", "--no-text"]);
    assert!(lean["data"].get("plan").is_none(), "plan should be opt-in");
}

#[test]
fn freshness_reflects_tracked_edits() {
    // The git fast-path must not report stale results after a tracked-file edit.
    let f = Fixture::new();
    f.write("a.rs", "fn original() {}\n");
    f.git_init_commit();
    assert!(field_set(&f.run(&["symbols"]), "name").contains(&"original".to_string()));

    f.write("a.rs", "fn renamed() {}\n"); // working-tree edit; git status sees it
    let names = field_set(&f.run(&["symbols"]), "name");
    assert!(
        names.contains(&"renamed".to_string()),
        "edit not picked up: {names:?}"
    );
    assert!(!names.contains(&"original".to_string()));
}

#[test]
fn freshness_reflects_untracked_edits() {
    // In-place edits to an untracked file must also invalidate the fast-path
    // (the token folds in untracked-file stats).
    let f = Fixture::new();
    f.write("tracked.rs", "fn t() {}\n");
    f.git_init_commit();

    f.write("note.rs", "fn first() {}\n"); // untracked
    assert!(field_set(&f.run(&["symbols"]), "name").contains(&"first".to_string()));

    f.write("note.rs", "fn second() {}\n"); // edit in place
    let names = field_set(&f.run(&["symbols"]), "name");
    assert!(
        names.contains(&"second".to_string()),
        "untracked edit missed: {names:?}"
    );
    assert!(!names.contains(&"first".to_string()));
}

#[test]
fn rebuild_repopulates_the_index() {
    // `index --rebuild` wipes derived tables; it must also clear the dir cache,
    // or the next refresh sees empty files + unchanged dirs, calls it "fresh",
    // and leaves the index empty.
    let f = Fixture::new();
    f.write("a.rs", "fn alpha() {}\nfn beta() {}\n");
    f.git_init_commit();
    assert_eq!(field_set(&f.run(&["symbols"]), "name").len(), 2);

    f.run(&["index", "--rebuild"]);
    let names = field_set(&f.run(&["symbols"]), "name");
    assert!(
        names.contains(&"alpha".to_string()) && names.contains(&"beta".to_string()),
        "rebuild left an empty index: {names:?}"
    );
}

#[test]
fn freshness_reflects_gitignore_edits() {
    // Editing .gitignore changes which files are indexed. Ignore files are hidden
    // (never walked, never in the manifest) and an in-place edit bumps no
    // directory mtime, so the git-free cache tracks their mtimes explicitly —
    // without that, an ignore-rule change is missed until some unrelated change.
    let f = Fixture::new();
    f.write("keep.rs", "fn keep() {}\n");
    f.write("gen/out.rs", "fn generated() {}\n");
    f.write(".gitignore", "\n");
    f.git_init_commit();
    assert!(field_set(&f.run(&["symbols"]), "name").contains(&"generated".to_string()));

    f.write(".gitignore", "gen/\n"); // now ignore the gen/ directory
    let names = field_set(&f.run(&["symbols"]), "name");
    assert!(
        !names.contains(&"generated".to_string()),
        "gitignored file still indexed: {names:?}"
    );
    assert!(names.contains(&"keep".to_string()));

    f.write(".gitignore", "\n"); // un-ignore
    assert!(
        field_set(&f.run(&["symbols"]), "name").contains(&"generated".to_string()),
        "un-ignored file not re-indexed"
    );
}

#[test]
fn freshness_reflects_git_exclude_changes() {
    let f = Fixture::new();
    f.write("keep.rs", "fn keep() {}\n");
    f.write("generated.rs", "fn generated() {}\n");
    f.git_init_commit();
    f.remove(".git/info/exclude");
    assert_eq!(
        field_set(&f.run(&["symbols"]), "name"),
        ["generated", "keep"]
    );

    for _ in 0..2 {
        f.write(".git/info/exclude", "generated.rs\n");
        assert_eq!(field_set(&f.run(&["symbols"]), "name"), ["keep"]);
        f.write(".git/info/exclude", "\n");
        assert_eq!(
            field_set(&f.run(&["symbols"]), "name"),
            ["generated", "keep"]
        );
    }
    f.write(".git/info/exclude", "generated.rs\n");
    assert_eq!(field_set(&f.run(&["symbols"]), "name"), ["keep"]);
    f.remove(".git/info/exclude");
    assert_eq!(
        field_set(&f.run(&["symbols"]), "name"),
        ["generated", "keep"]
    );
}

#[test]
fn freshness_reflects_linked_worktree_excludes() {
    let f = Fixture::new();
    f.write("keep.rs", "fn keep() {}\n");
    f.write("generated.rs", "fn generated() {}\n");
    f.git_init_commit();
    let linked = Fixture::new();
    f.git(&[
        "worktree",
        "add",
        "--detach",
        linked.path().to_str().unwrap(),
    ]);
    assert_eq!(
        field_set(&linked.run(&["symbols"]), "name"),
        ["generated", "keep"]
    );
    f.write(".git/info/exclude", "generated.rs\n");
    assert_eq!(field_set(&linked.run(&["symbols"]), "name"), ["keep"]);
    f.write(".git/info/exclude", "\n");
    assert_eq!(
        field_set(&linked.run(&["symbols"]), "name"),
        ["generated", "keep"]
    );
}

#[test]
fn freshness_detects_added_file() {
    // The git-free freshness cache detects a new file via its parent directory's
    // bumped mtime — no manual reindex, no git.
    let f = Fixture::new();
    f.write("a.rs", "fn first() {}\n");
    f.git_init_commit();
    assert!(field_set(&f.run(&["symbols"]), "name").contains(&"first".to_string()));

    f.write("b.rs", "fn added() {}\n"); // new file bumps the dir mtime
    let names = field_set(&f.run(&["symbols"]), "name");
    assert!(
        names.contains(&"added".to_string()),
        "added file not indexed: {names:?}"
    );
    assert!(names.contains(&"first".to_string()));
}

#[test]
fn freshness_detects_deleted_file() {
    let f = Fixture::new();
    f.write("keep.rs", "fn keep() {}\n");
    f.write("gone.rs", "fn vanishing() {}\n");
    f.git_init_commit();
    assert!(field_set(&f.run(&["symbols"]), "name").contains(&"vanishing".to_string()));

    f.remove("gone.rs"); // deletion also bumps the parent dir mtime
    let names = field_set(&f.run(&["symbols"]), "name");
    assert!(
        !names.contains(&"vanishing".to_string()),
        "deleted symbol still present: {names:?}"
    );
    assert!(names.contains(&"keep".to_string()));
}

#[test]
fn freshness_detects_added_file_in_new_subdir() {
    // A file in a brand-new subdirectory: the *root* dir's mtime bumps when the
    // subdir is created, so the change is caught even though the subdir wasn't
    // in the cache.
    let f = Fixture::new();
    f.write("a.rs", "fn root_fn() {}\n");
    f.git_init_commit();
    assert!(field_set(&f.run(&["symbols"]), "name").contains(&"root_fn".to_string()));

    f.write("sub/deep.rs", "fn deep_fn() {}\n"); // new subdir + file
    let names = field_set(&f.run(&["symbols"]), "name");
    assert!(
        names.contains(&"deep_fn".to_string()),
        "file in new subdir missed: {names:?}"
    );
}

#[test]
fn changed_without_git_fails_with_nonzero_exit() {
    let f = Fixture::new();
    f.write("a.py", "def x():\n    pass\n");
    let out = f.raw(&["changed", "--compare-to", "main"]);
    assert!(!out.status.success(), "expected non-zero exit");
    let stderr = String::from_utf8_lossy(&out.stderr);
    assert!(
        stderr.contains("Git"),
        "stderr should mention Git: {stderr}"
    );
}

// ---------------------------------------------------------------------------
// Cross-command invariants (fast, deterministic versions of the properties the
// differential harness checks across the corpus), extraction depth, and edges.
// ---------------------------------------------------------------------------

#[test]
fn definition_site_appears_in_its_references() {
    // A symbol's own definition line must be among its references.
    let f = Fixture::new();
    f.write("a.rs", "fn helper() {}\nfn caller() { helper(); }\n");
    let d0 = f.run(&["definitions", "helper"])["data"][0].clone();
    let dp = d0["path"].as_str().unwrap().to_string();
    let dl = d0["line"].as_u64().unwrap();
    let refs = f.run(&["references", "helper", "--limit", "0"]);
    let present = refs["data"]["references"]
        .as_array()
        .unwrap()
        .iter()
        .any(|r| r["path"] == dp.as_str() && r["line"].as_u64() == Some(dl));
    assert!(present, "definition site {dp}:{dl} missing from references");
}

#[test]
fn at_roundtrips_a_definition_and_encloses_its_body() {
    let f = Fixture::new();
    f.write("a.rs", "fn outer() {\n    let x = 1;\n    let y = 2;\n}\n");
    let d0 = f.run(&["definitions", "outer"])["data"][0].clone();
    let p = d0["path"].as_str().unwrap().to_string();
    let (s, e) = (
        d0["line"].as_u64().unwrap(),
        d0["end_line"].as_u64().unwrap(),
    );

    let at_def = f.run(&["at", &format!("{p}:{s}")]);
    assert_eq!(
        at_def["data"][0]["name"], "outer",
        "at the def line must resolve to it"
    );

    let mid = (s + e) / 2;
    let d = f.run(&["at", &format!("{p}:{mid}")])["data"][0].clone();
    assert_eq!(d["name"], "outer");
    assert!(
        d["line"].as_u64().unwrap() <= mid && mid <= d["end_line"].as_u64().unwrap(),
        "at a body line must be enclosed by the def"
    );
}

#[test]
fn outline_names_are_a_subset_of_symbols() {
    let f = Fixture::new();
    f.write("a.rs", "struct S;\nfn f() {}\nimpl S { fn m(&self) {} }\n");
    let symbols: std::collections::HashSet<String> = field_set(
        &f.run(&["symbols", "--path", "a.rs", "--limit", "0"]),
        "name",
    )
    .into_iter()
    .collect();
    for n in field_set(&f.run(&["outline", "a.rs"]), "name") {
        assert!(
            symbols.contains(&n),
            "outline name {n} absent from symbols {symbols:?}"
        );
    }
}

#[test]
fn read_line_range_clamps_past_eof() {
    let f = Fixture::new();
    f.write("a.rs", "fn a() {}\nfn b() {}\nfn c() {}\n");
    f.run(&["index"]);
    let out = f.raw(&["read", "a.rs", "--lines", "2-999"]);
    assert!(out.status.success());
    let text = String::from_utf8_lossy(&out.stdout);
    assert!(
        text.contains("fn b") && text.contains("fn c"),
        "range content missing: {text}"
    );
    assert!(!text.contains("fn a"), "range must start at line 2: {text}");
}

#[test]
fn read_rejects_ranges_that_cannot_mean_what_they_say() {
    let f = Fixture::new();
    f.write("a.txt", "l1\nl2\nl3\n");
    for spec in ["a.txt:3-1", "a.txt:0"] {
        let out = f.raw(&["read", spec]);
        assert!(!out.status.success(), "{spec} should be refused");
        let err = String::from_utf8_lossy(&out.stderr);
        assert!(
            err.contains("invalid line range"),
            "{spec} should name the range, not the file: {err}"
        );
    }
    let out = f.raw(&["read", "a.txt", "--lines", "3-1"]);
    assert!(
        !out.status.success(),
        "an inverted --lines range is refused"
    );

    // Past the end is not an error, but it must not look like an empty slice.
    let out = f.raw(&["read", "a.txt:9-12"]);
    assert!(out.status.success());
    assert!(out.stdout.is_empty());
    let err = String::from_utf8_lossy(&out.stderr);
    assert!(err.contains("has 3 line(s)"), "{err}");
}

#[test]
fn at_beyond_eof_resolves_to_nothing() {
    let f = Fixture::new();
    f.write("a.rs", "fn a() {}\n");
    let v = f.run(&["at", "a.rs:9999"]);
    assert!(
        v["data"].as_array().unwrap().is_empty(),
        "line past EOF should enclose nothing"
    );
}

#[test]
fn fields_ignores_unknown_columns() {
    let f = Fixture::new();
    f.write("a.rs", "fn f() {}\n");
    let row = f.run(&["symbols", "--fields", "name,not_a_real_field"])["data"][0].clone();
    let keys: Vec<&str> = row
        .as_object()
        .unwrap()
        .keys()
        .map(|s| s.as_str())
        .collect();
    assert_eq!(
        keys,
        ["name"],
        "only the known requested field survives: {keys:?}"
    );
}

#[test]
fn rust_impl_method_extracted_with_signature() {
    let f = Fixture::new();
    f.write(
        "a.rs",
        "struct S;\nimpl S {\n    pub fn method(&self, x: i32) -> i32 { x }\n}\n",
    );
    assert!(
        field_set(&f.run(&["symbols", "--path", "a.rs"]), "name").contains(&"method".to_string())
    );
    let sig = f.run(&["definitions", "method"])["data"][0]["signature"]
        .as_str()
        .unwrap()
        .to_string();
    assert!(
        sig.contains("fn method") && sig.contains("i32"),
        "signature: {sig}"
    );
}

#[test]
fn python_class_method_nests_in_outline() {
    let f = Fixture::new();
    f.write(
        "a.py",
        "class Widget:\n    def method(self):\n        pass\n",
    );
    let v = f.run(&["outline", "a.py"]);
    let rows = v["data"].as_array().unwrap();
    let cls = rows.iter().find(|r| r["name"] == "Widget").unwrap();
    let meth = rows.iter().find(|r| r["name"] == "method").unwrap();
    assert_eq!(cls["depth"], 0);
    assert!(
        meth["depth"].as_u64().unwrap() >= 1,
        "method must nest under its class"
    );
}

#[test]
fn python_import_forms_all_capture_the_module() {
    let f = Fixture::new();
    f.write(
        "a.py",
        "import os\nfrom sys import path\nfrom collections import OrderedDict, defaultdict\nimport json as j\n",
    );
    let mods = field_set(&f.run(&["imports", "a.py"]), "module");
    // Plain imports keep the bare module.
    assert!(mods.contains(&"os".to_string()), "{mods:?}");
    assert!(mods.contains(&"json".to_string()), "{mods:?}");
    // `from X import a, b` is captured qualified per name (X.a) so the imported
    // symbol — dropped by the bare module string — resolves to its defining file.
    assert!(mods.contains(&"sys.path".to_string()), "{mods:?}");
    assert!(
        mods.contains(&"collections.OrderedDict".to_string()),
        "{mods:?}"
    );
    assert!(
        mods.contains(&"collections.defaultdict".to_string()),
        "{mods:?}"
    );
}

#[test]
fn search_sql_scopes_preserve_literal_substrings_and_order() {
    let f = Fixture::new();
    f.write("src/A%_é.rs", "fn needle() {}\n");
    f.write("src/prefixA%_é.rs", "fn needle() {}\n");
    f.write("src/Axxé.rs", "fn needle() {}\n");
    f.write("src/a%_é.rs", "fn needle() {}\n");
    f.write("src/A%_é.py", "def needle(): pass\n");
    f.write("other/end.rs", "fn needle() {}\n");
    for pattern in ["needle", "n.e"] {
        let v = f.run(&[
            "search",
            pattern,
            "--path",
            "A%_é",
            "--language",
            "rust",
            "-l",
        ]);
        assert_eq!(
            v["data"]["files"],
            serde_json::json!(["src/A%_é.rs", "src/prefixA%_é.rs"])
        );
        let v = f.run(&[
            "search",
            pattern,
            "A%_é.rs",
            "other",
            "--language",
            "rust",
            "-l",
            "-g",
            "!**/prefix*",
        ]);
        assert_eq!(
            v["data"]["files"],
            serde_json::json!(["other/end.rs", "src/A%_é.rs"])
        );
    }
    let v = f.run(&[
        "search",
        "absent",
        "--path",
        "A%_é",
        "--language",
        "rust",
        "-L",
    ]);
    assert_eq!(
        v["data"]["files"],
        serde_json::json!(["src/A%_é.rs", "src/prefixA%_é.rs"])
    );
}

#[test]
fn search_composes_word_language_and_path_filters() {
    let f = Fixture::new();
    f.write("a.rs", "fn value() {}\nlet value = 1;\n");
    f.write("b.py", "value = 2\n");
    let v = f.run(&[
        "search",
        "value",
        "-w",
        "--language",
        "rust",
        "--path",
        "a.rs",
        "--limit",
        "0",
    ]);
    let matches = v["data"]["matches"].as_array().unwrap();
    assert!(!matches.is_empty(), "expected matches in a.rs");
    assert!(
        matches.iter().all(|m| m["path"] == "a.rs"),
        "path/language filter leaked: {matches:?}"
    );
}

#[test]
fn context_changed_mode_can_expand_to_referenced_defs() {
    let f = Fixture::new();
    f.write("a.py", "def helper():\n    pass\n");
    f.git_init_commit();
    f.write(
        "a.py",
        "def helper():\n    pass\n\ndef changed():\n    helper()\n",
    );
    let names = chunk_symbols(
        &f.run(&[
            "context",
            "--changed",
            "--compare-to",
            "HEAD",
            "--expand",
            "--no-text",
        ])["data"],
    );
    assert!(
        names.contains(&"changed".to_string()),
        "touched seed missing: {names:?}"
    );
    assert!(
        names.contains(&"helper".to_string()),
        "expansion to referenced def missing: {names:?}"
    );
}

#[test]
fn map_totals_agree_with_the_summary_and_symbols() {
    // The aggregate counts must reconcile: map.totals == map --totals == symbols.
    let f = Fixture::new();
    f.write("a.rs", "fn one() {}\nfn two() {}\n");
    f.write("b.py", "def three():\n    pass\n");
    f.run(&["index"]);
    let files_stats = f.run(&["map", "--totals"])["data"]["totals"]["files"]
        .as_u64()
        .unwrap();
    let map = f.run(&["map"]);
    assert_eq!(files_stats, 2);
    assert_eq!(
        map["data"]["totals"]["files"].as_u64().unwrap(),
        files_stats,
        "map totals.files must match the --totals summary"
    );
    let nsym = f.run(&["symbols", "--limit", "0"])["data"]
        .as_array()
        .unwrap()
        .len() as u64;
    assert_eq!(nsym, 3);
    assert_eq!(
        map["data"]["totals"]["symbols"].as_u64().unwrap(),
        nsym,
        "map totals.symbols must match the actual symbol count"
    );
}

#[test]
fn references_span_multiple_files() {
    let f = Fixture::new();
    f.write("a.rs", "pub fn shared() {}\n");
    f.write("b.rs", "fn use_it() { shared(); }\n");
    f.write("c.rs", "fn use_more() { shared(); shared(); }\n");
    let refs = f.run(&["references", "shared", "--limit", "0"]);
    let paths: std::collections::HashSet<String> = refs["data"]["references"]
        .as_array()
        .unwrap()
        .iter()
        .filter_map(|r| r["path"].as_str().map(str::to_string))
        .collect();
    for p in ["a.rs", "b.rs", "c.rs"] {
        assert!(paths.contains(p), "references should span {p}: {paths:?}");
    }
}

#[test]
fn go_struct_method_and_free_func_extracted() {
    let f = Fixture::new();
    f.write(
        "a.go",
        "package main\ntype T struct{ X int }\nfunc (t T) Method() int { return t.X }\nfunc Free() {}\n",
    );
    let names = field_set(&f.run(&["symbols", "--path", "a.go"]), "name");
    for n in ["T", "Method", "Free"] {
        assert!(
            names.iter().any(|x| x == n),
            "go symbol {n} missing: {names:?}"
        );
    }
}

#[test]
fn typescript_interface_class_and_method_extracted() {
    let f = Fixture::new();
    f.write(
        "a.ts",
        "interface Shape { area(): number; }\nclass Circle implements Shape {\n  area(): number { return 1; }\n}\n",
    );
    let names = field_set(&f.run(&["symbols", "--path", "a.ts"]), "name");
    for n in ["Shape", "Circle", "area"] {
        assert!(
            names.iter().any(|x| x == n),
            "ts symbol {n} missing: {names:?}"
        );
    }
}

#[test]
fn search_output_is_reproducible() {
    // Determinism is a core promise: identical inputs, byte-identical output.
    // Warm the index first so neither query reindexes (which would legitimately
    // add an index meta block to only the first response).
    let f = Fixture::new();
    f.write("a.rs", "fn alpha() {}\nfn beta() {}\nfn alpha_two() {}\n");
    f.run(&["index"]);
    let v1 = f.run(&["search", "alpha", "--limit", "0"]);
    let v2 = f.run(&["search", "alpha", "--limit", "0"]);
    assert_eq!(v1, v2, "search must be byte-for-byte reproducible");
}

#[test]
fn search_case_insensitive_matches_mixed_case() {
    let f = Fixture::new();
    f.write("a.rs", "let Value = 1;\nlet value = 2;\nlet VALUE = 3;\n");
    let ci = f.run(&["search", "value", "-i", "--limit", "0"]);
    assert_eq!(
        ci["data"]["matches"].as_array().unwrap().len(),
        3,
        "-i must match all three cases"
    );
    let cs = f.run(&["search", "value", "--limit", "0"]);
    assert_eq!(
        cs["data"]["matches"].as_array().unwrap().len(),
        1,
        "case-sensitive matches only 'value'"
    );
}

#[test]
fn search_regex_features_match_expected_lines() {
    let f = Fixture::new();
    f.write("a.rs", "let n = 42;\nlet name = bar;\nlet x = foo;\n");
    let lines = |v: &Value| -> Vec<u64> {
        let mut l: Vec<u64> = v["data"]["matches"]
            .as_array()
            .unwrap()
            .iter()
            .map(|m| m["line"].as_u64().unwrap())
            .collect();
        l.sort();
        l.dedup();
        l
    };
    assert_eq!(
        lines(&f.run(&["search", "[0-9]+", "--limit", "0"])),
        vec![1],
        "char class + quantifier"
    );
    assert_eq!(
        lines(&f.run(&["search", "foo|bar", "--limit", "0"])),
        vec![2, 3],
        "alternation"
    );
    assert_eq!(
        lines(&f.run(&["search", "names?", "--limit", "0"])),
        vec![2],
        "optional quantifier"
    );
}

#[test]
fn read_symbol_returns_only_that_definition() {
    let f = Fixture::new();
    f.write(
        "a.rs",
        "fn first() {\n    let a = 1;\n}\nfn second() {\n    let b = 2;\n}\n",
    );
    f.run(&["index"]);
    let out = String::from_utf8_lossy(&f.raw(&["read", "a.rs", "--symbol", "second"]).stdout)
        .into_owned();
    assert!(
        out.contains("fn second") && out.contains("let b = 2"),
        "second not returned: {out}"
    );
    assert!(
        !out.contains("fn first") && !out.contains("let a = 1"),
        "sibling leaked into --symbol read: {out}"
    );
}

#[test]
fn read_raw_emits_exact_bytes_ready_for_edit() {
    let f = Fixture::new();
    f.write(
        "a.rs",
        "fn other() {}\nfn helper() {\n    let x = 1;\n    x\n}\n",
    );

    // Default is line-numbered; --raw is the exact source, no prefixes.
    let numbered =
        String::from_utf8(f.raw(&["read", "a.rs", "--symbol", "helper"]).stdout).unwrap();
    assert!(
        numbered.starts_with("2\tfn helper"),
        "default should be line-numbered: {numbered:?}"
    );

    let raw = String::from_utf8(
        f.raw(&["read", "a.rs", "--symbol", "helper", "--raw"])
            .stdout,
    )
    .unwrap();
    // Byte-exact: the source of `helper` verbatim, and it drops into Edit's
    // old_string as-is (must be a substring of the file's own text).
    assert_eq!(raw, "fn helper() {\n    let x = 1;\n    x\n}\n");
    let file = std::fs::read_to_string(f.dir.path().join("a.rs")).unwrap();
    assert!(
        file.contains(raw.trim_end()),
        "raw output must be a verbatim slice of the file"
    );
}

#[test]
fn read_raw_preserves_line_endings_and_missing_final_newline() {
    let f = Fixture::new();
    f.write("a.rs", "fn one() {}\r\nfn two() {}\r\nlast");

    let whole = f.raw(&["read", "a.rs", "--raw"]);
    assert_eq!(whole.stdout, b"fn one() {}\r\nfn two() {}\r\nlast");

    let first_line = f.raw(&["read", "a.rs", "--lines", "1-1", "--raw"]);
    assert_eq!(first_line.stdout, b"fn one() {}\r\n");

    f.write_bytes("invalid.rs", b"// invalid: \xff\nfn exact() {}\n");
    let invalid_utf8 = f.raw(&["read", "invalid.rs", "--raw"]);
    assert_eq!(invalid_utf8.stdout, b"// invalid: \xff\nfn exact() {}\n");
}

#[test]
fn read_raw_symbol_refuses_ambiguous_match() {
    let f = Fixture::new();
    f.write("a.rs", "fn dup() {}\nmod m { fn dup() {} }\n");
    let r = f.raw(&["read", "a.rs", "--symbol", "dup", "--raw"]);
    assert!(!r.status.success(), "ambiguous --raw should fail");
    let err = String::from_utf8_lossy(&r.stderr);
    assert!(
        err.contains("needs one"),
        "should explain the ambiguity: {err}"
    );
}

#[test]
fn callees_report_the_functions_a_symbol_calls() {
    let f = Fixture::new();
    f.write(
        "a.rs",
        "fn caller() {\n    helper();\n    other();\n}\nfn helper() {}\nfn other() {}\nfn unrelated() {}\n",
    );
    let names = field_set(&f.run(&["callees", "caller"]), "name");
    assert!(
        names.contains(&"helper".to_string()),
        "helper missing: {names:?}"
    );
    assert!(
        names.contains(&"other".to_string()),
        "other missing: {names:?}"
    );
    assert!(
        !names.contains(&"unrelated".to_string()),
        "uncalled sibling listed as callee: {names:?}"
    );
}

#[test]
fn path_scopes_definitions_to_one_file() {
    // --path disambiguates a common name across files (the first dogfood fix).
    let f = Fixture::new();
    f.write("a.rs", "fn build() {}\n");
    f.write("b.rs", "fn build() {}\n");
    assert_eq!(
        f.run(&["definitions", "build"])["data"]
            .as_array()
            .unwrap()
            .len(),
        2,
        "both defs unscoped"
    );
    let scoped = f.run(&["definitions", "build", "--path", "a.rs"]);
    let rows = scoped["data"].as_array().unwrap();
    assert_eq!(rows.len(), 1, "scoped to one file");
    assert_eq!(rows[0]["path"], "a.rs");
}

#[test]
fn javascript_require_and_es_import_both_captured() {
    // CommonJS require("x") is a call, not a syntactic import, but it IS the
    // import mechanism for a huge amount of JS/TS — capture it alongside ES import.
    let f = Fixture::new();
    f.write(
        "a.js",
        "import fs from \"fs\";\nconst p = require(\"path\");\nfunction f() { console.log(\"hi\"); }\n",
    );
    let mods = field_set(&f.run(&["imports", "a.js"]), "module");
    assert!(
        mods.iter().any(|m| m == "fs"),
        "ES import missing: {mods:?}"
    );
    assert!(
        mods.iter().any(|m| m == "path"),
        "require() import missing: {mods:?}"
    );
    assert!(
        !mods.iter().any(|m| m == "hi" || m == "console"),
        "a non-require call leaked: {mods:?}"
    );
}

#[test]
fn tests_linkage_finds_tests_in_test_files() {
    let f = Fixture::new();
    f.write(
        "src/lib.rs",
        "pub fn widget() -> i32 { 1 }\npub fn other() {}\n",
    );
    f.write(
        "tests/w.rs",
        "fn test_widget_works() {\n    assert_eq!(widget(), 1);\n}\nfn test_other_only() {\n    other();\n}\n",
    );
    let names: Vec<String> = f.run(&["tests", "widget"])["data"]["tests"]
        .as_array()
        .unwrap()
        .iter()
        .map(|t| t["name"].as_str().unwrap().to_string())
        .collect();
    assert!(
        names.contains(&"test_widget_works".to_string()),
        "{names:?}"
    );
    assert!(
        !names.contains(&"test_other_only".to_string()),
        "doesn't reference widget: {names:?}"
    );
}

#[test]
fn tests_linkage_finds_rust_unit_tests_in_a_cfg_test_module() {
    // The enclosing `mod tests` is the only signal: `src/lib.rs` is not a test
    // path and `returns_one` is not a test-like name. `tests widget` used to
    // answer "none" for a function whose test sits six lines below it — and an
    // empty result reads as "no test covers this", so it was a confident lie.
    let f = Fixture::new();
    f.write(
        "src/lib.rs",
        "pub fn widget() -> i32 { 1 }\npub fn other() {}\n\n\
         #[cfg(test)]\n\
         mod tests {\n\
         \x20   use super::*;\n\n\
         \x20   #[test]\n\
         \x20   fn returns_one() {\n\
         \x20       assert_eq!(widget(), 1);\n\
         \x20   }\n\n\
         \x20   #[test]\n\
         \x20   fn touches_other() {\n\
         \x20       other();\n\
         \x20   }\n\
         }\n",
    );
    let names: Vec<String> = f.run(&["tests", "widget"])["data"]["tests"]
        .as_array()
        .unwrap()
        .iter()
        .map(|t| t["name"].as_str().unwrap().to_string())
        .collect();
    assert!(
        names.contains(&"returns_one".to_string()),
        "unit test not linked: {names:?}"
    );
    assert!(
        !names.contains(&"touches_other".to_string()),
        "doesn't reference widget: {names:?}"
    );
}

#[test]
fn tests_linkage_uses_test_name_heuristic() {
    // A test in a non-test-path file is still found via its test-like name.
    let f = Fixture::new();
    f.write(
        "a.py",
        "def target():\n    pass\ndef test_target():\n    target()\ndef helper():\n    target()\n",
    );
    let names: Vec<String> = f.run(&["tests", "target"])["data"]["tests"]
        .as_array()
        .unwrap()
        .iter()
        .map(|t| t["name"].as_str().unwrap().to_string())
        .collect();
    assert!(names.contains(&"test_target".to_string()), "{names:?}");
    assert!(
        !names.contains(&"helper".to_string()),
        "helper is not a test: {names:?}"
    );
}

#[test]
fn tests_linkage_finds_javascript_test_callbacks_and_keeps_locations() {
    let f = Fixture::new();
    f.write("src/helper.js", "export function helper() {}\n");
    f.write(
        "tests/helper.test.js",
        "describe('Widget', () => {\n  it('renders', () => { helper(); });\n  test.each([1, 2])('handles %s', (value) => { helper(value); });\n  register(() => { helper(); });\n});\n",
    );

    let rows = f.run(&["tests", "helper"])["data"]["tests"]
        .as_array()
        .unwrap()
        .clone();
    let found: Vec<(String, i64)> = rows
        .iter()
        .map(|row| {
            (
                row["name"].as_str().unwrap().to_string(),
                row["line"].as_i64().unwrap(),
            )
        })
        .collect();
    assert_eq!(
        found,
        [
            ("Widget > renders".into(), 2),
            ("Widget > handles %s".into(), 3)
        ]
    );
}

#[test]
fn astgrep_matches_method_calls_structurally() {
    // `$X.unwrap()` matches actual `.unwrap()` call nodes (any receiver), not the
    // field access that merely shares the name, and not text in strings/comments.
    let f = Fixture::new();
    f.write(
        "a.rs",
        "fn f() {\n    let x = foo().unwrap();\n    let y = self.unwrap;\n    bar.unwrap();\n    // z.unwrap() in a comment\n}\n",
    );
    let v = f.run(&["astgrep", "$X.unwrap()", "--language", "rust"]);
    let texts: Vec<String> = v["data"]["matches"]
        .as_array()
        .unwrap()
        .iter()
        .map(|r| r["text"].as_str().unwrap().to_string())
        .collect();
    assert_eq!(texts.len(), 2, "expected 2 real unwrap calls: {texts:?}");
    assert!(texts.iter().any(|t| t == "foo().unwrap()"), "{texts:?}");
    assert!(texts.iter().any(|t| t == "bar.unwrap()"), "{texts:?}");
}

#[test]
fn astgrep_candidate_filter_keeps_unicode_escapes_and_literal_free_patterns() {
    let f = Fixture::new();
    f.write(
        "src/A%_é.rs",
        "fn sample() { café(value); emit(\"a\\\"b\"); xy(value); left + right; }\n",
    );
    f.write(
        "src/other.rs",
        "fn other() { cafe(value); emit(\"different\"); xyz(value); }\n",
    );
    for (pattern, expected) in [
        ("café($X)", "café(value)"),
        ("emit(\"a\\\"b\")", "emit(\"a\\\"b\")"),
        ("xy($X)", "xy(value)"),
        ("$X + $Y", "left + right"),
    ] {
        let v = f.run(&["astgrep", pattern, "--language", "rust", "--limit", "0"]);
        let rows = v["data"]["matches"].as_array().unwrap();
        assert_eq!(rows.len(), 1, "{pattern}: {v}");
        assert_eq!(rows[0]["text"], expected);
        assert_eq!(rows[0]["path"], "src/A%_é.rs");
    }
    let v = f.run(&[
        "astgrep",
        "café($X)",
        "--language",
        "rust",
        "--path",
        "A%_é",
    ]);
    assert_eq!(v["data"]["matches"].as_array().unwrap().len(), 1);
    let out = f.raw(&[
        "astgrep",
        "missing_literal($X)",
        "--language",
        "rust",
        "--path",
        "src",
    ]);
    assert!(out.status.success());
    assert!(String::from_utf8(out.stderr)
        .unwrap()
        .contains("pattern ran over 2 rust file(s)"));
}

#[test]
fn astgrep_limit_returns_the_leading_matches_in_path_order() {
    // A limited search stops early, so which matches it keeps must be the same
    // ones an exhaustive search ranks first, across files, with the truncation
    // flag set exactly when more existed.
    let f = Fixture::new();
    for i in 0..30 {
        // Matches inside one file are discovered out of position order.
        f.write(
            &format!("src/m{i:02}.rs"),
            "fn a() {\n    x.unwrap();\n    if true { y.unwrap(); }\n    z.unwrap();\n}\n",
        );
    }
    let rows = |args: &[&str]| -> (Vec<(String, u64, u64)>, bool) {
        let mut a = vec!["astgrep", "$X.unwrap()", "--language", "rust"];
        a.extend_from_slice(args);
        let v = f.run(&a);
        let rows = v["data"]["matches"]
            .as_array()
            .unwrap()
            .iter()
            .map(|r| {
                (
                    r["path"].as_str().unwrap().to_string(),
                    r["line"].as_u64().unwrap(),
                    r["col"].as_u64().unwrap(),
                )
            })
            .collect();
        (rows, v["data"]["truncated"].as_bool().unwrap_or(false))
    };
    let (all, truncated) = rows(&["--limit", "0"]);
    assert_eq!(all.len(), 90);
    assert!(!truncated);
    let mut sorted = all.clone();
    sorted.sort();
    assert_eq!(all, sorted, "exhaustive result is path then position order");
    for limit in [1usize, 2, 3, 4, 31, 89, 90, 91] {
        let (got, truncated) = rows(&["--limit", &limit.to_string()]);
        assert_eq!(got, all[..limit.min(90)], "limit {limit}");
        assert_eq!(truncated, limit < 90, "truncated flag at limit {limit}");
    }
}

#[test]
fn astgrep_metavariable_binds_consistently() {
    let f = Fixture::new();
    f.write(
        "a.rs",
        "fn f(a: i32, b: i32) -> bool {\n    let _ = a == a;\n    a == b\n}\n",
    );
    let m = f.run(&["astgrep", "$X == $X", "--language", "rust"]);
    let rows = m["data"]["matches"].as_array().unwrap();
    assert_eq!(rows.len(), 1, "only a==a matches a consistent metavar");
    assert_eq!(rows[0]["text"], "a == a");
    assert_eq!(rows[0]["binds"]["X"], "a");
}

#[test]
fn astgrep_named_variadics_preserve_repeated_bindings_and_rollback() {
    let f = Fixture::new();
    f.write(
        "a.rs",
        "fn f() {
foo(a, b, a);
foo(a, a);
foo(a, b, c);
         pair(foo(a, b), bar(a, b));
pair(foo(a, b), bar(a, c));
         pair(foo(), bar());
}
",
    );
    let matches = f.run(&[
        "astgrep",
        "foo($X, $$$REST, $X)",
        "--language",
        "rust",
        "--limit",
        "0",
    ]);
    let rows = matches["data"]["matches"].as_array().unwrap();
    assert_eq!(rows.len(), 2, "{matches}");
    assert_eq!(rows[0]["binds"]["X"], "a");
    assert_eq!(rows[0]["binds"]["REST"], "b");
    assert_eq!(rows[1]["binds"]["REST"], "");

    let repeated = f.run(&[
        "astgrep",
        "pair(foo($$$ARGS), bar($$$ARGS))",
        "--language",
        "rust",
        "--limit",
        "0",
    ]);
    let rows = repeated["data"]["matches"].as_array().unwrap();
    assert_eq!(rows.len(), 2, "{repeated}");
    assert_eq!(rows[0]["binds"]["ARGS"], "a\u{0}b");
    assert_eq!(rows[1]["binds"]["ARGS"], "");
}

#[test]
fn astgrep_keeps_literals_and_keywords_exact() {
    let f = Fixture::new();
    f.write(
        "a.rs",
        "fn g() -> Result<i32> {\n    return Ok(1);\n}\nfn h() {\n    let _x = Ok(2);\n}\n",
    );
    let rows = f.run(&["astgrep", "return Ok($X)", "--language", "rust"]);
    let m = rows["data"]["matches"].as_array().unwrap();
    assert_eq!(
        m.len(),
        1,
        "must stay anchored to `return`, not match `let _x = Ok(2)`"
    );
    assert_eq!(m[0]["text"], "return Ok(1)");
}

#[test]
fn astgrep_works_across_languages() {
    let f = Fixture::new();
    f.write("a.py", "def f():\n    print(x)\n    print(y)\n    log(z)\n");
    let rows = f.run(&["astgrep", "print($X)", "--language", "python"]);
    assert_eq!(
        rows["data"]["matches"].as_array().unwrap().len(),
        2,
        "two print() calls"
    );
}

#[test]
fn astgrep_variadic_matches_any_arguments() {
    // `$$$` absorbs a run of zero-or-more siblings in a list position.
    let f = Fixture::new();
    f.write(
        "a.rs",
        "fn f() {\n    foo();\n    foo(1);\n    foo(1, 2, 3);\n    bar(9);\n}\n",
    );
    let all = f.run(&["astgrep", "foo($$$)", "--language", "rust"]);
    assert_eq!(
        all["data"]["matches"].as_array().unwrap().len(),
        3,
        "foo() with 0/1/3 args, not bar"
    );
    // A fixed leading arg plus a variadic requires at least that arg.
    let some = f.run(&["astgrep", "foo($A, $$$)", "--language", "rust"]);
    let texts: Vec<String> = some["data"]["matches"]
        .as_array()
        .unwrap()
        .iter()
        .map(|r| r["text"].as_str().unwrap().to_string())
        .collect();
    assert_eq!(
        texts.len(),
        2,
        "foo(1) and foo(1,2,3), not the no-arg call: {texts:?}"
    );
    assert!(!texts.iter().any(|t| t == "foo()"), "{texts:?}");
}

#[test]
fn astgrep_variadic_works_in_parameter_and_body_positions_at_once() {
    // `fn $N($$$) { $$$ }` — the module doc's own example, and the most natural
    // way to ask for "any function". Both `$$$` sit inside a list node that holds
    // nothing else, which used to read as two variadics at the declaration level
    // and trip the single-variadic guard, returning zero matches with exit 0.
    let f = Fixture::new();
    f.write(
        "a.rs",
        "fn none() { let a = 1; }\nfn two(x: i32, y: i32) { let b = x; }\n\
         fn empty() {}\nfn typed() -> i32 { 3 }\n",
    );
    let m = f.run(&["astgrep", "fn $N($$$) { $$$ }", "--language", "rust"]);
    let names: Vec<String> = m["data"]["matches"]
        .as_array()
        .unwrap()
        .iter()
        .map(|r| {
            r["binds"][0]["value"]
                .as_str()
                .unwrap_or_default()
                .to_string()
        })
        .collect();
    assert_eq!(
        m["data"]["matches"].as_array().unwrap().len(),
        3,
        "expected none/two/empty (typed() has a return type): {names:?}"
    );

    // Same shape in a language with untyped parameters.
    f.write(
        "p.py",
        "def one(a, b):\n    return a\ndef two():\n    return 1\n",
    );
    let py = f.run(&["astgrep", "def $N($$$): $$$", "--language", "python"]);
    assert_eq!(py["data"]["matches"].as_array().unwrap().len(), 2, "{py}");

    // The single-variadic behaviour it must not regress.
    f.write("b.rs", "fn g() {\n    foo();\n    foo(1, 2);\n}\n");
    let calls = f.run(&["astgrep", "foo($$$)", "--language", "rust"]);
    assert_eq!(
        calls["data"]["matches"].as_array().unwrap().len(),
        2,
        "{calls}"
    );
}

#[test]
fn callees_are_calls_not_field_or_type_references() {
    // callees is call-aware: a name counts only when it's actually called
    // (followed by `(`), so field accesses and type/variable references that
    // merely share a name with a definition are not reported as callees.
    let f = Fixture::new();
    f.write(
        "a.rs",
        "fn helper() {}\nfn field() {}\nstruct SomeType;\nfn caller() {\n    helper();\n    let y = self.field;\n    let z = SomeType;\n}\n",
    );
    let names = field_set(&f.run(&["callees", "caller"]), "name");
    assert!(
        names.contains(&"helper".to_string()),
        "called function missing: {names:?}"
    );
    assert!(
        !names.contains(&"field".to_string()),
        "a field access is not a callee: {names:?}"
    );
    assert!(
        !names.contains(&"SomeType".to_string()),
        "a type reference is not a callee: {names:?}"
    );
}

#[test]
fn cpp_header_method_declarations_are_extracted() {
    // Declaration-only members (the norm in .hpp headers) must be findable, not
    // just inline or out-of-line definitions — otherwise a header-only class
    // exposes none of its API.
    let f = Fixture::new();
    f.write(
        "w.hpp",
        "class Widget {\npublic:\n    void declared_only();\n    int compute(int x) const;\n    void inline_def() {}\n};\nint free_prototype(int a);\n",
    );
    let names = field_set(&f.run(&["symbols", "--path", "w.hpp"]), "name");
    for n in [
        "Widget",
        "declared_only",
        "compute",
        "inline_def",
        "free_prototype",
    ] {
        assert!(
            names.iter().any(|x| x == n),
            "cpp symbol {n} missing: {names:?}"
        );
    }
    // The inline definition is not double-counted.
    assert_eq!(
        names.iter().filter(|x| *x == "inline_def").count(),
        1,
        "inline method duplicated"
    );
    // A declaration-only method still resolves via `definitions`, with a signature.
    let sig = f.run(&["definitions", "declared_only"])["data"][0]["signature"]
        .as_str()
        .unwrap()
        .to_string();
    assert!(
        sig.contains("declared_only"),
        "declaration signature missing: {sig}"
    );
}

#[test]
fn php_require_and_include_are_imports() {
    // PHP's file-inclusion constructs (require/require_once/include/include_once)
    // are its other import mechanism alongside `use`; capture static string paths
    // in both quote styles.
    let f = Fixture::new();
    f.write(
        "a.php",
        "<?php\nuse App\\Thing;\nrequire 'bootstrap.php';\ninclude_once \"vendor/autoload.php\";\nfunction f() { echo \"hi\"; }\n",
    );
    let mods = field_set(&f.run(&["imports", "a.php"]), "module");
    assert!(
        mods.iter().any(|m| m == "App\\Thing"),
        "use statement missing: {mods:?}"
    );
    assert!(
        mods.iter().any(|m| m == "bootstrap.php"),
        "single-quoted require missing: {mods:?}"
    );
    assert!(
        mods.iter().any(|m| m == "vendor/autoload.php"),
        "double-quoted include_once missing: {mods:?}"
    );
    assert!(
        !mods.iter().any(|m| m == "hi"),
        "an echoed string leaked as an import: {mods:?}"
    );
}

#[test]
fn go_import_block_captures_each_module() {
    let f = Fixture::new();
    f.write(
        "a.go",
        "package main\nimport (\n\t\"fmt\"\n\t\"os\"\n)\nfunc F() { fmt.Println(os.Args) }\n",
    );
    let mods = field_set(&f.run(&["imports", "a.go"]), "module");
    assert!(
        mods.iter().any(|m| m == "fmt"),
        "fmt missing from import block: {mods:?}"
    );
    assert!(
        mods.iter().any(|m| m == "os"),
        "os missing from import block: {mods:?}"
    );
}

// ---------------------------------------------------------------------------
// deps (module-dependency graph)
// ---------------------------------------------------------------------------

#[test]
fn deps_resolves_imports_to_internal_files_and_drops_external() {
    let f = Fixture::new();
    // a imports an internal module (b) and an external one (os).
    f.write("a.py", "import os\nimport b\n");
    f.write("b.py", "x = 1\n");

    let v = f.run(&["deps", "a.py"]);
    let rows = v["data"].as_array().unwrap();
    let targets: Vec<&str> = rows.iter().map(|r| r["path"].as_str().unwrap()).collect();
    assert_eq!(
        targets,
        ["b.py"],
        "only the internal import resolves: {targets:?}"
    );
    assert_eq!(rows[0]["module"], "b", "edge carries the raw import string");
    assert_eq!(rows[0]["depth"], 1);
}

#[test]
fn deps_preserves_relative_paths_and_entry_files_with_nonstandard_names() {
    let f = Fixture::new();
    f.write(
        "app.js",
        "import './my-utils';\nimport './other/index.html';\nimport '.';\n",
    )
    .write("my-utils/index.html", "<p>directory entry</p>\n")
    .write("other/index.html", "<p>explicit file</p>\n")
    .write("index.js", "export const root = 1;\n")
    .write("unrelated/my-utils/index.html", "<p>unrelated</p>\n");
    assert_eq!(
        field_set(&f.run(&["deps", "app.js"]), "path"),
        ["index.js", "my-utils/index.html", "other/index.html"]
    );
}

#[test]
fn deps_does_not_resolve_relative_imports_that_escape_the_repo_root() {
    let f = Fixture::new();
    f.write("main.js", "import '../helper.js';\n")
        .write("helper.js", "export const value = 1;\n");

    let deps = field_set(&f.run(&["deps", "main.js"]), "path");
    assert!(
        deps.is_empty(),
        "an import outside the repository must not alias a root file: {deps:?}"
    );

    f.write("pkg/sub/inside.js", "import '../../helper.js';\n");
    let deps = field_set(&f.run(&["deps", "pkg/sub/inside.js"]), "path");
    assert_eq!(deps, ["helper.js"]);
}

#[test]
fn deps_prefers_python_submodule_over_package_initializer() {
    let f = Fixture::new();
    f.write("pkg/__init__.py", "\n");
    f.write("pkg/mod.py", "def real():\n    return 1\n");
    f.write("client.py", "from pkg.mod import real\n");

    let deps = f.run(&["deps", "client.py"]);
    let targets: Vec<&str> = deps["data"]
        .as_array()
        .unwrap()
        .iter()
        .map(|row| row["path"].as_str().unwrap())
        .collect();
    assert_eq!(targets, ["pkg/mod.py"]);
}

#[test]
fn deps_depth_walks_the_import_chain() {
    let f = Fixture::new();
    f.write("a.py", "import b\n"); // a -> b -> c
    f.write("b.py", "import c\n");
    f.write("c.py", "x = 1\n");

    // depth 1 stops at the direct import.
    let d1 = field_set(&f.run(&["deps", "a.py"]), "path");
    assert!(
        d1.contains(&"b.py".to_string()) && !d1.contains(&"c.py".to_string()),
        "{d1:?}"
    );

    // depth 2 reaches c through b, tagged with its hop distance.
    let d2 = f.run(&["deps", "a.py", "--depth", "2"]);
    let rows = d2["data"].as_array().unwrap();
    let depth_of = |p: &str| {
        rows.iter().find(|r| r["path"] == p).unwrap()["depth"]
            .as_u64()
            .unwrap()
    };
    assert_eq!(depth_of("b.py"), 1);
    assert_eq!(depth_of("c.py"), 2, "transitive dep is hop 2");
}

#[test]
fn deps_cycles_reports_import_cycle() {
    let f = Fixture::new();
    f.write("x.py", "import y\n");
    f.write("y.py", "import x\n");
    f.write("z.py", "x = 1\n"); // not in any cycle

    let v = f.run(&["deps", "--cycles"]);
    let rows = v["data"].as_array().unwrap();
    assert_eq!(rows.len(), 1, "exactly one cycle: {rows:?}");
    let cyc: Vec<&str> = rows[0]["cycle"]
        .as_array()
        .unwrap()
        .iter()
        .map(|v| v.as_str().unwrap())
        .collect();
    assert_eq!(
        cyc,
        ["x.py", "y.py"],
        "cycle canonicalized to start at least member: {cyc:?}"
    );
    assert_eq!(rows[0]["length"], 2);
}

#[test]
fn deps_cycles_reports_overlapping_cycles_through_a_shared_file() {
    // Three cycles sharing files. Pruning the walk must not lose a cycle whose
    // nodes were already explored as part of another one.
    let f = Fixture::new();
    f.write("a.py", "import b\nimport c\n")
        .write("b.py", "import a\nimport c\n")
        .write("c.py", "import b\n");

    let v = f.run(&["deps", "--cycles"]);
    let cycles: Vec<Vec<&str>> = v["data"]
        .as_array()
        .unwrap()
        .iter()
        .map(|r| {
            r["cycle"]
                .as_array()
                .unwrap()
                .iter()
                .map(|v| v.as_str().unwrap())
                .collect()
        })
        .collect();
    assert_eq!(
        cycles,
        vec![
            vec!["a.py", "b.py"],
            vec!["a.py", "c.py", "b.py"],
            vec!["b.py", "c.py"],
        ],
        "every cycle, each canonicalized to its least member: {cycles:?}"
    );
}

#[test]
fn deps_cycles_stays_fast_on_a_wide_acyclic_graph() {
    // Two files per layer, each importing both files of the next: 28 files, no
    // cycles at all, and 2^13 paths. Must be TypeScript named imports, because
    // each one yields two exact edges to the same file (the module `./m1_0` and
    // the binding `./m1_0.f1_0`), which doubles the branching factor again.
    // Walking paths instead of cycles took 20s on this fixture and grew ~4x per
    // added layer. The answer is empty either way, so a pass that costs the
    // whole exponential looks exactly like a correct one until it is timed.
    let f = Fixture::new();
    const LAYERS: usize = 14;
    for l in 0..LAYERS {
        for w in 0..2 {
            let mut src = String::new();
            if l + 1 < LAYERS {
                for k in 0..2 {
                    src.push_str(&format!(
                        "import {{ f{}_{} }} from \"./m{}_{}\";\n",
                        l + 1,
                        k,
                        l + 1,
                        k
                    ));
                }
            }
            src.push_str(&format!("export function f{l}_{w}() {{ return {l}; }}\n"));
            f.write(&format!("src/m{l}_{w}.ts"), &src);
        }
    }

    // Build the index outside the timed section: this assertion is about cycle
    // enumeration, not cold tree-sitter/query setup competing with other tests.
    f.run(&["files"]);

    let started = std::time::Instant::now();
    let v = f.run(&["deps", "--cycles"]);
    let elapsed = started.elapsed();
    assert!(
        v["data"].as_array().unwrap().is_empty(),
        "a DAG has no cycles: {v}"
    );
    assert!(
        elapsed < std::time::Duration::from_secs(5),
        "cycle search cost scaled with paths, not cycles: took {elapsed:?}"
    );
}

#[test]
fn deps_cycles_ignores_segment_overlap_that_is_not_an_import_target() {
    // `teams` imports a symbol from `dataframes`, whose module path happens to
    // share `updater.data` with `data.py`. The old best-segment heuristic turned
    // that overlap into a reverse edge and claimed a cycle. The real package
    // entry makes the intended target explicit.
    let f = Fixture::new();
    f.write("updater/data/data.py", "from updater.data import teams\n")
        .write(
            "updater/data/teams.py",
            "from updater.data.dataframes import serialize\n",
        )
        .write(
            "updater/data/dataframes/__init__.py",
            "def serialize(): pass\n",
        );

    let v = f.run(&["deps", "--cycles"]);
    assert!(v["data"].as_array().unwrap().is_empty(), "false cycle: {v}");

    let deps = field_set(&f.run(&["deps", "updater/data/teams.py"]), "path");
    assert!(
        deps.contains(&"updater/data/dataframes/__init__.py".to_string()),
        "concrete package dependency disappeared: {deps:?}"
    );
}

#[test]
fn deps_disambiguates_same_stem_by_path_alignment() {
    // Two files share the stem `util`; an import naming the package picks the
    // aligned one, not the unrelated same-stem file.
    let f = Fixture::new();
    f.write("pkg/util.py", "a = 1\n");
    f.write("other/util.py", "b = 1\n");
    f.write("main.py", "import pkg.util\n");

    let targets = field_set(&f.run(&["deps", "main.py"]), "path");
    assert!(
        targets.contains(&"pkg/util.py".to_string()),
        "aligned file missing: {targets:?}"
    );
    assert!(
        !targets.contains(&"other/util.py".to_string()),
        "unaligned same-stem file leaked: {targets:?}"
    );
}

#[test]
fn map_totals_reports_per_language_counts_and_the_byte_total() {
    // `stats` used to own this. It was a strict subset of `map` — the only thing
    // it stated that `map` did not was the repo's total bytes, which `map` merely
    // left derivable by summing its language rows. That is now in `totals`, and
    // the cheap path is a flag rather than a second command to choose between.
    let f = Fixture::new();
    f.write("a.rs", "fn a() {}\n");
    f.write("b.rs", "fn b() {}\n");
    f.write("c.py", "def c():\n    pass\n");
    f.run(&["index"]);

    let v = f.run(&["map", "--totals"]);
    let by: std::collections::HashMap<&str, i64> = v["data"]["languages"]
        .as_array()
        .unwrap()
        .iter()
        .map(|r| {
            (
                r["language"].as_str().unwrap(),
                r["files"].as_i64().unwrap(),
            )
        })
        .collect();
    assert_eq!(by.get("rust"), Some(&2), "{v}");
    assert_eq!(by.get("python"), Some(&1), "{v}");

    // The byte total is stated, not just derivable — and it reconciles with the
    // per-language rows it replaced summing.
    let total: i64 = v["data"]["totals"]["bytes"].as_i64().unwrap();
    let summed: i64 = v["data"]["languages"]
        .as_array()
        .unwrap()
        .iter()
        .map(|r| r["bytes"].as_i64().unwrap())
        .sum();
    assert_eq!(total, summed, "{v}");
    assert!(total > 0);

    // `--totals` is a view of `map`, not a different answer.
    let full = f.run(&["map"]);
    assert_eq!(full["data"]["totals"], v["data"]["totals"], "{full}");

    // And the command it replaced is gone.
    assert!(
        !f.raw(&["stats"]).status.success(),
        "`stats` should no longer exist"
    );
}

#[test]
fn context_accepts_multiple_seed_symbols() {
    let f = Fixture::new();
    f.write("a.rs", "fn aa() {}\nfn bb() {}\nfn cc() {}\n");
    let names = chunk_symbols(&f.run(&["context", "--symbols", "aa,bb", "--no-text"])["data"]);
    assert!(
        names.contains(&"aa".to_string()) && names.contains(&"bb".to_string()),
        "both seeds should be returned: {names:?}"
    );
    assert!(
        !names.contains(&"cc".to_string()),
        "unrequested symbol leaked: {names:?}"
    );
}

// ---------------------------------------------------------------------------
// Piped input (--stdin)
// ---------------------------------------------------------------------------

#[test]
fn search_can_fold_hits_to_symbols_and_return_their_source_once() {
    let f = Fixture::new();
    f.write(
        "a.rs",
        "fn one() { needle(); needle(); }\nfn two() { needle(); }\nfn three() { needle(); }\n",
    );

    let folded = f.run(&["search", "needle", "--symbols"]);
    let rows = folded["data"]["symbols"].as_array().unwrap();
    assert_eq!(rows.len(), 3, "one row per enclosing definition: {folded}");
    assert_eq!(rows[0]["name"], "one");
    assert_eq!(rows[0]["hits"], 1, "search rows are line-based: {folded}");

    let source = f.run(&["search", "needle", "--source", "--max-tokens", "100"]);
    let chunks = source["data"]["source"].as_array().unwrap();
    assert_eq!(chunks.len(), 3, "definitions are deduplicated: {source}");
    assert!(chunks
        .iter()
        .all(|c| c["text"].as_str().unwrap().contains("needle")));

    let limited = f.run(&["search", "needle", "--symbols", "--limit", "2"]);
    assert_eq!(limited["data"]["symbols"].as_array().unwrap().len(), 2);
    assert_eq!(limited["data"]["truncated"], true);
}

#[test]
fn search_ranks_definitions_before_limits_and_source_budgets() {
    let f = Fixture::new();
    let strongest = "fn strongest() {\n    needle();\n    needle();\n    needle();\n}";
    f.write(
        "a.rs",
        "fn early() { needle(); }\nfn second() { needle(); }\n",
    )
    .write("b.rs", "fn later() { needle(); }\n")
    .write("z.rs", &format!("{strongest}\n"));

    let ranked = f.run(&["search", "needle", "--symbols"]);
    let rows = ranked["data"]["symbols"].as_array().unwrap();
    let names: Vec<_> = rows.iter().map(|r| r["name"].as_str().unwrap()).collect();
    assert_eq!(names, ["strongest", "early", "second", "later"]);
    assert_eq!(rows[0]["hits"], 3);

    let limited = f.run(&["search", "needle", "--symbols", "--limit", "1"]);
    assert_eq!(limited["data"]["symbols"][0]["name"], "strongest");
    assert_eq!(limited["data"]["truncated"], true);

    // Every weaker definition fits individually, but reading one first would
    // leave too little budget for the strongest definition.
    let budget = strongest.len().div_ceil(4).to_string();
    let source = f.run(&["search", "needle", "--source", "--max-tokens", &budget]);
    let chunks = source["data"]["source"].as_array().unwrap();
    assert_eq!(chunks.len(), 1, "{source}");
    assert_eq!(chunks[0]["name"], "strongest");
    assert_eq!(chunks[0]["text"], strongest);
    assert_eq!(source["data"]["omitted"].as_array().unwrap().len(), 3);

    let full = f.run(&["search", "needle", "--source"]);
    let names: Vec<_> = full["data"]["source"]
        .as_array()
        .unwrap()
        .iter()
        .map(|c| c["name"].as_str().unwrap())
        .collect();
    assert_eq!(names, ["strongest", "early", "second", "later"]);
    let text = f.raw(&["search", "needle", "--source", "--max-tokens", &budget]);
    assert!(text.status.success());
    let text = String::from_utf8(text.stdout).unwrap();
    assert!(text.contains(strongest), "{text}");
    assert!(!text.contains("fn early"), "{text}");
}

/// Run codeq with `input` on stdin.
fn piped(dir: &Path, args: &[&str], input: &str) -> Output {
    use std::io::Write;
    use std::process::Stdio;

    let mut child = Command::cargo_bin("codeq")
        .unwrap()
        .current_dir(dir)
        .args(args)
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .expect("spawn codeq");
    child
        .stdin
        .take()
        .unwrap()
        .write_all(input.as_bytes())
        .unwrap();
    child.wait_with_output().expect("run codeq")
}

/// The property that protects every other use of codeq: `--stdin` is opt-in and
/// never inferred. An agent's shell hands the process a pipe rather than a TTY,
/// so auto-detection would silently turn ordinary searches into a scan of an
/// empty buffer, and the MCP server would answer nothing for every query.
#[test]
fn piped_input_is_never_inferred_only_opt_in() {
    let f = Fixture::new();
    f.write("a.rs", "pub fn indexed_only() {}\n");

    // Stdin is a pipe carrying text that does NOT contain the pattern; without
    // --stdin the search must still come from the index and find the file.
    let out = piped(
        f.path(),
        &["search", "indexed_only"],
        "nothing to see here\n",
    );
    let text = String::from_utf8(out.stdout).unwrap();
    assert!(out.status.success(), "search must not fail on a stray pipe");
    assert!(
        text.contains("a.rs:1"),
        "must read the index, not stdin: {text}"
    );
    assert!(
        !text.contains("(stdin)"),
        "must not label rows stdin: {text}"
    );
}

#[test]
fn piped_input_searches_the_buffer_not_the_index() {
    let f = Fixture::new();
    f.write("a.rs", "pub fn from_the_index() {}\n");

    let out = piped(
        f.path(),
        &["search", "needle"],
        "no match on this line\nhere is a needle\n",
    );
    assert!(out.status.success());
    // Without --stdin the pipe is ignored entirely: nothing in the index matches.
    assert!(String::from_utf8(out.stdout).unwrap().is_empty());

    let out = piped(
        f.path(),
        &["search", "needle", "--stdin"],
        "no match on this line\nhere is a needle\n",
    );
    let text = String::from_utf8(out.stdout).unwrap();
    assert!(out.status.success());
    assert_eq!(text.lines().count(), 1, "one row per matching line: {text}");
    assert!(
        text.starts_with("(stdin):2:"),
        "located in the buffer: {text}"
    );
    assert!(
        !text.contains("from_the_index"),
        "index not consulted: {text}"
    );
}

/// What piped input buys over grep: with --language the buffer is parsed, so
/// matches carry their enclosing symbol and --in can separate code from prose.
#[test]
fn piped_input_with_language_annotates_and_filters() {
    let f = Fixture::new();
    let src = "// target lives in a comment\nfn holder() {\n    let s = \"target in a string\";\n    target();\n}\n";

    let annotated = String::from_utf8(
        piped(
            f.path(),
            &["search", "target", "--stdin", "--language", "rust"],
            src,
        )
        .stdout,
    )
    .unwrap();
    assert!(
        annotated.contains("[holder]"),
        "match annotated with its enclosing definition: {annotated}"
    );

    for (filter, want, unwanted) in [
        ("comments", "// target lives", "target()"),
        ("strings", "target in a string", "// target lives"),
        ("code", "target()", "// target lives"),
    ] {
        let out = String::from_utf8(
            piped(
                f.path(),
                &[
                    "search",
                    "target",
                    "--stdin",
                    "--language",
                    "rust",
                    "--in",
                    filter,
                ],
                src,
            )
            .stdout,
        )
        .unwrap();
        assert!(out.contains(want), "--in {filter} keeps {want}: {out}");
        assert!(
            !out.contains(unwanted),
            "--in {filter} drops {unwanted}: {out}"
        );
    }
}

/// `--in` needs spans, which need a language. Say so rather than silently
/// returning everything (or nothing).
#[test]
fn piped_input_rejects_in_filter_without_a_language() {
    let f = Fixture::new();
    let out = piped(f.path(), &["search", "x", "--stdin", "--in", "code"], "x\n");
    assert!(!out.status.success());
    let err = String::from_utf8(out.stderr).unwrap();
    assert!(err.contains("--language"), "actionable message: {err}");
}

/// Piped input needs no repository and reads no index, so codeq works as a
/// plain filter anywhere.
#[test]
fn piped_input_needs_no_repository() {
    let empty = TempDir::new().unwrap();
    let out = piped(empty.path(), &["search", "hit", "--stdin"], "a hit here\n");
    assert!(
        out.status.success(),
        "{}",
        String::from_utf8_lossy(&out.stderr)
    );
    assert!(String::from_utf8(out.stdout)
        .unwrap()
        .contains("(stdin):1:"));
    assert!(
        !empty.path().join(".codeq").exists(),
        "must not create an index for a piped scan"
    );
}

/// The other output modes work on a buffer too, so `--stdin` is a drop-in.
#[test]
fn piped_input_supports_count_and_context() {
    let f = Fixture::new();
    let input = "one\ntwo hit\nthree\nfour hit\n";

    let counted =
        String::from_utf8(piped(f.path(), &["search", "hit", "--stdin", "-c"], input).stdout)
            .unwrap();
    assert!(counted.contains("count=2"), "per-buffer count: {counted}");

    let ctx =
        String::from_utf8(piped(f.path(), &["search", "hit", "--stdin", "-C", "1"], input).stdout)
            .unwrap();
    assert!(ctx.contains("(stdin)-1- one"), "context before: {ctx}");
    assert!(ctx.contains("(stdin)-3- three"), "context after: {ctx}");
}

// ---------------------------------------------------------------------------
// members
// ---------------------------------------------------------------------------

#[test]
fn members_lists_fields_and_variants() {
    let f = Fixture::new();
    f.write(
        "a.rs",
        "pub struct S {\n    pub a: u32,\n    b: String,\n}\npub enum E { A, B(u32) }\n",
    );

    let mut fields = field_set(&f.run(&["members", "S"]), "name");
    fields.sort();
    assert_eq!(fields, ["a", "b"], "struct fields");

    let mut variants = field_set(&f.run(&["members", "E"]), "name");
    variants.sort();
    assert_eq!(variants, ["A", "B"], "enum variants");

    // Kinds are distinguishable, and a field carries its declaration.
    let rows = f.run(&["members", "S"]);
    let row = &rows["data"][0];
    assert_eq!(row["kind"], "field");
    assert_eq!(row["path"], "a.rs");
    assert!(
        row["signature"].as_str().unwrap().contains("pub a: u32"),
        "signature is the declaration: {row}"
    );
}

/// Containment is structural, so unlike the name-based graph commands a member
/// cannot be a false positive from a same-named symbol elsewhere, and a local
/// inside a method is not a member of the class.
#[test]
fn members_are_structural_not_name_based() {
    let f = Fixture::new();
    f.write(
        "a.py",
        "class C:\n    a: int = 1\n    def m(self):\n        local = 3\n",
    );
    // An unrelated class of the same name elsewhere, with different members.
    f.write("b.py", "class C:\n    zzz = 9\n");

    let scoped = field_set(&f.run(&["members", "C", "--path", "a.py"]), "name");
    let has = |v: &[String], n: &str| v.iter().any(|x| x == n);
    assert!(has(&scoped, "a"), "class attribute is a member: {scoped:?}");
    assert!(
        !has(&scoped, "local"),
        "a local inside a method is not a member of the class: {scoped:?}"
    );
    assert!(
        !has(&scoped, "zzz"),
        "--path scopes to one owner: {scoped:?}"
    );
    // Methods are members of the class body they sit in.
    assert!(has(&scoped, "m"), "method is a member: {scoped:?}");

    // Unscoped, both same-named classes contribute, each tagged with its owner.
    let both = f.run(&["members", "C"]);
    let names = field_set(&both, "name");
    assert!(has(&names, "a") && has(&names, "zzz"), "{names:?}");
    assert!(
        both["data"][0]["owner"].is_string(),
        "owner disambiguates when the name resolves to several types: {both}"
    );
}

#[test]
fn members_kind_filter_separates_fields_from_methods() {
    let f = Fixture::new();
    f.write(
        "a.py",
        "class C:\n    a = 1\n    def m(self):\n        pass\n",
    );
    assert_eq!(
        field_set(&f.run(&["members", "C", "--kind", "field"]), "name"),
        ["a"]
    );
    assert_eq!(
        field_set(&f.run(&["members", "C", "--kind", "method"]), "name"),
        ["m"],
        "a def inside a class is a method, not a function"
    );
}

/// `about` on a type answers "what is this and what's in it" in one call; on a
/// function the section is absent rather than empty.
#[test]
fn about_includes_members_only_when_there_are_some() {
    let f = Fixture::new();
    f.write("a.rs", "pub struct S { pub a: u32 }\npub fn go() {}\n");

    let about_type = f.run(&["about", "S"]);
    let members: Vec<&str> = about_type["data"]["members"]
        .as_array()
        .expect("members section present for a type")
        .iter()
        .map(|m| m["name"].as_str().unwrap())
        .collect();
    assert_eq!(members, ["a"]);

    let about_fn = f.run(&["about", "go"]);
    assert!(
        about_fn["data"].get("members").is_none(),
        "no empty section for a symbol with no members: {about_fn}"
    );
}

/// Members are declared syntax in many languages; each of these is verified
/// against a real parse tree, not documentation.
#[test]
fn members_extracted_across_languages() {
    let cases: &[(&str, &str, &str, &[&str])] = &[
        (
            "a.go",
            "type S struct { A int; b string }",
            "S",
            &["A", "b"],
        ),
        (
            "a.ts",
            "class C { a: number = 1; private b: string; }",
            "C",
            &["a", "b"],
        ),
        ("a.js", "class C { a = 1; #b = 2; }", "C", &["a", "#b"]),
        (
            "a.java",
            "class C { public int a; private String b; }",
            "C",
            &["a", "b"],
        ),
        ("a.c", "struct S { int a; char *b; };", "S", &["a", "b"]),
        (
            "a.cs",
            "class C { public int A; public string B {get;set;} }",
            "C",
            &["A", "B"],
        ),
        (
            "a.php",
            "<?php class C { public int $a; private $b; }",
            "C",
            &["a", "b"],
        ),
        (
            "a.kt",
            "class C(val a: Int) { var b: String = \"\" }",
            "C",
            &["a", "b"],
        ),
        (
            "a.swift",
            "struct S { var a: Int\nlet b: String }",
            "S",
            &["a", "b"],
        ),
        (
            "a.scala",
            "case class C(a: Int) { val c: Int = 1 }",
            "C",
            &["a", "c"],
        ),
        ("a.dart", "class C { int a = 1; }", "C", &["a"]),
        (
            "a.zig",
            "const P = struct { x: i32, y: i32 };",
            "P",
            &["x", "y"],
        ),
        ("a.sol", "contract C { uint a; }", "C", &["a"]),
    ];

    for (file, src, ty, want) in cases {
        let f = Fixture::new();
        f.write(file, src);
        let got = field_set(&f.run(&["members", ty]), "name");
        for w in *want {
            assert!(
                got.iter().any(|g| g == w),
                "{file}: expected member {w}, got {got:?}"
            );
        }
    }
}

/// Enum variants, where the language declares them.
#[test]
fn a_variant_inherits_the_visibility_of_its_enum() {
    // No language codeq supports lets a variant declare its own visibility, so
    // asking the node gave `private` every time: `api` reported a public enum's
    // surface as the bare enum name, and `unused` called its variants private.
    let cases: &[(&str, &str)] = &[
        ("a.rs", "pub enum Pub { A }\nenum Priv { B }\n"),
        ("a.java", "public enum Pub { A }\nenum Priv { B }\n"),
        ("a.cs", "public enum Pub { A }\nenum Priv { B }\n"),
    ];
    for (file, src) in cases {
        let f = Fixture::new();
        f.write(file, src);
        let api = field_set(&f.run(&["api", file]), "name");
        assert!(
            api.contains(&"A".to_string()),
            "{file}: public variant missing from api: {api:?}"
        );
        assert!(
            !api.contains(&"B".to_string()),
            "{file}: private variant leaked into api: {api:?}"
        );
    }
}

#[test]
fn variants_extracted_across_languages() {
    let cases: &[(&str, &str, &str, &[&str])] = &[
        ("a.rs", "enum E { A, B }", "E", &["A", "B"]),
        ("a.java", "enum E { X, Y }", "E", &["X", "Y"]),
        ("a.cs", "enum E { X }", "E", &["X"]),
        ("a.c", "enum E { X, Y };", "E", &["X", "Y"]),
        ("a.php", "<?php enum E { case X; }", "E", &["X"]),
        // Both TS forms. The initialised one was the only case here, and it
        // is the rarer one: `enum E { A, B }` parses to bare
        // `property_identifier`s rather than `enum_assignment`s, so it
        // yielded no members at all while this test stayed green.
        ("a.ts", "enum E { X = 1 }", "E", &["X"]),
        ("b.ts", "enum E { A, B }", "E", &["A", "B"]),
    ];
    for (file, src, ty, want) in cases {
        let f = Fixture::new();
        f.write(file, src);
        let got = field_set(&f.run(&["members", ty, "--kind", "variant"]), "name");
        for w in *want {
            assert!(
                got.iter().any(|g| g == w),
                "{file}: expected variant {w}, got {got:?}"
            );
        }
    }
}

/// A malformed extraction query would make every worker bail, leaving an index
/// that is empty and silent — indistinguishable from a repo with no code. The
/// extractor is validated up front so that surfaces as an error instead.
#[test]
fn indexing_a_normal_repo_yields_facts() {
    let f = Fixture::new();
    f.write("a.rs", "pub struct S { pub a: u32 }\n");
    let stats = f.run(&["map", "--totals"]);
    assert!(
        stats["data"]["totals"]["files"].as_i64().unwrap() > 0,
        "index must not come back silently empty: {stats}"
    );
}

#[test]
fn uppercase_extensions_are_parsed_as_their_language() {
    let f = Fixture::new();
    f.write(
        "Component.TSX",
        "export function Widget() { return <div />; }\n",
    );

    let names = field_set(&f.run(&["symbols", "--language", "tsx"]), "name");
    assert!(
        names.contains(&"Widget".to_string()),
        "uppercase extension was not parsed: {names:?}"
    );
}

// ---------------------------------------------------------------------------
// docs in text output
// ---------------------------------------------------------------------------

/// `--docs` used to populate the JSON and be a silent no-op in the default text
/// mode, so the flag looked broken unless you also passed `--json`. A flag that
/// changes nothing visible is worse than a missing one.
#[test]
fn docs_are_visible_in_text_output_not_just_json() {
    let f = Fixture::new();
    f.write(
        "a.rs",
        "/// Adds two numbers.\npub fn add(a: u32, b: u32) -> u32 { a + b }\n",
    );

    let plain = String::from_utf8(f.raw(&["outline", "a.rs"]).stdout).unwrap();
    assert!(plain.contains("pub fn add"), "signature shown: {plain}");
    assert!(
        !plain.contains("Adds two numbers"),
        "docs stay off by default: {plain}"
    );

    let with_docs = String::from_utf8(f.raw(&["outline", "a.rs", "--docs"]).stdout).unwrap();
    assert!(
        with_docs.contains("// Adds two numbers."),
        "--docs must change the text output, not only the JSON: {with_docs}"
    );
    assert_eq!(
        with_docs.lines().count(),
        plain.lines().count(),
        "a doc rides on its definition's row, one row per line: {with_docs}"
    );
}

/// The symbol-focused commands carry the doc, since "what does this do" is the
/// question they exist to answer.
#[test]
fn symbol_dossiers_show_the_doc() {
    let f = Fixture::new();
    f.write(
        "a.rs",
        "/// Adds two numbers.\npub fn add(a: u32, b: u32) -> u32 { a + b }\n",
    );

    for args in [
        vec!["definitions", "add"],
        vec!["about", "add"],
        vec!["explore", "add"],
    ] {
        let out = String::from_utf8(f.raw(&args).stdout).unwrap();
        assert!(
            out.contains("// Adds two numbers."),
            "{args:?} should surface the doc: {out}"
        );
    }
}

/// A field's doc comment is part of understanding the type.
#[test]
fn members_carry_their_doc_comments() {
    let f = Fixture::new();
    f.write(
        "a.rs",
        "pub struct S {\n    /// How many times.\n    pub count: u32,\n    pub other: u32,\n}\n",
    );
    let out = String::from_utf8(f.raw(&["members", "S"]).stdout).unwrap();
    assert!(out.contains("pub count: u32  // How many times."), "{out}");
    assert!(
        out.lines()
            .any(|l| l.contains("other") && !l.contains("//")),
        "an undocumented field carries no empty marker: {out}"
    );
}

// ---------------------------------------------------------------------------
// empty-result diagnosis
// ---------------------------------------------------------------------------

/// stderr of a run, for the diagnostic note (stdout stays pure data).
fn note_of(f: &Fixture, args: &[&str]) -> String {
    String::from_utf8(f.raw(args).stderr).unwrap()
}

/// An empty result is ambiguous between "no such symbol", "no such relation",
/// and "your filter removed everything". Each must say which it was.
#[test]
fn empty_results_explain_themselves() {
    let f = Fixture::new();
    f.write(
        "a.rs",
        "pub struct ScanOpts { pub limit: usize }\npub fn lonely() {}\n",
    );

    // 1. The name does not exist, and the near-miss is offered.
    let note = note_of(&f, &["definitions", "ScanOpt"]);
    assert!(note.contains("no symbol named 'ScanOpt'"), "{note}");
    assert!(
        note.contains("closest: ScanOpts"),
        "suggests the real name: {note}"
    );

    // 2. The name exists but has none of the relation asked for.
    let note = note_of(&f, &["callers", "lonely"]);
    assert!(
        note.contains("'lonely' is defined at a.rs:2") && note.contains("no callers"),
        "{note}"
    );

    // 3. A --path filter removed every definition.
    let note = note_of(&f, &["members", "ScanOpts", "--path", "nowhere"]);
    assert!(
        note.contains("nothing matches --path nowhere"),
        "the filter is the reason, not the symbol: {note}"
    );

    // 4. A --path filter that *matched* scopes the note to what it selected.
    //    Reporting the first definition repo-wide answered about a file the
    //    caller had ruled out, and read as though the scope had been ignored.
    let f2 = Fixture::new();
    f2.write("A.cs", "public class E { }\n");
    f2.write("z.ts", "export interface E { }\n");
    let note = note_of(&f2, &["callers", "E", "--path", "z.ts"]);
    assert!(
        note.contains("z.ts:1"),
        "note should name the scoped definition: {note}"
    );
    assert!(
        !note.contains("A.cs"),
        "note names a file the scope excluded: {note}"
    );
    assert!(
        !note.contains("more definitions"),
        "count should be scoped too: {note}"
    );

    // A name with no plausible neighbour gets no invented suggestion.
    let note = note_of(&f, &["definitions", "zzzzzzzzzz"]);
    assert!(note.contains("no symbol named"), "{note}");
    assert!(!note.contains("closest:"), "no far-fetched guess: {note}");
}

/// The note must never be mistaken for data, and never pollute a pipe.
#[test]
fn the_note_is_diagnostic_not_data() {
    let f = Fixture::new();
    f.write("a.rs", "pub fn lonely() {}\n");

    let out = f.raw(&["callers", "nope_xyz"]);
    assert!(out.status.success(), "an empty result is not an error");
    assert!(
        String::from_utf8(out.stdout).unwrap().is_empty(),
        "stdout stays pure data in text mode"
    );

    // JSON carries it in the envelope, because the MCP server captures only
    // stdout and the agent behind it is who needs to know.
    let v = f.run(&["callers", "nope_xyz"]);
    assert!(
        v["note"]
            .as_str()
            .unwrap()
            .contains("no symbol named 'nope_xyz'"),
        "{v}"
    );
    assert!(
        v["data"].as_array().unwrap().is_empty(),
        "data is still empty"
    );

    // A non-empty result carries no note at all.
    let v = f.run(&["definitions", "lonely"]);
    assert!(
        v.get("note").is_none(),
        "no note when there are results: {v}"
    );
}

/// `api` returning nothing almost always means "nothing here is public", which
/// is very different from "wrong path".
#[test]
fn api_explains_an_empty_surface() {
    let f = Fixture::new();
    f.write(
        "a.rs",
        "pub(crate) fn restricted() {}\nfn private_fn() {}\n",
    );

    let note = note_of(&f, &["api", "a.rs"]);
    assert!(note.contains("no public definitions"), "{note}");
    assert!(
        note.contains("restricted") && note.contains("private"),
        "breaks down what is there instead: {note}"
    );
}

/// Finding no dead code is a real answer, not a failure.
#[test]
fn unused_says_so_when_there_is_no_dead_code() {
    let f = Fixture::new();
    // Scoped to a.rs, whose only definition is called from b.rs. (Unscoped, the
    // caller itself would be reported: nothing calls the root.)
    f.write("a.rs", "pub fn used() {}\n");
    f.write("b.rs", "fn root() { used(); }\n");
    let note = note_of(&f, &["unused", "a.rs"]);
    assert!(
        note.contains("no unused definitions"),
        "an empty result here means clean, and should say so: {note}"
    );
}

/// The worst case: a typo gives valid-looking empty sections under real headers.
#[test]
fn about_and_explore_flag_a_misspelled_symbol() {
    let f = Fixture::new();
    f.write("a.rs", "pub struct ScanOpts { pub limit: usize }\n");
    for cmd in ["about", "explore"] {
        let note = note_of(&f, &[cmd, "ScanOpt"]);
        assert!(
            note.contains("no symbol named 'ScanOpt'") && note.contains("ScanOpts"),
            "{cmd}: empty sections must not read as a real answer: {note}"
        );
    }
}

// ---------------------------------------------------------------------------
// markdown
// ---------------------------------------------------------------------------

const DOC: &str = "\
# Title

Intro prose.

## Install

Run it.

```bash
# not a heading: a shell comment inside a fence
codeq index
```

### Details

More prose about installing.

## Usage

See [the guide](./guide.md) and ![logo](assets/logo.png).
Also [external](https://example.com) and [anchor](#usage).
";

#[test]
fn markdown_headings_are_definitions() {
    let f = Fixture::new();
    f.write("doc.md", DOC);

    let outline = f.run(&["outline", "doc.md"]);
    let names = field_set(&outline, "name");
    assert_eq!(
        names,
        ["Title", "Install", "Details", "Usage"],
        "in source order"
    );

    // Nesting comes from byte containment, as it does for every other language.
    let depths: Vec<i64> = outline["data"]
        .as_array()
        .unwrap()
        .iter()
        .map(|r| r["depth"].as_i64().unwrap())
        .collect();
    assert_eq!(
        depths,
        [0, 1, 2, 1],
        "### nests under ##, which nests under #"
    );

    // A `#` line inside a fenced block is code, not structure.
    assert!(
        !names.iter().any(|n| n.contains("not a heading")),
        "fenced content must not yield headings: {names:?}"
    );
}

#[test]
fn markdown_sections_are_readable_and_foldable() {
    let f = Fixture::new();
    f.write("doc.md", DOC);

    // One section verbatim, instead of ingesting the whole document.
    let section =
        String::from_utf8(f.raw(&["read", "doc.md", "--symbol", "Install"]).stdout).unwrap();
    assert!(
        section.contains("## Install") && section.contains("Run it."),
        "{section}"
    );
    assert!(
        section.contains("### Details"),
        "a section includes its subsections"
    );
    assert!(
        !section.contains("## Usage"),
        "and stops at the next peer: {section}"
    );

    // Folded, a document becomes its table of contents: every heading visible,
    // prose collapsed with a copy-pasteable line range.
    let folded = String::from_utf8(f.raw(&["read", "doc.md"]).stdout).unwrap();
    for h in ["# Title", "## Install", "### Details", "## Usage"] {
        assert!(folded.contains(h), "heading {h} stays visible: {folded}");
    }
    assert!(!folded.contains("Run it."), "section prose folds: {folded}");
    assert!(folded.contains("lines ("), "with a range marker: {folded}");
}

/// Fenced blocks are `strings` and prose is `comments`, so an agent can search
/// the documented examples separately from the surrounding text.
#[test]
fn markdown_separates_examples_from_prose() {
    let f = Fixture::new();
    f.write("doc.md", DOC);

    let in_code = String::from_utf8(
        f.raw(&["search", "codeq", "--path", "doc.md", "--in", "strings"])
            .stdout,
    )
    .unwrap();
    assert!(in_code.contains("codeq index"), "example found: {in_code}");

    let in_prose = String::from_utf8(
        f.raw(&["search", "prose", "--path", "doc.md", "--in", "comments"])
            .stdout,
    )
    .unwrap();
    assert!(in_prose.contains("Intro prose"), "prose found: {in_prose}");
    assert!(
        !in_prose.contains("codeq index"),
        "not the example: {in_prose}"
    );

    // Search hits carry the section they sit in, like code hits carry a function.
    let hit =
        String::from_utf8(f.raw(&["search", "More prose", "--path", "doc.md"]).stdout).unwrap();
    assert!(
        hit.contains("[Details]"),
        "annotated with its section: {hit}"
    );
}

/// A link or image with a local target is a file reference, so the dependency
/// commands cover documentation and its assets.
#[test]
fn markdown_local_links_and_images_are_file_references() {
    let f = Fixture::new();
    f.write("doc.md", DOC);
    f.write("guide.md", "# Guide\n");

    let imports = field_set(&f.run(&["imports", "doc.md"]), "module");
    assert!(
        imports.iter().any(|i| i == "./guide.md"),
        "link: {imports:?}"
    );
    assert!(
        imports.iter().any(|i| i == "assets/logo.png"),
        "image: {imports:?}"
    );
    assert!(
        !imports.iter().any(|i| i.contains("example.com")),
        "an external URL names no file: {imports:?}"
    );
    assert!(
        !imports.iter().any(|i| i.starts_with('#')),
        "a bare anchor names no file: {imports:?}"
    );

    // Resolved into the dependency graph like any other import.
    let deps = field_set(&f.run(&["deps", "doc.md"]), "path");
    assert!(deps.iter().any(|p| p == "guide.md"), "{deps:?}");
    assert!(
        field_set(&f.run(&["importers", "guide.md"]), "path")
            .iter()
            .any(|p| p == "doc.md"),
        "reverse direction works too"
    );
}

/// Nothing ever references a heading by name, so without an exclusion every
/// section in every document would be reported as dead code.
#[test]
fn unused_does_not_report_headings() {
    let f = Fixture::new();
    f.write("doc.md", DOC);
    f.write("a.rs", "fn orphan() {}\n");

    let kinds = field_set(&f.run(&["unused"]), "kind");
    assert!(
        !kinds.iter().any(|k| k == "heading"),
        "prose is not dead code: {kinds:?}"
    );
    assert!(
        kinds.iter().any(|k| k == "function"),
        "real dead code still found"
    );
}

/// A bare `README.md` used to match every README in the repo at once, merging
/// their structures into one nonsensical outline.
#[test]
fn file_scoped_commands_resolve_to_one_file() {
    let f = Fixture::new();
    f.write("README.md", "# Root\n\nRoot prose.\n");
    f.write("bench/README.md", "# Bench\n\nBench prose.\n");

    let names = field_set(&f.run(&["outline", "README.md"]), "name");
    assert_eq!(names, ["Root"], "the exact path wins: {names:?}");

    let at = field_set(&f.run(&["at", "README.md:3"]), "name");
    assert_eq!(at, ["Root"], "same resolution for at: {at:?}");
}

#[test]
fn markdown_setext_headings_and_frontmatter() {
    let f = Fixture::new();
    f.write(
        "doc.md",
        "---\ntitle: meta\n---\n\nOld Style\n=========\n\nBody text.\n\nSub\n---\n\nMore.\n\nParagraph.\n\n---\n\nAfter a rule.\n",
    );
    let names = field_set(&f.run(&["outline", "doc.md"]), "name");
    assert!(
        names.iter().any(|n| n == "Old Style"),
        "setext h1: {names:?}"
    );
    assert!(names.iter().any(|n| n == "Sub"), "setext h2: {names:?}");
    assert!(
        !names.iter().any(|n| n == "title: meta"),
        "frontmatter is metadata, not a heading: {names:?}"
    );
    assert!(
        !names.iter().any(|n| n == "Paragraph."),
        "a --- after a blank line is a thematic break, not an underline: {names:?}"
    );
}

// ---------------------------------------------------------------------------
// reStructuredText and Jinja templates
// ---------------------------------------------------------------------------

#[test]
fn rst_headings_and_local_doc_links_are_indexed() {
    let f = Fixture::new();
    f.write(
        "docs/index.rst",
        "Guide\n=====\n\nSee :doc:`install` and `API <api.rst>`_.\n\n.. toctree::\n   :maxdepth: 1\n\n   tutorial/index\n\nInstall\n-------\n\n.. image:: images/logo.svg\n",
    );
    f.write("docs/install.rst", "Install\n=======\n");
    f.write("docs/api.rst", "API\n===\n");
    f.write("docs/tutorial/index.rst", "Tutorial\n========\n");

    assert_eq!(
        field_set(&f.run(&["outline", "docs/index.rst"]), "name"),
        ["Guide", "Install"]
    );
    let imports = field_set(&f.run(&["imports", "docs/index.rst"]), "module");
    for target in [
        "install.rst",
        "api.rst",
        "tutorial/index.rst",
        "images/logo.svg",
    ] {
        assert!(
            imports.iter().any(|item| item == target),
            "missing {target}: {imports:?}"
        );
    }
    assert!(
        field_set(&f.run(&["deps", "docs/index.rst"]), "path")
            .iter()
            .any(|path| path == "docs/install.rst"),
        "RST doc role resolves as a local dependency"
    );
}

#[test]
fn jinja_templates_expose_blocks_macros_and_composition() {
    let f = Fixture::new();
    f.write(
        "templates/base.html",
        "{% block body %}base{% endblock %}\n",
    );
    f.write(
        "templates/page.html",
        "{% extends 'base.html' %}\n{% import 'macros.j2' as ui %}\n{% block body %}{% include 'partials/card.html' %}{% endblock %}\n{% macro greeting(name) %}Hi {{ name }}{% endmacro %}\n",
    );
    f.write(
        "templates/macros.j2",
        "{% macro button(text) %}{{ text }}{% endmacro %}",
    );
    f.write("templates/partials/card.html", "<article>card</article>");

    let outline = field_set(&f.run(&["outline", "templates/page.html"]), "name");
    assert!(
        outline.iter().any(|name| name == "body"),
        "block: {outline:?}"
    );
    assert!(
        outline.iter().any(|name| name == "greeting"),
        "macro: {outline:?}"
    );
    let unused = field_set(&f.run(&["unused", "templates", "--kind", "block"]), "name");
    assert!(
        !unused.contains(&"body".to_string()),
        "template blocks are runtime entry points: {unused:?}"
    );
    let imports = field_set(&f.run(&["imports", "templates/page.html"]), "module");
    for target in ["base.html", "macros.j2", "partials/card.html"] {
        assert!(
            imports.iter().any(|item| item == target),
            "missing {target}: {imports:?}"
        );
    }
    assert!(
        field_set(&f.run(&["deps", "templates/page.html"]), "path")
            .iter()
            .any(|path| path == "templates/base.html"),
        "extends resolves as a local dependency"
    );
}

#[test]
fn extension_bearing_doc_and_template_references_prefer_the_relative_file() {
    let f = Fixture::new();
    f.write("docs/index.rst", "See :doc:`guide`.\n");
    f.write("docs/guide.rst", "Guide\n=====\n");
    f.write("elsewhere/guide.rst", "Other\n=====\n");
    f.write("docs/tutorial/index.rst", "Tutorial\n========\n");
    f.write("elsewhere/tutorial/index.html", "other");
    f.write("templates/page.html", "{% extends 'base.html' %}\n");
    f.write("templates/base.html", "base");
    f.write("elsewhere/base.html", "other");
    f.write("docs/_static/flask-name.svg", "<svg />");

    let docs = field_set(&f.run(&["deps", "docs/index.rst"]), "path");
    assert_eq!(
        docs,
        ["docs/guide.rst"],
        "RST file reference stays relative: {docs:?}"
    );
    f.write("docs/index.rst", "See :doc:`tutorial/index`.\n");
    let nested = field_set(&f.run(&["deps", "docs/index.rst"]), "path");
    assert_eq!(
        nested,
        ["docs/tutorial/index.rst"],
        "RST index file stays relative: {nested:?}"
    );
    f.write("docs/index.rst", ".. image:: _static/flask-name.svg\n");
    let asset = field_set(&f.run(&["deps", "docs/index.rst"]), "path");
    assert_eq!(
        asset,
        ["docs/_static/flask-name.svg"],
        "hyphenated asset stays relative: {asset:?}"
    );
    f.write("docs/page.rst", "See :doc:`page`.\n");
    let self_deps = field_set(&f.run(&["deps", "docs/page.rst"]), "path");
    assert!(
        self_deps.is_empty(),
        "an explicit self-link is not a dependency: {self_deps:?}"
    );
    let templates = field_set(&f.run(&["deps", "templates/page.html"]), "path");
    assert_eq!(
        templates,
        ["templates/base.html"],
        "template inheritance stays relative: {templates:?}"
    );
}

#[test]
fn bare_node_package_imports_do_not_resolve_local_same_stem_files() {
    let f = Fixture::new();
    f.write(
        "app.js",
        "import express from 'express';\nexport const app = express();\n",
    );
    f.write(
        "express.js",
        "import { app } from './app.js';\nexport default app;\n",
    );

    let deps = field_set(&f.run(&["deps", "app.js"]), "path");
    assert!(
        deps.is_empty(),
        "a bare package import is external: {deps:?}"
    );
    let cycles = f.run(&["deps", "--cycles"])["data"]
        .as_array()
        .unwrap()
        .clone();
    assert!(
        cycles.is_empty(),
        "package name must not manufacture a local cycle: {cycles:?}"
    );
}

// ---------------------------------------------------------------------------
// comments and strings are not uses
// ---------------------------------------------------------------------------

/// A stale `// TODO: wire up foo later` used to count as a use and hide genuinely
/// dead code. A comment is never a use.
#[test]
fn a_comment_mention_no_longer_masks_dead_code() {
    let f = Fixture::new();
    f.write(
        "a.rs",
        "fn actually_dead() {}\n// TODO: wire up actually_dead later\nfn main() {}\n",
    );
    let names = field_set(&f.run(&["unused"]), "name");
    assert!(
        names.iter().any(|n| n == "actually_dead"),
        "a comment must not mask dead code: {names:?}"
    );
}

/// But a *string* mention can be a real use — reflection, `getattr`, serde
/// renames, DI registration, SQL identifiers, config keys — so it still counts.
/// `unused` under-reports rather than misleads, and deleting live code is the
/// expensive mistake.
#[test]
fn a_string_mention_still_counts_as_a_use() {
    let f = Fixture::new();
    f.write(
        "a.rs",
        "fn kept_by_reflection() {}\nfn main() { dispatch(\"kept_by_reflection\"); }\nfn dispatch(_: &str) {}\n",
    );
    let names = field_set(&f.run(&["unused"]), "name");
    assert!(
        !names.iter().any(|n| n == "kept_by_reflection"),
        "a string mention may be a real use: {names:?}"
    );
}

/// A file codeq cannot parse has no spans, so nothing in it is *known* to be a
/// comment. Those occurrences must still count, or the exclusion would report
/// live code as dead.
#[test]
fn occurrences_in_unparsed_files_still_count_as_uses() {
    let f = Fixture::new();
    f.write("a.rs", "fn referenced_from_config() {}\nfn main() {}\n");
    f.write("config.toml", "handler = \"referenced_from_config\"\n");
    let names = field_set(&f.run(&["unused"]), "name");
    assert!(
        !names.iter().any(|n| n == "referenced_from_config"),
        "an unclassifiable file must not lose its occurrences: {names:?}"
    );
}

/// `helper()` written inside a string or a comment is not a call.
#[test]
fn callees_ignores_calls_written_in_comments_and_strings() {
    let f = Fixture::new();
    f.write(
        "a.rs",
        "fn helper() {}\n\
         fn only_mentions() {\n    let s = \"helper() in a string\";\n    // helper() in a comment\n}\n\
         fn really_calls() { helper(); }\n",
    );

    let mentioned = field_set(&f.run(&["callees", "only_mentions"]), "name");
    assert!(
        mentioned.is_empty(),
        "a mention in prose is not a callee: {mentioned:?}"
    );

    let called = field_set(&f.run(&["callees", "really_calls"]), "name");
    assert_eq!(called, ["helper"], "a real call is still found");
}

/// `fn foo(` and `def foo(` are followed by `(` exactly as a call is, so the
/// byte-level test cannot tell a definition from a call. The definition sites
/// are known exactly, so they are dropped from the position filters.
#[test]
fn a_definition_is_not_a_call_site() {
    let f = Fixture::new();
    f.write(
        "a.rs",
        "fn parse_range(s: &str) -> u32 { 0 }\nfn user() { parse_range(\"x\"); }\n",
    );

    let calls = f.run(&["references", "parse_range", "--calls"]);
    let lines: Vec<i64> = calls["data"]["references"]
        .as_array()
        .unwrap()
        .iter()
        .map(|r| r["line"].as_i64().unwrap())
        .collect();
    assert_eq!(lines, [2], "only the call site, not the declaration");

    // Unfiltered, the definition is still reported and still flagged.
    let all = f.run(&["references", "parse_range"]);
    assert!(
        all["data"]["references"]
            .as_array()
            .unwrap()
            .iter()
            .any(|r| r["definition"] == true),
        "plain references still shows the definition: {all}"
    );
}

/// A wrapped method chain is the same call as a single-line one. Whitespace,
/// including newlines, must not decide whether `--on` finds it.
#[test]
fn on_receiver_survives_formatting() {
    let f = Fixture::new();
    f.write(
        "a.rs",
        "struct Db;\nimpl Db { fn query(&self) {} }\n\
         fn f(db: Db) {\n    db.query();\n    db\n        .query();\n    db . query();\n}\n",
    );
    let lines: Vec<i64> = f.run(&["references", "query", "--on", "db"])["data"]["references"]
        .as_array()
        .unwrap()
        .iter()
        .map(|r| r["line"].as_i64().unwrap())
        .collect();
    assert_eq!(lines, [4, 6, 7], "same call, three formattings: {lines:?}");
}

/// Skipping whitespace must not loosen the receiver match itself.
#[test]
fn on_receiver_still_rejects_the_wrong_receiver() {
    let f = Fixture::new();
    f.write(
        "a.rs",
        "struct Db;\nimpl Db { fn query(&self) {} }\n\
         fn g(db: Db, other: Db, mydb: Db) {\n    db.query();\n    other.query();\n    mydb.query();\n    db.inner.query();\n}\n",
    );
    let lines: Vec<i64> = f.run(&["references", "query", "--on", "db"])["data"]["references"]
        .as_array()
        .unwrap()
        .iter()
        .map(|r| r["line"].as_i64().unwrap())
        .collect();
    assert_eq!(
        lines,
        [4],
        "a different receiver, a longer name ending in it, and a nested field \
         must all be excluded: {lines:?}"
    );
}

// ---------------------------------------------------------------------------
// structural call sites
// ---------------------------------------------------------------------------

/// `--calls` and `--on` are answered from the parse tree, so the receiver is the
/// expression as written rather than whatever bytes happen to precede the name.
#[test]
fn call_filters_use_the_parse_tree() {
    let f = Fixture::new();
    f.write(
        "a.rs",
        "struct Db;\nimpl Db { fn query(&self) {} }\n\
         fn f(db: Db, other: Db, mydb: Db, a: Db) {\n\
         \x20   db.query();\n\
         \x20   db\n        .query();\n\
         \x20   other.query();\n\
         \x20   mydb.query();\n\
         \x20   a.inner.query();\n\
         \x20   conn().query();\n\
         \x20   let s = \"db.query() in a string\";\n\
         \x20   // db.query() in a comment\n}\n\
         fn conn() -> Db { Db }\nfn query(x: u32) {}\n",
    );

    let lines = |args: &[&str]| -> Vec<i64> {
        f.run(args)["data"]["references"]
            .as_array()
            .unwrap()
            .iter()
            .map(|r| r["line"].as_i64().unwrap())
            .collect()
    };

    // Every call, and only calls: not the two declarations, not the string, not
    // the comment.
    assert_eq!(
        lines(&["references", "query", "--calls"]),
        [4, 6, 7, 8, 9, 10],
        "call sites only"
    );

    // The receiver is `db` for exactly two of them; a longer name ending in
    // `db`, a different receiver, and a nested field are all distinct.
    assert_eq!(lines(&["references", "query", "--on", "db"]), [4, 6]);
    assert_eq!(lines(&["references", "query", "--on", "other"]), [7]);
    assert_eq!(lines(&["references", "query", "--on", "mydb"]), [8]);

    // Receivers text could not express before: a nested field access and a
    // call-valued receiver.
    assert_eq!(
        lines(&["references", "query", "--on", "a.inner"]),
        [9],
        "the receiver is the whole expression, not its last segment"
    );
    assert_eq!(
        lines(&["references", "query", "--on", "conn()"]),
        [10],
        "a call-valued receiver resolves"
    );
    assert!(
        lines(&["references", "query", "--on", "inner"]).is_empty(),
        "`inner` is not the receiver of a.inner.query()"
    );
}

#[test]
fn call_filters_refresh_cached_receivers_and_empty_call_maps() {
    let f = Fixture::new();
    f.write("a.py", "def query(): pass\ndef run():\n    db.query()\n");
    for _ in 0..2 {
        let refs = f.run(&["references", "query", "--on", "db"]);
        assert_eq!(refs["data"]["references"].as_array().unwrap().len(), 1);
        let callers = f.run(&["callers", "query", "--calls"]);
        assert_eq!(field_set(&callers, "name"), ["run"]);
    }

    f.write("a.py", "def query(): pass\ndef run():\n    other.query()\n");
    let old = f.run(&["references", "query", "--on", "db"]);
    assert!(old["data"]["references"].as_array().unwrap().is_empty());
    let new = f.run(&["references", "query", "--on", "other"]);
    assert_eq!(new["data"]["references"].as_array().unwrap().len(), 1);

    f.write("a.py", "def query(): pass\ndef run():\n    return query\n");
    for _ in 0..2 {
        let refs = f.run(&["references", "query", "--calls"]);
        assert!(refs["data"]["references"].as_array().unwrap().is_empty());
        let callers = f.run(&["callers", "query", "--calls"]);
        assert!(callers["data"].as_array().unwrap().is_empty());
    }
}

/// The same call written across the grammars codeq knows. Each is checked
/// against the real tree, not against documentation.
#[test]
fn call_sites_resolve_across_languages() {
    let cases: &[(&str, &str, &str)] = &[
        (
            "a.py",
            "def query(x): pass\ndb.query()\nother.query()\n",
            "db",
        ),
        (
            "a.js",
            "function query(x) {}\ndb.query();\nother.query();\n",
            "db",
        ),
        ("a.ts", "function query(x: number) {}\ndb.query();\n", "db"),
        ("a.go", "package m\nfunc f() {\n\tdb.query()\n}\n", "db"),
        (
            "A.java",
            "class T {\n  void query() {}\n  void f() {\n    db.query();\n  }\n}\n",
            "db",
        ),
        ("a.rb", "def query(x); end\ndb.query\n", "db"),
        (
            "a.c",
            "void query(int x) {}\nvoid f() {\n  db->query();\n}\n",
            "db",
        ),
        (
            "a.cs",
            "class T {\n  void query() {}\n  void f() {\n    db.query();\n  }\n}\n",
            "db",
        ),
        (
            "a.php",
            "<?php\nfunction query($x) {}\n$db->query();\n",
            "$db",
        ),
        ("a.kt", "fun query() {}\nfun f() {\n  db.query()\n}\n", "db"),
        (
            "a.swift",
            "func query() {}\nfunc f() {\n  db.query()\n}\n",
            "db",
        ),
        (
            "a.scala",
            "object M {\n  def query() = {}\n  def f() = {\n    db.query()\n  }\n}\n",
            "db",
        ),
        (
            "a.dart",
            "void query() {}\nvoid f() {\n  db.query();\n}\n",
            "db",
        ),
        (
            "a.zig",
            "fn query() void {}\nfn f() void {\n    db.query();\n}\n",
            "db",
        ),
        (
            "a.ex",
            "defmodule M do\n  def f do\n    db.query()\n  end\nend\n",
            "db",
        ),
        (
            "a.sol",
            "contract C {\n  function f() public {\n    db.query();\n  }\n}\n",
            "db",
        ),
        (
            "a.lua",
            "local function query() end\nlocal function f()\n  db.query()\nend\n",
            "db",
        ),
    ];

    for (file, src, recv) in cases {
        let f = Fixture::new();
        f.write(file, src);
        let hits = f.run(&["references", "query", "--on", recv])["data"]["references"]
            .as_array()
            .unwrap()
            .len();
        assert!(hits > 0, "{file}: --on {recv} found no call in:\n{src}");
    }
}

// ---------------------------------------------------------------------------
// deps --external
// ---------------------------------------------------------------------------

/// An import that resolves to no file in the repo is either a third-party
/// dependency (code) or a broken reference (a local path that should exist).
#[test]
fn external_deps_separate_third_party_from_broken_links() {
    let f = Fixture::new();
    f.write("other.md", "# Other\n");
    f.write("assets/real.png", "x");
    f.write(
        "doc.md",
        "# Doc\n\nSee [good](./other.md) and ![img](assets/real.png).\n\
         See [dead](./missing.md) and ![gone](assets/nope.png).\n\
         An example of the syntax: `[x](./not-a-link.md)`.\n\
         External [site](https://example.com) is fine.\n",
    );
    f.write("a.rs", "use serde::Serialize;\nfn f() {}\n");

    let rows = f.run(&["deps", "--external"]);
    let modules = field_set(&rows, "module");
    let broken: Vec<&str> = rows["data"]
        .as_array()
        .unwrap()
        .iter()
        .filter(|r| r["broken"] == true)
        .map(|r| r["module"].as_str().unwrap())
        .collect();

    assert_eq!(
        broken,
        ["./missing.md", "assets/nope.png"],
        "only the links that point at nothing: {modules:?}"
    );
    assert!(
        !modules
            .iter()
            .any(|m| m == "./other.md" || m == "assets/real.png"),
        "a link that resolves is not external: {modules:?}"
    );
    assert!(
        !modules.iter().any(|m| m.contains("example.com")),
        "an external URL names no file, so it is not an import at all: {modules:?}"
    );
    // A third-party import is external but is not a defect.
    assert!(
        rows["data"]
            .as_array()
            .unwrap()
            .iter()
            .any(|r| r["module"] == "serde::Serialize" && r["broken"] != true),
        "third-party imports are listed, unflagged: {modules:?}"
    );
}

/// Documentation *about* markdown is full of link syntax inside backticks. An
/// example is not a link. (Found by running `deps --external` on this repo, which
/// reported its own README as having two dead links.)
#[test]
fn a_link_inside_inline_code_is_an_example_not_a_link() {
    let f = Fixture::new();
    f.write(
        "doc.md",
        "# Doc\n\nLinks (`[g](./guide.md)`, `![logo](assets/logo.png)`) are file references.\n",
    );
    let modules = field_set(&f.run(&["imports", "doc.md"]), "module");
    assert!(
        modules.is_empty(),
        "inline code must not produce dependency edges: {modules:?}"
    );
}

// ---------------------------------------------------------------------------
// --root: reading a dependency
// ---------------------------------------------------------------------------

/// Dependency trees are gitignored, so their source is invisible to codeq: "how
/// do I call this library function" could not be answered at all. `--root` takes
/// a directory literally — no walking up to a Git toplevel, and no consulting
/// the parent repo's ignore rules — so a dependency can be read like any repo.
#[test]
fn root_makes_an_ignored_dependency_readable() {
    let f = Fixture::new();
    // `.ignore` rather than `.gitignore`: gitignore semantics need a real .git
    // directory, and both go through the same walker filters.
    f.write(".ignore", "node_modules/\n");
    f.write(
        "node_modules/lodash/chunk.js",
        "export function chunk(array, size) { return []; }\nexport function head(a) { return a[0]; }\n",
    );
    f.write(
        "src/app.js",
        "import { chunk } from \"lodash\";\nchunk([1, 2], 1);\n",
    );

    // The dependency stays out of the ordinary index: it is not this repo's code.
    let own = field_set(&f.run(&["definitions", "chunk"]), "path");
    assert!(
        !own.iter().any(|p| p.contains("node_modules")),
        "an ignored tree must not leak into the repo's own index: {own:?}"
    );

    // Pointed at directly, it indexes and answers like any other tree.
    let api = field_set(&f.run(&["--root", "node_modules/lodash", "api"]), "name");
    let mut api = api;
    api.sort();
    assert_eq!(api, ["chunk", "head"], "the dependency's public API");

    let src = String::from_utf8(
        f.raw(&[
            "--root",
            "node_modules/lodash",
            "read",
            "chunk.js",
            "--symbol",
            "chunk",
        ])
        .stdout,
    )
    .unwrap();
    assert!(src.contains("export function chunk"), "its source: {src}");
}

// ---------------------------------------------------------------------------
// language support, untested, api diff
// ---------------------------------------------------------------------------

/// codeq's support is uneven and always has been. Presenting flat lists made
/// "no results" and "not supported" look identical; the matrix makes the
/// difference queryable, and is derived from the queries so it cannot drift.
#[test]
fn capabilities_reports_per_language_support() {
    let f = Fixture::new();
    f.write("a.rs", "fn x() {}\n");
    let rows = f.run(&["capabilities"])["data"]["language_support"]
        .as_array()
        .unwrap()
        .clone();
    let get = |lang: &str| {
        rows.iter()
            .find(|r| r["language"] == lang)
            .unwrap_or_else(|| panic!("{lang} missing from the support matrix"))
            .clone()
    };

    // Known-true and known-false cases, each documented in DOGFOOD.
    assert_eq!(get("rust")["relations"], true);
    assert_eq!(get("go")["relations"], false, "Go is structurally typed");
    assert_eq!(get("c")["relations"], false, "C has no inheritance");
    assert_eq!(
        get("ruby")["members"],
        true,
        "attr_accessor, read via a guard"
    );
    assert_eq!(get("bash")["imports"], true, "source ./lib.sh");
    assert_eq!(get("zig")["relations"], false, "Zig has no inheritance");
    assert_eq!(get("elixir")["members"], false, "no field declarations");
    assert_eq!(get("markdown")["definitions"], true, "headings");
    assert_eq!(get("markdown")["calls"], false);
    for lang in ["svelte", "vue"] {
        assert_eq!(
            get(lang)["calls"],
            true,
            "{lang} embedded scripts have structural calls"
        );
    }
    for lang in ["rust", "python", "go", "markdown"] {
        assert_eq!(
            get(lang)["spans"],
            true,
            "{lang} classifies comments/strings"
        );
    }
}

/// An unsupported feature is not evidence of absence, and must not read as one.
#[test]
fn an_unsupported_feature_says_so_instead_of_none() {
    let f = Fixture::new();
    f.write("a.go", "package m\ntype Reader interface { Read() }\n");
    f.write("a.rs", "pub trait T {}\n");

    let go = String::from_utf8(f.raw(&["implementors", "Reader"]).stderr).unwrap();
    assert!(
        go.contains("no inheritance edges for go") && go.contains("not evidence"),
        "must distinguish unsupported from none: {go}"
    );

    let rust = String::from_utf8(f.raw(&["implementors", "T"]).stderr).unwrap();
    assert!(
        rust.contains("has no implementors"),
        "Rust supports relations, so this is a real none: {rust}"
    );
}

/// `api` composed with `tests`, which otherwise costs one call per public symbol.
#[test]
fn untested_reports_public_definitions_no_test_names() {
    let f = Fixture::new();
    f.write(
        "src/lib.rs",
        "pub fn covered() {}\npub fn bare() {}\nfn private_helper() {}\n",
    );
    f.write("tests/t.rs", "#[test]\nfn works() { covered(); }\n");

    let names = field_set(&f.run(&["untested"]), "name");
    assert!(
        names.iter().any(|n| n == "bare"),
        "uncovered public fn: {names:?}"
    );
    assert!(!names.iter().any(|n| n == "covered"), "covered: {names:?}");
    assert!(
        !names.iter().any(|n| n == "private_helper"),
        "private definitions are out of scope: {names:?}"
    );
    assert!(
        !names.iter().any(|n| n == "works"),
        "the test itself is not production code needing cover: {names:?}"
    );

    let empty_scope = f.run(&["untested", "missing"]);
    assert!(
        empty_scope["note"]
            .as_str()
            .unwrap()
            .contains("no eligible public definitions to check in 'missing'"),
        "an unmatched path cannot establish test coverage: {empty_scope}"
    );
}

#[test]
fn untested_checks_many_names_in_one_pass_exactly_as_one_at_a_time() {
    // Coverage is decided for a batch of names per pass over the files. The
    // batch must not change what counts: a name that only appears inside a
    // longer identifier is not covered, a name shorter than a trigram still is
    // when a test names it, and a test that is merely named like the candidate
    // is not a test *of* it.
    let f = Fixture::new();
    f.write(
        "src/lib.rs",
        "pub fn parse() {}
pub fn parse_all() {}
pub fn ab() {}
pub fn lonely() {}
pub fn echo() {}
",
    );
    f.write(
        "tests/t.rs",
        "#[test]
fn t_parse_all() { parse_all(); }
#[test]
fn t_short() { ab(); }
#[test]
fn echo() { let _ = 1; }
",
    );
    let names = field_set(&f.run(&["untested"]), "name");
    assert!(
        names.iter().any(|n| n == "parse"),
        "parse is only a prefix: {names:?}"
    );
    assert!(names.iter().any(|n| n == "lonely"), "{names:?}");
    assert!(!names.iter().any(|n| n == "parse_all"), "{names:?}");
    assert!(
        !names.iter().any(|n| n == "ab"),
        "short name missed: {names:?}"
    );
    assert!(
        names.iter().any(|n| n == "echo"),
        "a test named like the candidate does not cover it: {names:?}"
    );

    let limited = f.run(&["untested", "--limit", "1"]);
    assert_eq!(field_set(&limited, "name").len(), 1, "{limited}");
}

/// "Does this break anyone" is not answerable from a diff, which shows every
/// edited line. This reports only the exported declarations that moved.
#[test]
fn api_diff_reports_the_exported_surface_only() {
    let f = Fixture::new();
    f.write(
        "lib.rs",
        "pub fn kept() {}\npub fn doomed() {}\npub fn shape(a: u32) {}\nfn internal() {}\n",
    );
    f.git_init_commit();

    f.write(
        "lib.rs",
        "pub fn kept() {}\npub fn shape(a: u32, b: u32) {}\npub fn fresh() {}\nfn internal(x: u32) {}\n",
    );

    let d = f.run(&["api", "--compare-to", "HEAD"]);
    let names = |k: &str| -> Vec<String> {
        d["data"][k]
            .as_array()
            .unwrap()
            .iter()
            .map(|r| r["name"].as_str().unwrap().to_string())
            .collect()
    };
    assert_eq!(names("removed"), ["doomed"]);
    assert_eq!(names("added"), ["fresh"]);
    assert_eq!(
        names("changed"),
        ["shape"],
        "same name, different signature"
    );
    let changed = &d["data"]["changed"][0];
    assert!(
        changed["was"].as_str().unwrap().contains("a: u32)"),
        "the previous signature is shown: {changed}"
    );
    // An unexported change is not an API change, however much the diff shows.
    for k in ["removed", "added", "changed"] {
        assert!(
            !names(k).iter().any(|n| n == "internal"),
            "{k} leaked a private fn"
        );
    }
}

#[test]
fn api_diff_reports_public_exports_moved_by_file_rename() {
    let f = Fixture::new();
    f.write("old.rs", "pub fn moved() {}\n");
    f.git_init_commit();
    f.git(&["mv", "old.rs", "new.rs"]);

    let diff = f.run(&["api", "--compare-to", "HEAD"]);
    assert_eq!(diff["data"]["removed"][0]["name"], "moved", "{diff}");
    assert_eq!(diff["data"]["removed"][0]["path"], "old.rs", "{diff}");
    assert_eq!(diff["data"]["added"][0]["name"], "moved", "{diff}");
    assert_eq!(diff["data"]["added"][0]["path"], "new.rs", "{diff}");
}

#[test]
fn api_diff_batches_unusual_paths_additions_and_deletions() {
    let f = Fixture::new();
    #[cfg(not(windows))]
    let changed = "b line\n\t:ü.rs";
    #[cfg(windows)]
    let changed = "b line ü.rs";
    f.write(changed, "pub fn shape(a: u32) {}\n");
    f.write("c deleted.rs", "pub fn gone() {}\n");
    f.git_init_commit();
    #[cfg(not(windows))]
    let added = "a new\nfile.rs";
    #[cfg(windows)]
    let added = "a new file.rs";
    f.write(added, "pub fn fresh() {}\n");
    f.write(changed, "pub fn shape(a: u64) {}\n");
    f.remove("c deleted.rs");

    let diff = f.run(&["api", "--compare-to", "HEAD"]);
    assert_eq!(diff["data"]["added"].as_array().unwrap().len(), 1, "{diff}");
    assert_eq!(diff["data"]["added"][0]["name"], "fresh");
    assert_eq!(diff["data"]["removed"][0]["name"], "gone");
    assert_eq!(diff["data"]["changed"][0]["path"], changed);
    assert_eq!(diff["data"]["changed"][0]["name"], "shape");
    assert!(diff["data"]["changed"][0]["was"]
        .as_str()
        .unwrap()
        .contains("a: u32"));

    let scoped = f.run(&["api", "b line", "--compare-to", "HEAD"]);
    assert!(scoped["data"]["added"].as_array().unwrap().is_empty());
    assert!(scoped["data"]["removed"].as_array().unwrap().is_empty());
    assert_eq!(scoped["data"]["changed"][0]["path"], changed);
    assert!(!f
        .raw(&["api", "--compare-to", "no-such-reference"])
        .status
        .success());
}

#[test]
fn api_diff_reads_blobs_from_the_merge_base() {
    let f = Fixture::new();
    f.write("lib.rs", "pub fn shape(a: u32) {}\n");
    f.git_init_commit();
    f.git(&["checkout", "-qb", "other"]);
    f.write("lib.rs", "pub fn shape(a: u64) {}\n");
    f.git(&["commit", "-qam", "other branch"]);
    f.git(&["checkout", "-q", "main"]);
    f.write("lib.rs", "pub fn shape(a: u8) {}\n");
    let diff = f.run(&["api", "--compare-to", "other"]);
    assert_eq!(diff["data"]["changed"].as_array().unwrap().len(), 1);
    assert!(diff["data"]["changed"][0]["was"]
        .as_str()
        .unwrap()
        .contains("a: u32"));
}

#[test]
fn api_empty_scope_note_uses_literal_path_matching() {
    let f = Fixture::new();
    f.write("pkg/acb.rs", "fn private() {}\n");

    let api = f.run(&["api", "a_b"]);
    assert!(
        api["note"]
            .as_str()
            .unwrap()
            .contains("no definitions at all in 'a_b'"),
        "a wildcard-only diagnostic must not count pkg/acb.rs: {api}"
    );
}

// ---------------------------------------------------------------------------
// coverage gaps closed
// ---------------------------------------------------------------------------

/// Inheritance for the languages that have it but codeq did not read. Each
/// pattern was written against a dumped parse tree.
#[test]
fn relations_extracted_for_the_remaining_languages() {
    let cases: &[(&str, &str)] = &[
        ("a.scala", "class A extends B with C\n"),
        ("a.kt", "class A : B(), C\n"),
        ("a.swift", "class A: B, C {}\n"),
        ("a.dart", "class A extends B implements C {}\n"),
        ("a.sol", "contract A is B, C {}\n"),
    ];
    for (file, src) in cases {
        let f = Fixture::new();
        f.write(file, src);
        let subs = field_set(&f.run(&["implementors", "B"]), "name");
        assert_eq!(subs, ["A"], "{file}: A derives from B");
        let supers = field_set(&f.run(&["supertypes", "A"]), "name");
        assert!(supers.iter().any(|s| s == "B"), "{file}: {supers:?}");
        assert!(supers.iter().any(|s| s == "C"), "{file}: {supers:?}");
    }

    // Dart is the one that distinguishes the two, so it must not flatten them.
    let f = Fixture::new();
    f.write("a.dart", "class A extends B implements C {}\n");
    let rel = |name: &str| -> String {
        f.run(&["implementors", name])["data"][0]["relation"]
            .as_str()
            .unwrap()
            .to_string()
    };
    assert_eq!(rel("B"), "extends");
    assert_eq!(rel("C"), "implements");
}

/// Imports expressed as calls rather than syntax, which is why they were missed.
#[test]
fn call_style_imports_are_extracted() {
    let cases: &[(&str, &str, &str)] = &[
        ("a.zig", "const std = @import(\"std\");\n", "std"),
        ("a.lua", "local m = require(\"mod\")\n", "mod"),
        ("a.sh", "source ./lib.sh\n", "./lib.sh"),
        ("a.ex", "defmodule M do\n  alias Foo.Bar\nend\n", "Foo.Bar"),
    ];
    for (file, src, want) in cases {
        let f = Fixture::new();
        f.write(file, src);
        let mods = field_set(&f.run(&["imports"]), "module");
        assert!(mods.iter().any(|m| m == want), "{file}: {mods:?}");
    }
}

/// Elixir spells a definition and an import the same way — both are macro calls —
/// so the keyword has to be checked against what the match claims to be.
#[test]
fn elixir_tells_a_definition_from_an_import() {
    let f = Fixture::new();
    f.write(
        "a.ex",
        "defmodule M do\n  alias Foo.Bar\n  def run do\n    :ok\n  end\nend\n",
    );

    let mods = field_set(&f.run(&["imports"]), "module");
    assert_eq!(
        mods,
        ["Foo.Bar"],
        "defmodule defines, it does not import: {mods:?}"
    );
    let defs = field_set(&f.run(&["symbols"]), "name");
    assert!(
        defs.iter().any(|d| d == "M"),
        "the module is a definition: {defs:?}"
    );
    assert!(defs.iter().any(|d| d == "run"));
}

/// Ruby declares fields through a method call, so `attr_accessor` needed the same
/// guard mechanism as `require` — and the leading `:` is symbol syntax, not part
/// of the field's name.
#[test]
fn ruby_attr_accessors_are_fields() {
    let f = Fixture::new();
    f.write(
        "a.rb",
        "class C\n  attr_accessor :a, :b\n  attr_reader :c\n  def m; end\nend\n",
    );

    let mut fields = field_set(&f.run(&["members", "C", "--kind", "field"]), "name");
    fields.sort();
    assert_eq!(fields, ["a", "b", "c"], "no leading colons");
    // Requires still work; the two share the call shape.
    f.write("b.rb", "require 'json'\n");
    assert!(field_set(&f.run(&["imports"]), "module")
        .iter()
        .any(|m| m == "json"));
}

// ---------------------------------------------------------------------------
// method vs function
// ---------------------------------------------------------------------------

/// Several grammars use one node for both (Rust `function_item`, Python
/// `function_definition`), so every method used to be reported as a plain
/// function and `--kind method` was near-useless.
#[test]
fn functions_inside_a_type_are_methods() {
    let cases: &[(&str, &str)] = &[
        ("a.py", "class C:\n    def meth(self): pass\n"),
        ("a.kt", "class C { fun meth() {} }\n"),
        ("a.swift", "class C { func meth() {} }\n"),
        ("a.scala", "class C {\n  def meth() = {}\n}\n"),
        ("a.sol", "contract C {\n  function meth() public {}\n}\n"),
        (
            "a.zig",
            "const C = struct {\n  fn meth(self: C) void {}\n};\n",
        ),
        ("a.rs", "struct S;\nimpl S { fn meth(&self) {} }\n"),
    ];
    for (file, src) in cases {
        let f = Fixture::new();
        f.write(file, src);
        let kinds = field_set(&f.run(&["definitions", "meth"]), "kind");
        assert_eq!(kinds, ["method"], "{file}: {src}");
    }
}

/// Rust's `impl` holds methods *and* associated functions, and only a `self`
/// parameter says which can be called on a value.
#[test]
fn rust_associated_functions_are_not_methods() {
    let f = Fixture::new();
    f.write(
        "a.rs",
        "struct S;\nimpl S {\n  fn meth(&self) {}\n  fn assoc() {}\n}\nfn free() {}\n",
    );
    let kind = |n: &str| field_set(&f.run(&["definitions", n]), "kind").join(",");
    assert_eq!(kind("meth"), "method", "takes self, so s.meth()");
    assert_eq!(kind("assoc"), "function", "no self: called as S::assoc()");
    assert_eq!(kind("free"), "function");
}

/// A function nested inside a method is not itself a member of the type,
/// however deeply the class sits above it.
#[test]
fn a_function_nested_in_a_method_is_not_a_method() {
    let f = Fixture::new();
    f.write(
        "a.py",
        "class C:\n    def meth(self):\n        def inner():\n            pass\n",
    );
    assert_eq!(
        field_set(&f.run(&["definitions", "meth"]), "kind"),
        ["method"]
    );
    assert_eq!(
        field_set(&f.run(&["definitions", "inner"]), "kind"),
        ["function"],
        "nested, not a member"
    );
}

/// A trait method declaration has no body, so the pattern requiring one missed
/// it — yet it is the whole point of the trait.
#[test]
fn rust_trait_method_declarations_are_extracted() {
    let f = Fixture::new();
    f.write("a.rs", "pub trait T {\n    fn required(&self);\n}\n");
    let rows = f.run(&["definitions", "required"]);
    assert_eq!(field_set(&rows, "kind"), ["method"]);
    assert_eq!(
        field_set(&f.run(&["members", "T", "--kind", "method"]), "name"),
        ["required"],
        "and it is a member of the trait"
    );
}

/// A receiver disambiguates which same-named definition a call means, but codeq
/// shows it rather than filtering on it: a receiver can be a value (`db.query()`)
/// or a module path (`helpers::stem()`), and telling those apart needs name
/// resolution codeq does not do. Pruning "a call with a receiver is not calling a
/// free function" would discard the module case, which is a real call.
#[test]
fn call_sites_report_the_receiver_they_are_called_on() {
    let f = Fixture::new();
    f.write(
        "a.rs",
        "mod helpers { pub fn stem(s: &str) -> &str { s } }\n\
         struct Db;\n\
         impl Db { fn stem(&self) -> u32 { 0 } }\n\
         fn use_them(db: Db) {\n    let a = helpers::stem(\"x\");\n    let b = db.stem();\n}\n",
    );

    // Both kinds exist, which is exactly when the receiver matters.
    let mut kinds = field_set(&f.run(&["definitions", "stem"]), "kind");
    kinds.sort();
    assert_eq!(kinds, ["function", "method"]);

    let rows = f.run(&["references", "stem", "--calls"]);
    let by_line = |line: i64| -> String {
        rows["data"]["references"]
            .as_array()
            .unwrap()
            .iter()
            .find(|r| r["line"] == line)
            .and_then(|r| r["receiver"].as_str())
            .unwrap_or("")
            .to_string()
    };
    assert_eq!(by_line(5), "helpers", "a module path is a receiver too");
    assert_eq!(by_line(6), "db", "and so is a value");

    // Visible in text mode, not just JSON.
    let text = String::from_utf8(f.raw(&["references", "stem", "--calls"]).stdout).unwrap();
    assert!(
        text.contains("on=helpers") && text.contains("on=db"),
        "{text}"
    );

    // An unfiltered query parses nothing, so it reports no receiver.
    let plain = f.run(&["references", "stem"]);
    assert!(
        plain["data"]["references"]
            .as_array()
            .unwrap()
            .iter()
            .all(|r| r.get("receiver").is_none()),
        "no receiver without a position filter: {plain}"
    );
}

#[test]
fn react_wrapped_components_are_definitions() {
    // The dominant way a React component is written is a module-scope binding
    // whose value is a *call* — `memo(…)`, `forwardRef(…)`, `styled.div`. None of
    // those is a function-valued declarator, so every one of them was invisible
    // to `symbols`, `outline` and `api` while the file looked fully indexed.
    let f = Fixture::new();
    f.write(
        "a.tsx",
        "import { memo, forwardRef } from 'react';\n\
         export const Card = memo(function Card({ id }: { id: string }) {\n\
           return <div>{id}</div>;\n\
         });\n\
         export const Input = forwardRef<HTMLInputElement, {}>((p, ref) => (\n\
           <input ref={ref} />\n\
         ));\n\
         export const Wrapper = styled.div`color: red;`;\n\
         export const Ctx = React.createContext(null);\n\
         export const VERSION = '1.0';\n",
    );
    let names = field_set(&f.run(&["symbols", "--path", "a.tsx"]), "name");
    for n in ["Card", "Input", "Wrapper", "Ctx", "VERSION"] {
        assert!(
            names.iter().any(|x| x == n),
            "tsx binding {n} missing: {names:?}"
        );
    }
    // All five are exported, so the public API must list them too.
    let api = field_set(&f.run(&["api", "a.tsx"]), "name");
    for n in ["Card", "Input", "Wrapper", "Ctx", "VERSION"] {
        assert!(
            api.iter().any(|x| x == n),
            "exported {n} missing from api: {api:?}"
        );
    }
}

#[test]
fn module_scope_bindings_do_not_duplicate_function_definitions() {
    // `const f = () => {}` matches both the function pattern and the new
    // module-scope binding pattern; only the function capture should survive, or
    // every arrow component would be reported twice.
    let f = Fixture::new();
    f.write(
        "a.ts",
        "export const go = () => 1;\nconst old = function () { return 2; };\n",
    );
    for name in ["go", "old"] {
        let rows = f.run(&["definitions", name]);
        assert_eq!(
            rows["data"].as_array().unwrap().len(),
            1,
            "'{name}' must be defined exactly once, got {rows}"
        );
    }
}

#[test]
fn commonjs_require_bindings_are_imports_not_definitions() {
    // CommonJS writes every dependency as a module-scope binding, and those are
    // already import facts. Counted as definitions too they buried each file's
    // outline under its own import block — 424 phantom definitions in Express
    // alone — so the binding pattern skips a value rooted in `require`.
    let f = Fixture::new();
    f.write(
        "a.js",
        "var http = require('http');\n\
         var Buffer = require('safe-buffer').Buffer;\n\
         var deprecate = require('depd')('express');\n\
         var real = compute();\n\
         function handler() {}\n",
    );
    let names = field_set(&f.run(&["symbols", "--path", "a.js"]), "name");
    for n in ["http", "Buffer", "deprecate"] {
        assert!(
            !names.iter().any(|x| x == n),
            "require binding '{n}' leaked: {names:?}"
        );
    }
    // A non-require binding in the same file must survive, or the exclusion is
    // just suppressing the whole pattern.
    for n in ["real", "handler"] {
        assert!(names.iter().any(|x| x == n), "'{n}' missing: {names:?}");
    }
    // And they must still be reachable as imports.
    let mods = field_set(&f.run(&["imports", "a.js"]), "module");
    for m in ["http", "safe-buffer", "depd"] {
        assert!(
            mods.iter().any(|x| x == m),
            "import '{m}' missing: {mods:?}"
        );
    }
}

#[test]
fn locals_are_not_definitions() {
    // The binding pattern is anchored to module scope on purpose: capturing every
    // declarator would bury the index in loop counters.
    let f = Fixture::new();
    f.write(
        "a.ts",
        "export function run() {\n  const tmp = compute();\n  return tmp;\n}\n",
    );
    let names = field_set(&f.run(&["symbols", "--path", "a.ts"]), "name");
    assert!(names.iter().any(|x| x == "run"), "run missing: {names:?}");
    assert!(
        !names.iter().any(|x| x == "tmp"),
        "function-local binding leaked: {names:?}"
    );
}

#[test]
fn namespaced_base_class_yields_a_relation() {
    // `class X extends React.Component` — the shape every React class component
    // is written in. The supertype is a member expression, not an identifier, so
    // the class came back with no supertypes at all.
    let f = Fixture::new();
    f.write(
        "a.tsx",
        "class Legacy extends React.Component<{}> {}\nclass Plain extends Base {}\n",
    );
    let supers = field_set(&f.run(&["supertypes", "Legacy"]), "name");
    assert!(
        supers.iter().any(|s| s == "React.Component"),
        "namespaced supertype missing: {supers:?}"
    );
    // Stored dotted, but nobody asks for it that way: the bare last segment must
    // find it, and so must the full path.
    for query in ["Component", "React.Component"] {
        let subs = field_set(&f.run(&["implementors", query]), "name");
        assert!(
            subs.iter().any(|s| s == "Legacy"),
            "implementors {query} missed Legacy: {subs:?}"
        );
    }
    // A bare supertype must not start matching everything as a side effect.
    let subs = field_set(&f.run(&["implementors", "Base"]), "name");
    assert_eq!(
        subs.len(),
        1,
        "Base gained unrelated implementors: {subs:?}"
    );
}

#[test]
fn capabilities_lists_every_extractable_language() {
    // `capabilities` is what codeq tells agents to consult before believing an
    // empty result, so a language missing from it is worse than an empty answer.
    let f = Fixture::new();
    f.write("a.rs", "fn main() {}\n");
    let v = f.run(&["capabilities"]);
    let langs: Vec<String> = v["data"]["language_support"]
        .as_array()
        .expect("language_support")
        .iter()
        .map(|e| e["language"].as_str().unwrap().to_string())
        .collect();
    for l in [
        "svelte",
        "vue",
        "css",
        "hcl",
        "powershell",
        "proto",
        "graphql",
        "sql",
    ] {
        assert!(
            langs.iter().any(|x| x == l),
            "'{l}' extracts but is undocumented: {langs:?}"
        );
    }
    for l in ["json", "yaml", "toml"] {
        assert!(langs.iter().any(|x| x == l));
    }
    // The three language lists are three views of one fact and used to be three
    // hand-kept copies. Every extraction language must be classifiable, and
    // classified = extraction + data-only exactly.
    let names = |k: &str| field_names(&v["data"][k]);
    let (extraction, classified, data_only) = (
        names("extraction_languages"),
        names("classified_languages"),
        names("data_only_languages"),
    );
    assert_eq!(
        extraction, langs,
        "extraction_languages must match language_support"
    );
    assert_eq!(
        classified.len(),
        extraction.len() + data_only.len(),
        "classified must be exactly extraction + data-only: {classified:?}"
    );
    for l in extraction.iter().chain(data_only.iter()) {
        assert!(classified.contains(l), "'{l}' is not classifiable at all");
    }
}

#[test]
fn group_hoists_the_path_and_keeps_the_row() {
    // Flat output stays the default because every line is self-describing: an
    // agent that greps one line out of a result must still know its file.
    // --group trades that away deliberately, so it has to be asked for.
    let f = Fixture::new();
    f.write("a.rs", "fn one() { target(); }\nfn two() { target(); }\n");
    f.write("b.rs", "fn three() { target(); }\n");

    let flat = String::from_utf8(f.raw(&["search", "target"]).stdout).unwrap();
    for line in flat.lines() {
        assert!(
            line.starts_with("a.rs:") || line.starts_with("b.rs:"),
            "flat row lost its path: {line}"
        );
    }

    let grouped = String::from_utf8(f.raw(&["search", "target", "--group"]).stdout).unwrap();
    // Header carries the path and the group size; a grouped row is the flat row
    // minus its path, so grouped output is never less precise than flat, only
    // less repetitive.
    assert!(
        grouped.contains("a.rs (2)\n"),
        "missing counted header: {grouped}"
    );
    assert!(
        grouped.contains("b.rs (1)\n"),
        "missing counted header: {grouped}"
    );
    assert!(
        !grouped.contains("a.rs:1:"),
        "grouped row still repeats the path: {grouped}"
    );
    for line in grouped.lines().filter(|l| !l.contains(".rs (")) {
        assert!(
            line.split(':')
                .next()
                .unwrap()
                .trim()
                .parse::<u32>()
                .is_ok(),
            "grouped row should start with a line number: {line}"
        );
        assert!(
            flat.lines().any(|f| f.ends_with(&format!(":{line}"))),
            "grouped row is not a flat row minus its path: {line}"
        );
    }
    // Which holds for the column too, when it is asked for.
    let fields = ["--fields", "path,line,col,text"];
    let grouped = f.raw(&[&["search", "target", "--group"][..], &fields].concat());
    assert!(
        String::from_utf8(grouped.stdout)
            .unwrap()
            .contains("\n1:11: fn one()"),
        "grouped row dropped a requested col"
    );
}

#[test]
fn group_does_not_change_json() {
    // --group is a text renderer concern. If it leaked into JSON, every
    // programmatic consumer would have to care about a display flag.
    let f = Fixture::new();
    f.write("a.rs", "fn one() { target(); }\nfn two() { target(); }\n");
    let plain = f.run(&["search", "target"]);
    let grouped = f.run(&["search", "target", "--group"]);
    assert_eq!(plain["data"], grouped["data"]);
}

#[test]
fn explore_at_a_position_resolves_the_enclosing_definition() {
    // A stack frame or diff hunk names a line, not a symbol. --at closes that
    // gap so it takes one call rather than `at` followed by `explore`.
    let f = Fixture::new();
    f.write(
        "a.rs",
        "fn helper() -> u32 { 1 }\n\nfn outer() -> u32 {\n    let v = helper();\n    v\n}\n",
    );
    let by_pos = f.run(&["explore", "--at", "a.rs:4"]);
    let focus = by_pos["data"]["focus"].as_array().unwrap();
    assert_eq!(
        focus.len(),
        1,
        "expected one enclosing definition: {focus:?}"
    );
    assert_eq!(focus[0]["name"], "outer");

    // The position form must agree with the name form, or they are two answers
    // to one question.
    let by_name = f.run(&["explore", "outer"]);
    assert_eq!(by_pos["data"]["focus"], by_name["data"]["focus"]);
}

#[test]
fn explore_at_picks_the_innermost_definition() {
    // A line inside a method sits inside its type too; the useful answer is the
    // method, so smallest enclosing range wins.
    let f = Fixture::new();
    f.write(
        "a.rs",
        "struct S;\n\nimpl S {\n    fn inner(&self) -> u32 {\n        7\n    }\n}\n",
    );
    let v = f.run(&["explore", "--at", "a.rs:5"]);
    assert_eq!(v["data"]["focus"][0]["name"], "inner");
}

#[test]
fn explore_at_keeps_same_named_methods_as_exact_focuses() {
    let f = Fixture::new();
    f.write(
        "sample.py",
        "class A:\n    def run(self):\n        return helper_a()\n\nclass B:\n    def run(self):\n        return helper_b()\n\ndef helper_a():\n    return 'A'\n\ndef helper_b():\n    return 'B'\n",
    );

    for (line, expected) in [(2, "'A'"), (6, "'B'")] {
        let result = f.run(&["explore", "--at", &format!("sample.py:{line}")]);
        let data = &result["data"];
        assert_eq!(data["focus"].as_array().unwrap().len(), 1, "{data}");
        assert_eq!(data["focus"][0]["name"], "run", "{data}");
        assert_eq!(data["focus"][0]["path"], "sample.py", "{data}");
        assert!(data["focus"][0]["byte"].as_u64().is_some(), "{data}");
        assert!(data["focus"][0]["end_byte"].as_u64().is_some(), "{data}");
        let expected_helper = if expected == "'A'" {
            "helper_a"
        } else {
            "helper_b"
        };
        assert_eq!(data["callees"].as_array().unwrap().len(), 1, "{data}");
        assert_eq!(data["callees"][0]["name"], expected_helper, "{data}");
        assert!(data["callees"][0]["byte"].as_u64().is_some(), "{data}");
        assert!(data["callees"][0]["end_byte"].as_u64().is_some(), "{data}");
        assert!(
            data["source"].as_array().unwrap().iter().any(|chunk| {
                chunk["reason"] == "focus"
                    && chunk["text"]
                        .as_str()
                        .unwrap_or_default()
                        .contains(expected_helper)
            }),
            "expected exact method source containing {expected_helper}: {data}"
        );
    }
}

#[test]
fn explore_at_a_header_line_resolves_to_the_type_not_its_constructor_parameter() {
    // A Kotlin primary-constructor parameter is a `field` owned by the class and
    // sits on the class's own line. It is a leaf by containment, so the line
    // resolved to `message` — and `explore` then pulled in every other `message`
    // in the file. Found by the differential harness on okio and os-lib.
    let f = Fixture::new();
    f.write(
        "e.kt",
        "class Other {\n    val message: String = \"\"\n}\n\
         class Boom(message: String) : Exception(message)\n\
         class Wide(val a: Int, val b: Int) {\n    fun run() {}\n}\n",
    );

    // `at` is unchanged: it lists everything covering the line, innermost first.
    let at = f.run(&["at", "e.kt:4"]);
    let names: Vec<_> = at["data"]
        .as_array()
        .unwrap()
        .iter()
        .map(|d| d["name"].as_str().unwrap().to_string())
        .collect();
    assert_eq!(names, ["message", "Boom"], "{at}");

    // `explore --at` answers about the class, and only the class: not `message`
    // and not the unrelated `message` field of `Other`.
    let v = f.run(&["explore", "--at", "e.kt:4", "--max-tokens", "400"]);
    let focus = v["data"]["focus"].as_array().unwrap();
    assert_eq!(focus.len(), 1, "{v}");
    assert_eq!(focus[0]["name"], "Boom", "{v}");

    // Two header parameters used to tie as leaves and refuse; the line is the
    // class's, so it resolves.
    let v = f.run(&["explore", "--at", "e.kt:5", "--max-tokens", "400"]);
    let focus = v["data"]["focus"].as_array().unwrap();
    assert_eq!(focus.len(), 1, "{v}");
    assert_eq!(focus[0]["name"], "Wide", "{v}");

    // A member in the body is still the innermost definition on its own line.
    let v = f.run(&["explore", "--at", "e.kt:2", "--max-tokens", "400"]);
    assert_eq!(v["data"]["focus"][0]["name"], "message", "{v}");
    assert_eq!(v["data"]["focus"][0]["line"], 2, "{v}");
}

#[test]
fn explore_at_a_position_with_no_definition_says_so() {
    // A blank line between definitions is a real empty answer, not a bad
    // argument, so it gets a note rather than an error.
    let f = Fixture::new();
    f.write("a.rs", "\n\nfn only() {}\n");
    let v = f.run(&["explore", "--at", "a.rs:1"]);
    assert!(v["data"]["focus"].as_array().unwrap().is_empty());
    let note = v["note"].as_str().unwrap_or_default();
    assert!(
        note.contains("no definition encloses"),
        "unhelpful note: {note}"
    );
}

#[test]
fn explore_reports_the_focus_files_imports() {
    // What is in scope at the edit site: without it an agent proposes a call to
    // something the file cannot see.
    let f = Fixture::new();
    f.write(
        "a.rs",
        "use std::collections::HashMap;\nuse std::fmt::Debug;\n\nfn target() -> u32 { 1 }\n",
    );
    let v = f.run(&["explore", "target"]);
    let mods: Vec<String> = v["data"]["imports"]
        .as_array()
        .unwrap()
        .iter()
        .filter_map(|r| r["module"].as_str().map(str::to_string))
        .collect();
    assert!(
        mods.iter().any(|m| m.contains("HashMap")),
        "focus file imports missing: {mods:?}"
    );
}

#[test]
fn context_expand_resolved_drops_unreachable_same_named_definitions() {
    // A direct named import is stronger evidence than a bare name: expansion can
    // select helper.rs even before --resolved adds its reachability filter.
    let f = Fixture::new();
    f.write(
        "seed.rs",
        "mod helper;\nuse helper::build;\n\nfn seed() -> u32 {\n    build()\n}\n",
    );
    f.write("helper.rs", "pub fn build() -> u32 { 1 }\n");
    f.write("unrelated.rs", "pub fn build() -> u32 { 999 }\n");

    let files = |v: &Value| -> Vec<String> {
        v["data"]["chunks"]
            .as_array()
            .unwrap()
            .iter()
            .filter_map(|c| c["path"].as_str().map(str::to_string))
            .collect()
    };
    let wide = f.run(&["context", "--symbols", "seed", "--expand", "--no-text"]);
    assert!(
        files(&wide).iter().any(|p| p == "helper.rs"),
        "named import should select helper.rs: {:?}",
        files(&wide)
    );
    assert!(
        !files(&wide).iter().any(|p| p == "unrelated.rs"),
        "named import should not include unrelated.rs: {:?}",
        files(&wide)
    );
    let narrow = f.run(&[
        "context",
        "--symbols",
        "seed",
        "--expand",
        "--resolved",
        "--no-text",
    ]);
    assert!(
        !files(&narrow).iter().any(|p| p == "unrelated.rs"),
        "--resolved kept an unreachable definition: {:?}",
        files(&narrow)
    );
}

#[test]
fn context_resolved_requires_expand() {
    // On its own it would silently do nothing, which reads as a broken flag.
    let f = Fixture::new();
    f.write("a.rs", "fn seed() {}\n");
    let out = f.raw(&["context", "--symbols", "seed", "--resolved"]);
    assert!(!out.status.success(), "--resolved alone should be rejected");
}

#[test]
fn max_per_file_caps_rows_and_never_hides_the_true_count() {
    // The lossy half of grouping. A silent cap would recreate the --limit 100
    // trap, where an agent reads a truncated answer as a complete one, so every
    // capped file has to report what it withheld.
    let f = Fixture::new();
    f.write(
        "a.rs",
        "fn a1() { t(); }\nfn a2() { t(); }\nfn a3() { t(); }\nfn a4() { t(); }\n",
    );
    f.write("b.rs", "fn b1() { t(); }\n");

    let v = f.run(&["search", "t()", "--max-per-file", "2"]);
    let rows = v["data"]["matches"].as_array().unwrap();
    let a_rows = rows.iter().filter(|r| r["path"] == "a.rs").count();
    assert_eq!(a_rows, 2, "cap not applied: {rows:?}");
    assert!(
        rows.iter().any(|r| r["path"] == "b.rs"),
        "uncapped file dropped"
    );
    assert_eq!(
        v["notes"]["truncated_per_file"]["a.rs"], 4,
        "true count missing: {:?}",
        v["notes"]
    );
    assert!(
        v["notes"]["truncated_per_file"].get("b.rs").is_none(),
        "an uncapped file must not be reported as truncated"
    );
}

#[test]
fn max_per_file_changes_json_and_text_alike() {
    // Capping in the renderer alone would have been simpler, but then --json and
    // the default text output would disagree about which rows the answer has.
    let f = Fixture::new();
    f.write(
        "a.rs",
        "fn a1() { t(); }\nfn a2() { t(); }\nfn a3() { t(); }\n",
    );
    let v = f.run(&["search", "t()", "--max-per-file", "1"]);
    assert_eq!(v["data"]["matches"].as_array().unwrap().len(), 1);

    let text = String::from_utf8(f.raw(&["search", "t()", "--max-per-file", "1"]).stdout).unwrap();
    let hits = text
        .lines()
        .filter(|l| l.contains("t()") && !l.contains("more"))
        .count();
    assert_eq!(
        hits, 1,
        "text kept a different number of rows than JSON: {text}"
    );
}

#[test]
fn a_capped_file_is_marked_in_both_flat_and_grouped_text() {
    let f = Fixture::new();
    f.write(
        "a.rs",
        "fn a1() { t(); }\nfn a2() { t(); }\nfn a3() { t(); }\n",
    );

    // Flat rows have no header to carry the count, so the marker is its own
    // line — led by the path, so it stays self-describing like every other row.
    let flat = String::from_utf8(f.raw(&["search", "t()", "--max-per-file", "1"]).stdout).unwrap();
    assert!(
        flat.contains("a.rs: +2 more (of 3)"),
        "missing flat marker: {flat}"
    );

    // Grouped output folds it into the header instead of spending a line.
    let grouped = String::from_utf8(
        f.raw(&["search", "t()", "--max-per-file", "1", "--group"])
            .stdout,
    )
    .unwrap();
    assert!(
        grouped.contains("a.rs (1 of 3)"),
        "missing grouped count: {grouped}"
    );
}

#[test]
fn max_per_file_leaves_pathless_rows_alone() {
    // The cap is a per-file idea; a row belonging to no file can't be over quota.
    let f = Fixture::new();
    f.write("a.rs", "fn one() {}\nfn two() {}\nfn three() {}\n");
    let v = f.run(&["map", "--max-per-file", "1"]);
    assert!(
        v["data"]["definitions_by_file"].as_array().is_some(),
        "map lost its shape under the cap: {:?}",
        v["data"]
    );
}

#[test]
fn a_result_stopped_by_limit_says_so_in_text() {
    // The bug this guards: `truncated: true` was in --json all along, but the
    // text renderer drops scalars sitting next to a single row array -- so the
    // default output of the most-used command showed 100 of 4000 hits and read
    // exactly like a complete answer. Every other truncation in codeq announces
    // itself; this one did not.
    let f = Fixture::new();
    let body: String = (0..40).map(|i| format!("fn f{i}() {{ t(); }}\n")).collect();
    f.write("a.rs", &body);

    let out = f.raw(&["search", "t()", "--limit", "5"]);
    let err = String::from_utf8(out.stderr).unwrap();
    assert!(
        err.contains("--limit"),
        "text mode hid the truncation: {err:?}"
    );

    // ... and stays quiet when the answer is whole, or the note is just noise.
    let err = String::from_utf8(f.raw(&["search", "t()", "--limit", "0"]).stderr).unwrap();
    assert!(
        !err.contains("stopped at --limit"),
        "cried truncation on a complete result: {err:?}"
    );
}

#[test]
fn search_does_not_mark_exactly_full_limit_results_truncated() {
    let f = Fixture::new();
    f.write("a.rs", "fn one() { target(); }\n");

    for mode in [
        vec!["search", "target", "--limit", "1"],
        vec!["search", "target", "--limit", "1", "--files-with-matches"],
        vec!["search", "target", "--limit", "1", "--count"],
    ] {
        let v = f.run(&mode);
        assert_eq!(v["data"]["truncated"], false, "{mode:?}: {v}");
    }

    f.write("b.rs", "fn two() { target(); }\n");
    for mode in [
        vec!["search", "target", "--limit", "1"],
        vec!["search", "target", "--limit", "1", "--files-with-matches"],
        vec!["search", "target", "--limit", "1", "--count"],
    ] {
        let v = f.run(&mode);
        assert_eq!(v["data"]["truncated"], true, "{mode:?}: {v}");
    }
}

#[test]
fn max_per_file_surveys_every_file_not_just_the_first_limit_rows() {
    // --limit used to be spent on raw matches, including the ones the cap was
    // about to discard: `--max-per-file 1 --limit 3` surveyed three *files*
    // worth of hits and reported their partial counts as file totals. Both
    // numbers below were wrong -- the file list was short, and `of N` under-read.
    let f = Fixture::new();
    for name in ["a.rs", "b.rs", "c.rs"] {
        let body: String = (0..10)
            .map(|i| format!("fn {}{i}() {{ t(); }}\n", &name[..1]))
            .collect();
        f.write(name, &body);
    }
    let v = f.run(&["search", "t()", "--max-per-file", "1", "--limit", "3"]);
    let rows = v["data"]["matches"].as_array().unwrap();
    assert_eq!(
        rows.len(),
        3,
        "the limit must still bound the rows returned: {rows:?}"
    );

    let paths: Vec<_> = rows.iter().filter_map(|r| r["path"].as_str()).collect();
    assert_eq!(
        paths,
        ["a.rs", "b.rs", "c.rs"],
        "the cap surveyed only the first file(s)"
    );
    for p in ["a.rs", "b.rs", "c.rs"] {
        assert_eq!(
            v["notes"]["truncated_per_file"][p], 10,
            "{p}'s true count was cut short by --limit: {:?}",
            v["notes"]
        );
    }
}

#[test]
fn max_per_file_totals_survive_rows_that_are_counted_and_not_built() {
    // A capped scan counts the matches past the cap instead of building rows
    // for them. The totals, the context on kept rows and the one-row-per-line
    // rule must all read as if every row had been built and then dropped.
    let f = Fixture::new();
    let body: String = (0..6)
        .map(|i| format!("// before {i}\nfn a{i}() {{ t(); t(); }}\n"))
        .collect();
    f.write("a.rs", &body);
    f.write("b.rs", "fn b() { t(); }\n");

    let v = f.run(&["search", "t()", "--max-per-file", "2", "-B", "1"]);
    let rows = v["data"]["matches"].as_array().unwrap();
    let a: Vec<_> = rows.iter().filter(|r| r["path"] == "a.rs").collect();
    assert_eq!(a.len(), 2, "{rows:?}");
    assert_eq!(a[0]["before"][0], "// before 0", "context lost: {:?}", a[0]);
    assert_eq!(
        v["notes"]["truncated_per_file"]["a.rs"], 6,
        "two matches on one line are one row; {:?}",
        v["notes"]
    );
    assert!(v["notes"]["truncated_per_file"].get("b.rs").is_none());

    let refs = f.run(&["references", "t", "--max-per-file", "2"]);
    assert_eq!(
        refs["data"]["references"]
            .as_array()
            .unwrap()
            .iter()
            .filter(|r| r["path"] == "a.rs")
            .count(),
        2,
        "{refs}"
    );
    assert_eq!(
        refs["notes"]["truncated_per_file"]["a.rs"], 12,
        "{:?}",
        refs["notes"]
    );

    let inverted = f.run(&["search", "fn", "-v", "--max-per-file", "2"]);
    let kept = inverted["data"]["matches"]
        .as_array()
        .unwrap()
        .iter()
        .filter(|r| r["path"] == "a.rs")
        .count();
    assert_eq!(kept, 2, "{inverted}");
    assert_eq!(
        inverted["notes"]["truncated_per_file"]["a.rs"], 6,
        "{:?}",
        inverted["notes"]
    );
}

#[test]
fn max_per_file_does_not_thin_the_matches_a_symbol_search_ranks() {
    // `--symbols` ranks definitions by how many lines matched in them, so the
    // rows past a per-file cap are still evidence there.
    let f = Fixture::new();
    f.write(
        "a.rs",
        "fn busy() {\n    t();\n    t();\n    t();\n}\nfn quiet() { t(); }\n",
    );
    let v = f.run(&["search", "t()", "--symbols", "--max-per-file", "1"]);
    let symbols = v["data"]["symbols"].as_array().unwrap();
    let busy = symbols.iter().find(|s| s["name"] == "busy");
    assert_eq!(busy.map(|s| &s["hits"]), Some(&serde_json::json!(3)), "{v}");
}

#[test]
fn fields_projection_does_not_disable_max_per_file() {
    // Both flags exist to cut tokens, so asking for both is natural -- and used
    // to give *less* reduction than either alone: --fields ran first and removed
    // the `path` column the cap groups by, so the cap silently kept every row.
    let f = Fixture::new();
    let body: String = (0..6).map(|i| format!("fn f{i}() {{ t(); }}\n")).collect();
    f.write("a.rs", &body);

    let v = f.run(&[
        "search",
        "t()",
        "--max-per-file",
        "2",
        "--fields",
        "line,text",
    ]);
    let rows = v["data"]["matches"].as_array().unwrap();
    assert_eq!(rows.len(), 2, "the cap was lost to --fields: {rows:?}");
    assert!(rows[0].get("path").is_none(), "--fields no longer projects");
    assert_eq!(
        v["notes"]["truncated_per_file"]["a.rs"], 6,
        "the true count went missing"
    );
}

#[test]
fn grouping_a_path_only_row_does_not_leave_a_dangling_separator() {
    // `files`/`tree` rows are one per path, so hoisting the path leaves nothing
    // before the `: ` -- every line came out as `: markdown size_bytes=12`.
    let f = Fixture::new();
    f.write("a.rs", "fn one() {}\n");

    let text = String::from_utf8(f.raw(&["files", "--group"]).stdout).unwrap();
    assert!(text.contains("a.rs (1)"), "missing group header: {text:?}");
    assert!(
        !text.lines().any(|l| l.trim_start().starts_with(':')),
        "row led by a bare separator: {text:?}"
    );
    // The payload itself must survive the fix.
    assert!(
        text.contains("rust"),
        "grouping dropped the row's payload: {text:?}"
    );
}

#[test]
fn members_and_symbols_owner_answer_different_questions() {
    // These two look like the same command under two names, and the docs once
    // said so. They aren't: `--owner` filters on the stored owner tag, which
    // only fields and variants carry, so it silently drops every method. Pinning
    // the difference here so the docs can't drift back to treating them as equal.
    let f = Fixture::new();
    f.write(
        "a.py",
        "class Thing:\n    name = 1\n    def go(self): pass\n",
    );

    let members: Vec<String> = f.run(&["members", "Thing"])["data"]
        .as_array()
        .unwrap()
        .iter()
        .filter_map(|r| r["name"].as_str().map(str::to_string))
        .collect();
    assert!(
        members.contains(&"go".to_string()),
        "members lost the method: {members:?}"
    );
    assert!(
        members.contains(&"name".to_string()),
        "members lost the field: {members:?}"
    );

    let owned: Vec<String> = f.run(&["symbols", "--owner", "Thing"])["data"]
        .as_array()
        .unwrap()
        .iter()
        .filter_map(|r| r["name"].as_str().map(str::to_string))
        .collect();
    assert_eq!(
        owned,
        ["name"],
        "--owner's scope changed; the docs need to change with it"
    );
}

#[test]
fn resolved_reports_ambiguity_in_both_directions() {
    // `via` was classified in two places -- Resolver for callers/references,
    // an inline branch in callees_of -- anchored in opposite directions but
    // answering the same question. The copies drifted: callees had no
    // `ambiguous` arm, so the identical situation came back as a confident
    // `imported` one way and `ambiguous` the other.
    let f = Fixture::new();
    f.write("main.rs", "mod a;\nmod b;\nfn run() { helper(); }\n");
    f.write("a.rs", "pub fn helper() {}\n");
    f.write("b.rs", "pub fn helper() {}\n");

    let callees = f.run(&["callees", "run", "--resolved"]);
    let vias: Vec<&str> = callees["data"]
        .as_array()
        .unwrap()
        .iter()
        .filter_map(|r| r["via"].as_str())
        .collect();
    assert_eq!(
        vias,
        ["ambiguous", "ambiguous"],
        "callees hid the ambiguity: {callees:?}"
    );

    let callers = f.run(&["callers", "helper", "--resolved"]);
    assert_eq!(
        callers["data"][0]["via"], "ambiguous",
        "the mirror question disagrees: {callers:?}"
    );

    // One reachable definition is still a confident resolution, not ambiguity.
    f.write("b.rs", "pub fn unrelated() {}\n");
    let one = f.run(&["callees", "run", "--resolved"]);
    assert_eq!(
        one["data"][0]["via"], "imported",
        "over-tagged a single match: {one:?}"
    );
}

#[test]
fn unused_and_untested_agree_on_what_counts_as_test_scaffolding() {
    // Both ask "is this definition test scaffolding?" and used to answer with
    // different rules -- `unused` with the full path/name/enclosing-scope test,
    // `untested` with the path alone. So a `pub fn` inside a `#[cfg(test)] mod
    // tests` in a production file was invisible to one and reported as untested
    // public API by the other.
    let f = Fixture::new();
    f.write(
        "lib.rs",
        "pub fn real() {}\n#[cfg(test)]\nmod tests {\n    pub fn fixture_builder() {}\n    #[test]\n    fn t() { super::real(); }\n}\n",
    );

    let untested: Vec<String> = f.run(&["untested"])["data"]
        .as_array()
        .unwrap()
        .iter()
        .filter_map(|r| r["name"].as_str().map(str::to_string))
        .collect();
    assert!(
        !untested.contains(&"fixture_builder".to_string()),
        "a test-module helper was reported as untested public API: {untested:?}"
    );

    let unused: Vec<String> = f.run(&["unused"])["data"]
        .as_array()
        .unwrap()
        .iter()
        .filter_map(|r| r["name"].as_str().map(str::to_string))
        .collect();
    assert!(
        !unused.contains(&"fixture_builder".to_string()),
        "the two disagree again: {unused:?}"
    );
}

#[test]
fn the_call_graph_walks_agree_on_depth_semantics() {
    // callers/callees/affected each hand-roll the same breadth-first skeleton.
    // They are consistent today and unifying them would cost more indirection
    // than it saves, so this pins the invariants instead: depth 1 is a direct
    // edge, a node is reported at its *shortest* depth, and the seed is never
    // in its own result. A fifth walk that gets one of these wrong fails here.
    let f = Fixture::new();
    f.write(
        "a.rs",
        "pub fn leaf() {}\npub fn mid() { leaf(); }\npub fn top() { mid(); leaf(); }\npub fn root() { top(); }\n",
    );

    let up = f.run(&["callers", "leaf", "--depth", "3"]);
    let up: Vec<(&str, i64)> = up["data"]
        .as_array()
        .unwrap()
        .iter()
        .map(|r| (r["name"].as_str().unwrap(), r["depth"].as_i64().unwrap()))
        .collect();
    // `top` calls leaf directly *and* sits above mid: shortest depth wins.
    assert_eq!(
        up,
        [("mid", 1), ("top", 1), ("root", 2)],
        "callers depth changed"
    );

    let down = f.run(&["callees", "root", "--depth", "3"]);
    let down: Vec<(&str, i64)> = down["data"]
        .as_array()
        .unwrap()
        .iter()
        .map(|r| (r["name"].as_str().unwrap(), r["depth"].as_i64().unwrap()))
        .collect();
    assert_eq!(
        down,
        [("top", 1), ("leaf", 2), ("mid", 2)],
        "callees depth changed"
    );

    for (cmd, seed, key) in [("callers", "leaf", "name"), ("callees", "root", "name")] {
        let v = f.run(&[cmd, seed, "--depth", "3"]);
        assert!(
            !v["data"].as_array().unwrap().iter().any(|r| r[key] == seed),
            "{cmd} reported its own seed"
        );
    }
}

// ---------------------------------------------------------------------------
// SQL: a schema is the one thing every language in a repo talks to
// ---------------------------------------------------------------------------

/// A schema in the shape real repositories have it: an initial migration, then a
/// later one that alters what the first created.
const SCHEMA: &str = "\
-- people who can log in
CREATE TABLE users (
  id BIGSERIAL PRIMARY KEY,
  email TEXT NOT NULL UNIQUE,
  org_id BIGINT REFERENCES orgs(id)
);

CREATE INDEX idx_users_email ON users (email);

CREATE VIEW active_users AS
  SELECT u.id, u.email FROM users u WHERE u.deleted_at IS NULL;

CREATE FUNCTION touch_updated_at() RETURNS trigger AS $$
BEGIN
  RETURN NEW;
END;
$$ LANGUAGE plpgsql;

CREATE TRIGGER users_touch BEFORE UPDATE ON users
  FOR EACH ROW EXECUTE FUNCTION touch_updated_at();
";

#[test]
fn sql_declarations_are_definitions_named_once_each() {
    // `.sql` was indexed as searchable text with no definitions at all. The risk
    // in fixing that is over-naming, not under-naming: `create_trigger` has three
    // `object_reference` children (itself, the table it fires on, the function it
    // executes) and a `REFERENCES` column has two identifiers, so an unanchored
    // query names each declaration several times over.
    let f = Fixture::new();
    f.write("db/001_init.sql", SCHEMA);

    let rows = f.run(&["symbols", "--path", "001_init", "--limit", "0"]);
    let mut got: Vec<(String, String)> = rows["data"]
        .as_array()
        .unwrap()
        .iter()
        .map(|r| {
            (
                r["kind"].as_str().unwrap().to_string(),
                r["name"].as_str().unwrap().to_string(),
            )
        })
        .collect();
    got.sort();
    assert_eq!(
        got,
        [
            ("field".to_string(), "email".to_string()),
            ("field".to_string(), "id".to_string()),
            ("field".to_string(), "org_id".to_string()),
            ("function".to_string(), "touch_updated_at".to_string()),
            ("index".to_string(), "idx_users_email".to_string()),
            ("table".to_string(), "users".to_string()),
            ("trigger".to_string(), "users_touch".to_string()),
            ("view".to_string(), "active_users".to_string()),
        ],
        "each declaration exactly once, and `orgs`/`id` from the REFERENCES clause \
         are a *use* of another table, not a declaration"
    );

    // The trigger names the function it fires, so the call graph joins them up
    // without SQL having anything a call-graph extractor would recognise.
    let callers = field_set(&f.run(&["callers", "touch_updated_at"]), "name");
    assert_eq!(callers, ["users_touch"], "{callers:?}");
}

#[test]
fn sql_quoted_column_names_keep_their_names_and_source_spelling() {
    let f = Fixture::new();
    f.write(
        "db/schema.sql",
        "CREATE TABLE t (\"select\" TEXT, `backtick` INTEGER, [bracket] TEXT);\n",
    );

    let mut names = field_set(&f.run(&["members", "t"]), "name");
    names.sort();
    assert_eq!(
        names,
        ["backtick", "bracket", "select"]
            .into_iter()
            .map(str::to_string)
            .collect::<Vec<_>>(),
        "quoted SQL identifiers should be addressable without their delimiters"
    );
    let outline = f.run(&["outline", "schema.sql"]);
    let bracket = outline["data"]
        .as_array()
        .unwrap()
        .iter()
        .find(|row| row["name"] == "bracket")
        .expect("bracket-quoted column should appear in the outline");
    assert_eq!(bracket["signature"], "[bracket] TEXT");
}

#[test]
fn sql_escaped_identifier_delimiters_are_unescaped_without_parse_errors() {
    let f = Fixture::new();
    f.write(
        "db/escaped.sql",
        r#"CREATE TABLE t ("quote""name" TEXT, `tick``name` TEXT, [close]]name] TEXT);
"#,
    );

    let mut names = field_set(&f.run(&["members", "t"]), "name");
    names.sort();
    assert_eq!(
        names,
        ["close]name", "quote\"name", "tick`name"]
            .into_iter()
            .map(str::to_string)
            .collect::<Vec<_>>()
    );
    let health = f.run(&["files", "escaped.sql"]);
    assert_eq!(health["data"][0]["parse_errors"], 0);

    let outline = f.run(&["outline", "escaped.sql"]);
    for (name, signature) in [
        ("quote\"name", "\"quote\"\"name\" TEXT"),
        ("tick`name", "`tick``name` TEXT"),
        ("close]name", "[close]]name] TEXT"),
    ] {
        let row = outline["data"]
            .as_array()
            .unwrap()
            .iter()
            .find(|row| row["name"] == name)
            .unwrap_or_else(|| panic!("missing SQL column {name:?}: {outline}"));
        assert_eq!(row["signature"], signature);
    }
}

#[test]
fn sql_statement_the_grammar_lacks_does_not_hide_the_definitions_after_it() {
    // A PostgreSQL extension script is full of statements the grammar has no
    // rule for, and starts with a psql meta-command. An `OPERATOR CLASS` with
    // `FOR ORDER BY` and `FUNCTION n` entries makes recovery wrap everything
    // after it in a single ERROR, so on pgvector's own install script 73 of 110
    // functions had no definition. Each statement is parsed on its own now.
    let f = Fixture::new();
    f.write(
        "ext/install.sql",
        "\\echo Use \"CREATE EXTENSION x\" to load this file. \\quit\n\
         CREATE FUNCTION before_fn(vector) RETURNS vector\n\
         \tAS 'MODULE_PATHNAME' LANGUAGE C;\n\
         CREATE OPERATOR CLASS sparsevec_l1_ops\n\
         \tFOR TYPE sparsevec USING hnsw AS\n\
         \tOPERATOR 1 <+> (sparsevec, sparsevec) FOR ORDER BY float_ops,\n\
         \tFUNCTION 1 l1_distance(sparsevec, sparsevec),\n\
         \tFUNCTION 3 hnsw_sparsevec_support(internal);\n\
         CREATE FUNCTION body_fn() RETURNS int AS $$ SELECT 1; SELECT 2; $$ LANGUAGE sql;\n\
         CREATE FUNCTION after_fn(vector) RETURNS vector\n\
         \tAS 'MODULE_PATHNAME' LANGUAGE C;\n\
         CREATE TABLE after_t (id integer);\n",
    );

    let defs = |name: &str| f.run(&["definitions", name]);
    for (name, line) in [
        ("before_fn", 2),
        ("body_fn", 9),
        ("after_fn", 10),
        ("after_t", 12),
    ] {
        let v = defs(name);
        assert_eq!(
            v["data"][0]["line"], line,
            "{name} is missing or misplaced: {v}"
        );
    }
    // Nothing inside the unsupported statement is mistaken for a definition.
    for ghost in ["sparsevec_l1_ops", "l1_distance", "float_ops"] {
        assert!(
            defs(ghost)["data"].as_array().unwrap().is_empty(),
            "{ghost} should not be a definition"
        );
    }

    // The unsupported statement is still reported, not hidden.
    let health = f.run(&["files", "install.sql"]);
    assert!(
        health["data"][0]["parse_errors"].as_u64().unwrap() > 0,
        "{health}"
    );

    // String spans survive per-statement parsing: the literals on either side
    // of the unsupported statement are strings, so `--in strings` finds both.
    let s = f.run(&["search", "MODULE_PATHNAME", "--in", "strings"]);
    let lines: Vec<_> = s["data"]["matches"]
        .as_array()
        .unwrap()
        .iter()
        .map(|m| m["line"].as_i64().unwrap())
        .collect();
    assert!(lines.contains(&3) && lines.contains(&11), "{lines:?}");
}

#[test]
fn escaped_language_identifiers_resolve_by_their_semantic_names() {
    let f = Fixture::new();
    f.write(
        "raw.rs",
        "pub fn r#type() {}\npub fn call() { r#type(); }\n",
    );
    f.write(
        "escaped.cs",
        "public class @class { public int @value { get; set; } public void @event() {} public void invoke() { @event(); } }\n",
    );

    let rust_def = f.run(&["definitions", "type", "--path", "raw.rs"]);
    assert_eq!(rust_def["data"][0]["name"], "type");
    assert!(rust_def["data"][0]["signature"]
        .as_str()
        .unwrap()
        .contains("r#type"));
    let rust_calls = field_set(&f.run(&["callees", "call", "--path", "raw.rs"]), "name");
    assert!(rust_calls.contains(&"type".to_string()), "{rust_calls:?}");

    let csharp_type = f.run(&["definitions", "class", "--path", "escaped.cs"]);
    assert_eq!(csharp_type["data"][0]["name"], "class");
    let value = f.run(&["definitions", "value", "--path", "escaped.cs"]);
    let value = &value["data"][0];
    assert_eq!(value["owner"], "class");

    let csharp_def = f.run(&["definitions", "event", "--path", "escaped.cs"]);
    assert_eq!(csharp_def["data"][0]["name"], "event");
    let csharp_calls = field_set(
        &f.run(&["callees", "invoke", "--path", "escaped.cs"]),
        "name",
    );
    assert!(
        csharp_calls.contains(&"event".to_string()),
        "{csharp_calls:?}"
    );
}

#[test]
fn a_column_added_by_a_later_migration_still_belongs_to_its_table() {
    // Members are dropped when they have no owning type, and owners are resolved
    // by walking up the parse tree — which finds `CREATE TABLE` but would miss
    // `ALTER TABLE`. In a migrations directory that is where most columns live,
    // so the newest half of a schema would have been the invisible half.
    let f = Fixture::new();
    f.write("db/001_init.sql", SCHEMA);
    f.write(
        "db/002_name.sql",
        "ALTER TABLE users ADD COLUMN display_name TEXT;\n",
    );

    let members = field_set(&f.run(&["members", "users"]), "name");
    assert!(
        members.contains(&"display_name".to_string()),
        "a column added by ALTER TABLE is still a column of that table: {members:?}"
    );
    let owner = f.run(&["definitions", "display_name"])["data"][0]["owner"]
        .as_str()
        .unwrap()
        .to_string();
    assert_eq!(owner, "users");
}

#[test]
fn an_out_of_body_member_is_only_trusted_when_its_owner_is_unambiguous() {
    // Reaching for the `owner` tag is what lets an `ALTER TABLE` column find its
    // table across files — but the tag is a *name*, so with two same-named tables
    // it would hand each one the other's columns. That is exactly the name-based
    // false positive `members` exists not to have, so the tag is only trusted
    // when the name has one definition in the repository.
    let f = Fixture::new();
    f.write("a/schema.sql", "CREATE TABLE jobs (id INT);\n");
    f.write("b/schema.sql", "CREATE TABLE jobs (id INT);\n");
    f.write("a/later.sql", "ALTER TABLE jobs ADD COLUMN retries INT;\n");

    let names = field_set(&f.run(&["members", "jobs"]), "name");
    assert!(
        !names.contains(&"retries".to_string()),
        "two tables share the name, so the tag cannot say which one gained the column: {names:?}"
    );
    // Scoping picks an owner; it does not make the name unambiguous.
    let scoped = field_set(&f.run(&["members", "jobs", "--path", "a/"]), "name");
    assert!(!scoped.contains(&"retries".to_string()), "{scoped:?}");
    // The column is still a definition that knows its owner — `members` is being
    // careful, not blind.
    let defs = f.run(&["definitions", "retries"]);
    assert_eq!(defs["data"][0]["owner"], "jobs");
}

#[test]
fn a_schema_qualified_table_is_addressable_by_its_bare_name() {
    // `CREATE TABLE analytics.events` is written both ways depending on
    // `search_path`, and reference matching is textual — so storing the qualified
    // form would leave `definitions events` empty while `references events`
    // happily found every use.
    let f = Fixture::new();
    f.write("db/a.sql", "CREATE TABLE analytics.events (id INT);\n");

    let v = f.run(&["definitions", "events"]);
    assert_eq!(v["data"].as_array().unwrap().len(), 1, "{v}");
    assert_eq!(v["data"][0]["name"], "events");
    // The qualifier is not lost — it is what the signature shows.
    assert!(
        v["data"][0]["signature"]
            .as_str()
            .unwrap()
            .contains("analytics.events"),
        "{v}"
    );
}

#[test]
fn sql_enum_variants_are_stored_without_their_quotes() {
    // A Postgres enum declares its members as quoted literals, so capturing them
    // verbatim would answer `members mood` with `'sad'` — a name no query for
    // `sad` can ever match.
    let f = Fixture::new();
    f.write(
        "db/a.sql",
        "CREATE TYPE mood AS ENUM ('sad', 'ok', 'happy');\n",
    );

    let members = field_set(&f.run(&["members", "mood"]), "name");
    assert_eq!(members, ["sad", "ok", "happy"], "{members:?}");
}

#[test]
fn sql_literals_are_strings_only_when_they_are_quoted() {
    // This grammar gives every scalar the same `literal` node, so a span query
    // cannot tell `'admin'` from `42`. Marking them all as strings would push
    // numeric and boolean literals out of `--in code`.
    let f = Fixture::new();
    f.write(
        "db/a.sql",
        "-- seed the admin\nINSERT INTO users (role, level) VALUES ('admin', 42);\n",
    );

    let hits = |pat: &str, scope: &str| {
        f.run(&["search", pat, "--in", scope])["data"]["matches"]
            .as_array()
            .unwrap()
            .len()
    };
    assert_eq!(hits("admin", "strings"), 1, "the quoted value is a string");
    assert_eq!(hits("admin", "code"), 0, "the quoted value is not code");
    assert_eq!(hits("admin", "comments"), 1, "`-- seed the admin` is prose");
    // The number stayed code, which is the whole reason the quote decides.
    assert_eq!(hits("42", "code"), 1, "a numeric literal is not a string");
    assert_eq!(hits("42", "strings"), 0, "and it is not a string either");
}

#[test]
fn a_table_is_reachable_from_the_query_that_names_it_in_another_language() {
    // The payoff for indexing SQL at all: the schema is the one artefact every
    // language in a polyglot repo talks to, and the query that reads it lives in
    // a string in some other file.
    let f = Fixture::new();
    f.write("db/001_init.sql", SCHEMA);
    f.write(
        "app.py",
        "def load(conn):\n    return conn.execute(\"SELECT id FROM users\")\n",
    );

    let v = f.run(&["references", "users", "--path", "app.py"]);
    let refs = v["data"]["references"].as_array().unwrap();
    assert_eq!(refs.len(), 1, "{v}");
    assert_eq!(
        refs[0]["symbol"], "load",
        "attributed to the function around it"
    );

    // And the definition it resolves to is the table, from `about`.
    let defs = field_set(&f.run(&["definitions", "users"]), "kind");
    assert_eq!(defs, ["table"], "{defs:?}");
}

#[test]
fn unused_reports_the_sql_objects_that_can_be_referenced_and_no_others() {
    // An index is named exactly once in the world — at its own `CREATE` — and a
    // trigger and an enum variant are no different, so reporting them would put
    // noise in every repository that has a schema, always. A table nothing
    // queries is a real finding, because codeq does find table names inside the
    // SQL strings in application code.
    let f = Fixture::new();
    f.write("db/001_init.sql", SCHEMA);
    f.write("db/002_dead.sql", "CREATE TABLE forgotten (id INT);\n");
    f.write("db/003_enum.sql", "CREATE TYPE mood AS ENUM ('sad');\n");
    f.write(
        "app.py",
        "def load(conn):\n    return conn.execute(\"SELECT id FROM users\")\n",
    );

    let dead = field_set(&f.run(&["unused"]), "name");
    assert!(
        dead.contains(&"forgotten".to_string()),
        "a table nothing queries: {dead:?}"
    );
    for quiet in ["idx_users_email", "users_touch", "sad"] {
        assert!(
            !dead.contains(&quiet.to_string()),
            "{quiet} is named once by construction, so it is not evidence of anything: {dead:?}"
        );
    }
    assert!(
        !dead.contains(&"users".to_string()),
        "queried from app.py: {dead:?}"
    );
}

// ---------------------------------------------------------------------------
// The four dogfooding fixes: explain, positional paths, rows pointer, one policy
// ---------------------------------------------------------------------------

#[test]
fn astgrep_explain_names_the_kinds_a_pattern_is_written_from() {
    // `astgrep` demands exact node kinds and offered no way to discover them, so
    // writing a pattern meant leaving codeq — and a wrong guess comes back as an
    // empty result indistinguishable from "no matches".
    let f = Fixture::new();
    f.write("a.rs", "fn main() {\n    let x = compute(&arg);\n}\n");

    let v = f.run(&["astgrep", "--explain", "a.rs:2"]);
    let kinds = field_set(&v, "kind");
    for want in ["let_declaration", "call_expression", "identifier"] {
        assert!(
            kinds.contains(&want.to_string()),
            "{want} missing: {kinds:?}"
        );
    }
    // Field names matter more than kinds — they are the part of a grammar that
    // is stable across languages, and `callsite.rs` is written against them.
    let fields = field_set(&v, "field");
    assert!(fields.contains(&"function".to_string()), "{fields:?}");

    // Nesting is real, and the walk starts at the line's own node rather than at
    // the file, so the tree is about the line you asked about.
    let depths: Vec<i64> = v["data"]
        .as_array()
        .unwrap()
        .iter()
        .map(|r| r["depth"].as_i64().unwrap())
        .collect();
    assert_eq!(depths.first(), Some(&0), "{v}");
    assert!(
        depths.iter().any(|d| *d > 1),
        "the tree has depth: {depths:?}"
    );

    // The whole point: the kinds it reports are the ones that then match.
    let hits = f.run(&["astgrep", "$F(&$A)", "--language", "rust"]);
    assert_eq!(
        hits["data"]["matches"].as_array().unwrap().len(),
        1,
        "{hits}"
    );
}

#[test]
fn astgrep_explain_refuses_to_invent_a_tree_it_does_not_have() {
    let f = Fixture::new();
    f.write("a.rs", "fn main() {}\n");
    f.write("doc.md", "# Title\n\nProse.\n");

    // A line past the end of the file has no tree. Clamping to the file's last
    // byte would silently return the root — a plausible answer to a question
    // with none, which is the exact failure this command exists to prevent.
    let v = f.run(&["astgrep", "--explain", "a.rs:9999"]);
    assert!(v["data"].as_array().unwrap().is_empty(), "{v}");
    assert!(
        v["note"]
            .as_str()
            .unwrap_or_default()
            .contains("no parse tree"),
        "an empty result must say why: {v}"
    );

    // A language codeq indexes but has no grammar for says so, rather than
    // returning nothing and letting that read as "no nodes here".
    let out = f.raw(&["astgrep", "--explain", "doc.md:1"]);
    assert!(!out.status.success());
    assert!(
        String::from_utf8_lossy(&out.stderr).contains("no grammar for 'markdown'"),
        "{:?}",
        String::from_utf8_lossy(&out.stderr)
    );
}

#[test]
fn search_takes_paths_positionally_the_way_grep_does() {
    // `codeq search PAT FILE` is the first thing anyone types, and it used to be
    // `error: unexpected argument` — from a command whose docs claim to be a
    // superset of grep, and while `imports`, `outline`, `api` and `read` all
    // take a positional path.
    let f = Fixture::new();
    f.write("src/a.rs", "// TODO: one\n");
    f.write("src/b.rs", "// TODO: two\n");
    f.write("docs/c.md", "TODO: three\n");

    let hits = |args: &[&str]| {
        let mut v = vec!["search", "TODO"];
        v.extend_from_slice(args);
        f.run(&v)["data"]["matches"].as_array().unwrap().len()
    };
    assert_eq!(hits(&[]), 3, "no path: everything");
    assert_eq!(hits(&["src/a.rs"]), 1, "one file");
    assert_eq!(hits(&["src"]), 2, "a directory scopes everything under it");
    assert_eq!(
        hits(&["src/a.rs", "docs"]),
        2,
        "several are OR'd, as a shell glob expands to"
    );
    // Composes with the flags rather than replacing them.
    assert_eq!(hits(&["src", "-g", "*.rs"]), 2);
    assert_eq!(
        hits(&["src", "-g", "!*.rs"]),
        0,
        "glob and path compose by AND"
    );
}

#[test]
fn the_envelope_says_where_its_rows_are() {
    // `codeq.result.v2` made every row *key* mean one thing; the container still
    // varies, and legitimately — `search -l` returns files where plain `search`
    // returns matches. So the envelope states which one it used, and a consumer
    // stops having to know thirty commands in advance.
    let f = Fixture::new();
    f.write("a.rs", "fn alpha() {}\nfn beta() { alpha(); }\n");

    let at = |v: &Value| -> Value {
        let p = v["rows"].as_str().expect("rows pointer");
        p.split('.').fold(v.clone(), |acc, k| acc[k].clone())
    };
    for (args, want) in [
        (vec!["symbols", "--limit", "0"], "data"),
        (vec!["search", "alpha"], "data.matches"),
        (vec!["search", "alpha", "-l"], "data.files"),
        (vec!["search", "alpha", "-c"], "data.counts"),
        (vec!["references", "alpha"], "data.references"),
    ] {
        let v = f.run(&args);
        assert_eq!(v["rows"], want, "{args:?}: {v}");
        // The pointer must actually resolve to the rows, not just look right.
        assert!(at(&v).is_array(), "{args:?} pointer does not resolve: {v}");
    }

    // A result with several lists has no single answer, and saying nothing beats
    // guessing which list was meant.
    let about = f.run(&["about", "alpha"]);
    assert!(
        about["rows"].is_null(),
        "about carries several lists: {about}"
    );
}

#[test]
fn unused_exclusions_are_one_policy_not_a_growing_query() {
    // The rule lives in `facts::liveness::UNREFERENCED_BY_DESIGN` now; this pins the
    // behaviour it governs so moving it cannot quietly change it.
    let f = Fixture::new();
    f.write("s.graphql", "type User { id: ID! }\n");
    f.write(
        "u.proto",
        "syntax = \"proto3\";\nmessage Order { string id = 1; }\n",
    );
    f.write(
        "main.tf",
        "output \"url\" { value = 1 }\nvariable \"dead_var\" {}\n",
    );
    // `ghost` is named nowhere else, so it is dead. `kept` is named by the index
    // below it, which is a real reference — the index itself is named nowhere,
    // by construction, which is exactly why it is excluded.
    f.write(
        "db.sql",
        "CREATE TABLE ghost (id INT);\nCREATE TABLE kept (id INT);\n\
         CREATE INDEX idx_kept ON kept (id);\n",
    );
    f.write("a.css", ".orphan { color: blue }\n");

    let dead = field_set(&f.run(&["unused"]), "name");
    for reported in ["dead_var", "orphan", "ghost"] {
        assert!(
            dead.contains(&reported.to_string()),
            "{reported} is dead: {dead:?}"
        );
    }
    for quiet in ["User", "Order", "url", "idx_kept", "kept"] {
        assert!(
            !dead.contains(&quiet.to_string()),
            "{quiet} is consumed elsewhere: {dead:?}"
        );
    }
}

// ---------------------------------------------------------------------------
// Found by dogfooding: cross-checking commands that must agree
// ---------------------------------------------------------------------------

#[test]
fn javascript_property_assignment_is_a_definition() {
    // How most of Node is written, and it was invisible: `outline` on Express's
    // whole router file — five methods, 673 lines — returned nothing.
    let f = Fixture::new();
    f.write(
        "router.js",
        "var proto = module.exports = function () { return 1; };\n\
         \n\
         proto.use = function use(fn) {\n  return gettype(fn);\n};\n\
         \n\
         Thing.prototype.render = function render(x) {\n  return x;\n};\n\
         \n\
         exports.helper = function () {\n  return 2;\n};\n",
    );
    f.write(
        "local.js",
        "function outer() {\n  var o = {};\n  o.inner = function () {};\n}\n",
    );

    let v = f.run(&["symbols", "--path", "router.js", "--limit", "0"]);
    let names = field_set(&v, "name");
    for want in ["use", "render", "helper"] {
        assert!(
            names.contains(&want.to_string()),
            "{want} missing: {names:?}"
        );
    }
    // The name is the property, not the path to it — what a call site writes.
    assert!(!names.iter().any(|n| n.contains('.')), "{names:?}");
    // They are methods: you call them on a value.
    let kinds: Vec<String> = v["data"]
        .as_array()
        .unwrap()
        .iter()
        .filter(|r| r["name"] == "use")
        .map(|r| r["kind"].as_str().unwrap().to_string())
        .collect();
    assert_eq!(kinds, ["method"], "{v}");

    // Module scope only: an assignment inside a function is not a definition, or
    // every local object literal would bury the index.
    let local = field_set(
        &f.run(&["symbols", "--path", "local.js", "--limit", "0"]),
        "name",
    );
    assert!(!local.contains(&"inner".to_string()), "{local:?}");

    // `exports.x` *is* the CommonJS export; `proto.x` cannot be known to escape.
    let api = field_set(&f.run(&["api", "router.js"]), "name");
    assert_eq!(api, ["helper"], "{api:?}");
}

#[test]
fn a_reference_at_file_scope_says_so_instead_of_coming_back_blank() {
    // `callers` promises definitions, but a module-level call has none — the row
    // used to arrive with a location and no `name`, which reads as a broken row
    // and crashed two consumers that indexed the key.
    let f = Fixture::new();
    // A bare call at module scope, deliberately *not* an assignment: a
    // module-scope binding is now a definition, so `VALUE = helper()` has an
    // enclosing definition to be attributed to and no longer exercises this path.
    f.write(
        "a.py",
        "def helper():\n    return 1\n\nhelper()\n\ndef user():\n    return helper()\n",
    );

    let v = f.run(&["callers", "helper"]);
    let rows = v["data"].as_array().unwrap();
    let anon: Vec<&Value> = rows.iter().filter(|r| r["name"].is_null()).collect();
    assert_eq!(
        anon.len(),
        1,
        "the module-level call is still reported: {v}"
    );
    assert_eq!(anon[0]["scope"], "file", "and it says what it is: {v}");
    assert_eq!(anon[0]["line"], 4);
    // A real definition never carries the label.
    let named: Vec<&Value> = rows.iter().filter(|r| r["name"] == "user").collect();
    assert_eq!(named.len(), 1, "{v}");
    assert!(named[0]["scope"].is_null(), "{v}");
}

#[test]
fn callers_and_callees_can_be_bounded_like_every_other_list() {
    // They were the only two list commands with no `--limit`, and the likeliest
    // to return a lot: `callers serverLog` in redis is 227 rows and 11 KB.
    let f = Fixture::new();
    let mut src = String::from("def target():\n    return 1\n");
    for i in 0..12 {
        src.push_str(&format!("\ndef c{i}():\n    return target()\n"));
    }
    f.write("a.py", &src);

    assert_eq!(
        f.run(&["callers", "target", "--limit", "0"])["data"]
            .as_array()
            .unwrap()
            .len(),
        12
    );
    assert_eq!(
        f.run(&["callers", "target", "--limit", "3"])["data"]
            .as_array()
            .unwrap()
            .len(),
        3
    );
    // Also on the transitive walk, where an unbounded result is worse.
    let deep = f.run(&["callers", "target", "--depth", "3", "--limit", "4"]);
    assert_eq!(deep["data"].as_array().unwrap().len(), 4, "{deep}");

    let callees = f.run(&["callees", "c0", "--limit", "1"]);
    assert!(callees["data"].as_array().unwrap().len() <= 1, "{callees}");
}

#[test]
fn a_sql_foreign_key_is_an_edge_but_not_an_is_a() {
    // The one genuinely structural fact a schema has. It gets its own relation
    // kind rather than being folded into implements/extends, which would have
    // `supertypes` assert an inheritance that does not exist.
    let f = Fixture::new();
    f.write(
        "db/001.sql",
        "CREATE TABLE orders (\n  id INT,\n  user_id BIGINT REFERENCES users(id),\n\
         \x20 CONSTRAINT fk_p FOREIGN KEY (p_id) REFERENCES products(id)\n);\n",
    );
    // The form a migrations directory actually uses for a key added later.
    f.write(
        "db/002.sql",
        "ALTER TABLE orders ADD CONSTRAINT fk_s FOREIGN KEY (s_id) REFERENCES shipments(id);\n",
    );

    let out = field_set(&f.run(&["supertypes", "orders"]), "name");
    assert_eq!(
        {
            let mut v = out.clone();
            v.sort();
            v
        },
        ["products", "shipments", "users"],
        "all three syntactic forms, inline / table-level / ALTER: {out:?}"
    );
    let rels = field_set(&f.run(&["supertypes", "orders"]), "relation");
    assert!(rels.iter().all(|r| r == "references"), "{rels:?}");

    // And the reverse direction: what points at a table.
    let into = field_set(&f.run(&["implementors", "users"]), "name");
    assert_eq!(into, ["orders"], "{into:?}");

    // Asking for inheritance gets nothing, which is the point of the separate kind.
    let inherit = f.run(&["supertypes", "orders", "--relation", "extends"]);
    assert!(inherit["data"].as_array().unwrap().is_empty(), "{inherit}");
    let fk = field_set(
        &f.run(&["supertypes", "orders", "--relation", "references"]),
        "name",
    );
    assert_eq!(fk.len(), 3, "{fk:?}");
}

#[test]
fn graph_says_which_relation_kinds_exist_when_the_filter_empties_it() {
    // C# writes `class C : B, I` for both inheritance and implementation, so
    // codeq can only record `extends` there. `--relation implements` on a C#
    // repo full of interface implementations used to answer "this repository
    // genuinely has no inheritance edges to graph" — a claim about the code,
    // when the true cause was the filter.
    let f = Fixture::new();
    f.write(
        "a.cs",
        "interface I { }\nabstract class B { }\nclass C : B, I { }\n",
    );

    let note = note_of(&f, &["graph", "--relation", "implements"]);
    assert!(
        note.contains("extends"),
        "note should name the kinds present: {note}"
    );
    assert!(
        !note.contains("genuinely has no"),
        "the filter emptied this, not the code: {note}"
    );

    // With no such filter, and no edges at all, the old wording still applies.
    let f2 = Fixture::new();
    f2.write("a.py", "def f():\n    pass\n");
    let note = note_of(&f2, &["graph", "--relation", "implements"]);
    assert!(note.contains("genuinely has no"), "{note}");
}

#[test]
fn graph_emits_the_whole_relation_graph_not_one_node() {
    // `implementors`/`supertypes` answer about one node; drawing a schema or a
    // hierarchy needs all the edges at once.
    let f = Fixture::new();
    f.write(
        "db.sql",
        "CREATE TABLE orders (id INT, u BIGINT REFERENCES users(id));\n\
         CREATE TABLE items (id INT, o BIGINT REFERENCES orders(id));\n",
    );
    f.write("a.ts", "interface Node {}\nclass Post implements Node {}\n");

    let all = f.run(&["graph"]);
    let pairs: Vec<(String, String)> = all["data"]
        .as_array()
        .unwrap()
        .iter()
        .map(|e| {
            (
                e["from"].as_str().unwrap().to_string(),
                e["to"].as_str().unwrap().to_string(),
            )
        })
        .collect();
    assert!(
        pairs.contains(&("orders".into(), "users".into())),
        "{pairs:?}"
    );
    assert!(
        pairs.contains(&("items".into(), "orders".into())),
        "{pairs:?}"
    );
    assert!(
        pairs.contains(&("Post".into(), "Node".into())),
        "one graph, all languages: {pairs:?}"
    );

    // Filterable to one kind of edge, which is how you get just the schema.
    let fks = f.run(&["graph", "--relation", "references"]);
    assert_eq!(fks["data"].as_array().unwrap().len(), 2, "{fks}");
    assert!(fks["data"]
        .as_array()
        .unwrap()
        .iter()
        .all(|e| e["relation"] == "references"));

    // Mermaid is a document, not rows — it deliberately leaves the row contract.
    let out = f.raw(&["graph", "--relation", "references", "--mermaid"]);
    let doc = String::from_utf8_lossy(&out.stdout);
    assert!(doc.starts_with("graph LR"), "{doc}");
    assert!(doc.contains("-->|references|"), "{doc}");
    assert!(doc.contains("[orders]") && doc.contains("[users]"), "{doc}");

    // An empty graph says why rather than looking like a failure.
    let none = f.raw(&["graph", "--relation", "extends"]);
    assert!(none.status.success());
    assert!(
        String::from_utf8_lossy(&none.stderr).contains("no 'extends' relation edges"),
        "{:?}",
        String::from_utf8_lossy(&none.stderr)
    );
}

#[test]
fn a_test_attribute_is_read_instead_of_guessed_from_the_name() {
    // Test recognition was three label checks — the file path, the definition's
    // own name, the name of the module around it. So a `#[test] fn` two lines
    // below the function it tests was invisible, and any framework that does not
    // make you name your tests `test_*` was unsupported by accident. The
    // attribute is a fact in the parse tree; this reads it.
    let f = Fixture::new();
    f.write(
        "lib.rs",
        "pub fn parse(s: &str) -> usize { s.len() }\n\n\
         #[test]\nfn parses_an_empty_string() { assert_eq!(parse(\"\"), 0); }\n\n\
         #[tokio::test]\nasync fn parses_async() { parse(\"a\"); }\n",
    );
    f.write(
        "lib.py",
        "import pytest\n\ndef parse(s):\n    return len(s)\n\n\
         @pytest.mark.parametrize(\"s\", [\"\"])\ndef handles_anything(s):\n    assert parse(s) >= 0\n",
    );
    f.write(
        "App.java",
        "class Calc { public int add(int a, int b) { return a + b; } }\n\
         class Verify {\n  @Test\n  public void addsTwoNumbers() { new Calc().add(1, 2); }\n}\n",
    );
    f.write(
        "App.cs",
        "class Calc { public int Mul(int a) { return a; } }\n\
         class Verify {\n  [Fact]\n  public void MultipliesOne() { new Calc().Mul(1); }\n}\n",
    );

    // `tests` nests its rows under `data.tests` (the envelope's `rows` pointer
    // says so); collect through that rather than assuming a bare array.
    let names = |args: &[&str]| -> Vec<String> {
        let mut v = vec!["tests"];
        v.extend_from_slice(args);
        f.run(&v)["data"]["tests"]
            .as_array()
            .unwrap()
            .iter()
            .filter_map(|r| r["name"].as_str().map(str::to_string))
            .collect()
    };

    // None of these names, paths or containers say "test".
    let rust = names(&["parse", "--path", "lib.rs"]);
    assert!(
        rust.contains(&"parses_an_empty_string".to_string()),
        "{rust:?}"
    );
    assert!(
        rust.contains(&"parses_async".to_string()),
        "`#[tokio::test]`: {rust:?}"
    );
    assert!(names(&["parse", "--path", "lib.py"]).contains(&"handles_anything".to_string()));
    assert!(names(&["add"]).contains(&"addsTwoNumbers".to_string()));
    assert!(names(&["Mul"]).contains(&"MultipliesOne".to_string()));

    // The three commands that share the predicate agree: an attributed test is
    // scaffolding, so it is neither dead code nor untested public API.
    let dead = field_set(&f.run(&["unused"]), "name");
    for t in [
        "parses_an_empty_string",
        "handles_anything",
        "addsTwoNumbers",
    ] {
        assert!(
            !dead.contains(&t.to_string()),
            "{t} is a test, not dead code: {dead:?}"
        );
    }
    let untested = field_set(&f.run(&["untested"]), "name");
    assert!(
        !untested.contains(&"parse".to_string()),
        "it has a test now: {untested:?}"
    );
}

#[test]
fn an_ordinary_attribute_does_not_make_something_a_test() {
    // "Contains the word test" would have caught `#[cfg(test)]` on a module and
    // `@TestOnly` on production code, so the marker list is fixed rather than a
    // pattern. This is the check that keeps it honest.
    let f = Fixture::new();
    f.write(
        "lib.rs",
        "pub fn helper() -> u32 { 1 }\n\n\
         #[inline]\nfn calls_helper() -> u32 { helper() }\n\n\
         #[derive(Debug)]\nstruct Config { field: u32 }\n",
    );

    assert!(
        f.run(&["tests", "helper"])["data"]["tests"]
            .as_array()
            .unwrap()
            .is_empty(),
        "#[inline] is not a test attribute"
    );
    // And an unreferenced definition next to those attributes is still dead code,
    // i.e. the change did not quietly widen what counts as test scaffolding.
    let dead = field_set(&f.run(&["unused"]), "name");
    assert!(dead.contains(&"calls_helper".to_string()), "{dead:?}");
}

#[test]
fn callers_and_references_see_a_declaration_below_its_annotation() {
    // The same first-line comparison `unused` used: with the annotation on its
    // own line the declaration was not recognised as one, so `callers` reported
    // an annotated method as its own caller and `references` never flagged the
    // declaration as the definition.
    let f = Fixture::new();
    f.write(
        "G.cs",
        "public class U {\n  [Obsolete]\n  public void NextLine() { }\n}\n",
    );

    let callers = field_set(&f.run(&["callers", "NextLine"]), "name");
    assert!(
        callers.is_empty(),
        "a declaration is not a call of itself: {callers:?}"
    );

    let v = f.run(&["references", "NextLine"]);
    let rows = v["data"]["references"].as_array().unwrap();
    assert_eq!(rows.len(), 1, "{v}");
    assert_eq!(
        rows[0]["definition"], true,
        "the declaration was not flagged: {v}"
    );
}

#[test]
fn a_recursive_call_is_still_a_caller() {
    // The declaration is the first occurrence inside its extent; a later one is
    // a real call. Claiming the whole extent instead would erase recursion.
    let f = Fixture::new();
    f.write(
        "a.py",
        "def fact(n):\n    return 1 if n < 2 else n * fact(n - 1)\n",
    );

    let callers = field_set(&f.run(&["callers", "fact"]), "name");
    assert_eq!(callers, ["fact".to_string()], "recursion lost: {callers:?}");
}

#[test]
fn calls_filter_preserves_recursive_references_and_callers() {
    for (path, source, line) in [
        (
            "a.py",
            "def recur(n):\n    return recur(n - 1) if n else 0\n",
            2,
        ),
        (
            "a.rs",
            "fn recur(n: u32) -> u32 {\n    if n == 0 { 0 } else { recur(n - 1) }\n}\n",
            2,
        ),
        (
            "a.rs",
            "fn recur(n: u32) -> u32 { if n == 0 { 0 } else { recur(n - 1) } }\n",
            1,
        ),
    ] {
        let f = Fixture::new();
        f.write(path, source);

        let references = f.run(&["references", "recur", "--calls"]);
        let rows = references["data"]["references"].as_array().unwrap();
        assert_eq!(rows.len(), 1, "{references}");
        assert_eq!(rows[0]["path"], path);
        assert_eq!(rows[0]["line"], line);
        assert!(rows[0].get("definition").is_none(), "{references}");

        let callers = f.run(&["callers", "recur", "--calls"]);
        let rows = callers["data"].as_array().unwrap();
        assert_eq!(rows.len(), 1, "{callers}");
        assert_eq!(rows[0]["name"], "recur");
        assert_eq!(rows[0]["path"], path);
        assert_eq!(rows[0]["line"], line);
    }
}

#[test]
fn qualified_recursive_calls_are_not_declarations() {
    let f = Fixture::new();
    f.write(
        "a.py",
        "class Counter:\n    def recur(self, n):\n        return self.recur(n - 1) if n else 0\n",
    );
    for flags in [vec!["--on", "self"], vec!["--calls", "--on", "self"]] {
        let mut args = vec!["callers", "recur"];
        args.extend(&flags);
        let callers = f.run(&args);
        let rows = callers["data"].as_array().unwrap();
        assert_eq!(rows.len(), 1, "{callers}");
        assert_eq!(rows[0]["name"], "recur");
        assert_eq!(rows[0]["line"], 3);

        args[0] = "references";
        let references = f.run(&args);
        let rows = references["data"]["references"].as_array().unwrap();
        assert_eq!(rows.len(), 1, "{references}");
        assert_eq!(rows[0]["line"], 3);
        assert!(rows[0].get("definition").is_none(), "{references}");
        assert_eq!(rows[0]["receiver"], "self");
    }
}

#[test]
fn unused_sees_a_declaration_written_below_its_annotation() {
    // C#, Java, Kotlin and PHP put the annotation *inside* the declaration node,
    // so the fact starts at `[Fact]` and the name sits a line below. The
    // definition site was matched on that first line alone, so the declaration
    // itself read as an external use and the method could never be reported
    // dead — in the conventional formatting, which is nearly all of it.
    let f = Fixture::new();
    f.write(
        "G.cs",
        "public class U {\n  [Obsolete] public void SameLine() { }\n  [Obsolete]\n  public void NextLine() { }\n}\n",
    );
    f.write(
        "H.java",
        "public class U {\n  @Bean public void sameLine() { }\n  @Bean\n  public void nextLine() { }\n}\n",
    );

    let names = field_set(&f.run(&["unused", "--include-tests"]), "name");
    for n in ["SameLine", "NextLine", "sameLine", "nextLine"] {
        assert!(
            names.iter().any(|x| x == n),
            "'{n}' missing from unused: {names:?}"
        );
    }
}

#[test]
fn unused_still_counts_recursion_as_a_use() {
    // The declaration is the *first* occurrence of the name inside its own
    // extent. A later one in the same extent is a real call, and treating the
    // whole extent as "the definition site" would report every recursive
    // function as dead code.
    let f = Fixture::new();
    f.write(
        "a.py",
        "def fact(n):\n    return 1 if n < 2 else n * fact(n - 1)\n\ndef orphan():\n    pass\n",
    );
    f.write(
        "b.rs",
        "fn walk(n: u32) -> u32 { if n == 0 { 0 } else { walk(n - 1) } }\nfn orphan_rs() {}\n",
    );

    let names = field_set(&f.run(&["unused"]), "name");
    for live in ["fact", "walk"] {
        assert!(
            !names.iter().any(|x| x == live),
            "recursive '{live}' called dead: {names:?}"
        );
    }
    for dead in ["orphan", "orphan_rs"] {
        assert!(
            names.iter().any(|x| x == dead),
            "'{dead}' missing: {names:?}"
        );
    }
}

#[test]
fn a_python_attribute_assignment_is_not_a_decorator() {
    // `self.x = 1` has an `attribute` node on its left, and the branch reading
    // Java/C#/PHP annotations matched any kind *containing* "attribute" — so an
    // ordinary assignment came back `decorator=self._hidden`. Java and Kotlin
    // put annotations and access modifiers in one `modifiers` node, so pushing
    // it whole reported `decorator=public` on every plain `public class`. Both
    // make the tag meaningless in the place it is supposed to carry meaning.
    let f = Fixture::new();
    f.write(
        "a.py",
        "class W:\n    def __init__(self):\n        self._hidden = True\n",
    );
    f.write("C.java", "public class T {\n  public void plain() { }\n}\n");
    f.write(
        "D.java",
        "public class S {\n  @Bean public void made() { }\n}\n",
    );

    let rows = f.run(&["unused", "--include-tests"]);
    let tagged: Vec<(String, String)> = rows["data"]
        .as_array()
        .unwrap()
        .iter()
        .filter_map(|r| {
            Some((
                r["name"].as_str()?.to_string(),
                r["decorator"].as_str()?.to_string(),
            ))
        })
        .collect();
    assert!(
        tagged.iter().all(|(n, _)| n != "_hidden"),
        "an attribute assignment is not a decorator: {tagged:?}"
    );
    assert!(
        tagged.iter().all(|(_, d)| d != "public"),
        "an access modifier is not a decorator: {tagged:?}"
    );
    assert!(
        tagged
            .iter()
            .any(|(n, d)| n == "made" && d.contains("@Bean")),
        "a real annotation must still be tagged: {tagged:?}"
    );
}

#[test]
fn a_multiline_astgrep_match_keeps_its_enclosing_symbol() {
    // Every `astgrep` match is documented as carrying the `[symbol]` it sits
    // inside — that annotation is the thing grep cannot produce. A match that
    // happened to span a line fell into the verbatim-block branch meant for
    // `context` chunks and lost it, so whether a pattern spans a line decided
    // whether the answer named where it was.
    let f = Fixture::new();
    f.write(
        "a.go",
        "package p\n\nfunc handler() {\n\tif err != nil {\n\t\treturn\n\t}\n}\n",
    );

    let out = f.raw(&["astgrep", "if err != nil { return }", "--language", "go"]);
    let text = String::from_utf8_lossy(&out.stdout);
    assert!(
        text.contains("[handler]"),
        "multi-line match lost its symbol:\n{text}"
    );
}

#[test]
fn capabilities_says_which_relation_kinds_a_language_emits() {
    // `relations: true` is too coarse to act on. C# writes `class C : B, I` for
    // both inheritance and implementation and the grammar cannot tell them
    // apart, so codeq only ever emits `extends` there; Rust only ever emits
    // `implements`. Without this, an empty `--relation` result looks like a
    // statement about the code.
    let f = Fixture::new();
    f.write("a.rs", "struct S;\n");

    let v = f.run(&["capabilities"]);
    let langs = v["data"]["language_support"].as_array().unwrap();
    let kinds = |lang: &str| -> Vec<String> {
        langs
            .iter()
            .find(|r| r["language"] == lang)
            .unwrap_or_else(|| panic!("no support row for {lang}"))["relation_kinds"]
            .as_array()
            .unwrap()
            .iter()
            .map(|k| k.as_str().unwrap().to_string())
            .collect()
    };
    assert_eq!(
        kinds("csharp"),
        ["extends"],
        "C# cannot distinguish the two"
    );
    assert_eq!(kinds("rust"), ["implements"], "Rust has no `extends`");
    assert_eq!(
        kinds("java"),
        ["extends", "implements"],
        "Java has separate syntax"
    );
    assert!(kinds("go").is_empty(), "Go has no relation extraction");
}

#[test]
fn a_capped_listing_says_it_was_capped() {
    // `search` and `references` carry `truncated` in the envelope and the
    // renderer warns. A result whose `data` is a bare array has nowhere to put
    // that flag, so those commands cut in silence: `imports` defaults to 200 and
    // a real repo had 1,030, which read exactly like "this repo has 200
    // imports".
    let f = Fixture::new();
    let body: String = (0..12).map(|i| format!("import m{i}\n")).collect();
    f.write("a.py", &body);

    let out = f.raw(&["imports", "--limit", "5"]);
    let err = String::from_utf8_lossy(&out.stderr);
    assert!(
        err.contains("stopped at --limit"),
        "silent truncation: {err}"
    );
    assert_eq!(
        String::from_utf8_lossy(&out.stdout).lines().count(),
        5,
        "{out:?}"
    );

    // A result that fits says nothing.
    let out = f.raw(&["imports", "--limit", "0"]);
    assert!(!String::from_utf8_lossy(&out.stderr).contains("stopped at --limit"));
}

#[test]
fn every_capped_listing_says_so_not_just_the_ones_measured() {
    // `files`, `symbols` and `members` truncate in SQL, so they did not even
    // know they had: `symbols` defaults to 200 and a real repo answered with
    // 200 of 1,228 in silence. `LIMIT n+1` is the whole cost of telling a page
    // that fills exactly from one that was cut.
    let f = Fixture::new();
    let body: String = (0..12)
        .map(|i| format!("def thing{i}():\n    pass\n"))
        .collect();
    f.write("a.py", &body);
    f.write("b.py", &body);
    f.write("c.py", &body);

    for args in [
        vec!["files", "--limit", "2"],
        vec!["symbols", "thing", "--limit", "3"],
    ] {
        let out = f.raw(&args);
        let err = String::from_utf8_lossy(&out.stderr);
        assert!(
            err.contains("stopped at --limit"),
            "{args:?} cut in silence: {err}"
        );
        let want: usize = args.last().unwrap().parse().unwrap();
        assert_eq!(
            String::from_utf8_lossy(&out.stdout).lines().count(),
            want,
            "{args:?}"
        );
    }

    // `members` reaches its rows a different way; same guarantee.
    let g = Fixture::new();
    g.write("t.rs", "struct S { a: u8, b: u8, c: u8 }\n");
    let out = g.raw(&["members", "S", "--limit", "2"]);
    assert!(
        String::from_utf8_lossy(&out.stderr).contains("stopped at --limit"),
        "members cut in silence: {out:?}"
    );

    // And a listing that fits stays quiet.
    let out = g.raw(&["members", "S", "--limit", "0"]);
    assert!(!String::from_utf8_lossy(&out.stderr).contains("stopped at --limit"));
}

#[test]
fn unused_skips_a_method_that_exists_to_satisfy_a_trait() {
    // A Rust `impl Trait for X` method cannot carry `pub`, is invoked through
    // the trait rather than by name, and cannot be deleted without breaking the
    // build. So it is not "looks dead but might not be" — the case the
    // `decorator=` tag exists for — it is never dead. On an actix service every
    // `new_transform` and `handle` was reported as dead code.
    let f = Fixture::new();
    f.write(
        "a.rs",
        "struct S;\ntrait T { fn contract_m(&self); }\nimpl T for S { fn contract_m(&self) {} }\nimpl S { fn inherent_dead(&self) {} }\n",
    );

    let rows = f.run(&["unused"]);
    let lines: Vec<i64> = rows["data"]
        .as_array()
        .unwrap()
        .iter()
        .filter_map(|r| r["line"].as_i64())
        .collect();
    assert!(
        lines.contains(&4),
        "an inherent method with no callers is still dead: {rows}"
    );
    assert!(
        !lines.contains(&3),
        "a trait implementation is not dead code: {rows}"
    );
}

#[test]
fn unused_tags_a_framework_entry_point_instead_of_calling_it_dead() {
    // On a decorator-driven codebase this was 100% of `unused`'s output: a route's
    // only reference is the line directly above it. Tagged, not suppressed —
    // suppressing anything decorated would hide the real dead code the command
    // exists to find.
    let f = Fixture::new();
    f.write(
        "app.py",
        "from fastapi import FastAPI\napp = FastAPI()\n\n\
         @app.get(\"/personas\")\ndef gen_personas():\n    return []\n\n\
         def truly_dead():\n    return 1\n\n\
         class C:\n    @property\n    def size(self):\n        return 0\n",
    );

    let rows = f.run(&["unused"]);
    let by_name: std::collections::HashMap<&str, &Value> = rows["data"]
        .as_array()
        .unwrap()
        .iter()
        .map(|r| (r["name"].as_str().unwrap(), r))
        .collect();

    // Still reported — nothing is hidden — but now readable as a route.
    let route = by_name.get("gen_personas").expect("route still reported");
    assert!(
        route["decorator"].as_str().unwrap().starts_with("@app.get"),
        "{route}"
    );
    // Genuinely dead code carries no tag, so the two are distinguishable.
    assert!(by_name["truly_dead"]["decorator"].is_null(), "{rows}");
    // `@property` changes how a definition behaves; it does not register it
    // anywhere, so tagging it would put noise on most rows.
    assert!(by_name["size"]["decorator"].is_null(), "{rows}");
}

#[test]
fn entrypoints_lists_syntax_registered_routes_and_optional_tests() {
    let f = Fixture::new();
    f.write(
        "app.py",
        "@app.get('/health')\ndef health():\n    return {}\n\n\
         def ordinary():\n    return None\n\n\
         class State:\n    def __init__(self):\n        self.status = 'ready'\n\n\
         class Config:\n    @property\n    def value(self):\n        return 0\n\n    @value.setter\n    def value(self, value):\n        pass\n\n\
         @contextmanager\ndef transaction():\n    yield\n\n\
         @cache\ndef cached():\n    return 1\n\n\
         @click.pass_context\ndef with_context(ctx):\n    pass\n\n\
         @setupmethod\ndef guarded():\n    pass\n\n\
         @deprecated\ndef legacy():\n    pass\n\n\
         @pytest.mark.unit\ndef test_health():\n    assert health() == {}\n",
    );

    let routes = f.run(&["entrypoints"])["data"].as_array().unwrap().clone();
    assert_eq!(
        routes.len(),
        1,
        "ordinary/test definitions leaked in: {routes:?}"
    );
    assert_eq!(routes[0]["name"], "health");
    assert!(routes[0]["registration"]
        .as_str()
        .unwrap()
        .starts_with("@app.get"));
    assert!(
        routes[0]["test"].is_null(),
        "route is not a test: {routes:?}"
    );
    assert!(
        !routes.iter().any(|row| row["name"] == "status"),
        "ordinary instance fields are not decorator registrations: {routes:?}"
    );

    let all = f.run(&["entrypoints", "--include-tests"])["data"]
        .as_array()
        .unwrap()
        .clone();
    assert!(
        all.iter()
            .any(|row| row["name"] == "test_health" && row["test"] == true),
        "{all:?}"
    );
}

#[test]
fn entrypoints_path_scope_treats_like_metacharacters_literally() {
    let f = Fixture::new();
    f.write(
        "pkg/acb.py",
        "@app.get('/health')\ndef health():\n    return {}\n",
    );

    let result = f.run(&["entrypoints", "a_b"]);
    assert!(
        result["data"].as_array().unwrap().is_empty(),
        "unrelated route matched the literal path scope: {result}"
    );
}

#[test]
fn a_qualified_stdlib_decorator_is_inert_however_it_is_imported() {
    // `functools` was on the inert list, but the check compared the whole dotted
    // head (`functools.cache`) and its last segment (`cache`) — never the module
    // — so every `@functools.cache` read as a framework registration.
    let f = Fixture::new();
    f.write(
        "a.py",
        "import functools\nfrom contextlib import contextmanager\n\n\
         @functools.cache\ndef memoized():\n    return 1\n\n\
         @contextmanager\ndef scoped():\n    yield\n\n\
         @app.get(\"/x\")\ndef route():\n    return {}\n",
    );

    let by_name: std::collections::HashMap<String, String> = f.run(&["entrypoints"])["data"]
        .as_array()
        .unwrap()
        .iter()
        .map(|r| {
            (
                r["name"].as_str().unwrap().to_string(),
                r["registration"].as_str().unwrap_or_default().to_string(),
            )
        })
        .collect();
    assert!(
        !by_name.contains_key("memoized"),
        "caching is behaviour: {by_name:?}"
    );
    assert!(
        !by_name.contains_key("scoped"),
        "a context manager registers nothing: {by_name:?}"
    );
    assert!(
        by_name
            .get("route")
            .is_some_and(|r| r.starts_with("@app.get")),
        "a real registration must survive the exclusion: {by_name:?}"
    );
}

#[test]
fn entrypoints_recognises_a_file_convention_with_no_decorator_to_read() {
    // Next.js loads `app/**/page.tsx` because of where the file sits, not
    // because anything imports or decorates it. `entrypoints` reported "none" on
    // a whole App Router app, and every route read as dead code.
    let f = Fixture::new();
    f.write(
        "app/about/page.tsx",
        "export const metadata = { title: \"About\" };\n\
         export default function AboutPage() { return null; }\n",
    )
    .write(
        "app/layout.tsx",
        "export default function RootLayout({ children }) { return children; }\n",
    )
    .write(
        "components/ErrorBoundary.tsx",
        "export class ErrorBoundary extends React.Component {\n\
        \x20 componentDidCatch(e) { console.log(e); }\n\
        \x20 helper() { return 1; }\n}\n",
    )
    .write(
        "components/Unused.tsx",
        "export function Orphan() { return null; }\n",
    );

    let by_name: std::collections::HashMap<String, String> = f.run(&["entrypoints"])["data"]
        .as_array()
        .unwrap()
        .iter()
        .map(|r| {
            (
                r["name"].as_str().unwrap().to_string(),
                r["registration"].as_str().unwrap_or_default().to_string(),
            )
        })
        .collect();
    assert_eq!(
        by_name.get("AboutPage").map(String::as_str),
        Some("next/page"),
        "{by_name:?}"
    );
    assert_eq!(
        by_name.get("metadata").map(String::as_str),
        Some("next/page"),
        "{by_name:?}"
    );
    assert_eq!(
        by_name.get("RootLayout").map(String::as_str),
        Some("next/layout"),
        "{by_name:?}"
    );
    assert_eq!(
        by_name.get("componentDidCatch").map(String::as_str),
        Some("react/lifecycle"),
        "React calls it and nothing else does: {by_name:?}"
    );
    // An ordinary component and an ordinary method are not entry points.
    assert!(!by_name.contains_key("Orphan"), "{by_name:?}");
    assert!(!by_name.contains_key("helper"), "{by_name:?}");

    // Tagged in `unused`, not suppressed: deleting a route still compiles, so
    // it stays a judgement for the reader.
    let unused: std::collections::HashMap<String, Value> = f.run(&["unused"])["data"]
        .as_array()
        .unwrap()
        .iter()
        .map(|r| (r["name"].as_str().unwrap().to_string(), r.clone()))
        .collect();
    assert_eq!(unused["AboutPage"]["entrypoint"], "next/page", "{unused:?}");
    assert!(
        unused.contains_key("Orphan"),
        "real dead code still reported: {unused:?}"
    );
    assert!(unused["Orphan"]["entrypoint"].is_null(), "{unused:?}");
}

#[test]
fn arrow_functions_read_as_signatures_not_assignments() {
    // The definition node is the declarator, so the source up to the body is
    // `name = (args) =>` — an assignment fragment with a dangling arrow, on the
    // form most JS/TS functions are actually written in.
    let f = Fixture::new();
    f.write(
        "a.ts",
        "export const bare = x => x + 1;\n\
         export const typed = (n: number): string => String(n);\n\
         export const waits = async (u: string): Promise<void> => { await go(u); };\n\
         export const notAFn = { a: 1 };\n",
    );

    let sigs: std::collections::HashMap<String, String> = f.run(&["outline", "a.ts"])["data"]
        .as_array()
        .unwrap()
        .iter()
        .map(|r| {
            (
                r["name"].as_str().unwrap().to_string(),
                r["signature"].as_str().unwrap_or_default().to_string(),
            )
        })
        .collect();
    // Parentheses come back on the single-parameter form, which source omits.
    assert_eq!(sigs["bare"], "bare(x)", "{sigs:?}");
    assert_eq!(sigs["typed"], "typed(n: number): string", "{sigs:?}");
    assert_eq!(
        sigs["waits"], "async waits(u: string): Promise<void>",
        "{sigs:?}"
    );
    // A const bound to something that is not a function is untouched.
    assert!(sigs["notAFn"].starts_with("const notAFn"), "{sigs:?}");
}

#[test]
fn clones_excludes_tests_and_takes_a_positional_path() {
    // Two assertions in sequence share a shape however different the assertions
    // are, so a test suite is a field of true positives nobody wants — measured
    // on a real repo, every row `clones` returned was test boilerplate.
    let f = Fixture::new();
    let dup = |n: &str| {
        format!("fn {n}(a: u32, b: u32) -> u32 {{\n    let x = a + b;\n    let y = x * 2;\n    y - a\n}}\n")
    };
    f.write("src/a.rs", &format!("{}{}", dup("one"), dup("two")));
    f.write(
        "tests/t.rs",
        &format!("#[test]\n{}#[test]\n{}", dup("case_a"), dup("case_b")),
    );

    let names = field_set(&f.run(&["clones", "--min-nodes", "8"]), "name");
    assert!(
        names.contains(&"one".to_string()) && names.contains(&"two".to_string()),
        "{names:?}"
    );
    assert!(
        !names.iter().any(|n| n.starts_with("case_")),
        "tests excluded: {names:?}"
    );

    // Opt back in for the old behaviour.
    let all = field_set(
        &f.run(&["clones", "--min-nodes", "8", "--include-tests"]),
        "name",
    );
    assert!(all.iter().any(|n| n.starts_with("case_")), "{all:?}");

    // Positional path, like `unused`/`untested`/`hotspots`/`tree`. `clones` was
    // the one command in that family that insisted on `--path`.
    let scoped = field_set(&f.run(&["clones", "src", "--min-nodes", "8"]), "name");
    assert_eq!(scoped.len(), 2, "{scoped:?}");
    assert!(
        !f.raw(&["clones", "--path", "src"]).status.success(),
        "--path is gone"
    );
}

#[test]
fn tree_and_map_can_separate_code_from_the_data_around_it() {
    // `--min-symbols` is documented as the way into an asset-heavy repo, and a
    // markdown heading is a definition — so a tree of per-directory READMEs
    // outranks the source and the one knob offered cannot separate them.
    let f = Fixture::new();
    f.write("src/lib.py", "def compute():\n    return 1\n");
    for i in 0..6 {
        f.write(
            &format!("data/set{i}/README.md"),
            "# A\n## B\n## C\n### D\n",
        );
    }

    let all = field_set(&f.run(&["tree", "--min-symbols", "1"]), "path");
    assert!(
        all.iter().any(|d| d.starts_with("data")),
        "prose outranks code: {all:?}"
    );

    let code = field_set(
        &f.run(&["tree", "--language", "python", "--min-symbols", "1"]),
        "path",
    );
    assert!(code.iter().any(|d| d == "src"), "{code:?}");
    assert!(!code.iter().any(|d| d.starts_with("data")), "{code:?}");
}

#[test]
fn path_filters_treat_like_metacharacters_as_literal_path_chars() {
    let f = Fixture::new();
    f.write("pkg/a_b.rs", "fn wanted() {}\n");
    f.write("pkg/acb.rs", "fn wildcard_match() {}\n");
    f.write("pkg/a%b.rs", "fn percent_name() {}\n");

    let paths = |path: &str| field_set(&f.run(&["files", path]), "path");
    for (filter, expected) in [("a_b", "pkg/a_b.rs"), ("a%b", "pkg/a%b.rs")] {
        let got = paths(filter);
        assert_eq!(got.len(), 1, "filter {filter}: {got:?}");
        assert_eq!(got[0], expected, "filter {filter}: {got:?}");
    }
}

#[test]
fn symbols_path_scope_treats_like_metacharacters_literally() {
    let f = Fixture::new();
    f.write("pkg/acb.rs", "fn unrelated() {}\n");

    let result = f.run(&["symbols", "--path", "a_b"]);
    assert!(
        result["data"].as_array().unwrap().is_empty(),
        "an unrelated symbol leaked through the path scope: {result}"
    );
}

#[test]
fn callees_path_scope_treats_like_metacharacters_literally() {
    let f = Fixture::new();
    f.write("pkg/acb.rs", "fn target() { leaf(); }\nfn leaf() {}\n");

    let result = f.run(&["callees", "target", "--path", "a_b"]);
    assert!(
        result["data"].as_array().unwrap().is_empty(),
        "an unrelated definition leaked through the path scope: {result}"
    );
}

#[test]
fn imports_path_scope_treats_like_metacharacters_literally() {
    let f = Fixture::new();
    f.write("pkg/acb.py", "import external_package\n");

    let result = f.run(&["imports", "a_b"]);
    assert!(
        result["data"].as_array().unwrap().is_empty(),
        "an unrelated file leaked through the import path scope: {result}"
    );
    assert!(
        result["note"]
            .as_str()
            .unwrap()
            .contains("no indexed file matches 'a_b'"),
        "scope note should use the same literal path matching: {result}"
    );
}

#[test]
fn file_commands_resolve_repository_path_forms() {
    let f = Fixture::new();
    f.write("pkg/sample.py", "def hello():\n    return 1\n");
    f.write("other/sample.py", "def decoy():\n    return 2\n");
    f.git_init_commit();
    let absolute = f.path().join("pkg/sample.py");
    for command in ["read", "outline", "at"] {
        let argument = |path: &str| {
            if command == "at" {
                format!("{path}:2")
            } else {
                path.to_string()
            }
        };
        let expected = f.raw(&[command, &argument("pkg/sample.py")]);
        assert!(expected.status.success());
        assert!(String::from_utf8_lossy(&expected.stdout).contains("hello"));
        for path in [
            "./pkg/sample.py",
            "pkg/../pkg/sample.py",
            absolute.to_str().unwrap(),
        ] {
            let result = f.raw(&[command, &argument(path)]);
            assert!(
                result.status.success(),
                "{command} {path}: {}",
                String::from_utf8_lossy(&result.stderr)
            );
            assert_eq!(result.stdout, expected.stdout, "{command} {path}");
        }
        for path in ["./sample.py", "../pkg/sample.py"] {
            let output = Command::cargo_bin("codeq")
                .unwrap()
                .current_dir(f.path().join("pkg"))
                .args([command, &argument(path)])
                .output()
                .unwrap();
            assert!(
                output.status.success(),
                "{command} {path}: {}",
                String::from_utf8_lossy(&output.stderr)
            );
            assert_eq!(output.stdout, expected.stdout, "{command} {path}");
        }
        let ambiguous = f.raw(&[command, &argument("sample.py")]);
        assert!(!ambiguous.status.success());
        assert!(String::from_utf8_lossy(&ambiguous.stderr).contains("ambiguous"));
    }

    f.write(".codeqignore", "ignored.py\n");
    f.write("ignored.py", "def ignored():\n    pass\n");
    let ignored = f.raw(&["outline", f.path().join("ignored.py").to_str().unwrap()]);
    assert!(!ignored.status.success());
    assert!(String::from_utf8_lossy(&ignored.stderr).contains("not in this index"));
}

#[test]
fn file_suffix_resolution_treats_like_metacharacters_literally() {
    let f = Fixture::new();
    f.write("pkg/a_b.rs", "fn wanted() {}\n");
    f.write("pkg/acb.rs", "fn wildcard_match() {}\n");

    let outline = f.run(&["outline", "a_b.rs"]);
    assert_eq!(outline["data"].as_array().unwrap().len(), 1, "{outline}");
    assert_eq!(outline["data"][0]["name"], "wanted", "{outline}");
}

#[test]
fn map_says_when_a_repo_is_mostly_things_it_cannot_index() {
    // The `(unknown)` row is accurate and still reads like a gap in coverage
    // rather than "this repo is mostly images".
    let f = Fixture::new();
    f.write("src/lib.py", "def compute():\n    return 1\n");
    for i in 0..8 {
        f.write_bytes(
            &format!("assets/flag{i}.png"),
            &[0x89, b'P', b'N', b'G', 0, 1, 2, 3],
        );
    }

    let out = f.raw(&["map", "--totals"]);
    let note = String::from_utf8_lossy(&out.stderr);
    assert!(note.contains("carry no symbols"), "{note}");
    assert!(note.contains("of 9 files"), "{note}");
    // The extension is the fact; the category was a guess.
    assert!(
        note.contains(".png (8)"),
        "note should name what the bulk is: {note}"
    );
    // Under a megabyte the quantities have to survive the unit.
    assert!(
        !note.contains("0 MB"),
        "sub-MB totals rendered as zero: {note}"
    );

    // A repo that is mostly code says nothing — the note is for the case that
    // misleads, not a banner on every call.
    let g = Fixture::new();
    g.write("a.py", "def x():\n    pass\n");
    assert!(
        !String::from_utf8_lossy(&g.raw(&["map", "--totals"]).stderr).contains("carry no symbols")
    );
}

#[test]
fn map_says_when_a_committed_data_tree_owns_the_index() {
    // The `(unknown)` note warns about the *free* case: a .png has no grammar,
    // so it is never content-indexed. A .json data tree is text, goes into the
    // trigram index whole, and is what actually blows the index up — 14.5 MB of
    // backups turned a 700 KB codebase into a 129 MB index.db, silently.
    let f = Fixture::new();
    f.write("src/lib.py", "def compute():\n    return 1\n");
    let blob = format!(
        "[{}]",
        vec!["{\"team\":\"x\",\"score\":1}"; 30_000].join(",")
    );
    f.write("updater/backups/general_2024.json", &blob);
    f.write("updater/backups/general_2025.json", &blob);

    let note = String::from_utf8_lossy(&f.raw(&["map", "--totals"]).stderr).to_string();
    assert!(note.contains("json that defines nothing"), "{note}");
    // Actionable only if it says where to look and what to do.
    assert!(note.contains("updater/backups/ (2 files)"), "{note}");
    assert!(note.contains(".codeqignore"), "{note}");

    // A repo whose data is a rounding error says nothing.
    let g = Fixture::new();
    g.write("a.py", "def x():\n    pass\n");
    g.write("package.json", "{\"name\":\"x\"}\n");
    let quiet = String::from_utf8_lossy(&g.raw(&["map", "--totals"]).stderr).to_string();
    assert!(!quiet.contains("defines nothing"), "{quiet}");
}

#[test]
fn map_says_when_generated_source_owns_most_of_the_symbols() {
    // The unindexable-assets note cannot see this case: a committed vendor
    // bundle *is* source, *is* parsed, and *does* produce symbols, so it costs
    // nothing in index size and quietly owns every symbol-ranking command. On a
    // real dashboard repo three files held 5,507 of 7,558 symbols and
    // `hotspots` answered `function t refs=45993`.
    let f = Fixture::new();
    f.write(
        "src/app.ts",
        "export function compute() {\n    return 1;\n}\n",
    );
    // One line, many declarations — what minification produces.
    let minified: String = (0..80)
        .map(|i| format!("const v{i}=()=>{i};"))
        .collect::<Vec<_>>()
        .join("");
    f.write("vendor/bundle.min.js", &format!("{minified}\n"));

    let note = String::from_utf8_lossy(&f.raw(&["map", "--totals"]).stderr).to_string();
    assert!(
        note.contains("vendor/bundle.min.js"),
        "should name the file: {note}"
    );
    assert!(note.contains("stacked on a few lines"), "{note}");
    assert!(
        note.contains("hotspots"),
        "should say what it distorts: {note}"
    );

    // Hand-written code declares about one thing per line, so an ordinary repo
    // stays silent — the ratio is bounded above by the grammar, not tuned.
    let g = Fixture::new();
    g.write("a.ts", "export function a() {}\nexport function b() {}\n");
    assert!(
        !String::from_utf8_lossy(&g.raw(&["map", "--totals"]).stderr)
            .contains("stacked on a few lines")
    );
}

#[test]
fn map_names_an_unsupported_language_rather_than_calling_it_data() {
    // A Business Central repo is 31 `.al` files — its entire source. Describing
    // them as "assets, data or binaries" was wrong in the direction that costs
    // most: it told the author their code was data, and implicitly proposed
    // `.codeqignore` for it. The extension tells a geospatial dump and an
    // unsupported language apart without codeq having to know which it sees.
    let f = Fixture::new();
    f.write("README.md", "# Docs\n");
    for i in 0..6 {
        f.write(&format!("src/Table{i}.al"), "table 50100 Thing\n{\n}\n");
    }

    let note = String::from_utf8_lossy(&f.raw(&["map", "--totals"]).stderr).to_string();
    assert!(note.contains("codeq has no grammar for"), "{note}");
    assert!(note.contains(".al (6)"), "{note}");
    assert!(
        !note.contains("assets"),
        "the category was the guess: {note}"
    );
}

#[test]
fn a_python_module_scope_binding_is_a_definition() {
    // JS module-scope bindings are definitions (the docs argue it at length) and
    // Python class-body assignments already extract as fields. Module scope was
    // the gap, which is where every settings constant in every Python project
    // lives — `definitions DATA_DIR` answered "no symbol named", the message
    // documented to mean "you misspelled it".
    let f = Fixture::new();
    f.write(
        "lib.py",
        "from pathlib import Path\n\
         DATA_DIR = Path(__file__).parent / \"data\"\n\
         ALIAS = {\n    \"a\": 1,\n    \"b\": 2,\n}\n\
         COUNT: int = 5\n\
         LATER: int\n\
         a, b = 1, 2\n\n\
         def fn():\n    local_thing = 1\n    return local_thing\n",
    );

    let names = field_set(&f.run(&["symbols", "--limit", "0"]), "name");
    for want in ["DATA_DIR", "ALIAS", "COUNT"] {
        assert!(
            names.contains(&want.to_string()),
            "{want} missing: {names:?}"
        );
    }
    // Module scope only, and only real bindings.
    assert!(
        !names.contains(&"local_thing".to_string()),
        "a local: {names:?}"
    );
    assert!(
        !names.contains(&"LATER".to_string()),
        "annotation, not a binding: {names:?}"
    );
    assert!(
        !names.contains(&"a".to_string()),
        "destructuring declares nothing here: {names:?}"
    );

    // The commands above it work again, which is how this failed: quietly.
    let defs = f.run(&["definitions", "DATA_DIR"]);
    assert_eq!(defs["data"].as_array().unwrap().len(), 1, "{defs}");
    let outline = field_set(&f.run(&["outline", "lib.py"]), "name");
    assert!(outline.contains(&"ALIAS".to_string()), "{outline:?}");
    // A multi-line value stops the signature where it begins, as for JS.
    let sig = defs["data"][0]["signature"].as_str().unwrap();
    assert!(sig.contains("DATA_DIR"), "{sig}");
}

#[test]
fn rust_items_in_nonpublic_modules_are_not_external_api() {
    let f = Fixture::new();
    f.write(
        "lib.rs",
        r#"pub fn visible() {}
pub mod external {
    pub fn exposed() {}
}
pub(crate) mod internal {
    pub fn hidden() {}
    pub struct Hidden { pub value: u8 }
    impl Hidden { pub fn method(&self) {} }
    pub enum State { Ready }
    pub mod nested { pub fn deeply_hidden() {} }
    fn local() {}
}
mod private {
    pub(super) mod bridge { pub fn secret() {} }
}
pub(super) mod parent_only { pub fn parent_hidden() {} }
pub(in crate) mod scoped { pub fn scoped_hidden() {} }
"#,
    );
    let mut api = field_set(&f.run(&["api", "lib.rs"]), "name");
    api.sort();
    assert_eq!(api, ["exposed", "external", "visible"]);

    let symbols = field_set(&f.run(&["symbols", "--path", "lib.rs"]), "name");
    for name in [
        "hidden",
        "Hidden",
        "value",
        "method",
        "State",
        "Ready",
        "nested",
        "deeply_hidden",
        "local",
        "bridge",
        "secret",
        "parent_hidden",
        "scoped_hidden",
    ] {
        assert!(symbols.contains(&name.to_string()), "{name}: {symbols:?}");
    }
    let mut untested = field_set(&f.run(&["untested", "lib.rs"]), "name");
    untested.sort();
    assert_eq!(untested, ["exposed", "external", "visible"]);
}

#[test]
fn python_function_local_definitions_are_private() {
    let f = Fixture::new();
    f.write(
        "sample.py",
        concat!(
            "def outer():\n",
            "    def inner():\n        return 3\n",
            "    class Local:\n",
            "        flag = 1\n",
            "        def run(self):\n            self.hidden = 2\n",
            "    return 1\n",
            "class Public:\n",
            "    def __init__(self):\n        self.value = 1\n",
            "    def method(self):\n",
            "        def helper():\n            return 2\n",
            "        return 1\n",
            "    class Nested:\n        def nested_method(self):\n            pass\n",
            "async def async_outer():\n",
            "    @decorate\n    async def async_inner():\n        return 1\n",
            "    return 1\n",
        ),
    );
    let mut api = field_set(&f.run(&["api", "sample.py"]), "name");
    api.sort();
    assert_eq!(
        api,
        [
            "Nested",
            "Public",
            "__init__",
            "async_outer",
            "method",
            "nested_method",
            "outer",
            "value"
        ]
    );

    let symbols = field_set(&f.run(&["symbols", "--path", "sample.py"]), "name");
    let locals = [
        "inner",
        "Local",
        "flag",
        "run",
        "hidden",
        "helper",
        "async_inner",
    ];
    for name in locals {
        assert!(symbols.contains(&name.to_string()), "{name}: {symbols:?}");
    }
    let untested = field_set(&f.run(&["untested", "sample.py"]), "name");
    assert!(untested.contains(&"outer".to_string()), "{untested:?}");
    for name in locals {
        assert!(
            !untested.contains(&name.to_string()),
            "{name}: {untested:?}"
        );
    }
}

#[test]
fn a_member_of_a_private_type_is_not_public_api() {
    let f = Fixture::new();
    f.write(
        "lib.py",
        "class _Draw:\n    value = 1\n    def method(self):\n        return 2\n\n\
         class Public:\n    size = 1\n    def visible(self):\n        return 3\n",
    );

    let api = field_set(&f.run(&["api", "lib.py"]), "name");
    assert!(api.contains(&"Public".to_string()), "{api:?}");
    assert!(
        api.contains(&"size".to_string()) && api.contains(&"visible".to_string()),
        "{api:?}"
    );
    for hidden in ["_Draw", "value", "method"] {
        assert!(
            !api.contains(&hidden.to_string()),
            "{hidden} is inside a private type: {api:?}"
        );
    }
    // `untested` is "public only", so it must agree.
    let untested = field_set(&f.run(&["untested", "lib.py"]), "name");
    assert!(!untested.contains(&"method".to_string()), "{untested:?}");
}

#[test]
fn an_external_name_is_not_reported_as_a_typo() {
    // An agent deleted an import, ran `references Path --path scripts`, read "no
    // symbol named 'Path'" as confirmation it was gone, and moved on. The message
    // was answering a different question.
    let f = Fixture::new();
    f.write("lib.py", "from pathlib import Path\nDATA = Path(\".\")\n");

    let out = f.raw(&["references", "Path", "--path", "nosuch"]);
    let note = String::from_utf8_lossy(&out.stderr);
    assert!(note.contains("no definition codeq indexes"), "{note}");
    assert!(!note.contains("no symbol named"), "wrong branch: {note}");

    // A genuine typo still gets the typo message.
    let typo_out = f.raw(&["references", "Pathhh"]);
    let typo = String::from_utf8_lossy(&typo_out.stderr);
    assert!(typo.contains("no symbol named 'Pathhh'"), "{typo}");
    // And an in-repo name narrowed to nothing still blames the filter.
    let scoped_out = f.raw(&["references", "DATA", "--path", "nosuch"]);
    let scoped = String::from_utf8_lossy(&scoped_out.stderr);
    assert!(scoped.contains("nothing matches --path"), "{scoped}");
}

#[test]
fn an_impacted_row_points_at_the_definition_not_the_call_site() {
    // `changed` rows carried the definition's line and `impacted` rows the call
    // site's — one command, one key, two meanings. Click the line and you land
    // mid-body.
    let f = Fixture::new();
    f.write(
        "mod.py",
        "def touched():\n    return 1\n\ndef caller():\n    return touched()\n",
    );
    f.git_init_commit();
    f.write(
        "mod.py",
        "def touched():\n    return 2\n\ndef caller():\n    return touched()\n",
    );

    let v = f.run(&["affected", "--compare-to", "HEAD"]);
    let imp = &v["data"]["impacted"][0];
    assert_eq!(imp["name"], "caller", "{v}");
    assert_eq!(imp["line"], 4, "the definition's own line: {v}");
    assert_eq!(imp["call_line"], 5, "where the impact enters it: {v}");
}

#[test]
fn explore_at_refuses_a_line_that_declares_several_things() {
    // A single line can declare several definitions — Ruby's `attr_accessor :a,
    // :b`, a C++ enum's variants — and they tie for innermost because they share
    // a span. `definition_at` took the first row, so asking about the line
    // silently explored whichever name sorted first: ask about `response`, get
    // `app`. `at` returned all five and was right to.
    let f = Fixture::new();
    f.write(
        "base.rb",
        "class Base\n  attr_accessor :app, :env, :request\n\n  def run\n    app\n  end\nend\n",
    );

    // `at` reports all three, as it always did.
    let at = field_set(&f.run(&["at", "base.rb:2"]), "name");
    for n in ["app", "env", "request"] {
        assert!(at.contains(&n.to_string()), "{at:?}");
    }

    // `explore --at` refuses rather than picking one, and says why.
    let out = f.raw(&["explore", "--at", "base.rb:2", "--max-tokens", "200"]);
    assert!(out.status.success(), "a refusal is an answer, not an error");
    let note = String::from_utf8_lossy(&out.stderr);
    assert!(note.contains("declares 3 definitions"), "{note}");
    assert!(note.contains("app") && note.contains("request"), "{note}");
    let v = f.run(&["explore", "--at", "base.rb:2", "--max-tokens", "200"]);
    assert!(v["data"]["focus"].as_array().unwrap().is_empty(), "{v}");

    // A line declaring one thing still resolves.
    let v = f.run(&["explore", "--at", "base.rb:4", "--max-tokens", "200"]);
    let focus = v["data"]["focus"].as_array().unwrap();
    assert!(focus.iter().any(|d| d["name"] == "run"), "{v}");
}

#[test]
fn api_diff_obeys_the_row_limit_and_reports_truncation() {
    let f = Fixture::new();
    f.write("lib.rs", "pub fn old_a() {}\npub fn old_b() {}\n");
    f.git_init_commit();
    f.write(
        "lib.rs",
        "pub fn new_a() {}\npub fn new_b() {}\npub fn new_c() {}\n",
    );

    let v = f.run(&["api", "--compare-to", "HEAD", "--limit", "2"]);
    assert_eq!(v["data"]["removed"].as_array().unwrap().len(), 2, "{v}");
    assert!(v["data"]["added"].as_array().unwrap().is_empty(), "{v}");
    assert_eq!(v["data"]["truncated"], true, "{v}");
}

#[test]
fn api_diff_obeys_the_kind_filter() {
    let f = Fixture::new();
    f.write(
        "lib.rs",
        "pub fn before() {}\npub struct Widget { pub value: i32 }\n",
    );
    f.git_init_commit();
    f.write(
        "lib.rs",
        "pub fn after() {}\npub struct Gadget { pub value: i32 }\n",
    );

    let v = f.run(&["api", "--compare-to", "HEAD", "--kind", "function"]);
    for section in ["removed", "changed", "added"] {
        for row in v["data"][section].as_array().unwrap() {
            assert_eq!(row["kind"], "function", "kind filter leaked a row: {v}");
        }
    }
    assert_eq!(v["data"]["removed"][0]["name"], "before", "{v}");
    assert_eq!(v["data"]["added"][0]["name"], "after", "{v}");
}

#[test]
fn api_diff_places_a_removed_export_on_the_line_it_had() {
    // A removed export has no line in the tree, so its row said `line 0`, which
    // no reader can open. The line it had at the compared ref is the real place.
    let f = Fixture::new();
    f.write(
        "lib.rs",
        "pub fn keep() {}\n\npub fn gone() {}\n\npub fn also() {}\n",
    );
    f.git_init_commit();
    f.write("lib.rs", "pub fn keep() {}\n\npub fn also() {}\n");

    let v = f.run(&["api", "--compare-to", "HEAD"]);
    let removed = v["data"]["removed"].as_array().unwrap();
    assert_eq!(removed.len(), 1, "{v}");
    assert_eq!(removed[0]["name"], "gone", "{v}");
    assert_eq!(removed[0]["line"], 3, "{v}");
}

#[test]
fn api_diff_reports_an_added_overload_as_added() {
    let f = Fixture::new();
    f.write(
        "Api.java",
        "public class Api {\n    public void run() {}\n}\n",
    );
    f.git_init_commit();
    f.write(
        "Api.java",
        "public class Api {\n    public void run() {}\n    public void run(int value) {}\n}\n",
    );

    let v = f.run(&["api", "--compare-to", "HEAD"]);
    let added = v["data"]["added"].as_array().unwrap();
    assert!(
        added
            .iter()
            .any(|row| row["name"] == "run"
                && row["signature"].as_str().unwrap().contains("int value")),
        "the new overload should be an added export: {v}"
    );

    f.git(&["add", "Api.java"]);
    f.git(&["commit", "-q", "-m", "add overload"]);
    f.write(
        "Api.java",
        "public class Api {\n    public void run() {}\n}\n",
    );
    let v = f.run(&["api", "--compare-to", "HEAD"]);
    assert!(
        v["data"]["removed"]
            .as_array()
            .unwrap()
            .iter()
            .any(|row| row["name"] == "run" && row["was"].as_str().unwrap().contains("int value")),
        "the removed overload should be a removed export: {v}"
    );
}

#[test]
fn a_moved_and_reexported_definition_is_not_a_removed_export() {
    // Moving a definition down a layer and importing it back keeps every
    // `from handler import resolve_key` working, so the surface is unchanged —
    // but a definitions-only comparison called it a removed public export, which
    // is the answer that would block a PR on a non-breaking change.
    let f = Fixture::new();
    f.write(
        "pkg/handler.py",
        "def helper():\n    return 1\n\ndef resolve_key(n):\n    return n\n",
    );
    f.write("pkg/generate.py", "def other():\n    return 2\n");
    f.git_init_commit();
    f.write(
        "pkg/handler.py",
        "from pkg.generate import resolve_key\n\ndef helper():\n    return 1\n",
    );
    f.write(
        "pkg/generate.py",
        "def other():\n    return 2\n\ndef resolve_key(n):\n    return n\n",
    );

    let names = |v: &Value, section: &str| -> Vec<String> {
        v["data"][section]
            .as_array()
            .map(|a| {
                a.iter()
                    .filter_map(|r| r["name"].as_str().map(str::to_string))
                    .collect()
            })
            .unwrap_or_default()
    };

    let v = f.run(&["api", "pkg/handler.py", "--compare-to", "HEAD"]);
    assert!(
        !names(&v, "removed").contains(&"resolve_key".to_string()),
        "still exported: {v}"
    );
    let out = f.raw(&["api", "pkg/handler.py", "--compare-to", "HEAD"]);
    let note = String::from_utf8_lossy(&out.stderr);
    assert!(note.contains("still exported, now via an import"), "{note}");
    assert!(note.contains("resolve_key"), "{note}");

    // A genuine removal is still a removal.
    f.write("pkg/handler.py", "def helper():\n    return 1\n");
    let v = f.run(&["api", "pkg/handler.py", "--compare-to", "HEAD"]);
    assert!(
        names(&v, "removed").contains(&"resolve_key".to_string()),
        "{v}"
    );
}

#[test]
fn clone_rows_carry_the_directories_their_group_spans() {
    // On a monorepo the biggest clone groups are deliberate vendoring, and they
    // bury the accidental ones. One value per group to dismiss, rather than one
    // judgement per row — and no config file encoding it, which would make an
    // empty result unfalsifiable without reading the file.
    let f = Fixture::new();
    let dup = |n: &str| {
        format!("fn {n}(a: u32, b: u32) -> u32 {{\n    let x = a + b;\n    let y = x * 2;\n    y - a\n}}\n")
    };
    f.write("core/a.rs", &dup("one"));
    f.write("vendor/b.rs", &dup("two"));
    f.write("core/c.rs", &format!("{}{}", dup("three"), dup("four")));

    let rows = f.run(&["clones", "--min-nodes", "8"]);
    let by_name: std::collections::HashMap<&str, &str> = rows["data"]
        .as_array()
        .unwrap()
        .iter()
        .map(|r| (r["name"].as_str().unwrap(), r["pair"].as_str().unwrap()))
        .collect();
    // Every row of a group carries the same span, so it groups.
    assert_eq!(by_name["one"], by_name["two"], "{rows}");
    assert!(
        by_name["one"].contains("core") && by_name["one"].contains("vendor"),
        "{rows}"
    );
    assert!(
        by_name["one"].contains("<->"),
        "a cross-directory group: {rows}"
    );
}

#[test]
fn astgrep_path_scopes_instead_of_emptying_the_result() {
    // `--path` returned nothing for every scope form, with exit 0. `content_fts`
    // is an FTS5 table tokenized with `trigram`, and the trigram tokenizer takes
    // over `LIKE` on the table — so `path LIKE ?` against an UNINDEXED column
    // matched nothing at all. It read as "astgrep can't express that", and the
    // documented defence (`--explain` on a line you know matches) does not help:
    // the tree was perfect while the scoped query stayed empty.
    let f = Fixture::new();
    f.write(
        "src/a.rs",
        "fn a() { let x: Option<u32> = None; x.unwrap(); }\n",
    );
    f.write(
        "src/b.rs",
        "fn b() { let y: Option<u32> = None; y.unwrap(); }\n",
    );
    f.write(
        "tests/t.rs",
        "fn t() { let z: Option<u32> = None; z.unwrap(); }\n",
    );

    let n = |args: &[&str]| {
        let mut v = vec![
            "astgrep",
            "$X.unwrap()",
            "--language",
            "rust",
            "--limit",
            "0",
        ];
        v.extend_from_slice(args);
        f.run(&v)["data"]["matches"].as_array().unwrap().len()
    };
    assert_eq!(n(&[]), 3, "unscoped");
    assert_eq!(n(&["--path", "src"]), 2, "a directory");
    assert_eq!(n(&["--path", "src/a.rs"]), 1, "an exact file");
    assert_eq!(n(&["--path", "a.rs"]), 1, "a bare substring");
    assert_eq!(n(&["--path", "tests"]), 1);

    // A `--path` matching no file says so, instead of looking like "no matches".
    let out = f.raw(&[
        "astgrep",
        "$X.unwrap()",
        "--language",
        "rust",
        "--path",
        "nosuchdir",
    ]);
    assert!(out.status.success());
    let note = String::from_utf8_lossy(&out.stderr);
    assert!(
        note.contains("no indexed rust file matches --path nosuchdir"),
        "{note}"
    );
    // A real path where the pattern genuinely misses stays quiet — the note is
    // for the case that misleads, not a banner on every empty result.
    let quiet = f.raw(&[
        "astgrep",
        "$X.never_called()",
        "--language",
        "rust",
        "--path",
        "src",
    ]);
    assert!(!String::from_utf8_lossy(&quiet.stderr).contains("matches --path"));
}

#[test]
fn a_file_outside_the_index_says_so_rather_than_not_found() {
    // An absolute path into another checkout exists on disk and is simply not
    // indexed here. "file not found" sends you checking the path; naming the
    // state sends you to the fix.
    let f = Fixture::new();
    f.write("a.rs", "fn a() {}\n");
    let outside = f.path().parent().unwrap().join("definitely-not-here.rs");
    std::fs::write(&outside, "fn b() {}\n").unwrap();

    let out = f.raw(&["outline", outside.to_str().unwrap()]);
    assert!(!out.status.success());
    let err = String::from_utf8_lossy(&out.stderr);
    assert!(err.contains("not in this index"), "{err}");
    assert!(
        err.contains("--root"),
        "it should say how to reach it: {err}"
    );

    // A path that really doesn't exist keeps the plain message.
    let gone = f.raw(&["outline", "nosuchfile.rs"]);
    assert!(String::from_utf8_lossy(&gone.stderr).contains("file not found in this index"));
    let _ = std::fs::remove_file(&outside);
}

#[test]
fn callpath_places_an_ambiguous_start_on_the_definition_that_reaches_the_target() {
    // `main` is defined in a.ts (which calls nothing) and b.py (which calls
    // helper). The chain goes through b.py, so that is where it must start.
    let f = Fixture::new();
    f.write("a.ts", "export function main() { return 1 }\n");
    f.write(
        "b.py",
        "def helper():\n    return 1\ndef main():\n    return helper()\n",
    );
    let v = f.run(&["callpath", "main", "helper"]);
    let steps = v["data"].as_array().unwrap();
    assert_eq!(steps.len(), 2, "{v}");
    assert_eq!(steps[0]["path"], "b.py", "{v}");
    assert_eq!(steps[0]["line"], 3, "{v}");
}

#[test]
fn callpath_says_which_of_the_four_reasons_it_found_nothing() {
    // The command where an empty answer has the most plausible causes — a
    // misspelled FROM, a misspelled TO, a real pair that does not connect, or a
    // chain past --max-depth — and it distinguished none of them: no stdout, no
    // stderr, exit 0, and no `note` under --json either.
    let f = Fixture::new();
    f.write(
        "a.py",
        "def alpha():\n    return 1\n\ndef beta():\n    return 2\n",
    );

    let note = |args: &[&str]| -> String {
        let mut v = vec!["callpath"];
        v.extend_from_slice(args);
        String::from_utf8_lossy(&f.raw(&v).stderr).to_string()
    };
    assert!(note(&["zzznotreal", "alpha"]).contains("(the FROM end)"));
    assert!(note(&["alpha", "zzznotreal"]).contains("(the TO end)"));
    assert!(note(&["zzznotreal", "alsonotreal"]).contains("neither"));
    let unreachable = note(&["alpha", "beta"]);
    assert!(unreachable.contains("both are defined"), "{unreachable}");
    assert!(
        unreachable.contains("--max-depth"),
        "raise it or give up: {unreachable}"
    );

    // The note reaches --json too, where there was nothing at all before.
    let v = f.run(&["callpath", "alpha", "beta"]);
    assert!(
        v["note"]
            .as_str()
            .unwrap_or_default()
            .contains("both are defined"),
        "{v}"
    );
}

#[test]
fn an_empty_graph_explains_this_repo_not_some_other_one() {
    // The note named Go, C and Zig unconditionally, so on a Python-only tree it
    // read as "your language is unsupported" when the truth was "your code has
    // no edges".
    let f = Fixture::new();
    f.write("a.py", "class A:\n    pass\n\nclass B:\n    pass\n");
    let py = String::from_utf8_lossy(&f.raw(&["graph"]).stderr).to_string();
    assert!(py.contains("python does produce them"), "{py}");
    assert!(
        !py.contains("Zig"),
        "not about languages that aren't here: {py}"
    );

    let g = Fixture::new();
    g.write("m.go", "package main\n\nfunc main() {}\n");
    let go = String::from_utf8_lossy(&g.raw(&["graph"]).stderr).to_string();
    assert!(go.contains("no inheritance edges for go"), "{go}");
    assert!(go.contains("not evidence"), "{go}");
}

#[test]
fn python_names_the_interpreter_reads_are_not_api_or_dead_code() {
    // `__all__` is declared in every package that curates its exports and named
    // nowhere else, by construction — the same sentence that already excludes
    // SQL indexes. `__version__` is not: that one gets referenced.
    let f = Fixture::new();
    f.write(
        "pkg/__init__.py",
        "__all__ = [\"a\"]\n__version__ = \"1.0\"\n\nclass Hooks:\n    def __enter__(self):\n        return self\n\n    def __get__(self, obj, owner):\n        return obj\n\n    def __post_init__(self):\n        pass\n\n    def dead(self):\n        pass\n\ndef a():\n    return 1\n",
    );

    let dead = field_set(&f.run(&["unused"]), "name");
    assert!(!dead.contains(&"__all__".to_string()), "{dead:?}");
    for hook in ["__enter__", "__get__", "__post_init__"] {
        assert!(
            !dead.contains(&hook.to_string()),
            "runtime hook {hook}: {dead:?}"
        );
    }
    assert!(
        dead.contains(&"dead".to_string()),
        "ordinary method: {dead:?}"
    );
    assert!(
        dead.contains(&"__version__".to_string()),
        "not a blanket dunder rule: {dead:?}"
    );

    let api = field_set(&f.run(&["api", "pkg/__init__.py"]), "name");
    assert!(
        !api.contains(&"__all__".to_string()),
        "metadata, not surface: {api:?}"
    );
    assert!(api.contains(&"a".to_string()), "{api:?}");
}

#[test]
fn a_heading_copied_out_of_outline_works_as_a_symbol() {
    // `outline` renders a heading's signature (`## Heading`); `--symbol` stores
    // it bare. Copy-paste between them is the obvious workflow and it failed.
    let f = Fixture::new();
    f.write(
        "doc.md",
        "# Title\n\nintro\n\n## Second Part\n\nbody text\n",
    );

    // `read` prints source, not the row envelope, so compare the bytes.
    let bare = f.raw(&["read", "doc.md", "--symbol", "Second Part"]);
    assert!(bare.status.success());
    let bare_out = String::from_utf8_lossy(&bare.stdout).to_string();
    assert!(bare_out.contains("body text"), "{bare_out}");
    let copied = f.raw(&["read", "doc.md", "--symbol", "## Second Part"]);
    assert!(
        copied.status.success(),
        "the form `outline` prints must work too"
    );
    assert_eq!(
        String::from_utf8_lossy(&copied.stdout),
        bare_out,
        "both forms, one answer"
    );

    // A name that genuinely doesn't exist still fails.
    assert!(!f
        .raw(&["read", "doc.md", "--symbol", "## Nope"])
        .status
        .success());
}

#[test]
fn a_folded_multiline_constant_says_what_it_folded() {
    // A 52-entry dict rendered as a bare `alias =`, which reads like the
    // renderer gave up rather than like a deliberate fold.
    let f = Fixture::new();
    f.write(
        "m.py",
        "alias = {\n  \"a\": 1,\n  \"b\": 2,\n}\nITEMS = [\n  1,\n  2,\n]\nVERSION = \"1.0\"\n",
    );

    let sigs: std::collections::HashMap<String, String> = f.run(&["outline", "m.py"])["data"]
        .as_array()
        .unwrap()
        .iter()
        .map(|r| {
            (
                r["name"].as_str().unwrap().to_string(),
                r["signature"].as_str().unwrap_or_default().to_string(),
            )
        })
        .collect();
    assert!(sigs["alias"].ends_with("{…}"), "{sigs:?}");
    assert!(sigs["ITEMS"].ends_with("[…]"), "{sigs:?}");
    // A single-line value is still worth showing whole.
    assert!(sigs["VERSION"].contains("1.0"), "{sigs:?}");
}

#[test]
fn an_empty_result_says_why_it_is_empty() {
    // Eleven of twenty-six commands returned empty with no note, no stderr and
    // exit 0. They were being fixed one at a time as each was tripped over; this
    // pins the class. Every case here is empty *because a filter selected
    // nothing*, which is the ambiguous kind — a bare pattern miss is deliberately
    // still silent, since an empty `search zzz` is unambiguous.
    let f = Fixture::new();
    // Line 1 is a comment, so it sits between definitions — that is the `at`
    // case, and it is what `explore --at` already explained while `at` did not.
    f.write(
        "src/a.py",
        "# a comment outside every definition\ndef alpha():\n    return 1\n",
    );

    let note =
        |args: &[&str]| -> String { String::from_utf8_lossy(&f.raw(args).stderr).to_string() };
    for (args, want) in [
        (vec!["imports", "nosuchdir"], "no indexed file matches"),
        (vec!["hotspots", "nosuchdir"], "no indexed file matches"),
        (vec!["clones", "nosuchdir"], "no indexed file matches"),
        (vec!["tree", "nosuchdir"], "no indexed file matches"),
        (
            vec!["search", "def", "--path", "nosuchdir"],
            "the pattern was never run",
        ),
        (
            vec!["search", "def", "--language", "cobol"],
            "the pattern was never run",
        ),
        (
            vec!["context", "--symbols", "zzznotreal"],
            "no symbol named",
        ),
        (vec!["at", "src/a.py:1"], "no definition encloses"),
    ] {
        let got = note(&args);
        assert!(
            got.contains(want),
            "{args:?} should say '{want}', said: {got}"
        );
    }

    // A real scope that genuinely holds none of the thing says *that* instead —
    // the two are different answers and must not collapse into one.
    let real = note(&["imports", "src/a.py"]);
    assert!(real.contains("but none has"), "{real}");

    // A bare pattern miss stays quiet.
    assert!(note(&["search", "zzznotrealpattern"]).is_empty());
}

#[test]
fn search_path_scope_note_treats_like_metacharacters_literally() {
    let f = Fixture::new();
    f.write("pkg/acb.rs", "fn target() {}\n");

    let note = String::from_utf8_lossy(&f.raw(&["search", "not-present", "--path", "a_b"]).stderr)
        .to_string();
    assert!(
        note.contains("no indexed file matches --path a_b"),
        "{note}"
    );
}

#[test]
fn context_names_a_seed_that_resolved_to_nothing() {
    // The worst of the silent empties: `context` exists to hand source to an
    // agent, so an empty bundle reads as "nothing more is needed". And a bad seed
    // beside good ones is *more* dangerous — the bundle looks full, and the name
    // you got wrong is the one missing from it.
    let f = Fixture::new();
    f.write(
        "a.py",
        "def alpha():\n    return 1\n\ndef beta():\n    return 2\n",
    );

    let note = |args: &[&str]| String::from_utf8_lossy(&f.raw(args).stderr).to_string();
    let only_bad = note(&["context", "--symbols", "zzznotreal"]);
    assert!(
        only_bad.contains("no symbol named zzznotreal"),
        "{only_bad}"
    );

    let mixed = note(&[
        "context",
        "--symbols",
        "alpha,zzznotreal",
        "--max-tokens",
        "400",
    ]);
    assert!(
        mixed.contains("zzznotreal"),
        "a bad seed among good ones: {mixed}"
    );
    assert!(mixed.contains("1 of 2 seeds"), "{mixed}");

    // All seeds good: no note, and the bundle is real.
    let v = f.run(&["context", "--symbols", "alpha,beta", "--max-tokens", "400"]);
    assert!(v["note"].is_null(), "{v}");
    assert_eq!(v["data"]["chunks"].as_array().unwrap().len(), 2, "{v}");
}

#[test]
fn a_huge_data_file_is_still_searchable() {
    // A body cap was tried — it made a repo committing 1.3 GB of geodata index
    // 200x smaller — and reverted, because the differential harness showed it
    // also stopped `search` being a strict superset of ripgrep: 20,000 lines of
    // a 6.6 MB test fixture that `rg` finds and codeq no longer would. That
    // guarantee is the loudest claim this tool makes, and no threshold separates
    // "somebody's test fixture" from "committed geodata" on principle rather
    // than by fitting the two data points in front of you. `.codeqignore` is the
    // answer. This pins the decision so the cap is not quietly reintroduced.
    let f = Fixture::new();
    let big = format!("{{\"needle\":\"{}\"}}\n", "x".repeat(3 * 1024 * 1024));
    f.write("huge.json", &big);

    let hits = f.run(&["search", "needle"])["data"]["matches"]
        .as_array()
        .unwrap()
        .len();
    assert!(
        hits >= 1,
        "a big data file stays searchable — search is a superset of grep"
    );
}

#[test]
fn an_undefined_name_does_not_guess_why_it_is_undefined() {
    // The note used to say "declared elsewhere (a dependency, or a built-in)".
    // For a function *parameter* in a file two lines away, that sent the reader
    // looking outward at dependencies — wrong in the most expensive direction.
    // A note that states the fact beats one that picks a cause.
    let f = Fixture::new();
    f.write("a.ts", "export function match(columnData: string, areaBank: string) { return areaBank + columnData }\n");

    let err = String::from_utf8_lossy(&f.raw(&["definitions", "areaBank"]).stderr).to_string();
    assert!(err.contains("no definition codeq indexes"), "{err}");
    assert!(
        err.contains("a parameter"),
        "the likeliest cause must be listed: {err}"
    );
    assert!(
        !err.contains("(a dependency, or a built-in)"),
        "no longer picks one: {err}"
    );

    // A genuine typo still gets the typo message, not this one.
    let typo =
        String::from_utf8_lossy(&f.raw(&["definitions", "zzznotanything"]).stderr).to_string();
    assert!(typo.contains("no symbol named"), "{typo}");
}

/// Deterministic prose-like filler: enough distinct trigrams, repeated often
/// enough, that a positional trigram index dwarfs the text it covers.
fn filler_text(bytes: usize) -> String {
    let mut out = String::with_capacity(bytes + 16);
    let mut state: u64 = 0x2545_F491_4F6C_DD1D;
    while out.len() < bytes {
        state = state
            .wrapping_mul(6364136223846793005)
            .wrapping_add(1442695040888963407);
        out.push_str(&format!("w{:x} ", (state >> 33) % 4096));
        if state.is_multiple_of(11) {
            out.push('\n');
        }
    }
    out
}

/// A generated tree of definitions and calls under `gen/`: what fills an index
/// now that file text is not stored in it. Many small files, because one large
/// one is far slower to extract.
fn write_symbol_heavy_tree(f: &Fixture) {
    let source: String = (0..100)
        .map(|i| format!("def fn_{i}(a, b):\n    return fn_{}(a, b)\n\n", i / 2))
        .collect();
    for k in 0..200 {
        f.write(&format!("gen/m{k}.py"), &source);
    }
}

fn remove_symbol_heavy_tree(f: &Fixture) {
    std::fs::remove_dir_all(f.path().join("gen")).unwrap();
}

fn index_bytes(f: &Fixture) -> u64 {
    std::fs::metadata(f.path().join(".codeq/index.db"))
        .unwrap()
        .len()
}

#[test]
fn the_content_index_costs_less_than_the_text_it_covers() {
    // The index holds which files contain each trigram and nothing more: not
    // where in the file (with positions it was twice the size of the source on
    // a C repo), and not the text, which is on disk already.
    let f = Fixture::new();
    let text = filler_text(1 << 20);
    f.write("notes.txt", &text);

    let hits = f.run(&["search", "w7ff", "--limit", "0"]);
    assert!(!hits["data"]["matches"].as_array().unwrap().is_empty());
    let (index, source) = (index_bytes(&f), text.len() as u64);
    assert!(index * 2 < source, "index {index} B for {source} B of text");
}

#[test]
fn indexing_a_file_is_linear_in_its_definitions() {
    // The root of a Python file is a `module` node, the kind Ruby gives
    // `module Foo`, so every definition asked the *file* for its type name:
    // a scan of all top-level children, once per definition. 10,000 functions
    // took about 45 s in this build and 20,000 about 200 s; linear, it is 2 s.
    // The bound is loose enough for a loaded machine and still far below the
    // quadratic cost.
    let f = Fixture::new();
    let source: String = (0..10_000)
        .map(|i| format!("def fn_{i}(a, b):\n    return fn_{}(a, b)\n\n", i / 2))
        .collect();
    f.write("big.py", &source);

    let started = std::time::Instant::now();
    let symbols = f.run(&["symbols", "fn_9999"]);
    let elapsed = started.elapsed();
    assert_eq!(field_set(&symbols, "name"), ["fn_9999"]);
    assert!(
        elapsed < std::time::Duration::from_secs(20),
        "indexing 10,000 definitions took {elapsed:?}"
    );
}

#[test]
fn explicit_compaction_reclaims_pages_left_by_refresh() {
    // SQLite parks deleted pages on a free list and never returns them, so an
    // index only grew: a real one was 5.3 GB with 3.1 GB of it free.
    let f = Fixture::new();
    f.write("src/lib.py", "def compute():\n    return 1\n");
    write_symbol_heavy_tree(&f);
    f.run(&["files"]);
    let before = index_bytes(&f);

    remove_symbol_heavy_tree(&f);
    let files = f.run(&["files"]);
    assert_eq!(field_set(&files, "path"), ["src/lib.py"]);
    let after_refresh = index_bytes(&f);
    assert!(
        after_refresh >= before,
        "refresh unexpectedly compacted the index"
    );
    let conn = rusqlite::Connection::open(f.path().join(".codeq/index.db")).unwrap();
    let free: i64 = conn
        .query_row("PRAGMA freelist_count", [], |r| r.get(0))
        .unwrap();
    assert!(free >= 256, "deletion should leave free pages for reuse");
    drop(conn);

    f.run(&["index", "--compact"]);
    let after = index_bytes(&f);
    assert!(after * 4 < before, "index went {before} B -> {after} B");
    f.run(&["index", "--compact"]);
    assert_eq!(field_set(&f.run(&["files"]), "path"), ["src/lib.py"]);
}

#[test]
fn compaction_keeps_spans_and_calls_attached_to_their_file() {
    // Spans and calls key on a file id, and compaction VACUUMs. An implicit
    // rowid may be renumbered by that, which would hand every later file's
    // comments and call sites to a different file or to none.
    let f = Fixture::new();
    f.write("a.py", "def helper():\n    return 1\n");
    write_symbol_heavy_tree(&f);
    f.write(
        "z.py",
        "from a import helper\n\n# zebra marker\ndef run():\n    return helper()\n",
    );
    f.run(&["files"]);
    let before = index_bytes(&f);

    // Removing the generated files leaves gaps in the ids and frees enough
    // pages to exercise explicit compaction.
    remove_symbol_heavy_tree(&f);
    f.run(&["index", "--compact"]);
    assert!(index_bytes(&f) * 4 < before);

    let comments = f.run(&["search", "zebra", "--in", "comments"]);
    let hits = comments["data"]["matches"].as_array().unwrap();
    assert_eq!(hits.len(), 1, "{comments}");
    assert_eq!(hits[0]["path"], "z.py");
    let callees = f.run(&["callees", "run"]);
    assert_eq!(field_set(&callees, "name"), ["helper"]);
}

#[test]
fn the_trigram_prefilter_treats_fts_syntax_as_text() {
    // Candidates come from an FTS5 query built out of the literal's trigrams,
    // so every character FTS5 gives meaning to — quotes, operators, column
    // and prefix syntax — has to reach the tokenizer as plain text. A clause
    // that parsed as syntax would match nothing, and nothing looks like "no
    // hits".
    let f = Fixture::new();
    let long = "abcdefghijklmnopqrstuvwxyz0123456789ABCDEFGHIJKLMNOPQRSTUVWXYZ";
    f.write(
        "notes.txt",
        &format!("say \"hi\" AND (x OR y) NOT z* ^caret col:on\n100%_done naïve Ünïcode\n{long}\n"),
    );
    f.write("other.txt", "nothing of interest here\n");
    let hit = |args: &[&str]| -> Vec<String> {
        let mut argv = vec!["search"];
        argv.extend_from_slice(args);
        let v = f.run(&argv);
        let mut paths = field_set(&serde_json::json!({ "data": v["data"]["matches"] }), "path");
        paths.dedup();
        paths
    };
    for literal in [
        "\"hi\" AND",
        "(x OR y)",
        "NOT z* ^caret",
        "col:on",
        "100%_done",
        "naïve",
        long,
    ] {
        assert_eq!(hit(&["--fixed", literal]), ["notes.txt"], "{literal}");
    }
    assert_eq!(hit(&["-i", "--fixed", "ünïcode"]), ["notes.txt"]);
    // `_` and `%` are themselves, not wildcards.
    assert!(hit(&["--fixed", "100x_done"]).is_empty());

    // An edit replaces the file's trigrams rather than adding to them.
    f.write("notes.txt", "rewritten entirely\n");
    assert!(hit(&["--fixed", "col:on"]).is_empty());
    assert_eq!(hit(&["--fixed", "rewritten"]), ["notes.txt"]);

    // A file that turns binary stops being searchable, and one that is
    // removed takes its trigrams with it.
    f.write_bytes("notes.txt", b"rewritten\0entirely\n");
    assert!(hit(&["--fixed", "rewritten"]).is_empty());
    f.remove("other.txt");
    assert!(hit(&["--fixed", "interest"]).is_empty());
}

#[test]
fn astgrep_source_ranks_definitions_before_limiting_and_keeps_occurrence_identity() {
    let f = Fixture::new();
    f.write(
        "a.rs",
        "fn same() { first.unwrap(); }\nfn same() { second.unwrap(); third.unwrap(); }\n",
    );
    let out = f.run(&[
        "astgrep",
        "$X.unwrap()",
        "--language",
        "rust",
        "--source",
        "--limit",
        "1",
    ]);
    let data = &out["data"];
    assert_eq!(data["symbols"].as_array().unwrap().len(), 1);
    assert_eq!(data["symbols"][0]["line"], 2);
    assert_eq!(data["symbols"][0]["hits"], 2);
    assert_eq!(data["source"].as_array().unwrap().len(), 1);
    let text = data["source"][0]["text"].as_str().unwrap();
    assert!(text.contains("second.unwrap()") && text.contains("third.unwrap()"));
    assert!(!text.contains("first.unwrap()"));
    assert_eq!(data["truncated"], true);
    let text = String::from_utf8(
        f.raw(&[
            "astgrep",
            "$X.unwrap()",
            "--language",
            "rust",
            "--source",
            "--limit",
            "1",
        ])
        .stdout,
    )
    .unwrap();
    assert_eq!(text.matches("fn same()").count(), 1);
}

#[test]
fn astgrep_source_reports_budget_omissions_and_fills_with_smaller_definitions() {
    let f = Fixture::new();
    f.write("a.rs", &format!("fn big() {{\n// {}\nfirst.unwrap(); second.unwrap();\n}}\nfn small() {{ value.unwrap(); }}\n", "padding ".repeat(200)));
    let out = f.run(&[
        "astgrep",
        "$X.unwrap()",
        "--language",
        "rust",
        "--source",
        "--max-tokens",
        "40",
    ]);
    let data = &out["data"];
    assert_eq!(data["symbols"][0]["name"], "big");
    assert_eq!(data["source"][0]["name"], "small");
    assert_eq!(data["source"].as_array().unwrap().len(), 1);
    assert_eq!(data["omitted"][0]["name"], "big");
    assert!(data["budget"]["used_tokens"].as_u64().unwrap() <= 40);
}

#[test]
fn astgrep_source_distinguishes_same_names_across_files_and_scopes_paths() {
    let f = Fixture::new();
    f.write("a.rs", "fn same() { first.unwrap(); }\n");
    f.write("b.rs", "fn same() { second.unwrap(); }\n");
    let out = f.run(&[
        "astgrep",
        "$X.unwrap()",
        "--language",
        "rust",
        "--source",
        "--limit",
        "0",
    ]);
    assert_eq!(out["data"]["source"].as_array().unwrap().len(), 2);
    let scoped = f.run(&[
        "astgrep",
        "$X.unwrap()",
        "--language",
        "rust",
        "--source",
        "--path",
        "b.rs",
    ]);
    assert_eq!(scoped["data"]["source"].as_array().unwrap().len(), 1);
    assert_eq!(scoped["data"]["source"][0]["path"], "b.rs");
}

#[test]
fn astgrep_source_explains_unscoped_matches_and_empty_queries() {
    let f = Fixture::new();
    f.write("a.py", "needle(0)\ndef chosen():\n    return needle(1)\n");
    let out = f.run(&[
        "astgrep",
        "needle($VALUE)",
        "--language",
        "python",
        "--source",
    ]);
    assert_eq!(out["data"]["symbols"][0]["hits"], 1);
    assert!(out["note"].as_str().unwrap().contains("1 structural match"));
    let empty = f.run(&[
        "astgrep",
        "absent($VALUE)",
        "--language",
        "python",
        "--source",
    ]);
    assert!(empty["data"]["source"].as_array().unwrap().is_empty());
    assert!(empty["note"].as_str().unwrap().contains("matched nothing"));
    let unsearched = f.run(&[
        "astgrep",
        "needle($VALUE)",
        "--language",
        "python",
        "--source",
        "--path",
        "missing",
    ]);
    assert!(unsearched["note"]
        .as_str()
        .unwrap()
        .contains("pattern was never run"));
}

#[test]
fn astgrep_source_conflicts_with_explain_and_budget_requires_source() {
    let f = Fixture::new();
    f.write("a.rs", "fn f() {}\n");
    assert!(!f
        .raw(&["astgrep", "--explain", "a.rs:1", "--source"])
        .status
        .success());
    assert!(!f
        .raw(&[
            "astgrep",
            "fn $N() {}",
            "--language",
            "rust",
            "--max-tokens",
            "10"
        ])
        .status
        .success());
}

#[test]
fn mcp_astgrep_source_returns_implementations_directly() {
    use std::io::Write;
    use std::process::Stdio;
    let f = Fixture::new();
    f.write("a.rs", "fn chosen() { value.unwrap(); other.unwrap(); }\n");
    let mut child = Command::cargo_bin("codeq")
        .unwrap()
        .arg("mcp")
        .current_dir(f.path())
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .spawn()
        .unwrap();
    writeln!(child.stdin.take().unwrap(), r#"{{"jsonrpc":"2.0","id":1,"method":"tools/call","params":{{"name":"codeq_astgrep","arguments":{{"pattern":"$X.unwrap()","language":"rust","source":true,"max_tokens":100,"limit":1}}}}}}"#).unwrap();
    let output = child.wait_with_output().unwrap();
    let response: Value = serde_json::from_slice(&output.stdout).unwrap();
    assert_ne!(response["result"]["isError"], true, "{response}");
    let text = response["result"]["content"][0]["text"].as_str().unwrap();
    assert!(text.contains("value.unwrap()") && text.contains("other.unwrap()"));
    assert_eq!(text.matches("fn chosen()").count(), 1);
}

#[test]
fn astgrep_source_marks_a_capped_budget_omission_list() {
    let f = Fixture::new();
    let source = (0..60)
        .map(|i| format!("fn f{i}() {{ value.unwrap(); }}\n"))
        .collect::<String>();
    f.write("a.rs", &source);
    let out = f.run(&[
        "astgrep",
        "$X.unwrap()",
        "--language",
        "rust",
        "--source",
        "--limit",
        "0",
        "--max-tokens",
        "0",
    ]);
    assert_eq!(out["data"]["symbols"].as_array().unwrap().len(), 60);
    assert!(out["data"]["source"].as_array().unwrap().is_empty());
    assert_eq!(out["data"]["omitted"].as_array().unwrap().len(), 50);
    assert_eq!(out["data"]["omitted_truncated"], true);
}

#[test]
fn rust_associated_calls_select_the_impl_owner_and_exact_source() {
    let f = Fixture::new();
    f.write(
        "types.rs",
        "pub struct Selected;\nimpl Selected { pub fn new() { selected_body(); } }\n\
        pub struct Other;\nimpl Other { pub fn new() { unrelated_body(); } }\n\
        fn new() { unrelated_free(); }\n",
    );
    f.write(
        "app.rs",
        "use crate::types::Selected as Choice;\nfn run() { Choice::new(); }\n",
    );
    let r = f.run(&["explore", "run"]);
    let callees = r["data"]["callees"].as_array().unwrap();
    assert_eq!(callees.len(), 1, "{r}");
    assert_eq!(callees[0]["line"], 2);
    let source = r["data"]["source"].as_array().unwrap();
    let text: String = source.iter().filter_map(|c| c["text"].as_str()).collect();
    assert!(text.contains("selected_body()"), "{r}");
    assert!(!text.contains("unrelated_body()"), "{r}");
    assert!(!text.contains("unrelated_free()"), "{r}");
}

#[test]
fn rust_associated_call_constraints_union_and_unknown_receivers_stay_conservative() {
    let f = Fixture::new();
    f.write(
        "app.rs",
        "struct A; struct B;\nimpl A { fn new() {} }\nimpl B { fn new() {} }\n\
        fn both() { A::new(); B::new(); }\n\
        fn unknown(value: Unknown) { value.new(); }\n\
        fn mixed(value: Unknown) { A::new(); value.new(); }\n\
        fn capital_value(A: Unknown) { A.new(); }\n\
        type Alias = A;\nfn alias() { Alias::new(); }\n",
    );
    for name in ["both", "unknown", "mixed", "alias", "capital_value"] {
        assert_eq!(
            f.run(&["callees", name])["data"].as_array().unwrap().len(),
            2,
            "{name}"
        );
    }
}

#[test]
fn rust_impl_owners_handle_generics_traits_and_nested_helpers() {
    let f = Fixture::new();
    f.write(
        "app.rs",
        "struct A<T>(T); struct B;\n\
        impl<T> A<T> { fn new() { fn nested() {} } }\n\
        impl Default for B { fn default() -> Self { B } }\n\
        fn run() { A::new(); B::default(); }\n",
    );
    let r = f.run(&["symbols", "", "--owner", "A"]);
    let names = field_set(&r, "name");
    assert!(names.contains(&"new".to_string()), "{r}");
    assert!(!names.contains(&"nested".to_string()), "{r}");
    assert_eq!(
        f.run(&["callees", "run"])["data"].as_array().unwrap().len(),
        2
    );
}

#[test]
fn rust_associated_calls_preserve_trait_defaults_without_an_owned_implementation() {
    let f = Fixture::new();
    f.write(
        "app.rs",
        "struct Selected;\ntrait Factory { fn build() { provided_default(); } }\n\
        impl Factory for Selected {}\nfn run() { Selected::build(); }\n",
    );
    let r = f.run(&["explore", "run"]);
    assert_eq!(r["data"]["callees"].as_array().unwrap().len(), 1, "{r}");
    assert_eq!(r["data"]["callees"][0]["name"], "build", "{r}");
    assert!(
        r["data"]["source"]
            .as_array()
            .unwrap()
            .iter()
            .any(|c| c["text"].as_str().unwrap().contains("provided_default()")),
        "{r}"
    );
}

#[test]
fn explore_lists_ambiguous_callees_without_spending_source_by_default() {
    let f = Fixture::new();
    f.write(
        "app.rs",
        "fn run(value: Unknown) { value.build(); unique(); }\nfn unique() {}\n",
    );
    f.write("a.rs", "fn build() { first_candidate(); }\n");
    f.write("b.rs", "fn build() { second_candidate(); }\n");
    let r = f.run(&["explore", "run"]);
    let candidates: Vec<_> = r["data"]["callees"]
        .as_array()
        .unwrap()
        .iter()
        .filter(|c| c["name"] == "build")
        .collect();
    assert_eq!(candidates.len(), 2, "{r}");
    assert!(
        candidates.iter().all(|c| c["resolution"] == "ambiguous"),
        "{r}"
    );
    assert!(
        r["note"].as_str().unwrap().contains("--include-ambiguous"),
        "{r}"
    );
    assert!(
        r["data"]["source"]
            .as_array()
            .unwrap()
            .iter()
            .all(|c| c["name"] != "build"),
        "{r}"
    );
    assert!(
        r["data"]["source"]
            .as_array()
            .unwrap()
            .iter()
            .any(|c| c["name"] == "unique"),
        "{r}"
    );
    let expanded = f.run(&["explore", "run", "--include-ambiguous"]);
    assert_eq!(
        expanded["data"]["source"]
            .as_array()
            .unwrap()
            .iter()
            .filter(|c| c["name"] == "build")
            .count(),
        2,
        "{expanded}"
    );
    let text = f.raw(&["explore", "run"]);
    assert!(String::from_utf8(text.stdout)
        .unwrap()
        .contains("resolution=ambiguous"));
    assert!(String::from_utf8(text.stderr)
        .unwrap()
        .contains("--include-ambiguous"));
}

#[test]
fn distinct_qualified_calls_are_not_ambiguous_and_resolution_follows_filters() {
    let f = Fixture::new();
    f.write(
        "app.rs",
        "struct A; struct B;\nimpl A { fn new() {} }\nimpl B { fn new() {} }\n\
        fn both() { A::new(); B::new(); }\nfn local() { helper(); }\nfn helper() {}\n",
    );
    f.write("other.rs", "fn helper() {}\n");
    let r = f.run(&["explore", "both"]);
    assert!(
        r["data"]["callees"]
            .as_array()
            .unwrap()
            .iter()
            .all(|c| c["resolution"] == "qualified"),
        "{r}"
    );
    assert_eq!(
        r["data"]["source"]
            .as_array()
            .unwrap()
            .iter()
            .filter(|c| c["name"] == "new")
            .count(),
        2,
        "{r}"
    );
    let all = f.run(&["callees", "local"]);
    assert!(
        all["data"]
            .as_array()
            .unwrap()
            .iter()
            .all(|c| c["resolution"] == "ambiguous"),
        "{all}"
    );
    let narrowed = f.run(&["callees", "local", "--resolved"]);
    assert_eq!(narrowed["data"][0]["resolution"], "name-only", "{narrowed}");
}

/// An index written by a different codeq is dropped and rebuilt on the next
/// query. That used to happen without a word, so an upgrade looked like one
/// unexplained slow call; the run that rebuilds now names both versions.
#[test]
fn a_schema_change_says_it_rebuilt_the_index() {
    let f = Fixture::new();
    f.write("a.rs", "pub fn kept() {}\n");
    f.run(&["symbols", "kept"]);

    let conn = rusqlite::Connection::open(f.path().join(".codeq/index.db")).unwrap();
    conn.execute(
        "UPDATE meta SET value = '1' WHERE key = 'schema_version'",
        [],
    )
    .unwrap();
    drop(conn);

    let out = f.raw(&["symbols", "kept"]);
    assert!(out.status.success());
    let stderr = String::from_utf8_lossy(&out.stderr);
    assert!(
        stderr.contains("index schema changed (v1 -> v"),
        "the rebuild went unexplained: {stderr}"
    );
    assert!(
        String::from_utf8_lossy(&out.stdout).contains("kept"),
        "the rebuilt index lost the definition"
    );

    // Said once, by the run that did it.
    let again = f.raw(&["symbols", "kept"]);
    assert!(
        !String::from_utf8_lossy(&again.stderr).contains("schema changed"),
        "a current index was reported as rebuilt"
    );
}

/// A query that arrives while another process holds the index write lock past
/// the busy timeout used to fail with SQLite's raw "database is locked", which
/// reads like a corrupt index. It names the real cause, and recovers by itself
/// once the writer is done.
#[test]
fn a_lock_held_past_the_timeout_is_explained_not_leaked() {
    let f = Fixture::new();
    f.write("a.rs", "pub fn first() {}\n");
    f.run(&["symbols", "first"]);

    // Another process mid-build: the write lock is taken and not released.
    let writer = rusqlite::Connection::open(f.path().join(".codeq/index.db")).unwrap();
    writer.execute_batch("BEGIN IMMEDIATE").unwrap();
    // A fresh WAL reader must finish even while another connection owns writes.
    let fresh = Command::cargo_bin("codeq")
        .unwrap()
        .current_dir(f.path())
        .env("CODEQ_BUSY_TIMEOUT_MS", "0")
        .args(["symbols", "first"])
        .output()
        .unwrap();
    assert!(
        fresh.status.success(),
        "{}",
        String::from_utf8_lossy(&fresh.stderr)
    );
    assert!(String::from_utf8_lossy(&fresh.stdout).contains("first"));

    // An edit, so the next query has to write too.
    f.write("a.rs", "pub fn second() {}\n");

    let blocked = Command::cargo_bin("codeq")
        .unwrap()
        .current_dir(f.path())
        .env("CODEQ_BUSY_TIMEOUT_MS", "200")
        .args(["symbols", "second"])
        .output()
        .unwrap();
    let stderr = String::from_utf8_lossy(&blocked.stderr);
    assert!(!blocked.status.success(), "{stderr}");
    assert!(
        stderr.contains("another codeq process is still writing"),
        "{stderr}"
    );
    assert!(!stderr.contains("database is locked"), "{stderr}");

    writer.execute_batch("ROLLBACK").unwrap();
    let after = f.run(&["symbols", "second"]);
    assert_eq!(field_set(&after, "name"), ["second"]);
}

/// Bash spells a call `command`, which the call-site reader did not recognise,
/// while the grammar's presence made its empty answer authoritative. So the
/// plain textual `callers` found a shell function's callers and the precise
/// forms answered "none" for every script.
#[test]
fn bash_commands_are_call_sites() {
    let f = Fixture::new();
    f.write(
        "run.sh",
        "label() {\n  echo circle\n}\n\ndescribe() {\n  # label is not called here\n  label\n}\n",
    );

    let callers = f.run(&["callers", "label", "--calls"]);
    assert_eq!(field_set(&callers, "name"), ["describe"], "{callers}");

    let callees = f.run(&["callees", "describe"]);
    assert_eq!(field_set(&callees, "name"), ["label"], "{callees}");

    let calls = f.run(&["references", "label", "--calls"]);
    let lines: Vec<i64> = calls["data"]["references"]
        .as_array()
        .unwrap()
        .iter()
        .map(|m| m["line"].as_i64().unwrap())
        .collect();
    assert_eq!(lines, [7], "the comment and the declaration are not calls");
}

/// `EXECUTE FUNCTION f()` in a trigger is a call to `f`, though the grammar has
/// no invocation node there.
#[test]
fn a_sql_trigger_is_a_call_site_of_its_function() {
    let f = Fixture::new();
    f.write(
        "schema.sql",
        "CREATE FUNCTION touch() RETURNS trigger AS $$ BEGIN RETURN NEW; END; $$ LANGUAGE plpgsql;\n\
         CREATE TABLE circles (id INTEGER);\n\
         CREATE TRIGGER circles_touch BEFORE UPDATE ON circles\n\
         FOR EACH ROW EXECUTE FUNCTION touch();\n",
    );
    let callers = f.run(&["callers", "touch", "--calls"]);
    assert_eq!(field_set(&callers, "name"), ["circles_touch"], "{callers}");
}

#[test]
fn batch_symbol_lookups_keep_order_duplicates_and_missing_names() {
    let f = Fixture::new();
    f.write("a.py", "def alpha():\n    beta()\ndef beta():\n    pass\n");
    for command in ["definitions", "about"] {
        let batch = f.run(&[command, "beta", "absent", "alpha", "beta"]);
        assert_eq!(batch["rows"], "data.results");
        let results = batch["data"]["results"].as_array().unwrap();
        assert_eq!(results.len(), 4);
        for (row, name) in results.iter().zip(["beta", "absent", "alpha", "beta"]) {
            assert_eq!(row["name"], name);
            let single = f.run(&[command, name]);
            assert_eq!(row["data"], single["data"]);
            assert_eq!(row["note"], single["note"]);
        }
        assert!(batch["note"].as_str().unwrap().contains("absent"));
        let all_missing = f.run(&[command, "absent", "missing"]);
        for (row, name) in all_missing["data"]["results"]
            .as_array()
            .unwrap()
            .iter()
            .zip(["absent", "missing"])
        {
            assert!(row["note"].as_str().unwrap().contains(name));
        }
        let no_names = f.raw(&[command]);
        assert!(!no_names.status.success());
    }
}

#[test]
fn configuration_files_expose_key_definitions() {
    let f = Fixture::new();
    for (path, body) in [
        ("app.json", "{\"needle\": true}\n"),
        ("app.yaml", "needle: true\n"),
        ("app.toml", "needle = true\n"),
    ] {
        f.write(path, body);
        assert_eq!(f.run(&["outline", path])["data"][0]["name"], "needle");
        assert_eq!(
            f.run(&["at", &format!("{path}:1")])["data"][0]["name"],
            "needle"
        );
        let out = f.raw(&["read", path, "--symbol", "needle", "--raw"]);
        assert!(out.status.success());
        assert!(String::from_utf8_lossy(&out.stdout).contains("needle"));
    }
}

#[test]
fn configuration_key_paths_keep_nested_ranges_arrays_and_literal_dots() {
    let f = Fixture::new();
    f.write("package.json", "{\n  \"scripts\": {\n    \"test\": \"cargo test\"\n  },\n  \"other\": {\"test\": false},\n  \"a.b\": {\"value\": 1},\n  \"servers\": [{\"host\": \"local\"}]\n}\n");
    let names = field_set(&f.run(&["outline", "package.json"]), "name");
    for name in [
        "scripts.test",
        "other.test",
        "[\"a.b\"].value",
        "servers[0].host",
    ] {
        assert!(names.contains(&name.to_string()), "{names:?}");
    }
    assert_eq!(
        f.run(&["at", "package.json:3"])["data"][0]["name"],
        "scripts.test"
    );
    let out = f.raw(&["read", "package.json", "--symbol", "scripts", "--raw"]);
    assert_eq!(
        String::from_utf8(out.stdout).unwrap().trim(),
        "\"scripts\": {\n    \"test\": \"cargo test\"\n  },"
    );
    f.write(
        "ci.yml",
        "jobs:\n  build:\n    steps:\n      - run: cargo test\n      - run: cargo fmt\n",
    );
    let names = field_set(&f.run(&["outline", "ci.yml"]), "name");
    assert!(
        names.contains(&"jobs.build.steps[0].run".to_string()),
        "{names:?}"
    );
    assert!(
        names.contains(&"jobs.build.steps[1].run".to_string()),
        "{names:?}"
    );
    f.write(
        "project.toml",
        "[tool.ruff]\nline-length = 100\n[[servers]]\nhost = 'one'\n[[servers]]\nhost = 'two'\n",
    );
    let names = field_set(&f.run(&["outline", "project.toml"]), "name");
    for name in [
        "tool.ruff",
        "tool.ruff.line-length",
        "servers[0].host",
        "servers[1].host",
    ] {
        assert!(names.contains(&name.to_string()), "{names:?}");
    }
    f.write("main.py", "def main():\n    test()\n");
    assert_eq!(f.run(&["callees", "main"])["data"], serde_json::json!([]));
    f.write("package-lock.json", "{\"lockfileVersion\": 3}");
    assert_eq!(
        f.run(&["outline", "package-lock.json"])["data"],
        serde_json::json!([])
    );
}

#[test]
fn batch_symbol_lookups_apply_path_resolution_and_limits_per_name() {
    let f = Fixture::new();
    f.write("a.py", "def alpha():\n    beta()\ndef beta():\n    pass\n");
    f.write("b.py", "def alpha():\n    beta()\ndef beta():\n    pass\n");
    for command in ["definitions", "about"] {
        let flags: &[&str] = if command == "definitions" {
            &["--path", "a.py", "--limit", "1"]
        } else {
            &["--path", "a.py", "--resolved"]
        };
        let mut args = vec![command, "beta", "alpha"];
        args.extend_from_slice(flags);
        let batch = f.run(&args);
        for (row, name) in batch["data"]["results"]
            .as_array()
            .unwrap()
            .iter()
            .zip(["beta", "alpha"])
        {
            let mut args = vec![command, name];
            args.extend_from_slice(flags);
            assert_eq!(row["data"], f.run(&args)["data"]);
        }
    }
    let limited = f.run(&["definitions", "alpha", "beta", "--limit", "1"]);
    for row in limited["data"]["results"].as_array().unwrap() {
        assert_eq!(row["data"].as_array().unwrap().len(), 1);
    }
    f.write("README.md", "# One, two\ntext\n# Other\ntext\n");
    let headings = f.run(&["definitions", "One, two", "Other"]);
    assert_eq!(
        headings["data"]["results"][0]["data"][0]["name"],
        "One, two"
    );
}

#[test]
fn batch_symbol_lookups_render_sections_and_preserve_output_flags() {
    let f = Fixture::new();
    f.write("a.py", "class Box:\n    def alpha(self):\n        pass\n    def beta(self):\n        self.alpha()\n");
    for command in ["definitions", "about"] {
        let output = f.raw(&[
            command,
            "beta",
            "absent",
            "alpha",
            "--group",
            "--fields",
            "path,line,signature",
        ]);
        assert!(output.status.success());
        let text = String::from_utf8(output.stdout).unwrap();
        assert!(text.find("# beta\n").unwrap() < text.find("# absent\n").unwrap());
        assert!(text.find("# absent\n").unwrap() < text.find("# alpha\n").unwrap());
        assert!(text.contains("def beta(self)"), "{text}");
        assert!(text.contains("def alpha(self)"), "{text}");
        assert!(String::from_utf8(output.stderr).unwrap().contains("absent"));
        let projected = f.run(&[command, "beta", "absent", "alpha", "--fields", "path,line"]);
        assert_eq!(projected["data"]["results"][1]["name"], "absent");
        assert!(projected["data"]["results"][1]["note"]
            .as_str()
            .unwrap()
            .contains("absent"));
        assert!(projected.get("notes").is_none(), "{projected}");
    }
    let capped = f.run(&[
        "about",
        "Box",
        "alpha",
        "--max-per-file",
        "1",
        "--fields",
        "path,line",
    ]);
    let box_result = &capped["data"]["results"][0];
    assert_eq!(box_result["data"]["members"].as_array().unwrap().len(), 1);
    assert_eq!(box_result["truncated_per_file"]["a.py"], 2);
    let output = f.raw(&["about", "Box", "alpha", "--max-per-file", "1"]);
    assert!(output.status.success());
    assert!(String::from_utf8(output.stdout)
        .unwrap()
        .contains("+1 more (of 2)"));
    let output = f.raw(&[
        "about",
        "Box",
        "alpha",
        "--max-per-file",
        "1",
        "--fields",
        "name",
    ]);
    assert!(output.status.success());
    let stderr = String::from_utf8(output.stderr).unwrap();
    assert!(
        stderr.contains("Box: --max-per-file capped rows"),
        "{stderr}"
    );
    assert!(stderr.contains("a.py=2"), "{stderr}");
}

#[test]
fn batch_symbol_lookups_work_through_mcp_with_scalar_compatibility() {
    let f = Fixture::new();
    f.write("a.py", "def alpha():\n    beta()\ndef beta():\n    pass\n");
    for (command, key) in [("definitions", "symbol"), ("about", "name")] {
        let tool = format!("codeq_{command}");
        let (error, text) = mcp_call(
            &f,
            &tool,
            serde_json::json!({key: ["beta", "absent", "alpha"]}),
        );
        assert!(!error, "{text}");
        assert!(text.find("# beta\n").unwrap() < text.find("# absent\n").unwrap());
        assert!(text.find("# absent\n").unwrap() < text.find("# alpha\n").unwrap());
        assert!(text.contains("def beta()"), "{text}");
        assert!(text.contains("def alpha()"), "{text}");
        assert!(text.contains("no symbol named 'absent'"), "{text}");
        let (error, text) = mcp_call(&f, &tool, serde_json::json!({key: "alpha"}));
        assert!(!error, "{text}");
        let single = f.raw(&[command, "alpha"]);
        assert!(single.status.success());
        assert!(
            text.contains(String::from_utf8(single.stdout).unwrap().trim()),
            "{text}"
        );
        let (error, _) = mcp_call(&f, &tool, serde_json::json!({key: []}));
        assert!(error);
    }
}

#[test]
fn extraction_notes_do_not_misclassify_supported_or_unknown_files() {
    let f = Fixture::new();
    for (path, body) in [
        ("empty.rs", "// no definitions\n"),
        ("empty.rst", "Prose without a heading.\n"),
        ("empty.jinja", "Plain template text.\n"),
        ("plain.txt", "Plain text.\n"),
    ] {
        f.write(path, body);
        let outline = f.run(&["outline", path]);
        assert_eq!(outline["data"], serde_json::json!([]));
        assert!(outline.get("note").is_none(), "{outline}");
        let location = format!("{path}:1");
        let at = f.run(&["at", &location]);
        assert!(at["note"]
            .as_str()
            .unwrap()
            .contains("no definition encloses"));
        let read = f.raw(&["read", path, "--symbol", "absent"]);
        assert!(!read.status.success());
        assert!(String::from_utf8_lossy(&read.stderr).contains("no definition named 'absent'"));
    }
}

#[test]
fn cpp_access_sections_control_external_api() {
    let f = Fixture::new();
    f.write(
        "a.cpp",
        r#"class Widget {
    int hidden;
    void secret() {}
    struct Hidden { void nested_secret() {} };
public:
    void visible() {}
    static void utility() {}
    static int count;
    struct Nested { void nested_visible() {} };
protected:
    int guarded;
    void inherited() {}
    struct Protected { void protected_nested() {} };
private:
    void later_secret() {}
public:
    void again() {}
};
struct Record {
    int value;
    void update() {}
private:
    int internal;
protected:
    void restricted() {}
public:
    static void factory() {}
};
union Choice { int option; private: int concealed; };
static void file_local() {}
void external() {}
"#,
    );
    let expected = [
        "Nested",
        "Record",
        "Widget",
        "again",
        "count",
        "external",
        "factory",
        "nested_visible",
        "option",
        "update",
        "utility",
        "value",
        "visible",
    ];
    let mut api = field_set(&f.run(&["api", "a.cpp"]), "name");
    api.sort();
    assert_eq!(api, expected);
    let mut untested = field_set(&f.run(&["untested", "a.cpp"]), "name");
    untested.sort();
    assert_eq!(
        untested,
        [
            "Nested",
            "Record",
            "Widget",
            "again",
            "external",
            "factory",
            "nested_visible",
            "update",
            "utility",
            "visible"
        ]
    );
    let symbols = field_set(&f.run(&["symbols", "--path", "a.cpp"]), "name");
    for name in [
        "hidden",
        "secret",
        "Hidden",
        "nested_secret",
        "guarded",
        "inherited",
        "Protected",
        "protected_nested",
        "later_secret",
        "internal",
        "restricted",
        "concealed",
        "file_local",
    ] {
        assert!(symbols.contains(&name.to_string()), "{name}: {symbols:?}");
    }
}

#[test]
fn search_word_retries_regex_alternatives() {
    let f = Fixture::new();
    f.write("words.txt", "foobar\nfoo foo\nunrelated\n");
    for pattern in [
        "foo|foobar",
        "foobar|foo",
        "foo(?:bar)??",
        "(?x) foo | foobar # trailing comment",
        "(foo)|(foobar)",
    ] {
        let result = f.run(&["search", pattern, "-w", "words.txt"]);
        let rows = result["data"]["matches"].as_array().unwrap();
        assert_eq!(
            rows.iter()
                .map(|r| r["line"].as_u64().unwrap())
                .collect::<Vec<_>>(),
            [1, 2],
            "{pattern}: {result}"
        );
        let only = f.run(&["search", pattern, "-w", "-o", "words.txt"]);
        assert_eq!(
            only["data"]["matches"]
                .as_array()
                .unwrap()
                .iter()
                .map(|r| r["text"].as_str().unwrap())
                .collect::<Vec<_>>(),
            ["foobar", "foo", "foo"]
        );
        assert_eq!(
            f.run(&["search", pattern, "-w", "-c", "words.txt"])["data"]["counts"][0]["count"],
            2
        );
        let inverted = f.run(&["search", pattern, "-w", "-v", "words.txt"]);
        assert_eq!(inverted["data"]["matches"][0]["line"], 3);
        assert_eq!(inverted["data"]["matches"].as_array().unwrap().len(), 1);
    }
    f.write("single.txt", "foobar\n");
    let files = f.run(&["search", "foo|foobar", "-w", "-l", "single.txt"]);
    assert_eq!(files["data"]["files"], serde_json::json!(["single.txt"]));
    assert!(
        f.run(&["search", "foo|foobar", "-w", "-L", "single.txt"])["data"]["files"]
            .as_array()
            .unwrap()
            .is_empty()
    );
}

#[test]
fn search_word_alternatives_preserve_identifier_boundaries() {
    let f = Fixture::new();
    f.write(
        "words.txt",
        "éclair。 foo。\n_foo foo_ éfoo fooé\n$foo foo$tail\nFOOBAR\n",
    );
    let result = f.run(&[
        "search",
        "é|éclair|foo|foobar",
        "-w",
        "-o",
        "-i",
        "words.txt",
    ]);
    assert_eq!(
        result["data"]["matches"]
            .as_array()
            .unwrap()
            .iter()
            .map(|r| r["text"].as_str().unwrap())
            .collect::<Vec<_>>(),
        ["éclair", "foo", "foo", "foo", "FOOBAR"]
    );
    f.write(
        "app.js",
        "foo$tail();\n$foo();\nconst text = \"foo$tail $foo\";\n",
    );
    let result = f.run(&["search", "foo|foo\\$tail", "-w", "-o", "app.js"]);
    assert_eq!(
        result["data"]["matches"]
            .as_array()
            .unwrap()
            .iter()
            .map(|r| r["text"].as_str().unwrap())
            .collect::<Vec<_>>(),
        ["foo$tail", "foo", "foo"]
    );
}

#[test]
fn search_word_alternatives_work_on_unindexed_files_and_stdin() {
    use std::io::Write;
    use std::process::Stdio;
    let f = Fixture::new();
    let external = Fixture::new();
    external.write("words.txt", "foobar foo\n");
    let result = f.run(&[
        "search",
        "foo|foobar",
        "-w",
        "-o",
        external.path().join("words.txt").to_str().unwrap(),
    ]);
    assert_eq!(
        result["data"]["matches"]
            .as_array()
            .unwrap()
            .iter()
            .map(|r| r["text"].as_str().unwrap())
            .collect::<Vec<_>>(),
        ["foobar", "foo"]
    );
    let mut child = Command::cargo_bin("codeq")
        .unwrap()
        .current_dir(f.path())
        .args(["--json", "search", "foo|foobar", "-w", "-o", "--stdin"])
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .unwrap();
    child
        .stdin
        .take()
        .unwrap()
        .write_all(b"foobar foo\n")
        .unwrap();
    let output = child.wait_with_output().unwrap();
    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    let result: Value = serde_json::from_slice(&output.stdout).unwrap();
    assert_eq!(
        result["data"]["matches"]
            .as_array()
            .unwrap()
            .iter()
            .map(|r| r["text"].as_str().unwrap())
            .collect::<Vec<_>>(),
        ["foobar", "foo"]
    );
}

#[test]
fn explicit_compaction_reports_active_readers_and_can_be_retried() {
    let f = Fixture::new();
    f.write("a.py", "def helper():\n    return 1\n");
    f.run(&["files"]);
    let reader = rusqlite::Connection::open(f.path().join(".codeq/index.db")).unwrap();
    reader.execute_batch("BEGIN; SELECT * FROM files;").unwrap();
    let blocked = Command::cargo_bin("codeq")
        .unwrap()
        .current_dir(f.path())
        .env("CODEQ_BUSY_TIMEOUT_MS", "0")
        .args(["index", "--compact"])
        .output()
        .unwrap();
    let stderr = String::from_utf8_lossy(&blocked.stderr);
    assert!(!blocked.status.success(), "{stderr}");
    assert!(
        stderr.contains("readers prevented WAL truncation"),
        "{stderr}"
    );
    let count: i64 = reader
        .query_row("SELECT COUNT(*) FROM files", [], |r| r.get(0))
        .unwrap();
    assert_eq!(count, 1);
    reader.execute_batch("ROLLBACK").unwrap();
    f.run(&["index", "--compact"]);
    assert_eq!(
        field_set(&f.run(&["symbols", "helper"]), "name"),
        ["helper"]
    );
    let integrity: String = reader
        .query_row("PRAGMA integrity_check", [], |r| r.get(0))
        .unwrap();
    assert_eq!(integrity, "ok");
    assert!(!reader
        .prepare("PRAGMA foreign_key_check")
        .unwrap()
        .query([])
        .unwrap()
        .next()
        .unwrap()
        .is_some());
}

#[test]
fn an_index_write_failure_stops_parse_workers_and_rolls_back() {
    let f = Fixture::new();
    f.write("seed.txt", "original marker\n");
    f.run(&["files"]);
    let db = rusqlite::Connection::open(f.path().join(".codeq/index.db")).unwrap();
    db.execute_batch(
        "CREATE TRIGGER reject_file BEFORE INSERT ON files BEGIN
         SELECT RAISE(ABORT, 'intentional indexing failure'); END;",
    )
    .unwrap();
    let body = "payload marker\n".repeat(32768);
    for i in 0..64 {
        f.write(&format!("new{i:03}.txt"), &body);
    }
    let output = assert_cmd::Command::cargo_bin("codeq")
        .unwrap()
        .current_dir(f.path())
        .timeout(std::time::Duration::from_secs(10))
        .arg("files")
        .output()
        .unwrap();
    assert_eq!(output.status.code(), Some(1), "{output:?}");
    assert!(String::from_utf8_lossy(&output.stderr).contains("intentional indexing failure"));
    let files: i64 = db
        .query_row("SELECT count(*) FROM files", [], |r| r.get(0))
        .unwrap();
    assert_eq!(files, 1);
}

#[test]
fn call_batches_preserve_offsets_and_receivers_across_edits() {
    let f = Fixture::new();
    for count in [63, 64, 65, 128, 129, 1, 0] {
        let mut source = String::from("def run():\n    pass\n");
        let mut expected = Vec::new();
        for i in 0..count {
            let receiver = (i % 2 == 1).then_some("service".to_string());
            let name = format!("target{i}");
            source.push_str("    ");
            if receiver.is_some() {
                source.push_str("service.");
            }
            let start = source.len() as i64;
            source.push_str(&format!("{name}()\n"));
            expected.push((start, name, receiver));
        }
        f.write("source.py", &source);
        f.run(&["files"]);
        let db = rusqlite::Connection::open(f.path().join(".codeq/index.db")).unwrap();
        let actual: Vec<(i64, String, Option<String>)> = db
            .prepare("SELECT start_byte, name, receiver FROM calls ORDER BY start_byte")
            .unwrap()
            .query_map([], |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?)))
            .unwrap()
            .collect::<Result<_, _>>()
            .unwrap();
        assert_eq!(actual, expected, "call count {count}");
    }
}

#[test]
fn fact_batches_preserve_metadata_across_boundaries_and_edits() {
    let f = Fixture::new();
    for count in [31, 32, 33, 64, 65, 1, 0] {
        let mut source = String::new();
        for i in 0..count {
            if i % 2 == 0 {
                source.push_str(&format!("/// doc {i}\n"));
            }
            if i % 3 == 0 {
                source.push_str("#[test]\n");
            }
            let visibility = ["pub ", "pub(crate) ", ""][i % 3];
            source.push_str(&format!("{visibility}fn item{i}() {{}}\n"));
        }
        f.write("source.rs", &source);
        f.run(&["files"]);
        let db = rusqlite::Connection::open(f.path().join(".codeq/index.db")).unwrap();
        let mut statement = db
            .prepare(
                "SELECT name, signature, doc, visibility, source, is_test, start_byte, end_byte
             FROM facts ORDER BY start_byte",
            )
            .unwrap();
        let mut rows = statement.query([]).unwrap();
        for i in 0..count {
            let row = rows.next().unwrap().unwrap();
            assert_eq!(row.get::<_, String>(0).unwrap(), format!("item{i}"));
            assert!(row
                .get::<_, String>(1)
                .unwrap()
                .contains(&format!("item{i}")));
            let doc: Option<String> = row.get(2).unwrap();
            if i % 2 == 0 {
                assert!(doc.unwrap().contains(&format!("doc {i}")));
            } else {
                assert!(doc.is_none());
            }
            assert_eq!(
                row.get::<_, String>(3).unwrap(),
                ["public", "restricted", "private"][i % 3]
            );
            assert_eq!(row.get::<_, String>(4).unwrap(), "treesitter/rust");
            assert_eq!(row.get::<_, bool>(5).unwrap(), i % 3 == 0);
            let start = row.get::<_, i64>(6).unwrap() as usize;
            let end = row.get::<_, i64>(7).unwrap() as usize;
            assert!(source[start..end].contains(&format!("fn item{i}()")));
        }
        assert!(rows.next().unwrap().is_none(), "fact count {count}");
    }
}

#[test]
fn a_fact_batch_failure_rolls_back_preceding_batches_and_recovers() {
    let f = Fixture::new();
    f.write("source.rs", "pub fn original() {}\n");
    f.run(&["files"]);
    let db = rusqlite::Connection::open(f.path().join(".codeq/index.db")).unwrap();
    db.execute_batch(
        "CREATE TRIGGER reject_fact BEFORE INSERT ON facts WHEN NEW.name = 'item34'
         BEGIN SELECT RAISE(ABORT, 'intentional fact batch failure'); END;",
    )
    .unwrap();
    let source: String = (0..65)
        .map(|i| format!("pub fn item{i}() {{}}\n"))
        .collect();
    f.write("source.rs", &source);
    let output = f.raw(&["files"]);
    assert_eq!(output.status.code(), Some(1), "{output:?}");
    assert!(String::from_utf8_lossy(&output.stderr).contains("intentional fact batch failure"));
    assert_eq!(
        db.query_row("SELECT count(*) FROM facts", [], |r| r.get::<_, i64>(0))
            .unwrap(),
        1
    );
    assert_eq!(
        db.query_row("SELECT name FROM facts", [], |r| r.get::<_, String>(0))
            .unwrap(),
        "original"
    );
    db.execute_batch("DROP TRIGGER reject_fact").unwrap();
    f.run(&["files"]);
    assert_eq!(
        db.query_row("SELECT count(*) FROM facts", [], |r| r.get::<_, i64>(0))
            .unwrap(),
        65
    );
}

#[test]
fn discover_ranks_split_identifiers_docs_paths_and_exact_sources() {
    let f = Fixture::new();
    f.write("src/session.py", "def refreshAuthToken(value):\n    \"\"\"Renew a session credential.\"\"\"\n    return value\n\ndef token():\n    pass\n");
    f.write(
        "src/cache.py",
        "def unrelated():\n    \"\"\"Refresh a cached response.\"\"\"\n    pass\n",
    );
    let result = f.run(&["discover", "refresh auth token", "--source", "--limit", "1"]);
    let data = &result["data"];
    assert_eq!(data["candidates"][0]["name"], "refreshAuthToken");
    assert!(data["candidates"][0]["reasons"]
        .as_array()
        .unwrap()
        .contains(&serde_json::json!("name:auth")));
    assert_eq!(data["truncated"], true);
    assert!(data["source"]
        .as_array()
        .unwrap()
        .iter()
        .any(|c| c["text"].as_str().unwrap().contains("return value")));
    let docs = f.run(&["discover", "session credential", "--limit", "1"]);
    assert_eq!(docs["data"]["candidates"][0]["name"], "refreshAuthToken");
    assert_eq!(
        f.run(&["discover", "refresh auth token", "--path", "src/cache.py"])["data"]["candidates"]
            [0]["name"],
        "unrelated"
    );
    assert_eq!(
        f.run(&["discover", "zzzzzz"])["data"]["candidates"],
        serde_json::json!([])
    );
}

#[test]
fn deps_reverse_and_directory_views_are_bounded_and_transitive() {
    let f = Fixture::new();
    f.write("core/util.ts", "export function helper() {}\n");
    f.write(
        "ui/view.ts",
        "import { helper } from '../core/util';\nexport function view() { helper(); }\n",
    );
    f.write(
        "app/main.ts",
        "import { view } from '../ui/view';\nview();\n",
    );
    let data = f.run(&["deps", "core/util.ts", "--reverse", "--depth", "3"]);
    assert_eq!(field_set(&data, "path"), ["ui/view.ts", "app/main.ts"]);
    assert_eq!(data["data"][1]["depth"], 2);
    let capped = f.run(&[
        "deps",
        "core/util.ts",
        "--reverse",
        "--depth",
        "3",
        "--limit",
        "1",
    ]);
    assert_eq!(capped["data"].as_array().unwrap().len(), 1);
    assert!(capped["note"].as_str().unwrap().contains("1"));
    let grouped = f.run(&["deps", "--by", "dir"]);
    assert!(grouped["data"]
        .as_array()
        .unwrap()
        .iter()
        .any(|r| r["from"] == "ui"
            && r["to"] == "core"
            && r["edges"] == 1
            && r["resolution"] == "internal"));
}

#[test]
fn deps_mermaid_preserves_direction_bounds_and_uncertainty() {
    let f = Fixture::new();
    f.write("core/util.ts", "export function helper() {}\n");
    f.write(
        "ui/view.ts",
        "import { helper } from '../core/util';\nexport function view() { helper(); }\n",
    );
    f.write(
        "app/main.ts",
        "import { view } from '../ui/view';\nimport missing from 'missing';\nview();\n",
    );
    let output = f.raw(&[
        "deps",
        "core/util.ts",
        "--reverse",
        "--depth",
        "3",
        "--mermaid",
    ]);
    assert!(output.status.success());
    let text = String::from_utf8(output.stdout).unwrap();
    assert_eq!(text, "graph LR\n  n0[\"app/main.ts\"]\n  n1[\"core/util.ts\"]\n  n2[\"ui/view.ts\"]\n  n0 -->|\"../ui/view\"| n2\n  n2 -->|\"../core/util\"| n1\n");
    let forward = f.raw(&["deps", "app/main.ts", "--depth", "3", "--mermaid"]);
    assert_eq!(forward.stdout, text.as_bytes());
    let capped = f.raw(&[
        "deps",
        "core/util.ts",
        "--reverse",
        "--depth",
        "3",
        "--limit",
        "1",
        "--mermaid",
    ]);
    let capped = String::from_utf8(capped.stdout).unwrap();
    assert!(!capped.contains("app/main.ts"));
    assert!(capped.contains("stopped at --limit"));
    let grouped = f.raw(&["deps", "--by", "dir", "--mermaid"]);
    assert!(grouped.status.success());
    let grouped = String::from_utf8(grouped.stdout).unwrap();
    assert!(grouped.contains("1 external-or-unresolved"));
    assert!(grouped.contains("n0[\"app\"]"));
    assert!(grouped.contains("-->|\"1\"|"));
    f.write("alone.ts", "export function alone() {}\n");
    let empty = f.raw(&["deps", "alone.ts", "--mermaid"]);
    assert_eq!(
        String::from_utf8(empty.stdout).unwrap(),
        "graph LR\n  n0[\"alone.ts\"]\n  %% no dependency edges\n"
    );
    f.write("odd.ts", "import odd from 'x\"|]';\n");
    let escaped = f.raw(&["deps", "--by", "dir", "--mermaid"]);
    assert!(String::from_utf8(escaped.stdout)
        .unwrap()
        .contains("x#34;#124;#93;"));
    for args in [
        vec!["deps", "--cycles", "--mermaid"],
        vec!["deps", "--external", "--mermaid"],
    ] {
        assert!(!f.raw(&args).status.success());
    }
}

#[test]
fn packages_join_declarations_imports_and_preserve_uncertainty() {
    let f = Fixture::new();
    f.write(
        "package.json",
        r#"{"dependencies":{"express":"^5","unused":"1"}}"#,
    );
    f.write(
        "main.ts",
        "import express from 'express';\nimport missing from '@/missing';\nexpress(); missing();\n",
    );
    let packages = f.run(&["packages", "express"]);
    assert_eq!(packages["data"][0]["declared"], true);
    assert_eq!(
        packages["data"][0]["importers"],
        serde_json::json!(["main.ts"])
    );
    assert_eq!(packages["data"][0]["match_method"], "exact");
    assert_eq!(
        field_set(&f.run(&["packages", "--unimported"]), "name"),
        ["unused"]
    );
    let undeclared = f.run(&["packages", "--undeclared"]);
    assert!(undeclared["data"]
        .as_array()
        .unwrap()
        .iter()
        .any(|p| p["resolution_reason"].is_string()));
    f.write(
        "rust/Cargo.toml",
        "[package]\nname='demo'\nversion='1.0.0'\n[dependencies]\nserde='1'\n",
    );
    f.write(
        "rust/src/lib.rs",
        "use serde::Serialize;\npub fn entry() {}\n",
    );
    let cargo = f.run(&["packages", "serde"]);
    assert_eq!(
        cargo["data"][0]["importers"],
        serde_json::json!(["rust/src/lib.rs"])
    );
    f.write(
        "python/pyproject.toml",
        "[project]\nname='demo'\ndependencies=['httpx>=0.27']\n",
    );
    f.write("python/main.py", "import httpx\n");
    let python = f.run(&["packages", "httpx"]);
    assert_eq!(
        python["data"][0]["match_method"],
        "normalized-name-heuristic"
    );
    f.write(
        "go/go.mod",
        "module example.com/demo\n\nrequire example.com/lib v1.0.0\n",
    );
    f.write(
        "go/main.go",
        "package main\nimport \"example.com/lib/sub\"\nfunc main() {}\n",
    );
    let go = f.run(&["packages", "example.com/lib"]);
    assert_eq!(
        go["data"][0]["importers"],
        serde_json::json!(["go/main.go"])
    );
}

#[test]
fn definition_scopes_select_exact_ranges_before_limits_and_counts() {
    let f = Fixture::new();
    f.write("sample.py", "class A:\n    def run(self):\n        token = 1\n        return token\n\nclass B:\n    def run(self):\n        token = 2\n        return token\n");
    let scoped = f.run(&[
        "search",
        "token",
        "--within-at",
        "sample.py:7",
        "--limit",
        "1",
    ]);
    assert_eq!(scoped["data"]["matches"][0]["line"], 8);
    assert_eq!(scoped["data"]["matches"].as_array().unwrap().len(), 1);
    let count = f.run(&["search", "token", "--within-at", "sample.py:7", "--count"]);
    assert_eq!(count["data"]["counts"][0]["count"], 2);
    let invert = f.run(&["search", "token", "--within-at", "sample.py:7", "-v"]);
    let rows = invert["data"]["matches"].as_array().unwrap();
    assert_eq!(rows.len(), 1);
    assert_eq!(rows[0]["line"], 7);
    let references = f.run(&["references", "token", "--within-at", "sample.py:7"]);
    let rows = references["data"]["references"].as_array().unwrap();
    assert_eq!(rows.len(), 2);
    assert!(rows.iter().all(|r| r["line"].as_u64().unwrap() >= 8));
    let ambiguous = f.raw(&["search", "token", "--within", "run"]);
    assert!(!ambiguous.status.success());
    assert!(String::from_utf8_lossy(&ambiguous.stderr).contains("matched 2 definitions"));
    assert!(!f
        .raw(&["search", "token", "--within", "absent"])
        .status
        .success());
    assert!(!f
        .raw(&[
            "search",
            "token",
            "--within-at",
            "sample.py:1",
            "--within",
            "run"
        ])
        .status
        .success());
}

#[test]
fn references_classify_each_occurrence_with_syntax_evidence() {
    let f = Fixture::new();
    f.write("handler.ts", "import { value } from './lib';\nfunction handler(state: State) {\n  state.value = state.value + 1;\n  state.value += 1;\n  state.value++;\n  value();\n  // value\n  const text = 'value';\n}\n");
    let uses = f.run(&["references", "value", "--path", "handler.ts"]);
    let rows = uses["data"]["references"].as_array().unwrap();
    let kinds: Vec<_> = rows
        .iter()
        .map(|r| (r["line"].as_u64().unwrap(), r["use_kind"].as_str().unwrap()))
        .collect();
    assert_eq!(
        kinds,
        vec![
            (1, "import"),
            (3, "write"),
            (3, "read"),
            (4, "read-write"),
            (5, "read-write"),
            (6, "call"),
            (7, "unknown"),
            (8, "unknown")
        ]
    );
    assert!(rows
        .iter()
        .all(|r| !r["evidence"].as_str().unwrap().is_empty()));
    let text = f.raw(&["references", "value", "--kind", "write"]);
    let text = String::from_utf8(text.stdout).unwrap();
    assert!(
        text.contains("use=write evidence=tree-sitter/typescript:"),
        "{text}"
    );
    let writes = f.run(&["references", "value", "--kind", "write", "--limit", "1"]);
    assert_eq!(writes["data"]["references"][0]["line"], 3);
    assert_eq!(writes["data"]["truncated"], true);
    assert!(writes["note"]
        .as_str()
        .unwrap()
        .contains("could not be classified"));
    let receivers = f.run(&["references", "state", "--kind", "write"]);
    assert!(receivers["data"]["references"]
        .as_array()
        .unwrap()
        .is_empty());
    let types = f.run(&["references", "State", "--kind", "type-use"]);
    assert_eq!(types["data"]["references"].as_array().unwrap().len(), 1);
}

#[test]
fn reference_categories_preserve_unknown_and_declarations() {
    let f = Fixture::new();
    f.write(
        "sample.rs",
        "fn target() {}\nfn handler() { let target = 1; target += 1; target(); }\n",
    );
    let refs = f.run(&["references", "target"]);
    let kinds: Vec<_> = refs["data"]["references"]
        .as_array()
        .unwrap()
        .iter()
        .map(|r| r["use_kind"].as_str().unwrap())
        .collect();
    assert_eq!(kinds, vec!["decl", "decl", "read-write", "call"]);
    f.write("other.go", "package main\nfunc target() {}\n");
    let unknown = f.run(&[
        "references",
        "target",
        "--path",
        "other.go",
        "--kind",
        "unknown",
    ]);
    assert_eq!(unknown["data"]["references"].as_array().unwrap().len(), 1);
    assert!(unknown["data"]["references"][0]["evidence"]
        .as_str()
        .unwrap()
        .contains("unsupported"));
}

#[test]
fn discovery_reasons_and_package_importers_are_visible_in_text() {
    let f = Fixture::new();
    f.write("package.json", r#"{"dependencies":{"lodash":"^4"}}"#);
    f.write(
        "src/auth.ts",
        "import { get } from 'lodash';\nexport function refreshAuthToken() { return get({}); }\n",
    );
    let discover = f.raw(&["discover", "refresh auth token"]);
    assert!(discover.status.success());
    let text = String::from_utf8(discover.stdout).unwrap();
    assert!(text.contains("reasons=name:auth"), "{text}");
    let packages = f.raw(&["packages", "lodash"]);
    assert!(packages.status.success());
    let text = String::from_utf8(packages.stdout).unwrap();
    assert!(text.contains("importers=src/auth.ts"), "{text}");
}

#[test]
fn scoped_refresh_matches_a_rebuild_after_structural_and_ignore_changes() {
    let f = Fixture::new();
    f.git(&["init", "-q"]);
    f.write("src/a.py", "def original(): pass\n");
    f.write("src_other/a.py", "def untouched(): pass\n");
    f.write("other/b.py", "def sibling(): pass\n");
    f.run(&["symbols", "--limit", "0"]);
    for step in 0..8 {
        match step {
            0 => {
                f.write("src/a.py", "def edited(): pass\n");
            }
            1 => {
                f.write("src/nested/new.py", "def added(): pass\n");
            }
            2 => {
                f.remove("src/a.py");
                f.write("other/b.py", "def changed_sibling(): pass\n");
            }
            3 => {
                std::fs::rename(f.path().join("src/nested"), f.path().join("src/renamed")).unwrap();
            }
            4 => {
                f.write("src/.gitignore", "renamed/\n");
            }
            5 => {
                f.write("src/.gitignore", "# include everything\n");
            }
            6 => {
                std::fs::remove_dir_all(f.path().join("src/renamed")).unwrap();
            }
            7 => {
                f.write(".codeqignore", "other/\n");
            }
            _ => unreachable!(),
        }
        let symbols = f.run(&["symbols", "--limit", "0"])["data"].clone();
        let search = f.run(&["search", "def", "--limit", "0"])["data"].clone();
        f.run(&["index", "--rebuild"]);
        let rebuilt = f.run(&["symbols", "--limit", "0"])["data"].clone();
        assert_eq!(symbols, rebuilt, "symbols after step {step}");
        assert_eq!(
            search,
            f.run(&["search", "def", "--limit", "0"])["data"],
            "search after step {step}"
        );
        assert_eq!(
            f.run(&["--meta", "symbols"])["index"]["reindexed_files"]
                .as_u64()
                .unwrap_or(0),
            0
        );
    }
}

#[test]
fn markup_formats_expose_elements_ranges_links_and_existing_templates() {
    let f = Fixture::new();
    f.write("pages/index.html", "<!doctype html>\n<main id='content'>\n<!-- <aside id='fake'> -->\n<p>Hello</p>\n<a href='next.html#here'>Next</a>\n<img src='https://example.org/x.png'>\n<script>const s = '<div id=\"fake\">';</script>\n{% block body %}Body{% endblock %}\n</main>\n");
    f.write("pages/next.html", "<h1 id='here'>Next</h1>\n");
    f.write("elsewhere/next.html", "<p>Wrong</p>\n");
    let outline = f.run(&["outline", "pages/index.html"]);
    let names = field_set(&outline, "name");
    assert!(names.contains(&"content".into()), "{outline}");
    assert!(
        names.contains(&"body".into()),
        "Jinja block preserved: {outline}"
    );
    assert!(!names.contains(&"fake".into()), "{outline}");
    assert_eq!(
        field_set(&f.run(&["deps", "pages/index.html"]), "path"),
        ["pages/next.html"]
    );
    let read = f.raw(&["read", "pages/index.html", "--symbol", "content", "--raw"]);
    assert!(read.status.success());
    let body = String::from_utf8(read.stdout).unwrap();
    assert!(
        body.starts_with("<main") && body.trim_end().ends_with("</main>"),
        "{body}"
    );
    assert!(field_set(&f.run(&["at", "pages/index.html:4"]), "name").contains(&"content".into()));
    f.write("diagram.svg", "<svg xmlns='http://www.w3.org/2000/svg'>\n<!-- <g id='fake'/> -->\n<g id='layer'><path id='shape'/><text><![CDATA[<fake/>]]></text></g>\n</svg>\n");
    let names = field_set(&f.run(&["outline", "diagram.svg"]), "name");
    assert!(
        names.contains(&"layer".into()) && names.contains(&"shape".into()),
        "{names:?}"
    );
    assert!(!names.iter().any(|s| s.contains("fake")), "{names:?}");
    f.write(
        "data.xml",
        "<root><item>one</item><item>two</item></root>\n",
    );
    assert_eq!(
        field_set(&f.run(&["outline", "data.xml"]), "name"),
        ["/root[1]", "/root[1]/item[1]", "/root[1]/item[2]"]
    );
    f.write("bad.xml", "<root><item></root>");
    f.run(&["outline", "bad.xml"]);
    f.write("pages/index.html", "<main id='updated'>New</main>\n");
    assert_eq!(
        field_set(&f.run(&["outline", "pages/index.html"]), "name"),
        ["updated"]
    );
}

#[test]
fn flat_configuration_formats_index_keys_without_exposing_values() {
    let f = Fixture::new();
    f.write(".ignore", "!.env*\n");
    f.write("settings.ini", "# note\r\n[server]\r\nhost = secret-host\r\nmultiline = first\r\n  second\r\n[other]\r\na.b: value\r\n");
    let outline = f.run(&["outline", "settings.ini"]);
    assert_eq!(
        field_set(&outline, "name"),
        [
            "server",
            "server.host",
            "server.multiline",
            "other",
            "other[\"a.b\"]"
        ]
    );
    assert!(!outline.to_string().contains("secret-host"), "{outline}");
    let read = f.raw(&[
        "read",
        "settings.ini",
        "--symbol",
        "server.multiline",
        "--raw",
    ]);
    assert!(read.status.success());
    assert_eq!(read.stdout, b"multiline = first\r\n  second\r\n");
    f.write(".env.local", "# comment\nexport API_TOKEN=secret-token\nMULTI=\"hello\nNOT_A_KEY=still value\nworld\"\nNEXT=ok\n9INVALID=no\n");
    let outline = f.run(&["outline", ".env.local"]);
    assert_eq!(field_set(&outline, "name"), ["API_TOKEN", "MULTI", "NEXT"]);
    assert!(!outline.to_string().contains("secret-token"), "{outline}");
    assert!(field_set(&f.run(&["at", ".env.local:4"]), "name").contains(&"MULTI".into()));
    f.write("indented.ini", "[server]\n  host = x\n  port = 80\n");
    assert_eq!(
        field_set(&f.run(&["outline", "indented.ini"]), "name"),
        ["server", "server.host", "server.port"]
    );
    f.write("messages.properties", "! comment\nhello\\ world = hi\nuni\\u0063ode: text\nlong = first\\\n  CONTINUATION=no\nempty\n");
    assert_eq!(
        field_set(&f.run(&["outline", "messages.properties"]), "name"),
        ["hello world", "unicode", "long", "empty"]
    );
    assert!(f.run(&["callees", "API_TOKEN"])["data"]
        .as_array()
        .unwrap()
        .is_empty());
    assert!(field_set(&f.run(&["unused", "."]), "name").is_empty());
}

#[test]
fn markup_entities_and_properties_logical_keys_keep_source_offsets() {
    let f = Fixture::new();
    f.write(
        "index.html",
        "<input id='input' name='other'><a href='a&amp;b.html'>Link</a>",
    );
    f.write("a&b.html", "<p>ok</p>");
    assert_eq!(
        field_set(&f.run(&["outline", "index.html"]), "name"),
        ["input", "a"]
    );
    assert_eq!(
        field_set(&f.run(&["deps", "index.html"]), "path"),
        ["a&b.html"]
    );
    f.write("keys.properties", "long\\\n  key=value\nfoo\\ =space\n");
    assert_eq!(
        field_set(&f.run(&["outline", "keys.properties"]), "name"),
        ["longkey", "foo "]
    );
    let read = f.raw(&["read", "keys.properties", "--symbol", "longkey", "--raw"]);
    assert!(read.status.success());
    assert_eq!(read.stdout, b"long\\\n  key=value\n");
}

#[test]
fn flat_configuration_limits_include_section_only_files() {
    let f = Fixture::new();
    let content: String = (0..10_001).map(|n| format!("[section{n}]\n")).collect();
    f.write("large.ini", &content);
    assert!(field_set(&f.run(&["outline", "large.ini"]), "name").is_empty());
    assert!(
        !f.run(&["search", "section10000", "large.ini"])["data"]["matches"]
            .as_array()
            .unwrap()
            .is_empty()
    );
}
