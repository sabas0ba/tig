//! git bundle (v2 / v3) の読み込み。
//!
//! bundle は `git bundle create` が生成する単一ファイルで、「署名行 + (v3 のみ
//! capability 行) + prerequisite / ref の列 + 空行 + packfile」から成る。
//! オフライン配布した repository を読むための入力形式として用いる。

use alloc::vec::Vec;

use crate::err::{Error, Result};
use crate::oid::Oid;
use crate::pack::Pack;

pub struct Bundle<'a> {
    /// ref 名 (例: b"refs/heads/main") と指し先。ファイル中の出現順。
    pub refs: Vec<(&'a [u8], Oid)>,
    /// bundle に含まれない前提 commit。thin な bundle の履歴境界になる。
    pub prerequisites: Vec<Oid>,
    pub pack: Pack<'a>,
}

impl<'a> Bundle<'a> {
    pub fn parse(data: &'a [u8]) -> Result<Self> {
        let (signature, mut rest) = split_line(data)?;
        let v3 = match signature {
            b"# v2 git bundle" => false,
            b"# v3 git bundle" => true,
            _ => return Err(Error::Unsupported("bundle signature")),
        };

        // v3 の capability 行。object-format=sha1 以外は未対応として拒否する
        // (黙って誤読しないため)。
        if v3 {
            while rest.first() == Some(&b'@') {
                let (line, next) = split_line(rest)?;
                rest = next;
                match &line[1..] {
                    b"object-format=sha1" => {}
                    _ => return Err(Error::Unsupported("bundle capability")),
                }
            }
        }

        let mut refs = Vec::new();
        let mut prerequisites = Vec::new();
        loop {
            let (line, next) = split_line(rest)?;
            rest = next;
            if line.is_empty() {
                break;
            }
            if let Some(v) = line.strip_prefix(b"-") {
                // "-<oid> <コメント>"。コメントは任意。
                let hex = v.get(..40).ok_or(Error::Corrupt("bundle prerequisite"))?;
                prerequisites.push(Oid::from_hex(hex)?);
            } else {
                let hex = line.get(..40).ok_or(Error::Corrupt("bundle ref"))?;
                let name = line
                    .get(41..)
                    .filter(|n| !n.is_empty() && line[40] == b' ')
                    .ok_or(Error::Corrupt("bundle ref name"))?;
                refs.push((name, Oid::from_hex(hex)?));
            }
        }

        Ok(Self {
            refs,
            prerequisites,
            pack: Pack::parse(rest)?,
        })
    }

    /// 名前で ref を引く。
    pub fn find_ref(&self, name: &[u8]) -> Option<Oid> {
        self.refs
            .iter()
            .find(|(n, _)| *n == name)
            .map(|(_, oid)| *oid)
    }
}

fn split_line(data: &[u8]) -> Result<(&[u8], &[u8])> {
    let nl = data
        .iter()
        .position(|&b| b == b'\n')
        .ok_or(Error::Corrupt("bundle header line"))?;
    Ok((&data[..nl], &data[nl + 1..]))
}

/// refs と packfile から bundle v2 を構成する。
///
/// `shallow` は fetch が報告した履歴の打ち切り点。その commit の parent のうち
/// pack に含まれないものを prerequisite として記録する (bundle に無い前提
/// commit を明示し、本ライブラリの walk と git の双方が境界を判定できるように
/// する)。
pub fn write(refs: &[(&[u8], Oid)], shallow: &[Oid], pack_data: &[u8]) -> Result<Vec<u8>> {
    use crate::object::{self, Kind};

    let mut prerequisites: Vec<Oid> = Vec::new();
    if !shallow.is_empty() {
        let pack = Pack::parse(pack_data)?;
        for oid in shallow {
            let Some((Kind::Commit, body)) = pack.read_object(oid)? else {
                return Err(Error::Corrupt("shallow oid not in pack"));
            };
            for parent in object::parse_commit(&body)?.parents {
                if !pack.contains(&parent) && !prerequisites.contains(&parent) {
                    prerequisites.push(parent);
                }
            }
        }
    }

    let mut out = Vec::with_capacity(pack_data.len() + 64 * (refs.len() + 2));
    out.extend_from_slice(b"# v2 git bundle\n");
    for oid in &prerequisites {
        out.push(b'-');
        push_hex(&mut out, oid);
        out.push(b'\n');
    }
    for (name, oid) in refs {
        push_hex(&mut out, oid);
        out.push(b' ');
        out.extend_from_slice(name);
        out.push(b'\n');
    }
    out.push(b'\n');
    out.extend_from_slice(pack_data);
    Ok(out)
}

fn push_hex(out: &mut Vec<u8>, oid: &Oid) {
    const HEX: &[u8; 16] = b"0123456789abcdef";
    for &b in oid.as_bytes() {
        out.push(HEX[usize::from(b >> 4)]);
        out.push(HEX[usize::from(b & 0xf)]);
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::sha1;
    use alloc::vec;

    /// entry 0 件の pack (正しい trailer 付き)。
    fn empty_pack() -> Vec<u8> {
        let mut c = Vec::new();
        c.extend_from_slice(b"PACK");
        c.extend_from_slice(&2u32.to_be_bytes());
        c.extend_from_slice(&0u32.to_be_bytes());
        let digest = sha1::digest(&c);
        c.extend_from_slice(&digest);
        c
    }

    #[test]
    fn write_parse_roundtrip() {
        let oid = Oid::from_bytes([0x42; 20]);
        let pack = empty_pack();
        let data = write(&[(b"refs/heads/main", oid)], &[], &pack).unwrap();

        let bundle = Bundle::parse(&data).unwrap();
        assert_eq!(bundle.refs, vec![(&b"refs/heads/main"[..], oid)]);
        assert!(bundle.prerequisites.is_empty());
        assert!(bundle.pack.is_empty());
    }
}
