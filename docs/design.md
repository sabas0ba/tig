# 設計

## 目的と制約

- WASM (ブラウザ)、組み込み (Cortex-M 等)、ホストの全てで動く git client を単一の core で提供する
- 外部 crate 依存なし。`core` + `alloc` のみを要求する (`no_std`)
- メモリフットプリントを優先する。速度とのトレードオフでは原則メモリを取る
- 機能は git の subset とする。full git の再実装は目的としない

## 層構成

```
cli (std) ─┐
web (M4)  ─┼─▶ tig-core (no_std + alloc)
mcu (例)  ─┘      ├─ 常時: oid / sha1 / zlib inflate / object parse
                  ├─ feature pack:    packfile v2 + delta 解決
                  ├─ feature bundle:  bundle v2/v3 (pack を内包)
                  ├─ feature history: committer date 順の walk
                  └─ (M4) feature transport-http: protocol v2 state machine
```

- core は I/O を一切行わない。入力は `&[u8]`、出力は `Vec<u8>` または呼び出し側の書き込み
- 環境差 (fetch / OPFS / flash / socket) は frontend 側の責務とする
- transport (M4) は sans-io で実装する。「入力 byte 列を渡すと、次に送るべき要求と解析結果が返る」状態機械とし、HTTP client 自体は持たない。async executor を core に持ち込まないための選択

## メモリ設計

- pack / bundle は全体を `&[u8]` として受け取る。組み込みでは flash / メモリマップ上のデータを直接参照でき、RAM へのコピーを要しない
- 常駐する索引は oid 順の `Vec<(Oid, u32)>` のみ (entry あたり 24 byte)
- object の内容は保持せず、読み出しのたびに伸長し直す (メモリを CPU で贖う)。delta chain の解決も再帰を使わず、chain を配列に積んでから base 側から適用する
- inflate は 32 KiB の独立した window を持たない。git object は常に全体を伸長するため、出力バッファ自身を back reference の参照先とする
- Huffman decode は lookup table を持たず、code 長ごとの範囲判定で 1 bit ずつ決定する (数百 byte / 表)

既知のトレードオフ: parse 時に全 entry を一度 materialize して oid を確定するため、深い delta chain では同じ base を繰り返し伸長する。必要になった時点で、上限付きの base cache を検討する。

## 検証戦略

依存ゼロ方針は dev-dependencies にも適用する。代わりに以下で品質を担保する。

1. 既知ベクタの単体テスト: SHA-1 (RFC 3174)、adler32、zlib (参照実装で生成した固定入力)、delta (手作り入力)
2. 実物の git との差分テスト (core/tests/differential.rs): git でフィクスチャ repository と bundle を生成し、以下を突き合わせる
   - refs と `git bundle list-heads`
   - walk の出力列と `git rev-list --date-order`
   - 全到達 object の内容と `git cat-file` (inflate / delta / SHA-1 の実データ検証を兼ねる)
3. クロスビルド検証: wasm32-unknown-unknown / thumbv7em-none-eabi で core をビルドし、no_std 逸脱を CI で検出する

git が生成する pack は fixed/dynamic Huffman、ofs delta 等を自然に含むため、差分テストが網羅的なテストベクタとして機能する。

## 入力形式として bundle を採る理由

オフライン配布には zip (.git の詰め合わせ) ではなく git bundle を native 入力とする。

- 標準 tooling (`git bundle create`) で生成・検証できる
- 中身が「refs 一覧 + packfile」そのものであり、pack の解析器がそのまま使える
- loose object を含む zip より小さい

## 互換性の範囲

- pack version 2 / bundle v2, v3 (object-format=sha1)。SHA-256 repository は明示的にエラーとする (誤読しない)
- oid は型として分離してあり、SHA-256 対応は `oid` module の拡張で吸収する
- walk の順序は `git log --date-order` と同一。default の `git log` (generation number を使う topo 順) は対象外
- 存在しない parent (bundle の prerequisite / shallow) は履歴の境界として扱う

## ロードマップ

- M1–M3 (実装済み): 環境、primitives、object / pack / bundle、history walk、CLI、差分テスト
- M4: transport-http (protocol v2 の sans-io state machine) → shallow fetch → web frontend (wasm-bindgen)。ブラウザからの clone は CORS の制約があるため、proxy 前提か対応サーバ限定かを実装前に決める
- M5 以降: commit / push (`write` feature)、checkout (Fs trait 経由)、base cache 等の性能改善

## toolchain の固定

- Nix: nixpkgs と fenix (公式 static.rust-lang.org の配布物を SHA256 検証付きで取得する) を rev で固定。nixpkgs は sabas0ba/dotfiles と同一 rev
- rustup (CI を含む): rust-toolchain.toml で同一の版・component・target を固定
- 版の更新は flake.nix の fenix rev と rust-toolchain.toml を同時に変更する
