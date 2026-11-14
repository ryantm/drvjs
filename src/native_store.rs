//! pnpm v10 CAFS reader/writer. No pnpm process participates in assembly.
use anyhow::{Context, Result, ensure};
use base64::{Engine, engine::general_purpose::STANDARD};
use serde_json::{Value, json};
use sha2::{Digest, Sha256, Sha384, Sha512};
use std::{
    collections::BTreeMap,
    fs,
    io::Read,
    os::unix::fs::PermissionsExt,
    path::{Component, Path, PathBuf},
    process::Command,
};
use walkdir::WalkDir;

pub fn safe_relative(name: &str) -> Result<&Path> {
    let p = Path::new(name);
    ensure!(
        !p.is_absolute() && !p.components().any(|c| matches!(c, Component::ParentDir)),
        "unsafe package path: {name}"
    );
    Ok(p)
}
fn hex(bytes: &[u8]) -> String {
    const HEX: &[u8; 16] = b"0123456789abcdef";
    let mut result = String::with_capacity(bytes.len() * 2);
    for &byte in bytes {
        result.push(HEX[(byte >> 4) as usize] as char);
        result.push(HEX[(byte & 15) as usize] as char);
    }
    result
}
pub fn sri_hex(integrity: &str) -> Result<String> {
    let (alg, encoded) = integrity.split_once('-').context("invalid integrity")?;
    let bytes = STANDARD.decode(encoded)?;
    ensure!(
        matches!(
            (alg, bytes.len()),
            ("sha256", 32) | ("sha384", 48) | ("sha512", 64)
        ),
        "invalid integrity algorithm/length"
    );
    Ok(hex(&bytes))
}
pub fn index_path(root: &Path, id: &str, integrity: &str) -> Result<PathBuf> {
    let hash = sri_hex(integrity)?;
    let id = if id.contains(':') {
        id.split_once('@')
            .filter(|(name, _)| !name.is_empty())
            .map(|(_, v)| v)
            .or_else(|| id[1..].split_once('@').map(|(_, v)| v))
            .unwrap_or(id)
    } else {
        id
    };
    let id: String = id
        .chars()
        .map(|c| if "\\/:*?\"<>|".contains(c) { '+' } else { c })
        .collect();
    Ok(root.join(format!(
        "v10/index/{}/{}-{id}.json",
        &hash[..2],
        &hash[2..64]
    )))
}
pub fn content_path(root: &Path, info: &Value) -> Result<PathBuf> {
    let hash = sri_hex(
        info["integrity"]
            .as_str()
            .context("file missing integrity")?,
    )?;
    let suffix = if info["mode"].as_u64().unwrap_or(0) & 0o111 != 0 {
        "-exec"
    } else {
        ""
    };
    Ok(root.join(format!("v10/files/{}/{}{suffix}", &hash[..2], &hash[2..])))
}
pub fn write_json(path: &Path, value: &Value) -> Result<()> {
    fs::create_dir_all(path.parent().context("missing parent")?)?;
    fs::write(path, serde_json::to_vec_pretty(value)?)?;
    Ok(())
}
fn save_file(store: &Path, bytes: &[u8], mode: u32) -> Result<Value> {
    let info = json!({"integrity":format!("sha512-{}", STANDARD.encode(Sha512::digest(bytes))),"mode":mode,"size":bytes.len()});
    let path = content_path(store, &info)?;
    // Parallel tarball extraction can encounter identical content. Publish each
    // file completely before another package can reuse it.
    static LOCKS: std::sync::LazyLock<Vec<std::sync::Mutex<()>>> =
        std::sync::LazyLock::new(|| (0..256).map(|_| std::sync::Mutex::new(())).collect());
    let shard = path
        .parent()
        .unwrap()
        .file_name()
        .unwrap()
        .to_str()
        .unwrap();
    let _guard = LOCKS[usize::from_str_radix(shard, 16)?]
        .lock()
        .map_err(|_| anyhow::anyhow!("content lock poisoned"))?;
    if !path.exists() {
        fs::create_dir_all(path.parent().unwrap())?;
        fs::write(&path, bytes)?;
        fs::set_permissions(
            path,
            fs::Permissions::from_mode(if mode & 0o111 != 0 { 0o755 } else { 0o644 }),
        )?;
    }
    Ok(info)
}
pub fn requires_build(manifest: &Value, files: &Value) -> bool {
    ["preinstall", "install", "postinstall"].iter().any(|k| {
        manifest["scripts"][k]
            .as_str()
            .is_some_and(|s| !s.is_empty())
    }) || files.get("binding.gyp").is_some()
        || files
            .as_object()
            .is_some_and(|f| f.keys().any(|k| k.starts_with(".hooks/")))
}
pub fn unpack(tarball: &Path, store: &Path, id: &str, integrity: &str) -> Result<Value> {
    unpack_impl(tarball, store, id, integrity, None, false, None)
}
pub fn unpack_to(tarball: &Path, target: &Path, id: &str, integrity: &str) -> Result<Value> {
    unpack_impl(tarball, target, id, integrity, None, true, None)
}
/// Prepared archives are trusted Nix transformations of an SRI-checked source.
/// Check the prepared payload separately; pnpm indexes retain the source SRI.
pub fn unpack_prepared(
    tarball: &Path,
    store: &Path,
    id: &str,
    integrity: &str,
    archive_integrity: &str,
    direct: bool,
) -> Result<Value> {
    unpack_impl(
        tarball,
        store,
        id,
        integrity,
        Some(archive_integrity),
        direct,
        None,
    )
}
/// Inspect authenticated manifest constraints before writing optional packages.
/// A rejected package returns only its manifest; it creates no files or index.
pub fn unpack_filtered(
    tarball: &Path,
    target: &Path,
    id: &str,
    integrity: &str,
    archive_integrity: Option<&str>,
    direct: bool,
    accept: Option<&dyn Fn(&Value) -> bool>,
) -> Result<Value> {
    if accept.is_none() {
        return match (archive_integrity, direct) {
            (Some(prepared), _) => {
                unpack_prepared(tarball, target, id, integrity, prepared, direct)
            }
            (None, true) => unpack_to(tarball, target, id, integrity),
            (None, false) => unpack(tarball, target, id, integrity),
        };
    }
    unpack_impl(
        tarball,
        target,
        id,
        integrity,
        archive_integrity,
        direct,
        accept,
    )
}
fn unpack_impl(
    tarball: &Path,
    store: &Path,
    id: &str,
    integrity: &str,
    archive_integrity: Option<&str>,
    direct: bool,
    accept: Option<&dyn Fn(&Value) -> bool>,
) -> Result<Value> {
    let bytes = fs::read(tarball)?;
    if archive_integrity.is_some() {
        ensure!(
            bytes.starts_with(&[0x28, 0xb5, 0x2f, 0xfd]),
            "expected prepared zstd archive"
        );
    }
    let checked_integrity = archive_integrity.unwrap_or(integrity);
    let expected = sri_hex(checked_integrity)?;
    let actual = match checked_integrity.split_once('-').unwrap().0 {
        "sha256" => hex(&Sha256::digest(&bytes)),
        "sha384" => hex(&Sha384::digest(&bytes)),
        _ => hex(&Sha512::digest(&bytes)),
    };
    ensure!(actual == expected, "tarball integrity mismatch: {id}");
    if archive_integrity.is_some() {
        let size = zstd::zstd_safe::get_frame_content_size(&bytes)
            .map_err(|_| anyhow::anyhow!("invalid zstd frame"))?;
        if let Some(size) = size.filter(|size| *size <= 256 * 1024 * 1024) {
            let output = zstd::bulk::decompress(&bytes, size as usize)?;
            return extract_raw(&output, store, id, integrity, direct, accept);
        }
        return extract_stream(
            || Ok(zstd::stream::read::Decoder::new(bytes.as_slice())?),
            store,
            id,
            integrity,
            direct,
            accept,
        );
    }
    // Decode ordinary gzip archives in one bounded buffer. Larger or unusual
    // streams use the existing streaming decoder. SRI is verified above.
    let expanded = if bytes.starts_with(&[0x1f, 0x8b]) && bytes.len() >= 18 {
        let size = u32::from_le_bytes(bytes[bytes.len() - 4..].try_into().unwrap()) as usize;
        if size <= 256 * 1024 * 1024 {
            let mut output = vec![0; size];
            match libdeflater::Decompressor::new().gzip_decompress(&bytes, &mut output) {
                Ok(size) => {
                    output.truncate(size);
                    Some(output)
                }
                Err(_) => None,
            }
        } else {
            None
        }
    } else {
        None
    };
    if let Some(output) = &expanded {
        return extract_raw(output, store, id, integrity, direct, accept);
    }
    if !bytes.starts_with(&[0x1f, 0x8b]) {
        return extract_raw(&bytes, store, id, integrity, direct, accept);
    }
    extract_stream(
        || Ok(flate2::read::GzDecoder::new(bytes.as_slice())),
        store,
        id,
        integrity,
        direct,
        accept,
    )
}

fn archive_manifest<R: Read>(entries: tar::Entries<'_, R>) -> Result<Value> {
    let mut manifest = None;
    for entry in entries {
        let mut entry = entry?;
        if !entry.header().entry_type().is_file() {
            continue;
        }
        let path: PathBuf = entry.path()?.components().skip(1).collect();
        if path == Path::new("package.json") {
            let mut bytes = Vec::new();
            entry.read_to_end(&mut bytes)?;
            manifest = Some(serde_json::from_slice(&bytes)?);
        }
    }
    manifest.context("package.json missing")
}

fn extract_raw(
    raw: &[u8],
    store: &Path,
    id: &str,
    integrity: &str,
    direct: bool,
    accept: Option<&dyn Fn(&Value) -> bool>,
) -> Result<Value> {
    let manifest = if let Some(accept) = accept {
        let mut archive = tar::Archive::new(std::io::Cursor::new(raw));
        let manifest = archive_manifest(archive.entries_with_seek()?)?;
        if !accept(&manifest) {
            return Ok(json!({"manifest":manifest, "files":{}}));
        }
        Some(manifest)
    } else {
        None
    };
    let mut archive = tar::Archive::new(std::io::Cursor::new(raw));
    let mut index = extract_entries(
        archive.entries_with_seek()?,
        Some(raw),
        store,
        id,
        integrity,
        direct,
    )?;
    if let Some(manifest) = manifest {
        index["manifest"] = manifest;
    }
    Ok(index)
}

fn extract_stream<R: Read>(
    reader: impl Fn() -> Result<R>,
    store: &Path,
    id: &str,
    integrity: &str,
    direct: bool,
    accept: Option<&dyn Fn(&Value) -> bool>,
) -> Result<Value> {
    // The uncommon >256 MiB/streaming fallback scans twice, with bounded memory.
    let manifest = if let Some(accept) = accept {
        let mut archive = tar::Archive::new(reader()?);
        let manifest = archive_manifest(archive.entries()?)?;
        if !accept(&manifest) {
            return Ok(json!({"manifest":manifest, "files":{}}));
        }
        Some(manifest)
    } else {
        None
    };
    let mut archive = tar::Archive::new(reader()?);
    let mut index = extract_entries(archive.entries()?, None, store, id, integrity, direct)?;
    if let Some(manifest) = manifest {
        index["manifest"] = manifest;
    }
    Ok(index)
}
fn extract_entries<R: Read>(
    entries: tar::Entries<'_, R>,
    raw: Option<&[u8]>,
    store: &Path,
    id: &str,
    integrity: &str,
    direct: bool,
) -> Result<Value> {
    let mut package_manifest = None;
    let mut files = serde_json::Map::new();
    let mut directories = std::collections::BTreeSet::new();
    for entry in entries {
        let mut entry = entry?;
        // npm/pnpm install regular entries only, not archive links or devices.
        if !entry.header().entry_type().is_file() {
            continue;
        }
        let path = entry.path()?.into_owned();
        let stripped: PathBuf = path.components().skip(1).collect();
        let name = stripped.to_str().context("non-UTF8 package filename")?;
        safe_relative(name)?;
        if name.is_empty() {
            continue;
        }
        let mode = entry.header().mode()?;
        let mut owned = Vec::new();
        let content = if let Some(raw) = raw {
            let start = usize::try_from(entry.raw_file_position())?;
            let size = usize::try_from(entry.size())?;
            raw.get(start..start.checked_add(size).context("tar entry size overflow")?)
                .context("truncated tar entry")?
        } else {
            owned.reserve(entry.size().min(8 * 1024 * 1024) as usize);
            entry.read_to_end(&mut owned)?;
            owned.as_slice()
        };
        if direct && name == "package.json" {
            package_manifest = Some(serde_json::from_slice::<Value>(content)?);
        }
        let info = if direct {
            let target = store.join(name);
            if directories.insert(target.parent().unwrap().to_path_buf()) {
                fs::create_dir_all(target.parent().unwrap())?;
            }
            fs::write(&target, content)?;
            if mode & 0o111 != 0 {
                fs::set_permissions(target, fs::Permissions::from_mode(0o755))?;
            }
            // Direct outputs need only file names for peer copies. Full CAFS
            // file metadata is retained only when constructing a pnpm store.
            Value::Null
        } else {
            save_file(store, content, mode)?
        };
        files.insert(name.to_owned(), info);
    }
    let files = Value::Object(files);
    let manifest: Value = if direct {
        package_manifest.context("package.json missing")?
    } else {
        serde_json::from_slice(&fs::read(content_path(store, &files["package.json"])?)?)?
    };
    let mut index = json!({"name":manifest["name"],"version":manifest["version"],"requiresBuild":requires_build(&manifest,&files),"files":files});
    if direct {
        index["manifest"] = manifest;
    }
    if !direct {
        write_json(&index_path(store, id, integrity)?, &index)?;
    }
    Ok(index)
}
pub fn manifest(store: &Path, index: &Value) -> Result<Value> {
    Ok(serde_json::from_slice(&fs::read(content_path(
        store,
        &index["files"]["package.json"],
    )?)?)?)
}
pub fn verified_manifest(store: &Path, index: &Value) -> Result<Value> {
    let info = &index["files"]["package.json"];
    let bytes = fs::read(content_path(store, info)?)?;
    verify_bytes(&bytes, info)?;
    Ok(serde_json::from_slice(&bytes)?)
}
/// Bounded independent I/O work. Never creates a thread for every package.
pub fn parallel_each<T: Sync>(items: &[T], f: impl Fn(&T) -> Result<()> + Sync) -> Result<()> {
    use std::sync::atomic::{AtomicUsize, Ordering};
    let available = std::thread::available_parallelism()
        .map(usize::from)
        .unwrap_or(1);
    // More writers increase filesystem contention on package trees with many
    // small files. Four workers performed better than eight in matched trials.
    let configured = |name| {
        std::env::var(name)
            .ok()
            .and_then(|s| s.parse::<usize>().ok())
            .filter(|n| *n > 0)
    };
    let budget = configured("NIX_BUILD_CORES").unwrap_or(available);
    let jobs = configured("DRVJS_JOBS")
        .unwrap_or(4)
        .min(budget)
        .min(items.len())
        .max(1);
    let next = AtomicUsize::new(0);
    std::thread::scope(|scope| {
        let handles: Vec<_> = (0..jobs)
            .map(|_| {
                let next = &next;
                let f = &f;
                scope.spawn(move || -> Result<()> {
                    while let Some(item) = items.get(next.fetch_add(1, Ordering::Relaxed)) {
                        f(item)?;
                    }
                    Ok(())
                })
            })
            .collect();
        for handle in handles {
            handle
                .join()
                .map_err(|_| anyhow::anyhow!("native assembly worker panicked"))??;
        }
        Ok(())
    })
}

/// Validate each content object once without modifying a supplied cache.
#[cfg(test)]
pub fn verify_indexes(store: &Path, indexes: &BTreeMap<String, Value>) -> Result<()> {
    let mut files = BTreeMap::new();
    for index in indexes.values() {
        for info in index["files"]
            .as_object()
            .context("missing package files")?
            .values()
        {
            files.entry(content_path(store, info)?).or_insert(info);
        }
    }
    let files: Vec<_> = files.into_iter().collect();
    parallel_each(&files, |(path, info)| verify_file(path, info))
}
#[cfg(test)]
fn verify_file(path: &Path, info: &Value) -> Result<()> {
    verify_bytes(&fs::read(path)?, info)
        .with_context(|| format!("cached file integrity mismatch: {}", path.display()))
}
fn verify_bytes(bytes: &[u8], info: &Value) -> Result<()> {
    let integrity = info["integrity"].as_str().context("missing integrity")?;
    let actual = match integrity.split_once('-').context("invalid integrity")?.0 {
        "sha256" => hex(&Sha256::digest(bytes)),
        "sha384" => hex(&Sha384::digest(bytes)),
        "sha512" => hex(&Sha512::digest(bytes)),
        _ => anyhow::bail!("unsupported file integrity"),
    };
    ensure!(
        actual == sri_hex(integrity)?,
        "cached file integrity mismatch"
    );
    Ok(())
}
pub fn materialize(
    store: &Path,
    index: &Value,
    target: &Path,
    verify: bool,
    share: bool,
) -> Result<()> {
    let mut directories = std::collections::BTreeSet::new();
    for (name, info) in index["files"]
        .as_object()
        .context("missing package files")?
    {
        let target = target.join(safe_relative(name)?);
        if directories.insert(target.parent().unwrap().to_path_buf()) {
            fs::create_dir_all(target.parent().unwrap())?;
        }
        let source = content_path(store, info)?;
        if share && fs::hard_link(&source, &target).is_ok() {
            continue;
        }
        let bytes = fs::read(&source).with_context(|| format!("reading {}", source.display()))?;
        if verify {
            verify_bytes(&bytes, info)
                .with_context(|| format!("verifying {}", source.display()))?;
        }
        fs::write(&target, bytes)?;
        if info["mode"].as_u64().unwrap_or(0) & 0o111 != 0 {
            fs::set_permissions(target, fs::Permissions::from_mode(0o755))?;
        }
    }
    Ok(())
}
pub fn materialize_directory(
    source: &Path,
    index: &Value,
    target: &Path,
    share: bool,
) -> Result<()> {
    if source == target {
        return Ok(());
    }
    let mut directories = std::collections::BTreeSet::new();
    for name in index["files"]
        .as_object()
        .context("missing package files")?
        .keys()
    {
        let relative = safe_relative(name)?;
        let target = target.join(relative);
        if directories.insert(target.parent().unwrap().to_path_buf()) {
            fs::create_dir_all(target.parent().unwrap())?;
        }
        if !share || fs::hard_link(source.join(relative), &target).is_err() {
            fs::copy(source.join(relative), target)?;
        }
    }
    Ok(())
}
pub fn make_executable(path: &Path) -> Result<()> {
    let mode = fs::metadata(path)?.permissions().mode();
    if mode & 0o111 == 0o111 {
        return Ok(());
    }
    let temp = tempfile::NamedTempFile::new_in(path.parent().context("bin missing parent")?)?;
    fs::copy(path, temp.path())?;
    fs::set_permissions(temp.path(), fs::Permissions::from_mode(mode | 0o111))?;
    temp.persist(path)?;
    Ok(())
}
pub fn patch(root: &Path, target: &Path, declaration: &Value) -> Result<()> {
    apply_patch(root, target, declaration, None)
}
pub fn patch_cached(root: &Path, target: &Path, declaration: &Value, package: &str) -> Result<()> {
    apply_patch(root, target, declaration, Some(package))
}
fn apply_patch(
    root: &Path,
    target: &Path,
    declaration: &Value,
    cached_package: Option<&str>,
) -> Result<()> {
    let relative = declaration["path"].as_str().context("patch missing path")?;
    let path = crate::config::safe_file(root, relative)?;
    let text = fs::read_to_string(&path)?.replace("\r\n", "\n");
    ensure!(
        hex(&Sha256::digest(text.as_bytes()))
            == declaration["hash"].as_str().context("patch missing hash")?,
        "patch checksum differs from lockfile: {relative}"
    );
    // pnpm's patch parser accepts a missing final newline; git apply requires
    // one. This framing change does not change patch contents or the hash check.
    let prepared = tempfile::NamedTempFile::new()?;
    fs::write(
        prepared.path(),
        if text.ends_with('\n') {
            text.clone()
        } else {
            format!("{text}\n")
        },
    )?;
    let command = || {
        let mut command = Command::new("git");
        command
            .args(["-c", "core.autocrlf=false", "apply", "--whitespace=nowarn"])
            .env("GIT_CONFIG_NOSYSTEM", "1")
            .env("GIT_CONFIG_GLOBAL", "/dev/null")
            .env("GIT_CEILING_DIRECTORIES", target.parent().unwrap())
            .current_dir(target);
        command
    };
    // Check the entire patch before writing: mixed pre/postimage hunks must not
    // be mistaken for a fully applied patch. A successful reverse check proves
    // every hunk already matches its postimage; it never reverses target files.
    let forward = command()
        .args(["--check", "--"])
        .arg(prepared.path())
        .output()
        .context("checking patch preimage")?;
    if forward.status.success() {
        let status = command()
            .arg("--")
            .arg(prepared.path())
            .status()
            .context("running git apply")?;
        ensure!(status.success(), "patch failed: {relative}");
    } else {
        let reverse = command()
            .args(["--reverse", "--check", "--"])
            .arg(prepared.path())
            .output()
            .context("checking complete patch postimage")?;
        if !reverse.status.success() {
            let detail = String::from_utf8_lossy(&forward.stderr);
            if let Some(package) = cached_package {
                anyhow::bail!(
                    "incompatible --unpacked-store entry or patch for {package}: neither the complete preimage nor postimage matches ({relative}); content may be partially patched or incompatible. Rebuild without --unpacked-store using the locked tarballs to check the source; optionally retain a new store with --keep-verification-store. The supplied store was not modified. git apply: {}",
                    detail.trim()
                );
            }
            anyhow::bail!(
                "patch failed: {relative}: neither the complete preimage nor postimage matches; content may be partially patched or incompatible. git apply: {}",
                detail.trim()
            );
        }
        eprintln!("drvjs: patch already applied: {relative}");
    }
    for entry in WalkDir::new(target) {
        let e = entry?;
        ensure!(
            !e.file_type().is_symlink(),
            "patch introduced unsupported symlink: {}",
            e.path().display()
        );
    }
    Ok(())
}
pub fn retain(
    source: &Path,
    destination: &Path,
    indexes: &BTreeMap<String, Value>,
    lock: &Value,
) -> Result<()> {
    fs::create_dir_all(destination.join("v10/files"))?;
    fs::create_dir_all(destination.join("v10/index"))?;
    for (id, index) in indexes {
        let integrity = lock["packages"][id]["resolution"]["integrity"]
            .as_str()
            .context("missing integrity")?;
        write_json(&index_path(destination, id, integrity)?, index)?;
        for info in index["files"]
            .as_object()
            .context("missing files")?
            .values()
        {
            let dst = content_path(destination, info)?;
            if !dst.exists() {
                fs::create_dir_all(dst.parent().unwrap())?;
                fs::copy(content_path(source, info)?, &dst)?;
            }
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn optional_manifest_filter_checks_integrity_before_skipping_and_never_writes_rejected_files() {
        let root = tempfile::tempdir().unwrap();
        let mut archive = tar::Builder::new(Vec::new());
        // Put the manifest last, as binary packages commonly do.
        for (name, bytes) in [
            ("package/large.node", vec![7; 1024 * 1024]),
            (
                "package/package.json",
                br#"{"name":"optional","version":"1.0.0","libc":["musl"]}"#.to_vec(),
            ),
        ] {
            let mut header = tar::Header::new_gnu();
            header.set_size(bytes.len() as u64);
            header.set_mode(0o644);
            header.set_cksum();
            archive
                .append_data(&mut header, name, bytes.as_slice())
                .unwrap();
        }
        let tar = archive.into_inner().unwrap();
        let mut encoder = flate2::write::GzEncoder::new(Vec::new(), flate2::Compression::fast());
        std::io::Write::write_all(&mut encoder, &tar).unwrap();
        let gzip = encoder.finish().unwrap();
        let zstd = zstd::bulk::compress(&tar, 1).unwrap();
        let reject = |manifest: &Value| manifest["libc"] != json!(["musl"]);
        for (i, bytes) in [&tar, &gzip, &zstd].into_iter().enumerate() {
            let path = root.path().join(format!("archive-{i}"));
            fs::write(&path, bytes).unwrap();
            let sri = format!("sha512-{}", STANDARD.encode(Sha512::digest(bytes)));
            let prepared = if i == 2 { Some(sri.as_str()) } else { None };
            let target = root.path().join(format!("rejected-{i}"));
            let index = unpack_filtered(
                &path,
                &target,
                "optional@1.0.0",
                &sri,
                prepared,
                true,
                Some(&reject),
            )
            .unwrap();
            assert_eq!(index["manifest"]["name"], "optional");
            assert!(!target.exists());
            let accepted = root.path().join(format!("accepted-{i}"));
            unpack_filtered(
                &path,
                &accepted,
                "optional@1.0.0",
                &sri,
                prepared,
                true,
                Some(&|_| true),
            )
            .unwrap();
            assert_eq!(
                fs::read(accepted.join("large.node")).unwrap(),
                vec![7; 1024 * 1024]
            );
            fs::write(&path, b"corrupt").unwrap();
            assert!(
                unpack_filtered(
                    &path,
                    &target,
                    "optional@1.0.0",
                    &sri,
                    prepared,
                    true,
                    Some(&reject)
                )
                .is_err()
            );
        }
        let streaming = root.path().join("streaming");
        let reader = || Ok(flate2::read::GzDecoder::new(gzip.as_slice()));
        extract_stream(
            reader,
            &streaming,
            "optional@1.0.0",
            "unused",
            true,
            Some(&reject),
        )
        .unwrap();
        assert!(!streaming.exists());
        extract_stream(
            reader,
            &streaming,
            "optional@1.0.0",
            "unused",
            true,
            Some(&|_| true),
        )
        .unwrap();
        assert_eq!(
            fs::read(streaming.join("large.node")).unwrap(),
            vec![7; 1024 * 1024]
        );
    }

    fn archive_fixture() -> Vec<u8> {
        use tar::{Builder, Header};
        let mut archive = Builder::new(Vec::new());
        let mut append = |path: &str, data: &[u8], mode| {
            let mut header = Header::new_gnu();
            header.set_size(data.len() as u64);
            header.set_mode(mode);
            header.set_cksum();
            archive.append_data(&mut header, path, data).unwrap();
        };
        append(
            "package/package.json",
            br#"{"name":"test","version":"1.0.0","scripts":{"install":"unused"}}"#,
            0o644,
        );
        append(
            &format!("package/lib/{}.js", "a".repeat(160)),
            b"long GNU filename",
            0o644,
        );
        append("package/native", &[0, 255, 1, 128], 0o755);
        let pax_name = "package/lib/pax name.js";
        archive
            .append_pax_extensions([("path", pax_name.as_bytes())])
            .unwrap();
        let mut header = Header::new_gnu();
        header.set_size(3);
        header.set_mode(0o644);
        header.set_cksum();
        archive
            .append_data(&mut header, "placeholder", &b"pax"[..])
            .unwrap();
        let mut link = Header::new_gnu();
        link.set_entry_type(tar::EntryType::Symlink);
        link.set_size(0);
        link.set_mode(0o777);
        archive
            .append_link(&mut link, "package/ignored-link", "outside")
            .unwrap();
        archive.into_inner().unwrap()
    }
    fn tree(root: &Path) -> BTreeMap<PathBuf, (Vec<u8>, bool)> {
        WalkDir::new(root)
            .into_iter()
            .map(|e| e.unwrap())
            .filter(|e| e.file_type().is_file())
            .map(|e| {
                (
                    e.path().strip_prefix(root).unwrap().to_path_buf(),
                    (
                        fs::read(e.path()).unwrap(),
                        e.metadata().unwrap().permissions().mode() & 0o111 != 0,
                    ),
                )
            })
            .collect()
    }
    #[test]
    fn memory_and_streaming_extraction_agree_on_extended_paths_and_modes() {
        use std::io::Write;
        let root = tempfile::tempdir().unwrap();
        let raw = archive_fixture();
        let mut gzip = flate2::write::GzEncoder::new(Vec::new(), flate2::Compression::default());
        gzip.write_all(&raw).unwrap();
        let gzip = gzip.finish().unwrap();
        let integrity = format!("sha512-{}", STANDARD.encode(Sha512::digest(&gzip)));
        let archive = root.path().join("archive.tgz");
        fs::write(&archive, &gzip).unwrap();
        let fast = root.path().join("fast");
        let index = unpack_to(&archive, &fast, "test@1.0.0", &integrity).unwrap();
        assert_eq!(index["requiresBuild"], true);
        assert_eq!(index["manifest"]["name"], "test");
        assert!(!fast.join("ignored-link").exists());
        assert_eq!(fs::read(fast.join("lib/pax name.js")).unwrap(), b"pax");
        let stream = root.path().join("stream");
        let mut reader = tar::Archive::new(flate2::read::GzDecoder::new(gzip.as_slice()));
        extract_entries(
            reader.entries().unwrap(),
            None,
            &stream,
            "test@1.0.0",
            &integrity,
            true,
        )
        .unwrap();
        assert_eq!(tree(&fast), tree(&stream));
        let store = root.path().join("store");
        let index = unpack(&archive, &store, "test@1.0.0", &integrity).unwrap();
        let copied = root.path().join("copied");
        materialize(&store, &index, &copied, true, false).unwrap();
        assert_eq!(tree(&fast), tree(&copied));
    }
    #[test]
    fn prepared_archives_preserve_source_indexes_and_reject_corruption() {
        let root = tempfile::tempdir().unwrap();
        let raw = archive_fixture();
        let source_integrity = format!(
            "sha512-{}",
            STANDARD.encode(Sha512::digest(b"original gzip"))
        );
        let mut reference = None;
        for (label, mut bytes) in [
            ("sized", zstd::bulk::compress(&raw, 1).unwrap()),
            (
                "stream",
                zstd::stream::encode_all(raw.as_slice(), 1).unwrap(),
            ),
        ] {
            let archive = root.path().join(format!("{label}.zst"));
            let integrity = format!("sha512-{}", STANDARD.encode(Sha512::digest(&bytes)));
            fs::write(&archive, &bytes).unwrap();
            let direct = root.path().join(format!("{label}-direct"));
            unpack_prepared(
                &archive,
                &direct,
                "test@1.0.0",
                &source_integrity,
                &integrity,
                true,
            )
            .unwrap();
            if let Some(expected) = &reference {
                assert_eq!(&tree(&direct), expected);
            } else {
                reference = Some(tree(&direct));
            }
            let cache = root.path().join(format!("{label}-store"));
            let index = unpack_prepared(
                &archive,
                &cache,
                "test@1.0.0",
                &source_integrity,
                &integrity,
                false,
            )
            .unwrap();
            assert!(
                index_path(&cache, "test@1.0.0", &source_integrity)
                    .unwrap()
                    .exists()
            );
            assert!(
                !index_path(&cache, "test@1.0.0", &integrity)
                    .unwrap()
                    .exists()
            );
            let copied = root.path().join(format!("{label}-copied"));
            materialize(&cache, &index, &copied, true, false).unwrap();
            assert_eq!(tree(&direct), tree(&copied));
            bytes[8] ^= 1;
            fs::write(&archive, bytes).unwrap();
            let invalid = root.path().join(format!("{label}-invalid"));
            let error = unpack_prepared(
                &archive,
                &invalid,
                "test@1.0.0",
                &source_integrity,
                &integrity,
                true,
            )
            .unwrap_err();
            assert!(error.to_string().contains("integrity mismatch"));
            assert!(!invalid.exists());
        }
    }
    #[test]
    fn seekable_extraction_rejects_truncated_file_data() {
        let mut archive = tar::Builder::new(Vec::new());
        for (path, data) in [
            ("package/package.json", &b"{}"[..]),
            ("package/file", &b"contents"[..]),
        ] {
            let mut header = tar::Header::new_gnu();
            header.set_size(data.len() as u64);
            header.set_mode(0o644);
            header.set_cksum();
            archive.append_data(&mut header, path, data).unwrap();
        }
        let mut bytes = archive.into_inner().unwrap();
        bytes.truncate(512 * 3 + 2);
        let root = tempfile::tempdir().unwrap();
        let input = root.path().join("archive.tar");
        fs::write(&input, &bytes).unwrap();
        let integrity = format!("sha512-{}", STANDARD.encode(Sha512::digest(&bytes)));
        let out = root.path().join("out");
        let error = unpack_to(&input, &out, "test@1.0.0", &integrity).unwrap_err();
        assert!(error.to_string().contains("truncated tar entry"));
        assert!(!out.join("file").exists());
    }

    #[test]
    fn shared_files_detach_before_bin_permissions_change() {
        use std::os::unix::fs::MetadataExt;
        let root = tempfile::tempdir().unwrap();
        let store = root.path().join("store");
        let info = save_file(&store, b"#!/usr/bin/env node\n", 0o644).unwrap();
        let index = json!({"files":{"bin.js":info}});
        let a = root.path().join("a");
        let b = root.path().join("b");
        materialize(&store, &index, &a, false, true).unwrap();
        materialize(&store, &index, &b, false, true).unwrap();
        let source = content_path(&store, &index["files"]["bin.js"]).unwrap();
        assert_eq!(
            fs::metadata(&source).unwrap().ino(),
            fs::metadata(a.join("bin.js")).unwrap().ino()
        );
        make_executable(&a.join("bin.js")).unwrap();
        assert_ne!(
            fs::metadata(&source).unwrap().ino(),
            fs::metadata(a.join("bin.js")).unwrap().ino()
        );
        assert_eq!(
            fs::metadata(&source).unwrap().permissions().mode() & 0o111,
            0
        );
        assert_eq!(
            fs::metadata(b.join("bin.js")).unwrap().permissions().mode() & 0o111,
            0
        );
        let patched = root.path().join("patched");
        materialize(&store, &index, &patched, false, false).unwrap();
        fs::write(patched.join("bin.js"), b"patched").unwrap();
        assert_eq!(fs::read(&source).unwrap(), b"#!/usr/bin/env node\n");
    }

    #[test]
    fn cached_patch_accepts_preimages_or_postimages_and_ignores_side_effects() {
        let root = tempfile::tempdir().unwrap();
        let cache = root.path().join("cache");
        let original = save_file(&cache, b"before\n", 0o644).unwrap();
        let patched = save_file(&cache, b"after\n", 0o644).unwrap();
        let text =
            "diff --git a/file b/file\n--- a/file\n+++ b/file\n@@ -1 +1 @@\n-before\n+after\n";
        fs::write(root.path().join("change.patch"), text).unwrap();
        let declaration =
            json!({"path":"change.patch","hash":hex(&Sha256::digest(text.as_bytes()))});
        let index = json!({"files":{"file":original}, "sideEffects":{"patched":{"added":{"file":patched},"deleted":[]}}});
        let before_cache = tree(&cache);
        let target = root.path().join("pristine");
        materialize(&cache, &index, &target, true, false).unwrap();
        assert_eq!(fs::read(target.join("file")).unwrap(), b"before\n");
        patch_cached(root.path(), &target, &declaration, "test@1.0.0").unwrap();
        assert_eq!(fs::read(target.join("file")).unwrap(), b"after\n");
        for (label, contents, reason) in [
            ("already", &b"after\n"[..], ""),
            (
                "unrelated",
                &b"unrelated\n"[..],
                "neither the complete preimage nor postimage matches",
            ),
        ] {
            let index = json!({"files":{"file":save_file(&cache, contents, 0o644).unwrap()}});
            let target = root.path().join(label);
            materialize(&cache, &index, &target, true, false).unwrap();
            let result = patch_cached(root.path(), &target, &declaration, "test@1.0.0");
            if reason.is_empty() {
                result.unwrap();
            } else {
                let error = result.unwrap_err().to_string();
                assert!(error.contains(reason), "{error}");
                assert!(error.contains("Rebuild without --unpacked-store"));
            }
            assert_eq!(fs::read(target.join("file")).unwrap(), contents);
        }
        for (path, contents) in before_cache {
            assert_eq!(fs::read(cache.join(path)).unwrap(), contents.0);
        }
    }

    #[test]
    fn patch_checks_complete_preimages_and_postimages_without_partial_writes() {
        let root = tempfile::tempdir().unwrap();
        let original = "A\nbefore-one\nC\nD\nE\nF\nG\nbefore-two\nI\n";
        let complete = original
            .replace("before-one", "after-one")
            .replace("before-two", "after-two");
        // Both hunks and both files must match a complete preimage or postimage.
        let text = "diff --git a/file b/file\n--- a/file\n+++ b/file\n@@ -1,3 +1,3 @@\n A\n-before-one\n+after-one\n C\n@@ -7,3 +7,3 @@\n G\n-before-two\n+after-two\n I\ndiff --git a/other b/other\n--- a/other\n+++ b/other\n@@ -1 +1 @@\n-before\n+after\n";
        fs::write(root.path().join("change.patch"), text).unwrap();
        let mut declaration =
            json!({"path":"change.patch", "hash":hex(&Sha256::digest(text.as_bytes()))});
        for (label, file, other, accepted) in [
            ("normal", original.to_owned(), "before\n", true),
            ("already", complete.clone(), "after\n", true),
            (
                "partial-hunks",
                original.replace("before-one", "after-one"),
                "before\n",
                false,
            ),
            ("partial-files", complete.clone(), "before\n", false),
            (
                "incompatible",
                "unrelated\n".to_owned(),
                "unrelated\n",
                false,
            ),
        ] {
            for cached in [false, true] {
                let target = root.path().join(format!("{label}-{cached}"));
                fs::create_dir(&target).unwrap();
                fs::write(target.join("file"), file.as_bytes()).unwrap();
                fs::write(target.join("other"), other).unwrap();
                let before = tree(&target);
                let result = if cached {
                    patch_cached(root.path(), &target, &declaration, "test@1.0.0")
                } else {
                    patch(root.path(), &target, &declaration)
                };
                if accepted {
                    result.unwrap();
                    assert_eq!(fs::read_to_string(target.join("file")).unwrap(), complete);
                    assert_eq!(fs::read(target.join("other")).unwrap(), b"after\n");
                } else {
                    assert!(
                        result
                            .unwrap_err()
                            .to_string()
                            .contains("neither the complete preimage nor postimage matches")
                    );
                    assert_eq!(tree(&target), before);
                }
            }
        }
        let malformed = text.replace("@@ -1,3 +1,3 @@", "@@ -1,4 +1,4 @@");
        fs::write(root.path().join("change.patch"), &malformed).unwrap();
        declaration["hash"] = json!(hex(&Sha256::digest(malformed.as_bytes())));
        let target = root.path().join("already-false");
        let before = tree(&target);
        assert!(patch(root.path(), &target, &declaration).is_err());
        assert_eq!(tree(&target), before);
        fs::write(root.path().join("change.patch"), text).unwrap();
        // Even a complete postimage cannot bypass the locked patch checksum.
        declaration["hash"] = json!("wrong");
        let target = root.path().join("already-false");
        let before = tree(&target);
        assert!(
            patch(root.path(), &target, &declaration)
                .unwrap_err()
                .to_string()
                .contains("checksum differs")
        );
        assert_eq!(tree(&target), before);
    }

    #[test]
    fn materialization_rejects_corrupt_cached_content() {
        let dir = tempfile::tempdir().unwrap();
        let info = save_file(dir.path(), b"original", 0o644).unwrap();
        let index = json!({"files":{"file":info}});
        fs::write(
            content_path(dir.path(), &index["files"]["file"]).unwrap(),
            b"corrupt",
        )
        .unwrap();
        let target = dir.path().join("target");
        assert!(materialize(dir.path(), &index, &target, true, false).is_err());
        assert!(!target.join("file").exists());
    }

    #[test]
    fn cached_corruption_is_rejected_without_modifying_cache() {
        let dir = tempfile::tempdir().unwrap();
        let info = save_file(dir.path(), b"original", 0o644).unwrap();
        let index = json!({"files": {"package.json": info}});
        verify_indexes(dir.path(), &BTreeMap::from([("pkg".into(), index.clone())])).unwrap();
        let path = content_path(dir.path(), &index["files"]["package.json"]).unwrap();
        fs::write(&path, b"tampered").unwrap();
        assert!(
            verify_indexes(dir.path(), &BTreeMap::from([("pkg".into(), index.clone())])).is_err()
        );
        assert_eq!(fs::read(path).unwrap(), b"tampered");
    }

    #[test]
    fn archive_integrity_is_checked_before_unpacking() {
        let dir = tempfile::tempdir().unwrap();
        let archive = dir.path().join("archive");
        fs::write(&archive, b"wrong archive").unwrap();
        let integrity = format!("sha512-{}", STANDARD.encode(Sha512::digest(b"expected")));
        let error =
            unpack(&archive, &dir.path().join("store"), "pkg@1.0.0", &integrity).unwrap_err();
        assert!(error.to_string().contains("integrity mismatch"));
        assert!(!dir.path().join("store").exists());
    }

    #[test]
    fn paths_cannot_escape_the_package() {
        for path in ["../outside", "a/../../outside", "/absolute"] {
            assert!(safe_relative(path).is_err());
        }
        assert!(safe_relative("lib/index.js").is_ok());
    }
}
