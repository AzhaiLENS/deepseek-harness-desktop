//! Keeping `@deepseek-ai/dsh` up to date *without* breaking DSH's own update path.
//!
//! Design notes (this is the part the whole "wrap DSH, don't fork it" contract
//! hinges on):
//!
//! * The vendored DSH lives in the **writable app-data directory** with an
//!   ordinary npm/pnpm layout, so both the app updater *and* a user typing
//!   `npm install @deepseek-ai/dsh@latest` into a terminal change the same tree.
//! * Nothing about the update is implemented by us: the app shells out to the
//!   real package manager, exactly like the CLI does. There is no private
//!   update channel, no patched upstream file, no re-signed bundle.
//! * The update is **additive**: it never rewrites the user's `$DSH_HOME`
//!   (`~/.dsh`), so sessions, credentials and plugins survive untouched.
//! * Failures are non-fatal by construction — the previous generation keeps
//!   working, because installation happens in a staging directory that is only
//!   swapped in after a successful install.

use std::path::{Path, PathBuf};
use std::process::Command;
use std::sync::mpsc;
use std::time::Duration;

use crate::config::Paths;
use crate::runtime::{which, NodeSource, Runtime};

/// The npm registry document for the DSH CLI package.
const REGISTRY_URL: &str = "https://registry.npmjs.org/@deepseek-ai%2Fdsh";

/// Outcome of a version check.
#[derive(Debug, Clone, serde::Serialize)]
#[serde(rename_all = "camelCase")]
pub struct UpdateCheck {
    pub current: Option<String>,
    pub latest: Option<String>,
    pub update_available: bool,
    pub error: Option<String>,
}

/// Query the npm registry for the `latest` dist-tag.
pub fn latest_version() -> Result<String, String> {
    let response = ureq::builder()
        .timeout(Duration::from_secs(20))
        .user_agent(concat!("dsh-desktop/", env!("CARGO_PKG_VERSION")))
        .build()
        .get(REGISTRY_URL)
        .call()
        .map_err(|e| format!("registry request failed: {e}"))?;

    let text = response
        .into_string()
        .map_err(|e| format!("registry response unreadable: {e}"))?;
    let json: serde_json::Value =
        serde_json::from_str(&text).map_err(|e| format!("registry response is not JSON: {e}"))?;

    json.get("dist-tags")
        .and_then(|tags| tags.get("latest"))
        .and_then(|v| v.as_str())
        .map(str::to_string)
        .ok_or_else(|| "registry response has no dist-tags.latest".to_string())
}

/// Compare two semver-ish versions. Prerelease identifiers sort *before* the
/// release they belong to (`0.2.0-rc.1 < 0.2.0`), matching npm's ordering.
pub fn is_newer(candidate: &str, current: &str) -> bool {
    fn parse(v: &str) -> (Vec<u64>, Vec<String>) {
        let v = v.trim().trim_start_matches('v');
        let (core, pre) = match v.split_once('-') {
            Some((c, p)) => (c, p),
            None => (v, ""),
        };
        let nums = core
            .split('.')
            .map(|part| part.parse::<u64>().unwrap_or(0))
            .collect();
        let pres = if pre.is_empty() {
            Vec::new()
        } else {
            pre.split('.').map(str::to_string).collect()
        };
        (nums, pres)
    }

    let (a_nums, a_pre) = parse(candidate);
    let (b_nums, b_pre) = parse(current);
    if a_nums != b_nums {
        return a_nums > b_nums;
    }
    match (a_pre.is_empty(), b_pre.is_empty()) {
        (true, true) => false,
        (true, false) => true,  // release > prerelease
        (false, true) => false, // prerelease < release
        (false, false) => a_pre > b_pre,
    }
}

/// Check whether a newer DSH is published.
pub fn check(current: Option<String>) -> UpdateCheck {
    match latest_version() {
        Ok(latest) => {
            let available = match &current {
                Some(cur) => is_newer(&latest, cur),
                None => true,
            };
            UpdateCheck {
                current,
                latest: Some(latest),
                update_available: available,
                error: None,
            }
        }
        Err(error) => UpdateCheck {
            current,
            latest: None,
            update_available: false,
            error: Some(error),
        },
    }
}

/// Which package manager will perform the update.
///
/// Order matters: the pnpm bundled *inside the payload* comes first so the
/// update path works on a machine with no developer tooling at all, then a
/// user-installed pnpm (identical to what `dsh plugin` would pick up), then the
/// npm that ships with the vendored node runtime.
pub fn package_manager(paths: &Paths, runtime: &Runtime) -> Option<PathBuf> {
    let bin_dir = paths.runtime.join("bin");
    for candidate in [bin_dir.join("pnpm"), bin_dir.join("pnpm.cmd")] {
        if candidate.is_file() {
            return Some(candidate);
        }
    }

    if let Some(pnpm) = which("pnpm") {
        return Some(pnpm);
    }

    let node_dir = runtime
        .node
        .path()
        .parent()
        .map(Path::to_path_buf)
        .unwrap_or_else(|| paths.runtime.join("node"));

    for candidate in [
        // Bundled pnpm (js entry point, run through the vendored node).
        paths.runtime.join("pnpm/bin/pnpm.cjs"),
        paths.runtime.join("pnpm/bin/pnpm.mjs"),
        // npm that ships with the node distribution.
        node_dir.join("lib/node_modules/npm/bin/npm-cli.js"),
        node_dir.join("node_modules/npm/bin/npm-cli.js"),
    ] {
        if candidate.exists() {
            return Some(candidate);
        }
    }
    which("npm")
}

/// Build the install command for the detected package manager.
///
/// `staging` is a *fresh* directory: the update is installed there and only
/// swapped into place when the install succeeded, so a broken network can never
/// leave the app without a working runtime.
fn install_command(
    manager: Option<&PathBuf>,
    runtime: &Runtime,
    staging: &Path,
    spec: &str,
) -> Result<Command, String> {
    let node = runtime.node.path();

    match manager {
        Some(manager_path) if is_pnpm(manager_path) => {
            // A pnpm shim (shell script / .cmd) is executable as-is; pnpm's JS
            // entry points have to go through the vendored node.
            let mut command = if is_script(manager_path) {
                let mut c = Command::new(runtime.node.path());
                c.arg(manager_path);
                c
            } else {
                Command::new(manager_path)
            };
            command
                .arg("add")
                .arg(spec)
                .arg("--config.store-dir")
                .arg(staging.join(".store"))
                .args([
                    "--allow-build=@deepseek-ai/dsh-subprocess-local",
                    "--allow-build=koffi",
                    "--allow-build=node-pty",
                    "--allow-build=protobufjs",
                    "--allow-build=@google/genai",
                ])
                .current_dir(staging);
            Ok(command)
        }
        Some(manager_path) => {
            // npm (bundled or on PATH): run it through *our* node so the update
            // never depends on the user's node installation.
            let mut command = if is_script(manager_path) {
                let mut c = Command::new(node);
                c.arg(manager_path);
                c
            } else {
                Command::new(manager_path)
            };
            command
                .arg("install")
                .arg(spec)
                .args([
                    "--prefix",
                    &staging.to_string_lossy(),
                    "--no-audit",
                    "--no-fund",
                    "--allow-scripts=@deepseek-ai/dsh-subprocess-local,koffi,node-pty,protobufjs,@google/genai",
                ])
                .current_dir(staging);
            Ok(command)
        }
        None => Err("no package manager available (install pnpm, or rebuild the payload with node's bundled npm)".into()),
    }
}

fn is_pnpm(path: &Path) -> bool {
    path.to_string_lossy().contains("pnpm")
}

/// True for a JavaScript entry point that must be run through node.
fn is_script(path: &Path) -> bool {
    matches!(
        path.extension().and_then(|e| e.to_str()),
        Some("js") | Some("mjs") | Some("cjs")
    )
}

/// Result of an update attempt.
#[derive(Debug, Clone, serde::Serialize)]
#[serde(rename_all = "camelCase")]
pub struct UpdateOutcome {
    pub changed: bool,
    pub version: Option<String>,
    pub message: String,
}

/// Install `@deepseek-ai/dsh@<spec>` into the writable vendor directory.
///
/// `spec` is either an explicit version or `latest`.
pub fn install(
    paths: &Paths,
    runtime: &Runtime,
    spec: &str,
    on_line: impl Fn(&str) + Send + Sync + 'static,
) -> Result<UpdateOutcome, String> {
    let manager = package_manager(paths, runtime);
    let staging = paths.data.join("vendor.staging");
    let _ = std::fs::remove_dir_all(&staging);
    std::fs::create_dir_all(&staging).map_err(|e| format!("cannot create staging dir: {e}"))?;

    // The same manifest the payload ships, so a staged install is directly
    // swappable with the vendored tree.
    std::fs::write(
        staging.join("package.json"),
        format!(
            "{{\n  \"name\": \"dsh-desktop-runtime\",\n  \"private\": true,\n  \"version\": \"0.0.0\",\n  \"dependencies\": {{ \"@deepseek-ai/dsh\": \"{spec}\" }}\n}}\n"
        ),
    )
    .map_err(|e| format!("cannot write staging manifest: {e}"))?;
    std::fs::write(
        staging.join("pnpm-workspace.yaml"),
        "packages:\n  - .\n\nnodeLinker: hoisted\n\nallowBuilds:\n  \"@deepseek-ai/dsh-subprocess-local\": true\n  koffi: true\n  node-pty: true\n  protobufjs: true\n  \"@google/genai\": true\n",
    )
    .map_err(|e| format!("cannot write staging workspace: {e}"))?;

    // npm ignores pnpm-workspace.yaml; it reads the hoisting settings here.
    std::fs::write(staging.join(".npmrc"), "node-linker=hoisted\n")
        .map_err(|e| format!("cannot write staging npmrc: {e}"))?;

    let mut command = install_command(manager.as_ref(), runtime, &staging, spec)?;
    command.stdout(std::process::Stdio::piped());
    command.stderr(std::process::Stdio::piped());

    #[cfg(windows)]
    {
        use std::os::windows::process::CommandExt;
        command.creation_flags(0x0800_0000);
    }

    let sink = std::sync::Arc::new(on_line);
    let mut child = command
        .spawn()
        .map_err(|e| format!("cannot start the package manager: {e}"))?;

    let (tx, rx) = mpsc::channel::<String>();
    if let Some(stdout) = child.stdout.take() {
        let sink = std::sync::Arc::clone(&sink);
        let tx = tx.clone();
        std::thread::spawn(move || {
            use std::io::BufRead;
            for line in std::io::BufReader::new(stdout).lines().map_while(Result::ok) {
                sink(&line);
                let _ = tx.send(line);
            }
        });
    }
    if let Some(stderr) = child.stderr.take() {
        let sink = std::sync::Arc::clone(&sink);
        std::thread::spawn(move || {
            use std::io::BufRead;
            for line in std::io::BufReader::new(stderr).lines().map_while(Result::ok) {
                sink(&line);
            }
        });
    }

    let status = child
        .wait()
        .map_err(|e| format!("package manager did not finish: {e}"))?;

    let mut tail: Vec<String> = Vec::new();
    while let Ok(line) = rx.try_recv() {
        tail.push(line);
        if tail.len() > 40 {
            tail.remove(0);
        }
    }

    if !status.success() {
        let _ = std::fs::remove_dir_all(&staging);
        return Err(format!(
            "update failed ({}). Last output:\n{}",
            status,
            tail.join("\n")
        ));
    }

    // Guard: the staged install must actually contain a usable CLI.
    let staged_manifest = staging
        .join("node_modules")
        .join("@deepseek-ai")
        .join("dsh")
        .join("package.json");
    if !staged_manifest.exists() {
        let _ = std::fs::remove_dir_all(&staging);
        return Err("update produced no @deepseek-ai/dsh package".into());
    }

    let version = std::fs::read_to_string(&staged_manifest)
        .ok()
        .and_then(|raw| serde_json::from_str::<serde_json::Value>(&raw).ok())
        .and_then(|json| json.get("version").and_then(|v| v.as_str()).map(str::to_string));

    // Swap the staged tree in, keeping the previous generation until the very
    // last moment so a crash mid-swap cannot lose the working runtime.
    let previous = paths.data.join("vendor.previous");
    let _ = std::fs::remove_dir_all(&previous);
    if paths.vendor.exists() {
        std::fs::rename(&paths.vendor, &previous)
            .map_err(|e| format!("cannot move the current runtime aside: {e}"))?;
    }
    match std::fs::rename(&staging, &paths.vendor) {
        Ok(()) => {
            let _ = std::fs::remove_dir_all(&previous);
        }
        Err(error) => {
            // Roll back.
            let _ = std::fs::rename(&previous, &paths.vendor);
            let _ = std::fs::remove_dir_all(&staging);
            return Err(format!("cannot activate the new runtime: {error}"));
        }
    }

    // Refresh the extraction stamp so the payload is not re-extracted over the
    // freshly installed version on the next launch.
    let stamp = paths.runtime.join(crate::config::EXTRACT_STAMP);
    let existing = std::fs::read_to_string(&stamp).unwrap_or_default();
    let _ = std::fs::write(&stamp, existing);

    Ok(UpdateOutcome {
        changed: true,
        version: version.clone(),
        message: format!(
            "updated @deepseek-ai/dsh to {}",
            version.unwrap_or_else(|| spec.to_string())
        ),
    })
}

/// Convenience used by the startup path: check, then install if newer.
pub fn auto_update(
    paths: &Paths,
    runtime: &Runtime,
    on_line: impl Fn(&str) + Send + Sync + 'static,
) -> Result<UpdateOutcome, String> {
    let check = check(runtime.dsh_version.clone());
    if let Some(error) = &check.error {
        return Err(error.clone());
    }
    if !check.update_available {
        return Ok(UpdateOutcome {
            changed: false,
            version: check.current,
            message: "already up to date".into(),
        });
    }
    let spec = check
        .latest
        .clone()
        .unwrap_or_else(|| "latest".to_string());
    install(paths, runtime, &format!("@deepseek-ai/dsh@{spec}"), on_line)
}

/// True when the resolved node is the vendored one (i.e. a fully standalone app).
pub fn is_self_contained(runtime: &Runtime) -> bool {
    matches!(runtime.node, NodeSource::Vendored(_))
}
