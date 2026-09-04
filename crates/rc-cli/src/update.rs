//! Release update discovery for the `sc update` command.
//!
//! Installation belongs to the Subconscious CLI (`subc sc install`), which
//! already detects the platform, downloads the correct signed release asset,
//! and verifies its checksum. This module only answers whether the running
//! binary is behind the newest full GitHub release.

use anyhow::{Context, Result};
use serde::{Deserialize, Serialize};
use std::cmp::Ordering;
use std::time::Duration;
use tokio::process::Command;

const REPOSITORY: &str = "subconscious-systems/subconscious-code";
const RELEASES_API: &str =
    "https://api.github.com/repos/subconscious-systems/subconscious-code/releases/latest";
const INSTALL_COMMAND: &str = "subc sc install";
const CHECK_TIMEOUT: Duration = Duration::from_secs(3);

#[derive(Debug, Deserialize)]
struct LatestRelease {
    #[serde(alias = "tagName")]
    tag_name: String,
    #[serde(default, alias = "url")]
    html_url: String,
}

#[derive(Debug, Serialize, PartialEq, Eq)]
struct UpdateReport {
    current_version: String,
    latest_version: String,
    update_available: bool,
    install_command: &'static str,
    release_url: String,
}

pub(crate) async fn run(json: bool) -> Result<()> {
    let release = latest_release().await?;
    let current = env!("CARGO_PKG_VERSION");
    let latest = release.tag_name.trim_start_matches('v');
    let ordering = compare_versions(latest, current).with_context(|| {
        format!(
            "published release tag {:?} is not a semantic version",
            release.tag_name
        )
    })?;
    let report = UpdateReport {
        current_version: current.to_string(),
        latest_version: latest.to_string(),
        update_available: ordering == Ordering::Greater,
        install_command: INSTALL_COMMAND,
        release_url: release.html_url,
    };

    if json {
        println!("{}", serde_json::to_string(&report)?);
        return Ok(());
    }

    match ordering {
        Ordering::Greater => {
            println!(
                "Update available: sc {} -> {}",
                report.current_version, report.latest_version
            );
            println!("Run: {}", report.install_command);
            if !report.release_url.is_empty() {
                println!("Release: {}", report.release_url);
            }
        }
        Ordering::Equal => println!("sc {} is up to date.", report.current_version),
        Ordering::Less => println!(
            "sc {} is newer than the latest release ({}).",
            report.current_version, report.latest_version
        ),
    }
    Ok(())
}

/// Prefer `gh`, exactly like `subc sc install`: it carries the user's existing
/// GitHub authentication and can see a private release. Anonymous/token HTTP
/// is the portable fallback and will work without credentials once public.
async fn latest_release() -> Result<LatestRelease> {
    if let Some(release) = latest_release_with_gh().await {
        return Ok(release);
    }
    latest_release_with_http().await
}

async fn latest_release_with_gh() -> Option<LatestRelease> {
    let mut command = Command::new("gh");
    command.kill_on_drop(true).args([
        "release",
        "view",
        "--repo",
        REPOSITORY,
        "--json",
        "tagName,url",
    ]);
    let output = tokio::time::timeout(CHECK_TIMEOUT, command.output())
        .await
        .ok()?
        .ok()?;
    if !output.status.success() {
        return None;
    }
    serde_json::from_slice(&output.stdout).ok()
}

async fn latest_release_with_http() -> Result<LatestRelease> {
    let client = reqwest::Client::builder()
        .timeout(CHECK_TIMEOUT)
        .user_agent(format!("subconscious-code/{}", env!("CARGO_PKG_VERSION")))
        .build()
        .context("build update-check HTTP client")?;
    let mut request = client
        .get(RELEASES_API)
        .header("Accept", "application/vnd.github+json")
        .header("X-GitHub-Api-Version", "2022-11-28");
    if let Some(token) = github_token() {
        request = request.bearer_auth(token);
    }
    let response = request
        .send()
        .await
        .context("could not reach GitHub to check for updates")?;
    let status = response.status();
    if !status.is_success() {
        if status == reqwest::StatusCode::NOT_FOUND {
            anyhow::bail!(
                "GitHub could not access the latest release while the repository is private; run `gh auth login` or set GH_TOKEN, then retry"
            );
        }
        anyhow::bail!("GitHub update check returned HTTP {status}");
    }
    response
        .json()
        .await
        .context("GitHub returned an invalid latest-release response")
}

fn github_token() -> Option<String> {
    ["GH_TOKEN", "GITHUB_TOKEN"].into_iter().find_map(|name| {
        std::env::var(name)
            .ok()
            .filter(|value| !value.trim().is_empty())
    })
}

#[derive(Debug, PartialEq, Eq)]
struct Version {
    core: [u64; 3],
    prerelease: Vec<String>,
}

fn compare_versions(left: &str, right: &str) -> Option<Ordering> {
    let left = parse_version(left)?;
    let right = parse_version(right)?;
    Some(
        left.core
            .cmp(&right.core)
            .then_with(|| compare_prerelease(&left.prerelease, &right.prerelease)),
    )
}

fn parse_version(raw: &str) -> Option<Version> {
    let version = raw.trim().trim_start_matches('v');
    let without_build = version.split_once('+').map_or(version, |(core, _)| core);
    let (core, prerelease) = without_build
        .split_once('-')
        .map_or((without_build, ""), |(core, prerelease)| (core, prerelease));
    let mut parts = core.split('.');
    let core = [
        parts.next()?.parse().ok()?,
        parts.next()?.parse().ok()?,
        parts.next()?.parse().ok()?,
    ];
    if parts.next().is_some() {
        return None;
    }
    let prerelease = if prerelease.is_empty() {
        Vec::new()
    } else {
        prerelease.split('.').map(str::to_string).collect()
    };
    Some(Version { core, prerelease })
}

fn compare_prerelease(left: &[String], right: &[String]) -> Ordering {
    match (left.is_empty(), right.is_empty()) {
        (true, true) => return Ordering::Equal,
        (true, false) => return Ordering::Greater,
        (false, true) => return Ordering::Less,
        (false, false) => {}
    }
    for index in 0..left.len().max(right.len()) {
        let Some(left) = left.get(index) else {
            return Ordering::Less;
        };
        let Some(right) = right.get(index) else {
            return Ordering::Greater;
        };
        let ordering = match (left.parse::<u64>(), right.parse::<u64>()) {
            (Ok(left), Ok(right)) => left.cmp(&right),
            (Ok(_), Err(_)) => Ordering::Less,
            (Err(_), Ok(_)) => Ordering::Greater,
            (Err(_), Err(_)) => left.cmp(right),
        };
        if ordering != Ordering::Equal {
            return ordering;
        }
    }
    Ordering::Equal
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn versions_compare_by_semver_precedence() {
        assert_eq!(compare_versions("0.1.3", "0.1.2"), Some(Ordering::Greater));
        assert_eq!(compare_versions("v1.0.0", "1.0.0"), Some(Ordering::Equal));
        assert_eq!(
            compare_versions("1.0.0-rc.2", "1.0.0-rc.10"),
            Some(Ordering::Less)
        );
        assert_eq!(
            compare_versions("1.0.0", "1.0.0-rc.10"),
            Some(Ordering::Greater)
        );
        assert_eq!(
            compare_versions("1.0.0+build.9", "1.0.0+build.1"),
            Some(Ordering::Equal)
        );
        assert_eq!(compare_versions("latest", "1.0.0"), None);
    }

    #[test]
    fn github_payloads_from_cli_and_rest_both_parse() {
        let cli: LatestRelease = serde_json::from_str(
            r#"{"tagName":"v0.1.3","url":"https://github.com/example/release"}"#,
        )
        .unwrap();
        let rest: LatestRelease = serde_json::from_str(
            r#"{"tag_name":"v0.1.3","html_url":"https://github.com/example/release"}"#,
        )
        .unwrap();
        assert_eq!(cli.tag_name, rest.tag_name);
        assert_eq!(cli.html_url, rest.html_url);
    }
}
