#!/usr/bin/env bash
# 构建网页节点的页面到 web/dist：WebAssembly（cargo + wasm-bindgen）+ 页面文件 + xterm.js。
# 需要：rustup 的 wasm32-unknown-unknown 目标、与 Cargo.lock 同版本的 wasm-bindgen 命令行、
# clang（编译 ring）、binaryen ≥ 123 的 wasm-opt（旧版会生成浏览器加载不了的文件，用 WASM_OPT 指定路径）、npm。
set -euo pipefail
cd "$(dirname "$0")/.."
out=web/dist
mkdir -p "$out/vendor"

# ring 编译成 WebAssembly 需要 clang。
export CC_wasm32_unknown_unknown="${CC_wasm32_unknown_unknown:-clang}"
export RUSTFLAGS="${RUSTFLAGS:-} --cfg getrandom_backend=\"wasm_js\""
cargo build --profile wasm --target wasm32-unknown-unknown -p nexus-web
wasm-bindgen --target web --no-typescript --out-dir "$out/pkg" target/wasm32-unknown-unknown/wasm/nexus_web.wasm
"${WASM_OPT:-wasm-opt}" -Oz --enable-bulk-memory --enable-nontrapping-float-to-int -o "$out/pkg/nexus_web_bg.wasm" "$out/pkg/nexus_web_bg.wasm"

cp web/index.html web/app.js web/app.css "$out/"

# xterm.js：版本固定，文件放进页面目录，不从第三方站点加载。
tmp=$(mktemp -d)
(cd "$tmp" && npm pack --silent @xterm/xterm@5.5.0 @xterm/addon-fit@0.10.0 >/dev/null && for f in *.tgz; do tar xzf "$f" && mv package "${f%.tgz}"; done)
cp "$tmp"/xterm-xterm-5.5.0/css/xterm.css "$out/vendor/"
cp "$tmp"/xterm-xterm-5.5.0/lib/xterm.js "$out/vendor/"
cp "$tmp"/xterm-addon-fit-0.10.0/lib/addon-fit.js "$out/vendor/"

echo "页面已构建到 $out："
ls -la "$out" "$out/pkg"
gzip -9 -c "$out/pkg/nexus_web_bg.wasm" | wc -c | awk '{printf "wasm gzip 后 %d KB\n", $1/1024}'
