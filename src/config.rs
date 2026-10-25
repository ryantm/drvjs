//! Configuration copied into an export must never contain authentication values.
use anyhow::{Context, Result, ensure};
use serde_json::Value;
use std::{collections::BTreeMap, fs, path::Path};
use url::Url;

pub fn url(value: &str) -> Result<Url> {
    let parsed = download_url(value)?;
    ensure!(
        parsed.query().is_none(),
        "registry/tarball URLs must not contain query strings"
    );
    Ok(parsed)
}

// Redirects may carry ephemeral signed queries. They stay host-side and are
// never written to plans, derivations, logs, or credential files.
pub fn download_url(value: &str) -> Result<Url> {
    let parsed = Url::parse(value).context("invalid registry/tarball URL")?;
    ensure!(
        parsed.username().is_empty() && parsed.password().is_none() && parsed.fragment().is_none(),
        "registry/tarball URLs must not contain credentials or fragments"
    );
    ensure!(
        parsed.scheme() == "https"
            || (parsed.scheme() == "http"
                && matches!(parsed.host_str(), Some("127.0.0.1" | "localhost" | "[::1]"))),
        "use HTTPS for registries (HTTP is allowed only on loopback for tests)"
    );
    Ok(parsed)
}

pub fn parse_npmrc(text: &str) -> Result<BTreeMap<String, String>> {
    let mut result = BTreeMap::new();
    for line in text.lines() {
        let line = line.trim();
        if line.is_empty() || line.starts_with(['#', ';']) {
            continue;
        }
        let (key, value) = line
            .split_once('=')
            .context("expected key=value in .npmrc")?;
        ensure!(
            !key.trim().ends_with("[]") || !result.contains_key(key.trim()),
            "duplicate array .npmrc setting is unsupported: {}",
            key.trim()
        );
        result.insert(
            key.trim().to_owned(),
            value.trim().trim_matches('"').trim_matches('\'').to_owned(),
        );
    }
    Ok(result)
}

pub fn npmrc(project: &Path) -> Result<BTreeMap<String, String>> {
    let path = project.join(".npmrc");
    if !path.exists() {
        return Ok(BTreeMap::new());
    }
    let input = parse_npmrc(&fs::read_to_string(path)?)?;
    let mut sanitized = BTreeMap::new();
    for (key, value) in input {
        // Scope credentials are accepted as input but only the host fetch command
        // reads them. They are never included in plan.json or project snapshots.
        if key.starts_with("//")
            || [
                "_authToken",
                "_auth",
                "_password",
                "username",
                "email",
                "always-auth",
            ]
            .contains(&key.as_str())
        {
            continue;
        }
        ensure!(
            !value.contains("${"),
            "environment expansion in non-auth .npmrc settings is unsupported: {key}"
        );
        if key == "registry" || key.ends_with(":registry") {
            url(&value)?;
        } else {
            ensure!(
                [
                    "strict-peer-dependencies",
                    "engine-strict",
                    "link-workspace-packages",
                    "ignore-workspace-root-check",
                    "shared-workspace-lockfile",
                    "auto-install-peers",
                    "dedupe-peer-dependents",
                    "public-hoist-pattern[]",
                    "hoist-pattern[]",
                    "shamefully-hoist"
                ]
                .contains(&key.as_str()),
                "unsupported .npmrc setting: {key}"
            );
        }
        sanitized.insert(key, value);
    }
    Ok(sanitized)
}

pub fn registry(name: &str, settings: &BTreeMap<String, String>) -> Result<String> {
    let scoped = name
        .starts_with('@')
        .then(|| format!("{}:registry", name.split('/').next().unwrap()));
    let value = scoped
        .as_ref()
        .and_then(|scope| settings.get(scope))
        .or_else(|| settings.get("registry"))
        .map(String::as_str)
        .unwrap_or("https://registry.npmjs.org/");
    url(value)?;
    Ok(format!("{}/", value.trim_end_matches('/')))
}

pub fn validate_settings(settings: &Value) -> Result<()> {
    let settings = settings
        .as_object()
        .context("pnpm settings must be an object")?;
    for key in settings.keys() {
        ensure!(
            [
                "packages",
                "catalog",
                "catalogs",
                "onlyBuiltDependencies",
                "ignoredBuiltDependencies",
                "neverBuiltDependencies",
                "allowedDeprecatedVersions",
                "peerDependencyRules",
                "overrides",
                "packageExtensions",
                "patchedDependencies",
                "supportedArchitectures",
                "publicHoistPatterns",
                "publicHoistPattern",
                "hoistPattern",
                "minimumReleaseAge",
                "minimumReleaseAgeExclude",
                "allowUnusedPatches",
                "ignorePatchFailures"
            ]
            .contains(&key.as_str()),
            "unsupported pnpm setting: {key}"
        );
    }
    Ok(())
}

pub fn safe_file(project: &Path, relative: &str) -> Result<std::path::PathBuf> {
    let path = Path::new(relative);
    ensure!(
        !path.is_absolute()
            && !path
                .components()
                .any(|p| matches!(p, std::path::Component::ParentDir)),
        "support file must be inside project: {relative}"
    );
    let resolved = project.join(path).canonicalize()?;
    ensure!(
        resolved.starts_with(project.canonicalize()?),
        "support file escapes project root: {relative}"
    );
    Ok(resolved)
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn accepts_deprecation_warning_and_never_build_settings() {
        for settings in [
            serde_json::json!({"allowedDeprecatedVersions":{"example":"1.0.0"}}),
            serde_json::json!({"neverBuiltDependencies":["example"]}),
            serde_json::json!({"allowedDeprecatedVersions":{"example":"1.0.0"},"neverBuiltDependencies":["example"]}),
        ] {
            validate_settings(&settings).unwrap();
        }
        assert!(validate_settings(&serde_json::json!({"unknownSetting":true})).is_err());
    }
    #[test]
    fn scopes_and_credentials() {
        let root = tempfile::tempdir().unwrap();
        fs::write(root.path().join(".npmrc"), "@private:registry=https://example.org/npm/\n//example.org/npm/:_authToken=TOPSECRET\nengine-strict=true\n").unwrap();
        let rc = npmrc(root.path()).unwrap();
        assert!(!format!("{rc:?}").contains("TOPSECRET"));
        assert_eq!(
            registry("@private/pkg", &rc).unwrap(),
            "https://example.org/npm/"
        );
        assert_eq!(
            registry("public", &rc).unwrap(),
            "https://registry.npmjs.org/"
        );
    }
    #[test]
    fn repeated_scalar_settings_use_last_value() {
        let rc = parse_npmrc("registry=https://first.example/\nregistry=https://last.example/\n")
            .unwrap();
        assert_eq!(rc["registry"], "https://last.example/");
        assert!(parse_npmrc("hoist-pattern[]=a\nhoist-pattern[]=b\n").is_err());
    }
    #[test]
    fn rejects_embedded_credentials() {
        assert!(url("https://user:password@example.org/a").is_err());
        assert!(url("https://example.org/a?token=secret").is_err());
        assert!(url("file:///etc/passwd").is_err());
    }
}
