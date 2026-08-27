//! 最小の HTTP/1.1 client (std のみ、http:// 専用)。
//!
//! TLS は依存ゼロでは実装できないため https は対象外とする。https が前提の
//! 環境 (ブラウザの fetch 等) では frontend 側の HTTP 実装を使う。接続は
//! request ごとに張り、Connection: close で読み切る。

use std::io::{Read, Write};
use std::net::TcpStream;

pub struct Url {
    host: String,
    port: u16,
    /// 先頭 '/' を含む repository の base path (末尾 '/' なし)。
    path: String,
}

impl Url {
    pub fn parse(s: &str) -> Result<Self, String> {
        let rest = s
            .strip_prefix("http://")
            .ok_or_else(|| format!("unsupported URL (http:// only): {s}"))?;
        let (authority, path) = match rest.find('/') {
            Some(i) => (&rest[..i], rest[i..].trim_end_matches('/')),
            None => (rest, ""),
        };

        // IPv6 リテラルは "[::1]:8080" 形式。接続には括弧を外した表記を使い、
        // Host header 用の表記は host_header() が復元する。
        let (host, port_str) = if let Some(inner) = authority.strip_prefix('[') {
            let (host, after) = inner
                .split_once(']')
                .ok_or_else(|| format!("invalid URL: {s}"))?;
            let port_str = match after {
                "" => None,
                _ => Some(
                    after
                        .strip_prefix(':')
                        .ok_or_else(|| format!("invalid URL: {s}"))?,
                ),
            };
            (host.to_owned(), port_str)
        } else {
            match authority.rsplit_once(':') {
                Some((h, p)) => (h.to_owned(), Some(p)),
                None => (authority.to_owned(), None),
            }
        };
        let port = match port_str {
            Some(p) => p.parse::<u16>().map_err(|_| format!("invalid port: {p}"))?,
            None => 80,
        };
        if host.is_empty() {
            return Err(format!("invalid URL: {s}"));
        }
        Ok(Self {
            host,
            port,
            path: path.to_owned(),
        })
    }

    /// Host header 用の表記。IPv6 リテラルは括弧付きに戻す。
    fn host_header(&self) -> String {
        if self.host.contains(':') {
            format!("[{}]:{}", self.host, self.port)
        } else {
            format!("{}:{}", self.host, self.port)
        }
    }
}

/// repository URL からの相対 path へ request を送り、200 の body を返す。
pub fn request(url: &Url, rel: &str, body: Option<(&str, &[u8])>) -> Result<Vec<u8>, String> {
    let mut stream = TcpStream::connect((url.host.as_str(), url.port))
        .map_err(|e| format!("{}:{}: {e}", url.host, url.port))?;

    let method = if body.is_some() { "POST" } else { "GET" };
    let mut head = format!(
        "{method} {}/{rel} HTTP/1.1\r\nHost: {}\r\nConnection: close\r\nGit-Protocol: version=2\r\nAccept: */*\r\n",
        url.path,
        url.host_header()
    );
    if let Some((content_type, data)) = body {
        head.push_str(&format!(
            "Content-Type: {content_type}\r\nContent-Length: {}\r\n",
            data.len()
        ));
    }
    head.push_str("\r\n");

    stream
        .write_all(head.as_bytes())
        .map_err(|e| e.to_string())?;
    if let Some((_, data)) = body {
        stream.write_all(data).map_err(|e| e.to_string())?;
    }

    let mut raw = Vec::new();
    stream.read_to_end(&mut raw).map_err(|e| e.to_string())?;
    parse_response(&raw)
}

fn parse_response(raw: &[u8]) -> Result<Vec<u8>, String> {
    let header_end = raw
        .windows(4)
        .position(|w| w == b"\r\n\r\n")
        .ok_or("truncated HTTP response")?;
    let head = String::from_utf8_lossy(&raw[..header_end]);
    let body = &raw[header_end + 4..];

    let status_line = head.lines().next().unwrap_or("");
    let status = status_line.split(' ').nth(1).unwrap_or("");
    if status != "200" {
        return Err(format!("HTTP {status}"));
    }

    let mut content_length: Option<usize> = None;
    let mut chunked = false;
    for line in head.lines().skip(1) {
        let Some((name, value)) = line.split_once(':') else {
            continue;
        };
        let value = value.trim();
        if name.eq_ignore_ascii_case("content-length") {
            content_length = Some(value.parse().map_err(|_| "invalid Content-Length")?);
        } else if name.eq_ignore_ascii_case("transfer-encoding")
            && value.eq_ignore_ascii_case("chunked")
        {
            chunked = true;
        }
    }

    if chunked {
        return decode_chunked(body);
    }
    if let Some(len) = content_length {
        return body
            .get(..len)
            .map(<[u8]>::to_vec)
            .ok_or_else(|| "truncated HTTP body".to_owned());
    }
    // Connection: close のため EOF までが body。
    Ok(body.to_vec())
}

fn decode_chunked(mut data: &[u8]) -> Result<Vec<u8>, String> {
    let mut out = Vec::new();
    loop {
        let line_end = data
            .windows(2)
            .position(|w| w == b"\r\n")
            .ok_or("truncated chunk size")?;
        let size_str = std::str::from_utf8(&data[..line_end]).map_err(|_| "invalid chunk size")?;
        let size_str = size_str.split(';').next().unwrap_or("");
        let size = usize::from_str_radix(size_str.trim(), 16).map_err(|_| "invalid chunk size")?;
        data = &data[line_end + 2..];
        if size == 0 {
            return Ok(out);
        }
        let chunk = data.get(..size).ok_or("truncated chunk")?;
        out.extend_from_slice(chunk);
        data = data.get(size + 2..).ok_or("truncated chunk terminator")?;
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn url_parse() {
        let u = Url::parse("http://example.com:8080/path/repo.git/").unwrap();
        assert_eq!(u.host, "example.com");
        assert_eq!(u.port, 8080);
        assert_eq!(u.path, "/path/repo.git");
        assert_eq!(u.host_header(), "example.com:8080");

        assert!(Url::parse("https://example.com/x").is_err());
        assert!(Url::parse("http://:80/").is_err());
    }

    #[test]
    fn url_parse_ipv6() {
        let u = Url::parse("http://[::1]:8000/repo.git").unwrap();
        assert_eq!(u.host, "::1");
        assert_eq!(u.port, 8000);
        assert_eq!(u.host_header(), "[::1]:8000");

        let u = Url::parse("http://[2001:db8::1]/repo.git").unwrap();
        assert_eq!(u.host, "2001:db8::1");
        assert_eq!(u.port, 80);
        assert_eq!(u.host_header(), "[2001:db8::1]:80");

        assert!(Url::parse("http://[::1/x").is_err());
        assert!(Url::parse("http://[::1]8000/x").is_err());
    }

    #[test]
    fn response_content_length() {
        let raw = b"HTTP/1.1 200 OK\r\nContent-Length: 5\r\n\r\nhellorest";
        assert_eq!(parse_response(raw).unwrap(), b"hello");
    }

    #[test]
    fn response_chunked() {
        let raw = b"HTTP/1.1 200 OK\r\nTransfer-Encoding: chunked\r\n\r\n5\r\nhello\r\n1\r\n!\r\n0\r\n\r\n";
        assert_eq!(parse_response(raw).unwrap(), b"hello!");
    }

    #[test]
    fn response_non_200() {
        let raw = b"HTTP/1.1 404 Not Found\r\n\r\n";
        assert_eq!(parse_response(raw).unwrap_err(), "HTTP 404");
    }
}
