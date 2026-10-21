use super::*;
use std::collections::BTreeSet;

#[derive(Serialize)]
struct Package {
    name: String,
    ecosystem: String,
    manifest: String,
    declared: bool,
    version: Option<String>,
    resolution: String,
    match_method: String,
    importers: BTreeSet<String>,
    importers_total: usize,
    importers_truncated: bool,
    #[serde(skip_serializing_if = "Option::is_none")]
    resolution_reason: Option<String>,
}

fn declaration(name: &str, ecosystem: &str, manifest: &str, version: String) -> Package {
    Package {
        name: name.into(),
        ecosystem: ecosystem.into(),
        manifest: manifest.into(),
        declared: true,
        version: Some(version),
        resolution: "external".into(),
        match_method: "exact".into(),
        importers: BTreeSet::new(),
        importers_total: 0,
        importers_truncated: false,
        resolution_reason: None,
    }
}

fn manifest_packages(path: &str, body: &str) -> Result<Vec<Package>> {
    let mut packages = Vec::new();
    let filename = path.rsplit('/').next().unwrap_or(path);
    if filename == "package.json" {
        let value: serde_json::Value = serde_json::from_str(body)?;
        for group in [
            "dependencies",
            "devDependencies",
            "peerDependencies",
            "optionalDependencies",
        ] {
            if let Some(deps) = value[group].as_object() {
                for (name, version) in deps {
                    packages.push(declaration(
                        name,
                        "npm",
                        path,
                        version.as_str().unwrap_or_default().into(),
                    ));
                }
            }
        }
    } else if matches!(filename, "Cargo.toml" | "pyproject.toml") {
        let value: toml_edit::DocumentMut = body.parse()?;
        if filename == "Cargo.toml" {
            fn cargo_deps(item: &toml_edit::Item, path: &str, out: &mut Vec<Package>) {
                if let Some(table) = item.as_table_like() {
                    for (name, item) in table.iter() {
                        let version = item
                            .as_str()
                            .map(str::to_string)
                            .unwrap_or_else(|| item.to_string().trim().to_string());
                        let mut package = declaration(name, "cargo", path, version);
                        if item.get("path").is_some()
                            || item.get("workspace").and_then(|i| i.as_bool()) == Some(true)
                        {
                            package.resolution = "internal-or-workspace".into();
                        }
                        if let Some(actual) = item.get("package").and_then(|i| i.as_str()) {
                            package.match_method = format!("manifest-alias:{actual}");
                        }
                        out.push(package);
                    }
                }
            }
            for group in ["dependencies", "dev-dependencies", "build-dependencies"] {
                if let Some(deps) = value.get(group) {
                    cargo_deps(deps, path, &mut packages);
                }
            }
            if let Some(targets) = value.get("target").and_then(|i| i.as_table()) {
                for (_, target) in targets.iter() {
                    for group in ["dependencies", "dev-dependencies", "build-dependencies"] {
                        if let Some(deps) = target.get(group) {
                            cargo_deps(deps, path, &mut packages);
                        }
                    }
                }
            }
        } else {
            let mut requirements = Vec::new();
            if let Some(deps) = value
                .get("project")
                .and_then(|i| i.get("dependencies"))
                .and_then(|i| i.as_array())
            {
                requirements.extend(deps.iter().filter_map(|v| v.as_str()));
            }
            if let Some(groups) = value
                .get("project")
                .and_then(|i| i.get("optional-dependencies"))
                .and_then(|i| i.as_table())
            {
                for (_, deps) in groups.iter() {
                    if let Some(deps) = deps.as_array() {
                        requirements.extend(deps.iter().filter_map(|v| v.as_str()));
                    }
                }
            }
            for requirement in requirements {
                let name = requirement
                    .split(|c: char| !(c.is_alphanumeric() || matches!(c, '-' | '_' | '.')))
                    .next()
                    .unwrap_or_default();
                if !name.is_empty() {
                    packages.push(declaration(name, "python", path, requirement.to_string()));
                }
            }
        }
    } else if filename == "go.mod" {
        let mut require = false;
        for raw in body.lines() {
            let line = raw.split("//").next().unwrap_or_default().trim();
            if line == "require (" {
                require = true;
                continue;
            }
            if line == ")" {
                require = false;
                continue;
            }
            let line = if let Some(line) = line.strip_prefix("require ") {
                line
            } else if require {
                line
            } else {
                continue;
            };
            let mut words = line.split_whitespace();
            if let (Some(name), Some(version)) = (words.next(), words.next()) {
                packages.push(declaration(name, "go", path, version.into()));
            }
        }
    }
    Ok(packages)
}

fn ecosystem(language: &str) -> Option<&'static str> {
    match language {
        "javascript" | "typescript" | "tsx" | "svelte" | "vue" => Some("npm"),
        "rust" => Some("cargo"),
        "python" => Some("python"),
        "go" => Some("go"),
        _ => None,
    }
}

fn import_matches(ecosystem: &str, package: &str, module: &str) -> bool {
    match ecosystem {
        "npm" => {
            module == package
                || module
                    .strip_prefix(package)
                    .is_some_and(|s| s.starts_with('/') || s.starts_with('.'))
        }
        "cargo" => module.split("::").next() == Some(package.replace('-', "_").as_str()),
        "python" => {
            module.split('.').next() == Some(package.replace('-', "_").to_lowercase().as_str())
        }
        "go" => {
            module == package
                || module
                    .strip_prefix(package)
                    .is_some_and(|s| s.starts_with('/'))
        }
        _ => false,
    }
}

pub fn cmd_packages(
    cwd: &Path,
    name: Option<&str>,
    undeclared: bool,
    unimported: bool,
    limit: usize,
    out: Out,
) -> Result<()> {
    let s = Session::open(cwd, false, out)?;
    let graph = s.dep_graph()?;
    let paths = all_file_paths(&s.db.conn)?;
    let mut packages = Vec::new();
    let mut manifests = Vec::new();
    for path in paths.iter().filter(|p| {
        matches!(
            p.rsplit('/').next(),
            Some("package.json" | "Cargo.toml" | "pyproject.toml" | "go.mod")
        )
    }) {
        let body = std::fs::read_to_string(s.repo.root.join(path))?;
        packages.extend(
            manifest_packages(path, &body)
                .map_err(|e| anyhow!("cannot read dependencies from {path}: {e}"))?,
        );
        let eco = match path.rsplit('/').next().unwrap() {
            "package.json" => "npm",
            "Cargo.toml" => "cargo",
            "pyproject.toml" => "python",
            _ => "go",
        };
        manifests.push((path.clone(), eco));
    }
    packages.sort_by(|a, b| (&a.manifest, &a.name).cmp(&(&b.manifest, &b.name)));
    packages.dedup_by(|a, b| a.manifest == b.manifest && a.name == b.name);
    let mut stmt = s.db.conn.prepare("SELECT DISTINCT f.path, f.name, files.language FROM facts f JOIN files ON f.path = files.path WHERE f.kind = 'import' AND f.is_derived = 0 ORDER BY f.path, f.name")?;
    let imports = stmt.query_map([], |r| {
        Ok((
            r.get::<_, String>(0)?,
            r.get::<_, String>(1)?,
            r.get::<_, String>(2)?,
        ))
    })?;
    for row in imports {
        let (path, module, language) = row?;
        let Some(eco) = ecosystem(&language) else {
            continue;
        };
        let manifest = manifests
            .iter()
            .filter(|(m, e)| {
                *e == eco
                    && m.rsplit_once('/')
                        .is_none_or(|(dir, _)| path.starts_with(&format!("{dir}/")))
            })
            .max_by_key(|(m, _)| m.len())
            .map(|(m, _)| m.as_str())
            .unwrap_or("");
        let matched: Vec<_> = packages
            .iter()
            .enumerate()
            .filter(|(_, p)| p.manifest == manifest && import_matches(eco, &p.name, &module))
            .map(|(i, _)| i)
            .collect();
        if !matched.is_empty() {
            for i in matched {
                packages[i].importers.insert(path.clone());
                if graph.resolves(&path, &module) {
                    packages[i].resolution = "internal".into();
                }
                if eco == "python" {
                    packages[i].match_method = "normalized-name-heuristic".into();
                }
            }
            continue;
        }
        if graph.resolves(&path, &module) {
            continue;
        }
        let unresolved = graph.unresolved_project_reason(&path, &module);
        let name = match eco {
            "npm" => {
                let parts: Vec<_> = module.split('/').collect();
                if module.starts_with('@') && parts.len() > 1 {
                    parts[..2].join("/")
                } else {
                    parts[0].to_string()
                }
            }
            "cargo" => module.split("::").next().unwrap_or_default().to_string(),
            "python" => module.split('.').next().unwrap_or_default().to_string(),
            _ => module.clone(),
        };
        let resolution = if unresolved.is_some()
            || module.starts_with('.')
            || matches!(name.as_str(), "crate" | "self" | "super")
        {
            "unresolved"
        } else {
            "external-or-standard-library"
        };
        if let Some(p) = packages
            .iter_mut()
            .find(|p| !p.declared && p.name == name && p.manifest == manifest && p.ecosystem == eco)
        {
            p.importers.insert(path);
        } else {
            packages.push(Package { name, ecosystem: eco.into(), manifest: manifest.into(), declared: false, version: None,
                resolution: resolution.into(), match_method: "import-prefix-heuristic".into(), importers: BTreeSet::from([path]), importers_total: 0, importers_truncated: false,
                resolution_reason: Some(unresolved.unwrap_or("no matching declaration; may be a standard library, local module, or undeclared dependency").to_string()),
            });
        }
    }
    packages.retain(|p| {
        name.is_none_or(|n| p.name == n)
            && (!undeclared || !p.declared)
            && (!unimported || (p.declared && p.importers.is_empty()))
    });
    packages.sort_by(|a, b| {
        (&a.ecosystem, &a.name, &a.manifest).cmp(&(&b.ecosystem, &b.name, &b.manifest))
    });
    for package in &mut packages {
        package.importers_total = package.importers.len();
        package.importers_truncated = limit > 0 && package.importers_total > limit;
        if package.importers_truncated {
            package.importers = package.importers.iter().take(limit).cloned().collect();
        }
    }
    let note = take_limit(&mut packages, limit);
    s.emit_noted(packages, note)
}
