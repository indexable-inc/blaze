//! Names for the addresses CPU samples hold, found when a dump or flush is written, never while
//! sampling. The symbolizer is ix-prof's (crates/ix-prof/src/symbols.rs): each address is named
//! by function, never with an offset, with the functions DWARF says were inlined there, so all
//! of a function's samples are one frame; a wasm proc macro's code by the function of its module
//! (`ix_prof::jit`); a shared library's by its exported symbol.
//!
//! Beside the names, a flush writes each code image an address of it is in, once a process
//! ([`ImagesWritten`]): the `images` table, which names `samples.stack_pcs` and `panics.stack_pcs` after the process
//! is gone.

use std::alloc::Global;

use arrow_array::RecordBatch;
use arrow_schema::ArrowError;
use blaze_hash::{HashMap, HashSet};

use crate::batches::{self, ImageRow, Process};

/// The address a stack's frame `i` (innermost first, as the samplers keep them) is named at: the
/// innermost is where the thread was, each other a return address, whose call is the
/// instruction before it. Naming a return address itself would name whatever follows the call:
/// the next line, or the next function when the call was a function's last instruction (a call
/// to a function that never returns).
pub(crate) fn lookup(i: usize, pc: u64) -> u64 {
    if i == 0 { pc } else { pc.saturating_sub(1) }
}

/// The frames of each address `stacks` hold (each innermost first), by the address it is
/// looked up at ([`lookup`]): its function and those inlined into it, outermost first. Every
/// address gets at least one name, `[code in no image]` at worst.
pub(crate) fn frames<'a>(
    stacks: impl Iterator<Item = &'a [u64]>,
) -> HashMap<u64, Vec<String>, Global> {
    #[cfg(test)]
    ASKED.with(|it| it.set(it.get() + 1));
    // alloc: a flush's addresses, once a flush
    let mut at: Vec<u64> =
        stacks.flat_map(|stack| stack.iter().enumerate().map(|(i, pc)| lookup(i, *pc))).collect();
    at.sort_unstable();
    at.dedup();
    let mut frames = HashMap::new_in(Global);
    frames.extend(ix_prof::symbols::functions(&at));
    frames
}

/// The named frames of `pcs` (innermost first, as the samplers and the panic ring keep them),
/// outermost first, as `samples.stack` holds them: each address's function and those inlined
/// into it, from `frames` ([`frames`]); an address it has none for, bare.
pub(crate) fn stack(pcs: &[u64], frames: &HashMap<u64, Vec<String>, Global>) -> Vec<String> {
    // alloc: a stack's names, once a row
    let mut out = Vec::new();
    for (i, pc) in pcs.iter().enumerate().rev() {
        match frames.get(&lookup(i, *pc)) {
            // clone: a stack's names, once a row
            Some(named) => out.extend(named.iter().cloned()),
            // alloc: as above
            None => out.push(format!("{pc:#x}")),
        }
    }
    out
}

/// The named frames of `pcs` (innermost first), addresses of another process of this same
/// executable loaded `slide` from its file's addresses (a crash record's), outermost first, as
/// [`stack`] names this process's: each address in the executable's code moved to where this
/// process has it and named by its function and those inlined there; any other (a shared
/// library's in that process, whose loads are not known here) bare.
pub(crate) fn stack_slid(pcs: &[u64], slide: u64) -> Vec<String> {
    let ours = ix_prof::images::loaded().into_iter().find(|it| it.main);
    // alloc: a crash's addresses, once a crash recovered
    let here: Vec<Option<u64>> = pcs
        .iter()
        .enumerate()
        .map(|(i, pc)| {
            let image = ours.as_ref()?;
            let at = lookup(i, pc.wrapping_sub(slide).wrapping_add(image.slide));
            image.contains(at).then_some(at)
        })
        .collect();
    // alloc: as above
    let mut asked: Vec<u64> = here.iter().flatten().copied().collect();
    asked.sort_unstable();
    asked.dedup();
    let named = ix_prof::symbols::functions(&asked);
    // alloc: a crash's names, once a crash recovered
    let mut out = Vec::new();
    for (pc, at) in pcs.iter().zip(here.iter().copied()).rev() {
        match at.and_then(|at| named.get(&at)) {
            // clone: a crash's names, once a crash recovered
            Some(frames) => out.extend(frames.iter().cloned()),
            // alloc: as above
            None => out.push(format!("{pc:#x}")),
        }
    }
    out
}

#[cfg(test)]
thread_local! {
    /// Calls to [`frames`] on this thread: a test of what asks for names reads it.
    pub(crate) static ASKED: std::cell::Cell<u64> = const { std::cell::Cell::new(0) };
}

/// The code images a history has written rows of, so each is written once a process: by its
/// load address and file (one unloaded and another loaded at its address is a new image).
pub(crate) struct ImagesWritten {
    written: HashSet<(u64, String), Global>,
}

impl ImagesWritten {
    pub(crate) fn new() -> ImagesWritten {
        // alloc: once a history
        ImagesWritten { written: HashSet::new_in(Global) }
    }

    /// The `images` rows of the images `stacks`' addresses are in that this history has not
    /// written, sorted by load address, written at `seen_ns`. An address in no image (JIT
    /// code, a wild frame pointer) adds none. No stacks lists no images.
    pub(crate) fn batch<'a>(
        &mut self,
        stacks: impl Iterator<Item = &'a [u64]>,
        seen_ns: i64,
        process: Process<'_>,
    ) -> Result<RecordBatch, ArrowError> {
        // alloc: a flush's addresses, once a flush
        let mut pcs: Vec<u64> = stacks.flat_map(|it| it.iter().copied()).collect();
        if pcs.is_empty() {
            return batches::images(&[], None, seen_ns, process);
        }
        pcs.sort_unstable();
        pcs.dedup();
        let loaded = ix_prof::images::loaded();
        // alloc: a flush's new images, once a flush that has any
        let mut rows = Vec::new();
        for image in &loaded {
            let lo = pcs.partition_point(|pc| *pc < image.base);
            let holds = pcs.get(lo).is_some_and(|pc| image.contains(*pc));
            // clone: an image's path, once a process an image
            if !holds || !self.written.insert((image.base, image.path.clone())) {
                continue;
            }
            rows.push(ImageRow {
                // clone: a row's path, once a process an image
                path: image.path.clone(),
                // clone: as above
                build_id: image.build_id.clone(),
                blake3: ix_prof::images::blake3(image),
                base: image.base,
                size: image.size,
                slide: image.slide,
                names: if image.main { ix_prof::symbols::names_kind() } else { "exports" },
            });
        }
        batches::images(&rows, ix_prof::images::boot(), seen_ns, process)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[inline(never)]
    fn a_function_to_find() -> u64 {
        std::hint::black_box(a_function_to_find as fn() -> u64 as usize as u64)
    }

    /// An address is named by its function, with no offset: every sample in it is one frame.
    #[test]
    fn an_address_in_a_function_is_named_by_it_without_an_offset() {
        let pc = a_function_to_find() + 4;
        let frames = frames([&[pc][..]].into_iter());
        let named = &frames[&pc];
        assert!(named.last().unwrap().contains("a_function_to_find"), "{named:?}");
        assert!(named.iter().all(|it| !it.contains("+0x")), "{named:?}");
    }

    /// The image of an address a flush wrote is written once, and an empty flush writes none.
    #[test]
    fn an_image_is_written_once_a_process() {
        let pc = a_function_to_find() + 4;
        let process = Process { pid: 7, name: "blaze test", commit: None };
        let mut written = ImagesWritten::new();
        assert_eq!(written.batch(std::iter::empty(), 1, process).unwrap().num_rows(), 0);
        let first = written.batch([&[pc][..]].into_iter(), 1, process).unwrap();
        assert_eq!(first.num_rows(), 1, "the executable holds the address");
        let again = written.batch([&[pc][..]].into_iter(), 2, process).unwrap();
        assert_eq!(again.num_rows(), 0, "written once");
    }
}
