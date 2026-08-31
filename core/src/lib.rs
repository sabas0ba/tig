//! no_std 前提の git client core。
//!
//! 外部 crate に依存せず、`alloc` のみを要求する。I/O を持たず、入力は常に
//! `&[u8]` として受け取る (組み込みでは flash 上のデータを直接参照できる)。
//!
//! feature 構成:
//! - (常時): oid、SHA-1、zlib inflate、loose object の parse
//! - `pack`: packfile v2 の解析と delta 解決
//! - `bundle`: bundle v2/v3 の読み書き (`pack` を内包)
//! - `history`: committer date 順の history walk
//! - `transport-http`: protocol v2 の request 構築と response 解析 (sans-io)
//! - `fetch`: smart HTTP からの clone 状態機械 (`transport-http` + `bundle`)
//! - `write`: object (tree / commit) の生成と packfile の書き出し
//! - `push`: smart HTTP への push 状態機械 (receive-pack、`write` + `transport-http`)
//! - `checkout`: tree の展開 (filesystem は frontend の責務)

#![cfg_attr(not(test), no_std)]

extern crate alloc;

pub mod err;
pub mod object;
pub mod oid;
pub mod sha1;
pub mod zlib;

#[cfg(feature = "pack")]
pub mod delta;
#[cfg(feature = "pack")]
pub mod pack;

#[cfg(feature = "bundle")]
pub mod bundle;

#[cfg(feature = "history")]
pub mod history;

#[cfg(feature = "transport-http")]
pub mod pkt;
#[cfg(feature = "transport-http")]
pub mod protov2;

#[cfg(feature = "fetch")]
pub mod clone;

#[cfg(feature = "write")]
pub mod build;

#[cfg(feature = "push")]
pub mod protov0;
#[cfg(feature = "push")]
pub mod push;

#[cfg(feature = "checkout")]
pub mod checkout;

use alloc::vec::Vec;

/// object database の読み出し抽象。pack、loose object、メモリ上のストア等が実装する。
pub trait Odb {
    /// oid に対応する object の種別と内容 (header を除いた body) を返す。
    /// 存在しない場合は None。
    fn read(&self, oid: &oid::Oid) -> Option<(object::Kind, Vec<u8>)>;
}
