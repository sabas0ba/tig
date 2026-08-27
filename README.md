# tig

no_std Rust による git client。ブラウザ (WASM)、組み込み (Cortex-M 等)、通常のホスト環境の全てで同一の core を動かすことを目的とする。

## 特徴

- 外部 crate 依存なし。`core` + `alloc` のみで動作し、zlib inflate と SHA-1 も自前実装
- I/O を持たない設計。入力は常に `&[u8]` で受け取り、組み込みでは flash 上のデータを直接参照できる
- feature gate による機能の切り離し。ネットワークを使わない用途 (bundle で配布した repository の履歴表示等) では該当機能のみをリンクする

## 構成

| crate | 内容 |
|---|---|
| `core` (tig-core) | no_std の本体。object / pack / bundle の解析、history walk、protocol v2 |
| `cli` (tig-cli) | 動作確認用 CLI (std)。clone と bundle の refs / log / cat-file |
| `web` (tig-web) | ブラウザ向け frontend。C ABI + 手書き JS glue (wasm-bindgen 不使用) |

tig-core の feature:

| feature | 内容 |
|---|---|
| (常時) | oid、SHA-1、zlib inflate、loose object の parse |
| `pack` | packfile v2 の解析と delta 解決 |
| `bundle` | bundle v2/v3 の読み書き (`pack` を内包) |
| `history` | committer date 順の history walk |
| `transport-http` | protocol v2 の request 構築と response 解析 (sans-io) |
| `fetch` | smart HTTP からの clone 状態機械 (`transport-http` + `bundle`) |

## 使用例 (CLI)

```console
$ tig clone http://host/repo.git --depth 1   # smart HTTP から bundle へ clone
$ git bundle create repo.bundle --all        # またはオフライン配布 (通常の git)
$ tig refs repo.bundle
$ tig log repo.bundle --ref refs/heads/main -n 10
$ tig cat-file repo.bundle <oid>
```

CLI の clone は http:// のみ対応する (TLS は依存ゼロでは持たない)。https はブラウザ frontend の fetch が担う。

## 使用例 (web)

```console
$ make serve   # wasm をビルドして http://127.0.0.1:8000 で配信
```

bundle ファイルを開いて refs / log / commit を閲覧できるほか、smart HTTP の URL から直接 clone できる (対象サーバが CORS を許可している場合。同一オリジンまたは proxy 経由を推奨)。

## GitHub Pages (docpages + playground)

main への push 時に CI が `make site` の生成物 (landing + rustdoc + web frontend) を gh-pages branch へ配置する。配信の有効化は repository の Settings → Pages で「Deploy from a branch」の gh-pages (root) を一度だけ指定する。ローカルでの確認は `make site` の後に `_site/` を任意の静的サーバで配信する。

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
