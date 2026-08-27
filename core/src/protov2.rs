//! git protocol version 2 (gitprotocol-v2) の request 構築と response 解析。
//!
//! sans-io 方針のため、本モジュールはバイト列の変換のみを行い、HTTP の送受信は
//! 呼び出し側の責務とする。smart HTTP (stateless-rpc) での使用を想定し、
//! - capability advertisement: `GET $URL/info/refs?service=git-upload-pack`
//! - ls-refs / fetch: `POST $URL/git-upload-pack`
//!
//! の各 body を扱う。GET / POST とも `Git-Protocol: version=2` header を要する。

use alloc::vec::Vec;

use crate::err::{Error, Result};
use crate::oid::Oid;
use crate::pkt::{self, Pkt, PktReader};

/// capability advertisement の解析結果。使用するものだけを保持する。
#[derive(Debug, Default, Clone, PartialEq, Eq)]
pub struct Advertisement {
    pub ls_refs: bool,
    pub fetch: bool,
    /// fetch が shallow (deepen) を受けるか。
    pub fetch_shallow: bool,
}

/// `info/refs?service=git-upload-pack` の response body を解析する。
pub fn parse_advertisement(body: &[u8]) -> Result<Advertisement> {
    let mut r = PktReader::new(body);

    // smart HTTP では "# service=git-upload-pack" 行と flush が前置される。
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

    match first {
        Pkt::Data(data) if pkt::trim_line(data) == b"version 2" => {}
        _ => return Err(Error::Unsupported("protocol version (v2 required)")),
    }

    let mut adv = Advertisement::default();
    loop {
        match r.expect_next()? {
            Pkt::Flush => return Ok(adv),
            Pkt::Data(data) => {
                let line = pkt::trim_line(data);
                let (name, value) = match line.iter().position(|&b| b == b'=') {
                    Some(eq) => (&line[..eq], &line[eq + 1..]),
                    None => (line, &b""[..]),
                };
                match name {
                    b"ls-refs" => adv.ls_refs = true,
                    b"fetch" => {
                        adv.fetch = true;
                        adv.fetch_shallow = value.split(|&b| b == b' ').any(|f| f == b"shallow");
                    }
                    _ => {}
                }
            }
            _ => return Err(Error::Corrupt("capability advertisement")),
        }
    }
}

/// ls-refs command の request body を構築する。
///
/// `prefixes` が空の場合は全 ref を要求する。symrefs と peel は常に要求する
/// (HEAD の解決と annotated tag の情報のため)。
pub fn ls_refs_request(prefixes: &[&[u8]]) -> Vec<u8> {
    let mut out = Vec::new();
    pkt::write_line(&mut out, b"command=ls-refs");
    pkt::write_delim(&mut out);
    pkt::write_line(&mut out, b"peel");
    pkt::write_line(&mut out, b"symrefs");
    for prefix in prefixes {
        let mut line = Vec::with_capacity(11 + prefix.len());
        line.extend_from_slice(b"ref-prefix ");
        line.extend_from_slice(prefix);
        pkt::write_line(&mut out, &line);
    }
    pkt::write_flush(&mut out);
    out
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RefEntry {
    pub name: Vec<u8>,
    pub oid: Oid,
    /// annotated tag の指す先 (peel 属性)。
    pub peeled: Option<Oid>,
    /// symref (例: HEAD) の指す先の ref 名。
    pub symref_target: Option<Vec<u8>>,
}

/// ls-refs の response body を解析する。
pub fn parse_ls_refs(body: &[u8]) -> Result<Vec<RefEntry>> {
    let mut r = PktReader::new(body);
    let mut refs = Vec::new();
    loop {
        match r.expect_next()? {
            Pkt::Flush | Pkt::ResponseEnd => return Ok(refs),
            Pkt::Data(data) => {
                let line = pkt::trim_line(data);
                let mut fields = line.split(|&b| b == b' ');
                let oid_hex = fields.next().ok_or(Error::Corrupt("ls-refs line"))?;
                let name = fields.next().ok_or(Error::Corrupt("ls-refs line"))?;
                let mut entry = RefEntry {
                    name: name.to_vec(),
                    oid: Oid::from_hex(oid_hex)?,
                    peeled: None,
                    symref_target: None,
                };
                for attr in fields {
                    if let Some(v) = attr.strip_prefix(b"peeled:") {
                        entry.peeled = Some(Oid::from_hex(v)?);
                    } else if let Some(v) = attr.strip_prefix(b"symref-target:") {
                        entry.symref_target = Some(v.to_vec());
                    }
                }
                refs.push(entry);
            }
            _ => return Err(Error::Corrupt("ls-refs response")),
        }
    }
}

/// fetch command の request body を構築する。
///
/// negotiation は行わず常に done を送る (clone 相当)。`depth` を与えると
/// shallow fetch (deepen) になる。delta の base は pack 内参照 (ofs-delta) を
/// 許可する。
pub fn fetch_request(wants: &[Oid], depth: Option<u32>) -> Vec<u8> {
    let mut out = Vec::new();
    pkt::write_line(&mut out, b"command=fetch");
    pkt::write_delim(&mut out);
    pkt::write_line(&mut out, b"no-progress");
    pkt::write_line(&mut out, b"ofs-delta");
    if let Some(depth) = depth {
        let mut line = Vec::new();
        line.extend_from_slice(b"deepen ");
        push_decimal(&mut line, depth);
        pkt::write_line(&mut out, &line);
    }
    for want in wants {
        let mut line = Vec::with_capacity(45);
        line.extend_from_slice(b"want ");
        push_hex(&mut line, want);
        pkt::write_line(&mut out, &line);
    }
    pkt::write_line(&mut out, b"done");
    pkt::write_flush(&mut out);
    out
}

#[derive(Debug, Default, PartialEq, Eq)]
pub struct FetchResponse {
    /// 履歴を打ち切った commit (shallow-info の shallow 行)。
    pub shallow: Vec<Oid>,
    pub pack: Vec<u8>,
}

/// fetch の response body を解析し、sideband を剥がした packfile を返す。
pub fn parse_fetch_response(body: &[u8]) -> Result<FetchResponse> {
    let mut r = PktReader::new(body);
    let mut resp = FetchResponse::default();

    // section header (テキスト行) を読み、対応する section を処理する。
    loop {
        match r.expect_next()? {
            Pkt::Flush | Pkt::ResponseEnd => {
                if resp.pack.is_empty() {
                    return Err(Error::Corrupt("fetch response without packfile"));
                }
                return Ok(resp);
            }
            Pkt::Data(data) => match pkt::trim_line(data) {
                b"shallow-info" => parse_shallow_info(&mut r, &mut resp)?,
                b"acknowledgments" => skip_section(&mut r)?,
                b"packfile" => {
                    parse_packfile_section(&mut r, &mut resp)?;
                    return Ok(resp);
                }
                _ => return Err(Error::Unsupported("fetch response section")),
            },
            _ => return Err(Error::Corrupt("fetch response")),
        }
    }
}

fn parse_shallow_info(r: &mut PktReader<'_>, resp: &mut FetchResponse) -> Result<()> {
    loop {
        match r.expect_next()? {
            Pkt::Delim => return Ok(()),
            Pkt::Flush | Pkt::ResponseEnd => return Err(Error::Corrupt("shallow-info section")),
            Pkt::Data(data) => {
                let line = pkt::trim_line(data);
                if let Some(v) = line.strip_prefix(b"shallow ") {
                    resp.shallow.push(Oid::from_hex(v)?);
                }
                // unshallow は clone では現れない。現れても無害なため無視する。
            }
        }
    }
}

fn skip_section(r: &mut PktReader<'_>) -> Result<()> {
    loop {
        match r.expect_next()? {
            Pkt::Delim => return Ok(()),
            Pkt::Flush | Pkt::ResponseEnd => return Err(Error::Corrupt("truncated section")),
            _ => {}
        }
    }
}

fn parse_packfile_section(r: &mut PktReader<'_>, resp: &mut FetchResponse) -> Result<()> {
    loop {
        match r.expect_next()? {
            Pkt::Flush | Pkt::ResponseEnd => {
                if resp.pack.is_empty() {
                    return Err(Error::Corrupt("empty packfile section"));
                }
                return Ok(());
            }
            Pkt::Data(data) => {
                let (&band, payload) = data.split_first().ok_or(Error::Corrupt("sideband pkt"))?;
                match band {
                    1 => resp.pack.extend_from_slice(payload),
                    2 => {} // progress。no-progress を送っているが、来ても無視する
                    3 => return Err(Error::Corrupt("remote error (sideband 3)")),
                    _ => return Err(Error::Corrupt("sideband channel")),
                }
            }
            _ => return Err(Error::Corrupt("packfile section")),
        }
    }
}

fn push_decimal(out: &mut Vec<u8>, mut value: u32) {
    let mut digits = [0u8; 10];
    let mut n = 0;
    loop {
        digits[n] = b'0' + (value % 10) as u8;
        value /= 10;
        n += 1;
        if value == 0 {
            break;
        }
    }
    for i in (0..n).rev() {
        out.push(digits[i]);
    }
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

    fn adv_body(with_service: bool, caps: &[&[u8]]) -> Vec<u8> {
        let mut b = Vec::new();
        if with_service {
            pkt::write_line(&mut b, b"# service=git-upload-pack");
            pkt::write_flush(&mut b);
        }
        pkt::write_line(&mut b, b"version 2");
        for c in caps {
            pkt::write_line(&mut b, c);
        }
        pkt::write_flush(&mut b);
        b
    }

    #[test]
    fn advertisement_with_service_prefix() {
        let body = adv_body(
            true,
            &[
                b"agent=git/2.54.0",
                b"ls-refs=unborn",
                b"fetch=shallow wait-for-done",
            ],
        );
        let adv = parse_advertisement(&body).unwrap();
        assert!(adv.ls_refs && adv.fetch && adv.fetch_shallow);
    }

    #[test]
    fn advertisement_without_service_prefix() {
        let body = adv_body(false, &[b"ls-refs", b"fetch"]);
        let adv = parse_advertisement(&body).unwrap();
        assert!(adv.ls_refs && adv.fetch && !adv.fetch_shallow);
    }

    #[test]
    fn advertisement_v1_rejected() {
        let mut body = Vec::new();
        pkt::write_line(&mut body, b"version 1");
        pkt::write_flush(&mut body);
        assert!(parse_advertisement(&body).is_err());
    }

    #[test]
    fn ls_refs_request_encoding() {
        let body = ls_refs_request(&[b"refs/heads/"]);
        let expected =
            b"0014command=ls-refs\n00010009peel\n000csymrefs\n001bref-prefix refs/heads/\n0000";
        assert_eq!(body, expected);
    }

    #[test]
    fn ls_refs_response_parse() {
        let hex1 = "1111111111111111111111111111111111111111";
        let hex2 = "2222222222222222222222222222222222222222";
        let mut body = Vec::new();
        pkt::write_line(
            &mut body,
            format!("{hex1} HEAD symref-target:refs/heads/main").as_bytes(),
        );
        pkt::write_line(
            &mut body,
            format!("{hex1} refs/tags/v1 peeled:{hex2}").as_bytes(),
        );
        pkt::write_flush(&mut body);

        let refs = parse_ls_refs(&body).unwrap();
        assert_eq!(refs.len(), 2);
        assert_eq!(refs[0].name, b"HEAD");
        assert_eq!(
            refs[0].symref_target.as_deref(),
            Some(&b"refs/heads/main"[..])
        );
        assert_eq!(refs[1].oid, oid(0x11));
        assert_eq!(refs[1].peeled, Some(oid(0x22)));
    }

    #[test]
    fn fetch_request_contents() {
        let body = fetch_request(&[oid(0xab)], Some(1));
        let text = String::from_utf8_lossy(&body);
        assert!(text.contains("command=fetch"));
        assert!(text.contains("deepen 1"));
        assert!(text.contains(&format!("want {}", oid(0xab))));
        assert!(text.contains("done"));
    }

    #[test]
    fn fetch_response_sideband() {
        let mut body = Vec::new();
        pkt::write_line(&mut body, b"shallow-info");
        pkt::write_line(
            &mut body,
            b"shallow 1111111111111111111111111111111111111111",
        );
        pkt::write_delim(&mut body);
        pkt::write_line(&mut body, b"packfile");
        pkt::write_data(&mut body, &[1, b'P', b'A']);
        pkt::write_data(&mut body, &[2, b'p', b'r', b'o', b'g']);
        pkt::write_data(&mut body, &[1, b'C', b'K']);
        pkt::write_flush(&mut body);

        let resp = parse_fetch_response(&body).unwrap();
        assert_eq!(resp.shallow, alloc::vec![oid(0x11)]);
        assert_eq!(resp.pack, b"PACK");
    }

    #[test]
    fn fetch_response_error_band() {
        let mut body = Vec::new();
        pkt::write_line(&mut body, b"packfile");
        pkt::write_data(&mut body, &[3, b'n', b'g']);
        pkt::write_flush(&mut body);
        assert!(parse_fetch_response(&body).is_err());
    }
}
