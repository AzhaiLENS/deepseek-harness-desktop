#!/usr/bin/env bash
# dev.sh — prepare everything a local build needs, then run the app.
#
#   bash scripts/dev.sh              # build (if needed) and run in dev mode
#   bash scripts/dev.sh --build      # only prepare the payload + icons
#   bash scripts/dev.sh --release    # prepare and build a release bundle
#
# Everything the developer needs is kept inside the repository:
#   .toolchain/   rust toolchain (CARGO_HOME / RUSTUP_HOME)
#   .scratch/     npm cache, scratch trees
#   dist/         vendored payload archives
#
# No global state is touched — the script never writes to ~/.cargo, ~/.npm or
# ~/.dsh unless the app itself is asked to.

set -euo pipefail

ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
cd "$ROOT"

MODE="run"
case "${1:-}" in
  --build)   MODE="build" ;;
  --release) MODE="release" ;;
  --run|"")  MODE="run" ;;
  -h|--help) sed -n '2,16p' "$0"; exit 0 ;;
  *) echo "dev.sh: unknown option $1" >&2; exit 2 ;;
esac

# ------------------------------------------------------------ toolchain ------
export CARGO_HOME="$ROOT/.toolchain/cargo"
export RUSTUP_HOME="$ROOT/.toolchain/rustup"
if [[ -d "$CARGO_HOME/bin" ]]; then
  export PATH="$CARGO_HOME/bin:$PATH"
fi
export npm_config_cache="$ROOT/.scratch/npm-cache"

log() { printf '\033[36m[dev]\033[0m %s\n' "$*"; }

if ! command -v cargo >/dev/null 2>&1; then
  echo "dev: cargo is not available." >&2
  echo "     Install rustup into the workspace with:" >&2
  echo "       curl -sSf https://sh.rustup.rs -o /tmp/rustup-init.sh" >&2
  echo "       CARGO_HOME=$CARGO_HOME RUSTUP_HOME=$RUSTUP_HOME \\" >&2
  echo "         sh /tmp/rustup-init.sh -y --profile minimal --no-modify-path" >&2
  exit 1
fi

# --------------------------------------------------------------- payload -----
PAYLOAD_DIR="$ROOT/src-tauri/payload"
HOST_TAG="$(node -e 'const os=process.platform, a=process.arch==="arm64"?"arm64":"x64"; console.log(`${os}-${a}`)')"

mkdir -p "$PAYLOAD_DIR"
if ! ls "$PAYLOAD_DIR"/dsh-payload-* >/dev/null 2>&1; then
  log "vendoring the DSH runtime payload for $HOST_TAG (first run, ~1 min)"
  bash scripts/vendor-runtime.sh
fi
for archive in "$ROOT"/dist/dsh-payload-*; do
  [[ -e "$archive" ]] || continue
  cp -f "$archive" "$PAYLOAD_DIR/"
done
log "payload staged: $(ls -1 "$PAYLOAD_DIR" | tr '\n' ' ')"

# ----------------------------------------------------------------- icons -----
if [[ ! -f src-tauri/icons/icon.png ]]; then
  log "generating icons"
  node scripts/make-icons.mjs
fi

# ----------------------------------------------------------------- build -----
if ! command -v cargo-tauri >/dev/null 2>&1; then
  log "installing the Tauri CLI (cargo install tauri-cli --version ^2)"
  cargo install tauri-cli --version "^2" --locked
fi

case "$MODE" in
  build)
    log "payload ready — nothing else to do (--build)"
    ;;
  release)
    log "building a release bundle"
    cargo tauri build
    ;;
  run)
    log "starting the app in dev mode"
    cargo tauri dev
    ;;
esac
