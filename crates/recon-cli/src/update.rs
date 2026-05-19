//! `recon update` — self-upgrade to the latest published release.
//!
//! Flow:
//!   1. GET `{origin}/latest.json` → `{ version: "v0.1.2" }`.
//!   2. Compare against `CARGO_PKG_VERSION`. Exit early if already up to
//!      date (unless `--force`). Exit early after reporting if `--check`.
//!   3. GET the target's archive from `{origin}/releases/{version}/`.
//!   4. GET `SHA256SUMS.txt` from the same directory; locate the line
//!      matching the archive filename; verify against the downloaded
//!      bytes. Any mismatch aborts — we never overwrite the running
//!      binary with an archive we can't integrity-check.
//!   5. Extract the binary to a tempdir adjacent to the current exe.
//!   6. Replace the running binary:
//!        - Unix: single `rename(new, current)`. POSIX unlinks the old
//!          inode; any still-running instance keeps its open fd valid.
//!        - Windows: `rename(current, current.exe.old)` then
//!          `rename(new, current)`. An already-running `.exe` cannot
//!          be directly overwritten; the .old stub is harmless (the
//!          next `recon update` prunes it).
//!
//! Cosign verification is intentionally NOT done here — the sigstore
//! crate would pull a big dep tree for a capability most users won't
//! invoke, and `install.sh | bash` still cosign-verifies on first
//! install. SHA256 against the published manifest is the primary
//! integrity gate.

use anyhow::{anyhow, bail, Context, Result};
use sha2::{Digest, Sha256};
use std::io::Read;
use std::path::Path;

/// Where latest.json and the release tree live. Single source so tests
/// and release.yml only need to agree on one hostname.
const ORIGIN: &str = "https://mcprecon.pages.dev";

/// Resolve the release-artifact target triple for the binary we are.
/// Must match the `target:` entries in `.github/workflows/release.yml`.
/// Any platform we don't ship for returns Err — we'd rather fail loud
/// than download a `recon-unknown-...tar.gz` that 404s silently.
fn current_target() -> Result<&'static str> {
    #[cfg(all(target_os = "linux", target_arch = "x86_64"))]
    {
        Ok("x86_64-unknown-linux-gnu")
    }
    #[cfg(all(target_os = "linux", target_arch = "aarch64"))]
    {
        Ok("aarch64-unknown-linux-gnu")
    }
    #[cfg(all(target_os = "macos", target_arch = "x86_64"))]
    {
        Ok("x86_64-apple-darwin")
    }
    #[cfg(all(target_os = "macos", target_arch = "aarch64"))]
    {
        Ok("aarch64-apple-darwin")
    }
    #[cfg(all(target_os = "windows", target_arch = "x86_64"))]
    {
        Ok("x86_64-pc-windows-msvc")
    }
    #[cfg(not(any(
        all(target_os = "linux", target_arch = "x86_64"),
        all(target_os = "linux", target_arch = "aarch64"),
        all(target_os = "macos", target_arch = "x86_64"),
        all(target_os = "macos", target_arch = "aarch64"),
        all(target_os = "windows", target_arch = "x86_64"),
    )))]
    {
        Err(anyhow!(
            "recon update: no published release for this platform. \
             See https://mcprecon.pages.dev/Docs#install for supported \
             targets, or contact support if you need a build for yours."
        ))
    }
}

/// Archive extension per platform. Release.yml packages Unix as
/// .tar.gz and Windows as .zip — mirrored here.
fn archive_ext() -> &'static str {
    if cfg!(target_os = "windows") {
        "zip"
    } else {
        "tar.gz"
    }
}

/// Binary filename inside the archive, as produced by release.yml.
fn binary_name() -> &'static str {
    if cfg!(target_os = "windows") {
        "recon.exe"
    } else {
        "recon"
    }
}

fn http_get_bytes(url: &str) -> Result<Vec<u8>> {
    let resp = ureq::get(url)
        .call()
        .with_context(|| format!("GET {} failed", url))?;
    if resp.status() != 200 {
        bail!("GET {} returned HTTP {}", url, resp.status());
    }
    let mut body: Vec<u8> = Vec::new();
    resp.into_body()
        .into_reader()
        .read_to_end(&mut body)
        .with_context(|| format!("reading body from {}", url))?;
    Ok(body)
}

fn http_get_string(url: &str) -> Result<String> {
    let bytes = http_get_bytes(url)?;
    String::from_utf8(bytes).with_context(|| format!("response from {} was not UTF-8", url))
}

/// Parse the `version` field from latest.json. Strips the leading `v`
/// so the caller can parse as a semver version directly.
fn fetch_latest_version() -> Result<String> {
    let body = http_get_string(&format!("{}/latest.json", ORIGIN))?;
    // Minimal parse — avoid adding serde_json just for one field lookup.
    // latest.json is published by release.yml as `{"version":"v0.1.2"}`.
    let raw = body
        .split("\"version\"")
        .nth(1)
        .and_then(|s| s.split('"').nth(1))
        .ok_or_else(|| anyhow!("latest.json: could not find 'version' field in {:?}", body))?;
    Ok(raw.trim_start_matches('v').to_string())
}

fn sha256_hex(bytes: &[u8]) -> String {
    let digest = Sha256::digest(bytes);
    let mut s = String::with_capacity(64);
    for b in digest {
        use std::fmt::Write;
        let _ = write!(s, "{:02x}", b);
    }
    s
}

/// Find the SHA256 in the manifest that matches `asset`. The manifest
/// is the output of `sha256sum`: "<hex>  <filename>\n" per line.
fn sha256_for(manifest: &str, asset: &str) -> Option<String> {
    manifest.lines().find_map(|line| {
        // `sha256sum` uses two spaces between hash and filename.
        let (hash, name) = line.split_once("  ")?;
        if name.trim() == asset {
            Some(hash.trim().to_string())
        } else {
            None
        }
    })
}

/// Extract the `recon` binary from the downloaded archive to
/// `out_path`. The caller picks a staging filename distinct from the
/// running exe — the staging dir is the exe's parent, so we would
/// otherwise rewrite the live binary mid-run.
fn extract_binary(archive: &[u8], out_path: &Path) -> Result<()> {
    #[cfg(not(target_os = "windows"))]
    {
        let gz = flate2::read::GzDecoder::new(archive);
        let mut tar = tar::Archive::new(gz);
        for entry in tar.entries().context("reading tarball entries")? {
            let mut entry = entry.context("reading tar entry")?;
            let path = entry.path().context("reading tar entry path")?.into_owned();
            // The release tarball has a single top-level entry named
            // `recon`. Match by filename so a future CI change that
            // introduces a subdirectory doesn't silently miss it.
            if path.file_name().and_then(|s| s.to_str()) == Some(binary_name()) {
                entry
                    .unpack(out_path)
                    .with_context(|| format!("unpacking {} from tarball", binary_name()))?;
                return Ok(());
            }
        }
        bail!("archive did not contain a `{}` entry", binary_name());
    }
    #[cfg(target_os = "windows")]
    {
        use std::io::Cursor;
        let reader = Cursor::new(archive);
        let mut zip = zip::ZipArchive::new(reader).context("reading zip archive")?;
        for i in 0..zip.len() {
            let mut entry = zip.by_index(i).context("reading zip entry")?;
            let name = entry.name().to_string();
            if Path::new(&name).file_name().and_then(|s| s.to_str()) == Some(binary_name()) {
                let mut out = std::fs::File::create(out_path)
                    .with_context(|| format!("creating {}", out_path.display()))?;
                std::io::copy(&mut entry, &mut out)
                    .with_context(|| format!("writing {}", out_path.display()))?;
                return Ok(());
            }
        }
        bail!("archive did not contain a `{}` entry", binary_name());
    }
}

/// Atomically replace the currently-running binary with `new_path`.
///
/// Unix: single rename; the old inode is unlinked but any running
/// instance keeps its open fd, so the binary finishes whatever it was
/// doing and the next invocation picks up the new version.
///
/// Windows: the running .exe can't be directly overwritten. Move it
/// aside first (`recon.exe.old`) and rename the new file into place.
/// A follow-up `recon update` prunes the .old file.
fn replace_running(new_path: &Path, current_exe: &Path) -> Result<()> {
    #[cfg(target_os = "windows")]
    {
        let backup = current_exe.with_extension("exe.old");
        // Clean a stale .old left by a previous update so we don't
        // accumulate them indefinitely.
        let _ = std::fs::remove_file(&backup);
        std::fs::rename(current_exe, &backup).with_context(|| {
            format!(
                "moving current exe to backup: {} -> {}",
                current_exe.display(),
                backup.display()
            )
        })?;
    }
    std::fs::rename(new_path, current_exe).with_context(|| {
        format!(
            "renaming new binary over current exe: {} -> {}",
            new_path.display(),
            current_exe.display()
        )
    })?;
    // Unix: tarball unpack already preserved mode bits.
    // Windows: no executable bit to set.
    Ok(())
}

/// Entry point called from `recon update`. Returns Ok on success
/// (already up-to-date OR updated) and Err otherwise.
pub fn run(check: bool, force: bool) -> Result<()> {
    let current_raw = env!("CARGO_PKG_VERSION");
    let current = semver::Version::parse(current_raw)
        .with_context(|| format!("parsing own version {:?}", current_raw))?;

    eprintln!("Current version: {}", current_raw);
    let latest_raw = fetch_latest_version().context("fetching latest.json")?;
    let latest = semver::Version::parse(&latest_raw)
        .with_context(|| format!("parsing latest.json version {:?}", latest_raw))?;
    eprintln!("Latest version:  {}", latest);

    if latest <= current && !force {
        eprintln!("Already up to date.");
        return Ok(());
    }
    if check {
        if latest > current {
            eprintln!("Update available: {} -> {}", current, latest);
            eprintln!("Run `recon update` to install.");
        }
        return Ok(());
    }

    let target = current_target()?;
    let ext = archive_ext();
    let asset = format!("recon-{}.{}", target, ext);
    let base = format!("{}/releases/v{}", ORIGIN, latest);

    eprintln!("Downloading {}/{}...", base, asset);
    let archive_bytes = http_get_bytes(&format!("{}/{}", base, asset))
        .with_context(|| format!("downloading {}", asset))?;

    eprintln!("Verifying SHA256...");
    let manifest =
        http_get_string(&format!("{}/SHA256SUMS.txt", base)).context("fetching SHA256SUMS.txt")?;
    let expected = sha256_for(&manifest, &asset)
        .ok_or_else(|| anyhow!("SHA256 for {} not found in published manifest", asset))?;
    let actual = sha256_hex(&archive_bytes);
    if actual != expected {
        bail!(
            "SHA256 mismatch for {} — refusing to install.\n  expected: {}\n  actual:   {}",
            asset,
            expected,
            actual,
        );
    }
    eprintln!("  ok: SHA256 matches published manifest");

    let current_exe = std::env::current_exe().context("locating current binary")?;
    // Stage into the directory that holds the current exe so the
    // final rename stays on the same filesystem and is atomic. Use
    // `.new` so a crash between extract and rename leaves a clearly
    // named leftover rather than a second `recon` on PATH. If the
    // install dir is read-only, `extract_binary` fails here with a
    // clear error rather than halfway through replacement.
    let staging_dir = current_exe
        .parent()
        .ok_or_else(|| anyhow!("current_exe has no parent directory"))?;
    let staged = staging_dir.join(format!("{}.new", binary_name()));
    // Stale `.new` from a previous interrupted update would make the
    // tarball unpacker fail (file already exists on some platforms).
    let _ = std::fs::remove_file(&staged);
    extract_binary(&archive_bytes, &staged)
        .context("extracting the new binary from the archive")?;

    replace_running(&staged, &current_exe)?;
    eprintln!("Updated recon: {} -> {}", current, latest);
    eprintln!(
        "Restart any running MCP/IDE session to use the new binary. \
         If this update changes indexing behavior, run `recon reindex` \
         in existing repos after restart."
    );
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn sha256_matches_manifest_line() {
        let manifest = "\
aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa  recon-x86_64-apple-darwin.tar.gz
bbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbb  recon-x86_64-unknown-linux-gnu.tar.gz
";
        assert_eq!(
            sha256_for(manifest, "recon-x86_64-unknown-linux-gnu.tar.gz"),
            Some("bbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbb".to_string())
        );
        assert_eq!(sha256_for(manifest, "nonexistent.tar.gz"), None);
    }

    #[test]
    fn parse_latest_json_trims_v_prefix() {
        // Exercise the tolerant parser directly by round-tripping
        // what release.yml actually writes.
        let body = r#"{"version":"v0.1.2"}"#;
        let raw = body
            .split("\"version\"")
            .nth(1)
            .and_then(|s| s.split('"').nth(1))
            .unwrap();
        assert_eq!(raw.trim_start_matches('v'), "0.1.2");
    }

    #[test]
    fn sha256_of_empty_is_known_constant() {
        // Sanity-check our sha256_hex wrapper matches the canonical
        // SHA-256("") value so a future dependency swap can't silently
        // regress the hashing.
        assert_eq!(
            sha256_hex(b""),
            "e3b0c44298fc1c149afbf4c8996fb92427ae41e4649b934ca495991b7852b855"
        );
    }
}
