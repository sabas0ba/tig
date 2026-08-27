//! firmware へ link する組み込み例。
//!
//! `cargo build --target thumbv7em-none-eabi --release` 等で staticlib (.a) を
//! 生成し、firmware から C ABI で呼ぶ。flash 上の bundle を直接渡せる。
//!
//! - allocator: 静的バッファ上の bump allocator (解放しない)。bundle を 1 回
//!   解析して結果を取り出す使い方を想定した例であり、常駐させる場合は各環境の
//!   allocator に差し替える
//! - panic: 無限 loop。実機では watchdog / reset に置き換える

#![no_std]

extern crate alloc;

use core::sync::atomic::{AtomicUsize, Ordering};

use tig_core::bundle::Bundle;
use tig_core::history::Walk;

// --- bump allocator ---------------------------------------------------------

const HEAP_SIZE: usize = 256 * 1024;
static mut HEAP: [u8; HEAP_SIZE] = [0; HEAP_SIZE];
static NEXT: AtomicUsize = AtomicUsize::new(0);

struct Bump;

unsafe impl core::alloc::GlobalAlloc for Bump {
    unsafe fn alloc(&self, layout: core::alloc::Layout) -> *mut u8 {
        let base = (&raw mut HEAP) as *mut u8;
        loop {
            let current = NEXT.load(Ordering::Relaxed);
            let aligned = (current + layout.align() - 1) & !(layout.align() - 1);
            let Some(end) = aligned.checked_add(layout.size()) else {
                return core::ptr::null_mut();
            };
            if end > HEAP_SIZE {
                return core::ptr::null_mut();
            }
            if NEXT
                .compare_exchange(current, end, Ordering::Relaxed, Ordering::Relaxed)
                .is_ok()
            {
                return unsafe { base.add(aligned) };
            }
        }
    }

    unsafe fn dealloc(&self, _ptr: *mut u8, _layout: core::alloc::Layout) {
        // bump allocator は解放しない。
    }
}

#[global_allocator]
static ALLOC: Bump = Bump;

#[panic_handler]
fn panic(_info: &core::panic::PanicInfo<'_>) -> ! {
    loop {}
}

// --- C ABI ------------------------------------------------------------------

/// bundle を解析し、全 ref から到達できる commit 数を返す。負値はエラー。
///
/// # Safety
/// `(ptr, len)` は有効な読み取り可能領域であること。
#[unsafe(no_mangle)]
pub unsafe extern "C" fn tig_mcu_commit_count(ptr: *const u8, len: usize) -> i32 {
    let data = unsafe { core::slice::from_raw_parts(ptr, len) };
    let Ok(bundle) = Bundle::parse(data) else {
        return -1;
    };
    let mut walk = Walk::new(&bundle.pack);
    for (_, oid) in &bundle.refs {
        if walk.push(*oid).is_err() {
            return -2;
        }
    }
    let mut count: i32 = 0;
    for item in walk {
        if item.is_err() {
            return -3;
        }
        count = count.saturating_add(1);
    }
    count
}

/// 最初の ref の tip commit の subject (1 行目) を out へ書き、長さを返す。
/// 負値はエラー、out_len を超える分は切り捨てる。
///
/// # Safety
/// `(ptr, len)` と `(out, out_len)` は有効な領域であること。
#[unsafe(no_mangle)]
pub unsafe extern "C" fn tig_mcu_head_subject(
    ptr: *const u8,
    len: usize,
    out: *mut u8,
    out_len: usize,
) -> i32 {
    let data = unsafe { core::slice::from_raw_parts(ptr, len) };
    let Ok(bundle) = Bundle::parse(data) else {
        return -1;
    };
    let Some((_, oid)) = bundle.refs.first() else {
        return -2;
    };
    let mut walk = Walk::new(&bundle.pack);
    if walk.push(*oid).is_err() {
        return -2;
    }
    let Some(Ok(tip)) = walk.next() else {
        return -3;
    };
    let Ok(commit) = tip.commit() else {
        return -3;
    };
    let subject = commit.message.split(|&b| b == b'\n').next().unwrap_or(b"");
    let n = subject.len().min(out_len);
    unsafe { core::ptr::copy_nonoverlapping(subject.as_ptr(), out, n) };
    n as i32
}
