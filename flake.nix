{
  description = "tig: no_std Rust による WASM / 組み込み向け git client";

  inputs = {
    # 入力はリビジョンで固定する。ブランチ名による参照は flake.lock が無い環境で
    # 取得結果が変動するため使用しない。nixpkgs は sabas0ba/dotfiles と同一の rev。
    nixpkgs.url = "github:NixOS/nixpkgs/597283ad8aa0b331c788e97c4c262d58877074ef"; # nixos-26.05

    # Rust toolchain の供給源。nixpkgs の rustc は host 以外の rust-std を持たず、
    # wasm32-unknown-unknown / thumbv7em-none-eabi のクロスビルドに使えないため
    # fenix を用いる。fenix は公式 static.rust-lang.org の配布物を SHA256 検証付きで
    # 取得する。この rev の stable は 1.97.1 (rust-toolchain.toml と一致させること)。
    fenix = {
      url = "github:nix-community/fenix/d57340fe40c2ee12f86c0e087d2239d682b54eb0"; # 2026-08-19
      inputs.nixpkgs.follows = "nixpkgs";
    };
  };

  outputs =
    {
      self,
      nixpkgs,
      fenix,
    }:
    let
      systems = [
        "x86_64-linux"
        "aarch64-linux"
        "x86_64-darwin"
        "aarch64-darwin"
      ];

      forAllSystems =
        f:
        nixpkgs.lib.genAttrs systems (
          system:
          f {
            pkgs = import nixpkgs {
              inherit system;
              config = { };
              overlays = [ ];
            };
            toolchain = import ./nix/toolchain.nix { fenix = fenix.packages.${system}; };
          }
        );
    in
    {
      # `nix develop` および direnv の `use flake` が使用する開発シェル。
      devShells = forAllSystems (
        { pkgs, toolchain }:
        {
          default = import ./nix/devshell.nix { inherit pkgs toolchain; };
        }
      );

      # `nix flake check` および `make check` が実行する検査。
      checks = forAllSystems (
        { pkgs, toolchain }:
        import ./nix/checks.nix {
          inherit pkgs toolchain;
          src = self;
        }
      );

      # `nix fmt` が使用するフォーマッタ。
      #
      # nixfmt を直接指定してはならない。`nix fmt` は引数無しでフォーマッタを起動する
      # ことがあり、その場合 nixfmt は標準入力を読もうとして失敗する。対象が
      # 与えられなかったときに対象を補うラッパを噛ませる。
      formatter = forAllSystems (
        { pkgs, ... }:
        pkgs.writeShellApplication {
          name = "tig-fmt";
          runtimeInputs = [
            pkgs.nixfmt
            pkgs.findutils
          ];
          text = ''
            if [ "$#" -gt 0 ]; then
              nixfmt "$@"
              exit 0
            fi

            find . \
              -type d \( -name .git -o -name .direnv -o -name target -o -name .work \) -prune -o \
              -type f -name '*.nix' -exec nixfmt {} +
          '';
        }
      );
    };
}
