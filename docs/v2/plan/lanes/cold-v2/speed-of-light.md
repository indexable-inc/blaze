---
status: plan
summary: lane cold-v2's bounds: a cold build is at best the larger of the download over the link and the work no one could do for this app (its own crates and the generic instances its code makes, only as far as it reaches) over the cores; the arithmetic, the measured inputs it needs, and where today's code stands.
date: 2026-10-30
---
# cold-v2: speed of light

## The bound of a cold build

A cold build must do, at least:

- **D**, the bytes of every public result the build uses, over the link. This is the bundle of
  the lock file's dependencies (fastcompile's `global.md`: images, checks, bodies, constants,
  goals, item records, library code, build-script and proc-macro runs), plus 4 round trips to the
  first byte.
- **W**, the work only this app can have: its members' front end and codegen, the generic
  instances only its code makes (each reached once), and its link. Spread over the cores.
- **C**, the critical path through that work. For a typical app this is the member chain's front
  end, then the walk's deepest level, then the link.

\[ T_{\text{cold}} \ge \max\left(4\,\text{RTT} + \frac{D}{\text{link}},\ \frac{W}{\text{cores}},\ C\right) \]

The download and the local work overlap. The bundle is fetched while planning and build scripts
run, and a dependency's results are needed only once the front end reaches it.

## Piece 2's part: what a dependency costs when it is not downloaded

Without a cache hit, a capped dependency crate of \(N\) bodies (\(F\) of them non-generic public
functions) used to cost:

\[ W_{\text{before}} = N\,(t_{\text{infer}} + t_{\text{borrowck}}) + F\,(t_{\text{mir}} + t_{\text{codegen}}) \]

Now it costs only what programs reach. Let \(r\) be the bodies reached (generic instances
counted per instance, as before):

\[ W_{\text{after}} = r\,(t_{\text{infer}} + t_{\text{mir}} + t_{\text{codegen}}) \]

The reached functions were compiled before too, by the program's walk or found compiled from the
library unit. So the saving is
\(N\,t_{\text{borrowck}} + (N - r)\,t_{\text{infer}} + (F - r_F)\,(t_{\text{mir}} + t_{\text{codegen}})\),
where \(r_F\) is the reached non-generic public functions.

Unit costs, from item-hash's speed-of-light: inference 50-500 µs a body. Borrow check is of the
same order, MIR building about a quarter of inference, and a dev (Cranelift) function about
100-300 µs.

The reached share \(r/N\) is the input that decides the saving, and it is measured, not assumed.
`harness/v2/reachable.py` times both cold builds (`seconds_full`, `seconds_reached` and their
ratio). It does not yet count the instances each mode compiles; `-v`'s `CodegenInputs` of the
two builds give \(r\) and \(F\). Linkers' dead-stripping of Rust binaries suggests a typical reached share of a
dependency's code well under half. If the share is 30%, the dependency share of front-end and
codegen work falls by roughly 70% (borrow check of dependencies to zero).

The decision itself is \(O(\text{env})\) per unit: one scan of the job's variables for `BLAZE_DEPS`,
and one package lookup already made for the unit's lint flags. It allocates nothing and runs once
per unit.

**At the bound?** For a capped crate, the work left is exactly the reached bodies, each inferred,
lowered and compiled once (the shared memo dedups across units), which is the minimum without a
download. The one remaining excess is name resolution: a crate's def map is resolved whole, since
an item's resolution reads its module's scope and imports. It is downloaded as the image (piece
1), so on a cold build with the cache it costs a map, not a resolution.

## Piece 1's part

With the bundle, a dependency costs its bytes over the link and one BLAKE3 pass in the store
(0.17 ns/B a core). That is fastcompile's `global.md` bound: link-bound on 1 and 10 Gbit/s links.
A front-end read of a fetched value is a mapped read. The size of \(D\) is unmeasured until the
builders publish (request 00844). For a feel: ripgrep's dependencies' images, outcomes and dev
code are some tens of MB, about 0.1 s at 10 Gbit/s, against cargo's 48.6 s cold `bench.py cargo`.
So piece 1 makes \(W/\text{cores}\) of the app's own code the bound.

## Against the state of the art

| system | what it skips | here |
|---|---|---|
| rustc | compiles every non-generic public item of every rlib; checks every body of every dependency | neither, for capped dependencies |
| linker `--gc-sections` / `-dead_strip` | drops unreached code after it was compiled | it is never compiled |
| Bazel / sccache remote caches | whole compiler invocations per crate, when the command line matches | per-result keys (image, check, body, function), so one changed feature or dependency misses only what it reaches |
| Nix binary caches | prebuilt packages | prebuilt per-result values of a lock file, one signed bundle |
