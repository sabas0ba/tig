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
