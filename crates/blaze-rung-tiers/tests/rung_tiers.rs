//! Every rung of blaze-rungs run at the Reference tier and at the Fast tier, both through the
//! tier seam (`blaze_tier::Tier::emit`), and compared on exit status and standard output with
//! each other and with the rung's `expected` file (backend-next.md D97 point 1; D197 root 6,
//! piece (c)).
//!
//! HOW ONE RUNG RUNS AT ONE TIER:
//!
//! 1. The rung's LIR is built by `blaze run`'s record walk (blaze-ladder run.rs
//!    `build_records_new` over code_records.rs `RecordRun::census`): every body `main` reaches,
//!    with the dev profile's overflow checks as its manifest says (D101) and the unwind
//!    strategy, as `blaze run` builds it. The census hands each body's verified LIR and its
//!    names here, and makes no record.
//! 2. Every body is emitted by the tier through the seam, sealed and encoded in blaze-tier's
//!    record form (`blaze_tier::record::MAGIC`), and read back. A body the tier refuses is filed under its class
//!    (the reason exactly as the tier's `TierError` names it, without the instruction number);
//!    a rung with a refused body is not run at that tier. The Fast tier's classes over all
//!    rungs are THE WORKLIST for piece (d), printed by [`every_rung_at_reference_and_fast`].
//! 3. The bodies are laid out in one executable mapping (blaze-inproc `Executable::written`):
//!    each code and relro part at its record's alignment (`code_align`, `relro_align`), the
//!    relro parts in the data after the code, then one import word
//!    per target, then a zeroed page for the poll page (`blaze_poll_page`), and each record is
//!    placed by blaze-tier's placer (`blaze_tier::record::place`). A function target is the
//!    body's code address; a C name is its `dlsym` address in this process. A static, a vtable
//!    or a thread-local is NOT bound by this linker: a rung that names one is reported as not
//!    linked, by name, and not compared (the record form's statics are records of their own,
//!    which the census does not hand over; piece (b) moves the run to the one placer that binds
//!    them).
//! 4. `main` (the walk's first body) is called in a forked child with its standard output on a
//!    pipe, as `extern "C" fn() -> i32` (its low byte the status) or, where its signature
//!    returns nothing, `extern "C" fn()` (status 0). A child that is not done in
//!    [`RUN_DEADLINE_MS`] is killed and reported as timed out (rule 18); one a signal ends is
//!    reported with the signal.
//!
//! WHAT IT ASSERTS: a rung that cannot pass yet reports (a refused body is a worklist row, a run
//! that gives other than `expected` is a RAN WRONG row) and does not fail the test. A rung in
//! `PASSING_AT_REFERENCE` or `PASSING_AT_FAST` must run right at that tier, byte for byte: the
//! ratchet, empty until the first run is read, as blaze-ladder tests/ladder_front2.rs `PASSING`.
//! The planted control [`a_wrong_add_template_changes_an_add`] proves the emit, place and call
//! path sees a wrong template today, on one `add` function; the whole-rung control
//! [`a_wrong_add_template_makes_a_rung_differ`] is ignored until a rung runs whole at Fast.
//!
//! Test code: the bookkeeping is in the test heap's allocator (`blaze_own::testing::heap`),
//! and the report is printed with `println!`.

#![cfg(target_arch = "aarch64")]
#![feature(allocator_api)]

use core::ffi::{c_char, c_int};
use core::panic::AssertUnwindSafe;
use std::panic::catch_unwind;

use blaze_code::image::ImageError;
use blaze_code::place::{At, Words};
use blaze_code::record::{Space, Symbol, Target};
use blaze_emit::table_gen::EncodedTable;
use blaze_emit::{Fast, Isa, Os, Reference};
use blaze_inproc::exec::Executable;
use blaze_ladder::code_records::{Builder, Callee, InstanceNames, LirSeen, MemoryStore, RecordRun, Routed, SeenInstance, Settings};
use blaze_ladder::declared_names::DeclaredNames;
use blaze_ladder::refusal::SourceFile;
use blaze_ladder::run::{NewFront, build_records_new};
use blaze_lir::builder::{Param, Variadic};
use blaze_lir::func::{Conv, SigId};
use blaze_lir::{BlockKind, Builder as LirBuilder, Flags, Full, Op, Ty, VerifiedFunc};
use blaze_own::{AString, AVec, Alloc, Vec, format_in};
use blaze_rungs::{RUNGS, Rung};
use blaze_tier::record::{RecordView, declare, encode, place, read, seal};
use blaze_tier::{ARM64, Machine, Named, Names, Outside, Pattern, Table, TableHead, Template, Tier, TierError, TierName};
use blaze_typeck::ItemKey;

/// The template table the Fast tier composes from (blaze-emit tier_tests.rs, fast_census.rs).
const TABLE: &[u8] = include_bytes!("../../blaze-emit/table/arm64.bztab");

/// The poll page's outside name (blaze-emit compose/emit.rs `POLL_PAGE`): the page a poll
/// reads; zero here, so no poll stops.
const POLL_PAGE: &str = "blaze_poll_page";

/// How long one rung's run may take before its child is killed.
const RUN_DEADLINE_MS: u64 = 10_000;

/// The test's allocator.
fn alloc() -> Alloc<'static> {
    blaze_own::testing::heap().alloc()
}

// ---- The walk ------------------------------------------------------------------------------

/// A function reference of a body, owned.
enum Owned {
    Symbol(Symbol),
    C(AString<'static>),
}

/// One body the walk built.
struct Body {
    function: AString<'static>,
    symbol: Symbol,
    /// The walk's function with the walk's proof (D163): copied, not checked again.
    func: VerifiedFunc<Alloc<'static>>,
    funcs: AVec<'static, Owned>,
    statics: AVec<'static, ItemKey>,
    c_statics: AVec<'static, (ItemKey, AString<'static>)>,
    vtables: AVec<'static, Symbol>,
    tls: AVec<'static, ItemKey>,
    /// The soft-float routines bound after its references, as the walk bound them.
    routines: blaze_tier::RoutinePlaces,
    /// Every target its code can name, declared as the walk declares it (D106).
    declares: DeclaredNames<'static>,
}

impl Body {
    /// `f` given the body's names and routine table as the walk handed them (blaze-ladder
    /// `InstanceNames`, `Routed`).
    fn with_names<R>(&self, f: impl FnOnce(&Routed<'_>) -> R) -> R {
        let a = alloc();
        let mut callees: AVec<'_, Callee<'_>> = Vec::with_capacity_in(self.funcs.len(), a);
        for c in self.funcs.iter() {
            callees.push(match c {
                Owned::Symbol(s) => Callee::Symbol(*s),
                Owned::C(name) => Callee::C(name.as_str()),
            });
        }
        let mut c_statics: AVec<'_, (ItemKey, &str)> = Vec::with_capacity_in(self.c_statics.len(), a);
        for (key, name) in self.c_statics.iter() {
            c_statics.push((*key, name.as_str()));
        }
        let names = InstanceNames { funcs: &callees, statics: &self.statics, c_statics: &c_statics, vtables: &self.vtables, tls: &self.tls };
        f(&Routed { names: &names, routines: self.routines })
    }
}

/// The census's [`LirSeen`]: every body, in the walk's order (the entry first: run.rs
/// `build_from` queues `main` before anything it reaches).
struct Collect {
    bodies: AVec<'static, Body>,
}

fn copied<T: Copy>(of: &[T]) -> AVec<'static, T> {
    let mut v = Vec::with_capacity_in(of.len(), alloc());
    v.extend_from_slice(of);
    v
}

impl LirSeen for Collect {
    fn seen(&mut self, instance: SeenInstance<'_>) {
        let SeenInstance { function, symbol, func, names, routines, declares } = instance;
        let a = alloc();
        let mut funcs = Vec::with_capacity_in(names.funcs.len(), a);
        for c in names.funcs {
            funcs.push(match *c {
                Callee::Symbol(s) => Owned::Symbol(s),
                Callee::C(name) => Owned::C(AString::from_str_in(name, a)),
            });
        }
        let mut c_statics = Vec::with_capacity_in(names.c_statics.len(), a);
        for &(key, name) in names.c_statics {
            c_statics.push((key, AString::from_str_in(name, a)));
        }
        self.bodies.push(Body {
            function: AString::from_str_in(function, a),
            symbol,
            func: func.to_verified_in(a),
            funcs,
            statics: copied(names.statics),
            c_statics,
            vtables: copied(names.vtables),
            tls: copied(names.tls),
            routines,
            declares,
        });
    }
}

/// The rung's bodies as `blaze run`'s record walk builds them, or why the walk stopped.
fn walked(rung: &Rung) -> Result<AVec<'static, Body>, AString<'static>> {
    let a = alloc();
    let overflow = match rung.dev_overflow_checks() {
        Ok(true) => blaze_build::OverflowChecks::On,
        Ok(false) => blaze_build::OverflowChecks::Off,
        Err(error) => return Err(format_in!(a, "{error}")),
    };
    let mut files: AVec<'_, SourceFile<'_>> = Vec::with_capacity_in(rung.files.len(), a);
    for &(path, text) in rung.files {
        files.push(SourceFile { path, text });
    }
    let new = NewFront { package: "ladder", edition: rung.edition, shelf: None, overflow };
    let os = if cfg!(target_os = "macos") { Os::MacOs } else { Os::Linux };
    let builder = Builder {
        isa: Isa { lse: true, os },
        tier: TierName::Reference,
        // The corpus's strategy, as the ladder builds the rungs (blaze-rungs `CODE_PANIC`).
        settings: Settings::baseline(overflow, match blaze_rungs::CODE_PANIC {
            blaze_rungs::Strategy::Abort => blaze_build::Panic::Abort,
            blaze_rungs::Strategy::Unwind => blaze_build::Panic::Unwind,
        }),
        toolchain: blaze_toolchain::PINNED,
        producer: blaze_ids::producer::Producers::for_tests(b"rung tiers").get::<blaze_ids::producer::CodeBuilder>(),
        edition: edition::PackageEdition::of_manifest(rung.edition).map_err(|e| format_in!(a, "the edition `{}` is no edition blaze knows: {e:?}", rung.edition))?,
    };
    let mut store = MemoryStore::new_in(a);
    let mut collect = Collect { bodies: Vec::new_in(a) };
    let walk = {
        let mut run = RecordRun::census(&mut store, builder, &mut collect).map_err(|e| format_in!(a, "the census builder: {e:?}"))?;
        build_records_new(&files, "src/main.rs", new, &mut run, a)
    };
    walk.map_err(|e| format_in!(a, "the walk stopped: {e}"))?;
    match collect.bodies.is_empty() {
        true => Err(format_in!(a, "the walk built no body")),
        false => Ok(collect.bodies),
    }
}

// ---- Emitting ------------------------------------------------------------------------------

/// The tier's refusal as its class: the reason as its error names it, without numbers that
/// belong to one function (fast_census.rs `fast_class`).
fn class(error: &TierError) -> AString<'static> {
    let a = alloc();
    match *error {
        TierError::Refused { what, .. } => AString::from_str_in(what, a),
        TierError::Table { error, .. } => format_in!(a, "{error}"),
        TierError::NoName { of, .. } => {
            let shown = format_in!(a, "{of:?}");
            let kind = &shown[..shown.find(['(', ' ', '{']).unwrap_or(shown.len())];
            format_in!(a, "no name for a reference of kind {kind}")
        }
        TierError::TooLarge { what, .. } => format_in!(a, "too large: {what}"),
        TierError::Machine { .. } | TierError::Record { .. } => numbers_out(&format_in!(a, "{error}")),
    }
}

/// `text` with every run of decimal digits written `N`.
fn numbers_out(text: &str) -> AString<'static> {
    let mut out = AString::new_in(alloc());
    let mut in_digits = false;
    for ch in text.chars() {
        match (ch.is_ascii_digit(), in_digits) {
            (true, false) => out.push_str("N"),
            (true, true) => {}
            (false, _) => {
                let mut buf = [0u8; 4];
                out.push_str(ch.encode_utf8(&mut buf));
            }
        }
        in_digits = ch.is_ascii_digit();
    }
    out
}

/// A panic's message, as its class (rule 20: one body's panic is that body's refusal).
fn panic_class(payload: &(dyn std::any::Any + Send)) -> AString<'static> {
    let said = payload.downcast_ref::<&str>().copied().or_else(|| payload.downcast_ref::<std::string::String>().map(|s| s.as_str()));
    format_in!(alloc(), "panic: {}", numbers_out(said.unwrap_or("(no message)")))
}

/// What one tier made of a rung's bodies: each body's encoded record, or its refusal's class.
struct Emits {
    records: AVec<'static, Result<AVec<'static, u8>, AString<'static>>>,
}

impl Emits {
    /// The refused bodies, as `(function, class)`.
    fn refused<'e>(&'e self, bodies: &'e [Body]) -> impl Iterator<Item = (&'e str, &'e str)> + 'e {
        bodies.iter().zip(self.records.iter()).filter_map(|(b, r)| r.as_ref().err().map(|c| (b.function.as_str(), c.as_str())))
    }
}

/// Every body emitted by `tier` through the seam, sealed, encoded and read back.
fn emit_all<T: Tier>(tier: &T, bodies: &[Body], machine: Machine) -> Emits {
    let a = alloc();
    let mut records = Vec::with_capacity_in(bodies.len(), a);
    for body in bodies {
        let made = body.with_names(|names| catch_unwind(AssertUnwindSafe(|| tier.emit(body.func.verified(), names, machine, a))));
        let record = match made {
            Err(payload) => Err(panic_class(&*payload)),
            Ok(Err(error)) => Err(class(&error)),
            Ok(Ok(mut emitted)) => match declare(&mut emitted, &body.declares) {
                Err(error) => Err(numbers_out(&format_in!(a, "the record does not declare its targets: {error}"))),
                Ok(()) => match seal(emitted) {
                    Err(error) => Err(numbers_out(&format_in!(a, "the record does not seal: {error}"))),
                    Ok(sealed) => {
                        let mut bytes = Vec::new_in(a);
                        encode(&sealed, &mut bytes);
                        match read(&bytes) {
                            Err(error) => Err(numbers_out(&format_in!(a, "the record does not read back: {error}"))),
                            Ok(_) => Ok(bytes),
                        }
                    }
                },
            },
        };
        records.push(record);
    }
    Emits { records }
}

// ---- Linking -------------------------------------------------------------------------------

/// What an import word holds.
#[derive(Clone, Copy)]
enum Value {
    /// The code address of the body at this offset in the code.
    Code(u64),
    /// An address in this process (a C name's).
    At(u64),
    /// The poll page.
    Poll,
}

/// The import words of one placement: every target of every record, sorted, with its value;
/// word `i` at `words_at + 8 * i`.
struct Bound<'b> {
    targets: &'b [Target],
    values: &'b [u64],
    words_at: u64,
}

impl Words for Bound<'_> {
    fn word(&mut self, target: Target, _call: bool) -> Result<u64, ImageError> {
        let i = self.targets.binary_search(&target).map_err(|_| ImageError::Unbound { target })?;
        Ok(self.words_at + 8 * i as u64)
    }

    fn value(&mut self, target: Target) -> Result<u64, ImageError> {
        let i = self.targets.binary_search(&target).map_err(|_| ImageError::Unbound { target })?;
        Ok(self.values[i])
    }
}

#[cfg(target_os = "linux")]
unsafe extern "C" {
    /// `void *dlsym(void *handle, const char *symbol)` (dlfcn.h, glibc's libc since 2.34):
    /// blaze-sys has no Linux form (blaze-sys posix/thread/dl.rs: "a name in the user program
    /// ... is the user program's C library"), and the program run here is this process.
    fn dlsym(handle: *mut core::ffi::c_void, symbol: *const c_char) -> *mut core::ffi::c_void;
}

/// The address of the C name `name` in this process, as the run's launcher binds an outside
/// name (`dlsym(RTLD_DEFAULT, ..)`).
fn c_address(name: &str) -> Option<u64> {
    let mut z: AVec<'_, u8> = Vec::with_capacity_in(name.len() + 1, alloc());
    z.extend_from_slice(name.as_bytes());
    z.push(0);
    let name = z.as_ptr().cast::<c_char>();
    // SAFETY: `name` is a live NUL-terminated string; `RTLD_DEFAULT` searches every loaded image.
    #[cfg(target_os = "macos")]
    let at = unsafe { blaze_sys::posix::thread::dlsym(blaze_sys::posix::thread::RTLD_DEFAULT, name) };
    // SAFETY: as above; glibc's `RTLD_DEFAULT` is the null handle.
    #[cfg(target_os = "linux")]
    let at = unsafe { dlsym(core::ptr::null_mut(), name) };
    (!at.is_null()).then_some(at as u64)
}

/// The records placed whole: the mapping, the first record's code at its start.
struct Linked {
    exec: Executable,
}

fn align_up(n: u64, to: u64) -> u64 {
    n.div_ceil(to) * to
}

/// One record's function: its symbol and its name (for a refusal's words).
#[derive(Clone, Copy)]
struct Id<'n> {
    symbol: Symbol,
    function: &'n str,
}

/// Every record placed into one mapping, `ids[i]` naming record `i`, or why they cannot be.
fn linked(bodies: &[Id<'_>], records: &[AVec<'static, u8>]) -> Result<Linked, AString<'static>> {
    let a = alloc();
    let mut views: AVec<'_, RecordView<'_>> = Vec::with_capacity_in(records.len(), a);
    for bytes in records {
        views.push(read(bytes).map_err(|e| format_in!(a, "a record does not read: {e}"))?);
    }
    // The layout: each body's code and relro part at its record's alignment (`code_align`,
    // `relro_align`: at least 16 and 8 bytes, at most a page) from the mapping's page-aligned
    // code and from the relro base, which is aligned to the most any record asks.
    let mut code_at: AVec<'_, u64> = Vec::with_capacity_in(views.len(), a);
    let mut relro_at: AVec<'_, u64> = Vec::with_capacity_in(views.len(), a);
    let (mut code_len, mut relro_len, mut relro_most) = (0u64, 0u64, 16u64);
    for view in views.iter() {
        code_len = align_up(code_len, 1 << view.code_align());
        relro_len = align_up(relro_len, 1 << view.relro_align());
        relro_most = relro_most.max(1 << view.relro_align());
        code_at.push(code_len);
        relro_at.push(relro_len);
        code_len += view.code().len() as u64;
        relro_len += view.relro().len() as u64;
    }
    // The bodies by symbol, for the function targets.
    let mut by_symbol: AVec<'_, (Symbol, u64)> = Vec::with_capacity_in(bodies.len(), a);
    for (body, at) in bodies.iter().zip(code_at.iter()) {
        by_symbol.push((body.symbol, *at));
    }
    by_symbol.sort_unstable_by(|x, y| x.0.cmp(&y.0));
    // Every target, once, with its value.
    let mut targets: AVec<'_, (Target, Value)> = Vec::new_in(a);
    for view in views.iter() {
        for i in 0..view.targets() {
            let target = view.target(i);
            if targets.iter().any(|(t, _)| *t == target) {
                continue;
            }
            let value = match target.space {
                Space::Func => match by_symbol.binary_search_by(|row| row.0.cmp(&target.symbol)) {
                    Ok(j) => Value::Code(by_symbol[j].1),
                    Err(_) => return Err(format_in!(a, "not linked: a function target the walk built no body for ({:?})", target.symbol)),
                },
                Space::Outside => {
                    let name = view.name(target.symbol).ok_or_else(|| format_in!(a, "not linked: an outside target with no name kept"))?;
                    match name == POLL_PAGE {
                        true => Value::Poll,
                        false => Value::At(c_address(name).ok_or_else(|| format_in!(a, "not linked: the C name `{name}` is not in this process"))?),
                    }
                }
                other => {
                    return Err(format_in!(
                        a,
                        "not linked: a {other:?} target (this linker binds functions and C names; statics, vtables and thread-locals are records the census does not hand over)"
                    ));
                }
            };
            targets.push((target, value));
        }
    }
    targets.sort_unstable_by(|x, y| x.0.cmp(&y.0));
    let mut sorted: AVec<'_, Target> = Vec::with_capacity_in(targets.len(), a);
    sorted.extend(targets.iter().map(|(t, _)| *t));
    let words_len = 8 * targets.len() as u64;
    let data_len = relro_most + relro_len + words_len + 2 * 4096;
    let mut done: Result<(), AString<'static>> = Ok(());
    let exec = Executable::written(code_len.max(4) as usize, data_len as usize, |code, data| {
        let code_base = code.as_ptr() as u64;
        let data_base = data.as_ptr() as u64;
        let relro0 = align_up(data_base, relro_most) - data_base;
        let words0 = relro0 + relro_len;
        let poll0 = align_up(data_base + words0 + words_len, 4096) - data_base;
        let mut values: AVec<'_, u64> = Vec::with_capacity_in(targets.len(), a);
        values.extend(targets.iter().map(|(_, v)| match *v {
            Value::Code(at) => code_base + at,
            Value::At(at) => at,
            Value::Poll => data_base + poll0,
        }));
        let mut words = Bound { targets: &sorted, values: &values, words_at: data_base + words0 };
        for (i, view) in views.iter().enumerate() {
            let (c, r) = (code_at[i] as usize, (relro0 + relro_at[i]) as usize);
            let at = At { code: code_base + code_at[i], relro: data_base + relro0 + relro_at[i] };
            let placed = place(view, bodies[i].symbol, at, &mut words, &mut code[c..c + view.code().len()], &mut data[r..r + view.relro().len()]);
            if let Err(error) = placed {
                done = Err(format_in!(a, "not linked: {} does not place: {error}", bodies[i].function));                return;
            }
        }
        for (i, value) in values.iter().enumerate() {
            let w = (words0 + 8 * i as u64) as usize;
            data[w..w + 8].copy_from_slice(&value.to_le_bytes());
        }
    })
    .map_err(|e| format_in!(a, "the code does not map: {e:?}"))?;
    done?;
    Ok(Linked { exec })
}

// ---- Running -------------------------------------------------------------------------------

/// How a run ended.
enum Ended {
    Exited { status: u8, stdout: AVec<'static, u8> },
    Signaled { signal: c_int, stdout: AVec<'static, u8> },
    TimedOut,
}

/// `fork` on each system: whether this is the child (blaze-sys posix/proc/tests.rs `forked`).
///
/// # Safety
///
/// The child calls only what is async-signal-safe until it exits.
unsafe fn forked() -> Option<i32> {
    // SAFETY: the caller's.
    #[cfg(target_os = "linux")]
    return match unsafe { blaze_sys::posix::proc::fork() }.expect("fork") {
        blaze_sys::linux::Cloned::Child => None,
        blaze_sys::linux::Cloned::Parent(pid) => Some(pid),
    };
    // SAFETY: the caller's.
    #[cfg(target_os = "macos")]
    let child = unsafe { blaze_sys::posix::proc::fork() };
    #[cfg(target_os = "macos")]
    assert!(child >= 0, "fork");
    #[cfg(target_os = "macos")]
    return (child != 0).then_some(child);
}

/// `main` at the start of `linked`'s code called in a child, its standard output read back.
fn ran(linked: &Linked, returns: bool) -> Ended {
    use blaze_sys::Fd;
    use blaze_sys::posix::io::{OwnedFd, close, dup2, pipe, read as read_fd};
    use blaze_sys::posix::proc::{WaitFlags, wait_pid};
    let (read_end, write_end) = pipe().expect("a pipe");
    let (read_end, write_end) = (read_end.into_raw(), write_end.into_raw());
    let main = linked.exec.code_address();
    // SAFETY: the child calls `dup2`, the placed code (which calls only the C names it was
    // linked to, `write` among them) and `_exit`.
    let Some(child) = (unsafe { forked() }) else {
        if dup2(Fd::new(write_end), Fd::new(1)).is_err() {
            blaze_sys::posix::signal::exit_now(125);
        }
        // SAFETY: `main` is the walk's entry, placed at the code's start by `linked`, emitted in
        // the C convention for a function of no parameters (checked by the caller) returning an
        // `i32` where `returns`, else nothing.
        let status = unsafe {
            match returns {
                true => core::mem::transmute::<u64, extern "C" fn() -> i32>(main)(),
                false => {
                    core::mem::transmute::<u64, extern "C" fn()>(main)();
                    0
                }
            }
        };
        blaze_sys::posix::signal::exit_now(status & 0xff);
    };
    // SAFETY: the write end is this process's copy, closed once.
    let _ = close(unsafe { OwnedFd::from_raw(write_end) });
    let mut waited = None;
    for _ in 0..RUN_DEADLINE_MS {
        match wait_pid(child, WaitFlags::NO_HANG) {
            Ok(Some((_, status))) => {
                waited = Some(status);
                break;
            }
            Ok(None) | Err(blaze_sys::Errno::Intr) => {
                let _ = blaze_sys::posix::io::nanosleep(blaze_sys::Timespec { sec: 0, nsec: 1_000_000 });
            }
            Err(error) => panic!("waiting for a rung's run: {error:?}"),
        }
    }
    let Some(status) = waited else {
        let _ = blaze_sys::posix::signal::kill(child, blaze_sys::posix::signal::SIGKILL);
        let _ = wait_pid(child, WaitFlags::NONE);
        // SAFETY: the read end is this process's, closed once.
        let _ = close(unsafe { OwnedFd::from_raw(read_end) });
        return Ended::TimedOut;
    };
    let mut stdout = Vec::new_in(alloc());
    let mut buf = [0u8; 4096];
    while let Ok(n @ 1..) = read_fd(Fd::new(read_end), &mut buf) {
        stdout.extend_from_slice(&buf[..n]);
    }
    // SAFETY: the read end is this process's, closed once.
    let _ = close(unsafe { OwnedFd::from_raw(read_end) });
    match (status.exit_code(), status.signal()) {
        (Some(code), _) => Ended::Exited { status: code as u8, stdout },
        (None, Some(signal)) => Ended::Signaled { signal, stdout },
        (None, None) => panic!("a rung's child neither exited nor was ended by a signal: {status:?}"),
    }
}

// ---- One rung ------------------------------------------------------------------------------

/// What one tier did with one rung.
enum TierRun {
    /// Bodies refused: no run.
    Refused,
    NotLinked(AString<'static>),
    /// `main` takes parameters: not called.
    NotCalled(&'static str),
    Ran(Ended),
}

/// One rung at one tier: emitted, placed and run.
fn at_tier<T: Tier>(tier: &T, bodies: &[Body], machine: Machine) -> (Emits, TierRun) {
    let emits = emit_all(tier, bodies, machine);
    if emits.records.iter().any(|r| r.is_err()) {
        return (emits, TierRun::Refused);
    }
    let mut records: AVec<'_, AVec<'static, u8>> = Vec::with_capacity_in(emits.records.len(), alloc());
    records.extend(emits.records.iter().filter_map(|r| r.as_ref().ok()).map(|bytes| {
        let mut copy = Vec::with_capacity_in(bytes.len(), alloc());
        copy.extend_from_slice(bytes);
        copy
    }));
    let main = bodies[0].func.view();
    let sig = main.sig(SigId(main.head.sig));
    if !sig.params.is_empty() {
        return (emits, TierRun::NotCalled("`main` takes parameters (this harness calls `fn()` and `fn() -> i32`)"));
    }
    let returns = match sig.rets.len() {
        0 => false,
        1 => true,
        _ => return (emits, TierRun::NotCalled("`main` returns more than one value")),
    };
    let mut ids: AVec<'_, Id<'_>> = Vec::with_capacity_in(bodies.len(), alloc());
    ids.extend(bodies.iter().map(|b| Id { symbol: b.symbol, function: b.function.as_str() }));
    let run = match linked(&ids, &records) {
        Err(why) => TierRun::NotLinked(why),
        Ok(linked) => TierRun::Ran(ran(&linked, returns)),
    };
    (emits, run)
}

/// One rung's row of the report.
struct Row {
    rung: &'static Rung,
    /// The walk's bodies, or why it stopped.
    walk: Result<AVec<'static, Body>, AString<'static>>,
    reference: Option<(Emits, TierRun)>,
    fast: Option<(Emits, TierRun)>,
}

/// Every rung at the tiers asked for, the Fast tier over `table`.
fn rows(table: &dyn Table, with_reference: bool) -> AVec<'static, Row> {
    let machine = table.head().machine;
    let fast = Fast { table, regs: &ARM64 };
    let mut out = Vec::with_capacity_in(RUNGS.len(), alloc());
    for rung in RUNGS {
        let walk = walked(rung);
        let (reference, fast) = match &walk {
            Err(_) => (None, None),
            Ok(bodies) => (with_reference.then(|| at_tier(&Reference, bodies, machine)), Some(at_tier(&fast, bodies, machine))),
        };
        out.push(Row { rung, walk, reference, fast });
    }
    out
}

/// Whether a run gave what the rung expects.
fn as_expected(rung: &Rung, ended: &Ended) -> bool {
    let want = rung.expected().unwrap_or_else(|e| panic!("{e}"));
    matches!(ended, Ended::Exited { status, stdout } if *status == want.status && stdout.as_slice() == want.stdout)
}

fn shown(run: &Option<(Emits, TierRun)>, bodies: &[Body], rung: &Rung) -> AString<'static> {
    let a = alloc();
    match run {
        None => AString::from_str_in("-", a),
        Some((emits, run)) => match run {
            TierRun::Refused => {
                let n = emits.refused(bodies).count();
                let (function, class) = emits.refused(bodies).next().expect("a refused body");
                format_in!(a, "REFUSED {n} of {} bodies (first: {function}: {class})", bodies.len())
            }
            TierRun::NotLinked(why) => format_in!(a, "NOT COMPARED: {why}"),
            TierRun::NotCalled(why) => format_in!(a, "NOT COMPARED: {why}"),
            TierRun::Ran(ended) => match (as_expected(rung, ended), ended) {
                (true, _) => AString::from_str_in("PASS", a),
                (false, Ended::Exited { status, stdout }) => format_in!(a, "WRONG: status {status}, stdout {:?}", core::str::from_utf8(stdout).unwrap_or("(not UTF-8)")),
                (false, Ended::Signaled { signal, stdout }) => format_in!(a, "WRONG: signal {signal}, stdout {:?}", core::str::from_utf8(stdout).unwrap_or("(not UTF-8)")),
                (false, Ended::TimedOut) => format_in!(a, "WRONG: not done in {RUN_DEADLINE_MS} ms"),
            },
        },
    }
}

/// The Fast tier's refusal classes over every rung, most bodies first: the worklist.
fn worklist(rows: &[Row]) -> AVec<'static, (AString<'static>, usize, AVec<'static, &'static str>)> {
    let a = alloc();
    let mut classes: AVec<'static, (AString<'static>, usize, AVec<'static, &'static str>)> = Vec::new_in(a);
    for row in rows {
        let (Ok(bodies), Some((emits, _))) = (&row.walk, &row.fast) else { continue };
        for (_, class) in emits.refused(bodies) {
            let i = match classes.iter().position(|c| c.0.as_str() == class) {
                Some(i) => i,
                None => {
                    classes.push((AString::from_str_in(class, a), 0, Vec::new_in(a)));
                    classes.len() - 1
                }
            };
            classes[i].1 += 1;
            if !classes[i].2.contains(&row.rung.name) {
                classes[i].2.push(row.rung.name);
            }
        }
    }
    classes.sort_by(|x, y| y.1.cmp(&x.1).then(x.0.as_str().cmp(y.0.as_str())));
    classes
}

fn ran_ok(run: &Option<(Emits, TierRun)>) -> Option<&Ended> {
    match run {
        Some((_, TierRun::Ran(ended))) => Some(ended),
        _ => None,
    }
}

/// The rungs that must run right at the Reference tier through this harness: each one is added
/// when a run shows it passing (as blaze-ladder tests/ladder_front2.rs `PASSING`), and a listed
/// rung that does not pass fails the test. Empty until the first run of this test is read.
const PASSING_AT_REFERENCE: &[&str] = &[];

/// The rungs that must run right at the Fast tier: as [`PASSING_AT_REFERENCE`]. A rung joins
/// when piece (d) has covered every class its bodies meet.
const PASSING_AT_FAST: &[&str] = &[];

/// Every rung at both tiers. Prints one line per rung, the Fast tier's worklist (every refused
/// body by class) and the rungs a tier ran WRONG, which are worklist rows too: a rung that
/// cannot pass yet reports, it does not fail. What fails: a rung of [`PASSING_AT_REFERENCE`] or
/// [`PASSING_AT_FAST`] that does not run right at that tier (the ratchet), and a rung listed
/// that is not in blaze-rungs.
/// Expected: passes.
#[test]
fn every_rung_at_reference_and_fast() {
    for name in PASSING_AT_REFERENCE.iter().chain(PASSING_AT_FAST) {
        assert!(blaze_rungs::rung(name).is_some(), "the passing list names `{name}`, which is no rung of blaze-rungs");
    }
    let table = EncodedTable::read(TABLE, alloc()).unwrap_or_else(|e| panic!("the committed table crates/blaze-emit/table/arm64.bztab does not read: {e}"));
    let rows = rows(&table, true);
    let mut wrong: AVec<'_, AString<'_>> = Vec::new_in(alloc());
    let mut regressed: AVec<'_, AString<'_>> = Vec::new_in(alloc());
    let (mut reference_ran, mut fast_ran) = (0, 0);
    println!("| rung | bodies | reference | fast |");
    println!("|---|---|---|---|");
    for row in rows.iter() {
        let bodies = match &row.walk {
            Err(why) => {
                println!("| {} | - | WALK: {why} | - |", row.rung.name);
                for (tier, listed) in [(TierName::Reference, PASSING_AT_REFERENCE), (TierName::Fast, PASSING_AT_FAST)] {
                    if listed.contains(&row.rung.name) {
                        regressed.push(format_in!(alloc(), "{} at {}: the walk stopped: {why}", row.rung.name, tier.as_str()));
                    }
                }
                continue;
            }
            Ok(bodies) => bodies,
        };
        println!("| {} | {} | {} | {} |", row.rung.name, bodies.len(), shown(&row.reference, bodies, row.rung), shown(&row.fast, bodies, row.rung));
        for (tier, run, listed) in [(TierName::Reference, &row.reference, PASSING_AT_REFERENCE), (TierName::Fast, &row.fast, PASSING_AT_FAST)] {
            let right = ran_ok(run).is_some_and(|ended| as_expected(row.rung, ended));
            if let Some(ended) = ran_ok(run) {
                match tier {
                    TierName::Reference => reference_ran += 1,
                    _ => fast_ran += 1,
                }
                if !as_expected(row.rung, ended) {
                    wrong.push(format_in!(alloc(), "{} at {}: {}", row.rung.name, tier.as_str(), shown(run, bodies, row.rung)));
                }
            }
            if listed.contains(&row.rung.name) && !right {
                regressed.push(format_in!(alloc(), "{} at {}: {}", row.rung.name, tier.as_str(), shown(run, bodies, row.rung)));
            }
        }
    }
    println!();
    println!("FAST WORKLIST (piece (d)): every body's refusal, by class");
    println!("| class | bodies | rungs |");
    println!("|---|---|---|");
    for (class, bodies, rungs) in worklist(&rows).iter() {
        println!("| {class} | {bodies} | {} |", rungs.join(", "));
    }
    println!();
    println!("RAN WRONG (worklist rows: a tier's code or this harness):");
    for w in wrong.iter() {
        println!("  {w}");
    }
    println!();
    println!("RAN: reference {reference_ran} of {} rungs, fast {fast_ran}", rows.len());
    assert!(regressed.is_empty(), "a rung listed as passing does not:\n{}", regressed.iter().map(|w| w.as_str()).collect::<std::vec::Vec<_>>().join("\n"));
}

// ---- The planted control -------------------------------------------------------------------

/// The planted wrong template (blaze-emit tier_tests.rs `WrongTemplate`): every `add` template
/// of `inner` with bit 30 of its first word set (byte 3's bit 6: an arm64 `add`'s `op` bit, so
/// `add` becomes `sub`). Every other pattern is served by `inner` unchanged.
struct WrongTemplate<'t> {
    inner: &'t EncodedTable<'static>,
    /// `(pattern, first byte in bytes, length)`, sorted by pattern (the table's order).
    adds: AVec<'static, (Pattern, u32, u32)>,
    bytes: AVec<'static, u8>,
}

impl<'t> WrongTemplate<'t> {
    fn over(inner: &'t EncodedTable<'static>) -> WrongTemplate<'t> {
        let mut adds: AVec<'static, (Pattern, u32, u32)> = Vec::new_in(alloc());
        let mut bytes: AVec<'static, u8> = Vec::new_in(alloc());
        for pattern in inner.patterns() {
            if pattern.op != Op::Add {
                continue;
            }
            let Some(template) = inner.find(pattern) else {
                panic!("the table lists `add` at width {} and has no template for it", pattern.width);
            };
            assert!(template.bytes.len() >= 4, "an arm64 template is whole words");
            let at = bytes.len();
            bytes.extend_from_slice(template.bytes);
            bytes[at + 3] |= 0x40;
            adds.push((*pattern, at as u32, template.bytes.len() as u32));
        }
        assert!(!adds.is_empty(), "the committed table has no `add` template: the plant would plant nothing");
        WrongTemplate { inner, adds, bytes }
    }
}

impl Table for WrongTemplate<'_> {
    fn head(&self) -> TableHead {
        self.inner.head()
    }

    fn find(&self, pattern: &Pattern) -> Option<Template<'_>> {
        let template = self.inner.find(pattern)?;
        if pattern.op != Op::Add {
            return Some(template);
        }
        let i = self.adds.binary_search_by(|row| row.0.cmp(pattern)).ok()?;
        let (_, at, len) = self.adds[i];
        Some(Template { bytes: &self.bytes[at as usize..(at + len) as usize], ..template })
    }
}

/// Generated and hand-built functions name nothing outside themselves.
struct Nameless;

impl Names for Nameless {
    fn name(&self, _: Outside) -> Option<Named<'_>> {
        None
    }

    fn routines(&self) -> blaze_tier::RoutinePlaces {
        blaze_tier::RoutinePlaces::NONE
    }
}

/// `extern "C" fn(a: i64, b: i64) -> i64 { a + b }` in LIR, verified.
fn add_func() -> VerifiedFunc<Alloc<'static>> {
    let mut b = LirBuilder::<Alloc<'static>, Full>::new_in(64, alloc());
    let sig = b.sig(Conv::C, Variadic::No, &[Param::of(Ty::I64), Param::of(Ty::I64)], &[Param::of(Ty::I64)]);
    b.set_signature(sig);
    let entry = b.create_block(BlockKind::Normal);
    b.begin(entry);
    let (x, y) = (b.param(Ty::I64), b.param(Ty::I64));
    let sum = b.binary(Op::Add, Flags::NONE, x, y);
    b.ret(&[sum]);
    let func = b.finish_checked().expect("the add function builds");
    VerifiedFunc::new(func).unwrap_or_else(|e| panic!("the add function does not verify: {e}"))
}

/// `func` emitted by `tier` through the seam, sealed, encoded, read back, placed alone by the
/// rung linker ([`linked`]) and called in this process with `(40, 2)`: straight-line code that
/// can neither trap nor loop. `Err` names why it was not called.
fn added<T: Tier>(tier: &T, func: &VerifiedFunc<Alloc<'static>>, machine: Machine) -> Result<i64, AString<'static>> {
    let a = alloc();
    let emitted = tier.emit(func.verified(), &Nameless, machine, a).map_err(|e| format_in!(a, "{} refused it: {}", T::NAME.as_str(), class(&e)))?;
    let sealed = seal(emitted).map_err(|e| format_in!(a, "the record does not seal: {e}"))?;
    let mut bytes = Vec::new_in(a);
    encode(&sealed, &mut bytes);
    let symbol = Symbol::of_name("blaze-rung-tiers: the add function");
    let placed = linked(&[Id { symbol, function: "add" }], core::slice::from_ref(&bytes))?;
    // SAFETY: the record's code is at the mapping's start, emitted from `add_func`'s signature:
    // the C convention, two `i64` in `x0` and `x1`, one `i64` in `x0`; it calls nothing.
    let f = unsafe { core::mem::transmute::<u64, extern "C" fn(i64, i64) -> i64>(placed.exec.code_address()) };
    Ok(f(40, 2))
}

/// The planted control at the grain the Fast tier runs today: one `add`, emitted at the
/// Reference tier, at the Fast tier over the committed table and at the Fast tier over
/// [`WrongTemplate`], placed by the same linker as the rungs and called. The two honest tiers
/// give 42; the planted table gives other than 42 (a `sub`: 38). So the harness's emit, seal,
/// place and call path sees a wrong template, which [`every_rung_at_reference_and_fast`] relies
/// on while no rung runs whole at the Fast tier.
/// Expected: passes.
#[test]
fn a_wrong_add_template_changes_an_add() {
    let table = EncodedTable::read(TABLE, alloc()).unwrap_or_else(|e| panic!("the committed table does not read: {e}"));
    let machine = table.head().machine;
    let func = add_func();
    let reference = added(&Reference, &func, machine).unwrap_or_else(|why| panic!("{why}"));
    let honest = added(&Fast { table: &table, regs: &ARM64 }, &func, machine).unwrap_or_else(|why| panic!("{why}"));
    let plant = WrongTemplate::over(&table);
    let planted = added(&Fast { table: &plant, regs: &ARM64 }, &func, machine).unwrap_or_else(|why| panic!("{why}"));
    assert_eq!(reference, 42, "the Reference tier's add of 40 and 2");
    assert_eq!(honest, 42, "the Fast tier's add of 40 and 2 over the committed table");
    assert_ne!(planted, 42, "the Fast tier over the planted `sub` table still added: the plant is not seen");
}

/// The control of [`every_rung_at_reference_and_fast`] at the grain of a whole rung: with every
/// `add` template made `sub`, at least one rung the Fast tier runs right gives other than its
/// `expected`. A rung whose `main` reaches no `add` at the Fast tier runs right under the plant
/// too; one rung is enough. Ignored until a rung runs whole at the Fast tier (then a rung of
/// `PASSING_AT_FAST` makes it meaningful, and the ignore goes).
/// Expected: passes once the Fast tier runs a rung whose result goes through an `add`.
#[test]
#[ignore = "no rung runs whole at Fast yet: the worklist"]
fn a_wrong_add_template_makes_a_rung_differ() {
    let table = EncodedTable::read(TABLE, alloc()).unwrap_or_else(|e| panic!("the committed table does not read: {e}"));
    let honest = rows(&table, false);
    let plant = WrongTemplate::over(&table);
    let planted = rows(&plant, false);
    let mut ran_right: AVec<'_, &str> = Vec::new_in(alloc());
    let mut differs: AVec<'_, &str> = Vec::new_in(alloc());
    for (h, p) in honest.iter().zip(planted.iter()) {
        let Some(good) = ran_ok(&h.fast) else { continue };
        if !as_expected(h.rung, good) {
            continue;
        }
        ran_right.push(h.rung.name);
        match ran_ok(&p.fast) {
            Some(bad) if as_expected(p.rung, bad) => {}
            // Run wrong, or no longer emitted or linked: either way the plant was seen.
            _ => differs.push(p.rung.name),
        }
    }
    println!("PLANT: {} rungs run right at the Fast tier; under the wrong `add` these differ: {:?}", ran_right.len(), differs.as_slice());
    assert!(
        !differs.is_empty(),
        "the wrong `add` template changed no rung's run: the Fast tier runs these rungs right, none through an `add`: {:?}",
        ran_right.as_slice()
    );
}
