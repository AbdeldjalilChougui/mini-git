//! A repository on disk: the `.git` directory layout, loose object storage,
//! refs, and the porcelain operations built on top of them.

use crate::object::{self, mode, Commit, Kind, Oid, Signature, TreeEntry};
use crate::{Error, Result};
use std::collections::BTreeMap;
use std::fs;
use std::io::Write;
use std::os::unix::fs::PermissionsExt;
use std::ffi::OsString;
use std::os::unix::ffi::OsStringExt;
use std::path::{Path, PathBuf};

pub struct Repo {
    pub worktree: PathBuf,
    pub git_dir: PathBuf,
}

/// Where HEAD points.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Head {
    /// `ref: refs/heads/<branch>`; the branch may not exist yet (unborn).
    Branch(String),
    Detached(Oid),
}

impl Repo {
    /// Create an empty repository in `path` (like `git init`).
    pub fn init(path: &Path) -> Result<Repo> {
        let git_dir = path.join(".git");
        for d in ["objects/info", "objects/pack", "refs/heads", "refs/tags"] {
            fs::create_dir_all(git_dir.join(d))?;
        }
        if !git_dir.join("HEAD").exists() {
            fs::write(git_dir.join("HEAD"), "ref: refs/heads/main\n")?;
        }
        if !git_dir.join("config").exists() {
            fs::write(
                git_dir.join("config"),
                "[core]\n\trepositoryformatversion = 0\n\tfilemode = true\n\tbare = false\n",
            )?;
        }
        Ok(Repo {
            worktree: path.to_path_buf(),
            git_dir,
        })
    }

    /// Find the repository containing `start` by walking up to a `.git` directory.
    pub fn discover(start: &Path) -> Result<Repo> {
        let start = start.canonicalize()?;
        let mut dir = Some(start.as_path());
        while let Some(d) = dir {
            if d.join(".git").join("HEAD").is_file() {
                return Ok(Repo {
                    worktree: d.to_path_buf(),
                    git_dir: d.join(".git"),
                });
            }
            dir = d.parent();
        }
        Err(Error::new("not a git repository (or any of the parent directories)"))
    }

    fn object_path(&self, oid: &Oid) -> PathBuf {
        let hex = oid.to_hex();
        self.git_dir.join("objects").join(&hex[..2]).join(&hex[2..])
    }

    pub fn has_object(&self, oid: &Oid) -> bool {
        self.object_path(oid).is_file()
    }

    /// Store an object as a loose object and return its id. Idempotent.
    pub fn write_object(&self, kind: Kind, data: &[u8]) -> Result<Oid> {
        let oid = object::hash(kind, data);
        let path = self.object_path(&oid);
        if path.exists() {
            return Ok(oid);
        }
        let dir = path.parent().expect("object path has a parent");
        fs::create_dir_all(dir)?;
        // Write to a temp file then rename, so readers never see a partial object.
        let tmp = dir.join(format!("tmp_obj_{}", std::process::id()));
        {
            let mut f = fs::File::create(&tmp)?;
            f.write_all(&object::compress(&object::encode(kind, data)))?;
        }
        fs::set_permissions(&tmp, fs::Permissions::from_mode(0o444))?;
        fs::rename(&tmp, &path)?;
        Ok(oid)
    }

    pub fn read_object(&self, oid: &Oid) -> Result<(Kind, Vec<u8>)> {
        let path = self.object_path(oid);
        let bytes = fs::read(&path).map_err(|_| {
            Error::new(format!(
                "object {oid} not found (packed objects are not supported)"
            ))
        })?;
        let (kind, data) = object::decode(&object::decompress(&bytes)?)?;
        if object::hash(kind, &data) != *oid {
            return Err(Error::new(format!("object {oid} is corrupt (hash mismatch)")));
        }
        Ok((kind, data))
    }

    fn read_typed(&self, oid: &Oid, want: Kind) -> Result<Vec<u8>> {
        let (kind, data) = self.read_object(oid)?;
        if kind != want {
            return Err(Error::new(format!(
                "object {oid} is a {}, expected a {}",
                kind.as_str(),
                want.as_str()
            )));
        }
        Ok(data)
    }

    pub fn read_commit(&self, oid: &Oid) -> Result<Commit> {
        Commit::parse(&self.read_typed(oid, Kind::Commit)?)
    }

    pub fn read_tree(&self, oid: &Oid) -> Result<Vec<TreeEntry>> {
        object::parse_tree(&self.read_typed(oid, Kind::Tree)?)
    }

    // ---------------------------------------------------------------- refs

    pub fn head(&self) -> Result<Head> {
        let s = fs::read_to_string(self.git_dir.join("HEAD"))?;
        let s = s.trim();
        match s.strip_prefix("ref: ") {
            Some(r) => Ok(Head::Branch(r.to_string())),
            None => Ok(Head::Detached(Oid::from_hex(s)?)),
        }
    }

    pub fn set_head(&self, head: &Head) -> Result<()> {
        let content = match head {
            Head::Branch(r) => format!("ref: {r}\n"),
            Head::Detached(oid) => format!("{oid}\n"),
        };
        fs::write(self.git_dir.join("HEAD"), content)?;
        Ok(())
    }

    /// Read a full ref name like `refs/heads/main` (loose first, then packed-refs).
    pub fn read_ref(&self, name: &str) -> Result<Option<Oid>> {
        let path = self.git_dir.join(name);
        if path.is_file() {
            let s = fs::read_to_string(path)?;
            let s = s.trim();
            if let Some(target) = s.strip_prefix("ref: ") {
                return self.read_ref(target);
            }
            return Oid::from_hex(s).map(Some);
        }
        if let Ok(packed) = fs::read_to_string(self.git_dir.join("packed-refs")) {
            for line in packed.lines() {
                if let Some((hex, r)) = line.split_once(' ') {
                    if r == name && !hex.starts_with('#') {
                        return Oid::from_hex(hex).map(Some);
                    }
                }
            }
        }
        Ok(None)
    }

    pub fn update_ref(&self, name: &str, oid: &Oid) -> Result<()> {
        let path = self.git_dir.join(name);
        if let Some(parent) = path.parent() {
            fs::create_dir_all(parent)?;
        }
        fs::write(path, format!("{oid}\n"))?;
        Ok(())
    }

    pub fn head_commit(&self) -> Result<Option<Oid>> {
        match self.head()? {
            Head::Branch(r) => self.read_ref(&r),
            Head::Detached(oid) => Ok(Some(oid)),
        }
    }

    pub fn branches(&self) -> Result<Vec<String>> {
        let mut out = Vec::new();
        collect_refs(&self.git_dir.join("refs/heads"), "", &mut out)?;
        out.sort();
        Ok(out)
    }

    /// Resolve a user-supplied name: HEAD, a branch, a full ref, a tag,
    /// a full hex id, or an unambiguous abbreviated id (>= 4 hex chars).
    pub fn resolve(&self, name: &str) -> Result<Oid> {
        if name == "HEAD" {
            return self
                .head_commit()?
                .ok_or_else(|| Error::new("HEAD does not point to a commit yet"));
        }
        for candidate in [
            name.to_string(),
            format!("refs/heads/{name}"),
            format!("refs/tags/{name}"),
        ] {
            if candidate.starts_with("refs/") {
                if let Some(oid) = self.read_ref(&candidate)? {
                    return Ok(oid);
                }
            }
        }
        if name.len() == 40 {
            return Oid::from_hex(name);
        }
        if name.len() >= 4 && name.bytes().all(|b| b.is_ascii_hexdigit()) {
            let name = name.to_ascii_lowercase();
            let dir = self.git_dir.join("objects").join(&name[..2]);
            let mut matches = Vec::new();
            if let Ok(rd) = fs::read_dir(dir) {
                for e in rd.flatten() {
                    let rest = e.file_name().to_string_lossy().into_owned();
                    if rest.starts_with(&name[2..]) && rest.len() == 38 {
                        matches.push(format!("{}{rest}", &name[..2]));
                    }
                }
            }
            return match matches.len() {
                1 => Oid::from_hex(&matches[0]),
                0 => Err(Error::new(format!("unknown revision: {name}"))),
                _ => Err(Error::new(format!("ambiguous short id: {name}"))),
            };
        }
        Err(Error::new(format!("unknown revision: {name}")))
    }

    // ------------------------------------------------------------- trees

    /// Write the whole working tree as tree objects and return the root tree id.
    pub fn write_tree(&self) -> Result<Oid> {
        self.write_tree_dir(&self.worktree)
    }

    fn write_tree_dir(&self, dir: &Path) -> Result<Oid> {
        match self.build_tree(dir)? {
            Some(oid) => Ok(oid),
            None => self.write_object(Kind::Tree, &[]),
        }
    }

    /// Returns None for a directory with nothing trackable in it
    /// (git does not record empty directories).
    fn build_tree(&self, dir: &Path) -> Result<Option<Oid>> {
        let mut entries = Vec::new();
        for e in fs::read_dir(dir)? {
            let e = e?;
            let name = e.file_name();
            if name == ".git" {
                continue;
            }
            let path = e.path();
            let meta = fs::symlink_metadata(&path)?;
            let ft = meta.file_type();
            let name = name.into_encoded_bytes();
            if ft.is_symlink() {
                let target = fs::read_link(&path)?;
                let oid = self.write_object(Kind::Blob, target.as_os_str().as_encoded_bytes())?;
                entries.push(TreeEntry { mode: mode::SYMLINK.into(), name, oid });
            } else if ft.is_dir() {
                if let Some(oid) = self.build_tree(&path)? {
                    entries.push(TreeEntry { mode: mode::TREE.into(), name, oid });
                }
            } else if ft.is_file() {
                let data = fs::read(&path)?;
                let oid = self.write_object(Kind::Blob, &data)?;
                let m = if meta.permissions().mode() & 0o111 != 0 {
                    mode::EXEC
                } else {
                    mode::FILE
                };
                entries.push(TreeEntry { mode: m.into(), name, oid });
            }
        }
        if entries.is_empty() {
            return Ok(None);
        }
        let body = object::serialize_tree(&entries);
        self.write_object(Kind::Tree, &body).map(Some)
    }

    /// Flatten a tree into `path -> (mode, blob id)`.
    pub fn flatten_tree(&self, tree: &Oid) -> Result<BTreeMap<PathBuf, (String, Oid)>> {
        let mut out = BTreeMap::new();
        self.flatten_into(tree, PathBuf::new(), &mut out)?;
        Ok(out)
    }

    fn flatten_into(
        &self,
        tree: &Oid,
        prefix: PathBuf,
        out: &mut BTreeMap<PathBuf, (String, Oid)>,
    ) -> Result<()> {
        for e in self.read_tree(tree)? {
            let path = prefix.join(OsString::from_vec(e.name.clone()));
            if e.is_tree() {
                self.flatten_into(&e.oid, path, out)?;
            } else {
                out.insert(path, (e.mode, e.oid));
            }
        }
        Ok(())
    }

    // ----------------------------------------------------------- commits

    pub fn commit_tree(
        &self,
        tree: Oid,
        parents: Vec<Oid>,
        message: &str,
    ) -> Result<Oid> {
        self.read_typed(&tree, Kind::Tree)?;
        for p in &parents {
            self.read_typed(p, Kind::Commit)?;
        }
        let commit = Commit {
            tree,
            parents,
            author: signature_from_env("AUTHOR"),
            committer: signature_from_env("COMMITTER"),
            message: message.to_string(),
        };
        self.write_object(Kind::Commit, &commit.serialize())
    }

    /// write-tree + commit-tree + advance the current branch (or detached HEAD).
    pub fn commit(&self, message: &str) -> Result<Oid> {
        let tree = self.write_tree()?;
        let parents: Vec<Oid> = self.head_commit()?.into_iter().collect();
        let oid = self.commit_tree(tree, parents, message)?;
        match self.head()? {
            Head::Branch(r) => self.update_ref(&r, &oid)?,
            Head::Detached(_) => self.set_head(&Head::Detached(oid))?,
        }
        Ok(oid)
    }

    /// First-parent history starting at `start`.
    pub fn log(&self, start: Oid) -> Result<Vec<(Oid, Commit)>> {
        let mut out = Vec::new();
        let mut cur = Some(start);
        while let Some(oid) = cur {
            let c = self.read_commit(&oid)?;
            cur = c.parents.first().copied();
            out.push((oid, c));
        }
        Ok(out)
    }

    // ---------------------------------------------------------- checkout

    /// Switch the working tree to `target` (a branch name or any revision).
    /// Refuses when the working tree differs from HEAD unless `force` is set.
    /// Untracked files are left alone unless the target would overwrite them.
    pub fn checkout(&self, target: &str, force: bool) -> Result<Head> {
        let branch_ref = format!("refs/heads/{target}");
        let (new_head, new_commit) = match self.read_ref(&branch_ref)? {
            Some(oid) => (Head::Branch(branch_ref), oid),
            None => {
                let oid = self.resolve(target)?;
                (Head::Detached(oid), oid)
            }
        };
        let new_tree = self.read_commit(&new_commit)?.tree;
        let old_files = match self.head_commit()? {
            Some(c) => {
                let old_tree = self.read_commit(&c)?.tree;
                if !force {
                    let current = self.worktree_tree_id()?;
                    if current != old_tree {
                        return Err(Error::new(
                            "working tree has changes not in HEAD; commit them or use --force",
                        ));
                    }
                }
                self.flatten_tree(&old_tree)?
            }
            None => BTreeMap::new(),
        };
        let new_files = self.flatten_tree(&new_tree)?;

        // Remove files tracked by the old tree that the new tree lacks.
        for path in old_files.keys() {
            if !new_files.contains_key(path) {
                let full = self.worktree.join(path);
                if fs::symlink_metadata(&full).is_ok() {
                    fs::remove_file(&full)?;
                }
                remove_empty_parents(&self.worktree, full.parent());
            }
        }
        // Write every file of the new tree.
        for (path, (m, oid)) in &new_files {
            if old_files.get(path) == Some(&(m.clone(), *oid)) && !force {
                continue; // unchanged
            }
            let full = self.worktree.join(path);
            if let Some(parent) = full.parent() {
                fs::create_dir_all(parent)?;
            }
            let data = self.read_typed(oid, Kind::Blob)?;
            if let Ok(meta) = fs::symlink_metadata(&full) {
                if meta.is_dir() {
                    fs::remove_dir_all(&full)?;
                } else {
                    fs::remove_file(&full)?;
                }
            }
            if m == mode::SYMLINK {
                let target = OsString::from_vec(data);
                std::os::unix::fs::symlink(target, &full)?;
            } else {
                fs::write(&full, &data)?;
                let perm = if m == mode::EXEC { 0o755 } else { 0o644 };
                fs::set_permissions(&full, fs::Permissions::from_mode(perm))?;
            }
        }
        self.set_head(&new_head)?;
        Ok(new_head)
    }

    /// Hash the working tree without writing objects, to detect local changes.
    fn worktree_tree_id(&self) -> Result<Oid> {
        // Writing objects is harmless (unreferenced loose objects), and keeps
        // one code path for hashing trees.
        self.write_tree()
    }
}

fn remove_empty_parents(root: &Path, mut dir: Option<&Path>) {
    while let Some(d) = dir {
        if d == root || fs::remove_dir(d).is_err() {
            break;
        }
        dir = d.parent();
    }
}

fn collect_refs(dir: &Path, prefix: &str, out: &mut Vec<String>) -> Result<()> {
    let Ok(rd) = fs::read_dir(dir) else {
        return Ok(());
    };
    for e in rd {
        let e = e?;
        let name = format!("{prefix}{}", e.file_name().to_string_lossy());
        if e.file_type()?.is_dir() {
            collect_refs(&e.path(), &format!("{name}/"), out)?;
        } else {
            out.push(name);
        }
    }
    Ok(())
}

/// Build a signature from `GIT_<ROLE>_NAME/EMAIL/DATE`, falling back to
/// `$USER` and the current time in UTC. `GIT_*_DATE` accepts `"<unix> <+hhmm>"`.
pub fn signature_from_env(role: &str) -> Signature {
    let var = |k: &str| std::env::var(format!("GIT_{role}_{k}")).ok();
    let user = std::env::var("USER").unwrap_or_else(|_| "mgit".into());
    let (time, tz) = var("DATE")
        .and_then(|d| {
            let d = d.trim_start_matches('@');
            let mut it = d.split_whitespace();
            let t: i64 = it.next()?.parse().ok()?;
            Some((t, it.next().unwrap_or("+0000").to_string()))
        })
        .unwrap_or_else(|| {
            let now = std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .map(|d| d.as_secs() as i64)
                .unwrap_or(0);
            (now, "+0000".to_string())
        });
    Signature {
        name: var("NAME").unwrap_or_else(|| user.clone()),
        email: var("EMAIL").unwrap_or_else(|| format!("{user}@localhost")),
        time,
        tz,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn write_and_read_back() {
        let dir = tempfile::tempdir().unwrap();
        let repo = Repo::init(dir.path()).unwrap();
        let oid = repo.write_object(Kind::Blob, b"hello\n").unwrap();
        assert_eq!(oid.to_hex(), "ce013625030ba8dba906f756967f9e9ca394464a");
        assert_eq!(repo.read_object(&oid).unwrap(), (Kind::Blob, b"hello\n".to_vec()));
        // Idempotent.
        assert_eq!(repo.write_object(Kind::Blob, b"hello\n").unwrap(), oid);
    }

    #[test]
    fn empty_worktree_gives_empty_tree() {
        let dir = tempfile::tempdir().unwrap();
        let repo = Repo::init(dir.path()).unwrap();
        fs::create_dir(dir.path().join("empty")).unwrap();
        assert_eq!(
            repo.write_tree().unwrap().to_hex(),
            "4b825dc642cb6eb9a060e54bf8d69288fbee4904"
        );
    }

    #[test]
    fn resolve_short_and_branch() {
        let dir = tempfile::tempdir().unwrap();
        let repo = Repo::init(dir.path()).unwrap();
        fs::write(dir.path().join("f"), "x").unwrap();
        let c = repo.commit("first").unwrap();
        assert_eq!(repo.resolve("HEAD").unwrap(), c);
        assert_eq!(repo.resolve("main").unwrap(), c);
        assert_eq!(repo.resolve(&c.to_hex()[..8]).unwrap(), c);
        assert!(repo.resolve("nope").is_err());
    }

    #[test]
    fn checkout_switches_files() {
        let dir = tempfile::tempdir().unwrap();
        let repo = Repo::init(dir.path()).unwrap();
        fs::write(dir.path().join("a"), "one").unwrap();
        let c1 = repo.commit("one").unwrap();
        fs::create_dir(dir.path().join("sub")).unwrap();
        fs::write(dir.path().join("sub/b"), "two").unwrap();
        fs::write(dir.path().join("a"), "changed").unwrap();
        repo.commit("two").unwrap();

        repo.checkout(&c1.to_hex(), false).unwrap();
        assert_eq!(fs::read_to_string(dir.path().join("a")).unwrap(), "one");
        assert!(!dir.path().join("sub").exists());
        assert_eq!(repo.head().unwrap(), Head::Detached(c1));

        repo.checkout("main", false).unwrap();
        assert_eq!(fs::read_to_string(dir.path().join("sub/b")).unwrap(), "two");
        assert_eq!(repo.head().unwrap(), Head::Branch("refs/heads/main".into()));
    }

    #[test]
    fn checkout_refuses_dirty_tree() {
        let dir = tempfile::tempdir().unwrap();
        let repo = Repo::init(dir.path()).unwrap();
        fs::write(dir.path().join("a"), "one").unwrap();
        let c1 = repo.commit("one").unwrap();
        fs::write(dir.path().join("a"), "dirty").unwrap();
        assert!(repo.checkout(&c1.to_hex(), false).is_err());
        repo.checkout(&c1.to_hex(), true).unwrap();
        assert_eq!(fs::read_to_string(dir.path().join("a")).unwrap(), "one");
    }
}
