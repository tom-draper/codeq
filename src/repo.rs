//! Repository discovery and Git facts.
//!
//! Git is treated as an optimization, not a hard requirement: we still index a
//! plain directory tree, we just can't answer `changed --compare-to <ref>` or
//! report a HEAD/dirty state without it.

use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::process::Command;
use std::time::UNIX_EPOCH;

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

    /// A cheap fingerprint of the working-tree state, for git repos: HEAD plus
    /// the porcelain status plus untracked-file stats. If two fingerprints are
    /// equal the tree is byte-identical, so the index can be trusted without a
    /// full walk. Covers commits and checkouts (HEAD), staged/unstaged edits and
    /// untracked add/remove (status), and in-place edits to untracked files
    /// (their stats). Returns `None` for non-git repos, which have no cheap
    /// signal — the caller must then walk.
    pub fn freshness_token(&self) -> Result<Option<String>> {
        if !self.is_git {
            return Ok(None);
        }
        let head = git(&self.root, &["rev-parse", "HEAD"]).unwrap_or_default();
        let status = git(
            &self.root,
            &["status", "--porcelain=v1", "-z", "--untracked-files=all"],
        )?;

        let mut h = blake3::Hasher::new();
        h.update(head.trim().as_bytes());
        h.update(&[0]);
        h.update(status.as_bytes());
        // Status only lists untracked files by path; include their stats so an
        // in-place edit to one changes the fingerprint.
        for entry in status.split('\0') {
            if let Some(path) = entry.strip_prefix("?? ") {
                if let Ok(meta) = std::fs::metadata(self.root.join(path)) {
                    h.update(path.as_bytes());
                    h.update(&meta.len().to_le_bytes());
                    if let Ok(mt) = meta.modified() {
                        if let Ok(d) = mt.duration_since(UNIX_EPOCH) {
                            h.update(&(d.as_nanos() as u64).to_le_bytes());
                        }
                    }
                }
            }
        }
        Ok(Some(h.finalize().to_hex().to_string()))
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

    /// New-side changed line ranges per file, relative to `reference` (merge-base
    /// with HEAD). Untracked files map to the whole file `(1, usize::MAX)`.
    /// Used to find which definitions a change actually touched.
    pub fn changed_line_ranges(
        &self,
        reference: &str,
    ) -> Result<HashMap<String, Vec<(usize, usize)>>> {
        if !self.is_git {
            return Err(anyhow!("`changed --symbols` requires a Git repository"));
        }
        let base = git(&self.root, &["merge-base", reference, "HEAD"])
            .map(|s| s.trim().to_string())
            .unwrap_or_else(|_| reference.to_string());

        let mut map: HashMap<String, Vec<(usize, usize)>> = HashMap::new();

        // `-U0` gives hunk headers with no context, so each @@ range is exactly
        // the changed new-side lines.
        let diff = git(&self.root, &["diff", "-U0", "--no-color", &base])?;
        let mut current: Option<String> = None;
        for line in diff.lines() {
            if let Some(rest) = line.strip_prefix("+++ ") {
                current = if rest == "/dev/null" {
                    None
                } else {
                    Some(rest.strip_prefix("b/").unwrap_or(rest).to_string())
                };
            } else if line.starts_with("@@ ") {
                if let (Some(path), Some((start, count))) = (&current, parse_new_hunk(line)) {
                    if count > 0 {
                        map.entry(path.clone())
                            .or_default()
                            .push((start, start + count - 1));
                    }
                }
            }
        }

        // Untracked files: treat the whole file as changed.
        let untracked = git(&self.root, &["ls-files", "--others", "--exclude-standard"])?;
        for line in untracked.lines() {
            let p = line.trim();
            if !p.is_empty() {
                map.insert(p.to_string(), vec![(1, usize::MAX)]);
            }
        }

        Ok(map)
    }
}

/// Parse the new-side `+c,d` field of a `@@ -a,b +c,d @@` hunk header into
/// `(start_line, line_count)`. A missing count means 1.
fn parse_new_hunk(line: &str) -> Option<(usize, usize)> {
    let plus = line.split_whitespace().find(|t| t.starts_with('+'))?;
    let spec = &plus[1..];
    let (start, count) = match spec.split_once(',') {
        Some((a, b)) => (a.parse().ok()?, b.parse().ok()?),
        None => (spec.parse().ok()?, 1),
    };
    Some((start, count))
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
