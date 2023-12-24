# mini-git

A small Git implementation in Rust that reads and writes **real git loose objects**
(zlib-compressed, SHA-1 addressed). Repositories created by `mgit` can be opened by
`git`, and `mgit` can read repositories created by `git` (as long as objects are loose).

## Quick start

```sh
cargo build --release
alias mgit=target/release/mgit

mgit init demo && cd demo
echo hello > a.txt
mgit hash-object -w a.txt        # same id as `git hash-object a.txt`
mgit commit -m "first"
git log --oneline                # real git reads it
mgit log
mgit branch feature && mgit checkout feature
```

Commands: `init`, `hash-object [-w] [-t type]`, `cat-file (-p|-t|-s|-e)`, `write-tree`,
`commit-tree <tree> [-p parent]... -m msg`, `commit -m msg`, `log`, `branch`,
`checkout [--force]`, `rev-parse`.

Author/committer come from `GIT_AUTHOR_NAME`/`GIT_AUTHOR_EMAIL` (and the committer
equivalents), falling back to `$USER`.

## How it works

- `object.rs`: object encoding (`"<type> <len>\0<body>"`), SHA-1 ids, zlib via `flate2`,
  tree serialization with git's sort order (directories compare as if suffixed with `/`),
  commit parsing/serialization and signature/date formatting.
- `repo.rs`: `.git` layout, loose object store (`objects/ab/cdef…`), refs and symbolic
  `HEAD`, revision resolution (full/short ids, branches, `HEAD`), `write-tree` straight
  from the working directory, first-parent `log`, and `checkout` which diffs the
  current and target trees and refuses to clobber uncommitted changes unless `--force`.

## Tests

`cargo test` runs unit tests plus `tests/git_compat.rs`, which drives the `mgit`
binary and checks with real `git` (`git fsck --strict`, `git log`, `git cat-file`,
`git write-tree`) that the objects are byte-for-byte compatible, in both directions.
Those tests skip themselves if `git` is not installed.

## Status / limitations

Learning project. Not supported: the index/staging area (`commit` snapshots the whole
working tree, so there is no `add`), packfiles (objects packed by `git gc` can't be read),
merges, remotes, tags, `.gitignore` (only `.git` is skipped), symlinks and submodules.

## License

MIT, see [LICENSE](LICENSE).
