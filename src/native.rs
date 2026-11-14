//! Native assembly for the pinned pnpm isolated node_modules layout.
use crate::native_store as store;
use anyhow::{Context, Result, ensure};
use serde_json::{Value, json};
use sha2::{Digest, Sha256};
use std::{
    collections::{BTreeMap, BTreeSet, VecDeque},
    fs,
    os::unix::fs::{PermissionsExt, symlink},
    path::{Component, Path, PathBuf},
};

type Edges = indexmap::IndexMap<String, String>;
#[derive(Clone)]
struct Package {
    dir: PathBuf,
    children: Edges,
    requires_build: bool,
}
pub fn base_id(id: &str) -> &str {
    id.split('(').next().unwrap()
}
pub fn patch_hash(id: &str) -> Option<&str> {
    id.strip_prefix(base_id(id))?
        .strip_prefix("(patch_hash=")?
        .split(')')
        .next()
}
fn name(id: &str) -> Result<&str> {
    let i = id[1..].find('@').context("invalid package identity")? + 1;
    Ok(&id[..i])
}
pub fn filename(id: &str) -> String {
    let mut s: String = id
        .trim_start_matches('/')
        .chars()
        .map(|c| if "\\/:*?\"<>|#".contains(c) { '+' } else { c })
        .collect();
    if s.contains('(') {
        s = s
            .strip_suffix(')')
            .unwrap_or(&s)
            .replace(")(", "_")
            .replace(['(', ')'], "_");
    }
    if s.len() > 120 || s.to_lowercase() != s {
        s = format!(
            "{}_{}",
            &s[..s.len().min(87)],
            &format!("{:x}", Sha256::digest(s.as_bytes()))[..32]
        );
    }
    s
}
fn reference(alias: &str, value: &str) -> String {
    if value.starts_with("link:") || value.starts_with('@') {
        return value.into();
    }
    if let Some(at) = value.find('@')
        && value.find(':').is_none_or(|colon| at < colon)
        && value.find('(').is_none_or(|bracket| at < bracket)
    {
        value.into()
    } else {
        format!("{alias}@{value}")
    }
}
fn dependencies(value: &Value, importer: bool) -> Edges {
    let mut result = Edges::new();
    for field in ["devDependencies", "dependencies", "optionalDependencies"] {
        if !importer && field == "devDependencies" {
            continue;
        }
        if let Some(deps) = value[field].as_object() {
            for (alias, version) in deps {
                if let Some(v) = if importer {
                    version["version"].as_str()
                } else {
                    version.as_str()
                } {
                    result.insert(alias.clone(), reference(alias, v));
                }
            }
        }
    }
    result
}
fn normalized(path: &Path) -> PathBuf {
    let mut result = PathBuf::new();
    for c in path.components() {
        match c {
            Component::CurDir => {}
            Component::ParentDir => {
                result.pop();
            }
            _ => result.push(c.as_os_str()),
        }
    }
    result
}
fn relative(from: &Path, to: &Path) -> PathBuf {
    let a: Vec<_> = from.components().collect();
    let b: Vec<_> = to.components().collect();
    let common = a.iter().zip(&b).take_while(|(x, y)| x == y).count();
    let mut p = PathBuf::new();
    for _ in common..a.len() {
        p.push("..");
    }
    for c in &b[common..] {
        p.push(c.as_os_str());
    }
    p
}
fn link(target: &Path, path: &Path) -> Result<()> {
    if fs::symlink_metadata(path).is_ok() {
        return Ok(());
    }
    fs::create_dir_all(path.parent().unwrap())?;
    symlink(relative(path.parent().unwrap(), target), path)?;
    Ok(())
}
fn matches_patterns(alias: &str, patterns: &[String]) -> bool {
    let mut matched = false;
    for p in patterns {
        let (neg, p) = p
            .strip_prefix('!')
            .map_or((false, p.as_str()), |s| (true, s));
        let re = format!(
            "^{}$",
            regex::escape(p).replace("\\*", ".*").replace("\\?", ".")
        );
        if regex::Regex::new(&re).unwrap().is_match(alias) {
            if neg {
                return false;
            }
            matched = true;
        }
    }
    matched
}
fn patterns(value: &Value, default: &[&str]) -> Vec<String> {
    value
        .as_array()
        .map(|a| {
            a.iter()
                .filter_map(Value::as_str)
                .map(str::to_owned)
                .collect()
        })
        .unwrap_or_else(|| default.iter().map(|s| s.to_string()).collect())
}
fn supported(package: &Value, settings: &Value, arch: &str) -> bool {
    for (key, current) in [("os", "linux"), ("cpu", arch), ("libc", "glibc")] {
        let Some(values) = package[key].as_array() else {
            continue;
        };
        let requested = settings["supportedArchitectures"][key].as_array();
        let targets: Vec<&str> = requested
            .map(|a| {
                a.iter()
                    .filter_map(Value::as_str)
                    .map(|s| if s == "current" { current } else { s })
                    .collect()
            })
            .unwrap_or_else(|| vec![current]);
        let ok = targets.iter().any(|target| {
            !values
                .iter()
                .any(|v| v.as_str() == Some(&format!("!{target}")))
                && (!values
                    .iter()
                    .any(|v| v.as_str().is_some_and(|s| !s.starts_with('!')))
                    || values
                        .iter()
                        .any(|v| v.as_str() == Some(target) || v.as_str() == Some("any")))
        });
        if !ok {
            return false;
        }
    }
    true
}
fn with_manifest_constraints(metadata: &Value, manifest: &Value) -> Value {
    let mut result = metadata.clone();
    for key in ["os", "cpu", "libc"] {
        // Recorded lock metadata (including hook overrides) remains authoritative.
        if result.get(key).is_none() && manifest[key].is_array() {
            result[key] = manifest[key].clone();
        }
    }
    result
}
fn read_json(p: &Path) -> Result<Value> {
    Ok(serde_json::from_slice(
        &fs::read(p).with_context(|| format!("reading {}", p.display()))?,
    )?)
}
fn settings(root: &Path) -> Result<(Value, Value, BTreeMap<String, String>)> {
    let manifest = read_json(&root.join("package.json"))?;
    let workspace = if root.join("pnpm-workspace.yaml").exists() {
        serde_yaml_ng::from_slice(&fs::read(root.join("pnpm-workspace.yaml"))?)?
    } else {
        json!({})
    };
    let mut result = json!({});
    for key in [
        "overrides",
        "packageExtensions",
        "patchedDependencies",
        "peerDependencyRules",
        "supportedArchitectures",
        "onlyBuiltDependencies",
        "ignoredBuiltDependencies",
        "neverBuiltDependencies",
        "allowedDeprecatedVersions",
        "allowUnusedPatches",
        "ignorePatchFailures",
    ] {
        if let Some(value) = manifest["pnpm"].get(key) {
            result[key] = value.clone();
        }
    }
    if let Some(o) = workspace.as_object() {
        for (k, v) in o {
            result[k] = v.clone();
        }
    }
    let mut overrides = manifest.get("resolutions").cloned().unwrap_or(json!({}));
    if let Some(o) = result["overrides"].as_object() {
        for (k, v) in o {
            overrides[k] = v.clone();
        }
    }
    if overrides.as_object().is_some_and(|o| !o.is_empty()) {
        result["overrides"] = overrides;
    }
    let rc = crate::config::npmrc(root)?;
    for (key, value) in &rc {
        match key.as_str() {
            "hoist-pattern[]" => result["hoistPattern"] = json!([value]),
            "public-hoist-pattern[]" => result["publicHoistPattern"] = json!([value]),
            "shamefully-hoist" if value == "true" => result["publicHoistPattern"] = json!(["*"]),
            _ => {}
        }
    }
    Ok((result, workspace, rc))
}
fn ranges_intersect(a: &str, b: &str) -> bool {
    a.is_empty()
        || a == b
        || match (node_semver::Range::parse(a), node_semver::Range::parse(b)) {
            (Ok(a), Ok(b)) => a.allows_any(&b),
            _ => false,
        }
}
fn override_spec(manifest: &Value, alias: &str, spec: &str, lock: &Value) -> String {
    let mut generic = Vec::new();
    let mut specific = Vec::new();
    if let Some(overrides) = lock["overrides"].as_object() {
        static PARENT: std::sync::LazyLock<regex::Regex> = std::sync::LazyLock::new(|| {
            regex::Regex::new(r"^(@[^/]+/[^@>]+|[^@>]+)(?:@([^>]+))?>([^=].*)$").unwrap()
        });
        let parent_re = &*PARENT;
        for (selector, replacement) in overrides {
            let mut target = selector.as_str();
            let mut parent = false;
            if let Some(c) = parent_re.captures(selector) {
                if manifest["name"].as_str() != Some(&c[1]) {
                    continue;
                }
                if let Some(range) = c.get(2) {
                    let matches = manifest["version"]
                        .as_str()
                        .and_then(|v| node_semver::Version::parse(v).ok())
                        .zip(node_semver::Range::parse(range.as_str()).ok())
                        .is_some_and(|(v, r)| r.satisfies(&v));
                    if !matches {
                        continue;
                    }
                }
                target = c.get(3).unwrap().as_str();
                parent = true;
            }
            let (package, range) = target[1..]
                .find('@')
                .map(|i| (&target[..i + 1], &target[i + 2..]))
                .unwrap_or((target, ""));
            if package != alias || !ranges_intersect(range, spec) {
                continue;
            }
            let Some(replacement) = replacement.as_str() else {
                continue;
            };
            if parent {
                specific.push((range, replacement));
            } else {
                generic.push((range, replacement));
            }
        }
    }
    let matches = if specific.is_empty() {
        generic
    } else {
        specific
    };
    matches
        .iter()
        .find(|(r, _)| !r.is_empty())
        .or(matches.first())
        .map(|(_, v)| v.to_string())
        .unwrap_or_else(|| spec.to_string())
}
// pnpm merges declarations in this order; shadowed sections have no importer entry.
fn dependency_field(manifest: &Value, alias: &str) -> Option<&'static str> {
    ["optionalDependencies", "dependencies", "devDependencies"]
        .into_iter()
        .find(|field| manifest[*field].get(alias).is_some())
}
fn validate(root: &Path, lock: &Value) -> Result<()> {
    let (settings, _, _) = settings(root)?;
    let empty = json!({});
    ensure!(
        settings.get("overrides").unwrap_or(&empty) == lock.get("overrides").unwrap_or(&empty),
        "ERR_PNPM_LOCKFILE_CONFIG_MISMATCH: overrides differ"
    );
    ensure!(
        root.join(".pnpmfile.cjs").exists() || lock.get("pnpmfileChecksum").is_none(),
        "ERR_PNPM_LOCKFILE_CONFIG_MISMATCH: hook was removed"
    );
    for (id, importer) in lock["importers"].as_object().context("missing importers")? {
        let manifest = read_json(&root.join(store::safe_relative(id)?).join("package.json"))?;
        for field in ["dependencies", "devDependencies", "optionalDependencies"] {
            if let Some(locked) = importer[field].as_object() {
                for alias in locked.keys() {
                    ensure!(
                        (manifest[field].get(alias).is_some()
                            && dependency_field(&manifest, alias) == Some(field))
                            || (dependency_field(&manifest, alias).is_none()
                                && field == "dependencies"
                                && lock["settings"]["autoInstallPeers"]
                                    .as_bool()
                                    .unwrap_or(true)
                                && manifest["peerDependencies"].get(alias).is_some()),
                        "ERR_PNPM_OUTDATED_LOCKFILE: {id}: removed dependency {alias}"
                    );
                }
            }
            if let Some(deps) = manifest[field].as_object() {
                for (alias, spec) in deps {
                    if dependency_field(&manifest, alias) != Some(field) {
                        continue;
                    }
                    let actual = importer[field][alias]["specifier"].as_str();
                    ensure!(
                        actual
                            == Some(
                                override_spec(&manifest, alias, spec.as_str().unwrap_or(""), lock)
                                    .as_str()
                            ),
                        "ERR_PNPM_OUTDATED_LOCKFILE: {id}: {alias} disagrees with lockfile"
                    );
                }
            }
        }
    }
    if root.join(".pnpmfile.cjs").exists() {
        use base64::Engine;
        let bytes = fs::read_to_string(root.join(".pnpmfile.cjs"))?.replace("\r\n", "\n");
        let hash = format!(
            "sha256-{}",
            base64::engine::general_purpose::STANDARD.encode(Sha256::digest(bytes.as_bytes()))
        );
        ensure!(
            lock["pnpmfileChecksum"].as_str() == Some(&hash),
            "ERR_PNPM_LOCKFILE_CONFIG_MISMATCH: hook checksum differs"
        );
    }
    Ok(())
}
// Export and assembly share exactly the same platform and reachability rules.
fn skipped_packages(lock: &Value, settings: &Value, arch: &str) -> Result<BTreeSet<String>> {
    let empty = json!({});
    let snapshot = lock
        .get("snapshots")
        .unwrap_or(&empty)
        .as_object()
        .context("invalid snapshots")?;
    let package_metadata = lock
        .get("packages")
        .unwrap_or(&empty)
        .as_object()
        .context("invalid packages")?;
    let mut skipped = BTreeSet::new();
    for (id, meta) in package_metadata {
        if !supported(meta, settings, arch) {
            for s in snapshot.keys().filter(|s| base_id(s) == id) {
                skipped.insert(s.clone());
            }
        }
    }
    // Optional packages reachable only through an incompatible platform package
    // are skipped too; they need not be present in a host-specific seed store.
    let mut reachable = BTreeSet::new();
    let mut todo = VecDeque::new();
    for importer in lock["importers"]
        .as_object()
        .context("missing importers")?
        .values()
    {
        todo.extend(dependencies(importer, true).into_values());
    }
    while let Some(id) = todo.pop_front() {
        if id.starts_with("link:") || skipped.contains(&id) || !reachable.insert(id.clone()) {
            continue;
        }
        let package = snapshot
            .get(&id)
            .with_context(|| format!("missing snapshot: {id}"))?;
        todo.extend(dependencies(package, false).into_values());
    }
    skipped.extend(
        snapshot
            .keys()
            .filter(|id| !reachable.contains(*id))
            .cloned(),
    );
    Ok(skipped)
}

pub fn target_packages(source: &Path) -> Result<BTreeMap<String, BTreeSet<String>>> {
    let lock: Value = serde_yaml_ng::from_slice(&fs::read(source.join("pnpm-lock.yaml"))?)?;
    let (settings, _, _) = settings(source)?;
    let mut targets = BTreeMap::new();
    for (system, arch) in [("x86_64-linux", "x64"), ("aarch64-linux", "arm64")] {
        let skipped = skipped_packages(&lock, &settings, arch)?;
        let ids = lock["snapshots"]
            .as_object()
            .into_iter()
            .flat_map(|s| s.keys())
            .filter(|id| !skipped.contains(*id))
            // Cached patches are independent variants. An active unpatched base
            // does not imply that every patched snapshot of it is reachable.
            .flat_map(|id| {
                let base = base_id(id);
                std::iter::once(base.to_owned())
                    .chain(patch_hash(id).map(|hash| format!("{base}(patch_hash={hash})")))
            })
            .collect();
        targets.insert(system.into(), ids);
    }
    Ok(targets)
}

pub fn assemble(
    source: &Path,
    out: &Path,
    tarballs: &Path,
    keep: bool,
    cache: Option<&Path>,
    package_contents: Option<&Path>,
) -> Result<()> {
    ensure!(
        package_contents.is_none() || (!keep && cache.is_none()),
        "package contents cannot be combined with pnpm verification stores"
    );
    let contents = package_contents
        .map(crate::package_cache::Contents::load)
        .transpose()?;
    let phase = std::time::Instant::now();
    ensure!(!out.exists(), "output already exists");
    fs::create_dir(out)?;
    let out = out.canonicalize()?;
    crate::workspace::copy_tree(source, &out)?;
    let lock: Value = serde_yaml_ng::from_slice(&fs::read(out.join("pnpm-lock.yaml"))?)?;
    validate(&out, &lock)?;
    let (settings, workspace, rc) = settings(&out)?;
    // Empty and workspace-only v9 lockfiles omit these maps.
    let empty = json!({});
    let snapshot = lock
        .get("snapshots")
        .unwrap_or(&empty)
        .as_object()
        .context("invalid snapshots")?;
    let package_metadata = lock
        .get("packages")
        .unwrap_or(&empty)
        .as_object()
        .context("invalid packages")?;
    let arch = match std::env::consts::ARCH {
        "x86_64" => "x64",
        "aarch64" => "arm64",
        arch => arch,
    };
    let skipped = skipped_packages(&lock, &settings, arch)?;
    let scratch = tempfile::tempdir_in(&out)?;
    let local_store = scratch.path().join("store");
    let cache = cache
        .map(Path::to_path_buf)
        .unwrap_or_else(|| local_store.clone());
    let sources: Vec<Value> = if cache == local_store && contents.is_none() {
        serde_json::from_slice(&fs::read(tarballs)?)?
    } else {
        Vec::new()
    };
    let sources: BTreeMap<_, _> = sources
        .iter()
        .filter_map(|p| Some((p["id"].as_str()?, p)))
        .collect();
    eprintln!("drvjs native: plan {:.3}s", phase.elapsed().as_secs_f64());
    let phase = std::time::Instant::now();
    let active: BTreeSet<_> = snapshot
        .keys()
        .filter(|id| !skipped.contains(*id))
        .map(|id| base_id(id))
        .collect();
    let mut optional = BTreeMap::new();
    for (id, snap) in snapshot {
        let entry = optional.entry(base_id(id)).or_insert(true);
        *entry &= snap["optional"].as_bool().unwrap_or(false);
    }
    let inputs: Vec<_> = package_metadata
        .iter()
        .filter(|(id, _)| active.contains(id.as_str()))
        .collect();
    let modules = out.join("node_modules");
    let virtual_store = modules.join(".pnpm");
    fs::create_dir_all(&virtual_store)?;
    let direct = cache == local_store && !keep;
    let mut direct_dirs = BTreeMap::new();
    if direct {
        for id in snapshot.keys().filter(|id| !skipped.contains(*id)) {
            let base = base_id(id);
            if !id[base.len()..].starts_with("(patch_hash=") {
                direct_dirs.entry(base.to_owned()).or_insert(
                    virtual_store
                        .join(filename(id))
                        .join("node_modules")
                        .join(name(base)?),
                );
            }
        }
        for id in &active {
            direct_dirs
                .entry((*id).to_owned())
                .or_insert(scratch.path().join(filename(id)));
        }
    }
    let loaded = std::sync::Mutex::new(BTreeMap::new());
    store::parallel_each(&inputs, |(id, meta)| {
        let integrity = meta["resolution"]["integrity"]
            .as_str()
            .context("missing integrity")?;
        let inspect = optional.get(id.as_str()) == Some(&true);
        let accept = |manifest: &Value| {
            supported(&with_manifest_constraints(meta, manifest), &settings, arch)
        };
        let index = if let Some(contents) = &contents {
            let (source, index) = contents.get(id, integrity, None)?;
            // A patched-only base needs metadata but no unpatched file copy.
            if !direct_dirs[*id].starts_with(scratch.path())
                && (!inspect || accept(&index["manifest"]))
            {
                store::materialize_directory(&source, &index, &direct_dirs[*id], false)?;
            }
            index
        } else if cache == local_store {
            let source = sources
                .get(id.as_str())
                .with_context(|| format!("missing tarball: {id}"))?;
            let file = Path::new(source["file"].as_str().context("missing archive file")?);
            let target = if direct { &direct_dirs[*id] } else { &cache };
            let prepared = if let Some(prepared) = source.get("archiveIntegrity") {
                ensure!(
                    source["sourceIntegrity"].as_str() == Some(integrity),
                    "prepared source integrity mismatch: {id}"
                );
                Some(
                    prepared
                        .as_str()
                        .context("invalid prepared archive integrity")?,
                )
            } else {
                None
            };
            store::unpack_filtered(
                file,
                target,
                id,
                integrity,
                prepared,
                direct,
                // A retained pnpm store must stay complete enough to seed this
                // same exported lockfile, which may omit the learned constraints.
                if inspect && direct {
                    Some(&accept)
                } else {
                    None
                },
            )?
        } else {
            let mut index = read_json(&store::index_path(&cache, id, integrity)?)
                .with_context(|| format!("missing or invalid cached package: {id}"))?;
            // A corrupted external manifest must not hide its package by claiming
            // an incompatible platform and thereby bypassing materialization checks.
            index["manifest"] = store::verified_manifest(&cache, &index)?;
            index
        };
        loaded
            .lock()
            .map_err(|_| anyhow::anyhow!("index lock poisoned"))?
            .insert((*id).clone(), index);
        Ok(())
    })?;
    let mut indexes = loaded
        .into_inner()
        .map_err(|_| anyhow::anyhow!("index lock poisoned"))?;
    let mut manifests = BTreeMap::new();
    for (id, index) in &mut indexes {
        manifests.insert(
            id.clone(),
            std::sync::Arc::new(
                if let Some(manifest) = index.as_object_mut().unwrap().remove("manifest") {
                    manifest
                } else {
                    store::manifest(&cache, index)?
                },
            ),
        );
    }
    // Some v9 lockfiles omit libc even though the authenticated package manifest
    // declares it. Supplement only optional packages; preserve required packages
    // and the project's explicitly requested supportedArchitectures.
    let mut complete_lock = lock.clone();
    for (id, manifest) in &manifests {
        if optional.get(id.as_str()) == Some(&true) {
            complete_lock["packages"][id] =
                with_manifest_constraints(&package_metadata[id], manifest);
        }
    }
    let skipped = skipped_packages(&complete_lock, &settings, arch)?;
    if complete_lock != lock {
        fs::write(
            out.join("pnpm-lock.yaml"),
            serde_yaml_ng::to_string(&complete_lock)?,
        )?;
    }
    let lock = complete_lock;
    eprintln!(
        "drvjs native: load/unpack {:.3}s",
        phase.elapsed().as_secs_f64()
    );
    let phase = std::time::Instant::now();

    let mut packages = BTreeMap::new();
    let mut had_patch = false;
    let mut materializations = Vec::new();
    for (id, snap) in snapshot {
        if skipped.contains(id) {
            continue;
        }
        let base = base_id(id);
        let index = indexes.get(base).context("snapshot missing package")?;
        let dir = virtual_store
            .join(filename(id))
            .join("node_modules")
            .join(name(base)?);
        let mut patch = None;
        if let Some(hash) = patch_hash(id) {
            let declaration = lock["patchedDependencies"]
                .as_object()
                .context("missing patch declarations")?
                .values()
                .find(|d| d["hash"].as_str() == Some(hash))
                .context("missing patch hash")?;
            patch = Some(declaration);
            had_patch = true;
        }
        materializations.push((base, index, dir.clone(), patch));
        packages.insert(
            id.clone(),
            Package {
                dir,
                children: dependencies(snap, false),
                requires_build: index["requiresBuild"].as_bool().unwrap_or(false),
            },
        );
    }
    store::parallel_each(&materializations, |(base, index, dir, patch)| {
        if let (Some(contents), Some(declaration)) = (&contents, patch) {
            let integrity = package_metadata[*base]["resolution"]["integrity"]
                .as_str()
                .context("missing integrity")?;
            let (source, index) = contents.get(base, integrity, declaration["hash"].as_str())?;
            store::materialize_directory(&source, &index, dir, false)?;
        } else if direct {
            store::materialize_directory(&direct_dirs[*base], index, dir, patch.is_none())?;
        } else {
            store::materialize(
                &cache,
                index,
                dir,
                cache != local_store,
                cache == local_store && patch.is_none(),
            )?;
        }
        if let Some(declaration) = patch.filter(|_| contents.is_none()) {
            if cache != local_store {
                store::patch_cached(&out, dir, declaration, base)?;
            } else {
                store::patch(&out, dir, declaration)?;
            }
        }
        Ok(())
    })?;
    // A newly incompatible optional parent can make otherwise compatible
    // descendants unreachable. Delete any early direct copies after peer copies
    // have finished, since an active peer may share their original base files.
    if direct {
        for id in &skipped {
            let dir = virtual_store.join(filename(id));
            if dir.exists() {
                fs::remove_dir_all(dir)?;
            }
        }
    }
    eprintln!(
        "drvjs native: integrity/materialize/patch {:.3}s",
        phase.elapsed().as_secs_f64()
    );
    let phase = std::time::Instant::now();
    let mut importers = BTreeMap::new();
    let mut project_manifests = BTreeMap::new();
    for (id, data) in lock["importers"].as_object().unwrap() {
        importers.insert(id.clone(), dependencies(data, true));
        project_manifests.insert(id.clone(), read_json(&out.join(id).join("package.json"))?);
    }
    let resolve = |edge: &str, owner: &Path| -> Option<PathBuf> {
        if let Some(local) = edge.strip_prefix("link:") {
            Some(normalized(&owner.join(local)))
        } else {
            packages.get(edge).map(|p| p.dir.clone())
        }
    };
    for package in packages.values() {
        for (alias, edge) in &package.children {
            if let Some(target) = resolve(edge, &package.dir) {
                link(
                    &target,
                    &package
                        .dir
                        .ancestors()
                        .find(|p| p.file_name().is_some_and(|n| n == "node_modules"))
                        .context("missing package modules directory")?
                        .join(store::safe_relative(alias)?),
                )?;
            }
        }
    }
    for (id, edges) in &importers {
        for (alias, edge) in edges {
            if let Some(target) = resolve(edge, &out.join(id)) {
                link(
                    &target,
                    &out.join(id)
                        .join("node_modules")
                        .join(store::safe_relative(alias)?),
                )?;
            }
        }
    }
    let private = patterns(&settings["hoistPattern"], &["*"]);
    let public = patterns(&settings["publicHoistPattern"], &[]);
    let mut claimed: BTreeSet<_> = importers
        .get(".")
        .unwrap()
        .keys()
        .map(|s| s.to_lowercase())
        .collect();
    let mut roots = Edges::new();
    let mut start = Vec::new();
    let mut visited = BTreeSet::new();
    for edges in importers.values() {
        for (alias, id) in edges {
            roots.entry(alias.clone()).or_insert(id.clone());
            if packages.contains_key(id) && visited.insert(id.clone()) {
                start.push(id.clone());
            }
        }
    }
    // pnpm's graph walker discovers each level's children before recursively
    // visiting those child steps, retaining the first-discovered depth.
    fn walk(
        nodes: Vec<String>,
        depth: usize,
        packages: &BTreeMap<String, Package>,
        visited: &mut BTreeSet<String>,
        output: &mut Vec<(usize, String, Edges)>,
    ) {
        let mut steps = Vec::new();
        for id in nodes {
            output.push((depth, id.clone(), packages[&id].children.clone()));
            let next = packages[&id]
                .children
                .values()
                .filter(|id| packages.contains_key(*id) && visited.insert((*id).clone()))
                .cloned()
                .collect();
            steps.push(next);
        }
        for next in steps {
            walk(next, depth + 1, packages, visited, output);
        }
    }
    let mut ordered = Vec::new();
    walk(start, 1, &packages, &mut visited, &mut ordered);
    // pnpm uses the physical package directory as graph node identity.
    // Peer suffix hashing changes its lexical order relative to lockfile IDs.
    ordered.sort_by(|a, b| (&a.0, &packages[&a.1].dir).cmp(&(&b.0, &packages[&b.1].dir)));
    ordered.insert(0, (0, String::new(), roots));
    let mut hoisted = json!({});
    let mut private_bins = Vec::new();
    let mut public_bins = Vec::new();
    if !packages.is_empty() && !workspace.is_null() {
        for (id, manifest) in &project_manifests {
            if id == "." {
                continue;
            }
            if let Some(alias) = manifest["name"].as_str()
                && !claimed.contains(alias)
                && matches_patterns(alias, &private)
            {
                link(
                    &out.join(id),
                    &virtual_store.join("node_modules").join(alias),
                )?;
                claimed.insert(alias.into());
            }
        }
    }
    for (_, _, children) in ordered {
        for (alias, id) in children {
            if claimed.contains(&alias.to_lowercase()) {
                continue;
            }
            let kind = if matches_patterns(&alias, &public) {
                "public"
            } else if matches_patterns(&alias, &private) {
                "private"
            } else {
                continue;
            };
            let Some(pkg) = packages.get(&id) else {
                continue;
            };
            let destination = if kind == "public" {
                modules.clone()
            } else {
                virtual_store.join("node_modules")
            };
            link(&pkg.dir, &destination.join(&alias))?;
            hoisted[&id][&alias] = kind.into();
            claimed.insert(alias.to_lowercase());
            if kind == "public" {
                public_bins.push(pkg.dir.clone())
            } else {
                private_bins.push(destination.join(&alias))
            }
        }
    }
    let mut bin_manifests: BTreeMap<_, _> = packages
        .iter()
        .filter(|(id, _)| !id[base_id(id).len()..].starts_with("(patch_hash="))
        .map(|(id, pkg)| (pkg.dir.clone(), manifests[base_id(id)].clone()))
        .collect();
    for (id, package) in &packages {
        let build_links = had_patch
            && (package.requires_build || id[base_id(id).len()..].starts_with("(patch_hash="));
        let mut targets: Vec<_> = package
            .children
            .values()
            .filter_map(|e| resolve(e, &package.dir))
            .collect();
        if build_links {
            targets.push(package.dir.clone());
        }
        link_bins(
            &out,
            &package.dir.join("node_modules/.bin"),
            &targets,
            !build_links,
            &mut bin_manifests,
        )?;
        if build_links && package.dir.join("node_modules").is_dir() {
            let mut bundled = Vec::new();
            for entry in fs::read_dir(package.dir.join("node_modules"))? {
                let entry = entry?;
                if !entry.file_type()?.is_dir()
                    || entry.file_name().to_string_lossy().starts_with('.')
                {
                    continue;
                }
                if entry.file_name().to_string_lossy().starts_with('@') {
                    for child in fs::read_dir(entry.path())? {
                        bundled.push(child?.path());
                    }
                } else {
                    bundled.push(entry.path());
                }
            }
            link_bins(
                &out,
                &package.dir.join("node_modules/.bin"),
                &bundled,
                false,
                &mut bin_manifests,
            )?;
        }
    }
    for (id, edges) in &importers {
        let mut targets: Vec<_> = edges
            .values()
            .filter_map(|e| resolve(e, &out.join(id)))
            .collect();
        if id == "." {
            targets.extend(public_bins.clone());
        }
        link_bins(
            &out,
            &out.join(id).join("node_modules/.bin"),
            &targets,
            true,
            &mut bin_manifests,
        )?;
    }
    link_bins(
        &out,
        &virtual_store.join("node_modules/.bin"),
        &private_bins,
        true,
        &mut bin_manifests,
    )?;
    let mut pending: Vec<_> = packages
        .iter()
        .filter(|(_, p)| p.requires_build)
        .map(|(id, _)| id.clone())
        .collect();
    for (id, manifest) in &project_manifests {
        if ["preinstall", "install", "postinstall", "prepare"]
            .iter()
            .any(|k| {
                manifest["scripts"][k]
                    .as_str()
                    .is_some_and(|s| !s.is_empty())
            })
        {
            pending.push(id.clone());
        }
    }
    pending.sort();
    let mut registries =
        json!({"default":"https://registry.npmjs.org/","@jsr":"https://npm.jsr.io/"});
    for (k, v) in &rc {
        if k == "registry" {
            registries["default"] = v.clone().into();
        } else if let Some(scope) = k.strip_suffix(":registry") {
            registries[scope] = v.clone().into();
        }
    }
    eprintln!(
        "drvjs native: links/bins {:.3}s",
        phase.elapsed().as_secs_f64()
    );
    // With no importer dependencies pnpm omits installation metadata entirely.
    if importers.values().any(|edges| !edges.is_empty()) {
        let mut metadata = json!({"hoistPattern":private,"publicHoistPattern":public,"hoistedDependencies":hoisted,"included":{"dependencies":true,"devDependencies":true,"optionalDependencies":true},"injectedDeps":{},"layoutVersion":5,"nodeLinker":"isolated","packageManager":"pnpm@10.34.5","pendingBuilds":pending,"prunedAt":"Thu, 01 Jan 1970 00:00:00 GMT","registries":registries,"skipped":skipped,"storeDir":out.join(".pnpm-store/v10"),"virtualStoreDir":".pnpm","virtualStoreDirMaxLength":120});
        if had_patch {
            metadata["ignoredBuilds"] = json!([]);
        }
        store::write_json(&modules.join(".modules.yaml"), &metadata)?;
        let mut current = lock.clone();
        // Workspace projection can leave empty maps. pnpm omits these in its
        // installed lockfile, including when every dependency is a workspace link.
        for key in ["packages", "snapshots"] {
            if current.get(key).is_some_and(|value| {
                value.is_null() || value.as_object().is_some_and(|map| map.is_empty())
            }) {
                current.as_object_mut().unwrap().remove(key);
            }
        }
        fs::write(
            virtual_store.join("lock.yaml"),
            serde_yaml_ng::to_string(&current)?,
        )?;
    }
    write_state(
        &out,
        &project_manifests,
        &settings,
        &workspace,
        &lock,
        &private,
        &public,
    )?;
    if keep {
        store::retain(&cache, &out.join(".pnpm-store"), &indexes, &lock)?;
    }
    eprintln!(
        "drvjs: native assembly: {} package instances, {} importers; no pnpm subprocess",
        packages.len(),
        importers.len()
    );
    Ok(())
}
fn write_state(
    out: &Path,
    projects: &BTreeMap<String, Value>,
    config: &Value,
    workspace: &Value,
    lock: &Value,
    private: &[String],
    public: &[String],
) -> Result<()> {
    let mut settings = json!({"autoInstallPeers":lock["settings"]["autoInstallPeers"].as_bool().unwrap_or(true),"dedupeDirectDeps":false,"dedupeInjectedDeps":true,"dedupePeerDependents":true,"dedupePeers":false,"dev":true,"excludeLinksFromLockfile":lock["settings"]["excludeLinksFromLockfile"].as_bool().unwrap_or(false),"hoistPattern":private,"hoistWorkspacePackages":true,"injectWorkspacePackages":false,"linkWorkspacePackages":false,"nodeLinker":"isolated","optional":true,"peersSuffixMaxLength":1000,"preferWorkspacePackages":false,"production":true,"publicHoistPattern":public});
    for key in ["overrides", "packageExtensions"] {
        if let Some(v) = config.get(key) {
            settings[key] = v.clone();
        }
    }
    if let Some(patches) = config["patchedDependencies"].as_object() {
        let mut p = json!({});
        for (key, path) in patches {
            p[key] = json!(out.join(path.as_str().context("invalid patch path")?));
        }
        settings["patchedDependencies"] = p;
    }
    let mut project_state = json!({});
    if workspace.get("packages").is_some() {
        settings["workspacePackagePatterns"] = workspace["packages"].clone();
        for (id, m) in projects {
            let mut p = json!({});
            for key in ["name", "version"] {
                if let Some(v) = m.get(key) {
                    p[key] = v.clone();
                }
            }
            project_state[out.join(id).to_string_lossy().trim_end_matches("/.")] = p;
        }
    }
    // pnpm records an empty catalog map even for workspaces without catalogs.
    if out.join("pnpm-workspace.yaml").exists() {
        let mut c = workspace.get("catalogs").cloned().unwrap_or(json!({}));
        if let Some(default) = workspace.get("catalog") {
            c["default"] = default.clone();
        }
        settings["catalogs"] = c;
    }
    let hooks = if out.join(".pnpmfile.cjs").exists() {
        vec![out.join(".pnpmfile.cjs")]
    } else {
        vec![]
    };
    store::write_json(
        &out.join("node_modules/.pnpm-workspace-state-v1.json"),
        &json!({"filteredInstall":false,"lastValidatedTimestamp":0,"pnpmfiles":hooks,"projects":project_state,"settings":settings}),
    )
}
fn link_bins(
    root: &Path,
    bin: &Path,
    targets: &[PathBuf],
    extend_node_path: bool,
    manifests: &mut BTreeMap<PathBuf, std::sync::Arc<Value>>,
) -> Result<()> {
    let mut used = BTreeMap::new();
    static SHEBANG: std::sync::LazyLock<regex::Regex> = std::sync::LazyLock::new(|| {
        regex::Regex::new(r"^#!\s*(?:/usr/bin/env(?:\s+-S\s*)?)?\s*([^ \t]+)(.*)$").unwrap()
    });
    let shebang = &*SHEBANG;
    for target in targets {
        let manifest = manifests.entry(target.clone()).or_insert_with(|| {
            std::sync::Arc::new(read_json(&target.join("package.json")).unwrap_or(Value::Null))
        });
        if manifest.is_null() {
            continue;
        }
        let bins: Vec<(String, String)> = if let Some(path) = manifest["bin"].as_str() {
            vec![(
                manifest["name"]
                    .as_str()
                    .unwrap_or("")
                    .rsplit('/')
                    .next()
                    .unwrap()
                    .into(),
                path.into(),
            )]
        } else {
            manifest["bin"]
                .as_object()
                .map(|o| {
                    o.iter()
                        .filter_map(|(k, v)| Some((k.clone(), v.as_str()?.into())))
                        .collect()
                })
                .unwrap_or_default()
        };
        for (name, path) in bins {
            store::safe_relative(&name)?;
            let file = target.join(store::safe_relative(&path)?);
            if !file.is_file() {
                continue;
            }
            let package_name = manifest["name"].as_str().unwrap_or("");
            let priority = (
                name == package_name,
                package_name.to_owned(),
                node_semver::Version::parse(manifest["version"].as_str().unwrap_or("0.0.0")).ok(),
            );
            if used.get(&name).is_some_and(|old| old >= &priority) {
                continue;
            }
            used.insert(name.clone(), priority);
            fs::create_dir_all(bin)?;
            store::make_executable(&file)?;
            use std::io::Read;
            let mut content = Vec::new();
            fs::File::open(&file)?
                .take(4096)
                .read_to_end(&mut content)?;
            ensure!(
                content.len() < 4096 || !content.starts_with(b"#!") || content.contains(&b'\n'),
                "executable shebang exceeds 4096 bytes: {}",
                file.display()
            );
            let first = std::str::from_utf8(content.split(|b| *b == b'\n').next().unwrap_or(&[]))
                .unwrap_or("");
            let runtime = shebang.captures(first.trim());
            let interpreter = runtime.as_ref().map(|c| c[1].trim()).unwrap_or("");
            let args = runtime.as_ref().map(|c| c[2].trim()).unwrap_or("");
            ensure!(
                !interpreter.contains(['\n', '\r', '"', '`', '$'])
                    && !args.contains(['\n', '\r', '"', '`', '$', ';', '&', '|', '<', '>']),
                "unsupported executable interpreter"
            );
            let relative = relative(bin, &file).to_string_lossy().into_owned();
            let real_target = target.canonicalize()?;
            let mut paths = vec![real_target.join("node_modules")];
            let mut ancestor = real_target.parent();
            while let Some(p) = ancestor {
                if p == root.join("node_modules/.pnpm") {
                    break;
                }
                if p.file_name()
                    .is_some_and(|s| s.to_string_lossy().starts_with('@'))
                {
                    ancestor = p.parent();
                    continue;
                }
                if p.file_name().is_some_and(|s| s != "node_modules") {
                    paths.push(p.join("node_modules"));
                } else {
                    paths.push(p.to_path_buf());
                }
                ancestor = p.parent();
                if p == root {
                    break;
                }
            }
            // Workspace binaries are outside node_modules; pnpm supplies only
            // its additional hoisted lookup path for these scripts.
            if !real_target.starts_with(root.join("node_modules")) {
                paths.clear();
            }
            paths.push(root.join("node_modules/.pnpm/node_modules"));
            paths.dedup();
            let node_path = paths
                .iter()
                .map(|p| p.to_string_lossy())
                .collect::<Vec<_>>()
                .join(":");
            let mut script = format!("{}\n", include_str!("native_shim.txt"));
            if extend_node_path {
                script.push_str(&format!("if [ -z \"$NODE_PATH\" ]; then\n  export NODE_PATH=\"{node_path}\"\nelse\n  export NODE_PATH=\"{node_path}:$NODE_PATH\"\nfi\n"));
            }
            if runtime.is_none() {
                script.push_str(&format!("\"$basedir/{relative}\"   \"$@\"\nexit $?\n"));
            } else {
                script.push_str(&format!("if [ -x \"$basedir/{interpreter}\" ]; then\n  exec \"$basedir/{interpreter}\" {args} \"$basedir/{relative}\" \"$@\"\nelse\n  exec {interpreter} {args} \"$basedir/{relative}\" \"$@\"\nfi\n"));
            }
            let destination = bin.join(name);
            fs::write(&destination, script)?;
            fs::set_permissions(destination, fs::Permissions::from_mode(0o755))?;
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn target_closure_respects_architectures_and_skipped_transitive_dependencies() {
        let lock = json!({
            "importers": {".": {"optionalDependencies": {
                "x64-only": {"version":"1.0.0"}, "arm-only": {"version":"1.0.0"}
            }}},
            "packages": {"x64-only@1.0.0":{"cpu":["x64"]},
                "arm-only@1.0.0":{"cpu":["arm64"]}, "child@1.0.0":{}},
            "snapshots": {"x64-only@1.0.0":{"dependencies":{"child":"1.0.0"}},
                "arm-only@1.0.0":{}, "child@1.0.0":{}}
        });
        assert_eq!(
            skipped_packages(&lock, &json!({}), "x64").unwrap(),
            BTreeSet::from(["arm-only@1.0.0".into()])
        );
        assert_eq!(
            skipped_packages(&lock, &json!({}), "arm64").unwrap(),
            BTreeSet::from(["x64-only@1.0.0".into(), "child@1.0.0".into()])
        );
        let settings = json!({"supportedArchitectures":{"cpu":["current","x64"]}});
        assert!(
            skipped_packages(&lock, &settings, "arm64")
                .unwrap()
                .is_empty()
        );
        assert!(!supported(&json!({"os":["!linux"]}), &json!({}), "x64"));
        assert!(!supported(&json!({"libc":["musl"]}), &json!({}), "x64"));
        assert!(supported(&json!({"os":["any"]}), &json!({}), "arm64"));
        let mut lock = lock;
        lock["snapshots"]["arm-only@1.0.0"] =
            json!({"dependencies":{"child":"1.0.0(patch_hash=abc)"}});
        lock["snapshots"]["child@1.0.0(patch_hash=abc)"] = json!({});
        let root = tempfile::tempdir().unwrap();
        fs::write(root.path().join("package.json"), "{\"name\":\"targets\"}").unwrap();
        fs::write(root.path().join("pnpm-lock.yaml"), lock.to_string()).unwrap();
        let targets = target_packages(root.path()).unwrap();
        assert_eq!(
            targets["x86_64-linux"],
            BTreeSet::from(["x64-only@1.0.0".into(), "child@1.0.0".into()])
        );
        assert_eq!(
            targets["aarch64-linux"],
            BTreeSet::from([
                "arm-only@1.0.0".into(),
                "child@1.0.0".into(),
                "child@1.0.0(patch_hash=abc)".into()
            ])
        );
    }

    #[test]
    fn peer_instances_and_url_aliases_keep_their_identity() {
        assert_eq!(
            filename("a@1.0.0(b@2.0.0(c@3.0.0))"),
            "a@1.0.0_b@2.0.0_c@3.0.0_"
        );
        assert_eq!(reference("alias", "@scope/name@1.0.0"), "@scope/name@1.0.0");
        assert_eq!(
            reference("alias", "https://example.com/@scope/name.tgz"),
            "alias@https://example.com/@scope/name.tgz"
        );
        assert!(filename(&format!("pkg@1.0.0({})", "a".repeat(200))).len() <= 120);
    }

    #[test]
    fn bins_prefer_the_package_own_name_and_preserve_interpreters() {
        let dir = tempfile::tempdir().unwrap();
        let mut targets = Vec::new();
        for (directory, package, body) in [
            ("other", "@scope/other", "#!/usr/bin/env node\n"),
            ("tool", "tool", "#!/bin/node --flag\n"),
        ] {
            let path = dir.path().join(directory);
            fs::create_dir(&path).unwrap();
            store::write_json(
                &path.join("package.json"),
                &json!({"name":package,"version":"1.0.0","bin":{"tool":"cli.js"}}),
            )
            .unwrap();
            fs::write(path.join("cli.js"), body).unwrap();
            targets.push(path);
        }
        let bin = dir.path().join(".bin");
        link_bins(dir.path(), &bin, &targets, false, &mut BTreeMap::new()).unwrap();
        let shim = fs::read_to_string(bin.join("tool")).unwrap();
        assert!(shim.contains("exec /bin/node --flag"));
        assert!(shim.contains("../tool/cli.js"));
        assert!(!shim.contains("NODE_PATH"));
    }

    #[test]
    fn frozen_validation_uses_dependency_section_precedence() {
        let root = tempfile::tempdir().unwrap();
        for spec in ["1.0.0", "workspace:*"] {
            for winner in ["dependencies", "optionalDependencies"] {
                let mut manifest = json!({"name":"app", "dependencies":{"example":spec}, "devDependencies":{"example":"shadowed"}});
                if winner == "optionalDependencies" {
                    manifest["optionalDependencies"] = json!({"example":spec});
                    manifest["dependencies"]["example"] = json!("also-shadowed");
                }
                store::write_json(&root.path().join("package.json"), &manifest).unwrap();
                let mut lock = json!({"importers":{".":{winner:{"example":{"specifier":spec,"version":"1.0.0"}}}}});
                validate(root.path(), &lock).unwrap();
                lock["importers"]["."][winner]["example"]["specifier"] = json!("stale");
                assert!(validate(root.path(), &lock).is_err());
                lock["importers"]["."][winner]["example"]["specifier"] = json!(spec);
                lock["importers"]["."]["devDependencies"] =
                    json!({"example":{"specifier":"shadowed","version":"1.0.0"}});
                assert!(validate(root.path(), &lock).is_err());
            }
        }
    }

    #[test]
    fn removed_manifest_dependency_is_rejected() {
        let dir = tempfile::tempdir().unwrap();
        store::write_json(&dir.path().join("package.json"), &json!({"name":"app"})).unwrap();
        let lock = json!({"importers":{".":{"dependencies":{"gone":{"specifier":"1.0.0","version":"1.0.0"}}}}});
        assert!(
            validate(dir.path(), &lock)
                .unwrap_err()
                .to_string()
                .contains("removed dependency")
        );
    }

    #[test]
    fn qualified_overrides_match_manifest_ranges() {
        let lock = json!({"overrides":{"dep@^1":"1.9.0","app>dep@^2":"2.9.0"}});
        let manifest = json!({"name":"app","version":"1.0.0"});
        assert_eq!(override_spec(&manifest, "dep", "^1.2.0", &lock), "1.9.0");
        assert_eq!(override_spec(&manifest, "dep", "^2.1.0", &lock), "2.9.0");
        assert_eq!(override_spec(&manifest, "dep", "^3.0.0", &lock), "^3.0.0");
    }
}
