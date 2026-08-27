//! ライブラリ全体で共有するエラー型。

use core::fmt;

/// 解析・検証の失敗。入力データの破損や未対応形式を表す。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Error {
    /// 入力が途中で尽きた。
    UnexpectedEof,
    /// 形式違反。値は違反箇所の簡潔な説明。
    Corrupt(&'static str),
    /// SHA-256 repository 等、未対応の形式。値は形式の名前。
    Unsupported(&'static str),
    /// checksum (adler32 / SHA-1) の不一致。値は対象の名前。
    Checksum(&'static str),
    /// delta の base object が見つからない。
    MissingBase,
}

impl fmt::Display for Error {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Error::UnexpectedEof => write!(f, "unexpected end of input"),
            Error::Corrupt(what) => write!(f, "corrupt data: {what}"),
            Error::Unsupported(what) => write!(f, "unsupported: {what}"),
            Error::Checksum(what) => write!(f, "checksum mismatch: {what}"),
            Error::MissingBase => write!(f, "delta base object not found"),
        }
    }
}

impl core::error::Error for Error {}

pub type Result<T> = core::result::Result<T, Error>;
