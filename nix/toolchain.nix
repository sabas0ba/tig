# Rust toolchain の単一情報源。
#
# stable の版は flake.nix で固定した fenix の rev が決める (現在 1.97.1)。
# rustup 環境 (CI を含む) は rust-toolchain.toml を参照するため、版を更新する
# 場合は両方を同時に変更する。
{ fenix }:

fenix.combine [
  (fenix.stable.withComponents [
    "cargo"
    "clippy"
    "rustc"
    "rustfmt"
    "rust-std"
  ])
  # クロスビルド対象の rust-std。core crate (no_std) のビルド検証に使用する。
  fenix.targets.wasm32-unknown-unknown.stable.rust-std
  fenix.targets.thumbv7em-none-eabi.stable.rust-std
]
