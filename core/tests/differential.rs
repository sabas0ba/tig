//! 実物の git との差分テスト。
//!
//! git でフィクスチャの repository と bundle を生成し、refs / 履歴の順序 /
//! object の内容を tig-core の解析結果と突き合わせる。git が生成した実データを
//! テストベクタとして使うことで、自前実装 (inflate / SHA-1 / pack) の正しさを
//! 外部依存なしに検証する。

#![cfg(all(feature = "bundle", feature = "history"))]

use std::path::{Path, PathBuf};
use std::process::Command;

use tig_core::bundle::Bundle;
use tig_core::history::Walk;
use tig_core::object::{Kind, TreeIter, parse_tag};
use tig_core::oid::Oid;

/// フィクスチャ repository の起点時刻。commit ごとに +100 秒して順序を一意にする。
const T0: i64 = 1_700_000_000;

fn git(dir: &Path, args: &[&str]) -> Vec<u8> {
    git_at(dir, args, T0)
}

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
    String::from_utf8(git(dir, args))
        .unwrap()
        .lines()
        .map(str::to_owned)
        .collect()
}

/// merge を含む repository と bundle を生成し、(repo dir, bundle bytes) を返す。
fn fixture(name: &str) -> (PathBuf, Vec<u8>) {
    let dir = std::env::temp_dir().join(format!("tig-difftest-{}-{name}", std::process::id()));
    if dir.exists() {
        std::fs::remove_dir_all(&dir).unwrap();
    }
    std::fs::create_dir_all(&dir).unwrap();
    git(&dir, &["init", "-q", "-b", "main"]);

    // 内容の近い blob を並べ、pack-objects に delta を作らせる。
    let mut time = T0;
    let mut content = String::new();
    for i in 0..8 {
        content.push_str(&format!(
            "line {i}: 0123456789 abcdefghijklmnopqrstuvwxyz\n"
        ));
        std::fs::write(dir.join("a.txt"), &content).unwrap();
        git_at(&dir, &["add", "a.txt"], time);
        git_at(&dir, &["commit", "-q", "-m", &format!("commit {i}")], time);
        time += 100;
    }

    // 分岐と merge。
    git_at(&dir, &["checkout", "-q", "-b", "topic"], time);
    std::fs::write(dir.join("b.txt"), "topic file\n").unwrap();
    git_at(&dir, &["add", "b.txt"], time);
    git_at(&dir, &["commit", "-q", "-m", "topic change"], time);
    time += 100;
    git_at(&dir, &["checkout", "-q", "main"], time);
    std::fs::write(dir.join("c.txt"), "main file\n").unwrap();
    git_at(&dir, &["add", "c.txt"], time);
    git_at(&dir, &["commit", "-q", "-m", "main change"], time);
    time += 100;
    git_at(
        &dir,
        &["merge", "-q", "--no-ff", "-m", "merge topic", "topic"],
        time,
    );
    time += 100;

    // annotated tag (tag object の parse 対象)。
    git_at(&dir, &["tag", "-a", "-m", "release", "v1"], time);

    git(&dir, &["bundle", "create", "out.bundle", "--all"]);
    let bundle = std::fs::read(dir.join("out.bundle")).unwrap();
    (dir, bundle)
}

#[test]
fn refs_match_git_bundle_list_heads() {
    let (dir, data) = fixture("refs");
    let bundle = Bundle::parse(&data).unwrap();

    let mut ours: Vec<String> = bundle
        .refs
        .iter()
        .map(|(name, oid)| format!("{oid} {}", String::from_utf8_lossy(name)))
        .collect();
    let mut expected = git_lines(&dir, &["bundle", "list-heads", "out.bundle"]);
    ours.sort();
    expected.sort();
    assert_eq!(ours, expected);
}

#[test]
fn walk_matches_git_rev_list_date_order() {
    let (dir, data) = fixture("walk");
    let bundle = Bundle::parse(&data).unwrap();

    // 全 ref を開始点にした walk と `git rev-list --date-order --all` の比較。
    let mut walk = Walk::new(&bundle.pack);
    for (_, oid) in &bundle.refs {
        walk.push(*oid).unwrap();
    }
    let ours: Vec<String> = walk.map(|c| c.unwrap().oid.to_string()).collect();
    let expected = git_lines(&dir, &["rev-list", "--date-order", "--all"]);
    assert_eq!(ours, expected);

    // 単一 ref からの walk。
    for refname in ["refs/heads/main", "refs/heads/topic", "refs/tags/v1"] {
        let mut walk = Walk::new(&bundle.pack);
        walk.push(bundle.find_ref(refname.as_bytes()).unwrap())
            .unwrap();
        let ours: Vec<String> = walk.map(|c| c.unwrap().oid.to_string()).collect();
        let expected = git_lines(&dir, &["rev-list", "--date-order", refname]);
        assert_eq!(ours, expected, "refname={refname}");
    }
}

#[test]
fn objects_match_git_cat_file() {
    let (dir, data) = fixture("objects");
    let bundle = Bundle::parse(&data).unwrap();

    // 到達可能な全 object (commit / tree / blob) が pack に含まれ、内容が一致すること。
    let listed = git_lines(&dir, &["rev-list", "--objects", "--all"]);
    assert!(listed.len() > 10);
    for line in listed {
        let hex = line.split(' ').next().unwrap();
        let oid = Oid::from_hex(hex.as_bytes()).unwrap();
        let (kind, body) = bundle
            .pack
            .read_object(&oid)
            .unwrap()
            .unwrap_or_else(|| panic!("pack に無い object: {hex}"));
        let expected = git(&dir, &["cat-file", kind.as_str(), hex]);
        assert_eq!(body, expected, "oid={hex}");
    }
}

#[test]
fn annotated_tag_peels_to_commit() {
    let (dir, data) = fixture("tag");
    let bundle = Bundle::parse(&data).unwrap();

    let tag_oid = bundle.find_ref(b"refs/tags/v1").unwrap();
    let (kind, body) = bundle.pack.read_object(&tag_oid).unwrap().unwrap();
    assert_eq!(kind, Kind::Tag);
    let tag = parse_tag(&body).unwrap();
    assert_eq!(tag.name, b"v1");

    let expected = git_lines(&dir, &["rev-parse", "refs/tags/v1^{commit}"]);
    assert_eq!(tag.object.to_string(), expected[0]);
}

#[test]
fn tree_lookup_reads_blob_content() {
    let (dir, data) = fixture("tree");
    let bundle = Bundle::parse(&data).unwrap();

    // HEAD の tree から a.txt を辿り、working tree の内容と一致することを確かめる。
    let head = bundle.find_ref(b"HEAD").unwrap();
    let mut walk = Walk::new(&bundle.pack);
    walk.push(head).unwrap();
    let newest = walk.next().unwrap().unwrap();
    let commit = newest.commit().unwrap();

    let (kind, tree_body) = bundle.pack.read_object(&commit.tree).unwrap().unwrap();
    assert_eq!(kind, Kind::Tree);
    let entry = TreeIter::new(&tree_body)
        .map(|e| e.unwrap())
        .find(|e| e.name == b"a.txt")
        .expect("tree に a.txt が無い");

    let (kind, blob) = bundle.pack.read_object(&entry.oid).unwrap().unwrap();
    assert_eq!(kind, Kind::Blob);
    assert_eq!(blob, std::fs::read(dir.join("a.txt")).unwrap());
}

#[test]
fn single_commit_bundle() {
    let dir = std::env::temp_dir().join(format!("tig-difftest-{}-single", std::process::id()));
    if dir.exists() {
        std::fs::remove_dir_all(&dir).unwrap();
    }
    std::fs::create_dir_all(&dir).unwrap();
    git(&dir, &["init", "-q", "-b", "main"]);
    std::fs::write(dir.join("x.txt"), "x\n").unwrap();
    git(&dir, &["add", "x.txt"]);
    git(&dir, &["commit", "-q", "-m", "only"]);
    git(&dir, &["bundle", "create", "out.bundle", "--all"]);

    let data = std::fs::read(dir.join("out.bundle")).unwrap();
    let bundle = Bundle::parse(&data).unwrap();
    assert_eq!(bundle.pack.len(), 3); // commit + tree + blob
    assert!(bundle.prerequisites.is_empty());

    let mut walk = Walk::new(&bundle.pack);
    walk.push(bundle.find_ref(b"refs/heads/main").unwrap())
        .unwrap();
    assert_eq!(walk.count(), 1);
}
