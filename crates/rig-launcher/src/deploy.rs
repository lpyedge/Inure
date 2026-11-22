use std::ffi::OsString;
use std::fs::{self, File, OpenOptions};
use std::io::{self, Read, Write};
use std::path::{Path, PathBuf};
use std::time::Duration;

use ring::digest::{Context, SHA256};
use serde::Deserialize;
use ureq::tls::{RootCerts, TlsConfig};
use ureq::Agent;
use zip::ZipArchive;

use crate::Result;

const MAX_ARCHIVE_SIZE: u64 = 1024 * 1024 * 1024;
const CHROME_CATALOG: &str =
    "https://googlechromelabs.github.io/chrome-for-testing/last-known-good-versions-with-downloads.json";
const WINDOWS_ARM_SNAPSHOTS: &str =
    "https://storage.googleapis.com/chromium-browser-snapshots/Win_Arm64";

#[derive(Deserialize)]
struct Release {
    target: String,
    abi: String,
    deps_base_url: Option<String>,
}

#[derive(Deserialize)]
struct MediaAsset {
    archive: String,
    sha256: String,
}

#[derive(Deserialize)]
struct ChromeCatalog {
    channels: ChromeChannels,
}

#[derive(Deserialize)]
struct ChromeChannels {
    #[serde(rename = "Stable")]
    stable: ChromeRelease,
}

#[derive(Deserialize)]
struct ChromeRelease {
    downloads: ChromeDownloads,
}

#[derive(Deserialize)]
struct ChromeDownloads {
    chrome: Vec<ChromeAsset>,
}

#[derive(Deserialize)]
struct ChromeAsset {
    platform: String,
    url: String,
}

pub(crate) fn install(root: &Path, args: &[OsString]) -> Result<()> {
    let (with_chromium, base_url) = install_options(args)?;
    let release: Release = serde_json::from_reader(File::open(root.join("release.json"))?)?;
    let base = base_url
        .or(release.deps_base_url.as_deref())
        .ok_or("Set --deps-base-url to the HTTPS location of the published release assets.")?;
    if !base.starts_with("https://") || base.contains(['?', '#']) {
        return Err("The dependency base URL must use HTTPS and have no query or fragment.".into());
    }
    let base = base.trim_end_matches('/');
    let lock = OpenOptions::new()
        .read(true)
        .write(true)
        .create(true)
        .truncate(false)
        .open(root.join(".rig-install.lock"))?;
    lock.try_lock()
        .map_err(|error| format!("Cannot lock dependency installation: {error}"))?;
    let staging = tempfile::tempdir_in(root)?;
    let stage = staging.path();
    let agent: Agent = Agent::config_builder()
        .https_only(true)
        .tls_config(
            TlsConfig::builder()
                .unversioned_rustls_crypto_provider(
                    rustls::crypto::aws_lc_rs::default_provider().into(),
                )
                .root_certs(RootCerts::PlatformVerifier)
                .build(),
        )
        .timeout_global(Some(Duration::from_secs(600)))
        .build()
        .into();
    let index = format!(
        "rig-edge-browser-deps-{}-{}.json",
        release.target, release.abi
    );
    let asset: MediaAsset = serde_json::from_str(
        &agent
            .get(format!("{base}/{index}"))
            .call()?
            .body_mut()
            .read_to_string()?,
    )?;
    if !asset
        .archive
        .ends_with(&format!("-{}-full.zip", release.target))
        || !asset
            .archive
            .bytes()
            .all(|byte| byte.is_ascii_alphanumeric() || matches!(byte, b'.' | b'-' | b'_'))
    {
        return Err("Dependency index does not name a matching full release archive.".into());
    }

    eprintln!(
        "Downloading portable media runtime for {} ({})",
        release.target, release.abi
    );
    let media_zip = stage.join("media.zip");
    let actual_sha = download(&agent, &format!("{base}/{}", asset.archive), &media_zip)?;
    if actual_sha != asset.sha256.to_ascii_lowercase() {
        return Err("Media release SHA-256 does not match the dependency index.".into());
    }
    let media = stage.join("media");
    extract(&media_zip, &media)?;
    let runtime = media.join("runtime");
    if !runtime.join("lib").is_dir()
        || fs::read_to_string(runtime.join("abi.txt"))?.trim() != release.abi
    {
        return Err("The downloaded media runtime is incompatible with this release.".into());
    }
    // 用当前发行版的主程序检查动态链接和编码器，下载包里的程序不会执行。
    let verification = stage.join("verification");
    fs::create_dir(&verification)?;
    let candidate = verification.join("runtime");
    fs::rename(runtime, &candidate)?;
    let local_engine = crate::engine_path(root);
    let check_engine = crate::engine_path(&verification);
    fs::create_dir_all(check_engine.parent().ok_or("Invalid engine path.")?)?;
    fs::copy(local_engine, check_engine)?;
    if !crate::engine_command(&verification)?
        .arg("--check-media-runtime")
        .status()?
        .success()
    {
        return Err("Downloaded media runtime failed its compatibility check.".into());
    }

    let chromium = if with_chromium {
        Some(prepare_chromium(&agent, stage, &release.target)?)
    } else {
        None
    };
    let mut replacements = vec![(candidate, "runtime")];
    if let Some(chromium) = chromium {
        replacements.push((chromium, "chromium"));
    }
    if let Err(error) = replace_directories(root, stage, &replacements) {
        if replacements
            .iter()
            .any(|(_, name)| stage.join(format!("previous-{name}")).exists())
        {
            return Err(format!(
                "Replacement failed ({error}); previous dependencies were saved in {}",
                staging.keep().display()
            )
            .into());
        }
        return Err(error.into());
    }
    eprintln!("Portable dependencies ready in {}", root.display());
    Ok(())
}

fn install_options(args: &[OsString]) -> Result<(bool, Option<&str>)> {
    let mut chromium = false;
    let mut base_url = None;
    let mut args = args.iter().map(|arg| arg.to_str());
    while let Some(arg) = args.next() {
        match arg {
            Some("--with-chromium") => chromium = true,
            Some("--deps-base-url") => {
                base_url = Some(
                    args.next()
                        .flatten()
                        .ok_or("Missing --deps-base-url value.")?,
                );
            }
            _ => {
                return Err(
                    "--install-deps accepts only --with-chromium and --deps-base-url.".into(),
                )
            }
        }
    }
    Ok((chromium, base_url))
}

fn download(agent: &Agent, url: &str, path: &Path) -> Result<String> {
    let mut response = agent.get(url).call()?;
    let mut body = response
        .body_mut()
        .with_config()
        .limit(MAX_ARCHIVE_SIZE)
        .reader();
    let mut file = File::create(path)?;
    let mut hash = Context::new(&SHA256);
    let mut buffer = [0_u8; 65536];
    loop {
        let count = body.read(&mut buffer)?;
        if count == 0 {
            break;
        }
        let chunk = &buffer[..count];
        file.write_all(chunk)?;
        hash.update(chunk);
    }
    Ok(hash
        .finish()
        .as_ref()
        .iter()
        .map(|byte| format!("{byte:02x}"))
        .collect())
}

fn extract(path: &Path, output: &Path) -> Result<()> {
    let mut archive = ZipArchive::new(File::open(path)?)?;
    if archive
        .decompressed_size()
        .is_none_or(|size| size > u128::from(MAX_ARCHIVE_SIZE) * 2)
    {
        return Err("The downloaded archive exceeds the expanded size limit.".into());
    }
    fs::create_dir(output)?;
    archive.extract(output)?;
    Ok(())
}

fn prepare_chromium(agent: &Agent, stage: &Path, target: &str) -> Result<PathBuf> {
    let (url, top_name) = chromium_asset(agent, target)?;
    eprintln!("Downloading portable Chromium for {target}");
    let zip = stage.join("chromium.zip");
    download(agent, &url, &zip)?;
    let output = stage.join("browser");
    extract(&zip, &output)?;
    let top = output.join(top_name);
    let valid = if target == "macos-arm64" {
        top.join("Google Chrome for Testing.app/Contents/MacOS/Google Chrome for Testing")
            .is_file()
    } else {
        fs::read_dir(&top)?.any(|entry| {
            entry.is_ok_and(|entry| {
                crate::executable_name(&entry.file_name()) && entry.path().is_file()
            })
        })
    };
    if !valid {
        return Err("The downloaded browser does not contain a Chromium executable.".into());
    }
    Ok(top)
}

fn chromium_asset(agent: &Agent, target: &str) -> Result<(String, String)> {
    if target == "windows-arm64" {
        let body = agent
            .get(format!("{WINDOWS_ARM_SNAPSHOTS}/LAST_CHANGE"))
            .call()?
            .body_mut()
            .read_to_string()?;
        let revision = body.trim().parse::<u64>()?;
        let url = format!("{WINDOWS_ARM_SNAPSHOTS}/{revision}/chrome-win.zip");
        return Ok((url, "chrome-win".into()));
    }
    let platform = match target {
        "linux-x86_64" => "linux64",
        "linux-arm64" => "linux-arm64",
        "windows-x64" => "win64",
        "macos-arm64" => "mac-arm64",
        _ => return Err(format!("Unsupported Chromium target: {target}").into()),
    };
    let body = agent
        .get(CHROME_CATALOG)
        .call()?
        .body_mut()
        .read_to_string()?;
    let catalog: ChromeCatalog = serde_json::from_str(&body)?;
    let asset = catalog
        .channels
        .stable
        .downloads
        .chrome
        .into_iter()
        .find(|item| item.platform == platform)
        .ok_or_else(|| {
            format!("Chrome for Testing does not list {platform} on its stable channel.")
        })?;
    if !asset
        .url
        .starts_with("https://storage.googleapis.com/chrome-for-testing-public/")
    {
        return Err("Unexpected Chrome for Testing download origin.".into());
    }
    Ok((asset.url, format!("chrome-{platform}")))
}

fn replace_directories(
    root: &Path,
    stage: &Path,
    replacements: &[(PathBuf, &str)],
) -> io::Result<()> {
    let mut replaced = Vec::new();
    for (source, name) in replacements {
        let destination = root.join(name);
        let backup = stage.join(format!("previous-{name}"));
        let result = match fs::rename(&destination, &backup) {
            Ok(()) => {
                replaced.push((source, destination.clone(), Some(backup)));
                fs::rename(source, &destination)
            }
            Err(error) if error.kind() == io::ErrorKind::NotFound => {
                replaced.push((source, destination.clone(), None));
                fs::rename(source, &destination)
            }
            Err(error) => Err(error),
        };
        if let Err(error) = result {
            for (source, destination, backup) in replaced.into_iter().rev() {
                if destination.exists() {
                    fs::rename(&destination, source)?;
                }
                if let Some(backup) = backup {
                    fs::rename(backup, destination)?;
                }
            }
            return Err(error);
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn deployment_rejects_server_options() {
        assert!(install_options(&["--stdio".into()]).is_err());
        assert!(install_options(&["--deps-base-url".into()]).is_err());
        assert_eq!(
            install_options(&[
                "--with-chromium".into(),
                "--deps-base-url".into(),
                "https://example.com".into()
            ])
            .unwrap(),
            (true, Some("https://example.com"))
        );
    }

    #[test]
    fn replace_updates_and_restores_on_second_failure() {
        let temp = tempfile::tempdir().unwrap();
        let root = temp.path();
        let stage = root.join("stage");
        let initial = root.join("runtime");
        fs::create_dir(&stage).unwrap();
        fs::create_dir(&initial).unwrap();
        fs::write(initial.join("old"), "old").unwrap();
        let new = stage.join("new");
        fs::create_dir(&new).unwrap();
        fs::write(new.join("updated"), "updated").unwrap();
        assert!(replace_directories(
            root,
            &stage,
            &[(new, "runtime"), (stage.join("missing"), "chromium")]
        )
        .is_err());
        assert!(initial.join("old").is_file());
        let new = stage.join("again");
        fs::create_dir(&new).unwrap();
        fs::write(new.join("updated"), "updated").unwrap();
        replace_directories(root, &stage, &[(new, "runtime")]).unwrap();
        assert!(initial.join("updated").is_file());
        assert!(!initial.join("old").exists());
    }
}
