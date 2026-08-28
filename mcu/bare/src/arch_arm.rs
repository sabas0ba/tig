//! Cortex-M (thumbv7em) ランタイム。QEMU mps2-an386 で動かす。
//!
//! reset ハンドラで .data の初期化と .bss のゼロ埋めを行い、共通の main を
//! 呼ぶ。出力と終了は ARM semihosting を使う (QEMU の `-semihosting-config`)。

use core::arch::asm;
use core::fmt;

unsafe extern "C" {
    static mut _sdata: u8;
    static mut _edata: u8;
    static _sidata: u8;
    static mut _sbss: u8;
    static mut _ebss: u8;
}

/// reset ベクタ。初期 SP はハードウェアが vector[0] から設定するため、ここでは
/// .data / .bss を整えてから main へ入る。
#[unsafe(no_mangle)]
pub unsafe extern "C" fn reset() -> ! {
    unsafe {
        // .data を LMA (flash) から VMA (RAM) へコピーする。
        let mut src = &raw const _sidata;
        let mut dst = &raw mut _sdata;
        let end = &raw mut _edata;
        while dst < end {
            core::ptr::write(dst, core::ptr::read(src));
            dst = dst.add(1);
            src = src.add(1);
        }
        // .bss をゼロ埋めする。
        let mut b = &raw mut _sbss;
        let bend = &raw mut _ebss;
        while b < bend {
            core::ptr::write(b, 0);
            b = b.add(1);
        }
    }
    crate::firmware::main()
}

// --- ARM semihosting --------------------------------------------------------

const SYS_WRITEC: usize = 0x03;
const SYS_EXIT: usize = 0x18;
/// SYS_EXIT の reason (ADP_Stopped_ApplicationExit)。
const APP_EXIT: usize = 0x2_0026;

unsafe fn semihost(op: usize, arg: usize) -> usize {
    let mut r = op;
    unsafe {
        // bkpt 0xAB が semihosting call。op を r0、引数を r1 に置く。
        asm!("bkpt 0xAB", inout("r0") r, in("r1") arg, options(nostack, preserves_flags));
    }
    r
}

/// semihosting へ 1 文字ずつ書く console。
pub struct Console;

impl Console {
    pub fn new() -> Self {
        Console
    }
}

impl fmt::Write for Console {
    fn write_str(&mut self, s: &str) -> fmt::Result {
        for &byte in s.as_bytes() {
            let c = byte;
            unsafe { semihost(SYS_WRITEC, (&c as *const u8) as usize) };
        }
        Ok(())
    }
}

/// QEMU を終了する。code は semihosting では直接は反映されないため、成否は
/// 出力マーカで判定する (テスト側)。
pub fn exit(_code: u32) -> ! {
    unsafe { semihost(SYS_EXIT, APP_EXIT) };
    loop {
        unsafe { asm!("wfi", options(nomem, nostack)) };
    }
}
