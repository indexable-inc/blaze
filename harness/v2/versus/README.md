# versus: blaze against Relax on one machine (D21)

`harness/v2/versus.py` builds the same projects with blaze and with makepad's Relax on one
machine, each tool on its own copy with its own fresh state (blaze: `BLAZE_HOME=<root>/blaze-home`;
Relax: `MAKEPAD_RUN_HOME=<root>/relax-home` and the copy's `target/`), under HOME, PATH and
TMPDIR only. Fixtures: `hello` (no dependency) and `six` (regex, hashbrown, bitflags, itoa,
memchr, smallvec, pinned here with their Cargo.lock) and `bstd-hello` (hello on blaze's own
std, crates/blaze-std of `tree`; only blaze builds it, so it is judged against Relax's `hello`
rows, and its rows say so); `extra` adds real checkouts (reth, tempo).

Rows, per project and tool, n samples after two warm-ups, every run's exit status and printed
line checked: cold (state deleted, 3 runs), no-op run, comment edit + run, body edit + run,
restart + no-op run (blaze's daemon stopped and seen gone; Relax keeps no process, so its no-op),
held between runs (blaze's daemon a second after each body edit, and beside it blaze's own peak of
one no-op run from `/usr/bin/time` with no daemon before it; Relax 0, beside it one no-op run's
peak from `/usr/bin/time`), on disk (blocks, not lengths, under each tool's state, plus the
growth of its copy and of the shared `~/.blaze`). Two slow rows take 3 samples each: idle 45 s
(the daemon 45 s after the fifth of five body edits, judged as held) and body edit after 60 s
idle (judged as body edit). blaze's no-op and restart rows and its one-run peak record
`daemon_after` (whether `serve.lock` in `<BLAZE_HOME>/v2` has a holder after the run: a shared
try-lock of it fails) and after how many samples, and the table shows it.

Example input (`python3 -I harness/v2/versus.py '<json>'`, or a file path, or `-`):

    {"blaze": "/Users/me/inst/blaze", "relax": "/tmp/relax-src/relax/target/release/relax",
     "root": "/private/tmp/versus", "n": 20, "factor": 3.0,
     "tree": "/Volumes/Projects/blaze-int-1112",
     "extra": [{"name": "tempo", "dir": "/Volumes/Projects/tempo", "package": "tempo",
                "edit": {"file": "bin/tempo/src/main.rs", "find": "x", "replace": "y"},
                "expect": null}]}

How a row is judged (one median a tool, no retry, no sample dropped; the table shows the range):

- timed rows: won when Relax's median is at least `factor` times blaze's;
- held and idle 45 s: won when blaze's idle daemon is under Relax's peak for one no-op run, footprint against
  footprint on macOS, resident against resident on Linux;
- on disk: won when blaze's bytes are under Relax's;
- an `extra` a tool cannot build: that tool loses each row, its exit status and last 20 lines
  are the record, and the other tool's numbers stand.

Output: `<root>/versus.json` (every sample, the machine, both versions, the date) and the table.
Exit 0 when every row is won, 1 when any is lost, 2 when a fixture run failed.
