//! Names for the addresses CPU samples hold, found when a dump is written, never while
//! sampling. The symbolizer is ix-prof's (crates/ix-prof/src/symbols.rs): the executable's
//! symbol table and its load slide, and `dladdr` for code in shared libraries.

use blaze_hash::HashMap;
use std::alloc::Global;

/// A name for each of `pcs`: the function demangled with its offset, or the library and offset,
/// or the bare address.
pub(crate) fn names(pcs: &[u64]) -> HashMap<u64, String, Global> {
    #[cfg(test)]
    ASKED.with(|it| it.set(it.get() + 1));
    let mut names = HashMap::new_in(Global);
    names.extend(ix_prof::symbols::names(pcs));
    names
}

/// A name for each of `pcs` of another process of this same executable, loaded `slide` from its
/// file's addresses: its code in the executable named by this one's symbols, the rest bare
/// addresses (where its libraries were loaded is not known here).
pub(crate) fn names_slid(pcs: &[u64], slide: u64) -> HashMap<u64, String, Global> {
    let mut names = HashMap::new_in(Global);
    names.extend(ix_prof::symbols::names_slid(pcs, slide));
    names
}

#[cfg(test)]
thread_local! {
    /// Calls to [`names`] on this thread: a test of what asks for names reads it.
    pub(crate) static ASKED: std::cell::Cell<u64> = const { std::cell::Cell::new(0) };
}

#[cfg(test)]
mod tests {
    use super::*;

    #[inline(never)]
    fn a_function_to_find() -> u64 {
        std::hint::black_box(a_function_to_find as fn() -> u64 as usize as u64)
    }

    #[test]
    fn an_address_in_a_function_is_named_by_it() {
        let pc = a_function_to_find() + 4;
        let names = names(&[pc]);
        let name = &names[&pc];
        assert!(name.contains("a_function_to_find"), "{name}");
    }
}
