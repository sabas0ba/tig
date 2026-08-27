//! pkt-line (gitprotocol-common) の符号化・復号。
//!
//! pkt-line は「4 桁 16 進の長さ + データ」の列で、長さ 0000 (flush) / 0001
//! (delim) / 0002 (response-end) は区切りとして予約される。protocol v2 の
//! request / response は全てこの形式で運ばれる。

use alloc::vec::Vec;

use crate::err::{Error, Result};

/// データ部の最大長 (gitprotocol-common: 65520 - 4)。
pub const MAX_DATA_LEN: usize = 65_516;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Pkt<'a> {
    Flush,
    Delim,
    ResponseEnd,
    Data(&'a [u8]),
}

/// 入力を pkt-line の列として読む iterator。
pub struct PktReader<'a> {
    rest: &'a [u8],
}

impl<'a> PktReader<'a> {
    pub fn new(data: &'a [u8]) -> Self {
        Self { rest: data }
    }

    /// 未消費の残り。
    pub fn rest(&self) -> &'a [u8] {
        self.rest
    }

    /// 次の pkt を読む。入力が尽きたら None。
    #[allow(clippy::should_implement_trait)]
    pub fn next(&mut self) -> Option<Result<Pkt<'a>>> {
        if self.rest.is_empty() {
            return None;
        }
        Some(self.read_one())
    }

    /// 次の pkt を読む。入力が尽きている場合はエラー。
    pub fn expect_next(&mut self) -> Result<Pkt<'a>> {
        self.next().unwrap_or(Err(Error::UnexpectedEof))
    }

    fn read_one(&mut self) -> Result<Pkt<'a>> {
        let head = self.rest.get(..4).ok_or(Error::UnexpectedEof)?;
        let mut len = 0usize;
        for &c in head {
            let v = match c {
                b'0'..=b'9' => c - b'0',
                b'a'..=b'f' => c - b'a' + 10,
                b'A'..=b'F' => c - b'A' + 10,
                _ => return Err(Error::Corrupt("pkt-line length")),
            };
            len = len * 16 + usize::from(v);
        }
        match len {
            0 => {
                self.rest = &self.rest[4..];
                Ok(Pkt::Flush)
            }
            1 => {
                self.rest = &self.rest[4..];
                Ok(Pkt::Delim)
            }
            2 => {
                self.rest = &self.rest[4..];
                Ok(Pkt::ResponseEnd)
            }
            3 => Err(Error::Corrupt("pkt-line length 0003")),
            _ => {
                let data = self.rest.get(4..len).ok_or(Error::UnexpectedEof)?;
                self.rest = &self.rest[len..];
                Ok(Pkt::Data(data))
            }
        }
    }
}

/// データ pkt を書く。
pub fn write_data(out: &mut Vec<u8>, data: &[u8]) {
    debug_assert!(data.len() <= MAX_DATA_LEN);
    let len = data.len() + 4;
    const HEX: &[u8; 16] = b"0123456789abcdef";
    out.push(HEX[(len >> 12) & 0xf]);
    out.push(HEX[(len >> 8) & 0xf]);
    out.push(HEX[(len >> 4) & 0xf]);
    out.push(HEX[len & 0xf]);
    out.extend_from_slice(data);
}

/// テキスト行の pkt を書く (LF を付加する)。
pub fn write_line(out: &mut Vec<u8>, line: &[u8]) {
    debug_assert!(line.len() < MAX_DATA_LEN);
    let len = line.len() + 5;
    const HEX: &[u8; 16] = b"0123456789abcdef";
    out.push(HEX[(len >> 12) & 0xf]);
    out.push(HEX[(len >> 8) & 0xf]);
    out.push(HEX[(len >> 4) & 0xf]);
    out.push(HEX[len & 0xf]);
    out.extend_from_slice(line);
    out.push(b'\n');
}

pub fn write_flush(out: &mut Vec<u8>) {
    out.extend_from_slice(b"0000");
}

pub fn write_delim(out: &mut Vec<u8>) {
    out.extend_from_slice(b"0001");
}

/// データ pkt の末尾 LF を取り除く (テキスト行の慣例)。
pub fn trim_line(data: &[u8]) -> &[u8] {
    data.strip_suffix(b"\n").unwrap_or(data)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn roundtrip() {
        let mut buf = Vec::new();
        write_line(&mut buf, b"version 2");
        write_delim(&mut buf);
        write_data(&mut buf, &[0x01, 0xff]);
        write_flush(&mut buf);

        let mut r = PktReader::new(&buf);
        assert_eq!(r.expect_next().unwrap(), Pkt::Data(b"version 2\n"));
        assert_eq!(r.expect_next().unwrap(), Pkt::Delim);
        assert_eq!(r.expect_next().unwrap(), Pkt::Data(&[0x01, 0xff]));
        assert_eq!(r.expect_next().unwrap(), Pkt::Flush);
        assert!(r.next().is_none());
    }

    #[test]
    fn known_encoding() {
        let mut buf = Vec::new();
        write_line(&mut buf, b"a");
        assert_eq!(buf, b"0006a\n");
        buf.clear();
        write_flush(&mut buf);
        assert_eq!(buf, b"0000");
    }

    #[test]
    fn truncated_rejected() {
        let mut r = PktReader::new(b"000");
        assert_eq!(r.expect_next().unwrap_err(), Error::UnexpectedEof);
        let mut r = PktReader::new(b"0010short");
        assert_eq!(r.expect_next().unwrap_err(), Error::UnexpectedEof);
    }

    #[test]
    fn bad_length_rejected() {
        let mut r = PktReader::new(b"00xx");
        assert!(r.expect_next().is_err());
        let mut r = PktReader::new(b"0003");
        assert!(r.expect_next().is_err());
    }
}
