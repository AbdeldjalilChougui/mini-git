//! Git object model: object ids, the loose-object encoding
//! (`"<type> <size>\0<content>"`, SHA-1 hashed, zlib compressed),
//! and parsers for tree and commit bodies.

use crate::{Error, Result};
use flate2::read::ZlibDecoder;
use flate2::write::ZlibEncoder;
use flate2::Compression;
use sha1::{Digest, Sha1};
use std::fmt;
use std::io::{Read, Write};

/// A 20-byte SHA-1 object id.
#[derive(Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub struct Oid(pub [u8; 20]);

impl Oid {
    pub fn from_hex(s: &str) -> Result<Oid> {
        if s.len() != 40 || !s.bytes().all(|b| b.is_ascii_hexdigit()) {
            return Err(Error::new(format!("not a valid object id: {s}")));
        }
        let mut out = [0u8; 20];
        for (i, byte) in out.iter_mut().enumerate() {
            *byte = u8::from_str_radix(&s[i * 2..i * 2 + 2], 16).expect("checked hex");
        }
        Ok(Oid(out))
    }

    pub fn to_hex(&self) -> String {
        self.0.iter().map(|b| format!("{b:02x}")).collect()
    }
}

impl fmt::Display for Oid {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.to_hex())
    }
}

impl fmt::Debug for Oid {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "Oid({})", self.to_hex())
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Kind {
    Blob,
    Tree,
    Commit,
    Tag,
}

impl Kind {
    pub fn as_str(self) -> &'static str {
        match self {
            Kind::Blob => "blob",
            Kind::Tree => "tree",
            Kind::Commit => "commit",
            Kind::Tag => "tag",
        }
    }

    pub fn parse(s: &str) -> Result<Kind> {
        match s {
            "blob" => Ok(Kind::Blob),
            "tree" => Ok(Kind::Tree),
            "commit" => Ok(Kind::Commit),
            "tag" => Ok(Kind::Tag),
            _ => Err(Error::new(format!("unknown object type: {s}"))),
        }
    }
}

/// The uncompressed on-disk form: header followed by content.
pub fn encode(kind: Kind, data: &[u8]) -> Vec<u8> {
    let mut out = format!("{} {}\0", kind.as_str(), data.len()).into_bytes();
    out.extend_from_slice(data);
    out
}

/// The id git would assign to this object.
pub fn hash(kind: Kind, data: &[u8]) -> Oid {
    let mut h = Sha1::new();
    h.update(encode(kind, data));
    Oid(h.finalize().into())
}

pub fn compress(bytes: &[u8]) -> Vec<u8> {
    let mut enc = ZlibEncoder::new(Vec::new(), Compression::default());
    enc.write_all(bytes).expect("writing to a Vec cannot fail");
    enc.finish().expect("writing to a Vec cannot fail")
}

pub fn decompress(bytes: &[u8]) -> Result<Vec<u8>> {
    let mut out = Vec::new();
    ZlibDecoder::new(bytes)
        .read_to_end(&mut out)
        .map_err(|e| Error::new(format!("corrupt zlib stream: {e}")))?;
    Ok(out)
}

/// Split a decompressed loose object into its type and content, validating the size.
pub fn decode(raw: &[u8]) -> Result<(Kind, Vec<u8>)> {
    let nul = raw
        .iter()
        .position(|&b| b == 0)
        .ok_or_else(|| Error::new("object header has no NUL"))?;
    let header = std::str::from_utf8(&raw[..nul]).map_err(|_| Error::new("bad object header"))?;
    let (kind, size) = header
        .split_once(' ')
        .ok_or_else(|| Error::new("bad object header"))?;
    let kind = Kind::parse(kind)?;
    let size: usize = size.parse().map_err(|_| Error::new("bad object size"))?;
    let body = &raw[nul + 1..];
    if body.len() != size {
        return Err(Error::new(format!(
            "object size mismatch: header says {size}, got {}",
            body.len()
        )));
    }
    Ok((kind, body.to_vec()))
}

/// File modes git stores in trees.
pub mod mode {
    pub const FILE: &str = "100644";
    pub const EXEC: &str = "100755";
    pub const SYMLINK: &str = "120000";
    pub const TREE: &str = "40000";
    pub const GITLINK: &str = "160000";
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TreeEntry {
    pub mode: String,
    pub name: Vec<u8>,
    pub oid: Oid,
}

impl TreeEntry {
    pub fn is_tree(&self) -> bool {
        self.mode == mode::TREE
    }

    pub fn kind(&self) -> Kind {
        match self.mode.as_str() {
            mode::TREE => Kind::Tree,
            mode::GITLINK => Kind::Commit,
            _ => Kind::Blob,
        }
    }

    /// Git sorts tree entries by name, comparing directories as if their
    /// name ended with '/'.
    fn sort_key(&self) -> Vec<u8> {
        let mut k = self.name.clone();
        if self.is_tree() {
            k.push(b'/');
        }
        k
    }
}

pub fn sort_entries(entries: &mut [TreeEntry]) {
    entries.sort_by_key(|a| a.sort_key());
}

/// Serialize entries (sorted in git order) as a tree body:
/// `<mode> <name>\0<20 raw bytes>` repeated.
pub fn serialize_tree(entries: &[TreeEntry]) -> Vec<u8> {
    let mut sorted = entries.to_vec();
    sort_entries(&mut sorted);
    let mut out = Vec::new();
    for e in &sorted {
        out.extend_from_slice(e.mode.as_bytes());
        out.push(b' ');
        out.extend_from_slice(&e.name);
        out.push(0);
        out.extend_from_slice(&e.oid.0);
    }
    out
}

pub fn parse_tree(data: &[u8]) -> Result<Vec<TreeEntry>> {
    let mut entries = Vec::new();
    let mut i = 0;
    while i < data.len() {
        let sp = data[i..]
            .iter()
            .position(|&b| b == b' ')
            .ok_or_else(|| Error::new("truncated tree entry (mode)"))?;
        let mode = std::str::from_utf8(&data[i..i + sp])
            .map_err(|_| Error::new("bad tree mode"))?
            .to_string();
        i += sp + 1;
        let nul = data[i..]
            .iter()
            .position(|&b| b == 0)
            .ok_or_else(|| Error::new("truncated tree entry (name)"))?;
        let name = data[i..i + nul].to_vec();
        i += nul + 1;
        if i + 20 > data.len() {
            return Err(Error::new("truncated tree entry (oid)"));
        }
        let mut oid = [0u8; 20];
        oid.copy_from_slice(&data[i..i + 20]);
        i += 20;
        entries.push(TreeEntry {
            mode,
            name,
            oid: Oid(oid),
        });
    }
    Ok(entries)
}

/// Human-readable tree listing, like `git cat-file -p <tree>`.
pub fn pretty_tree(entries: &[TreeEntry]) -> String {
    let mut s = String::new();
    for e in entries {
        s.push_str(&format!(
            "{:0>6} {} {}\t{}\n",
            e.mode,
            e.kind().as_str(),
            e.oid,
            String::from_utf8_lossy(&e.name)
        ));
    }
    s
}

/// `Name <email> <unix-seconds> <+hhmm>`
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Signature {
    pub name: String,
    pub email: String,
    pub time: i64,
    pub tz: String,
}

impl Signature {
    pub fn format(&self) -> String {
        format!("{} <{}> {} {}", self.name, self.email, self.time, self.tz)
    }

    pub fn parse(s: &str) -> Result<Signature> {
        let lt = s.find('<').ok_or_else(|| Error::new("signature missing '<'"))?;
        let gt = s.find('>').ok_or_else(|| Error::new("signature missing '>'"))?;
        let name = s[..lt].trim_end().to_string();
        let email = s[lt + 1..gt].to_string();
        let mut rest = s[gt + 1..].split_whitespace();
        let time = rest
            .next()
            .and_then(|t| t.parse().ok())
            .ok_or_else(|| Error::new("signature missing time"))?;
        let tz = rest.next().unwrap_or("+0000").to_string();
        Ok(Signature {
            name,
            email,
            time,
            tz,
        })
    }

    /// Offset in seconds east of UTC, from `+hhmm` / `-hhmm`.
    pub fn tz_offset_seconds(&self) -> i64 {
        let b = self.tz.as_bytes();
        if b.len() != 5 {
            return 0;
        }
        let sign = if b[0] == b'-' { -1 } else { 1 };
        let hh: i64 = self.tz[1..3].parse().unwrap_or(0);
        let mm: i64 = self.tz[3..5].parse().unwrap_or(0);
        sign * (hh * 3600 + mm * 60)
    }

    /// Date in git's default log format, e.g. `Thu Sep 24 10:00:00 2026 +0000`.
    pub fn git_date(&self) -> String {
        let local = self.time + self.tz_offset_seconds();
        let days = local.div_euclid(86_400);
        let secs = local.rem_euclid(86_400);
        let (y, m, d) = civil_from_days(days);
        const WEEKDAYS: [&str; 7] = ["Thu", "Fri", "Sat", "Sun", "Mon", "Tue", "Wed"];
        const MONTHS: [&str; 12] = [
            "Jan", "Feb", "Mar", "Apr", "May", "Jun", "Jul", "Aug", "Sep", "Oct", "Nov", "Dec",
        ];
        format!(
            "{} {} {} {:02}:{:02}:{:02} {} {}",
            WEEKDAYS[days.rem_euclid(7) as usize],
            MONTHS[(m - 1) as usize],
            d,
            secs / 3600,
            (secs / 60) % 60,
            secs % 60,
            y,
            self.tz
        )
    }
}

/// Days since 1970-01-01 to (year, month, day). Howard Hinnant's algorithm.
fn civil_from_days(z: i64) -> (i64, i64, i64) {
    let z = z + 719_468;
    let era = z.div_euclid(146_097);
    let doe = z.rem_euclid(146_097);
    let yoe = (doe - doe / 1460 + doe / 36_524 - doe / 146_096) / 365;
    let y = yoe + era * 400;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let d = doy - (153 * mp + 2) / 5 + 1;
    let m = if mp < 10 { mp + 3 } else { mp - 9 };
    (if m <= 2 { y + 1 } else { y }, m, d)
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Commit {
    pub tree: Oid,
    pub parents: Vec<Oid>,
    pub author: Signature,
    pub committer: Signature,
    pub message: String,
}

impl Commit {
    pub fn serialize(&self) -> Vec<u8> {
        let mut s = format!("tree {}\n", self.tree);
        for p in &self.parents {
            s.push_str(&format!("parent {p}\n"));
        }
        s.push_str(&format!("author {}\n", self.author.format()));
        s.push_str(&format!("committer {}\n", self.committer.format()));
        s.push('\n');
        s.push_str(&self.message);
        if !self.message.ends_with('\n') {
            s.push('\n');
        }
        s.into_bytes()
    }

    pub fn parse(data: &[u8]) -> Result<Commit> {
        let text = String::from_utf8_lossy(data);
        let (headers, message) = text
            .split_once("\n\n")
            .unwrap_or((text.as_ref(), ""));
        let mut tree = None;
        let mut parents = Vec::new();
        let mut author = None;
        let mut committer = None;
        for line in headers.lines() {
            if let Some(v) = line.strip_prefix("tree ") {
                tree = Some(Oid::from_hex(v)?);
            } else if let Some(v) = line.strip_prefix("parent ") {
                parents.push(Oid::from_hex(v)?);
            } else if let Some(v) = line.strip_prefix("author ") {
                author = Some(Signature::parse(v)?);
            } else if let Some(v) = line.strip_prefix("committer ") {
                committer = Some(Signature::parse(v)?);
            }
            // Other headers (gpgsig, encoding, continuation lines) are ignored.
        }
        let author = author.ok_or_else(|| Error::new("commit has no author"))?;
        Ok(Commit {
            tree: tree.ok_or_else(|| Error::new("commit has no tree"))?,
            parents,
            committer: committer.unwrap_or_else(|| author.clone()),
            author,
            message: message.to_string(),
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn known_blob_hashes() {
        assert_eq!(
            hash(Kind::Blob, b"").to_hex(),
            "e69de29bb2d1d6434b8b29ae775ad8c2e48c5391"
        );
        assert_eq!(
            hash(Kind::Blob, b"hello\n").to_hex(),
            "ce013625030ba8dba906f756967f9e9ca394464a"
        );
    }

    #[test]
    fn empty_tree_hash() {
        assert_eq!(
            hash(Kind::Tree, &serialize_tree(&[])).to_hex(),
            "4b825dc642cb6eb9a060e54bf8d69288fbee4904"
        );
    }

    #[test]
    fn encode_decode_roundtrip() {
        let raw = encode(Kind::Blob, b"abc");
        assert_eq!(raw, b"blob 3\0abc");
        let z = compress(&raw);
        let (k, body) = decode(&decompress(&z).unwrap()).unwrap();
        assert_eq!(k, Kind::Blob);
        assert_eq!(body, b"abc");
    }

    #[test]
    fn decode_rejects_bad_size() {
        assert!(decode(b"blob 5\0abc").is_err());
        assert!(decode(b"blob 3abc").is_err());
        assert!(decode(b"frob 3\0abc").is_err());
    }

    #[test]
    fn oid_hex_roundtrip() {
        let h = "ce013625030ba8dba906f756967f9e9ca394464a";
        assert_eq!(Oid::from_hex(h).unwrap().to_hex(), h);
        assert!(Oid::from_hex("xyz").is_err());
        assert!(Oid::from_hex(&"g".repeat(40)).is_err());
    }

    fn entry(mode: &str, name: &str) -> TreeEntry {
        TreeEntry {
            mode: mode.into(),
            name: name.as_bytes().to_vec(),
            oid: Oid([0; 20]),
        }
    }

    #[test]
    fn tree_sort_treats_dirs_as_slash_suffixed() {
        // "foo.txt" < "foo/" because '.' (0x2e) < '/' (0x2f), but "foo0" > "foo/".
        let mut v = vec![
            entry(mode::FILE, "foo0"),
            entry(mode::TREE, "foo"),
            entry(mode::FILE, "foo.txt"),
        ];
        sort_entries(&mut v);
        let names: Vec<_> = v.iter().map(|e| String::from_utf8_lossy(&e.name).into_owned()).collect();
        assert_eq!(names, ["foo.txt", "foo", "foo0"]);
    }

    #[test]
    fn tree_roundtrip() {
        let v = vec![entry(mode::FILE, "a"), entry(mode::TREE, "b"), entry(mode::EXEC, "c")];
        let parsed = parse_tree(&serialize_tree(&v)).unwrap();
        assert_eq!(parsed, v);
        assert!(pretty_tree(&parsed).contains("040000 tree"));
    }

    #[test]
    fn commit_roundtrip() {
        let sig = Signature {
            name: "A U Thor".into(),
            email: "a@example.com".into(),
            time: 1_700_000_000,
            tz: "+0100".into(),
        };
        let c = Commit {
            tree: hash(Kind::Tree, b""),
            parents: vec![hash(Kind::Blob, b"x")],
            author: sig.clone(),
            committer: sig,
            message: "hello\n\nbody".into(),
        };
        let parsed = Commit::parse(&c.serialize()).unwrap();
        assert_eq!(parsed.tree, c.tree);
        assert_eq!(parsed.parents, c.parents);
        assert_eq!(parsed.author, c.author);
        assert_eq!(parsed.message, "hello\n\nbody\n");
    }

    #[test]
    fn git_date_format() {
        let sig = Signature {
            name: "x".into(),
            email: "y".into(),
            time: 1_700_000_000,
            tz: "+0000".into(),
        };
        assert_eq!(sig.git_date(), "Tue Nov 14 22:13:20 2023 +0000");
        let sig = Signature { tz: "-0130".into(), ..sig };
        assert_eq!(sig.git_date(), "Tue Nov 14 20:43:20 2023 -0130");
    }
}
