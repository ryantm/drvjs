mod config;
mod fetch;
mod native;
mod native_store;
mod plan;
mod pnpm;
mod verify;
mod workspace;

use anyhow::{Context, Result, ensure};
use clap::{Parser, Subcommand};
use std::{
    fs,
    path::{Path, PathBuf},
};

#[derive(Parser)]
#[command(
    version,
    about = "Build pnpm node_modules with package-granular Nix fetches"
)]
struct Cli {
    #[command(subcommand)]
    command: Commands,
}

#[derive(Subcommand)]
enum Commands {
    Export {
        #[arg(short = 'C', long, default_value = ".")]
        directory: PathBuf,
        #[arg(short, long)]
        out: PathBuf,
        /// Exact workspace name or relative path; dependencies are included. Repeat to select several.
        #[arg(long = "filter")]
        filters: Vec<String>,
        /// Host-only credentials for URL dependencies that have no integrity hash.
        #[arg(long)]
        npmrc: Option<PathBuf>,
        /// Retain pnpm's package store for later offline `drvjs verify`.
        #[arg(long)]
        keep_verification_store: bool,
        /// Cache zstd-normalized archives in Nix (experimental).
        #[arg(long)]
        zstd_archives: bool,
    },
    /// Import authenticated tarballs into Nix without storing credentials.
    Fetch {
        directory: PathBuf,
        #[arg(long)]
        npmrc: Option<PathBuf>,
        /// Fetch public tarballs too (default: custom/private registries only).
        #[arg(long)]
        all: bool,
    },
    /// Compare node_modules with pnpm's fresh offline reconstruction.
    Verify {
        directory: PathBuf,
        /// Use a compatible external pnpm v10 store; the original store is never modified.
        #[arg(long)]
        store: Option<PathBuf>,
    },
    /// Internal Nix builder entry point.
    #[command(hide = true)]
    Assemble {
        source: PathBuf,
        out: PathBuf,
        tarballs: PathBuf,
        #[arg(long)]
        keep_verification_store: bool,
        /// Experimental: seed installation from a compatible retained pnpm store.
        #[arg(long)]
        unpacked_store: Option<PathBuf>,
    },
}

fn export(
    project: &Path,
    out: &Path,
    filters: &[String],
    npmrc: Option<&Path>,
    keep_verification_store: bool,
    zstd_archives: bool,
) -> Result<()> {
    let project = project.canonicalize()?;
    let lock = fs::read_to_string(project.join("pnpm-lock.yaml"))?;
    let mut selection = workspace::select(&project, &lock, filters)?;
    fetch::complete_integrities(&mut selection.lock, npmrc)?;
    let settings = config::npmrc(&project)?;
    let plan = plan::parse_with_registry(&selection.lock, &settings)?;
    ensure!(
        !out.exists(),
        "export destination already exists: {}",
        out.display()
    );
    // Stage atomically so an unsuccessful export does not leave a usable-looking graph.
    let parent = out
        .parent()
        .filter(|p| !p.as_os_str().is_empty())
        .unwrap_or(Path::new("."));
    fs::create_dir_all(parent)?;
    let stage = tempfile::tempdir_in(parent)?;
    workspace::write_project(&project, &stage.path().join("project"), &selection)?;
    fs::write(
        stage.path().join("plan.json"),
        serde_json::to_vec_pretty(&plan)?,
    )?;
    fs::write(
        stage.path().join("options.json"),
        serde_json::to_vec_pretty(
            &serde_json::json!({"keepVerificationStore": keep_verification_store, "zstdArchives": zstd_archives}),
        )?,
    )?;
    fs::write(
        stage.path().join("default.nix"),
        include_str!("../nix/build.nix"),
    )?;
    fs::create_dir_all(stage.path().join("drvjs-src/src"))?;
    fs::create_dir_all(stage.path().join("drvjs-src/nix"))?;
    for (name, content) in [
        ("Cargo.toml", include_str!("../Cargo.toml")),
        ("Cargo.lock", include_str!("../Cargo.lock")),
        ("src/main.rs", include_str!("main.rs")),
        ("src/config.rs", include_str!("config.rs")),
        ("src/fetch.rs", include_str!("fetch.rs")),
        ("src/plan.rs", include_str!("plan.rs")),
        ("src/pnpm.rs", include_str!("pnpm.rs")),
        ("src/native.rs", include_str!("native.rs")),
        ("src/native_store.rs", include_str!("native_store.rs")),
        ("src/native_shim.txt", include_str!("native_shim.txt")),
        ("src/verify.rs", include_str!("verify.rs")),
        ("src/workspace.rs", include_str!("workspace.rs")),
        ("nix/build.nix", include_str!("../nix/build.nix")),
        ("nix/package.nix", include_str!("../nix/package.nix")),
    ] {
        fs::write(stage.path().join("drvjs-src").join(name), content)?;
    }
    fs::rename(stage.path(), out)?;
    println!(
        "Exported {} workspace importers and {} package fetches to {}",
        selection.importers.len(),
        plan.packages.len(),
        out.display()
    );
    Ok(())
}

fn main() {
    let result = match Cli::parse().command {
        Commands::Export {
            directory,
            out,
            filters,
            npmrc,
            keep_verification_store,
            zstd_archives,
        } => export(
            &directory,
            &out,
            &filters,
            npmrc.as_deref(),
            keep_verification_store,
            zstd_archives,
        ),
        Commands::Fetch {
            directory,
            npmrc,
            all,
        } => fetch::fetch(&directory, npmrc.as_deref(), all),
        Commands::Verify { directory, store } => verify::verify(&directory, store.as_deref()),
        Commands::Assemble {
            source,
            out,
            tarballs,
            keep_verification_store,
            unpacked_store,
        } => native::assemble(
            &source,
            &out,
            &tarballs,
            keep_verification_store,
            unpacked_store.as_deref(),
        ),
    };
    if let Err(error) = result.context("drvjs failed") {
        eprintln!("{error:#}");
        std::process::exit(1);
    }
}
