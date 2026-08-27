//! git object (commit / tree / blob / tag) の解析。
//!
//! object の内容は「`<type> <size>\0` header + body」を SHA-1 したものが oid になる。
//! 本モジュールの parse 関数は header を除いた body を対象とする。

use alloc::vec::Vec;

use crate::err::{Error, Result};
use crate::oid::Oid;
use crate::sha1;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Kind {
    Commit,
    Tree,
    Blob,
    Tag,
}

impl Kind {
    pub const fn as_str(&self) -> &'static str {
        match self {
            Kind::Commit => "commit",
            Kind::Tree => "tree",
            Kind::Blob => "blob",
            Kind::Tag => "tag",
        }
    }

    pub fn from_name(name: &[u8]) -> Result<Self> {
        match name {
            b"commit" => Ok(Kind::Commit),
            b"tree" => Ok(Kind::Tree),
            b"blob" => Ok(Kind::Blob),
            b"tag" => Ok(Kind::Tag),
            _ => Err(Error::Corrupt("object type name")),
        }
    }
}

/// body から oid を計算する。
pub fn compute_oid(kind: Kind, body: &[u8]) -> Oid {
    let mut header = [0u8; 32];
    let mut n = 0;
    for &b in kind.as_str().as_bytes() {
        header[n] = b;
        n += 1;
    }
    header[n] = b' ';
    n += 1;
    n += write_decimal(&mut header[n..], body.len());
    header[n] = 0;
    n += 1;
    sha1::digest_oid(&[&header[..n], body])
}

/// 10 進表記を buf へ書き、書いた桁数を返す。
fn write_decimal(buf: &mut [u8], mut value: usize) -> usize {
    let mut digits = [0u8; 20];
    let mut n = 0;
    loop {
        digits[n] = b'0' + (value % 10) as u8;
        value /= 10;
        n += 1;
        if value == 0 {
            break;
        }
    }
    for i in 0..n {
        buf[i] = digits[n - 1 - i];
    }
    n
}

/// author / committer / tagger の署名行。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Sig<'a> {
    pub name: &'a [u8],
    pub email: &'a [u8],
    /// UNIX time (秒)。
    pub time: i64,
    /// タイムゾーン表記 (例: b"+0900")。
    pub tz: &'a [u8],
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Commit<'a> {
    pub tree: Oid,
    pub parents: Vec<Oid>,
    pub author: Sig<'a>,
    pub committer: Sig<'a>,
    /// header と空行を除いたコミットメッセージ。
    pub message: &'a [u8],
}

/// commit body を解析する。gpgsig 等の未知 header (継続行を含む) は読み飛ばす。
pub fn parse_commit(body: &[u8]) -> Result<Commit<'_>> {
    let mut tree = None;
    let mut parents = Vec::new();
    let mut author = None;
    let mut committer = None;

    let mut rest = body;
    loop {
        let (line, next) = split_line(rest)?;
        rest = next;
        if line.is_empty() {
            break;
        }
        if let Some(v) = strip_prefix(line, b"tree ") {
            tree = Some(Oid::from_hex(v)?);
        } else if let Some(v) = strip_prefix(line, b"parent ") {
            parents.push(Oid::from_hex(v)?);
        } else if let Some(v) = strip_prefix(line, b"author ") {
            author = Some(parse_sig(v)?);
        } else if let Some(v) = strip_prefix(line, b"committer ") {
            committer = Some(parse_sig(v)?);
        }
        // 未知 header は無視する。継続行 (先頭が空白) も同じ経路で読み飛ばされる。
    }

    Ok(Commit {
        tree: tree.ok_or(Error::Corrupt("commit without tree"))?,
        parents,
        author: author.ok_or(Error::Corrupt("commit without author"))?,
        committer: committer.ok_or(Error::Corrupt("commit without committer"))?,
        message: rest,
    })
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Tag<'a> {
    pub object: Oid,
    pub kind: Kind,
    pub name: &'a [u8],
    pub tagger: Option<Sig<'a>>,
    pub message: &'a [u8],
}

/// annotated tag の body を解析する。
pub fn parse_tag(body: &[u8]) -> Result<Tag<'_>> {
    let mut object = None;
    let mut kind = None;
    let mut name = None;
    let mut tagger = None;

    let mut rest = body;
    loop {
        let (line, next) = split_line(rest)?;
        rest = next;
        if line.is_empty() {
            break;
        }
        if let Some(v) = strip_prefix(line, b"object ") {
            object = Some(Oid::from_hex(v)?);
        } else if let Some(v) = strip_prefix(line, b"type ") {
            kind = Some(Kind::from_name(v)?);
        } else if let Some(v) = strip_prefix(line, b"tag ") {
            name = Some(v);
        } else if let Some(v) = strip_prefix(line, b"tagger ") {
            tagger = Some(parse_sig(v)?);
        }
    }

    Ok(Tag {
        object: object.ok_or(Error::Corrupt("tag without object"))?,
        kind: kind.ok_or(Error::Corrupt("tag without type"))?,
        name: name.ok_or(Error::Corrupt("tag without name"))?,
        tagger,
        message: rest,
    })
}

/// tree body の entry を先頭から列挙する iterator。
///
/// entry は「`<mode> <name>\0` + oid (20 byte 生値)」の繰り返しで、名前順に並ぶ。
pub struct TreeIter<'a> {
    rest: &'a [u8],
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct TreeEntry<'a> {
    /// 8 進表記のままの mode (例: b"100644"、b"40000")。
    pub mode: &'a [u8],
    pub name: &'a [u8],
    pub oid: Oid,
}

impl<'a> TreeIter<'a> {
    pub fn new(body: &'a [u8]) -> Self {
        Self { rest: body }
    }
}

impl<'a> Iterator for TreeIter<'a> {
    type Item = Result<TreeEntry<'a>>;

    fn next(&mut self) -> Option<Self::Item> {
        if self.rest.is_empty() {
            return None;
        }
        Some(self.parse_next())
    }
}

impl<'a> TreeIter<'a> {
    fn parse_next(&mut self) -> Result<TreeEntry<'a>> {
        let sp = find(self.rest, b' ').ok_or(Error::Corrupt("tree entry mode"))?;
        let mode = &self.rest[..sp];
        let after_mode = &self.rest[sp + 1..];
        let nul = find(after_mode, 0).ok_or(Error::Corrupt("tree entry name"))?;
        let name = &after_mode[..nul];
        let oid_bytes = after_mode
            .get(nul + 1..nul + 21)
            .ok_or(Error::UnexpectedEof)?;
        self.rest = &after_mode[nul + 21..];
        Ok(TreeEntry {
            mode,
            name,
            oid: Oid::from_bytes(oid_bytes.try_into().unwrap()),
        })
    }
}

// --- 内部ヘルパー -----------------------------------------------------------

fn find(data: &[u8], byte: u8) -> Option<usize> {
    data.iter().position(|&b| b == byte)
}

fn strip_prefix<'a>(line: &'a [u8], prefix: &[u8]) -> Option<&'a [u8]> {
    line.strip_prefix(prefix)
}

/// 次の LF までを返す。header 部の途中で入力が尽きた場合はエラー。
fn split_line(data: &[u8]) -> Result<(&[u8], &[u8])> {
    let nl = find(data, b'\n').ok_or(Error::Corrupt("unterminated header line"))?;
    Ok((&data[..nl], &data[nl + 1..]))
}

/// `name <email> time tz` を解析する。
fn parse_sig(v: &[u8]) -> Result<Sig<'_>> {
    let lt = find(v, b'<').ok_or(Error::Corrupt("signature email start"))?;
    let gt = lt + find(&v[lt..], b'>').ok_or(Error::Corrupt("signature email end"))?;
    let name = v[..lt].strip_suffix(b" ").unwrap_or(&v[..lt]);
    let email = &v[lt + 1..gt];

    // "> " の後は "time tz"。tz が欠ける不正歴史も存在するため tz は任意とする。
    let rest = v.get(gt + 2..).unwrap_or(b"");
    let (time_str, tz) = match find(rest, b' ') {
        Some(sp) => (&rest[..sp], &rest[sp + 1..]),
        None => (rest, &b""[..]),
    };
    let time = parse_i64(time_str).ok_or(Error::Corrupt("signature timestamp"))?;
    Ok(Sig {
        name,
        email,
        time,
        tz,
    })
}

fn parse_i64(s: &[u8]) -> Option<i64> {
    let (neg, digits) = match s.split_first() {
        Some((b'-', rest)) => (true, rest),
        _ => (false, s),
    };
    if digits.is_empty() {
        return None;
    }
    let mut value: i64 = 0;
    for &c in digits {
        if !c.is_ascii_digit() {
            return None;
        }
        value = value.checked_mul(10)?.checked_add(i64::from(c - b'0'))?;
    }
    Some(if neg { -value } else { value })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn oid_of_empty_blob() {
        // `git hash-object -t blob /dev/null` の既知値。
        assert_eq!(
            format!("{}", compute_oid(Kind::Blob, b"")),
            "e69de29bb2d1d6434b8b29ae775ad8c2e48c5391"
        );
    }

    #[test]
    fn oid_of_hello_blob() {
        // `printf 'hello\n' | git hash-object --stdin` の既知値。
        assert_eq!(
            format!("{}", compute_oid(Kind::Blob, b"hello\n")),
            "ce013625030ba8dba906f756967f9e9ca394464a"
        );
    }

    #[test]
    fn parse_minimal_commit() {
        let body = b"tree a9993e364706816aba3e25717850c26c9cd0d89d\n\
              parent e69de29bb2d1d6434b8b29ae775ad8c2e48c5391\n\
              author Alice <alice@example.com> 1700000000 +0900\n\
              committer Bob <bob@example.com> 1700000100 -0500\n\
              \n\
              subject\n\nbody\n";
        let c = parse_commit(body).unwrap();
        assert_eq!(c.parents.len(), 1);
        assert_eq!(c.author.name, b"Alice");
        assert_eq!(c.author.email, b"alice@example.com");
        assert_eq!(c.author.time, 1_700_000_000);
        assert_eq!(c.author.tz, b"+0900");
        assert_eq!(c.committer.time, 1_700_000_100);
        assert_eq!(c.message, b"subject\n\nbody\n");
    }

    #[test]
    fn parse_commit_skips_gpgsig() {
        let body = b"tree a9993e364706816aba3e25717850c26c9cd0d89d\n\
              author A <a@e> 1 +0000\n\
              committer A <a@e> 1 +0000\n\
              gpgsig -----BEGIN PGP SIGNATURE-----\n\
               lines\n\
               -----END PGP SIGNATURE-----\n\
              \n\
              msg";
        let c = parse_commit(body).unwrap();
        assert_eq!(c.message, b"msg");
        assert!(c.parents.is_empty());
    }

    #[test]
    fn tree_iteration() {
        let oid = Oid::from_hex(b"e69de29bb2d1d6434b8b29ae775ad8c2e48c5391").unwrap();
        let mut body = Vec::new();
        body.extend_from_slice(b"100644 a.txt\0");
        body.extend_from_slice(oid.as_bytes());
        body.extend_from_slice(b"40000 dir\0");
        body.extend_from_slice(oid.as_bytes());

        let entries: Vec<_> = TreeIter::new(&body).collect::<Result<_>>().unwrap();
        assert_eq!(entries.len(), 2);
        assert_eq!(entries[0].mode, b"100644");
        assert_eq!(entries[0].name, b"a.txt");
        assert_eq!(entries[1].mode, b"40000");
        assert_eq!(entries[1].oid, oid);
    }
}
