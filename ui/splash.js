// Splash screen controller.
//
// Progress arrives WITHOUT any Tauri API: the Rust shell calls
// webview.eval("window.__boot(entry)") on this window. The collector lives in
// index.html <head> and queues entries until this script attaches
// window.__bootRender, then replays them — so no update can be lost even when
// the shell gets ahead of the renderer.

const bar = document.getElementById("bar");
const fill = document.getElementById("fill");
const pct = document.getElementById("pct");
const phase = document.getElementById("phase");
const log = document.getElementById("log");
const hint = document.getElementById("hint");

// 阶段文案（与 lib.rs 的 reporter 阶段名一一对应）
const LABELS = {
  resolve: "校验运行时",
  extract: "解包运行时",
  update: "检查更新",
  boot: "启动 Harness 服务",
  ready: "启动完成",
  error: "启动失败",
};

const seen = [];
let lastPhase = "";

function render(entry) {
  if (!entry || !entry.phase) return;

  // 阶段标题：有中文文案用文案，否则直接显示 detail
  const label = LABELS[entry.phase] ?? entry.phase;
  if (entry.phase !== lastPhase) {
    lastPhase = entry.phase;
    phase.textContent = label;
    phase.classList.remove("swap");
    void phase.offsetWidth; // restart the swap animation
    phase.classList.add("swap");
  }

  // 真实进度：宽度由 shell 给出的 ratio 决定（解包进度 = 已解包条目 / 总条目）
  if (typeof entry.ratio === "number") {
    const r = Math.max(0, Math.min(1, entry.ratio));
    fill.style.width = (r * 100).toFixed(1) + "%";
    pct.textContent = Math.round(r * 100) + "%";
  }

  if (entry.phase === "error") {
    bar.classList.remove("live");
    bar.classList.add("error");
    fill.style.width = "100%";
    pct.textContent = "";
    hint.classList.add("error");
    hint.textContent = "运行时未能启动：反馈问题时请复制上方日志。";
  } else if (entry.ratio === 1 || entry.phase === "ready") {
    bar.classList.remove("live");
    fill.style.width = "100%";
    pct.textContent = "100%";
    phase.textContent = "启动完成，正在打开界面…";
  }

  if (entry.detail) {
    seen.push(entry.detail);
    log.textContent = seen.slice(-8).join("\n");
    log.scrollTop = log.scrollHeight;
  }
}

window.__bootRender = render;
// 回放渲染器挂上之前 shell 已经推进的进度
for (const entry of window.__bootLog || []) render(entry);
