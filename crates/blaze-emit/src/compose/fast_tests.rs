//! The Fast tier's tests (piece K2): `sum(n)` composed from T1's committed table, its record's
//! loop map, poll and relocations checked, then run as machine code; the control (rule 8) a
//! table whose `add` serves `sub`'s template, caught by the run; and the measurement against the
//! same loop with every value in a register.
//!
//! Every test reads crates/blaze-emit/table/arm64.bztab, so it needs T1's table to hold the
//! composer's own patterns (emit.rs, `glue`); until then each fails with
//! `TableError::Missing` naming the first pattern it lacks.

use blaze_code::record::{Space, Symbol};
use blaze_lir::Op;
use blaze_own::Alloc;
use blaze_tier::{ARM64, Arch, Emitted, Named, Names, Outside, Pattern, RelocKind, Table, TableHead, Template, Tier};

use super::Fast;
use super::emit::POLL_PAGE;
use crate::table_gen::EncodedTable;
use crate::tests::sum_func;

/// The table T1 commits.
fn table() -> EncodedTable<'static> {
    match EncodedTable::read(include_bytes!("../../table/arm64.bztab"), Alloc::global()) {
        Ok(table) => table,
        Err(error) => panic!("the committed table does not read: {error}"),
    }
}

/// `sum` names nothing outside itself: every name request is a failure of the test.
struct Nameless;

impl Names for Nameless {
    fn name(&self, _: Outside) -> Option<Named<'_>> {
        None
    }

    fn routines(&self) -> crate::RoutinePlaces {
        crate::RoutinePlaces::NONE
    }
}

fn composed(table: &dyn Table) -> Emitted<'static> {
    let fast = Fast { table, regs: &ARM64 };
    let machine = table.head().machine;
    assert_eq!(machine.arch, Arch::Arm64, "the committed table is arm64's");
    let func = sum_func();
    match fast.emit(blaze_lir::verify(func.view()).expect("the test's function is LIR"), &Nameless, machine, Alloc::global()) {
        Ok(emitted) => emitted,
        Err(error) => panic!("the Fast tier refused sum: {error:?}"),
    }
}

/// `sum`'s record: one loop, its `at` filled to an instruction of the code; one poll, on the
/// loop; its two relocations `Page21` then `PageOff12 { scale: 3 }` at the poll's two words, to
/// the poll page by its C name; relocations and positions sorted; the frame word set.
/// Expected: passes.
#[test]
fn sum_has_one_loop_one_poll_and_its_relocations() {
    let table = table();
    let e = composed(&table);
    assert_eq!(e.code.len() % 4, 0, "whole arm64 words");
    assert_eq!(e.loops.len(), 1, "sum has one loop");
    let header = e.loops[0];
    assert!(header.at != 0 && header.at % 4 == 0 && (header.at as usize) < e.code.len(), "the loop's `at` is filled");
    assert_eq!(e.polls.len(), 1, "one back edge, one poll");
    let poll = e.polls[0];
    assert_eq!(poll.header, 0, "the poll is the loop's");
    assert!(poll.at > header.at, "the poll is on the back edge, after the header");
    let at_poll: Vec<_> = e.relocs.iter().filter(|r| r.at == poll.at || r.at == poll.at + 4).collect();
    assert_eq!(at_poll.len(), 2, "the poll carries two relocations");
    assert_eq!(at_poll[0].kind, RelocKind::Page21);
    assert_eq!(at_poll[1].kind, RelocKind::PageOff12 { scale: 3 });
    let target = e.targets[at_poll[0].target as usize];
    assert_eq!(at_poll[1].target, at_poll[0].target, "both to one target");
    assert_eq!(target.space, Space::Outside);
    assert_eq!(target.symbol, Symbol::of_name(POLL_PAGE));
    let named = e.names.iter().find(|n| n.0 == target.symbol).expect("the poll page has its C name");
    assert_eq!(&e.text[named.1 as usize..named.2 as usize], POLL_PAGE.as_bytes());
    assert!(e.relocs.windows(2).all(|w| w[0].at < w[1].at), "relocations sorted, one per byte");
    assert!(e.positions.windows(2).all(|w| w[0].at < w[1].at), "positions sorted, one per offset");
    assert!(e.unwind_frame.is_some(), "the frame word is set");
    println!("fast sum: {} bytes ({} instructions), poll at {}, loop at {}", e.code.len(), e.code.len() / 4, poll.at, header.at);
}

/// A table whose `add` templates are `sub`'s (the control's planted bug).
struct WrongTemplate<'t> {
    inner: &'t dyn Table,
}

impl Table for WrongTemplate<'_> {
    fn head(&self) -> TableHead {
        self.inner.head()
    }

    fn find(&self, pattern: &Pattern) -> Option<Template<'_>> {
        match pattern.op {
            Op::Add => self.inner.find(&Pattern { op: Op::Sub, ..*pattern }),
            _ => self.inner.find(pattern),
        }
    }
}

#[cfg(target_arch = "aarch64")]
mod run {
    use blaze_inproc::exec::Executable;
    use blaze_tier::{Emitted, RelocKind};

    /// `e`'s code placed with its poll relocations patched to a zeroed readable page inside the
    /// same `Executable` mapping (guaranteed within 1 GiB of every code byte, so `adrp`'s ±4 GiB
    /// `Page21` reach holds on every host ASLR layout, including Linux PIE): `adrp`'s page delta
    /// into `immlo` (bits 29..31) and `immhi` (bits 5..24), the page offset in units of 8 into
    /// the `ldr`'s `imm12` (bits 10..22). Any other relocation fails the test.
    pub(super) fn place(e: &Emitted<'_>) -> Executable {
        Executable::written(e.code.len(), 4096, |out, data| {
            let base = out.as_ptr() as u64;
            let page = data.as_ptr() as u64;
            out.copy_from_slice(&e.code);
            for r in e.relocs.iter() {
                let at = r.at as usize;
                let mut word = u32::from_le_bytes([out[at], out[at + 1], out[at + 2], out[at + 3]]);
                match r.kind {
                    RelocKind::Page21 => {
                        let signed_delta = (page >> 12) as i64 - ((base + u64::from(r.at)) >> 12) as i64;
                        assert!(
                            (-(1i64 << 20)..(1i64 << 20)).contains(&signed_delta),
                            "Page21 delta {signed_delta} out of ±4 GiB range (base={base:#x}, page={page:#x})"
                        );
                        let delta = signed_delta as u64;
                        word &= !((0b11 << 29) | (0x7ffff << 5));
                        // Truncation on purpose: the delta's low 21 bits are the field.
                        word |= ((delta as u32 & 0b11) << 29) | (((delta >> 2) as u32 & 0x7ffff) << 5);
                    }
                    RelocKind::PageOff12 { scale } => {
                        word &= !(0xfff << 10);
                        // Truncation on purpose: twelve bits of the page offset.
                        word |= (((page & 0xfff) >> scale) as u32 & 0xfff) << 10;
                    }
                    other => panic!("an unexpected relocation {other:?} in sum"),
                }
                out[at..at + 4].copy_from_slice(&word.to_le_bytes());
            }
        })
        .expect("the code maps")
    }

    pub(super) fn call(exec: &Executable, n: u64) -> u64 {
        // SAFETY: `exec` holds a composed function of the C convention taking one `i64` and
        // returning one, mapped executable while `exec` lives; its only relocations were
        // patched to a static readable page.
        let f: extern "C" fn(u64) -> u64 = unsafe { exec.as_fn() };
        f(n)
    }
}

/// `sum(n)` composed and run: `n(n+1)/2` for each `n`.
/// Expected: passes.
#[cfg(target_arch = "aarch64")]
#[test]
fn fast_sum_runs() {
    let table = table();
    let e = composed(&table);
    let exec = run::place(&e);
    for n in [0u64, 1, 2, 10, 1000, 65_537] {
        assert_eq!(run::call(&exec, n), n * (n + 1) / 2, "sum({n})");
    }
}

/// The control (rule 8): `add` served by `sub`'s template composes without complaint and is
/// caught by the run.
/// Expected: prints "plant WrongTemplate: caught" and passes.
#[cfg(target_arch = "aarch64")]
#[test]
fn plant_wrong_template_is_caught() {
    let table = table();
    let planted = WrongTemplate { inner: &table };
    let e = composed(&planted);
    let exec = run::place(&e);
    let got = run::call(&exec, 100);
    assert_ne!(got, 5050, "the planted table's sum must not be right");
    println!("plant WrongTemplate: caught (sum(100) = {got}, not 5050)");
}

/// The Fast tier's `sum(n)` against the same loop with every value in a register (tests.rs,
/// `the_cell_baseline_against_registers`: the cell tier's ratio was 4.41). Asserts only that
/// both agree.
/// Expected: prints "fast sum(100000000): ns/iter X, registers ns/iter Y, ratio R".
#[cfg(target_arch = "aarch64")]
#[test]
fn the_fast_baseline_against_registers() {
    let table = table();
    let e = composed(&table);
    let exec = run::place(&e);
    let n: u64 = 100_000_000;
    let floor = |n: u64| -> u64 {
        let (mut i, mut acc) = (1u64, 0u64);
        // SAFETY: register arithmetic and branches within the block only; no memory.
        unsafe {
            core::arch::asm!(
                "2:", "cmp {i}, {n}", "b.hi 3f", "add {acc}, {acc}, {i}", "add {i}, {i}, #1", "b 2b", "3:",
                i = inout(reg) i, acc = inout(reg) acc, n = in(reg) n, options(nomem, nostack),
            );
        }
        let _ = i;
        acc
    };
    let time = |f: &dyn Fn() -> u64| {
        let mut best = u128::MAX;
        let mut out = 0;
        for _ in 0..3 {
            let start = std::time::Instant::now();
            out = core::hint::black_box(f());
            best = best.min(start.elapsed().as_nanos());
        }
        (out, best)
    };
    let (fast, fast_ns) = time(&|| run::call(&exec, core::hint::black_box(n)));
    let (regs, regs_ns) = time(&|| floor(core::hint::black_box(n)));
    assert_eq!(fast, regs);
    println!(
        "fast sum({n}): ns/iter {:.3}, registers ns/iter {:.3}, ratio {:.2}",
        fast_ns as f64 / n as f64,
        regs_ns as f64 / n as f64,
        fast_ns as f64 / regs_ns as f64,
    );
}
