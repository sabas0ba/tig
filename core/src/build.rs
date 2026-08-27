//! object (tree / commit / tag) の生成。
//!
//! 生成する body は `object` module の parse 対象と同じ形式で、oid は
//! `object::compute_oid` で求める。blob の body は内容そのものであり、
//! 専用の builder を持たない。

use alloc::vec::Vec;

use crate::err::{Error, Result};
use crate::object::Sig;
use crate::oid::Oid;

/// tree に登録する entry。mode は 8 進表記のまま渡す (例: b"100644"、b"40000")。
#[derive(Debug, Clone)]
pub struct TreeEntry<'a> {
    pub mode: &'a [u8],
    pub name: &'a [u8],
    pub oid: Oid,
}

/// tree の body を生成する。entry は git の正規順 (directory は名前に '/' を
/// 補って比較する) に並べ替える。
pub fn tree(entries: &[TreeEntry<'_>]) -> Result<Vec<u8>> {
    for entry in entries {
        if entry.name.is_empty()
            || entry.name == b"."
            || entry.name == b".."
            || entry.name.contains(&b'/')
            || entry.name.contains(&0)
        {
            return Err(Error::Corrupt("tree entry name"));
        }
        if entry.mode.is_empty() || !entry.mode.iter().all(|b| (b'0'..=b'7').contains(b)) {
            return Err(Error::Corrupt("tree entry mode"));
        }
    }

    let mut sorted: Vec<&TreeEntry<'_>> = entries.iter().collect();
    sorted.sort_by(|a, b| cmp_tree(a, b));

    let mut out = Vec::new();
    for entry in sorted {
        out.extend_from_slice(entry.mode);
        out.push(b' ');
        out.extend_from_slice(entry.name);
        out.push(0);
        out.extend_from_slice(entry.oid.as_bytes());
    }
    Ok(out)
}

/// git の tree 順: directory は名前の末尾に '/' を補ったものとして byte 比較する
/// (例: "foo.txt" < "foo" (dir)、'.' 0x2e < '/' 0x2f のため)。
fn cmp_tree(a: &TreeEntry<'_>, b: &TreeEntry<'_>) -> core::cmp::Ordering {
    let pad = |e: &TreeEntry<'_>| if is_dir(e) { b'/' } else { 0 };
    let min = a.name.len().min(b.name.len());
    a.name[..min].cmp(&b.name[..min]).then_with(|| {
        let ac = a.name.get(min).copied().unwrap_or_else(|| pad(a));
        let bc = b.name.get(min).copied().unwrap_or_else(|| pad(b));
        ac.cmp(&bc)
    })
}

fn is_dir(entry: &TreeEntry<'_>) -> bool {
    entry.mode == b"40000" || entry.mode == b"040000"
}

/// commit の body を生成する。
pub fn commit(
    tree: Oid,
    parents: &[Oid],
    author: &Sig<'_>,
    committer: &Sig<'_>,
    message: &[u8],
) -> Result<Vec<u8>> {
    let mut out = Vec::new();
    out.extend_from_slice(b"tree ");
    push_hex(&mut out, &tree);
    out.push(b'\n');
    for parent in parents {
        out.extend_from_slice(b"parent ");
        push_hex(&mut out, parent);
        out.push(b'\n');
    }
    out.extend_from_slice(b"author ");
    push_sig(&mut out, author)?;
    out.extend_from_slice(b"committer ");
    push_sig(&mut out, committer)?;
    out.push(b'\n');
    out.extend_from_slice(message);
    Ok(out)
}

/// `name <email> time tz` + LF。
fn push_sig(out: &mut Vec<u8>, sig: &Sig<'_>) -> Result<()> {
    // 形式を壊す byte を拒否する (git の fsck と同等の最低限)。
    let bad = |field: &[u8]| field.iter().any(|&b| b == b'<' || b == b'>' || b == b'\n');
    if bad(sig.name) || bad(sig.email) || sig.tz.contains(&b'\n') {
        return Err(Error::Corrupt("signature field"));
    }
    if !sig.name.is_empty() {
        out.extend_from_slice(sig.name);
        out.push(b' ');
    }
    out.push(b'<');
    out.extend_from_slice(sig.email);
    out.extend_from_slice(b"> ");
    push_i64(out, sig.time);
    out.push(b' ');
    out.extend_from_slice(sig.tz);
    out.push(b'\n');
    Ok(())
}

fn push_hex(out: &mut Vec<u8>, oid: &Oid) {
    const HEX: &[u8; 16] = b"0123456789abcdef";
    for &b in oid.as_bytes() {
        out.push(HEX[usize::from(b >> 4)]);
        out.push(HEX[usize::from(b & 0xf)]);
    }
}

fn push_i64(out: &mut Vec<u8>, value: i64) {
    if value < 0 {
        out.push(b'-');
    }
    let mut v = value.unsigned_abs();
    let mut digits = [0u8; 20];
    let mut n = 0;
    loop {
        digits[n] = b'0' + (v % 10) as u8;
        v /= 10;
        n += 1;
        if v == 0 {
            break;
        }
    }
    for i in (0..n).rev() {
        out.push(digits[i]);
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::object::{self, Kind};

    fn oid(n: u8) -> Oid {
        Oid::from_bytes([n; 20])
    }

    // 生成した body が自前の parser で往復できること。
    #[test]
    fn commit_roundtrip() {
        let author = Sig {
            name: b"Alice",
            email: b"alice@example.com",
            time: 1_700_000_000,
            tz: b"+0900",
        };
        let body = commit(oid(1), &[oid(2), oid(3)], &author, &author, b"subject\n").unwrap();
        let parsed = object::parse_commit(&body).unwrap();
        assert_eq!(parsed.tree, oid(1));
        assert_eq!(parsed.parents, alloc::vec![oid(2), oid(3)]);
        assert_eq!(parsed.author, author);
        assert_eq!(parsed.message, b"subject\n");
    }

    #[test]
    fn tree_sorts_directories_padded() {
        // "foo" (dir) は "foo/" として比較され、"foo.txt" より後になる。
        let entries = [
            TreeEntry {
                mode: b"40000",
                name: b"foo",
                oid: oid(1),
            },
            TreeEntry {
                mode: b"100644",
                name: b"foo.txt",
                oid: oid(2),
            },
        ];
        let body = tree(&entries).unwrap();
        let names: Vec<&[u8]> = object::TreeIter::new(&body)
            .map(|e| e.unwrap().name)
            .collect();
        assert_eq!(names, alloc::vec![&b"foo.txt"[..], &b"foo"[..]]);
        // parse も往復できること。
        assert_eq!(object::compute_oid(Kind::Tree, &body), {
            let again = tree(&entries).unwrap();
            object::compute_oid(Kind::Tree, &again)
        });
    }

    #[test]
    fn invalid_names_rejected() {
        for name in [&b""[..], b".", b"..", b"a/b", b"a\0b"] {
            let entries = [TreeEntry {
                mode: b"100644",
                name,
                oid: oid(1),
            }];
            assert!(tree(&entries).is_err(), "name={name:?}");
        }
    }

    #[test]
    fn invalid_signature_rejected() {
        let bad = Sig {
            name: b"a<b",
            email: b"e",
            time: 0,
            tz: b"+0000",
        };
        assert!(commit(oid(1), &[], &bad, &bad, b"m").is_err());
    }
}
