# Security

blaze is a prototype and has not had a security review. This page says where the parts
that matter for security are described and how to report a problem.

## Reporting

Please report a vulnerability privately through GitHub's "Report a vulnerability" button on
the repository's Security tab rather than in a public issue. There is no bounty and no
promised response time.

A miscompile, where a program built with blaze behaves differently from the one rustc
builds, is a correctness bug and can be reported in a normal issue.

## Supported versions

Only the latest release, and `main`, get fixes.

## What to know before using it

- **The daemon runs your build.** It runs as your user, loads workspaces, runs build
  scripts and proc macros as cargo does, and writes executables into `target/`. It has the
  same access to your machine as cargo.
- **The daemon's socket is for one user.** It lives in a directory that must be owned by
  you with mode 0700, the socket is 0600, and a connection from another user is refused
  before its request is read. On Windows the directory and peer checks are not implemented.
  The refusal of another user is not covered by a test. See
  [docs/cargo.md](../docs/cargo.md), "The daemon".
- **A shared store is trusted input unless you turn signing on.** Packs fetched from a
  remote are checked against the hash in their name, so storage cannot alter them. But
  whoever can write to the bucket can list a pack of their own making, and blaze will link
  that code into your programs. With `BLAZE_REMOTE_TRUST` set, only manifests signed by
  keys you list are accepted. See [docs/cache.md](../docs/cache.md), "Trust and signing".
- **The local store is trusted.** Its records carry checksums against damage, not against
  tampering: anyone who can write to the directory `BLAZE_CACHE` names can put code into
  your builds. Keep it writable by you alone.
- **Credentials.** blaze does not hold S3 credentials; the AWS SDK finds and uses them, and
  they are not printed.
- **blaze is not a sandbox and not a checker.** It builds programs rustc would reject for
  borrow or move errors. See [docs/support.md](../docs/support.md).
