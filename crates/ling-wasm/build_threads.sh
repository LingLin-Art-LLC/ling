#!/usr/bin/env bash
# Multithreaded browser engine (rayon on a Web Worker pool via wasm-bindgen-rayon).
#   crates/ling-wasm/build_threads.sh            -> crates/ling-wasm/pkg_mt/
# Needs: rustup nightly (+ rust-src), the wasm32-unknown-unknown target, wasm-bindgen
# CLI matching Cargo.lock's wasm-bindgen (0.2.126; wasm-pack caches one), wasm-opt.
# wasm-pack can't do this build: it runs the repo's pinned stable toolchain, and
# threads need -Z build-std. The page must be cross-origin isolated (COOP/COEP) and
# load the engine with a module import — see LingForFun web/play/web/worker_mt.js.
set -euo pipefail
cd "$(dirname "$0")"
export RUSTUP_TOOLCHAIN=nightly
export RUSTFLAGS='-C target-feature=+atomics,+bulk-memory,+mutable-globals,+simd128
 -C link-arg=--shared-memory -C link-arg=--import-memory -C link-arg=--max-memory=4294967296
 -C link-arg=--export=__wasm_init_tls -C link-arg=--export=__tls_size -C link-arg=--export=__tls_align
 -C link-arg=--export=__tls_base -C link-arg=--export=__heap_base -C link-arg=--export=__data_end'
export RUSTFLAGS="${RUSTFLAGS//$'\n'/}"
cargo build --release --target wasm32-unknown-unknown --features threads -Z build-std=panic_abort,std
WASM=../../target/wasm32-unknown-unknown/release/ling_wasm.wasm
"${WASM_BINDGEN:-wasm-bindgen}" "$WASM" --target web --out-dir pkg_mt
"${WASM_OPT:-wasm-opt}" -O2 --enable-threads --enable-simd --enable-bulk-memory --enable-mutable-globals \
  --enable-nontrapping-float-to-int --enable-sign-ext pkg_mt/ling_wasm_bg.wasm -o pkg_mt/ling_wasm_bg.wasm
echo "pkg_mt ready: copy ling_wasm.js, ling_wasm_bg.wasm and snippets/ into a runtime/<hash>/ folder with worker_mt.js as worker.js"
