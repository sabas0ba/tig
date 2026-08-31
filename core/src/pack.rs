//! packfile (version 2) の解析。
//!
//! pack は「12 byte header + object entry の列 + SHA-1 trailer」から成る。entry の
//! 圧縮後長は記録されないため、先頭から順に伸長しながら境界を求める。parse 時に
//! 全 entry を一度 materialize して oid を確定し、oid 順の index (entry あたり
//! 24 byte) を構築する。object の内容は保持せず、読み出しのたびに伸長し直す
//! (メモリを CPU で贖う方針)。

use alloc::vec::Vec;

use crate::delta;
use crate::err::{Error, Result};
use crate::object::Kind;
use crate::oid::Oid;
use crate::sha1;
use crate::zlib;

/// 解析済みの packfile。`data` は pack 全体 (header と trailer を含む)。
pub struct Pack<'a> {
    data: &'a [u8],
    /// trailer (SHA-1) を除いた末尾位置。
    content_end: usize,
    /// oid 順に整列した (oid, entry の開始 offset)。
    index: Vec<(Oid, u32)>,
}

/// entry の種別 code (pack 形式の生値)。
const OBJ_COMMIT: u8 = 1;
const OBJ_TREE: u8 = 2;
const OBJ_BLOB: u8 = 3;
const OBJ_TAG: u8 = 4;
const OBJ_OFS_DELTA: u8 = 6;
const OBJ_REF_DELTA: u8 = 7;

/// delta chain 長の上限。循環参照 (破損データ) の検出を兼ねる。
const MAX_DELTA_DEPTH: usize = 4096;

impl<'a> Pack<'a> {
    pub fn parse(data: &'a [u8]) -> Result<Self> {
        if data.len() < 12 + 20 {
            return Err(Error::UnexpectedEof);
        }
        if &data[0..4] != b"PACK" {
            return Err(Error::Corrupt("pack magic"));
        }
        if u32::from_be_bytes(data[4..8].try_into().unwrap()) != 2 {
            return Err(Error::Unsupported("pack version"));
        }
        if data.len() as u64 > u64::from(u32::MAX) {
            return Err(Error::Unsupported("pack larger than 4 GiB"));
        }
        let count = u32::from_be_bytes(data[8..12].try_into().unwrap()) as usize;

        let content_end = data.len() - 20;
        if sha1::digest(&data[..content_end]) != data[content_end..] {
            return Err(Error::Checksum("pack trailer"));
        }

        // pass 1: 全 entry を走査して境界を確定し、delta でない object の oid を求める。
        let mut index: Vec<(Oid, u32)> = Vec::with_capacity(count.min(1 << 16));
        let mut pending: Vec<u32> = Vec::new();
        let mut offset = 12usize;
        for _ in 0..count {
            if offset >= content_end {
                return Err(Error::UnexpectedEof);
            }
            let (code, size, pos) = entry_header(data, offset, content_end)?;
            let (_, zlib_start) = base_ref(data, code, pos, offset, content_end)?;
            let inflated = zlib::inflate_zlib(&data[zlib_start..content_end], Some(size))?;
            if inflated.data.len() != size {
                return Err(Error::Corrupt("entry size"));
            }
            match code {
                OBJ_COMMIT | OBJ_TREE | OBJ_BLOB | OBJ_TAG => {
                    let kind = kind_of(code)?;
                    index.push((
                        crate::object::compute_oid(kind, &inflated.data),
                        offset as u32,
                    ));
                }
                _ => pending.push(offset as u32),
            }
            offset = zlib_start + inflated.consumed;
        }
        if offset != content_end {
            return Err(Error::Corrupt("trailing bytes after entries"));
        }
        index.sort_unstable();

        // pass 2: delta entry を base の解決可能なものから順に materialize する。
        // ofs delta の base は常に前方にあるが、ref delta は任意の位置を指せるため、
        // 進展が無くなるまで繰り返す (通常は 1 回で完了する)。
        while !pending.is_empty() {
            let mut unresolved = Vec::new();
            let mut resolved_any = false;
            for &entry_offset in &pending {
                match materialize(data, content_end, &index, entry_offset as usize) {
                    Ok((kind, body)) => {
                        index.push((crate::object::compute_oid(kind, &body), entry_offset));
                        resolved_any = true;
                    }
                    Err(Error::MissingBase) => unresolved.push(entry_offset),
                    Err(e) => return Err(e),
                }
            }
            if !resolved_any {
                return Err(Error::MissingBase);
            }
            index.sort_unstable();
            pending = unresolved;
        }

        Ok(Self {
            data,
            content_end,
            index,
        })
    }

    /// pack 内の object 数。
    pub fn len(&self) -> usize {
        self.index.len()
    }

    pub fn is_empty(&self) -> bool {
        self.index.is_empty()
    }

    pub fn contains(&self, oid: &Oid) -> bool {
        lookup(&self.index, oid).is_some()
    }

    /// pack 内の全 oid (oid 順)。
    pub fn oids(&self) -> impl Iterator<Item = &Oid> {
        self.index.iter().map(|(oid, _)| oid)
    }

    /// object を読み出す (delta は都度解決する)。
    pub fn read_object(&self, oid: &Oid) -> Result<Option<(Kind, Vec<u8>)>> {
        match lookup(&self.index, oid) {
            None => Ok(None),
            Some(offset) => {
                materialize(self.data, self.content_end, &self.index, offset as usize).map(Some)
            }
        }
    }
}

impl crate::Odb for Pack<'_> {
    fn read(&self, oid: &Oid) -> Option<(Kind, Vec<u8>)> {
        // parse 時に全 entry の materialize が成功しているため、ここでの失敗は
        // 実質的に到達しない。
        self.read_object(oid).ok().flatten()
    }
}

fn lookup(index: &[(Oid, u32)], oid: &Oid) -> Option<u32> {
    index
        .binary_search_by(|(o, _)| o.cmp(oid))
        .ok()
        .map(|i| index[i].1)
}

fn kind_of(code: u8) -> Result<Kind> {
    match code {
        OBJ_COMMIT => Ok(Kind::Commit),
        OBJ_TREE => Ok(Kind::Tree),
        OBJ_BLOB => Ok(Kind::Blob),
        OBJ_TAG => Ok(Kind::Tag),
        _ => Err(Error::Corrupt("object type code")),
    }
}

/// entry の base 参照。
enum BaseRef {
    None,
    /// base entry の絶対 offset。
    Offset(usize),
    Oid(Oid),
}

/// entry 先頭の type/size varint を読む。返り値は (type code, 伸長後サイズ, 次の位置)。
fn entry_header(data: &[u8], offset: usize, end: usize) -> Result<(u8, usize, usize)> {
    let mut pos = offset;
    let mut byte = *data
        .get(pos)
        .filter(|_| pos < end)
        .ok_or(Error::UnexpectedEof)?;
    pos += 1;
    let code = (byte >> 4) & 0x07;
    let mut size = usize::from(byte & 0x0f);
    let mut shift = 4;
    while byte & 0x80 != 0 {
        byte = *data
            .get(pos)
            .filter(|_| pos < end)
            .ok_or(Error::UnexpectedEof)?;
        pos += 1;
        if shift >= usize::BITS {
            return Err(Error::Corrupt("entry size varint"));
        }
        size |= usize::from(byte & 0x7f) << shift;
        shift += 7;
    }
    Ok((code, size, pos))
}

/// type/size varint の直後にある base 参照を読む。返り値は (base, zlib stream の開始位置)。
///
/// 読み取りは `end` (trailer の手前) で打ち切る。trailer は checksum であって
/// entry の一部ではなく、境界検査を怠ると checksum 込みで細工した pack が
/// `zlib_start > end` の slice を作り panic に至る。
fn base_ref(
    data: &[u8],
    code: u8,
    pos: usize,
    entry_offset: usize,
    end: usize,
) -> Result<(BaseRef, usize)> {
    match code {
        OBJ_COMMIT | OBJ_TREE | OBJ_BLOB | OBJ_TAG => Ok((BaseRef::None, pos)),
        OBJ_OFS_DELTA => {
            // 負 offset の varint (MSB first、継続時に +1 する git 独自形式)。
            let mut p = pos;
            let mut byte = *data
                .get(p)
                .filter(|_| p < end)
                .ok_or(Error::UnexpectedEof)?;
            p += 1;
            let mut value = u64::from(byte & 0x7f);
            while byte & 0x80 != 0 {
                byte = *data
                    .get(p)
                    .filter(|_| p < end)
                    .ok_or(Error::UnexpectedEof)?;
                p += 1;
                value = value
                    .checked_add(1)
                    .and_then(|v| v.checked_shl(7))
                    .ok_or(Error::Corrupt("ofs delta varint"))?
                    | u64::from(byte & 0x7f);
            }
            let base = (entry_offset as u64)
                .checked_sub(value)
                .filter(|&b| b >= 12)
                .ok_or(Error::Corrupt("ofs delta base offset"))?;
            Ok((BaseRef::Offset(base as usize), p))
        }
        OBJ_REF_DELTA => {
            if pos + 20 > end {
                return Err(Error::UnexpectedEof);
            }
            let bytes = &data[pos..pos + 20];
            Ok((
                BaseRef::Oid(Oid::from_bytes(bytes.try_into().unwrap())),
                pos + 20,
            ))
        }
        _ => Err(Error::Corrupt("object type code")),
    }
}

/// offset の entry を delta chain を解決しつつ復元する。
///
/// 再帰を使わず、chain を配列に積んでから base 側から適用する。組み込みの
/// 小さい stack でも chain 長に依存せず動作する。
fn materialize(
    data: &[u8],
    content_end: usize,
    index: &[(Oid, u32)],
    offset: usize,
) -> Result<(Kind, Vec<u8>)> {
    // chain: 適用すべき delta entry の zlib 開始位置と伸長後サイズ (外側から順)。
    let mut chain: Vec<(usize, usize)> = Vec::new();
    let mut cur = offset;

    let (kind, mut body) = loop {
        if chain.len() > MAX_DELTA_DEPTH {
            return Err(Error::Corrupt("delta chain too deep"));
        }
        let (code, size, pos) = entry_header(data, cur, content_end)?;
        let (base, zlib_start) = base_ref(data, code, pos, cur, content_end)?;
        match base {
            BaseRef::None => {
                let inflated = zlib::inflate_zlib(&data[zlib_start..content_end], Some(size))?;
                if inflated.data.len() != size {
                    return Err(Error::Corrupt("entry size"));
                }
                break (kind_of(code)?, inflated.data);
            }
            BaseRef::Offset(base_offset) => {
                chain.push((zlib_start, size));
                cur = base_offset;
            }
            BaseRef::Oid(oid) => {
                chain.push((zlib_start, size));
                cur = lookup(index, &oid).ok_or(Error::MissingBase)? as usize;
            }
        }
    };

    for &(zlib_start, size) in chain.iter().rev() {
        let inflated = zlib::inflate_zlib(&data[zlib_start..content_end], Some(size))?;
        if inflated.data.len() != size {
            return Err(Error::Corrupt("entry size"));
        }
        body = delta::apply(&body, &inflated.data)?;
    }
    Ok((kind, body))
}

/// object の列から packfile (version 2、非 delta) を生成する。
///
/// entry の zlib stream は無圧縮 (stored block)。push の転送量よりも実装の
/// 小ささを優先している (docs/design.md)。
#[cfg(feature = "write")]
pub fn write_pack(objects: &[(Kind, &[u8])]) -> Vec<u8> {
    let mut out = Vec::new();
    out.extend_from_slice(b"PACK");
    out.extend_from_slice(&2u32.to_be_bytes());
    out.extend_from_slice(&(objects.len() as u32).to_be_bytes());

    for (kind, body) in objects {
        let code: u8 = match kind {
            Kind::Commit => OBJ_COMMIT,
            Kind::Tree => OBJ_TREE,
            Kind::Blob => OBJ_BLOB,
            Kind::Tag => OBJ_TAG,
        };
        let mut size = body.len();
        let mut byte = (code << 4) | (size & 0x0f) as u8;
        size >>= 4;
        while size > 0 {
            out.push(byte | 0x80);
            byte = (size & 0x7f) as u8;
            size >>= 7;
        }
        out.push(byte);
        out.extend_from_slice(&zlib::deflate_zlib_stored(body));
    }

    let digest = sha1::digest(&out);
    out.extend_from_slice(&digest);
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    /// content に正しい SHA-1 trailer を付けて pack として成立させる。
    /// checksum は転送誤り検出であって悪意への防御ではないため、細工された
    /// pack も checksum は正しくなり得る。
    fn with_trailer(mut content: Vec<u8>) -> Vec<u8> {
        let digest = sha1::digest(&content);
        content.extend_from_slice(&digest);
        content
    }

    fn header(count: u32) -> Vec<u8> {
        let mut c = Vec::new();
        c.extend_from_slice(b"PACK");
        c.extend_from_slice(&2u32.to_be_bytes());
        c.extend_from_slice(&count.to_be_bytes());
        c
    }

    // ref delta の 20 byte base oid が trailer に食い込む pack を拒否すること
    // (panic ではなくエラーで返す)。
    #[test]
    fn ref_delta_base_into_trailer_rejected() {
        let mut c = header(1);
        c.push(0x70); // type=ref_delta, size=0
        c.extend_from_slice(&[0xaa; 5]); // base oid の途中で entry 領域が尽きる
        assert!(Pack::parse(&with_trailer(c)).is_err());
    }

    // ofs delta の負 offset varint が trailer に食い込む pack を拒否すること。
    #[test]
    fn ofs_delta_varint_into_trailer_rejected() {
        let mut c = header(1);
        c.push(0x60); // type=ofs_delta, size=0
        c.extend_from_slice(&[0x80; 3]); // 継続 bit が立ったまま entry 領域が尽きる
        assert!(Pack::parse(&with_trailer(c)).is_err());
    }

    // 空 (entry 0 件) の pack は正常に解析できること。
    #[test]
    fn empty_pack() {
        let pack = with_trailer(header(0));
        assert!(Pack::parse(&pack).unwrap().is_empty());
    }

    // 書いた pack を自前の parser (git との差分テスト済み) で往復できること。
    #[cfg(feature = "write")]
    #[test]
    fn write_pack_roundtrip() {
        let blob = b"hello\n".as_slice();
        let large: Vec<u8> = (0..100_000usize).map(|i| (i % 251) as u8).collect();
        let objects = [(Kind::Blob, blob), (Kind::Blob, large.as_slice())];
        let data = write_pack(&objects);

        let pack = Pack::parse(&data).unwrap();
        assert_eq!(pack.len(), 2);
        for (kind, body) in &objects {
            let oid = crate::object::compute_oid(*kind, body);
            let (got_kind, got_body) = pack.read_object(&oid).unwrap().unwrap();
            assert_eq!(got_kind, *kind);
            assert_eq!(got_body, *body);
        }
    }
}
