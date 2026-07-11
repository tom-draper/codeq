//! Repository discovery and Git facts.
//!
//! Git is treated as an optimization, not a hard requirement: we still index a
//! plain directory tree, we just can't answer `changed --compare-to <ref>` or
//! report a HEAD/dirty state without it.

use std::path::{Path, PathBuf};
use std::process::Command;

use anyhow::{anyhow, Result};

pub struct Repo {
    /// Absolute repository root (the Git toplevel, or CWD for non-Git trees).
    pub root: PathBuf,
    pub is_git: bool,
}

impl Repo {
    /// Discover the repo containing `start`. Prefers the Git toplevel; falls
    /// back to `start` itself when there is no Git repository.
    pub fn discover(start: &Path) -> Result<Repo> {
        if let Some(root) = git_toplevel(start) {
            return Ok(Repo { root, is_git: true });
        }
        let root = start
            .canonicalize()
            .map_err(|e| anyhow!("cannot resolve {}: {e}", start.display()))?;
        Ok(Repo { root, is_git: false })
    }

    pub fn vcs(&self) -> &'static str {
        if self.is_git {
            "git"
        } else {
            "none"
        }
    }

    /// Short HEAD commit, if this is a Git repo with at least one commit.
    pub fn head(&self) -> Option<String> {
        if !self.is_git {
            return None;
        }
        let out = git(&self.root, &["rev-parse", "--short", "HEAD"]).ok()?;
        let s = out.trim();
        if s.is_empty() {
            None
        } else {
            Some(s.to_string())
        }
    }

    /// Whether the working tree has uncommitted changes.
    pub fn dirty(&self) -> Option<bool> {
        if !self.is_git {
            return None;
        }
        let out = git(&self.root, &["status", "--porcelain"]).ok()?;
        Some(!out.trim().is_empty())
    }

    /// Paths changed relative to `reference` (a ref, or the working tree vs a
    /// ref). Returns repo-relative paths.
    pub fn changed_since(&self, reference: &str) -> Result<Vec<String>> {
        if !self.is_git {
            return Err(anyhow!("`changed` requires a Git repository"));
        }
        // Diff the working tree against the merge-base with `reference` so this
        // matches "what have I changed on this branch" rather than raw ref diff.
        let base = git(&self.root, &["merge-base", reference, "HEAD"])
            .map(|s| s.trim().to_string())
            .unwrap_or_else(|_| reference.to_string());

        let mut paths = Vec::new();
        // Committed changes since the base.
        let committed = git(&self.root, &["diff", "--name-only", &base])?;
        for line in committed.lines() {
            let p = line.trim();
            if !p.is_empty() {
                paths.push(p.to_string());
            }
        }
        // Untracked files aren't in the diff; include them so agents see new files.
        let untracked = git(
            &self.root,
            &["ls-files", "--others", "--exclude-standard"],
        )?;
        for line in untracked.lines() {
            let p = line.trim();
            if !p.is_empty() {
                paths.push(p.to_string());
            }
        }
        paths.sort();
        paths.dedup();
        Ok(paths)
    }
}

fn git_toplevel(start: &Path) -> Option<PathBuf> {
    let out = git(start, &["rev-parse", "--show-toplevel"]).ok()?;
    let top = out.trim();
    if top.is_empty() {
        None
    } else {
        Some(PathBuf::from(top))
    }
}

/// Run `git` in `dir` and return stdout on success.
fn git(dir: &Path, args: &[&str]) -> Result<String> {
    let out = Command::new("git")
        .arg("-C")
        .arg(dir)
        .args(args)
        .output()
        .map_err(|e| anyhow!("failed to run git: {e}"))?;
    if !out.status.success() {
        return Err(anyhow!(
            "git {} failed: {}",
            args.join(" "),
            String::from_utf8_lossy(&out.stderr).trim()
        ));
    }
    Ok(String::from_utf8_lossy(&out.stdout).into_owned())
}
