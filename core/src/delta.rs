//! packfile の delta (git 独自形式) の適用。
//!
//! delta は「base サイズ、結果サイズ、命令列」から成る。命令は base からの
//! copy と literal の insert の 2 種。

use alloc::vec::Vec;

use crate::err::{Error, Result};

/// delta を base に適用し、結果の object body を返す。
pub fn apply(base: &[u8], delta: &[u8]) -> Result<Vec<u8>> {
    let mut pos = 0;
    let base_size = read_varint(delta, &mut pos)?;
    if base_size != base.len() as u64 {
        return Err(Error::Corrupt("delta base size"));
    }
    let result_size = read_varint(delta, &mut pos)?;
    let mut out = Vec::with_capacity(result_size as usize);

    while pos < delta.len() {
        let op = delta[pos];
        pos += 1;
        if op & 0x80 != 0 {
            // copy 命令: bit 0-3 が offset、bit 4-6 が size の存在フラグ。
            let mut offset: usize = 0;
            for i in 0..4 {
                if op & (1 << i) != 0 {
                    let b = *delta.get(pos).ok_or(Error::UnexpectedEof)?;
                    offset |= usize::from(b) << (8 * i);
                    pos += 1;
                }
            }
            let mut size: usize = 0;
            for i in 0..3 {
                if op & (1 << (4 + i)) != 0 {
                    let b = *delta.get(pos).ok_or(Error::UnexpectedEof)?;
                    size |= usize::from(b) << (8 * i);
                    pos += 1;
                }
            }
            // size 0 は 0x10000 を意味する (git の仕様)。
            if size == 0 {
                size = 0x10000;
            }
            let src = base
                .get(offset..offset.checked_add(size).ok_or(Error::UnexpectedEof)?)
                .ok_or(Error::Corrupt("delta copy range"))?;
            out.extend_from_slice(src);
        } else if op != 0 {
            // insert 命令: op が literal の byte 数。
            let n = usize::from(op);
            let src = delta.get(pos..pos + n).ok_or(Error::UnexpectedEof)?;
            out.extend_from_slice(src);
            pos += n;
        } else {
            return Err(Error::Corrupt("delta opcode 0"));
        }
    }

    if out.len() as u64 != result_size {
        return Err(Error::Corrupt("delta result size"));
    }
    Ok(out)
}

/// delta header の可変長整数 (LSB first、7 bit 単位)。
fn read_varint(data: &[u8], pos: &mut usize) -> Result<u64> {
    let mut value: u64 = 0;
    let mut shift = 0;
    loop {
        let b = *data.get(*pos).ok_or(Error::UnexpectedEof)?;
        *pos += 1;
        value |= u64::from(b & 0x7f) << shift;
        if b & 0x80 == 0 {
            return Ok(value);
        }
        shift += 7;
        if shift >= 64 {
            return Err(Error::Corrupt("varint overflow"));
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn insert_only() {
        // base サイズ 0、結果サイズ 5、literal 5 byte。
        let delta = [0, 5, 5, b'h', b'e', b'l', b'l', b'o'];
        assert_eq!(apply(b"", &delta).unwrap(), b"hello");
    }

    #[test]
    fn copy_and_insert() {
        // base "hello world" から offset 6 size 5 を copy し、"!" を insert。
        let delta = [11, 6, 0x91, 6, 5, 1, b'!'];
        assert_eq!(apply(b"hello world", &delta).unwrap(), b"world!");
    }

    #[test]
    fn size_mismatch_rejected() {
        let delta = [0, 4, 5, b'h', b'e', b'l', b'l', b'o'];
        assert!(apply(b"", &delta).is_err());
    }

    #[test]
    fn out_of_range_copy_rejected() {
        let delta = [5, 5, 0x91, 4, 5];
        assert!(apply(b"abcde", &delta).is_err());
    }
}
