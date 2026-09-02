# 設計

## 目的と制約

- WASM (ブラウザ)、組み込み (Cortex-M 等)、ホストの全てで動く git client を単一の core で提供する
- 外部 crate 依存なし。`core` + `alloc` のみを要求する (`no_std`)
- メモリフットプリントを優先する。速度とのトレードオフでは原則メモリを取る
- 機能は git の subset とする。full git の再実装は目的としない

## 層構成

```
cli (std) ─┐
web (wasm) ─┼─▶ tig-core (no_std + alloc)
mcu (例)  ─┘      ├─ 常時: oid / sha1 / zlib inflate / object parse
                  ├─ feature pack:           packfile v2 + delta 解決
                  ├─ feature bundle:         bundle v2/v3 の読み書き (pack を内包)
                  ├─ feature history:        committer date 順の walk
                  ├─ feature transport-http: protocol v2 state machine (sans-io)
                  ├─ feature fetch:          clone 状態機械 (transport-http + bundle)
                  ├─ feature write:          object 生成 + packfile 書き出し
                  ├─ feature push:           push 状態機械 (receive-pack v0、write を内包)
                  └─ feature checkout:       tree の展開 (filesystem は frontend)
```

- core は I/O を一切行わない。入力は `&[u8]`、出力は `Vec<u8>` または呼び出し側の書き込み
- 環境差 (fetch / OPFS / flash / socket) は frontend 側の責務とする
- transport は sans-io で実装する。clone は「次に送るべき HTTP request を返し、response body を受け取って進む」状態機械 (`clone::Clone`) で、HTTP client 自体は持たない。async executor を core に持ち込まないための選択。CLI は std の最小 HTTP/1.1 client (http のみ)、web は fetch() で同じ状態機械を駆動する
- web frontend は wasm-bindgen 等の glue crate に依存しない。C ABI (長さ前置バッファの受け渡し規約) を web crate が公開し、手書きの JS glue (web/app.js) が接続する

## メモリ設計

- pack / bundle は全体を `&[u8]` として受け取る。組み込みでは flash / メモリマップ上のデータを直接参照でき、RAM へのコピーを要しない
- 常駐する索引は oid 順の `Vec<(Oid, u32)>` のみ (entry あたり 24 byte)
- object の内容は保持せず、読み出しのたびに伸長し直す (メモリを CPU で贖う)。delta chain の解決も再帰を使わず、chain を配列に積んでから base 側から適用する
- inflate は 32 KiB の独立した window を持たない。git object は常に全体を伸長するため、出力バッファ自身を back reference の参照先とする
- Huffman decode は lookup table を持たず、code 長ごとの範囲判定で 1 bit ずつ決定する (数百 byte / 表)
- 圧縮 (feature `write`) は fixed Huffman のみで、符号表を持たない。LZ77 の一致探索は 3 byte hash から直近 1 候補を引くだけで chain を持たず、作業領域は 16 KiB の hash 表と出力 buffer のみ

既知のトレードオフ: parse 時に全 entry を一度 materialize して oid を確定するため、深い delta chain では同じ base を繰り返し伸長する。必要になった時点で、上限付きの base cache を検討する。

## 検証戦略

依存ゼロ方針は dev-dependencies にも適用する。代わりに以下で品質を担保する。

1. 既知ベクタの単体テスト: SHA-1 (RFC 3174)、adler32、zlib (参照実装で生成した固定入力)、delta (手作り入力)
2. 実物の git との差分テスト (core/tests/differential.rs): git でフィクスチャ repository と bundle を生成し、以下を突き合わせる
   - refs と `git bundle list-heads`
   - walk の出力列と `git rev-list --date-order`
   - 全到達 object の内容と `git cat-file` (inflate / delta / SHA-1 の実データ検証を兼ねる)
3. クロスビルド検証: wasm32-unknown-unknown / thumbv7em-none-eabi / riscv32imac-unknown-none-elf / riscv64gc-unknown-none-elf で core をビルドし、no_std 逸脱と word size 依存を CI で検出する
4. QEMU 実行検証: ベアメタルの例 (mcu/bare) を QEMU (Cortex-M4 = mps2-an386、RISC-V = virt) で実際に起動し、埋め込んだ bundle の解析結果 (commit 数と HEAD subject) を出力マーカで確認する。ビルドが通るだけでなく、reset ベクタからの初期化・alloc・object 解析が実機クラスの環境で動くことまで CI で検証する

git が生成する pack は fixed/dynamic Huffman、ofs delta 等を自然に含むため、差分テストが網羅的なテストベクタとして機能する。

## 入力形式として bundle を採る理由

オフライン配布には zip (.git の詰め合わせ) ではなく git bundle を native 入力とする。

- 標準 tooling (`git bundle create`) で生成・検証できる
- 中身が「refs 一覧 + packfile」そのものであり、pack の解析器がそのまま使える
- loose object を含む zip より小さい

## 互換性の範囲

- pack version 2 / bundle v2, v3 (object-format=sha1)。SHA-256 repository は明示的にエラーとする (誤読しない)
- oid は型として分離してあり、SHA-256 対応は `oid` module の拡張で吸収する
- walk の順序は `git log --date-order` と同一 (topology 制約 + committer date 順。date 同点の順序のみ oid で決定的にしており git の投入順とは異なりうる)。default の `git log` (generation number を使う topo 順) は対象外
- 存在しない parent (bundle の prerequisite / shallow) は履歴の境界として扱う
- fetch は protocol v2 のみ対応 (v0/v1 の server は明示的にエラー)。negotiation なしの clone 相当 (常に done) で、深さは `deepen` のみ
- push は receive-pack (protocol v0) を使う (push は protocol v2 に定義が無いため)。report-status を必須とし、送る packfile は非 delta。各 object の zlib stream は fixed Huffman (RFC 1951 3.2.6) + LZ77 で圧縮する。dynamic Huffman と delta 生成は持たず、圧縮率より実装の小ささを優先する。圧縮が効かない object (乱数に近い blob 等) は stored block に落とす
- 生成する object (tree / commit) は git の plumbing (mktree / commit-tree) と oid が一致することを差分テストで検証している。tree の並びは正規順 (directory は名前に '/' を補って比較)
- shallow clone の bundle 表現: bundle 形式には shallow graft が無いため、「pack に含まれない親を prerequisite に記録する」までを行う。複数の tip を depth 付きで clone した場合、tip 同士が pack 内で親子だと walk はそこを辿る (git の shallow clone は graft で打ち切る点が異なる)。また prerequisite 付き bundle は形式の定義上 incremental bundle であり、git はその commit を持つ repository でしか verify / clone できない。tig 自身の閲覧には支障ない。prerequisite を省略する表現は「宣言なしに object が欠けた bundle」となり悪化するため採らない

## ロードマップ

- M1–M3 (実装済み): 環境、primitives、object / pack / bundle、history walk、CLI、差分テスト
- M4 (実装済み): transport-http (protocol v2 の sans-io state machine)、shallow fetch、CLI clone (git http-backend との end-to-end 差分テスト付き)、web frontend、GitHub Pages (docpages + playground)。ブラウザからの clone は対象サーバの CORS 許可が前提 (同一オリジンまたは proxy 経由を推奨)
- M5 (実装済み): object 生成と packfile 書き出し (`write`)、receive-pack への push (`push`、git fsck --strict を通る)、tree の展開 (`checkout`)、firmware へ link する組み込み例 (mcu/)
- M6 (実装中): 送信 pack の fixed Huffman 圧縮 (実装済み)、delta chain の base cache 等の性能改善、web frontend からの push、SHA-256 repository 対応

## toolchain の固定

- Nix: nixpkgs と fenix (公式 static.rust-lang.org の配布物を SHA256 検証付きで取得する) を rev で固定。nixpkgs は sabas0ba/dotfiles と同一 rev
- rustup (CI を含む): rust-toolchain.toml で同一の版・component・target を固定
- 版の更新は flake.nix の fenix rev と rust-toolchain.toml を同時に変更する
