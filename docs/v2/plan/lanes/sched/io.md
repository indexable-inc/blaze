---
status: plan
summary: lane sched's inventory (2026-10-31) of every I/O call reachable from job code (a plane task's body on a pool worker, and the queries it calls), path:line with its owner lane, and what each becomes when a job may do no I/O and asks the driver instead.
date: 2026-10-31
---
# sched: I/O reachable from job code

Andrew, 2026-10-31 18:31: no I/O inside a job. A job that needs a file, a process exit or an
output written makes a request to the driver (io_uring on Linux, kqueue or one I/O thread on
macOS) and suspends on its completion the same way it suspends on a query. This is the list
of what a job does today, so that each site has an owner and a replacement.

**Job code** is what runs inside `plane::Work::run` for `Step<S>`
(`crates/blaze-cargo/src/serve/job.rs:247`) on a pool worker: `Steps::library`, `plan`,
`fetch`, `lower`; `Front::compile`, `ub_tests`, `evaluate`, `reach`, `codegen_inputs`,
`tables`, `edit`, `ahead`, `pack`, `doc`; `verify` and `trace`. It also includes every query
those call. `crates/blaze-cargo/src/serve/work.rs:2-20` says which steps run on a worker and
which on the loop. The loop's own I/O and the driver's are listed apart at the end.

Counted on 2026-10-31 with `rg` over each crate's non-test sources, then read site by site.
Line numbers are as of that date. Owners are from `docs/v2/plan/reference/crates.md`. Clock
reads are not I/O and are left out.

## Summary

| What | Where | Owner | Becomes |
|---|---|---|---|
| Host file reads in `Front::compile` (profile-use file, native libraries) | `blaze-cargo` serve/unit/codegen.rs | cargo | a driver read (`Effect::ReadAll`) and `Wait::Key` |
| Linker toolchain discovery: `cc`/`xcrun`/`vswhere`/`reg` runs, sysroot and SDK reads, `-l` search directory listings, linker scripts, archive sniffing | `blaze-link` target.rs | link | discovery once per daemon as an input (it is cached per process already); per-link reads and listings as driver requests |
| External assembler (`asm!` via `cc` when LLVM is absent or `BLAZE_ASSEMBLER=cc`) | `hir-ty-codegen` asm.rs | codegen | a driver spawn (`Effect::Spawn`) and its files as driver requests |
| `include_bytes!`/`include_str!` of a path outside the VFS | `base-db` files.rs | db | a VFS input (mounted by the loop), never a read in a query |
| Proc-macro "OS" file opens, readlink, readdir | `blaze-macro-eval` os/files.rs | usercode | driver reads; the macro's thread suspends on them |
| Rustup toolchain directory scan in `sysroot()` | `blaze-cargo` toolchain.rs | cargo | an input fixed at daemon start (and see own-nightly below) |
| std `library/` fetch, copy and digest (`Steps::library`, the IO pool) | `blaze-cargo` toolchain/library.rs, serve/unit.rs | cargo | driver requests: HTTP, unpack, digest |
| Crate and git fetches (`Steps::fetch`, the IO pool) | `blaze-cargo` serve/fetch*, `blaze-registry` | cargo | driver requests: HTTP, `git` spawn, file reads |
| Store reads (`blaze_cas::Reader::get`/`has`/`pinned`) | many in serve/unit | store, cargo | none: the reader reads mapped pack memory, which the driver maps (`Effect::Map`); a page fault is the only wait (see below) |
| Env-gated debug prints to stderr | hir-ty, hir-ty-borrowck, hir-ty-mir, hir-def, hir-ty-codegen | db, items, codegen | events to the loop (the profiler's stream), never a write from a job |
| Unconditional stderr from a job (LLVM warnings, macro warnings, library progress, registry lock wait) | see the tables | codegen, usercode, cargo | diagnostics or events returned to the loop |

## Job I/O by owner

### cargo (`crates/blaze-cargo/`, `crates/blaze-registry/`)

| path:line | call | what it does | task |
|---|---|---|---|
| `crates/blaze-cargo/src/serve/unit/codegen.rs:306` | `std::fs::read` | reads the `-C profile-use` file | `Front::compile` |
| `crates/blaze-cargo/src/serve/unit/codegen.rs:964-967` | `std::fs::read` | reads a native library's bytes | `Front::compile` |
| `crates/blaze-cargo/src/serve/unit/codegen.rs:948-951` | `blaze_usercode::trace::Inputs::file` | stats and hashes native library paths | `Front::compile` |
| `crates/blaze-cargo/src/serve/unit/codegen.rs:209`, `:843` | `toolchain::sysroot`, `unstable::runtimes` | finds the sysroot and sanitizer runtimes on disk | `Front::compile` |
| `crates/blaze-cargo/src/serve/unit/codegen.rs:839-845` | `blaze_link::target::output`/`locate` | resolves native libraries on disk (link's sites below) | `Front::compile` |
| `crates/blaze-cargo/src/serve/unit/compile.rs:296` | `blaze_usercode::trace::Inputs::file` | stats and hashes machine paths for freshness | `Front::compile` |
| `crates/blaze-cargo/src/toolchain.rs:229-234` | `std::fs::read_dir` | scans rustup's `toolchains/` for the sysroot | `Front::compile` via `sysroot()` |
| `crates/blaze-cargo/src/serve/unit.rs:156` | `toolchain::library::fetch` | locates or downloads std's `library/` | `Steps::library` (IO pool) |
| `crates/blaze-cargo/src/serve/unit.rs:161`, `:167` | `read_dir`, `metadata`, `read` | fingerprints and reads std's sources | `Steps::library` |
| `crates/blaze-cargo/src/toolchain/library.rs:136-137` | `current_exe`, `canonicalize` | resolves blaze's own path | `Steps::library` |
| `crates/blaze-cargo/src/toolchain/library.rs:205-206` | `create_dir_all`, `File::create` | takes the install directory's lock | `Steps::library` |
| `crates/blaze-cargo/src/toolchain/library.rs:218`, `:250` | `eprintln!` | progress to stderr | `Steps::library` |
| `crates/blaze-cargo/src/toolchain/library.rs:222-270` | `remove_dir_all`, `create_dir_all`, `rename`, `read_dir`, `symlink`, `copy` | stages and copies the library tree | `Steps::library` |
| `crates/blaze-cargo/src/toolchain/library.rs:276-296` | `std::fs::read`, `HyperHttp::get_async` | reads or downloads the library tarball | `Steps::library` |
| `crates/blaze-cargo/src/toolchain/library.rs:303-328` | `read_dir`, `read_link`, `File::open` | digests the library tree | `Steps::library` |
| `crates/blaze-cargo/src/serve/fetch.rs:76` | `std::fs::read` | reads a `file://` crate | `Steps::fetch` (IO pool) |
| `crates/blaze-cargo/src/serve/fetch.rs:151-156` | `Retry::run` → `Http::get` | HTTP GET of a crate or index | `Steps::fetch` |
| `crates/blaze-cargo/src/serve/fetch/git.rs:48` | `blaze_registry::git::fetch` | runs `git` (registry's sites below) | `Steps::fetch` |
| `crates/blaze-cargo/src/serve/fetch/git.rs:71-84` | `remove_dir_all`, `create_dir_all` | makes and removes a scratch `$CARGO_HOME` | `Steps::fetch` |
| `crates/blaze-cargo/src/serve/fetch/git.rs:117-139` | `read_dir`, `read_link`, `read`, `metadata` | reads the checkout into memory | `Steps::fetch` |
| `crates/blaze-registry/src/download.rs:153-247` | `read`, `remove_file`, `metadata`, `create_dir_all`, `OpenOptions` | caches and unpacks a `.crate` | `Steps::fetch` |
| `crates/blaze-registry/src/download.rs:182-188` | `http.get` | downloads a `.crate` | `Steps::fetch` |
| `crates/blaze-registry/src/lib.rs:121-152` | `create_dir_all`, `File::create`, `rename`; `eprintln!` at `:132` | atomic cache writes, the package lock, "blocking file lock" | `Steps::fetch` |
| `crates/blaze-registry/src/registry.rs:282-382` | `read`, `remove_file`, `create_dir_all`, `metadata` | index cache | `Steps::fetch` |
| `crates/blaze-registry/src/git.rs:38-616` | `fs::*`, `Command::new("git")` | git fetch and checkout | `Steps::fetch` (git) |
| `crates/blaze-registry/src/sparse.rs:133-172` | `read`, `create_dir_all` | sparse index config | `Steps::fetch` |
| `crates/blaze-registry/src/http.rs:167` | hyper TLS GET | the HTTP transport | `Steps::fetch` |
| `crates/blaze-registry/src/auth/process.rs:18` | `Command::new` | runs a credential helper | `Steps::fetch` (auth) |
| `crates/blaze-registry/src/directory.rs:116-170` | `read_dir`, `read_to_string`, `read` | scans a local directory registry | `Steps::fetch` |

`Steps::plan`, `Steps::lower`, `Front::edit`, the script analysis in `Front::compile`
(`crates/blaze-cargo/src/serve/unit/script.rs:128`, `:145-148`) and `blaze-units`'
manifest loading read through the VFS (`Mounted::read`,
`crates/blaze-cargo/src/serve/unit/files.rs:59`) and the store. That is mapped memory, not a
syscall.

### link (`crates/blaze-link/`)

| path:line | call | what it does | task |
|---|---|---|---|
| `crates/blaze-link/src/target.rs:575-601` | `Command::new(..).spawn`, `wait_with_output` | runs `cc`/`xcrun`/`vswhere`/`reg` to find the toolchain (cached per process) | the first link (`Front::compile`) |
| `crates/blaze-link/src/target.rs:801-827` | `is_dir`, `is_file`, `canonicalize` | parses `cc -print-*`, resolves libc | the toolchain query (Linux) |
| `crates/blaze-link/src/target.rs:850-865` | `xcrun`, `read_to_string` | the macOS SDK path and `SDKSettings.json` | the toolchain query (Mach-O) |
| `crates/blaze-link/src/target.rs:898-979` | `vswhere`/`reg`, `read_to_string`, `read_dir`, `is_file` | MSVC and Windows SDK discovery | the toolchain query (PE) |
| `crates/blaze-link/src/target.rs:205`, `:994` | `is_file` | sanitizer runtime, WASI crt object | `Front::compile` |
| `crates/blaze-link/src/target.rs:654` | `std::fs::read` | reads an archive or object for autolink metadata | each link |
| `crates/blaze-link/src/target.rs:1026-1029` | `std::fs::read_dir` | lists a `-l` search directory | each link |
| `crates/blaze-link/src/target.rs:1088`, `:1174`, `:1185`, `:1226` | `is_file` | resolves libraries and scripts on the link line | each link |
| `crates/blaze-link/src/target.rs:1202-1205` | `std::fs::read` | reads a linker script | each link with one |
| `crates/blaze-link/src/target.rs:1339-1344` | `read_dir`, `is_file` | case-insensitive library lookup | each link (PE) |
| `crates/blaze-link/src/target.rs:1363-1371` | `File::open`, `read` | sniffs archive, object or script magic | following scripts |

Linking itself is in-process: objects come from memory and the image goes back to the loop.

### codegen (`crates/hir-ty-codegen/`, `crates/hir-ty-mir/`)

| path:line | call | what it does | task |
|---|---|---|---|
| `crates/hir-ty-codegen/asm.rs:1675-1720` | `create_dir_all`, `Command::new(..).spawn`/`wait`, `read`, `remove_file` | assembles `asm!` with the host C compiler into a temporary `.o` | `Front::compile`, when `BLAZE_ASSEMBLER=cc` or blaze has no LLVM |
| `crates/hir-ty-codegen/llvm.rs:1157` | `eprintln!` | an LLVM warning to stderr, unconditional | `Front::compile` |

LLVM writes objects to memory (`crates/hir-ty-codegen/llvm.rs:419`, `:986-1002`), not files.

### usercode (`crates/blaze-macro-eval/`, `crates/proc-macro-api/`)

| path:line | call | what it does | task |
|---|---|---|---|
| `crates/blaze-macro-eval/src/os/files.rs:175-196` | `symlink_metadata`, `read_link` | path resolution in the macro's emulated OS | proc-macro expansion in `Front::compile` |
| `crates/blaze-macro-eval/src/os/files.rs:409` | `File::open`, `read_to_end` | reads a file the macro opened | proc-macro expansion |
| `crates/blaze-macro-eval/src/os/files.rs:671`, `:769` | `read_link`, `read_dir` | the macro's readlink and readdir | proc-macro expansion |
| `crates/blaze-macro-eval/src/expander.rs:404` | `eprintln!` | a macro warning to stderr | proc-macro expansion |
| `crates/blaze-macro-eval/src/tier/machine.rs:247`, `:422`, `:426` | `eprintln!` | stub and escape diagnostics | proc-macro expansion |
| `crates/proc-macro-api/src/kept.rs:414` | `eprintln!` | the first server-side macro failure | proc-macro expansion |

### db (`crates/base-db/`, `crates/hir-def/`, `crates/hir-ty/`, `crates/hir-ty-borrowck/`)

| path:line | call | what it does | task |
|---|---|---|---|
| `crates/base-db/src/files.rs:557` | `std::fs::read` | reads an `include_*!` file outside the VFS | macro expansion in any front-end query |
| `crates/base-db/src/db/executions.rs:21` | `env::var("RUSTD_TRACE")` | a hidden input, read when a `Db` is built | every front-end task |

The rest of the front end reads only the VFS and its memos. `rustc_type_ir`,
`rustc_next_trait_solver`, `blaze-query`, `hir-expand`, `hir`, `blaze-items` and the format
crates do no I/O. One exception: `crates/syntax-tree/src/cursor.rs:143` calls
`process::abort` on a refcount overflow, which is an invariant, not I/O.

### Env-gated debug prints (all `eprintln!` from a job, off unless the variable is set)

| path:line | gate | owner |
|---|---|---|
| `crates/hir-def/src/expr_store/body.rs:439-442` (gate read at `:325`) | `RA_CHECK_RELATIVE_BODIES` | db |
| `crates/hir-ty/src/traits.rs:331-361` (gate at `:329`) | `BLAZE_TRACE_01225` | items |
| `crates/hir-ty-borrowck/src/lib.rs:597-600`, `check.rs:270-298`, `regions.rs:6194` (gate at `lib.rs:101-113`) | `BLAZE_BORROWCK_DEBUG` | db |
| `crates/hir-ty-mir/src/mir/lower.rs:5424-5432` (gate at `:5421`) | `RUSTD_DUMP_ERRORS` | codegen |
| `crates/hir-ty-codegen/llvm.rs:375-377`, `:415-416` | `BLAZE_DUMP_LLVM` | codegen |
| `crates/hir-ty-codegen/mod.rs:959-960`, `:1083-1094` | `BLAZE_DUMP_MIR` | codegen |
| `crates/hir-ty-codegen/mod.rs:1050-1051` | `BLAZE_DUMP_CLIF` | codegen |
| `crates/hir-ty-codegen/tier.rs:768-779`, `:915-927`, `:1087-1088`; `mod.rs:9857-9858` | `BLAZE_DUMP_TIER` | codegen |
| `crates/hir-ty-codegen/fncache.rs:736-737`; `fncache/dispatch.rs:40`, `:76`, `:109` | `BLAZE_FN_CACHE_TRACE` | codegen |
| `crates/hir-ty-codegen/places.rs:74-75` | `BLAZE_CHECK_PLACES` | codegen |

`BLAZE_BORROWCK` (`crates/hir-ty-borrowck/src/lib.rs:101-113`,
`crates/hir-ty-wfcheck/src/lib.rs:132`) is also read inside queries as a mode switch. Like
`RUSTD_TRACE`, that is a hidden input to a memo. It belongs in the job's inputs, not
`env::var`.

## Store and VFS reads are not a wait today, but can be

`blaze_cas::Reader` (`get`, `has`, `pinned`) and the VFS reader (`Mounted::read`) read pack
and file memory that the driver mapped (`Effect::Map`), so a job does no syscall. A cold page
is a major fault that blocks the core's thread, invisible to the scheduler. Under fibers it
stays a fault, not a request. If the profiler shows fault stalls on the critical path, the
fix is the driver prefaulting (`MADV_WILLNEED`, `MAP_POPULATE`) what a posted task will read,
not a suspend.

## Not job code: the loop and the driver

The loop runs on its own thread, not on a worker, but it does blocking I/O itself that the
driver could take. These are listed so nobody moves them onto a worker:

- **Build-script, test and run starts** (`Front::script`, `Front::tests`, `Front::run`;
  `crates/blaze-cargo/src/serve/unit/script.rs:812-822`):
  - `blaze_build_script::run::begin` (`crates/blaze-build-script/src/run.rs:72-102`: `OUT_DIR`
    and `invoked.timestamp`);
  - `crates/blaze-usercode/src/run.rs:294-447`: empties, collects and restores `OUT_DIR`;
  - `crates/blaze-usercode/src/confine.rs:166-179`: writes the spec and reads the report;
  - `crates/blaze-usercode/src/inputs.rs:274-479`: hashes inputs.

  The sandboxed process itself runs under `Effect::Spawn`.
- **Script finish**: `crates/blaze-cargo/src/serve/unit/script.rs:453-477` walks `OUT_DIR`.
- **Mounts, image writes and the daemon loop itself**: `mounts.rs`, `image.rs`, `daemon.rs`,
  `heads.rs` and `signals.rs` in `crates/blaze-cargo/src/serve/`.
- **The sandbox child and supervisor**: `crates/blaze-cargo/src/sandbox.rs`, `sandbox/`
  (usercode). This is another process, not a job.
- **The driver's own backends** (core): `crates/blaze-core/src/{uring,kqueue,iocp,sim}.rs`,
  and its helpers `fs.rs`, `threads.rs`, `process.rs`, `net.rs`, `watch.rs`.
- **Setup, once per process**: `crates/blaze-graph/src/topology.rs` (`/sys` CPU reads) and
  `crates/blaze-graph/src/arena.rs:361-473` (`/proc/meminfo`, cgroups), at pool and daemon
  start (core).
- **Not reached from any task**: `blaze-live` (the patched program's process),
  `blaze-profile`'s file writes (at job finish on the loop), `blaze-doc`'s `html::write` (the
  CLI; the `Doc` task renders to memory), and `blaze-protocol`'s `follow.rs` (the client).
  The CI crates, `blaze-fmt`, `blaze-lints`' config reads, `blaze-metadata` and
  `blaze-manifest`'s `Disk` are not reached either.
- **Memory syscalls**: `crates/blaze-mem/src/os.rs:67-97`, `:366-514` and `vm.rs:22-47`
  (anonymous `mmap`, `mprotect`, `madvise` on allocation paths) are memory management, not
  I/O. They stay in the job.

## Found on the way

- **own-nightly**: `crates/blaze-cargo/src/toolchain.rs:229-234` finds the sysroot by
  scanning rustup's `toolchains/` directory, inside `Front::compile`. Blaze is its own
  toolchain (AGENTS.md rule 6), so this is a request to lane cargo regardless of where it
  runs.
- **A nested pool scope inside a task**: `crates/blaze-cargo/src/serve/unit/deps.rs:570-581`
  runs `blaze_graph::exec::shared().scope` from `Steps::lower`. That is the shape phase 2's
  counter wait replaces: one suspend for the batch, not a blocked worker.

## Requests this makes, by owner

Queued once phase 2's driver request API exists (a `claim(key, None)` + `Wait::Key` per
request, completed by `key_done` from the driver):
- **cargo**: profile-use and native-library reads in `serve/unit/codegen.rs`; the rustup scan
  in `toolchain.rs`; the library and fetch tasks onto driver requests.
- **link**: toolchain discovery as a daemon-start input; per-link reads and listings in
  `target.rs` as driver requests.
- **codegen**: the external assembler as a driver spawn; LLVM warnings returned as
  diagnostics; the env-gated dumps as profiler events.
- **usercode**: `blaze-macro-eval`'s OS file calls as driver requests; macro warnings returned
  as diagnostics.
- **db**: `include_*!` files outside the VFS mounted as VFS inputs; `RUSTD_TRACE` and
  `BLAZE_BORROWCK` as declared inputs; the env-gated dumps as events.
- **items**: `BLAZE_TRACE_01225`'s dump as an event.
