# How blake3-servil is checked

blake3-servil is a fast BLAKE3 implementation. It aims to produce
exactly the official digests.

Most of the extra speed comes from code written for specific processors
and from splitting work across processor cores. That is also where
mistakes are most likely, so the code is checked in several different
ways.

## Proofs of the kernels

The fastest parts are small routines called kernels, every one of them
written in assembly.

For the assembly kernels, the project uses formal verification:
symbolic execution compares the assembled instructions with a written
specification of BLAKE3. Each completed proof covers every input value
within its stated contract. The checks also examine memory accesses,
the calling convention, and whether input bytes or keys affect
branches or addresses.

The Arm kernels are covered at every block count they support. The
SME2 kernels process data in groups of sixteen chunks, and their
proofs cover any number of groups. In those proofs, each group's bytes
are separate symbols. So if a kernel read the wrong group's bytes, its
output would not match the specification, and the proof would fail.

The Intel and AMD assembly kernels are checked the same way. The
fork's own kernels there are generated and proved for every input. One
hashes whole chunks and a partial one in one pass, and one hashes a
message of one chunk or less to its root in one call. On AMD Zen 5 and
later, the wide AVX-512 kernels hash 32 chunks or 16 parent nodes at
once, proved at every block count from 1 to 16. Single
compressions and extended output at 1 to 32 blocks are proved. For
batch hashing, an induction proof covers every input count in the
stated range for each instruction set, at block
lengths 1 to 16 with a fixed counter and at 16 blocks with an
incrementing counter. Each proof uses one symbolic entry-stack
alignment covering the four legal ABI alignments.

Two further checks test the prover itself. The instruction models are
run against real processors on random states, including memory
instructions and deliberate unaligned-address controls. We also plant
mistakes in the assembly (a wrong stride, a wrong rotation, a loop
that cannot finish, a write into the caller's stack) and require the
proof to reject every one.

The proofs cover every kernel. The Rust code that prepares work and
combines results is covered by tests.

## Tests, and tests of the tests

Everything else is tested.

The test suites compare outputs with published BLAKE3 examples, called
test vectors, and with fixed answers that official BLAKE3 generated.
They run these checks on the operating systems and processor types the
project supports.

The project also uses mutation testing to check the tests themselves.
It introduces one small mistake at a time (changing a comparison, say,
or skipping a step) and requires at least one test to fail. If no test
catches the newly introduced bug, the developers add a test to catch
that bug, or figure out why it isn't actually a bug.

Each program also runs a startup self-test. Before doing real work, it
hashes a few inputs with known answers. This can catch a broken build
or a processor that does not execute the code as expected.

## Memory and thread safety

Some performance-critical Rust code is `unsafe`: there the programmer
keeps Rust's memory rules, which the compiler checks everywhere else.

- **A model checker (Shuttle)** runs the small pieces of threaded code
  where one thread waits for another under thousands of thread
  schedules. A wait that could hang shows up immediately as a deadlock
  with the schedule that caused it.
- **Guard pages** cover every call. Tests place an inaccessible page
  of memory immediately beside each input, output, key and context of
  every public call, in Rust and in C. If the code reads or writes even
  one byte outside the caller's memory, the processor faults and the
  test crashes. This reaches inside the assembly, where the other
  checks cannot see.

## What the checks found

The checks found eight bugs. All are fixed. Examples:

- an Arm kernel produced the wrong digest for the shortest messages;
- a function silently ignored a partial final block of input;
- a reading helper could lose bytes it had already read when a later
  operation failed;
- Miri found an unsafe pattern in code shared between threads;
- batch assembly on Intel and AMD processors read beyond the end of
  an input, which could crash a program.

The proofs have not found a bug in the Arm kernels. They did find
mistakes in some of the tools and models used to construct the proofs.

## What remains uncertain

No human has carefully reviewed every line, and nobody outside the
project has audited the code. The proofs trust the written
specification, the instruction models, the automated prover (Z3),
and each kernel's stated
memory and calling convention. Tests cover the Rust code around the
assembly. Nothing checks that the `unsafe` Rust keeps Rust's rules on
shared memory (aliasing) under every thread interleaving. Miri
checked this until 0.8, on Rust kernels users never ran, and found one
such bug. These checks reduce risk, but they cannot guarantee that no bug
remains.

## Run the checks

```sh
git clone -b servil https://github.com/johnservil/BLAKE3
cd BLAKE3
cargo nextest run --release --lib --tests   # with cargo-nextest (https://nexte.st)
```

The [quality manual](docs/quality-manual.md) gives details for
experts and maintainers, including the limits of each check and how
to run the proofs.
