//! Bounded MCP startup and tool-discovery verification, without repository queries.
use anyhow::{bail, Context, Result};
use serde_json::{json, Value};
use std::io::{Read, Seek, SeekFrom, Write};
use std::process::{Command, Stdio};
use std::time::{Duration, Instant};

pub(super) fn probe(bin: &str) -> Result<()> {
    probe_with_timeout(bin, Duration::from_secs(5))
}

fn probe_with_timeout(bin: &str, timeout: Duration) -> Result<()> {
    let mut stdout = tempfile::tempfile()?;
    let stderr = tempfile::tempfile()?;
    let mut child = Command::new(bin)
        .arg("mcp")
        .stdin(Stdio::piped())
        .stdout(stdout.try_clone()?)
        .stderr(stderr.try_clone()?)
        .spawn()
        .with_context(|| format!("cannot start MCP server at {bin}"))?;
    let result = (|| -> Result<()> {
        {
            let mut stdin = child.stdin.take().context("MCP stdin unavailable")?;
            writeln!(
                stdin,
                "{}",
                json!({"jsonrpc":"2.0","id":1,"method":"initialize","params":{"protocolVersion":"2025-06-18","capabilities":{},"clientInfo":{"name":"codeq-doctor","version":"1"}}})
            )?;
            writeln!(
                stdin,
                "{}",
                json!({"jsonrpc":"2.0","method":"notifications/initialized"})
            )?;
            writeln!(
                stdin,
                "{}",
                json!({"jsonrpc":"2.0","id":2,"method":"tools/list"})
            )?;
        }
        let start = Instant::now();
        loop {
            if stdout.metadata()?.len() > 1_048_576 || stderr.metadata()?.len() > 1_048_576 {
                bail!("MCP startup output exceeds 1 MiB");
            }
            if let Some(status) = child.try_wait()? {
                if !status.success() {
                    bail!("MCP server exited unsuccessfully ({status})");
                }
                break;
            }
            if start.elapsed() >= timeout {
                bail!("MCP startup timed out after {} seconds", timeout.as_secs());
            }
            std::thread::sleep(Duration::from_millis(10));
        }
        if stdout.metadata()?.len() > 1_048_576 {
            bail!("MCP startup output exceeds 1 MiB");
        }
        stdout.seek(SeekFrom::Start(0))?;
        let mut text = String::new();
        stdout.take(1_048_577).read_to_string(&mut text)?;
        let messages: Vec<Value> = text
            .lines()
            .filter(|line| !line.trim().is_empty())
            .map(serde_json::from_str)
            .collect::<std::result::Result<_, _>>()
            .context("MCP returned invalid JSON")?;
        let init = messages
            .iter()
            .find(|v| v["id"] == 1)
            .context("MCP initialize response missing")?;
        if init["result"]["serverInfo"]["name"] != "codeq"
            || !init["result"]["capabilities"]["tools"].is_object()
            || init["result"]["protocolVersion"] != "2025-06-18"
        {
            bail!("MCP initialize response is not a compatible codeq server");
        }
        let tools = messages
            .iter()
            .find(|v| v["id"] == 2)
            .and_then(|v| v["result"]["tools"].as_array())
            .context("MCP tools/list response missing")?;
        for name in ["codeq_search", "codeq_read"] {
            if !tools.iter().any(|v| v["name"] == name) {
                bail!("MCP does not expose {name}");
            }
        }
        Ok(())
    })();
    if result.is_err() {
        let _ = child.kill();
    }
    let _ = child.wait();
    result
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn missing_executable_fails() {
        assert!(probe("/nonexistent/codeq-server").is_err());
    }
    #[cfg(unix)]
    #[test]
    fn timeout_kills_and_reaps_server() {
        use std::os::unix::fs::PermissionsExt;
        let dir = tempfile::tempdir().unwrap();
        let bin = dir.path().join("server");
        std::fs::write(&bin, "#!/bin/sh\nexec sleep 10\n").unwrap();
        std::fs::set_permissions(&bin, std::fs::Permissions::from_mode(0o700)).unwrap();
        let start = Instant::now();
        assert!(probe_with_timeout(bin.to_str().unwrap(), Duration::from_millis(100)).is_err());
        assert!(start.elapsed() < Duration::from_secs(2));
    }
}

#[cfg(unix)]
#[test]
fn rejects_non_servers_and_incomplete_protocol_responses() {
    use std::os::unix::fs::PermissionsExt;
    let dir = tempfile::tempdir().unwrap();
    let script = dir.path().join("server");
    for output in [
        "not json",
        r#"{"id":1,"result":{"serverInfo":{"name":"other"}}}"#,
        r#"{"id":1,"result":{"serverInfo":{"name":"codeq"},"protocolVersion":"2025-06-18","capabilities":{"tools":{}}}}
{"id":2,"result":{"tools":[]}}"#,
    ] {
        std::fs::write(
            &script,
            format!("#!/bin/sh\ncat >/dev/null\ncat <<'END'\n{output}\nEND\n"),
        )
        .unwrap();
        std::fs::set_permissions(&script, std::fs::Permissions::from_mode(0o700)).unwrap();
        assert!(probe(script.to_str().unwrap()).is_err());
    }
}
