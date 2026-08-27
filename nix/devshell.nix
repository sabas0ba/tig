# `nix develop` および direnv が使用する開発シェルの定義。
#
# 本ファイルはシェルの構成のみを定義し、ツールの一覧は nix/packages.nix に置く。
{ pkgs, toolchain }:

pkgs.mkShellNoCC {
  name = "tig";

  packages = import ./packages.nix { inherit pkgs toolchain; };

  env = {
    # 開発シェル内であることをスクリプトから判定するために使用する。
    TIG_ENV = "nix-develop";

    # ロケールによる挙動の差異を排除する。
    LC_ALL = "C.UTF-8";
  };

  shellHook = ''
    echo "tig dev shell (rustc $(rustc --version | cut -d' ' -f2), ${pkgs.stdenv.hostPlatform.system})"
    echo "  make help: 利用可能な操作の一覧"
  '';
}
