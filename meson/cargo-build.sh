#!/usr/bin/env bash
# meson → cargo 桥接:release 构建生产二进制并复制进 @OUTDIR@。
#
# 用法: cargo-build.sh <源码根目录> <输出目录> <cargo target 目录>
# 仅构建 fdebi-gtk 与 fdebi-daemon;fdebi-core CLI 仅用于开发测试,不构建。
set -euo pipefail

SRC="$1"
OUT="$2"
CARGO_TARGET_DIR="$3"

cd "$SRC"
CARGO_TARGET_DIR="$CARGO_TARGET_DIR" cargo build --release --locked \
    -p fdebi-gtk -p fdebi-daemon

install -m755 "$CARGO_TARGET_DIR/release/fdebi-gtk"    "$OUT/fdebi-gtk"
install -m755 "$CARGO_TARGET_DIR/release/fdebi-daemon" "$OUT/fdebi-daemon"
