//! Shared self-update for fleet CLI tools.
//!
//! `upgrade(cfg)` downloads a GitHub release's assets, checksum-verifies them against the
//! release's `SHA256SUMS`, then atomically installs them into `install_dir` — replacing
//! the currently-running binary in place (safe on Linux: rename over a running executable
//! keeps the old inode alive for the running process).
//!
//! It's parameterized by `repo` + `bin_name` + `assets`, so `supervisor upgrade` and
//! `inbox upgrade` are the *same code* with different config — the fleet-release
//! generalization (one crate, no per-tool bash wrappers).

use anyhow::{anyhow, bail, Context, Result};
use std::io::Read;
use std::path::{Path, PathBuf};

pub struct UpgradeConfig {
    /// "owner/repo", e.g. "colinrozzi/supervisor".
    pub repo: String,
    /// The executable asset name (also chmod +x'd), e.g. "supervisor".
    pub bin_name: String,
    /// Every asset to download + install (includes bin_name). e.g. ["supervisor", "supervisor.wasm"].
    pub assets: Vec<String>,
    /// Checksums manifest asset name (sha256sum -c format). Usually "SHA256SUMS".
    pub checksums: String,
    /// Where to install (e.g. ~/.local/bin). Created if absent.
    pub install_dir: PathBuf,
}

impl UpgradeConfig {
    /// Convenience for a supervisor-shaped tool: one binary + one embedded-wasm sidecar.
    pub fn new(repo: &str, bin_name: &str, assets: &[&str], install_dir: PathBuf) -> Self {
        Self {
            repo: repo.to_string(),
            bin_name: bin_name.to_string(),
            assets: assets.iter().map(|s| s.to_string()).collect(),
            checksums: "SHA256SUMS".to_string(),
            install_dir,
        }
    }
}

/// Download the latest release's assets, verify checksums, install atomically.
/// Returns the installed paths. Prints progress to stderr.
pub fn upgrade(cfg: &UpgradeConfig) -> Result<Vec<PathBuf>> {
    let base = format!("https://github.com/{}/releases/latest/download", cfg.repo);
    let tmp = std::env::temp_dir().join(format!("fleet-release-{}", cfg.bin_name));
    let _ = std::fs::remove_dir_all(&tmp);
    std::fs::create_dir_all(&tmp).with_context(|| format!("creating temp dir {}", tmp.display()))?;

    // 1. Download the checksums manifest + every asset into the temp dir.
    eprintln!("fetching latest {} release from {}…", cfg.bin_name, cfg.repo);
    let sums_raw = download(&format!("{base}/{}", cfg.checksums))
        .with_context(|| format!("downloading {}", cfg.checksums))?;
    let sums = String::from_utf8(sums_raw).context("SHA256SUMS is not UTF-8")?;

    for asset in &cfg.assets {
        let url = format!("{base}/{asset}");
        let bytes = download(&url).with_context(|| format!("downloading {asset}"))?;
        // 2. Verify against the manifest BEFORE writing to the install location.
        let want = expected_sum(&sums, asset)
            .ok_or_else(|| anyhow!("{} not listed in {}", asset, cfg.checksums))?;
        let got = sha256_hex(&bytes);
        if got != want {
            bail!("checksum mismatch for {asset}: expected {want}, got {got}");
        }
        std::fs::write(tmp.join(asset), &bytes).with_context(|| format!("writing temp {asset}"))?;
        eprintln!("  verified {asset} ({} bytes)", bytes.len());
    }

    // 3. All verified — install atomically.
    std::fs::create_dir_all(&cfg.install_dir)
        .with_context(|| format!("creating install dir {}", cfg.install_dir.display()))?;
    let mut installed = Vec::new();
    for asset in &cfg.assets {
        let dest = cfg.install_dir.join(asset);
        install_file(&tmp.join(asset), &dest, asset == &cfg.bin_name)
            .with_context(|| format!("installing {}", dest.display()))?;
        installed.push(dest);
    }
    let _ = std::fs::remove_dir_all(&tmp);
    eprintln!("installed {} to {}", cfg.bin_name, cfg.install_dir.display());
    Ok(installed)
}

/// HTTP GET following redirects; returns the body bytes. 32 MiB cap.
fn download(url: &str) -> Result<Vec<u8>> {
    let resp = ureq::get(url)
        .set("User-Agent", "fleet-release")
        .call()
        .map_err(|e| anyhow!("GET {url}: {e}"))?;
    let mut buf = Vec::new();
    resp.into_reader()
        .take(32 * 1024 * 1024)
        .read_to_end(&mut buf)
        .with_context(|| format!("reading {url}"))?;
    if buf.is_empty() {
        bail!("empty response from {url}");
    }
    Ok(buf)
}

/// Find the expected hex digest for `asset` in a `sha256sum`-format manifest
/// ("<hex>  <name>" per line; a leading '*' binary marker on the name is tolerated).
fn expected_sum(manifest: &str, asset: &str) -> Option<String> {
    for line in manifest.lines() {
        let mut it = line.split_whitespace();
        let hash = it.next()?;
        let name = it.next().unwrap_or("").trim_start_matches('*');
        if name == asset {
            return Some(hash.to_lowercase());
        }
    }
    None
}

fn sha256_hex(bytes: &[u8]) -> String {
    use sha2::{Digest, Sha256};
    let mut h = Sha256::new();
    h.update(bytes);
    h.finalize().iter().map(|b| format!("{b:02x}")).collect()
}

/// Install `src` at `dest` (rename if same fs, else copy+replace); chmod +x if executable.
fn install_file(src: &Path, dest: &Path, executable: bool) -> Result<()> {
    // rename works over a running binary on Linux; fall back to copy across filesystems.
    if std::fs::rename(src, dest).is_err() {
        std::fs::copy(src, dest)?;
    }
    #[cfg(unix)]
    if executable {
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(dest, std::fs::Permissions::from_mode(0o755))?;
    }
    #[cfg(not(unix))]
    let _ = executable;
    Ok(())
}
