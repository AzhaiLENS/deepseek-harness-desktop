//! 窗口几何的跨显示器持久化（**逻辑像素**）。
//!
//! 为什么不用 `tauri-plugin-window-state`：它把 `inner_size()`（**物理像素**）
//! 原样存盘，再用 `set_size(PhysicalSize)` 还原。于是「先在 1x 外接屏把窗口调到
//! 1778×1200，再回到 2x 内置屏」重启后，窗口就变成 889×600 **逻辑点**——
//! 在高分屏上界面被压扁、启动页底部直接被裁掉。位置同理。
//!
//! 这里只存逻辑点（点 = 用户看到的尺寸，与缩放无关），并在还原前按当前显示器
//! 夹取范围，因此换屏、换缩放、拔插外接显示器都不会再压扁窗口。

use std::path::{Path, PathBuf};
use std::sync::Mutex;
use std::time::{Duration, Instant};

use serde::{Deserialize, Serialize};
use tauri::{LogicalPosition, LogicalSize, Manager, WebviewWindow};

/// 几何状态文件名（位于应用数据目录）。
pub const FILE: &str = "window.json";

/// 主窗口的最小逻辑尺寸。DSH 界面在更窄的宽度下会明显挤压（设置面板尤其）。
pub const MIN_W: f64 = 960.0;
pub const MIN_H: f64 = 640.0;
/// 首次启动的默认逻辑尺寸。
pub const DEFAULT_W: f64 = 1280.0;
pub const DEFAULT_H: f64 = 860.0;

/// 两次写盘之间的最小间隔（毫秒）：拖动窗口时事件会连发，合并写入。
const SAVE_THROTTLE_MS: u64 = 600;

/// 持久化的窗口几何（全部是逻辑点）。
#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(default)]
pub struct Geometry {
    pub width: f64,
    pub height: f64,
    pub x: Option<f64>,
    pub y: Option<f64>,
    pub maximized: bool,
}

impl Default for Geometry {
    fn default() -> Self {
        Self {
            width: DEFAULT_W,
            height: DEFAULT_H,
            x: None,
            y: None,
            maximized: false,
        }
    }
}

impl Geometry {
    fn sane(&self) -> bool {
        self.width.is_finite()
            && self.height.is_finite()
            && self.width >= 320.0
            && self.height >= 240.0
            && self.width <= 20000.0
            && self.height <= 20000.0
    }
}

fn file(data: &Path) -> PathBuf {
    data.join(FILE)
}

/// 读取磁盘上的几何值；缺失、损坏或不合常理时回落到默认尺寸。
pub fn load(data: &Path) -> Geometry {
    let parsed: Option<Geometry> = std::fs::read_to_string(file(data))
        .ok()
        .and_then(|raw| serde_json::from_str(&raw).ok());
    match parsed {
        Some(g) if g.sane() => g,
        _ => Geometry::default(),
    }
}

/// 还原前按当前显示器夹取：尺寸落在 [最小, 屏幕可用区] 内，位置保证可见。
///
/// 返回可直接喂给窗口构建器的逻辑尺寸与位置。
pub fn fit(window: &WebviewWindow, saved: &Geometry) -> Geometry {
    let mut out = saved.clone();

    // 1) 选显示器：优先与记忆中位置相交的那块，否则主显示器。
    let monitors = window.available_monitors().unwrap_or_default();
    let picked = monitors
        .iter()
        .find(|m| match saved.x.zip(saved.y) {
            Some((x, y)) => {
                let scale = m.scale_factor();
                let pos = m.position().to_logical::<f64>(scale);
                let size = m.size().to_logical::<f64>(scale);
                x >= pos.x - 8.0
                    && y >= pos.y - 8.0
                    && x < pos.x + size.width - 80.0
                    && y < pos.y + size.height - 80.0
            }
            None => false,
        })
        .cloned()
        .or_else(|| window.primary_monitor().ok().flatten());

    let (mut area_w, mut area_h, mut area_x, mut area_y) = (DEFAULT_W, DEFAULT_H, 0.0, 0.0);
    if let Some(monitor) = picked {
        let scale = monitor.scale_factor();
        let size = monitor.size().to_logical::<f64>(scale);
        let pos = monitor.position().to_logical::<f64>(scale);
        area_w = size.width;
        area_h = size.height;
        area_x = pos.x;
        area_y = pos.y;
    }

    // 2) 尺寸：不小于最小尺寸，也不大于屏幕（留出菜单栏/程序坞的余量）。
    out.width = out.width.max(MIN_W).min(area_w.max(MIN_W));
    out.height = out.height.max(MIN_H).min(area_h.max(MIN_H));
    if !out.sane() {
        let fallback = Geometry::default();
        out.width = fallback.width;
        out.height = fallback.height;
    }

    // 3) 位置：整块落在显示器内，否则交给系统居中。
    if let (Some(x), Some(y)) = (out.x, out.y) {
        let max_x = area_x + (area_w - out.width).max(0.0);
        let max_y = area_y + (area_h - out.height).max(0.0);
        if x < area_x - 4.0 || y < area_y - 4.0 || x > max_x + 4.0 || y > max_y + 4.0 {
            out.x = None;
            out.y = None;
        }
    }
    out
}

/// 把几何应用到窗口（已存在的窗口：还原时用）。
pub fn apply(window: &WebviewWindow, geom: &Geometry) {
    let _ = window.set_size(LogicalSize::new(geom.width, geom.height));
    if let (Some(x), Some(y)) = (geom.x, geom.y) {
        let _ = window.set_position(LogicalPosition::new(x, y));
    }
    if geom.maximized {
        let _ = window.maximize();
    }
}

/// 读回窗口当前几何并写盘（始终换算成逻辑点）。
///
/// 最大化时只记 `maximized`，不覆盖还原尺寸——这样取消最大化能回到原来的大小。
pub fn capture(window: &WebviewWindow, previous: &Geometry) -> Option<Geometry> {
    let scale = window.scale_factor().ok()?;
    let maximized = window.is_maximized().unwrap_or(false);
    let mut next = previous.clone();
    next.maximized = maximized;
    if !maximized {
        if let Ok(size) = window.inner_size() {
            let logical = size.to_logical::<f64>(scale);
            if logical.width >= 1.0 && logical.height >= 1.0 {
                next.width = logical.width;
                next.height = logical.height;
            }
        }
        if let Ok(pos) = window.outer_position() {
            let logical = pos.to_logical::<f64>(scale);
            next.x = Some(logical.x);
            next.y = Some(logical.y);
        }
    }
    Some(next)
}

/// 写盘（原子替换：先写临时文件再 rename，避免断电留下半个 JSON）。
pub fn store(data: &Path, geom: &Geometry) -> std::io::Result<()> {
    let target = file(data);
    let tmp = target.with_extension("json.tmp");
    let raw = serde_json::to_string_pretty(geom).unwrap_or_default();
    if let Some(parent) = target.parent() {
        let _ = std::fs::create_dir_all(parent);
    }
    std::fs::write(&tmp, raw)?;
    std::fs::rename(&tmp, &target)
}

/// 事件洪流的节流器（Resized/Moved 连发时合并为一次写盘）。
pub struct Throttle(Mutex<Option<Instant>>);

impl Default for Throttle {
    fn default() -> Self {
        Self(Mutex::new(None))
    }
}

impl Throttle {
    /// true = 允许这次写盘。
    pub fn allow(&self) -> bool {
        let mut last = match self.0.lock() {
            Ok(guard) => guard,
            Err(_) => return false,
        };
        let now = Instant::now();
        match *last {
            Some(previous) if now.duration_since(previous) < Duration::from_millis(SAVE_THROTTLE_MS) => {
                false
            }
            _ => {
                *last = Some(now);
                true
            }
        }
    }
}

/// 几何状态的当前值（主窗口关闭/退出时还要用它，所以放进受管状态）。
pub struct Store {
    pub data: PathBuf,
    pub value: Mutex<Geometry>,
}

impl Store {
    pub fn new(data: &Path, value: Geometry) -> Self {
        Self {
            data: data.to_path_buf(),
            value: Mutex::new(value),
        }
    }

    pub fn get(&self) -> Geometry {
        self.value.lock().map(|g| g.clone()).unwrap_or_default()
    }

    /// 覆盖当前值（还原/夹取之后调用，作为后续比较的基准）。
    pub fn set(&self, geom: Geometry) {
        if let Ok(mut slot) = self.value.lock() {
            *slot = geom;
        }
    }

    /// 立即写盘（内存值同步成刚写的这份）。
    ///
    /// 还原时被显示器夹取过（比如记忆里是 889x600，实际落到最小 960x640），
    /// 就把修正后的值落盘，免得文件与真实窗口长期不一致。
    pub fn write(&self, geom: &Geometry) -> bool {
        self.set(geom.clone());
        store(&self.data, geom).is_ok()
    }

    /// 从窗口读回并写盘。
    pub fn save_from(&self, window: &WebviewWindow) -> bool {
        let previous = self.get();
        let Some(next) = capture(window, &previous) else {
            return false;
        };
        let changed = (next.width - previous.width).abs() > 0.5
            || (next.height - previous.height).abs() > 0.5
            || next.x != previous.x
            || next.y != previous.y
            || next.maximized != previous.maximized;
        if let Ok(mut slot) = self.value.lock() {
            *slot = next.clone();
        }
        if !changed {
            return false;
        }
        store(&self.data, &next).is_ok()
    }
}

/// 便捷入口：窗口事件里调用。`throttle` 为 `None` 时强制写盘。
pub fn save_event(window: &WebviewWindow, throttle: Option<&Throttle>) {
    let Some(store) = window.try_state::<Store>() else {
        return;
    };
    if let Some(throttle) = throttle {
        if !throttle.allow() {
            return;
        }
    }
    store.save_from(window);
}
