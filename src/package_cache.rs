//! Immutable package contents, independent of workspace selection and graph edges.
//! Only Nix-produced inputs use this internal path. External pnpm stores still
//! go through native_store's per-file integrity validation.
use crate::native_store as store;
use anyhow::{Context, Result, ensure};
use serde_json::{Value, json};
use std::{
    collections::BTreeMap,
    fs,
    os::unix::fs::PermissionsExt,
    path::{Path, PathBuf},
};
use walkdir::WalkDir;

pub fn prepare(archive: &Path, out: &Path, id: &str, integrity: &str) -> Result<()> {
    ensure!(!out.exists(), "output already exists");
    let mut index = store::unpack_to(archive, &out.join("package"), id, integrity)?;
    index["sourceIntegrity"] = integrity.into();
    index["id"] = id.into();
    store::write_json(&out.join("index.json"), &index)
}

pub fn patch(source: &Path, out: &Path, patch: &Path, hash: &str) -> Result<()> {
    ensure!(!out.exists(), "output already exists");
    let mut index = read_index(source)?;
    let target = out.join("package");
    store::materialize_directory(&source.join("package"), &index, &target, false)?;
    // Nix inputs are read-only. Git must be able to update the private copies.
    for entry in WalkDir::new(&target) {
        let entry = entry?;
        let mode = entry.metadata()?.permissions().mode() | 0o200;
        fs::set_permissions(entry.path(), fs::Permissions::from_mode(mode))?;
    }
    let scratch = tempfile::tempdir()?;
    fs::copy(patch, scratch.path().join("package.patch"))?;
    store::patch(
        scratch.path(),
        &target,
        &json!({"path":"package.patch", "hash":hash}),
    )?;
    // Patches can add/delete files or change package.json. requiresBuild stays
    // tied to the original archive, matching the existing assembly semantics.
    let mut files = serde_json::Map::new();
    for entry in WalkDir::new(&target) {
        let entry = entry?;
        if entry.file_type().is_file() {
            files.insert(
                entry
                    .path()
                    .strip_prefix(&target)?
                    .to_str()
                    .context("non-UTF8 package path")?
                    .into(),
                Value::Null,
            );
        }
    }
    index["files"] = Value::Object(files);
    // As in link_bins, an absent/invalid patched manifest supplies no bins.
    index["manifest"] = fs::read(target.join("package.json"))
        .ok()
        .and_then(|bytes| serde_json::from_slice(&bytes).ok())
        .unwrap_or(Value::Null);
    index["patchHash"] = hash.into();
    store::write_json(&out.join("index.json"), &index)
}

fn read_index(source: &Path) -> Result<Value> {
    Ok(serde_json::from_slice(&fs::read(
        source.join("index.json"),
    )?)?)
}

pub struct Contents {
    paths: BTreeMap<String, PathBuf>,
}

impl Contents {
    pub fn load(manifest: &Path) -> Result<Self> {
        Ok(Self {
            paths: serde_json::from_slice(&fs::read(manifest)?)?,
        })
    }

    pub fn get(
        &self,
        id: &str,
        integrity: &str,
        patch_hash: Option<&str>,
    ) -> Result<(PathBuf, Value)> {
        let key = match patch_hash {
            Some(hash) => format!("{id}(patch_hash={hash})"),
            None => id.into(),
        };
        let path = self
            .paths
            .get(&key)
            .with_context(|| format!("missing package contents: {key}"))?;
        let index = read_index(path)?;
        ensure!(
            index["id"].as_str() == Some(id)
                && index["sourceIntegrity"].as_str() == Some(integrity)
                && index["patchHash"].as_str() == patch_hash,
            "package contents identity mismatch: {key}"
        );
        Ok((path.join("package"), index))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use base64::{Engine, engine::general_purpose::STANDARD};
    use sha2::{Digest, Sha256, Sha512};

    #[test]
    fn cached_patch_tracks_added_removed_files_without_mutating_its_base() {
        let root = tempfile::tempdir().unwrap();
        let archive = root.path().join("package.tgz");
        let encoder = flate2::write::GzEncoder::new(Vec::new(), flate2::Compression::fast());
        let mut tar = tar::Builder::new(encoder);
        for (path, content) in [
            (
                "package.json",
                "{\"name\":\"example\",\"version\":\"1.0.0\"}\n",
            ),
            ("old.js", "old\n"),
        ] {
            let mut header = tar::Header::new_gnu();
            header.set_size(content.len() as u64);
            header.set_mode(0o644);
            header.set_cksum();
            tar.append_data(&mut header, format!("package/{path}"), content.as_bytes())
                .unwrap();
        }
        let bytes = tar.into_inner().unwrap().finish().unwrap();
        fs::write(&archive, &bytes).unwrap();
        let integrity = format!("sha512-{}", STANDARD.encode(Sha512::digest(&bytes)));
        let base = root.path().join("base");
        prepare(&archive, &base, "example@1.0.0", &integrity).unwrap();
        // Match the permissions of an immutable Nix input.
        for entry in WalkDir::new(&base) {
            let entry = entry.unwrap();
            if entry.file_type().is_file() {
                fs::set_permissions(entry.path(), fs::Permissions::from_mode(0o444)).unwrap();
            }
        }
        let patch_text = "diff --git a/old.js b/old.js\n--- a/old.js\n+++ /dev/null\n@@ -1 +0,0 @@\n-old\ndiff --git a/new.js b/new.js\n--- /dev/null\n+++ b/new.js\n@@ -0,0 +1 @@\n+new\n";
        let patch_file = root.path().join("edit.patch");
        fs::write(&patch_file, patch_text).unwrap();
        let hash = format!("{:x}", Sha256::digest(patch_text));
        let patched = root.path().join("patched");
        patch(&base, &patched, &patch_file, &hash).unwrap();
        let index = read_index(&patched).unwrap();
        assert!(index["files"].get("old.js").is_none());
        assert!(index["files"].get("new.js").is_some());
        assert_eq!(
            fs::read_to_string(base.join("package/old.js")).unwrap(),
            "old\n"
        );
        assert!(!base.join("package/new.js").exists());
        let installed = root.path().join("installed");
        store::materialize_directory(&patched.join("package"), &index, &installed, false).unwrap();
        assert!(!installed.join("old.js").exists());
        assert_eq!(
            fs::read_to_string(installed.join("new.js")).unwrap(),
            "new\n"
        );
        assert!(patch(&base, &root.path().join("invalid"), &patch_file, "wrong").is_err());
        fs::write(&archive, b"corrupt").unwrap();
        assert!(
            prepare(
                &archive,
                &root.path().join("corrupt"),
                "example@1.0.0",
                &integrity
            )
            .is_err()
        );
    }
}
