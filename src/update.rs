use anyhow::{anyhow, bail, Context, Result};
use reqwest::Client;
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
#[cfg(unix)]
use std::os::unix::fs::PermissionsExt;
use std::{
    env, fs,
    io::Write,
    path::{Path, PathBuf},
    time::{Duration, SystemTime, UNIX_EPOCH},
};

const OWNER: &str = "thegreatyamori";
const REPOSITORY: &str = "oracle-fusion-erp-catalog-mcp";
const CHECK_INTERVAL: Duration = Duration::from_secs(6 * 60 * 60);

#[derive(Debug, Deserialize)]
struct GitHubRelease {
    tag_name: String,
}

#[derive(Debug, Serialize, Deserialize)]
struct UpdateCache {
    checked_at: u64,
    latest_version: String,
}

pub async fn notify_if_available() {
    if let Ok(Some(version)) = check_for_update().await {
        eprintln!(
            "Update available: v{version}. Run `oracle-fusion-erp-catalog-mcp update`, \
             then restart your MCP agent."
        );
    }
}

pub async fn run(args: &[String]) -> Result<()> {
    let (check_only, requested_version) = parse_args(args)?;
    let current = current_version()?;
    let target = match requested_version {
        Some(version) => version,
        None => latest_version().await?,
    };

    if check_only {
        if target > current {
            println!("Update available: v{target}");
        } else {
            println!("Already up to date: v{current}");
        }
        return Ok(());
    }

    ensure_newer(&current, &target)?;
    install_version(&target).await?;
    println!(
        "Update completed successfully: v{target}\n\
         Please restart your MCP agent for the new version to take effect."
    );
    Ok(())
}

pub async fn check_for_update() -> Result<Option<String>> {
    let current = current_version()?;
    if let Some(cached) = read_fresh_cache()? {
        if let Ok(cached_version) = normalize_version(&cached.latest_version) {
            return Ok(version_is_newer(&current, &cached_version).then_some(cached.latest_version));
        }
    }

    let latest = latest_version().await?;
    write_cache(&latest)?;
    Ok(version_is_newer(&current, &latest).then_some(latest.to_string()))
}

fn parse_args(args: &[String]) -> Result<(bool, Option<semver::Version>)> {
    let mut check_only = false;
    let mut version = None;
    let mut index = 0;
    while index < args.len() {
        match args[index].as_str() {
            "--check" => check_only = true,
            "--version" => {
                index += 1;
                let value = args
                    .get(index)
                    .filter(|value| !value.trim().is_empty())
                    .ok_or_else(|| anyhow!("--version requires a value"))?;
                version = Some(normalize_version(value)?);
            }
            "-h" | "--help" => {
                bail!("Usage: oracle-fusion-erp-catalog-mcp update [--check] [--version VERSION]")
            }
            value => bail!("unsupported update argument: {value}"),
        }
        index += 1;
    }
    Ok((check_only, version))
}

fn current_version() -> Result<semver::Version> {
    semver::Version::parse(env!("CARGO_PKG_VERSION")).context("invalid package version")
}

fn normalize_version(value: &str) -> Result<semver::Version> {
    let value = value.trim().strip_prefix('v').unwrap_or(value.trim());
    semver::Version::parse(value).with_context(|| format!("invalid version: {value}"))
}

fn version_is_newer(current: &semver::Version, target: &semver::Version) -> bool {
    target > current
}

fn ensure_newer(current: &semver::Version, target: &semver::Version) -> Result<()> {
    if target <= current {
        bail!(
            "version v{} is not newer than the current version v{}",
            target,
            current
        );
    }
    Ok(())
}

async fn latest_version() -> Result<semver::Version> {
    let client = http_client()?;
    let releases = client
        .get(format!(
            "https://api.github.com/repos/{OWNER}/{REPOSITORY}/releases?per_page=100"
        ))
        .send()
        .await
        .context("could not query GitHub releases")?
        .error_for_status()
        .context("GitHub releases returned an error")?
        .json::<Vec<GitHubRelease>>()
        .await
        .context("could not parse GitHub release response")?;
    latest_semver_release(&releases)
}

fn latest_semver_release(releases: &[GitHubRelease]) -> Result<semver::Version> {
    releases
        .iter()
        .filter_map(|release| normalize_version(&release.tag_name).ok())
        .max()
        .ok_or_else(|| anyhow!("no semver binary release was found"))
}

async fn install_version(version: &semver::Version) -> Result<()> {
    let asset = platform_asset()?;
    let tag = format!("v{version}");
    let base_url = format!("https://github.com/{OWNER}/{REPOSITORY}/releases/download/{tag}");
    let client = http_client()?;
    let binary = client
        .get(format!("{base_url}/{asset}"))
        .send()
        .await
        .context("could not download update binary")?
        .error_for_status()
        .context("update binary download returned an error")?
        .bytes()
        .await
        .context("could not read update binary")?;
    let checksums = client
        .get(format!("{base_url}/SHA256SUMS"))
        .send()
        .await
        .context("could not download update checksums")?
        .error_for_status()
        .context("checksum download returned an error")?
        .text()
        .await
        .context("could not read update checksums")?;
    verify_checksum(&binary, &asset, &checksums)?;
    replace_current_binary(&binary)
}

fn http_client() -> Result<Client> {
    Client::builder()
        .user_agent(format!("{REPOSITORY}/{}", env!("CARGO_PKG_VERSION")))
        .timeout(Duration::from_secs(10))
        .build()
        .context("could not create HTTP client")
}

fn platform_asset() -> Result<String> {
    let os = match env::consts::OS {
        "macos" => "macos",
        "linux" => "linux",
        os => bail!("updates are not supported on {os}"),
    };
    let arch = match env::consts::ARCH {
        "aarch64" => "aarch64",
        "x86_64" => "x86_64",
        arch => bail!("updates are not supported on {arch}"),
    };
    Ok(format!("{REPOSITORY}-{os}-{arch}"))
}

fn verify_checksum(binary: &[u8], asset: &str, checksums: &str) -> Result<()> {
    let expected = checksums
        .lines()
        .find_map(|line| {
            let mut parts = line.split_whitespace();
            let hash = parts.next()?;
            let filename = parts.next()?;
            let filename = filename.strip_prefix('*').unwrap_or(filename);
            (filename == asset).then_some(hash)
        })
        .ok_or_else(|| anyhow!("checksum for {asset} was not published"))?;
    let actual = format!("{:x}", Sha256::digest(binary));
    if actual != expected {
        bail!("downloaded update failed SHA-256 verification");
    }
    Ok(())
}

fn replace_current_binary(binary: &[u8]) -> Result<()> {
    let current = env::current_exe().context("could not determine current binary path")?;
    let parent = current
        .parent()
        .ok_or_else(|| anyhow!("current binary has no parent directory"))?;
    let temporary = temporary_path(parent);
    let result = write_and_replace(&temporary, &current, binary);
    if result.is_err() {
        let _ = fs::remove_file(&temporary);
    }
    result
}

fn temporary_path(parent: &Path) -> PathBuf {
    parent.join(format!(".{REPOSITORY}.update-{}", std::process::id()))
}

fn write_and_replace(temporary: &Path, current: &Path, binary: &[u8]) -> Result<()> {
    let mut file = fs::File::create(temporary).context("could not create update temporary file")?;
    file.write_all(binary)
        .context("could not write update temporary file")?;
    file.sync_all()
        .context("could not flush update temporary file")?;
    #[cfg(unix)]
    {
        let mut permissions = file
            .metadata()
            .context("could not inspect update temporary file")?
            .permissions();
        permissions.set_mode(0o755);
        fs::set_permissions(temporary, permissions)
            .context("could not make updated binary executable")?;
    }
    fs::rename(temporary, current).context("could not replace current binary")?;
    Ok(())
}

fn read_fresh_cache() -> Result<Option<UpdateCache>> {
    let path = crate::paths::update_cache_path()?;
    let content = match fs::read_to_string(path) {
        Ok(content) => content,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(None),
        Err(error) => return Err(error).context("could not read update cache"),
    };
    let cache: UpdateCache =
        serde_json::from_str(&content).context("could not parse update cache")?;
    let now = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .context("system clock is before Unix epoch")?
        .as_secs();
    Ok((now.saturating_sub(cache.checked_at) < CHECK_INTERVAL.as_secs()).then_some(cache))
}

fn write_cache(latest: &semver::Version) -> Result<()> {
    let path = crate::paths::update_cache_path()?;
    crate::paths::ensure_parent_directory(&path)?;
    let checked_at = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .context("system clock is before Unix epoch")?
        .as_secs();
    let content = serde_json::to_vec(&UpdateCache {
        checked_at,
        latest_version: latest.to_string(),
    })?;
    fs::write(path, content).context("could not write update cache")?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn normalizes_release_versions() {
        assert_eq!(normalize_version("v1.2.3").unwrap().to_string(), "1.2.3");
    }

    #[test]
    fn verifies_published_checksum() {
        let binary = b"binary";
        let checksum = format!("{:x}  asset", Sha256::digest(binary));
        verify_checksum(binary, "asset", &checksum).expect("checksum");
    }

    #[test]
    fn ignores_catalog_releases_when_finding_latest_binary() {
        let releases = vec![
            GitHubRelease {
                tag_name: "catalog-26B".to_owned(),
            },
            GitHubRelease {
                tag_name: "v0.1.4".to_owned(),
            },
            GitHubRelease {
                tag_name: "v0.1.5".to_owned(),
            },
        ];
        assert_eq!(
            latest_semver_release(&releases)
                .expect("latest release")
                .to_string(),
            "0.1.5"
        );
    }
}
