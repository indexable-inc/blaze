# Hermetic tests by default: how many, why not, and what it costs (lane hermetic, #836, #901)

`blaze test` keeps a hermetic test's pass by the hash of the code it reaches and runs every other
test every time (#836). This measures, on ripgrep, fd and just, how many tests are hermetic, what
keeps the others running, and what the default costs against main, main before #836 and
`cargo test`.

ix VM, x86_64, 32 vCPU. Four sides, each with its own checkout, target, daemon home and store:

- **branch**: #910 (this directory's PR).
- **main**: c01b89c486, #836 merged.
- **pre836**: a7b7d9dd1a.
- **cargo**: stable cargo 1.99.0. Nightly cargo puts test executables where ripgrep's, fd's and
  just's integration tests can't find the program they run.

blaze builds with nightly 1.101.0 (2026-10-26) and cranelift. The projects are ripgrep 15.1.0
(af60c2d), fd 10.3 (d38148f) and just 1.43.0 (b7e7c46). `measure.sh` is the script and
`times.txt` its raw output.

## How many are hermetic, and why the rest are not

From `blaze test --why` on the branch (`*.summary.json`; regenerate `tests.ndjson`, 2 to 16 MB,
with `blaze test --workspace --why --json`):

| project | hermetic | top reasons (tests each blocks) |
|---|---|---|
| ripgrep | 363 / 1144 (32%) | env `getenv` 371; global-state `integration::util::NEXT_ID` 329; process `posix_spawnp` 329; fs `readlink` 328; global-state `log::MAX_LOG_LEVEL_FILTER` 264 |
| fd | 24 / 237 (10%) | ffi `syscall` 195; env `getenv` 102; indirect getrandom's `fill_inner` 102; global-state `GETRANDOM_FN` 100 |
| just | 31 / 1443 (2%) | process `posix_spawnp` 1199; indirect yansi's `AtomicCondition::read` 1113; env `environ` 880; global-state regex-automata's pool `COUNTER` 871 |

On main, ripgrep kept 191 of 1144: memchr's runtime dispatch (`memchr_raw` through a
transmute, indirect, 846 tests) blocked most of its unit tests. #910 takes a block-local
`AtomicPtr` that only ever holds a function's address as a function chosen once by the CPU
(`docs/cargo.md`).

The next levers:

- **getrandom's `GETRANDOM_FN`.** It is the same shape as memchr's, but it is a module-level
  static, so the rule does not take it.
- **`log::max_level`.** 161 tests on ripgrep are blocked only through it.
- **The integration tests.** They spawn the program they test, so they are not hermetic.

## Second run: `blaze test --workspace` against `cargo test --workspace`

Medians in ms (min and max in `times.txt`). Warm (nothing edited) is 8 interleaved rounds. After
an edit is 7 rounds, each after a fresh leaf edit (`std::hint::black_box(N)`) in a library every
test reaches.

| project | branch | main | pre836 | `cargo test` |
|---|---|---|---|---|
| ripgrep warm | 4834 | 4792 | 4797 | 4085 |
| ripgrep edit | 8842 | 8703 | 9115 | 7106 |
| fd warm | 4308 | 4035 | 4638 | 504 |
| fd edit | 4243 | 3789 | 5012 | 1250 |
| just warm | 790 | 776 | 752 | 1070 |
| just edit | 2415 | 2948 | 3413 | 5135 |

- **The regression #901 was about** is the walk after an edit. `Store::get` reads only sealed
  packs, so the summaries one walk stored were invisible to the next walk in the same daemon. On
  just, the walk after an edit took 13.4 s. #910 keeps the summaries in the daemon and seals the
  store after a walk that added many, so it takes 0.26 s, and 0.17 to 0.55 s right after a fresh
  daemon.
- **fd and ripgrep** are within noise across branch, main and pre836. Their gap to cargo is not
  in the tests: a warm no-op `blaze build --workspace` on fd takes 1.35 s, and `blaze test` reports
  2.06 s before any test runs. That is the no-op build path, from before #836.
- **ripgrep's warm gap** is mostly its doctests (8 crates, 0.2 to 0.5 s each) and
  `grep_searcher`'s tests (1.3 s).
- **Fresh daemon on just**: the first command takes 21 to 66 s of load, the same before #836.
