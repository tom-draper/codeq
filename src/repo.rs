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
        // Keep both sides of renames. Consumers such as `api --compare-to` need
        // to see a removed module path as well as the destination path; with
        // rename detection enabled `--name-only` reports only the destination.
        let committed = git(
            &self.root,
            &["diff", "--name-only", "-z", "--no-renames", &base],
        )?;
        paths.extend(nul_separated(&committed).map(str::to_string));
        // Untracked files aren't in the diff; include them so agents see new files.
        let untracked = git(
            &self.root,
            &["ls-files", "--others", "--exclude-standard", "-z"],
        )?;
        paths.extend(nul_separated(&untracked).map(str::to_string));
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
        // the changed new-side lines. A pure deletion has no new-side lines;
        // anchor that zero-width change to the preceding line (or line 1 at the
        // start of a file) so the definition containing the deletion is touched.
        // The prefixes are spelled out because `diff.noprefix` and
        // `diff.mnemonicPrefix` in a user's config change what a `+++` line
        // starts with.
        let diff = git(
            &self.root,
            &[
                "diff",
                "-U0",
                "--no-color",
                "--no-ext-diff",
                "--no-renames",
                "--src-prefix=a/",
                "--dst-prefix=b/",
                &base,
            ],
        )?;
        let mut current: Option<String> = None;
        for line in diff.lines() {
            if line.starts_with("diff --git ") {
                // A file with no `+++` line (binary, mode-only) must not inherit
                // the previous file's path for its hunks.
                current = None;
            } else if let Some(rest) = line.strip_prefix("+++ ") {
                current = new_side_path(rest);
            } else if line.starts_with("@@ ") {
                if let (Some(path), Some((start, count))) = (&current, parse_new_hunk(line)) {
                    let range = if count > 0 {
                        (start, start + count - 1)
                    } else {
                        let anchor = start.max(1);
                        (anchor, anchor)
                    };
                    map.entry(path.clone()).or_default().push(range);
                }
            }
        }

        // Untracked files: treat the whole file as changed.
        let untracked = git(
            &self.root,
            &["ls-files", "--others", "--exclude-standard", "-z"],
        )?;
        for p in nul_separated(&untracked) {
            map.insert(p.to_string(), vec![(1, usize::MAX)]);
        }

        Ok(map)
    }
}

/// The names in `git ... -z` output. Without `-z` git quotes any path holding a
/// non-ASCII byte, a quote or a control character (`"\303\274.rs"`), and the
/// quoted text names no file; trimming it would also eat a leading space.
fn nul_separated(out: &str) -> impl Iterator<Item = &str> {
    out.split('\0').filter(|p| !p.is_empty())
}

/// The path on the `+++ ` line of a patch, without git's decoration: a trailing
/// TAB after a name that holds a space, C-style quoting of unusual names, and
/// the `b/` prefix. `None` for `/dev/null` (a deleted file has no new side).
fn new_side_path(rest: &str) -> Option<String> {
    let rest = rest.strip_suffix('\t').unwrap_or(rest);
    let path = unquote_c_style(rest);
    if path == "/dev/null" {
        return None;
    }
    Some(path.strip_prefix("b/").unwrap_or(&path).to_string())
}

/// Undo git's C-style quoting (`"a\"b\303\274"`); an unquoted name is returned as is.
fn unquote_c_style(raw: &str) -> String {
    let Some(inner) = raw.strip_prefix('"').and_then(|r| r.strip_suffix('"')) else {
        return raw.to_string();
    };
    let b = inner.as_bytes();
    let mut out: Vec<u8> = Vec::with_capacity(b.len());
    let mut i = 0;
    while i < b.len() {
        if b[i] != b'\\' || i + 1 == b.len() {
            out.push(b[i]);
            i += 1;
            continue;
        }
        i += 1;
        match b[i] {
            b'a' => out.push(7),
            b'b' => out.push(8),
            b'f' => out.push(12),
            b'n' => out.push(b'\n'),
            b'r' => out.push(b'\r'),
            b't' => out.push(b'\t'),
            b'v' => out.push(11),
            b'0'..=b'7' => {
                // Up to three octal digits: one raw byte of a UTF-8 sequence.
                let mut value = 0u32;
                let mut digits = 0;
                while digits < 3 && i < b.len() && (b'0'..=b'7').contains(&b[i]) {
                    value = value * 8 + u32::from(b[i] - b'0');
                    i += 1;
                    digits += 1;
                }
                out.push(value as u8);
                continue;
            }
            other => out.push(other),
        }
        i += 1;
    }
    String::from_utf8_lossy(&out).into_owned()
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

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn unquotes_git_c_style_paths() {
        assert_eq!(unquote_c_style("plain.rs"), "plain.rs");
        assert_eq!(unquote_c_style(r#""q\"uote.rs""#), "q\"uote.rs");
        assert_eq!(unquote_c_style(r#""\303\274n\303\257.rs""#), "ünï.rs");
        assert_eq!(unquote_c_style(r#""a\tb\\c""#), "a\tb\\c");
        // Digits after a three-digit octal escape are literal text.
        assert_eq!(unquote_c_style(r#""\3031""#), "\u{FFFD}1");
    }

    #[test]
    fn new_side_path_strips_git_decoration() {
        assert_eq!(new_side_path("b/src/a.rs"), Some("src/a.rs".into()));
        // A trailing TAB follows any name that contains a space.
        assert_eq!(new_side_path("b/sp ace.rs\t"), Some("sp ace.rs".into()));
        assert_eq!(
            new_side_path(r#""b/\303\274.rs""#),
            Some("ü.rs".to_string())
        );
        assert_eq!(new_side_path("/dev/null"), None);
    }

    #[test]
    fn nul_separated_keeps_leading_spaces() {
        let names: Vec<&str> = nul_separated(" lead.rs\0b.rs\0").collect();
        assert_eq!(names, vec![" lead.rs", "b.rs"]);
    }
}
