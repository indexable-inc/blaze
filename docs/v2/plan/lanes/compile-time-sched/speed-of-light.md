---
status: plan
summary: lane compile-time-sched's speed of light: the compile-time phase of a cold build bounded by compiling each distinct function once on all cores plus the chain of script runs, and where v2 stands against it.
date: 2026-10-30
---
# compile-time-sched: speed of light

The compile-time phase builds every build script and proc macro of a job and runs the scripts.
Its bound is the larger of two:

- **Work over cores.** Each distinct function the compile-time programs reach analysed and
  compiled once, std's included, divided over every core: \(\sum_f (a_f + c_f) / N\), where
  \(a_f\) is the front end's cost of \(f\) (names, bodies, MIR) and \(c_f\) its compile at
  Cranelift (build-time code is always Cranelift, `plan.rs`), or LLVM for std's shipped O3.
- **The chain.** The longest path of compile, run and dependent compile: a build script must
  be compiled and run before the crate it configures is checked, and a proc macro compiled
  before the crates that expand it. A run waiting on its child or on the C compiles it starts
  holds no core, so the chain adds wall time, not core time.

| what | v1 | bound | v2 now |
|---|---|---|---|
| std's names and bodies | once per process: 41 times in blaze's cold build | once a job | once a job: one `base_db` |
| `syn`, `quote`, `proc-macro2` analysed | once per proc macro | once a job | once a job |
| a shared function compiled | once per proc macro or build script reaching it | once | once (`blaze_codegen::flight`); release summaries still once per build |
| cores used | `ceil(cores/4).max(4)` processes, about 7 of 18 busy | all | all: the plane's work pool, rayon within a unit |
| a run's wait | a slot of the pool | no core | no core: `Task::Loop` |

Where v2 stands short, until a cold build measures it:

- **Release summaries** are prepared and summarized by each release build that reaches them,
  before compiles are claimed. Build-time code is Cranelift, so this costs only where std's or
  another O3 crate's functions are reached and the store has no summary.
- **Policy keys.** A unit's settings hash the whole policy id, so units under different
  per-package overrides compile `syn` once each.
- **A waiter's thread.** A build waiting on another's claim holds its plane worker (not a rayon
  thread) until the landing; the claimer is compiling on rayon, so cores stay busy, but a
  plane with fewer workers than waiting builds queues the rest of the plane behind them.
