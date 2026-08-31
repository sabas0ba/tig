//! smart HTTP clone の実物の git との差分テスト。
//!
//! common のハーネス (git http-backend を CGI として叩く HTTP server) に対して
//! tig の clone を実行し、得られた bundle を git 自身の clone / rev-list と
//! 突き合わせる。protocol v2 の実装 (pkt-line、ls-refs、fetch、sideband) を
//! end-to-end で検証する。

mod common;

use std::path::{Path, PathBuf};

use common::{T0, git_at, git_lines, run_tig, serve, test_dir};
use tig_core::bundle::Bundle;
use tig_core::history::Walk;

/// fixture repository (branch + merge + annotated tag) と bare mirror を作る。
/// 返り値は (作業 repo, HTTP で公開する root, root 内の repo 名)。
fn fixture(name: &str) -> (PathBuf, PathBuf, &'static str) {
    let base = test_dir(&format!("clone-{name}"));
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

fn tig_clone(url: &str, out: &Path, extra: &[&str]) {
    let mut args = vec!["clone", url, "-o", out.to_str().unwrap()];
    args.extend_from_slice(extra);
    run_tig(&args);
}

#[test]
fn full_clone_interoperates_with_git() {
    let (work, root, repo) = fixture("full");
    let url = serve(root.clone(), repo);
    let bundle_path = root.join("full.bundle");
    tig_clone(&url, &bundle_path, &[]);

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
    tig_clone(
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
    tig_clone(&url, &bundle_path, &["--ref", "refs/heads/topic"]);

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
