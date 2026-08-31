//! object builder (tree / commit) の実物の git との差分テスト。
//!
//! 同じ内容の object を git の plumbing (hash-object / mktree / commit-tree)
//! で作り、oid が一致することを確かめる。tree の正規順 (directory の '/'
//! 補完) の検証を含む。

#![cfg(feature = "write")]

use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};

use tig_core::build;
use tig_core::object::{Kind, Sig, compute_oid};
use tig_core::oid::Oid;

fn repo(name: &str) -> PathBuf {
    let dir = std::env::temp_dir().join(format!("tig-writetest-{}-{name}", std::process::id()));
    if dir.exists() {
        std::fs::remove_dir_all(&dir).unwrap();
    }
    std::fs::create_dir_all(&dir).unwrap();
    git(&dir, &["init", "-q"], b"");
    dir
}

fn git(dir: &Path, args: &[&str], stdin: &[u8]) -> String {
    let mut child = Command::new("git")
        .arg("-C")
        .arg(dir)
        .args(args)
        .env("GIT_CONFIG_GLOBAL", "/dev/null")
        .env("GIT_CONFIG_SYSTEM", "/dev/null")
        .env("GIT_AUTHOR_NAME", "Tester")
        .env("GIT_AUTHOR_EMAIL", "tester@example.com")
        .env("GIT_COMMITTER_NAME", "Tester")
        .env("GIT_COMMITTER_EMAIL", "tester@example.com")
        .env("GIT_AUTHOR_DATE", "@1700000000 +0900")
        .env("GIT_COMMITTER_DATE", "@1700000100 +0900")
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .unwrap();
    use std::io::{Read, Write};
    child.stdin.take().unwrap().write_all(stdin).unwrap();
    let mut stdout = String::new();
    child
        .stdout
        .take()
        .unwrap()
        .read_to_string(&mut stdout)
        .unwrap();
    let status = child.wait().unwrap();
    assert!(status.success(), "git {args:?} が失敗");
    stdout.trim().to_owned()
}

#[test]
fn tree_oid_matches_git_mktree() {
    let dir = repo("tree");
    let blob_hex = git(&dir, &["hash-object", "-w", "--stdin"], b"content\n");
    let blob = Oid::from_hex(blob_hex.as_bytes()).unwrap();

    // sub tree を先に作る。
    let sub_body = build::tree(&[build::TreeEntry {
        mode: b"100644",
        name: b"inner.txt",
        oid: blob,
    }])
    .unwrap();
    let sub = compute_oid(Kind::Tree, &sub_body);
    let sub_hex = git(
        &dir,
        &["mktree"],
        format!("100644 blob {blob_hex}\tinner.txt\n").as_bytes(),
    );
    assert_eq!(sub.to_string(), sub_hex);

    // "foo" (dir) と "foo.txt" の並び順が git と一致することを含めて検証する。
    // 入力は意図的に逆順で与える。
    let root_body = build::tree(&[
        build::TreeEntry {
            mode: b"40000",
            name: b"foo",
            oid: sub,
        },
        build::TreeEntry {
            mode: b"100755",
            name: b"run.sh",
            oid: blob,
        },
        build::TreeEntry {
            mode: b"100644",
            name: b"foo.txt",
            oid: blob,
        },
        build::TreeEntry {
            mode: b"120000",
            name: b"link",
            oid: blob,
        },
    ])
    .unwrap();
    let mktree_input = format!(
        "040000 tree {sub_hex}\tfoo\n100755 blob {blob_hex}\trun.sh\n100644 blob {blob_hex}\tfoo.txt\n120000 blob {blob_hex}\tlink\n"
    );
    let root_hex = git(&dir, &["mktree"], mktree_input.as_bytes());
    assert_eq!(compute_oid(Kind::Tree, &root_body).to_string(), root_hex);
}

#[test]
fn commit_oid_matches_git_commit_tree() {
    let dir = repo("commit");
    let blob_hex = git(&dir, &["hash-object", "-w", "--stdin"], b"x\n");
    let tree_hex = git(
        &dir,
        &["mktree"],
        format!("100644 blob {blob_hex}\ta.txt\n").as_bytes(),
    );
    let tree = Oid::from_hex(tree_hex.as_bytes()).unwrap();

    // 親なし commit。
    let root_hex = git(&dir, &["commit-tree", &tree_hex, "-m", "first"], b"");
    let author = Sig {
        name: b"Tester",
        email: b"tester@example.com",
        time: 1_700_000_000,
        tz: b"+0900",
    };
    let committer = Sig {
        time: 1_700_000_100,
        ..author
    };
    let body = build::commit(tree, &[], &author, &committer, b"first\n").unwrap();
    assert_eq!(compute_oid(Kind::Commit, &body).to_string(), root_hex);

    // 親あり commit。
    let child_hex = git(
        &dir,
        &["commit-tree", &tree_hex, "-p", &root_hex, "-m", "second"],
        b"",
    );
    let parent = Oid::from_hex(root_hex.as_bytes()).unwrap();
    let body = build::commit(tree, &[parent], &author, &committer, b"second\n").unwrap();
    assert_eq!(compute_oid(Kind::Commit, &body).to_string(), child_hex);
}
