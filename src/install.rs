//! Persistent per-user MCP and skill setup, with ownership-aware updates.
use std::path::{Path, PathBuf};

use anyhow::{bail, Context, Result};
use serde::Serialize;

mod config;
mod health;
mod storage;
#[cfg(test)]
mod tests;

use config::Config;
use storage::{Manifest, Owned};

/// One source of truth, compiled with the CLI that the guide describes.
pub const SKILL: &str = include_str!("../.claude/skills/codeq/SKILL.md");
pub const AGENTS: &[&str] = &["claude-code", "cursor", "windsurf", "gemini", "codex"];

struct Agent {
    name: &'static str,
    marker: PathBuf,
    config: PathBuf,
    skill: PathBuf,
    toml: bool,
}

fn known_agents(home: &Path, codex_home: Option<&Path>) -> Vec<Agent> {
    let shared = home.join(".agents/skills/codeq/SKILL.md");
    let codex = codex_home
        .map(Path::to_path_buf)
        .unwrap_or_else(|| home.join(".codex"));
    vec![
        Agent {
            name: "claude-code",
            marker: home.join(".claude"),
            config: home.join(".claude.json"),
            skill: home.join(".claude/skills/codeq/SKILL.md"),
            toml: false,
        },
        Agent {
            name: "cursor",
            marker: home.join(".cursor"),
            config: home.join(".cursor/mcp.json"),
            skill: shared.clone(),
            toml: false,
        },
        Agent {
            name: "windsurf",
            marker: home.join(".codeium/windsurf"),
            config: home.join(".codeium/windsurf/mcp_config.json"),
            skill: home.join(".codeium/windsurf/skills/codeq/SKILL.md"),
            toml: false,
        },
        Agent {
            name: "gemini",
            marker: home.join(".gemini"),
            config: home.join(".gemini/settings.json"),
            skill: shared.clone(),
            toml: false,
        },
        Agent {
            name: "codex",
            config: codex.join("config.toml"),
            marker: codex,
            skill: shared,
            toml: true,
        },
    ]
}

#[derive(Serialize, Debug)]
pub struct Action {
    pub agent: &'static str,
    pub what: &'static str,
    pub config: String,
    pub action: &'static str,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub note: Option<String>,
}

impl Action {
    fn new(
        agent: &Agent,
        what: &'static str,
        path: &Path,
        action: &'static str,
        note: Option<String>,
    ) -> Self {
        Self {
            agent: agent.name,
            what,
            config: path.display().to_string(),
            action,
            note,
        }
    }
    pub fn failed(&self) -> bool {
        self.action == "failed"
    }
}

pub struct Options {
    pub dry_run: bool,
    pub uninstall: bool,
    pub only: Option<String>,
    pub bin: String,
    pub no_skill: bool,
    /// Explicit replacement saves the prior entry/file for uninstall to restore.
    pub force: bool,
    pub codex_home: Option<PathBuf>,
}

pub fn home_dir() -> Result<PathBuf> {
    std::env::var_os("HOME")
        .or_else(|| std::env::var_os("USERPROFILE"))
        .map(PathBuf::from)
        .context("cannot determine home directory")
}

fn validate_agent(only: Option<&str>) -> Result<()> {
    if let Some(name) = only {
        if !AGENTS.contains(&name) {
            bail!("unknown agent '{name}'; choose {}", AGENTS.join(", "));
        }
    }
    Ok(())
}

/// Mutations serialize through a per-user lock. The journal is saved before
/// install writes, so a partial install remains owned and can be completed by
/// rerunning. Uninstall releases ownership only after its writes succeed.
pub fn apply(home: &Path, opts: &Options) -> Result<Vec<Action>> {
    validate_agent(opts.only.as_deref())?;
    let mut out = Vec::new();
    let prior = storage::load(home)?;
    let agents: Vec<_> = known_agents(home, opts.codex_home.as_deref())
        .into_iter()
        .filter(|a| opts.only.as_deref().is_none_or(|only| a.name == only))
        .collect();
    let selected: Vec<_> = agents
        .iter()
        .filter(|a| {
            let detected = a.marker.exists()
                || a.config.exists()
                || (opts.uninstall && prior.entries.values().any(|r| r.owners.contains(a.name)));
            if !detected && opts.only.is_none() {
                out.push(Action::new(
                    a,
                    "mcp",
                    &a.config,
                    "skipped",
                    Some("not detected".into()),
                ));
            }
            detected || opts.only.is_some()
        })
        .collect();
    if selected.is_empty() {
        return Ok(out);
    }
    let _lock = if opts.dry_run {
        None
    } else {
        Some(storage::lock(home)?)
    };
    let mut state = storage::load(home)?;
    for agent in selected {
        match apply_agent(home, agent, opts, &mut state) {
            Ok(actions) => out.extend(actions),
            Err(e) => out.push(Action::new(
                agent,
                "setup",
                &agent.config,
                "failed",
                Some(format!("{e:#}")),
            )),
        }
    }
    Ok(out)
}

struct Decision {
    after: Option<Vec<u8>>,
    owned: Option<Owned>,
    status: &'static str,
}

fn decide(
    owned: Option<&Owned>,
    owner: &str,
    current: Option<Vec<u8>>,
    desired: Vec<u8>,
    opts: &Options,
) -> Result<Decision> {
    if opts.uninstall {
        let Some(record) = owned else {
            return Ok(Decision {
                status: if current.is_some() {
                    "preserved"
                } else {
                    "already-absent"
                },
                after: current,
                owned: None,
            });
        };
        let mut record = record.clone();
        if !record.owners.remove(owner) || !record.owners.is_empty() {
            return Ok(Decision {
                after: current,
                owned: Some(record),
                status: "preserved",
            });
        }
        if current.is_some()
            && current.as_ref() != Some(&record.installed)
            && current != record.previous
        {
            bail!("managed content was edited; left unchanged (restore it or manage it manually)");
        }
        let after = if current.is_none() {
            None
        } else {
            record.previous
        };
        let status = if current == after {
            "already-absent"
        } else if after.is_some() {
            "restored"
        } else {
            "removed"
        };
        return Ok(Decision {
            after,
            owned: None,
            status,
        });
    }
    let conflict = match owned {
        Some(record) => {
            current.is_some()
                && current.as_ref() != Some(&record.installed)
                && current != record.previous
        }
        None => current.is_some() && current.as_ref() != Some(&desired),
    };
    if conflict && !opts.force {
        bail!("existing content is customized or unmanaged; left unchanged (use --force to back it up and replace it)");
    }
    let previous = match owned {
        Some(record) if !conflict => record.previous.clone(),
        _ => current.clone(),
    };
    let mut owners = owned.map(|o| o.owners.clone()).unwrap_or_default();
    owners.insert(owner.to_owned());
    let status = if current.as_ref() == Some(&desired) {
        "already-current"
    } else if current.is_some() {
        "updated"
    } else {
        "installed"
    };
    Ok(Decision {
        after: Some(desired.clone()),
        owned: Some(Owned {
            owners,
            installed: desired,
            previous,
        }),
        status,
    })
}

struct Write {
    path: PathBuf,
    before: Option<Vec<u8>>,
    after: Option<Vec<u8>>,
}

fn dry_status(status: &'static str, dry: bool) -> &'static str {
    if !dry {
        return status;
    }
    match status {
        "installed" => "would-install",
        "updated" => "would-update",
        "removed" => "would-remove",
        "restored" => "would-restore",
        other => other,
    }
}

fn apply_agent(
    home: &Path,
    agent: &Agent,
    opts: &Options,
    state: &mut Manifest,
) -> Result<Vec<Action>> {
    storage::require_regular(home, &agent.config)?;
    let before = storage::read(&agent.config)?;
    let mut config = Config::parse(before.as_deref(), agent.toml)?;
    let key = format!("mcp:{}", agent.config.display());
    let current = config.entry()?;
    let desired = config.desired(&opts.bin)?;
    let mcp = decide(
        state.entries.get(&key),
        agent.name,
        current.clone(),
        desired,
        opts,
    )
    .with_context(|| format!("{} MCP entry", agent.config.display()))?;
    let mut planned = state.clone();
    match mcp.owned {
        Some(record) => {
            planned.entries.insert(key, record);
        }
        None => {
            planned.entries.remove(&key);
        }
    }
    let mut writes = Vec::new();
    if current != mcp.after {
        config.set(mcp.after.as_deref())?;
        writes.push(Write {
            path: agent.config.clone(),
            before,
            after: Some(config.render()?),
        });
    }
    let mut actions = vec![Action::new(
        agent,
        "mcp",
        &agent.config,
        dry_status(mcp.status, opts.dry_run),
        (mcp.status == "preserved")
            .then(|| "not owned by this installer or still used by another agent".into()),
    )];
    if !opts.no_skill {
        if storage::linked(home, &agent.skill) {
            actions.push(Action::new(
                agent,
                "skill",
                &agent.skill,
                "preserved",
                Some("symlinked; managed externally".into()),
            ));
        } else {
            storage::require_regular(home, &agent.skill)?;
            let before = storage::read(&agent.skill)?;
            let key = format!("skill:{}", agent.skill.display());
            let skill = decide(
                state.entries.get(&key),
                agent.name,
                before.clone(),
                SKILL.as_bytes().to_vec(),
                opts,
            )
            .with_context(|| format!("{} skill", agent.skill.display()))?;
            match skill.owned {
                Some(record) => {
                    planned.entries.insert(key, record);
                }
                None => {
                    planned.entries.remove(&key);
                }
            }
            if before != skill.after {
                writes.push(Write {
                    path: agent.skill.clone(),
                    before,
                    after: skill.after,
                });
            }
            actions.push(Action::new(
                agent,
                "skill",
                &agent.skill,
                dry_status(skill.status, opts.dry_run),
                (skill.status == "preserved")
                    .then(|| "not owned by this installer or still used by another agent".into()),
            ));
        }
    }
    if opts.dry_run {
        return Ok(actions);
    }
    if !opts.uninstall {
        storage::save(home, &planned)?;
        *state = planned.clone();
    }
    for write in writes {
        storage::replace(home, &write.path, &write.before, write.after.as_deref())?;
    }
    if opts.uninstall {
        storage::save(home, &planned)?;
        *state = planned;
    }
    Ok(actions)
}

/// Read-only diagnostics. Probe each distinct executable once; never rewrite
/// configuration or query repository contents.
pub fn doctor(home: &Path, only: Option<&str>, codex_home: Option<&Path>) -> Result<Vec<Action>> {
    validate_agent(only)?;
    let state = storage::load(home)?;
    let mut out = Vec::new();
    let mut checked = std::collections::BTreeMap::new();
    for agent in known_agents(home, codex_home)
        .into_iter()
        .filter(|a| only.is_none_or(|n| a.name == n))
    {
        if !agent.marker.exists()
            && !agent.config.exists()
            && only.is_none()
            && !state
                .entries
                .values()
                .any(|r| r.owners.contains(agent.name))
        {
            out.push(Action::new(
                &agent,
                "mcp",
                &agent.config,
                "skipped",
                Some("not detected".into()),
            ));
            continue;
        }
        let status = (|| -> Result<()> {
            let before = storage::read(&agent.config)?;
            let config = Config::parse(before.as_deref(), agent.toml)?;
            let command = config.command()?;
            let record = state
                .entries
                .get(&format!("mcp:{}", agent.config.display()));
            if let Some(record) = record {
                if config.entry()?.as_ref() != Some(&record.installed) {
                    bail!("registration differs from the ownership journal");
                }
            }
            let result = checked
                .entry(command.clone())
                .or_insert_with(|| health::probe(&command).map_err(|e| format!("{e:#}")));
            if let Err(e) = result {
                bail!("{e}");
            }
            Ok(())
        })();
        out.push(Action::new(
            &agent,
            "mcp",
            &agent.config,
            if status.is_ok() { "healthy" } else { "failed" },
            status.err().map(|e| format!("{e:#}")),
        ));
        let status = storage::read(&agent.skill).and_then(|bytes| {
            let bytes = bytes.context("skill is missing; rerun codeq install")?;
            if bytes != SKILL.as_bytes() { bail!("skill differs from this binary's bundled guide; rerun install or update the external copy"); }
            Ok(())
        });
        out.push(Action::new(
            &agent,
            "skill",
            &agent.skill,
            if status.is_ok() { "healthy" } else { "failed" },
            status.err().map(|e| format!("{e:#}")),
        ));
    }
    Ok(out)
}

pub fn verify_binary(bin: &str) -> Result<()> {
    health::probe(bin)
}
