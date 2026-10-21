use std::io::{BufRead, BufReader, Write};
use std::path::Path;
use std::process::{Child, ChildStdin, Command, Stdio};
use std::sync::mpsc::{self, Receiver};
use std::time::Duration;

use serde_json::{json, Value};

struct Server {
    child: Child,
    input: Option<ChildStdin>,
    replies: Receiver<Value>,
    next_id: u64,
}

impl Server {
    fn new(root: &Path) -> Self {
        Self::with_workers(root, 1)
    }

    fn with_workers(root: &Path, workers: u8) -> Self {
        Self::with_tools(root, workers, "all")
    }

    fn with_tools(root: &Path, workers: u8, profile: &str) -> Self {
        let mut child = Command::new(assert_cmd::cargo::cargo_bin!("codeq"))
            .args([
                "--root",
                root.to_str().unwrap(),
                "mcp",
                "--workers",
                &workers.to_string(),
                "--tools",
                profile,
            ])
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::inherit())
            .spawn()
            .unwrap();
        let input = child.stdin.take();
        let output = child.stdout.take().unwrap();
        let (send, replies) = mpsc::channel();
        std::thread::spawn(move || {
            for line in BufReader::new(output).lines() {
                let value = serde_json::from_str(&line.unwrap()).expect("JSON-RPC stdout");
                if send.send(value).is_err() {
                    break;
                }
            }
        });
        Self {
            child,
            input,
            replies,
            next_id: 0,
        }
    }

    fn send(&mut self, value: Value) {
        writeln!(self.input.as_mut().unwrap(), "{value}").unwrap();
    }

    fn receive(&self) -> Value {
        self.replies
            .recv_timeout(Duration::from_secs(30))
            .expect("MCP response")
    }

    fn call(&mut self, name: &str, arguments: Value) -> String {
        self.next_id += 1;
        writeln!(
            self.input.as_mut().unwrap(),
            "{}",
            json!({
                "jsonrpc": "2.0", "id": self.next_id, "method": "tools/call",
                "params": { "name": name, "arguments": arguments }
            })
        )
        .unwrap();
        let reply = self
            .replies
            .recv_timeout(Duration::from_secs(30))
            .expect("MCP response");
        assert_eq!(reply["id"], self.next_id, "{reply}");
        assert!(reply.get("error").is_none(), "{reply}");
        assert_ne!(reply["result"]["isError"], true, "{reply}");
        reply["result"]["content"][0]["text"]
            .as_str()
            .unwrap()
            .to_string()
    }
}

#[test]
fn core_profile_lists_navigation_tools_and_rejects_advanced_calls() {
    let dir = tempfile::tempdir().unwrap();
    std::fs::write(dir.path().join("sample.rs"), "fn target() {}\n").unwrap();
    let mut server = Server::with_tools(dir.path(), 1, "core");
    server.send(json!({"jsonrpc":"2.0","id":1,"method":"tools/list"}));
    let reply = server.receive();
    let mut names: Vec<&str> = reply["result"]["tools"]
        .as_array()
        .unwrap()
        .iter()
        .map(|tool| tool["name"].as_str().unwrap())
        .collect();
    names.sort();
    assert_eq!(
        names,
        [
            "codeq_api",
            "codeq_callees",
            "codeq_callers",
            "codeq_definitions",
            "codeq_deps",
            "codeq_explore",
            "codeq_map",
            "codeq_read",
            "codeq_search",
            "codeq_symbols",
            "codeq_tests",
            "codeq_tree"
        ]
    );
    server.next_id = 1;
    assert_eq!(
        server.call(
            "codeq_read",
            json!({"file":"sample.rs", "symbol":"target", "raw":true})
        ),
        "fn target() {}\n"
    );
    server.send(json!({"jsonrpc":"2.0","id":3,"method":"tools/call",
        "params":{"name":"codeq_unused","arguments":{}}}));
    let reply = server.receive();
    assert_eq!(reply["result"]["isError"], true);
    assert!(reply["result"]["content"][0]["text"]
        .as_str()
        .unwrap()
        .contains("--tools all"));
}

impl Drop for Server {
    fn drop(&mut self) {
        self.input.take();
        let _ = self.child.kill();
        let _ = self.child.wait();
    }
}

#[test]
fn refreshes_graph_tests_and_source_after_edits_and_external_refreshes() {
    let dir = tempfile::tempdir().unwrap();
    let root = dir.path();
    std::fs::write(
        root.join("lib.py"),
        "def helper():\n    return 'old_body'\n",
    )
    .unwrap();
    std::fs::write(
        root.join("app.py"),
        "from lib import helper\ndef run():\n    return helper()\n",
    )
    .unwrap();
    std::fs::write(
        root.join("test_app.py"),
        "from app import run\ndef test_run():\n    assert run()\n",
    )
    .unwrap();
    let mut server = Server::new(root);
    let text = server.call("codeq_explore", json!({"name": "run"}));
    assert!(
        text.contains("old_body") && text.contains("test_run"),
        "{text}"
    );

    std::fs::write(
        root.join("lib.py"),
        "def replacement():\n    return 'new_body'\n",
    )
    .unwrap();
    std::fs::write(
        root.join("app.py"),
        "from lib import replacement\ndef run():\n    return replacement()\n",
    )
    .unwrap();
    std::fs::remove_file(root.join("test_app.py")).unwrap();
    // Another process can refresh the shared index before this server observes it.
    assert!(Command::new(assert_cmd::cargo::cargo_bin!("codeq"))
        .args(["--root", root.to_str().unwrap(), "map", "--totals"])
        .output()
        .unwrap()
        .status
        .success());
    let text = server.call("codeq_explore", json!({"name": "run"}));
    assert!(
        text.contains("new_body") && text.contains("replacement"),
        "{text}"
    );
    assert!(
        !text.contains("old_body") && !text.contains("test_run"),
        "{text}"
    );
    let text = server.call("codeq_tests", json!({"name": "run"}));
    assert!(!text.contains("test_run"), "{text}");
}

#[test]
fn shared_dispatch_preserves_raw_bytes_and_result_notes() {
    let dir = tempfile::tempdir().unwrap();
    std::fs::write(
        dir.path().join("sample.rs"),
        b"fn first() {}\r\nfn second() {}\r\n",
    )
    .unwrap();
    let mut server = Server::new(dir.path());
    server.call("codeq_map", json!({"totals": true}));
    assert_eq!(
        server.call("codeq_read", json!({"file": "sample.rs", "raw": true})),
        "fn first() {}\r\nfn second() {}\r\n"
    );
    let text = server.call("codeq_search", json!({"pattern": "fn", "limit": 1}));
    assert!(text.contains("--limit"), "{text}");
    let text = server.call("codeq_definitions", json!({"symbol": "not_here"}));
    assert!(text.contains("no symbol named 'not_here'"), "{text}");
}

#[test]
fn project_alias_cache_refreshes_after_a_config_only_edit() {
    let dir = tempfile::tempdir().unwrap();
    let root = dir.path();
    std::fs::create_dir_all(root.join("app/one")).unwrap();
    std::fs::create_dir_all(root.join("app/two")).unwrap();
    std::fs::write(
        root.join("app/main.ts"),
        "import { helper } from '@/lib';\n",
    )
    .unwrap();
    for directory in ["one", "two"] {
        std::fs::write(
            root.join(format!("app/{directory}/lib.ts")),
            "export function helper() {}\n",
        )
        .unwrap();
    }
    let config = root.join("app/tsconfig.json");
    std::fs::write(
        &config,
        r#"{"compilerOptions":{"paths":{"@/*":["one/*"]}}}"#,
    )
    .unwrap();
    let mut server = Server::new(root);
    let first = server.call("codeq_deps", json!({"file":"app/main.ts"}));
    assert!(first.contains("app/one/lib.ts"), "{first}");
    std::fs::write(
        &config,
        r#"{"compilerOptions":{"paths":{"@/*":["two/*"]}}}"#,
    )
    .unwrap();
    let second = server.call("codeq_deps", json!({"file":"app/main.ts"}));
    assert!(second.contains("app/two/lib.ts"), "{second}");
    assert!(!second.contains("app/one/lib.ts"), "{second}");
}

#[test]
fn project_alias_cache_detects_creation_and_removal_of_ignored_configs() {
    let dir = tempfile::tempdir().unwrap();
    let root = dir.path();
    std::fs::create_dir_all(root.join("pkg")).unwrap();
    std::fs::create_dir_all(root.join("root")).unwrap();
    std::fs::write(root.join(".ignore"), "pkg/tsconfig.json\n").unwrap();
    std::fs::write(
        root.join("tsconfig.json"),
        r#"{"compilerOptions":{"paths":{"@/*":["root/*"]}}}"#,
    )
    .unwrap();
    std::fs::write(root.join("root/value.ts"), "export const value = 0;\n").unwrap();
    std::fs::write(
        root.join("pkg/main.ts"),
        "import { value } from '@/value';\n",
    )
    .unwrap();
    let mut server = Server::new(root);
    let first = server.call("codeq_deps", json!({"file":"pkg/main.ts"}));
    assert!(first.contains("root/value.ts"), "{first}");
    let ignored = root.join("pkg/tsconfig.json");
    std::fs::write(&ignored, "{}").unwrap();
    let blocked = server.call("codeq_deps", json!({"external":true}));
    assert!(blocked.contains("not indexed"), "{blocked}");
    assert!(!server
        .call("codeq_deps", json!({"file":"pkg/main.ts"}))
        .contains("root/value.ts"));
    std::fs::remove_file(ignored).unwrap();
    let restored = server.call("codeq_deps", json!({"file":"pkg/main.ts"}));
    assert!(restored.contains("root/value.ts"), "{restored}");
}

#[test]
fn schema_rebuild_is_reported_once_and_does_not_end_the_session() {
    let dir = tempfile::tempdir().unwrap();
    std::fs::write(dir.path().join("sample.rs"), "fn target() {}\n").unwrap();
    let mut server = Server::new(dir.path());
    server.call("codeq_definitions", json!({"symbol": "target"}));
    let db = rusqlite::Connection::open(dir.path().join(".codeq/index.db")).unwrap();
    db.execute(
        "UPDATE meta SET value = '0' WHERE key = 'schema_version'",
        [],
    )
    .unwrap();
    let text = server.call("codeq_definitions", json!({"symbol": "target"}));
    assert!(
        text.contains("target") && text.contains("schema changed"),
        "{text}"
    );
    let text = server.call("codeq_definitions", json!({"symbol": "target"}));
    assert!(
        text.contains("target") && !text.contains("schema changed"),
        "{text}"
    );
}

#[test]
fn cheap_requests_progress_during_a_blocked_query_and_cancellation_is_silent() {
    let dir = tempfile::tempdir().unwrap();
    let file = dir.path().join("sample.rs");
    std::fs::write(&file, "fn before() {}\n").unwrap();
    let mut server = Server::with_workers(dir.path(), 2);
    server.call("codeq_definitions", json!({"symbol": "before"}));
    let db = rusqlite::Connection::open(dir.path().join(".codeq/index.db")).unwrap();
    db.execute_batch("BEGIN IMMEDIATE").unwrap();
    std::fs::write(&file, "fn after() {}\n").unwrap();
    server.send(json!({"jsonrpc":"2.0","id":41,"method":"tools/call",
        "params":{"name":"codeq_search","arguments":{"pattern":"after"}}}));
    server.send(json!({"jsonrpc":"2.0","id":42,"method":"tools/call",
        "params":{"name":"codeq_capabilities","arguments":{}}}));
    server.send(json!({"jsonrpc":"2.0","id":43,"method":"ping"}));
    let mut ids = Vec::new();
    for _ in 0..2 {
        let reply = server.receive();
        assert!(reply.get("error").is_none(), "{reply}");
        assert_ne!(reply["result"]["isError"], true, "{reply}");
        ids.push(reply["id"].as_u64().unwrap());
    }
    ids.sort();
    assert_eq!(ids, [42, 43]);
    server.send(
        json!({"jsonrpc":"2.0","method":"notifications/cancelled","params":{"requestId":41}}),
    );
    server.send(json!({"jsonrpc":"2.0","id":44,"method":"ping"}));
    assert_eq!(server.receive()["id"], 44);
    db.execute_batch("ROLLBACK").unwrap();
    server.next_id = 44;
    assert!(server
        .call("codeq_definitions", json!({"symbol":"after"}))
        .contains("fn after()"));
}

#[test]
fn batches_preserve_ids_and_errors_without_replying_to_notifications() {
    let dir = tempfile::tempdir().unwrap();
    std::fs::write(dir.path().join("sample.rs"), "fn target() {}\n").unwrap();
    let mut server = Server::with_workers(dir.path(), 2);
    server.send(json!([
        {"jsonrpc":"2.0","id":"definition","method":"tools/call","params":{"name":"codeq_definitions","arguments":{"symbol":"target"}}},
        {"jsonrpc":"2.0","method":"notifications/initialized"},
        {"jsonrpc":"2.0","id":"invalid","method":"tools/call","params":{"name":"codeq_read","arguments":{"file":"missing.rs"}}},
        {"jsonrpc":"2.0","id":"ping","method":"ping"}
    ]));
    let reply = server.receive();
    let batch = reply.as_array().unwrap();
    assert_eq!(batch.len(), 3, "{reply}");
    let find = |id| batch.iter().find(|v| v["id"] == id).unwrap();
    assert!(find("definition")["result"]["content"][0]["text"]
        .as_str()
        .unwrap()
        .contains("fn target()"));
    assert_eq!(find("invalid")["result"]["isError"], true);
    assert_eq!(find("ping")["result"], json!({}));
    assert!(server
        .call("codeq_definitions", json!({"symbol":"target"}))
        .contains("fn target()"));
}

#[test]
fn bounded_queue_rejects_overload_and_cancelled_jobs_do_not_run() {
    let dir = tempfile::tempdir().unwrap();
    let file = dir.path().join("sample.rs");
    std::fs::write(&file, "fn before() {}\n").unwrap();
    let mut server = Server::new(dir.path());
    server.call("codeq_map", json!({"totals":true}));
    let db = rusqlite::Connection::open(dir.path().join(".codeq/index.db")).unwrap();
    db.execute_batch("BEGIN IMMEDIATE").unwrap();
    std::fs::write(&file, "fn after() {}\n").unwrap();
    for id in 10..80 {
        server.send(
            json!({"jsonrpc":"2.0","id":id,"method":"tools/call", "params":{
            "name":"codeq_definitions","arguments":{"symbol":"after"}}}),
        );
    }
    server.send(json!({"jsonrpc":"2.0","id":100,"method":"ping"}));
    let mut rejected = 0;
    loop {
        let reply = server.receive();
        if reply["id"] == 100 {
            break;
        }
        assert_eq!(reply["error"]["code"], -32000, "{reply}");
        assert!(reply["error"]["message"]
            .as_str()
            .unwrap()
            .contains("queue is full"));
        rejected += 1;
    }
    assert!(rejected > 0);
    for id in 10..80 {
        server.send(
            json!({"jsonrpc":"2.0","method":"notifications/cancelled","params":{"requestId":id}}),
        );
    }
    server.send(json!({"jsonrpc":"2.0","id":101,"method":"ping"}));
    assert_eq!(server.receive()["id"], 101);
    db.execute_batch("ROLLBACK").unwrap();
    for id in 102..202 {
        server.send(
            json!({"jsonrpc":"2.0","id":id,"method":"tools/call", "params":{
            "name":"codeq_definitions","arguments":{"symbol":"after"}}}),
        );
        let reply = server.receive();
        assert_eq!(reply["id"], id, "cancelled job replied: {reply}");
        if reply["error"]["code"] == -32000 {
            std::thread::sleep(Duration::from_millis(10));
            continue;
        }
        assert!(reply["result"]["content"][0]["text"]
            .as_str()
            .unwrap()
            .contains("fn after()"));
        return;
    }
    panic!("queue did not recover after cancellation");
}
