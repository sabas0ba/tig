//! object id (SHA-1)。
//!
//! 内部表現は 20 byte 固定とするが、型として分離してあるため、将来の SHA-256
//! 対応は本モジュールの拡張で吸収する。

use core::fmt;

use crate::err::{Error, Result};

/// SHA-1 の object id。
#[derive(Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct Oid([u8; 20]);

impl Oid {
    pub const fn from_bytes(bytes: [u8; 20]) -> Self {
        Self(bytes)
    }

    /// 40 桁の 16 進表記から変換する。
    pub fn from_hex(hex: &[u8]) -> Result<Self> {
        if hex.len() != 40 {
            return Err(Error::Corrupt("oid hex length"));
        }
        let mut bytes = [0u8; 20];
        for (i, chunk) in hex.chunks_exact(2).enumerate() {
            bytes[i] = (hex_val(chunk[0])? << 4) | hex_val(chunk[1])?;
        }
        Ok(Self(bytes))
    }

    pub const fn as_bytes(&self) -> &[u8; 20] {
        &self.0
    }
}

fn hex_val(c: u8) -> Result<u8> {
    match c {
        b'0'..=b'9' => Ok(c - b'0'),
        b'a'..=b'f' => Ok(c - b'a' + 10),
        b'A'..=b'F' => Ok(c - b'A' + 10),
        _ => Err(Error::Corrupt("oid hex digit")),
    }
}

fn fmt_hex(oid: &Oid, f: &mut fmt::Formatter<'_>) -> fmt::Result {
    for b in oid.0 {
        write!(f, "{b:02x}")?;
    }
    Ok(())
}

impl fmt::Display for Oid {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        fmt_hex(self, f)
    }
}

impl fmt::Debug for Oid {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        fmt_hex(self, f)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn hex_roundtrip() {
        let hex = "a9993e364706816aba3e25717850c26c9cd0d89d";
        let oid = Oid::from_hex(hex.as_bytes()).unwrap();
        assert_eq!(format!("{oid}"), hex);
    }

    #[test]
    fn rejects_bad_hex() {
        assert!(Oid::from_hex(b"xyz").is_err());
        assert!(Oid::from_hex(&[b'g'; 40]).is_err());
    }
}
