//! target の arch に応じてリンカスクリプトを選ぶ。
//!
//! パスは絶対 (CARGO_MANIFEST_DIR 基準) で渡し、リンカの cwd に依存しない。

use std::env;

fn main() {
    let dir = env::var("CARGO_MANIFEST_DIR").unwrap();
    let arch = env::var("CARGO_CFG_TARGET_ARCH").unwrap();
    let script = match arch.as_str() {
        "arm" => "link-arm.ld",
        "riscv32" | "riscv64" => "link-riscv.ld",
        // host (build やテスト) 向けにはスクリプトを渡さない。
        _ => return,
    };
    println!("cargo::rustc-link-arg=-T{dir}/{script}");
    println!("cargo::rerun-if-changed={dir}/{script}");
}
