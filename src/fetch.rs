//! Authenticated downloads happen on the host; only verified bytes enter Nix.
use crate::{config, plan::Plan};
use anyhow::{Context, Result, bail, ensure};
use base64::{Engine, engine::general_purpose::STANDARD};
use sha2::{Digest, Sha256, Sha384, Sha512};
use std::{
    collections::{BTreeMap, BTreeSet},
    fs,
    io::{Read, Write},
    path::Path,
    process::{Command, Stdio},
};

fn credentials(npmrc: Option<&Path>) -> Result<BTreeMap<String, String>> {
    let path = npmrc
        .map(Path::to_path_buf)
        .or_else(|| std::env::var_os("HOME").map(|home| Path::new(&home).join(".npmrc")));
    match path {
        Some(path) if path.exists() => config::parse_npmrc(&fs::read_to_string(path)?),
        Some(_) if npmrc.is_some() => bail!("credential .npmrc does not exist"),
        _ => Ok(BTreeMap::new()),
    }
}

fn expand(value: &str) -> Result<String> {
    if let Some(name) = value.strip_prefix("${").and_then(|s| s.strip_suffix('}')) {
        return std::env::var(name).context("credential environment variable is unset");
    }
    Ok(value.to_owned())
}

pub fn authorization(
    address: &str,
    credentials: &BTreeMap<String, String>,
) -> Result<Option<String>> {
    let url = config::download_url(address)?;
    let target = format!("//{}{}", url.authority(), url.path());
    let mut candidates = Vec::new();
    for (key, value) in credentials {
        for (suffix, scheme) in [(":_authToken", "Bearer"), (":_auth", "Basic")] {
            if let Some(scope) = key.strip_suffix(suffix) {
                // npm auth scopes must end with /: matching a hostname or path
                // prefix alone could disclose credentials to a neighboring path.
                if scope.starts_with("//") && scope.ends_with('/') && target.starts_with(scope) {
                    candidates.push((scope.len(), format!("{scheme} {}", expand(value)?)));
                }
            }
        }
    }
    candidates.sort_by_key(|(len, _)| *len);
    Ok(candidates.pop().map(|(_, value)| value))
}

pub fn download(address: &str, destination: &Path, npmrc: Option<&Path>) -> Result<()> {
    config::url(address)?;
    let credentials = credentials(npmrc)?;
    let mut address = address.to_owned();
    for _ in 0..=5 {
        config::download_url(&address)?;
        let auth = authorization(&address, &credentials)?;
        // Re-evaluate the credential scope on every redirect, including redirects
        // to a different path on the same host. Secrets only go through stdin.
        let mut config_text = String::new();
        if let Some(auth) = auth {
            ensure!(
                !auth.contains(['\n', '\r', '\0']),
                "invalid registry credential"
            );
            let escaped = auth.replace('\\', "\\\\").replace('"', "\\\"");
            config_text = format!("header = \"Authorization: {escaped}\"\n");
        }
        let mut child = Command::new("curl")
            .args([
                "--disable",
                "--fail",
                "--silent",
                "--connect-timeout",
                "30",
                "--max-time",
                "300",
                "--proto",
                "=https,http",
                "--write-out",
                "%{redirect_url}",
                "--config",
                "-",
                "--output",
            ])
            .arg(destination)
            .arg(&address)
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::null())
            .spawn()
            .context("running curl")?;
        child
            .stdin
            .take()
            .unwrap()
            .write_all(config_text.as_bytes())?;
        let output = child.wait_with_output()?;
        ensure!(
            output.status.success(),
            "tarball download failed (curl status {}); check registry access and supply --npmrc for authentication",
            output.status
        );
        let redirect = String::from_utf8(output.stdout)?;
        if redirect.is_empty() {
            return Ok(());
        }
        address = redirect;
    }
    bail!("too many tarball redirects")
}

pub fn hash_file(path: &Path, algorithm: &str) -> Result<String> {
    let mut input = fs::File::open(path)?;
    let mut buffer = [0u8; 65536];
    macro_rules! hash {
        ($type:ty) => {{
            let mut hash = <$type>::new();
            loop {
                let n = input.read(&mut buffer)?;
                if n == 0 {
                    break;
                }
                hash.update(&buffer[..n]);
            }
            STANDARD.encode(hash.finalize())
        }};
    }
    let digest = match algorithm {
        "sha256" => hash!(Sha256),
        "sha384" => hash!(Sha384),
        "sha512" => hash!(Sha512),
        _ => bail!("unsupported integrity algorithm"),
    };
    Ok(format!("{algorithm}-{digest}"))
}

pub fn complete_integrities(lock: &mut String, npmrc: Option<&Path>) -> Result<()> {
    let mut data: serde_json::Value = serde_yaml_ng::from_str(lock)?;
    let mut changed = false;
    if let Some(packages) = data["packages"].as_object_mut() {
        for package in packages.values_mut() {
            if package["resolution"].get("integrity").is_none() {
                let address = package["resolution"]["tarball"]
                    .as_str()
                    .context("resolution needs an integrity hash or HTTPS tarball")?;
                let tmp = tempfile::tempdir()?;
                let file = tmp.path().join("package.tgz");
                download(address, &file, npmrc)?;
                package["resolution"]["integrity"] = hash_file(&file, "sha512")?.into();
                changed = true;
            }
        }
    }
    if changed {
        *lock = serde_yaml_ng::to_string(&data)?;
    }
    Ok(())
}

pub fn fetch(export: &Path, npmrc: Option<&Path>, all: bool) -> Result<()> {
    let plan: Plan = serde_json::from_slice(&fs::read(export.join("plan.json"))?)?;
    let packages: Vec<_> = plan
        .packages
        .into_iter()
        .filter(|p| all || p.private)
        .collect();
    if packages.is_empty() {
        println!("Imported 0 verified tarballs into Nix");
        return Ok(());
    }
    let mut input = tempfile::NamedTempFile::new()?;
    serde_json::to_writer(&mut input, &packages)?;
    let expected = Command::new("nix-instantiate")
        .args([
            "--eval", "--strict", "--json", "--expr",
            "{ plan }: map (p: (import <nix/fetchurl.nix> { inherit (p) url hash name; }).outPath) (builtins.fromJSON (builtins.readFile plan))",
            "--argstr", "plan",
        ])
        .arg(input.path())
        .output()?;
    ensure!(
        expected.status.success(),
        "cannot compute Nix fixed-output store paths"
    );
    let paths: Vec<String> = serde_json::from_slice(&expected.stdout)?;
    ensure!(
        paths.len() == packages.len(),
        "incomplete Nix fixed-output store paths"
    );
    let mut query = tempfile::NamedTempFile::new()?;
    for path in &paths {
        writeln!(query, "{path}")?;
    }
    // File-backed stdin avoids argument limits and pipe deadlocks on large plans.
    let validity = Command::new("nix-store")
        .args(["--check-validity", "--print-invalid", "--stdin"])
        .stdin(fs::File::open(query.path())?)
        .output()?;
    ensure!(validity.status.success(), "cannot query Nix store validity");
    let invalid: BTreeSet<_> = String::from_utf8(validity.stdout)?
        .lines()
        .map(str::to_owned)
        .collect();
    let mut seen = BTreeSet::new();
    let missing: Vec<_> = packages
        .iter()
        .zip(&paths)
        .filter(|(_, path)| invalid.contains(path.as_str()) && seen.insert(path.as_str()))
        .collect();
    crate::native_store::parallel_each(&missing, |(package, path)| {
        let algorithm = package.hash.split_once('-').context("invalid SRI")?.0;
        let tmp = tempfile::tempdir()?;
        let file = tmp.path().join(&package.name);
        download(&package.url, &file, npmrc).with_context(|| format!("fetching {}", package.id))?;
        ensure!(
            hash_file(&file, algorithm)? == package.hash,
            "integrity mismatch for {}",
            package.id
        );
        let imported = Command::new("nix-store")
            .args(["--add-fixed", algorithm])
            .arg(&file)
            .output()?;
        ensure!(
            imported.status.success(),
            "Nix import failed for {}",
            package.id
        );
        ensure!(
            String::from_utf8(imported.stdout)?.trim() == path.as_str(),
            "unexpected imported store path"
        );
        Ok(())
    })?;
    println!("Imported {} verified tarballs into Nix", missing.len());
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn auth_is_scoped() {
        let auth = BTreeMap::from([("//example.org/npm/:_authToken".into(), "secret".into())]);
        assert_eq!(
            authorization("https://example.org/npm/pkg", &auth).unwrap(),
            Some("Bearer secret".into())
        );
        assert_eq!(
            authorization("https://example.org/other/pkg", &auth).unwrap(),
            None
        );
        assert_eq!(
            authorization("https://example.org.evil/npm/pkg", &auth).unwrap(),
            None
        );
    }
}
