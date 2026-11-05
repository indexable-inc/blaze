# docs/upstream/bun: what was fetched, from where, when

Fetched on **2026-10-21** for [docs/research/bun.md](../../research/bun.md). Files are verbatim
unless the notes say otherwise. In file names, `/` in the upstream path became `_`.

| file | source |
|---|---|
| `blog_bun-in-rust.md` | `https://bun.sh/blog/bun-in-rust.md` ("Rewriting Bun in Rust", Jarred Sumner, 2026-07-08) |
| `docs_project_contributing.mdx` | `https://raw.githubusercontent.com/oven-sh/bun/c6da4a4d3010e5553438c60f6bd76d981976867c/docs/project/contributing.mdx` (what bun's build needs) |
| `rust-toolchain.toml` | `rust-toolchain.toml` at the same commit (the pinned nightly) |
| `pr-30412.json` | `gh pr view 30412 -R oven-sh/bun --json number,title,author,mergedAt,mergeCommit,additions,deletions,changedFiles,state,url` ("Rewrite Bun in Rust"), the body left out |

The commit is the tag `bun-v1.4.3` (`git ls-remote https://github.com/oven-sh/bun refs/tags/bun-v1.4.3`).
Licence of bun's repository: MIT for bun's own code (its `LICENSE.md`); the blog post and the
documentation are bun's.
