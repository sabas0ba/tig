//! smart HTTP への push (sans-io の状態機械)。
//!
//! fetch 側 (`clone`) と同じ方針で、HTTP の送受信は呼び出し側の責務とする。
//!
//! ```text
//! GET  <url>/info/refs?service=git-receive-pack   (advertisement)
//! POST <url>/git-receive-pack                     (update commands + packfile)
//! ```
//!
//! 送る packfile は呼び出し側が用意する (bundle の全 object を `pack::write_pack`
//! で詰め直す等)。remote が既に持つ object が含まれていても害はない。

use alloc::vec::Vec;

use crate::err::{Error, Result};
use crate::oid::Oid;
use crate::protov0::{self, Command, PushReport, ZERO_OID};

/// advertisement の要求先 (repository URL からの相対 path)。
pub const INFO_REFS_PATH: &str = "info/refs?service=git-receive-pack";
/// update の要求先 (repository URL からの相対 path)。
pub const RECEIVE_PACK_PATH: &str = "git-receive-pack";
/// POST body の Content-Type。
pub const REQUEST_CONTENT_TYPE: &str = "application/x-git-receive-pack-request";

/// 呼び出し側が送るべき HTTP request。path は repository URL からの相対。
#[derive(Debug, PartialEq, Eq)]
pub enum Request {
    Get { path: &'static str },
    Post { path: &'static str, body: Vec<u8> },
}

enum State {
    InfoRefs,
    Send { side_band_64k: bool },
    Done,
}

pub struct Push {
    state: State,
    /// push したい (ref 名, 新しい oid)。
    updates: Vec<(Vec<u8>, Oid)>,
    pack: Vec<u8>,
    commands: Vec<Command>,
    report: Option<PushReport>,
}

/// push の結果。
pub struct PushOutcome {
    /// 全 ref が既に一致しており、送信自体を行わなかった。
    pub up_to_date: bool,
    /// 送信した場合の report-status。
    pub report: Option<PushReport>,
}

impl Push {
    pub fn new(updates: Vec<(Vec<u8>, Oid)>, pack: Vec<u8>) -> Self {
        Self {
            state: State::InfoRefs,
            updates,
            pack,
            commands: Vec::new(),
            report: None,
        }
    }

    /// 次に送るべき request。None なら完了。
    pub fn next_request(&self) -> Option<Request> {
        match &self.state {
            State::InfoRefs => Some(Request::Get {
                path: INFO_REFS_PATH,
            }),
            State::Send { side_band_64k } => Some(Request::Post {
                path: RECEIVE_PACK_PATH,
                body: protov0::push_request(&self.commands, *side_band_64k, &self.pack),
            }),
            State::Done => None,
        }
    }

    /// 直前の request に対する response body を渡して状態を進める。
    pub fn on_response(&mut self, body: &[u8]) -> Result<()> {
        match &self.state {
            State::InfoRefs => {
                let adv = protov0::parse_receive_advertisement(body)?;
                if !adv.report_status {
                    return Err(Error::Unsupported("server without report-status"));
                }
                for (name, new) in &self.updates {
                    let old = adv
                        .refs
                        .iter()
                        .find(|(n, _)| n == name)
                        .map_or(ZERO_OID, |(_, oid)| *oid);
                    if old != *new {
                        self.commands.push(Command {
                            old,
                            new: *new,
                            name: name.clone(),
                        });
                    }
                }
                if self.commands.is_empty() {
                    self.state = State::Done;
                } else {
                    self.state = State::Send {
                        side_band_64k: adv.side_band_64k,
                    };
                }
            }
            State::Send { side_band_64k } => {
                let report = protov0::parse_report_status(body, *side_band_64k)?;
                // 途中で切れた response を成功と誤認しないよう、送った全 command に
                // ちょうど 1 つの結果があることを要求する。
                for command in &self.commands {
                    let n = report
                        .results
                        .iter()
                        .filter(|(name, _)| *name == command.name)
                        .count();
                    if n != 1 {
                        return Err(Error::Corrupt("report-status missing ref result"));
                    }
                }
                self.report = Some(report);
                self.state = State::Done;
            }
            State::Done => return Err(Error::Corrupt("response after completion")),
        }
        Ok(())
    }

    pub fn is_done(&self) -> bool {
        matches!(self.state, State::Done)
    }

    pub fn finish(self) -> Result<PushOutcome> {
        if !self.is_done() {
            return Err(Error::Corrupt("push not finished"));
        }
        Ok(PushOutcome {
            up_to_date: self.report.is_none(),
            report: self.report,
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::pkt;

    fn oid(n: u8) -> Oid {
        Oid::from_bytes([n; 20])
    }

    fn advertisement(refs: &[(&str, Oid)]) -> Vec<u8> {
        let mut b = Vec::new();
        pkt::write_line(&mut b, b"# service=git-receive-pack");
        pkt::write_flush(&mut b);
        if refs.is_empty() {
            pkt::write_line(
                &mut b,
                b"0000000000000000000000000000000000000000 capabilities^{}\0report-status side-band-64k",
            );
        } else {
            for (i, (name, oid)) in refs.iter().enumerate() {
                let caps = if i == 0 {
                    "\0report-status side-band-64k"
                } else {
                    ""
                };
                pkt::write_line(&mut b, format!("{oid} {name}{caps}").as_bytes());
            }
        }
        pkt::write_flush(&mut b);
        b
    }

    fn ok_report(name: &str) -> Vec<u8> {
        let mut inner = Vec::new();
        pkt::write_line(&mut inner, b"unpack ok");
        pkt::write_line(&mut inner, format!("ok {name}").as_bytes());
        pkt::write_flush(&mut inner);
        let mut body = Vec::new();
        let mut payload = alloc::vec![1u8];
        payload.extend_from_slice(&inner);
        pkt::write_data(&mut body, &payload);
        pkt::write_flush(&mut body);
        body
    }

    #[test]
    fn create_ref_on_empty_repository() {
        let mut push = Push::new(
            alloc::vec![(b"refs/heads/main".to_vec(), oid(0x11))],
            b"PACK".to_vec(),
        );
        assert!(matches!(push.next_request(), Some(Request::Get { .. })));
        push.on_response(&advertisement(&[])).unwrap();

        let Some(Request::Post { body, .. }) = push.next_request() else {
            panic!("update request expected");
        };
        let text = String::from_utf8_lossy(&body);
        assert!(text.contains("0000000000000000000000000000000000000000 1111"));
        assert!(body.ends_with(b"PACK"));

        push.on_response(&ok_report("refs/heads/main")).unwrap();
        let outcome = push.finish().unwrap();
        assert!(!outcome.up_to_date);
        assert!(outcome.report.unwrap().is_success());
    }

    #[test]
    fn up_to_date_skips_send() {
        let mut push = Push::new(
            alloc::vec![(b"refs/heads/main".to_vec(), oid(0x11))],
            b"PACK".to_vec(),
        );
        push.next_request();
        push.on_response(&advertisement(&[("refs/heads/main", oid(0x11))]))
            .unwrap();
        assert!(push.is_done());
        assert!(push.finish().unwrap().up_to_date);
    }

    // unpack ok の直後で切れた response (ref の結果なし) を成功と誤認しない。
    #[test]
    fn truncated_report_rejected() {
        let mut push = Push::new(
            alloc::vec![(b"refs/heads/main".to_vec(), oid(0x11))],
            b"PACK".to_vec(),
        );
        push.next_request();
        push.on_response(&advertisement(&[])).unwrap();

        let mut inner = Vec::new();
        pkt::write_line(&mut inner, b"unpack ok");
        pkt::write_flush(&mut inner);
        let mut body = Vec::new();
        let mut payload = alloc::vec![1u8];
        payload.extend_from_slice(&inner);
        pkt::write_data(&mut body, &payload);
        pkt::write_flush(&mut body);

        assert_eq!(
            push.on_response(&body).unwrap_err(),
            Error::Corrupt("report-status missing ref result")
        );
    }

    #[test]
    fn update_uses_advertised_old_oid() {
        let mut push = Push::new(
            alloc::vec![(b"refs/heads/main".to_vec(), oid(0x22))],
            b"PACK".to_vec(),
        );
        push.next_request();
        push.on_response(&advertisement(&[("refs/heads/main", oid(0x11))]))
            .unwrap();
        let Some(Request::Post { body, .. }) = push.next_request() else {
            panic!("update request expected");
        };
        let text = String::from_utf8_lossy(&body);
        assert!(text.contains(&format!("{} {}", oid(0x11), oid(0x22))));
    }
}
