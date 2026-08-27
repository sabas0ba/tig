//! tig-core の動作確認用 CLI。
//!
//! bundle ファイルを入力として、refs の一覧、履歴の表示、object の内容表示を行う。
//! 引数解析は依存を増やさないため手書きとする。

use std::io::Write;
use std::process::ExitCode;

use tig_core::bundle::Bundle;
use tig_core::history::Walk;
use tig_core::oid::Oid;

const USAGE: &str = "使用方法:
  tig refs <bundle>                     ref の一覧を表示する
  tig log <bundle> [options]            履歴を committer date 順に表示する
    --ref <name>   開始点の ref (既定: 全 ref)
    -n <count>     表示する commit 数の上限
    --format <f>   oid | oneline (既定) | full
  tig cat-file <bundle> <oid>           object の内容を表示する";

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
        "refs" => refs(rest),
        "log" => log(rest),
        "cat-file" => cat_file(rest),
        _ => Err(USAGE.into()),
    }
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
                .ok_or_else(|| format!("ref が見つかりません: {name}"))?;
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
        .ok_or_else(|| format!("object が見つかりません: {oid_hex}"))?;
    // git cat-file と同様、body をそのまま出力する (tree は生のバイナリになる)。
    let mut out = std::io::stdout().lock();
    out.write_all(&body).map_err(|e| e.to_string())?;
    Ok(())
}
