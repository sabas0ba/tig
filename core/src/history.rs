//! commit history の walk。
//!
//! `git log --date-order` と同じ規則で、発見済み commit のうち committer date が
//! 最新のものから順に返す。到達した parent が store に無い場合 (shallow な入力や
//! bundle の prerequisite) は、そこを履歴の境界として黙って打ち切る。

use alloc::collections::{BTreeSet, BinaryHeap};
use alloc::vec::Vec;

use crate::Odb;
use crate::err::{Error, Result};
use crate::object::{self, Kind};
use crate::oid::Oid;

/// walk が返す commit。body は所有権ごと返す (parse は `commit()` で行う)。
pub struct WalkedCommit {
    pub oid: Oid,
    pub raw: Vec<u8>,
}

impl WalkedCommit {
    /// body を解析して構造化された commit を返す。
    pub fn commit(&self) -> Result<object::Commit<'_>> {
        object::parse_commit(&self.raw)
    }
}

pub struct Walk<'a, O: Odb> {
    odb: &'a O,
    /// (committer date, oid) の max-heap。date 同点は oid 降順で決定的にする。
    queue: BinaryHeap<(i64, Oid)>,
    seen: BTreeSet<Oid>,
}

impl<'a, O: Odb> Walk<'a, O> {
    pub fn new(odb: &'a O) -> Self {
        Self {
            odb,
            queue: BinaryHeap::new(),
            seen: BTreeSet::new(),
        }
    }

    /// 開始点を追加する。annotated tag は commit まで剥がす。
    /// 対象が store に無い場合はエラー。
    pub fn push(&mut self, oid: Oid) -> Result<()> {
        let mut oid = oid;
        // tag が tag を指す入れ子に備えて有限回で打ち切る。
        for _ in 0..16 {
            let (kind, body) = self.odb.read(&oid).ok_or(Error::MissingBase)?;
            match kind {
                Kind::Commit => {
                    if self.seen.insert(oid) {
                        let commit = object::parse_commit(&body)?;
                        self.queue.push((commit.committer.time, oid));
                    }
                    return Ok(());
                }
                Kind::Tag => {
                    oid = object::parse_tag(&body)?.object;
                }
                _ => return Err(Error::Corrupt("start point is not a commit")),
            }
        }
        Err(Error::Corrupt("tag nesting too deep"))
    }
}

impl<O: Odb> Iterator for Walk<'_, O> {
    type Item = Result<WalkedCommit>;

    fn next(&mut self) -> Option<Self::Item> {
        let (_, oid) = self.queue.pop()?;
        let Some((kind, raw)) = self.odb.read(&oid) else {
            // queue には store に存在する commit だけを入れているため到達しない。
            return Some(Err(Error::MissingBase));
        };
        if kind != Kind::Commit {
            return Some(Err(Error::Corrupt("walked object is not a commit")));
        }

        let commit = match object::parse_commit(&raw) {
            Ok(c) => c,
            Err(e) => return Some(Err(e)),
        };
        for &parent in &commit.parents {
            if !self.seen.contains(&parent) {
                // 存在しない parent は履歴の境界 (prerequisite / shallow)。
                if let Some((Kind::Commit, body)) = self.odb.read(&parent) {
                    match object::parse_commit(&body) {
                        Ok(p) => {
                            self.seen.insert(parent);
                            self.queue.push((p.committer.time, parent));
                        }
                        Err(e) => return Some(Err(e)),
                    }
                }
            }
        }
        Some(Ok(WalkedCommit { oid, raw }))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::object::Kind;
    use alloc::vec;

    /// テスト用の単純な in-memory store。
    struct MemOdb(Vec<(Oid, Kind, Vec<u8>)>);

    impl Odb for MemOdb {
        fn read(&self, oid: &Oid) -> Option<(Kind, Vec<u8>)> {
            self.0
                .iter()
                .find(|(o, _, _)| o == oid)
                .map(|(_, k, b)| (*k, b.clone()))
        }
    }

    fn commit(store: &mut MemOdb, parents: &[Oid], time: i64) -> Oid {
        let mut body = Vec::new();
        body.extend_from_slice(b"tree e69de29bb2d1d6434b8b29ae775ad8c2e48c5391\n");
        for p in parents {
            body.extend_from_slice(format!("parent {p}\n").as_bytes());
        }
        body.extend_from_slice(format!("author A <a@e> {time} +0000\n").as_bytes());
        body.extend_from_slice(format!("committer A <a@e> {time} +0000\n").as_bytes());
        body.extend_from_slice(b"\nmsg\n");
        let oid = crate::object::compute_oid(Kind::Commit, &body);
        store.0.push((oid, Kind::Commit, body));
        oid
    }

    #[test]
    fn linear_history_newest_first() {
        let mut store = MemOdb(vec![]);
        let c1 = commit(&mut store, &[], 100);
        let c2 = commit(&mut store, &[c1], 200);
        let c3 = commit(&mut store, &[c2], 300);

        let mut walk = Walk::new(&store);
        walk.push(c3).unwrap();
        let oids: Vec<Oid> = walk.map(|c| c.unwrap().oid).collect();
        assert_eq!(oids, vec![c3, c2, c1]);
    }

    #[test]
    fn merge_interleaved_by_date() {
        let mut store = MemOdb(vec![]);
        let base = commit(&mut store, &[], 100);
        let a = commit(&mut store, &[base], 300);
        let b = commit(&mut store, &[base], 200);
        let m = commit(&mut store, &[a, b], 400);

        let mut walk = Walk::new(&store);
        walk.push(m).unwrap();
        let oids: Vec<Oid> = walk.map(|c| c.unwrap().oid).collect();
        assert_eq!(oids, vec![m, a, b, base]);
    }

    #[test]
    fn missing_parent_is_boundary() {
        let mut store = MemOdb(vec![]);
        let absent = commit(&mut store, &[], 50);
        store.0.clear();
        let c = commit(&mut store, &[absent], 100);

        let mut walk = Walk::new(&store);
        walk.push(c).unwrap();
        let oids: Vec<Oid> = walk.map(|r| r.unwrap().oid).collect();
        assert_eq!(oids, vec![c]);
    }

    #[test]
    fn duplicate_start_points_dedupe() {
        let mut store = MemOdb(vec![]);
        let c1 = commit(&mut store, &[], 100);
        let c2 = commit(&mut store, &[c1], 200);

        let mut walk = Walk::new(&store);
        walk.push(c2).unwrap();
        walk.push(c2).unwrap();
        walk.push(c1).unwrap();
        let oids: Vec<Oid> = walk.map(|r| r.unwrap().oid).collect();
        assert_eq!(oids, vec![c2, c1]);
    }
}
