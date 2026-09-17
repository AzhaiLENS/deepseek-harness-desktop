//! First-run materialisation of the vendored runtime payload.
//!
//! The application bundle ships a single archive holding the Node.js runtime
//! and the full `@deepseek-ai/dsh` dependency closure. On first launch (and
//! whenever the shipped payload changes) that archive is unpacked into the
//! writable app-data directory, because:
//!
//! * macOS/Windows install locations are read-only for the running user;
//! * DSH's own auto-update (`npm install @deepseek-ai/dsh@latest`) has to be
//!   able to rewrite `node_modules` — impossible inside a signed bundle.

use std::fs::{self, File};
use std::io::{self, BufReader, Read};
use std::path::{Path, PathBuf};

use flate2::read::GzDecoder;
use sha2::{Digest, Sha256};

use crate::config::{Paths, EXTRACT_STAMP};

/// How the payload was found on this machine.
#[derive(Debug, Clone)]
pub enum PayloadSource {
    /// An already-extracted tree (`DSH_DESKTOP_PAYLOAD` pointing at a directory
    /// containing `vendor/` and `runtime/`).
    LegacyDirectory(PathBuf),
    /// A `.tar.zst` / `.tar.gz` archive shipped as a resource.
    Archive(PathBuf),
    /// Nothing usable — the app must tell the user to run the vendoring script.
    Missing(Vec<PathBuf>),
}

/// Locate the payload archive (or an already-extracted directory).
pub fn locate(payload: &Path) -> PayloadSource {
    let mut probed: Vec<PathBuf> = Vec::new();

    if payload.is_dir() {
        // An extracted tree (developer override): `<dir>/vendor` + `<dir>/runtime`.
        if payload.join("vendor").join("node_modules").exists() {
            return PayloadSource::LegacyDirectory(payload.to_path_buf());
        }
        // Otherwise treat it as an archive directory.
        if let Ok(entries) = fs::read_dir(payload) {
            let mut candidates: Vec<PathBuf> = entries
                .flatten()
                .map(|e| e.path())
                .filter(|p| {
                    let name = p.file_name().and_then(|n| n.to_str()).unwrap_or_default();
                    name.starts_with("dsh-payload-")
                        && (name.ends_with(".tar.zst") || name.ends_with(".tar.gz"))
                })
                .collect();
            // Prefer the archive matching the host platform, then any.
            candidates.sort();
            let host = host_tag();
            if let Some(matched) = candidates
                .iter()
                .find(|p| {
                    p.file_name()
                        .and_then(|n| n.to_str())
                        .map(|n| n.contains(&host))
                        .unwrap_or(false)
                })
                .cloned()
            {
                return PayloadSource::Archive(matched);
            }
            if let Some(first) = candidates.into_iter().next() {
                return PayloadSource::Archive(first);
            }
        }
        probed.push(payload.to_path_buf());
    } else {
        probed.push(payload.to_path_buf());
    }

    PayloadSource::Missing(probed)
}

fn host_tag() -> String {
    let arch = if cfg!(target_arch = "aarch64") {
        "arm64"
    } else {
        "x64"
    };
    let os = if cfg!(target_os = "macos") {
        "darwin"
    } else if cfg!(target_os = "windows") {
        "win32"
    } else {
        "linux"
    };
    format!("{os}-{arch}")
}

/// Identity of the payload generation, used as the extraction stamp.
///
/// Derived from the file size + mtime + name: cheap, and changes whenever the
/// shipped payload is rebuilt (a full hash of a 300 MB archive on every launch
/// would be wasteful).
pub fn generation_of(source: &PayloadSource) -> String {
    match source {
        PayloadSource::Archive(path) => {
            let meta = fs::metadata(path).ok();
            let len = meta.as_ref().map(|m| m.len()).unwrap_or(0);
            let mtime = meta
                .as_ref()
                .and_then(|m| m.modified().ok())
                .and_then(|t| t.duration_since(std::time::UNIX_EPOCH).ok())
                .map(|d| d.as_secs())
                .unwrap_or(0);
            let name = path
                .file_name()
                .and_then(|n| n.to_str())
                .unwrap_or("payload")
                .to_string();
            format!("{name}:{len}:{mtime}")
        }
        PayloadSource::LegacyDirectory(path) => {
            let version = read_local_dsh_version(path).unwrap_or_else(|| "unknown".into());
            format!("dir:{version}")
        }
        PayloadSource::Missing(_) => "missing".into(),
    }
}

/// Read the DSH version out of a materialised tree, when present.
pub fn read_local_dsh_version(vendor_root: &Path) -> Option<String> {
    let manifest = vendor_root
        .join("node_modules")
        .join("@deepseek-ai")
        .join("dsh")
        .join("package.json");
    let raw = fs::read_to_string(manifest).ok()?;
    let json: serde_json::Value = serde_json::from_str(&raw).ok()?;
    json.get("version")?.as_str().map(str::to_string)
}

/// Extract the payload into the writable data directory.
///
/// `progress(extracted_entries)` is called periodically so the splash screen can
/// show real progress instead of an indeterminate spinner.
pub fn materialise(
    paths: &Paths,
    source: &PayloadSource,
    generation: &str,
    mut progress: impl FnMut(u64),
) -> Result<(), String> {
    match source {
        PayloadSource::LegacyDirectory(dir) => {
            // Developer override: link/copy the prepared tree into place.
            copy_tree(dir, &paths.data)?;
            write_stamp(paths, generation)?;
            Ok(())
        }
        PayloadSource::Archive(archive) => {
            // Extract into a sibling staging dir and swap, so an interrupted
            // extraction can never leave a half-written runtime behind.
            let staging = paths.data.join(format!("{}.staging", crate::config::RUNTIME_DIR));
            let _ = fs::remove_dir_all(&staging);
            fs::create_dir_all(&staging).map_err(|e| format!("cannot create staging dir: {e}"))?;

            extract_archive(archive, &staging, &mut progress)?;

            // Replace the previous generation.
            for target in [&paths.runtime, &paths.vendor] {
                if target.exists() {
                    fs::remove_dir_all(target)
                        .map_err(|e| format!("cannot clear {}: {e}", target.display()))?;
                }
            }
            let staged_runtime = staging.join(crate::config::RUNTIME_DIR);
            let staged_vendor = staging.join(crate::config::VENDOR_DIR);
            if !staged_vendor.exists() {
                return Err(format!(
                    "payload archive has no `{}/` directory",
                    crate::config::VENDOR_DIR
                ));
            }
            rename_or_copy(&staged_runtime, &paths.runtime)?;
            rename_or_copy(&staged_vendor, &paths.vendor)?;
            let _ = fs::remove_dir_all(&staging);
            write_stamp(paths, generation)?;
            Ok(())
        }
        PayloadSource::Missing(probed) => Err(format!(
            "no DSH runtime payload found. Looked in: {}. Build one with `scripts/vendor-runtime.sh`.",
            probed
                .iter()
                .map(|p| p.display().to_string())
                .collect::<Vec<_>>()
                .join(", ")
        )),
    }
}

fn write_stamp(paths: &Paths, generation: &str) -> Result<(), String> {
    fs::create_dir_all(&paths.runtime).map_err(|e| format!("cannot create runtime dir: {e}"))?;
    fs::write(paths.runtime.join(EXTRACT_STAMP), generation)
        .map_err(|e| format!("cannot write extraction stamp: {e}"))?;
    Ok(())
}

fn rename_or_copy(from: &Path, to: &Path) -> Result<(), String> {
    if !from.exists() {
        return Ok(());
    }
    if let Some(parent) = to.parent() {
        fs::create_dir_all(parent).map_err(|e| format!("cannot create {}: {e}", parent.display()))?;
    }
    match fs::rename(from, to) {
        Ok(()) => Ok(()),
        Err(_) => copy_tree(from, to),
    }
}

fn copy_tree(from: &Path, to: &Path) -> Result<(), String> {
    fs::create_dir_all(to).map_err(|e| format!("cannot create {}: {e}", to.display()))?;
    for entry in fs::read_dir(from).map_err(|e| format!("cannot read {}: {e}", from.display()))? {
        let entry = entry.map_err(|e| e.to_string())?;
        let src = entry.path();
        let dst = to.join(entry.file_name());
        let meta = fs::symlink_metadata(&src).map_err(|e| e.to_string())?;
        if meta.is_dir() {
            copy_tree(&src, &dst)?;
        } else if meta.file_type().is_symlink() {
            let resolved = fs::canonicalize(&src).map_err(|e| e.to_string())?;
            if resolved.is_dir() {
                copy_tree(&resolved, &dst)?;
            } else {
                fs::copy(&resolved, &dst).map_err(|e| e.to_string())?;
            }
        } else {
            fs::copy(&src, &dst).map_err(|e| format!("copy {}: {e}", src.display()))?;
        }
    }
    Ok(())
}

/// Extract `.tar.zst` / `.tar.gz` (auto-detected by magic bytes).
fn extract_archive(
    archive: &Path,
    dest: &Path,
    progress: &mut impl FnMut(u64),
) -> Result<(), String> {
    let file = File::open(archive).map_err(|e| format!("cannot open {}: {e}", archive.display()))?;
    let mut reader = BufReader::with_capacity(1 << 20, file);

    let mut magic = [0u8; 4];
    let read = reader.read(&mut magic).map_err(|e| e.to_string())?;
    let header = &magic[..read];

    // Re-open: the tar reader needs the stream from the beginning.
    let file = File::open(archive).map_err(|e| format!("cannot open {}: {e}", archive.display()))?;
    let buffered = BufReader::with_capacity(1 << 20, file);

    let decoder: Box<dyn Read> = if header.starts_with(&[0x28, 0xB5, 0x2F, 0xFD]) {
        Box::new(zstd::stream::read::Decoder::new(buffered).map_err(|e| e.to_string())?)
    } else if header.starts_with(&[0x1F, 0x8B]) {
        Box::new(GzDecoder::new(buffered))
    } else {
        return Err(format!(
            "{} is neither zstd nor gzip compressed",
            archive.display()
        ));
    };

    let mut tar = tar::Archive::new(decoder);
    tar.set_preserve_permissions(true);

    let mut count: u64 = 0;
    for entry in tar.entries().map_err(|e| e.to_string())? {
        let mut entry = entry.map_err(|e| e.to_string())?;
        let path = entry.path().map_err(|e| e.to_string())?.into_owned();
        // `tar -C <stage> .` produces `./vendor/...`; strip the leading `./`.
        let relative: PathBuf = path.components().skip_while(|c| matches!(c, std::path::Component::CurDir)).collect();
        if relative.as_os_str().is_empty() {
            continue;
        }
        let out = dest.join(&relative);
        if let Some(parent) = out.parent() {
            fs::create_dir_all(parent).map_err(|e| format!("mkdir {}: {e}", parent.display()))?;
        }
        entry.unpack(&out).map_err(|e| format!("unpack {}: {e}", out.display()))?;
        count += 1;
        if count % 500 == 0 {
            progress(count);
        }
    }
    progress(count);
    Ok(())
}

/// Verify a payload archive against the sha256 recorded next to it.
///
/// The sidecar is written by `scripts/vendor-runtime.sh`. Checking it catches
/// the realistic packaging failure modes — a truncated copy into the app bundle,
/// a partial download of a release artifact — before the user hits a confusing
/// unpack error.
pub fn verify_archive(archive: &Path) -> Result<(), String> {
    // `dsh-payload-<platform>-<arch>.tar.zst` -> `dsh-payload-<platform>-<arch>.json`
    let stem = archive
        .file_name()
        .and_then(|n| n.to_str())
        .unwrap_or_default()
        .trim_end_matches(".tar.zst")
        .trim_end_matches(".tar.gz")
        .to_string();
    let sidecar = archive.with_file_name(format!("{stem}.json"));
    if !sidecar.exists() {
        return Ok(()); // nothing to verify against
    }

    let raw = fs::read_to_string(&sidecar).map_err(|e| format!("cannot read {}: {e}", sidecar.display()))?;
    let json: serde_json::Value =
        serde_json::from_str(&raw).map_err(|e| format!("{} is not valid JSON: {e}", sidecar.display()))?;
    let Some(expected) = json.get("sha256").and_then(|v| v.as_str()) else {
        return Ok(());
    };

    let actual = sha256_of(archive).map_err(|e| format!("cannot hash {}: {e}", archive.display()))?;
    if actual.eq_ignore_ascii_case(expected) {
        Ok(())
    } else {
        Err(format!(
            "payload integrity check failed for {}: expected {expected}, got {actual}",
            archive.display()
        ))
    }
}

/// Total entry count recorded in the sidecar (`"entries"`), when the vendoring
/// script provided one. This is what turns the extraction callback into real
/// percentage progress on the splash screen instead of a crawling guess.
pub fn entry_total(archive: &Path) -> Option<u64> {
    let stem = archive
        .file_name()
        .and_then(|n| n.to_str())
        .unwrap_or_default()
        .trim_end_matches(".tar.zst")
        .trim_end_matches(".tar.gz")
        .to_string();
    let sidecar = archive.with_file_name(format!("{stem}.json"));
    let raw = fs::read_to_string(sidecar).ok()?;
    let json: serde_json::Value = serde_json::from_str(&raw).ok()?;
    // A zero/absent count means "unknown" — the splash must not divide by it.
    json.get("entries").and_then(|v| v.as_u64()).filter(|n| *n > 0)
}

/// Convenience: SHA-256 of a file (used by the integrity check of a payload).
pub fn sha256_of(path: &Path) -> io::Result<String> {
    let file = File::open(path)?;
    let mut reader = BufReader::with_capacity(1 << 20, file);
    let mut hasher = Sha256::new();
    let mut buf = [0u8; 1 << 16];
    loop {
        let n = reader.read(&mut buf)?;
        if n == 0 {
            break;
        }
        hasher.update(&buf[..n]);
    }
    Ok(format!("{:x}", hasher.finalize()))
}
