//! Each stencil against `cranelift-interpreter` and Cranelift's code (stencil.md, "Tests"):
//! for every shape of the set, the function that selects exactly it ([`build`]), run on its
//! edges and seeded random inputs three ways (the interpreter, Cranelift's code, the stencil
//! code), with every failure of a shape collected (seed, inputs, the three results, the CLIF).
//!
//! The per-shape runs are of the host's variant (x86-64 Linux: `x86_64`; AArch64 Linux:
//! `aarch64`; AArch64 macOS: `aarch64_lse`); the other variants are covered by those hosts'
//! CI. The compile-only test (each shape's function compiles with the stencil backend exactly
//! when Cranelift compiles it, naming only the libcalls Cranelift's code names) covers all
//! four targets everywhere, and the library test all three libraries.
//!
//! `BLAZE_STENCIL_SHAPES_ALL=1` runs every edge pair and many more random inputs per shape.

mod build;

use cranelift_codegen::ir;

use self::build::{Baked, OFFSET, build};
use super::{
    host,
    rng::{Rng, edges},
};
use crate::{
    Target,
    code::{Code, Name},
    enc::Form,
    shape::{self, Disp, Dst, Isa, Op, Shape, Src, Ty},
    table::{
        Library, Variant,
        format::{self, HoleKind, Stencil},
    },
};

const SEED: u64 = 0x5354_454e_4349_4c00;

fn seed(shape: Shape) -> u64 {
    SEED ^ (u64::from(shape.key()) << 4)
}

/// How many inputs each shape is run on.
#[derive(Clone, Copy, Debug)]
#[cfg_attr(not(all(unix, any(target_arch = "x86_64", target_arch = "aarch64"))), allow(dead_code))]
struct Budget {
    /// Edge combinations of the operands (all of them when there are no more).
    pairs: usize,
    /// Seeded random inputs after the edges.
    random: usize,
    /// Functions of a shape made with immediates or constants: one per value.
    baked: usize,
    /// Inputs of each of those functions.
    per_baked: usize,
}

impl Budget {
    fn get() -> Budget {
        if std::env::var("BLAZE_STENCIL_SHAPES_ALL").as_deref() == Ok("1") {
            Budget { pairs: 4096, random: 256, baked: 256, per_baked: 64 }
        } else {
            Budget { pairs: 48, random: 16, baked: 24, per_baked: 8 }
        }
    }

    /// The compile-only test's: one function per shape.
    const ONE: Budget = Budget { pairs: 0, random: 0, baked: 1, per_baked: 0 };
}

const fn sign_extend(value: i64, bits: u32) -> i64 {
    if bits >= 64 {
        return value;
    }
    let shift = 64 - bits;
    (value << shift) >> shift
}

fn le_i64(bytes: &[u8]) -> i64 {
    let mut wide = [0u8; 8];
    let n = bytes.len().min(8);
    wide[..n].copy_from_slice(&bytes[..n]);
    i64::from_le_bytes(wide)
}

fn padded(bytes: &[u8]) -> [u8; 16] {
    let mut out = [0u8; 16];
    out[..bytes.len()].copy_from_slice(bytes);
    out
}

fn push_new<T: PartialEq>(out: &mut Vec<T>, it: T) {
    if !out.contains(&it) {
        out.push(it);
    }
}

/// Up to `cap` picks of one element from each list: every combination when there are no more,
/// else each element of each list at least once (while `cap` allows) and seeded picks after.
fn combos<T: Copy>(lists: &[Vec<T>], cap: usize, rng: &mut Rng) -> Vec<Vec<T>> {
    if lists.iter().any(|it| it.is_empty()) {
        return Vec::new();
    }
    let product = lists.iter().try_fold(1usize, |n, it| n.checked_mul(it.len()));
    let mut out = Vec::new();
    match product {
        Some(n) if n <= cap => {
            for k in 0..n {
                let mut rest = k;
                out.push(
                    lists
                        .iter()
                        .map(|list| {
                            let pick = list[rest % list.len()];
                            rest /= list.len();
                            pick
                        })
                        .collect(),
                );
            }
        }
        _ => {
            let longest = lists.iter().map(Vec::len).max().unwrap_or(0);
            for k in 0..longest.min(cap) {
                out.push(lists.iter().map(|list| list[k % list.len()]).collect());
            }
            while out.len() < cap {
                out.push(lists.iter().map(|list| list[rng.below(list.len() as u64) as usize]).collect());
            }
        }
    }
    out
}

/// Whether an `iconst` of `ty` holding `value` (sign-extended from the width) folds into a
/// stencil's 32-bit immediate (`patch/select.rs`'s `folds`).
fn folds(ty: Ty, value: i64) -> bool {
    ty.bytes() <= 4 || i32::try_from(value).is_ok()
}

/// The shift amounts every shift and rotate of `lane` is tried at besides the `i8` edges: up
/// to and past its width and twice it.
fn amounts(lane: Ty) -> [u8; 6] {
    let bits = u32::from(lane.bytes()) * 8;
    [bits - 1, bits, bits + 1, 2 * bits - 1, 2 * bits, 2 * bits + 1].map(|it| it as u8)
}

/// The immediates operand `n` of `shape` is tried at: its type's edges that fold, then random
/// ones, `cap` in all.
fn imm_values(shape: Shape, n: usize, cap: usize, rng: &mut Rng) -> Vec<i64> {
    let ty = shape.arg(n);
    let bits = u32::from(ty.bytes()) * 8;
    let mut out = Vec::new();
    for edge in edges(ty) {
        let value = sign_extend(le_i64(&edge), bits);
        if folds(ty, value) {
            push_new(&mut out, value);
        }
    }
    if matches!(shape.op, Op::Ishl | Op::Ushr | Op::Sshr | Op::Rotl | Op::Rotr) && n == 1 {
        for amount in amounts(shape.ty.lane()) {
            push_new(&mut out, sign_extend(i64::from(amount), 8));
        }
    }
    out.truncate(cap.saturating_sub(cap / 3).max(1));
    while out.len() < cap {
        let raw = rng.next() as i64;
        let value = if bits >= 64 { i64::from(raw as i32) } else { sign_extend(raw, bits) };
        out.push(value);
    }
    out
}

/// The `iconst`s an `iconst` stencil of `shape` is tried at: values that fit 32 bits signed
/// for `aux` 0, ones that don't for `aux` 1.
fn iconst_values(shape: Shape, cap: usize, rng: &mut Rng) -> Vec<i64> {
    let bits = u32::from(shape.ty.bytes()) * 8;
    let wide = shape.aux == 1;
    let mut out = Vec::new();
    for edge in edges(shape.ty) {
        let value = sign_extend(le_i64(&edge), bits);
        if i32::try_from(value).is_ok() != wide {
            push_new(&mut out, value);
        }
    }
    out.truncate(cap.saturating_sub(cap / 3).max(1));
    while out.len() < cap {
        let raw = rng.next() as i64;
        let value = match (wide, bits) {
            (true, _) if i32::try_from(raw).is_err() => raw,
            (true, _) => continue,
            (false, 64) => i64::from(raw as i32),
            (false, _) => sign_extend(raw, bits),
        };
        out.push(value);
    }
    out
}

/// The bits an `fconst` stencil of `shape` is tried at, as the patcher's `Imm0` sees them:
/// for `f64`, `aux` 0 is bits that fit 32 bits signed, `aux` 1 the others.
fn fconst_values(shape: Shape, cap: usize, rng: &mut Rng) -> Vec<i64> {
    let wide = shape.aux == 1;
    let mut out = Vec::new();
    for edge in edges(shape.ty) {
        let bits = le_i64(&edge);
        let fits = i32::try_from(bits).is_ok();
        if shape.ty != Ty::F64 || fits != wide {
            push_new(&mut out, bits);
        }
    }
    out.truncate(cap.saturating_sub(cap / 3).max(1));
    while out.len() < cap {
        let raw = rng.next();
        let bits = match shape.ty {
            Ty::F16 => i64::from(raw as u16),
            Ty::F32 => i64::from(raw as u32),
            _ if wide && i32::try_from(raw as i64).is_err() => raw as i64,
            _ if wide => continue,
            _ => i64::from(raw as i32),
        };
        out.push(bits);
    }
    out
}

/// Shuffle masks: the identity, a reversal, an interleave, all of the second vector, then
/// random ones (every index below 32).
fn masks(cap: usize, rng: &mut Rng) -> Vec<[u8; 16]> {
    let mut out = vec![
        std::array::from_fn(|i| i as u8),
        std::array::from_fn(|i| 15 - i as u8),
        std::array::from_fn(|i| if i % 2 == 0 { i as u8 / 2 } else { 16 + i as u8 / 2 }),
        std::array::from_fn(|i| 16 + i as u8),
    ];
    out.truncate(cap.max(1));
    while out.len() < cap {
        out.push(std::array::from_fn(|_| rng.below(32) as u8));
    }
    out
}

/// Every function of `shape` to make: one per immediate or constant tried, or just one.
fn bakeds(shape: Shape, budget: &Budget, rng: &mut Rng) -> Vec<Baked> {
    let one = |imm: i64| Baked { imms: [imm, 0, 0], bytes: [0; 16] };
    match shape.op {
        Op::Iconst => iconst_values(shape, budget.baked, rng).into_iter().map(one).collect(),
        Op::Fconst => fconst_values(shape, budget.baked, rng).into_iter().map(one).collect(),
        Op::Vconst => {
            let mut out: Vec<Baked> =
                edges(shape.ty).iter().map(|edge| Baked { imms: [0; 3], bytes: padded(edge) }).collect();
            out.truncate(budget.baked.saturating_sub(budget.baked / 3).max(1));
            while out.len() < budget.baked {
                out.push(Baked { imms: [0; 3], bytes: rng.value(shape.ty) });
            }
            out
        }
        Op::Shuffle => masks(budget.baked, rng).into_iter().map(|bytes| Baked { imms: [0; 3], bytes }).collect(),
        _ => {
            let mut imms: Vec<usize> = (0..3).filter(|n| shape.srcs[*n] == Src::Imm).collect();
            let mut lists: Vec<Vec<i64>> = imms.iter().map(|n| imm_values(shape, *n, budget.baked, rng)).collect();
            if let Some(offsets) = confined_offsets(shape) {
                imms.push(OFFSET);
                lists.push(offsets);
            }
            if imms.is_empty() {
                return vec![Baked::default()];
            }
            combos(&lists, budget.baked, rng)
                .into_iter()
                .map(|picks| {
                    let mut baked = Baked::default();
                    for (n, value) in imms.iter().zip(picks) {
                        baked.imms[*n] = value;
                    }
                    baked
                })
                .collect()
        }
    }
}

/// The offsets a confined access of `shape` is tried at, when its [`Disp`] adds one: both ends
/// of `Lo12`'s range and an aligned one, and for `Imm32` one past 12 bits, a negative one and
/// both ends of 32 bits (none of them `Lo12`'s, so AArch64 selects `Imm32` for each).
fn confined_offsets(shape: Shape) -> Option<Vec<i64>> {
    match shape.confined()?.1 {
        Disp::Zero => None,
        Disp::Lo12 => Some(vec![1, 8, 4095]),
        Disp::Imm32 => Some(vec![0x1000, -8, i64::from(i32::MIN), i64::from(i32::MAX)]),
    }
}

fn panic_text(panic: Box<dyn std::any::Any + Send>) -> String {
    panic
        .downcast_ref::<String>()
        .cloned()
        .or_else(|| panic.downcast_ref::<&str>().map(|it| (*it).to_owned()))
        .unwrap_or_else(|| "a panic".to_owned())
}

/// [`crate::compile`], with a panic as its error.
fn compile_caught(func: &ir::Function, target: &Target) -> Result<Code, String> {
    match std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| crate::compile(func, target))) {
        Ok(Ok(code)) => Ok(code),
        Ok(Err(e)) => Err(e.to_string()),
        Err(panic) => Err(format!("panicked: {}", panic_text(panic))),
    }
}

/// Whether `code` holds `stencil`'s bytes (its holes' fields any bytes): that the patcher
/// selected it.
fn has_stencil(code: &[u8], stencil: &Stencil<'_>) -> bool {
    let body = stencil.body();
    if body.is_empty() {
        return true;
    }
    let mut wild = vec![false; body.len()];
    for hole in stencil.holes() {
        let start = usize::from(hole.at);
        let end = (start + usize::from(hole.form.bytes())).min(body.len());
        for at in start..end {
            wild[at] = true;
        }
    }
    code.len() >= body.len()
        && code.windows(body.len()).any(|window| {
            window.iter().zip(body).zip(&wild).all(|((got, want), wild)| *wild || got == want)
        })
}

fn selected(code: &Code, library: &Library, shape: Shape) -> Result<(), String> {
    match library.find(shape) {
        Some(stencil) if has_stencil(&code.bytes, &stencil) => Ok(()),
        Some(_) => Err(format!("the code has no `{shape}` stencil: the patcher selected another shape")),
        None => Err(format!("the {:?} library has no `{shape}` stencil", library.variant())),
    }
}

fn fail_with(failures: Vec<String>, what: &str) {
    if !failures.is_empty() {
        panic!("{} {what}:\n\n{}", failures.len(), failures.join("\n\n"));
    }
}

/// The library `target`'s code is made of.
fn library_of(target: &Target) -> &'static Library {
    let variant = Variant::of(target).unwrap_or_else(|e| panic!("{e}"));
    Library::get(variant).unwrap_or_else(|e| panic!("{e}"))
}

fn compiles_where_cranelift_does(target: Target) {
    let isa = host::isa(&target);
    let library = library_of(&target);
    let variant = library.variant();
    let mut shapes = Vec::new();
    shape::each(|shape| shapes.push(shape));
    let mut failures = Vec::new();
    for shape in shapes {
        let mut rng = Rng::new(seed(shape));
        let baked = bakeds(shape, &Budget::ONE, &mut rng)[0];
        let built = match build(shape, &target, &*isa, &baked) {
            Ok(built) => built,
            Err(why) => {
                failures.push(format!("{shape}: {why}"));
                continue;
            }
        };
        let clif = || built.func.display().to_string();
        match (host::cranelift_caught(&built.func, &*isa), compile_caught(&built.func, &target)) {
            (Ok(compiled), Ok(code)) => {
                let named: Vec<ir::LibCall> = compiled
                    .relocs
                    .iter()
                    .filter_map(|reloc| match reloc.name {
                        Name::LibCall(call) => Some(call),
                        _ => None,
                    })
                    .collect();
                for reloc in &code.relocs {
                    match reloc.name {
                        Name::LibCall(call) if named.contains(&call) => {}
                        Name::Own(_) | Name::Counter => {}
                        name => failures.push(format!(
                            "{shape}: a relocation against {name:?}, which neither the function nor \
                             Cranelift's code of it names (Cranelift's libcalls: {named:?})\n{}",
                            clif()
                        )),
                    }
                }
                if variant.has(shape) {
                    if let Err(e) = selected(&code, library, shape) {
                        failures.push(format!("{shape}: {e}\n{}", clif()));
                    }
                }
            }
            (Ok(_), Err(e)) => failures.push(format!(
                "{shape}: Cranelift compiles it and the stencil backend refuses: {e}\n{}",
                clif()
            )),
            (Err(e), Ok(_)) => failures.push(format!(
                "{shape}: Cranelift refuses it ({e}) and the stencil backend compiles it\n{}",
                clif()
            )),
            (Err(_), Err(_)) => {}
        }
    }
    fail_with(failures, &format!("shapes compile differently from Cranelift for {}", host::triple(&target)));
}

#[test]
fn shapes_compile_where_cranelift_does_x86_64_linux() {
    compiles_where_cranelift_does(host::TARGETS[0]);
}

#[test]
fn shapes_compile_where_cranelift_does_aarch64_linux() {
    compiles_where_cranelift_does(host::TARGETS[1]);
}

#[test]
fn shapes_compile_where_cranelift_does_aarch64_macos() {
    compiles_where_cranelift_does(host::TARGETS[2]);
}

#[test]
fn shapes_compile_where_cranelift_does_x86_64_windows() {
    compiles_where_cranelift_does(host::TARGETS[3]);
}

/// What is wrong with the holes of `shape`'s stencil: a form that is not one `enc` patches
/// for the kind and the architecture, a field out of the code, a hole of something the shape
/// does not have, or a hole the shape needs missing.
fn hole_problems(shape: Shape, stencil: &Stencil<'_>, a64: bool) -> Vec<String> {
    let info = shape.op.info();
    let op = shape.op;
    let mut out = Vec::new();
    let mut seen = [false; HoleKind::ALL.len()];
    for hole in stencil.holes() {
        seen[hole.kind as usize] = true;
        let (kind, form) = (hole.kind, hole.form);
        if !format::fits(kind, form) {
            out.push(format!("a {kind:?} hole in a {form:?} field"));
        }
        if (form as u8 >= Form::A64MovwG0 as u8) != a64 {
            out.push(format!("a {form:?} field in a stencil of the other architecture"));
        }
        if usize::from(hole.at) + usize::from(form.bytes()) > stencil.code.len() {
            out.push(format!("a {kind:?} hole at {} past the {} bytes", hole.at, stencil.code.len()));
        }
        if a64 && hole.at % 4 != 0 {
            out.push(format!("a {kind:?} hole at {}, inside an instruction", hole.at));
        }
        let ok = match kind {
            HoleKind::Slot0 | HoleKind::Slot1 | HoleKind::Slot2 => {
                shape.srcs[kind as usize - HoleKind::Slot0 as usize] == Src::Slot
            }
            HoleKind::Imm0 | HoleKind::Imm1 | HoleKind::Imm2 => {
                let n = kind as usize - HoleKind::Imm0 as usize;
                shape.srcs[n] == Src::Imm || (n == 0 && matches!(op, Op::Iconst | Op::Fconst))
            }
            HoleKind::Result0 => matches!(shape.dst, Dst::Slot | Dst::Both),
            HoleKind::Result1 => info.results == 2,
            HoleKind::Offset => matches!(
                op,
                Op::Load
                    | Op::Store
                    | Op::Uload8
                    | Op::Sload8
                    | Op::Istore8
                    | Op::Uload16
                    | Op::Sload16
                    | Op::Istore16
                    | Op::Uload32
                    | Op::Sload32
                    | Op::Istore32
                    | Op::Uload8x8
                    | Op::Sload8x8
                    | Op::Uload16x4
                    | Op::Sload16x4
                    | Op::Uload32x2
                    | Op::Sload32x2
                    | Op::StackAddr
            ) || confined_offsets(shape).is_some(),
            HoleKind::Next | HoleKind::Rodata => true,
            HoleKind::Target => info.branches,
            HoleKind::Trap | HoleKind::TrapOverflow => info.traps,
            HoleKind::Const => matches!(op, Op::Vconst | Op::Shuffle),
        };
        if !ok {
            out.push(format!("a {kind:?} hole, which the shape has nothing for"));
        }
    }
    let mut need = |kind: HoleKind, why: &str| {
        if !seen[kind as usize] {
            out.push(format!("no {kind:?} hole for {why}"));
        }
    };
    for n in 0..3 {
        match shape.srcs[n] {
            Src::Slot => need(HoleKind::slot(n), "an operand from a slot"),
            Src::Imm => need(HoleKind::imm(n), "an immediate operand"),
            Src::Acc | Src::None => {}
        }
    }
    if matches!(op, Op::Iconst | Op::Fconst) {
        need(HoleKind::Imm0, "the constant");
    }
    if matches!(shape.dst, Dst::Slot | Dst::Both) {
        need(HoleKind::Result0, "the result's slot");
    }
    if info.results == 2 {
        need(HoleKind::Result1, "the second result's slot");
    }
    if info.branches {
        need(HoleKind::Target, "the taken edge");
    }
    if info.traps {
        need(HoleKind::Trap, "the trap");
    }
    if matches!(op, Op::Vconst | Op::Shuffle) {
        need(HoleKind::Const, "the 16-byte constant");
    }
    if confined_offsets(shape).is_some() {
        need(HoleKind::Offset, "the confined access's offset");
    }
    if op == Op::Jump {
        need(HoleKind::Next, "the jump");
    }
    out
}

#[test]
fn every_library_has_exactly_the_set() {
    let mut all = Vec::new();
    shape::each(|shape| all.push(shape));
    let mut failures = Vec::new();
    for shape in &all {
        if Shape::from_key(shape.key()) != Some(*shape) {
            failures.push(format!("{shape}: its key {:#x} is not it", shape.key()));
        }
    }
    for variant in Variant::ALL {
        let library = match Library::get(variant) {
            Ok(library) => library,
            Err(e) => {
                failures.push(e.to_string());
                continue;
            }
        };
        let isa = variant.isa();
        let a64 = isa == Isa::Aarch64;
        let reader = library.reader();
        let mut shapes = Vec::new();
        shape::each_on(isa, |shape| {
            if variant.has(shape) {
                shapes.push(shape);
            }
        });
        if reader.len() as usize != shapes.len() {
            failures.push(format!("{variant:?}: {} stencils for {} shapes", reader.len(), shapes.len()));
        }
        let mut indexed = vec![false; reader.len() as usize];
        for shape in &shapes {
            let Some(index) = reader.find(shape.key()) else {
                failures.push(format!("{variant:?}: no `{shape}` stencil"));
                continue;
            };
            match indexed.get_mut(index as usize) {
                Some(seen) if !*seen => *seen = true,
                _ => failures.push(format!("{variant:?}: `{shape}` finds stencil {index}, which another shape found")),
            }
            let stencil = reader.stencil(index);
            for problem in hole_problems(*shape, &stencil, a64) {
                failures.push(format!("{variant:?} `{shape}`: {problem}"));
            }
            let key = shape.key();
            let bits = (0..32).map(|bit| key ^ (1 << bit));
            for near in bits.chain([key.wrapping_add(1), key.wrapping_sub(1)]) {
                let exists = Shape::from_key(near).is_some_and(|it| variant.has(it));
                if reader.find(near).is_some() != exists {
                    failures.push(format!(
                        "{variant:?}: key {near:#x} (next to `{shape}`) {} a stencil, and is {}a shape of the set",
                        if exists { "finds no" } else { "finds" },
                        if exists { "" } else { "not " },
                    ));
                }
            }
        }
    }
    fail_with(failures, "problems with the stencil libraries");
}

/// The tests that run code: each shape of the host's variant, three ways.
#[cfg(all(unix, any(target_arch = "x86_64", target_arch = "aarch64")))]
mod run {
    use cranelift_codegen::{data_value::DataValue, isa::TargetIsa};

    use super::{
        super::{
            exec::{Exec, Link, Unit},
            interp::{self, Nans, Outcome, Run},
        },
        build::{Built, Input, ir_ty},
        *,
    };

    const SHARDS: usize = 8;

    /// The edges one input is tried at, 16 bytes each.
    fn input_edges(input: &Input) -> Vec<[u8; 16]> {
        let mut out: Vec<[u8; 16]> = Vec::new();
        for edge in edges(input.ty) {
            push_new(&mut out, padded(&edge));
        }
        if let Some(lane) = input.amount_of {
            for amount in amounts(lane) {
                push_new(&mut out, padded(&[amount]));
            }
        }
        out
    }

    /// The inputs a function is run on: edge combinations, then seeded random values. An
    /// `atomic_cas` is given its memory as the expected value every other time.
    fn input_sets(op: Op, inputs: &[Input], pairs: usize, random: usize, rng: &mut Rng) -> Vec<Vec<[u8; 16]>> {
        if inputs.is_empty() {
            return vec![Vec::new()];
        }
        let lists: Vec<Vec<[u8; 16]>> = inputs.iter().map(input_edges).collect();
        let mut sets = combos(&lists, pairs, rng);
        for _ in 0..random {
            sets.push(inputs.iter().map(|input| rng.value(input.ty)).collect());
        }
        if op == Op::AtomicCas {
            for set in sets.iter_mut().skip(1).step_by(2) {
                set[1] = set[0];
            }
        }
        sets
    }

    /// The arguments carrying `set`'s inputs: each through its parameter's [`build::Xform`].
    fn args(inputs: &[Input], set: &[[u8; 16]]) -> Vec<DataValue> {
        inputs
            .iter()
            .zip(set)
            .map(|(input, bytes)| DataValue::read_from_slice_le(&input.xform.apply(input.ty, *bytes), ir_ty(input.xform.param(input.ty))))
            .collect()
    }

    fn hex(inputs: &[Input], set: &[[u8; 16]]) -> String {
        let mut out = String::from("[");
        for (k, (input, bytes)) in inputs.iter().zip(set).enumerate() {
            let width = usize::from(input.ty.bytes());
            let mut wide = [0u8; 16];
            wide[..width].copy_from_slice(&bytes[..width]);
            if k > 0 {
                out.push_str(", ");
            }
            out.push_str(&format!("{}:{:#x}", input.ty.name(), u128::from_le_bytes(wide)));
        }
        out.push(']');
        out
    }

    /// [`interp::run`] of the one function, with a panic as its error.
    fn interpret(func: &ir::Function, args: &[DataValue]) -> Result<Run, String> {
        match std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| interp::run(&[func], 0, args))) {
            Ok(it) => it,
            Err(panic) => Err(format!("panicked: {}", panic_text(panic))),
        }
    }

    /// Whether the interpreter has the op: not the x86-only ones, which `cranelift-interpreter`
    /// 0.136 does not implement. Their stencils are held to Cranelift's code alone.
    fn interpreted(op: Op) -> bool {
        !matches!(op, Op::X86Cvtt2dq | Op::X86Pshufb | Op::X86Pmulhrsw | Op::X86Pmaddubsw | Op::Blendv)
    }

    /// Every failure of one function of a shape, after its CLIF.
    struct Failures<'a> {
        built: &'a Built,
        lines: Vec<String>,
    }

    impl Failures<'_> {
        fn push(&mut self, line: String) {
            if self.lines.is_empty() {
                self.lines.push(format!("{}", self.built.func.display()));
            }
            self.lines.push(line);
        }
    }

    /// Runs `exec`'s two units (the stencil code, Cranelift's code) and the interpreter on one
    /// input set of `built`, giving what is wrong.
    fn run_one(op: Op, built: &Built, exec: &Exec, set: &[[u8; 16]]) -> Option<String> {
        let args = args(&built.inputs, set);
        let reference = interpreted(op).then(|| interpret(&built.func, &args));
        // A confined access whose stencil computes the wrong address faults: in a child, so
        // only its shape fails.
        let confined = matches!(op, Op::LoadConfined | Op::StoreConfined);
        let traps = confined
            || match &reference {
                Some(Ok(Run::Trapped(_))) => true,
                Some(Ok(Run::Returned(_))) => false,
                _ => op.info().traps,
            };
        let calls = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
            if traps {
                (exec.values_forked(0, &args), exec.values_forked(1, &args))
            } else {
                (Outcome::Returned(exec.values(0, &args)), Outcome::Returned(exec.values(1, &args)))
            }
        }));
        let (stencil, cranelift) = match calls {
            Ok(it) => it,
            Err(panic) => return Some(format!("inputs {}: calling the code panicked: {}", hex(&built.inputs, set), panic_text(panic))),
        };
        let mut problems = Vec::new();
        match (&stencil, &cranelift) {
            (Outcome::Returned(a), Outcome::Returned(b)) => {
                if !interp::same_all(&built.rets, a, b, Nans::Exact) {
                    problems.push("the stencil code and Cranelift's differ".to_owned());
                }
            }
            (Outcome::Trapped(_), Outcome::Trapped(_)) => {}
            (Outcome::Trapped(_), Outcome::Returned(_)) => problems.push("the stencil code traps and Cranelift's returns".to_owned()),
            (Outcome::Returned(_), Outcome::Trapped(_)) => problems.push("the stencil code returns and Cranelift's traps".to_owned()),
        }
        match &reference {
            Some(Ok(run)) => {
                if let Err(e) = interp::agrees(&built.rets, run, &stencil) {
                    problems.push(format!("stencil: {e}"));
                }
                if let Err(e) = interp::agrees(&built.rets, run, &cranelift) {
                    problems.push(format!("cranelift: {e}"));
                }
            }
            Some(Err(e)) => problems.push(format!("the interpreter can't run it: {e}")),
            None => {}
        }
        if problems.is_empty() {
            return None;
        }
        Some(format!(
            "inputs {} (args {args:?}): {}\n    interpreter {reference:?}\n    cranelift   {cranelift:?}\n    stencil     {stencil:?}",
            hex(&built.inputs, set),
            problems.join("; "),
        ))
    }

    /// Every failure of `shape` on the host, or `None`.
    fn check(shape: Shape, target: &Target, isa: &dyn TargetIsa, library: &Library, budget: &Budget) -> Option<String> {
        let seed = seed(shape);
        let mut rng = Rng::new(seed);
        let bakeds = bakeds(shape, budget, &mut rng);
        let baked_alone = bakeds.len() == 1 && bakeds[0] == Baked::default();
        let mut report = Vec::new();
        for baked in bakeds {
            let built = match build(shape, target, isa, &baked) {
                Ok(built) => built,
                Err(why) => {
                    report.push(why);
                    break;
                }
            };
            let mut failures = Failures { built: &built, lines: Vec::new() };
            match (host::cranelift_caught(&built.func, isa), compile_caught(&built.func, target)) {
                (Err(_), Err(_)) => {}
                (Ok(_), Err(e)) => failures.push(format!("Cranelift compiles it and the stencil backend refuses: {e}")),
                (Err(e), Ok(_)) => failures.push(format!("Cranelift refuses it ({e}) and the stencil backend compiles it")),
                (Ok(compiled), Ok(code)) => {
                    if let Err(e) = selected(&code, library, shape) {
                        failures.push(e);
                    }
                    let link = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
                        Exec::link(&Link {
                            isa,
                            units: &[Unit::Stencil(&code, &built.sig), Unit::Cranelift(&compiled, &built.sig)],
                            data: &[],
                            host: &[],
                        })
                    }));
                    match link {
                        Ok(exec) => {
                            let (pairs, random) =
                                if baked_alone { (budget.pairs, budget.random) } else { (budget.per_baked, budget.per_baked / 2) };
                            for set in input_sets(shape.op, &built.inputs, pairs, random, &mut rng) {
                                if let Some(line) = run_one(shape.op, &built, &exec, &set) {
                                    failures.push(format!("  {baked:?} {line}"));
                                }
                            }
                        }
                        Err(panic) => failures.push(format!("linking the code panicked: {}", panic_text(panic))),
                    }
                }
            }
            report.extend(failures.lines);
        }
        (!report.is_empty()).then(|| format!("{shape} (seed {seed:#x}):\n{}", report.join("\n")))
    }

    fn shard(k: usize) {
        let Some(target) = host::host() else {
            eprintln!(
                "no stencil variant runs here: the per-stencil runs are x86-64 Linux's, AArch64 \
                 Linux's and AArch64 macOS's"
            );
            return;
        };
        let isa = host::isa(&target);
        let library = library_of(&target);
        let budget = Budget::get();
        let mut shapes = Vec::new();
        let variant = library.variant();
        shape::each_on(variant.isa(), |shape| {
            if variant.has(shape) {
                shapes.push(shape);
            }
        });
        let mut failures = Vec::new();
        for (i, shape) in shapes.into_iter().enumerate() {
            if i % SHARDS != k {
                continue;
            }
            if let Some(failure) = check(shape, &target, &*isa, library, &budget) {
                failures.push(failure);
            }
        }
        fail_with(failures, &format!("stencils (shard {k} of {SHARDS}) differ from Cranelift or the interpreter"));
    }

    #[test]
    fn stencils_shard_0() {
        shard(0);
    }

    #[test]
    fn stencils_shard_1() {
        shard(1);
    }

    #[test]
    fn stencils_shard_2() {
        shard(2);
    }

    #[test]
    fn stencils_shard_3() {
        shard(3);
    }

    #[test]
    fn stencils_shard_4() {
        shard(4);
    }

    #[test]
    fn stencils_shard_5() {
        shard(5);
    }

    #[test]
    fn stencils_shard_6() {
        shard(6);
    }

    #[test]
    fn stencils_shard_7() {
        shard(7);
    }
}

#[test]
fn combos_cover_every_edge_and_respect_the_cap() {
    let mut rng = Rng::new(1);
    let lists = vec![vec![1, 2, 3], vec![10, 20]];
    let all = combos(&lists, 100, &mut rng);
    assert_eq!(all.len(), 6);
    let capped = combos(&[(0..40).collect::<Vec<_>>(), vec![7, 8]], 48, &mut rng);
    assert_eq!(capped.len(), 48);
    for n in 0..40 {
        assert!(capped.iter().any(|it| it[0] == n), "{n}");
    }
}

#[test]
fn immediates_fold_and_constants_fit_their_aux() {
    each_with_imm_or_const(|shape| {
        let mut rng = Rng::new(seed(shape));
        for baked in bakeds(shape, &Budget::get(), &mut rng) {
            for n in 0..3 {
                if shape.srcs[n] == Src::Imm {
                    assert!(folds(shape.arg(n), baked.imms[n]), "{shape}: {}", baked.imms[n]);
                }
            }
            if matches!(shape.op, Op::Iconst | Op::Fconst) && shape.ty.bytes() == 8 {
                assert_eq!(i32::try_from(baked.imms[0]).is_err(), shape.aux == 1, "{shape}: {:#x}", baked.imms[0]);
            }
        }
    });
}

fn each_with_imm_or_const(mut f: impl FnMut(Shape)) {
    shape::each(|shape| {
        if shape.srcs.contains(&Src::Imm) || matches!(shape.op, Op::Iconst | Op::Fconst) {
            f(shape);
        }
    });
}

#[test]
fn every_shape_has_a_function() {
    let target = host::TARGETS[0];
    let isa = host::isa(&target);
    let mut failures = Vec::new();
    shape::each(|shape| {
        let mut rng = Rng::new(seed(shape));
        let baked = bakeds(shape, &Budget::ONE, &mut rng)[0];
        match build(shape, &target, &*isa, &baked) {
            Ok(built) => {
                if let Err(e) = cranelift_codegen::verify_function(&built.func, &*isa) {
                    failures.push(format!("{shape}: {e}\n{}", built.func.display()));
                }
                let params = built.func.signature.params.len();
                if params != built.inputs.len() {
                    failures.push(format!("{shape}: {params} parameters for {} inputs", built.inputs.len()));
                }
            }
            Err(why) => failures.push(format!("{shape}: {why}")),
        }
    });
    fail_with(failures, "shapes have no valid function");
}
