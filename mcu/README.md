# tig-mcu-example

firmware へ link する組み込み例。no_std の staticlib として tig-core を包み、bump allocator と C ABI (`tig_mcu_commit_count` / `tig_mcu_head_subject`) を提供する。

```console
$ cargo build --target thumbv7em-none-eabi --release          # Cortex-M4/M7
$ cargo build --target riscv32imac-unknown-none-elf --release # RISC-V 32bit
```

生成される `target/<target>/release/libtig_mcu_example.a` を firmware に link し、flash 上の bundle をそのまま渡す:

```c
int32_t tig_mcu_commit_count(const uint8_t *bundle, size_t len);
int32_t tig_mcu_head_subject(const uint8_t *bundle, size_t len,
                             uint8_t *out, size_t out_len);
```

本 crate は workspace から exclude された独立 crate である (no_std staticlib は panic=abort を要し、profile を crate 単位で設定できないため)。allocator と panic handler は例示であり、実機では各環境の実装に差し替える。
