# `nix flake check` (`make check`) が実行する検査。
#
# ローカル、CI、コンテナのいずれでも同一の derivation が実行される。crate の外部
# 依存が無いため、cargo はネットワーク遮断された sandbox 内で --offline のまま
# 完結する。
{
  pkgs,
  toolchain,
  src,
}:

let
  # 各検査で共通に使用するヘルパー。検査が成功した場合のみ $out を生成する。
  mkCheck =
    name: deps: script:
    pkgs.runCommandLocal "check-${name}" { nativeBuildInputs = deps; } ''
      cd ${src}
      ${script}
      touch "$out"
    '';

  # src は store 内で読み取り専用のため、cargo の書き込み先を $TMPDIR へ向ける。
  cargoEnv = ''
    export HOME="$TMPDIR"
    export CARGO_HOME="$TMPDIR/cargo-home"
    export CARGO_TARGET_DIR="$TMPDIR/target"
  '';
in
{
  # Nix コードが nixfmt で整形済みであること。
  nixfmt = mkCheck "nixfmt" [ pkgs.nixfmt pkgs.findutils ] ''
    find . -type f -name '*.nix' -exec nixfmt --check {} +
  '';

  # Nix コードの静的解析。
  statix = mkCheck "statix" [ pkgs.statix ] ''
    statix check .
  '';

  # 未使用の let 束縛および関数引数の検出。
  deadnix = mkCheck "deadnix" [ pkgs.deadnix ] ''
    deadnix --fail .
  '';

  # Rust コードが rustfmt で整形済みであること。
  rustfmt = mkCheck "rustfmt" [ toolchain ] ''
    ${cargoEnv}
    cargo fmt --check
  '';

  # Rust コードの静的解析。警告をエラーとして扱う。
  clippy = mkCheck "clippy" [ toolchain ] ''
    ${cargoEnv}
    cargo clippy --offline --locked --workspace --all-targets --all-features -- -D warnings
  '';

  # 単体テストおよび git との差分テスト。後者はフィクスチャの生成に git を使う。
  test = mkCheck "test" [ toolchain pkgs.git ] ''
    ${cargoEnv}
    cargo test --offline --locked --workspace --all-features
  '';

  # core crate (no_std) が WASM 向けにビルドできること。
  build-wasm32 = mkCheck "build-wasm32" [ toolchain ] ''
    ${cargoEnv}
    cargo build --offline --locked -p tig-core --all-features --target wasm32-unknown-unknown
  '';

  # core crate (no_std) が Cortex-M (組み込み) 向けにビルドできること。
  build-thumbv7em = mkCheck "build-thumbv7em" [ toolchain ] ''
    ${cargoEnv}
    cargo build --offline --locked -p tig-core --all-features --target thumbv7em-none-eabi
  '';
}
