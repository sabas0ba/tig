//! smart HTTP clone の実物の git との差分テスト。
//!
//! テスト内に最小の HTTP server を立て、request を CGI として `git http-backend`
//! に渡す (Apache 等の設定と同じ構成)。これに対して tig の clone を実行し、
//! 得られた bundle を git 自身の clone / rev-list と突き合わせる。protocol v2
//! の実装 (pkt-line、ls-refs、fetch、sideband) を end-to-end で検証する。

use std::io::{Read, Write};
use std::net::{TcpListener, TcpStream};
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};

use tig_core::bundle::Bundle;
use tig_core::history::Walk;

const T0: i64 = 1_700_000_000;

fn git_at(dir: &Path, args: &[&str], time: i64) -> Vec<u8> {
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

fn git_lines(dir: &Path, args: &[&str]) -> Vec<String> {
    String::from_utf8(git_at(dir, args, T0))
        .unwrap()
        .lines()
        .map(str::to_owned)
        .collect()
}

/// fixture repository (branch + merge + annotated tag) と bare mirror を作る。
/// 返り値は (作業 repo, HTTP で公開する root, root 内の repo 名)。
fn fixture(name: &str) -> (PathBuf, PathBuf, &'static str) {
    let base = std::env::temp_dir().join(format!("tig-httptest-{}-{name}", std::process::id()));
    if base.exists() {
        std::fs::remove_dir_all(&base).unwrap();
    }
    let work = base.join("work");
    let root = base.join("root");
    std::fs::create_dir_all(&work).unwrap();
    std::fs::create_dir_all(&root).unwrap();

    git_at(&work, &["init", "-q", "-b", "main"], T0);
    let mut time = T0;
    let mut content = String::new();
    for i in 0..5 {
        content.push_str(&format!("line {i}\n"));
        std::fs::write(work.join("a.txt"), &content).unwrap();
        git_at(&work, &["add", "a.txt"], time);
        git_at(&work, &["commit", "-q", "-m", &format!("commit {i}")], time);
        time += 100;
    }
    git_at(&work, &["checkout", "-q", "-b", "topic"], time);
    std::fs::write(work.join("b.txt"), "topic\n").unwrap();
    git_at(&work, &["add", "b.txt"], time);
    git_at(&work, &["commit", "-q", "-m", "topic change"], time);
    time += 100;
    git_at(&work, &["checkout", "-q", "main"], time);
    git_at(
        &work,
        &["merge", "-q", "--no-ff", "-m", "merge topic", "topic"],
        time,
    );
    time += 100;
    git_at(&work, &["tag", "-a", "-m", "release", "v1"], time);

    git_at(
        &work,
        &[
            "clone",
            "-q",
            "--bare",
            work.to_str().unwrap(),
            root.join("repo.git").to_str().unwrap(),
        ],
        time,
    );
    (work, root, "repo.git")
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
fn serve(root: PathBuf, repo: &str) -> String {
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

fn run_tig_clone(url: &str, out: &Path, extra: &[&str]) {
    let status = Command::new(env!("CARGO_BIN_EXE_tig"))
        .arg("clone")
        .arg(url)
        .arg("-o")
        .arg(out)
        .args(extra)
        .status()
        .unwrap();
    assert!(status.success(), "tig clone が失敗");
}

#[test]
fn full_clone_interoperates_with_git() {
    let (work, root, repo) = fixture("full");
    let url = serve(root.clone(), repo);
    let bundle_path = root.join("full.bundle");
    run_tig_clone(&url, &bundle_path, &[]);

    // refs が server の advertise (refs/* + HEAD) と一致すること。
    let data = std::fs::read(&bundle_path).unwrap();
    let bundle = Bundle::parse(&data).unwrap();
    let mut ours: Vec<String> = bundle
        .refs
        .iter()
        .map(|(name, oid)| format!("{oid} {}", String::from_utf8_lossy(name)))
        .collect();
    let bare = root.join(repo);
    let mut expected = git_lines(
        &bare,
        &["for-each-ref", "--format=%(objectname) %(refname)"],
    );
    expected.push(format!(
        "{} HEAD",
        git_lines(&bare, &["rev-parse", "HEAD"])[0]
    ));
    ours.sort();
    expected.sort();
    assert_eq!(ours, expected);

    // git 自身が bundle を検証・clone でき、履歴が元 repository と一致すること。
    git_at(
        &work,
        &["bundle", "verify", "-q", bundle_path.to_str().unwrap()],
        T0,
    );
    let unpacked = root.join("from-bundle");
    git_at(
        &work,
        &[
            "clone",
            "-q",
            bundle_path.to_str().unwrap(),
            unpacked.to_str().unwrap(),
        ],
        T0,
    );
    assert_eq!(
        git_lines(&unpacked, &["rev-list", "--date-order", "HEAD"]),
        git_lines(&work, &["rev-list", "--date-order", "refs/heads/main"]),
    );
}

#[test]
fn shallow_clone_depth1() {
    let (work, root, repo) = fixture("shallow");
    let url = serve(root.clone(), repo);
    let bundle_path = root.join("shallow.bundle");
    run_tig_clone(
        &url,
        &bundle_path,
        &["--depth", "1", "--ref", "refs/heads/main"],
    );

    let data = std::fs::read(&bundle_path).unwrap();
    let bundle = Bundle::parse(&data).unwrap();
    // main の tip は merge commit で、両 parent が prerequisite になる。
    assert_eq!(bundle.prerequisites.len(), 2);

    // walk は深さ 1 で境界に達し、tip だけを返す。
    let tip = bundle.find_ref(b"refs/heads/main").unwrap();
    assert_eq!(
        tip.to_string(),
        git_lines(&work, &["rev-parse", "refs/heads/main"])[0]
    );
    let mut walk = Walk::new(&bundle.pack);
    walk.push(tip).unwrap();
    assert_eq!(walk.count(), 1);
}

#[test]
fn single_ref_clone() {
    let (work, root, repo) = fixture("single");
    let url = serve(root.clone(), repo);
    let bundle_path = root.join("topic.bundle");
    run_tig_clone(&url, &bundle_path, &["--ref", "refs/heads/topic"]);

    let data = std::fs::read(&bundle_path).unwrap();
    let bundle = Bundle::parse(&data).unwrap();
    assert_eq!(bundle.refs.len(), 1);
    assert_eq!(bundle.refs[0].0, b"refs/heads/topic");

    let mut walk = Walk::new(&bundle.pack);
    walk.push(bundle.refs[0].1).unwrap();
    let ours: Vec<String> = walk.map(|c| c.unwrap().oid.to_string()).collect();
    assert_eq!(
        ours,
        git_lines(&work, &["rev-list", "--date-order", "refs/heads/topic"]),
    );
}
