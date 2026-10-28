# Build UI: what the build events cost (2026-10-28)

Lane tui, [#878](https://github.com/indexable-inc/blaze/pull/878). The daemon now publishes
every build as deterministic CBOR records (`blaze_protocol::events`), and `blaze` renders them
(a live region on a terminal, cargo-like lines when piped, `--events cbor|json`, `blaze why`,
`blaze watch`, `blaze status`). This measures what that costs a build next to the same tree
without it.

## Method

GitHub Actions `ubuntu-latest` (x86_64, AMD EPYC 9V74, 4 cores, 16 GiB, rustc nightly
2026-10-27; run 40648624282 on a throwaway probe branch), because ix VMs could not be made
(arm64: "streams volume not served", x86_64: memory_full). Both arms were built in the same job:
*before* is main at the merge base (`27c6d0ba86`), *after* is the change (`c98594fe84`). Each
arm runs with its own `BLAZE_HOME`, so each has its own daemon.

- `bench.py`: anim (the repo's example) per arm: one cold build, 3 warm-ups, then 30 no-op
  builds alternating between the arms, then 10 body edits per arm. The output (`report.json`) is in `anim.json`.
- ripgrep 15.1.0 through `harness/bench.py` (cranelift, 5 runs, 1 cold), one arm after the
  other. The output is in `ripgrep-before.json` and `ripgrep-after.json`.
- `captures.sh`: ripgrep cold and then a body edit under `script` (100 columns), the same edit
  piped, `blaze why`, `blaze why search`, and a no-op with `--events json`. `render.py` plays the
  terminal captures back as text (`captures/cold.txt`, `captures/edit.txt`).
- `profile.sh`: the stats, `strace -f` of the client, and of the daemon and worker, for an anim
  no-op in each arm.

## Numbers

anim, ms (wall time of `blaze build` as the user runs it):

| | before | after | difference |
|---|---|---|---|
| no-op, median of 30 | 5.53 | 5.68 | +0.15 |
| no-op, p90 | 5.78 | 5.97 | +0.19 |
| body edit, median of 10 | 10.83 | 11.22 | +0.39 |
| cold | 22 528 | 22 332 | −196 |

ripgrep, s (cargo in brackets, same machine):

| | before | after |
|---|---|---|
| cold | 24.34 (7.03) | 24.66 (7.00) |
| no-op | 0.011 (0.029) | 0.012 (0.030) |
| body | 0.141 (1.356) | 0.142 (1.354) |
| signature | 0.216 (1.686) | 0.217 (1.685) |
| leaf | 0.131 (1.926) | 0.130 (1.911) |
| generic | 0.137 (1.923) | 0.134 (1.903) |
| struct | 0.183 (1.708) | 0.185 (1.680) |
| newfn | 0.164 (1.367) | 0.167 (1.348) |

A no-op costs 0.15 ms more at the median, which is under the 0.2 ms budget. The ripgrep edits
are within the run-to-run noise, and a single cold sample (+0.3 s of 24 s) is too. The cold
ripgrep build is slower than cargo with or without the change. That cost is in loading and
compiling, not in the events, and it is not this lane's.

The first try of the same bench (run 40642151590) cost a no-op +0.88 ms. Its strace showed a
relay thread for each request, a reader thread in the client, and a write for each record. The
change was rewritten so that one daemon thread relays every waiting command, the records and
answer of a quick build go out in one write, and the client reads with a timeout instead of
starting a reader thread. The client's syscalls for a no-op went from 369 to 373, and the daemon's and worker's from 261 to 346. The
daemon's records put an anim no-op's Began to Finished at 0.67 to 0.83 ms.

## What a build shows

The cold ripgrep build on a terminal (every 25th frame the live region drew, then what it
left), from `captures/cold.txt`:

```
--- frame 0
     Waiting for the daemon 0.10s
--- frame 25
     Loading workspace 2.59s
--- frame 200
   Compiling rg 0.20s
--- frame 250
   Compiling rg 5.03s · 0/1 crates
            └─ rg  17240 fns  4.62s
--- frame 300
    Checking 74 files 0.47s · 0/1 crates
            └─ rg  31894 fns  9.27s
--- left on the screen (312 frames drawn)
   Compiling ripgrep v15.1.0 (/tmp/…/ripgrep)
      Loaded workspace in 18.8s
    Resolved names in 1.41s
    Compiled rg: 31848 fns compiled, 0 kept in 9.43s
     Checked 74 files in 0.63s
      Linked rg in 0.63s
    Compiled 31848 fns in 1 crate, 0 kept; 30.9s
    Finished `dev` profile [blaze] target(s) in 30.95s
```

The region redraws every 0.1 s, so it is never silent for more than a second. One body of
`search` edited, on a terminal:

```
     Rebuilt 1 fn: search (crates/core/main.rs edited); 0.17s
   Compiling ripgrep v15.1.0 (/tmp/…/ripgrep)
    Finished `dev` profile [blaze] target(s) in 0.20s
```

Piped (`captures/edit.piped`):

```
   Compiling rg
     Linking rg
     Rebuilt 1 fn: search (crates/core/main.rs edited); 0.13s
   Compiling ripgrep v15.1.0 (/tmp/…/ripgrep)
    Finished `dev` profile [blaze] target(s) in 0.13s
```

`blaze why`, then `blaze why search`:

```
the last `blaze build` that compiled anything took 0.13s
  search (rg): crates/core/main.rs edited
  rg: 31848 fns, 2 compiled, 31846 kept
```

```
  search (rg): crates/core/main.rs edited
```

`blaze build --events json` (`captures/noop-events.jsonl`) prints one record per line:

```
{"at_ns":0,"event":{"Began":{"build":4,"command":"build","root":"/tmp/…/ripgrep"}}}
{"at_ns":92884376,"event":{"Finished":{"ok":true,"summary":{"took_ns":92884376,"crates":0,"fns":{"total":0,"done":0,"kept":0},"rebuilt":[],"more":0,"error":null}}}}
```

That no-op came straight after the piped edit build and the two `blaze why`. Its 93 ms is
spent in the worker (Finished's time is the worker's), not in sending or rendering. Why the
first no-op after an edit takes that long is not known, and the before arm was not measured in
the same position. The harness's ripgrep no-ops take 12 ms in both arms, and a clean anim
no-op's records span 0.67 to 0.83 ms (`profile.sh`).
