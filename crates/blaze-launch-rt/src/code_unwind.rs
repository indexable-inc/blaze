//! A code record's unwind entry told to the unwinder before its slot's word is published
//! (docs/v2/plan/design/run.md, "End state", "Panics"; piece 11, part C).
//!
//! The bytes are blaze-code eh.rs's ([`blaze_code::eh::entry`]: one CIE, one FDE, the LSDA),
//! made sans-IO from the record's unwind rows at the address the entry is copied to. This file
//! is the order and the one call: [`publish`] runs a slot's publication as four effects in a
//! fixed order, and the register effect is [`register`], a thin call of [`crate::unwind::register`].
//!
//! **The order.** A word published names a body any thread may call at once; a panic in that
//! call (or in anything it calls) walks the body's frame, and the unwinder finds the frame only
//! by its registered entry. So the entry is registered before the word's release store: copy
//! the code, make it visible to instruction fetch, register, store. An entry registered after
//! the store leaves a window in which a thread in the body panics and the unwinder, finding no
//! entry, ends the process (`_Unwind_RaiseException` gives `_URC_END_OF_STACK`, and std's
//! panic aborts). The test below drives [`publish`] with every point a reader can look at, and
//! catches the plant [`Plant::RegisterAfterStore`].
//!
//! **Which interface: `__register_frame`, one function per call.** It maps one to one onto a
//! slot's life (registered at publish, taken back at free, [`crate::unwind::deregister`]); it
//! is in the library the launcher links on both hosts (libSystem; libgcc_s on Linux) and is
//! already the launcher's (unwind.rs). macOS's other interface,
//! `__unw_add_find_dynamic_unwind_sections`, registers a callback that finds sections by
//! address, which would let one call cover the whole code area; it is the path if one entry
//! per function proves slow, and is not used now. TO VERIFY BY A TEST: (1) Apple's
//! `__register_frame` takes the address of one FDE, libgcc's the start of a run ended by a
//! zero length (unwind.rs `handle` assumes both; eh.rs ends its bytes with the zero word, so
//! one buffer serves both); (2) an entry registered by one thread is found by an unwind on
//! another as soon as the call returns (Apple's `DwarfFDECache` and libgcc's object list take
//! a lock, which would make it so); (3) the cost of a lookup among many registered FDEs on
//! macOS (`DwarfFDECache` is searched linearly).

use blaze_code::eh::Entry;

/// One step of a slot's publication, in the order [`publish`] makes them.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Step {
    /// The body's bytes and its entry's bytes copied to their addresses.
    Copy,
    /// The new code made visible to instruction fetch (the cache maintenance).
    Flush,
    /// The entry told to the unwinder ([`register`]).
    Register,
    /// The slot's word stored with release: from here any thread may call the body.
    Store,
}

/// A planted bug for the test of [`publish`]'s order (rule 43): `None` in the driver.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Plant {
    None,
    /// The entry registered after the word's store.
    RegisterAfterStore,
}

/// What a publication does, one method per [`Step`]; the driver's impl does the IO, a test's
/// records it.
pub trait Effects {
    fn step(&mut self, step: Step);
}

/// Publishes a slot: the four steps in their order (the module's doc says why).
pub fn publish(effects: &mut impl Effects, plant: Plant) {
    let order = match plant {
        Plant::None => [Step::Copy, Step::Flush, Step::Register, Step::Store],
        Plant::RegisterAfterStore => [Step::Copy, Step::Flush, Step::Store, Step::Register],
    };
    for step in order {
        effects.step(step);
    }
}

/// Tells the unwinder of `entry`, whose bytes ([`blaze_code::eh::entry`]'s, made for
/// `Placed::entry_at == at`) are at address `at`: the [`Step::Register`] of a publication.
///
/// # Safety
///
/// As [`crate::unwind::register`]: the bytes stay at `at` unchanged until the matching
/// [`crate::unwind::deregister`], and the function they describe is at the address they say.
pub unsafe fn register(at: usize, entry: Entry) {
    // SAFETY: the caller's contract, which is register's.
    unsafe { crate::unwind::register(at, entry.fde) }
}

#[cfg(test)]
mod tests {
    use super::{Effects, Plant, Step, publish};

    /// The model: what a reader sees after the first `seen` steps.
    struct Seen {
        done: [Option<Step>; 4],
        len: usize,
    }

    impl Effects for Seen {
        fn step(&mut self, step: Step) {
            self.done[self.len] = Some(step);
            self.len += 1;
        }
    }

    /// The oracle, at every point a reader can look (after 0 to 4 steps): a reader that sees
    /// the word published may call the body and panic in it, so the entry must be registered
    /// by then. Gives the first point at which it is not.
    fn first_unwind_without_entry(plant: Plant) -> Option<usize> {
        let mut seen = Seen { done: [None; 4], len: 0 };
        publish(&mut seen, plant);
        assert_eq!(seen.len, 4, "four steps");
        (0..=4).find(|&point| {
            let done = &seen.done[..point];
            done.contains(&Some(Step::Store)) && !done.contains(&Some(Step::Register))
        })
    }

    /// The order holds at every point, and the plant (registration after the store) is caught
    /// at the point between them. Prints "publish: the entry is registered before the word at
    /// every point; registration after the store caught after step 3".
    #[test]
    fn the_entry_is_registered_before_the_word_is_published() {
        assert_eq!(first_unwind_without_entry(Plant::None), None);
        let caught = first_unwind_without_entry(Plant::RegisterAfterStore).expect("the plant is caught");
        assert_eq!(caught, 3);
        println!("publish: the entry is registered before the word at every point; registration after the store caught after step {caught}");
    }

    /// The symbols the unwind path calls are in the process (rule `macos-private-apis` asks
    /// it of private ones; these are public, and a missing one would fail only at the first
    /// panic). Prints "unwind symbols: __register_frame, __deregister_frame, _Unwind_Resume
    /// resolve".
    #[test]
    fn the_unwind_symbols_resolve() {
        for name in [c"__register_frame", c"__deregister_frame", c"_Unwind_Resume"] {
            #[cfg(target_os = "macos")]
            // SAFETY: RTLD_DEFAULT searches loaded images and name is a live C string.
            let found = unsafe { blaze_sys::posix::thread::dlsym(blaze_sys::posix::thread::RTLD_DEFAULT, name.as_ptr()) };
            #[cfg(target_os = "linux")]
            // SAFETY: a lookup, by a C string's name, among the images this process has loaded.
            let found = unsafe { libc::dlsym(libc::RTLD_DEFAULT, name.as_ptr()) };
            assert!(!found.is_null(), "{name:?} is not in this process");
        }
        println!("unwind symbols: __register_frame, __deregister_frame, _Unwind_Resume resolve");
    }
}
