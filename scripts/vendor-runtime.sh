#!/usr/bin/env bash
# vendor-runtime.sh — build the self-contained DSH runtime payload for DeepSeek Harness Desktop.
#
# The payload is a single archive holding everything the app needs to run with
# NO external dependency at all:
#
#   runtime/node/<exe>            official Node.js runtime (with npm + corepack)
#   vendor/package.json           npm-compatible manifest (drives auto-update)
#   vendor/pnpm-workspace.yaml    pnpm settings (hoisted linker + build allowlist)
#   vendor/node_modules/**        @deepseek-ai/dsh + its FULL dependency closure
#   manifest.json                 version + platform stamped by this script
#
# The app extracts the archive into its per-user data directory on first launch
# and then runs `node vendor/node_modules/@deepseek-ai/dsh/lib/bin.js`.
# Because the payload lives in a WRITABLE directory with a normal npm/pnpm
# layout, `npm install @deepseek-ai/dsh@latest` / `pnpm update` keeps working —
# that is what preserves DSH's own auto-update path.
#
# Usage:
#   scripts/vendor-runtime.sh [--version <x.y.z>] [--node <x.y.z>] [--out dir]
#                             [--platform <id>] [--arch <id>] [--compression zstd|gzip]
#
# Defaults target the host (node's process.platform -> the tarball ids below).

set -euo pipefail

# ---------------------------------------------------------------- args -------
DSH_VERSION="latest"
NODE_VERSION=""
OUT_DIR=""
PLATFORM=""
ARCH=""
COMPRESSION="zstd"
SKIP_NODE=0

while [[ $# -gt 0 ]]; do
  case "$1" in
    --version)     DSH_VERSION="$2"; shift 2 ;;
    --node)        NODE_VERSION="$2"; shift 2 ;;
    --out)         OUT_DIR="$2"; shift 2 ;;
    --platform)    PLATFORM="$2"; shift 2 ;;
    --arch)        ARCH="$2"; shift 2 ;;
    --compression) COMPRESSION="$2"; shift 2 ;;
    --skip-node)   SKIP_NODE=1; shift ;;
    -h|--help)     sed -n '2,25p' "$0"; exit 0 ;;
    *) echo "unknown argument: $1" >&2; exit 2 ;;
  esac
done

ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
OUT_DIR="${OUT_DIR:-$ROOT/dist}"

# ------------------------------------------------------- host detection ------
detect_platform() {
  case "$(uname -s)" in
    Darwin) echo darwin ;;
    Linux)  echo linux ;;
    MINGW*|MSYS*|CYGWIN*) echo win32 ;;
    *) echo "unsupported OS: $(uname -s)" >&2; exit 1 ;;
  esac
}
detect_arch() {
  case "$(uname -m)" in
    arm64|aarch64) echo arm64 ;;
    x86_64|amd64)  echo x64 ;;
    *) echo "unsupported arch: $(uname -m)" >&2; exit 1 ;;
  esac
}
PLATFORM="${PLATFORM:-$(detect_platform)}"
ARCH="${ARCH:-$(detect_arch)}"

# node-os-arch ids used by the Node.js distribution server.
case "$PLATFORM" in
  darwin) NODE_OS=darwin ;;
  linux)  NODE_OS=linux ;;
  win32)  NODE_OS=win ;;
  *) echo "unsupported --platform $PLATFORM" >&2; exit 1 ;;
esac
NODE_ARCH="$ARCH"

NODE_EXE="node"
[[ "$PLATFORM" == "win32" ]] && NODE_EXE="node.exe"

# ------------------------------------------------------------ helpers --------
need() {
  command -v "$1" >/dev/null 2>&1 || { echo "vendor-runtime: '$1' is required but not on PATH" >&2; exit 1; }
}
log() { printf '\033[36m[vendor]\033[0m %s\n' "$*"; }
# Node is a native binary: under MSYS/MinGW (Windows runners) it cannot resolve
# POSIX paths like /tmp/... — hand it a native path when cygpath exists.
node_path() {
  if command -v cygpath >/dev/null 2>&1; then cygpath -m "$1"; else printf '%s' "$1"; fi
}


need node
need npm
need tar
need curl || true

WORK="$(mktemp -d "${TMPDIR:-/tmp}/dsh-vendor-XXXXXX")"
cleanup() { rm -rf "$WORK"; }
trap cleanup EXIT

# --------------------------------------------- 0. resolve versions -----------
if [[ -z "$NODE_VERSION" ]]; then
  # Prefer the node that runs this script — it is known to satisfy DSH.
  NODE_VERSION="$(node -p "process.versions.node")"
fi
if [[ "$NODE_VERSION" == v* ]]; then NODE_VERSION="${NODE_VERSION#v}"; fi

if ! command -v pnpm >/dev/null 2>&1; then
  echo "vendor-runtime: pnpm is required to materialize the dependency closure" >&2
  echo "  install with:  npm i -g pnpm   (or corepack enable pnpm)" >&2
  exit 1
fi

log "target       : $PLATFORM-$ARCH"
log "dsh version  : $DSH_VERSION"
log "node version : $NODE_VERSION"
log "output dir   : $OUT_DIR"

# npm cache + pnpm store are kept inside the workspace so the vendoring never
# depends on (or pollutes) a user-level cache that may not be writable.
export npm_config_cache="$WORK/npm-cache"
export npm_config_update_notifier=false

# --------------------------------------- 1. download the node runtime --------
STAGE="$WORK/stage"
mkdir -p "$STAGE/runtime/node" "$STAGE/vendor"
RESOLVED_NODE=""

if [[ "$SKIP_NODE" == "1" ]]; then
  log "skipping node runtime download (--skip-node); the app will use system node"
  NODE_DIST=""
else
  if [[ "$NODE_OS" == "win" ]]; then
    NODE_PKG="node-v${NODE_VERSION}-${NODE_OS}-${NODE_ARCH}.zip"
  else
    NODE_PKG="node-v${NODE_VERSION}-${NODE_OS}-${NODE_ARCH}.tar.gz"
  fi
  NODE_BASE="${DSH_NODE_MIRROR:-https://nodejs.org/dist}"
  NODE_URL="$NODE_BASE/v${NODE_VERSION}/$NODE_PKG"

  log "downloading  : $NODE_URL"
  if ! curl -fsSL --retry 3 --retry-delay 2 -o "$WORK/$NODE_PKG" "$NODE_URL"; then
    echo "vendor-runtime: could not download the node runtime from $NODE_URL" >&2
    echo "  (set DSH_NODE_MIRROR to a mirror base URL, e.g. https://npmmirror.com/mirrors/node)" >&2
    exit 1
  fi

  mkdir -p "$WORK/node-dist"
  if [[ "$NODE_OS" == "win" ]]; then
    need unzip
    unzip -q "$WORK/$NODE_PKG" -d "$WORK/node-dist"
  else
    tar -xzf "$WORK/$NODE_PKG" -C "$WORK/node-dist"
  fi
  NODE_ROOT="$(find "$WORK/node-dist" -maxdepth 1 -mindepth 1 -type d | head -1)"
  [[ -n "$NODE_ROOT" ]] || { echo "vendor-runtime: malformed node archive" >&2; exit 1; }

  if [[ "$NODE_OS" == "win" ]]; then
    cp "$NODE_ROOT/node.exe" "$STAGE/runtime/node/"
    # npm/corepack ship as cmd shims pointing at node_modules/npm — keep the
    # whole layout so `npm` remains runnable from the bundled runtime.
    cp -R "$NODE_ROOT/node_modules" "$STAGE/runtime/node/" 2>/dev/null || true
    cp "$NODE_ROOT/npm.cmd" "$NODE_ROOT/npx.cmd" "$NODE_ROOT/corepack.cmd" "$STAGE/runtime/node/" 2>/dev/null || true
  else
    cp "$NODE_ROOT/bin/node" "$STAGE/runtime/node/"
    chmod +x "$STAGE/runtime/node/node"
    cp -R "$NODE_ROOT/lib" "$STAGE/runtime/node/" 2>/dev/null || true
    cp "$NODE_ROOT/bin/npm" "$NODE_ROOT/bin/npx" "$STAGE/runtime/node/" 2>/dev/null || true
    [[ -f "$NODE_ROOT/bin/corepack" ]] && cp "$NODE_ROOT/bin/corepack" "$STAGE/runtime/node/" || true
  fi
  RESOLVED_NODE="$("$STAGE/runtime/node/$NODE_EXE" --version 2>/dev/null || echo "v$NODE_VERSION")"
  log "node runtime : $RESOLVED_NODE ($(du -sh "$STAGE/runtime/node" | cut -f1))"
fi

# ------------------------------------------- 1b. bundle pnpm ------------------
# DSH itself shells out to pnpm (`dsh plugin add|remove` forwards to pnpm inside
# the profile directory) and this shell uses it as the preferred package manager
# for its own updates. Shipping pnpm *inside the payload* is what makes the app
# genuinely standalone: no global pnpm, no corepack download, works offline.
PNPM_IN_PAYLOAD=0
if [[ "${DSH_SKIP_PNPM:-0}" != "1" ]]; then
  PNPM_VERSION="$(pnpm --version 2>/dev/null || true)"
  if [[ -n "$PNPM_VERSION" ]]; then
    log "bundling     : pnpm $PNPM_VERSION (self-contained package-manager path)"
    mkdir -p "$WORK/pnpm-dist"
    (
      cd "$WORK/pnpm-dist"
      npm pack "pnpm@$PNPM_VERSION" >/dev/null 2>&1
      tar -xzf pnpm-*.tgz
    )
    if [[ -f "$WORK/pnpm-dist/package/bin/pnpm.cjs" || -f "$WORK/pnpm-dist/package/bin/pnpm.mjs" ]]; then
      mkdir -p "$STAGE/runtime/pnpm"
      cp -R "$WORK/pnpm-dist/package/." "$STAGE/runtime/pnpm/"
      PNPM_IN_PAYLOAD=1
    else
      log "warning      : the pnpm package layout was not recognised; skipping"
    fi
  else
    log "notice       : pnpm is not available here; the payload will rely on npm"
  fi
fi

# ------------------------------------------------- 1c. pnpm shims -------------
# The payload's bin directory goes first on PATH for every DSH process the app
# spawns, so these shims make a plain `pnpm` call work (DSH forwards `dsh plugin`
# to pnpm) without anything installed globally on the machine.
if [[ "$PNPM_IN_PAYLOAD" == "1" ]]; then
  mkdir -p "$STAGE/runtime/bin"
  PNPM_ENTRY="pnpm.cjs"
  [[ -f "$STAGE/runtime/pnpm/bin/pnpm.cjs" ]] || PNPM_ENTRY="pnpm.mjs"

  if [[ "$PLATFORM" == "win32" ]]; then
    printf '@echo off\r\n"%%~dp0..\\node\\node.exe" "%%~dp0..\\pnpm\\bin\\%s" %%*\r\n' "$PNPM_ENTRY" \
      > "$STAGE/runtime/bin/pnpm.cmd"
  else
    cat > "$STAGE/runtime/bin/pnpm" <<EOF
#!/bin/sh
# Generated by vendor-runtime.sh — runs the pnpm bundled in the payload.
here=\$(cd "\$(dirname "\$0")" && pwd)
exec "\$here/../node/node" "\$here/../pnpm/bin/$PNPM_ENTRY" "\$@"
EOF
    chmod +x "$STAGE/runtime/bin/pnpm"
  fi
  log "shims        : runtime/bin/pnpm -> runtime/pnpm/bin/$PNPM_ENTRY"
fi

# --------------------------------- 2. materialize the DSH closure ------------
log "resolving    : @deepseek-ai/dsh@$DSH_VERSION (full closure, builds allowed)"
cd "$STAGE/vendor"

cat > package.json <<EOF
{
  "name": "dsh-desktop-runtime",
  "private": true,
  "version": "0.0.0",
  "description": "Vendored DeepSeek Harness runtime for DeepSeek Harness Desktop (managed directory).",
  "dependencies": {
    "@deepseek-ai/dsh": "$DSH_VERSION"
  }
}
EOF

# hoisted linker => real files, no symlinks into a content-addressable store,
# so the payload is relocatable and survives a plain tar/copy.
#
# `allowBuilds` is the pnpm 12 setting that lets the handful of packages with
# native post-install steps build themselves. Without it pnpm refuses the
# install (ERR_PNPM_IGNORED_BUILDS) and the closure would ship without its
# spawn helper / native addons.
cat > pnpm-workspace.yaml <<'EOF'
packages:
  - .

nodeLinker: hoisted

allowBuilds:
  "@deepseek-ai/dsh-subprocess-local": true
  koffi: true
  node-pty: true
  protobufjs: true
  "@google/genai": true
EOF

# npm fallback settings (used by the in-app updater when pnpm is unavailable).
cat > .npmrc <<'EOF'
node-linker=hoisted
EOF

pnpm install --config.store-dir="$WORK/pnpm-store" --reporter=append-only

# --------------------------- 2b. prune foreign-platform prebuilds ------------
# Several packages ship prebuilds for EVERY platform inside one package
# (node-pty is the big one: ~46 files / ~25 MB of darwin + linux + win32
# binaries). Only `<platform>-<arch>` is ever loaded at runtime, so the rest is
# dead weight in a payload that is built per platform anyway.
PREBUILD_TAG="${PLATFORM}-${ARCH}"      # darwin-arm64 | linux-x64 | win32-arm64 ...
PRUNED_DIRS=0
PRUNED_KB=0
while IFS= read -r prebuilds_dir; do
  [ -d "$prebuilds_dir" ] || continue
  for cand in "$prebuilds_dir"/*/; do
    [ -d "$cand" ] || continue
    base="$(basename "$cand")"
    [ "$base" = "$PREBUILD_TAG" ] && continue
    case "$base" in
      darwin-*|linux-*|win32-*|win-*|freebsd-*)
        kb="$(du -sk "$cand" 2>/dev/null | cut -f1)"
        PRUNED_KB=$((PRUNED_KB + ${kb:-0}))
        PRUNED_DIRS=$((PRUNED_DIRS + 1))
        rm -rf "$cand"
        ;;
    esac
  done
done < <(find "$STAGE/vendor/node_modules" -type d -name prebuilds 2>/dev/null)
if [ "$PRUNED_DIRS" -gt 0 ]; then
  log "pruned       : $PRUNED_DIRS foreign prebuild dirs (~$((PRUNED_KB / 1024)) MB) — kept only $PREBUILD_TAG"
fi

# ------------------------------------------- 3. verify the payload -----------
DSH_PKG="vendor/node_modules/@deepseek-ai/dsh"
[[ -f "$STAGE/$DSH_PKG/lib/bin.js" ]] || { echo "vendor-runtime: bin.js missing at $DSH_PKG" >&2; exit 1; }

RESOLVED_DSH="$(node -p "require('$(node_path "$STAGE/$DSH_PKG/package.json")').version")"
log "dsh resolved : $RESOLVED_DSH"

# Absolute/escaping symlinks would break after extraction: fail loudly.
BAD_LINKS="$(find "$STAGE/vendor" -type l -exec sh -c 'for l; do t=$(readlink "$l"); case "$t" in /*) echo "$l";; esac; done' sh {} + 2>/dev/null || true)"
if [[ -n "$BAD_LINKS" ]]; then
  echo "vendor-runtime: payload contains absolute symlinks (not relocatable):" >&2
  echo "$BAD_LINKS" | head -10 >&2
  exit 1
fi

# --------------------------------------- 4. stamp the manifest ---------------
GENERATED="$(date -u +%Y-%m-%dT%H:%M:%SZ)"
cat > "$STAGE/manifest.json" <<EOF
{
  "schemaVersion": 1,
  "dshVersion": "$RESOLVED_DSH",
  "nodeVersion": "$RESOLVED_NODE",
  "platform": "$PLATFORM",
  "arch": "$ARCH",
  "generatedAt": "$GENERATED"
}
EOF

# --------------------------------------- 5. pack ------------------------------
mkdir -p "$OUT_DIR"
EXT="tar.zst"
USE_ZSTD=1
if [[ "$COMPRESSION" == "gzip" ]]; then
  EXT="tar.gz"
  USE_ZSTD=0
elif ! command -v zstd >/dev/null 2>&1; then
  log "zstd not found; falling back to gzip"
  EXT="tar.gz"
  USE_ZSTD=0
fi

ARCHIVE="$OUT_DIR/dsh-payload-$PLATFORM-$ARCH.$EXT"
rm -f "$ARCHIVE"
log "packing      : $ARCHIVE"
# NOTE: do NOT use `tar --use-compress-program=zstd`. On macOS 27 (bsdtar
# 3.5.3 / libarchive 3.7.4) that path writes an archive whose zstd frames fail
# their own integrity check (`zstd -t` reports "unsupported format") even though
# extraction appears to work. A plain producer/consumer pipe is correct.
if [[ "$USE_ZSTD" == "1" ]]; then
  # -h dereferences the (rare) in-tree symlinks so the archive is relocatable.
  tar -c -h -C "$STAGE" . | zstd -q -T0 -o "$ARCHIVE" -f
  if ! zstd -t "$ARCHIVE" >/dev/null 2>&1; then
    echo "vendor-runtime: the produced zstd archive failed its integrity check" >&2
    exit 1
  fi
else
  tar -c -h -z -C "$STAGE" . > "$ARCHIVE"
  if ! gzip -t "$ARCHIVE" >/dev/null 2>&1; then
    echo "vendor-runtime: the produced gzip archive failed its integrity check" >&2
    exit 1
  fi
fi

SIZE="$(du -sh "$ARCHIVE" | cut -f1)"
SHA="$(node -e "const c=require('crypto'),f=require('fs');const h=c.createHash('sha256');h.update(f.readFileSync(process.argv[1]));console.log(h.digest('hex'))" "$(node_path "$ARCHIVE")")"
# Total tar entries: lets the desktop shell turn the extraction callback into
# real percentage progress on the splash screen (single streaming pass).
log "counting    : tar entries for the progress sidecar…"
# `tar -tf` reads the archive back — but some tar builds (notably the bsdtar
# shipped with Windows runners) have no libzstd and fail on a .tar.zst with
# exit 128. Fall back to counting the staged tree: `tar -c .` emits one entry
# per staged path plus "." itself.
ENTRIES="$(tar -tf "$ARCHIVE" 2>/dev/null | wc -l | tr -d ' ')" || ENTRIES=""
if [ -z "$ENTRIES" ] || [ "$ENTRIES" = "0" ]; then
  ENTRIES=$(( 1 + $(find "$STAGE" -mindepth 1 | wc -l | tr -d ' ') ))
  log "counting    : archive not readable by tar here; counted the staged tree: $ENTRIES entries"
fi

cat > "$OUT_DIR/dsh-payload-$PLATFORM-$ARCH.json" <<EOF
{
  "archive": "$(basename "$ARCHIVE")",
  "sha256": "$SHA",
  "sizeBytes": $(wc -c < "$ARCHIVE" | tr -d ' '),
  "entries": $ENTRIES,
  "dshVersion": "$RESOLVED_DSH",
  "nodeVersion": "$RESOLVED_NODE",
  "platform": "$PLATFORM",
  "arch": "$ARCH"
}
EOF

log "done         : $SIZE  sha256=$SHA"
