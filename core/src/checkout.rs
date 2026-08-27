//! tree の展開 (checkout)。
//!
//! filesystem は持たず、tree を辿って path / 種別 / 内容を callback で渡す。
//! ファイルへの書き出しや権限の設定は frontend (CLI 等) の責務とする。
//! 再帰は使わず、明示的な stack で辿る。

use alloc::vec::Vec;

use crate::Odb;
use crate::err::{Error, Result};
use crate::object::{Kind, TreeIter};
use crate::oid::Oid;

/// entry の種別。git の mode から導出する。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum EntryKind {
    File,
    Executable,
    /// content はリンク先の path。
    Symlink,
    /// submodule。content は指し先 commit の oid (20 byte 生値) で、blob は存在しない。
    Gitlink,
}

/// `walk` の callback。引数は (path, 種別, 内容)。
pub type Visit<'a> = dyn FnMut(&[u8], EntryKind, &[u8]) -> Result<()> + 'a;

/// tree 直下から再帰的に entry を列挙し、`visit(path, kind, content)` を呼ぶ。
/// path は '/' 区切り (先頭に '/' なし)。列挙順は tree の格納順 (名前順)。
pub fn walk<O: Odb>(odb: &O, tree: Oid, visit: &mut Visit<'_>) -> Result<()> {
    let root = read_tree(odb, &tree)?;
    // (path prefix, tree body, body 内の現在位置)
    let mut stack: Vec<(Vec<u8>, Vec<u8>, usize)> = alloc::vec![(Vec::new(), root, 0)];

    loop {
        // 現在の tree から次の entry を読み、必要な値を所有権ごと取り出してから
        // stack を更新する (借用を跨がないため)。
        let Some((prefix, body, pos)) = stack.last() else {
            return Ok(());
        };
        let Some(entry) = TreeIter::new(&body[*pos..]).next() else {
            stack.pop();
            continue;
        };
        let entry = entry?;
        // entry の占有長: mode + SP + name + NUL + oid(20)。
        let consumed = entry.mode.len() + 1 + entry.name.len() + 1 + 20;

        let mut path = prefix.clone();
        if !path.is_empty() {
            path.push(b'/');
        }
        path.extend_from_slice(entry.name);
        let (mode, oid) = (entry.mode.to_vec(), entry.oid);

        let top = stack.last_mut().expect("stack is non-empty");
        top.2 += consumed;

        match mode.as_slice() {
            b"40000" | b"040000" => {
                if stack.len() >= 256 {
                    return Err(Error::Corrupt("tree nesting too deep"));
                }
                let child = read_tree(odb, &oid)?;
                stack.push((path, child, 0));
            }
            b"160000" => visit(&path, EntryKind::Gitlink, oid.as_bytes())?,
            _ => {
                let kind = match mode.as_slice() {
                    b"100644" | b"100664" => EntryKind::File,
                    b"100755" => EntryKind::Executable,
                    b"120000" => EntryKind::Symlink,
                    _ => return Err(Error::Corrupt("tree entry mode")),
                };
                let Some((Kind::Blob, content)) = odb.read(&oid) else {
                    return Err(Error::MissingBase);
                };
                visit(&path, kind, &content)?;
            }
        }
    }
}

fn read_tree<O: Odb>(odb: &O, oid: &Oid) -> Result<Vec<u8>> {
    match odb.read(oid) {
        Some((Kind::Tree, body)) => Ok(body),
        Some(_) => Err(Error::Corrupt("not a tree")),
        None => Err(Error::MissingBase),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::object::compute_oid;
    use alloc::vec;

    struct MemOdb(Vec<(Oid, Kind, Vec<u8>)>);

    impl MemOdb {
        fn put(&mut self, kind: Kind, body: Vec<u8>) -> Oid {
            let oid = compute_oid(kind, &body);
            self.0.push((oid, kind, body));
            oid
        }
    }

    impl Odb for MemOdb {
        fn read(&self, oid: &Oid) -> Option<(Kind, Vec<u8>)> {
            self.0
                .iter()
                .find(|(o, _, _)| o == oid)
                .map(|(_, k, b)| (*k, b.clone()))
        }
    }

    fn tree_body(entries: &[(&[u8], &[u8], Oid)]) -> Vec<u8> {
        let mut body = Vec::new();
        for (mode, name, oid) in entries {
            body.extend_from_slice(mode);
            body.push(b' ');
            body.extend_from_slice(name);
            body.push(0);
            body.extend_from_slice(oid.as_bytes());
        }
        body
    }

    #[test]
    fn walks_nested_trees_in_order() {
        let mut odb = MemOdb(vec![]);
        let blob_a = odb.put(Kind::Blob, b"A".to_vec());
        let blob_b = odb.put(Kind::Blob, b"B".to_vec());
        let sub = odb.put(Kind::Tree, tree_body(&[(b"100755", b"run.sh", blob_b)]));
        let root = odb.put(
            Kind::Tree,
            tree_body(&[(b"100644", b"a.txt", blob_a), (b"40000", b"dir", sub)]),
        );

        let mut seen: Vec<(Vec<u8>, EntryKind, Vec<u8>)> = Vec::new();
        walk(&odb, root, &mut |path, kind, content| {
            seen.push((path.to_vec(), kind, content.to_vec()));
            Ok(())
        })
        .unwrap();

        assert_eq!(
            seen,
            vec![
                (b"a.txt".to_vec(), EntryKind::File, b"A".to_vec()),
                (b"dir/run.sh".to_vec(), EntryKind::Executable, b"B".to_vec()),
            ]
        );
    }

    #[test]
    fn missing_blob_is_error() {
        let mut odb = MemOdb(vec![]);
        let ghost = compute_oid(Kind::Blob, b"ghost");
        let root = odb.put(Kind::Tree, tree_body(&[(b"100644", b"x", ghost)]));
        assert_eq!(
            walk(&odb, root, &mut |_, _, _| Ok(())).unwrap_err(),
            Error::MissingBase
        );
    }
}
