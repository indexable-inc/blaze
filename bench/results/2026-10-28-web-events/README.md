# Functions' records: what they cost a build (2026-10-28)

Lane web. A worker now says every function it puts in the program as `Event::Functions`
records (`blaze_protocol::events`): its crate, how a backtrace names it, what it calls in the
object, and whether this build compiled it (and in how long), took it from the store or kept
it. The codegen says them as it places each function (`crates/hir-ty-codegen/watch.rs`), and
times `compiled_root` and `compiled_instance` while a build is watched. The daemon reads them
from the worker's events file with the rest and sends them to no command; the build page reads
them (docs/web.md, next).

## Method

ix VM, x86_64, 16 vCPU, 32 GiB committed (us-west-2), nothing else running. *Before* is main
at the merge base (`4e644980cc`), *after* the change on it; both `cargo build --release -p
blaze-engine` in `nix develop`. The project is anim (indexable-inc/anim 92c64f7, wgpu, taffy,
tiny-skia: 79 644 functions and instances, 149 831 calls in the cold build's object).

`bench.py` with `input.json`: each arm has its own `BLAZE_HOME`, store and copy of anim. The
arms take turns throughout: 2 cold builds each (all of it removed before each), then with the
last cold's daemon 3 warm-ups, 30 no-op `blaze build`s, and 8 body edits of anim's `main`.
Each is a fresh process timed whole from Python. `report.json` has every run.

## Numbers

anim, ms (wall time of `blaze build`):

| | before | after | difference |
|---|---|---|---|
| no-op, median of 30 | 5.84 | 5.82 | −0.02 |
| no-op, p90 | 6.39 | 6.76 | +0.37 |
| body edit, median of 8 | 1 272.7 | 1 264.8 | −7.9 |
| cold, median of 2 | 239 286 | 232 907 | −6 380 |

A no-op does not reach the codegen and costs the same. The cold build's records are 37 MB
(names are 33 MB of it: a backtrace's name of an instance is 418 bytes on average), written
beside the build's other records once; the two cold samples of each arm differ by more than
the arms do. The body edit's patch says the few functions it compiled again.

The after arm measured here also kept the program in the daemon on the relay's thread (an
earlier build page), which this change leaves out: this change alone does less.

## The daemon's memory

The resident memory of the daemon after a cold build of anim, in a run of its own on the same
VM (a fresh `BLAZE_HOME` and store, `ps -o rss`), 5 s and 60 s after the build ended:

| | before (`4e644980cc`) | this change |
|---|---|---|
| 5 s | 23 MB | 30.6 MB |
| 60 s | not measured | 30.6 MB |

The daemon reads the 37 MB of records from the worker's events file and drops the functions';
what it keeps is the allocator's.
