//! The claim-free verify walk: a row of an earlier view is checked by its edges in read order,
//! depth first, with no flight and no lock, marking each row it proves unchanged with a
//! compare-and-swap of its `verified` word. It gives up (and the caller takes the row's flight)
//! at the first edge that changed, or that needs a run, or a row that is running, provisional,
//! made in a cycle or untracked: those are the claimed path's. A row whose kind did not change,
//! or whose input summary shares no bit with the fields written since it was verified, is
//! proved without its edges; a walked row's summary grows by its dependencies' current ones.
//!
//! The walk reads headers untyped: every function row starts with its [`Header`], and the
//! ingredient's vtable gives the row size. With a hook set it gives up too, so that every
//! validation is reported by the claimed path, which may call the hook.

use std::{ptr::NonNull, sync::atomic::Ordering};

use super::{
    counts,
    ingredient::{self, Kind},
    local::{self, Walk},
    row::{
        CYCLE, DURABILITY, Header, PROVISIONAL, RUNNING, UNTRACKED, VALUE, VIEW_MASK, header_at,
        span,
    },
    storage::{FIELD, Storage, color, key_parts, or},
};

/// Edges ahead of the one checked whose colors are prefetched.
const AHEAD: u64 = 4;

/// Verifies the row of `header` (whose word was `word`, of an earlier view, its node `node`)
/// without claiming anything: `true` when it and every row below it it had to look at were
/// marked verified in this view, `false` to take the claimed path.
pub(crate) fn unclaimed(storage: &Storage, header: &Header, word: u32, node: u32) -> bool {
    if storage.has_hook() {
        return false;
    }
    let meta = header.meta.load(Ordering::Relaxed);
    if word & (RUNNING | PROVISIONAL) != 0
        || meta & (CYCLE | UNTRACKED) != 0
        || word & VIEW_MASK == 0
    {
        return false;
    }
    if storage.unchanged_since(meta as u8 & DURABILITY as u8, word & VIEW_MASK)
        || storage.inputs_unchanged(node, word & VIEW_MASK)
    {
        return mark(storage, header, word, node);
    }
    local::with(|local| {
        let base = local.walk.len();
        let proved = walk(storage, &mut local.walk, base, header, word, node);
        local.walk.truncate(base);
        proved
    })
}

fn entry(header: &Header, word: u32, node: u32) -> Walk {
    let edges = header.edges.load(Ordering::Acquire);
    let start = span::start(edges);
    Walk {
        header: std::ptr::from_ref(header),
        node,
        verified: word & VIEW_MASK,
        word,
        next: start,
        end: start + span::len(edges),
        summary: [0; 2],
    }
}

fn walk(
    storage: &Storage,
    stack: &mut Vec<Walk>,
    base: usize,
    header: &Header,
    word: u32,
    node: u32,
) -> bool {
    let view = storage.view_bits();
    let depth = storage.nodes_made() as usize;
    // alloc: none once warm: the walk stack grows to its high-water depth once per thread
    stack.push(entry(header, word, node));
    loop {
        let top = stack.last_mut().expect("the walk's root is on its stack");
        if top.next == top.end {
            let done = *top;
            // SAFETY: the header is in a row page of `storage` (pushed from one), which lives
            // as long as the storage, borrowed for this walk.
            let header = unsafe { &*done.header };
            // The summary goes in before the mark publishes the row in this view.
            storage.grow_summary(done.node, done.summary);
            if !mark(storage, header, done.word, done.node) {
                return false;
            }
            stack.pop();
            if stack.len() == base {
                return true;
            }
            let parent = stack.last_mut().expect("a walked row's reader");
            if header.changed.load(Ordering::Relaxed) > parent.verified {
                return false;
            }
            parent.summary = or(parent.summary, storage.summary_of(done.node));
            continue;
        }
        let at = top.next;
        top.next += 1;
        let verified = top.verified;
        if at + AHEAD < top.end {
            let ahead = storage.edge(at + AHEAD) as usize;
            prefetch::read(storage.colors.get(ahead));
            prefetch::read(storage.summaries.get(ahead));
        }
        let dep = storage.edge(at);
        let seen = storage.color_of(dep);
        let painted = (seen >> 32) as u32;
        let proved = if painted >= FIELD || painted == view {
            seen as u32 <= verified
        } else {
            let Some(header) = function_header(storage, dep) else { return false };
            let word = header.verified.load(Ordering::Acquire);
            let meta = header.meta.load(Ordering::Relaxed);
            if word & (RUNNING | PROVISIONAL) != 0
                || meta & (CYCLE | UNTRACKED) != 0
                || word & VIEW_MASK == 0
            {
                return false;
            }
            if word & VIEW_MASK == view {
                header.changed.load(Ordering::Relaxed) <= verified
            } else if storage.unchanged_since(meta as u8 & DURABILITY as u8, word & VIEW_MASK)
                || storage.inputs_unchanged(dep, word & VIEW_MASK)
            {
                mark(storage, header, word, dep)
                    && header.changed.load(Ordering::Relaxed) <= verified
            } else {
                if stack.len() - base > depth {
                    return false;
                }
                // alloc: none once warm: the walk stack grows to its high-water depth once per thread
                stack.push(entry(header, word, dep));
                continue;
            }
        };
        if !proved {
            return false;
        }
        let top = stack.last_mut().expect("the walked row");
        top.summary = or(top.summary, storage.summary_of(dep));
    }
}

/// The header of the function row whose node is `node`, if its page was made.
fn function_header(storage: &Storage, node: u32) -> Option<&Header> {
    let (index, id, _) = key_parts(storage.key_of(node));
    let vtable = ingredient::vtable(index)?;
    if vtable.kind != Kind::Function {
        return None;
    }
    let dir = storage.dir(index);
    if dir.is_null() {
        return None;
    }
    // SAFETY: a published directory is committed whole for `MAX_PAGES` entries, and
    // `id.page() < MAX_PAGES`.
    let page = NonNull::new(unsafe { &*dir.add(id.page()) }.load(Ordering::Acquire))?;
    // SAFETY: the page is the function's, made with rows of its vtable's `row_size`; the
    // header lives as long as `storage`.
    Some(unsafe { header_at(page, id.slot(), vtable.row_size) })
}

/// Marks a row verified in this view, keeping whether it has its value: `false` when another
/// task changed its word meanwhile.
fn mark(storage: &Storage, header: &Header, word: u32, node: u32) -> bool {
    let view = storage.view_bits();
    let to = view | (word & VALUE);
    match header.verified.compare_exchange(word, to, Ordering::AcqRel, Ordering::Acquire) {
        Ok(_) => {
            storage.paint(node, color(view, header.changed.load(Ordering::Relaxed)));
            counts::count(key_parts(storage.key_of(node)).0, counts::VERIFIED);
            true
        }
        Err(now) => now == to,
    }
}

/// A prefetch hint for a color the walk reads soon. The one unsafe concern: the instruction.
mod prefetch {
    #[inline(always)]
    pub(super) fn read<T>(at: &T) {
        let at = std::ptr::from_ref(at);
        #[cfg(target_arch = "x86_64")]
        // SAFETY: `prefetcht0` is a hint: it never faults, writes nothing, and `at` is a
        // valid reference besides.
        unsafe {
            std::arch::asm!("prefetcht0 [{at}]", at = in(reg) at, options(nostack, preserves_flags, readonly));
        }
        #[cfg(target_arch = "aarch64")]
        // SAFETY: `prfm pldl1keep` is a hint: it never faults, writes nothing, and `at` is a
        // valid reference besides.
        unsafe {
            std::arch::asm!("prfm pldl1keep, [{at}]", at = in(reg) at, options(nostack, preserves_flags, readonly));
        }
        #[cfg(not(any(target_arch = "x86_64", target_arch = "aarch64")))]
        let _ = at;
    }
}
