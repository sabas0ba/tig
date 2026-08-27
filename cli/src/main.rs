//! tig-core の動作確認用 CLI。
//!
//! bundle ファイルを入力として、refs の一覧、履歴の表示、object の内容表示を行う。
//! 引数解析は依存を増やさないため手書きとする。

use std::io::Write;
use std::process::ExitCode;

use tig_core::bundle::{self, Bundle};
use tig_core::clone::{Clone as CloneDriver, CloneOptions, REQUEST_CONTENT_TYPE, Request};
use tig_core::history::Walk;
use tig_core::object::Kind;
use tig_core::oid::Oid;
use tig_core::{checkout, pack, push};

mod http;

const USAGE: &str = "usage:
  tig clone <url> [options]             clone over smart HTTP into a bundle
    -o <file>      output bundle path (default: <repo>.bundle)
    --depth <n>    shallow clone depth
    --ref <name>   fetch a single ref (e.g. refs/heads/main)
  tig push <url> <bundle> [options]     push refs from a bundle over smart HTTP
    --ref <name>   push a single ref (default: all refs/*)
    --to <name>    destination ref name (requires --ref)
  tig checkout <bundle> <rev> -o <dir>  extract a tree into a directory
                                        (<rev> is a ref name or an oid)
  tig refs <bundle>                     list refs
  tig log <bundle> [options]            show history in committer date order
    --ref <name>   start point ref (default: all refs)
    -n <count>     limit the number of commits shown
    --format <f>   oid | oneline (default) | full
  tig cat-file <bundle> <oid>           print object content";

fn main() -> ExitCode {
    let args: Vec<String> = std::env::args().skip(1).collect();
    match run(&args) {
        Ok(()) => ExitCode::SUCCESS,
        Err(msg) => {
            eprintln!("tig: {msg}");
            ExitCode::FAILURE
        }
    }
}

fn run(args: &[String]) -> Result<(), String> {
    let (cmd, rest) = args.split_first().ok_or(USAGE)?;
    match cmd.as_str() {
        "clone" => clone(rest),
        "push" => push_cmd(rest),
        "checkout" => checkout_cmd(rest),
        "refs" => refs(rest),
        "log" => log(rest),
        "cat-file" => cat_file(rest),
        _ => Err(USAGE.into()),
    }
}

fn push_cmd(args: &[String]) -> Result<(), String> {
    let [url_str, bundle_path, opts @ ..] = args else {
        return Err(USAGE.into());
    };
    let mut rest = opts;
    let mut src_ref: Option<String> = None;
    let mut dst_ref: Option<String> = None;
    while let Some((flag, next)) = rest.split_first() {
        rest = next;
        let mut value = || -> Result<&String, String> {
            let (v, next) = rest.split_first().ok_or(USAGE)?;
            rest = next;
            Ok(v)
        };
        match flag.as_str() {
            "--ref" => src_ref = Some(value()?.clone()),
            "--to" => dst_ref = Some(value()?.clone()),
            _ => return Err(USAGE.into()),
        }
    }
    if dst_ref.is_some() && src_ref.is_none() {
        return Err("--to requires --ref".into());
    }

    let data = load(bundle_path)?;
    let src = Bundle::parse(&data).map_err(|e| e.to_string())?;

    // push する ref。既定は bundle 内の refs/* (HEAD 等の symbolic 名は除く)。
    let updates: Vec<(Vec<u8>, Oid)> = match &src_ref {
        Some(name) => {
            let oid = src
                .find_ref(name.as_bytes())
                .ok_or_else(|| format!("ref not found: {name}"))?;
            let target = dst_ref.as_deref().unwrap_or(name.as_str());
            vec![(target.as_bytes().to_vec(), oid)]
        }
        None => src
            .refs
            .iter()
            .filter(|(name, _)| name.starts_with(b"refs/"))
            .map(|(name, oid)| (name.to_vec(), *oid))
            .collect(),
    };
    if updates.is_empty() {
        return Err("no refs to push".into());
    }

    // bundle の全 object を非 delta の pack に詰め直す。remote が既に持つ
    // object が混ざっても害はない。
    let objects: Vec<(Kind, Vec<u8>)> = src
        .pack
        .oids()
        .map(|oid| {
            src.pack
                .read_object(oid)
                .map_err(|e| e.to_string())?
                .ok_or_else(|| "object disappeared".to_owned())
        })
        .collect::<Result<_, _>>()?;
    let refs: Vec<(Kind, &[u8])> = objects.iter().map(|(k, b)| (*k, b.as_slice())).collect();
    let pack_data = pack::write_pack(&refs);

    let url = http::Url::parse(url_str)?;
    let mut driver = push::Push::new(updates, pack_data);
    while let Some(request) = driver.next_request() {
        let body = match &request {
            push::Request::Get { path } => http::request(&url, path, None)?,
            push::Request::Post { path, body } => {
                http::request(&url, path, Some((push::REQUEST_CONTENT_TYPE, body)))?
            }
        };
        driver.on_response(&body).map_err(|e| e.to_string())?;
    }
    let outcome = driver.finish().map_err(|e| e.to_string())?;

    if outcome.up_to_date {
        println!("up to date");
        return Ok(());
    }
    let report = outcome.report.expect("report exists unless up to date");
    for (name, error) in &report.results {
        match error {
            None => println!("ok {}", String::from_utf8_lossy(name)),
            Some(msg) => println!(
                "ng {} ({})",
                String::from_utf8_lossy(name),
                String::from_utf8_lossy(msg)
            ),
        }
    }
    if !report.is_success() {
        return Err("push rejected by remote".into());
    }
    Ok(())
}

fn checkout_cmd(args: &[String]) -> Result<(), String> {
    let [bundle_path, rev, flag, dir] = args else {
        return Err(USAGE.into());
    };
    if flag != "-o" {
        return Err(USAGE.into());
    }

    let data = load(bundle_path)?;
    let src = Bundle::parse(&data).map_err(|e| e.to_string())?;

    // rev (ref 名または oid) を tree まで剥がす。
    let mut oid = match src.find_ref(rev.as_bytes()) {
        Some(oid) => oid,
        None => Oid::from_hex(rev.as_bytes()).map_err(|_| format!("unknown rev: {rev}"))?,
    };
    let tree = loop {
        let (kind, body) = src
            .pack
            .read_object(&oid)
            .map_err(|e| e.to_string())?
            .ok_or_else(|| format!("object not found: {oid}"))?;
        match kind {
            Kind::Tree => break oid,
            Kind::Commit => {
                break tig_core::object::parse_commit(&body)
                    .map_err(|e| e.to_string())?
                    .tree;
            }
            Kind::Tag => {
                oid = tig_core::object::parse_tag(&body)
                    .map_err(|e| e.to_string())?
                    .object;
            }
            Kind::Blob => return Err("rev points to a blob".into()),
        }
    };

    let out_dir = std::path::Path::new(dir);
    if out_dir.exists()
        && std::fs::read_dir(out_dir)
            .map_err(|e| e.to_string())?
            .count()
            > 0
    {
        return Err(format!("directory not empty: {dir}"));
    }
    std::fs::create_dir_all(out_dir).map_err(|e| e.to_string())?;

    let mut count = 0usize;
    checkout::walk(&src.pack, tree, &mut |path, kind, content| {
        let path_str = String::from_utf8_lossy(path).into_owned();
        let target = out_dir.join(&path_str);
        if let Some(parent) = target.parent() {
            std::fs::create_dir_all(parent).map_err(io_err)?;
        }
        match kind {
            checkout::EntryKind::File | checkout::EntryKind::Executable => {
                std::fs::write(&target, content).map_err(io_err)?;
                #[cfg(unix)]
                if kind == checkout::EntryKind::Executable {
                    use std::os::unix::fs::PermissionsExt;
                    std::fs::set_permissions(&target, std::fs::Permissions::from_mode(0o755))
                        .map_err(io_err)?;
                }
            }
            checkout::EntryKind::Symlink => {
                #[cfg(unix)]
                std::os::unix::fs::symlink(
                    std::path::Path::new(&String::from_utf8_lossy(content).into_owned()),
                    &target,
                )
                .map_err(io_err)?;
                #[cfg(not(unix))]
                return Err(tig_core::err::Error::Unsupported(
                    "symlink on this platform",
                ));
            }
            checkout::EntryKind::Gitlink => {
                eprintln!("note: skipping submodule {path_str}");
                return Ok(());
            }
        }
        count += 1;
        Ok(())
    })
    .map_err(|e| e.to_string())?;

    println!("{dir}: {count} entries");
    Ok(())
}

/// checkout の callback 内で std::io::Error を core のエラーへ写す。
/// 詳細は失われるため、直前の出力と path から特定する。
fn io_err(_: std::io::Error) -> tig_core::err::Error {
    tig_core::err::Error::Corrupt("filesystem write failed")
}

fn clone(args: &[String]) -> Result<(), String> {
    let (url_str, mut rest) = args.split_first().ok_or(USAGE)?;
    let mut out_path: Option<String> = None;
    let mut opts = CloneOptions::default();
    while let Some((flag, next)) = rest.split_first() {
        rest = next;
        let mut value = || -> Result<&String, String> {
            let (v, next) = rest.split_first().ok_or(USAGE)?;
            rest = next;
            Ok(v)
        };
        match flag.as_str() {
            "-o" => out_path = Some(value()?.clone()),
            "--depth" => opts.depth = Some(value()?.parse().map_err(|_| USAGE.to_string())?),
            "--ref" => opts.want_ref = Some(value()?.clone().into_bytes()),
            _ => return Err(USAGE.into()),
        }
    }

    let url = http::Url::parse(url_str)?;
    let out_path = out_path.unwrap_or_else(|| default_bundle_name(url_str));

    let mut driver = CloneDriver::new(opts);
    while let Some(request) = driver.next_request() {
        let body = match &request {
            Request::Get { path } => http::request(&url, path, None)?,
            Request::Post { path, body } => {
                http::request(&url, path, Some((REQUEST_CONTENT_TYPE, body)))?
            }
        };
        driver.on_response(&body).map_err(|e| e.to_string())?;
    }
    let outcome = driver.finish().map_err(|e| e.to_string())?;

    let refs: Vec<(&[u8], Oid)> = outcome
        .refs
        .iter()
        .map(|e| (e.name.as_slice(), e.oid))
        .collect();
    let data = bundle::write(&refs, &outcome.shallow, &outcome.pack).map_err(|e| e.to_string())?;
    std::fs::write(&out_path, &data).map_err(|e| format!("{out_path}: {e}"))?;

    println!(
        "{}: {} refs, {} bytes ({})",
        out_path,
        refs.len(),
        data.len(),
        if outcome.shallow.is_empty() {
            "full".to_owned()
        } else {
            format!("shallow, {} boundary commits", outcome.shallow.len())
        }
    );
    if !outcome.shallow.is_empty() {
        // prerequisite 付きの bundle は git では前提 commit を持つ repository で
        // しか使えない (bundle 形式に shallow の表現が無いため)。
        println!(
            "note: shallow bundle is readable by tig; git can use it only where the prerequisite commits already exist"
        );
    }
    Ok(())
}

/// URL の末尾要素から出力ファイル名を導出する (例: .../repo.git -> repo.bundle)。
fn default_bundle_name(url: &str) -> String {
    let last = url
        .trim_end_matches('/')
        .rsplit('/')
        .next()
        .unwrap_or("repo");
    let stem = last.strip_suffix(".git").unwrap_or(last);
    let stem = if stem.is_empty() { "repo" } else { stem };
    format!("{stem}.bundle")
}

fn load(path: &str) -> Result<Vec<u8>, String> {
    std::fs::read(path).map_err(|e| format!("{path}: {e}"))
}

fn refs(args: &[String]) -> Result<(), String> {
    let [path] = args else {
        return Err(USAGE.into());
    };
    let data = load(path)?;
    let bundle = Bundle::parse(&data).map_err(|e| e.to_string())?;
    let mut out = std::io::stdout().lock();
    for (name, oid) in &bundle.refs {
        writeln!(out, "{oid} {}", String::from_utf8_lossy(name)).map_err(|e| e.to_string())?;
    }
    Ok(())
}

fn log(args: &[String]) -> Result<(), String> {
    let (path, mut rest) = args.split_first().ok_or(USAGE)?;
    let mut start_ref: Option<&str> = None;
    let mut limit = usize::MAX;
    let mut format = "oneline";
    while let Some((flag, next)) = rest.split_first() {
        rest = next;
        let mut value = || -> Result<&String, String> {
            let (v, next) = rest.split_first().ok_or(USAGE)?;
            rest = next;
            Ok(v)
        };
        match flag.as_str() {
            "--ref" => start_ref = Some(value()?),
            "-n" => limit = value()?.parse().map_err(|_| USAGE.to_string())?,
            "--format" => {
                format = match value()?.as_str() {
                    "oid" => "oid",
                    "oneline" => "oneline",
                    "full" => "full",
                    _ => return Err(USAGE.into()),
                }
            }
            _ => return Err(USAGE.into()),
        }
    }

    let data = load(path)?;
    let bundle = Bundle::parse(&data).map_err(|e| e.to_string())?;

    let mut walk = Walk::new(&bundle.pack);
    match start_ref {
        Some(name) => {
            let oid = bundle
                .find_ref(name.as_bytes())
                .ok_or_else(|| format!("ref not found: {name}"))?;
            walk.push(oid).map_err(|e| e.to_string())?;
        }
        None => {
            for (_, oid) in &bundle.refs {
                walk.push(*oid).map_err(|e| e.to_string())?;
            }
        }
    }

    let mut out = std::io::stdout().lock();
    for item in walk.take(limit) {
        let walked = item.map_err(|e| e.to_string())?;
        let commit = walked.commit().map_err(|e| e.to_string())?;
        match format {
            "oid" => writeln!(out, "{}", walked.oid),
            "oneline" => {
                let subject = commit.message.split(|&b| b == b'\n').next().unwrap_or(b"");
                writeln!(out, "{} {}", walked.oid, String::from_utf8_lossy(subject))
            }
            _ => writeln!(
                out,
                "commit {}\nAuthor: {} <{}>\nDate:   {} {}\n",
                walked.oid,
                String::from_utf8_lossy(commit.author.name),
                String::from_utf8_lossy(commit.author.email),
                commit.author.time,
                String::from_utf8_lossy(commit.author.tz),
            )
            .and_then(|()| {
                for line in commit.message.split(|&b| b == b'\n') {
                    writeln!(out, "    {}", String::from_utf8_lossy(line))?;
                }
                Ok(())
            }),
        }
        .map_err(|e| e.to_string())?;
    }
    Ok(())
}

fn cat_file(args: &[String]) -> Result<(), String> {
    let [path, oid_hex] = args else {
        return Err(USAGE.into());
    };
    let data = load(path)?;
    let bundle = Bundle::parse(&data).map_err(|e| e.to_string())?;
    let oid = Oid::from_hex(oid_hex.as_bytes()).map_err(|e| e.to_string())?;
    let (_, body) = bundle
        .pack
        .read_object(&oid)
        .map_err(|e| e.to_string())?
        .ok_or_else(|| format!("object not found: {oid_hex}"))?;
    // git cat-file と同様、body をそのまま出力する (tree は生のバイナリになる)。
    let mut out = std::io::stdout().lock();
    out.write_all(&body).map_err(|e| e.to_string())?;
    Ok(())
}
