---
status: plan
summary: "Where a tempo check spends its wall time, memory and I/O, measured from 37 scoreboards, and the redesign of each waste found: the Names schedule, the three long poles, records written by the coordinator, the answer's store, the edit's fixed costs, the no-op's."
date: 2026-11-20
---
# Utilization: CPU, memory and I/O (D153)

Every number is from a kept run on tempo, 16 workers on an 18-core Mac, the page cache warm:
`reports/util-1120/` in the notes (runs.csv, phases.csv, timeline.csv, lines.csv, FACTS.md) mined
from `reports/score-*`. The three runs quoted: green5 (c130517, before the 2026-11-19 wave),
all10 (c132652, today's green) and wave11 (c212351, the wave's first snapshot). A cause is
stated only where a profile row shows it; where none does, the item is a measurement first
(rule 17).

## What is measured

Cold check (`first`), wave11: 270.0 s wall, 1,103 s of CPU (4.1 cores of 16), 33.0 GB peak.

| part | wall s | task CPU s | cores | what the profile shows |
|---|---|---|---|---|
| Names (286 waves, Scripts beside) | 118.2 | 208.5 | 1.8 | wave 7: 53.7 s of wall for 54 ms of work; wave 16: 29.3 s for 72 ms; put 19.8 s |
| Heads (1 wave) | 21.2 | 255.1 | 12.0 | put 3.6 s |
| Bodies (1 wave) | 77.4 | 1,221.0 | 15.8 | merge 4.2 s |
| answer stored | 31.0 | | 1 | footprint 10.7 GB to 32.9 GB while it runs |
| store committed | 3.6 | | 1 | |
| the rest (finish, index, plan, open) | about 19 | | 1 | finish 2.1 s; index 21.5 M edges, 4.73 GB |

The slowest single tasks: `librocksdb_sys`'s build script 50.8 s; one call of
`tempo_precompiles_macros::storable_arrays!` 27.9 s; `crate::sol!` 22.2 s;
`alloy_sol_types::sol!` 19.5 s; `storable_nested_arrays!` 9.0 s.

History of the same three: Names wall 39.2 s (green5), 86.5 s (all10), 100.4 s (wave11) at
2.1 to 2.3 cores each; Heads 41%, 48%, 59% of 16; Bodies 98 to 102%; peak 32.6 to 33.2 GB in
all three, reached after the last body task is done.

One-body edit, wave11: 1.63 s wall, 1.17 GB peak.

| part | ms | what the profile shows |
|---|---|---|
| Bodies stage | 791 | the body's own check 35.9 ms; put 73.6 ms for a 180-byte record; 979 MB of records opened (1,204 records, asked again 96,285 times); member rows read 150 of 152; expansion text asked 1,107 times (611 keys) |
| plan (units read back) | 208 | 1,797 plan rows, 920 sources statted |
| Scripts | 144 | 4 build scripts entered, 2 identical: their reads are git facts |
| answer printed | 143 | |
| journal replayed | 91 | 398 event ids |
| sweep | 48 | 191 of 1,249 files statted |
| open checkout | 26 | 5 ms on all10 |

No-op, wave11: 0.29 s. journal replayed 139 ms (99,681 of 99,681 ids: the checkpoint did not
shorten it), sweep 59 ms (1,247 of 1,249 files statted), open checkout 45 ms (5 ms on all10),
store committed 38 ms (8 ms on all10).

I/O: no block was read or written by any of the three commands (the page cache holds
everything). What moves is memory: 9.98 M page reclaims in a cold check, 154 K in an edit;
Names tasks took 43.4 GB of scratch in all (2.32 GB the largest, `tempo_contracts`). Records
written cold: Names 980 MB and 1.29 GB of expansions (221,933 put, 115,426 of them equal to
one already there); Heads 353 MB; Bodies 26 MB; the answer 2.82 GB in 29,977 pages.
Voluntary context switches: 244,241 in wave11's cold check against 3,634 in all10's.

## The floors

- Cold: the longest chain of tasks that must follow one another, plus the tail. With today's
  poles that chain is at least the 50.8 s script and what depends on it; without them it is
  Bodies' 77 s (1,221 s over 16 cores). So the target is cold wall = max(longest chain,
  CPU / workers) + a tail under 2 s, and the poles are shortened one by one after that.
- Edit: the body's own check (36 ms here, itself to be measured) plus the fixed costs of D150.
  Target under 300 ms on tempo.
- No-op: the certificate read and the journal's new ids. Target under 30 ms.
- Memory, cold: workers x a task's scratch, plus what is mapped. Target under 8 GB.
  Edit: under 100 MB.

## The redesign, by what the numbers show

**U1. Names runs by readiness, not by wave.** Today the stage is 286 waves in order; a wave ends
when its slowest task ends, and nothing of the next wave starts before. One build script
holds 3 tasks' wave for 53.7 s while every crate that does not depend on it waits too. The
engine's rule that makes this safe is "a task runs after every task it reads from has merged":
that is a per-task condition, so the schedule is a ready set: a task is entered when the last
task it waits on merges; merges stay in one deterministic order (by name within a tick), so
serials and cells move as they do now. The engine's simulation holds the two schedules to equal
answers on every history; the control is a task entered before a dependency merged. Judge:
Names wall within 10% of its longest chain.

**U2. The poles are measured, then cut.** A 50.8 s build script and a 27.9 s macro call are P0
until a profile names where the time goes (rule 16): for the script, what it runs (a C++
compile is its own work, done once per machine by the shared store); for the proc macros on
wasm, whether it is the module's compile, its execution, or the tokens crossing. Judge: each
pole's span tree in the profile; then a target per pole.

**U3. A worker writes its own record.** Names spends 19.8 s and Heads 3.6 s in `put` on the
coordinator between waves. The record's key and bytes are the worker's; the store's put is by
content. The worker puts; the merge files the key. Judge: put ms on the coordinator under 1%
of the stage.

**U4. An expansion is made once.** Of 221,933 expansion records put, 115,426 were equal to one
already put: the library and its test build expanding the same calls. D152 (test.md) removes
the second crate key and with it the second expansion.

**U5. The answer is stored as it is made.** Storing the 2.82 GB answer takes 31.0 s on one
thread (6.8 s on all10: the wave made it 4.5 times slower, cause not yet profiled) and takes
the footprint from 10.7 GB to 32.9 GB: the whole peak of the command. Each stage's rows are
written to their pages when the stage merges, by the workers, and the rows leave memory then;
the read-edge index is written in the same pass at its floor (82 bytes an edge against 220
today, 21.5 M edges). Judge: no "answer stored" phase over 1 s; the peak is the Bodies stage's.

**U6. Memory after U5.** The Bodies stage ends at 10.7 GB. What holds it is measured by tag at
the peak before a design: `Api` live 25.7 GB at exit, Heads' 1.6 GB kept past its phase, 3.5 GB
of heap at the tail. Target 8 GB.

**U7. The edit's Bodies stage.** 755 ms around a 36 ms body. The stack sample of this stage on
the wave's binary comes first (`edit-sample.sh`); the rows above name suspects, not causes: a
body with no macro asks 1,107 expansion texts; 150 member rows are read to list one file; the
names records of 1,204 crates are opened. The floor is the body's own reads.

**U8. A script that reads git facts does not gate the bodies.** Four scripts are entered on
every edit (their reads move with the working tree's state) and run before Bodies; two come out
identical. By U1's rule a body waits only on what it reads, so they run beside it. 144 ms off
the edit's chain.

**U9. The edit's and the no-op's fixed costs (D150, not yet at their floors).** The journal
replay costs 228 microseconds an id on an edit (91 ms for 398 ids) and replays all 99,681 ids on
a no-op: the checkpoint written by D150 is not taken on tempo; find why from the journal's
profile line, fix, and bring an id's cost down. The sweep stats 1,247 files on a no-op the
journal already covers: zero is the floor. Plan read-back 208 ms and printing 143 ms are over
D149's floors: each gets its span rows. `open checkout` and `store committed` grew 5 to 9 times
in the wave: regressions to find before the wave is admitted.

**U10. Waits.** 67 times the voluntary context switches of all10 in the wave's cold check: a
blocking wait or a contended lock came in with the wave. A `sample` of the cold check names it.

**U11. Heads at 59%.** One wave, 12 of 16 cores: the profile's head-job lines say how the jobs
split; after U1 the stage is entered by readiness too.

## Order

Before the wave is admitted: U9's two regressions, U10, and U5's 4.5 times. Then U1 with U8
(the engine's schedule, simulated first), U3 and U5 (the store's writes), U2 (profiles, then
cuts), U7 (a sample, then the design), U6.
