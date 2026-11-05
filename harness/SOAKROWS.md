# soakrows.py

22 of 22 rows right, cranelift.

| row | result | who built each comparison |
|---|---|---|
| symlinked file | right | as loaded: daemon; what it points at edited: daemon; pointed at another file: daemon |
| symlinked directory | right | as loaded: daemon; a file under it edited: daemon; edited through the link: daemon |
| symlinked workspace root | right | cold through the link: cargo; edited, through the link: daemon; edited through the link, by the real path: cargo; again through the link: daemon |
| save by rename | right | renamed over, 0: daemon; renamed over, 1: daemon; renamed over, 2: daemon; renamed over, 3: daemon |
| save by truncate and write | right | truncated and written, 0: daemon; truncated and written, 1: daemon; truncated and written, 2: daemon; truncated and written, 3: daemon |
| changed twice within the time stamp | right | written again at once, 0: daemon; written again at once, 1: daemon; written again at once, 2: daemon; same size and time: daemon; same size and time: stale, as warm cargo is (same size and time (daemon): output differs: [('mark 1006', 'mark 1007')]) |
| checkout of thousands of files | right | before: daemon; checkout of 3000 files, one: daemon; checkout of 3000 files, two: daemon; checkout of 3000 files, one: daemon; checkout that removes them: daemon |
| case-only rename | right | as loaded: daemon; renamed in case only: daemon; renamed back: daemon |
| worktree deleted and recreated | right | the worktree: daemon; removed and added again at another commit: cargo; deleted and added again: cargo; edited after that: daemon; the first checkout still: cargo: the daemon has not loaded this build of the workspace |
| worktrees whose libraries were built apart | right | the worktree whose library keeps readings: daemon; the one whose library does not: cargo: the link failed (exit status: 1): rust-lld: error: undefined symbol: <usize as core::iter::range::Step>::forward_unchecked / >>> referenced by range.rs:1103 (/rustc/b940084d7eb6a299eb4bfeb8e34901bc051e7ac4/library/core/src/iter/range.rs:1103) / >>>               /tmp/blaze-soakrows-3n083k2o/worktrees-whose-libraries-were-built-apart/ws/target/blaze/939db0409ca99767/programs/soak_app-cbefde3bb3f0013d/blaze.o:(blaze::spec_next::67) / collect2: error: ld returned 1 exit status; the one whose library does not, edited: cargo: the link failed (exit status: 1): rust-lld: error: undefined symbol: <usize as core::iter::range::Step>::forward_unchecked / >>> referenced by range.rs:1103 (/rustc/b940084d7eb6a299eb4bfeb8e34901bc051e7ac4/library/core/src/iter/range.rs:1103) / >>>               /tmp/blaze-soakrows-3n083k2o/worktrees-whose-libraries-were-built-apart/ws/target/blaze/939db0409ca99767/programs/soak_app-cbefde3bb3f0013d/blaze.o:(blaze::spec_next::67) / collect2: error: ld returned 1 exit status; the one whose library does not, edited again: daemon; the one whose library does not, edited again: daemon; the one whose library does not, edited again: daemon; the first: cargo: the daemon has not loaded this build of the workspace; the first, edited: cargo: the link failed (exit status: 1): rust-lld: error: undefined symbol: <usize as core::iter::range::Step>::forward_unchecked / >>> referenced by range.rs:1103 (/rustc/b940084d7eb6a299eb4bfeb8e34901bc051e7ac4/library/core/src/iter/range.rs:1103) / >>>               /tmp/blaze-soakrows-3n083k2o/worktrees-whose-libraries-were-built-apart/tree/target/blaze/5a23a6f4cea16f13/programs/soak_app-cbefde3bb3f0013d/blaze.o:(blaze::spec_next::34) / collect2: error: ld returned 1 exit status; the first, edited again: daemon; the first, edited again: daemon; the first, edited again: daemon |
| worktrees built by other code | right | the first, with a salt and data of its own: daemon; the worktree asked second: cargo; salt 7, edited: daemon; salt 5, edited: daemon; salt 7, edited: daemon |
| names and text that are not UTF-8 | right | with files cargo does not compile: a name and a text that are not UTF-8: daemon; edited: daemon; the text that is not UTF-8 changed: daemon; tests: cargo |
| workspace under a name that is not UTF-8 | right | cold: cargo (cargo itself exits 101); edited: cargo (cargo itself exits 101) |
| store on another filesystem | right | edited, 0: daemon; edited, 1: daemon; edited, 2: daemon; tests: cargo |
| toolchain file switched | right | the toolchain blaze is built for: daemon; the file now says 1.94.0: cargo; edited under 1.94.0: cargo: rustc 1.94.0 is not the 1.99.0 this blaze compiles for; the file says the first again: cargo: the toolchain file /tmp/blaze-soakrows-3n083k2o/toolchain-file-switched/ws/rust-toolchain.toml changed; edited after that: daemon |
| two toolchains in turn | right | 1.94.0, 0: cargo; 1.99.0, 0: daemon; 1.94.0, 1: cargo: rustc 1.94.0 is not the 1.99.0 this blaze compiles for; 1.99.0, 1: daemon; 1.94.0, 2: cargo: rustc 1.94.0 is not the 1.99.0 this blaze compiles for; 1.99.0, 2: daemon |
| daemon binary replaced | right | before: daemon; the binary replaced under the daemon: cargo; the build after, by the new daemon: daemon; an older client and a newer daemon: cargo; and the newer client again: cargo; the binary deleted, another workspace: cargo; the build after that: cargo: no daemon is running; edited there: daemon |
| memory ceiling | right | the first workspace, loaded: daemon; the first workspace, its worker stopped: cargo: the daemon has not loaded this workspace; the first workspace, loaded again: daemon; its tests: cargo |
| no memory ceiling | right | the first workspace: daemon |
| tests of a proc macro | right | cold: cargo; warm: cargo: a library its unit tests are linked with as rustc built it: /tmp/blaze-soakrows-3n083k2o/tests-of-a-proc-macro/ws/core/src/lib.rs changed; the proc macro alone: cargo |
| a function that is another under test | right | cold: cargo; cold tests: cargo; edited: daemon; edited, tests: daemon; another profile, tests: cargo: the manifest /tmp/blaze-soakrows-3n083k2o/a-function-that-is-another-under-test/ws/Cargo.toml changed; edited: daemon; edited, tests: daemon; edited: daemon; edited, tests: daemon |
| functions rustc keeps to an object | right | cold tests: cargo; edited, tests: cargo: `mark` is not compiled by blaze and rustc's copy is from before the edit, build with cargo: unsupported: rustc's object keeps the function to itself; a test that now fails: cargo: `sums` is not compiled by blaze and rustc's copy is from before the edit, build with cargo: unsupported: rustc's object keeps the function to itself; edited again, tests: cargo: the daemon is still loading the workspace (246ms so far) |

## symlinked workspace root

```
status after a build through the link:
daemon: pid 4027858, version 0.0.0-3dbb3e0-18df8554f87b32ab, exits after 1800s idle, workers hold at most 16047 MB
  worker: pid 4027861, /tmp/blaze-soakrows-3n083k2o/symlinked-workspace-root/ws/, 992 MB, log /tmp/blaze-soakrows-3n083k2o/symlinked-workspace-root/home/workers/3e635d7763e63220/log
    /tmp/blaze-soakrows-3n083k2o/symlinked-workspace-root/ws: ready
/tmp/blaze-soakrows-3n083k2o/symlinked-workspace-root/ws (dev, c3031dc6bc0403b1): 0 builds by the daemon, 0 by cargo

```

## changed twice within the time stamp

```
same size and time: warm cargo is stale
```

## worktree deleted and recreated

```
status with the worktree deleted:
daemon: pid 4053875, version 0.0.0-3dbb3e0-18df8554f87b32ab, exits after 1800s idle, workers hold at most 16047 MB
/tmp/blaze-soakrows-3n083k2o/worktree-deleted-and-recreated/ws (dev, 1cb704f83dd5b740): 0 builds by the daemon, 0 by cargo

```

## store on another filesystem

```
278852 bytes in 1 packs in the store on the other filesystem
```

## daemon binary replaced

```
daemon 4080246 was replaced by 4081335
the older client replaced daemon 4081335 by 4082640
daemon 4083196 whose binary was deleted was replaced by 4084454
```

## memory ceiling

```
one workspace loaded:
daemon: pid 4085930, version 0.0.0-3dbb3e0-18df8554f87b32ab, exits after 1800s idle, workers hold at most 1 MB
  worker: pid 4085933, /tmp/blaze-soakrows-3n083k2o/memory-ceiling/ws/, 993 MB, log /tmp/blaze-soakrows-3n083k2o/memory-ceiling/home/workers/7d0428d53c27bf4a/log
    /tmp/blaze-soakrows-3n083k2o/memory-ceiling/ws: ready
/tmp/blaze-soakrows-3n083k2o/memory-ceiling/ws (dev, 30f1dba31e53bc4f): 1 builds by the daemon, 0 by cargo

a second loaded, three seconds later:
daemon: pid 4085930, version 0.0.0-3dbb3e0-18df8554f87b32ab, exits after 1800s idle, workers hold at most 1 MB
  worker: pid 4087182, /tmp/blaze-soakrows-3n083k2o/memory-ceiling/ws2/, 840 MB, log /tmp/blaze-soakrows-3n083k2o/memory-ceiling/home/workers/669444e08b2e8df6/log
    /tmp/blaze-soakrows-3n083k2o/memory-ceiling/ws2: ready
/tmp/blaze-soakrows-3n083k2o/memory-ceiling/ws (dev, 30f1dba31e53bc4f): 1 builds by the daemon, 0 by cargo
/tmp/blaze-soakrows-3n083k2o/memory-ceiling/ws2 (dev, 2ff2bf3bf318179d): 0 builds by the daemon, 0 by cargo

after the first was asked again:
daemon: pid 4085930, version 0.0.0-3dbb3e0-18df8554f87b32ab, exits after 1800s idle, workers hold at most 1 MB
  worker: pid 4089118, /tmp/blaze-soakrows-3n083k2o/memory-ceiling/ws/, 746 MB, log /tmp/blaze-soakrows-3n083k2o/memory-ceiling/home/workers/abdcaed9ca929402/log
    /tmp/blaze-soakrows-3n083k2o/memory-ceiling/ws: loading for 4s
  worker: pid 4089115, /tmp/blaze-soakrows-3n083k2o/memory-ceiling/ws/, 957 MB, log /tmp/blaze-soakrows-3n083k2o/memory-ceiling/home/workers/fb6cd0e53ec61ff1/log
    /tmp/blaze-soakrows-3n083k2o/memory-ceiling/ws: loading for 4s
/tmp/blaze-soakrows-3n083k2o/memory-ceiling/ws (test, 810f675272fb561a): 0 builds by the daemon, 0 by cargo
/tmp/blaze-soakrows-3n083k2o/memory-ceiling/ws (dev, 30f1dba31e53bc4f): 2 builds by the daemon, 1 by cargo
  1 x the daemon has not loaded this workspace
/tmp/blaze-soakrows-3n083k2o/memory-ceiling/ws2 (dev, 2ff2bf3bf318179d): 0 builds by the daemon, 0 by cargo

worker 7d0428d53c27bf4a of /tmp/blaze-soakrows-3n083k2o/memory-ceiling/ws/ stopped: the workers hold 1283 MB, over the 1 MB of BLAZE_MEMORY_MAX, and it was asked longest ago
worker 669444e08b2e8df6 of /tmp/blaze-soakrows-3n083k2o/memory-ceiling/ws2/ stopped: the workers hold 1168 MB, over the 1 MB of BLAZE_MEMORY_MAX, and it was asked longest ago
worker 7d0428d53c27bf4a of /tmp/blaze-soakrows-3n083k2o/memory-ceiling/ws/ stopped: the workers hold 1522 MB, over the 1 MB of BLAZE_MEMORY_MAX, and it was asked longest ago
```

## no memory ceiling

```
daemon: pid 4090381, version 0.0.0-3dbb3e0-18df8554f87b32ab, exits after 1800s idle, workers hold any amount
  worker: pid 4091382, /tmp/blaze-soakrows-3n083k2o/no-memory-ceiling/ws2/, 990 MB, log /tmp/blaze-soakrows-3n083k2o/no-memory-ceiling/home/workers/1468e2a337f41f92/log
    /tmp/blaze-soakrows-3n083k2o/no-memory-ceiling/ws2: ready
  worker: pid 4090384, /tmp/blaze-soakrows-3n083k2o/no-memory-ceiling/ws/, 988 MB, log /tmp/blaze-soakrows-3n083k2o/no-memory-ceiling/home/workers/0b5e42383f48875e/log
    /tmp/blaze-soakrows-3n083k2o/no-memory-ceiling/ws: ready
/tmp/blaze-soakrows-3n083k2o/no-memory-ceiling/ws2 (dev, 4636896133d06866): 0 builds by the daemon, 0 by cargo
/tmp/blaze-soakrows-3n083k2o/no-memory-ceiling/ws (dev, 6a93787323dab24a): 0 builds by the daemon, 0 by cargo

```
