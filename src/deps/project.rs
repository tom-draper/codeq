use super::config;
use anyhow::{bail, ensure, Context, Result};
use globset::{GlobBuilder, GlobMatcher};
use serde::Deserialize;
use serde_json::Value;
use std::collections::{HashMap, HashSet};
use std::path::{Path, PathBuf};

struct Alias {
    pub pattern: String,
    pub targets: Vec<String>,
}

struct Project {
    path: String,
    directory: String,
    aliases: Vec<Alias>,
    files: HashSet<String>,
    includes: Vec<GlobMatcher>,
    excludes: Vec<GlobMatcher>,
    owned: HashSet<String>,
}

impl Project {
    fn contains(&self, file: &str) -> bool {
        self.files.contains(file)
            || (self.includes.iter().any(|g| g.is_match(file))
                && !self.excludes.iter().any(|g| g.is_match(file)))
    }
}

struct Projects {
    projects: Vec<Project>,
    unindexed: HashMap<String, String>,
    guards: Vec<(PathBuf, bool)>,
}

#[derive(Clone, Default)]
struct Effective {
    base_url: Option<String>,
    paths: Option<(String, Vec<(String, Vec<String>)>)>,
    files: Option<Vec<String>>,
    include: Option<Vec<String>>,
    exclude: Option<Vec<String>>,
    out_dir: Option<String>,
    references: Vec<String>,
}

#[derive(Deserialize)]
#[serde(untagged)]
enum Extends {
    One(String),
    Many(Vec<String>),
}

#[derive(Default, Deserialize)]
#[serde(rename_all = "camelCase")]
struct CompilerOptions {
    base_url: Option<String>,
    paths: Option<serde_json::Map<String, Value>>,
    out_dir: Option<String>,
}

#[derive(Deserialize)]
struct Reference {
    path: String,
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct RawConfig {
    extends: Option<Extends>,
    #[serde(default)]
    compiler_options: CompilerOptions,
    files: Option<Vec<String>>,
    include: Option<Vec<String>>,
    exclude: Option<Vec<String>>,
    #[serde(default)]
    references: Vec<Reference>,
}

/// Config paths stay repository-relative, including the directory that declared
/// an inherited paths map. An effective baseUrl overrides that map's origin.
impl Projects {
    fn load(
        root: &Path,
        indexed: &[String],
        imports: &[(String, String, Option<String>, bool)],
    ) -> Result<Self> {
        let files: HashSet<&str> = indexed.iter().map(String::as_str).collect();
        let mut pending: Vec<String> = indexed
            .iter()
            .filter(|p| {
                p.ends_with("tsconfig.json")
                    && Path::new(p)
                        .file_name()
                        .is_some_and(|n| n == "tsconfig.json")
                    || p.strip_suffix("jsconfig.json").is_some_and(|prefix| {
                        Path::new(p)
                            .file_name()
                            .is_some_and(|n| n == "jsconfig.json")
                            && !files.contains(format!("{prefix}tsconfig.json").as_str())
                    })
            })
            .cloned()
            .collect();
        for name in ["tsconfig.json", "jsconfig.json"] {
            if name == "jsconfig.json" && files.contains("tsconfig.json") {
                continue;
            }
            ensure!(
                !root.join(name).exists() || files.contains(name),
                "{name} is not indexed; project configuration must be indexed to stay fresh"
            );
        }
        let mut seen = HashSet::new();
        let mut cache = HashMap::new();
        let mut projects = Vec::new();
        while let Some(path) = pending.pop() {
            if !seen.insert(path.clone()) {
                continue;
            }
            let effective = load_effective(root, &path, &files, &mut cache, &mut HashSet::new())?;
            let directory = parent(&path).to_string();
            pending.extend(effective.references);
            let mut aliases = Vec::new();
            if let Some((origin, rules)) = effective.paths {
                let base = effective.base_url.as_deref().unwrap_or(&origin);
                for (pattern, targets) in rules {
                    ensure!(
                        pattern.matches('*').count() <= 1,
                        "{path}: alias '{pattern}' has multiple wildcards"
                    );
                    ensure!(
                        targets.iter().all(|t| t.matches('*').count() <= 1),
                        "{path}: alias '{pattern}' has a target with multiple wildcards"
                    );
                    let targets = targets
                        .iter()
                        .map(|t| repo_path(base, t))
                        .collect::<Result<Vec<_>>>()
                        .with_context(|| format!("{path}: invalid alias target for '{pattern}'"))?;
                    aliases.push(Alias { pattern, targets });
                }
            }
            // Stable sorting preserves declaration order when prefixes tie.
            aliases.sort_by_key(|rule| {
                (
                    rule.pattern.contains('*'),
                    std::cmp::Reverse(rule.pattern.find('*').unwrap_or(rule.pattern.len())),
                )
            });
            let includes = effective.include.unwrap_or_else(|| {
                if effective.files.is_some() {
                    Vec::new()
                } else {
                    vec![if directory.is_empty() {
                        "**/*".to_string()
                    } else {
                        format!("{directory}/**/*")
                    }]
                }
            });
            let excludes = effective
                .exclude
                .unwrap_or_else(|| effective.out_dir.into_iter().collect());
            projects.push(Project {
                path,
                directory,
                aliases,
                files: effective.files.unwrap_or_default().into_iter().collect(),
                includes: compile_patterns(root, &includes)?,
                excludes: compile_patterns(root, &excludes)?,
                owned: HashSet::new(),
            });
        }
        projects.sort_by(|a, b| a.path.cmp(&b.path));
        let mut guards = HashMap::new();
        let mut unindexed = HashMap::new();
        for (from, _, _, _) in imports {
            if !super::uses_node_resolution(from) {
                continue;
            }
            let mut directory = Some(parent(from));
            while let Some(dir) = directory {
                for name in ["tsconfig.json", "jsconfig.json"] {
                    if name == "jsconfig.json"
                        && files.contains(repo_path(dir, "tsconfig.json")?.as_str())
                    {
                        continue;
                    }
                    let path = repo_path(dir, name)?;
                    if !files.contains(path.as_str()) {
                        let absolute = root.join(&path);
                        let exists = absolute
                            .try_exists()
                            .with_context(|| format!("cannot inspect {path}"))?;
                        guards.insert(absolute, exists);
                        if exists {
                            unindexed.entry(from.clone()).or_insert_with(|| format!(
                                "project configuration '{path}' is not indexed; parent aliases were not applied"));
                        }
                    }
                }
                directory = if dir.is_empty() {
                    None
                } else {
                    Some(parent(dir))
                };
            }
        }
        let mut by_file: HashMap<&str, Vec<(&str, bool)>> = HashMap::new();
        for (from, module, _, derived) in imports {
            by_file.entry(from).or_default().push((module, *derived));
        }
        for project in &mut projects {
            let mut pending: Vec<String> = indexed
                .iter()
                .filter(|p| project.contains(p))
                .cloned()
                .collect();
            while let Some(from) = pending.pop() {
                if !project.owned.insert(from.clone()) {
                    continue;
                }
                for &(module, derived) in by_file.get(from.as_str()).into_iter().flatten() {
                    for spelling in module_spellings(module, derived) {
                        let targets = if spelling.starts_with('.') {
                            repo_path(parent(&from), &spelling)
                                .ok()
                                .into_iter()
                                .collect()
                        } else if let Some((rule, remainder)) =
                            project.aliases.iter().find_map(|rule| {
                                wildcard_match(&rule.pattern, &spelling)
                                    .map(|remainder| (rule, remainder))
                            })
                        {
                            rule.targets
                                .iter()
                                .map(|t| t.replace('*', remainder))
                                .collect()
                        } else {
                            Vec::new()
                        };
                        if let Some(candidates) = targets.iter().find_map(|target| {
                            let candidates = files_at_path(target, indexed);
                            (!candidates.is_empty()).then_some(candidates)
                        }) {
                            pending.extend(candidates);
                            break;
                        }
                    }
                }
            }
        }
        Ok(Self {
            projects,
            unindexed,
            guards: guards.into_iter().collect(),
        })
    }

    fn aliases(&self, importer: &str) -> std::result::Result<&[Alias], String> {
        if let Some(reason) = self.unindexed.get(importer) {
            return Err(reason.clone());
        }
        let candidates: Vec<&Project> = self
            .projects
            .iter()
            .filter(|p| p.owned.contains(importer))
            .collect();
        let strongest = candidates
            .iter()
            .map(|p| (p.files.contains(importer), p.directory.len()))
            .max();
        let owners: Vec<_> = candidates
            .into_iter()
            .filter(|p| Some((p.files.contains(importer), p.directory.len())) == strongest)
            .collect();
        match owners.as_slice() {
            [] => Ok(&[]),
            [owner] => Ok(&owner.aliases),
            _ => Err(format!(
                "ambiguous project ownership for {importer}: {}",
                owners
                    .iter()
                    .map(|p| p.path.as_str())
                    .collect::<Vec<_>>()
                    .join(", ")
            )),
        }
    }
}

fn parent(path: &str) -> &str {
    path.rsplit_once('/').map_or("", |(dir, _)| dir)
}

fn repo_path(base: &str, relative: &str) -> Result<String> {
    let relative = relative.replace('\\', "/");
    ensure!(
        !Path::new(&relative).is_absolute() && !relative.contains(':'),
        "path is outside the repository: {relative}"
    );
    let mut parts: Vec<&str> = base.split('/').filter(|p| !p.is_empty()).collect();
    for part in relative.split('/') {
        match part {
            "" | "." => {}
            ".." => {
                ensure!(
                    parts.pop().is_some(),
                    "path is outside the repository: {base}/{relative}"
                );
            }
            _ => parts.push(part),
        }
    }
    Ok(parts.join("/"))
}

fn compile_patterns(root: &Path, patterns: &[String]) -> Result<Vec<GlobMatcher>> {
    patterns
        .iter()
        .map(|pattern| {
            let pattern = if root.join(pattern).is_dir()
                || (!pattern.contains('*') && Path::new(pattern).extension().is_none())
            {
                if pattern.is_empty() {
                    "**/*".to_string()
                } else {
                    format!("{}/**/*", pattern.trim_end_matches('/'))
                }
            } else {
                pattern.clone()
            };
            // TypeScript gives only * and ? wildcard meaning. globset also
            // interprets braces and character classes, so quote those literally.
            let pattern = pattern
                .chars()
                .flat_map(|c| {
                    if matches!(c, '{' | '}' | '[' | ']') {
                        vec!['\\', c]
                    } else {
                        vec![c]
                    }
                })
                .collect::<String>();
            Ok(GlobBuilder::new(&pattern)
                .literal_separator(true)
                .backslash_escape(true)
                .build()?
                .compile_matcher())
        })
        .collect()
}

fn config_target(
    root: &Path,
    directory: &str,
    value: &str,
    indexed: &HashSet<&str>,
) -> Result<String> {
    let path = repo_path(directory, value)?;
    for candidate in [
        path.clone(),
        format!("{path}.json"),
        format!("{path}/tsconfig.json"),
    ] {
        if indexed.contains(candidate.as_str()) {
            return Ok(candidate);
        }
    }
    ensure!(
        !root.join(&path).exists(),
        "configuration is not indexed: {path}"
    );
    bail!("missing or unsupported project configuration: {path}")
}

fn merge(target: &mut Effective, incoming: Effective) {
    if incoming.base_url.is_some() {
        target.base_url = incoming.base_url;
    }
    if incoming.paths.is_some() {
        target.paths = incoming.paths;
    }
    if incoming.files.is_some() {
        target.files = incoming.files;
    }
    if incoming.include.is_some() {
        target.include = incoming.include;
    }
    if incoming.exclude.is_some() {
        target.exclude = incoming.exclude;
    }
    if incoming.out_dir.is_some() {
        target.out_dir = incoming.out_dir;
    }
}

fn load_effective(
    root: &Path,
    path: &str,
    indexed: &HashSet<&str>,
    cache: &mut HashMap<String, Effective>,
    active: &mut HashSet<String>,
) -> Result<Effective> {
    if let Some(effective) = cache.get(path) {
        return Ok(effective.clone());
    }
    ensure!(
        active.len() < 64,
        "{path}: project inheritance exceeds depth 64"
    );
    ensure!(
        active.insert(path.to_string()),
        "{path}: project inheritance cycle"
    );
    ensure!(
        indexed.contains(path),
        "{path}: project configuration is not indexed"
    );
    let absolute = root.join(path);
    ensure!(
        absolute.canonicalize()?.starts_with(root),
        "{path}: configuration is outside the repository"
    );
    let bytes = std::fs::read(&absolute).with_context(|| format!("cannot read {path}"))?;
    let raw: RawConfig = serde_json::from_value(
        config::parse_jsonc(&bytes).with_context(|| format!("cannot parse {path}"))?,
    )
    .with_context(|| format!("cannot parse {path}"))?;
    let directory = parent(path);
    let mut effective = Effective::default();
    let parents = match raw.extends {
        None => Vec::new(),
        Some(Extends::One(p)) => vec![p],
        Some(Extends::Many(p)) => p,
    };
    for extends in parents {
        ensure!(
            extends.starts_with('.'),
            "{path}: package-based extends is unsupported: {extends}"
        );
        let target = config_target(root, directory, &extends, indexed)
            .with_context(|| format!("{path}: cannot resolve extends"))?;
        merge(
            &mut effective,
            load_effective(root, &target, indexed, cache, active)?,
        );
    }
    if let Some(base) = raw.compiler_options.base_url {
        effective.base_url =
            Some(repo_path(directory, &base).with_context(|| format!("{path}: invalid baseUrl"))?);
    }
    if let Some(out) = raw.compiler_options.out_dir {
        effective.out_dir =
            Some(repo_path(directory, &out).with_context(|| format!("{path}: invalid outDir"))?);
    }
    if let Some(paths) = raw.compiler_options.paths {
        let rules = paths
            .into_iter()
            .map(|(pattern, value)| {
                let targets: Vec<String> = serde_json::from_value(value)
                    .with_context(|| format!("{path}: invalid paths for '{pattern}'"))?;
                ensure!(
                    !targets.is_empty(),
                    "{path}: alias '{pattern}' has no targets"
                );
                Ok((pattern, targets))
            })
            .collect::<Result<Vec<_>>>()?;
        effective.paths = Some((directory.to_string(), rules));
    }
    let resolve = |items: Vec<String>| {
        items
            .iter()
            .map(|p| repo_path(directory, p))
            .collect::<Result<Vec<_>>>()
    };
    if let Some(files) = raw.files {
        effective.files = Some(resolve(files)?);
    }
    if let Some(include) = raw.include {
        effective.include = Some(resolve(include)?);
    }
    if let Some(exclude) = raw.exclude {
        effective.exclude = Some(resolve(exclude)?);
    }
    effective.references = raw
        .references
        .iter()
        .map(|r| {
            config_target(root, directory, &r.path, indexed)
                .with_context(|| format!("{path}: cannot resolve project reference"))
        })
        .collect::<Result<_>>()?;
    active.remove(path);
    cache.insert(path.to_string(), effective.clone());
    Ok(effective)
}

pub(super) struct ProjectResolution {
    configs: Projects,
    packages: HashMap<String, (String, Vec<String>)>,
}

impl ProjectResolution {
    pub(super) fn config_guards(self) -> Vec<(PathBuf, bool)> {
        self.configs.guards
    }
    pub(super) fn load(
        root: &Path,
        paths: &[String],
        imports: &[(String, String, Option<String>, bool)],
    ) -> Result<Self> {
        let mut result = Self {
            configs: Projects::load(root, paths, imports)?,
            packages: HashMap::new(),
        };

        let root_manifest = root.join("package.json");
        let workspaces = std::fs::read(root_manifest)
            .ok()
            .and_then(|bytes| serde_json::from_slice::<Value>(&bytes).ok())
            .and_then(|json| {
                json.pointer("/workspaces/packages")
                    .or_else(|| json.get("workspaces"))
                    .and_then(Value::as_array)
                    .map(|items| {
                        items
                            .iter()
                            .filter_map(Value::as_str)
                            .map(str::to_string)
                            .collect::<Vec<_>>()
                    })
            })
            .unwrap_or_default();
        for manifest in paths
            .iter()
            .filter(|path| path.ends_with("package.json") && path.as_str() != "package.json")
        {
            let Some(parent) = Path::new(manifest).parent() else {
                continue;
            };
            let package_root = normalize_path(parent);
            if !workspaces
                .iter()
                .any(|pattern| workspace_match(pattern, &package_root))
            {
                continue;
            }
            let Some(package) = std::fs::read(root.join(manifest))
                .ok()
                .and_then(|bytes| serde_json::from_slice::<Value>(&bytes).ok())
            else {
                continue;
            };
            let Some(name) = package
                .get("name")
                .and_then(Value::as_str)
                .map(str::to_string)
            else {
                continue;
            };
            let entries = ["source", "module", "types", "main"]
                .iter()
                .filter_map(|key| package.get(key).and_then(Value::as_str))
                .map(|entry| normalize_path(&Path::new(&package_root).join(entry)))
                .collect::<Vec<_>>();
            result.packages.insert(name, (package_root, entries));
        }
        Ok(result)
    }

    pub(super) fn targets(
        &self,
        importer: &str,
        module: &str,
        allow_imported_name: bool,
        paths: &[String],
    ) -> Option<Result<Vec<String>, String>> {
        let aliases = match self.configs.aliases(importer) {
            Ok(aliases) => aliases,
            Err(reason) => return Some(Err(reason)),
        };
        let mut unresolved_reason = None;
        for spelling in module_spellings(module, allow_imported_name) {
            if let Some((rule, remainder)) = aliases.iter().find_map(|rule| {
                wildcard_match(&rule.pattern, &spelling).map(|remainder| (rule, remainder))
            }) {
                let pattern = &rule.pattern;
                let targets = &rule.targets;
                let resolved: Vec<String> = targets
                    .iter()
                    .map(|target| target.replace('*', remainder))
                    .collect();
                for target in &resolved {
                    let candidates = files_at_path(target, paths);
                    if !candidates.is_empty() {
                        return Some(Ok(candidates));
                    }
                }
                unresolved_reason = Some(format!(
                    "TypeScript path alias '{pattern}' matched, but none of [{}] have indexed files",
                    resolved.join(", ")
                ));
                continue;
            }
            let package_name = spelling
                .split('/')
                .take(if spelling.starts_with('@') { 2 } else { 1 })
                .collect::<Vec<_>>()
                .join("/");
            if let Some((package_root, entries)) = self.packages.get(&package_name) {
                let subpath = spelling
                    .strip_prefix(&package_name)
                    .unwrap_or("")
                    .trim_start_matches('/');
                let expected: Vec<String> = if !subpath.is_empty() {
                    vec![normalize_path(&Path::new(package_root).join(subpath))]
                } else if !entries.is_empty() {
                    entries.clone()
                } else {
                    vec![
                        normalize_path(&Path::new(package_root).join("src/index")),
                        normalize_path(&Path::new(package_root).join("index")),
                    ]
                };
                let candidates = expected
                    .iter()
                    .find_map(|target| {
                        let candidates = files_at_path(target, paths);
                        (!candidates.is_empty()).then_some(candidates)
                    })
                    .unwrap_or_default();
                if candidates.is_empty() {
                    unresolved_reason = Some(format!(
                        "workspace package '{package_name}' matched, but no indexed entry file was found"
                    ));
                } else {
                    return Some(Ok(candidates));
                }
            }
        }
        if let Some(reason) = unresolved_reason {
            return Some(Err(reason));
        }
        if module.starts_with("@/") || module.starts_with("~/") || module.starts_with("#/") {
            return Some(Err(format!(
                "project-style import '{module}' did not match a configured TypeScript path alias"
            )));
        }
        None
    }
}

fn normalize_path(path: &Path) -> String {
    let mut parts: Vec<String> = Vec::new();
    for component in path.components() {
        match component {
            std::path::Component::Normal(part) => parts.push(part.to_string_lossy().into_owned()),
            std::path::Component::ParentDir if parts.last().is_some_and(|part| part != "..") => {
                parts.pop();
            }
            std::path::Component::ParentDir => parts.push("..".into()),
            std::path::Component::CurDir
            | std::path::Component::RootDir
            | std::path::Component::Prefix(_) => {}
        }
    }
    parts.join("/")
}

fn wildcard_match<'a>(pattern: &str, value: &'a str) -> Option<&'a str> {
    if let Some((prefix, suffix)) = pattern.split_once('*') {
        value.strip_prefix(prefix)?.strip_suffix(suffix)
    } else if pattern == value {
        Some("")
    } else {
        None
    }
}

fn workspace_match(pattern: &str, path: &str) -> bool {
    wildcard_match(pattern.trim_end_matches('/'), path).is_some()
        || pattern.trim_end_matches('/') == path
}

fn module_spellings(module: &str, allow_imported_name: bool) -> Vec<String> {
    let mut spellings = vec![module.to_string()];
    if allow_imported_name || module.starts_with('@') {
        if let Some((base, _)) = module.rsplit_once('.') {
            spellings.push(base.to_string());
        }
    }
    spellings
}

fn files_at_path(expected: &str, paths: &[String]) -> Vec<String> {
    const EXTENSIONS: &[&str] = &[
        "ts", "tsx", "mts", "cts", "js", "jsx", "mjs", "cjs", "vue", "svelte", "json",
    ];
    let mut candidates: Vec<String> = paths
        .iter()
        .filter(|path| {
            let normalized = expected.trim_start_matches("./");
            path.as_str() == normalized
                || EXTENSIONS
                    .iter()
                    .any(|ext| path.as_str() == format!("{normalized}.{ext}"))
                || [
                    "index.ts",
                    "index.tsx",
                    "index.js",
                    "index.jsx",
                    "index.mjs",
                    "index.cjs",
                ]
                .iter()
                .any(|entry| path.as_str() == format!("{normalized}/{entry}"))
        })
        .cloned()
        .collect();
    candidates.sort();
    candidates.dedup();
    candidates
}
