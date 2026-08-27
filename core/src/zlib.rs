//! zlib (RFC 1950) / DEFLATE (RFC 1951) の伸長。
//!
//! git の object は個々に zlib stream として圧縮される。packfile では stream の
//! 圧縮後長が記録されないため、伸長と同時に消費 byte 数を返す。
//!
//! window は別途持たず、伸長済み出力そのものを back reference の参照先とする。
//! git object は常に全体を伸長するため、追加の 32 KiB を確保するより省メモリになる。

use alloc::vec::Vec;

use crate::err::{Error, Result};

/// 伸長結果。`consumed` は入力先頭からの消費 byte 数 (adler32 を含む)。
#[derive(Debug)]
pub struct Inflated {
    pub data: Vec<u8>,
    pub consumed: usize,
}

/// zlib stream を伸長する。`size_hint` は既知の伸長後サイズ (packfile の header 等)。
pub fn inflate_zlib(input: &[u8], size_hint: Option<usize>) -> Result<Inflated> {
    if input.len() < 2 {
        return Err(Error::UnexpectedEof);
    }
    let cmf = input[0];
    let flg = input[1];
    if cmf & 0x0f != 8 {
        return Err(Error::Unsupported("zlib compression method"));
    }
    if (u16::from(cmf) << 8 | u16::from(flg)) % 31 != 0 {
        return Err(Error::Corrupt("zlib header check"));
    }
    if flg & 0x20 != 0 {
        return Err(Error::Unsupported("zlib preset dictionary"));
    }

    let mut br = BitReader::new(&input[2..]);
    let data = inflate_raw(&mut br, size_hint)?;

    // DEFLATE stream の直後に adler32 (big endian) が続く。
    br.align_to_byte();
    let pos = 2 + br.byte_pos();
    let sum = input
        .get(pos..pos + 4)
        .ok_or(Error::UnexpectedEof)
        .map(|b| u32::from_be_bytes(b.try_into().unwrap()))?;
    if sum != adler32(&data) {
        return Err(Error::Checksum("zlib adler32"));
    }

    Ok(Inflated {
        data,
        consumed: pos + 4,
    })
}

/// zlib stream を生成する (DEFLATE の stored block のみ、feature `write`)。
///
/// 圧縮は行わない。git は伸長後の内容だけを検証するため、無圧縮の zlib stream
/// でも相互運用できる。圧縮率より実装の小ささと検証容易性を優先した選択で、
/// 転送量が問題になる場合は fixed Huffman の追加を検討する (docs/design.md)。
#[cfg(feature = "write")]
pub fn deflate_zlib_stored(data: &[u8]) -> Vec<u8> {
    // stored block は「1 byte header + LE の len / !len + データ」。
    const BLOCK: usize = 65_535;
    let blocks = data.len().div_ceil(BLOCK).max(1);
    let mut out = Vec::with_capacity(2 + data.len() + blocks * 5 + 4);
    out.extend_from_slice(&[0x78, 0x01]); // CMF/FLG (32 KiB window、check bits 調整済み)

    if data.is_empty() {
        out.extend_from_slice(&[0x01, 0x00, 0x00, 0xff, 0xff]);
    } else {
        let mut chunks = data.chunks(BLOCK).peekable();
        while let Some(chunk) = chunks.next() {
            out.push(if chunks.peek().is_none() { 0x01 } else { 0x00 });
            let len = chunk.len() as u16;
            out.extend_from_slice(&len.to_le_bytes());
            out.extend_from_slice(&(!len).to_le_bytes());
            out.extend_from_slice(chunk);
        }
    }

    out.extend_from_slice(&adler32(data).to_be_bytes());
    out
}

/// adler32 (RFC 1950)。
pub fn adler32(data: &[u8]) -> u32 {
    const MOD: u32 = 65_521;
    let mut a: u32 = 1;
    let mut b: u32 = 0;
    // 5552 は u32 が溢れない最大のまとめ幅 (zlib と同じ値)。
    for chunk in data.chunks(5552) {
        for &byte in chunk {
            a += u32::from(byte);
            b += a;
        }
        a %= MOD;
        b %= MOD;
    }
    (b << 16) | a
}

// --- DEFLATE ----------------------------------------------------------------

struct BitReader<'a> {
    data: &'a [u8],
    /// 次に読む byte の位置。
    pos: usize,
    bit_buf: u32,
    bit_cnt: u32,
}

impl<'a> BitReader<'a> {
    fn new(data: &'a [u8]) -> Self {
        Self {
            data,
            pos: 0,
            bit_buf: 0,
            bit_cnt: 0,
        }
    }

    /// LSB first で n bit (n <= 16) を読む。
    fn bits(&mut self, n: u32) -> Result<u32> {
        while self.bit_cnt < n {
            let byte = *self.data.get(self.pos).ok_or(Error::UnexpectedEof)?;
            self.bit_buf |= u32::from(byte) << self.bit_cnt;
            self.bit_cnt += 8;
            self.pos += 1;
        }
        let out = self.bit_buf & ((1 << n) - 1);
        self.bit_buf >>= n;
        self.bit_cnt -= n;
        Ok(out)
    }

    /// byte 境界まで読み捨て、先読み分を入力へ戻す。
    fn align_to_byte(&mut self) {
        self.bit_buf >>= self.bit_cnt % 8;
        self.bit_cnt -= self.bit_cnt % 8;
        // bit_buf に残った丸ごとの byte は未消費として位置を戻す。
        self.pos -= (self.bit_cnt / 8) as usize;
        self.bit_buf = 0;
        self.bit_cnt = 0;
    }

    /// byte 境界に揃った状態での現在位置。
    fn byte_pos(&self) -> usize {
        debug_assert_eq!(self.bit_cnt, 0);
        self.pos
    }

    fn read_bytes(&mut self, n: usize, out: &mut Vec<u8>) -> Result<()> {
        let end = self.pos.checked_add(n).ok_or(Error::UnexpectedEof)?;
        let src = self.data.get(self.pos..end).ok_or(Error::UnexpectedEof)?;
        out.extend_from_slice(src);
        self.pos = end;
        Ok(())
    }
}

/// canonical Huffman code の decode 表。
///
/// 表引きの高速化 (lookup table) は行わず、code 長ごとの範囲判定で 1 bit ずつ
/// 決定する。速度よりメモリ (数百 byte) を優先した選択。
struct Huffman {
    /// count[n] = 長さ n の code の個数。
    count: [u16; 16],
    /// code 値順に並べた symbol。
    symbols: Vec<u16>,
}

impl Huffman {
    /// symbol ごとの code 長 (0 = 割り当てなし) から構築する。
    fn new(lengths: &[u8]) -> Result<Self> {
        let mut count = [0u16; 16];
        for &len in lengths {
            if len > 15 {
                return Err(Error::Corrupt("huffman code length"));
            }
            count[usize::from(len)] += 1;
        }

        // over-subscribed な code 表を拒否する (incomplete は distance 表で正規に現れる)。
        let mut left: i32 = 1;
        for &len_count in &count[1..] {
            left = (left << 1) - i32::from(len_count);
            if left < 0 {
                return Err(Error::Corrupt("oversubscribed huffman table"));
            }
        }

        let mut offsets = [0u16; 16];
        for n in 1..15 {
            offsets[n + 1] = offsets[n] + count[n];
        }
        let mut symbols = alloc::vec![0u16; lengths.iter().filter(|&&l| l != 0).count()];
        for (sym, &len) in lengths.iter().enumerate() {
            if len != 0 {
                symbols[usize::from(offsets[usize::from(len)])] = sym as u16;
                offsets[usize::from(len)] += 1;
            }
        }
        Ok(Self { count, symbols })
    }

    fn decode(&self, br: &mut BitReader<'_>) -> Result<u16> {
        let mut code: u32 = 0;
        let mut first: u32 = 0;
        let mut index: u32 = 0;
        for len in 1..16 {
            code |= br.bits(1)?;
            let count = u32::from(self.count[len]);
            if code < first + count {
                return Ok(self.symbols[(index + code - first) as usize]);
            }
            index += count;
            first = (first + count) << 1;
            code <<= 1;
        }
        Err(Error::Corrupt("invalid huffman code"))
    }
}

const LEN_BASE: [u16; 29] = [
    3, 4, 5, 6, 7, 8, 9, 10, 11, 13, 15, 17, 19, 23, 27, 31, 35, 43, 51, 59, 67, 83, 99, 115, 131,
    163, 195, 227, 258,
];
const LEN_EXTRA: [u8; 29] = [
    0, 0, 0, 0, 0, 0, 0, 0, 1, 1, 1, 1, 2, 2, 2, 2, 3, 3, 3, 3, 4, 4, 4, 4, 5, 5, 5, 5, 0,
];
const DIST_BASE: [u16; 30] = [
    1, 2, 3, 4, 5, 7, 9, 13, 17, 25, 33, 49, 65, 97, 129, 193, 257, 385, 513, 769, 1025, 1537,
    2049, 3073, 4097, 6145, 8193, 12289, 16385, 24577,
];
const DIST_EXTRA: [u8; 30] = [
    0, 0, 0, 0, 1, 1, 2, 2, 3, 3, 4, 4, 5, 5, 6, 6, 7, 7, 8, 8, 9, 9, 10, 10, 11, 11, 12, 12, 13,
    13,
];

/// dynamic block の code 長列を並べる際の symbol 順 (RFC 1951 3.2.7)。
const CLEN_ORDER: [usize; 19] = [
    16, 17, 18, 0, 8, 7, 9, 6, 10, 5, 11, 4, 12, 3, 13, 2, 14, 1, 15,
];

fn inflate_raw(br: &mut BitReader<'_>, size_hint: Option<usize>) -> Result<Vec<u8>> {
    let mut out = Vec::with_capacity(size_hint.unwrap_or(0));
    loop {
        let bfinal = br.bits(1)?;
        match br.bits(2)? {
            0 => inflate_stored(br, &mut out)?,
            1 => {
                let (lit, dist) = fixed_tables()?;
                inflate_block(br, &lit, &dist, &mut out)?;
            }
            2 => {
                let (lit, dist) = dynamic_tables(br)?;
                inflate_block(br, &lit, &dist, &mut out)?;
            }
            _ => return Err(Error::Corrupt("deflate block type")),
        }
        if bfinal == 1 {
            return Ok(out);
        }
    }
}

fn inflate_stored(br: &mut BitReader<'_>, out: &mut Vec<u8>) -> Result<()> {
    br.align_to_byte();
    let pos = br.byte_pos();
    let header = br.data.get(pos..pos + 4).ok_or(Error::UnexpectedEof)?;
    let len = u16::from_le_bytes(header[0..2].try_into().unwrap());
    let nlen = u16::from_le_bytes(header[2..4].try_into().unwrap());
    if len != !nlen {
        return Err(Error::Corrupt("stored block length"));
    }
    br.pos = pos + 4;
    br.read_bytes(usize::from(len), out)
}

fn fixed_tables() -> Result<(Huffman, Huffman)> {
    let mut lit_lengths = [0u8; 288];
    lit_lengths[0..144].fill(8);
    lit_lengths[144..256].fill(9);
    lit_lengths[256..280].fill(7);
    lit_lengths[280..288].fill(8);
    Ok((Huffman::new(&lit_lengths)?, Huffman::new(&[5u8; 30])?))
}

fn dynamic_tables(br: &mut BitReader<'_>) -> Result<(Huffman, Huffman)> {
    let hlit = br.bits(5)? as usize + 257;
    let hdist = br.bits(5)? as usize + 1;
    let hclen = br.bits(4)? as usize + 4;
    if hlit > 286 || hdist > 30 {
        return Err(Error::Corrupt("dynamic table size"));
    }

    let mut clen_lengths = [0u8; 19];
    for &idx in CLEN_ORDER.iter().take(hclen) {
        clen_lengths[idx] = br.bits(3)? as u8;
    }
    let clen = Huffman::new(&clen_lengths)?;

    // literal/length 表と distance 表の code 長は連続して符号化される。
    let mut lengths = [0u8; 286 + 30];
    let mut i = 0;
    while i < hlit + hdist {
        let sym = clen.decode(br)?;
        match sym {
            0..=15 => {
                lengths[i] = sym as u8;
                i += 1;
            }
            16 => {
                if i == 0 {
                    return Err(Error::Corrupt("length repeat without previous"));
                }
                let prev = lengths[i - 1];
                let n = br.bits(2)? as usize + 3;
                repeat(&mut lengths, &mut i, hlit + hdist, prev, n)?;
            }
            17 => {
                let n = br.bits(3)? as usize + 3;
                repeat(&mut lengths, &mut i, hlit + hdist, 0, n)?;
            }
            18 => {
                let n = br.bits(7)? as usize + 11;
                repeat(&mut lengths, &mut i, hlit + hdist, 0, n)?;
            }
            _ => return Err(Error::Corrupt("code length symbol")),
        }
    }

    if lengths[256] == 0 {
        return Err(Error::Corrupt("missing end-of-block code"));
    }
    Ok((
        Huffman::new(&lengths[..hlit])?,
        Huffman::new(&lengths[hlit..hlit + hdist])?,
    ))
}

fn repeat(lengths: &mut [u8], i: &mut usize, limit: usize, value: u8, n: usize) -> Result<()> {
    if *i + n > limit {
        return Err(Error::Corrupt("length repeat overflow"));
    }
    lengths[*i..*i + n].fill(value);
    *i += n;
    Ok(())
}

fn inflate_block(
    br: &mut BitReader<'_>,
    lit: &Huffman,
    dist: &Huffman,
    out: &mut Vec<u8>,
) -> Result<()> {
    loop {
        let sym = lit.decode(br)?;
        match sym {
            0..=255 => out.push(sym as u8),
            256 => return Ok(()),
            257..=285 => {
                let idx = usize::from(sym - 257);
                let len = usize::from(LEN_BASE[idx]) + br.bits(u32::from(LEN_EXTRA[idx]))? as usize;

                let dsym = usize::from(dist.decode(br)?);
                if dsym >= 30 {
                    return Err(Error::Corrupt("distance symbol"));
                }
                let distance =
                    usize::from(DIST_BASE[dsym]) + br.bits(u32::from(DIST_EXTRA[dsym]))? as usize;
                if distance > out.len() {
                    return Err(Error::Corrupt("distance beyond output"));
                }

                // 距離 < 長さの重複コピー (RLE 相当) があるため 1 byte ずつ写す。
                let start = out.len() - distance;
                for k in 0..len {
                    let byte = out[start + k];
                    out.push(byte);
                }
            }
            _ => return Err(Error::Corrupt("literal/length symbol")),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    // フィクスチャは Python の zlib で生成した (tests のコメント参照)。
    // python3 -c "import zlib; print(list(zlib.compress(b'hello', 6)))"
    const HELLO_FIXED: &[u8] = &[120, 156, 203, 72, 205, 201, 201, 7, 0, 6, 44, 2, 21];
    // python3 -c "import zlib; print(list(zlib.compress(b'hello', 0)))" (stored)
    const HELLO_STORED: &[u8] = &[
        120, 1, 1, 5, 0, 250, 255, 104, 101, 108, 108, 111, 6, 44, 2, 21,
    ];

    #[test]
    fn fixed_block() {
        let r = inflate_zlib(HELLO_FIXED, None).unwrap();
        assert_eq!(r.data, b"hello");
        assert_eq!(r.consumed, HELLO_FIXED.len());
    }

    #[test]
    fn stored_block() {
        let r = inflate_zlib(HELLO_STORED, Some(5)).unwrap();
        assert_eq!(r.data, b"hello");
        assert_eq!(r.consumed, HELLO_STORED.len());
    }

    // stream の後ろに続きのデータがあっても consumed が境界を正しく指すこと。
    #[test]
    fn trailing_data_ignored() {
        let mut input = HELLO_FIXED.to_vec();
        input.extend_from_slice(&[0xde, 0xad, 0xbe, 0xef]);
        let r = inflate_zlib(&input, None).unwrap();
        assert_eq!(r.data, b"hello");
        assert_eq!(r.consumed, HELLO_FIXED.len());
    }

    #[test]
    fn corrupt_adler32_rejected() {
        let mut input = HELLO_FIXED.to_vec();
        let last = input.len() - 1;
        input[last] ^= 0xff;
        assert_eq!(
            inflate_zlib(&input, None).unwrap_err(),
            Error::Checksum("zlib adler32")
        );
    }

    #[test]
    fn truncated_input_rejected() {
        for n in 0..HELLO_FIXED.len() {
            assert!(inflate_zlib(&HELLO_FIXED[..n], None).is_err(), "n={n}");
        }
    }

    #[test]
    fn adler32_vectors() {
        assert_eq!(adler32(b""), 1);
        assert_eq!(adler32(b"Wikipedia"), 0x11e6_0398);
    }

    // 圧縮側の出力を自前の伸長器で往復させる。伸長器は git の実データとの
    // 差分テストで検証済みのため、往復一致が相互運用の根拠になる。
    #[cfg(feature = "write")]
    #[test]
    fn stored_deflate_roundtrip() {
        for len in [0usize, 1, 100, 65_534, 65_535, 65_536, 200_000] {
            let data: Vec<u8> = (0..len).map(|i| (i % 251) as u8).collect();
            let compressed = deflate_zlib_stored(&data);
            let inflated = inflate_zlib(&compressed, Some(len)).unwrap();
            assert_eq!(inflated.data, data, "len={len}");
            assert_eq!(inflated.consumed, compressed.len(), "len={len}");
        }
    }
}
