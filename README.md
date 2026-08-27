# tig

no_std Rust による git client。ブラウザ (WASM)、組み込み (Cortex-M 等)、通常のホスト環境の全てで同一の core を動かすことを目的とする。

## 特徴

- 外部 crate 依存なし。`core` + `alloc` のみで動作し、zlib inflate と SHA-1 も自前実装
- I/O を持たない設計。入力は常に `&[u8]` で受け取り、組み込みでは flash 上のデータを直接参照できる
- feature gate による機能の切り離し。ネットワークを使わない用途 (bundle で配布した repository の履歴表示等) では該当機能のみをリンクする

## 構成

| crate | 内容 |
|---|---|
| `core` (tig-core) | no_std の本体。object / pack / bundle の解析と history walk |
| `cli` (tig-cli) | 動作確認用 CLI (std)。bundle の refs / log / cat-file |

tig-core の feature:

| feature | 内容 |
|---|---|
| (常時) | oid、SHA-1、zlib inflate、loose object の parse |
| `pack` | packfile v2 の解析と delta 解決 |
| `bundle` | bundle v2/v3 の読み込み (`pack` を内包) |
| `history` | committer date 順の history walk |

## 使用例 (CLI)

```console
$ git bundle create repo.bundle --all   # 配布側 (通常の git)
$ tig refs repo.bundle
$ tig log repo.bundle --ref refs/heads/main -n 10
$ tig cat-file repo.bundle <oid>
```

## 開発環境

Nix + direnv を基本とするが、rustup でも同一の toolchain (rust-toolchain.toml で固定) が入る。

```console
$ direnv allow        # または make shell
$ make test           # 単体テスト + git との差分テスト
$ make check          # nix flake check (fmt / lint / test / クロスビルド)
$ make wasm thumb riscv  # クロスビルド
```

検証は「実物の git との差分テスト」を主軸とする。git でフィクスチャの repository と bundle を生成し、refs・履歴の順序・object の内容を本実装の解析結果と突き合わせる (core/tests/differential.rs)。

## 設計

[docs/design.md](docs/design.md) を参照。ロードマップ (fetch / push、smart HTTP transport の sans-io 実装) も同文書に記載する。

## License

Apache-2.0
