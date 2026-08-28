use std::{
    env,
    fs::{self, File},
    io::{self, Read},
    path::{Path, PathBuf},
    process::{Command, Stdio},
};

#[cfg(windows)]
use std::{thread, time::Duration};

use anyhow::{Context, Result, bail};
use serde_json::Value;
use sha2::{Digest, Sha256};

const DEFAULT_REPOSITORY: &str = "eidos-space/graft";
const CURRENT_VERSION: &str = env!("CARGO_PKG_VERSION");

#[derive(Debug, Clone, Copy)]
struct UpgradeTarget {
    triple: &'static str,
    archive_extension: &'static str,
    executable: &'static str,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
struct Version {
    major: u64,
    minor: u64,
    patch: u64,
}

#[derive(Debug, Clone, PartialEq, Eq)]
struct Release {
    tag: String,
    version: Version,
}

pub(crate) fn run() -> Result<()> {
    let target = target_for_current_platform()?;
    let destination = current_executable_path()?;
    let current_version = parse_version(CURRENT_VERSION)
        .with_context(|| format!("invalid current graft version `{CURRENT_VERSION}`"))?;
    let repository = configured_repository()?;
    let temporary_directory = tempfile::tempdir().context("failed to create upgrade directory")?;
    let release_json = temporary_directory.path().join("releases.json");
    let releases_url = format!("https://api.github.com/repos/{repository}/releases?per_page=100");

    println!("Checking for the latest graft release...");
    download(&releases_url, &release_json)?;
    let latest = latest_stable_release(&fs::read_to_string(&release_json)?)?;
    if latest.version <= current_version {
        println!("graft {CURRENT_VERSION} is already up to date.");
        return Ok(());
    }

    let archive_name = format!(
        "graft-cli-{}-{}.{}",
        version_string(latest.version),
        target.triple,
        target.archive_extension
    );
    let base_url = format!(
        "https://github.com/{repository}/releases/download/{}",
        latest.tag
    );
    let archive_path = temporary_directory.path().join(&archive_name);
    let checksum_path = temporary_directory.path().join("SHA256SUMS");
    let extract_directory = temporary_directory.path().join("extract");

    println!(
        "Upgrading graft from {CURRENT_VERSION} to {}...",
        version_string(latest.version)
    );
    download(&format!("{base_url}/{archive_name}"), &archive_path)?;
    download(&format!("{base_url}/SHA256SUMS"), &checksum_path)?;
    verify_archive(&archive_path, &checksum_path, &archive_name)?;
    extract_archive(&archive_path, target, &extract_directory)?;

    let replacement = extract_directory.join(target.executable);
    verify_replacement(&replacement, latest.version)?;
    replace_executable(&replacement, &destination)?;

    println!(
        "Successfully upgraded graft to {}.",
        version_string(latest.version)
    );
    Ok(())
}

fn target_for_current_platform() -> Result<UpgradeTarget> {
    match (env::consts::OS, env::consts::ARCH) {
        ("macos", "x86_64") => Ok(UpgradeTarget {
            triple: "x86_64-apple-darwin",
            archive_extension: "tar.gz",
            executable: "graft",
        }),
        ("macos", "aarch64") => Ok(UpgradeTarget {
            triple: "aarch64-apple-darwin",
            archive_extension: "tar.gz",
            executable: "graft",
        }),
        ("linux", "x86_64") => Ok(UpgradeTarget {
            triple: "x86_64-unknown-linux-gnu",
            archive_extension: "tar.gz",
            executable: "graft",
        }),
        ("linux", "aarch64") => Ok(UpgradeTarget {
            triple: "aarch64-unknown-linux-gnu",
            archive_extension: "tar.gz",
            executable: "graft",
        }),
        ("windows", "x86_64") => Ok(UpgradeTarget {
            triple: "x86_64-pc-windows-msvc",
            archive_extension: "zip",
            executable: "graft.exe",
        }),
        ("windows", "aarch64") => Ok(UpgradeTarget {
            triple: "aarch64-pc-windows-msvc",
            archive_extension: "zip",
            executable: "graft.exe",
        }),
        (os, arch) => bail!("unsupported platform for graft upgrade: {os}-{arch}"),
    }
}

fn configured_repository() -> Result<String> {
    let repository = env::var("GRAFT_REPO").unwrap_or_else(|_| DEFAULT_REPOSITORY.to_string());
    validate_repository(&repository)?;
    Ok(repository)
}

fn validate_repository(repository: &str) -> Result<()> {
    let valid = repository.split('/').count() == 2
        && repository.split('/').all(|part| {
            !part.is_empty()
                && part
                    .bytes()
                    .all(|byte| byte.is_ascii_alphanumeric() || matches!(byte, b'-' | b'_' | b'.'))
        });
    if !valid {
        bail!("GRAFT_REPO must be a GitHub repository in OWNER/REPOSITORY form");
    }
    Ok(())
}

fn current_executable_path() -> Result<PathBuf> {
    let current = env::current_exe().context("failed to determine the current graft executable")?;
    let metadata = fs::symlink_metadata(&current)
        .with_context(|| format!("failed to inspect {}", current.display()))?;
    if metadata.file_type().is_symlink() {
        current
            .canonicalize()
            .with_context(|| format!("failed to resolve {}", current.display()))
    } else {
        Ok(current)
    }
}

fn parse_version(value: &str) -> Option<Version> {
    let mut components = value.strip_prefix('v').unwrap_or(value).split('.');
    let version = Version {
        major: components.next()?.parse().ok()?,
        minor: components.next()?.parse().ok()?,
        patch: components.next()?.parse().ok()?,
    };
    components.next().is_none().then_some(version)
}

fn version_string(version: Version) -> String {
    format!("{}.{}.{}", version.major, version.minor, version.patch)
}

fn latest_stable_release(body: &str) -> Result<Release> {
    let releases: Value =
        serde_json::from_str(body).context("GitHub returned invalid release metadata")?;
    let releases = releases
        .as_array()
        .context("GitHub release metadata was not an array")?;
    let mut latest = None;
    for release in releases {
        if release
            .get("draft")
            .and_then(Value::as_bool)
            .unwrap_or(false)
            || release
                .get("prerelease")
                .and_then(Value::as_bool)
                .unwrap_or(false)
        {
            continue;
        }
        let Some(tag) = release.get("tag_name").and_then(Value::as_str) else {
            continue;
        };
        let Some(version) = tag.strip_prefix('v').and_then(parse_version) else {
            continue;
        };
        let candidate = Release { tag: tag.to_string(), version };
        if latest
            .as_ref()
            .is_none_or(|release: &Release| candidate.version > release.version)
        {
            latest = Some(candidate);
        }
    }
    latest.context("could not find a stable graft CLI release")
}

fn command_available(program: &str) -> bool {
    Command::new(program)
        .arg("--version")
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .status()
        .is_ok()
}

fn download(url: &str, output: &Path) -> Result<()> {
    let user_agent = format!("graft/{CURRENT_VERSION}");
    let status = if command_available("curl") {
        Command::new("curl")
            .args([
                "-fsSL",
                "--retry",
                "3",
                "--connect-timeout",
                "20",
                "-A",
                &user_agent,
                "-o",
            ])
            .arg(output)
            .arg(url)
            .status()
            .context("failed to start curl")?
    } else if command_available("wget") {
        Command::new("wget")
            .args(["-q", "--tries=3", "--timeout=20", "--user-agent"])
            .arg(&user_agent)
            .args(["-O"])
            .arg(output)
            .arg(url)
            .status()
            .context("failed to start wget")?
    } else {
        bail!("graft upgrade requires curl or wget");
    };
    if !status.success() {
        bail!("failed to download {url}");
    }
    Ok(())
}

fn sha256_file(path: &Path) -> Result<String> {
    let mut file =
        File::open(path).with_context(|| format!("failed to open {}", path.display()))?;
    let mut digest = Sha256::new();
    let mut buffer = [0_u8; 64 * 1024];
    loop {
        let count = file.read(&mut buffer)?;
        if count == 0 {
            break;
        }
        digest.update(&buffer[..count]);
    }
    Ok(format!("{:x}", digest.finalize()))
}

fn verify_archive(archive: &Path, checksum_file: &Path, archive_name: &str) -> Result<()> {
    let checksums = fs::read_to_string(checksum_file)
        .with_context(|| format!("failed to read {}", checksum_file.display()))?;
    let expected = checksums.lines().find_map(|line| {
        let mut fields = line.split_whitespace();
        let digest = fields.next()?;
        let name = fields.next()?.trim_start_matches('*');
        (name == archive_name).then_some(digest)
    });
    let Some(expected) = expected else {
        bail!("SHA256SUMS did not contain a checksum for {archive_name}");
    };
    let actual = sha256_file(archive)?;
    if actual != expected {
        bail!("checksum mismatch for {archive_name}");
    }
    Ok(())
}

fn extract_archive(archive: &Path, target: UpgradeTarget, destination: &Path) -> Result<()> {
    fs::create_dir_all(destination)
        .with_context(|| format!("failed to create {}", destination.display()))?;
    let status = if target.archive_extension == "tar.gz" {
        Command::new("tar")
            .args(["-xzf"])
            .arg(archive)
            .args(["-C"])
            .arg(destination)
            .status()
            .context("failed to start tar")?
    } else if command_available("unzip") {
        Command::new("unzip")
            .args(["-q"])
            .arg(archive)
            .args(["-d"])
            .arg(destination)
            .status()
            .context("failed to start unzip")?
    } else {
        bail!("graft upgrade requires unzip to extract the Windows release");
    };
    if !status.success() {
        bail!("failed to extract {}", archive.display());
    }
    Ok(())
}

fn verify_replacement(path: &Path, expected: Version) -> Result<()> {
    let metadata = fs::symlink_metadata(path)
        .with_context(|| format!("release archive did not contain {}", path.display()))?;
    if !metadata.file_type().is_file() {
        bail!("release archive contained a non-file graft executable");
    }
    ensure_replacement_is_executable(path)?;
    let output = Command::new(path)
        .arg("--version")
        .output()
        .with_context(|| format!("failed to run {}", path.display()))?;
    if !output.status.success() {
        bail!("downloaded graft executable failed its version check");
    }
    let reported = String::from_utf8(output.stdout)
        .context("downloaded graft executable returned invalid version output")?;
    let Some(reported) = reported_version(&reported) else {
        bail!("downloaded graft executable reported an invalid version");
    };
    if reported != expected {
        bail!(
            "downloaded graft executable reported {}, expected {}",
            version_string(reported),
            version_string(expected)
        );
    }
    Ok(())
}

#[cfg(unix)]
fn ensure_replacement_is_executable(path: &Path) -> Result<()> {
    use std::os::unix::fs::PermissionsExt;

    let mut permissions = fs::metadata(path)
        .with_context(|| format!("failed to inspect {}", path.display()))?
        .permissions();
    let mode = permissions.mode();
    if mode & 0o100 == 0 {
        permissions.set_mode(mode | 0o100);
        fs::set_permissions(path, permissions)
            .with_context(|| format!("failed to make {} executable", path.display()))?;
    }
    Ok(())
}

#[cfg(not(unix))]
fn ensure_replacement_is_executable(_path: &Path) -> Result<()> {
    Ok(())
}

fn reported_version(output: &str) -> Option<Version> {
    let output = output.trim();
    let version = output
        .strip_prefix("graft-cli ")
        .or_else(|| output.strip_prefix("graft "))
        .unwrap_or(output);
    parse_version(version)
}

fn prepare_replacement(source: &Path, destination: &Path) -> Result<tempfile::NamedTempFile> {
    let parent = destination
        .parent()
        .context("current graft executable has no parent directory")?;
    let mut temporary = tempfile::NamedTempFile::new_in(parent).with_context(|| {
        format!(
            "failed to create a replacement beside {}",
            destination.display()
        )
    })?;
    let mut source_file = File::open(source).with_context(|| {
        format!(
            "failed to open the downloaded graft executable {}",
            source.display()
        )
    })?;
    io::copy(&mut source_file, temporary.as_file_mut()).with_context(|| {
        format!(
            "failed to copy the downloaded graft executable to {}",
            temporary.path().display()
        )
    })?;
    let permissions = fs::metadata(destination)
        .with_context(|| format!("failed to inspect {}", destination.display()))?
        .permissions();
    fs::set_permissions(temporary.path(), permissions)?;
    temporary.as_file_mut().sync_all()?;
    Ok(temporary)
}

#[cfg(not(windows))]
fn replace_executable(source: &Path, destination: &Path) -> Result<()> {
    let temporary = prepare_replacement(source, destination)?;
    temporary.persist(destination).map_err(|error| {
        anyhow::anyhow!(
            "failed to replace {}: {}",
            destination.display(),
            error.error
        )
    })?;
    Ok(())
}

#[cfg(windows)]
fn replace_executable(source: &Path, destination: &Path) -> Result<()> {
    let temporary = prepare_replacement(source, destination)?;
    let temporary = temporary.into_temp_path();
    let source = temporary.to_path_buf();

    let result = Command::new(destination)
        .args(["_upgrade-replace", "--source"])
        .arg(&source)
        .args(["--destination"])
        .arg(destination)
        .spawn()
        .context("failed to start the Windows upgrade helper");
    match result {
        Ok(_) => {
            std::mem::forget(temporary);
            Ok(())
        }
        Err(error) => Err(error),
    }
}

#[cfg(windows)]
pub(crate) fn replace_after_parent_exit(source: &Path, destination: &Path) -> Result<()> {
    for _ in 0..100 {
        match fs::copy(source, destination) {
            Ok(_) => {
                let _ = fs::remove_file(source);
                return Ok(());
            }
            Err(error)
                if matches!(
                    error.kind(),
                    std::io::ErrorKind::PermissionDenied | std::io::ErrorKind::WouldBlock
                ) =>
            {
                thread::sleep(Duration::from_millis(100))
            }
            Err(error) => {
                return Err(error).context("failed to replace the Windows graft executable");
            }
        }
    }
    bail!("timed out waiting for the previous graft process to exit")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn latest_stable_release_ignores_prereleases_and_drafts() {
        let body = r#"[
          {"tag_name":"v0.15.3","draft":false,"prerelease":false},
          {"tag_name":"v0.16.0-rc.1","draft":false,"prerelease":true},
          {"tag_name":"v0.15.4","draft":true,"prerelease":false},
          {"tag_name":"v0.15.2","draft":false,"prerelease":false}
        ]"#;

        assert_eq!(
            latest_stable_release(body).unwrap(),
            Release {
                tag: "v0.15.3".to_string(),
                version: Version { major: 0, minor: 15, patch: 3 },
            }
        );
    }

    #[test]
    fn configured_repository_rejects_url_injection() {
        assert!(validate_repository("eidos-space/graft/releases").is_err());
    }

    #[test]
    fn archive_checksum_must_match_the_downloaded_bytes() {
        let temporary_directory = tempfile::tempdir().unwrap();
        let archive = temporary_directory.path().join("graft-cli.tar.gz");
        let checksums = temporary_directory.path().join("SHA256SUMS");
        fs::write(&archive, b"verified release").unwrap();
        let digest = sha256_file(&archive).unwrap();
        fs::write(&checksums, format!("{digest}  graft-cli.tar.gz\n")).unwrap();

        verify_archive(&archive, &checksums, "graft-cli.tar.gz").unwrap();
        fs::write(&archive, b"tampered release").unwrap();
        assert!(verify_archive(&archive, &checksums, "graft-cli.tar.gz").is_err());
    }

    #[test]
    fn release_version_check_accepts_the_cli_version_output() {
        assert_eq!(
            reported_version("graft-cli 0.15.3\n"),
            Some(Version { major: 0, minor: 15, patch: 3 })
        );
        assert_eq!(
            reported_version("graft 0.15.3\n"),
            Some(Version { major: 0, minor: 15, patch: 3 })
        );
    }

    #[cfg(unix)]
    #[test]
    fn replacement_verification_restores_executable_permission() {
        use std::os::unix::fs::PermissionsExt;

        let temporary_directory = tempfile::tempdir().unwrap();
        let replacement = temporary_directory.path().join("graft");
        fs::write(&replacement, "#!/bin/sh\nprintf 'graft-cli 0.15.5\\n'\n").unwrap();
        fs::set_permissions(&replacement, fs::Permissions::from_mode(0o644)).unwrap();

        verify_replacement(&replacement, Version { major: 0, minor: 15, patch: 5 }).unwrap();

        let mode = fs::metadata(&replacement).unwrap().permissions().mode();
        assert_ne!(mode & 0o111, 0);
    }
}
