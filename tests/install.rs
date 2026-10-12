//! Installer checks isolate the user's home and exercise the real MCP handshake.
use assert_cmd::cargo::CommandCargoExt;
use serde_json::Value;
use std::process::{Command, Output};
use tempfile::TempDir;

fn run(home: &TempDir, args: &[&str]) -> Output {
    Command::cargo_bin("codeq")
        .unwrap()
        .args(args)
        .arg("--json")
        .env("HOME", home.path())
        .env("USERPROFILE", home.path())
        .env("CODEX_HOME", home.path().join("custom-config"))
        .current_dir(home.path())
        .output()
        .unwrap()
}

fn success(out: Output) -> Value {
    assert!(
        out.status.success(),
        "{}",
        String::from_utf8_lossy(&out.stderr)
    );
    serde_json::from_slice(&out.stdout).unwrap()
}

#[test]
fn guide_matches_installed_skill_and_detects_stale_instructions_without_writes() {
    let home = TempDir::new().unwrap();
    let guide = success(run(&home, &["guide"]));
    assert!(guide["data"]["guide"]
        .as_str()
        .unwrap()
        .contains("codeq search PATTERN --source"));
    assert_eq!(std::fs::read_dir(home.path()).unwrap().count(), 0);

    let text = Command::cargo_bin("codeq")
        .unwrap()
        .arg("guide")
        .current_dir(home.path())
        .output()
        .unwrap();
    assert!(text.status.success());
    assert_eq!(
        text.stdout,
        guide["data"]["guide"].as_str().unwrap().as_bytes()
    );

    success(run(&home, &["install", "--agent", "cursor"]));
    let path = home.path().join(".agents/skills/codeq/SKILL.md");
    assert_eq!(std::fs::read(&path).unwrap(), text.stdout);
    let path_arg = path.to_str().unwrap();
    let current = success(run(&home, &["guide", "--check", path_arg]));
    assert_eq!(current["data"]["matches"], true);
    std::fs::write(&path, "old instructions\n").unwrap();
    let stale = run(&home, &["guide", "--check", path_arg]);
    assert!(!stale.status.success());
    let result: Value = serde_json::from_slice(&stale.stdout).unwrap();
    assert_eq!(result["data"]["matches"], false);
    assert!(String::from_utf8_lossy(&stale.stderr).contains("codeq guide"));
    assert_eq!(std::fs::read(&path).unwrap(), b"old instructions\n");
    assert!(!run(&home, &["guide", "--check", "missing.md"])
        .status
        .success());
    assert!(!home.path().join(".codeq/index.db").exists());
}

#[test]
fn install_doctor_and_uninstall_work_for_every_supported_agent() {
    for agent in ["claude-code", "cursor", "windsurf", "gemini", "codex"] {
        let home = TempDir::new().unwrap();
        success(run(&home, &["install", "--agent", agent]));
        let health = success(run(&home, &["doctor", "--agent", agent]));
        assert!(health["data"]
            .as_array()
            .unwrap()
            .iter()
            .all(|a| a["action"] == "healthy"));
        let repeated = success(run(&home, &["install", "--agent", agent]));
        assert!(repeated["data"]
            .as_array()
            .unwrap()
            .iter()
            .all(|a| a["action"] == "already-current"));
        let removed = success(run(&home, &["uninstall", "--agent", agent]));
        assert!(removed["data"]
            .as_array()
            .unwrap()
            .iter()
            .all(|a| a["action"] == "removed"));
        assert!(!run(&home, &["doctor", "--agent", agent]).status.success());
        success(run(&home, &["install", "--uninstall", "--agent", agent]));
        assert!(!home.path().join(".codeq/index.db").exists());
    }
}

#[test]
fn partial_failure_is_nonzero_and_successful_agents_are_reported() {
    let home = TempDir::new().unwrap();
    std::fs::create_dir_all(home.path().join(".cursor")).unwrap();
    std::fs::write(home.path().join(".cursor/mcp.json"), "{broken").unwrap();
    std::fs::create_dir_all(home.path().join(".gemini")).unwrap();
    let out = run(&home, &["install"]);
    assert!(!out.status.success());
    let result: Value = serde_json::from_slice(&out.stdout).unwrap();
    let actions = result["data"].as_array().unwrap();
    assert!(actions
        .iter()
        .any(|a| a["agent"] == "cursor" && a["action"] == "failed"));
    assert!(actions
        .iter()
        .any(|a| a["agent"] == "gemini" && a["action"] == "installed"));
    success(run(&home, &["doctor", "--agent", "gemini"]));
}

#[test]
fn doctor_reports_missing_binary_without_rewriting_configuration() {
    let home = TempDir::new().unwrap();
    success(run(&home, &["install", "--agent", "cursor"]));
    let path = home.path().join(".cursor/mcp.json");
    let mut config: Value = serde_json::from_slice(&std::fs::read(&path).unwrap()).unwrap();
    config["mcpServers"]["codeq"]["command"] =
        Value::String(home.path().join("missing").display().to_string());
    let bytes = serde_json::to_vec(&config).unwrap();
    std::fs::write(&path, &bytes).unwrap();
    let out = run(&home, &["doctor", "--agent", "cursor"]);
    assert!(!out.status.success());
    let result: Value = serde_json::from_slice(&out.stdout).unwrap();
    assert!(result["data"]
        .as_array()
        .unwrap()
        .iter()
        .any(|a| a["what"] == "mcp" && a["action"] == "failed"));
    assert_eq!(std::fs::read(&path).unwrap(), bytes);
}

#[test]
fn invalid_agent_and_dry_run_write_nothing() {
    let home = TempDir::new().unwrap();
    assert!(!run(&home, &["install", "--agent", "unknown"])
        .status
        .success());
    assert_eq!(std::fs::read_dir(home.path()).unwrap().count(), 0);
    success(run(&home, &["install", "--agent", "cursor", "--dry-run"]));
    assert_eq!(std::fs::read_dir(home.path()).unwrap().count(), 0);
}
