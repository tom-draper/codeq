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

/// The agent-facing guide to codeq, compiled into the binary.
///
/// Embedded rather than copied from the checkout, because the two have to agree:
/// a skill describing flags the binary does not have is worse than no skill, and
/// this repo's own log records several rounds of exactly that drift. Shipping the
/// text inside the binary makes "the installed guide matches the installed
/// codeq" true by construction instead of by discipline.
const SKILL: &str = include_str!("../.claude/skills/codeq/SKILL.md");

/// A supported agent: its install marker (a dir/file whose presence means the
/// agent is installed) and the JSON config file its MCP servers live in. Every
/// agent here uses the common `{ "mcpServers": { ... } }` shape.
///
/// `skill` is where that agent looks for a `SKILL.md`, when it has such a
/// convention at all. Only Claude Code does among these — Cursor, Windsurf and
/// Gemini take project rules in their own shapes, and guessing at those would
/// write files no one reads.
struct Agent {
    name: &'static str,
    marker: PathBuf,
    config: PathBuf,
    skill: Option<PathBuf>,
}

fn known_agents(home: &Path) -> Vec<Agent> {
    vec![
        Agent {
            name: "claude-code",
            marker: home.join(".claude"),
            config: home.join(".claude.json"),
            skill: Some(home.join(".claude/skills/codeq/SKILL.md")),
        },
        Agent {
            name: "cursor",
            marker: home.join(".cursor"),
            config: home.join(".cursor/mcp.json"),
            skill: None,
        },
        Agent {
            name: "windsurf",
            marker: home.join(".codeium/windsurf"),
            config: home.join(".codeium/windsurf/mcp_config.json"),
            skill: None,
        },
        Agent {
            name: "gemini",
            marker: home.join(".gemini"),
            config: home.join(".gemini/settings.json"),
            skill: None,
        },
    ]
}

/// What happened (or would happen) for one agent.
#[derive(Serialize, Debug)]
pub struct Action {
    pub agent: &'static str,
    /// What was written: the MCP server entry (`mcp`) or the skill (`skill`).
    pub what: &'static str,
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
    /// Skip the skill and register only the MCP server.
    pub no_skill: bool,
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
                what: "mcp",
                config: agent.config.display().to_string(),
                action: "skipped",
                note: Some("not detected".into()),
            });
            continue;
        }
        out.push(apply_one(&agent, opts));
        if !opts.no_skill {
            if let Some(dest) = &agent.skill {
                out.push(apply_skill(agent.name, home, dest, opts));
            }
        }
    }
    Ok(out)
}

/// Write (or remove) the agent's copy of the skill.
///
/// An existing **symlink** is left exactly as it is. That is how a codeq
/// developer wires the checkout's `SKILL.md` into their agent, and silently
/// replacing that link with a snapshot would detach their edits from the file
/// they are editing — the tool quietly undoing the setup its own repo uses.
fn apply_skill(agent: &'static str, home: &Path, dest: &Path, opts: &Options) -> Action {
    let done = |action, note| Action {
        agent,
        what: "skill",
        config: dest.display().to_string(),
        action,
        note,
    };
    // Any component may be the link, and usually is: linking the whole
    // `skills/codeq` directory is the natural way to wire up a checkout, and
    // testing only the final `SKILL.md` would follow that link and write
    // *through* it — editing the user's repository from an install command.
    let linked = dest
        .ancestors()
        .take_while(|p| p.starts_with(home) && *p != home)
        .any(|p| std::fs::symlink_metadata(p).is_ok_and(|m| m.file_type().is_symlink()));

    if opts.uninstall {
        // Deleting through the link would remove the file from the checkout it
        // points at. codeq did not create that link and will not undo it.
        if linked {
            return done(
                "skipped",
                Some("symlinked to a checkout — remove the link by hand".into()),
            );
        }
        if !dest.exists() {
            return done("already-absent", None);
        }
        if opts.dry_run {
            return done("would-remove", None);
        }
        if let Err(e) = std::fs::remove_file(dest) {
            return done("skipped", Some(format!("remove failed: {e}")));
        }
        // Only if codeq created it and nothing else moved in.
        if let Some(dir) = dest.parent() {
            let empty = std::fs::read_dir(dir).is_ok_and(|mut d| d.next().is_none());
            if empty {
                std::fs::remove_dir(dir).ok();
            }
        }
        return done("removed", None);
    }

    if linked {
        return done(
            "already-current",
            Some("symlinked to a checkout — left alone".into()),
        );
    }
    let current = std::fs::read_to_string(dest).ok();
    if current.as_deref() == Some(SKILL) {
        return done("already-current", None);
    }
    let action = if current.is_some() {
        "updated"
    } else {
        "installed"
    };
    if opts.dry_run {
        return done(
            if current.is_some() {
                "would-update"
            } else {
                "would-install"
            },
            None,
        );
    }
    if let Some(dir) = dest.parent() {
        if let Err(e) = std::fs::create_dir_all(dir) {
            return done(
                "skipped",
                Some(format!("cannot create {}: {e}", dir.display())),
            );
        }
    }
    match std::fs::write(dest, SKILL) {
        Ok(()) => done(action, None),
        Err(e) => done("skipped", Some(format!("write failed: {e}"))),
    }
}

fn apply_one(agent: &Agent, opts: &Options) -> Action {
    let config = agent.config.display().to_string();
    let done = |action, note| Action {
        agent: agent.name,
        what: "mcp",
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
    let servers = obj.entry("mcpServers").or_insert_with(|| json!({}));
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
        return done(
            if present {
                "would-update"
            } else {
                "would-install"
            },
            None,
        );
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
            no_skill: true,
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
        assert_eq!(
            acts.iter().find(|a| a.agent == "cursor").unwrap().action,
            "already-current"
        );

        std::fs::remove_dir_all(&home).ok();
    }

    #[test]
    fn skips_undetected_agents_and_uninstall_removes_only_codeq() {
        let home = tmp();
        // Nothing installed: every agent is skipped, nothing is written.
        let acts = apply(&home, &opts("/usr/bin/codeq")).unwrap();
        assert!(acts.iter().all(|a| a.action == "skipped"), "{acts:?}");
        assert!(
            !home.join(".cursor/mcp.json").exists(),
            "must not create config for undetected agent"
        );

        // Target claude-code explicitly (creates its config), then uninstall.
        std::fs::write(home.join(".claude.json"), "{}").unwrap();
        let mut o = opts("/usr/bin/codeq");
        o.only = Some("claude-code".into());
        apply(&home, &o).unwrap();
        let cfg: Value =
            serde_json::from_str(&std::fs::read_to_string(home.join(".claude.json")).unwrap())
                .unwrap();
        assert_eq!(cfg["mcpServers"]["codeq"]["command"], "/usr/bin/codeq");

        o.uninstall = true;
        let acts = apply(&home, &o).unwrap();
        assert_eq!(acts[0].action, "removed");
        let cfg: Value =
            serde_json::from_str(&std::fs::read_to_string(home.join(".claude.json")).unwrap())
                .unwrap();
        assert!(
            cfg["mcpServers"].get("codeq").is_none(),
            "codeq should be gone"
        );

        std::fs::remove_dir_all(&home).ok();
    }

    #[test]
    fn installs_the_skill_beside_the_mcp_entry_and_is_idempotent() {
        let home = tmp();
        std::fs::create_dir_all(home.join(".claude")).unwrap();
        let dest = home.join(".claude/skills/codeq/SKILL.md");

        let mut o = opts("/usr/bin/codeq");
        o.no_skill = false;
        let acts = apply(&home, &o).unwrap();
        let skill = acts.iter().find(|a| a.what == "skill").unwrap();
        assert_eq!(skill.action, "installed", "{:?}", skill.note);
        assert_eq!(std::fs::read_to_string(&dest).unwrap(), SKILL);

        // Re-running changes nothing.
        let acts = apply(&home, &o).unwrap();
        assert_eq!(
            acts.iter().find(|a| a.what == "skill").unwrap().action,
            "already-current"
        );

        // An out-of-date copy is refreshed, so the guide tracks the binary.
        std::fs::write(&dest, "stale").unwrap();
        let acts = apply(&home, &o).unwrap();
        assert_eq!(
            acts.iter().find(|a| a.what == "skill").unwrap().action,
            "updated"
        );
        assert_eq!(std::fs::read_to_string(&dest).unwrap(), SKILL);

        // --no-skill leaves it out of the run entirely.
        o.no_skill = true;
        assert!(apply(&home, &o).unwrap().iter().all(|a| a.what == "mcp"));

        std::fs::remove_dir_all(&home).ok();
    }

    #[cfg(unix)]
    #[test]
    fn a_symlinked_skill_is_never_written_through_or_deleted() {
        // Linking `skills/codeq` at the *directory* is how a codeq developer
        // wires up their checkout. Writing the embedded copy to
        // `skills/codeq/SKILL.md` would follow that link and edit the
        // repository; removing it would delete the repository's file.
        let home = tmp();
        let checkout = home.join("checkout/.claude/skills/codeq");
        std::fs::create_dir_all(&checkout).unwrap();
        std::fs::write(checkout.join("SKILL.md"), "my working copy").unwrap();
        std::fs::create_dir_all(home.join(".claude/skills")).unwrap();
        std::os::unix::fs::symlink(&checkout, home.join(".claude/skills/codeq")).unwrap();

        let mut o = opts("/usr/bin/codeq");
        o.no_skill = false;
        let acts = apply(&home, &o).unwrap();
        let skill = acts.iter().find(|a| a.what == "skill").unwrap();
        assert_eq!(skill.action, "already-current", "{skill:?}");
        assert!(
            skill.note.as_deref().is_some_and(|n| n.contains("symlink")),
            "{skill:?}"
        );
        assert_eq!(
            std::fs::read_to_string(checkout.join("SKILL.md")).unwrap(),
            "my working copy",
            "install wrote through the symlink into the checkout"
        );

        o.uninstall = true;
        let acts = apply(&home, &o).unwrap();
        assert_eq!(
            acts.iter().find(|a| a.what == "skill").unwrap().action,
            "skipped"
        );
        assert!(
            checkout.join("SKILL.md").exists(),
            "uninstall deleted the checkout's file through the symlink"
        );

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
        assert_eq!(
            acts.iter().find(|a| a.agent == "gemini").unwrap().action,
            "would-install"
        );
        let cfg: Value = serde_json::from_str(
            &std::fs::read_to_string(home.join(".gemini/settings.json")).unwrap(),
        )
        .unwrap();
        assert!(cfg.get("mcpServers").is_none(), "dry-run must not write");

        std::fs::remove_dir_all(&home).ok();
    }
}
