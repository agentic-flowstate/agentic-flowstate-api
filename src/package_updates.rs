use anyhow::{bail, Context, Result};
use chrono::Utc;
use serde::{Deserialize, Serialize};
use serde_json::Value;
use std::{collections::BTreeMap, env};
use tokio::process::Command;

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct PackageUpdateScanReport {
    pub timestamp: String,
    pub host: String,
    pub managers: Vec<PackageManagerScan>,
    pub updates: Vec<PackageUpdate>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct PackageManagerScan {
    pub manager: String,
    pub command: String,
    pub status: String,
    pub update_count: usize,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub error: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct PackageUpdate {
    pub manager: String,
    pub package: String,
    pub installed_version: String,
    pub available_version: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub note: Option<String>,
}

struct CommandOutput {
    stdout: String,
    stderr: String,
    status_code: Option<i32>,
}

pub async fn scan_available_updates() -> Result<PackageUpdateScanReport> {
    let mut managers = Vec::new();
    let mut updates = Vec::new();

    let brew_updates = scan_brew().await?;
    managers.push(manager_scan(
        "brew",
        "brew outdated --json=v2",
        brew_updates.len(),
        None,
    ));
    updates.extend(brew_updates);

    let npm_updates = scan_npm_global().await?;
    managers.push(manager_scan(
        "npm-global",
        "npm outdated -g --json",
        npm_updates.len(),
        None,
    ));
    updates.extend(npm_updates);

    let rustup_updates = scan_rustup().await?;
    managers.push(manager_scan(
        "rustup",
        "rustup check",
        rustup_updates.len(),
        None,
    ));
    updates.extend(rustup_updates);

    Ok(PackageUpdateScanReport {
        timestamp: Utc::now().to_rfc3339(),
        host: host_name().await?,
        managers,
        updates,
    })
}

pub async fn apply_updates(report: &PackageUpdateScanReport) -> Result<String> {
    if report.updates.is_empty() {
        bail!("Package update report contains no updates to apply");
    }

    let mut by_manager: BTreeMap<&str, Vec<&PackageUpdate>> = BTreeMap::new();
    for update in &report.updates {
        by_manager
            .entry(update.manager.as_str())
            .or_default()
            .push(update);
    }

    let mut output = Vec::new();

    if let Some(updates) = by_manager.get("brew-formula") {
        let mut args = vec!["upgrade".to_string()];
        args.extend(
            updates
                .iter()
                .map(|update| update.package.as_str())
                .map(str::to_string),
        );
        output.push(run_apply_command("brew", args).await?);
    }

    if let Some(updates) = by_manager.get("brew-cask") {
        let mut args = vec!["upgrade".to_string(), "--cask".to_string()];
        args.extend(
            updates
                .iter()
                .map(|update| update.package.as_str())
                .map(str::to_string),
        );
        output.push(run_apply_command("brew", args).await?);
    }

    if let Some(updates) = by_manager.get("npm-global") {
        let packages = updates
            .iter()
            .map(|update| format!("{}@latest", update.package))
            .collect::<Vec<_>>();
        let args = ["install", "-g"]
            .into_iter()
            .map(str::to_string)
            .chain(packages)
            .collect();
        output.push(run_apply_command("npm", args).await?);
    }

    if by_manager.contains_key("rustup") {
        output.push(run_apply_command("rustup", vec!["update".to_string()]).await?);
    }

    Ok(output.join("\n\n"))
}

pub fn parse_report(raw: &str) -> Result<PackageUpdateScanReport> {
    serde_json::from_str(raw).context("Failed to parse package update scanner report")
}

fn manager_scan(
    manager: &str,
    command: &str,
    update_count: usize,
    error: Option<String>,
) -> PackageManagerScan {
    PackageManagerScan {
        manager: manager.to_string(),
        command: command.to_string(),
        status: if error.is_some() { "failed" } else { "ok" }.to_string(),
        update_count,
        error,
    }
}

async fn scan_brew() -> Result<Vec<PackageUpdate>> {
    let output = run_command("brew", &["outdated", "--json=v2"], &[0]).await?;
    let value: Value = serde_json::from_str(&output.stdout).with_context(|| {
        format!(
            "Failed to parse brew outdated JSON. stderr={}",
            output.stderr.trim()
        )
    })?;

    let mut updates = Vec::new();
    if let Some(formulae) = value.get("formulae").and_then(Value::as_array) {
        for formula in formulae {
            if let Some(update) = parse_brew_update(formula, "brew-formula") {
                updates.push(update);
            }
        }
    }
    if let Some(casks) = value.get("casks").and_then(Value::as_array) {
        for cask in casks {
            if let Some(update) = parse_brew_update(cask, "brew-cask") {
                updates.push(update);
            }
        }
    }

    Ok(updates)
}

fn parse_brew_update(value: &Value, manager: &str) -> Option<PackageUpdate> {
    let package = value.get("name")?.as_str()?.to_string();
    let available_version = value
        .get("current_version")
        .and_then(Value::as_str)
        .unwrap_or("unknown")
        .to_string();
    let installed_version = value
        .get("installed_versions")
        .and_then(Value::as_array)
        .map(|versions| {
            versions
                .iter()
                .filter_map(Value::as_str)
                .collect::<Vec<_>>()
                .join(", ")
        })
        .filter(|versions| !versions.is_empty())
        .unwrap_or_else(|| "unknown".to_string());

    Some(PackageUpdate {
        manager: manager.to_string(),
        package,
        installed_version,
        available_version,
        note: None,
    })
}

async fn scan_npm_global() -> Result<Vec<PackageUpdate>> {
    let output = run_command("npm", &["outdated", "-g", "--json"], &[0, 1]).await?;
    let trimmed = output.stdout.trim();
    if trimmed.is_empty() {
        return Ok(Vec::new());
    }

    let value: Value = serde_json::from_str(trimmed).with_context(|| {
        format!(
            "Failed to parse npm outdated JSON. code={:?} stderr={}",
            output.status_code,
            output.stderr.trim()
        )
    })?;

    let mut updates = Vec::new();
    if let Some(packages) = value.as_object() {
        for (package, info) in packages {
            let installed_version = info
                .get("current")
                .and_then(Value::as_str)
                .unwrap_or("unknown")
                .to_string();
            let available_version = info
                .get("latest")
                .or_else(|| info.get("wanted"))
                .and_then(Value::as_str)
                .unwrap_or("unknown")
                .to_string();
            updates.push(PackageUpdate {
                manager: "npm-global".to_string(),
                package: package.clone(),
                installed_version,
                available_version,
                note: None,
            });
        }
    }

    Ok(updates)
}

async fn scan_rustup() -> Result<Vec<PackageUpdate>> {
    let output = run_command("rustup", &["check"], &[0, 1]).await?;
    let mut updates = Vec::new();

    for line in output.stdout.lines() {
        let Some((toolchain, remainder)) = line.split_once(" - update available: ") else {
            continue;
        };
        let Some((installed_version, available_version)) = remainder.split_once(" -> ") else {
            continue;
        };
        updates.push(PackageUpdate {
            manager: "rustup".to_string(),
            package: toolchain.trim().to_string(),
            installed_version: installed_version.trim().to_string(),
            available_version: available_version.trim().to_string(),
            note: None,
        });
    }

    Ok(updates)
}

async fn host_name() -> Result<String> {
    let output = run_command("hostname", &[], &[0]).await?;
    let host = output.stdout.trim();
    if host.is_empty() {
        bail!("hostname returned an empty value");
    }
    Ok(host.to_string())
}

async fn run_apply_command(command: &str, args: Vec<String>) -> Result<String> {
    let borrowed_args = args.iter().map(String::as_str).collect::<Vec<_>>();
    let output = run_command(command, &borrowed_args, &[0]).await?;
    Ok(format!(
        "$ {} {}\n{}{}",
        command,
        borrowed_args.join(" "),
        output.stdout,
        output.stderr
    ))
}

async fn run_command(command: &str, args: &[&str], allowed_codes: &[i32]) -> Result<CommandOutput> {
    let tool_path = package_update_tool_path()?;
    let mut env_args = Vec::with_capacity(args.len() + 2);
    env_args.push(format!("PATH={tool_path}"));
    env_args.push(command.to_string());
    env_args.extend(args.iter().map(|arg| (*arg).to_string()));

    let output = Command::new("/usr/bin/env")
        .args(&env_args)
        .output()
        .await
        .with_context(|| format!("Failed to spawn package update command: {command}"))?;
    let status_code = output.status.code();
    let stdout = String::from_utf8_lossy(&output.stdout).to_string();
    let stderr = String::from_utf8_lossy(&output.stderr).to_string();

    if !status_code
        .map(|code| allowed_codes.contains(&code))
        .unwrap_or(false)
    {
        bail!(
            "Package update command failed: {} {} code={:?} stderr={}",
            command,
            args.join(" "),
            status_code,
            stderr.trim()
        );
    }

    Ok(CommandOutput {
        stdout,
        stderr,
        status_code,
    })
}

fn package_update_tool_path() -> Result<String> {
    let home_dir =
        dirs::home_dir().context("Failed to resolve home directory for package update PATH")?;
    let home = home_dir
        .to_str()
        .context("Home directory path is not valid UTF-8")?;

    let mut entries = vec![
        format!("{home}/.cargo/bin"),
        format!("{home}/.local/bin"),
        "/opt/homebrew/opt/node@20/bin".to_string(),
        "/opt/homebrew/bin".to_string(),
        "/usr/local/bin".to_string(),
        "/usr/bin".to_string(),
        "/bin".to_string(),
    ];

    if let Ok(existing_path) = env::var("PATH") {
        for entry in existing_path.split(':').filter(|entry| !entry.is_empty()) {
            let candidate = entry.to_string();
            if !entries.iter().any(|existing| existing == &candidate) {
                entries.push(candidate);
            }
        }
    }

    Ok(entries.join(":"))
}
