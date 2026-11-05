# eden/fs/docs/Overview.md (facebook/sapling, main, fetched 2026-10-24)

EdenFS is a virtual filesystem designed for efficiently serving large source
control repositories.

In particular, EdenFS is targeted at massive
monorepos, where a single repository
may contain numerous projects, potentially spanning many millions of files in
total. In most situations individual developers may only need to interact with a
fraction of the files in the repository when working on their specific projects.
EdenFS speeds up workflows in this case by lazily fetching file data, so that it
only needs to fetch file information for portions of the repository that are
actually used.

EdenFS aims to speed up several different types of operations:

- Determining files modified from the current source control state. (e.g.,
 computing the output for `hg status` or `git status`)
- Switching the filesystem state from one commit to another. (e.g., performing
 an `hg checkout` or `git checkout` operation).
- Tracking and delivering notifications about modified files. EdenFS can deliver
 notifications of file changes events through Watchman, to allow downstream
 tools like build tools and IDEs to build functionality that depends on file
 notification events.

Additionally, EdenFS also provides several additional features like efficiently
returning file content hashes. This allows downstream build tools to retrieve
file hashes without actually needing to read and hash the file contents.

## Operating System Interface

EdenFS is supported on Linux, macOS, and Windows. The mechanism used to interact
with the filesystem layer is different across these three different platforms.

On Linux, EdenFS uses
FUSE to provide
filesystem functionality.

On macOS, EdenFS uses NFSv3.

On Windows, EdenFS uses Microsoft's
Projected File System.
This behaves fairly differently from FUSE and NFS, but EdenFS still shares most
of the same internal logic for tracking file state.
