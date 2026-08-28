//! RISC-V (rv32/rv64) ランタイム。QEMU virt で動かす。
//!
//! `_start` で stack を設定して rust_main へ入り、.bss をゼロ埋めしてから共通の
//! main を呼ぶ。出力は virt の NS16550 UART (0x1000_0000)、終了は sifive_test
//! デバイス (0x10_0000) を使う。

use core::fmt;

unsafe extern "C" {
    static mut _sbss: u8;
    static mut _ebss: u8;
}

// entry。stack pointer を設定して rust_main へ飛ぶ。`-kernel` は .data (PROGBITS)
// を RAM へ読み込むため、.data のコピーは不要。
core::arch::global_asm!(
    ".section .text.init",
    ".global _start",
    "_start:",
    "la sp, _stack_top",
    "call {main}",
    "1: wfi",
    "j 1b",
    main = sym rust_main,
);

unsafe extern "C" fn rust_main() -> ! {
    unsafe {
        let mut b = &raw mut _sbss;
        let end = &raw mut _ebss;
        while b < end {
            core::ptr::write(b, 0);
            b = b.add(1);
        }
    }
    crate::firmware::main()
}

// --- MMIO -------------------------------------------------------------------

/// virt の NS16550 UART。THR はベース、LSR はベース+5。
const UART_BASE: usize = 0x1000_0000;
const UART_LSR: usize = UART_BASE + 5;
/// LSR の THR empty ビット。
const LSR_THRE: u8 = 0x20;

/// virt の sifive_test デバイス。0x5555 書き込みで pass 終了、0x3333|code<<16 で fail。
const TEST_BASE: usize = 0x10_0000;
const TEST_PASS: u32 = 0x5555;
const TEST_FAIL: u32 = 0x3333;

pub struct Console;

impl Console {
    pub fn new() -> Self {
        Console
    }
}

impl fmt::Write for Console {
    fn write_str(&mut self, s: &str) -> fmt::Result {
        for &byte in s.as_bytes() {
            unsafe {
                while core::ptr::read_volatile(UART_LSR as *const u8) & LSR_THRE == 0 {}
                core::ptr::write_volatile(UART_BASE as *mut u8, byte);
            }
        }
        Ok(())
    }
}

/// QEMU を終了する。code==0 で pass、それ以外は fail として exit code に載せる。
pub fn exit(code: u32) -> ! {
    let value = if code == 0 {
        TEST_PASS
    } else {
        TEST_FAIL | (code << 16)
    };
    unsafe { core::ptr::write_volatile(TEST_BASE as *mut u32, value) };
    loop {
        unsafe { core::arch::asm!("wfi", options(nomem, nostack)) };
    }
}
