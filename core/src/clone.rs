//! smart HTTP からの clone (sans-io の状態機械)。
//!
//! HTTP client は持たない。呼び出し側は `next_request()` が返す request を
//! repository URL に対して送り、response body を `on_response()` に渡す。
//! これを request が無くなるまで繰り返し、`finish()` で結果を取り出す。
//!
//! ```text
//! GET  <url>/info/refs?service=git-upload-pack   (capability advertisement)
//! POST <url>/git-upload-pack                     (ls-refs)
//! POST <url>/git-upload-pack                     (fetch)
//! ```
//!
//! いずれの request にも `Git-Protocol: version=2` header を付けること。

use alloc::vec::Vec;

use crate::err::{Error, Result};
use crate::oid::Oid;
use crate::protov2::{self, RefEntry};

/// capability advertisement の要求先 (repository URL からの相対 path)。
pub const INFO_REFS_PATH: &str = "info/refs?service=git-upload-pack";
/// command の要求先 (repository URL からの相対 path)。
pub const UPLOAD_PACK_PATH: &str = "git-upload-pack";
/// POST body の Content-Type。
pub const REQUEST_CONTENT_TYPE: &str = "application/x-git-upload-pack-request";
/// 全 request に付ける header。
pub const PROTOCOL_HEADER: (&str, &str) = ("Git-Protocol", "version=2");

/// 呼び出し側が送るべき HTTP request。path は repository URL からの相対。
#[derive(Debug, PartialEq, Eq)]
pub enum Request {
    Get { path: &'static str },
    Post { path: &'static str, body: Vec<u8> },
}

#[derive(Debug, Clone, Default)]
pub struct CloneOptions {
    /// shallow clone の深さ。None は全履歴。
    pub depth: Option<u32>,
    /// 取得する ref 名 (完全一致、例: b"refs/heads/main")。None は全 ref。
    pub want_ref: Option<Vec<u8>>,
}

enum State {
    InfoRefs,
    LsRefs,
    Fetch,
    Done,
}

pub struct Clone {
    state: State,
    opts: CloneOptions,
    refs: Vec<RefEntry>,
    shallow: Vec<Oid>,
    pack: Vec<u8>,
}

/// clone の結果。`bundle::write` にそのまま渡せる。
pub struct CloneOutcome {
    pub refs: Vec<RefEntry>,
    /// 履歴を打ち切った commit (depth 指定時)。
    pub shallow: Vec<Oid>,
    pub pack: Vec<u8>,
}

impl Clone {
    pub fn new(opts: CloneOptions) -> Self {
        Self {
            state: State::InfoRefs,
            opts,
            refs: Vec::new(),
            shallow: Vec::new(),
            pack: Vec::new(),
        }
    }

    /// 次に送るべき request。None なら完了。
    pub fn next_request(&self) -> Option<Request> {
        match self.state {
            State::InfoRefs => Some(Request::Get {
                path: INFO_REFS_PATH,
            }),
            State::LsRefs => Some(Request::Post {
                path: UPLOAD_PACK_PATH,
                body: protov2::ls_refs_request(&[]),
            }),
            State::Fetch => Some(Request::Post {
                path: UPLOAD_PACK_PATH,
                body: protov2::fetch_request(&self.wants(), self.opts.depth),
            }),
            State::Done => None,
        }
    }

    /// 直前の request に対する response body を渡して状態を進める。
    pub fn on_response(&mut self, body: &[u8]) -> Result<()> {
        match self.state {
            State::InfoRefs => {
                let adv = protov2::parse_advertisement(body)?;
                if !adv.ls_refs || !adv.fetch {
                    return Err(Error::Unsupported("server without ls-refs/fetch"));
                }
                if self.opts.depth.is_some() && !adv.fetch_shallow {
                    return Err(Error::Unsupported("server without shallow fetch"));
                }
                self.state = State::LsRefs;
            }
            State::LsRefs => {
                self.refs = protov2::parse_ls_refs(body)?;
                if self.wants().is_empty() {
                    return Err(Error::Corrupt("no matching ref to fetch"));
                }
                self.state = State::Fetch;
            }
            State::Fetch => {
                let resp = protov2::parse_fetch_response(body)?;
                self.shallow = resp.shallow;
                self.pack = resp.pack;
                self.state = State::Done;
            }
            State::Done => return Err(Error::Corrupt("response after completion")),
        }
        Ok(())
    }

    /// fetch で want する oid の一覧 (重複除去済み)。
    fn wants(&self) -> Vec<Oid> {
        let mut wants: Vec<Oid> = Vec::new();
        for entry in &self.refs {
            if let Some(name) = &self.opts.want_ref
                && entry.name != *name
            {
                continue;
            }
            if !wants.contains(&entry.oid) {
                wants.push(entry.oid);
            }
        }
        wants
    }

    pub fn is_done(&self) -> bool {
        matches!(self.state, State::Done)
    }

    pub fn finish(self) -> Result<CloneOutcome> {
        if !self.is_done() {
            return Err(Error::Corrupt("clone not finished"));
        }
        let refs = match &self.opts.want_ref {
            None => self.refs,
            Some(name) => self.refs.into_iter().filter(|e| e.name == *name).collect(),
        };
        Ok(CloneOutcome {
            refs,
            shallow: self.shallow,
            pack: self.pack,
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::pkt;

    fn advertisement() -> Vec<u8> {
        let mut b = Vec::new();
        pkt::write_line(&mut b, b"version 2");
        pkt::write_line(&mut b, b"ls-refs");
        pkt::write_line(&mut b, b"fetch=shallow");
        pkt::write_flush(&mut b);
        b
    }

    fn ls_refs_response(hex: &str) -> Vec<u8> {
        let mut b = Vec::new();
        pkt::write_line(&mut b, format!("{hex} refs/heads/main").as_bytes());
        pkt::write_flush(&mut b);
        b
    }

    fn fetch_response() -> Vec<u8> {
        let mut b = Vec::new();
        pkt::write_line(&mut b, b"packfile");
        pkt::write_data(&mut b, &[1, b'P', b'A', b'C', b'K']);
        pkt::write_flush(&mut b);
        b
    }

    #[test]
    fn walks_three_requests() {
        let hex = "1111111111111111111111111111111111111111";
        let mut clone = Clone::new(CloneOptions::default());

        assert!(matches!(clone.next_request(), Some(Request::Get { .. })));
        clone.on_response(&advertisement()).unwrap();

        let Some(Request::Post { body, .. }) = clone.next_request() else {
            panic!("ls-refs request expected");
        };
        assert!(String::from_utf8_lossy(&body).contains("command=ls-refs"));
        clone.on_response(&ls_refs_response(hex)).unwrap();

        let Some(Request::Post { body, .. }) = clone.next_request() else {
            panic!("fetch request expected");
        };
        let text = String::from_utf8_lossy(&body);
        assert!(text.contains("command=fetch"));
        assert!(text.contains(&format!("want {hex}")));
        clone.on_response(&fetch_response()).unwrap();

        assert!(clone.is_done());
        assert_eq!(clone.finish().unwrap().pack, b"PACK");
    }

    #[test]
    fn depth_requires_shallow_capability() {
        let mut b = Vec::new();
        pkt::write_line(&mut b, b"version 2");
        pkt::write_line(&mut b, b"ls-refs");
        pkt::write_line(&mut b, b"fetch");
        pkt::write_flush(&mut b);

        let mut clone = Clone::new(CloneOptions {
            depth: Some(1),
            want_ref: None,
        });
        clone.next_request();
        assert_eq!(
            clone.on_response(&b).unwrap_err(),
            Error::Unsupported("server without shallow fetch")
        );
    }

    #[test]
    fn missing_ref_is_error() {
        let mut clone = Clone::new(CloneOptions {
            depth: None,
            want_ref: Some(b"refs/heads/nonexistent".to_vec()),
        });
        clone.next_request();
        clone.on_response(&advertisement()).unwrap();
        assert!(
            clone
                .on_response(&ls_refs_response(
                    "1111111111111111111111111111111111111111"
                ))
                .is_err()
        );
    }
}
