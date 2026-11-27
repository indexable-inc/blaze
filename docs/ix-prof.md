---
status: current
summary: Profiling any Rust program, not only blaze, like Go's pprof. Covers the ix-prof crate (CPU samples of every thread, tracing spans with their own CPU time and allocations, named waits, allocations by stack), `IX_PROF=<dir>`, the loopback `/debug/pprof` endpoint and live stream, and the report functions (top, peek, tree, list, flame, diff, pprof and Chrome export); built, not wired into any binary, and `blaze pprof` not built.
---
# Profiling any Rust program: ix-prof

`crates/ix-prof` is a profiler any Rust program can link. Nothing in it is specific to blaze.
The file format and the live stream are specified in [ix-prof-format.md](ix-prof-format.md).

It is built, not wired: no binary in the tree links it except through `crates/blaze-profile`,
which no binary reaches. `blaze pprof`, the reader its reports and token checks were written
for, is not built.

## Turn it on

```toml
[dependencies]
ix-prof = { path = "…/crates/ix-prof", features = ["http"] }   # `http`: the endpoint
```

```rust
// Optional: counts allocations and gives the heap profile.
#[global_allocator]
static ALLOC: ix_prof::alloc::Counting<std::alloc::System> =
    ix_prof::alloc::Counting::new(std::alloc::System);

fn main() {
    let _prof = ix_prof::start_from_env().expect("ix-prof");
    // Optional: spans, and the waits you mark with a field `wait = "<kind>"`.
    _ = blaze_trace::install(ix_prof::layer());
    // …
}
```

Then run it with:

```sh
IX_PROF=target/prof ./my-program      # writes target/prof/my-program-<pid>-<session>.ixprof at exit
```

The directory must be yours alone: owner-only mode and no ACL, on a local volume that keeps
ownership and that you or root mounted. ix-prof makes it, mode 0700, if it is missing, and
refuses (at start, naming the directory and the rule) one that is a symbolic link (`link/` too),
belongs to another user, has any group or other mode bits, has an ACL (access control list)
entry (on Linux, a default ACL too), or is on a volume that ignores ownership, on a volume
another user mounted, on a network file system, or on a FUSE mount it can't trust (below); it
never changes the mode or ACL of a directory it did not make. On macOS it sets the directory's
owner to you, which is who macOS shows already (see uid 99 below).
The profile and the endpoint's token go there, and whoever owns or can write a directory can
replace the files in it: in a shared `/tmp/prof` that another user made first, they could swap
the endpoint's address for their own port and collect the token. An ACL grants access the mode
bits never show: on macOS, the owner of `/tmp/prof` can put inheritable entries on it, and
`/tmp/prof/mine`, made 0700, and the 0600 files in it inherit them. `ls -le` shows an ACL and
`chmod -N` removes it (`getfacl` and `setfacl -b` on Linux). On a macOS volume that ignores
ownership (Finder's "Ignore ownership on this volume", common on external drives) every local
user counts as the owner of every file, so the mode protects nothing: `diskutil enableOwnership`
it and then make a new directory, or use another volume. A directory made while ownership was
ignored stays on disk as uid 99, which macOS shows as owned by whoever looks (only `sudo ls -ln`
reveals it), so every user could replace its files; ix-prof makes it yours for real by setting
its owner to you when it opens it, which needs no root, but files already in it were writable by
anyone, hence the new directory. Whoever mounted a volume can read every file on it whatever its
mode (on macOS, a disk image's owner re-attaches it with ownership ignored), so a volume mounted
by another user (macOS `f_owner`) is refused. On NFS, SMB, AFS and Coda (and any network volume
on macOS) the server can grant access by its own ACLs, which neither the mode nor a local ACL
shows, so they are refused too: put `IX_PROF` on a local disk. A FUSE mount's daemon makes up
the owner, mode and ACL ix-prof checks and receives every byte written to it. On Linux, ix-prof
finds the directory's mount in `/proc/self/mountinfo` (by the descriptor's mount ID and device)
and accepts FUSE only when you mounted it, without `allow_other` or with
`default_permissions`; when root mounted it with `default_permissions` (rootless containers'
fuse-overlayfs), unless it is a network store (sshfs, s3fs, rclone, gcsfuse, goofys, juicefs,
davfs), refused like NFS; or when it is virtiofs, which only root can mount. Any other FUSE
mount, including one another user made with `allow_other`, is refused, naming who mounted it and
its options. The kernel shows who mounted it (`user_id`) in the mounter's user namespace, so a
mount whose `user_id` is your uid counts as yours only when you are root or in the initial user
namespace (`/proc/self/uid_map` is `0 0 4294967295`): inside a container, a host user's FUSE
mount bound in with `/tmp` can show your uid. On macOS a macFUSE volume is a network volume unless mounted with `-o local`, and then
must be yours or root's like any other.

With no `IX_PROF` and no `IX_PROF_HTTP` set, nothing starts. Each span callback and each
allocation then costs one relaxed atomic load.

| variable | meaning | default |
|---|---|---|
| `IX_PROF=<dir>` | turns profiling on; the profile is written to this directory when the guard drops | off |
| `IX_PROF_HZ` | CPU samples per second per thread; `0` turns the sampler off | 99 |
| `IX_PROF_ALLOC=<bytes>\|off` | mean bytes between sampled allocations (Go's `MemProfileRate`) | 524288 |
| `IX_PROF_HTTP=127.0.0.1:<port>` | serves `/debug/pprof` (needs the `http` feature, Unix); loopback only, every request needs the token | off |
| `IX_PROF_RETAIN=<bytes>` | how many bytes of the live stream are kept for resuming | 64 MiB |

What gets recorded:

- **CPU samples of every thread.** No thread has to register.
  - Linux: `perf_event_open`, using each thread's task clock. If the kernel refuses (for
    example `kernel.perf_event_paranoid` > 2), `start` returns an error that says so.
  - arm64 macOS: each thread is stopped briefly and its frame pointers are walked.
  - Blocked threads are sampled one tick in ten, marked off-CPU and weighted for the ticks
    that were skipped.
- **The span tree.** Each span records its wall time, its own CPU time (minus its
  children's) and the allocations made inside it.
- **Waits.** A span with fields `wait`, `on` and `owner` is a wait: what it waits for and who
  will wake it.
- **Allocations**, sampled by stack, with live and total bytes. Heap counters are recorded
  every 250 ms.
- **Symbols and line numbers** come from the binary's DWARF (or its dSYM on macOS) when the
  profile is written, never while sampling.

## Read it

The reports are library functions over a `View` (`View::of(&profile, Kind)`, kinds CPU, spans,
heap and waits; `View::from_pprof` reads Go's, pprof-rs's or parca's files): `ix_prof::report`'s
`top`, `peek`, `tree`, `list`, `flame`, `folded` and `diff`, `View::to_pprof` (for go tool pprof,
speedscope, Perfetto) and `chrome::to_chrome`. A heap view's sample types are `inuse_space`,
`alloc_space` and the matching object counts. No command calls them yet.

## Live

With `IX_PROF_HTTP=127.0.0.1:6060`, the program serves Go's layout:

- `/debug/pprof/profile?seconds=30` (CPU)
- `/heap` and `/allocs`
- `/spans` and `/waits`
- `/threads`: each thread's open spans and what it waits on, right now
- `/ixprof?last=N`: a window snapshot
- `/live`: the stream the explorer draws, which can be resumed from a cursor

Every request needs the token: 128 random bits made at start. With `IX_PROF=<dir>`, ix-prof
writes the address and the token to `<dir>/http.endpoint` (one 0600 file, the address on the
first line and the token on the second, renamed into place whole) and prints only where it is
(`ix-prof: live at http://…/debug/pprof/live (its token is in <dir>/http.endpoint; …)`): a
program's stderr often lands where other users can read it, such as a CI log or a shared
`2>run.log`. Without `IX_PROF` there is no file to put the token in, so the stderr line carries
it (`…/live?token=…`), and so does whatever stderr goes to. The file is removed when the
profiler stops.

Send the token as a header, read from that file, never as an argument:

```sh
curl -sH @<(printf 'Authorization: Bearer %s\n' "$(sed -n 2p target/prof/http.endpoint)") \
  "http://$(sed -n 1p target/prof/http.endpoint)/debug/pprof/threads"   # printf is a builtin: not in argv
```

A client's half is built, not wired: `endpoint::read` and `endpoint::read_token_file` read the
token only from your own regular file, mode 0600, no ACL, at most 64 bytes, in a directory that
is yours, with owner-only mode and no ACL (the same rule as ix-prof's), and `endpoint::loopback`
accepts only a loopback IP address and port (`localhost` is refused: write `127.0.0.1` or
`[::1]`).

A token on a command line (`go tool pprof '…&token=…'`, `curl -H "Authorization: Bearer
$(cat …)"`) is readable by every local user for as long as that command runs (`ps`,
`/proc/<pid>/cmdline`), and the token stays good for the profiled program's whole life.
`token=` in the query is for the browser (the ix.dev explorer), which cannot send the header.

The server binds loopback only, and answers 403 unless the `Host` header names loopback and
the request carries the token (`Authorization: Bearer <token>`, or `token=<token>` in the
query). Loopback alone is not enough: every local account can connect to a loopback port.
What runs before the token check is bounded: a request over 16 KiB, or not whole within 5 s of
its connection being accepted (one deadline for the whole request, so a client that trickles
a byte at a time is cut off all the same), is refused; at most 32 connections wait for their
token check at once, and one more evicts the oldest of them rather than being turned away, so
connections another user holds open cannot lock you out (a connection that passed the check is
never evicted); an `accept` that fails (out of file descriptors) pauses accepting for up to
1 s at a time instead of spinning. The profile file is mode 0600, and a directory ix-prof makes for it is 0700. The
endpoint is HTTP and not a Unix socket because the ix.dev explorer, a browser page, reads it.
The server stops, and lets its port go, when the guard drops. A pprof read with
`View::from_pprof` is capped at 256 MiB unpacked (`pprof::MAX_PPROF_BYTES`).
