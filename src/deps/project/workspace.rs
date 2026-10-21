use super::{files_at_path, repo_path, wildcard_match};
use anyhow::{bail, ensure, Context, Result};
use globset::{GlobBuilder, GlobSetBuilder};
use serde_json::Value;
use std::collections::{HashMap, HashSet};
use std::path::{Path, PathBuf};

pub(super) struct Workspace {
    packages: HashMap<String, Vec<Package>>,
    pub guards: Vec<(PathBuf, bool)>,
}

struct Package {
    root: String,
    entries: Vec<String>,
    exports: Option<Exports>,
}

enum Exports {
    Target(String),
    Subpaths(Vec<(String, Exports)>),
    Conditions(Vec<(String, Exports)>),
    Blocked,
    Unsupported(String),
}

impl Exports {
    fn parse(value: &Value) -> Self {
        Self::parse_value(value, true)
    }

    fn parse_value(value: &Value, allow_subpaths: bool) -> Self {
        match value {
            Value::Null => Self::Blocked,
            Value::String(target) => Self::Target(target.clone()),
            Value::Object(map) => {
                let subpaths = map.keys().any(|k| k.starts_with('.'));
                if !subpaths
                    && map.keys().any(|key| {
                        key.parse::<u32>()
                            .is_ok_and(|n| n < u32::MAX && n.to_string() == *key)
                    })
                {
                    return Self::Unsupported(
                        "package exports cannot contain numeric condition keys".into(),
                    );
                }
                if subpaths && !allow_subpaths {
                    return Self::Unsupported("nested subpath exports are invalid".into());
                }
                if subpaths && map.keys().any(|k| !k.starts_with('.')) {
                    return Self::Unsupported("exports mixes subpaths and conditions".into());
                }
                let mut entries: Vec<_> = map
                    .iter()
                    .map(|(k, v)| (k.clone(), Self::parse_value(v, false)))
                    .collect();
                if subpaths {
                    if entries.iter().any(|(k, _)| {
                        (k != "." && !k.starts_with("./")) || k.matches('*').count() > 1
                    }) {
                        return Self::Unsupported("unsupported exports subpath pattern".into());
                    }
                    entries.sort_by_key(|(key, _)| {
                        (
                            key.contains('*'),
                            std::cmp::Reverse(key.find('*').unwrap_or(key.len())),
                            std::cmp::Reverse(key.len()),
                        )
                    });
                    Self::Subpaths(entries)
                } else {
                    Self::Conditions(entries)
                }
            }
            Value::Array(_) => Self::Unsupported(
                "array exports require target fallback semantics not inferred by Codeq".into(),
            ),
            _ => Self::Unsupported("invalid exports target".into()),
        }
    }

    fn target(&self, subpath: &str, replacement: Option<&str>) -> Result<String, String> {
        match self {
            Self::Blocked => Err("package exports blocks this subpath".into()),
            Self::Unsupported(reason) => Err(reason.clone()),
            Self::Target(target) => {
                if replacement.is_none() && subpath != "." {
                    return Err("subpath is not exposed by package exports".into());
                }
                if replacement.is_none() && target.contains('*') {
                    return Err(
                        "exports target contains a wildcard without a matching pattern".into(),
                    );
                }
                let target = match replacement {
                    Some(value) => target.replace('*', value),
                    None => target.clone(),
                };
                if !target.starts_with("./")
                    || target.contains(['\\', '%'])
                    || target[2..]
                        .split('/')
                        .any(|p| matches!(p, "." | ".." | "node_modules"))
                {
                    return Err("invalid or unsupported package exports target".into());
                }
                Ok(target)
            }
            Self::Subpaths(entries) => {
                let Some((rule, replacement)) = entries.iter().find_map(|(pattern, rule)| {
                    wildcard_match(pattern, subpath)
                        .map(|r| (rule, pattern.contains('*').then_some(r)))
                }) else {
                    return Err("subpath is not exposed by package exports".into());
                };
                rule.target(".", replacement)
            }
            Self::Conditions(entries) => {
                let mut outcomes = Vec::new();
                let mut fallback = false;
                for (condition, rule) in entries {
                    let outcome = rule.target(subpath, replacement).map_err(|reason| {
                        format!("conditional exports are unresolved: {reason}")
                    })?;
                    outcomes.push(outcome);
                    if condition == "default" {
                        fallback = true;
                        break;
                    }
                }
                if fallback && !outcomes.is_empty() && outcomes.iter().all(|t| t == &outcomes[0]) {
                    Ok(outcomes.remove(0))
                } else {
                    Err("conditional exports need runtime or type conditions; no unique target inferred".into())
                }
            }
        }
    }
}

impl Workspace {
    pub fn load(root: &Path, indexed: &HashSet<String>) -> Result<Self> {
        let mut guards = Vec::new();
        for name in ["package.json", "pnpm-workspace.yaml"] {
            let path = root.join(name);
            let exists = path
                .try_exists()
                .with_context(|| format!("cannot inspect {name}"))?;
            if !indexed.contains(name) {
                guards.push((path, exists));
                ensure!(
                    !exists,
                    "{name} is not indexed; workspace configuration must be indexed to stay fresh"
                );
            }
        }
        let root_manifest = if indexed.contains("package.json") {
            read_manifest(root, "package.json")?
        } else {
            Value::Null
        };
        let patterns: Vec<String> = if indexed.contains("pnpm-workspace.yaml") {
            pnpm_patterns(&std::fs::read(root.join("pnpm-workspace.yaml"))?)
                .context("cannot parse pnpm-workspace.yaml")?
        } else if let Some(value) = root_manifest
            .pointer("/workspaces/packages")
            .or_else(|| root_manifest.get("workspaces"))
        {
            serde_json::from_value(value.clone())
                .context("package.json: workspaces must contain string patterns")?
        } else {
            Vec::new()
        };
        let mut included = GlobSetBuilder::new();
        let mut excluded = GlobSetBuilder::new();
        for pattern in &patterns {
            let (negative, pattern) = pattern
                .strip_prefix('!')
                .map_or((false, pattern.as_str()), |p| (true, p));
            let glob = GlobBuilder::new(pattern.trim_end_matches('/'))
                .literal_separator(true)
                .build()
                .with_context(|| format!("invalid workspace pattern: {pattern}"))?;
            if negative {
                excluded.add(glob);
            } else {
                included.add(glob);
            }
        }
        let (included, excluded) = (included.build()?, excluded.build()?);
        let mut packages: HashMap<String, Vec<Package>> = HashMap::new();
        let mut manifests: Vec<_> = indexed
            .iter()
            .filter(|p| {
                Path::new(p)
                    .file_name()
                    .is_some_and(|n| n == "package.json")
            })
            .collect();
        manifests.sort();
        for path in manifests {
            let directory = path.rsplit_once('/').map_or("", |(dir, _)| dir);
            if !directory.is_empty()
                && (!included.is_match(directory) || excluded.is_match(directory))
            {
                continue;
            }
            let manifest = if directory.is_empty() {
                root_manifest.clone()
            } else {
                read_manifest(root, path)?
            };
            let Some(name) = manifest.get("name").and_then(Value::as_str) else {
                continue;
            };
            let entries = ["source", "module", "types", "main"]
                .iter()
                .filter_map(|key| manifest.get(key).and_then(Value::as_str))
                .map(|entry| repo_path(directory, entry))
                .collect::<Result<Vec<_>>>()
                .with_context(|| format!("{path}: invalid package entry"))?;
            packages.entry(name.to_string()).or_default().push(Package {
                root: directory.to_string(),
                entries,
                exports: manifest.get("exports").map(Exports::parse),
            });
        }
        Ok(Self { packages, guards })
    }

    pub fn resolve(
        &self,
        module: &str,
        indexed: &HashSet<String>,
    ) -> Option<Result<Vec<String>, String>> {
        let name = module
            .split('/')
            .take(if module.starts_with('@') { 2 } else { 1 })
            .collect::<Vec<_>>()
            .join("/");
        let packages = self.packages.get(&name)?;
        if packages.len() != 1 {
            return Some(Err(format!(
                "workspace package '{name}' has multiple declarations"
            )));
        }
        let package = &packages[0];
        let subpath = module
            .strip_prefix(&name)
            .unwrap_or("")
            .strip_prefix('/')
            .unwrap_or("");
        if subpath
            .split('/')
            .any(|p| matches!(p, "." | ".." | "node_modules"))
            || subpath.contains(['\\', '%'])
            || subpath.starts_with('/')
            || subpath.contains("//")
        {
            return Some(Err(format!("invalid workspace package subpath: {module}")));
        }
        if let Some(exports) = &package.exports {
            let key = if subpath.is_empty() {
                ".".to_string()
            } else {
                format!("./{subpath}")
            };
            let target = match exports.target(&key, None) {
                Ok(target) => target,
                Err(reason) => return Some(Err(format!("workspace package '{name}': {reason}"))),
            };
            let target = match repo_path(&package.root, &target) {
                Ok(target) => target,
                Err(error) => return Some(Err(error.to_string())),
            };
            // Exports designate a file, with no directory or extension guessing.
            return Some(if indexed.contains(&target) {
                Ok(vec![target])
            } else {
                Err(format!(
                    "workspace package '{name}' exports '{target}', but it is not indexed"
                ))
            });
        }
        let expected = if !subpath.is_empty() {
            repo_path(&package.root, subpath).map(|p| vec![p])
        } else if !package.entries.is_empty() {
            Ok(package.entries.clone())
        } else {
            ["src/index", "index"]
                .iter()
                .map(|p| repo_path(&package.root, p))
                .collect::<Result<Vec<_>>>()
        };
        let expected = match expected {
            Ok(paths) => paths,
            Err(error) => return Some(Err(error.to_string())),
        };
        let candidates = expected.iter().find_map(|target| {
            let candidates = files_at_path(target, indexed);
            (!candidates.is_empty()).then_some(candidates)
        });
        Some(candidates.ok_or_else(|| {
            format!("workspace package '{name}' matched, but no indexed entry file was found")
        }))
    }
}

fn read_manifest(root: &Path, path: &str) -> Result<Value> {
    let manifest: Value = serde_json::from_slice(
        &std::fs::read(root.join(path)).with_context(|| format!("cannot read {path}"))?,
    )
    .with_context(|| format!("cannot parse {path}"))?;
    ensure!(manifest.is_object(), "{path} must be an object");
    Ok(manifest)
}

fn unwrap_node(mut node: tree_sitter::Node<'_>) -> tree_sitter::Node<'_> {
    while matches!(
        node.kind(),
        "stream" | "document" | "block_node" | "flow_node" | "block_sequence_item"
    ) {
        let mut cursor = node.walk();
        let Some(child) = node
            .named_children(&mut cursor)
            .find(|n| n.kind() != "comment")
        else {
            break;
        };
        node = child;
    }
    node
}

fn scalar(node: tree_sitter::Node<'_>, bytes: &[u8]) -> Result<String> {
    let node = unwrap_node(node);
    let text = node.utf8_text(bytes)?;
    ensure!(
        !text.contains(['\n', '\r']),
        "multiline workspace patterns are unsupported"
    );
    match node.kind() {
        "single_quote_scalar" => Ok(text[1..text.len() - 1].replace("''", "'")),
        "double_quote_scalar" => {
            serde_json::from_str(text).context("unsupported quoted workspace pattern")
        }
        "plain_scalar"
            if node
                .named_child(0)
                .is_some_and(|n| n.kind() == "string_scalar") =>
        {
            Ok(text.to_string())
        }
        _ => bail!("workspace patterns must be plain or quoted strings"),
    }
}

fn pnpm_patterns(bytes: &[u8]) -> Result<Vec<String>> {
    let mut parser = tree_sitter::Parser::new();
    parser.set_language(&tree_sitter_yaml::LANGUAGE.into())?;
    let tree = parser
        .parse(bytes, None)
        .context("cannot parse workspace YAML")?;
    ensure!(!tree.root_node().has_error(), "invalid workspace YAML");
    let mut cursor = tree.root_node().walk();
    let documents = tree
        .root_node()
        .named_children(&mut cursor)
        .filter(|n| n.kind() == "document")
        .count();
    ensure!(
        documents <= 1,
        "multiple workspace YAML documents are unsupported"
    );
    if documents == 0 {
        return Ok(Vec::new());
    }
    let mapping = unwrap_node(tree.root_node());
    ensure!(
        matches!(mapping.kind(), "block_mapping" | "flow_mapping"),
        "workspace YAML must be a mapping"
    );
    let mut cursor = mapping.walk();
    for pair in mapping
        .named_children(&mut cursor)
        .filter(|n| n.kind() != "comment")
    {
        let Some(key) = pair.child_by_field_name("key") else {
            continue;
        };
        if scalar(key, bytes)? != "packages" {
            continue;
        }
        let value = unwrap_node(
            pair.child_by_field_name("value")
                .context("missing packages list")?,
        );
        ensure!(
            matches!(value.kind(), "block_sequence" | "flow_sequence"),
            "packages must be a sequence of strings"
        );
        let mut cursor = value.walk();
        return value
            .named_children(&mut cursor)
            .filter(|n| n.kind() != "comment")
            .map(|n| scalar(n, bytes))
            .collect();
    }
    Ok(Vec::new())
}
