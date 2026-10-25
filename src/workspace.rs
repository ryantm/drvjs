//! Project a frozen pnpm lockfile onto selected workspace dependency closures.
use anyhow::{Context, Result, bail, ensure};
use serde_json::{Value, json};
use std::{
    collections::{BTreeMap, BTreeSet, VecDeque},
    fs,
    path::{Component, Path, PathBuf},
};
use walkdir::WalkDir;

pub struct Selection {
    pub lock: String,
    pub config: Option<Value>,
    pub manifests: BTreeMap<String, Value>,
    pub importers: BTreeSet<String>,
    pub all_importers: BTreeSet<String>,
}

fn normalized(base: &str, relative: &str) -> Result<String> {
    let mut parts = Vec::new();
    for component in Path::new(base).join(relative).components() {
        match component {
            Component::Normal(p) => {
                parts.push(p.to_str().context("non-UTF8 workspace path")?.to_owned())
            }
            Component::CurDir => {}
            Component::ParentDir => {
                ensure!(parts.pop().is_some(), "workspace link escapes project root");
            }
            _ => bail!("absolute workspace paths are unsupported"),
        }
    }
    Ok(if parts.is_empty() {
        ".".into()
    } else {
        parts.join("/")
    })
}

fn edges(value: &Value, importer: bool) -> Result<Vec<(String, String)>> {
    let mut result = Vec::new();
    for section in ["dependencies", "optionalDependencies", "devDependencies"] {
        if let Some(deps) = value[section].as_object() {
            for (name, dep) in deps {
                let version = if importer { &dep["version"] } else { dep };
                result.push((
                    name.clone(),
                    version
                        .as_str()
                        .context("invalid lockfile dependency version")?
                        .into(),
                ));
            }
        }
    }
    Ok(result)
}

pub fn select(project: &Path, input: &str, filters: &[String]) -> Result<Selection> {
    let mut lock: Value = serde_yaml_ng::from_str(input)?;
    let importers = lock["importers"]
        .as_object()
        .context("lockfile has no importers")?
        .clone();
    ensure!(!importers.is_empty(), "lockfile has no importers");
    let mut manifests = BTreeMap::new();
    let canonical_root = project.canonicalize()?;
    for path in importers.keys() {
        ensure!(
            normalized(".", path)? == *path,
            "noncanonical workspace importer: {path}"
        );
        let file = project.join(path).join("package.json");
        ensure!(
            file.canonicalize()?.starts_with(&canonical_root),
            "workspace manifest escapes project root"
        );
        let manifest: Value = serde_json::from_slice(
            &fs::read(file).with_context(|| format!("reading {path}/package.json"))?,
        )?;
        if let Some(settings) = manifest.get("pnpm") {
            ensure!(
                path == ".",
                "pnpm settings in nested manifests are unsupported: {path}"
            );
            crate::config::validate_settings(settings)?;
        }
        manifests.insert(path.clone(), manifest);
    }
    let config_file = project.join("pnpm-workspace.yaml");
    let mut config: Option<Value> = if config_file.exists() {
        Some(serde_yaml_ng::from_slice(&fs::read(config_file)?)?)
    } else {
        None
    };
    ensure!(
        config.is_some() || (importers.len() == 1 && importers.contains_key(".")),
        "multiple importers require pnpm-workspace.yaml"
    );
    if let Some(config) = &config {
        crate::config::validate_settings(config)?;
    }
    let root_manifest = manifests.get(".").cloned().unwrap_or(json!({}));
    for importer in importers.values() {
        for key in ["packageManagerDependencies", "configDependencies"] {
            ensure!(
                importer
                    .get(key)
                    .and_then(Value::as_object)
                    .is_none_or(|v| v.is_empty()),
                "pnpm split lockfiles with {key} are not yet supported"
            );
        }
    }
    let all_importers: BTreeSet<_> = importers.keys().cloned().collect();
    let mut queue = VecDeque::new();
    if filters.is_empty() {
        for path in importers.keys() {
            queue.push_back((true, path.clone()));
        }
    } else {
        for filter in filters {
            // Exact names or relative paths; closure is always included.
            let name = filter.strip_suffix("...").unwrap_or(filter);
            let matches: Vec<_> = manifests
                .iter()
                .filter(|(path, manifest)| {
                    path.as_str() == name.trim_start_matches("./")
                        || (name == "." && path.as_str() == ".")
                        || manifest["name"].as_str() == Some(name)
                })
                .map(|(path, _)| path.clone())
                .collect();
            ensure!(
                matches.len() == 1,
                "filter {filter:?} must match exactly one workspace name or path (matched {})",
                matches.len()
            );
            queue.push_back((true, matches[0].clone()));
        }
    }
    let snapshots = lock["snapshots"].as_object().cloned().unwrap_or_default();
    let mut selected = BTreeSet::new();
    let mut reached = BTreeSet::new();
    while let Some((is_importer, id)) = queue.pop_front() {
        if !(if is_importer {
            selected.insert(id.clone())
        } else {
            reached.insert(id.clone())
        }) {
            continue;
        }
        if is_importer && id != "." {
            for config in [".npmrc", ".pnpmfile.cjs"] {
                ensure!(
                    !project.join(&id).join(config).exists(),
                    "nested {id}/{config} is not supported"
                );
            }
        }
        let value = if is_importer {
            importers.get(&id)
        } else {
            snapshots.get(&id)
        }
        .with_context(|| {
            format!(
                "missing {} {id}",
                if is_importer { "importer" } else { "snapshot" }
            )
        })?;
        for (name, version) in edges(value, is_importer)? {
            if let Some(link) = version.strip_prefix("link:") {
                let target = normalized(if is_importer { &id } else { "." }, link)?;
                ensure!(
                    importers.contains_key(&target),
                    "{id}: local link targets a non-workspace package: {version}"
                );
                queue.push_back((true, target));
            } else {
                ensure!(
                    !version.starts_with("file:"),
                    "file/injected workspace dependencies are not yet supported: {id} -> {version}"
                );
                let candidate = format!("{name}@{version}");
                let target = if snapshots.contains_key(&candidate) {
                    candidate
                } else if snapshots.contains_key(&version) {
                    version
                } else {
                    bail!("{id}: missing dependency snapshot {candidate}")
                };
                queue.push_back((false, target));
            }
        }
    }
    // Full single-package exports preserve the original lock bytes for backwards compatibility.
    if config.is_none() && filters.is_empty() {
        return Ok(Selection {
            lock: input.into(),
            config,
            manifests,
            importers: selected,
            all_importers,
        });
    }
    let package_ids: BTreeSet<_> = reached
        .iter()
        .map(|id| id.split('(').next().unwrap().to_string())
        .collect();
    for id in &package_ids {
        ensure!(
            lock["packages"].get(id).is_some(),
            "missing package resolution: {id}"
        );
    }
    let mut retained = serde_json::Map::new();
    for path in &selected {
        retained.insert(path.clone(), importers[path].clone());
    }
    // pnpm always includes the root. Keep it empty unless explicitly selected.
    retained.entry(".".to_owned()).or_insert(json!({}));
    lock["importers"] = Value::Object(retained);
    lock["packages"] = Value::Object(
        lock["packages"]
            .as_object()
            .cloned()
            .unwrap_or_default()
            .into_iter()
            .filter(|(id, _)| package_ids.contains(id))
            .collect(),
    );
    lock["snapshots"] = Value::Object(
        snapshots
            .into_iter()
            .filter(|(id, _)| reached.contains(id))
            .collect(),
    );
    manifests.retain(|path, _| selected.contains(path));
    manifests
        .entry(".".into())
        .or_insert(json!({"name":"drvjs-workspace-root","private":true}));
    if !selected.contains(".") {
        for field in ["pnpm", "engines", "resolutions"] {
            if let Some(value) = root_manifest.get(field) {
                manifests.get_mut(".").unwrap()[field] = value.clone();
            }
        }
    }
    // Patch identities include their content hash in snapshot peer suffixes.
    // Prune declarations and lock metadata together, retaining frozen validation.
    if let Some(patches) = lock
        .get_mut("patchedDependencies")
        .and_then(Value::as_object_mut)
    {
        patches.retain(|_, patch| {
            patch["hash"].as_str().is_some_and(|hash| {
                reached
                    .iter()
                    .any(|id| id.contains(&format!("patch_hash={hash}")))
            })
        });
        let retained: BTreeSet<_> = patches.keys().cloned().collect();
        if let Some(patches) = manifests
            .get_mut(".")
            .and_then(|root| root.get_mut("pnpm"))
            .and_then(|settings| settings.get_mut("patchedDependencies"))
            .and_then(Value::as_object_mut)
        {
            patches.retain(|name, _| retained.contains(name));
        }
        if let Some(patches) = config
            .as_mut()
            .and_then(|settings| settings.get_mut("patchedDependencies"))
            .and_then(Value::as_object_mut)
        {
            patches.retain(|name, _| retained.contains(name));
        }
    }
    if let Some(config) = &mut config {
        config["packages"] = json!(
            selected
                .iter()
                .filter(|path| *path != ".")
                .collect::<Vec<_>>()
        );
        // Retain only catalogs referenced by selected manifests. Unrelated catalog
        // edits must not change the resulting Nix derivation.
        let mut used: BTreeMap<String, BTreeSet<String>> = BTreeMap::new();
        for manifest in manifests.values() {
            for section in [
                "dependencies",
                "devDependencies",
                "optionalDependencies",
                "peerDependencies",
            ] {
                if let Some(deps) = manifest[section].as_object() {
                    for (name, spec) in deps {
                        if let Some(catalog) =
                            spec.as_str().and_then(|s| s.strip_prefix("catalog:"))
                        {
                            used.entry(
                                if catalog.is_empty() {
                                    "default"
                                } else {
                                    catalog
                                }
                                .into(),
                            )
                            .or_default()
                            .insert(name.clone());
                        }
                    }
                }
            }
        }
        if let Some(catalogs) = lock["catalogs"].as_object_mut() {
            catalogs.retain(|catalog, entries| {
                if let Some(names) = used.get(catalog) {
                    if let Some(entries) = entries.as_object_mut() {
                        entries.retain(|name, _| names.contains(name));
                    }
                    true
                } else {
                    false
                }
            });
            if catalogs.is_empty() {
                lock.as_object_mut().unwrap().remove("catalogs");
            }
        }
        if let Some(entries) = config.get_mut("catalog").and_then(Value::as_object_mut) {
            entries.retain(|name, _| {
                used.get("default")
                    .is_some_and(|names| names.contains(name))
            });
        }
        if let Some(catalogs) = config.get_mut("catalogs").and_then(Value::as_object_mut) {
            catalogs.retain(|catalog, entries| {
                if let Some(names) = used.get(catalog) {
                    if let Some(entries) = entries.as_object_mut() {
                        entries.retain(|name, _| names.contains(name));
                    }
                    true
                } else {
                    false
                }
            });
        }
    }
    Ok(Selection {
        lock: serde_yaml_ng::to_string(&lock)?,
        config,
        manifests,
        importers: selected,
        all_importers,
    })
}

pub fn copy_tree(source: &Path, destination: &Path) -> Result<()> {
    for entry in WalkDir::new(source).into_iter().filter_entry(|e| {
        !["node_modules", ".pnpm-store", ".git"]
            .iter()
            .any(|n| e.file_name() == *n)
    }) {
        let entry = entry?;
        let target = destination.join(entry.path().strip_prefix(source)?);
        if entry.file_type().is_dir() {
            fs::create_dir_all(&target)?;
        } else {
            ensure!(
                entry.file_type().is_file(),
                "workspace source symlinks are not yet supported: {}",
                entry.path().display()
            );
            fs::copy(entry.path(), &target)?;
            // Store inputs are immutable; scratch reconstructions must be writable.
            use std::os::unix::fs::PermissionsExt;
            let mode = fs::metadata(&target)?.permissions().mode() | 0o200;
            fs::set_permissions(target, fs::Permissions::from_mode(mode))?;
        }
    }
    Ok(())
}

pub fn write_project(project: &Path, output: &Path, selection: &Selection) -> Result<()> {
    fs::create_dir_all(output)?;
    let staging_root = output
        .parent()
        .context("missing staging directory")?
        .canonicalize()?;
    for path in selection
        .importers
        .iter()
        .filter(|_| selection.config.is_some())
    {
        // Each workspace gets its own sources; skip nested importers and build
        // outputs so selecting a parent cannot accidentally capture siblings.
        let source = project.join(path);
        for entry in WalkDir::new(&source).into_iter().filter_entry(|entry| {
            if ["node_modules", ".pnpm-store", ".git", ".npmrc"]
                .iter()
                .any(|n| entry.file_name() == *n)
            {
                return false;
            }
            if entry.path().starts_with(&staging_root) {
                return false;
            }
            if entry.path() == source {
                return true;
            }
            let relative = entry
                .path()
                .strip_prefix(project)
                .unwrap_or(entry.path())
                .to_string_lossy();
            !selection.all_importers.contains(relative.as_ref())
        }) {
            let entry = entry?;
            let relative = entry.path().strip_prefix(project)?;
            if ["pnpm-lock.yaml", "pnpm-workspace.yaml"]
                .iter()
                .any(|name| relative == Path::new(name))
            {
                continue;
            }
            // A single project only needs its manifest to install dependencies.
            if selection.config.is_none() && relative != Path::new("package.json") {
                continue;
            }
            let target = output.join(relative);
            if entry.file_type().is_dir() {
                fs::create_dir_all(&target)?;
            } else {
                ensure!(
                    entry.file_type().is_file(),
                    "workspace source symlinks are not yet supported: {}",
                    relative.display()
                );
                if let Some(parent) = target.parent() {
                    fs::create_dir_all(parent)?;
                }
                fs::copy(entry.path(), target)?;
            }
        }
    }
    for (path, manifest) in &selection.manifests {
        fs::create_dir_all(output.join(path))?;
        fs::write(
            output.join(path).join("package.json"),
            serde_json::to_vec_pretty(manifest)?,
        )?;
    }
    let npmrc = crate::config::npmrc(project)?;
    if project.join(".npmrc").exists() {
        fs::write(
            output.join(".npmrc"),
            npmrc
                .iter()
                .map(|(key, value)| format!("{key}={value}\n"))
                .collect::<String>(),
        )?;
    }
    copy_support(project, output, &selection.lock)?;
    fs::write(output.join("pnpm-lock.yaml"), &selection.lock)?;
    if let Some(config) = &selection.config {
        fs::write(
            output.join("pnpm-workspace.yaml"),
            serde_yaml_ng::to_string(config)?,
        )?;
    }
    Ok(())
}

pub fn copy_support(project: &Path, output: &Path, lock: &str) -> Result<()> {
    let data: Value = serde_yaml_ng::from_str(lock)?;
    let mut paths = vec![
        "package.json".to_owned(),
        "pnpm-workspace.yaml".into(),
        ".pnpmfile.cjs".into(),
    ];
    if let Some(patches) = data["patchedDependencies"].as_object() {
        for patch in patches.values() {
            let relative = patch["path"].as_str().context("patch missing path")?;
            crate::config::safe_file(project, relative)
                .with_context(|| format!("reading declared patch {relative}"))?;
            paths.push(
                patch["path"]
                    .as_str()
                    .context("patch missing path")?
                    .to_owned(),
            );
        }
    }
    for relative in paths {
        if !project.join(&relative).exists() {
            continue;
        }
        // Root package/config are already projected by write_project. Other
        // callers (the temporary store seed) need exact copies of them.
        if output.join(&relative).exists() {
            continue;
        }
        let source = crate::config::safe_file(project, &relative)?;
        let target = output.join(relative);
        fs::create_dir_all(target.parent().unwrap())?;
        fs::copy(source, target)?;
    }
    Ok(())
}

pub fn importer_paths(project: &Path) -> Result<Vec<PathBuf>> {
    let lock: Value = serde_yaml_ng::from_slice(&fs::read(project.join("pnpm-lock.yaml"))?)?;
    lock["importers"]
        .as_object()
        .context("missing importers")?
        .keys()
        .map(|path| {
            ensure!(normalized(".", path)? == *path, "invalid importer path");
            Ok(PathBuf::from(path))
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn link_paths() {
        assert_eq!(normalized("packages/a", "../b").unwrap(), "packages/b");
        assert!(normalized("packages/a", "../../../outside").is_err());
        assert!(normalized(".", "/absolute").is_err());
    }
}

#[cfg(test)]
mod closure_tests {
    use super::*;
    fn fixture() -> (tempfile::TempDir, Value) {
        let root = tempfile::tempdir().unwrap();
        for (path, name) in [
            (".", "root"),
            ("packages/a", "@test/a"),
            ("packages/b", "@test/b"),
            ("packages/other", "other"),
        ] {
            fs::create_dir_all(root.path().join(path)).unwrap();
            fs::write(
                root.path().join(path).join("package.json"),
                json!({"name": name, "version":"1.0.0"}).to_string(),
            )
            .unwrap();
        }
        fs::write(
            root.path().join("pnpm-workspace.yaml"),
            "packages: ['packages/*']\n",
        )
        .unwrap();
        let lock = json!({"lockfileVersion":"9.0", "importers": {
            ".":{"devDependencies":{"unrelated":{"specifier":"1.0.0","version":"1.0.0"}}},
            "packages/a":{"dependencies":{"@test/b":{"version":"link:../b"},"alias":{"version":"real@1.0.0(peer@2.0.0)"}}},
            "packages/b":{"dependencies":{"@test/a":{"version":"link:../a"}}},
            "packages/other":{}
        }, "packages": {"real@1.0.0":{}, "peer@2.0.0":{},"optional@1.0.0":{},"unrelated@1.0.0":{}},
        "snapshots": {
            "real@1.0.0(peer@2.0.0)":{"dependencies":{"peer":"2.0.0"},"optionalDependencies":{"optional":"1.0.0"}},
            "peer@2.0.0":{},"optional@1.0.0":{},"unrelated@1.0.0":{}
        }});
        (root, lock)
    }
    #[test]
    fn aliases_peers_optional_cycles_and_pruning() {
        let (root, lock) = fixture();
        let selected = select(root.path(), &lock.to_string(), &["@test/a".into()]).unwrap();
        assert_eq!(
            selected.importers,
            BTreeSet::from(["packages/a".into(), "packages/b".into()])
        );
        let pruned: Value = serde_yaml_ng::from_str(&selected.lock).unwrap();
        assert_eq!(pruned["snapshots"].as_object().unwrap().len(), 3);
        assert!(pruned["packages"].get("unrelated@1.0.0").is_none());
        assert_eq!(pruned["importers"]["."], json!({}));
        assert_eq!(selected.manifests["."]["name"], "drvjs-workspace-root");
    }
    #[test]
    fn subset_preserves_root_settings_and_only_reachable_patches() {
        let (root, mut lock) = fixture();
        let manifest = json!({"name":"root", "resolutions":{"peer":"2.0.0"},
        "engines":{"node":">=22"}, "pnpm":{
            "packageExtensions":{"real@1.0.0":{"dependencies":{"peer":"2.0.0"}}},
            "patchedDependencies":{"peer@2.0.0":"patches/peer.patch", "unrelated@1.0.0":"patches/unused.patch"}
        }});
        fs::write(root.path().join("package.json"), manifest.to_string()).unwrap();
        lock["snapshots"]["real@1.0.0(peer@2.0.0)"]["dependencies"]["peer"] =
            "2.0.0(patch_hash=abc)".into();
        lock["snapshots"]
            .as_object_mut()
            .unwrap()
            .remove("peer@2.0.0");
        lock["snapshots"]["peer@2.0.0(patch_hash=abc)"] = json!({});
        lock["patchedDependencies"] = json!({"peer@2.0.0":{"hash":"abc","path":"patches/peer.patch"},
            "unrelated@1.0.0":{"hash":"def","path":"patches/unused.patch"}});
        let selection = select(root.path(), &lock.to_string(), &["@test/a".into()]).unwrap();
        assert_eq!(
            selection.manifests["."]["resolutions"],
            manifest["resolutions"]
        );
        assert_eq!(
            selection.manifests["."]["pnpm"]["packageExtensions"],
            manifest["pnpm"]["packageExtensions"]
        );
        assert_eq!(
            selection.manifests["."]["pnpm"]["patchedDependencies"],
            json!({"peer@2.0.0":"patches/peer.patch"})
        );
        let projected: Value = serde_yaml_ng::from_str(&selection.lock).unwrap();
        assert_eq!(
            projected["patchedDependencies"].as_object().unwrap().len(),
            1
        );
    }
    #[test]
    fn excluded_workspace_config_does_not_block_subset() {
        let (root, lock) = fixture();
        fs::write(
            root.path().join("packages/other/.npmrc"),
            "node-linker=hoisted\n",
        )
        .unwrap();
        assert!(select(root.path(), &lock.to_string(), &["@test/a".into()]).is_ok());
        assert!(select(root.path(), &lock.to_string(), &["other".into()]).is_err());
    }
    #[test]
    fn repeated_filters_and_root_opt_in() {
        let (root, lock) = fixture();
        let selected = select(
            root.path(),
            &lock.to_string(),
            &["packages/a".into(), ".".into()],
        )
        .unwrap();
        let pruned: Value = serde_yaml_ng::from_str(&selected.lock).unwrap();
        assert!(pruned["packages"].get("unrelated@1.0.0").is_some());
        assert!(!selected.importers.contains("packages/other"));
    }
    #[test]
    fn unknown_filters_and_broken_edges_fail() {
        let (root, mut lock) = fixture();
        assert!(select(root.path(), &lock.to_string(), &["missing".into()]).is_err());
        lock["snapshots"]
            .as_object_mut()
            .unwrap()
            .remove("peer@2.0.0");
        assert!(select(root.path(), &lock.to_string(), &["@test/a".into()]).is_err());
    }
    #[test]
    fn unrelated_changes_do_not_change_projection() {
        let (root, mut lock) = fixture();
        let before = select(root.path(), &lock.to_string(), &["@test/a".into()]).unwrap();
        lock["packages"]["unrelated@1.0.0"] = json!({"changed":true});
        lock["importers"]["packages/other"] =
            json!({"dependencies":{"unused":{"version":"9.0.0"}}});
        let after = select(root.path(), &lock.to_string(), &["@test/a".into()]).unwrap();
        assert_eq!(before.lock, after.lock);
    }
    #[test]
    fn relative_staging_inside_root_is_excluded() {
        let root = tempfile::tempdir_in(".").unwrap();
        fs::write(root.path().join("package.json"), "{\"name\":\"root\"}").unwrap();
        fs::write(root.path().join("pnpm-workspace.yaml"), "packages: []\n").unwrap();
        let canonical = root.path().canonicalize().unwrap();
        let lock =
            json!({"lockfileVersion":"9.0", "importers":{".":{}}, "packages":{}, "snapshots":{}});
        let selected = select(&canonical, &lock.to_string(), &[".".into()]).unwrap();
        let output = root.path().join(".staging/project");
        write_project(&canonical, &output, &selected).unwrap();
        assert!(output.join("package.json").exists());
        assert!(!output.join(".staging").exists());
    }
    #[test]
    fn catalog_projection() {
        let (root, mut lock) = fixture();
        fs::write(
            root.path().join("pnpm-workspace.yaml"),
            "packages: ['packages/*']\ncatalog: { real: '^1.0.0', unused: '9.0.0' }\n",
        )
        .unwrap();
        fs::write(
            root.path().join("packages/a/package.json"),
            json!({"name":"@test/a", "dependencies":{"real":"catalog:"}}).to_string(),
        )
        .unwrap();
        lock["catalogs"] = json!({"default":{"real":{"specifier":"^1.0.0","version":"1.0.0"},"unused":{"specifier":"9.0.0","version":"9.0.0"}}});
        let selected = select(root.path(), &lock.to_string(), &["@test/a".into()]).unwrap();
        assert_eq!(
            selected.config.unwrap()["catalog"],
            json!({"real":"^1.0.0"})
        );
        let pruned: Value = serde_yaml_ng::from_str(&selected.lock).unwrap();
        assert_eq!(pruned["catalogs"]["default"].as_object().unwrap().len(), 1);
    }
}
