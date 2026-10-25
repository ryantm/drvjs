use anyhow::{Context, Result, anyhow, ensure};
use base64::{Engine, engine::general_purpose::STANDARD};
use serde::Deserialize;
use serde_json::Value;
use std::{
    collections::{BTreeMap, BTreeSet},
    fs,
    path::{Path, PathBuf},
    process::Command,
    sync::{
        Arc,
        atomic::{AtomicBool, Ordering},
    },
    thread,
    time::{Duration, Instant},
};
use tiny_http::{Response, Server};
use walkdir::WalkDir;

pub fn pnpm(cwd: &Path, args: &[&str]) -> Result<()> {
    let home = tempfile::tempdir()?;
    let started = Instant::now();
    let status = Command::new("pnpm")
        .args(args)
        .env_clear()
        .env("PATH", std::env::var_os("PATH").unwrap_or_default())
        .env("HOME", home.path())
        .env("PNPM_SKIP_AUTH_CHECK", "true")
        .current_dir(cwd)
        .env("CI", "true")
        .env("npm_config_update_notifier", "false")
        .status()
        .context("running pnpm")?;
    eprintln!(
        "drvjs: pnpm {} took {:.3}s",
        args[0],
        started.elapsed().as_secs_f64()
    );
    ensure!(status.success(), "pnpm {} failed: {status}", args.join(" "));
    Ok(())
}

pub fn install(root: &Path) -> Result<()> {
    install_with_method(root, "copy")
}

fn install_with_method(root: &Path, method: &str) -> Result<()> {
    fs::create_dir_all(root.join("node_modules"))?;
    let store = root.join(".pnpm-store");
    pnpm(
        root,
        &[
            "install",
            "--offline",
            "--frozen-lockfile",
            "--ignore-scripts",
            "--store-dir",
            store.to_str().context("non-UTF8 path")?,
            &format!("--package-import-method={method}"),
            "--config.manage-package-manager-versions=false",
            "--config.package-manager-strict=false",
        ],
    )
}

/// pnpm 10's store status compares installed files with the *unpatched*
/// tarball index, so intentional patches are reported as corruption. Check
/// unpatched snapshots in a temporary view; verify() independently reconstructs
/// and compares all installed packages, including patched ones.
pub fn store_status(root: &Path) -> Result<()> {
    let lock = fs::read_to_string(root.join("pnpm-lock.yaml"))?;
    let mut data: Value = serde_yaml_ng::from_str(&lock)?;
    let tmp = tempfile::tempdir()?;
    crate::workspace::copy_support(root, tmp.path(), &lock)?;
    if let Some(snapshots) = data["snapshots"].as_object_mut() {
        snapshots.retain(|id, _| !id.contains("(patch_hash="));
    }
    fs::write(
        tmp.path().join("pnpm-lock.yaml"),
        serde_yaml_ng::to_string(&data)?,
    )?;
    std::os::unix::fs::symlink(root.join("node_modules"), tmp.path().join("node_modules"))?;
    pnpm(
        tmp.path(),
        &[
            "store",
            "status",
            "--store-dir",
            root.join(".pnpm-store").to_str().context("non-UTF8 path")?,
        ],
    )
}

#[derive(Deserialize)]
struct Tarball {
    id: String,
    path: String,
    file: String,
}

pub fn assemble(
    source: &Path,
    out: &Path,
    manifest: &Path,
    keep_verification_store: bool,
    unpacked_store: Option<&Path>,
) -> Result<()> {
    fs::create_dir(out)?;
    crate::workspace::copy_tree(source, out)?;
    let store = out.join(".pnpm-store");
    fs::create_dir_all(store.join("v10/index"))?;
    if let Some(cache) = unpacked_store {
        let started = Instant::now();
        prepare_unpacked_store(
            cache,
            &store,
            &fs::read_to_string(source.join("pnpm-lock.yaml"))?,
        )?;
        eprintln!(
            "drvjs: preparing immutable store took {:.3}s",
            started.elapsed().as_secs_f64()
        );
    } else {
        let entries: Vec<Tarball> = serde_json::from_slice(&fs::read(manifest)?)?;
        let tarballs: BTreeMap<String, String> = entries
            .iter()
            .map(|entry| (entry.path.clone(), entry.file.clone()))
            .collect();
        let server = Server::http("127.0.0.1:0").map_err(|e| anyhow!("local registry: {e}"))?;
        let origin = format!("http://{}", server.server_addr());
        let registry = format!("--registry={origin}");
        let stop = Arc::new(AtomicBool::new(false));
        let stopped = stop.clone();
        let worker = thread::spawn(move || -> Result<()> {
            while !stopped.load(Ordering::Relaxed) {
                if let Some(request) = server.recv_timeout(Duration::from_millis(100))? {
                    let path = percent_encoding::percent_decode_str(
                        request.url().split('?').next().unwrap(),
                    )
                    .decode_utf8()?;
                    if let Some(file) = tarballs.get(path.as_ref()) {
                        request.respond(Response::from_file(fs::File::open(file)?))?;
                    } else {
                        request.respond(
                            Response::from_string("not in the drvjs lockfile")
                                .with_status_code(404),
                        )?;
                    }
                }
            }
            Ok(())
        });
        // Keep the seed on the output filesystem: moving its virtual store is
        // atomic, and pnpm can hardlink package files instead of copying bytes.
        let seed = tempfile::tempdir_in(out)?;
        let original_lock = fs::read_to_string(source.join("pnpm-lock.yaml"))?;
        crate::workspace::copy_support(source, seed.path(), &original_lock)?;
        if source.join(".npmrc").exists() {
            fs::copy(source.join(".npmrc"), seed.path().join(".npmrc"))?;
        }
        let mut seed_lock: Value = serde_yaml_ng::from_str(&original_lock)?;
        for entry in &entries {
            ensure!(
                seed_lock["packages"].get(&entry.id).is_some(),
                "tarball manifest does not match lockfile"
            );
            seed_lock["packages"][&entry.id]["resolution"]["tarball"] =
                format!("{origin}{}", entry.path).into();
        }
        fs::write(
            seed.path().join("pnpm-lock.yaml"),
            serde_yaml_ng::to_string(&seed_lock)?,
        )?;
        let fetched = pnpm(
            seed.path(),
            &[
                "fetch",
                "--ignore-scripts",
                "--store-dir",
                store.to_str().context("non-UTF8 path")?,
                "--package-import-method=hardlink",
                "--config.manage-package-manager-versions=false",
                "--config.package-manager-strict=false",
                &registry,
                "--fetch-retries=0",
            ],
        );
        stop.store(true, Ordering::Relaxed);
        worker
            .join()
            .map_err(|_| anyhow!("registry thread panicked"))??;
        fetched?;
        // fetch already materialized the package files. Reuse that virtual store;
        // the offline install only finishes workspace links, hoisting and bin shims.
        fs::rename(seed.path().join("node_modules"), out.join("node_modules"))?;
        // fetch records empty hoist patterns and a provisional current lockfile.
        // Discard both so install regenerates *all* bin shims with the real config,
        // including bins inside already-materialized packages and bundled deps.
        for relative in ["node_modules/.modules.yaml", "node_modules/.pnpm/lock.yaml"] {
            let metadata = out.join(relative);
            if metadata.exists() {
                fs::remove_file(metadata)?;
            }
        }
    }
    // Hardlinking overlay symlinks would leak CAFS paths into node_modules,
    // breaking Node's realpath-based module resolution. Import regular files.
    install_with_method(
        out,
        if unpacked_store.is_some() {
            "copy"
        } else {
            "hardlink"
        },
    )?;
    if keep_verification_store {
        if unpacked_store.is_some() {
            materialize_store_links(&store)?;
        }
        store_status(out)?;
    }
    ensure!(
        fs::read(source.join("pnpm-lock.yaml"))? == fs::read(out.join("pnpm-lock.yaml"))?,
        "pnpm changed the lockfile"
    );
    if keep_verification_store {
        // Remove temporary project registrations and normalize volatile metadata.
        let projects = store.join("v10/projects");
        if projects.exists() {
            fs::remove_dir_all(projects)?;
        }
        for entry in WalkDir::new(store.join("v10/index")) {
            let entry = entry?;
            if entry.file_type().is_file() {
                let mut data: Value = serde_json::from_slice(&fs::read(entry.path())?)?;
                remove_checked_at(&mut data);
                fs::write(entry.path(), serde_json::to_vec(&data)?)?;
            }
        }
    } else {
        // Hardlinks remain valid after their cache names are removed. Drop
        // the build-time cache before Nix registers the output.
        fs::remove_dir_all(&store)?;
    }
    let metadata = out.join("node_modules/.modules.yaml");
    if metadata.exists() {
        let mut data: Value = serde_yaml_ng::from_slice(&fs::read(&metadata)?)?;
        data["prunedAt"] = "Thu, 01 Jan 1970 00:00:00 GMT".into();
        fs::write(metadata, serde_json::to_vec_pretty(&data)?)?;
    }
    let state = out.join("node_modules/.pnpm-workspace-state-v1.json");
    if state.exists() {
        let mut data: Value = serde_json::from_slice(&fs::read(&state)?)?;
        data["lastValidatedTimestamp"] = 0.into();
        fs::write(state, serde_json::to_vec_pretty(&data)?)?;
    }
    Ok(())
}

// Patch side-effect indexes also contain per-file checkedAt timestamps.
fn remove_checked_at(value: &mut Value) {
    match value {
        Value::Object(object) => {
            object.remove("checkedAt");
            for value in object.values_mut() {
                remove_checked_at(value);
            }
        }
        Value::Array(array) => array.iter_mut().for_each(remove_checked_at),
        _ => {}
    }
}

/// Decode pnpm's integrity-addressed CAFS paths, without accepting paths from
/// index metadata. The pnpm version/store format is pinned by the Nix package.
fn integrity_hex(integrity: &str) -> Result<String> {
    let (algorithm, digest) = integrity
        .split_once('-')
        .context("invalid store integrity")?;
    let bytes = STANDARD.decode(digest)?;
    let length = match algorithm {
        "sha256" => 32,
        "sha384" => 48,
        "sha512" => 64,
        _ => anyhow::bail!("unsupported store integrity algorithm: {algorithm}"),
    };
    ensure!(bytes.len() == length, "invalid store integrity length");
    Ok(bytes.iter().map(|byte| format!("{byte:02x}")).collect())
}

fn collect_patch_files(value: &Value, files: &mut BTreeSet<PathBuf>) -> Result<()> {
    if let Some(object) = value.as_object() {
        if let (Some(integrity), Some(mode)) = (
            object.get("integrity").and_then(Value::as_str),
            object.get("mode").and_then(Value::as_u64),
        ) {
            let hex = integrity_hex(integrity)?;
            let suffix = if mode & 0o111 != 0 { "-exec" } else { "" };
            files.insert(PathBuf::from(format!(
                "v10/files/{}/{}{suffix}",
                &hex[..2],
                &hex[2..]
            )));
        }
        for item in object.values() {
            collect_patch_files(item, files)?;
        }
    }
    Ok(())
}

fn patched_files(source: &Path, lock: &str) -> Result<BTreeSet<PathBuf>> {
    let lock: Value = serde_yaml_ng::from_str(lock)?;
    let mut prefixes = BTreeSet::new();
    if let Some(snapshots) = lock["snapshots"].as_object() {
        for id in snapshots.keys().filter(|id| id.contains("(patch_hash=")) {
            let base = id.split('(').next().unwrap();
            let integrity = lock["packages"][base]["resolution"]["integrity"]
                .as_str()
                .context("patched package missing integrity")?;
            // pnpm truncates tarball hashes to 64 hex characters for indexes.
            let hex = integrity_hex(integrity)?;
            prefixes.insert(format!("{}/{}-", &hex[..2], &hex[2..64]));
        }
    }
    let mut files = BTreeSet::new();
    if prefixes.is_empty() {
        return Ok(files);
    }
    let index = source.join("v10/index");
    for entry in WalkDir::new(&index) {
        let entry = entry?;
        if !entry.file_type().is_file() {
            continue;
        }
        let relative = entry.path().strip_prefix(&index)?.to_string_lossy();
        if prefixes.iter().any(|prefix| relative.starts_with(prefix)) {
            collect_patch_files(
                &serde_json::from_slice::<Value>(&fs::read(entry.path())?)?,
                &mut files,
            )?;
        }
    }
    Ok(files)
}

/// A private writable directory tree overlays immutable contents with symlinks.
/// pnpm may add/replace CAFS entries and update indexes without touching the
/// input. Only patched package contents need writable copies before import.
fn prepare_unpacked_store(source: &Path, destination: &Path, lock: &str) -> Result<()> {
    use std::os::unix::fs::symlink;
    let source = source.canonicalize()?;
    let mutable = patched_files(&source, lock)?;
    let mut linked = 0;
    let mut copied = 0;
    for component in ["v10/files", "v10/index"] {
        let root = source.join(component);
        ensure!(root.is_dir(), "unpacked store is missing {component}");
        for entry in WalkDir::new(&root).follow_links(false) {
            let entry = entry?;
            let relative = entry.path().strip_prefix(&source)?;
            let target = destination.join(relative);
            if entry.file_type().is_dir() {
                fs::create_dir_all(&target)?;
            } else {
                ensure!(
                    entry.file_type().is_file(),
                    "unexpected unpacked-store symlink: {}",
                    entry.path().display()
                );
                if component == "v10/files" && !mutable.contains(relative) {
                    symlink(entry.path(), &target)?;
                    linked += 1;
                } else {
                    copy_writable(entry.path(), &target)?;
                    if component == "v10/files" {
                        copied += 1;
                    }
                }
            }
        }
    }
    eprintln!(
        "drvjs: immutable store: {linked} linked content files, {copied} writable patch files"
    );
    Ok(())
}

fn copy_writable(source: &Path, destination: &Path) -> Result<()> {
    use std::os::unix::fs::PermissionsExt;
    fs::copy(source, destination)?;
    let mode = fs::metadata(source)?.permissions().mode() | 0o200;
    fs::set_permissions(destination, fs::Permissions::from_mode(mode))?;
    Ok(())
}

/// Opt-in verification outputs remain self-contained and can seed later builds.
fn materialize_store_links(store: &Path) -> Result<()> {
    for entry in WalkDir::new(store.join("v10/files")) {
        let entry = entry?;
        if entry.file_type().is_symlink() {
            let source = fs::read_link(entry.path())?;
            ensure!(source.is_file(), "invalid immutable store link");
            fs::remove_file(entry.path())?;
            copy_writable(&source, entry.path())?;
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::os::unix::fs::{PermissionsExt, symlink};

    #[test]
    fn unpacked_store_links_immutable_files_and_materializes_retained_store() {
        let tmp = tempfile::tempdir().unwrap();
        let cache = tmp.path().join("cache");
        let output = tmp.path().join("output");
        fs::create_dir_all(cache.join("v10/files/aa")).unwrap();
        fs::create_dir_all(cache.join("v10/index")).unwrap();
        let input = cache.join("v10/files/aa/file");
        fs::write(&input, b"original").unwrap();
        fs::set_permissions(&input, fs::Permissions::from_mode(0o555)).unwrap();
        prepare_unpacked_store(&cache, &output, "snapshots: {}\n").unwrap();
        assert!(output.join("v10/files/aa/file").is_symlink());
        materialize_store_links(&output).unwrap();
        let installed = output.join("v10/files/aa/file");
        assert_eq!(
            fs::metadata(&installed).unwrap().permissions().mode() & 0o777,
            0o755
        );
        fs::write(&installed, b"changed").unwrap();
        assert_eq!(fs::read(input).unwrap(), b"original");
    }

    #[test]
    fn unpacked_store_rejects_symlinks() {
        let tmp = tempfile::tempdir().unwrap();
        let cache = tmp.path().join("cache");
        fs::create_dir_all(cache.join("v10/files")).unwrap();
        fs::create_dir_all(cache.join("v10/index")).unwrap();
        symlink("/etc/passwd", cache.join("v10/files/link")).unwrap();
        assert!(
            prepare_unpacked_store(&cache, &tmp.path().join("output"), "snapshots: {}\n").is_err()
        );
    }
}
