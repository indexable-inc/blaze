//! Names for the addresses CPU samples hold, found when a dump is written, never while
//! sampling. The symbolizer is ix-prof's (crates/ix-prof/src/symbols.rs): the executable's
//! symbol table and its load slide, and `dladdr` for code in shared libraries.

use blaze_hash::HashMap;
use std::alloc::Global;

/// A name for each of `pcs`: the function demangled with its offset, or the library and offset,
/// or the bare address.
pub(crate) fn names(pcs: &[u64]) -> HashMap<u64, String, Global> {
    let mut names = HashMap::new_in(Global);
    names.extend(ix_prof::symbols::names(pcs));
    names
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
