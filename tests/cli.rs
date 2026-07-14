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
    assert_eq!(v["index"]["schema"], 6);
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
    assert!(out.contains('⋮'), "body folded: {out}");
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
    for m in ["os", "sys", "collections", "json"] {
        assert!(mods.iter().any(|x| x == m), "module '{m}' not captured: {mods:?}");
    }
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
