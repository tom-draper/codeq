//! `codeq install` — register the MCP server with detected AI coding agents.
//!
//! This writes to each agent's own MCP config (in the user's home directory), not
//! to any repository. It is the one codeq command that touches files outside the
//! index, so it is deliberately conservative: it only edits agents it detects
//! (unless one is named explicitly), merges into existing config without dropping
//! other keys, is idempotent, and supports `--dry-run` and `--uninstall`.

use std::path::{Path, PathBuf};

use anyhow::{Context, Result};
use serde::Serialize;
use serde_json::{json, Value};

/// A supported agent: its install marker (a dir/file whose presence means the
/// agent is installed) and the JSON config file its MCP servers live in. Every
/// agent here uses the common `{ "mcpServers": { ... } }` shape.
struct Agent {
    name: &'static str,
    marker: PathBuf,
    config: PathBuf,
}

fn known_agents(home: &Path) -> Vec<Agent> {
    vec![
        Agent {
            name: "claude-code",
            marker: home.join(".claude"),
            config: home.join(".claude.json"),
        },
        Agent {
            name: "cursor",
            marker: home.join(".cursor"),
            config: home.join(".cursor/mcp.json"),
        },
        Agent {
            name: "windsurf",
            marker: home.join(".codeium/windsurf"),
            config: home.join(".codeium/windsurf/mcp_config.json"),
        },
        Agent {
            name: "gemini",
            marker: home.join(".gemini"),
            config: home.join(".gemini/settings.json"),
        },
    ]
}

/// What happened (or would happen) for one agent.
#[derive(Serialize, Debug)]
pub struct Action {
    pub agent: &'static str,
    pub config: String,
    /// installed / updated / already-current / removed / already-absent /
    /// would-install / would-update / would-remove / skipped.
    pub action: &'static str,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub note: Option<String>,
}

pub struct Options {
    pub dry_run: bool,
    pub uninstall: bool,
    /// Target just this agent (and create its config even if not detected).
    pub only: Option<String>,
    /// The codeq binary path written into each agent's config.
    pub bin: String,
}

/// The user's home directory, from `$HOME` (or `%USERPROFILE%` on Windows).
pub fn home_dir() -> Result<PathBuf> {
    std::env::var_os("HOME")
        .or_else(|| std::env::var_os("USERPROFILE"))
        .map(PathBuf::from)
        .context("cannot determine home directory ($HOME is not set)")
}

/// Detect agents and install/remove the codeq MCP entry per `opts`. Pure in
/// `home`, so it is unit-testable against a temporary directory.
pub fn apply(home: &Path, opts: &Options) -> Result<Vec<Action>> {
    let mut out = Vec::new();
    for agent in known_agents(home) {
        if let Some(only) = &opts.only {
            if only != agent.name {
                continue;
            }
        }
        // Without an explicit --agent, only touch agents that are actually
        // installed; with one, target it even if its config doesn't exist yet.
        let detected = agent.marker.exists() || agent.config.exists();
        if !detected && opts.only.is_none() {
            out.push(Action {
                agent: agent.name,
                config: agent.config.display().to_string(),
                action: "skipped",
                note: Some("not detected".into()),
            });
            continue;
        }
        out.push(apply_one(&agent, opts));
    }
    Ok(out)
}

fn apply_one(agent: &Agent, opts: &Options) -> Action {
    let config = agent.config.display().to_string();
    let done = |action, note| Action {
        agent: agent.name,
        config: config.clone(),
        action,
        note,
    };

    // Load existing config (or start empty). A malformed file is reported and
    // skipped rather than crashing the whole run or clobbering the user's file.
    let mut root: Value = if agent.config.exists() {
        match std::fs::read_to_string(&agent.config) {
            Ok(t) if t.trim().is_empty() => json!({}),
            Ok(t) => match serde_json::from_str(&t) {
                Ok(v) => v,
                Err(e) => return done("skipped", Some(format!("config is not valid JSON: {e}"))),
            },
            Err(e) => return done("skipped", Some(format!("cannot read config: {e}"))),
        }
    } else {
        json!({})
    };

    let Some(obj) = root.as_object_mut() else {
        return done("skipped", Some("config root is not a JSON object".into()));
    };
    let servers = obj
        .entry("mcpServers")
        .or_insert_with(|| json!({}));
    let Some(servers) = servers.as_object_mut() else {
        return done("skipped", Some("`mcpServers` is not a JSON object".into()));
    };

    let present = servers.contains_key("codeq");

    if opts.uninstall {
        if !present {
            return done("already-absent", None);
        }
        if opts.dry_run {
            return done("would-remove", None);
        }
        servers.remove("codeq");
        return match write_json(&agent.config, &root) {
            Ok(()) => done("removed", None),
            Err(e) => done("skipped", Some(format!("write failed: {e}"))),
        };
    }

    let entry = json!({ "command": opts.bin, "args": ["mcp"] });
    if present && servers.get("codeq") == Some(&entry) {
        return done("already-current", None);
    }
    let action = if present { "updated" } else { "installed" };
    if opts.dry_run {
        return done(if present { "would-update" } else { "would-install" }, None);
    }
    servers.insert("codeq".into(), entry);
    match write_json(&agent.config, &root) {
        Ok(()) => done(action, None),
        Err(e) => done("skipped", Some(format!("write failed: {e}"))),
    }
}

fn write_json(path: &Path, root: &Value) -> Result<()> {
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)?;
    }
    // Pretty-printed and, thanks to serde_json's preserve_order feature, in the
    // file's original key order (codeq's key is appended), so a rewrite is a
    // minimal diff to a machine-managed config.
    let mut s = serde_json::to_string_pretty(root)?;
    s.push('\n');
    std::fs::write(path, s)?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn opts(bin: &str) -> Options {
        Options {
            dry_run: false,
            uninstall: false,
            only: None,
            bin: bin.into(),
        }
    }

    fn tmp() -> PathBuf {
        static N: std::sync::atomic::AtomicU32 = std::sync::atomic::AtomicU32::new(0);
        let n = N.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
        let d = std::env::temp_dir().join(format!("codeq-install-{}-{n}", std::process::id()));
        std::fs::create_dir_all(&d).unwrap();
        d
    }

    #[test]
    fn installs_into_a_detected_agent_and_is_idempotent() {
        let home = tmp();
        // Pretend Cursor is installed, with a pre-existing unrelated server.
        std::fs::create_dir_all(home.join(".cursor")).unwrap();
        std::fs::write(
            home.join(".cursor/mcp.json"),
            r#"{"mcpServers":{"other":{"command":"x"}}}"#,
        )
        .unwrap();

        let acts = apply(&home, &opts("/usr/bin/codeq")).unwrap();
        let cursor = acts.iter().find(|a| a.agent == "cursor").unwrap();
        assert_eq!(cursor.action, "installed", "{:?}", cursor.note);

        let cfg: Value =
            serde_json::from_str(&std::fs::read_to_string(home.join(".cursor/mcp.json")).unwrap())
                .unwrap();
        // codeq added, the existing server preserved.
        assert_eq!(cfg["mcpServers"]["codeq"]["command"], "/usr/bin/codeq");
        assert_eq!(cfg["mcpServers"]["codeq"]["args"][0], "mcp");
        assert_eq!(cfg["mcpServers"]["other"]["command"], "x");

        // Re-running is idempotent.
        let acts = apply(&home, &opts("/usr/bin/codeq")).unwrap();
        assert_eq!(acts.iter().find(|a| a.agent == "cursor").unwrap().action, "already-current");

        std::fs::remove_dir_all(&home).ok();
    }

    #[test]
    fn skips_undetected_agents_and_uninstall_removes_only_codeq() {
        let home = tmp();
        // Nothing installed: every agent is skipped, nothing is written.
        let acts = apply(&home, &opts("/usr/bin/codeq")).unwrap();
        assert!(acts.iter().all(|a| a.action == "skipped"), "{acts:?}");
        assert!(!home.join(".cursor/mcp.json").exists(), "must not create config for undetected agent");

        // Target claude-code explicitly (creates its config), then uninstall.
        std::fs::write(home.join(".claude.json"), "{}").unwrap();
        let mut o = opts("/usr/bin/codeq");
        o.only = Some("claude-code".into());
        apply(&home, &o).unwrap();
        let cfg: Value =
            serde_json::from_str(&std::fs::read_to_string(home.join(".claude.json")).unwrap()).unwrap();
        assert_eq!(cfg["mcpServers"]["codeq"]["command"], "/usr/bin/codeq");

        o.uninstall = true;
        let acts = apply(&home, &o).unwrap();
        assert_eq!(acts[0].action, "removed");
        let cfg: Value =
            serde_json::from_str(&std::fs::read_to_string(home.join(".claude.json")).unwrap()).unwrap();
        assert!(cfg["mcpServers"].get("codeq").is_none(), "codeq should be gone");

        std::fs::remove_dir_all(&home).ok();
    }

    #[test]
    fn dry_run_writes_nothing() {
        let home = tmp();
        std::fs::create_dir_all(home.join(".gemini")).unwrap();
        std::fs::write(home.join(".gemini/settings.json"), "{}").unwrap();

        let mut o = opts("/usr/bin/codeq");
        o.dry_run = true;
        let acts = apply(&home, &o).unwrap();
        assert_eq!(acts.iter().find(|a| a.agent == "gemini").unwrap().action, "would-install");
        let cfg: Value =
            serde_json::from_str(&std::fs::read_to_string(home.join(".gemini/settings.json")).unwrap())
                .unwrap();
        assert!(cfg.get("mcpServers").is_none(), "dry-run must not write");

        std::fs::remove_dir_all(&home).ok();
    }
}
