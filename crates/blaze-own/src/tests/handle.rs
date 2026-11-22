//! The collections through an [`Alloc`] handle: the model cases again over a handle of the
//! counting allocator, `every_failure` through a handle (both ways round: a handle of the failing
//! allocator, and the failing allocator over a handle of an `Inline` buffer), the owners, the
//! global handle, and a planted bug in the handle's `grow` entry that the vector's oracle must
//! catch (rule 53).

use std::{format, string::String, vec::Vec as StdVec};

use blaze_sim::{Broke, Tape};

use super::{
    Counting, Why, boxed_values, btree_case_in, cases, deque_case_in, soa_case_in, sorted_case_in, step, under_every_failure,
    vec_case_in, words,
};
use crate::{
    AVec, Alloc, Arc, Box, Cow, Erased, Inline, Packed, Rc, SharedAlloc, Vec,
    plant::{self, Plant},
    testing::every_failure,
};

/// Runs `case` with a handle of a fresh counting allocator (and the allocator, to count with),
/// then checks that nothing is live or was freed wrong.
fn through(case: impl FnOnce(&Counting, Alloc<'_>) -> Result<(), Broke>) -> Result<(), Broke> {
    let counted = Counting::new();
    let erased = Erased::new(&counted);
    case(&counted, erased.handle())?;
    counted.check()
}

/// The refinement: blaze-own collections through an `Alloc` handle match std collections and grow in place when the underlying allocator supports it.
#[test]
fn vec_matches_std_through_a_handle() {
    blaze_sim::check(cases(300), |tape: &Tape| through(|_, alloc| vec_case_in(tape, alloc)));
}

#[test]
fn deque_matches_std_through_a_handle() {
    blaze_sim::check(cases(300), |tape: &Tape| through(|_, alloc| deque_case_in(tape, alloc)));
}

#[test]
fn sorted_map_and_set_match_std_through_a_handle() {
    blaze_sim::check(cases(300), |tape: &Tape| through(|_, alloc| sorted_case_in(tape, alloc)));
}

#[test]
fn soa_matches_a_vec_of_rows_through_a_handle() {
    blaze_sim::check(cases(300), |tape: &Tape| through(|counted, alloc| soa_case_in(tape, counted, alloc)));
}

#[test]
fn btree_matches_std_through_a_handle() {
    blaze_sim::check(cases(300), |tape: &Tape| through(|_, alloc| btree_case_in(tape, alloc)));
}

#[test]
#[cfg_attr(miri, ignore = "a planted bug: tests the oracle, not unsafe code, and leaks on purpose; its clean half is `vec_matches_std_through_a_handle`")]
fn the_oracle_catches_a_handle_whose_grow_allocates() {
    let run = |tape: &Tape, p: Plant| plant::with(p, || through(|_, alloc| vec_case_in(tape, alloc)));
    blaze_sim::control(cases(300), Plant::Clean, Plant::HandleGrowAllocates, "differs from the model", run);
}

#[test]
fn a_handle_is_one_word_and_copies_are_the_same_allocator() -> Result<(), Broke> {
    if size_of::<Alloc<'_>>() != size_of::<usize>() || size_of::<Option<Alloc<'_>>>() != size_of::<usize>() {
        return Err(Broke::Oracle(format!("a handle is {} bytes", size_of::<Alloc<'_>>())));
    }
    let (one, other) = (Counting::new(), Counting::new());
    let (one_erased, other_erased) = (Erased::new(&one), Erased::new(&other));
    let a = one_erased.handle();
    let copy = a;
    if !a.same(copy) || !a.same(one_erased.handle()) || a.same(other_erased.handle()) {
        return Err(Broke::Oracle(String::from("`same` does not say which allocator a handle is")));
    }
    // A block made through one copy is grown and freed through another.
    let mut v: AVec<'_, u64> = Vec::new_in(a);
    v.extend(0..10);
    let mut moved = Vec::new_in(copy);
    moved.append(&mut v);
    drop(v);
    moved.extend(10..100);
    if moved.iter().copied().ne(0..100) || other.allocs() != 0 {
        return Err(Broke::Oracle(format!("{moved:?}, {} blocks of the other allocator", other.allocs())));
    }
    drop(moved);
    one.check()?;
    other.check()
}

#[test]
fn every_owner_works_through_a_handle() -> Result<(), Broke> {
    through(|_, alloc| {
        let boxed: Box<[u64], Alloc<'_>> = Box::new_in([1u64, 2, 3], alloc);
        let text = crate::String::from_str_in("through a handle", alloc);
        let arc = Arc::new_in(Box::new_in(4u64, alloc), alloc);
        let shared = Arc::clone(&arc);
        let rc = Rc::new_in(5u64, alloc);
        let rc_copy = Rc::clone(&rc);
        let mut packed = Packed::<u64, Alloc<'_>>::new_in(alloc);
        for n in [1u64, 300, 70_000, u64::MAX] {
            packed.push(&n);
        }
        let borrowed = [7u8, 8, 9];
        let mut cow: Cow<'_, [u8], Alloc<'_>> = Cow::Borrowed(&borrowed);
        cow.to_mut(alloc).push(10);
        let read = (
            *boxed == [1, 2, 3],
            text.as_str() == "through a handle",
            **shared == 4,
            *rc_copy == 5,
            packed.iter().eq([1u64, 300, 70_000, u64::MAX]),
            *cow == [7, 8, 9, 10],
        );
        if read != (true, true, true, true, true, true) {
            return Err(Broke::Oracle(format!("an owner through a handle reads wrong: {read:?}")));
        }
        Ok(())
    })
}

#[test]
fn vec_try_forms_fail_cleanly_through_a_handle() -> Result<(), Broke> {
    under_every_failure("Vec through a handle", |failing| {
        let erased = Erased::new(failing);
        let alloc = erased.handle();
        let mut v = Vec::try_with_capacity_in(2, alloc)?;
        for n in 0..9 {
            let value = Box::try_new_in(n, alloc)?;
            step(&mut v, boxed_values, |v| {
                v.try_push(value).map_err(|refused| {
                    if *refused.value == n { Why::from(refused) } else { Why::Changed(String::from("another value came back")) }
                })
            })?;
        }
        step(&mut v, boxed_values, |v| v.try_reserve(40))?;
        step(&mut v, boxed_values, |v| v.try_shrink_to_fit())?;
        let mut w = Vec::try_with_capacity_in(1, alloc)?;
        step(&mut w, words, |w| w.try_extend_from_slice(&[6; 20]))?;
        if boxed_values(&v) != (0..9).collect::<StdVec<u64>>() || w.len() != 20 {
            return Err(Why::Changed(format!("the pushes gave {:?} and {:?}", boxed_values(&v), words(&w))));
        }
        Ok(())
    })?;
    Ok(())
}

#[test]
fn a_handle_of_an_inline_buffer_grows_in_place_and_refuses_when_full() -> Result<(), Broke> {
    let inline = Inline::<1024>::new();
    let erased = Erased::new(&inline);
    let alloc = erased.handle();
    {
        let mut v: AVec<'_, u64> = Vec::with_capacity_in(4, alloc);
        let first = v.as_ptr();
        v.extend(0..100);
        // The newest block grows in place: `grow` reached the buffer's own, not a copy.
        if v.as_ptr() != first || v.iter().copied().ne(0..100) {
            return Err(Broke::Oracle(String::from("the vector moved or lost values as it grew through the handle")));
        }
        if v.try_reserve(1000).is_ok() {
            return Err(Broke::Oracle(String::from("a full buffer granted through the handle")));
        }
    }
    // Under every failure, with the handle as the base: refusals stay typed, the buffer whole.
    let events = every_failure(&alloc, |failing| {
        let mut s = crate::String::try_with_capacity_in(2, failing)?;
        s.try_push_str("fixed buffers!!!")?;
        let mut v = Vec::try_with_capacity_in(1, failing)?;
        v.try_extend_from_slice(&[1u32, 2, 3, 4, 5, 6])?;
        drop(v);
        drop(s);
        Ok::<(), crate::AllocFailure>(())
    })
    .map_err(|e| Broke::Oracle(format!("{e}")))?;
    if events < 4 {
        return Err(Broke::Oracle(format!("{events} events through the handle")));
    }
    Ok(())
}

#[test]
fn a_vector_over_a_shared_handle_crosses_threads() -> Result<(), Broke> {
    let counted = Counting::new();
    let erased = Erased::new(&counted);
    let shared = erased.shared();
    let mut v: Vec<u64, SharedAlloc<'_>> = Vec::new_in(shared);
    v.extend(0..10);
    // Grown, then freed, on another thread, into the same allocator.
    let sum = std::thread::scope(|s| {
        s.spawn(move || {
            v.extend(10..1000);
            v.iter().sum::<u64>()
        })
        .join()
    })
    .map_err(|_| Broke::Harness(String::from("the other thread panicked")))?;
    let global: Vec<u32, SharedAlloc<'static>> = Vec::from_slice_in(&[1, 2, 3], SharedAlloc::global());
    let global_sum = std::thread::spawn(move || global.iter().sum::<u32>())
        .join()
        .map_err(|_| Broke::Harness(String::from("the other thread panicked")))?;
    // The same handle as the kind functions take.
    let local: Alloc<'_> = shared.local();
    if sum != 499_500 || global_sum != 6 || !local.same(erased.handle()) {
        return Err(Broke::Oracle(format!("sums {sum} and {global_sum}, or `local` is another allocator")));
    }
    counted.check()
}

#[test]
fn the_global_handle_allocates() -> Result<(), Broke> {
    let mut v: AVec<'static, u32> = Vec::new_in(Alloc::global());
    v.extend(0..10_000);
    let mut text = crate::String::new_in(Alloc::global());
    text.push_str("global");
    if v.iter().map(|&n| u64::from(n)).sum::<u64>() != 49_995_000 || text.as_str() != "global" {
        return Err(Broke::Oracle(String::from("the global handle's collections read wrong")));
    }
    Ok(())
}

/// The heap capability (D102): a test's heap and the process's are the one global allocator,
/// so a block made through one handle is freed through another; prints "heap: one allocator".
#[test]
fn heap_is_the_global_allocator() {
    let tests = crate::testing::heap();
    assert!(tests.alloc().same(crate::Heap::process().alloc()), "a test's heap is not the process's");
    assert!(tests.alloc().same(Alloc::global()), "a test's heap is not the global allocator");
    let mut v: AVec<'_, u32> = Vec::new_in(tests.alloc());
    v.extend_from_slice(&[1, 2, 3]);
    let moved: Vec<u32, Alloc<'_>> = Vec::from_slice_in(&v, tests.shared().local());
    drop(v);
    assert_eq!(moved.as_slice(), &[1, 2, 3]);
    std::println!("heap: one allocator");
}

/// The `Default` test heap is the process heap: a vector made over it is freed through a
/// handle of the heap, and a struct holding one is `Default` and `mem::take`n.
#[test]
fn the_default_test_heap_is_the_process_heap() {
    use crate::testing::{TestHeap, heap};

    #[derive(Default)]
    struct Holder {
        heap: TestHeap,
    }

    let mut v: Vec<u32, TestHeap> = Vec::new_in(TestHeap::default());
    v.extend_from_slice(&[4, 5, 6]);
    let (ptr, len, cap, _test_heap) = v.into_raw_parts_with_alloc();
    // SAFETY: the parts are the vector's own, moved out once; its allocator, `TestHeap`, is the
    // process heap's shared handle, which `heap().alloc()` is a handle of.
    let adopted = unsafe { Vec::from_raw_parts_in(ptr, len, cap, heap().alloc()) };
    assert_eq!(adopted.as_slice(), &[4, 5, 6]);
    drop(adopted);
    let mut holder = Holder::default();
    let taken = core::mem::take(&mut holder);
    assert_eq!(taken.heap, holder.heap, "every `TestHeap` is the one heap");
}
