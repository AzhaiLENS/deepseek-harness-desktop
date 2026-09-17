//! Locating the runtime and supervising the DSH server process.

use std::io::{BufRead, BufReader, Read};
use std::path::{Path, PathBuf};
use std::process::{Child, Command, Stdio};
use std::sync::mpsc;
use std::time::{Duration, Instant};

use crate::config::{Paths, Settings};

/// Where the node executable came from.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum NodeSource {
    /// The node runtime shipped inside the payload (fully self-contained).
    Vendored(PathBuf),
    /// A node found on `PATH` (fallback when the payload was built `--skip-node`).
    System(PathBuf),
}

impl NodeSource {
    pub fn path(&self) -> &Path {
        match self {
            NodeSource::Vendored(p) | NodeSource::System(p) => p,
        }
    }
    pub fn label(&self) -> &'static str {
        match self {
            NodeSource::Vendored(_) => "vendored",
            NodeSource::System(_) => "system",
        }
    }
}

/// The resolved runtime composition for this launch.
#[derive(Debug, Clone)]
pub struct Runtime {
    pub node: NodeSource,
    pub dsh_bin: PathBuf,
    pub dsh_version: Option<String>,
}

impl Runtime {
    /// Resolve node + the DSH entry point, preferring the vendored copies.
    pub fn resolve(paths: &Paths) -> Result<Self, String> {
        let vendored = paths.vendored_node();
        let node = if vendored.exists() {
            NodeSource::Vendored(vendored)
        } else {
            match which(&Paths::node_exe_name()) {
                Some(found) => NodeSource::System(found),
                None => {
                    return Err(format!(
                        "no node runtime available: {} is missing and `node` is not on PATH",
                        vendored.display()
                    ))
                }
            }
        };

        let dsh_bin = paths.dsh_bin();
        if !dsh_bin.exists() {
            return Err(format!(
                "the DSH entry point {} does not exist — the runtime payload is incomplete",
                dsh_bin.display()
            ));
        }

        let dsh_version = crate::payload::read_local_dsh_version(&paths.vendor);

        Ok(Self {
            node,
            dsh_bin,
            dsh_version,
        })
    }
}

/// `which`-style PATH lookup without pulling in an extra crate.
pub fn which(program: &str) -> Option<PathBuf> {
    let path = std::env::var_os("PATH")?;
    for dir in std::env::split_paths(&path) {
        let candidate = dir.join(program);
        if candidate.is_file() {
            return Some(candidate);
        }
        #[cfg(windows)]
        {
            let with_exe = dir.join(format!("{program}.cmd"));
            if with_exe.is_file() {
                return Some(with_exe);
            }
            let with_bat = dir.join(format!("{program}.bat"));
            if with_bat.is_file() {
                return Some(with_bat);
            }
        }
    }
    None
}

/// A running DSH server.
pub struct Server {
    child: Child,
    /// Authenticated loopback URL (carries the one-shot session token).
    pub url: String,
}

impl Server {
    /// Terminate the server and everything it spawned.
    pub fn shutdown(&mut self) {
        let _ = self.child.kill();
        let _ = self.child.wait();
    }
}

/// Build the environment DSH runs with.
///
/// The important part for "must not break DSH's auto-update" is that the
/// vendored node's directory goes on `PATH`: `dsh plugin` forwards to pnpm, and
/// the in-app updater resolves npm/pnpm the same way, so the bundled runtime is
/// preferred over anything the user happens to have installed.
pub fn build_env(paths: &Paths, runtime: &Runtime, dsh_home: &Path) -> Vec<(String, String)> {
    let mut env: Vec<(String, String)> = std::env::vars().collect();

    // Never leak a foreign node contract into the child.
    env.retain(|(k, _)| {
        !matches!(
            k.as_str(),
            "NODE_OPTIONS" | "NODE_PATH" | "npm_config_prefix" | "npm_config_cache" | "ELECTRON_RUN_AS_NODE"
        )
    });

    let node_dir = runtime
        .node
        .path()
        .parent()
        .map(Path::to_path_buf)
        .unwrap_or_else(|| paths.runtime.join("node"));
    let bin_dir = paths.runtime.join("bin");

    let existing_path = std::env::var_os("PATH").unwrap_or_default();
    // `runtime/bin` first: it holds the bundled `pnpm` shim, which is what makes
    // `dsh plugin add` and the in-app updater work with no global tooling.
    let mut entries: Vec<PathBuf> = Vec::new();
    if bin_dir.is_dir() {
        entries.push(bin_dir);
    }
    entries.push(node_dir.clone());
    entries.extend(std::env::split_paths(&existing_path));
    if let Ok(joined) = std::env::join_paths(entries) {
        env.push(("PATH".into(), joined.to_string_lossy().into_owned()));
    }

    env.push(("DSH_HOME".into(), dsh_home.to_string_lossy().into_owned()));
    env.push((
        "DSH_DESKTOP".into(),
        env!("CARGO_PKG_VERSION").to_string(),
    ));
    // The app owns its own node runtime; keep npm quiet and reproducible.
    env.push(("npm_config_update_notifier".into(), "false".into()));
    env.push(("npm_config_fund".into(), "false".into()));
    env.push(("npm_config_audit".into(), "false".into()));

    env
}

/// Spawn `dsh --profile <p> --host <h> --port <n> --no-open` and wait for the
/// authenticated URL line.
///
/// `--no-open` is essential: DSH would otherwise open the user's default
/// browser, while the desktop app embeds the UI itself.
pub fn start(
    paths: &Paths,
    runtime: &Runtime,
    settings: &Settings,
    dsh_home: &Path,
    on_line: impl Fn(&str) + Send + Sync + 'static,
) -> Result<Server, String> {
    let host = if settings.host.trim().is_empty() {
        "127.0.0.1".to_string()
    } else {
        settings.host.clone()
    };

    let mut args: Vec<String> = vec![
        runtime.dsh_bin.to_string_lossy().into_owned(),
        "--profile".into(),
        settings.profile.clone(),
        "--host".into(),
        host,
        "--port".into(),
        settings.port.to_string(),
        "--no-open".into(),
    ];
    args.extend(settings.extra_args.iter().cloned());

    let mut command = Command::new(runtime.node.path());
    command
        .args(&args)
        .env_clear()
        .envs(build_env(paths, runtime, dsh_home))
        .current_dir(std::env::var_os("HOME").map_or_else(|| paths.data.clone(), PathBuf::from))
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped());

    #[cfg(windows)]
    {
        use std::os::windows::process::CommandExt;
        // CREATE_NO_WINDOW: never flash a console window for the server.
        command.creation_flags(0x0800_0000);
    }

    let mut child = command
        .spawn()
        .map_err(|e| format!("cannot start DSH ({}): {e}", runtime.node.path().display()))?;

    let stdout = child
        .stdout
        .take()
        .ok_or_else(|| "cannot capture DSH stdout".to_string())?;
    let stderr = child
        .stderr
        .take()
        .ok_or_else(|| "cannot capture DSH stderr".to_string())?;

    // Both pipes are drained on their own threads so a chatty DSH can never
    // block on a full pipe buffer.
    let sink = std::sync::Arc::new(on_line);
    let stderr_sink = std::sync::Arc::clone(&sink);
    std::thread::spawn(move || {
        let reader = BufReader::new(stderr);
        for line in reader.lines().map_while(Result::ok) {
            stderr_sink(&line);
        }
    });

    let (tx, rx) = mpsc::channel::<String>();
    let stdout_sink = std::sync::Arc::clone(&sink);
    std::thread::spawn(move || {
        let reader = BufReader::new(stdout);
        for line in reader.lines().map_while(Result::ok) {
            stdout_sink(&line);
            if let Some(url) = parse_web_url(&line) {
                let _ = tx.send(url);
            }
        }
    });

    let deadline = Instant::now() + Duration::from_secs(180);
    loop {
        if let Ok(url) = rx.recv_timeout(Duration::from_millis(250)) {
            return Ok(Server { child, url });
        }
        if let Ok(Some(status)) = child.try_wait() {
            return Err(format!(
                "DSH exited before serving the UI (status {status}). See the log for details."
            ));
        }
        if Instant::now() > deadline {
            let _ = child.kill();
            return Err("timed out waiting for the DSH web server to report its URL".into());
        }
    }
}

/// Extract the authenticated loopback URL from a `dsh web: <url>` line.
pub fn parse_web_url(line: &str) -> Option<String> {
    let idx = line.find("http://")?;
    let candidate = line[idx..].trim();
    let end = candidate
        .find(|c: char| c.is_whitespace())
        .unwrap_or(candidate.len());
    let url = &candidate[..end];
    if url.starts_with("http://127.0.0.1") || url.starts_with("http://localhost") {
        Some(url.to_string())
    } else {
        None
    }
}

/// Read at most `limit` bytes from a reader.
///
/// Used to capture a child process's diagnostic output (npm/pnpm phase banners)
/// without risking an unbounded read.
pub fn read_capped(mut reader: impl Read, limit: usize) -> String {
    let mut buf = vec![0u8; limit];
    match reader.read(&mut buf) {
        Ok(n) => String::from_utf8_lossy(&buf[..n]).into_owned(),
        Err(_) => String::new(),
    }
}

/// `node --version` for the resolved runtime, as shown in the diagnostics panel.
pub fn probe_node_version(node: &Path) -> String {
    let mut command = Command::new(node);
    command.arg("--version");
    #[cfg(windows)]
    {
        use std::os::windows::process::CommandExt;
        command.creation_flags(0x0800_0000);
    }
    command
        .output()
        .ok()
        .map(|out| read_capped(&out.stdout[..], 64).trim().to_string())
        .filter(|v| !v.is_empty())
        .unwrap_or_else(|| "unknown".into())
}
