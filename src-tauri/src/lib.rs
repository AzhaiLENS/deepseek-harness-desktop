//! DeepSeek Harness Desktop — DeepSeek Harness as a standalone desktop application.
//!
//! # What this shell is (and is not)
//!
//! The app is a *wrapper*, not a fork. It ships three things and nothing else:
//!
//! 1. a stock Node.js runtime,
//! 2. a verbatim copy of the published `@deepseek-ai/dsh` dependency closure,
//! 3. a thin Tauri shell that extracts them into a writable directory, starts
//!    `dsh --profile web --no-open`, and points a native webview at the URL the
//!    server prints on stdout.
//!
//! Because the DSH tree lives in a writable directory with an ordinary
//! npm/pnpm layout, DSH keeps full ownership of its own runtime: sessions,
//! credentials, plugins and — crucially — the upstream auto-update path all
//! behave exactly as they do for the CLI.

mod config;
mod geometry;
mod payload;
mod runtime;
mod update;

use std::io::{Read, Write};
use std::net::TcpStream;
use std::sync::Mutex;

use serde::Serialize;
use tauri::{Manager, WebviewUrl, WebviewWindowBuilder};

use config::{Paths, Settings};
use runtime::{Runtime, Server};

/// Tail of the DSH log kept in memory for the diagnostics panel.
const LOG_TAIL_LINES: usize = 400;

/// Shared application state.
#[derive(Default)]
struct AppState {
    paths: Option<Paths>,
    settings: Option<Settings>,
    runtime: Option<Runtime>,
    server: Option<Server>,
    log: Vec<String>,
    phase: String,
    node_version: Option<String>,
    /// Persistent copy of the boot log: a GUI process has no visible stdout, so
    /// `<data>/boot.log` is the only way a user (or developer) can see why a
    /// launch failed.
    log_file: Option<std::fs::File>,
}

/// Status snapshot handed to the webview (`get_status` command).
#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
struct Status {
    phase: String,
    url: Option<String>,
    dsh_version: Option<String>,
    node_version: Option<String>,
    node_source: Option<String>,
    /// True when the app runs entirely on its own bundled runtime.
    self_contained: bool,
    /// The package manager the in-app updater would use.
    package_manager: Option<String>,
    data_dir: Option<String>,
    dsh_home: Option<String>,
    log: Vec<String>,
}

/// Progress payload pushed to the splash screen.
#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
struct BootProgress {
    phase: String,
    detail: String,
    ratio: Option<f64>,
}

/// Entry point used by both `main.rs` and the mobile targets.
pub fn run() {
    // `--self-check` (or DSH_DESKTOP_SELF_CHECK=1) runs the whole bootstrap
    // headlessly — extract, resolve, start DSH, fetch the UI — prints a report
    // and exits. It exercises the real code path with no windows involved, which
    // is what makes it usable from a terminal and from CI.
    if self_check_requested() {
        std::process::exit(self_check());
    }
    // 信号桥只存在于 Unix：macOS/Linux 用它把 TERM 变成优雅退出（回收服务进程）；
    // Windows 无 SIGHUP，且进程终止语义不同，直接跳过。
    #[cfg(unix)]
    install_signal_bridge();

    let app = tauri::Builder::default()
        // 这里**故意不挂** tauri-plugin-window-state：它把窗口尺寸按物理像素存盘、
        // 再按物理像素还原，于是在 2x 高分屏上会把窗口缩成一半逻辑点（界面被压扁、
        // 启动页底部被裁）。几何持久化改由 geometry.rs 用逻辑点实现。
        .invoke_handler(tauri::generate_handler![
            get_status,
            get_log,
            check_for_update,
            install_update,
            restart,
            open_data_dir,
        ])
        .setup(|app| {
            let handle = app.handle().clone();

            // 单实例守卫：同一份数据目录跑两个实例会互相踩（会话索引、设置、
            // 凭据都是「读-改-写」）。第二个实例把已有窗口带到前台后立刻退出。
            claim_single_instance(&handle);

            // The splash window is declared in tauri.conf.json and tears itself
            // down once the DSH server is up.
            let phase_handle = handle.clone();
            std::thread::spawn(move || {
                // Let the splash window paint before the first progress update.
                std::thread::sleep(std::time::Duration::from_millis(200));
                if let Err(error) = bootstrap(&phase_handle) {
                    emit(&phase_handle, "error", &error, None);
                    if let Some(state) = phase_handle.try_state::<Mutex<AppState>>() {
                        if let Ok(mut state) = state.lock() {
                            state.phase = "error".into();
                            push_log(&mut state, &format!("fatal: {error}"));
                        }
                    }
                }
            });

            Ok(())
        })
        .on_window_event(|window, event| {
            match event {
                // 窗口几何：尺寸/位置变化后写盘（逻辑点，见 geometry.rs）。
                // Resized/Moved 会连发，节流器把它们合并成一次写入。
                tauri::WindowEvent::Resized(_) | tauri::WindowEvent::Moved(_)
                    if window.label() == "main" =>
                {
                    if let Some(target) = window.app_handle().get_webview_window("main") {
                        if let Some(throttle) = window.try_state::<geometry::Throttle>() {
                            geometry::save_event(&target, Some(&throttle));
                        }
                    }
                }
                // 关窗 ≠ 退出（报告 A3）：隐藏窗口，服务与长任务继续跑；
                // Dock 点图标或再次启动时重开窗口。真正退出走 ⌘Q / Dock 退出。
                tauri::WindowEvent::CloseRequested { api, .. } if window.label() == "main" => {
                    if let Some(target) = window.app_handle().get_webview_window("main") {
                        geometry::save_event(&target, None);
                    }
                    api.prevent_close();
                    let _ = window.hide();
                }
                tauri::WindowEvent::Destroyed if window.label() == "main" => {
                    if let Some(target) = window.app_handle().get_webview_window("main") {
                        geometry::save_event(&target, None);
                    }
                    if let Some(state) = window.try_state::<Mutex<AppState>>() {
                        if let Ok(mut state) = state.lock() {
                            if let Some(server) = state.server.as_mut() {
                                server.shutdown();
                            }
                        }
                    }
                    window.app_handle().exit(0);
                }
                _ => {}
            }
        })
        .build(tauri::generate_context!())
        .expect("error while building DeepSeek Harness Desktop");
    {
        let handle = app.handle().clone();
        std::thread::spawn(move || {
            // 等待 setup 完成、AppState 就绪后再交给信号桥使用
            std::thread::sleep(std::time::Duration::from_millis(300));
            set_app_handle_for_signals(handle);
        });
    }
    app.run(|app_handle: &tauri::AppHandle, event| match event {
        // macOS：Dock 图标点击（或再次 open）重新显示被隐藏的主窗口。
        // 该变体仅存在于 macOS 构建中（Linux/Windows 上 RunEvent 没有 Reopen），
        // 缺门控会让另外两端编译失败。
        #[cfg(target_os = "macos")]
        tauri::RunEvent::Reopen { .. } => {
            if let Some(w) = app_handle.get_webview_window("main") {
                let _ = w.show();
                let _ = w.set_focus();
            }
        }
        // ⌘Q / Dock 退出：优雅关停 DSH 服务（报告 F2 的温和面）。
        tauri::RunEvent::ExitRequested { .. } => {
            // 退出前把窗口几何落盘（强制写，不受节流影响）。
            if let Some(window) = app_handle.get_webview_window("main") {
                geometry::save_event(&window, None);
            }
            if let Some(state) = app_handle.try_state::<Mutex<AppState>>() {
                if let Ok(mut state) = state.lock() {
                    if let Some(server) = state.server.as_mut() {
                        server.shutdown();
                    }
                }
            }
        }
        _ => {}
    });
}

// --------------------------------------------------------------- bootstrap ---

/// Outcome of the shared boot sequence.
struct Booted {
    paths: Paths,
    runtime: Runtime,
    server: Server,
    dsh_home: std::path::PathBuf,
}

/// The boot sequence itself, independent of any window.
///
/// Both the real app and `--self-check` drive this, so "the app works" and
/// "the headless check passes" cannot drift apart.
type Sink = std::sync::Arc<dyn Fn(&str) + Send + Sync>;

/// Kill orphaned DSH servers left behind by a previous crash of THIS app.
///
/// When the shell is force-killed, its vendored node child survives (reparented
/// to launchd) — a zombie still serving an old code tree. With `port: 0` it
/// cannot block a new instance, but it wastes memory, holds open handles and
/// keeps a stale URL alive. We only reclaim processes whose command line
/// points at THIS app's private vendor tree AND whose parent is launchd — a
/// user's own `dsh` CLI (different path, still-parented) can never match.
/// Set by the signal handler when the process receives TERM/INT/HUP.
/// (Unix only — Windows has no SIGHUP and terminates the shell directly.)
#[cfg(unix)]
static SIGNALLED: std::sync::atomic::AtomicBool = std::sync::atomic::AtomicBool::new(false);

/// Global handle for the signal monitor thread (set shortly after build).
static APP_HANDLE: std::sync::Mutex<Option<tauri::AppHandle>> = std::sync::Mutex::new(None);

fn set_app_handle_for_signals(handle: tauri::AppHandle) {
    if let Ok(mut slot) = APP_HANDLE.lock() {
        *slot = Some(handle);
    }
}

fn app_handle_for_signals() -> Option<tauri::AppHandle> {
    APP_HANDLE.lock().ok().and_then(|slot| slot.clone())
}

#[cfg(unix)]
extern "C" fn on_signal(_sig: i32) {
    SIGNALLED.store(true, std::sync::atomic::Ordering::SeqCst);
}

/// Bridge termination signals into the normal exit path.
///
/// Without this, `pkill`/`kill` kills the shell outright and its vendored node
/// child is left running as an orphan (报告 A5/A1). The handler itself only
/// flips an atomic (async-signal-safe); a monitor thread performs the real
/// shutdown via `AppHandle::exit`, which runs `ExitRequested` → server stop.
#[cfg(unix)]
fn install_signal_bridge() {
    for sig in [libc::SIGTERM, libc::SIGINT, libc::SIGHUP] {
        unsafe {
            libc::signal(sig, on_signal as *const () as usize);
        }
    }
    std::thread::spawn(|| loop {
        if SIGNALLED.load(std::sync::atomic::Ordering::SeqCst) {
            if let Some(state) = crate::app_handle_for_signals() {
                state.exit(0);
            }
            std::thread::sleep(std::time::Duration::from_millis(2500));
            // graceful path did not finish in time — die hard so the process
            // never lingers; the next start reclaims any leftover node.
            std::process::exit(0);
        }
        std::thread::sleep(std::time::Duration::from_millis(200));
    });
}

fn reclaim_orphan_dsh(data: &std::path::Path, log: &dyn Fn(&str)) {
    let marker = format!("{}/vendor/node_modules/@deepseek-ai/dsh", data.display());
    let output = match std::process::Command::new("ps").args(["-axo", "pid=,ppid=,command="]).output() {
        Ok(o) if o.status.success() => String::from_utf8_lossy(&o.stdout).into_owned(),
        _ => return,
    };
    let mut orphans: Vec<i32> = Vec::new();
    for line in output.lines() {
        // ps 对齐输出列间是多空格：splitn(按空白字符) 会切出空字段，
        // 必须按"位置"取前两列，剩余整体作为命令行。
        let trimmed = line.trim();
        let i1 = match trimmed.find(char::is_whitespace) { Some(i) => i, None => continue };
        let pid = &trimmed[..i1];
        let rest = trimmed[i1..].trim_start();
        let i2 = match rest.find(char::is_whitespace) { Some(i) => i, None => continue };
        let ppid = &rest[..i2];
        let cmd = rest[i2..].trim_start();
        if !cmd.contains(&marker) {
            continue;
        }
        let Ok(ppid) = ppid.trim().parse::<i32>() else { continue };
        if ppid > 1 {
            continue; // parent still alive → a live instance, never touch it
        }
        let Ok(pid) = pid.trim().parse::<i32>() else { continue };
        orphans.push(pid);
    }
    if orphans.is_empty() {
        return;
    }
    for pid in &orphans {
        log(&format!("reclaiming orphaned DSH server (pid {pid}) from a previous run"));
        let _ = std::process::Command::new("kill").arg(pid.to_string()).status();
    }
    std::thread::sleep(std::time::Duration::from_millis(1500));
    for pid in &orphans {
        // Still alive after SIGTERM → SIGKILL (report A1: 不恋战).
        let _ = std::process::Command::new("kill").arg("-9").arg(pid.to_string()).status();
    }
}

fn boot_core(
    paths: &Paths,
    reporter: &(dyn Fn(&str, &str, Option<f64>) + Send + Sync),
    log: &Sink,
) -> Result<Booted, String> {
    std::fs::create_dir_all(&paths.data)
        .map_err(|e| format!("cannot create {}: {e}", paths.data.display()))?;
    let settings = Settings::load(&paths.data);

    // 1. Materialise the runtime payload into the writable data directory.
    let source = payload::locate(&paths.payload);
    let generation = payload::generation_of(&source);
    if paths.needs_extract(&generation) {
        reporter("extract", "Unpacking the DeepSeek Harness runtime…", Some(0.15));
        // Verify before unpacking: a truncated copy into the app bundle is the
        // one packaging failure that would otherwise surface as a confusing
        // archive error.
        if let payload::PayloadSource::Archive(archive) = &source {
            if let Err(error) = payload::verify_archive(archive) {
                log(&format!("warning: {error}"));
            }
        }
        let extract_total = if let payload::PayloadSource::Archive(archive) = &source {
            payload::entry_total(archive)
        } else {
            None
        };
        payload::materialise(&paths, &source, &generation, |count| {
            let ratio = extract_total
                .map(|total| 0.15 + 0.40 * (count as f64 / total as f64).min(1.0));
            reporter("extract", &format!("已解包 {count} 个文件"), ratio);
        })?;
    }

    // 2. Resolve node + the DSH entry point, then update if asked to.
    let mut runtime = Runtime::resolve(&paths)?;
    let dsh_home = settings.dsh_home(&paths.data);
    std::fs::create_dir_all(&dsh_home)
        .map_err(|e| format!("cannot create {}: {e}", dsh_home.display()))?;

    reporter(
        "resolve",
        &format!(
            "DSH {} · node {} ({})",
            runtime.dsh_version.clone().unwrap_or_else(|| "unknown".into()),
            runtime::probe_node_version(runtime.node.path()),
            runtime.node.label()
        ),
        Some(0.10),
    );

    if settings.auto_update && settings.update_on_first_boot {
        reporter("update", "Checking for a newer DeepSeek Harness…", Some(0.62));
        let update_sink = std::sync::Arc::clone(log);
        match update::auto_update(&paths, &runtime, move |line| update_sink(line)) {
            Ok(outcome) => log(&format!("update: {}", outcome.message)),
            Err(error) => log(&format!("update skipped: {error}")),
        }
        runtime = Runtime::resolve(&paths)?;
    }

    // 3. Start the DSH web server and capture the authenticated URL it prints.
    reporter("boot", "Starting the DeepSeek Harness server…", Some(0.75));
    let server_sink = std::sync::Arc::clone(log);
    let server = runtime::start(&paths, &runtime, &settings, &dsh_home, move |line| server_sink(line))?;

    Ok(Booted {
        paths: paths.clone(),
        runtime,
        server,
        dsh_home,
    })
}

/// The windowed bootstrap: run `boot_core`, publish state, show the DSH window.
fn bootstrap(app: &tauri::AppHandle) -> Result<(), String> {
    let paths = Paths::resolve(app)?;
    std::fs::create_dir_all(&paths.data)
        .map_err(|e| format!("cannot create {}: {e}", paths.data.display()))?;
    let settings = Settings::load(&paths.data);
    // Truncate-per-launch: the file reflects the current session.
    let log_file = std::fs::File::create(paths.data.join("boot.log"))
        .map_err(|e| format!("cannot open boot log: {e}"))?;
    app.manage(Mutex::new(AppState {
        paths: Some(paths.clone()),
        settings: Some(settings),
        phase: "starting".into(),
        log_file: Some(log_file),
        ..Default::default()
    }));
    // 窗口几何（逻辑点）就地落在数据目录里，主窗口构建时读它。
    let saved = geometry::load(&paths.data);
    app.manage(geometry::Store::new(&paths.data, saved));
    app.manage(geometry::Throttle::default());
    // 报告 A5：先回收上次异常退出留下的孤儿 dsh（只认本 APP 私有目录的进程）。
    reclaim_orphan_dsh(&paths.data, &|line| {
        if let Some(state) = app.try_state::<Mutex<AppState>>() {
            if let Ok(mut state) = state.lock() {
                push_log(&mut state, line);
            }
        }
    });

    // A single sink shared by every phase that produces diagnostics. It has to
    // be an Arc because `update` and `runtime` hand it to reader threads.
    let log_sink: std::sync::Arc<dyn Fn(&str) + Send + Sync> = {
        let app = app.clone();
        std::sync::Arc::new(move |line: &str| {
            if let Some(state) = app.try_state::<Mutex<AppState>>() {
                if let Ok(mut state) = state.lock() {
                    push_log(&mut state, line);
                }
            }
        })
    };

    let reporter = {
        let app = app.clone();
        move |phase: &str, detail: &str, ratio: Option<f64>| emit(&app, phase, detail, ratio)
    };
    let booted = boot_core(&paths, &reporter, &log_sink)?;

    if let Some(state) = app.try_state::<Mutex<AppState>>() {
        if let Ok(mut state) = state.lock() {
            state.node_version = Some(runtime::probe_node_version(booted.runtime.node.path()));
            state.runtime = Some(booted.runtime);
            state.server = Some(booted.server);
            state.phase = "ready".into();
        }
    }

    let url = {
        let state = app
            .try_state::<Mutex<AppState>>()
            .ok_or_else(|| "app state unavailable".to_string())?;
        let guard = state.lock().map_err(|_| "app state poisoned".to_string())?;
        match guard.server.as_ref() {
            Some(server) => server.url.clone(),
            None => return Err("server URL disappeared during bootstrap".into()),
        }
    };

    emit(app, "ready", "Ready.", Some(1.0));
    create_main_window(app, &url)?; // the splash closes when the UI has painted
    Ok(())
}

fn create_main_window(app: &tauri::AppHandle, url: &str) -> Result<(), String> {
    if app.get_webview_window("main").is_some() {
        return Ok(());
    }
    let parsed: url::Url = url.parse().map_err(|e| format!("bad server URL {url}: {e}"))?;

    // 上次的窗口几何（逻辑点）。先按原值建窗，构建完、显示前再按当前显示器夹取，
    // 所以先建后调不会闪烁（窗口是 visible(false) 建出来的）。
    let saved = app
        .try_state::<geometry::Store>()
        .map(|store| store.get())
        .unwrap_or_default();

    let window = WebviewWindowBuilder::new(app, "main", WebviewUrl::External(parsed))
        .title("DeepSeek Harness")
        .inner_size(saved.width, saved.height)
        // 最小逻辑尺寸：比这更窄时 DSH 界面会明显挤压（尤其设置面板）。
        .min_inner_size(geometry::MIN_W, geometry::MIN_H)
        .center()
        .visible(false)
        // Match the DSH dark theme so nothing white can flash before first paint.
        .background_color(tauri::window::Color(13, 17, 23, 255))
        .focused(true)
        // The DSH UI is a loopback web app: nothing here may navigate away from
        // it, which keeps the desktop shell immune to a stray external link.
        .on_navigation(|target| is_loopback(target))
        // Reveal the window only once the page has actually painted — showing
        // it earlier is what produced the white flash on every launch.
        .on_page_load(|window, event| {
            if event.event() == tauri::webview::PageLoadEvent::Finished {
                // 桌面端适配层：DSH 的 Web UI 不带这些修正，由壳注入（见 DESKTOP_CSS）。
                inject_desktop_css(&window);
                let _ = window.show();
                let _ = window.set_focus();
                if let Some(splash) = window.get_webview_window("splash") {
                    let _ = splash.close();
                }
            }
        })
        .build()
        .map_err(|e| format!("cannot open the DSH window: {e}"))?;

    // 落定几何：按当前显示器夹取（换屏/换缩放/拔插外接屏都安全），并把修正值落盘。
    let fitted = geometry::fit(&window, &saved);
    geometry::apply(&window, &fitted);
    if let Some(store) = app.try_state::<geometry::Store>() {
        store.write(&fitted);
    }

    // Safety net: if the page never finishes loading, reveal the window anyway
    // (the app must never end up as an invisible process).
    let safety = window.clone();
    std::thread::spawn(move || {
        std::thread::sleep(std::time::Duration::from_secs(15));
        if !safety.is_visible().unwrap_or(true) {
            let _ = safety.show();
            let _ = safety.set_focus();
        }
    });
    Ok(())
}

/// Only loopback HTTP(S) targets are allowed inside the app window.
fn is_loopback(url: &url::Url) -> bool {
    matches!(url.scheme(), "http" | "https")
        && matches!(
            url.host_str(),
            Some("127.0.0.1") | Some("localhost") | Some("::1")
        )
}

// ------------------------------------------------- 桌面端适配层（CSS 注入） ---

/// 注入到 DSH 界面里的桌面端修正。
///
/// 为什么放在壳里而不是改 DSH 的文件：DSH 会自更新，改它的 assets 会被下一次
/// 更新覆盖；壳的注入则是「每次加载都重新执行」，跟着 DSH 一起升级。
///
/// 目前只修一个已复现的问题：**设置面板左侧导航在插件多时溢出被裁**。
/// 面板根节点 `[role=dialog][class*="_panel"]` 是 `overflow: hidden`，而里面的
/// `<nav>` 高度被 flex 拉伸到面板高度、内容更高（实测 588 > 552），多出来的
/// 36px 被直接裁掉 —— 后装的插件贡献的设置项（自动续跑 / 侧边卡片 / 使用统计 /
/// 会话归档管理…）既看不到也点不到。让该 nav 自己滚动即可（已实测修复）。
const DESKTOP_CSS: &str = r#"
/* 设置面板左侧导航：允许滚动（插件多时最后几项原本被裁掉、点不到） */
[role="dialog"] > nav[class*="_nav"],
[role="dialog"] > nav {
  overflow-y: auto !important;
  overscroll-behavior: contain;
  min-height: 0;
  scrollbar-width: thin;
}
[role="dialog"] > nav[class*="_nav"]::-webkit-scrollbar { width: 8px; }
[role="dialog"] > nav[class*="_nav"]::-webkit-scrollbar-thumb {
  background: rgba(255, 255, 255, 0.18);
  border-radius: 4px;
}
[role="dialog"] > nav[class*="_nav"]::-webkit-scrollbar-thumb:hover {
  background: rgba(255, 255, 255, 0.3);
}
/* 浮层里的滚动容器一律允许收缩：flex 子项默认 min-height:auto 会让内容顶破父级 */
[role="dialog"] > * { min-height: 0; }
"#;

/// 把 `DESKTOP_CSS` 注入当前页面（幂等：同一个页面重复加载只保留一份）。
///
/// 优先用 `adoptedStyleSheets`（构造式样式表不受页面 CSP 限制），失败再退回
/// `<style>` 标签。注入失败绝不能让界面起不来，所以整体吞掉异常。
fn inject_desktop_css(window: &tauri::WebviewWindow) {
    let css = serde_json::to_string(DESKTOP_CSS).unwrap_or_else(|_| "\"\"".into());
    let script = format!(
        r#"(function () {{
  try {{
    var CSS = {css};
    var MARK = "dsh-desktop-fixes";
    var applied = false;
    try {{
      if (window.CSSStyleSheet && "adoptedStyleSheets" in document) {{
        var sheets = document.adoptedStyleSheets || [];
        var found = null;
        for (var i = 0; i < sheets.length; i++) {{
          if (sheets[i].__dshMark === MARK) {{ found = sheets[i]; break; }}
        }}
        if (found) {{
          found.replaceSync(CSS);
        }} else {{
          var sheet = new CSSStyleSheet();
          sheet.__dshMark = MARK;
          sheet.replaceSync(CSS);
          document.adoptedStyleSheets = sheets.concat([sheet]);
        }}
        applied = true;
      }}
    }} catch (e) {{ applied = false; }}
    if (!applied) {{
      var style = document.querySelector("style[data-" + MARK + "]");
      if (!style) {{
        style = document.createElement("style");
        style.setAttribute("data-" + MARK, "");
        document.head.appendChild(style);
      }}
      if (style.textContent !== CSS) style.textContent = CSS;
    }}
  }} catch (error) {{ /* 注入失败不影响界面本身 */ }}
}})();"#
    );
    let _ = window.eval(&script);
}

// --------------------------------------------------------------- 单实例守卫 ---

/// 同一份数据目录**只允许一个实例**。
///
/// 两个实例抢同一个 `$DSH_HOME` 是真实的数据损坏路径：会话索引、settings.yaml、
/// 凭据都是「读-改-写」，后写的会覆盖先写的。这里在窗口出现前就把第二个实例劝退：
/// 把已有窗口带到前台（macOS 用 `open -b <bundle id>`），然后本进程退出。
///
/// 自测/调试需要双开时设 `DSH_DESKTOP_ALLOW_MULTI=1` 跳过守卫。
fn claim_single_instance(app: &tauri::AppHandle) {
    if std::env::var_os("DSH_DESKTOP_ALLOW_MULTI").is_some_and(|v| v != "0") {
        return;
    }
    let Ok(paths) = Paths::resolve(app) else {
        return; // 解析不出数据目录时不拦（宁可多开，也不能启动不了）
    };
    let stamp = paths.data.join("app.pid");

    if let Some(pid) = std::fs::read_to_string(&stamp)
        .ok()
        .and_then(|raw| raw.trim().parse::<i32>().ok())
    {
        if pid != std::process::id() as i32 && process_is_this_app(pid) {
            if let Some(window) = app.get_webview_window("splash") {
                let _ = window.close();
            }
            raise_existing_instance(app);
            std::process::exit(0);
        }
    }

    let _ = std::fs::create_dir_all(&paths.data);
    let _ = std::fs::write(&stamp, std::process::id().to_string());
}

/// 记录里的 pid 是否真的是本应用（防止 pid 复用误判）。
fn process_is_this_app(pid: i32) -> bool {
    let output = std::process::Command::new("ps")
        .args(["-p", &pid.to_string(), "-o", "command="])
        .output();
    match output {
        Ok(out) if out.status.success() => {
            let command = String::from_utf8_lossy(&out.stdout);
            command.contains("dsh-desktop") || command.contains("DeepSeekHarness")
        }
        _ => false,
    }
}

/// 把已经在跑的实例带到前台（macOS 按 bundle id 唤回）。
fn raise_existing_instance(app: &tauri::AppHandle) {
    #[cfg(target_os = "macos")]
    {
        let identifier = app.config().identifier.clone();
        let _ = std::process::Command::new("open")
            .args(["-b", &identifier])
            .status();
    }
    #[cfg(not(target_os = "macos"))]
    {
        let _ = app;
    }
}


// ---------------------------------------------------------------- helpers ----

fn emit(app: &tauri::AppHandle, phase: &str, detail: &str, ratio: Option<f64>) {
    // Push straight into the splash webview. The splash runs with
    // `withGlobalTauri: false`, so it has no `__TAURI__` to listen with —
    // eval is the only channel that always works, and it exposes nothing.
    if let Some(splash) = app.get_webview_window("splash") {
        if let Ok(payload) = serde_json::to_string(&BootProgress {
            phase: phase.into(),
            detail: detail.into(),
            ratio,
        }) {
            let _ = splash.eval(&format!("window.__boot && window.__boot({payload});"));
        }
    }
    if let Some(state) = app.try_state::<Mutex<AppState>>() {
        if let Ok(mut state) = state.lock() {
            state.phase = phase.into();
            push_log(&mut state, &format!("[{phase}] {detail}"));
        }
    }
}

fn push_log(state: &mut AppState, line: &str) {
    state.log.push(line.to_string());
    trim_log(&mut state.log);
    if let Some(file) = state.log_file.as_mut() {
        let timestamp = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|d| d.as_secs())
            .unwrap_or(0);
        let _ = writeln!(file, "{timestamp} {line}");
    }
}

fn trim_log(log: &mut Vec<String>) {
    if log.len() > LOG_TAIL_LINES {
        let overflow = log.len() - LOG_TAIL_LINES;
        log.drain(0..overflow);
    }
}

// --------------------------------------------------------------- commands ----

fn snapshot(app: &tauri::AppHandle) -> Status {
    let blank = |phase: &str| Status {
        phase: phase.into(),
        url: None,
        dsh_version: None,
        node_version: None,
        node_source: None,
        self_contained: false,
        package_manager: None,
        data_dir: None,
        dsh_home: None,
        log: Vec::new(),
    };

    let Some(state) = app.try_state::<Mutex<AppState>>() else {
        return blank("unknown");
    };
    let state = match state.lock() {
        Ok(state) => state,
        Err(_) => return blank("poisoned"),
    };
    let settings = state.settings.clone().unwrap_or_default();
    Status {
        phase: state.phase.clone(),
        url: state.server.as_ref().map(|s| s.url.clone()),
        dsh_version: state.runtime.as_ref().and_then(|r| r.dsh_version.clone()),
        node_version: state.node_version.clone(),
        node_source: state.runtime.as_ref().map(|r| r.node.label().to_string()),
        self_contained: state.runtime.as_ref().is_some_and(update::is_self_contained),
        package_manager: state
            .runtime
            .as_ref()
            .zip(state.paths.as_ref())
            .and_then(|(runtime, paths)| update::package_manager(paths, runtime))
            .map(|p| p.display().to_string()),
        data_dir: state.paths.as_ref().map(|p| p.data.display().to_string()),
        dsh_home: state
            .paths
            .as_ref()
            .map(|p| settings.dsh_home(&p.data).display().to_string()),
        log: state.log.clone(),
    }
}

#[tauri::command]
fn get_status(app: tauri::AppHandle) -> Status {
    snapshot(&app)
}

#[tauri::command]
fn get_log(app: tauri::AppHandle) -> Vec<String> {
    snapshot(&app).log
}

#[tauri::command]
async fn check_for_update(app: tauri::AppHandle) -> Result<update::UpdateCheck, String> {
    let state = app
        .try_state::<Mutex<AppState>>()
        .ok_or_else(|| "app state unavailable".to_string())?;
    let runtime = {
        let state = state.lock().map_err(|_| "app state poisoned".to_string())?;
        state.runtime.clone()
    };
    let current = runtime.and_then(|r| r.dsh_version);
    Ok(update::check(current))
}

#[tauri::command]
async fn install_update(app: tauri::AppHandle) -> Result<update::UpdateOutcome, String> {
    let state = app
        .try_state::<Mutex<AppState>>()
        .ok_or_else(|| "app state unavailable".to_string())?;
    let (paths, runtime) = {
        let state = state.lock().map_err(|_| "app state poisoned".to_string())?;
        (
            state.paths.clone().ok_or("paths unavailable")?,
            state.runtime.clone().ok_or("runtime not resolved yet")?,
        )
    };
    let log_app = app.clone();
    let outcome = update::install(&paths, &runtime, "@deepseek-ai/dsh@latest", move |line| {
        if let Some(state) = log_app.try_state::<Mutex<AppState>>() {
            if let Ok(mut state) = state.lock() {
                push_log(&mut state, line);
            }
        }
    })?;
    Ok(outcome)
}

#[tauri::command]
fn restart(app: tauri::AppHandle) {
    app.restart();
}

#[tauri::command]
fn open_data_dir(app: tauri::AppHandle) -> Result<(), String> {
    let dir = app
        .try_state::<Mutex<AppState>>()
        .and_then(|state| state.lock().ok().and_then(|s| s.paths.clone()))
        .map(|p: Paths| p.data)
        .ok_or_else(|| "paths unavailable".to_string())?;

    let opener = if cfg!(target_os = "macos") {
        "open"
    } else if cfg!(windows) {
        "explorer"
    } else {
        "xdg-open"
    };
    std::process::Command::new(opener)
        .arg(&dir)
        .spawn()
        .map(|_| ())
        .map_err(|e| format!("cannot open {}: {e}", dir.display()))
}

// ------------------------------------------------------------- self-check ----

/// `--self-check` on the command line, or `DSH_DESKTOP_SELF_CHECK=1`.
fn self_check_requested() -> bool {
    std::env::args().any(|arg| arg == "--self-check")
        || std::env::var_os("DSH_DESKTOP_SELF_CHECK").is_some_and(|v| v != "0")
}

/// Headless end-to-end verification of the real boot path.
///
/// Checks, in order: payload present and (when a sidecar exists) hash-correct,
/// extraction succeeds, the vendored node runs, the DSH server reports its URL,
/// and that URL actually serves the Harness UI over loopback. Exits non-zero on
/// the first failure, so it is usable as a CI gate.
fn self_check() -> i32 {
    let started = std::time::Instant::now();
    let mut ok = true;
    let report = |phase: &str, detail: &str| {
        println!("[self-check] {phase:<8} {detail}");
        let _ = std::io::stdout().flush();
    };
    let log: Sink = std::sync::Arc::new(|line: &str| {
        println!("[self-check] log      {line}");
        let _ = std::io::stdout().flush();
    });

    report("start", &format!("dsh-desktop {}", env!("CARGO_PKG_VERSION")));

    // A verification run must never touch the user's real Harness home (it
    // seeds profiles there). Force the app-private home for the duration.
    if let Ok(data) = std::env::var("DSH_DESKTOP_DATA") {
        let settings_path = std::path::Path::new(&data).join("settings.json");
        let _ = std::fs::create_dir_all(&data);
        let _ = std::fs::write(
            &settings_path,
            "{\"shareDshHome\": false, \"autoUpdate\": false}\n",
        );
        report("config", &format!("isolated settings written to {}", settings_path.display()));
    }

    let paths = match Paths::resolve_headless() {
        Ok(paths) => paths,
        Err(error) => {
            eprintln!("[self-check] FAILED: {error}");
            return 1;
        }
    };
    let result = boot_core(&paths, &|phase, detail, _| report(phase, detail), &log);
    let booted = match result {
        Ok(booted) => booted,
        Err(error) => {
            eprintln!("[self-check] FAILED during boot: {error}");
            return 1;
        }
    };

    report(
        "runtime",
        &format!(
            "node {} ({}) at {}",
            runtime::probe_node_version(booted.runtime.node.path()),
            booted.runtime.node.label(),
            booted.runtime.node.path().display()
        ),
    );
    report(
        "runtime",
        &format!(
            "dsh {} at {}",
            booted.runtime.dsh_version.clone().unwrap_or_else(|| "unknown".into()),
            booted.runtime.dsh_bin.display()
        ),
    );
    report("runtime", &format!("self-contained: {}", update::is_self_contained(&booted.runtime)));
    report(
        "runtime",
        &format!(
            "package manager: {}",
            update::package_manager(&booted.paths, &booted.runtime)
                .map(|p| p.display().to_string())
                .unwrap_or_else(|| "none (updates disabled)".into())
        ),
    );
    report("runtime", &format!("DSH_HOME: {}", booted.dsh_home.display()));

    match http_get(&booted.server.url, None) {
        Ok(response) => {
            report(
                "http",
                &format!(
                    "token exchange: HTTP {} with session cookie ({})",
                    response.status,
                    !response.cookies.is_empty()
                ),
            );
            if !(300..400).contains(&response.status) || response.cookies.is_empty() {
                eprintln!(
                    "[self-check] FAILED: the token URL did not redirect with a session cookie (HTTP {})",
                    response.status
                );
                ok = false;
            }
            let replay = response.cookies.join("; ");
            match http_get(&response.location, Some(&replay)) {
                Ok(ui) if ui.status == 200 && ui.body.contains("__ModuleLoader__") => {
                    report(
                        "http",
                        &format!("UI served: HTTP 200, {} bytes, Harness bootstrap present", ui.body.len()),
                    );
                }
                Ok(ui) => {
                    eprintln!(
                        "[self-check] FAILED: the UI did not load (HTTP {}, bootstrap {}, {} bytes, head: {:?})",
                        ui.status,
                        ui.body.contains("__ModuleLoader__"),
                        ui.body.len(),
                        ui.body.chars().take(120).collect::<String>()
                    );
                    ok = false;
                }
                Err(error) => {
                    eprintln!("[self-check] FAILED: cannot load the UI: {error}");
                    ok = false;
                }
            }
        }
        Err(error) => {
            eprintln!("[self-check] FAILED: cannot reach the DSH server: {error}");
            ok = false;
        }
    }

    let mut server = booted.server;
    server.shutdown();

    println!(
        "[self-check] {} in {} ms",
        if ok { "PASS" } else { "FAIL" },
        started.elapsed().as_millis()
    );
    let _ = std::io::stdout().flush();
    if ok {
        0
    } else {
        1
    }
}

/// A minimal HTTP response for the self-check (no extra dependency needed).
struct HttpResponse {
    status: u16,
    location: String,
    body: String,
    /// `name=value` pairs of every `Set-Cookie` header.
    cookies: Vec<String>,
}

/// Send `GET <url>` over a plain loopback socket and read the whole response.
///
/// `cookie` (optional) is sent as the `Cookie` header — the self-check uses it
/// to replay the session cookie the token exchange installed, exactly the way a
/// webview would.
fn http_get(url: &str, cookie: Option<&str>) -> Result<HttpResponse, String> {
    let rest = url
        .strip_prefix("http://")
        .ok_or_else(|| format!("only http:// URLs are supported, got {url:?}"))?;
    let (authority, path) = match rest.find('/') {
        Some(index) => (&rest[..index], &rest[index..]),
        None => (rest, "/"),
    };

    let mut stream = TcpStream::connect(authority).map_err(|e| format!("connect {authority}: {e}"))?;
    stream
        .set_read_timeout(Some(std::time::Duration::from_secs(20)))
        .map_err(|e| e.to_string())?;
    let cookie_header = cookie
        .filter(|value| !value.is_empty())
        .map(|value| format!("Cookie: {value}\r\n"))
        .unwrap_or_default();
    // HTTP/1.0 on purpose: it forbids chunked transfer coding, so the raw body
    // can be searched as-is by this dependency-free client.
    let request = format!(
        "GET {path} HTTP/1.0\r\nHost: {authority}\r\nAccept: */*\r\n{cookie_header}Connection: close\r\n\r\n"
    );
    stream
        .write_all(request.as_bytes())
        .map_err(|e| format!("write request: {e}"))?;

    let mut raw = String::new();
    stream
        .read_to_string(&mut raw)
        .map_err(|e| format!("read response: {e}"))?;

    let mut lines = raw.lines();
    let status_line = lines.next().unwrap_or_default();
    let status = status_line
        .split_whitespace()
        .nth(1)
        .and_then(|code| code.parse::<u16>().ok())
        .ok_or_else(|| format!("malformed status line: {status_line:?}"))?;

    let mut location = String::new();
    let mut cookies = Vec::new();
    for line in raw.lines().take_while(|line| !line.trim().is_empty()) {
        if let Some(value) = line.strip_prefix("Location: ").or_else(|| line.strip_prefix("location: ")) {
            location = value.trim().to_string();
        }
        if let Some(value) = line.strip_prefix("Set-Cookie: ").or_else(|| line.strip_prefix("set-cookie: ")) {
            // Only the `name=value` part matters for the replay.
            let pair = value.trim().split(';').next().unwrap_or_default().trim().to_string();
            if !pair.is_empty() {
                cookies.push(pair);
            }
        }
    }
    if location.starts_with('/') {
        location = format!("http://{authority}{location}");
    }

    let body = raw
        .split_once("\r\n\r\n")
        .map(|(_, body)| body.to_string())
        .unwrap_or_default();

    Ok(HttpResponse {
        status,
        location,
        body,
        cookies,
    })
}
