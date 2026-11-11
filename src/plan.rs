use anyhow::{Context, Result, bail, ensure};
use base64::{Engine, engine::general_purpose::STANDARD};
use regex::Regex;
use serde::{Deserialize, Serialize};
use serde_json::Value;
use sha2::{Digest, Sha256};
use std::collections::BTreeMap;

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct Lock {
    lockfile_version: Value,
    importers: BTreeMap<String, Value>,
    #[serde(default)]
    packages: BTreeMap<String, Value>,
    #[serde(default)]
    patched_dependencies: BTreeMap<String, Value>,
}
#[derive(Serialize, Deserialize, Debug, PartialEq)]
pub struct Package {
    pub id: String,
    pub path: String,
    pub url: String,
    pub hash: String,
    pub name: String,
    pub private: bool,
}
#[derive(Serialize, Deserialize)]
pub struct Plan {
    pub format: u32,
    pub packages: Vec<Package>,
    #[serde(rename = "hasPatches", default)]
    pub has_patches: bool,
}

#[cfg(test)]
pub fn parse(input: &str) -> Result<Plan> {
    parse_with_registry(input, &BTreeMap::new())
}

pub fn parse_with_registry(input: &str, npmrc: &BTreeMap<String, String>) -> Result<Plan> {
    let lock: Lock = serde_yaml_ng::from_str(input).context("invalid pnpm lockfile")?;
    ensure!(
        lock.lockfile_version.as_str() == Some("9.0")
            || lock.lockfile_version.as_f64() == Some(9.0),
        "only pnpm lockfileVersion 9.0 is supported"
    );
    ensure!(
        lock.importers.contains_key("."),
        "lockfile must include a root importer"
    );
    let identity = Regex::new(r"^((?:@[a-z0-9._-]+/)?[a-z0-9._-]+)@(.+)$")?;
    let safe_filename = Regex::new(r"[^a-zA-Z0-9._+-]")?;
    let mut packages = Vec::new();
    for (id, package) in lock.packages {
        let captures = identity
            .captures(&id)
            .with_context(|| format!("unsupported package identity: {id}"))?;
        let resolution = package["resolution"]
            .as_object()
            .context("missing resolution")?;
        ensure!(
            resolution
                .keys()
                .all(|key| ["integrity", "tarball"].contains(&key.as_str()))
                && resolution.contains_key("integrity"),
            "{id}: expected integrity and optional tarball URL"
        );
        let hash = resolution["integrity"]
            .as_str()
            .context("integrity must be a string")?;
        let (algorithm, digest) = hash.split_once('-').context("expected SRI hash")?;
        let size = match algorithm {
            "sha256" => 32,
            "sha384" => 48,
            "sha512" => 64,
            _ => bail!("{id}: expected SHA-256/384/512 SRI hash"),
        };
        ensure!(
            STANDARD.decode(digest)?.len() == size,
            "{id}: invalid SRI digest length"
        );
        let name = &captures[1];
        let version = &captures[2];
        let registry = crate::config::registry(name, npmrc)?;
        let address = if let Some(tarball) = resolution.get("tarball").and_then(Value::as_str) {
            crate::config::url(&registry)?.join(tarball)?.to_string()
        } else {
            ensure!(
                !version.contains('/'),
                "URL dependencies require explicit tarball resolutions"
            );
            format!(
                "{registry}{name}/-/{}-{version}.tgz",
                name.rsplit('/').next().unwrap()
            )
        };
        let address = crate::config::url(&address)?;
        let digest = format!("{:x}", Sha256::digest(id.as_bytes()));
        let filename = address
            .path_segments()
            .and_then(|mut s| s.next_back())
            .filter(|s| !s.is_empty())
            .unwrap_or("package.tgz");
        let filename = safe_filename.replace_all(filename, "-");
        packages.push(Package {
            path: format!("/drvjs/{digest}"),
            name: format!("drvjs-{filename}"),
            private: address.host_str() != Some("registry.npmjs.org"),
            id,
            url: address.to_string(),
            hash: hash.to_owned(),
        });
    }
    for importer in lock.importers.values() {
        for section in ["dependencies", "devDependencies", "optionalDependencies"] {
            if let Some(deps) = importer[section].as_object() {
                for (name, dep) in deps {
                    let version = dep["version"]
                        .as_str()
                        .context("invalid dependency version")?;
                    ensure!(
                        !["file:", "workspace:"]
                            .iter()
                            .any(|prefix| version.starts_with(prefix)),
                        "{name}: local dependencies are not yet supported"
                    );
                }
            }
        }
    }
    Ok(Plan {
        format: 1,
        packages,
        has_patches: !lock.patched_dependencies.is_empty(),
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    fn fixture() -> Value {
        serde_json::json!({"lockfileVersion": "9.0", "importers": {".": {}}, "packages": {"@scope/example@1.2.3": {"resolution": {"integrity": format!("sha512-{}", STANDARD.encode([0;64]))}}}})
    }
    #[test]
    fn scoped_url() {
        let p = parse(&fixture().to_string()).unwrap();
        assert_eq!(
            p.packages[0].url,
            "https://registry.npmjs.org/@scope/example/-/example-1.2.3.tgz"
        );
    }
    #[test]
    fn unsupported_inputs() {
        for (key, value) in [
            ("lockfileVersion", serde_json::json!("6.0")),
            ("importers", serde_json::json!({"other": {}})),
        ] {
            let mut lock = fixture();
            lock[key] = value;
            assert!(parse(&lock.to_string()).is_err());
        }
    }
    #[test]
    fn bad_resolution() {
        for resolution in [
            serde_json::json!({"integrity":"sha512-invalid"}),
            serde_json::json!({"tarball":"https://example.org/file.tgz"}),
            serde_json::json!({"type":"git"}),
        ] {
            let mut lock = fixture();
            lock["packages"]["@scope/example@1.2.3"]["resolution"] = resolution;
            assert!(parse(&lock.to_string()).is_err());
        }
    }
    #[test]
    fn stable_fetch_identity() {
        let mut lock = fixture();
        let before = parse(&lock.to_string()).unwrap();
        lock["packages"]["z@2.0.0"] = lock["packages"]["@scope/example@1.2.3"].clone();
        assert_eq!(
            before.packages[0],
            parse(&lock.to_string()).unwrap().packages[0]
        );
    }

    #[test]
    fn patch_tools_follow_the_selected_lockfile() {
        let mut lock = fixture();
        let plain = parse(&lock.to_string()).unwrap();
        assert!(!plain.has_patches);
        lock["patchedDependencies"] = serde_json::json!({
            "@scope/example@1.2.3": {"path":"patches/example.patch", "hash":"locked"}
        });
        let patched = parse(&lock.to_string()).unwrap();
        assert!(patched.has_patches);
        assert_eq!(plain.packages, patched.packages);
    }
}
