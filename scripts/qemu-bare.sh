#!/usr/bin/env bash
#
# ベアメタルの例 (mcu/bare) を QEMU で実行し、出力マーカを検証する。
# CI とローカルの双方から呼ぶ。qemu-system-* は PATH 上にあること。
#
#   使用方法: scripts/qemu-bare.sh <arch>
#     arch: arm | riscv32 | riscv64
#
# 成功時は "TIG-OK count=3 ..." と "TIG-DONE" を出力に確認する。TIG-FAIL /
# TIG-PANIC / マーカ欠落は失敗とする。
set -euo pipefail

arch=${1:?"使用方法: scripts/qemu-bare.sh <arm|riscv32|riscv64>"}
here=$(cd "$(dirname "$0")/.." && pwd)
bare="$here/mcu/bare"

case "$arch" in
arm)
  target=thumbv7em-none-eabi
  qemu="qemu-system-arm"
  qemu_args=(-M mps2-an386 -cpu cortex-m4 -semihosting-config "enable=on,target=native")
  ;;
riscv32)
  target=riscv32imac-unknown-none-elf
  qemu="qemu-system-riscv32"
  qemu_args=(-M virt -bios none)
  ;;
riscv64)
  target=riscv64gc-unknown-none-elf
  qemu="qemu-system-riscv64"
  qemu_args=(-M virt -bios none)
  ;;
*)
  echo "未知の arch: $arch" >&2
  exit 2
  ;;
esac

command -v "$qemu" >/dev/null || {
  echo "$qemu が見つかりません (QEMU を導入してください)" >&2
  exit 2
}

# CARGO_TARGET_DIR が設定されていればそちらへ出力される (nix sandbox 等)。
target_dir="${CARGO_TARGET_DIR:-$bare/target}"
elf="$target_dir/$target/release/tig-bare"
cargo build --manifest-path "$bare/Cargo.toml" --release --target "$target"

# QEMU は semihosting / sifive_test の終了要求で自走停止するが、暴走に備えて
# タイムアウトを掛ける。-nographic で出力を stdout に流す。成否は QEMU の
# 終了コードではなく出力マーカで判定するため、非 0 終了でも出力を取り込む。
output=$(timeout 60 "$qemu" "${qemu_args[@]}" -nographic -kernel "$elf" 2>&1) || true
echo "$output"

if echo "$output" | grep -qE 'TIG-(FAIL|PANIC|DONE-FAIL)'; then
  echo "FAILED ($arch): 失敗マーカを検出" >&2
  exit 1
fi
if ! echo "$output" | grep -q 'TIG-OK count=3 head=commit 3: embedded demo'; then
  echo "FAILED ($arch): 期待するマーカが出力に無い" >&2
  exit 1
fi
if ! echo "$output" | grep -q 'TIG-DONE'; then
  echo "FAILED ($arch): 完了マーカが出力に無い" >&2
  exit 1
fi
echo "OK ($arch)"
