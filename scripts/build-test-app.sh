#!/usr/bin/env bash
# 构建"测试版"应用包，并放到开发目录根部（不装进 /Applications、不进启动台）。
#
# ── 两个渠道（靠「应用标识符」区分，标识符决定数据目录）────────────────────
#   测试版  ai.deepseek.harness.desktop.test   →  ~/Library/Application Support/ai.deepseek.harness.desktop.test
#   日用版  ai.deepseek.harness.desktop.daily  →  ~/Library/Application Support/ai.deepseek.harness.desktop.daily
#   两边数据完全独立，可同时运行、互不可见。
#
#   注意：标识符必须写在 src-tauri/tauri.conf.json 里。`tauri build --config` 的
#   覆盖只改到 Info.plist，改不到编译期配置，而运行时数据目录取自编译期标识符——
#   只改 plist 会导致应用仍写旧身份的数据目录。
#
# 用法：bash scripts/build-test-app.sh [--vendor]
#   --vendor  同时重新准备运行时载荷（DSH/Node 版本变化时才需要，约 5 分钟）
set -euo pipefail

ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
cd "$ROOT"

EXPECT_ID="ai.deepseek.harness.desktop.test"
PRODUCT_NAME="DeepSeek Harness Desktop 测试版"
BUNDLE_DIR="src-tauri/target/release/bundle/macos"
TARGET_APP="$ROOT/$PRODUCT_NAME.app"

if [[ "${1:-}" == "--vendor" ]]; then
  echo "[test-app] 准备运行时载荷（scripts/vendor-runtime.sh）…"
  bash scripts/vendor-runtime.sh
fi

if ! ls dist/dsh-payload-*.tar.zst >/dev/null 2>&1; then
  echo "[test-app] 缺少运行时载荷：先跑一次 bash scripts/vendor-runtime.sh" >&2
  exit 1
fi
mkdir -p src-tauri/payload
cp -f dist/dsh-payload-* src-tauri/payload/

# 标识符必须已经在配置文件里（见文件头说明）
if ! grep -q "\"identifier\": \"$EXPECT_ID\"" src-tauri/tauri.conf.json; then
  echo "[test-app] src-tauri/tauri.conf.json 的 identifier 不是 $EXPECT_ID" >&2
  echo "          测试版需要独立身份，否则会读到别的渠道的数据目录。" >&2
  exit 1
fi

export CARGO_HOME="${CARGO_HOME:-$ROOT/.toolchain/cargo}"
export RUSTUP_HOME="${RUSTUP_HOME:-$ROOT/.toolchain/rustup}"
export PATH="$CARGO_HOME/bin:$PATH"

echo "[test-app] 构建 $PRODUCT_NAME …"
cargo tauri build --ci --bundles app

echo "[test-app] 复制到开发目录：$TARGET_APP"
rm -rf "$TARGET_APP"
cp -R "$BUNDLE_DIR/$PRODUCT_NAME.app" "$TARGET_APP"

echo "[test-app] 校验："
/usr/libexec/PlistBuddy -c "Print :CFBundleIdentifier" "$TARGET_APP/Contents/Info.plist"
du -sh "$TARGET_APP"
echo "[test-app] 完成。双击即可调试；数据在 ~/Library/Application Support/$EXPECT_ID"
