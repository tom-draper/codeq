//! Repository discovery and Git facts.
//!
//! Git is treated as an optimization, not a hard requirement: we still index a
//! plain directory tree, we just can't answer `changed --compare-to <ref>` or
//! report a HEAD/dirty state without it.

use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::process::Command;

use anyhow::{anyhow, Result};

pub struct Repo {
    /// Absolute repository root (the Git toplevel, or CWD for non-Git trees).
    pub root: PathBuf,
    pub is_git: bool,
    /// The root was named explicitly with `--root`, so the walk must not consult
    /// a parent repository's ignore files.
    pub pinned: bool,
}

impl Repo {
    /// Discover the repo containing `start` — preferring the Git toplevel and
    /// falling back to `start` itself — or, when `pinned`, treat `start` as the
    /// root without walking up at all.
    ///
    /// Pinning is what makes a dependency readable: `node_modules/lodash` sits
    /// inside the outer repo and is ignored by it, so discovery would otherwise
    /// resolve to the parent and the walk would skip every file.
    pub fn at(start: &Path, pinned: bool) -> Result<Repo> {
        if pinned {
            let root = start
                .canonicalize()
                .map_err(|e| anyhow!("cannot resolve {}: {e}", start.display()))?;
            let is_git = git_toplevel(&root).is_some_and(|t| t == root);
            return Ok(Repo {
                root,
                is_git,
                pinned: true,
            });
        }
        if let Some(root) = git_toplevel(start) {
            return Ok(Repo {
                root,
                is_git: true,
                pinned: false,
            });
        }
        let root = start
            .canonicalize()
            .map_err(|e| anyhow!("cannot resolve {}: {e}", start.display()))?;
        Ok(Repo {
            root,
            is_git: false,
            pinned: false,
        })
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

    // Freshness no longer shells out to git: `index::refresh` maintains codeq's
    // own index cache (per-file size/mtime + per-directory mtime) and checks it
    // natively — git-free, and faster on a clean tree than spawning git.

    /// Paths changed relative to `reference` (a ref, or the working tree vs a
    /// ref). Returns repo-relative paths.
    /// The contents of `path` as of `reference`, or `None` when the file did not
    /// exist there (which is how an added file is recognised).
    pub fn file_at(&self, reference: &str, path: &str) -> Option<Vec<u8>> {
        let base = git(&self.root, &["merge-base", reference, "HEAD"])
            .map(|s| s.trim().to_string())
            .unwrap_or_else(|_| reference.to_string());
        let out = std::process::Command::new("git")
            .arg("-C")
            .arg(&self.root)
            .args(["show", &format!("{base}:{path}")])
            .output()
            .ok()?;
        out.status.success().then_some(out.stdout)
    }

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
        let untracked = git(&self.root, &["ls-files", "--others", "--exclude-standard"])?;
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
        let stderr = String::from_utf8_lossy(&out.stderr);
        // An unresolvable ref is the one git failure a caller routinely causes,
        // and git's own wording for it points the wrong way: it advises using
        // `--` to disambiguate paths from revisions, which is not the problem
        // when the ref simply doesn't exist. `--compare-to` defaults to `main`,
        // so this is what a repo whose trunk is `master` hits on the very first
        // `changed` or `affected`. Say what actually went wrong.
        if let Some(rev) = stderr
            .lines()
            .find_map(|l| l.strip_prefix("fatal: ambiguous argument '"))
            .and_then(|l| l.split('\'').next())
        {
            // The trunk hint only helps when the ref came from the default; when
            // the caller named one explicitly, it is just noise.
            let hint = if rev == "main" {
                " — this is the --compare-to default, so pass the ref this repo \
                  actually uses as its trunk"
            } else {
                " — pass a --compare-to ref that exists in this repo"
            };
            return Err(anyhow!("no such git ref: '{rev}'{hint}"));
        }
        return Err(anyhow!("git {} failed: {}", args.join(" "), stderr.trim()));
    }
    Ok(String::from_utf8_lossy(&out.stdout).into_owned())
}
