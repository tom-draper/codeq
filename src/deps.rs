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
//! (`crate`, `self`, `super`) are dropped. An import that matches no
//! repo file is *external* (a stdlib or third-party dependency) and carries no
//! edge. Ambiguity that scoring can't break is reported as multiple edges — a
//! fact, not a guess.

use std::collections::{HashMap, HashSet};
use std::path::Path;

/// Segments of a module string that never name a file — language roots and
/// relative markers. Dropped before matching so `crate::db` matches `db`.
const DROP_SEGMENTS: &[&str] = &["crate", "self", "super"];

fn is_file_reference(module: &str) -> bool {
    module.rsplit_once('.').is_some_and(|(_, ext)| {
        crate::index::EXTENSIONS
            .iter()
            .any(|(known, _)| known.eq_ignore_ascii_case(ext))
            // Documentation commonly links assets which remain intentionally
            // unparsed, but are still real files in the dependency graph.
            || matches!(ext.to_ascii_lowercase().as_str(), "svg" | "png" | "jpg" | "jpeg" | "gif" | "webp" | "ico")
    })
}

fn is_jinja_template(path: &str) -> bool {
    matches!(
        path.rsplit_once('.').map(|(_, ext)| ext.to_ascii_lowercase()),
        Some(ext) if matches!(ext.as_str(), "html" | "jinja" | "jinja2" | "j2")
    )
}

/// Node treats every non-relative module specifier as a package (or a built-in).
/// Project aliases need bundler configuration that codeq deliberately does not
/// read, so they remain external rather than borrowing a same-named local file.
fn uses_node_resolution(path: &str) -> bool {
    matches!(
        path.rsplit_once('.').map(|(_, ext)| ext.to_ascii_lowercase()),
        Some(ext) if matches!(ext.as_str(), "js" | "cjs" | "mjs" | "jsx" | "ts" | "cts" | "mts" | "tsx" | "svelte" | "vue")
    )
}

/// A resolved internal dependency edge.
pub struct Edge {
    /// The depended-upon file.
    pub to: String,
    /// The raw import string that produced this edge (for verification).
    pub module: String,
    /// The import spelling structurally matches this file. Heuristic edges are
    /// still useful for broad dependency exploration, but must not manufacture
    /// a high-confidence architectural claim such as an import cycle.
    exact: bool,
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
    /// Local import binding -> resolved direct files. Kept separately from the
    /// raw dependency edge because `make as m` must be resolved by `m()` while
    /// `imports` continues to truthfully report the source spelling `make`.
    bindings: HashMap<String, HashMap<String, HashSet<String>>>,
    /// Local binding -> source symbol name, when an import binds a named export
    /// (`import { make as m }`). Module aliases have no source symbol here.
    binding_names: HashMap<String, HashMap<String, String>>,
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

/// The symbol name an import of `module` would bind: its last segment. The
/// same reading [`Graph::build`] records a binding's source name from.
pub fn imported_name(module: &str) -> Option<&str> {
    segments(module).last().copied()
}

/// The stem of a file path — its base name without extension (`src/db.rs` -> `db`).
fn stem(path: &str) -> &str {
    Path::new(path)
        .file_stem()
        .and_then(|s| s.to_str())
        .unwrap_or(path)
}

/// The parent directory's name, when this file is what an import of that
/// directory actually reaches: a language's conventional entry file, or any
/// `.tf` (Terraform loads every file in a module directory, so each one answers
/// for it). `None` for an ordinary file, which must be named directly.
fn directory_alias(path: &str) -> Option<&str> {
    let file = path.rsplit(['/', '\\']).next()?;
    let is_entry = matches!(
        file,
        "index.js"
            | "index.mjs"
            | "index.cjs"
            | "index.jsx"
            | "index.ts"
            | "index.tsx"
            | "__init__.py"
            | "mod.rs"
            | "index.php"
    ) || file.ends_with(".tf");
    if !is_entry {
        return None;
    }
    let dir = path.rsplit_once(['/', '\\'])?.0;
    let name = dir.rsplit(['/', '\\']).next()?;
    if name.is_empty() {
        None
    } else {
        Some(name)
    }
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

/// Whether `module` names `path` structurally, rather than merely sharing one
/// segment with it. Directory entry files stand in for their directory. A
/// named import can append an exported symbol (`from pkg.util import run`), but
/// facts intentionally do not retain enough syntax to distinguish that from an
/// ordinary dotted module import, so that useful resolution stays heuristic.
fn exact_module_match(from: &str, path: &str, module: &str, allow_imported_name: bool) -> bool {
    // A known file extension makes this a path reference, even when a document
    // or template writes `guide.rst` / `base.html` instead of `./guide.rst`.
    // Do not use `Path::extension` alone: Python's `pkg.module` would then look
    // like a file path. The extension must be one codeq classifies as a file.
    let file_reference = is_file_reference(module);
    let mut parts: Vec<&str> = path.split(['/', '\\']).filter(|s| !s.is_empty()).collect();
    let file = parts.pop().unwrap_or(path);
    let stem = stem(file);
    if matches!(file, "__init__.py" | "mod.rs") || (file.starts_with("index.") && !file_reference) {
        // `pkg/__init__.py`, `pkg/index.ts`, and `pkg/mod.rs` are imported as
        // `pkg`, not their conventional entry-file name.
    } else {
        parts.push(stem);
    }
    if module.starts_with('.') || file_reference {
        // Jinja resolves a bare `base.html` through its template search path.
        // The practical, repository-local approximation is every ancestor of
        // the referring template's directory: `templates/blog/post.html` can
        // extend `templates/base.html`, but must not pick a same-named template
        // from an unrelated example.
        if file_reference && is_jinja_template(from) && !module.starts_with('.') {
            let from_parts: Vec<&str> = from
                .rsplit_once(['/', '\\'])
                .map(|(dir, _)| dir.split(['/', '\\']).filter(|s| !s.is_empty()).collect())
                .unwrap_or_default();
            let module_parts: Vec<&str> = module
                .split(['/', '\\'])
                .filter(|s| !s.is_empty())
                .collect();
            for keep in (0..=from_parts.len()).rev() {
                let mut expected = from_parts[..keep].to_vec();
                expected.extend(module_parts.iter().copied());
                if let Some(last) = expected.pop() {
                    expected.push(
                        Path::new(last)
                            .file_stem()
                            .and_then(|s| s.to_str())
                            .unwrap_or(last),
                    );
                }
                if parts == expected {
                    return true;
                }
            }
        }
        let mut expected: Vec<&str> = from
            .rsplit_once(['/', '\\'])
            .map(|(dir, _)| dir.split(['/', '\\']).filter(|s| !s.is_empty()).collect())
            .unwrap_or_default();
        for segment in module.split(['/', '\\']) {
            match segment {
                "" | "." => {}
                ".." => {
                    if expected.pop().is_none() {
                        return false;
                    }
                }
                other => expected.push(other),
            }
        }
        if let Some(last) = expected.pop() {
            expected.push(
                Path::new(last)
                    .file_stem()
                    .and_then(|s| s.to_str())
                    .unwrap_or(last),
            );
        }
        return parts == expected;
    }

    let module = segments(module);
    if module.is_empty() || (!allow_imported_name && module.len() > parts.len()) {
        return false;
    }
    // A Python `from pkg.mod import value` is stored as `pkg.mod.value`, so
    // imported symbols may trail the module path. Compare the longest suffix
    // of the file path against a prefix of the import. The resolver's caller
    // chooses the strongest such match, which keeps `pkg/__init__.py` from
    // winning over `pkg/mod.py` merely because both share `pkg`.
    if allow_imported_name {
        (1..=parts.len().min(module.len()))
            .rev()
            .any(|n| parts.ends_with(&module[..n]))
    } else {
        parts.ends_with(&module)
    }
}

fn exact_module_match_score(
    from: &str,
    path: &str,
    module: &str,
    allow_imported_name: bool,
) -> Option<usize> {
    if !exact_module_match(from, path, module, allow_imported_name) {
        return None;
    }
    if module.starts_with('.') || is_file_reference(module) {
        return Some(path.split(['/', '\\']).filter(|s| !s.is_empty()).count());
    }
    let mut parts: Vec<&str> = path.split(['/', '\\']).filter(|s| !s.is_empty()).collect();
    let file = parts.pop().unwrap_or(path);
    if matches!(file, "__init__.py" | "mod.rs") || file.starts_with("index.") {
        // Entry files stand for the directory component already in `parts`.
    } else {
        parts.push(stem(file));
    }
    let module = segments(module);
    if allow_imported_name {
        (1..=parts.len().min(module.len()))
            .rev()
            .find(|&n| parts.ends_with(&module[..n]))
    } else if parts.ends_with(&module) {
        Some(module.len())
    } else {
        None
    }
}

fn exact_candidates<'a>(
    by_tail: &HashMap<&str, Vec<&'a str>>,
    from: &str,
    module: &str,
    allow_imported_name: bool,
) -> Vec<&'a str> {
    let tails = if module.starts_with('.') || is_file_reference(module) {
        let mut expected: Vec<&str> = from
            .rsplit_once(['/', '\\'])
            .map(|(dir, _)| dir.split(['/', '\\']).filter(|s| !s.is_empty()).collect())
            .unwrap_or_default();
        for segment in module.split(['/', '\\']) {
            match segment {
                "" | "." => {}
                ".." => {
                    if expected.pop().is_none() {
                        return Vec::new();
                    }
                }
                other => expected.push(other),
            }
        }
        vec![stem(expected.last().copied().unwrap_or(""))]
    } else {
        let parts = segments(module);
        if allow_imported_name {
            parts
        } else {
            parts.last().copied().into_iter().collect()
        }
    };
    let mut seen = HashSet::new();
    tails
        .into_iter()
        .flat_map(|tail| by_tail.get(tail).into_iter().flatten().copied())
        .filter(|path| seen.insert(*path))
        .collect()
}

/// The `.go` files that form an internal Go package imported by `module`.
///
/// Go imports name a package directory relative to the module declared in
/// `go.mod`, not a source-file stem. Treating `example.com/acme/app/worker` as
/// a bag of segments can therefore connect it to an unrelated `worker.go` in a
/// different directory. When the module root is known, map that prefix exactly
/// and include the package's non-test source files. `Some(empty)` is meaningful:
/// it is an external Go module and must not fall back to stem matching.
fn go_package_targets<'a>(
    packages: &HashMap<&str, Vec<&'a str>>,
    from: &str,
    module: &str,
    go_module: Option<&str>,
) -> Option<Vec<&'a str>> {
    if !from.ends_with(".go") {
        return None;
    }
    let root = go_module?;
    let package = if module == root {
        ""
    } else if let Some(package) = module
        .strip_prefix(root)
        .and_then(|rest| rest.strip_prefix('/'))
    {
        package
    } else {
        // This is a Go import, but it belongs to another module. Returning an
        // empty target set tells the caller to keep it external rather than
        // falling through to filename-stem guessing.
        return Some(Vec::new());
    };
    Some(packages.get(package).cloned().unwrap_or_default())
}

/// Every Go package directory, with the non-test `.go` files that form it.
///
/// Built once per graph: a Go repository has an import for most of its files,
/// so finding each import's package by scanning every path is quadratic.
fn go_packages(paths: &[String]) -> HashMap<&str, Vec<&str>> {
    let mut packages: HashMap<&str, Vec<&str>> = HashMap::new();
    for path in paths {
        if !path.ends_with(".go") || path.ends_with("_test.go") {
            continue;
        }
        let dir = path
            .rsplit_once(['/', '\\'])
            .map(|(dir, _)| dir)
            .unwrap_or("");
        packages.entry(dir).or_default().push(path);
    }
    packages
}

impl Graph {
    /// Build the internal graph from all repo `files` and all import facts
    /// `(importing_file, raw_module)`.
    pub fn build<F, I>(files: F, imports: I, go_module: Option<&str>) -> Graph
    where
        F: IntoIterator<Item = String>,
        I: IntoIterator<Item = (String, String, Option<String>, bool)>,
    {
        // stem -> files with that stem (usually one; more when names collide).
        let paths: Vec<String> = files.into_iter().collect();
        let mut by_stem: HashMap<&str, Vec<&str>> = HashMap::new();
        let mut by_exact_tail: HashMap<&str, Vec<&str>> = HashMap::new();
        let go_packages = if go_module.is_some() {
            go_packages(&paths)
        } else {
            HashMap::new()
        };
        for p in &paths {
            let mut parts = p.split(['/', '\\']).filter(|s| !s.is_empty());
            let file = parts.next_back().unwrap_or(p);
            by_exact_tail.entry(stem(file)).or_default().push(p);
            if matches!(file, "__init__.py" | "mod.rs") || file.starts_with("index.") {
                by_exact_tail
                    .entry(parts.next_back().unwrap_or(""))
                    .or_default()
                    .push(p);
            }
            by_stem.entry(stem(p)).or_default().push(p.as_str());
            // An import usually names a *directory*, not a file: `./utils` means
            // `utils/index.js`, `from pkg import x` means `pkg/__init__.py`,
            // `mod util` means `util/mod.rs`, and a Terraform `source` names a
            // directory of `.tf` files. Matching stems alone never resolves any
            // of those — the dominant idiom in the two largest ecosystems here —
            // so a file that stands in for its directory is also filed under the
            // directory's name.
            if let Some(dir) = directory_alias(p) {
                by_stem.entry(dir).or_default().push(p.as_str());
            }
        }
        // Precompute each path's component set for scoring.
        let comps: HashMap<&str, HashSet<&str>> = paths
            .iter()
            .map(|p| (p.as_str(), path_components(p)))
            .collect();

        let mut edges: HashMap<String, Vec<Edge>> = HashMap::new();
        let mut bindings: HashMap<String, HashMap<String, HashSet<String>>> = HashMap::new();
        let mut binding_names: HashMap<String, HashMap<String, String>> = HashMap::new();
        // Every importing file gets an entry (even if all imports are external),
        // so a file with only external deps is still a known node.
        for p in &paths {
            edges.entry(p.clone()).or_default();
        }
        for (from, module, binding, allow_imported_name) in imports {
            // `import express from "express"` must not resolve a sibling
            // `express.js`: unlike Python, Node's bare specifiers are packages
            // or built-ins. Relative paths retain the ordinary file resolver.
            if uses_node_resolution(&from) && !module.starts_with('.') {
                continue;
            }
            // Go's module path is enough to resolve an internal package exactly.
            // Do this before the cross-language stem heuristic, and do not let an
            // external module such as `example.net/lib/util` borrow a local
            // `util.go` merely because their final segment happens to agree.
            if let Some(targets) = go_package_targets(&go_packages, &from, &module, go_module) {
                if targets.is_empty() {
                    continue;
                }
                let bucket = edges.entry(from.clone()).or_default();
                for cand in targets {
                    bucket.push(Edge {
                        to: cand.to_string(),
                        module: module.clone(),
                        exact: true,
                    });
                    if let Some(binding) = &binding {
                        bindings
                            .entry(from.clone())
                            .or_default()
                            .entry(binding.clone())
                            .or_default()
                            .insert(cand.to_string());
                    }
                }
                continue;
            }
            // File references are unambiguous relative dependencies. Check them
            // before stem scoring so `_static/flask-name.svg` cannot borrow a
            // source file merely because `flask` is one of its hyphen-separated
            // words. This also lets the Jinja ancestor rule above select its
            // template-root match.
            let exact_files: Vec<&str> =
                exact_candidates(&by_exact_tail, &from, &module, allow_imported_name)
                    .into_iter()
                    .filter(|path| {
                        *path != from
                            && exact_module_match(&from, path, &module, allow_imported_name)
                    })
                    .collect();
            if !exact_files.is_empty() {
                let strongest = exact_files
                    .iter()
                    .filter_map(|path| {
                        exact_module_match_score(&from, path, &module, allow_imported_name)
                    })
                    .max()
                    .unwrap_or(0);
                let bucket = edges.entry(from.clone()).or_default();
                for cand in exact_files.into_iter().filter(|path| {
                    exact_module_match_score(&from, path, &module, allow_imported_name)
                        == Some(strongest)
                }) {
                    bucket.push(Edge {
                        to: cand.to_string(),
                        module: module.clone(),
                        exact: true,
                    });
                    if let Some(binding) = &binding {
                        bindings
                            .entry(from.clone())
                            .or_default()
                            .entry(binding.clone())
                            .or_default()
                            .insert(cand.to_string());
                        if let Some(name) = segments(&module).last() {
                            binding_names
                                .entry(from.clone())
                                .or_default()
                                .insert(binding.clone(), (*name).to_string());
                        }
                    }
                }
                continue;
            }
            // An explicit file reference that does not exist locally is an
            // external/broken link, not a license to borrow a same-stem source
            // file elsewhere in the repository.
            if is_file_reference(&module) {
                continue;
            }
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
            // A structural path match is stronger evidence than shared words.
            // In particular, `templates/index.html` should not also depend on
            // every unrelated `index.html` merely because their stems tie.
            let exact: Vec<&str> = best
                .iter()
                .copied()
                .filter(|cand| exact_module_match(&from, cand, &module, allow_imported_name))
                .collect();
            if !exact.is_empty() {
                best = exact;
            }
            best.sort_unstable();
            best.dedup();
            let bucket = edges.entry(from.clone()).or_default();
            for cand in best {
                bucket.push(Edge {
                    to: cand.to_string(),
                    module: module.clone(),
                    exact: exact_module_match(&from, cand, &module, allow_imported_name),
                });
                if let Some(binding) = &binding {
                    bindings
                        .entry(from.clone())
                        .or_default()
                        .entry(binding.clone())
                        .or_default()
                        .insert(cand.to_string());
                    if let Some(name) = segments(&module).last() {
                        binding_names
                            .entry(from.clone())
                            .or_default()
                            .insert(binding.clone(), (*name).to_string());
                    }
                }
            }
        }
        // Deterministic edge order per node.
        for bucket in edges.values_mut() {
            bucket.sort_by(|a, b| a.to.cmp(&b.to).then(a.module.cmp(&b.module)));
            bucket.dedup_by(|a, b| a.to == b.to && a.module == b.module);
        }
        Graph {
            edges,
            bindings,
            binding_names,
        }
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

    /// Direct internal targets of an import whose path explicitly contains the
    /// single-segment module name `receiver`. This is deliberately narrower
    /// than general dependency resolution: it is used to disambiguate a call
    /// like `context::build()` only when `context` is actually an imported
    /// module. Value receivers such as `client.build()` stay ambiguous rather
    /// than being mistaken for a module based on their spelling alone.
    pub fn direct_targets_for_receiver<'a>(
        &'a self,
        from: &str,
        receiver: &str,
    ) -> HashSet<&'a str> {
        if receiver.is_empty()
            || !receiver
                .bytes()
                .all(|b| b == b'_' || b.is_ascii_alphanumeric())
        {
            return HashSet::new();
        }
        let bound: HashSet<&str> = self
            .bindings
            .get(from)
            .and_then(|bindings| bindings.get(receiver))
            .into_iter()
            .flatten()
            .map(String::as_str)
            .collect();
        if !bound.is_empty() {
            return bound;
        }
        self.edges
            .get(from)
            .into_iter()
            .flatten()
            .filter(|edge| segments(&edge.module).contains(&receiver))
            .map(|edge| edge.to.as_str())
            .collect()
    }

    /// The exported definition name bound locally as `binding`, if this was a
    /// named import. `m()` after `import { make as m }` therefore looks up
    /// `make` in the resolved target file rather than looking for a definition
    /// literally named `m`.
    pub fn source_name_for_binding(&self, from: &str, binding: &str) -> Option<&str> {
        self.binding_names
            .get(from)
            .and_then(|bindings| bindings.get(binding))
            .map(String::as_str)
    }

    /// Local aliases that name an imported source symbol. `use crate::util::make
    /// as m` contributes `(app.rs, m)` for `make`, allowing a query for the
    /// source name to find syntax that necessarily refers to it.
    pub fn bindings_for_source(&self, source: &str) -> Vec<(String, String)> {
        let mut out: Vec<(String, String)> = self
            .binding_names
            .iter()
            .flat_map(|(path, bindings)| {
                bindings
                    .iter()
                    .filter(|(_, name)| *name == source)
                    .map(move |(binding, _)| (path.clone(), binding.clone()))
            })
            .collect();
        out.sort_unstable();
        out
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
                            out.push(DepHop {
                                path: e.to.clone(),
                                module: e.module.clone(),
                                depth: d,
                            });
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
    ///
    /// Johnson's algorithm. The obvious version of this — walk every path and
    /// report a back-edge into it — costs one visit per simple *path*, and a
    /// dependency graph has exponentially more paths than cycles: 28 files in
    /// fourteen layers of two, with no cycles at all, took 20s to answer
    /// "none", and a real 200-file TypeScript app took 61s to find 11. Blocking
    /// is what makes the walk cost scale with the answer instead: a node is
    /// unblocked only when it is found to lie on a cycle, so a subtree that
    /// cannot reach `start` is explored once rather than once per path into it.
    ///
    /// Deduplicating successors matters for the same reason. A named import
    /// records both the module and the binding (`./m` and `./m.f`), so every
    /// TypeScript import is two exact edges to one file — a second doubling of
    /// the branching factor that changes no answer.
    pub fn cycles(&self) -> Vec<Vec<String>> {
        // Index every file that has, or is, an exact edge. Sorted order makes
        // "index >= start" the same restriction the canonical form wants: a
        // cycle is enumerated once, from its lexically-least member.
        let mut nodes: Vec<&str> = Vec::new();
        for (from, bucket) in &self.edges {
            nodes.push(from.as_str());
            nodes.extend(bucket.iter().filter(|e| e.exact).map(|e| e.to.as_str()));
        }
        nodes.sort_unstable();
        nodes.dedup();
        let index: HashMap<&str, usize> = nodes.iter().enumerate().map(|(i, &n)| (n, i)).collect();

        let n = nodes.len();
        let mut succ: Vec<Vec<usize>> = vec![Vec::new(); n];
        for (from, bucket) in &self.edges {
            let f = index[from.as_str()];
            for e in bucket.iter().filter(|e| e.exact) {
                succ[f].push(index[e.to.as_str()]);
            }
        }
        for s in succ.iter_mut() {
            s.sort_unstable();
            s.dedup();
        }
        let (component, component_sizes) = strongly_connected_components(&succ);

        let mut found: HashSet<Vec<String>> = HashSet::new();
        let mut result: Vec<Vec<String>> = Vec::new();
        let mut blocked: Vec<bool> = vec![false; n];
        // `b_list[w]` holds the nodes to unblock when `w` unblocks — those that
        // failed to reach `start` only because `w` was blocked at the time.
        let mut b_list: Vec<Vec<usize>> = vec![Vec::new(); n];
        // Which nodes this round dirtied, so the reset is proportional to the
        // work done and not to the size of the repo.
        let mut dirty: Vec<usize> = Vec::new();

        for start in 0..n {
            // A singleton is cyclic only when it imports itself. More
            // importantly, a cycle cannot cross an SCC boundary, so starting a
            // Johnson walk at every node in a large DAG repeats the same dead
            // traversal once per node. Redis's mostly-acyclic import graph made
            // `deps --cycles` take minutes for exactly that reason.
            let start_component = component[start];
            if component_sizes[start_component] == 1 && !succ[start].contains(&start) {
                continue;
            }
            for &v in &dirty {
                blocked[v] = false;
                b_list[v].clear();
            }
            dirty.clear();

            // Frames are (node, next successor to try, found a cycle through it).
            let mut stack: Vec<(usize, usize, bool)> = vec![(start, 0, false)];
            let mut path: Vec<usize> = vec![start];
            blocked[start] = true;
            dirty.push(start);

            while let Some(&(v, i, _)) = stack.last() {
                if i < succ[v].len() {
                    stack.last_mut().unwrap().1 += 1;
                    let w = succ[v][i];
                    // Cycles rooted below `start` were reported on an earlier round.
                    if w < start || component[w] != start_component {
                        continue;
                    }
                    if w == start {
                        // `path` opens at `start`, and every other node on it is
                        // greater, so it is already in canonical form.
                        let cyc: Vec<String> = path.iter().map(|&x| nodes[x].to_string()).collect();
                        if let Some(c) = canonical_cycle(cyc) {
                            if found.insert(c.clone()) {
                                result.push(c);
                            }
                        }
                        stack.last_mut().unwrap().2 = true;
                    } else if !blocked[w] {
                        blocked[w] = true;
                        dirty.push(w);
                        path.push(w);
                        stack.push((w, 0, false));
                    }
                } else {
                    let (v, _, on_cycle) = stack.pop().unwrap();
                    path.pop();
                    if on_cycle {
                        // Unblock `v` and everything that was waiting on it.
                        let mut work = vec![v];
                        while let Some(u) = work.pop() {
                            blocked[u] = false;
                            for w in std::mem::take(&mut b_list[u]) {
                                if blocked[w] {
                                    work.push(w);
                                }
                            }
                        }
                        if let Some(parent) = stack.last_mut() {
                            parent.2 = true;
                        }
                    } else {
                        // `v` reaches no cycle while its successors stay blocked;
                        // record it so unblocking one of them reconsiders `v`.
                        for &w in &succ[v] {
                            if w >= start && !b_list[w].contains(&v) {
                                b_list[w].push(v);
                                dirty.push(w);
                            }
                        }
                    }
                }
            }
        }
        result.sort();
        result
    }
}

/// Component id and size for every node, using iterative Kosaraju passes.
///
/// Keeping it iterative matters for dependency graphs: a generated project can
/// contain an import chain thousands of files deep, where recursive DFS would
/// trade a slow cycle query for a stack overflow.
fn strongly_connected_components(succ: &[Vec<usize>]) -> (Vec<usize>, Vec<usize>) {
    let n = succ.len();
    let mut reverse = vec![Vec::new(); n];
    for (from, targets) in succ.iter().enumerate() {
        for &to in targets {
            reverse[to].push(from);
        }
    }

    let mut seen = vec![false; n];
    let mut finish = Vec::with_capacity(n);
    for root in 0..n {
        if seen[root] {
            continue;
        }
        seen[root] = true;
        let mut stack = vec![(root, 0usize)];
        while let Some(&(node, next)) = stack.last() {
            if next < succ[node].len() {
                stack.last_mut().unwrap().1 += 1;
                let to = succ[node][next];
                if !seen[to] {
                    seen[to] = true;
                    stack.push((to, 0));
                }
            } else {
                finish.push(node);
                stack.pop();
            }
        }
    }

    let mut component = vec![usize::MAX; n];
    let mut sizes = Vec::new();
    for &root in finish.iter().rev() {
        if component[root] != usize::MAX {
            continue;
        }
        let id = sizes.len();
        let mut size = 0;
        let mut stack = vec![root];
        component[root] = id;
        while let Some(node) = stack.pop() {
            size += 1;
            for &from in &reverse[node] {
                if component[from] == usize::MAX {
                    component[from] = id;
                    stack.push(from);
                }
            }
        }
        sizes.push(size);
    }
    (component, sizes)
}

/// Rotate a cycle to start at its lexically-least member so the same cycle
/// discovered from different entry points collapses to one representation.
fn canonical_cycle(cyc: Vec<String>) -> Option<Vec<String>> {
    if cyc.is_empty() {
        return None;
    }
    let min = cyc
        .iter()
        .enumerate()
        .min_by(|a, b| a.1.cmp(b.1))
        .map(|(i, _)| i)?;
    let mut out = Vec::with_capacity(cyc.len());
    out.extend_from_slice(&cyc[min..]);
    out.extend_from_slice(&cyc[..min]);
    Some(out)
}
