use std::ffi::OsString;

use blaze_wasi::{
    Embed, Stop, System,
    seed::{self, EmbedSeed, Fibers},
};
use wasmtime::{Caller, Engine, Linker, Store};

use crate::{Config, Exit, run};

/// A memory64 module that reaches the host only through `wasi_snapshot_preview1_64`: it reads
/// its arguments and environment, checks the realtime clock, writes a line to stdout, creates
/// the file its second argument names (relative to `/`), writes its argument and environment
/// buffers there in one two-buffer write, and exits with 7. Any call that fails traps.
const WASI64: &str = r#"
(module
  (import "wasi_snapshot_preview1_64" "args_sizes_get" (func $args_sizes_get (param i64 i64) (result i32)))
  (import "wasi_snapshot_preview1_64" "args_get" (func $args_get (param i64 i64) (result i32)))
  (import "wasi_snapshot_preview1_64" "environ_sizes_get" (func $environ_sizes_get (param i64 i64) (result i32)))
  (import "wasi_snapshot_preview1_64" "environ_get" (func $environ_get (param i64 i64) (result i32)))
  (import "wasi_snapshot_preview1_64" "clock_time_get" (func $clock_time_get (param i32 i64 i64) (result i32)))
  (import "wasi_snapshot_preview1_64" "path_open"
    (func $path_open (param i32 i32 i64 i64 i32 i64 i64 i32 i64) (result i32)))
  (import "wasi_snapshot_preview1_64" "fd_write" (func $fd_write (param i32 i64 i64 i64) (result i32)))
  (import "wasi_snapshot_preview1_64" "fd_close" (func $fd_close (param i32) (result i32)))
  (import "wasi_snapshot_preview1_64" "proc_exit" (func $proc_exit (param i32)))
  ;; 0 argc, 8 argv_buf_size, 16 envc, 24 environ_buf_size, 32 the time, 40 the opened fd,
  ;; 48 bytes written, 64 two iovecs, 256 argv, 512 environ, 1024 the line, 4096 the
  ;; arguments' strings, 8192 the environment's.
  (memory (export "memory") i64 1)
  (data (i64.const 1024) "hello from a 64-bit seed\n")
  (func $ok (param i32) (if (local.get 0) (then unreachable)))
  (func (export "_start")
    (local $fd i32)
    (call $ok (call $args_sizes_get (i64.const 0) (i64.const 8)))
    (call $ok (call $args_get (i64.const 256) (i64.const 4096)))
    (call $ok (call $environ_sizes_get (i64.const 16) (i64.const 24)))
    (call $ok (call $environ_get (i64.const 512) (i64.const 8192)))
    ;; Two arguments; the second starts after "seed\0", its pointer 8 bytes after the first.
    (if (i64.ne (i64.load (i64.const 0)) (i64.const 2)) (then unreachable))
    (if (i64.ne (i64.load (i64.const 264)) (i64.const 4101)) (then unreachable))
    ;; Two variables; the second's pointer is 8 bytes after the first's, past "A=1\0".
    (if (i64.ne (i64.load (i64.const 16)) (i64.const 2)) (then unreachable))
    (if (i64.ne (i64.load (i64.const 520)) (i64.const 8196)) (then unreachable))
    ;; The realtime clock is past 2020.
    (call $ok (call $clock_time_get (i32.const 0) (i64.const 1) (i64.const 32)))
    (if (i64.lt_u (i64.load (i64.const 32)) (i64.const 1577836800000000000)) (then unreachable))
    (i64.store (i64.const 64) (i64.const 1024))
    (i64.store (i64.const 72) (i64.const 25))
    (call $ok (call $fd_write (i32.const 1) (i64.const 64) (i64.const 1) (i64.const 48)))
    (if (i64.ne (i64.load (i64.const 48)) (i64.const 25)) (then unreachable))
    ;; `creat | trunc`, the right `fd_write`, the path the second argument names.
    (call $ok (call $path_open (i32.const 3) (i32.const 1)
      (i64.const 4101) (i64.sub (i64.load (i64.const 8)) (i64.const 6))
      (i32.const 9) (i64.const 64) (i64.const 0) (i32.const 0) (i64.const 40)))
    (local.set $fd (i32.load (i64.const 40)))
    (i64.store (i64.const 64) (i64.const 4096))
    (i64.store (i64.const 72) (i64.load (i64.const 8)))
    (i64.store (i64.const 80) (i64.const 8192))
    (i64.store (i64.const 88) (i64.load (i64.const 24)))
    (call $ok (call $fd_write (local.get $fd) (i64.const 64) (i64.const 2) (i64.const 48)))
    (if (i64.ne (i64.load (i64.const 48)) (i64.add (i64.load (i64.const 8)) (i64.load (i64.const 24))))
      (then unreachable))
    (call $ok (call $fd_close (local.get $fd)))
    (call $proc_exit (i32.const 7))))
"#;

fn config(args: &[&str], env: &[(&str, &str)]) -> Config {
    Config {
        args: args.iter().map(OsString::from).collect(),
        env: env
            .iter()
            .map(|(name, value)| (OsString::from(name), OsString::from(value)))
            .collect(),
        memory_max: crate::MEMORY_MAX,
    }
}

#[test]
fn a_memory64_module_reaches_args_env_clock_stdout_and_files_through_the_64_bit_wasi() {
    let dir = std::env::temp_dir().join(format!("blaze-seed-run-wasi64-{}", std::process::id()));
    std::fs::create_dir_all(&dir).unwrap();
    let out = dir.canonicalize().unwrap().join("out");
    let relative = out.to_str().unwrap().strip_prefix('/').unwrap().to_owned();
    let env = [("A", "1"), ("SEED", "yes")];
    let exit = run(&wat::parse_str(WASI64).unwrap(), &config(&["seed", &relative], &env)).unwrap();
    assert_eq!(exit.code, 7, "{exit:?}");
    let written = std::fs::read(&out).unwrap();
    std::fs::remove_dir_all(&dir).unwrap();
    assert_eq!(written, format!("seed\0{relative}\0A=1\0SEED=yes\0").into_bytes());
}

/// Grows its memory to 1 TiB in sixteen steps of 64 GiB and touches 100 MiB of it spread across
/// the whole: 50 blocks of 2 MiB, 20 GiB apart, every 4 KiB. Whole aligned 2 MiB blocks hold the
/// same with transparent huge pages or without. Then it touches 64 MiB more at the end,
/// decommits them through the runner, reads them back as zeros and decommits them again (the
/// reads map zero pages, which count as held). Last it decommits 8 unaligned bytes over a
/// word of 0xff inside the first block, checking only those went to zero.
const TEBIBYTE: &str = r#"
(module
  (import "blaze_seed" "decommit" (func $decommit (param i64 i64)))
  (memory (export "memory") i64 1 17000000)
  (func (export "_start")
    (local $i i64) (local $at i64) (local $end i64)
    (loop $grow
      (if (i64.eq (memory.grow (i64.const 1048576)) (i64.const -1)) (then unreachable))
      (local.set $i (i64.add (local.get $i) (i64.const 1)))
      (br_if $grow (i64.lt_u (local.get $i) (i64.const 16))))
    (local.set $i (i64.const 0))
    (loop $blocks
      (local.set $at (i64.mul (local.get $i) (i64.const 21474836480)))
      (local.set $end (i64.add (local.get $at) (i64.const 2097152)))
      (loop $pages
        (i64.store8 (local.get $at) (i64.const 1))
        (local.set $at (i64.add (local.get $at) (i64.const 4096)))
        (br_if $pages (i64.lt_u (local.get $at) (local.get $end))))
      (local.set $i (i64.add (local.get $i) (i64.const 1)))
      (br_if $blocks (i64.lt_u (local.get $i) (i64.const 50))))
    ;; 64 MiB at 1 TiB - 64 MiB, touched, decommitted, read back as zeros at both ends, and
    ;; decommitted again.
    (local.set $at (i64.const 1099444518912))
    (local.set $end (i64.const 1099511627776))
    (loop $more
      (i64.store8 (local.get $at) (i64.const 2))
      (local.set $at (i64.add (local.get $at) (i64.const 4096)))
      (br_if $more (i64.lt_u (local.get $at) (local.get $end))))
    (call $decommit (i64.const 1099444518912) (i64.const 67108864))
    (if (i64.ne (i64.load (i64.const 1099444518912)) (i64.const 0)) (then unreachable))
    (if (i64.ne (i64.load (i64.const 1099511627768)) (i64.const 0)) (then unreachable))
    (call $decommit (i64.const 1099444518912) (i64.const 67108864))
    ;; Bytes 4..12 of a word of 0xff at 8 go to zero; 12..16 stay.
    (i64.store (i64.const 8) (i64.const -1))
    (call $decommit (i64.const 4) (i64.const 8))
    (if (i32.ne (i32.load (i64.const 8)) (i32.const 0)) (then unreachable))
    (if (i32.ne (i32.load (i64.const 12)) (i32.const -1)) (then unreachable))))
"#;

#[test]
fn a_seed_grown_to_a_tebibyte_that_touches_100_mib_holds_about_100_mib() {
    const MIB: u64 = 1 << 20;
    let exit = run(&wat::parse_str(TEBIBYTE).unwrap(), &config(&["seed"], &[])).unwrap();
    let Exit { code, memory_bytes, held_bytes } = exit;
    assert_eq!(code, 0);
    assert_eq!(memory_bytes, ((1 << 24) + 1) * 65536);
    assert!(
        (100 * MIB..=102 * MIB).contains(&held_bytes),
        "a 1 TiB memory with 100 MiB touched holds {held_bytes} bytes ({} MiB)",
        held_bytes / MIB
    );
}

/// Grows its memory to 256 MiB and touches one byte every 64 KiB of it: 4096 sparse touches, as
/// blaze-mem's lightly used 64 KiB-aligned reservations are touched.
const SPARSE: &str = r#"
(module
  (memory (export "memory") i64 1)
  (func (export "_start")
    (local $at i64)
    (if (i64.eq (memory.grow (i64.const 4095)) (i64.const -1)) (then unreachable))
    (loop $touch
      (i64.store8 (local.get $at) (i64.const 1))
      (local.set $at (i64.add (local.get $at) (i64.const 65536)))
      (br_if $touch (i64.lt_u (local.get $at) (i64.const 268435456))))))
"#;

/// Each sparse touch costs one host page, not a 2 MiB transparent huge page: the reservation
/// is `MADV_NOHUGEPAGE` on Linux (under `transparent_hugepage=always` this memory would
/// otherwise hold all 256 MiB), and macOS has none.
#[test]
fn sparse_touches_hold_a_host_page_each_not_a_huge_page() {
    // SAFETY: `sysconf` reads a constant of the system.
    let page = unsafe { libc::sysconf(libc::_SC_PAGESIZE) } as u64;
    let exit = run(&wat::parse_str(SPARSE).unwrap(), &config(&["seed"], &[])).unwrap();
    assert_eq!((exit.code, exit.memory_bytes), (0, 256 << 20));
    let touched = 4096 * page;
    assert!(
        (touched..=touched + (1 << 20)).contains(&exit.held_bytes),
        "4096 touches of {page}-byte pages hold {} bytes, not about {touched}",
        exit.held_bytes
    );
}

/// The seed's own calls (`blaze_seed`): its ABI version, then the directory its second argument
/// names (an absolute path) made, refused as existing when made again, and removed; a relative
/// path is `EINVAL`. Exits with 7 when every answer is the one expected.
const SEED_CALLS: &str = r#"
(module
  (import "wasi_snapshot_preview1_64" "args_sizes_get" (func $args_sizes_get (param i64 i64) (result i32)))
  (import "wasi_snapshot_preview1_64" "args_get" (func $args_get (param i64 i64) (result i32)))
  (import "wasi_snapshot_preview1_64" "proc_exit" (func $proc_exit (param i32)))
  (import "blaze_seed" "abi_version" (func $abi_version (result i32)))
  (import "blaze_seed" "create_dir" (func $create_dir (param i64 i64) (result i32)))
  (import "blaze_seed" "remove_dir" (func $remove_dir (param i64 i64) (result i32)))
  ;; 0 argc, 8 argv_buf_size, 256 argv, 1024 "x", 4096 the arguments' strings.
  (memory (export "memory") i64 1)
  (data (i64.const 1024) "x")
  (func $expect (param i32 i32) (if (i32.ne (local.get 0) (local.get 1)) (then unreachable)))
  (func (export "_start")
    (local $len i64)
    (call $expect (call $abi_version) (i32.const 1))
    (call $expect (call $args_sizes_get (i64.const 0) (i64.const 8)) (i32.const 0))
    (call $expect (call $args_get (i64.const 256) (i64.const 4096)) (i32.const 0))
    ;; The second argument starts after "seed\0" and ends before its NUL.
    (local.set $len (i64.sub (i64.load (i64.const 8)) (i64.const 6)))
    (call $expect (call $create_dir (i64.const 4101) (local.get $len)) (i32.const 0))
    (call $expect (call $create_dir (i64.const 4101) (local.get $len)) (i32.const 20))
    (call $expect (call $remove_dir (i64.const 4101) (local.get $len)) (i32.const 0))
    (call $expect (call $create_dir (i64.const 1024) (i64.const 1)) (i32.const 28))
    (call $proc_exit (i32.const 7))))
"#;

#[test]
fn the_seeds_own_calls_make_and_remove_a_directory_at_an_absolute_path() {
    let dir = std::env::temp_dir().join(format!("blaze-seed-run-calls-{}", std::process::id()));
    let exit =
        run(&wat::parse_str(SEED_CALLS).unwrap(), &config(&["seed", dir.to_str().unwrap()], &[]))
            .unwrap();
    assert_eq!(exit.code, 7, "{exit:?}");
    assert!(!dir.exists(), "{} was made and not removed", dir.display());
}

/// Store data that links the seed's calls and serves none: the names are what is checked.
#[derive(Default)]
struct Nothing(Fibers);

impl Embed for Nothing {
    fn with<R>(
        _: &mut Caller<'_, Self>,
        _: impl FnOnce(&mut [u8], &mut dyn System) -> Result<R, Stop>,
    ) -> wasmtime::Result<R> {
        Err(wasmtime::Error::msg("no guest"))
    }
}

impl EmbedSeed for Nothing {
    fn with_seed<R>(
        _: &mut Caller<'_, Self>,
        _: impl FnOnce(&mut [u8], &mut dyn seed::Seed) -> Result<R, Stop>,
    ) -> wasmtime::Result<R> {
        Err(wasmtime::Error::msg("no guest"))
    }

    fn fibers(&mut self) -> &mut Fibers {
        &mut self.0
    }
}

/// `seed::FUNCTIONS` (what the macro host refuses, and the seed's ABI) is exactly what
/// `seed::link` defines, all under `blaze_seed`.
#[test]
fn the_seeds_list_of_calls_is_exactly_what_its_link_defines() {
    let engine = Engine::default();
    let mut l = Linker::new(&engine);
    seed::link::<Nothing>(&mut l).unwrap();
    let mut store = Store::new(&engine, Nothing::default());
    let mut names: Vec<String> = l
        .iter(&mut store)
        .map(|(module, name, _)| {
            assert_eq!(module, seed::MODULE, "`{name}` is defined under `{module}`");
            name.to_owned()
        })
        .collect();
    names.sort();
    let mut expected: Vec<String> = seed::FUNCTIONS.iter().map(|name| (*name).to_owned()).collect();
    expected.sort();
    assert_eq!(names, expected);
}

/// The seed's fibers, as blaze-graph's `sched::stack` uses them on wasm. Memory word `8 * arg`
/// counts a fiber's suspends.
///
/// - Fibers 1 and 2 (`$body`) each check their shadow stack starts at `10000 * (arg + 1)`, then
///   three times recurse 50 frames deep and suspend there, checking `__stack_pointer` is theirs
///   again when resumed. `_start` interleaves them, checking its own `__stack_pointer` after
///   each switch; fiber 1's fourth resume returns (1); fiber 2 is dropped while suspended
///   mid-body, which unwinds it.
/// - Fiber 3 (`$nest`) resumes fiber 4 (a `$body`) from inside itself, suspends, and when
///   resumed resumes fiber 4 again and drops it: resumes nest.
/// - Fiber 5 (`$exit`) calls `proc_exit(7)`, which ends the run with 7 through both stacks.
const FIBERS: &str = r#"
(module
  (import "blaze_seed" "fiber_new" (func $new (param i64 i64 i64) (result i64)))
  (import "blaze_seed" "fiber_resume" (func $resume (param i64) (result i32)))
  (import "blaze_seed" "fiber_suspend" (func $suspend))
  (import "blaze_seed" "fiber_drop" (func $drop (param i64)))
  (import "wasi_snapshot_preview1_64" "proc_exit" (func $proc_exit (param i32)))
  (memory (export "memory") i64 1)
  (global $sp (export "__stack_pointer") (mut i64) (i64.const 60000))
  (table (export "__indirect_function_table") 4 funcref)
  (elem (i32.const 1) $body $nest $exit)
  (func $count (param $arg i64) (result i64)
    (i64.load (i64.mul (local.get $arg) (i64.const 8))))
  (func $deep (param $n i32) (param $arg i64) (param $sp i64)
    (if (local.get $n)
      (then
        (call $deep (i32.sub (local.get $n) (i32.const 1)) (local.get $arg) (local.get $sp))
        (return)))
    (i64.store (i64.mul (local.get $arg) (i64.const 8)) (i64.add (call $count (local.get $arg)) (i64.const 1)))
    (call $suspend)
    (if (i64.ne (global.get $sp) (local.get $sp)) (then unreachable)))
  (func $body (param $arg i64)
    (local $i i32) (local $sp i64)
    (if (i64.ne (global.get $sp) (i64.mul (i64.add (local.get $arg) (i64.const 1)) (i64.const 10000)))
      (then unreachable))
    (global.set $sp (i64.sub (global.get $sp) (i64.const 16)))
    (local.set $sp (global.get $sp))
    (loop $again
      (call $deep (i32.const 50) (local.get $arg) (local.get $sp))
      (local.set $i (i32.add (local.get $i) (i32.const 1)))
      (br_if $again (i32.lt_u (local.get $i) (i32.const 3)))))
  (func $nest (param $arg i64)
    (local $d i64)
    (local.set $d (call $new (i64.const 1) (i64.const 4) (i64.const 50000)))
    (if (i32.ne (call $resume (local.get $d)) (i32.const 0)) (then unreachable))
    (call $suspend)
    (if (i32.ne (call $resume (local.get $d)) (i32.const 0)) (then unreachable))
    (call $drop (local.get $d)))
  (func $exit (param $arg i64)
    (call $proc_exit (i32.const 7)))
  (func (export "_start")
    (local $a i64) (local $b i64) (local $c i64) (local $i i32)
    (local.set $a (call $new (i64.const 1) (i64.const 1) (i64.const 20000)))
    (local.set $b (call $new (i64.const 1) (i64.const 2) (i64.const 30000)))
    (loop $again
      (if (i32.ne (call $resume (local.get $a)) (i32.const 0)) (then unreachable))
      (if (i64.ne (global.get $sp) (i64.const 60000)) (then unreachable))
      (if (i32.ne (call $resume (local.get $b)) (i32.const 0)) (then unreachable))
      (if (i64.ne (global.get $sp) (i64.const 60000)) (then unreachable))
      (local.set $i (i32.add (local.get $i) (i32.const 1)))
      (br_if $again (i32.lt_u (local.get $i) (i32.const 3))))
    (if (i64.ne (call $count (i64.const 1)) (i64.const 3)) (then unreachable))
    (if (i64.ne (call $count (i64.const 2)) (i64.const 3)) (then unreachable))
    (if (i32.ne (call $resume (local.get $a)) (i32.const 1)) (then unreachable))
    (call $drop (local.get $a))
    (call $drop (local.get $b))
    (local.set $c (call $new (i64.const 2) (i64.const 3) (i64.const 40000)))
    (if (i32.ne (call $resume (local.get $c)) (i32.const 0)) (then unreachable))
    (if (i64.ne (call $count (i64.const 4)) (i64.const 1)) (then unreachable))
    (if (i32.ne (call $resume (local.get $c)) (i32.const 1)) (then unreachable))
    (if (i64.ne (call $count (i64.const 4)) (i64.const 2)) (then unreachable))
    (if (i64.ne (global.get $sp) (i64.const 60000)) (then unreachable))
    (drop (call $resume (call $new (i64.const 3) (i64.const 5) (i64.const 0))))
    unreachable))
"#;

#[test]
fn fibers_switch_their_own_stacks_nest_and_end_the_run_from_inside() {
    let exit = run(&wat::parse_str(FIBERS).unwrap(), &config(&["seed"], &[])).unwrap();
    assert_eq!(exit.code, 7, "{exit:?}");
}

/// A suspend off any fiber traps, naming the call.
#[test]
fn a_suspend_on_no_fiber_is_refused_naming_it() {
    let wat = r#"
(module
  (import "blaze_seed" "fiber_suspend" (func $suspend))
  (memory (export "memory") i64 1)
  (func (export "_start") (call $suspend)))
"#;
    let error =
        run(&wat::parse_str(wat).unwrap(), &config(&["seed"], &[])).unwrap_err().to_string();
    assert!(error.contains("blaze_seed.fiber_suspend") && error.contains("no fiber"), "{error}");
}
