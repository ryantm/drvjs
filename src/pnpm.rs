//! Independent verification only. Production assembly never invokes pnpm.
use anyhow::{Context, Result, ensure};
use serde_json::Value;
use std::{fs, path::Path, process::Command, time::Instant};

fn run(cwd: &Path, args: &[&str]) -> Result<()> {
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
    fs::create_dir_all(root.join("node_modules"))?;
    let store = root.join(".pnpm-store");
    run(
        root,
        &[
            "install",
            "--offline",
            "--frozen-lockfile",
            "--ignore-scripts",
            "--store-dir",
            store.to_str().context("non-UTF8 path")?,
            "--package-import-method=copy",
            "--config.manage-package-manager-versions=false",
            "--config.package-manager-strict=false",
        ],
    )
}

/// pnpm store status compares against unpatched tarball indexes. Check
/// unpatched snapshots in a temporary view; verify() independently reconstructs
/// and compares every installed package, including patched ones.
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
    run(
        tmp.path(),
        &[
            "store",
            "status",
            "--store-dir",
            root.join(".pnpm-store").to_str().context("non-UTF8 path")?,
        ],
    )
}
