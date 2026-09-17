//! Path, environment and persisted-state resolution for DeepSeek Harness Desktop.
//!
//! Everything the application *writes* lives under a single per-user data
//! directory so the installed application bundle itself stays immutable (a
//! requirement on macOS, where the `.app` is code-signed and read-only, and on
//! Windows, where `Program Files` is not writable for a standard user).

use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};

/// Directory (inside the Tauri app-data dir) that holds the writable runtime.
pub const RUNTIME_DIR: &str = "runtime";
/// Directory (inside the Tauri app-data dir) that holds the vendored DSH tree.
pub const VENDOR_DIR: &str = "vendor";
/// Marker written after a successful extraction of a payload generation.
pub const EXTRACT_STAMP: &str = ".extracted";

/// Resolved application paths.
#[derive(Debug, Clone)]
pub struct Paths {
    /// `<app-data>` — writable, stable across upgrades of the shell app.
    pub data: PathBuf,
    /// `<app-data>/runtime` — the extracted node runtime (`runtime/node/node`).
    pub runtime: PathBuf,
    /// `<app-data>/vendor` — the npm/pnpm-compatible DSH installation.
    pub vendor: PathBuf,
    /// `<resource>/payload` — shipped, read-only payload (dev override wins).
    pub payload: PathBuf,
}

impl Paths {
    /// Resolve from the Tauri app handle.
    pub fn resolve(app: &tauri::AppHandle) -> Result<Self, String> {
        use tauri::Manager;

        // Explicit override first: it makes the app testable on a machine where
        // the real profile directory must not be touched (CI smoke tests,
        // portable installs on a USB stick, ...).
        let data = match std::env::var_os("DSH_DESKTOP_DATA") {
            Some(dir) if !dir.is_empty() => PathBuf::from(dir),
            _ => app
                .path()
                .app_data_dir()
                .map_err(|e| format!("cannot resolve app data dir: {e}"))?,
        };
        let payload = payload_dir(app);

        Ok(Self::assemble(data, payload))
    }

    /// Resolve without a Tauri app handle (`--self-check`, CI).
    ///
    /// Requires `DSH_DESKTOP_DATA`; the payload comes from `DSH_DESKTOP_PAYLOAD`
    /// or, failing that, a `payload/` directory next to the executable.
    pub fn resolve_headless() -> Result<Self, String> {
        let data = std::env::var_os("DSH_DESKTOP_DATA")
            .filter(|d| !d.is_empty())
            .map(PathBuf::from)
            .ok_or_else(|| {
                "DSH_DESKTOP_DATA must be set for a headless run (it selects the writable \
                 runtime directory; the real user profile is never used)"
                    .to_string()
            })?;

        let payload = match std::env::var_os("DSH_DESKTOP_PAYLOAD") {
            Some(dir) if !dir.is_empty() => PathBuf::from(dir),
            _ => {
                let mut found = PathBuf::from("payload");
                if let Ok(exe) = std::env::current_exe() {
                    // Two layouts to satisfy: the build tree (`<root>/payload`)
                    // and a real bundle, where the payload sits in the bundle's
                    // resource directory — macOS `Contents/Resources/payload`,
                    // Linux AppImage `usr/lib/<app>/payload`, Windows
                    // `resources/payload` next to the executable.
                    for ancestor in exe.ancestors().skip(1).take(6) {
                        for candidate in [
                            ancestor.join("payload"),
                            ancestor.join("Resources/payload"),
                            ancestor.join("resources/payload"),
                        ] {
                            if candidate.exists() {
                                found = candidate;
                                break;
                            }
                        }
                        if found != PathBuf::from("payload") {
                            break;
                        }
                    }
                }
                found
            }
        };

        Ok(Self::assemble(data, payload))
    }

    fn assemble(data: PathBuf, payload: PathBuf) -> Self {
        Self {
            runtime: data.join(RUNTIME_DIR),
            vendor: data.join(VENDOR_DIR),
            data,
            payload,
        }
    }

    /// Destination of the vendored DSH CLI entry point.
    pub fn dsh_bin(&self) -> PathBuf {
        self.vendor
            .join("node_modules")
            .join("@deepseek-ai")
            .join("dsh")
            .join("lib")
            .join("bin.js")
    }

    /// Per-platform node executable name.
    pub fn node_exe_name() -> &'static str {
        if cfg!(windows) {
            "node.exe"
        } else {
            "node"
        }
    }

    /// The vendored node executable, if it was extracted.
    pub fn vendored_node(&self) -> PathBuf {
        self.runtime.join("node").join(Self::node_exe_name())
    }

    /// True when the payload still has to be materialised into the data dir.
    pub fn needs_extract(&self, generation: &str) -> bool {
        let stamp = self.runtime.join(EXTRACT_STAMP);
        match std::fs::read_to_string(&stamp) {
            Ok(found) => found.trim() != generation || !self.dsh_bin().exists(),
            Err(_) => true,
        }
    }
}

/// Locate the shipped payload directory.
///
/// Order: an explicit `DSH_DESKTOP_PAYLOAD` override (used by `dev.sh` and by
/// the CI smoke tests), then the bundled resource dir, then the classic
/// `resources/` directory next to the executable (Linux AppImage / deb).
fn payload_dir(app: &tauri::AppHandle) -> PathBuf {
    use tauri::Manager;

    if let Ok(dir) = std::env::var("DSH_DESKTOP_PAYLOAD") {
        if !dir.trim().is_empty() {
            return PathBuf::from(dir);
        }
    }

    if let Ok(resource) = app.path().resource_dir() {
        let candidate = resource.join("payload");
        if candidate.exists() {
            return candidate;
        }
        if resource.join("manifest.json").exists() {
            return resource;
        }
    }

    if let Ok(exe) = std::env::current_exe() {
        for ancestor in exe.ancestors().skip(1).take(4) {
            let candidate = ancestor.join("payload");
            if candidate.exists() {
                return candidate;
            }
        }
    }

    PathBuf::from("payload")
}

/// User-tunable settings, persisted as JSON under the data dir.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(default, rename_all = "camelCase")]
pub struct Settings {
    /// Boot this DSH profile (defaults to the shipped `web` profile).
    pub profile: String,
    /// Fixed port; `0` lets DSH pick a free one.
    pub port: u16,
    /// Bind host for the DSH web server (kept on loopback for safety).
    pub host: String,
    /// Check the npm registry for a newer `@deepseek-ai/dsh` on startup.
    pub auto_update: bool,
    /// Allow a cold-start update attempt before the first boot.
    pub update_on_first_boot: bool,
    /// Share `$HOME/.dsh` (the CLI's home) instead of the app-private home.
    ///
    /// Defaults to `false`: the app is a *standalone* install — its own DSH
    /// runtime, its own Harness home (`<data>/home`), its own sessions and
    /// credentials. Sharing the CLI home would couple this app to whatever
    /// other DSH instances exist on the machine (a stale `node_modules.lock`
    /// orphan from one crashed process blocks every new instance that shares
    /// the same home). Flip to `true` only if sessions/credentials should be
    /// shared with the `dsh` CLI — and accept the cross-instance coupling.
    pub share_dsh_home: bool,
    /// Extra arguments handed to the DSH CLI verbatim.
    pub extra_args: Vec<String>,
}

impl Default for Settings {
    fn default() -> Self {
        Self {
            profile: "web".into(),
            port: 0,
            host: "127.0.0.1".into(),
            auto_update: true,
            update_on_first_boot: false,
            share_dsh_home: false,
            extra_args: Vec::new(),
        }
    }
}

impl Settings {
    /// Load `settings.json` from the data dir, falling back to defaults.
    pub fn load(data: &Path) -> Self {
        let file = data.join("settings.json");
        match std::fs::read_to_string(&file) {
            Ok(raw) => serde_json::from_str(&raw).unwrap_or_default(),
            Err(_) => Self::default(),
        }
    }

    /// The Harness home (`$DSH_HOME`) this app will boot with.
    ///
    /// Default is the app-private `<data>/home`: a fully independent install
    /// that shares nothing with other DSH instances on the machine. Only an
    /// explicit `shareDshHome: true` in `settings.json` opts into the CLI's
    /// `~/.dsh` (shared sessions/credentials, shared locks).
    pub fn dsh_home(&self, data: &Path) -> PathBuf {
        if self.share_dsh_home {
            if let Some(home) = std::env::var_os("HOME").or_else(|| std::env::var_os("USERPROFILE"))
            {
                return PathBuf::from(home).join(".dsh");
            }
        }
        data.join("home")
    }
}
