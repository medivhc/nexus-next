#!/usr/bin/env bash
# Wrapper around repo's tools/build-web.sh — ensures deps present, builds to web/dist
set -euo pipefail

cd "$(dirname "$0")/../../.."   # repo root

# Check prerequisites
for cmd in cargo wasm-bindgen wasm-opt clang npm; do
  if ! command -v "$cmd" >/dev/null 2>&1; then
    echo "ERROR: missing prerequisite: $cmd" >&2
    echo "Install: cargo (rustup), wasm-bindgen-cli (cargo install), wasm-opt (binaryen), clang, npm" >&2
    exit 1
  fi
done

# Verify wasm32 target
if ! rustup target list --installed | grep -q 'wasm32-unknown-unknown'; then
  echo "ERROR: wasm32-unknown-unknown target not installed" >&2
  echo "Run: rustup target add wasm32-unknown-unknown" >&2
  exit 1
done

# Run the repo's build script
tools/build-web.sh

# Verify output
if [[ ! -f web/dist/index.html || ! -f web/dist/pkg/nexus_web_bg.wasm ]]; then
  echo "ERROR: web/dist missing expected files" >&2
  exit 1
fi

echo "✅ web/dist ready"
ls -la web/dist/ web/dist/pkg/