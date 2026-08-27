//! commit history の walk。
//!
//! `git log --date-order` と同じ規則で返す: parent はその子がすべて出力されるまで
//! 出力せず (topology 制約)、その制約の下で committer date の新しいものから順に
//! 選ぶ。committer date が単調な履歴では単純な date 順と一致するが、clock skew の
//! ある履歴や、ancestor を指す ref が別の tip と並ぶ場合は topology 制約が効く。
//!
//! 実装は git と同様に 2 段階を踏む。最初の取り出しで到達可能な commit を全て
//! 辿って「未出力の子の数」を数え、以後は子を出し切った commit だけを date 順の
//! heap から取り出す (Kahn の topological sort の date 優先版)。到達した parent が
//! store に無い場合 (shallow な入力や bundle の prerequisite) は、そこを履歴の
//! 境界として黙って打ち切る。

use alloc::collections::{BTreeMap, BinaryHeap};
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

/// 到達可能集合に載った commit の walk 用情報。
struct Node {
    time: i64,
    parents: Vec<Oid>,
    /// まだ出力されていない子の数。0 になった commit だけが出力候補になる。
    pending_children: u32,
}

pub struct Walk<'a, O: Odb> {
    odb: &'a O,
    tips: Vec<Oid>,
    prepared: bool,
    nodes: BTreeMap<Oid, Node>,
    /// 出力候補 (pending_children == 0) の (committer date, oid) max-heap。
    /// date 同点は oid で決定的にする (git のような投入順ではない)。
    ready: BinaryHeap<(i64, Oid)>,
}

impl<'a, O: Odb> Walk<'a, O> {
    pub fn new(odb: &'a O) -> Self {
        Self {
            odb,
            tips: Vec::new(),
            prepared: false,
            nodes: BTreeMap::new(),
            ready: BinaryHeap::new(),
        }
    }

    /// 開始点を追加する。annotated tag は commit まで剥がす。
    /// 対象が store に無い場合はエラー。最初の取り出しの後は追加できない。
    pub fn push(&mut self, oid: Oid) -> Result<()> {
        debug_assert!(!self.prepared, "push after iteration start");
        let mut oid = oid;
        // tag が tag を指す入れ子に備えて有限回で打ち切る。
        for _ in 0..16 {
            let (kind, body) = self.odb.read(&oid).ok_or(Error::MissingBase)?;
            match kind {
                Kind::Commit => {
                    object::parse_commit(&body)?;
                    self.tips.push(oid);
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

    /// 到達可能な commit を全て辿り、pending_children と初期の出力候補を確定する。
    fn prepare(&mut self) -> Result<()> {
        self.prepared = true;
        let mut stack: Vec<Oid> = self.tips.clone();
        while let Some(oid) = stack.pop() {
            if self.nodes.contains_key(&oid) {
                continue;
            }
            // 存在しない・commit でない parent は境界として集合に載せない。
            let Some((Kind::Commit, body)) = self.odb.read(&oid) else {
                continue;
            };
            let commit = object::parse_commit(&body)?;
            let parents = unique(&commit.parents);
            for &parent in &parents {
                stack.push(parent);
            }
            self.nodes.insert(
                oid,
                Node {
                    time: commit.committer.time,
                    parents,
                    pending_children: 0,
                },
            );
        }

        // 子の数を数える。nodes に載っている commit 同士の辺だけが対象。
        let edges: Vec<Oid> = self
            .nodes
            .values()
            .flat_map(|n| n.parents.iter().copied())
            .collect();
        for parent in edges {
            if let Some(node) = self.nodes.get_mut(&parent) {
                node.pending_children += 1;
            }
        }

        for (oid, node) in &self.nodes {
            if node.pending_children == 0 {
                self.ready.push((node.time, *oid));
            }
        }
        Ok(())
    }
}

/// 出現順を保ったまま重複を除く (merge が同じ parent を重複して持つ場合に備える)。
fn unique(oids: &[Oid]) -> Vec<Oid> {
    let mut out: Vec<Oid> = Vec::with_capacity(oids.len());
    for &oid in oids {
        if !out.contains(&oid) {
            out.push(oid);
        }
    }
    out
}

impl<O: Odb> Iterator for Walk<'_, O> {
    type Item = Result<WalkedCommit>;

    fn next(&mut self) -> Option<Self::Item> {
        if !self.prepared
            && let Err(e) = self.prepare()
        {
            return Some(Err(e));
        }

        let (_, oid) = self.ready.pop()?;
        let parents = match self.nodes.get(&oid) {
            Some(node) => node.parents.clone(),
            None => return Some(Err(Error::Corrupt("walked oid without node"))),
        };
        for parent in parents {
            if let Some(node) = self.nodes.get_mut(&parent) {
                node.pending_children -= 1;
                if node.pending_children == 0 {
                    self.ready.push((node.time, parent));
                }
            }
        }

        match self.odb.read(&oid) {
            // prepare で読めた commit だけを nodes に載せているため、通常は到達しない。
            None => Some(Err(Error::MissingBase)),
            Some((Kind::Commit, raw)) => Some(Ok(WalkedCommit { oid, raw })),
            Some(_) => Some(Err(Error::Corrupt("walked object is not a commit"))),
        }
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

    fn walk_oids<O: Odb>(odb: &O, tips: &[Oid]) -> Vec<Oid> {
        let mut walk = Walk::new(odb);
        for &tip in tips {
            walk.push(tip).unwrap();
        }
        walk.map(|c| c.unwrap().oid).collect()
    }

    #[test]
    fn linear_history_newest_first() {
        let mut store = MemOdb(vec![]);
        let c1 = commit(&mut store, &[], 100);
        let c2 = commit(&mut store, &[c1], 200);
        let c3 = commit(&mut store, &[c2], 300);
        assert_eq!(walk_oids(&store, &[c3]), vec![c3, c2, c1]);
    }

    #[test]
    fn merge_interleaved_by_date() {
        let mut store = MemOdb(vec![]);
        let base = commit(&mut store, &[], 100);
        let a = commit(&mut store, &[base], 300);
        let b = commit(&mut store, &[base], 200);
        let m = commit(&mut store, &[a, b], 400);
        assert_eq!(walk_oids(&store, &[m]), vec![m, a, b, base]);
    }

    #[test]
    fn missing_parent_is_boundary() {
        let mut store = MemOdb(vec![]);
        let absent = commit(&mut store, &[], 50);
        store.0.clear();
        let c = commit(&mut store, &[absent], 100);
        assert_eq!(walk_oids(&store, &[c]), vec![c]);
    }

    #[test]
    fn duplicate_start_points_dedupe() {
        let mut store = MemOdb(vec![]);
        let c1 = commit(&mut store, &[], 100);
        let c2 = commit(&mut store, &[c1], 200);
        assert_eq!(walk_oids(&store, &[c2, c2, c1]), vec![c2, c1]);
    }

    // ancestor を指す tip の committer date が descendant の tip より新しくても、
    // 子を出し切るまで parent を出力しない (--date-order の topology 制約)。
    #[test]
    fn ancestor_tip_with_newer_date_waits_for_child() {
        let mut store = MemOdb(vec![]);
        let p = commit(&mut store, &[], 2000);
        let c = commit(&mut store, &[p], 1000);
        assert_eq!(walk_oids(&store, &[c, p]), vec![c, p]);
    }

    // 分岐内の clock skew。P(1000) の子 C(500) と D(800)、merge M(1200) の walk は
    // M, D, C, P の順になる (P は date では D より新しいが、C を出すまで待つ)。
    #[test]
    fn skewed_diamond_respects_topology() {
        let mut store = MemOdb(vec![]);
        let p = commit(&mut store, &[], 1000);
        let c = commit(&mut store, &[p], 500);
        let d = commit(&mut store, &[p], 800);
        let m = commit(&mut store, &[c, d], 1200);
        assert_eq!(walk_oids(&store, &[m]), vec![m, d, c, p]);
    }

    // 同じ parent を重複して持つ merge でも二重に数えず、walk が完走すること。
    #[test]
    fn duplicate_parents_counted_once() {
        let mut store = MemOdb(vec![]);
        let p = commit(&mut store, &[], 100);
        let m = commit(&mut store, &[p, p], 200);
        assert_eq!(walk_oids(&store, &[m]), vec![m, p]);
    }
}
