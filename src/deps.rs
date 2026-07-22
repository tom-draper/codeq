//! Module-dependency graph — resolve raw import strings to internal files.
//!
//! `imports`/`importers` report the *raw* module string as written in source
//! (`crate::db`, `./foo/bar`, `foo.bar.baz`). This module turns those strings
//! into file→file edges so you can ask "what does this file depend on,
//! transitively" and "are there import cycles".
//!
//! Resolution is deterministic but heuristic — it does not run the language's
//! real module resolver. An import resolves to an internal file by matching the
//! import's path *segments* against repository file stems, preferring the file
//! whose path shares the most segments (so `foo.bar.util` picks `foo/bar/util`
//! over an unrelated `util`). Segments a real resolver would treat specially
//! (`crate`, `self`, `super`, `mod`) are dropped. An import that matches no
//! repo file is *external* (a stdlib or third-party dependency) and carries no
//! edge. Ambiguity that scoring can't break is reported as multiple edges — a
//! fact, not a guess.

use std::collections::{HashMap, HashSet};
use std::path::Path;

/// Segments of a module string that never name a file — language roots and
/// relative markers. Dropped before matching so `crate::db` matches `db`.
const DROP_SEGMENTS: &[&str] = &["crate", "self", "super", "mod"];

/// A resolved internal dependency edge.
pub struct Edge {
    /// The depended-upon file.
    pub to: String,
    /// The raw import string that produced this edge (for verification).
    pub module: String,
}

/// One reached file in a transitive walk.
pub struct DepHop {
    pub path: String,
    /// The import string on the edge that first reached it.
    pub module: String,
    /// Hops from the start file (1 = direct).
    pub depth: usize,
}

/// The internal import graph: file -> the files it depends on.
pub struct Graph {
    edges: HashMap<String, Vec<Edge>>,
}

/// Split a module string into candidate path segments. Anything that is not an
/// identifier char is a separator, so `crate::context::{self, Request}` and
/// `foo.bar/baz` both tokenize cleanly; drop language roots and relative markers.
fn segments(module: &str) -> Vec<&str> {
    module
        .split(|c: char| !(c.is_ascii_alphanumeric() || c == '_'))
        .filter(|s| !s.is_empty() && !DROP_SEGMENTS.contains(s))
        .collect()
}

/// The stem of a file path — its base name without extension (`src/db.rs` -> `db`).
fn stem(path: &str) -> &str {
    Path::new(path)
        .file_stem()
        .and_then(|s| s.to_str())
        .unwrap_or(path)
}

/// The path components of a file, without extension on the final one, as a set
/// for segment scoring (`src/foo/bar.rs` -> {src, foo, bar}).
fn path_components(path: &str) -> HashSet<&str> {
    let mut set: HashSet<&str> = path.split(['/', '\\']).filter(|s| !s.is_empty()).collect();
    // Replace the file component with its stem so `bar.rs` scores against `bar`.
    let file = path.rsplit(['/', '\\']).next().unwrap_or(path);
    set.remove(file);
    set.insert(stem(path));
    set
}

impl Graph {
    /// Build the internal graph from all repo `files` and all import facts
    /// `(importing_file, raw_module)`.
    pub fn build<F, I>(files: F, imports: I) -> Graph
    where
        F: IntoIterator<Item = String>,
        I: IntoIterator<Item = (String, String)>,
    {
        // stem -> files with that stem (usually one; more when names collide).
        let paths: Vec<String> = files.into_iter().collect();
        let mut by_stem: HashMap<&str, Vec<&str>> = HashMap::new();
        for p in &paths {
            by_stem.entry(stem(p)).or_default().push(p.as_str());
        }
        // Precompute each path's component set for scoring.
        let comps: HashMap<&str, HashSet<&str>> =
            paths.iter().map(|p| (p.as_str(), path_components(p))).collect();

        let mut edges: HashMap<String, Vec<Edge>> = HashMap::new();
        // Every importing file gets an entry (even if all imports are external),
        // so a file with only external deps is still a known node.
        for p in &paths {
            edges.entry(p.clone()).or_default();
        }
        for (from, module) in imports {
            let segs = segments(&module);
            if segs.is_empty() {
                continue;
            }
            // Candidate targets: any file whose stem is one of the segments.
            let mut best_score = 0usize;
            let mut best: Vec<&str> = Vec::new();
            let seg_set: HashSet<&str> = segs.iter().copied().collect();
            for seg in &seg_set {
                for &cand in by_stem.get(seg).map(|v| v.as_slice()).unwrap_or(&[]) {
                    if cand == from {
                        continue; // no self-edge
                    }
                    // Score = how many of this file's components the import names.
                    let score = comps[cand].iter().filter(|c| seg_set.contains(**c)).count();
                    if score > best_score {
                        best_score = score;
                        best.clear();
                        best.push(cand);
                    } else if score == best_score {
                        best.push(cand);
                    }
                }
            }
            if best_score == 0 {
                continue; // external, or only self
            }
            best.sort_unstable();
            best.dedup();
            let bucket = edges.entry(from.clone()).or_default();
            for cand in best {
                bucket.push(Edge { to: cand.to_string(), module: module.clone() });
            }
        }
        // Deterministic edge order per node.
        for bucket in edges.values_mut() {
            bucket.sort_by(|a, b| a.to.cmp(&b.to).then(a.module.cmp(&b.module)));
            bucket.dedup_by(|a, b| a.to == b.to && a.module == b.module);
        }
        Graph { edges }
    }

    /// Whether `file` is a known node in the graph.
    /// Whether `module`, imported by `from`, resolved to an internal file.
    ///
    /// False means the import points outside the repo: a third-party package for
    /// code, or — when the module is a relative path — a file reference that
    /// does not exist.
    pub fn resolves(&self, from: &str, module: &str) -> bool {
        self.edges
            .get(from)
            .is_some_and(|es| es.iter().any(|e| e.module == module))
    }

    pub fn contains(&self, file: &str) -> bool {
        self.edges.contains_key(file)
    }

    /// Every internal file transitively reachable from `start` via imports
    /// (excluding `start` itself). Used to resolve which definition a reference
    /// can actually see.
    pub fn reachable_from<'a>(&'a self, start: &str) -> HashSet<&'a str> {
        let mut seen: HashSet<&'a str> = HashSet::new();
        let mut stack: Vec<&'a str> = vec![];
        // Seed with `start`'s direct targets (start itself is excluded).
        if let Some(bucket) = self.edges.get(start) {
            for e in bucket {
                if seen.insert(e.to.as_str()) {
                    stack.push(e.to.as_str());
                }
            }
        }
        while let Some(f) = stack.pop() {
            if let Some(bucket) = self.edges.get(f) {
                for e in bucket {
                    if seen.insert(e.to.as_str()) {
                        stack.push(e.to.as_str());
                    }
                }
            }
        }
        seen
    }

    /// Transitive dependencies of `file` up to `depth` hops (BFS), each reached
    /// file reported once at its shortest depth with the linking import.
    pub fn deps(&self, file: &str, depth: usize) -> Vec<DepHop> {
        let mut seen: HashSet<&str> = HashSet::from([file]);
        let mut frontier: Vec<&str> = vec![file];
        let mut out: Vec<DepHop> = Vec::new();
        for d in 1..=depth {
            let mut next: Vec<&str> = Vec::new();
            for f in &frontier {
                if let Some(bucket) = self.edges.get(*f) {
                    for e in bucket {
                        if seen.insert(e.to.as_str()) {
                            out.push(DepHop { path: e.to.clone(), module: e.module.clone(), depth: d });
                            next.push(e.to.as_str());
                        }
                    }
                }
            }
            frontier = next;
            if frontier.is_empty() {
                break;
            }
        }
        out.sort_by(|a, b| a.depth.cmp(&b.depth).then(a.path.cmp(&b.path)));
        out
    }

    /// Every import cycle, as a list of files in dependency order (the last
    /// depends back on the first). Deterministic: nodes visited in sorted order,
    /// each cycle reported once, canonicalized to start at its lexically-least
    /// member.
    pub fn cycles(&self) -> Vec<Vec<String>> {
        let mut nodes: Vec<&str> = self.edges.keys().map(|s| s.as_str()).collect();
        nodes.sort_unstable();

        let mut found: HashSet<Vec<String>> = HashSet::new();
        let mut result: Vec<Vec<String>> = Vec::new();
        // Iterative DFS tracking the current path; a back-edge into the path is a cycle.
        for &start in &nodes {
            let mut stack: Vec<(&str, usize)> = vec![(start, 0)];
            let mut path: Vec<&str> = Vec::new();
            let mut on_path: HashSet<&str> = HashSet::new();
            while let Some(&mut (node, ref mut idx)) = stack.last_mut() {
                if *idx == 0 {
                    path.push(node);
                    on_path.insert(node);
                }
                let targets = self.edges.get(node);
                let deg = targets.map(|v| v.len()).unwrap_or(0);
                if *idx < deg {
                    let to = targets.unwrap()[*idx].to.as_str();
                    *idx += 1;
                    // Only explore forward from `start` to avoid re-finding cycles
                    // rooted at already-processed nodes.
                    if to < start {
                        continue;
                    }
                    if let Some(pos) = path.iter().position(|&p| p == to) {
                        let cyc: Vec<String> = path[pos..].iter().map(|s| s.to_string()).collect();
                        if let Some(c) = canonical_cycle(cyc) {
                            if found.insert(c.clone()) {
                                result.push(c);
                            }
                        }
                    } else if !on_path.contains(to) {
                        stack.push((to, 0));
                    }
                } else {
                    on_path.remove(node);
                    path.pop();
                    stack.pop();
                }
            }
        }
        result.sort();
        result
    }
}

/// Rotate a cycle to start at its lexically-least member so the same cycle
/// discovered from different entry points collapses to one representation.
fn canonical_cycle(cyc: Vec<String>) -> Option<Vec<String>> {
    if cyc.is_empty() {
        return None;
    }
    let min = cyc.iter().enumerate().min_by(|a, b| a.1.cmp(b.1)).map(|(i, _)| i)?;
    let mut out = Vec::with_capacity(cyc.len());
    out.extend_from_slice(&cyc[min..]);
    out.extend_from_slice(&cyc[..min]);
    Some(out)
}
