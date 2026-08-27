//! push と checkout の実物の git との差分テスト。
//!
//! push: git http-backend (receive-pack 有効) へ tig で push し、remote 側を
//! git 自身 (rev-parse / fsck / rev-list) で検証する。pack writer と無圧縮
//! zlib stream を git が受理することの確認を兼ねる。
//! checkout: tig の展開結果を `git checkout` の worktree と比較する。

mod common;

use std::path::{Path, PathBuf};
use std::process::Command;

use common::{T0, git_at, git_lines, run_tig, serve, test_dir};

/// fixture repository と bundle、受け側の空 bare repository を作る。
/// 返り値は (作業 repo, HTTP root, bundle path)。受け側は root/dst.git。
fn fixture(name: &str) -> (PathBuf, PathBuf, PathBuf) {
    let base = test_dir(&format!("push-{name}"));
    let work = base.join("work");
    let root = base.join("root");
    std::fs::create_dir_all(&work).unwrap();
    std::fs::create_dir_all(&root).unwrap();

    git_at(&work, &["init", "-q", "-b", "main"], T0);
    let mut time = T0;
    for i in 0..3 {
        std::fs::write(work.join("a.txt"), format!("rev {i}\n")).unwrap();
        git_at(&work, &["add", "a.txt"], time);
        git_at(&work, &["commit", "-q", "-m", &format!("commit {i}")], time);
        time += 100;
    }
    git_at(&work, &["tag", "-a", "-m", "release", "v1"], time);
    git_at(&work, &["bundle", "create", "out.bundle", "--all"], time);

    let dst = root.join("dst.git");
    git_at(
        &work,
        &["init", "-q", "--bare", dst.to_str().unwrap()],
        time,
    );
    git_at(&dst, &["config", "http.receivepack", "true"], time);

    (work.clone(), root, work.join("out.bundle"))
}

#[test]
fn push_to_empty_repository() {
    let (work, root, bundle) = fixture("empty");
    let url = serve(root.clone(), "dst.git");
    run_tig(&["push", &url, bundle.to_str().unwrap()]);

    let dst = root.join("dst.git");
    // ref が一致し、object が壊れていないこと (無圧縮 zlib と pack writer の検証)。
    for refname in ["refs/heads/main", "refs/tags/v1"] {
        assert_eq!(
            git_lines(&dst, &["rev-parse", refname]),
            git_lines(&work, &["rev-parse", refname]),
            "refname={refname}"
        );
    }
    git_at(&dst, &["fsck", "--strict"], T0);
    assert_eq!(
        git_lines(&dst, &["rev-list", "--date-order", "refs/heads/main"]),
        git_lines(&work, &["rev-list", "--date-order", "refs/heads/main"]),
    );
}

#[test]
fn push_update_and_up_to_date() {
    let (work, root, bundle) = fixture("update");
    let url = serve(root.clone(), "dst.git");
    run_tig(&["push", &url, bundle.to_str().unwrap()]);

    // 新しい commit を積んで再度 push すると、既存 ref の old-oid 経路を通る。
    std::fs::write(work.join("b.txt"), "more\n").unwrap();
    git_at(&work, &["add", "b.txt"], T0 + 1000);
    git_at(&work, &["commit", "-q", "-m", "more"], T0 + 1000);
    git_at(
        &work,
        &["bundle", "create", "out2.bundle", "--all"],
        T0 + 1000,
    );
    let bundle2 = work.join("out2.bundle");
    run_tig(&["push", &url, bundle2.to_str().unwrap()]);

    let dst = root.join("dst.git");
    git_at(&dst, &["fsck", "--strict"], T0);
    assert_eq!(
        git_lines(&dst, &["rev-parse", "refs/heads/main"]),
        git_lines(&work, &["rev-parse", "refs/heads/main"]),
    );

    // 同じ bundle をもう一度 push すると up to date で成功する。
    let out = Command::new(env!("CARGO_BIN_EXE_tig"))
        .args(["push", &url, bundle2.to_str().unwrap()])
        .output()
        .unwrap();
    assert!(out.status.success());
    assert!(String::from_utf8_lossy(&out.stdout).contains("up to date"));
}

#[test]
fn push_single_ref_with_rename() {
    let (work, root, bundle) = fixture("single");
    let url = serve(root.clone(), "dst.git");
    run_tig(&[
        "push",
        &url,
        bundle.to_str().unwrap(),
        "--ref",
        "refs/heads/main",
        "--to",
        "refs/heads/mirror",
    ]);

    let dst = root.join("dst.git");
    assert_eq!(
        git_lines(&dst, &["rev-parse", "refs/heads/mirror"]),
        git_lines(&work, &["rev-parse", "refs/heads/main"]),
    );
    // 他の ref は作られない。
    assert_eq!(
        git_lines(&dst, &["for-each-ref", "--format=%(refname)"]),
        vec!["refs/heads/mirror".to_owned()],
    );
}

/// checkout の結果が git の worktree と一致すること (実行 bit と symlink を含む)。
#[test]
fn checkout_matches_git_worktree() {
    let base = test_dir("checkout");
    let work = base.join("work");
    std::fs::create_dir_all(&work).unwrap();

    git_at(&work, &["init", "-q", "-b", "main"], T0);
    std::fs::create_dir_all(work.join("dir/sub")).unwrap();
    std::fs::write(work.join("a.txt"), "plain\n").unwrap();
    std::fs::write(work.join("dir/run.sh"), "#!/bin/sh\n").unwrap();
    std::fs::write(work.join("dir/sub/deep.txt"), "deep\n").unwrap();
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(
            work.join("dir/run.sh"),
            std::fs::Permissions::from_mode(0o755),
        )
        .unwrap();
        std::os::unix::fs::symlink("a.txt", work.join("link")).unwrap();
    }
    git_at(&work, &["add", "-A"], T0);
    git_at(&work, &["commit", "-q", "-m", "tree"], T0);
    git_at(&work, &["bundle", "create", "out.bundle", "--all"], T0);

    let out_dir = base.join("extracted");
    run_tig(&[
        "checkout",
        work.join("out.bundle").to_str().unwrap(),
        "refs/heads/main",
        "-o",
        out_dir.to_str().unwrap(),
    ]);

    // .git を除いた worktree と抽出結果を diff で比較する (-r は symlink も追う
    // ため、リンク先内容の一致まで確認される)。
    let out = Command::new("diff")
        .args([
            "-r",
            "--exclude=.git",
            "--exclude=out.bundle",
            work.to_str().unwrap(),
            out_dir.to_str().unwrap(),
        ])
        .output()
        .unwrap();
    assert!(
        out.status.success(),
        "diff: {}",
        String::from_utf8_lossy(&out.stdout)
    );

    // 実行 bit の再現を個別に確認する。
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        let mode = std::fs::metadata(out_dir.join("dir/run.sh"))
            .unwrap()
            .permissions()
            .mode();
        assert_ne!(mode & 0o111, 0, "executable bit not restored");
        let link = std::fs::read_link(out_dir.join("link")).unwrap();
        assert_eq!(link, Path::new("a.txt"));
    }
}

/// 非 UTF-8 の filename が U+FFFD 置換されず byte 列のまま復元されること。
#[cfg(unix)]
#[test]
fn checkout_preserves_non_utf8_filename() {
    use std::os::unix::ffi::OsStrExt;

    let base = test_dir("checkout-nonutf8");
    let work = base.join("work");
    std::fs::create_dir_all(&work).unwrap();

    git_at(&work, &["init", "-q", "-b", "main"], T0);
    // Latin-1 の 0xff を含む名前 (invalid UTF-8)。git は byte 列として扱う。
    let name = std::ffi::OsStr::from_bytes(b"caf\xe9.txt");
    std::fs::write(work.join(name), "x\n").unwrap();
    git_at(&work, &["add", "-A"], T0);
    git_at(&work, &["commit", "-q", "-m", "nonutf8"], T0);
    git_at(&work, &["bundle", "create", "out.bundle", "--all"], T0);

    let out_dir = base.join("extracted");
    run_tig(&[
        "checkout",
        work.join("out.bundle").to_str().unwrap(),
        "refs/heads/main",
        "-o",
        out_dir.to_str().unwrap(),
    ]);

    let names: Vec<Vec<u8>> = std::fs::read_dir(&out_dir)
        .unwrap()
        .map(|e| e.unwrap().file_name().as_bytes().to_vec())
        .collect();
    assert_eq!(names, vec![b"caf\xe9.txt".to_vec()]);
}
