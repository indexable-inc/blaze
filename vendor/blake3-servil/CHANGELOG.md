# blake3-servil 0.8.0+b3c5efc7c975d903762dea809066f7c0a0603861, against official BLAKE3 1.8.7

The same digests. Faster than official BLAKE3 1.8.7 on 64-bit Arm and
x86-64. Every kernel is assembly, and every one is proved correct
within its stated contract. Everything else is tested, and the tests
are tested ([QUALITY.md](QUALITY.md)).

## Faster

Each figure is blake3-servil's speed over official BLAKE3 1.8.7's, both
measured in the same run by [bench-hashes](https://github.com/johnservil/bench-hashes)
0.18.1 at this release's commit (one run per machine, October 27).
On the every-core rows each uses its multithreaded function:

| | Intel i7-12700K | AMD EPYC 9255 | Apple M4 Max | Linux VM on the M4 Max |
|---|---|---|---|---|
| One message of 64 B | 0.8× | 1.1× | 1.5× | 1.1× |
| One message of 1 KiB | 1.1× | 1.1× | 1.2× | 1.2× |
| One message of 1 MiB, one thread | 1.0× | 1.2× | 2.5× | 2.5× |
| One message of 128 MiB, every core | 1.1× | 1.0× | 1.3× | 1.3× |
| A stream of 64 KiB messages, every core | 8.0× | 7.2× | 33× | 156× |
| A batch of 1024 messages, 64 B each | 1.1× | 1.1× | 1.9× | 1.8× |
| b3sum over 1000 files of mixed sizes | 3.4× | 5.8× | 4.6× | 26× |

On the M4 Max and in the VM, official's multithreaded call on each 64 KiB
message takes longer than its one-thread call (1.08 and 7.13 ns per byte,
against 0.42 and 0.43), so the stream row's figures there are large.

On 64-bit Arm, new kernels make every size faster: SME2 on the M4 and
later, and kernels that run the integer and NEON units side by side.
On x86-64 with SSE4.1 or AVX2, a single message hashes with official's
own assembly (each proved below), so it runs at official's speed (one
message of 64 B varies from run to run: 0.8× to 1.9× on the i7). On Linux on AMD Zen 5 and later,
new kernels hash 32 whole chunks or 16 parent nodes at once with AVX-512
(proved too): on the EPYC 9255, one thread hashing 32 KiB or more takes
about 20% less time than with upstream's 16-input AVX-512 kernel. Other
AVX-512 CPUs keep the 16-input kernel, which measured faster there (Zen 4:
the wide kernels 4-5% slower from memory; Intel Ice Lake: 3-4.5% slower
throughout). The other gains on x86-64 come
from many messages at once: a stream that hashes on other cores while
the program reads the next input, batches of short messages, and b3sum
over many files.

## Checked more

Beyond official's tests (published test vectors and a reference
implementation), this crate adds:

- **Formal proofs of every assembly kernel**, official's own x86-64
  assembly among them, at every shape this crate calls it: symbolic
  execution against a specification of BLAKE3, with memory accesses,
  the calling convention and secret-dependent branches checked too.
- **Mutation testing**: one small mistake planted at a time, more than
  2,000 in all, and a test must fail for each; every one that survives
  is listed with its reason.
- **A model checker (Shuttle)** for the thread pool's waits, under
  thousands of thread schedules.
- **Guard pages**: tests put an inaccessible page beside every input,
  output, key and context of every public call, in Rust and in C, so
  a call that touches one byte past the caller's memory crashes the
  test.
- **A startup self-test** of the build and the CPU before the first
  hash.

These found eight bugs before release, among them the AVX2 and AVX-512
batch kernels reading past an input's end at shapes this crate's batch
calls use (fixed here; see below). [QUALITY.md](QUALITY.md) says what
each check covers and what remains unproved.

## Before you switch

1. Targets: 64-bit Arm on Linux and macOS, x86-64 on Linux. Latest
   stable Rust and a C compiler, GCC 12 or Clang. Official BLAKE3 keeps
   Windows, 32-bit CPUs, big-endian CPUs, WebAssembly and older compilers;
   a build for any other target stops and names it.
2. Call `blake3_servil::initialize()` once before any other call. A
   program that makes multithreaded calls or uses a stream calls
   `initialize_multithreaded(None)` instead. A call before it panics.
3. Two kinds of call, in two modules. `convenience`: one call per hash,
   every feature, any slice, the result returned: `hash(input)` is now
   `convenience::hash(input)`, `keyed_hash(key, input)` is
   `convenience::hash_with(Mode::Keyed(key), input)`, `derive_key(context,
   material)` is `convenience::hash_with(Mode::DeriveKey(context),
   material)`, and `convenience::hash_xof(mode, input)` gives extended
   output. `performance`, for power users, has every way to hash fast.
   There the program takes on more of the work: inputs as `Block`s (64
   bytes on a 64-byte boundary: a message is blocks and a length, a
   batch messages of one length), digests into its places, and a `Mode`
   in every call. The `Hasher` is there too: `Hasher::new(mode)` replaces
   `Hasher::new()`, `Hasher::new_keyed` and `Hasher::new_derive_key`, the
   plain hash is `Mode::Plain`, and `Hasher` has no `Default`.
4. Threads are the crate's own. `Hasher::update_rayon` is now
   `update_multithreaded`. `performance::hash_multithreaded(mode, input)`
   hashes one message over the threads. There is no `rayon` feature.
5. Removed: `Hasher::update_mmap` and the `mmap` feature (map the file
   yourself, or use `update_reader`), the `traits-preview` feature, the
   `guts`, `hazmat`, and `platform` modules, and the `no_avx2`,
   `no_avx512`, `no_neon`, and `no_sme2` features, and `pure`: this crate
   runs its assembly or not at all. For portable Rust, or an x86-64 CPU
   without SSE4.1 (which stops at `initialize`), use official BLAKE3.
6. b3sum installs from the package `b3sums` (`cargo install b3sums`);
   crates.io's `b3sum` is official BLAKE3's. The program is still `b3sum`.
   `--num-threads N` means at most N threads in all, and 1 means one
   thread. `--no-mmap` does nothing. Every file is read to its end. A
   broken pipe ends b3sum quietly.

## New

- `performance::hash_many` and `hash_many_multithreaded`: a `Batch` of
  messages of one length, each in `Block`s (64 bytes on a 64-byte
  boundary). A `Batch` lets the program write only its messages' bytes,
  so the bytes past each message, which the batch kernels read in place,
  stay zero; a Merkle layer writes its digests straight into the next
  layer's `Batch`.
- `performance::hash_each` and `hash_each_multithreaded`: a collection of
  any lengths.
- `performance::update_each` and `finalize_each`: many messages in
  progress at once, each with its own `Hasher`.
- `performance::stream()`: inputs one after another, hashed behind the
  caller.
- A C API in `capi/b3.h`. The library is `libb3_servil`. C11 and C++11.
- `Mode::DeriveKeyWith` with `performance::context_key`: one context
  hashed once for many derivations.

## Different

- A startup self-test checks the build and the CPU. A wrong digest stops
  the process, uncatchably: with `std` it names the code path and aborts;
  without it, the CPU's trap instruction.
- A panic on one of the crate's threads, or in a caller during a
  multithreaded call, ends the process.
- The threads are the process's: in a child forked after they started,
  every multithreaded call and stream panics, saying so. A program that
  forks calls `initialize_multithreaded` in the process that hashes,
  after any fork; single-threaded calls work anywhere.
- A `Hasher` is about 18 KiB. It gathers short updates and hashes them
  side by side, so a message arriving in small pieces hashes as fast as
  one in a buffer.
- The AVX2 and AVX-512 batch kernels read nothing past an input's end.
- The crate leaves Arm's data-independent timing (DIT) as the calling
  thread has it. A program that wants Arm's promise turns DIT on in its
  own threads before it hashes (the quality manual, "Timing and secrets":
  [docs/quality-manual.md](docs/quality-manual.md)).
- Building the Arm kernels needs an assembler that knows armv8.2-a+sha3,
  GNU as 2.30 or LLVM 7; building the x86-64 kernels needs a C compiler
  that takes the AVX-512 flags, GCC 12 or Clang. Without one, the build
  stops and says so; official BLAKE3 builds without assembly. The SME2 kernels need Clang 17
  or GCC 14 with binutils 2.41; without those the build warns and leaves
  them out.
