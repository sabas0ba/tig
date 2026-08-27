//! SHA-1 (RFC 3174)。
//!
//! git の object id と packfile の checksum 計算に用いる。ストリーミング入力に
//! 対応し、固定サイズの内部状態 (window 64 byte) のみを持つ。
//!
//! SHA-1 は衝突攻撃が実証済みであり、暗号学的用途には使用しない。ここでは
//! 既存 git repository との相互運用のための識別子としてのみ用いる。

use crate::oid::Oid;

pub struct Sha1 {
    state: [u32; 5],
    /// 入力の総 byte 数。
    len: u64,
    buf: [u8; 64],
    buf_len: usize,
}

impl Default for Sha1 {
    fn default() -> Self {
        Self::new()
    }
}

impl Sha1 {
    pub const fn new() -> Self {
        Self {
            state: [
                0x6745_2301,
                0xefcd_ab89,
                0x98ba_dcfe,
                0x1032_5476,
                0xc3d2_e1f0,
            ],
            len: 0,
            buf: [0; 64],
            buf_len: 0,
        }
    }

    pub fn update(&mut self, mut data: &[u8]) {
        self.len += data.len() as u64;

        if self.buf_len > 0 {
            let take = (64 - self.buf_len).min(data.len());
            self.buf[self.buf_len..self.buf_len + take].copy_from_slice(&data[..take]);
            self.buf_len += take;
            data = &data[take..];
            if self.buf_len < 64 {
                // バッファが埋まらなかった (入力が尽きた)。以降の remainder 処理に
                // 落とすと buf_len を上書きしてしまうため、ここで抜ける。
                return;
            }
            let block = self.buf;
            self.compress(&block);
            self.buf_len = 0;
        }

        let mut chunks = data.chunks_exact(64);
        for block in &mut chunks {
            self.compress(block.try_into().unwrap());
        }
        let rest = chunks.remainder();
        self.buf[..rest.len()].copy_from_slice(rest);
        self.buf_len = rest.len();
    }

    pub fn finalize(mut self) -> [u8; 20] {
        let bit_len = self.len * 8;
        self.update(&[0x80]);
        while self.buf_len != 56 {
            self.update(&[0]);
        }
        // 上の update で len が進むが、bit_len は事前に確定済みのため影響しない。
        self.update(&bit_len.to_be_bytes());
        debug_assert_eq!(self.buf_len, 0);

        let mut out = [0u8; 20];
        for (i, word) in self.state.iter().enumerate() {
            out[i * 4..i * 4 + 4].copy_from_slice(&word.to_be_bytes());
        }
        out
    }

    fn compress(&mut self, block: &[u8; 64]) {
        let mut w = [0u32; 80];
        for (i, chunk) in block.chunks_exact(4).enumerate() {
            w[i] = u32::from_be_bytes(chunk.try_into().unwrap());
        }
        for i in 16..80 {
            w[i] = (w[i - 3] ^ w[i - 8] ^ w[i - 14] ^ w[i - 16]).rotate_left(1);
        }

        let [mut a, mut b, mut c, mut d, mut e] = self.state;
        for (i, &word) in w.iter().enumerate() {
            let (f, k) = match i {
                0..=19 => ((b & c) | (!b & d), 0x5a82_7999),
                20..=39 => (b ^ c ^ d, 0x6ed9_eba1),
                40..=59 => ((b & c) | (b & d) | (c & d), 0x8f1b_bcdc),
                _ => (b ^ c ^ d, 0xca62_c1d6),
            };
            let t = a
                .rotate_left(5)
                .wrapping_add(f)
                .wrapping_add(e)
                .wrapping_add(k)
                .wrapping_add(word);
            e = d;
            d = c;
            c = b.rotate_left(30);
            b = a;
            a = t;
        }

        self.state[0] = self.state[0].wrapping_add(a);
        self.state[1] = self.state[1].wrapping_add(b);
        self.state[2] = self.state[2].wrapping_add(c);
        self.state[3] = self.state[3].wrapping_add(d);
        self.state[4] = self.state[4].wrapping_add(e);
    }
}

/// 一括入力のダイジェスト計算。
pub fn digest(data: &[u8]) -> [u8; 20] {
    let mut h = Sha1::new();
    h.update(data);
    h.finalize()
}

/// ダイジェストを Oid として返す。
pub fn digest_oid(parts: &[&[u8]]) -> Oid {
    let mut h = Sha1::new();
    for part in parts {
        h.update(part);
    }
    Oid::from_bytes(h.finalize())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn hex(bytes: &[u8]) -> String {
        bytes.iter().map(|b| format!("{b:02x}")).collect()
    }

    // RFC 3174 のテストベクタ。
    #[test]
    fn rfc3174_vectors() {
        assert_eq!(
            hex(&digest(b"abc")),
            "a9993e364706816aba3e25717850c26c9cd0d89d"
        );
        assert_eq!(
            hex(&digest(
                b"abcdbcdecdefdefgefghfghighijhijkijkljklmklmnlmnomnopnopq"
            )),
            "84983e441c3bd26ebaae4aa1f95129e5e54670f1"
        );
        assert_eq!(
            hex(&digest(&[b'a'; 1_000_000])),
            "34aa973cd4c4daa4f61eeb2bdbad27316534016f"
        );
    }

    #[test]
    fn empty_input() {
        assert_eq!(
            hex(&digest(b"")),
            "da39a3ee5e6b4b0d3255bfef95601890afd80709"
        );
    }

    // 分割入力と一括入力が一致すること (境界 64 byte をまたぐケースを含む)。
    #[test]
    fn streaming_matches_oneshot() {
        let data: Vec<u8> = (0..=255u8).cycle().take(1000).collect();
        for split in [1usize, 63, 64, 65, 127, 500] {
            let mut h = Sha1::new();
            for chunk in data.chunks(split) {
                h.update(chunk);
            }
            assert_eq!(h.finalize(), digest(&data), "split={split}");
        }
    }
}
