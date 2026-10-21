use super::config;
use anyhow::{Context, Result};
use serde_json::Value;
use std::collections::HashMap;
use std::path::Path;

#[derive(Default)]
pub(super) struct ProjectResolution {
    aliases: Vec<(String, Vec<String>)>,
    packages: HashMap<String, (String, Vec<String>)>,
}

impl ProjectResolution {
    pub(super) fn load(root: &Path, paths: &[String]) -> Result<Self> {
        let mut result = Self::default();
        let path = root.join("tsconfig.json");
        match std::fs::read(&path) {
            Ok(bytes) => {
                let json = config::parse_jsonc(&bytes)
                    .with_context(|| format!("cannot parse {}", path.display()))?;
                let base = json
                    .pointer("/compilerOptions/baseUrl")
                    .and_then(Value::as_str)
                    .unwrap_or(".");
                if let Some(aliases) = json
                    .pointer("/compilerOptions/paths")
                    .and_then(Value::as_object)
                {
                    for (pattern, targets) in aliases {
                        let targets: Vec<String> = targets
                            .as_array()
                            .into_iter()
                            .flatten()
                            .filter_map(Value::as_str)
                            .map(|target| normalize_path(&Path::new(base).join(target)))
                            .collect();
                        if !targets.is_empty() {
                            result.aliases.push((pattern.clone(), targets));
                        }
                    }
                }
            }
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
            Err(error) => {
                return Err(error).with_context(|| format!("cannot read {}", path.display()))
            }
        }

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
        result.aliases.sort_by(|a, b| a.0.cmp(&b.0));
        Ok(result)
    }

    pub(super) fn targets(
        &self,
        module: &str,
        allow_imported_name: bool,
        paths: &[String],
    ) -> Option<Result<Vec<String>, String>> {
        let mut unresolved_reason = None;
        for spelling in module_spellings(module, allow_imported_name) {
            for (pattern, targets) in &self.aliases {
                let Some(remainder) = wildcard_match(pattern, &spelling) else {
                    continue;
                };
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
