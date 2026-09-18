#!/usr/bin/env bash
# 构建"测试版"应用包，并放到开发目录根部（不装进 /Applications、不进启动台）。
#
# 测试版 = 应用标识符 ai.deepseek.harness.desktop      （数据目录同名）
# 日用版 = 应用标识符 ai.deepseek.harness.desktop.daily（数据目录同名，装在 /Applications）
# 两者标识符不同，可同时运行、数据互不可见。
#
# 用法：bash scripts/build-test-app.sh [--vendor]
#   --vendor  同时重新准备运行时载荷（DSH/Node 版本变化时才需要，约 5 分钟）
set -euo pipefail

ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
cd "$ROOT"

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

export CARGO_HOME="${CARGO_HOME:-$ROOT/.toolchain/cargo}"
export RUSTUP_HOME="${RUSTUP_HOME:-$ROOT/.toolchain/rustup}"
export PATH="$CARGO_HOME/bin:$PATH"

echo "[test-app] 构建 $PRODUCT_NAME …"
cargo tauri build --ci --bundles app --config "{\"productName\":\"$PRODUCT_NAME\"}"

echo "[test-app] 复制到开发目录：$TARGET_APP"
rm -rf "$TARGET_APP"
cp -R "$BUNDLE_DIR/$PRODUCT_NAME.app" "$TARGET_APP"

echo "[test-app] 校验身份："
/usr/libexec/PlistBuddy -c "Print :CFBundleIdentifier" "$TARGET_APP/Contents/Info.plist"
du -sh "$TARGET_APP"
echo "[test-app] 完成。双击即可调试；数据在 ~/Library/Application Support/ai.deepseek.harness.desktop"
