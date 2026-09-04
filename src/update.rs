//! `rcdu update` — replace the running binary with the latest GitHub release.
//!
//! Downloads via `curl` (the same tool the install script relies on) and
//! verifies the artifact against the release's SHA-256 checksums in-process.

use std::path::Path;
use std::process::Command;

use crate::sha256;

const REPO: &str = "mayankasthana/rcdu";

/// Asset name for the platform rcdu was built for, as published on Releases.
fn asset_name() -> Option<&'static str> {
    match (std::env::consts::OS, std::env::consts::ARCH) {
        ("macos", "aarch64") => Some("rcdu-portable-macos-aarch64"),
        ("macos", "x86_64") => Some("rcdu-portable-macos-x86_64"),
        ("linux", "x86_64") => Some("rcdu-portable-linux-x86_64"),
        ("linux", "aarch64") => Some("rcdu-portable-linux-aarch64"),
        _ => None,
    }
}

pub fn run(check_only: bool) -> i32 {
    match run_inner(check_only) {
        Ok(()) => 0,
        Err(e) => {
            eprintln!("rcdu: update failed: {e}");
            1
        }
    }
}

fn run_inner(check_only: bool) -> Result<(), String> {
    let current = env!("CARGO_PKG_VERSION");
    let asset =
        asset_name().ok_or_else(|| "no prebuilt release exists for this platform".to_string())?;

    let tag = fetch_latest_tag()?;
    if !is_newer(&tag, current) {
        println!("rcdu {current} is up to date (latest release: {tag})");
        return Ok(());
    }
    println!("updating rcdu {current} -> {tag}");

    if check_only {
        println!("run `rcdu update` to install it");
        return Ok(());
    }

    let exe = std::env::current_exe()
        .map_err(|e| format!("cannot locate the running binary: {e}"))
        .and_then(|p| {
            std::fs::canonicalize(&p).map_err(|e| format!("cannot resolve binary path: {e}"))
        })?;

    let base = format!("https://github.com/{REPO}/releases/download/{tag}");
    println!("downloading {asset} ...");
    let bytes = curl(&format!("{base}/{asset}"), 300)?;
    println!("verifying checksum ...");
    let sums = String::from_utf8(curl(&format!("{base}/sha256sums.txt"), 60)?)
        .map_err(|_| "sha256sums.txt is not valid UTF-8".to_string())?;
    let expected = expected_digest(&sums, asset)
        .ok_or_else(|| format!("{asset} is not listed in sha256sums.txt — refusing to install"))?;
    if !fixed_eq(&sha256::hex_digest(&bytes), &expected) {
        return Err(format!(
            "checksum mismatch for {asset} (expected {expected}) — aborting"
        ));
    }

    install(&bytes, &exe)?;
    println!("installed rcdu {tag} to {}", exe.display());
    Ok(())
}

/// Latest release tag from the GitHub API (e.g. `v0.2.0`).
fn fetch_latest_tag() -> Result<String, String> {
    let url = format!("https://api.github.com/repos/{REPO}/releases/latest");
    let body = curl(&url, 30)?;
    let json: serde_json::Value = serde_json::from_slice(&body)
        .map_err(|e| format!("unexpected response from the GitHub API: {e}"))?;
    json.get("tag_name")
        .and_then(|t| t.as_str())
        .map(str::to_string)
        .ok_or_else(|| format!("no releases found — see https://github.com/{REPO}/releases"))
}

fn curl(url: &str, max_time: u64) -> Result<Vec<u8>, String> {
    let out = Command::new("curl")
        .args([
            "-fsSL",
            "--retry",
            "3",
            "--max-time",
            &max_time.to_string(),
            "-H",
            &format!("User-Agent: rcdu/{}", env!("CARGO_PKG_VERSION")),
            url,
        ])
        .output()
        .map_err(|e| format!("cannot run curl ({e}) — it is required for `rcdu update`"))?;
    if !out.status.success() {
        return Err(format!(
            "download failed (curl exit {}): {url}",
            out.status.code().unwrap_or(-1)
        ));
    }
    Ok(out.stdout)
}

/// True when `latest` is a strictly newer dotted version than `current`.
/// Ignores a leading `v` and any pre-release/build suffix.
fn is_newer(latest: &str, current: &str) -> bool {
    fn parts(v: &str) -> Vec<u64> {
        let v = v.trim().trim_start_matches('v');
        v.split(['-', '+'])
            .next()
            .unwrap_or(v)
            .split('.')
            .map(|p| p.parse().unwrap_or(0))
            .collect()
    }
    let (l, c) = (parts(latest), parts(current));
    for i in 0..l.len().max(c.len()) {
        let li = l.get(i).copied().unwrap_or(0);
        let ci = c.get(i).copied().unwrap_or(0);
        if li != ci {
            return li > ci;
        }
    }
    false
}

/// First column of the `<digest>  <name>` line matching `asset`.
fn expected_digest(sums: &str, asset: &str) -> Option<String> {
    sums.lines().find_map(|line| {
        let mut it = line.split_whitespace();
        let digest = it.next()?;
        let name = it.next()?.trim_start_matches('*');
        (name == asset).then(|| digest.to_lowercase())
    })
}

/// Constant-time-enough comparison for equal-length hex digests.
fn fixed_eq(a: &str, b: &str) -> bool {
    a.len() == b.len()
        && a.bytes()
            .zip(b.bytes())
            .fold(0u8, |acc, (x, y)| acc | (x ^ y))
            == 0
}

/// Write the new binary next to the old one and atomically rename it over.
fn install(bytes: &[u8], exe: &Path) -> Result<(), String> {
    use std::os::unix::fs::PermissionsExt;
    let dir = exe
        .parent()
        .ok_or_else(|| "binary has no parent directory".to_string())?;
    let tmp = dir.join(format!(".rcdu.update.{}", std::process::id()));
    std::fs::write(&tmp, bytes).map_err(|e| {
        format!(
            "cannot write {} ({e}) — is the install directory writable?",
            tmp.display()
        )
    })?;
    std::fs::set_permissions(&tmp, std::fs::Permissions::from_mode(0o755))
        .and_then(|()| std::fs::rename(&tmp, exe))
        .map_err(|e| {
            let _ = std::fs::remove_file(&tmp);
            format!("cannot replace {} ({e})", exe.display())
        })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn version_comparison() {
        assert!(is_newer("0.2.0", "0.1.0"));
        assert!(is_newer("v0.1.1", "0.1.0"));
        assert!(is_newer("v1.0.0", "0.9.9"));
        assert!(is_newer("0.10.0", "0.9.9"));
        assert!(!is_newer("0.1.0", "0.1.0"));
        assert!(!is_newer("0.1.0", "0.2.0"));
        assert!(!is_newer("v0.1.0", "0.1.0"));
        assert!(is_newer("0.2.0-rc1", "0.1.9"));
        assert!(!is_newer("0.2.0-rc1", "0.2.0"));
    }

    #[test]
    fn digest_lookup() {
        let sums = "e3b0c442  rcdu-portable-linux-x86_64\n\
                    ba7816bf  rcdu-portable-macos-aarch64\n";
        assert_eq!(
            expected_digest(sums, "rcdu-portable-macos-aarch64").as_deref(),
            Some("ba7816bf")
        );
        assert_eq!(expected_digest(sums, "rcdu-portable-linux-aarch64"), None);
        // sha256sum's binary-mode marker (`*name`) is tolerated.
        let sums = "ba7816bf *rcdu-portable-macos-aarch64\n";
        assert_eq!(
            expected_digest(sums, "rcdu-portable-macos-aarch64").as_deref(),
            Some("ba7816bf")
        );
    }

    #[test]
    fn digest_equality() {
        assert!(fixed_eq("abcd", "abcd"));
        assert!(!fixed_eq("abcd", "abce"));
        assert!(!fixed_eq("abc", "abcd"));
    }
}
