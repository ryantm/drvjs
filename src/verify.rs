use crate::assemble;
use anyhow::{Context, Result, ensure};
use serde_json::Value;
use sha2::{Digest, Sha256};
use std::{
    collections::BTreeMap,
    fs,
    os::unix::fs::PermissionsExt,
    path::{Path, PathBuf},
};
use walkdir::WalkDir;

#[derive(PartialEq, Debug)]
enum Entry {
    Link(PathBuf),
    File(bool, Vec<u8>),
}
fn snapshot(root: &Path) -> Result<BTreeMap<PathBuf, Entry>> {
    let mut result = BTreeMap::new();
    for importer in crate::workspace::importer_paths(root)? {
        let modules = root.join(&importer).join("node_modules");
        if !modules.exists() {
            continue;
        }
        for entry in WalkDir::new(&modules).min_depth(1) {
            let entry = entry?;
            let relative = entry.path().strip_prefix(&modules)?;
            if entry.file_type().is_symlink() {
                result.insert(
                    importer.join("node_modules").join(relative),
                    Entry::Link(fs::read_link(entry.path())?),
                );
            } else if entry.file_type().is_file() {
                let mut content = fs::read(entry.path())?;
                if relative == Path::new(".modules.yaml") {
                    let mut data: Value = serde_yaml_ng::from_slice(&content)?;
                    data.as_object_mut()
                        .context("invalid modules metadata")?
                        .remove("prunedAt");
                    // These are sets of pending/skipped package identities, not
                    // ordered execution plans. Keep every entry, canonicalize order.
                    for key in ["pendingBuilds", "skipped", "ignoredBuilds"] {
                        if let Some(entries) = data[key].as_array_mut() {
                            entries.sort_by_key(|v| v.to_string());
                        }
                    }
                    data["storeDir"] = "<STORE>".into();
                    content = serde_json::to_string(&data)?
                        .replace(root.to_str().context("non-UTF8 path")?, "<ROOT>")
                        .into_bytes();
                } else if relative == Path::new(".pnpm/lock.yaml") {
                    // YAML formatting is not installation state. Compare its full
                    // parsed value, retaining all package and importer metadata.
                    let data: Value = serde_yaml_ng::from_slice(&content)?;
                    content = serde_json::to_vec(&data)?;
                } else if relative == Path::new(".pnpm-workspace-state-v1.json") {
                    let mut data: Value = serde_json::from_slice(&content)?;
                    data.as_object_mut()
                        .context("invalid workspace metadata")?
                        .remove("lastValidatedTimestamp");
                    content = serde_json::to_string(&data)?
                        .replace(root.to_str().context("non-UTF8 path")?, "<ROOT>")
                        .into_bytes();
                } else if relative.components().any(|p| p.as_os_str() == ".bin")
                    && let Ok(text) = std::str::from_utf8(&content)
                {
                    content = text
                        .replace(root.to_str().context("non-UTF8 path")?, "<ROOT>")
                        .into_bytes();
                }
                result.insert(
                    importer.join("node_modules").join(relative),
                    Entry::File(
                        entry.metadata()?.permissions().mode() & 0o111 != 0,
                        Sha256::digest(content).to_vec(),
                    ),
                );
            }
        }
    }
    Ok(result)
}

pub fn verify(directory: &Path, external_store: Option<&Path>) -> Result<()> {
    let directory = directory.canonicalize()?;
    let source = external_store
        .map(Path::to_path_buf)
        .unwrap_or_else(|| directory.join(".pnpm-store"));
    ensure!(
        source.is_dir(),
        "verification store is not retained in this output; rebuild an export made with --keep-verification-store, use nix-build EXPORT --arg keepVerificationStore true, or supply --store PATH"
    );
    let tmp = tempfile::tempdir()?;
    crate::workspace::copy_tree(&directory, tmp.path())?;
    let store = tmp.path().join(".pnpm-store");
    for entry in WalkDir::new(&source) {
        let entry = entry?;
        let destination = store.join(entry.path().strip_prefix(&source)?);
        if entry.file_type().is_dir() {
            fs::create_dir_all(&destination)?;
        } else {
            ensure!(
                entry.file_type().is_file(),
                "unexpected store symlink: {}",
                entry.path().display()
            );
            fs::copy(entry.path(), &destination)?;
            let mut permissions = fs::metadata(&destination)?.permissions();
            permissions.set_mode(permissions.mode() | 0o200);
            fs::set_permissions(destination, permissions)?;
        }
    }
    // Installation records platform-skipped optional packages in .modules.yaml;
    // store status needs that metadata to avoid expecting their absent contents.
    assemble::install(tmp.path())?;
    assemble::store_status(tmp.path())?;

    let actual = snapshot(&directory)?;
    let expected = snapshot(tmp.path())?;
    let differences: Vec<_> = actual
        .keys()
        .chain(expected.keys())
        .filter(|p| actual.get(*p) != expected.get(*p))
        .take(10)
        .collect();
    ensure!(
        differences.is_empty(),
        "node_modules differs from offline pnpm reconstruction: {differences:?}"
    );
    println!("Verified: node_modules matches a fresh frozen-lockfile offline pnpm install");
    Ok(())
}
