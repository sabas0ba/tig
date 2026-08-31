//! 統合テスト共通のハーネス。
//!
//! - 決定的な日時での git 実行
//! - request を CGI として `git http-backend` に渡す最小の HTTP server
//!   (Apache 等の設定と同じ構成)

#![allow(dead_code)]

use std::io::{Read, Write};
use std::net::{TcpListener, TcpStream};
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};

pub const T0: i64 = 1_700_000_000;

pub fn git_at(dir: &Path, args: &[&str], time: i64) -> Vec<u8> {
    let date = format!("@{time} +0000");
    let out = Command::new("git")
        .arg("-C")
        .arg(dir)
        .args(args)
        .env("GIT_CONFIG_GLOBAL", "/dev/null")
        .env("GIT_CONFIG_SYSTEM", "/dev/null")
        .env("GIT_AUTHOR_NAME", "Tester")
        .env("GIT_AUTHOR_EMAIL", "tester@example.com")
        .env("GIT_COMMITTER_NAME", "Tester")
        .env("GIT_COMMITTER_EMAIL", "tester@example.com")
        .env("GIT_AUTHOR_DATE", &date)
        .env("GIT_COMMITTER_DATE", &date)
        .output()
        .expect("git の起動に失敗");
    assert!(
        out.status.success(),
        "git {args:?} が失敗: {}",
        String::from_utf8_lossy(&out.stderr)
    );
    out.stdout
}

pub fn git_lines(dir: &Path, args: &[&str]) -> Vec<String> {
    String::from_utf8(git_at(dir, args, T0))
        .unwrap()
        .lines()
        .map(str::to_owned)
        .collect()
}

/// テストごとに一意な作業ディレクトリを用意する (残骸があれば消す)。
pub fn test_dir(name: &str) -> PathBuf {
    let dir = std::env::temp_dir().join(format!("tig-test-{}-{name}", std::process::id()));
    if dir.exists() {
        std::fs::remove_dir_all(&dir).unwrap();
    }
    std::fs::create_dir_all(&dir).unwrap();
    dir
}

/// 1 接続分の request を読み、`git http-backend` を CGI として実行して応答する。
fn serve_one(mut stream: TcpStream, root: &Path) {
    let mut buf = Vec::new();
    let mut tmp = [0u8; 4096];
    let header_end = loop {
        if let Some(i) = buf.windows(4).position(|w| w == b"\r\n\r\n") {
            break i;
        }
        let n = stream.read(&mut tmp).unwrap();
        assert!(n > 0, "truncated request");
        buf.extend_from_slice(&tmp[..n]);
    };

    let head = String::from_utf8_lossy(&buf[..header_end]).into_owned();
    let mut lines = head.lines();
    let request_line = lines.next().unwrap();
    let mut parts = request_line.split(' ');
    let method = parts.next().unwrap().to_owned();
    let target = parts.next().unwrap();
    let (path, query) = match target.split_once('?') {
        Some((p, q)) => (p.to_owned(), q.to_owned()),
        None => (target.to_owned(), String::new()),
    };

    let mut content_length = 0usize;
    let mut content_type = String::new();
    let mut git_protocol = String::new();
    for line in lines {
        let Some((name, value)) = line.split_once(':') else {
            continue;
        };
        let value = value.trim().to_owned();
        if name.eq_ignore_ascii_case("content-length") {
            content_length = value.parse().unwrap();
        } else if name.eq_ignore_ascii_case("content-type") {
            content_type = value;
        } else if name.eq_ignore_ascii_case("git-protocol") {
            git_protocol = value;
        }
    }

    let mut body = buf[header_end + 4..].to_vec();
    while body.len() < content_length {
        let n = stream.read(&mut tmp).unwrap();
        assert!(n > 0, "truncated request body");
        body.extend_from_slice(&tmp[..n]);
    }

    let mut child = Command::new("git")
        .arg("http-backend")
        .env("GIT_PROJECT_ROOT", root)
        .env("GIT_HTTP_EXPORT_ALL", "1")
        .env("REQUEST_METHOD", &method)
        .env("PATH_INFO", &path)
        .env("QUERY_STRING", &query)
        .env("CONTENT_TYPE", &content_type)
        .env("CONTENT_LENGTH", content_length.to_string())
        .env("GIT_PROTOCOL", &git_protocol)
        .env("REMOTE_ADDR", "127.0.0.1")
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .spawn()
        .unwrap();
    child.stdin.take().unwrap().write_all(&body).unwrap();
    let mut cgi_out = Vec::new();
    child
        .stdout
        .take()
        .unwrap()
        .read_to_end(&mut cgi_out)
        .unwrap();
    assert!(child.wait().unwrap().success());

    // CGI 出力 (header + body) を HTTP/1.1 response に変換する。
    let cgi_header_end = cgi_out
        .windows(4)
        .position(|w| w == b"\r\n\r\n")
        .expect("CGI header");
    let cgi_head = String::from_utf8_lossy(&cgi_out[..cgi_header_end]).into_owned();
    let cgi_body = &cgi_out[cgi_header_end + 4..];
    let status = cgi_head
        .lines()
        .find_map(|l| l.strip_prefix("Status:"))
        .map_or_else(|| "200 OK".to_owned(), |s| s.trim().to_owned());

    let mut response = format!("HTTP/1.1 {status}\r\n");
    for line in cgi_head.lines() {
        if !line.starts_with("Status:") {
            response.push_str(line);
            response.push_str("\r\n");
        }
    }
    response.push_str(&format!("Content-Length: {}\r\n\r\n", cgi_body.len()));
    stream.write_all(response.as_bytes()).unwrap();
    stream.write_all(cgi_body).unwrap();
}

/// HTTP server を起動し、URL を返す。listener thread はテスト終了まで動き続ける。
pub fn serve(root: PathBuf, repo: &str) -> String {
    let listener = TcpListener::bind("127.0.0.1:0").unwrap();
    let port = listener.local_addr().unwrap().port();
    std::thread::spawn(move || {
        for stream in listener.incoming() {
            let Ok(stream) = stream else { break };
            serve_one(stream, &root);
        }
    });
    format!("http://127.0.0.1:{port}/{repo}")
}

/// tig の CLI を実行し、成功を確認する。
pub fn run_tig(args: &[&str]) {
    let status = Command::new(env!("CARGO_BIN_EXE_tig"))
        .args(args)
        .status()
        .unwrap();
    assert!(status.success(), "tig {args:?} が失敗");
}
