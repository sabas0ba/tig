//! QEMU 上のベアメタルで tig-core を動かす例。
//!
//! flash に埋め込んだ bundle (demo.bundle) を解析し、全 ref から到達できる
//! commit 数と HEAD の subject を求めて出力し、成功マーカ `TIG-OK ...` を
//! 出したうえで QEMU を終了する。ランタイム (vector table / reset / 出力 /
//! 終了) は arch ごとに手書きし、外部 crate を使わない。
//!
//! 対応: thumbv7em-none-eabi (QEMU mps2-an386) / riscv32imac / riscv64gc
//! (QEMU virt)。本 crate はベアメタル (target_os = "none") 専用で、host 向けは
//! ビルドが通るだけのスタブになる (CI は各ベアメタル target で明示的にビルド・
//! 実行する)。

#![no_std]
#![cfg_attr(target_os = "none", no_main)]

#[cfg(target_os = "none")]
extern crate alloc;

#[cfg(all(target_os = "none", target_arch = "arm"))]
#[path = "arch_arm.rs"]
mod arch;
#[cfg(all(
    target_os = "none",
    any(target_arch = "riscv32", target_arch = "riscv64")
))]
#[path = "arch_riscv.rs"]
mod arch;

#[cfg(target_os = "none")]
mod alloc_impl;

#[cfg(target_os = "none")]
mod firmware {
    use core::fmt::Write;

    use tig_core::bundle::Bundle;
    use tig_core::history::Walk;

    use crate::arch;

    /// flash に置く demo bundle (deterministic に生成した固定ファイル)。
    static DEMO_BUNDLE: &[u8] = include_bytes!("../../demo.bundle");

    /// 全 arch 共通の本体。console に出力し、期待どおり (commit 3 件) なら true。
    fn run(console: &mut arch::Console) -> bool {
        let Ok(bundle) = Bundle::parse(DEMO_BUNDLE) else {
            let _ = console.write_str("TIG-FAIL parse\n");
            return false;
        };

        let mut walk = Walk::new(&bundle.pack);
        for (_, oid) in &bundle.refs {
            if walk.push(*oid).is_err() {
                let _ = console.write_str("TIG-FAIL push\n");
                return false;
            }
        }

        let mut count: u32 = 0;
        let mut head_subject = [0u8; 64];
        let mut head_len = 0usize;
        for (i, item) in walk.enumerate() {
            let Ok(walked) = item else {
                let _ = console.write_str("TIG-FAIL walk\n");
                return false;
            };
            if i == 0
                && let Ok(commit) = walked.commit()
            {
                let subject = commit.message.split(|&b| b == b'\n').next().unwrap_or(b"");
                head_len = subject.len().min(head_subject.len());
                head_subject[..head_len].copy_from_slice(&subject[..head_len]);
            }
            count += 1;
        }

        // マーカ行。テストはこの count と subject を検証する。
        let _ = write!(console, "TIG-OK count={count} head=");
        for &b in &head_subject[..head_len] {
            let _ = console.write_char(b as char);
        }
        let _ = console.write_char('\n');
        count == 3
    }

    /// arch 側の entry (reset / _start) から呼ばれる共通処理。戻らない。
    pub fn main() -> ! {
        let mut console = arch::Console::new();
        let ok = run(&mut console);
        let _ = console.write_str(if ok { "TIG-DONE\n" } else { "TIG-DONE-FAIL\n" });
        arch::exit(!ok as u32)
    }
}

#[cfg(target_os = "none")]
#[panic_handler]
fn panic(_info: &core::panic::PanicInfo<'_>) -> ! {
    use core::fmt::Write;
    let mut console = arch::Console::new();
    let _ = console.write_str("TIG-PANIC\n");
    arch::exit(2)
}

// host (target_os != none) 向けはランタイムを持たない。ビルドが通るための
// スタブ。QEMU での実行はベアメタル target 側で行う。
#[cfg(not(target_os = "none"))]
fn main() {}
