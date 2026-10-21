//! Repository discovery and Git facts.
//!
//! Git is treated as an optimization, not a hard requirement: we still index a
//! plain directory tree, we just can't answer `changed --compare-to <ref>` or
//! report a HEAD/dirty state without it.

use std::collections::HashMap;
use std::io::{BufRead, BufReader, Read, Write};
use std::path::{Path, PathBuf};
use std::process::{Child, ChildStdin, ChildStdout, Command, Stdio};

use anyhow::{anyhow, Result};

pub struct Repo {
    /// Absolute repository root (the Git toplevel, or CWD for non-Git trees).
    pub root: PathBuf,
    pub is_git: bool,
    /// The root was named explicitly with `--root`, so the walk must not consult
    /// a parent repository's ignore files.
    pub pinned: bool,
}

pub struct Changes {
    pub base: String,
    pub paths: Vec<String>,
}

/// One request at a time keeps memory bounded by the largest historical file.
pub struct GitBlobs {
    child: Child,
    input: ChildStdin,
    output: BufReader<ChildStdout>,
    base: String,
}

impl GitBlobs {
    pub fn open(repo: &Repo, base: String) -> Result<Self> {
        let mut child = Command::new("git")
            .arg("-C")
            .arg(&repo.root)
            .args(["cat-file", "--batch", "-z"])
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::null())
            .spawn()?;
        let input = child.stdin.take().unwrap();
        let output = BufReader::new(child.stdout.take().unwrap());
        Ok(Self {
            child,
            input,
            output,
            base,
        })
    }

    pub fn read(&mut self, path: &str) -> Result<Option<Vec<u8>>> {
        let request = format!("{}:{path}", self.base);
        self.input.write_all(request.as_bytes())?;
        self.input.write_all(&[0])?;
        self.input.flush()?;
        let mut header = Vec::new();
        self.output.read_until(b'\n', &mut header)?;
        // With -z only requests are NUL-delimited. Missing responses echo the
        // request verbatim, including any newlines in its filename.
        let missing = format!("{request} missing\n").into_bytes();
        if !header.is_empty() && missing.starts_with(&header) {
            let read = header.len();
            header.resize(missing.len(), 0);
            self.output.read_exact(&mut header[read..])?;
            anyhow::ensure!(header == missing, "invalid git cat-file missing response");
            return Ok(None);
        }
        let fields: Vec<_> = std::str::from_utf8(&header)?.split_whitespace().collect();
        anyhow::ensure!(fields.len() == 3, "invalid git cat-file header");
        let size: usize = fields[2].parse()?;
        let mut bytes = vec![0; size];
        self.output.read_exact(&mut bytes)?;
        let mut newline = [0];
        self.output.read_exact(&mut newline)?;
        anyhow::ensure!(newline == [b'\n'], "invalid git cat-file terminator");
        Ok((fields[1] == "blob").then_some(bytes))
    }
}

impl Drop for GitBlobs {
    fn drop(&mut self) {
        // Also reap the process on early returns, including extraction errors.
        let _ = self.child.kill();
        let _ = self.child.wait();
    }
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

    /// Changed paths and the resolved comparison base, shared with blob reads.
    pub fn changed_since(&self, reference: &str) -> Result<Changes> {
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
        Ok(Changes { base, paths })
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

/// The Git toplevel containing `start`.
///
/// Found by walking up for a `.git` entry rather than by asking git: discovery
/// runs on every query, and `git rev-parse` was a process spawn costing about
/// as much as a warm lookup does. Anything the walk cannot answer the way git
/// would is still handed to git.
fn git_toplevel(start: &Path) -> Option<PathBuf> {
    native_toplevel(start).unwrap_or_else(|| git_toplevel_via_git(start))
}

/// `Some(answer)` when the walk is sure of it, `None` to defer to git.
#[cfg(unix)]
fn native_toplevel(start: &Path) -> Option<Option<PathBuf>> {
    use std::os::unix::fs::MetadataExt;

    // Each of these moves or bounds discovery in a way only git implements.
    const OVERRIDES: [&str; 4] = [
        "GIT_DIR",
        "GIT_WORK_TREE",
        "GIT_CEILING_DIRECTORIES",
        "GIT_DISCOVERY_ACROSS_FILESYSTEM",
    ];
    if OVERRIDES.iter().any(|v| std::env::var_os(v).is_some()) {
        return None;
    }
    let start = start.canonicalize().ok()?;
    // Inside a `.git` directory there is no work tree, and git says so.
    if start.components().any(|c| c.as_os_str() == ".git") {
        return None;
    }
    let device = std::fs::metadata(&start).ok()?.dev();
    for dir in start.ancestors() {
        // Git does not cross a filesystem boundary looking for a repository.
        if std::fs::metadata(dir).ok()?.dev() != device {
            return Some(None);
        }
        let dot = dir.join(".git");
        let Ok(meta) = std::fs::metadata(&dot) else {
            continue;
        };
        // A repository's `.git` is a directory holding `HEAD`, or (a linked
        // worktree, a submodule) a file naming the real one. Anything else
        // called `.git` is for git to judge.
        let is_repo = if meta.is_dir() {
            dot.join("HEAD").is_file()
        } else {
            std::fs::read(&dot).is_ok_and(|body| body.starts_with(b"gitdir:"))
        };
        return is_repo.then(|| Some(dir.to_path_buf()));
    }
    Some(None)
}

#[cfg(not(unix))]
fn native_toplevel(_start: &Path) -> Option<Option<PathBuf>> {
    None
}

fn git_toplevel_via_git(start: &Path) -> Option<PathBuf> {
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
    fn historical_blobs_preserve_bytes_and_missing_response_alignment() {
        let dir = tempfile::tempdir().unwrap();
        let git = |args: &[&str]| {
            let out = Command::new("git")
                .arg("-C")
                .arg(dir.path())
                .args([
                    "-c",
                    "user.name=test",
                    "-c",
                    "user.email=test@example.com",
                    "-c",
                    "commit.gpgsign=false",
                ])
                .args(args)
                .output()
                .unwrap();
            assert!(
                out.status.success(),
                "{}",
                String::from_utf8_lossy(&out.stderr)
            );
        };
        git(&["init", "-q"]);
        #[cfg(not(windows))]
        let name = " leading\nline\t:ü.rs";
        #[cfg(windows)]
        let name = " leading ü.rs";
        let bytes = [
            b"pub fn old() {}\n\0\xff".as_slice(),
            &vec![b'x'; 256 * 1024],
        ]
        .concat();
        std::fs::write(dir.path().join(name), &bytes).unwrap();
        git(&["add", "-A"]);
        git(&["commit", "-qm", "initial"]);
        let repo = Repo::at(dir.path(), false).unwrap();
        let mut blobs = GitBlobs::open(&repo, "HEAD".into()).unwrap();
        assert_eq!(blobs.read("absent\n\t:ü.rs").unwrap(), None);
        assert_eq!(blobs.read(name).unwrap(), Some(bytes.clone()));
        assert_eq!(blobs.read("missing.rs").unwrap(), None);
        assert_eq!(blobs.read(name).unwrap(), Some(bytes));
    }

    #[test]
    fn nul_separated_keeps_leading_spaces() {
        let names: Vec<&str> = nul_separated(" lead.rs\0b.rs\0").collect();
        assert_eq!(names, vec![" lead.rs", "b.rs"]);
    }
}
