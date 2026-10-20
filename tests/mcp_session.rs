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
        let mut child = Command::new(assert_cmd::cargo::cargo_bin!("codeq"))
            .args(["--root", root.to_str().unwrap(), "mcp"])
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
