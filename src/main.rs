use mini_git::object::{self, Kind};
use mini_git::repo::{Head, Repo};
use mini_git::{Error, Result};
use std::io::{Read, Write};
use std::path::Path;
use std::process::ExitCode;

const USAGE: &str = "usage: mgit <command> [args]

commands:
  init [dir]                         create an empty repository
  hash-object [-w] [-t type] (--stdin | <file>)
  cat-file (-p | -t | -s | -e) <object>
  write-tree                         write the working tree as tree objects
  commit-tree <tree> [-p <parent>]... -m <message>
  commit -m <message>                snapshot the working tree onto the current branch
  log [<rev>]                        first-parent history
  branch [<name> [<rev>]]            list branches, or create one
  checkout [--force] <branch|rev>    switch the working tree
  rev-parse <rev>                    print the full id of a revision";

fn main() -> ExitCode {
    let args: Vec<String> = std::env::args().skip(1).collect();
    match run(&args) {
        Ok(()) => ExitCode::SUCCESS,
        Err(e) => {
            eprintln!("mgit: {e}");
            ExitCode::FAILURE
        }
    }
}

fn repo() -> Result<Repo> {
    Repo::discover(&std::env::current_dir()?)
}

fn run(args: &[String]) -> Result<()> {
    let Some(cmd) = args.first() else {
        return Err(Error::new(USAGE));
    };
    let rest = &args[1..];
    let mut out = std::io::stdout().lock();
    match cmd.as_str() {
        "init" => {
            let dir = rest.first().map(String::as_str).unwrap_or(".");
            std::fs::create_dir_all(dir)?;
            let r = Repo::init(Path::new(dir))?;
            writeln!(
                out,
                "Initialized empty Git repository in {}",
                r.git_dir.canonicalize()?.display()
            )?;
        }
        "hash-object" => {
            let mut write = false;
            let mut kind = Kind::Blob;
            let mut stdin = false;
            let mut file = None;
            let mut it = rest.iter();
            while let Some(a) = it.next() {
                match a.as_str() {
                    "-w" => write = true,
                    "--stdin" => stdin = true,
                    "-t" => {
                        kind = Kind::parse(it.next().ok_or_else(|| Error::new("-t needs a type"))?)?
                    }
                    f => file = Some(f.to_string()),
                }
            }
            let data = if stdin {
                let mut v = Vec::new();
                std::io::stdin().read_to_end(&mut v)?;
                v
            } else {
                std::fs::read(file.ok_or_else(|| Error::new("hash-object needs a file"))?)?
            };
            let oid = if write {
                repo()?.write_object(kind, &data)?
            } else {
                object::hash(kind, &data)
            };
            writeln!(out, "{oid}")?;
        }
        "cat-file" => {
            let [flag, name] = rest else {
                return Err(Error::new("usage: mgit cat-file (-p|-t|-s|-e) <object>"));
            };
            let r = repo()?;
            let oid = r.resolve(name)?;
            if flag == "-e" {
                return if r.has_object(&oid) {
                    Ok(())
                } else {
                    Err(Error::new(format!("object {oid} does not exist")))
                };
            }
            let (kind, data) = r.read_object(&oid)?;
            match flag.as_str() {
                "-t" => writeln!(out, "{}", kind.as_str())?,
                "-s" => writeln!(out, "{}", data.len())?,
                "-p" => match kind {
                    Kind::Tree => out.write_all(object::pretty_tree(&object::parse_tree(&data)?).as_bytes())?,
                    _ => out.write_all(&data)?,
                },
                f => return Err(Error::new(format!("unknown flag {f}"))),
            }
        }
        "write-tree" => writeln!(out, "{}", repo()?.write_tree()?)?,
        "commit-tree" => {
            let r = repo()?;
            let mut tree = None;
            let mut parents = Vec::new();
            let mut msg = None;
            let mut it = rest.iter();
            while let Some(a) = it.next() {
                match a.as_str() {
                    "-p" => parents.push(r.resolve(it.next().ok_or_else(|| Error::new("-p needs a commit"))?)?),
                    "-m" => msg = Some(it.next().ok_or_else(|| Error::new("-m needs a message"))?.clone()),
                    t => tree = Some(r.resolve(t)?),
                }
            }
            let tree = tree.ok_or_else(|| Error::new("commit-tree needs a tree"))?;
            let msg = msg.ok_or_else(|| Error::new("commit-tree needs -m <message>"))?;
            writeln!(out, "{}", r.commit_tree(tree, parents, &msg)?)?;
        }
        "commit" => {
            let msg = match rest {
                [m, msg] if m == "-m" => msg,
                _ => return Err(Error::new("usage: mgit commit -m <message>")),
            };
            let r = repo()?;
            let oid = r.commit(msg)?;
            let branch = match r.head()? {
                Head::Branch(b) => b.trim_start_matches("refs/heads/").to_string(),
                Head::Detached(_) => "detached HEAD".into(),
            };
            let summary = msg.lines().next().unwrap_or("");
            writeln!(out, "[{branch} {}] {summary}", &oid.to_hex()[..7])?;
        }
        "log" => {
            let r = repo()?;
            let start = r.resolve(rest.first().map(String::as_str).unwrap_or("HEAD"))?;
            for (oid, c) in r.log(start)? {
                writeln!(out, "commit {oid}")?;
                writeln!(out, "Author: {} <{}>", c.author.name, c.author.email)?;
                writeln!(out, "Date:   {}", c.author.git_date())?;
                writeln!(out)?;
                for line in c.message.lines() {
                    writeln!(out, "    {line}")?;
                }
                writeln!(out)?;
            }
        }
        "branch" => {
            let r = repo()?;
            match rest {
                [] => {
                    let current = match r.head()? {
                        Head::Branch(b) => Some(b),
                        Head::Detached(_) => None,
                    };
                    for b in r.branches()? {
                        let mark = if current.as_deref() == Some(&format!("refs/heads/{b}")) { "*" } else { " " };
                        writeln!(out, "{mark} {b}")?;
                    }
                }
                [name, start @ ..] => {
                    let oid = r.resolve(start.first().map(String::as_str).unwrap_or("HEAD"))?;
                    r.update_ref(&format!("refs/heads/{name}"), &oid)?;
                }
            }
        }
        "checkout" => {
            let (force, target) = match rest {
                [f, t] if f == "--force" || f == "-f" => (true, t),
                [t] => (false, t),
                _ => return Err(Error::new("usage: mgit checkout [--force] <branch|rev>")),
            };
            match repo()?.checkout(target, force)? {
                Head::Branch(b) => writeln!(out, "Switched to branch '{}'", b.trim_start_matches("refs/heads/"))?,
                Head::Detached(oid) => writeln!(out, "HEAD is now at {}", &oid.to_hex()[..7])?,
            }
        }
        "rev-parse" => {
            let name = rest.first().ok_or_else(|| Error::new("rev-parse needs a revision"))?;
            writeln!(out, "{}", repo()?.resolve(name)?)?;
        }
        "-h" | "--help" | "help" => writeln!(out, "{USAGE}")?,
        other => return Err(Error::new(format!("unknown command '{other}'\n{USAGE}"))),
    }
    Ok(())
}
