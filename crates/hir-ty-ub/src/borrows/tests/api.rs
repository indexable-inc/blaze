//! The parts of [`Borrows`] the ported programs don't pin down by themselves: no-op retags,
//! the implicit accesses of a retag, protectors and frees, wildcard roots, and the GC (what it
//! keeps, what it merges, and that it never changes a verdict).

use super::sim::{Ptr, R, Sim, expect_detail, expect_history, expect_ub, normalize};
use crate::{
    borrows::{AliasErrorKind, Borrows, NewPermission, PtrKind, RetagMode},
    ids::{AccessKind, SiteId, Span},
};

fn tags(s: &Sim, p: Ptr) -> usize {
    s.b.tag_count(p.alloc).unwrap()
}

#[test]
fn raw_none_and_pinned_retags_are_noops() {
    let mut s = Sim::new();
    let x = s.alloc(4);
    for mode in [RetagMode::Raw, RetagMode::None] {
        for kind in [PtrKind::Shared, PtrKind::Mut, PtrKind::Box] {
            assert_eq!(s.retag(x, kind, mode, true, true, &[], 4).unwrap(), x);
        }
    }
    // `&mut` and `Box` to a type that is not `Unpin`: Miri's `NewPermission::new` gives none.
    assert_eq!(s.retag(x, PtrKind::Mut, RetagMode::Default, true, false, &[], 4).unwrap(), x);
    assert_eq!(s.retag(x, PtrKind::Box, RetagMode::FnEntry, true, false, &[], 4).unwrap(), x);
    assert!(s.b.retag_accesses().is_empty());
    assert_eq!(tags(&s, x), 1);
    // `&T` to a `!Unpin` type is still retagged.
    assert_ne!(s.retag(x, PtrKind::Shared, RetagMode::Default, true, false, &[], 4).unwrap(), x);
    assert_eq!(tags(&s, x), 2);
}

/// A reborrow reads each non-`Cell` part of its range through the parent; a `Cell` part does
/// no access. Miri: `tb_reborrow`, `associated_access`.
#[test]
fn retag_accesses() {
    let mut s = Sim::new();
    let x = s.alloc(8);
    let read = |start, len| (Span::new(start, len), AccessKind::Read);
    s.shared(x.add(2), 4).unwrap();
    assert_eq!(s.b.retag_accesses(), &[read(2, 4)]);
    s.shared_cell(x, 4, &[Span::new(1, 1)]).unwrap();
    assert_eq!(s.b.retag_accesses(), &[read(0, 1), read(2, 2)]);
    s.shared_cell(x, 4, &[Span::new(0, 4)]).unwrap();
    assert!(s.b.retag_accesses().is_empty());
    // `&mut UnsafeCell`: `ReservedIM` inside the cell, which reads too.
    s.mutable_cell(x, 4, &[Span::new(1, 1)]).unwrap();
    assert_eq!(s.b.retag_accesses(), &[read(0, 1), read(1, 1), read(2, 2)]);
    s.mutable(x, 8).unwrap();
    assert_eq!(s.b.retag_accesses(), &[read(0, 8)]);

    // `-Zmiri-tree-borrows-implicit-writes`: a protected `&mut` is `Unique` from the start and
    // its retag a write; outside function entry nothing changes.
    let write = NewPermission::new(PtrKind::Mut, RetagMode::FnEntry, true, true, &[]).with_implicit_writes(true);
    assert!(write.writes());
    s.call();
    s.retag_with(x, write, 8).unwrap();
    assert_eq!(s.b.retag_accesses(), &[(Span::new(0, 8), AccessKind::Write)]);
    s.ret().unwrap();
    let default = NewPermission::new(PtrKind::Mut, RetagMode::Default, true, true, &[]).with_implicit_writes(true);
    assert!(!default.writes());
    s.retag_with(x, default, 8).unwrap();
    assert_eq!(s.b.retag_accesses(), &[read(0, 8)]);
    // A no-op retag clears the last one's accesses.
    s.retag(x, PtrKind::Mut, RetagMode::Raw, true, true, &[], 8).unwrap();
    assert!(s.b.retag_accesses().is_empty());
}

#[test]
fn write_through_shared_reference() {
    let mut s = Sim::new();
    let x = s.alloc(1);
    let r = s.shared(x, 1).unwrap();
    let created = s.last_site();
    let e = expect_ub(s.write(r, 1), AliasErrorKind::ChildAccessForbidden, "write access through");
    assert_eq!(e.title(), "write access through <2> at alloc1[0x0] is forbidden");
    assert_eq!(e.details().len(), 1);
    expect_detail(&e, "the accessed tag <TAG> has state Frozen which forbids this child write access");
    expect_history(&e, &["the accessed tag <TAG> was created here, in the initial state Frozen"]);
    assert_eq!(e.history()[0].site, Some(created));
    assert_eq!((e.alloc(), e.offset()), (x.alloc, 0));
    assert_eq!(
        normalize(&e.to_string()),
        "write access through <TAG> at ALLOC[0x0] is forbidden\n  \
         = help: this indicates a potential bug in the program: it performed an invalid operation, but the Tree Borrows rules it violated are still experimental\n  \
         = help: see https://github.com/rust-lang/unsafe-code-guidelines/blob/master/wip/tree-borrows.md for further information\n  \
         = help: the accessed tag <TAG> has state Frozen which forbids this child write access"
    );
}

/// The parent's write disables the `&mut`; its history shows the transition that matters
/// for a read (to `Disabled`), not the earlier one to `Unique`.
#[test]
fn foreign_write_disables_mut() {
    let mut s = Sim::new();
    let x = s.alloc(2);
    let m = s.mutable(x, 2).unwrap();
    s.write(m, 2).unwrap();
    s.write(x.add(1), 1).unwrap();
    let disabled = s.last_site();
    s.read(m, 1).unwrap();
    let e = expect_ub(s.read(m.add(1), 1), AliasErrorKind::ChildAccessForbidden, "read access through");
    assert_eq!(e.title(), "read access through <2> at alloc1[0x1] is forbidden");
    expect_detail(&e, "the accessed tag <TAG> has state Disabled which forbids this child read access");
    expect_history(
        &e,
        &[
            "the accessed tag <TAG> was created here, in the initial state Reserved",
            "the accessed tag <TAG> later transitioned to Disabled due to a foreign write access at offsets [0x1..0x2]",
            "this transition corresponds to a loss of read and write permissions",
        ],
    );
    assert_eq!(e.history()[1].site, Some(disabled));
    assert_eq!(e.history()[2].site, None);
    assert!(e.history().iter().all(|ev| ev.tag == m.tag()));
}

/// A parent's state stops a child access: the error names both.
#[test]
fn child_of_frozen_cannot_write() {
    let mut s = Sim::new();
    let x = s.alloc(1);
    let f = s.shared(x, 1).unwrap();
    // `&mut *(f as *const u8 as *mut u8)`.
    let m = s.mutable(f, 1).unwrap();
    let e = expect_ub(s.write(m, 1), AliasErrorKind::ChildAccessForbidden, "write access through");
    assert_eq!(
        e.details(),
        &[
            "the accessed tag <3> is a child of the conflicting tag <2>".to_owned(),
            "the conflicting tag <2> has state Frozen which forbids this child write access".to_owned(),
        ]
    );
    expect_history(
        &e,
        &[
            "the accessed tag <TAG> was created here",
            "the conflicting tag <TAG> was created here, in the initial state Frozen",
        ],
    );
}

#[test]
fn protector_forbids_foreign_write() {
    let mut s = Sim::new();
    let x = s.alloc(1);
    s.call();
    let a = s.arg_mut(x, 1).unwrap();
    let e = expect_ub(s.write(x, 1), AliasErrorKind::ProtectedDisabled, "write access through");
    assert_eq!(
        e.title(),
        "write access through <1> (root of the allocation) at alloc1[0x0] is forbidden"
    );
    assert_eq!(
        e.details(),
        &[
            "the accessed tag <1> (root of the allocation) is foreign to the protected tag <2> (i.e., it is not a child)"
                .to_owned(),
            "this foreign write access would cause the protected tag <2> (currently Reserved) to become Disabled"
                .to_owned(),
            "protected tags must never be Disabled".to_owned(),
        ]
    );
    expect_history(
        &e,
        &[
            // A history note names a tag by its `BorTag` alone (Miri `HistoryData::extend`).
            "the accessed tag <TAG> was created here",
            "the protected tag <TAG> was created here, in the initial state Reserved",
        ],
    );
    let _ = a;
}

/// After the return the same write is allowed, and disables the argument.
#[test]
fn protector_ends_at_return() {
    let mut s = Sim::new();
    let x = s.alloc(1);
    s.call();
    let a = s.arg_mut(x, 1).unwrap();
    s.ret().unwrap();
    s.write(x, 1).unwrap();
    expect_ub(s.read(a, 1), AliasErrorKind::ChildAccessForbidden, "read access through");
}

/// A protector only guards the locations its tag accessed: here the argument covers one byte
/// and the write is to the other.
#[test]
fn protector_only_on_accessed_locations() {
    let mut s = Sim::new();
    let x = s.alloc(2);
    s.call();
    let a = s.arg_mut(x, 1).unwrap();
    s.write(x.add(1), 1).unwrap();
    s.write(a, 1).unwrap();
    s.ret().unwrap();
}

/// The protector's end of a written `&mut` is a write through it, which leaves it `Unique`.
#[test]
fn protector_end_of_unique() {
    let mut s = Sim::new();
    let x = s.alloc(1);
    s.call();
    let a = s.arg_mut(x, 1).unwrap();
    s.write(a, 1).unwrap();
    s.ret().unwrap();
    // After the protector-end write through `a`, `a` is still usable; the root reads fine.
    s.read(a, 1).unwrap();
    s.read(x, 1).unwrap();
    // The root's read froze `a`.
    expect_ub(s.write(a, 1), AliasErrorKind::ChildAccessForbidden, "write access through");
}

/// A protected `&` is disabled by a write through a sibling `&mut`.
#[test]
fn protected_shared_forbids_foreign_write() {
    let mut s = Sim::new();
    let x = s.alloc(1);
    let sibling = s.mutable(x, 1).unwrap();
    s.call();
    let a = s.arg_shared(x, 1).unwrap();
    let e = expect_ub(s.write(sibling, 1), AliasErrorKind::ProtectedDisabled, "write access through");
    expect_detail(&e, "protected tags must never be Disabled");
    let _ = a;
}

/// Each thread has its own stack of frames: thread 1's call returns while thread 2's runs,
/// and only thread 1's protector ends.
#[test]
fn frames_are_per_thread() {
    let mut s = Sim::new();
    let x = s.alloc(2);
    s.thread(1);
    s.call();
    s.arg_mut(x, 1).unwrap();
    s.thread(2);
    s.call();
    s.arg_mut(x.add(1), 1).unwrap();
    s.thread(1);
    s.ret().unwrap();
    s.write(x, 1).unwrap();
    expect_ub(s.write(x.add(1), 1), AliasErrorKind::ProtectedDisabled, "write access through");
}

/// A refused function-entry retag leaves nothing protected for the frame's end.
#[test]
fn refused_fn_entry_retag_protects_nothing() {
    let mut s = Sim::new();
    let x = s.alloc(1);
    let f = s.shared(x, 1).unwrap();
    s.write(x, 1).unwrap();
    s.call();
    expect_ub(s.arg_mut(f, 1), AliasErrorKind::ChildAccessForbidden, "reborrow through");
    s.ret().unwrap();
}

#[test]
fn strong_protector_forbids_dealloc() {
    let mut s = Sim::new();
    let x = s.alloc(4);
    s.call();
    let a = s.arg_mut(x, 4).unwrap();
    let e = expect_ub(s.free(a, 4), AliasErrorKind::ProtectedDealloc, "deallocation through");
    assert_eq!(e.title(), "deallocation through <2> at alloc1[0x0] is forbidden");
    assert_eq!(
        e.details(),
        &[
            "the allocation of the accessed tag <2> also contains the strongly protected tag <2>".to_owned(),
            "the strongly protected tag <2> disallows deallocations".to_owned(),
        ]
    );
}

/// A strongly protected argument doesn't stop the free of locations it never accessed: here a
/// zero-size `&[u8; 0]`.
#[test]
fn strong_protector_only_on_accessed_locations() {
    let mut s = Sim::new();
    let x = s.alloc(4);
    s.call();
    s.arg_shared(x, 0).unwrap();
    s.free(x, 4).unwrap();
    // The allocation is gone; the frame's end skips it.
    s.ret().unwrap();
    assert_eq!(s.b.tag_count(x.alloc), None);
}

/// `Box` arguments are weakly protected: the callee may free them, and the frame's end then
/// skips the allocation.
#[test]
fn weak_protector_allows_dealloc() {
    let mut s = Sim::new();
    let x = s.alloc(4);
    let b = s.boxed(x, 4).unwrap();
    s.call();
    let arg = s.arg_box(b, 4).unwrap();
    s.free(arg, 4).unwrap();
    s.ret().unwrap();
}

/// A weak protector still forbids disabling the `Box`.
#[test]
fn weak_protector_forbids_foreign_write() {
    let mut s = Sim::new();
    let x = s.alloc(4);
    s.call();
    let arg = s.arg_box(x, 4).unwrap();
    expect_ub(s.write(x, 4), AliasErrorKind::ProtectedDisabled, "write access through");
    let _ = arg;
}

/// A shared reference to a `Cell` allows writes through itself and through others, protected
/// or not.
#[test]
fn cells_allow_everything() {
    let mut s = Sim::new();
    let x = s.alloc(4);
    let whole = [Span::new(0, 4)];
    let c1 = s.shared_cell(x, 4, &whole).unwrap();
    let c2 = s.shared_cell(c1, 4, &whole).unwrap();
    s.call();
    let a = s.arg_shared_cell(c1, 4, &whole).unwrap();
    s.write(c2, 4).unwrap();
    s.write(a, 4).unwrap();
    s.write(x, 4).unwrap();
    s.ret().unwrap();
    s.write(c1, 4).unwrap();
    s.free(x, 4).unwrap();
}

/// Frozen bytes and cell bytes of one reference behave per byte.
#[test]
fn precise_interior_mutability() {
    let mut s = Sim::new();
    let x = s.alloc(4);
    // `&(u16, Cell<u16>)`.
    let r = s.shared_cell(x, 4, &[Span::new(2, 2)]).unwrap();
    s.write(r.add(2), 2).unwrap();
    let e = expect_ub(s.write(r, 2), AliasErrorKind::ChildAccessForbidden, "write access through");
    assert_eq!(e.offset(), 0);
    expect_detail(&e, "the accessed tag <TAG> has state Frozen which forbids this child write access");
}

/// A two-phase borrow survives the parent's reads until its first write.
#[test]
fn two_phase_borrow() {
    let mut s = Sim::new();
    let x = s.alloc(8);
    let tp = s.two_phase(x, 8).unwrap();
    s.read(x, 8).unwrap();
    s.write(tp, 8).unwrap();
}

/// A free is a write through the pointer: it disables every other tag first.
#[test]
fn free_through_shared_is_ub() {
    let mut s = Sim::new();
    let x = s.alloc(1);
    let r = s.shared(x, 1).unwrap();
    let e = expect_ub(s.free(r, 1), AliasErrorKind::ChildAccessForbidden, "deallocation through");
    expect_detail(&e, "the accessed tag <TAG> has state Frozen which forbids this deallocation (acting as a child write access)");
}

/// After a free a zero-size retag of the dangling pointer still makes a tag.
#[test]
fn zero_size_retag_of_dead_allocation() {
    let mut s = Sim::new();
    let x = s.alloc(4);
    s.free(x, 4).unwrap();
    let r = s.mutable(x, 0).unwrap();
    assert_ne!(r.tag(), x.tag());
    assert!(s.b.retag_accesses().is_empty());
}

#[test]
fn wildcard_needs_an_exposed_tag() {
    let mut s = Sim::new();
    let x = s.alloc(1);
    let e = expect_ub(s.write(x.wildcard(), 1), AliasErrorKind::NoExposedTag, "write access through <wildcard>");
    assert_eq!(e.title(), "write access through <wildcard> at alloc1[0x0] is forbidden");
    assert_eq!(e.details(), &["there are no exposed tags which may perform this access here".to_owned()]);
    assert!(e.history().is_empty());
    s.expose(x);
    s.write(x.wildcard(), 1).unwrap();
}

/// A wildcard access may only come through an exposed tag that allows it: an exposed `&`
/// allows reads, not writes.
#[test]
fn wildcard_respects_exposed_permissions() {
    let mut s = Sim::new();
    let x = s.alloc(1);
    let r = s.shared(x, 1).unwrap();
    s.expose(r);
    s.read(x.wildcard(), 1).unwrap();
    expect_ub(s.write(x.wildcard(), 1), AliasErrorKind::NoExposedTag, "write access through <wildcard>");
}

/// A reference made from a wildcard pointer roots a new subtree; an access through a tag
/// it can't be a parent of is foreign to it.
#[test]
fn retag_from_wildcard_is_a_new_root() {
    let mut s = Sim::new();
    let x = s.alloc(1);
    s.expose(x);
    let w = s.mutable(x.wildcard(), 1).unwrap();
    assert_eq!(tags(&s, x), 2);
    s.write(w, 1).unwrap();
    s.read(x, 1).unwrap();
    expect_ub(s.write(w, 1), AliasErrorKind::ChildAccessForbidden, "write access through");
}

#[test]
fn names_show_in_errors() {
    let mut s = Sim::new();
    let x = s.alloc(1);
    let r = s.shared(x, 1).unwrap();
    s.name(r, "r");
    assert!(!s.b.name_tag(x.alloc, r.tag(), 3, "too far"));
    let e = expect_ub(s.write(r, 1), AliasErrorKind::ChildAccessForbidden, "write access through");
    assert_eq!(e.title(), "write access through <2> (r) at alloc1[0x0] is forbidden");
}

#[test]
fn gc_is_due_every_10000_retags() {
    let mut s = Sim::new();
    let x = s.alloc(1);
    s.free(x, 1).unwrap();
    for _ in 0..Borrows::GC_EVERY - 1 {
        s.mutable(x, 0).unwrap();
    }
    assert!(!s.b.gc_due());
    s.mutable(x, 0).unwrap();
    assert!(s.b.gc_due());
    s.gc(&[]);
    assert!(!s.b.gc_due());
}

#[test]
fn gc_keeps_held_protected_exposed_and_roots() {
    let mut s = Sim::new();
    let x = s.alloc(4);
    let r = s.shared(x, 4).unwrap();
    assert_eq!(tags(&s, x), 2);
    s.gc(&[r]);
    assert_eq!(tags(&s, x), 2);
    s.gc(&[]);
    assert_eq!(tags(&s, x), 1);

    s.call();
    let a = s.arg_mut(x, 4).unwrap();
    s.gc(&[]);
    assert_eq!(tags(&s, x), 2);
    s.ret().unwrap();
    s.gc(&[]);
    assert_eq!(tags(&s, x), 1);
    let _ = a;

    let e = s.shared(x, 4).unwrap();
    s.expose(e);
    s.gc(&[]);
    assert_eq!(tags(&s, x), 2);
    s.read(x.wildcard(), 4).unwrap();
}

/// A dead node with one child merges into it where the child is at least as strict
/// everywhere (`can_be_replaced_by_child`); the verdicts stay.
#[test]
fn gc_merges_dead_parent_into_its_child() {
    let mut s = Sim::new();
    let x = s.alloc(4);
    let m = s.mutable(x, 4).unwrap();
    let f = s.shared(m, 4).unwrap();
    assert_eq!(tags(&s, x), 3);
    s.gc(&[f]);
    assert_eq!(tags(&s, x), 2);
    s.read(f, 4).unwrap();
    expect_ub(s.write(f, 4), AliasErrorKind::ChildAccessForbidden, "write access through");
}

/// A dead `Unique` parent of a `Cell` child stays: the child allows writes the parent
/// forbids after a foreign read.
#[test]
fn gc_keeps_parent_stricter_than_child() {
    let mut s = Sim::new();
    let x = s.alloc(4);
    let m = s.mutable(x, 4).unwrap();
    s.write(m, 4).unwrap();
    let c = s.shared_cell(m, 4, &[Span::new(0, 4)]).unwrap();
    s.gc(&[c]);
    assert_eq!(tags(&s, x), 3);
    s.read(x, 4).unwrap();
    expect_ub(s.write(c, 4), AliasErrorKind::ChildAccessForbidden, "write access through");
}

/// The GC never changes whether a step is UB, nor which rule refuses it: random programs on
/// a 3-byte allocation, run once with a GC (holding only the pointers the program still
/// uses) after every step and once without. Wildcards are left out: whether a merge keeps
/// every wildcard verdict is not a property Miri's GC claims.
#[test]
fn gc_never_changes_a_verdict() {
    use blaze_ids::rng::SplitMix64;

    const SIZE: u64 = 3;

    /// `a` and `b` pick a range at or after the pointer's offset.
    #[derive(Clone, Copy, Debug)]
    enum Op {
        Retag { from: usize, kind: u8, a: u64, b: u64 },
        Access { through: usize, write: bool, a: u64, b: u64 },
        Drop { which: usize },
        Call { from: usize, kind: u8 },
        Ret,
    }

    /// `p` moved to a start in `p.offset..SIZE`, and a length that stays in bounds.
    fn range(p: Ptr, a: u64, b: u64) -> (Ptr, u64) {
        let start = p.offset + a % (SIZE - p.offset);
        (Ptr { offset: start, ..p }, 1 + b % (SIZE - start))
    }

    fn step(s: &mut Sim, held: &mut Vec<Ptr>, frames: &mut usize, op: Op) -> R<()> {
        match op {
            Op::Retag { from, kind, a, b } => {
                let (base, len) = range(held[from], a, b);
                let cell = [Span::new(0, 1)];
                let new = match kind {
                    0 => s.shared(base, len)?,
                    1 => s.mutable(base, len)?,
                    2 => s.two_phase(base, len)?,
                    3 => s.shared_cell(base, len, &cell)?,
                    _ => s.mutable_cell(base, len, &cell)?,
                };
                held.push(new);
            }
            Op::Access { through, write, a, b } => {
                let (p, len) = range(held[through], a, b);
                if write {
                    s.write(p, len)?;
                } else {
                    s.read(p, len)?;
                }
            }
            Op::Drop { which } => {
                held.remove(which);
            }
            Op::Call { from, kind } => {
                let p = held[from];
                let len = SIZE - p.offset;
                s.call();
                *frames += 1;
                let a = match kind {
                    0 => s.arg_shared(p, len)?,
                    1 => s.arg_mut(p, len)?,
                    _ => s.arg_box(p, len)?,
                };
                held.push(a);
            }
            Op::Ret => {
                *frames -= 1;
                s.ret()?;
            }
        }
        Ok(())
    }

    let mut rng = SplitMix64::new(0x9e37_79b9_7f4a_7c15);
    let mut checked_ub = 0;
    for _program in 0..3000 {
        let mut plain = Sim::new();
        let mut collected = Sim::new();
        let x = plain.alloc(SIZE);
        assert_eq!(collected.alloc(SIZE), x);
        // The root stays held: a program always has its local.
        let mut held = vec![x];
        let mut frames = 0usize;
        for _ in 0..14 {
            let op = match rng.index(10) {
                0..=2 => Op::Retag {
                    from: rng.index(held.len()),
                    kind: rng.index(5) as u8,
                    a: rng.next_u64(),
                    b: rng.next_u64(),
                },
                3..=6 => Op::Access {
                    through: rng.index(held.len()),
                    write: rng.index(2) == 0,
                    a: rng.next_u64(),
                    b: rng.next_u64(),
                },
                7 if held.len() > 1 => Op::Drop { which: 1 + rng.index(held.len() - 1) },
                8 if frames < 2 => Op::Call { from: rng.index(held.len()), kind: rng.index(3) as u8 },
                9 if frames > 0 => Op::Ret,
                _ => continue,
            };
            let mut held2 = held.clone();
            let mut frames2 = frames;
            let a = step(&mut plain, &mut held, &mut frames, op);
            let b = step(&mut collected, &mut held2, &mut frames2, op);
            assert_eq!(held, held2);
            match (a, b) {
                (Ok(()), Ok(())) => collected.gc(&held),
                (Err(ea), Err(eb)) => {
                    assert_eq!(ea.kind(), eb.kind(), "{op:?}:\n{ea}\nvs\n{eb}");
                    checked_ub += 1;
                    break;
                }
                (a, b) => panic!("{op:?}: without GC {a:?}, with GC {b:?}"),
            }
        }
    }
    // The programs reach UB often enough for the property to mean something.
    assert!(checked_ub > 300, "only {checked_ub} programs reached UB");
}

/// `SiteId`s given to the model come back in the history unchanged.
#[test]
fn history_sites_are_the_callers() {
    let mut b = Borrows::new();
    let alloc = crate::ids::AllocId::new(1).unwrap();
    let root = b.new_alloc(alloc, 1, SiteId(100));
    let root_prov = crate::ids::Prov::Concrete { alloc, tag: root };
    let new = NewPermission::new(PtrKind::Shared, RetagMode::Default, true, true, &[]);
    let r = b.retag(alloc, root_prov, new, Span::new(0, 1), SiteId(200)).unwrap().unwrap();
    let prov = crate::ids::Prov::Concrete { alloc, tag: r };
    let e = b.access(alloc, prov, AccessKind::Write, Span::new(0, 1), SiteId(300)).unwrap_err();
    assert_eq!(e.history()[0].site, Some(SiteId(200)));
}
