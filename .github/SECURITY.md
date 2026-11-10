# Security

blaze is a prototype and has not had a security review. This page says where the parts
that matter for security are described and how to report a problem.

## Reporting

Please report a vulnerability privately through GitHub's "Report a vulnerability" button on
the repository's Security tab rather than in a public issue. There is no bounty and no
promised response time.

A miscompile, where a program built with blaze behaves differently from the one the pinned
nightly's rustc builds, is a correctness bug and can be reported in a normal issue.

## Supported versions

Only the latest release, and `main`, get fixes.

## What to know before using it

- **The daemon runs your build.** It runs as your user and loads workspaces. It writes no
  build file into the checkout: a workspace builds into its directory under blaze's root
  (`$BLAZE_HOME`, else `~/.cache/blaze`). See [docs/cargo.md](../docs/cargo.md), "Where a
  build writes".
- **Build scripts and proc macros run in a sandbox.** Every build script is compiled by
  blaze and run in the sandbox tests run in; proc macros are compiled to WebAssembly and run
  in Wasmtime, in the daemon. Where no sandbox can be made, blaze stops with an error and
  does not run them outside one, unless you opt out by a documented setting. The sandbox bounds what user code of a build can read and
  write; it has not been reviewed as a security boundary against hostile code. Tests and the
  programs `blaze run` starts are your code run as you: plain `blaze test` runs them outside
  any sandbox, as cargo does. See [docs/cargo.md](../docs/cargo.md), "Build scripts and proc
  macros run in a sandbox" and "Proc macros run as WebAssembly".
- **The daemon's socket is for one user.** It lives in a directory that must be owned by
  you with mode 0700, the socket is 0600, and a connection from another user is refused
  before its request is read. `blaze serve --http ADDR` listens on TCP only with a bearer
  token in `BLAZE_API_TOKEN`. On Windows the directory and peer checks are not implemented.
  See [docs/cargo.md](../docs/cargo.md), "Files".
- **The local store is trusted.** Its records carry checksums against damage, not against
  tampering: anyone who can write to blaze's root can put code into your builds. Keep it
  writable by you alone.
- **There is no shared store.** The blaze of today pulls from and pushes to no remote cache,
  and holds no cloud credentials. The plan for one, with its trust and signing, is
  [docs/remote-cache.md](../docs/remote-cache.md); nothing of it is a setting yet.
- **What blaze fetches is checked.** Registry packages are checked against the checksums of
  the lock file, and the toolchain and the pinned LLVM against the sha256 blaze pins.
- **blaze refuses what its checks find, and they are not all of rustc's.** Its borrow checker
  refuses the errors it finds, which is not yet every error rustc gives, so a program rustc
  rejects may build. See [docs/borrowck.md](../docs/borrowck.md).
