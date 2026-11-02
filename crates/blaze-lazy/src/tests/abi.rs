//! The stubs' ABI on this machine: a call through a lazy function's entry, whose resolver
//! calls a hook that overwrites every argument register (general and vector, whole), gives
//! the same result as calling the function directly, for every class of argument and
//! result: integers and floats past the registers onto the stack, mixed, a large aggregate
//! passed by reference and returned through the indirect result register, small aggregates
//! in register pairs, homogeneous float aggregates, whole vector registers, and a variadic
//! call (`snprintf`, which reads `al` on x86-64). A hook that returns 0 traps.

#![allow(unsafe_code)]

use std::sync::OnceLock;
use std::sync::atomic::{AtomicU32, Ordering};

use super::exec::Code;
use crate::{
    LazyId,
    stub::{self, Isa, Layout},
};

const COUNT: usize = 16;
const INTS: u32 = 0;
const FLOATS: u32 = 1;
const MIXED: u32 = 2;
const BIG: u32 = 3;
const PAIR: u32 = 4;
const HFA: u32 = 5;
const VECTOR: u32 = 6;
const SNPRINTF: u32 = 7;
/// Its hook answers 0.
const TRAP: u32 = 15;

static CALLS: [AtomicU32; COUNT] = [const { AtomicU32::new(0) }; COUNT];

fn targets() -> &'static [u64; COUNT] {
    static TARGETS: OnceLock<[u64; COUNT]> = OnceLock::new();
    TARGETS.get_or_init(|| {
        let mut out = [0; COUNT];
        out[INTS as usize] = ints as *const () as u64;
        out[FLOATS as usize] = floats as *const () as u64;
        out[MIXED as usize] = mixed as *const () as u64;
        out[BIG as usize] = big as *const () as u64;
        out[PAIR as usize] = pair as *const () as u64;
        out[HFA as usize] = hfa as *const () as u64;
        out[VECTOR as usize] = vector as *const () as u64;
        out[SNPRINTF as usize] = libc::snprintf as *const () as u64;
        out
    })
}

extern "C" fn hook(lazy: u32) -> u64 {
    clobber();
    let Some(calls) = CALLS.get(lazy as usize) else { return 0 };
    calls.fetch_add(1, Ordering::Relaxed);
    targets()[lazy as usize]
}

fn region() -> &'static (Code, Layout) {
    static REGION: OnceLock<(Code, Layout)> = OnceLock::new();
    REGION.get_or_init(|| {
        let _ = targets();
        let isa = Isa::host().expect("a host with lazy stubs");
        let stubs = stub::generate(&isa, COUNT as u32, hook as *const () as u64).expect("the region");
        (Code::new(&stubs.bytes), stubs.layout)
    })
}

/// The entry of `lazy`, as a function of type `F`.
fn through<F: Copy>(lazy: u32) -> F {
    assert_eq!(size_of::<F>(), size_of::<usize>());
    let (code, layout) = region();
    let entry = code.address(layout.entry(LazyId(lazy))) as usize;
    // SAFETY: `F` is the function pointer type of the function `lazy`'s hook answers with,
    // and the entry forwards every argument and result of it unchanged.
    unsafe { std::mem::transmute_copy::<usize, F>(&entry) }
}

fn calls(lazy: u32) -> u32 {
    CALLS[lazy as usize].load(Ordering::Relaxed)
}

/// Overwrites every register a call may carry an argument in, and the indirect result
/// register, with all ones.
#[cfg(target_arch = "x86_64")]
fn clobber() {
    // SAFETY: writes only the registers it names as outputs; touches no memory or stack.
    unsafe {
        std::arch::asm!(
            "mov rax, -1", "mov rcx, -1", "mov rdx, -1", "mov rsi, -1", "mov rdi, -1",
            "mov r8, -1", "mov r9, -1", "mov r10, -1", "mov r11, -1",
            "pcmpeqd xmm0, xmm0", "pcmpeqd xmm1, xmm1", "pcmpeqd xmm2, xmm2", "pcmpeqd xmm3, xmm3",
            "pcmpeqd xmm4, xmm4", "pcmpeqd xmm5, xmm5", "pcmpeqd xmm6, xmm6", "pcmpeqd xmm7, xmm7",
            "pcmpeqd xmm8, xmm8", "pcmpeqd xmm9, xmm9", "pcmpeqd xmm10, xmm10", "pcmpeqd xmm11, xmm11",
            "pcmpeqd xmm12, xmm12", "pcmpeqd xmm13, xmm13", "pcmpeqd xmm14, xmm14", "pcmpeqd xmm15, xmm15",
            out("rax") _, out("rcx") _, out("rdx") _, out("rsi") _, out("rdi") _,
            out("r8") _, out("r9") _, out("r10") _, out("r11") _,
            out("xmm0") _, out("xmm1") _, out("xmm2") _, out("xmm3") _,
            out("xmm4") _, out("xmm5") _, out("xmm6") _, out("xmm7") _,
            out("xmm8") _, out("xmm9") _, out("xmm10") _, out("xmm11") _,
            out("xmm12") _, out("xmm13") _, out("xmm14") _, out("xmm15") _,
            options(nomem, nostack),
        );
    }
}

#[cfg(target_arch = "aarch64")]
fn clobber() {
    // SAFETY: writes only the registers it names as outputs; touches no memory or stack.
    unsafe {
        std::arch::asm!(
            "mov x0, #-1", "mov x1, #-1", "mov x2, #-1", "mov x3, #-1",
            "mov x4, #-1", "mov x5, #-1", "mov x6, #-1", "mov x7, #-1",
            "mov x8, #-1", "mov x9, #-1", "mov x10, #-1", "mov x11, #-1",
            "mov x12, #-1", "mov x13, #-1", "mov x14, #-1", "mov x15, #-1",
            "movi v0.2d, #0xffffffffffffffff", "movi v1.2d, #0xffffffffffffffff",
            "movi v2.2d, #0xffffffffffffffff", "movi v3.2d, #0xffffffffffffffff",
            "movi v4.2d, #0xffffffffffffffff", "movi v5.2d, #0xffffffffffffffff",
            "movi v6.2d, #0xffffffffffffffff", "movi v7.2d, #0xffffffffffffffff",
            "movi v8.2d, #0xffffffffffffffff", "movi v9.2d, #0xffffffffffffffff",
            "movi v10.2d, #0xffffffffffffffff", "movi v11.2d, #0xffffffffffffffff",
            "movi v12.2d, #0xffffffffffffffff", "movi v13.2d, #0xffffffffffffffff",
            "movi v14.2d, #0xffffffffffffffff", "movi v15.2d, #0xffffffffffffffff",
            "movi v16.2d, #0xffffffffffffffff", "movi v17.2d, #0xffffffffffffffff",
            "movi v18.2d, #0xffffffffffffffff", "movi v19.2d, #0xffffffffffffffff",
            "movi v20.2d, #0xffffffffffffffff", "movi v21.2d, #0xffffffffffffffff",
            "movi v22.2d, #0xffffffffffffffff", "movi v23.2d, #0xffffffffffffffff",
            "movi v24.2d, #0xffffffffffffffff", "movi v25.2d, #0xffffffffffffffff",
            "movi v26.2d, #0xffffffffffffffff", "movi v27.2d, #0xffffffffffffffff",
            "movi v28.2d, #0xffffffffffffffff", "movi v29.2d, #0xffffffffffffffff",
            "movi v30.2d, #0xffffffffffffffff", "movi v31.2d, #0xffffffffffffffff",
            out("x0") _, out("x1") _, out("x2") _, out("x3") _,
            out("x4") _, out("x5") _, out("x6") _, out("x7") _,
            out("x8") _, out("x9") _, out("x10") _, out("x11") _,
            out("x12") _, out("x13") _, out("x14") _, out("x15") _,
            out("v0") _, out("v1") _, out("v2") _, out("v3") _,
            out("v4") _, out("v5") _, out("v6") _, out("v7") _,
            out("v8") _, out("v9") _, out("v10") _, out("v11") _,
            out("v12") _, out("v13") _, out("v14") _, out("v15") _,
            out("v16") _, out("v17") _, out("v18") _, out("v19") _,
            out("v20") _, out("v21") _, out("v22") _, out("v23") _,
            out("v24") _, out("v25") _, out("v26") _, out("v27") _,
            out("v28") _, out("v29") _, out("v30") _, out("v31") _,
            options(nomem, nostack),
        );
    }
}

/// Each argument weighed by its position, so a swapped or lost one changes the result.
extern "C" fn ints(a: u64, b: i32, c: u16, d: i8, e: u64, f: u64, g: u64, h: u64, i: u64, j: u64) -> u64 {
    let all = [a, b as u64, u64::from(c), d as u64, e, f, g, h, i, j];
    all.iter().enumerate().fold(0u64, |sum, (n, it)| sum.wrapping_mul(1_000_003).wrapping_add(it ^ n as u64))
}

extern "C" fn floats(a: f64, b: f32, c: f64, d: f64, e: f64, f: f64, g: f64, h: f64, i: f64, j: f32, k: f64) -> f64 {
    let all = [a, f64::from(b), c, d, e, f, g, h, i, f64::from(j), k];
    all.iter().enumerate().fold(0.0, |sum, (n, it)| sum * 3.0 + it * (n + 1) as f64)
}

extern "C" fn mixed(
    a: u8,
    x: f64,
    b: u64,
    y: f32,
    c: i64,
    z: f64,
    d: u32,
    w: f64,
    e: u64,
    v: f64,
    f: u64,
    u: f64,
    g: u64,
    t: f64,
    h: u64,
    s: f64,
    k: f64,
    l: u64,
) -> f64 {
    let ints = [u64::from(a), b, c as u64, u64::from(d), e, f, g, h, l];
    let floats = [x, f64::from(y), z, w, v, u, t, s, k];
    let ints = ints.iter().enumerate().fold(0u64, |sum, (n, it)| sum.wrapping_mul(31).wrapping_add(it ^ n as u64));
    floats.iter().enumerate().fold(ints as f64, |sum, (n, it)| sum * 0.5 + it * (n + 2) as f64)
}

#[repr(C)]
#[derive(Clone, Copy, PartialEq, Debug)]
struct Big([u64; 6]);

extern "C" fn big(x: Big, k: u64) -> Big {
    let mut out = x.0;
    out.reverse();
    for (n, it) in out.iter_mut().enumerate() {
        *it = it.wrapping_mul(k).wrapping_add(n as u64);
    }
    Big(out)
}

#[repr(C)]
#[derive(Clone, Copy, PartialEq, Debug)]
struct Pair {
    a: u64,
    b: f64,
}

extern "C" fn pair(p: Pair, q: Pair) -> Pair {
    Pair { a: p.a.wrapping_mul(7) ^ q.a, b: p.b * 2.0 - q.b }
}

#[repr(C)]
#[derive(Clone, Copy, PartialEq, Debug)]
struct Quad([f32; 4]);

extern "C" fn hfa(a: Quad, b: Quad) -> Quad {
    Quad([a.0[0] - b.0[3], a.0[1] * b.0[2], a.0[2] + b.0[1], a.0[3] / b.0[0]])
}

#[cfg(target_arch = "x86_64")]
type Vector = std::arch::x86_64::__m128i;
#[cfg(target_arch = "aarch64")]
type Vector = std::arch::aarch64::uint64x2_t;

fn vector_of(lanes: [u64; 2]) -> Vector {
    // SAFETY: a vector of two 64-bit lanes is 16 plain bytes either way.
    unsafe { std::mem::transmute::<[u64; 2], Vector>(lanes) }
}

fn lanes_of(vector: Vector) -> [u64; 2] {
    // SAFETY: as in `vector_of`.
    unsafe { std::mem::transmute::<Vector, [u64; 2]>(vector) }
}

extern "C" fn vector(a: Vector, b: Vector, c: Vector) -> Vector {
    let (a, b, c) = (lanes_of(a), lanes_of(b), lanes_of(c));
    vector_of([a[0] ^ b[1] ^ c[0].rotate_left(7), a[1].wrapping_add(b[0]) ^ c[1]])
}

#[test]
fn integers_past_the_registers_go_through_whole() {
    type F = extern "C" fn(u64, i32, u16, i8, u64, u64, u64, u64, u64, u64) -> u64;
    let lazy: F = through(INTS);
    let before = calls(INTS);
    let args = (u64::MAX - 3, -77, 0xbeef, -5, 1 << 40, 6, 7, 8, 0x9999_0000_1111, 10);
    let direct = ints(args.0, args.1, args.2, args.3, args.4, args.5, args.6, args.7, args.8, args.9);
    let via = lazy(args.0, args.1, args.2, args.3, args.4, args.5, args.6, args.7, args.8, args.9);
    assert_eq!(via, direct);
    assert!(calls(INTS) > before, "the call went through the hook");
}

#[test]
fn floats_past_the_registers_go_through_whole() {
    type F = extern "C" fn(f64, f32, f64, f64, f64, f64, f64, f64, f64, f32, f64) -> f64;
    let lazy: F = through(FLOATS);
    let direct = floats(1.5, 2.25, -3.0, 4.125, 5e10, -6e-3, 7.0, 8.5, 9.75, -10.5, 11.0);
    let via = lazy(1.5, 2.25, -3.0, 4.125, 5e10, -6e-3, 7.0, 8.5, 9.75, -10.5, 11.0);
    assert_eq!(via.to_bits(), direct.to_bits());
}

#[test]
fn mixed_integers_and_floats_go_through_whole() {
    type F = extern "C" fn(
        u8,
        f64,
        u64,
        f32,
        i64,
        f64,
        u32,
        f64,
        u64,
        f64,
        u64,
        f64,
        u64,
        f64,
        u64,
        f64,
        f64,
        u64,
    ) -> f64;
    let lazy: F = through(MIXED);
    let direct = mixed(
        200, 1.0, 2, 3.5, -4, 5.25, 6, 7.75, 8, 9.0, 10, 11.5, 12, 13.0, 14, 15.5, 16.25, 17,
    );
    let via = lazy(200, 1.0, 2, 3.5, -4, 5.25, 6, 7.75, 8, 9.0, 10, 11.5, 12, 13.0, 14, 15.5, 16.25, 17);
    assert_eq!(via.to_bits(), direct.to_bits());
}

#[test]
fn a_large_aggregate_and_its_indirect_result_go_through() {
    type F = extern "C" fn(Big, u64) -> Big;
    let lazy: F = through(BIG);
    let x = Big([1, u64::MAX, 3, 1 << 63, 5, 0x0123_4567_89ab_cdef]);
    assert_eq!(lazy(x, 0x1_0001), big(x, 0x1_0001));
}

#[test]
fn small_aggregates_in_register_pairs_go_through() {
    type F = extern "C" fn(Pair, Pair) -> Pair;
    let lazy: F = through(PAIR);
    let (p, q) = (Pair { a: 0xdead_beef, b: 1.25 }, Pair { a: u64::MAX, b: -0.5 });
    assert_eq!(lazy(p, q), pair(p, q));
    type H = extern "C" fn(Quad, Quad) -> Quad;
    let lazy: H = through(HFA);
    let (a, b) = (Quad([1.0, 2.0, 3.0, 4.0]), Quad([-0.5, 0.25, 8.0, 16.0]));
    assert_eq!(lazy(a, b), hfa(a, b));
}

#[test]
fn whole_vector_registers_go_through() {
    type F = extern "C" fn(Vector, Vector, Vector) -> Vector;
    let lazy: F = through(VECTOR);
    let (a, b, c) = (
        vector_of([0x0102_0304_0506_0708, 0x1112_1314_1516_1718]),
        vector_of([u64::MAX, 0]),
        vector_of([0xa5a5_a5a5_5a5a_5a5a, 0x8000_0000_0000_0001]),
    );
    assert_eq!(lanes_of(lazy(a, b, c)), lanes_of(vector(a, b, c)));
}

#[test]
fn a_variadic_call_keeps_its_vector_count() {
    type F = unsafe extern "C" fn(*mut libc::c_char, libc::size_t, *const libc::c_char, ...) -> libc::c_int;
    let lazy: F = through(SNPRINTF);
    let format = c"%d %.3f %s %.1f %lld %.2f %.2f %.2f %.2f %.2f %.2f %.2f";
    let word = c"word";
    let mut direct = [0u8; 256];
    let mut via = [0u8; 256];
    // SAFETY: the buffers are 256 bytes, and the arguments are what the format names.
    let (a, b) = unsafe {
        (
            libc::snprintf(
                direct.as_mut_ptr().cast(),
                direct.len(),
                format.as_ptr(),
                42,
                1.5f64,
                word.as_ptr(),
                -2.25f64,
                1i64 << 40,
                3.0f64,
                4.0f64,
                5.0f64,
                6.0f64,
                7.0f64,
                8.0f64,
                9.0f64,
            ),
            lazy(
                via.as_mut_ptr().cast(),
                via.len(),
                format.as_ptr(),
                42,
                1.5f64,
                word.as_ptr(),
                -2.25f64,
                1i64 << 40,
                3.0f64,
                4.0f64,
                5.0f64,
                6.0f64,
                7.0f64,
                8.0f64,
                9.0f64,
            ),
        )
    };
    assert_eq!(a, b);
    assert_eq!(direct, via, "{:?}", String::from_utf8_lossy(&via));
}

#[test]
fn a_hook_that_answers_zero_traps_instead_of_jumping() {
    let lazy: extern "C" fn() = through(TRAP);
    // SAFETY: the child only calls the entry, which reaches the hook (no allocation, no
    // lock: the region and targets were made before the fork) and traps; it never returns
    // into the test harness.
    let child = unsafe { libc::fork() };
    assert!(child >= 0, "fork: {}", std::io::Error::last_os_error());
    if child == 0 {
        lazy();
        // SAFETY: leaves the child at once, running nothing of the parent's.
        unsafe { libc::_exit(0) };
    }
    let mut status = 0;
    // SAFETY: waits for the child just forked.
    let waited = unsafe { libc::waitpid(child, &mut status, 0) };
    assert_eq!(waited, child);
    assert!(libc::WIFSIGNALED(status), "the child exited with {status:#x} instead of trapping");
    let signal = libc::WTERMSIG(status);
    assert!(signal == libc::SIGILL || signal == libc::SIGTRAP, "signal {signal}");
}
