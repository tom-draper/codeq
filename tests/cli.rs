//! Integration tests: drive the real `codeq` binary against programmatic
//! fixture repositories and assert on the JSON contract agents consume.
//!
//! These deliberately exercise the CLI end to end (not internals), because the
//! stable output envelope *is* the product surface.

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

/// Collect the `symbol` of every chunk in a `context` result's `data` object.
fn chunk_symbols(data: &Value) -> Vec<String> {
    data["chunks"]
        .as_array()
        .unwrap()
        .iter()
        .filter_map(|c| c["symbol"].as_str().map(|s| s.to_string()))
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
    let v = f.run(&["stats"]); // second call: nothing changed

    let mut keys: Vec<&str> = v.as_object().unwrap().keys().map(|s| s.as_str()).collect();
    keys.sort();
    assert_eq!(keys, ["data", "schema"], "clean call should carry no meta");
    assert_eq!(v["schema"], "codeq.result.v1");
}

#[test]
fn meta_flag_returns_full_stable_envelope() {
    let f = Fixture::new();
    f.write("a.py", "def x():\n    pass\n");
    let v = f.run(&["stats", "--meta"]);

    let mut keys: Vec<&str> = v.as_object().unwrap().keys().map(|s| s.as_str()).collect();
    keys.sort();
    assert_eq!(keys, ["data", "index", "repo", "schema"]);
    assert_eq!(v["schema"], "codeq.result.v1");
    assert_eq!(v["index"]["schema"], 16);
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
        let mut keys: Vec<&str> = row.as_object().unwrap().keys().map(|s| s.as_str()).collect();
        keys.sort();
        assert_eq!(keys, ["name", "path"], "projection should drop other fields");
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
    let d = &v["data"];
    let cmds = field_names(&d["commands"]);
    assert!(cmds.contains(&"context".to_string()), "{cmds:?}");
    assert!(cmds.contains(&"capabilities".to_string()));
    let langs = field_names(&d["extraction_languages"]);
    assert!(langs.contains(&"rust".to_string()) && langs.contains(&"python".to_string()));
    assert_eq!(d["result_schema"], "codeq.result.v1");

    // Default output is text like every other command, not a raw JSON dump.
    let text = String::from_utf8_lossy(&f.raw(&["capabilities"]).stdout).into_owned();
    assert!(!text.contains("\"schema\""), "capabilities text mode dumped JSON: {text}");
    assert!(text.contains("# commands"), "capabilities text missing sections: {text}");
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
fn non_git_directory_works() {
    let f = Fixture::new();
    f.write("a.py", "def z():\n    pass\n");
    let v = f.run(&["stats", "--meta"]);
    assert_eq!(v["repo"]["vcs"], "none");
    assert!(v["repo"].get("head").is_none());
    assert_eq!(v["data"]["files"], 1);
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
    let v = f.run(&["stats"]);
    assert!(v["data"]["files"].as_i64().unwrap() >= 2);
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
    let v = f.run(&["stats"]);
    assert!(
        v.get("index").is_none(),
        "identical rewrite must not count as reindex: {v}"
    );

    // Real content change: exactly one file reparsed.
    f.write("a.rs", "fn one() {}\nfn two() {}\n");
    let v = f.run(&["stats"]);
    assert_eq!(v["index"]["reindexed_files"], 1);
}

#[test]
fn deleting_a_file_cascades_to_its_facts() {
    let f = Fixture::new();
    f.write("a.rs", "fn alpha() {}\n");
    assert_eq!(f.run(&["definitions", "alpha"])["data"].as_array().unwrap().len(), 1);

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
    assert_eq!(by_path, vec!["lib.rs".to_string()], "path query should resolve via stem");
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
    assert!(!mods.iter().any(|m| m == "nope" || m == "no"),
            "a non-require string leaked as an import: {mods:?}");
    // Definitions must still be extracted alongside imports.
    assert!(field_set(&f.run(&["symbols"]), "name").contains(&"m".to_string()));
}

#[test]
fn definitions_returns_precise_ranges() {
    let f = Fixture::new();
    f.write("m.py", "def first():\n    pass\n\ndef target():\n    return 1\n");
    let v = f.run(&["definitions", "target"]);
    let rows = v["data"].as_array().unwrap();
    assert_eq!(rows.len(), 1);
    let r = &rows[0];
    assert_eq!(r["kind"], "function");
    assert_eq!(r["start_line"], 4); // 1-based
    assert!(r["end_byte"].as_i64().unwrap() > r["start_byte"].as_i64().unwrap());
}

// ---------------------------------------------------------------------------
// Git-aware changed + exit codes
// ---------------------------------------------------------------------------

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
    assert!(!paths.contains(&"b.rs".to_string()), "unchanged b leaked: {paths:?}");
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
            .as_array().unwrap().iter()
            .filter(|r| r["kind"] == k)
            .map(|r| r["name"].as_str().unwrap().to_string())
            .collect()
    };
    assert!(by_kind("interface").contains(&"User".to_string()));
    assert!(by_kind("type").contains(&"UserId".to_string()));
    assert!(by_kind("enum").contains(&"Role".to_string()));
    assert!(by_kind("class").contains(&"Service".to_string()));
    assert!(by_kind("method").contains(&"find".to_string()), "method under class");
    assert!(by_kind("function").contains(&"auth".to_string()), "arrow-const fn");

    let mods = field_set(&f.run(&["imports"]), "module");
    assert!(mods.contains(&"./client".to_string()), "quotes stripped: {mods:?}");
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
            .as_array().unwrap().iter()
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
            .as_array().unwrap().iter()
            .filter(|r| r["kind"] == k)
            .map(|r| r["name"].as_str().unwrap().to_string())
            .collect()
    };
    assert!(by_kind("struct").contains(&"Server".to_string()));
    assert!(by_kind("interface").contains(&"Handler".to_string()));
    assert!(by_kind("function").contains(&"New".to_string()));
    assert!(by_kind("method").contains(&"Start".to_string()));

    let mods = field_set(&f.run(&["imports"]), "module");
    assert!(mods.contains(&"fmt".to_string()), "go import quotes stripped: {mods:?}");
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
        syms["data"].as_array().unwrap().iter()
            .filter(|r| r["kind"] == k)
            .map(|r| r["name"].as_str().unwrap().to_string()).collect()
    };
    assert!(by_kind("interface").contains(&"H".to_string()));
    assert!(by_kind("class").contains(&"S".to_string()));
    assert!(by_kind("method").contains(&"go".to_string()));
    assert!(field_set(&f.run(&["imports"]), "module").contains(&"java.util.List".to_string()));
}

#[test]
fn ruby_symbols_and_nesting() {
    let f = Fixture::new();
    f.write("s.rb", "module A\n  class B\n    def m\n    end\n  end\nend\n\ndef top\nend\n");
    let syms = field_set(&f.run(&["symbols"]), "name");
    for want in ["A", "B", "m", "top"] {
        assert!(syms.contains(&want.to_string()), "missing {want}: {syms:?}");
    }
    // Method m nests under class B under module A.
    let out = f.run(&["outline", "s.rb"]);
    let m = out["data"].as_array().unwrap().iter().find(|r| r["name"] == "m").unwrap();
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
    assert!(names.contains(&"g".to_string()), "pointer-returning fn: {names:?}");
    let mods = field_set(&f.run(&["imports"]), "module");
    assert!(mods.contains(&"stdio.h".to_string()), "angle include stripped: {mods:?}");
    assert!(mods.contains(&"local.h".to_string()), "quoted include stripped: {mods:?}");
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
        assert!(names.contains(&want.to_string()), "missing {want}: {names:?}");
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
        syms["data"].as_array().unwrap().iter()
            .filter(|r| r["kind"] == k)
            .map(|r| r["name"].as_str().unwrap().to_string()).collect()
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
        assert!(names.contains(&want.to_string()), "missing {want}: {names:?}");
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
    f.write("m.py", "def greet():\n    \"\"\"Say hi.\"\"\"\n    return 1\n");
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
    assert!(names.contains(&"exported".to_string()), "pub fn missing: {names:?}");
    assert!(names.contains(&"Public".to_string()), "pub struct missing: {names:?}");
    assert!(!names.contains(&"internal".to_string()), "private fn leaked: {names:?}");
    assert!(!names.contains(&"crate_only".to_string()),
            "pub(crate) is restricted, not public: {names:?}");
}

#[test]
fn api_visibility_rules_per_language() {
    // Go: exported by capitalization.
    let g = Fixture::new();
    g.write("m.go", "package m\nfunc Exported() {}\nfunc unexported() {}\n");
    let gn = field_set(&g.run(&["api", "m.go"]), "name");
    assert!(gn.contains(&"Exported".to_string()) && !gn.contains(&"unexported".to_string()), "{gn:?}");

    // Python: public unless leading underscore.
    let p = Fixture::new();
    p.write("m.py", "def public():\n    pass\ndef _private():\n    pass\n");
    let pn = field_set(&p.run(&["api", "m.py"]), "name");
    assert!(pn.contains(&"public".to_string()) && !pn.contains(&"_private".to_string()), "{pn:?}");

    // JS: exported iff `export`.
    let j = Fixture::new();
    j.write("m.js", "export function shown() {}\nfunction hidden() {}\n");
    let jn = field_set(&j.run(&["api", "m.js"]), "name");
    assert!(jn.contains(&"shown".to_string()) && !jn.contains(&"hidden".to_string()), "{jn:?}");
}

#[test]
fn api_kind_filter_and_signature() {
    let f = Fixture::new();
    f.write("a.rs", "/// A doc.\npub fn func() {}\npub struct S;\n");
    // --kind narrows to one kind.
    let v = f.run(&["api", "a.rs", "--kind", "function"]);
    let rows = v["data"].as_array().unwrap();
    assert_eq!(rows.len(), 1, "kind filter should return only the function: {rows:?}");
    assert_eq!(rows[0]["name"], "func");
    assert_eq!(rows[0]["signature"], "pub fn func()", "signature is carried");
}

#[test]
fn symbols_signature_is_opt_in() {
    let f = Fixture::new();
    f.write("a.rs", "fn solo() {}\n");

    let plain = f.run(&["symbols"]);
    assert!(plain["data"][0].get("signature").is_none(), "should be off by default");

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
        rows.iter()
            .find(|r| r["name"] == name)
            .unwrap()["depth"]
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
    assert!(use_site.get("definition").is_none(), "use site is not a definition");
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
    let syms: Vec<String> = callers["data"].as_array().unwrap().iter()
        .filter_map(|r| r["symbol"].as_str().map(String::from)).collect();
    assert!(syms.contains(&"caller".to_string()) && syms.contains(&"other".to_string()), "{syms:?}");
    assert!(!syms.contains(&"helper".to_string()), "own definition is not a caller");
    // The comment mention must not appear (code-only).
    assert_eq!(callers["data"].as_array().unwrap().len(), 2, "comment mention leaked");

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
fn callers_resolved_prunes_unreachable_and_tags_via() {
    let f = Fixture::new();
    f.write("def.rs", "pub fn target() {}\nfn self_caller() { target(); }\n");
    f.write("user.rs", "use crate::def::target;\nfn user() { target(); }\n");
    f.write("stranger.rs", "fn stranger() { let target = 1; let _ = target; }\n");

    // Default (name-based) includes the unrelated `target` in stranger.rs.
    let all = field_set(&f.run(&["callers", "target"]), "path");
    assert!(all.contains(&"stranger.rs".to_string()), "default should include stranger: {all:?}");

    // --resolved drops the reference that can't reach a definition, and tags the
    // rest by how they connect.
    let res = f.run(&["callers", "target", "--resolved"]);
    let rows = res["data"].as_array().unwrap();
    let via = |sym: &str| rows.iter()
        .find(|r| r["symbol"] == sym)
        .map(|r| r["via"].as_str().unwrap().to_string());
    assert_eq!(via("self_caller"), Some("same-file".into()), "{rows:?}");
    assert_eq!(via("user"), Some("imported".into()), "{rows:?}");
    assert!(!rows.iter().any(|r| r["path"] == "stranger.rs"),
            "unreachable reference should be pruned: {rows:?}");
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
    assert!(all.contains(&"orphan.rs".to_string()),
            "default should over-match the unrelated same-named def: {all:?}");

    // --resolved keeps only the reachable (same-file) callee, tags it via, and
    // prunes the unrelated same-named def in the unreachable file.
    let res = f.run(&["callees", "run", "--resolved"]);
    let rows = res["data"].as_array().unwrap();
    assert!(rows.iter().any(|r| r["path"] == "core.rs" && r["via"] == "same-file"), "{rows:?}");
    assert!(!rows.iter().any(|r| r["path"] == "orphan.rs"),
            "unreachable same-named callee should be pruned: {rows:?}");
}

#[test]
fn callers_examples_surface_the_call_expression() {
    let f = Fixture::new();
    f.write("a.rs", "fn target(x: i32) {}\nfn caller() { target(41); }\n");

    // Plain callers: the enclosing definition, no source text.
    let plain = f.run(&["callers", "target"]);
    let row = &plain["data"].as_array().unwrap()[0];
    assert_eq!(row["symbol"], "caller");
    assert!(row.get("text").is_none(), "plain callers should carry no source text: {row:?}");

    // --examples: the row becomes a usage example with the actual call line + column.
    let ex = f.run(&["callers", "target", "--examples"]);
    let row = &ex["data"].as_array().unwrap()[0];
    assert_eq!(row["symbol"], "caller");
    assert!(row["text"].as_str().unwrap().contains("target(41)"),
            "example should show the call expression: {row:?}");
    assert!(row["col"].as_i64().is_some(), "example row should carry a column: {row:?}");
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
    assert!(d["focus"][0]["signature"].as_str().unwrap().contains("helper"), "{d}");

    // callers include the enclosing caller definition.
    let callers: Vec<&str> = d["callers"].as_array().unwrap().iter()
        .filter_map(|c| c["name"].as_str()).collect();
    assert!(callers.contains(&"caller"), "{callers:?}");

    // source carries verbatim bodies, each tagged by role; the focus body is present.
    let src = d["source"].as_array().unwrap();
    let focus_chunk = src.iter().find(|c| c["reason"] == "focus").expect("focus source chunk");
    assert!(focus_chunk["text"].as_str().unwrap().contains("x + 1"), "{focus_chunk}");
    assert!(src.iter().any(|c| c["reason"] == "caller" && c["symbol"] == "caller"),
            "caller source missing: {src:?}");
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
    assert!(src.iter().any(|c| c["reason"] == "focus"), "focus must survive the budget: {src:?}");
    let omitted: Vec<&str> = d["data"]["omitted"].as_array().unwrap().iter()
        .filter_map(|o| o["symbol"].as_str()).collect();
    assert!(omitted.contains(&"caller"), "oversized caller should be reported omitted: {omitted:?}");
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
    let changed: Vec<&str> = d["changed"].as_array().unwrap().iter()
        .filter_map(|c| c["name"].as_str()).collect();
    assert!(changed.contains(&"core"), "{changed:?}");

    // blast radius includes the direct caller (transitively, the test too).
    let impacted: Vec<&str> = d["impacted"].as_array().unwrap().iter()
        .filter_map(|c| c["name"].as_str()).collect();
    assert!(impacted.contains(&"mid"), "blast radius missing caller: {impacted:?}");

    // the test that exercises the change only via mid is reported, at file level.
    let test_files: Vec<&str> = d["tests"].as_array().unwrap().iter()
        .filter_map(|t| t["path"].as_str()).collect();
    assert!(test_files.contains(&"t.rs"),
            "transitively-affected test file missing: {test_files:?}");
}

#[test]
fn extracts_definitions_for_added_languages() {
    let f = Fixture::new();
    f.write("s.sh", "greet() { echo hi; }\nfunction bye { echo bye; }\n");
    f.write("m.lua", "local function helper(x) return x end\nfunction process(n) return helper(n) end\n");
    f.write("a.scala", "class Foo { def bar(): Int = 1 }\ntrait Greeter { def greet(): String }\n");
    f.write("K.kt", "package p\nclass Widget {\n    fun render(): String = \"w\"\n}\nfun topLevel(): Int = 1\n");
    f.write("V.swift", "class Vehicle { func drive() -> String { return \"v\" } }\nprotocol Named { func name() -> String }\n");

    // Each new language's definitions land in the symbol table.
    let syms = field_set(&f.run(&["symbols", "--limit", "0"]), "name");
    for want in [
        "greet", "bye", "helper", "process", "Foo", "bar", "Greeter",
        "Widget", "render", "topLevel", "Vehicle", "drive", "Named",
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
    f.write("w.dart", "class Widget {\n  int render(String s) => s.length;\n}\nint topLevel(int n) => n * 2;\n");
    f.write("m.zig", "pub fn compute(a: i32, b: i32) i32 { return a + b; }\nconst Point = struct { x: i32 };\n");
    f.write("app.ex", "defmodule Math do\n  def run(a, b) do\n    helper(a) + b\n  end\n  defp helper(x), do: x\nend\n");
    f.write("W.sol", "contract Wallet {\n  function deposit(uint amt) public {}\n}\n");

    let syms = field_set(&f.run(&["symbols", "--limit", "0"]), "name");
    for want in [
        "Widget", "render", "topLevel", "compute", "Point",
        "Math", "run", "helper", "Wallet", "deposit",
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
fn resolved_follows_rust_mod_declarations_as_edges() {
    // A Rust file that pulls in another via `mod thing;` (not `use crate::…`)
    // can still reach that file's definitions — the `mod` edge must count.
    let f = Fixture::new();
    f.write("lib.rs", "mod thing;\nfn root() { make(); }\n");
    f.write("thing.rs", "pub fn make() {}\n");

    let res = f.run(&["callers", "make", "--resolved"]);
    let row = res["data"].as_array().unwrap().iter()
        .find(|r| r["symbol"] == "root").expect("root should survive resolution");
    assert_eq!(row["via"], "imported", "mod-declared dependency should resolve: {res}");
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
    f.write("user.py", "from pkg import target\ndef use():\n    return target.make()\n");

    let res = f.run(&["callers", "make", "--resolved"]);
    let row = res["data"].as_array().unwrap().iter()
        .find(|r| r["symbol"] == "use").expect("use() should survive resolution");
    assert_eq!(row["via"], "imported", "from-import of a submodule should resolve: {res}");
}

#[test]
fn resolved_follows_js_named_imports() {
    // `import { make } from './util'` — the named binding is captured qualified
    // (./util.make) so the importer resolves to the file exporting it.
    let f = Fixture::new();
    f.write("util.js", "export function make() { return 1; }\n");
    f.write("user.js", "import { make } from './util';\nexport function use() { return make(); }\n");
    f.write("alias.js", "import { make as m } from './util';\nexport function viaAlias() { return m(); }\n");

    let mods = field_set(&f.run(&["imports", "user.js"]), "module");
    assert!(mods.contains(&"./util.make".to_string()), "named import not qualified: {mods:?}");
    // Aliases record the *source* name (what determines the exporting file).
    let amods = field_set(&f.run(&["imports", "alias.js"]), "module");
    assert!(amods.contains(&"./util.make".to_string()), "alias should record source name: {amods:?}");

    // user.js calls make() directly → resolves as imported.
    let res = f.run(&["callers", "make", "--resolved"]);
    let row = res["data"].as_array().unwrap().iter()
        .find(|r| r["symbol"] == "use").expect("use() should survive resolution");
    assert_eq!(row["via"], "imported", "{res}");
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
    let all = field_set(&f.run(&["callers", "make"]), "symbol");
    assert!(all.contains(&"mentioner".to_string()), "default should include the mention: {all:?}");

    // --calls keeps only the real call site.
    let calls = field_set(&f.run(&["callers", "make", "--calls"]), "symbol");
    assert!(calls.contains(&"caller".to_string()), "real caller dropped: {calls:?}");
    assert!(!calls.contains(&"mentioner".to_string()), "bare mention not excluded: {calls:?}");
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
            .as_array().unwrap().iter().map(|r| r["line"].as_i64().unwrap()).collect()
    };
    assert_eq!(lines("Foo"), [2], "only Foo::make()");
    assert_eq!(lines("Bar"), [3], "only Bar::make()");
    assert_eq!(lines("thing"), [4], "method-call receiver");
}

#[test]
fn resolved_is_a_noop_without_a_definition_to_anchor() {
    // A name with no definition in the repo (an external symbol) can't be
    // resolved against anything, so --resolved must not silently empty the result.
    let f = Fixture::new();
    f.write("a.rs", "fn c1() { external_thing(); }\nfn c2() { external_thing(); }\n");
    let plain = f.run(&["callers", "external_thing"]);
    let resolved = f.run(&["callers", "external_thing", "--resolved"]);
    assert_eq!(
        plain["data"].as_array().unwrap().len(),
        resolved["data"].as_array().unwrap().len(),
        "no definition to anchor → --resolved should leave the result unchanged",
    );
    // And it must not have tagged anything.
    assert!(resolved["data"].as_array().unwrap().iter().all(|r| r.get("via").is_none()));
}

#[test]
fn callers_default_has_no_via_field() {
    // Resolution is opt-in: the default output shape is unchanged.
    let f = Fixture::new();
    f.write("a.rs", "fn helper() {}\nfn caller() { helper(); }\n");
    let v = f.run(&["callers", "helper"]);
    assert!(v["data"].as_array().unwrap().iter().all(|r| r.get("via").is_none()),
            "default callers must not carry a via field");
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
    let syms1: Vec<String> = d1["data"].as_array().unwrap().iter()
        .filter_map(|r| r["symbol"].as_str().map(String::from)).collect();
    assert_eq!(syms1, ["mid"], "depth 1 is direct only: {syms1:?}");

    // depth 2: reaches top through mid, each tagged with its hop distance.
    let d2 = f.run(&["callers", "leaf", "--depth", "2"]);
    let rows = d2["data"].as_array().unwrap();
    let depth_of = |sym: &str| rows.iter()
        .find(|r| r["symbol"] == sym).unwrap()["depth"].as_u64().unwrap();
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
    let subs: Vec<&str> = rows.iter().map(|r| r["subtype"].as_str().unwrap()).collect();
    assert!(subs.contains(&"Circle") && subs.contains(&"Square"), "{subs:?}");
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
    assert_eq!(animal["data"][0]["subtype"], "Dog");
    assert_eq!(animal["data"][0]["relation"], "extends");

    let runner = f.run(&["implementors", "Runner"]);
    assert_eq!(runner["data"][0]["subtype"], "Dog");
    assert_eq!(runner["data"][0]["relation"], "implements");

    // --relation filters.
    let only_ext = f.run(&["implementors", "Animal", "--relation", "implements"]);
    assert_eq!(only_ext["data"].as_array().unwrap().len(), 0,
               "Animal is extended, not implemented");
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
    let by: std::collections::HashMap<&str, &str> = rows.iter()
        .map(|r| (r["supertype"].as_str().unwrap(), r["relation"].as_str().unwrap()))
        .collect();
    assert_eq!(by.get("Animal"), Some(&"extends"), "{by:?}");
    assert_eq!(by.get("Runner"), Some(&"implements"), "{by:?}");

    // Inverse: Dog appears in implementors of each of its supertypes.
    for (sup_name, rel) in [("Animal", "extends"), ("Runner", "implements")] {
        let imp = f.run(&["implementors", sup_name]);
        let row = imp["data"].as_array().unwrap().iter()
            .find(|r| r["subtype"] == "Dog").expect("Dog missing from implementors");
        assert_eq!(row["relation"], rel);
    }

    // --relation filters supertypes too.
    let ext = f.run(&["supertypes", "Dog", "--relation", "extends"]);
    let names = field_set(&ext, "supertype");
    assert!(names.contains(&"Animal".to_string()) && !names.contains(&"Runner".to_string()), "{names:?}");
}

#[test]
fn implementors_covers_python_and_ruby_inheritance() {
    let f = Fixture::new();
    f.write("m.py", "class Animal:\n    pass\nclass Dog(Animal):\n    pass\n");
    f.write("m.rb", "class Base\nend\nclass Sub < Base\nend\n");

    let py = f.run(&["implementors", "Animal"]);
    assert_eq!(py["data"][0]["subtype"], "Dog");
    let rb = f.run(&["implementors", "Base"]);
    assert_eq!(rb["data"][0]["subtype"], "Sub");
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
    assert!(names.contains(&"dead".to_string()), "dead fn not reported: {names:?}");
    assert!(!names.contains(&"used".to_string()), "used fn wrongly reported: {names:?}");
    // `caller` references `used` but nothing references `caller` → also unused.
    assert!(names.contains(&"caller".to_string()), "{names:?}");
}

#[test]
fn unused_is_conservative_about_string_mentions() {
    // `unused` under-reports rather than risk flagging something an agent might
    // delete, so a mention in a *string* counts as a use: it may be reflection,
    // a serde rename, a config key or a SQL identifier. (A mention in a comment
    // does not count — see `a_comment_mention_no_longer_masks_dead_code`.)
    let f = Fixture::new();
    f.write("a.rs", "fn ghost() {}\nfn main() { log(\"ghost is named here\"); }\nfn log(_: &str) {}\n");
    let names = field_set(&f.run(&["unused"]), "name");
    assert!(!names.contains(&"ghost".to_string()),
            "a string mention should count as a use: {names:?}");
}

#[test]
fn unused_excludes_test_files_by_default() {
    let f = Fixture::new();
    f.write("src/lib.rs", "fn dead() {}\n");
    f.write("tests/it.rs", "fn a_test_case() {}\n");

    // Default: the test-file definition is not reported.
    let def = field_set(&f.run(&["unused"]), "name");
    assert!(def.contains(&"dead".to_string()), "real dead code missing: {def:?}");
    assert!(!def.contains(&"a_test_case".to_string()),
            "test-file def leaked by default: {def:?}");

    // --include-tests brings it in.
    let inc = field_set(&f.run(&["unused", "--include-tests"]), "name");
    assert!(inc.contains(&"a_test_case".to_string()), "{inc:?}");
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
    let v = f.run(&["path", "top", "leaf"]);
    let steps: Vec<&str> = v["data"].as_array().unwrap().iter()
        .map(|r| r["symbol"].as_str().unwrap()).collect();
    assert_eq!(steps, ["top", "mid", "leaf"], "chain wrong: {steps:?}");
    // Depth indexes the position in the chain.
    let depths: Vec<u64> = v["data"].as_array().unwrap().iter()
        .map(|r| r["depth"].as_u64().unwrap()).collect();
    assert_eq!(depths, [0, 1, 2]);
}

#[test]
fn path_scopes_the_start_symbol_by_path() {
    // Two `entry` functions in different files; only one reaches `leaf`.
    let f = Fixture::new();
    f.write("a.rs", "fn leaf() {}\nfn entry() { leaf(); }\n");
    f.write("b.rs", "fn entry() { unrelated(); }\nfn unrelated() {}\n");

    // Scoped to b.rs, `entry` can't reach leaf.
    let none = f.run(&["path", "entry", "leaf", "--path", "b.rs"]);
    assert_eq!(none["data"].as_array().unwrap().len(), 0, "b.rs entry shouldn't reach leaf");

    // Scoped to a.rs, it does — and starts from the a.rs definition.
    let hit = f.run(&["path", "entry", "leaf", "--path", "a.rs"]);
    let rows = hit["data"].as_array().unwrap();
    assert_eq!(rows[0]["symbol"], "entry");
    assert_eq!(rows[0]["path"], "a.rs", "start location should be the scoped def");
    let syms: Vec<&str> = rows.iter().map(|r| r["symbol"].as_str().unwrap()).collect();
    assert_eq!(syms, ["entry", "leaf"]);
}

#[test]
fn path_empty_when_unreachable_and_single_when_self() {
    let f = Fixture::new();
    f.write("a.rs", "fn leaf() {}\nfn top() { leaf(); }\n");

    // leaf does not reach top.
    let none = f.run(&["path", "leaf", "top"]);
    assert_eq!(none["data"].as_array().unwrap().len(), 0, "should be unreachable");

    // a symbol reaches itself in zero hops.
    let self_path = f.run(&["path", "top", "top"]);
    let rows = self_path["data"].as_array().unwrap();
    assert_eq!(rows.len(), 1);
    assert_eq!(rows[0]["symbol"], "top");
    assert_eq!(rows[0]["depth"], 0);
}

#[test]
fn path_respects_max_depth() {
    let f = Fixture::new();
    f.write(
        "a.rs",
        "fn d4() {}\nfn c3() { d4(); }\nfn b2() { c3(); }\nfn a1() { b2(); }\n",
    );
    // a1 -> b2 -> c3 -> d4 is 3 hops; a max-depth of 2 can't reach d4.
    let shallow = f.run(&["path", "a1", "d4", "--max-depth", "2"]);
    assert_eq!(shallow["data"].as_array().unwrap().len(), 0, "3-hop chain found at depth 2");
    let deep = f.run(&["path", "a1", "d4", "--max-depth", "3"]);
    let steps: Vec<&str> = deep["data"].as_array().unwrap().iter()
        .map(|r| r["symbol"].as_str().unwrap()).collect();
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
    assert!(!d1.contains(&"leaf".to_string()), "leaf is two hops down: {d1:?}");

    let d2 = f.run(&["callees", "top", "--depth", "2"]);
    let rows = d2["data"].as_array().unwrap();
    let names: Vec<&str> = rows.iter().map(|r| r["name"].as_str().unwrap()).collect();
    assert!(names.contains(&"mid") && names.contains(&"leaf"), "{names:?}");
    let depth_of = |n: &str| rows.iter()
        .find(|r| r["name"] == n).unwrap()["depth"].as_u64().unwrap();
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
    assert_eq!(all["data"]["definitions"].as_array().unwrap().len(), 2, "unscoped sees both");

    let scoped = f.run(&["about", "thing", "--path", "a.rs"]);
    let d = &scoped["data"];
    assert_eq!(d["definitions"].as_array().unwrap().len(), 1);
    let callees: Vec<&str> = d["callees"].as_array().unwrap().iter()
        .map(|c| c["name"].as_str().unwrap()).collect();
    assert!(callees.contains(&"helper_a"), "{callees:?}");
    assert!(!callees.contains(&"helper_b"), "scoped callees leaked the other def: {callees:?}");
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
    assert_eq!(rows[0]["kind"], "function");
}

#[test]
fn at_rejects_bad_location() {
    let f = Fixture::new();
    f.write("m.py", "def x():\n    pass\n");
    assert!(!f.raw(&["at", "m.py"]).status.success(), "missing :line");
    assert!(!f.raw(&["at", "m.py:abc"]).status.success(), "non-numeric line");
}

#[test]
fn changed_symbols_reports_only_touched_definitions() {
    let f = Fixture::new();
    f.write("a.rs", "fn keep() {\n    1;\n}\n\nfn change() {\n    1;\n}\n");
    f.git_init_commit();
    // Edit only change()'s body.
    f.write("a.rs", "fn keep() {\n    1;\n}\n\nfn change() {\n    2;\n}\n");

    let v = f.run(&["changed", "--symbols", "--compare-to", "HEAD"]);
    let names = field_set(&v, "name");
    assert!(names.contains(&"change".to_string()), "{names:?}");
    assert!(!names.contains(&"keep".to_string()), "untouched fn leaked: {names:?}");
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
    assert!(row.get("symbol").is_none(), "comment is inside no definition");
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
        f.run(&["search", "zzunique"])["data"]["matches"].as_array().unwrap().len(),
        1
    );
    f.remove("a.rs");
    assert_eq!(
        f.run(&["search", "zzunique"])["data"]["matches"].as_array().unwrap().len(),
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
    let text = String::from_utf8_lossy(&f.raw(&["search", "target", "-C", "1"]).stdout).into_owned();
    assert!(text.contains("a.rs-1- fn a() {}"), "before-context line missing: {text}");
    assert!(text.contains("a.rs:2:") && text.contains("let target = 1;"), "match line missing: {text}");
    assert!(text.contains("a.rs-3- fn b() {}"), "after-context line missing: {text}");
}

#[test]
fn search_files_with_matches() {
    let f = Fixture::new();
    f.write("a.rs", "fn needle() {}\n");
    f.write("b.rs", "fn other() {}\n");
    let v = f.run(&["search", "needle", "-l"]);
    let files = field_names(&v["data"]["files"]);
    assert_eq!(files, vec!["a.rs".to_string()]);
    assert!(v["data"].get("matches").is_none(), "-l returns paths, not matches");
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
    assert_eq!(code[0]["col"].as_u64().unwrap(), any_col, "unfiltered col == first (code) hit");

    let com = matches(&f.run(&["search", "value", "--in", "comments"]));
    assert_eq!(com.len(), 1);
    assert_eq!(com[0]["line"], 1);
    assert!(com[0]["col"].as_u64().unwrap() > any_col, "comment hit is later on the line");
}

#[test]
fn search_in_excludes_unparseable_languages() {
    let f = Fixture::new();
    f.write("notes.md", "needle in markdown\n"); // markdown: no parser
    // Without --in it's found; with --in we can't classify, so it's excluded
    // (never guessed) rather than returned as code.
    assert_eq!(
        f.run(&["search", "needle", "--path", "notes.md"])["data"]["matches"]
            .as_array().unwrap().len(),
        1
    );
    assert_eq!(
        f.run(&["search", "needle", "--path", "notes.md", "--in", "code"])["data"]["matches"]
            .as_array().unwrap().len(),
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
        f.run(&["search", pat])["data"]["matches"].as_array().unwrap().len()
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
    let count = |pat: &str| f.run(&["search", pat])["data"]["matches"].as_array().unwrap().len();
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
    assert_eq!(files, vec!["src/a.rs".to_string(), "tests/c.rs".to_string()]);

    // Exclude a subtree.
    let no_tests = f.run(&["search", "hit", "-g", "!tests/", "-l"]);
    let files = field_names(&no_tests["data"]["files"]);
    assert!(!files.iter().any(|p| p.starts_with("tests/")), "{files:?}");
    assert!(files.contains(&"src/a.rs".to_string()));
}

#[test]
fn search_word_matches_whole_words_only() {
    let f = Fixture::new();
    f.write("a.rs", "let count = 1;\nlet counter = 2;\n");
    // Plain matches both lines (substring); -w only the exact word.
    assert_eq!(
        f.run(&["search", "count"])["data"]["matches"].as_array().unwrap().len(),
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
    assert_eq!(em.len(), 2, "x;$ should match `mod x;` and `use crate::x;`: {em:?}");
}

#[test]
fn search_strips_leading_utf8_bom() {
    // A UTF-8 BOM before the first char must not hide line 1 from `^` anchors or
    // shift offsets — the index strips it, like ripgrep and editors do.
    let f = Fixture::new();
    f.write_bytes("a.cs", b"\xEF\xBB\xBF#region Foo\nint x = 1;\n#endregion\n");

    let hits = f.run(&["search", "^#"]);
    let m = hits["data"]["matches"].as_array().unwrap();
    assert_eq!(m.len(), 2, "both #region (line 1) and #endregion must match: {m:?}");
    assert_eq!(m[0]["line"], 1, "line-1 #region must be found despite the BOM");
    // The BOM must not leak into the matched line text either.
    assert!(m[0]["text"].as_str().unwrap().starts_with("#region"),
            "BOM leaked into text: {:?}", m[0]["text"]);
}

#[test]
fn text_is_the_default_output_json_is_opt_in() {
    let f = Fixture::new();
    f.write("a.rs", "fn alpha() {}\nfn beta() {}\n");

    // Default: compact grep-style text, no JSON envelope.
    let out = f.raw(&["symbols"]);
    assert!(out.status.success());
    let text = String::from_utf8_lossy(&out.stdout);
    assert!(!text.contains("\"schema\""), "default output must not be JSON: {text}");
    assert!(text.contains("a.rs:1: function alpha"), "grep-style row missing: {text}");
    assert!(text.contains("a.rs:2: function beta"), "grep-style row missing: {text}");

    // Search text is `path:line:col: text`, like ripgrep, plus codeq's
    // differentiator: the enclosing symbol annotated as `[name]`.
    let so = f.raw(&["search", "beta"]);
    let s = String::from_utf8_lossy(&so.stdout);
    assert!(s.contains("a.rs:2:"), "search text row missing: {s}");
    assert!(s.contains("fn beta() {}"), "search text payload missing: {s}");
    assert!(s.trim_end().ends_with("[beta]"), "enclosing symbol annotation missing: {s}");

    // --json restores the structured envelope.
    let jo = f.raw(&["--json", "symbols"]);
    let j = String::from_utf8_lossy(&jo.stdout);
    assert!(j.contains("\"schema\""), "--json must emit the envelope: {j}");
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
    assert!(sym.contains("2  fn big()"), "line-numbered from the def start: {sym}");
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

    // The tool result carries codeq's JSON output.
    let text = msgs[2]["result"]["content"][0]["text"].as_str().unwrap();
    let data: Value = serde_json::from_str(text).unwrap();
    assert_eq!(data["data"][0]["name"], "target");
    assert_eq!(data["data"][0]["kind"], "function");
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
    f.write("a.rs", "fn target() {}\nfn caller() { target(); }\n");

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

    let call = msgs.iter().find(|m| m["id"] == 2).expect("tool result for id 2");
    assert!(call["error"].is_null(), "unexpected JSON-RPC error: {call:?}");
    assert_ne!(call["result"]["isError"].as_bool(), Some(true), "tool errored: {call:?}");
    let text = call["result"]["content"][0]["text"].as_str().unwrap();
    let data: Value = serde_json::from_str(text).unwrap();
    let chunks = data["data"]["chunks"].as_array().expect("chunks array");
    // --path a.rs kept the seed; --no-text dropped its source text.
    assert!(chunks.iter().any(|c| c["symbol"] == "target"), "seed missing: {chunks:?}");
    assert!(chunks.iter().all(|c| c.get("text").is_none()),
            "no_text was not forwarded — source text leaked: {chunks:?}");
}

// ---------------------------------------------------------------------------
// Map (factual aggregation, no opinion)
// ---------------------------------------------------------------------------

#[test]
fn map_is_factual_and_reproducible() {
    let f = Fixture::new();
    f.write("src/a.rs", "use crate::db::Store;\nfn one() {}\nfn two() {}\n");
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
    assert_eq!(df[0]["path"], "src/a.rs");
    assert_eq!(df[0]["definitions"], 2);

    // Same inputs, identical output — no hidden state, no ranking drift.
    let v2 = f.run(&["map"]);
    assert_eq!(v, v2, "map must be byte-for-byte reproducible");

    // Default (text) output must render the per-row metric counts, not drop them
    // into empty payloads (the generic renderer's key=value fallback).
    let text = String::from_utf8_lossy(&f.raw(&["map"]).stdout).into_owned();
    assert!(text.contains("src/a.rs: ") && text.contains("definitions=2"),
            "map text output lost its counts: {text}");
    assert!(text.contains("importers=2"), "import counts missing from map text: {text}");
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
    assert_eq!(c["symbol"], "solo");
    assert_eq!(c["reason"], "seed");
    assert_eq!(c["start_line"], 1);
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
    let seed = chunks.iter().find(|c| c["symbol"] == "caller").unwrap();
    let neigh = chunks.iter().find(|c| c["symbol"] == "helper").unwrap();
    assert_eq!(seed["reason"], "seed");
    assert_eq!(neigh["reason"], "referenced");
    // --no-text means provenance only.
    assert!(seed.get("text").is_none());
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
    assert!(d.get("omitted").map_or(true, |o| o.as_array().unwrap().is_empty()));

    // Starve the budget: the explicitly-requested seeds are still returned — you
    // asked for them, and dropping the very symbol you named is worse than
    // overshooting. The budget caps automatic --expand growth, not the seeds.
    let v = f.run(&[
        "context", "--symbols", "aa,bb", "--no-text", "--max-tokens", "1",
    ]);
    let names = chunk_symbols(&v["data"]);
    assert!(names.contains(&"aa".to_string()) && names.contains(&"bb".to_string()),
            "seeds dropped by a starved budget: {names:?}");
}

#[test]
fn context_changed_mode_seeds_from_git() {
    let f = Fixture::new();
    f.write("a.py", "def old():\n    pass\n");
    f.git_init_commit();
    f.write("a.py", "def old():\n    pass\n\ndef fresh():\n    pass\n");

    // --plan opts into the explanatory plan (omitted by default to stay lean).
    let v = f.run(&["context", "--changed", "--compare-to", "HEAD", "--no-text", "--plan"]);
    let d = &v["data"];
    assert!(d["plan"]["modes"]
        .as_array()
        .unwrap()
        .iter()
        .any(|m| m == "changed"));
    let names = chunk_symbols(d);
    assert!(names.contains(&"fresh".to_string()), "changed seed missing: {names:?}");
    // Seed granularity is the touched *hunk*, not the changed *file*: `old()` is
    // in the same file but untouched, so it must not be seeded.
    assert!(!names.contains(&"old".to_string()), "untouched def leaked as seed: {names:?}");

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
    assert!(names.contains(&"renamed".to_string()), "edit not picked up: {names:?}");
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
    assert!(names.contains(&"second".to_string()), "untracked edit missed: {names:?}");
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
    assert!(names.contains(&"alpha".to_string()) && names.contains(&"beta".to_string()),
            "rebuild left an empty index: {names:?}");
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
    assert!(!names.contains(&"generated".to_string()), "gitignored file still indexed: {names:?}");
    assert!(names.contains(&"keep".to_string()));

    f.write(".gitignore", "\n"); // un-ignore
    assert!(field_set(&f.run(&["symbols"]), "name").contains(&"generated".to_string()),
            "un-ignored file not re-indexed");
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
    assert!(names.contains(&"added".to_string()), "added file not indexed: {names:?}");
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
    assert!(!names.contains(&"vanishing".to_string()), "deleted symbol still present: {names:?}");
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
    assert!(names.contains(&"deep_fn".to_string()), "file in new subdir missed: {names:?}");
}

#[test]
fn changed_without_git_fails_with_nonzero_exit() {
    let f = Fixture::new();
    f.write("a.py", "def x():\n    pass\n");
    let out = f.raw(&["changed", "--compare-to", "main"]);
    assert!(!out.status.success(), "expected non-zero exit");
    let stderr = String::from_utf8_lossy(&out.stderr);
    assert!(stderr.contains("Git"), "stderr should mention Git: {stderr}");
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
    let dl = d0["start_line"].as_u64().unwrap();
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
    let (s, e) = (d0["start_line"].as_u64().unwrap(), d0["end_line"].as_u64().unwrap());

    let at_def = f.run(&["at", &format!("{p}:{s}")]);
    assert_eq!(at_def["data"][0]["name"], "outer", "at the def line must resolve to it");

    let mid = (s + e) / 2;
    let d = f.run(&["at", &format!("{p}:{mid}")])["data"][0].clone();
    assert_eq!(d["name"], "outer");
    assert!(
        d["start_line"].as_u64().unwrap() <= mid && mid <= d["end_line"].as_u64().unwrap(),
        "at a body line must be enclosed by the def"
    );
}

#[test]
fn outline_names_are_a_subset_of_symbols() {
    let f = Fixture::new();
    f.write("a.rs", "struct S;\nfn f() {}\nimpl S { fn m(&self) {} }\n");
    let symbols: std::collections::HashSet<String> =
        field_set(&f.run(&["symbols", "--path", "a.rs", "--limit", "0"]), "name")
            .into_iter()
            .collect();
    for n in field_set(&f.run(&["outline", "a.rs"]), "name") {
        assert!(symbols.contains(&n), "outline name {n} absent from symbols {symbols:?}");
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
    assert!(text.contains("fn b") && text.contains("fn c"), "range content missing: {text}");
    assert!(!text.contains("fn a"), "range must start at line 2: {text}");
}

#[test]
fn at_beyond_eof_resolves_to_nothing() {
    let f = Fixture::new();
    f.write("a.rs", "fn a() {}\n");
    let v = f.run(&["at", "a.rs:9999"]);
    assert!(v["data"].as_array().unwrap().is_empty(), "line past EOF should enclose nothing");
}

#[test]
fn fields_ignores_unknown_columns() {
    let f = Fixture::new();
    f.write("a.rs", "fn f() {}\n");
    let row = f.run(&["symbols", "--fields", "name,not_a_real_field"])["data"][0].clone();
    let keys: Vec<&str> = row.as_object().unwrap().keys().map(|s| s.as_str()).collect();
    assert_eq!(keys, ["name"], "only the known requested field survives: {keys:?}");
}

#[test]
fn rust_impl_method_extracted_with_signature() {
    let f = Fixture::new();
    f.write("a.rs", "struct S;\nimpl S {\n    pub fn method(&self, x: i32) -> i32 { x }\n}\n");
    assert!(field_set(&f.run(&["symbols", "--path", "a.rs"]), "name").contains(&"method".to_string()));
    let sig = f.run(&["definitions", "method"])["data"][0]["signature"]
        .as_str()
        .unwrap()
        .to_string();
    assert!(sig.contains("fn method") && sig.contains("i32"), "signature: {sig}");
}

#[test]
fn python_class_method_nests_in_outline() {
    let f = Fixture::new();
    f.write("a.py", "class Widget:\n    def method(self):\n        pass\n");
    let v = f.run(&["outline", "a.py"]);
    let rows = v["data"].as_array().unwrap();
    let cls = rows.iter().find(|r| r["name"] == "Widget").unwrap();
    let meth = rows.iter().find(|r| r["name"] == "method").unwrap();
    assert_eq!(cls["depth"], 0);
    assert!(meth["depth"].as_u64().unwrap() >= 1, "method must nest under its class");
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
    assert!(mods.contains(&"collections.OrderedDict".to_string()), "{mods:?}");
    assert!(mods.contains(&"collections.defaultdict".to_string()), "{mods:?}");
}

#[test]
fn search_composes_word_language_and_path_filters() {
    let f = Fixture::new();
    f.write("a.rs", "fn value() {}\nlet value = 1;\n");
    f.write("b.py", "value = 2\n");
    let v = f.run(&["search", "value", "-w", "--language", "rust", "--path", "a.rs", "--limit", "0"]);
    let matches = v["data"]["matches"].as_array().unwrap();
    assert!(!matches.is_empty(), "expected matches in a.rs");
    assert!(matches.iter().all(|m| m["path"] == "a.rs"), "path/language filter leaked: {matches:?}");
}

#[test]
fn context_changed_mode_can_expand_to_referenced_defs() {
    let f = Fixture::new();
    f.write("a.py", "def helper():\n    pass\n");
    f.git_init_commit();
    f.write("a.py", "def helper():\n    pass\n\ndef changed():\n    helper()\n");
    let names = chunk_symbols(
        &f.run(&["context", "--changed", "--compare-to", "HEAD", "--expand", "--no-text"])["data"],
    );
    assert!(names.contains(&"changed".to_string()), "touched seed missing: {names:?}");
    assert!(names.contains(&"helper".to_string()), "expansion to referenced def missing: {names:?}");
}

#[test]
fn map_totals_agree_with_stats_and_symbols() {
    // The aggregate counts must reconcile: map.totals == stats == actual symbols.
    let f = Fixture::new();
    f.write("a.rs", "fn one() {}\nfn two() {}\n");
    f.write("b.py", "def three():\n    pass\n");
    f.run(&["index"]);
    let files_stats = f.run(&["stats"])["data"]["files"].as_u64().unwrap();
    let map = f.run(&["map"]);
    assert_eq!(files_stats, 2);
    assert_eq!(map["data"]["totals"]["files"].as_u64().unwrap(), files_stats,
               "map totals.files must match stats.files");
    let nsym = f.run(&["symbols", "--limit", "0"])["data"].as_array().unwrap().len() as u64;
    assert_eq!(nsym, 3);
    assert_eq!(map["data"]["totals"]["symbols"].as_u64().unwrap(), nsym,
               "map totals.symbols must match the actual symbol count");
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
        assert!(names.iter().any(|x| x == n), "go symbol {n} missing: {names:?}");
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
        assert!(names.iter().any(|x| x == n), "ts symbol {n} missing: {names:?}");
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
    assert_eq!(ci["data"]["matches"].as_array().unwrap().len(), 3, "-i must match all three cases");
    let cs = f.run(&["search", "value", "--limit", "0"]);
    assert_eq!(cs["data"]["matches"].as_array().unwrap().len(), 1, "case-sensitive matches only 'value'");
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
    assert_eq!(lines(&f.run(&["search", "[0-9]+", "--limit", "0"])), vec![1], "char class + quantifier");
    assert_eq!(lines(&f.run(&["search", "foo|bar", "--limit", "0"])), vec![2, 3], "alternation");
    assert_eq!(lines(&f.run(&["search", "names?", "--limit", "0"])), vec![2], "optional quantifier");
}

#[test]
fn read_symbol_returns_only_that_definition() {
    let f = Fixture::new();
    f.write("a.rs", "fn first() {\n    let a = 1;\n}\nfn second() {\n    let b = 2;\n}\n");
    f.run(&["index"]);
    let out = String::from_utf8_lossy(&f.raw(&["read", "a.rs", "--symbol", "second"]).stdout).into_owned();
    assert!(out.contains("fn second") && out.contains("let b = 2"), "second not returned: {out}");
    assert!(!out.contains("fn first") && !out.contains("let a = 1"), "sibling leaked into --symbol read: {out}");
}

#[test]
fn read_raw_emits_exact_bytes_ready_for_edit() {
    let f = Fixture::new();
    f.write("a.rs", "fn other() {}\nfn helper() {\n    let x = 1;\n    x\n}\n");

    // Default is line-numbered; --raw is the exact source, no prefixes.
    let numbered = String::from_utf8(f.raw(&["read", "a.rs", "--symbol", "helper"]).stdout).unwrap();
    assert!(numbered.contains("  2  fn helper"), "default should be line-numbered: {numbered:?}");

    let raw = String::from_utf8(f.raw(&["read", "a.rs", "--symbol", "helper", "--raw"]).stdout).unwrap();
    // Byte-exact: the source of `helper` verbatim, and it drops into Edit's
    // old_string as-is (must be a substring of the file's own text).
    assert_eq!(raw, "fn helper() {\n    let x = 1;\n    x\n}\n");
    let file = std::fs::read_to_string(f.dir.path().join("a.rs")).unwrap();
    assert!(file.contains(raw.trim_end()), "raw output must be a verbatim slice of the file");
}

#[test]
fn read_raw_symbol_refuses_ambiguous_match() {
    let f = Fixture::new();
    f.write("a.rs", "fn dup() {}\nmod m { fn dup() {} }\n");
    let r = f.raw(&["read", "a.rs", "--symbol", "dup", "--raw"]);
    assert!(!r.status.success(), "ambiguous --raw should fail");
    let err = String::from_utf8_lossy(&r.stderr);
    assert!(err.contains("needs one"), "should explain the ambiguity: {err}");
}

#[test]
fn callees_report_the_functions_a_symbol_calls() {
    let f = Fixture::new();
    f.write(
        "a.rs",
        "fn caller() {\n    helper();\n    other();\n}\nfn helper() {}\nfn other() {}\nfn unrelated() {}\n",
    );
    let names = field_set(&f.run(&["callees", "caller"]), "name");
    assert!(names.contains(&"helper".to_string()), "helper missing: {names:?}");
    assert!(names.contains(&"other".to_string()), "other missing: {names:?}");
    assert!(!names.contains(&"unrelated".to_string()), "uncalled sibling listed as callee: {names:?}");
}

#[test]
fn path_scopes_definitions_to_one_file() {
    // --path disambiguates a common name across files (the first dogfood fix).
    let f = Fixture::new();
    f.write("a.rs", "fn build() {}\n");
    f.write("b.rs", "fn build() {}\n");
    assert_eq!(f.run(&["definitions", "build"])["data"].as_array().unwrap().len(), 2, "both defs unscoped");
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
    assert!(mods.iter().any(|m| m == "fs"), "ES import missing: {mods:?}");
    assert!(mods.iter().any(|m| m == "path"), "require() import missing: {mods:?}");
    assert!(!mods.iter().any(|m| m == "hi" || m == "console"), "a non-require call leaked: {mods:?}");
}

#[test]
fn tests_linkage_finds_tests_in_test_files() {
    let f = Fixture::new();
    f.write("src/lib.rs", "pub fn widget() -> i32 { 1 }\npub fn other() {}\n");
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
    assert!(names.contains(&"test_widget_works".to_string()), "{names:?}");
    assert!(!names.contains(&"test_other_only".to_string()), "doesn't reference widget: {names:?}");
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
    assert!(!names.contains(&"helper".to_string()), "helper is not a test: {names:?}");
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
fn astgrep_metavariable_binds_consistently() {
    let f = Fixture::new();
    f.write("a.rs", "fn f(a: i32, b: i32) -> bool {\n    let _ = a == a;\n    a == b\n}\n");
    let m = f.run(&["astgrep", "$X == $X", "--language", "rust"]);
    let rows = m["data"]["matches"].as_array().unwrap();
    assert_eq!(rows.len(), 1, "only a==a matches a consistent metavar");
    assert_eq!(rows[0]["text"], "a == a");
    assert_eq!(rows[0]["binds"]["X"], "a");
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
    assert_eq!(m.len(), 1, "must stay anchored to `return`, not match `let _x = Ok(2)`");
    assert_eq!(m[0]["text"], "return Ok(1)");
}

#[test]
fn astgrep_works_across_languages() {
    let f = Fixture::new();
    f.write("a.py", "def f():\n    print(x)\n    print(y)\n    log(z)\n");
    let rows = f.run(&["astgrep", "print($X)", "--language", "python"]);
    assert_eq!(rows["data"]["matches"].as_array().unwrap().len(), 2, "two print() calls");
}

#[test]
fn astgrep_variadic_matches_any_arguments() {
    // `$$$` absorbs a run of zero-or-more siblings in a list position.
    let f = Fixture::new();
    f.write("a.rs", "fn f() {\n    foo();\n    foo(1);\n    foo(1, 2, 3);\n    bar(9);\n}\n");
    let all = f.run(&["astgrep", "foo($$$)", "--language", "rust"]);
    assert_eq!(all["data"]["matches"].as_array().unwrap().len(), 3, "foo() with 0/1/3 args, not bar");
    // A fixed leading arg plus a variadic requires at least that arg.
    let some = f.run(&["astgrep", "foo($A, $$$)", "--language", "rust"]);
    let texts: Vec<String> = some["data"]["matches"]
        .as_array()
        .unwrap()
        .iter()
        .map(|r| r["text"].as_str().unwrap().to_string())
        .collect();
    assert_eq!(texts.len(), 2, "foo(1) and foo(1,2,3), not the no-arg call: {texts:?}");
    assert!(!texts.iter().any(|t| t == "foo()"), "{texts:?}");
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
    assert!(names.contains(&"helper".to_string()), "called function missing: {names:?}");
    assert!(!names.contains(&"field".to_string()), "a field access is not a callee: {names:?}");
    assert!(!names.contains(&"SomeType".to_string()), "a type reference is not a callee: {names:?}");
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
    for n in ["Widget", "declared_only", "compute", "inline_def", "free_prototype"] {
        assert!(names.iter().any(|x| x == n), "cpp symbol {n} missing: {names:?}");
    }
    // The inline definition is not double-counted.
    assert_eq!(names.iter().filter(|x| *x == "inline_def").count(), 1, "inline method duplicated");
    // A declaration-only method still resolves via `definitions`, with a signature.
    let sig = f.run(&["definitions", "declared_only"])["data"][0]["signature"]
        .as_str()
        .unwrap()
        .to_string();
    assert!(sig.contains("declared_only"), "declaration signature missing: {sig}");
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
    assert!(mods.iter().any(|m| m == "App\\Thing"), "use statement missing: {mods:?}");
    assert!(mods.iter().any(|m| m == "bootstrap.php"), "single-quoted require missing: {mods:?}");
    assert!(mods.iter().any(|m| m == "vendor/autoload.php"), "double-quoted include_once missing: {mods:?}");
    assert!(!mods.iter().any(|m| m == "hi"), "an echoed string leaked as an import: {mods:?}");
}

#[test]
fn go_import_block_captures_each_module() {
    let f = Fixture::new();
    f.write("a.go", "package main\nimport (\n\t\"fmt\"\n\t\"os\"\n)\nfunc F() { fmt.Println(os.Args) }\n");
    let mods = field_set(&f.run(&["imports", "a.go"]), "module");
    assert!(mods.iter().any(|m| m == "fmt"), "fmt missing from import block: {mods:?}");
    assert!(mods.iter().any(|m| m == "os"), "os missing from import block: {mods:?}");
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
    assert_eq!(targets, ["b.py"], "only the internal import resolves: {targets:?}");
    assert_eq!(rows[0]["module"], "b", "edge carries the raw import string");
    assert_eq!(rows[0]["depth"], 1);
}

#[test]
fn deps_depth_walks_the_import_chain() {
    let f = Fixture::new();
    f.write("a.py", "import b\n"); // a -> b -> c
    f.write("b.py", "import c\n");
    f.write("c.py", "x = 1\n");

    // depth 1 stops at the direct import.
    let d1 = field_set(&f.run(&["deps", "a.py"]), "path");
    assert!(d1.contains(&"b.py".to_string()) && !d1.contains(&"c.py".to_string()), "{d1:?}");

    // depth 2 reaches c through b, tagged with its hop distance.
    let d2 = f.run(&["deps", "a.py", "--depth", "2"]);
    let rows = d2["data"].as_array().unwrap();
    let depth_of = |p: &str| rows.iter()
        .find(|r| r["path"] == p).unwrap()["depth"].as_u64().unwrap();
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
    let cyc: Vec<&str> = rows[0]["cycle"].as_array().unwrap()
        .iter().map(|v| v.as_str().unwrap()).collect();
    assert_eq!(cyc, ["x.py", "y.py"], "cycle canonicalized to start at least member: {cyc:?}");
    assert_eq!(rows[0]["length"], 2);
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
    assert!(targets.contains(&"pkg/util.py".to_string()), "aligned file missing: {targets:?}");
    assert!(!targets.contains(&"other/util.py".to_string()),
            "unaligned same-stem file leaked: {targets:?}");
}

#[test]
fn stats_reports_per_language_file_counts() {
    let f = Fixture::new();
    f.write("a.rs", "fn a() {}\n");
    f.write("b.rs", "fn b() {}\n");
    f.write("c.py", "def c():\n    pass\n");
    f.run(&["index"]);
    let bylang = &f.run(&["stats"])["data"]["by_language"];
    assert_eq!(bylang["rust"], 2, "rust file count wrong: {bylang}");
    assert_eq!(bylang["python"], 1, "python file count wrong: {bylang}");
}

#[test]
fn context_accepts_multiple_seed_symbols() {
    let f = Fixture::new();
    f.write("a.rs", "fn aa() {}\nfn bb() {}\nfn cc() {}\n");
    let names = chunk_symbols(&f.run(&["context", "--symbols", "aa,bb", "--no-text"])["data"]);
    assert!(names.contains(&"aa".to_string()) && names.contains(&"bb".to_string()),
            "both seeds should be returned: {names:?}");
    assert!(!names.contains(&"cc".to_string()), "unrequested symbol leaked: {names:?}");
}

// ---------------------------------------------------------------------------
// Piped input (--stdin)
// ---------------------------------------------------------------------------

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
    let out = piped(f.path(), &["search", "indexed_only"], "nothing to see here\n");
    let text = String::from_utf8(out.stdout).unwrap();
    assert!(out.status.success(), "search must not fail on a stray pipe");
    assert!(text.contains("a.rs:1"), "must read the index, not stdin: {text}");
    assert!(!text.contains("(stdin)"), "must not label rows stdin: {text}");
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
    assert!(text.starts_with("(stdin):2:"), "located in the buffer: {text}");
    assert!(!text.contains("from_the_index"), "index not consulted: {text}");
}

/// What piped input buys over grep: with --language the buffer is parsed, so
/// matches carry their enclosing symbol and --in can separate code from prose.
#[test]
fn piped_input_with_language_annotates_and_filters() {
    let f = Fixture::new();
    let src = "// target lives in a comment\nfn holder() {\n    let s = \"target in a string\";\n    target();\n}\n";

    let annotated = String::from_utf8(
        piped(f.path(), &["search", "target", "--stdin", "--language", "rust"], src).stdout,
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
                &["search", "target", "--stdin", "--language", "rust", "--in", filter],
                src,
            )
            .stdout,
        )
        .unwrap();
        assert!(out.contains(want), "--in {filter} keeps {want}: {out}");
        assert!(!out.contains(unwanted), "--in {filter} drops {unwanted}: {out}");
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
    assert!(out.status.success(), "{}", String::from_utf8_lossy(&out.stderr));
    assert!(String::from_utf8(out.stdout).unwrap().contains("(stdin):1:"));
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

    let ctx = String::from_utf8(
        piped(f.path(), &["search", "hit", "--stdin", "-C", "1"], input).stdout,
    )
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
    assert!(!has(&scoped, "zzz"), "--path scopes to one owner: {scoped:?}");
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
        field_set(&f.run(&["members", "C", "--kind", "function"]), "name"),
        ["m"]
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
        ("a.go", "type S struct { A int; b string }", "S", &["A", "b"]),
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
        ("a.kt", "class C(val a: Int) { var b: String = \"\" }", "C", &["a", "b"]),
        ("a.swift", "struct S { var a: Int\nlet b: String }", "S", &["a", "b"]),
        ("a.scala", "case class C(a: Int) { val c: Int = 1 }", "C", &["a", "c"]),
        ("a.dart", "class C { int a = 1; }", "C", &["a"]),
        ("a.zig", "const P = struct { x: i32, y: i32 };", "P", &["x", "y"]),
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
fn variants_extracted_across_languages() {
    let cases: &[(&str, &str, &str, &[&str])] = &[
        ("a.rs", "enum E { A, B }", "E", &["A", "B"]),
        ("a.java", "enum E { X, Y }", "E", &["X", "Y"]),
        ("a.cs", "enum E { X }", "E", &["X"]),
        ("a.c", "enum E { X, Y };", "E", &["X", "Y"]),
        ("a.php", "<?php enum E { case X; }", "E", &["X"]),
        ("a.ts", "enum E { X = 1 }", "E", &["X"]),
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
    let stats = f.run(&["stats"]);
    assert!(
        stats["data"]["files"].as_i64().unwrap() > 0,
        "index must not come back silently empty: {stats}"
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
        out.lines().any(|l| l.contains("other") && !l.contains("//")),
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
    f.write("a.rs", "pub struct ScanOpts { pub limit: usize }\npub fn lonely() {}\n");

    // 1. The name does not exist, and the near-miss is offered.
    let note = note_of(&f, &["definitions", "ScanOpt"]);
    assert!(note.contains("no symbol named 'ScanOpt'"), "{note}");
    assert!(note.contains("closest: ScanOpts"), "suggests the real name: {note}");

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
        v["note"].as_str().unwrap().contains("no symbol named 'nope_xyz'"),
        "{v}"
    );
    assert!(v["data"].as_array().unwrap().is_empty(), "data is still empty");

    // A non-empty result carries no note at all.
    let v = f.run(&["definitions", "lonely"]);
    assert!(v.get("note").is_none(), "no note when there are results: {v}");
}

/// `api` returning nothing almost always means "nothing here is public", which
/// is very different from "wrong path".
#[test]
fn api_explains_an_empty_surface() {
    let f = Fixture::new();
    f.write("a.rs", "pub(crate) fn restricted() {}\nfn private_fn() {}\n");

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
    assert_eq!(names, ["Title", "Install", "Details", "Usage"], "in source order");

    // Nesting comes from byte containment, as it does for every other language.
    let depths: Vec<i64> = outline["data"]
        .as_array()
        .unwrap()
        .iter()
        .map(|r| r["depth"].as_i64().unwrap())
        .collect();
    assert_eq!(depths, [0, 1, 2, 1], "### nests under ##, which nests under #");

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
    let section = String::from_utf8(f.raw(&["read", "doc.md", "--symbol", "Install"]).stdout).unwrap();
    assert!(section.contains("## Install") && section.contains("Run it."), "{section}");
    assert!(section.contains("### Details"), "a section includes its subsections");
    assert!(!section.contains("## Usage"), "and stops at the next peer: {section}");

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
        f.raw(&["search", "codeq", "--path", "doc.md", "--in", "strings"]).stdout,
    )
    .unwrap();
    assert!(in_code.contains("codeq index"), "example found: {in_code}");

    let in_prose = String::from_utf8(
        f.raw(&["search", "prose", "--path", "doc.md", "--in", "comments"]).stdout,
    )
    .unwrap();
    assert!(in_prose.contains("Intro prose"), "prose found: {in_prose}");
    assert!(!in_prose.contains("codeq index"), "not the example: {in_prose}");

    // Search hits carry the section they sit in, like code hits carry a function.
    let hit = String::from_utf8(f.raw(&["search", "More prose", "--path", "doc.md"]).stdout).unwrap();
    assert!(hit.contains("[Details]"), "annotated with its section: {hit}");
}

/// A link or image with a local target is a file reference, so the dependency
/// commands cover documentation and its assets.
#[test]
fn markdown_local_links_and_images_are_file_references() {
    let f = Fixture::new();
    f.write("doc.md", DOC);
    f.write("guide.md", "# Guide\n");

    let imports = field_set(&f.run(&["imports", "doc.md"]), "module");
    assert!(imports.iter().any(|i| i == "./guide.md"), "link: {imports:?}");
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
    assert!(kinds.iter().any(|k| k == "function"), "real dead code still found");
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
    assert!(names.iter().any(|n| n == "Old Style"), "setext h1: {names:?}");
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
        lines, [4],
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

/// The same call written across the grammars codeq knows. Each is checked
/// against the real tree, not against documentation.
#[test]
fn call_sites_resolve_across_languages() {
    let cases: &[(&str, &str, &str)] = &[
        ("a.py", "def query(x): pass\ndb.query()\nother.query()\n", "db"),
        ("a.js", "function query(x) {}\ndb.query();\nother.query();\n", "db"),
        ("a.ts", "function query(x: number) {}\ndb.query();\n", "db"),
        ("a.go", "package m\nfunc f() {\n\tdb.query()\n}\n", "db"),
        ("A.java", "class T {\n  void query() {}\n  void f() {\n    db.query();\n  }\n}\n", "db"),
        ("a.rb", "def query(x); end\ndb.query\n", "db"),
        ("a.c", "void query(int x) {}\nvoid f() {\n  db->query();\n}\n", "db"),
        ("a.cs", "class T {\n  void query() {}\n  void f() {\n    db.query();\n  }\n}\n", "db"),
        ("a.php", "<?php\nfunction query($x) {}\n$db->query();\n", "$db"),
        ("a.kt", "fun query() {}\nfun f() {\n  db.query()\n}\n", "db"),
        ("a.swift", "func query() {}\nfunc f() {\n  db.query()\n}\n", "db"),
        ("a.scala", "object M {\n  def query() = {}\n  def f() = {\n    db.query()\n  }\n}\n", "db"),
        ("a.dart", "void query() {}\nvoid f() {\n  db.query();\n}\n", "db"),
        ("a.zig", "fn query() void {}\nfn f() void {\n    db.query();\n}\n", "db"),
        ("a.ex", "defmodule M do\n  def f do\n    db.query()\n  end\nend\n", "db"),
        ("a.sol", "contract C {\n  function f() public {\n    db.query();\n  }\n}\n", "db"),
        ("a.lua", "local function query() end\nlocal function f()\n  db.query()\nend\n", "db"),
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
        !modules.iter().any(|m| m == "./other.md" || m == "assets/real.png"),
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
    f.write("src/app.js", "import { chunk } from \"lodash\";\nchunk([1, 2], 1);\n");

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
        f.raw(&["--root", "node_modules/lodash", "read", "chunk.js", "--symbol", "chunk"])
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
    assert_eq!(get("ruby")["members"], true, "attr_accessor, read via a guard");
    assert_eq!(get("bash")["imports"], true, "source ./lib.sh");
    assert_eq!(get("zig")["relations"], false, "Zig has no inheritance");
    assert_eq!(get("elixir")["members"], false, "no field declarations");
    assert_eq!(get("markdown")["definitions"], true, "headings");
    assert_eq!(get("markdown")["calls"], false);
    for lang in ["rust", "python", "go", "markdown"] {
        assert_eq!(get(lang)["spans"], true, "{lang} classifies comments/strings");
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
    assert!(names.iter().any(|n| n == "bare"), "uncovered public fn: {names:?}");
    assert!(!names.iter().any(|n| n == "covered"), "covered: {names:?}");
    assert!(
        !names.iter().any(|n| n == "private_helper"),
        "private definitions are out of scope: {names:?}"
    );
    assert!(
        !names.iter().any(|n| n == "works"),
        "the test itself is not production code needing cover: {names:?}"
    );
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
    assert_eq!(names("changed"), ["shape"], "same name, different signature");
    let changed = &d["data"]["changed"][0];
    assert!(
        changed["was"].as_str().unwrap().contains("a: u32)"),
        "the previous signature is shown: {changed}"
    );
    // An unexported change is not an API change, however much the diff shows.
    for k in ["removed", "added", "changed"] {
        assert!(!names(k).iter().any(|n| n == "internal"), "{k} leaked a private fn");
    }
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
        let subs = field_set(&f.run(&["implementors", "B"]), "subtype");
        assert_eq!(subs, ["A"], "{file}: A derives from B");
        let supers = field_set(&f.run(&["supertypes", "A"]), "supertype");
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
    f.write("a.ex", "defmodule M do\n  alias Foo.Bar\n  def run do\n    :ok\n  end\nend\n");

    let mods = field_set(&f.run(&["imports"]), "module");
    assert_eq!(mods, ["Foo.Bar"], "defmodule defines, it does not import: {mods:?}");
    let defs = field_set(&f.run(&["symbols"]), "name");
    assert!(defs.iter().any(|d| d == "M"), "the module is a definition: {defs:?}");
    assert!(defs.iter().any(|d| d == "run"));
}

/// Ruby declares fields through a method call, so `attr_accessor` needed the same
/// guard mechanism as `require` — and the leading `:` is symbol syntax, not part
/// of the field's name.
#[test]
fn ruby_attr_accessors_are_fields() {
    let f = Fixture::new();
    f.write("a.rb", "class C\n  attr_accessor :a, :b\n  attr_reader :c\n  def m; end\nend\n");

    let mut fields = field_set(&f.run(&["members", "C", "--kind", "field"]), "name");
    fields.sort();
    assert_eq!(fields, ["a", "b", "c"], "no leading colons");
    // Requires still work; the two share the call shape.
    f.write("b.rb", "require 'json'\n");
    assert!(field_set(&f.run(&["imports"]), "module").iter().any(|m| m == "json"));
}
