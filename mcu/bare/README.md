# tig-bare-example

QEMU 上のベアメタルで tig-core を実行する例。staticlib の例 (`../`) が「firmware へ link する」側なのに対し、こちらは「実際に動くことを QEMU で検証する」側である。ランタイム (vector table / reset / 出力 / 終了) も外部 crate を使わず手書きしており、依存ゼロの方針は変わらない。

flash に埋め込んだ `../demo.bundle` を解析し、全 ref から到達できる commit 数と HEAD の subject を出力する。

```console
$ scripts/qemu-bare.sh arm        # Cortex-M4  (QEMU mps2-an386)
$ scripts/qemu-bare.sh riscv32    # RISC-V 32  (QEMU virt)
$ scripts/qemu-bare.sh riscv64    # RISC-V 64  (QEMU virt)
TIG-OK count=3 head=commit 3: embedded demo
TIG-DONE
OK (arm)
```

## 構成

| ファイル | 内容 |
|---|---|
| `src/main.rs` | 共通の本体 (bundle 解析 → commit 数と subject を出力) と panic handler |
| `src/arch_arm.rs` | Cortex-M ランタイム: reset ベクタ、.data/.bss 初期化、ARM semihosting による出力・終了 |
| `src/arch_riscv.rs` | RISC-V ランタイム: `_start`、.bss ゼロ埋め、NS16550 UART 出力と sifive_test での終了 |
| `src/alloc_impl.rs` | 静的バッファ上の bump allocator |
| `link-arm.ld` / `link-riscv.ld` | リンカスクリプト (QEMU マシンのメモリマップに対応) |

出力は semihosting (ARM) と MMIO UART (RISC-V) に流れ、QEMU が自走停止する。allocator と panic handler は例示であり、実機では各環境の実装に差し替える。本 crate は workspace から exclude された独立 crate である (no_std bin は panic=abort の profile を要するため)。
