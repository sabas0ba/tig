//! receive-pack (push) の protocol version 0。
//!
//! push は protocol v2 に定義が無く、smart HTTP でも v0 の receive-pack を使う。
//! - advertisement: `GET $URL/info/refs?service=git-receive-pack`
//! - update: `POST $URL/git-receive-pack` (command 列 + flush + packfile)
//!
//! sans-io 方針は fetch 側と同じで、本 module はバイト列の変換のみを行う。

use alloc::vec::Vec;

use crate::err::{Error, Result};
use crate::oid::Oid;
use crate::pkt::{self, Pkt, PktReader};

/// ref の新規作成・削除で使う全零の oid。
pub const ZERO_OID: Oid = Oid::from_bytes([0; 20]);

/// receive-pack の advertisement。使用する capability だけを保持する。
#[derive(Debug, Default, Clone)]
pub struct ReceiveAdvertisement {
    /// remote に存在する ref (空 repository では空)。
    pub refs: Vec<(Vec<u8>, Oid)>,
    pub report_status: bool,
    pub side_band_64k: bool,
    pub delete_refs: bool,
}

/// `info/refs?service=git-receive-pack` の response body を解析する。
pub fn parse_receive_advertisement(body: &[u8]) -> Result<ReceiveAdvertisement> {
    let mut r = PktReader::new(body);

    // smart HTTP の service 前置行。
    let mut first = r.expect_next()?;
    if let Pkt::Data(data) = first
        && pkt::trim_line(data).starts_with(b"# service=")
    {
        loop {
            match r.expect_next()? {
                Pkt::Flush => break,
                Pkt::Data(_) => {}
                _ => return Err(Error::Corrupt("service announcement")),
            }
        }
        first = r.expect_next()?;
    }

    let mut adv = ReceiveAdvertisement::default();

    // 先頭行: "<oid> <name>\0<cap> <cap> ..."。空 repository では name が
    // "capabilities^{}" になる。
    let Pkt::Data(data) = first else {
        return Err(Error::Corrupt("receive-pack advertisement"));
    };
    let line = pkt::trim_line(data);
    let nul = line
        .iter()
        .position(|&b| b == 0)
        .ok_or(Error::Corrupt("advertisement capabilities"))?;
    for cap in line[nul + 1..].split(|&b| b == b' ') {
        match cap {
            b"report-status" => adv.report_status = true,
            b"side-band-64k" => adv.side_band_64k = true,
            b"delete-refs" => adv.delete_refs = true,
            _ => {}
        }
    }
    push_ref_line(&mut adv.refs, &line[..nul])?;

    loop {
        match r.expect_next()? {
            Pkt::Flush => return Ok(adv),
            Pkt::Data(data) => push_ref_line(&mut adv.refs, pkt::trim_line(data))?,
            _ => return Err(Error::Corrupt("receive-pack advertisement")),
        }
    }
}

fn push_ref_line(refs: &mut Vec<(Vec<u8>, Oid)>, line: &[u8]) -> Result<()> {
    let sp = line
        .iter()
        .position(|&b| b == b' ')
        .ok_or(Error::Corrupt("advertisement ref line"))?;
    let oid = Oid::from_hex(&line[..sp])?;
    let name = &line[sp + 1..];
    // 空 repository のダミー行は ref として扱わない。
    if name != b"capabilities^{}" {
        refs.push((name.to_vec(), oid));
    }
    Ok(())
}

/// ref の更新 command。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Command {
    /// remote の現在値 (新規作成は [`ZERO_OID`])。
    pub old: Oid,
    pub new: Oid,
    pub name: Vec<u8>,
}

/// update request の body を構築する。command 列 + flush の後に packfile が
/// そのまま (pkt 化せずに) 続く。
pub fn push_request(commands: &[Command], side_band_64k: bool, pack: &[u8]) -> Vec<u8> {
    let mut out = Vec::with_capacity(pack.len() + commands.len() * 96 + 64);
    for (i, command) in commands.iter().enumerate() {
        let mut line = Vec::with_capacity(96);
        push_hex(&mut line, &command.old);
        line.push(b' ');
        push_hex(&mut line, &command.new);
        line.push(b' ');
        line.extend_from_slice(&command.name);
        if i == 0 {
            line.push(0);
            line.extend_from_slice(b"report-status");
            if side_band_64k {
                line.extend_from_slice(b" side-band-64k");
            }
            line.extend_from_slice(b" agent=tig/0.1");
        }
        pkt::write_line(&mut out, &line);
    }
    pkt::write_flush(&mut out);
    out.extend_from_slice(pack);
    out
}

/// report-status の解析結果。
#[derive(Debug, Default, PartialEq, Eq)]
pub struct PushReport {
    pub unpack_ok: bool,
    /// ref 名と、失敗した場合の理由。
    pub results: Vec<(Vec<u8>, Option<Vec<u8>>)>,
}

impl PushReport {
    /// unpack と全 ref の更新が成功したか。
    pub fn is_success(&self) -> bool {
        self.unpack_ok && self.results.iter().all(|(_, err)| err.is_none())
    }
}

/// update request への response (report-status) を解析する。
/// `side_band_64k` を要求した場合、report は band 1 に包まれて届く。
pub fn parse_report_status(body: &[u8], side_band_64k: bool) -> Result<PushReport> {
    let unwrapped;
    let report_bytes = if side_band_64k {
        let mut inner = Vec::new();
        let mut r = PktReader::new(body);
        loop {
            match r.next() {
                None => break,
                Some(Ok(Pkt::Flush)) | Some(Ok(Pkt::ResponseEnd)) => break,
                Some(Ok(Pkt::Data(data))) => {
                    let (&band, payload) =
                        data.split_first().ok_or(Error::Corrupt("sideband pkt"))?;
                    match band {
                        1 => inner.extend_from_slice(payload),
                        2 => {}
                        3 => return Err(Error::Corrupt("remote error (sideband 3)")),
                        _ => return Err(Error::Corrupt("sideband channel")),
                    }
                }
                Some(Ok(Pkt::Delim)) => return Err(Error::Corrupt("report-status")),
                Some(Err(e)) => return Err(e),
            }
        }
        unwrapped = inner;
        unwrapped.as_slice()
    } else {
        body
    };

    let mut report = PushReport::default();
    let mut r = PktReader::new(report_bytes);
    loop {
        match r.next() {
            None | Some(Ok(Pkt::Flush)) => break,
            Some(Ok(Pkt::Data(data))) => {
                let line = pkt::trim_line(data);
                if let Some(v) = line.strip_prefix(b"unpack ") {
                    report.unpack_ok = v == b"ok";
                } else if let Some(v) = line.strip_prefix(b"ok ") {
                    report.results.push((v.to_vec(), None));
                } else if let Some(v) = line.strip_prefix(b"ng ") {
                    let (name, msg) = match v.iter().position(|&b| b == b' ') {
                        Some(sp) => (&v[..sp], v[sp + 1..].to_vec()),
                        None => (v, Vec::new()),
                    };
                    report.results.push((name.to_vec(), Some(msg)));
                }
            }
            Some(Ok(_)) => return Err(Error::Corrupt("report-status")),
            Some(Err(e)) => return Err(e),
        }
    }
    Ok(report)
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

    fn oid(n: u8) -> Oid {
        Oid::from_bytes([n; 20])
    }

    #[test]
    fn advertisement_with_refs() {
        let mut b = Vec::new();
        pkt::write_line(&mut b, b"# service=git-receive-pack");
        pkt::write_flush(&mut b);
        let hex = "1111111111111111111111111111111111111111";
        pkt::write_line(
            &mut b,
            format!("{hex} refs/heads/main\0report-status side-band-64k delete-refs").as_bytes(),
        );
        pkt::write_line(&mut b, format!("{hex} refs/tags/v1").as_bytes());
        pkt::write_flush(&mut b);

        let adv = parse_receive_advertisement(&b).unwrap();
        assert!(adv.report_status && adv.side_band_64k && adv.delete_refs);
        assert_eq!(adv.refs.len(), 2);
        assert_eq!(adv.refs[0].0, b"refs/heads/main");
    }

    #[test]
    fn advertisement_empty_repository() {
        let mut b = Vec::new();
        pkt::write_line(&mut b, b"# service=git-receive-pack");
        pkt::write_flush(&mut b);
        pkt::write_line(
            &mut b,
            b"0000000000000000000000000000000000000000 capabilities^{}\0report-status",
        );
        pkt::write_flush(&mut b);

        let adv = parse_receive_advertisement(&b).unwrap();
        assert!(adv.report_status);
        assert!(adv.refs.is_empty());
    }

    #[test]
    fn push_request_layout() {
        let commands = [Command {
            old: ZERO_OID,
            new: oid(0x11),
            name: b"refs/heads/main".to_vec(),
        }];
        let body = push_request(&commands, true, b"PACKDATA");
        let text = String::from_utf8_lossy(&body);
        assert!(text.contains("report-status side-band-64k"));
        assert!(body.ends_with(b"PACKDATA"));
        // command 部は pkt として読める。
        let mut r = PktReader::new(&body);
        assert!(matches!(r.expect_next().unwrap(), Pkt::Data(_)));
        assert_eq!(r.expect_next().unwrap(), Pkt::Flush);
        assert_eq!(r.rest(), b"PACKDATA");
    }

    #[test]
    fn report_status_sideband() {
        let mut inner = Vec::new();
        pkt::write_line(&mut inner, b"unpack ok");
        pkt::write_line(&mut inner, b"ok refs/heads/main");
        pkt::write_line(&mut inner, b"ng refs/heads/x non-fast-forward");
        pkt::write_flush(&mut inner);

        let mut body = Vec::new();
        let mut payload = alloc::vec![1u8];
        payload.extend_from_slice(&inner);
        pkt::write_data(&mut body, &payload);
        pkt::write_flush(&mut body);

        let report = parse_report_status(&body, true).unwrap();
        assert!(report.unpack_ok);
        assert!(!report.is_success());
        assert_eq!(report.results.len(), 2);
        assert_eq!(
            report.results[1].1.as_deref(),
            Some(&b"non-fast-forward"[..])
        );
    }

    #[test]
    fn report_status_plain() {
        let mut body = Vec::new();
        pkt::write_line(&mut body, b"unpack ok");
        pkt::write_line(&mut body, b"ok refs/heads/main");
        pkt::write_flush(&mut body);

        let report = parse_report_status(&body, false).unwrap();
        assert!(report.is_success());
    }
}
