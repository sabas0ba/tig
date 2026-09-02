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

/// zlib stream を生成する (feature `write`)。
///
/// DEFLATE の fixed Huffman block (RFC 1951 3.2.6) 1 つに LZ77 の一致を
/// 符号化する。dynamic Huffman は持たない (表の構築と符号化で実装が倍になる
/// 割に、git object 程度の大きさでは利得が小さい)。圧縮結果が stored block
/// より大きくなる入力 (乱数に近い blob 等) では stored block を返す。
///
/// 一致探索は 3 byte の hash から直近の位置 1 つを引く単純なもので、chain を
/// 持たない。作業領域は hash 表 (`HASH_SIZE` entry の u32) と出力のみ。
#[cfg(feature = "write")]
pub fn deflate_zlib(data: &[u8]) -> Vec<u8> {
    let stored_len = 2 + data.len() + data.len().div_ceil(STORED_BLOCK).max(1) * 5 + 4;
    let mut out = Vec::with_capacity(stored_len.min(2 + data.len() / 2 + 64));
    out.extend_from_slice(&[0x78, 0x01]); // CMF/FLG (32 KiB window、check bits 調整済み)

    let mut bw = BitWriter::new(&mut out);
    bw.bits(1, 1); // BFINAL
    bw.bits(1, 2); // BTYPE = 01 (fixed Huffman)
    deflate_fixed_block(data, &mut bw);
    bw.code(FIXED_END_CODE, FIXED_END_LEN);
    bw.flush();

    if out.len() + 4 >= stored_len {
        return deflate_zlib_stored(data);
    }
    out.extend_from_slice(&adler32(data).to_be_bytes());
    out
}

/// zlib stream を生成する (DEFLATE の stored block のみ、feature `write`)。
///
/// 圧縮は行わない。git は伸長後の内容だけを検証するため、無圧縮の zlib stream
/// でも相互運用できる。[`deflate_zlib`] が圧縮の効かない入力で用いるほか、
/// 圧縮器を検証する際の参照にもなる。
#[cfg(feature = "write")]
pub fn deflate_zlib_stored(data: &[u8]) -> Vec<u8> {
    // stored block は「1 byte header + LE の len / !len + データ」。
    let blocks = data.len().div_ceil(STORED_BLOCK).max(1);
    let mut out = Vec::with_capacity(2 + data.len() + blocks * 5 + 4);
    out.extend_from_slice(&[0x78, 0x01]);

    if data.is_empty() {
        out.extend_from_slice(&[0x01, 0x00, 0x00, 0xff, 0xff]);
    } else {
        let mut chunks = data.chunks(STORED_BLOCK).peekable();
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

/// stored block 1 つに入る最大 byte 数。
#[cfg(feature = "write")]
const STORED_BLOCK: usize = 65_535;

// --- DEFLATE 圧縮 (fixed Huffman + LZ77) --------------------------------------

/// end-of-block (symbol 256) の fixed code。7 bit の 0。
#[cfg(feature = "write")]
const FIXED_END_CODE: u32 = 0;
#[cfg(feature = "write")]
const FIXED_END_LEN: u32 = 7;

/// 一致探索の hash 表の entry 数 (2 の冪)。u32 で 16 KiB。
#[cfg(feature = "write")]
const HASH_BITS: u32 = 12;
#[cfg(feature = "write")]
const HASH_SIZE: usize = 1 << HASH_BITS;
/// back reference の最大距離 (RFC 1951 の window)。
#[cfg(feature = "write")]
const MAX_DIST: usize = 32_768;
/// 一致の最小・最大長 (RFC 1951)。
#[cfg(feature = "write")]
const MIN_MATCH: usize = 3;
#[cfg(feature = "write")]
const MAX_MATCH: usize = 258;

#[cfg(feature = "write")]
struct BitWriter<'a> {
    out: &'a mut Vec<u8>,
    bit_buf: u64,
    bit_cnt: u32,
}

#[cfg(feature = "write")]
impl<'a> BitWriter<'a> {
    fn new(out: &'a mut Vec<u8>) -> Self {
        Self {
            out,
            bit_buf: 0,
            bit_cnt: 0,
        }
    }

    /// `n` bit を LSB から順に書く (header / extra bits 用)。n <= 32。
    fn bits(&mut self, value: u32, n: u32) {
        self.bit_buf |= u64::from(value) << self.bit_cnt;
        self.bit_cnt += n;
        while self.bit_cnt >= 8 {
            self.out.push(self.bit_buf as u8);
            self.bit_buf >>= 8;
            self.bit_cnt -= 8;
        }
    }

    /// Huffman code を書く。code は MSB から順に並ぶため bit 順を反転する。
    fn code(&mut self, code: u32, len: u32) {
        let reversed = code.reverse_bits() >> (32 - len);
        self.bits(reversed, len);
    }

    /// 端数 bit を 0 で埋めて byte 境界に揃える。
    fn flush(&mut self) {
        if self.bit_cnt > 0 {
            self.out.push(self.bit_buf as u8);
            self.bit_buf = 0;
            self.bit_cnt = 0;
        }
    }

    /// literal / length symbol を fixed Huffman code で書く (RFC 1951 3.2.6)。
    fn fixed_lit(&mut self, sym: u16) {
        let (code, len) = match sym {
            0..=143 => (0x30 + u32::from(sym), 8),
            144..=255 => (0x190 + u32::from(sym - 144), 9),
            256..=279 => (u32::from(sym - 256), 7),
            _ => (0xc0 + u32::from(sym - 280), 8),
        };
        self.code(code, len);
    }

    /// 一致 (長さ・距離) を symbol と extra bits に分けて書く。
    fn fixed_match(&mut self, len: usize, dist: usize) {
        let li = LEN_BASE
            .iter()
            .rposition(|&b| usize::from(b) <= len)
            .unwrap();
        self.fixed_lit(257 + li as u16);
        self.bits(
            (len - usize::from(LEN_BASE[li])) as u32,
            u32::from(LEN_EXTRA[li]),
        );

        let di = DIST_BASE
            .iter()
            .rposition(|&b| usize::from(b) <= dist)
            .unwrap();
        // distance code は 5 bit 固定長。
        self.code(di as u32, 5);
        self.bits(
            (dist - usize::from(DIST_BASE[di])) as u32,
            u32::from(DIST_EXTRA[di]),
        );
    }
}

/// 3 byte の hash。乗算 hash の上位 `HASH_BITS` bit を取る。
#[cfg(feature = "write")]
fn hash3(data: &[u8], pos: usize) -> usize {
    let v = u32::from(data[pos]) | u32::from(data[pos + 1]) << 8 | u32::from(data[pos + 2]) << 16;
    (v.wrapping_mul(0x9E37_79B1) >> (32 - HASH_BITS)) as usize
}

/// `data` を LZ77 の literal / 一致列に分解し、fixed Huffman で `bw` に書く。
/// end-of-block は書かない。
#[cfg(feature = "write")]
fn deflate_fixed_block(data: &[u8], bw: &mut BitWriter<'_>) {
    // head[h] = hash h を持つ直近の位置 (+1、0 は未登録)。
    let mut head = alloc::vec![0u32; HASH_SIZE];
    let mut pos = 0;
    while pos < data.len() {
        let remaining = data.len() - pos;
        let mut best = 0;
        let mut best_dist = 0;
        if remaining >= MIN_MATCH {
            let h = hash3(data, pos);
            let cand = head[h] as usize;
            head[h] = (pos + 1) as u32;
            if cand > 0 {
                let cand = cand - 1;
                let dist = pos - cand;
                if dist <= MAX_DIST {
                    let limit = remaining.min(MAX_MATCH);
                    let len = (0..limit)
                        .take_while(|&k| data[cand + k] == data[pos + k])
                        .count();
                    if len >= MIN_MATCH {
                        best = len;
                        best_dist = dist;
                    }
                }
            }
        }

        if best == 0 {
            bw.fixed_lit(u16::from(data[pos]));
            pos += 1;
        } else {
            bw.fixed_match(best, best_dist);
            // 一致の内側も hash 表へ登録し、以降の探索候補にする。
            for p in pos + 1..pos + best {
                if data.len() - p >= MIN_MATCH {
                    head[hash3(data, p)] = (p + 1) as u32;
                }
            }
            pos += best;
        }
    }
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
    fn fixed_deflate_roundtrip() {
        // 疑似乱数 (LCG) で圧縮の効かない入力も作る。
        let mut seed: u32 = 12345;
        let mut rand = || {
            seed = seed.wrapping_mul(1_664_525).wrapping_add(1_013_904_223);
            (seed >> 24) as u8
        };
        let cases: Vec<Vec<u8>> = vec![
            Vec::new(),
            vec![0x41],
            b"hello".to_vec(),
            vec![0u8; 1000],                                 // 距離 1 の長い一致
            (0..=255u8).collect(),                           // 9 bit literal を含む
            (0..40_000).map(|i| (i % 7) as u8).collect(),    // 周期の短い繰り返し
            (0..100_000).map(|i| (i % 251) as u8).collect(), // 32 KiB を超える距離
            (0..70_000).map(|_| rand()).collect(),           // 圧縮不能 → stored
            b"tree 1234\nparent abcd\nauthor A <a@example.com> 1 +0000\n\nmsg\n".repeat(50),
        ];
        for (i, data) in cases.iter().enumerate() {
            let compressed = deflate_zlib(data);
            let inflated = inflate_zlib(&compressed, Some(data.len())).unwrap();
            assert_eq!(&inflated.data, data, "case {i}");
            assert_eq!(inflated.consumed, compressed.len(), "case {i}");
            assert!(
                compressed.len() <= deflate_zlib_stored(data).len(),
                "case {i}: 圧縮結果が stored より大きい"
            );
        }
        // 繰り返しの多い入力は実際に縮むこと。
        let text = cases.last().unwrap();
        assert!(deflate_zlib(text).len() < text.len() / 10);
    }

    #[cfg(feature = "write")]
    #[test]
    fn fixed_deflate_boundary_distances() {
        // 一致距離が window 境界 (32768) の前後にある場合。
        for gap in [MAX_DIST - 1, MAX_DIST, MAX_DIST + 1] {
            let mut data = b"ABCDEFGHIJKLMNOP".to_vec();
            data.extend((0..gap - 16).map(|i| (i % 13) as u8 + b'a'));
            data.extend_from_slice(b"ABCDEFGHIJKLMNOP");
            let compressed = deflate_zlib(&data);
            let inflated = inflate_zlib(&compressed, None).unwrap();
            assert_eq!(inflated.data, data, "gap={gap}");
        }
        // 最大長 258 を超える一致の分割。
        let data = vec![b'x'; 258 * 3 + 7];
        let inflated = inflate_zlib(&deflate_zlib(&data), None).unwrap();
        assert_eq!(inflated.data, data);
    }

    #[cfg(feature = "write")]
    #[test]
    fn fixed_deflate_matches_reference_stream() {
        // 参照実装 (Python zlib, level 6) は "hello" を fixed block 1 つで
        // 符号化する。literal 列のみなら符号は一意なので、zlib header の FLG
        // (圧縮レベルの目安、伸長には無関係) を除いて byte 単位で一致する。
        assert_eq!(deflate_zlib(b"hello")[2..], HELLO_FIXED[2..]);
    }

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
