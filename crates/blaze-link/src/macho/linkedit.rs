//! The tables of `__LINKEDIT` dyld reads (`<mach-o/loader.h>`): the opcodes that say which
//! addresses it slides (rebase) and which it binds to a symbol of a library, the trie of what
//! the image exports, and the starts of its functions.
//!
//! Each table is appended to one buffer, the `__LINKEDIT` being written, and padded to 8 bytes
//! from where it started: [`capacity`] sizes that buffer for all four, so encoding them grows
//! it never. Each returns how many bytes it appended, 0 for a table with nothing in it.
//!
//! The rebase opcodes and the function starts are each one stream whose next bytes depend on
//! the cursor the bytes before left. Both are cut where that cursor follows from the one
//! address before the cut alone ([`cuts`]), the pieces encoded in parallel and joined in
//! order: the bytes the one stream gives.

#![forbid(unsafe_code)]

use std::borrow::Cow;

use blaze_graph::par;
use blaze_ids::radix;
use object::macho;

/// The fewest addresses a piece of a stream encodes: below two pieces' worth the stream is
/// encoded whole.
const CHUNK: usize = 16 * 1024;

/// Where the pieces of a stream over `len` addresses start: 0, then the first index at or past
/// each `every`th from the last cut where `fresh` holds (where the encoder's cursor follows from
/// the address before). `every` is at least `CHUNK` and makes no more pieces than the pool has
/// threads, so the pieces run in one round; the bytes are the same for any cuts.
fn cuts(len: usize, fresh: impl Fn(usize) -> bool) -> Vec<usize> {
    let every = CHUNK.max(len.div_ceil(par::threads().max(1)));
    // alloc: once per table, one cut per piece
    let mut cuts = Vec::with_capacity(len / every + 1);
    cuts.push(0);
    let mut at = every;
    while at < len {
        if fresh(at) {
            cuts.push(at);
            at += every;
        } else {
            at += 1;
        }
    }
    cuts
}

/// Appends `pieces` to `out` in order, copied in parallel; the first error in order instead.
fn join(out: &mut Vec<u8>, pieces: Vec<Result<Vec<u8>, String>>) -> Result<(), String> {
    // alloc: once per table, one entry per piece
    let mut done = Vec::with_capacity(pieces.len());
    for piece in pieces {
        done.push(piece?);
    }
    let at = out.len();
    out.resize(at + done.iter().map(Vec::len).sum::<usize>(), 0);
    let mut rest = &mut out[at..];
    // alloc: once per table, one slice per piece
    let mut jobs = Vec::with_capacity(done.len());
    for piece in &done {
        let (these, after) = std::mem::take(&mut rest).split_at_mut(piece.len());
        jobs.push((these, &piece[..]));
        rest = after;
    }
    par::for_each(jobs, |(out, piece)| out.copy_from_slice(piece));
    Ok(())
}

/// A segment, by its index among the image's `LC_SEGMENT_64`s.
#[derive(Clone)]
pub(super) struct Span {
    pub name: &'static str,
    pub start: u64,
    pub end: u64,
    pub writable: bool,
}

/// An address dyld binds to `name` of the library `ordinal` (or a `BIND_SPECIAL_DYLIB_*`).
/// 40 bytes; the name is borrowed (from the symbol table's strings).
#[derive(Clone, PartialEq, Eq, Debug)]
pub(super) struct Bind<'a> {
    pub addr: u64,
    pub ordinal: i64,
    /// As Mach-O names it, with its underscore.
    pub name: &'a [u8],
    pub weak: bool,
    pub addend: i64,
}

pub(super) use blaze_format::leb::{write_i64 as sleb, write_u64 as uleb};

fn uleb_len(value: u64) -> u64 {
    blaze_format::leb::len_u64(value) as u64
}

/// The most bytes the four tables take together: `rebases` pointers to slide, `binds`,
/// `exports` and `functions` starts. Each bound holds for the worst case of its encoding
/// (a ULEB of an address is at most 10 bytes), plus 8 bytes of padding each.
pub(super) fn capacity<N: AsRef<[u8]>>(
    rebases: usize,
    binds: &[Bind<'_>],
    exports: &[(N, u64, u64)],
    functions: usize,
) -> usize {
    // A run's opcodes: a segment and offset (1 + 10), a count (1 + 10); and the type and DONE.
    let rebase = 22 * rebases + 2;
    // Per bind: the ordinal (1 + 10), the symbol (2 + its name), the addend (1 + 10), the
    // address (1 + 10) and DO_BIND; the type and DONE.
    let bind = binds.iter().map(|it| 36 + it.name.len()).sum::<usize>() + 2;
    // At most two nodes and two edges an export: a node's info length, flags and address (at
    // most 1 + 10 + 10) and its edge count; an edge's label (at most its export's name), NUL
    // and child offset (5, as the trie is under 4 GiB).
    let trie = exports.iter().map(|it| 2 * 22 + 2 * (it.0.as_ref().len() + 6)).sum::<usize>();
    let starts = 10 * functions + 1;
    rebase + bind + trie + starts + 4 * 8
}

/// Pads the table started at `start` of `out` to 8 bytes and says its length.
fn finish(out: &mut Vec<u8>, start: usize) -> usize {
    let len = (out.len() - start).next_multiple_of(8);
    out.resize(start + len, 0);
    len
}

/// `values` in ascending order: borrowed when they already are, as the core lists them.
fn sorted(values: &[u64]) -> Cow<'_, [u64]> {
    if values.is_sorted() {
        return Cow::Borrowed(values);
    }
    // alloc: only for addresses out of order, which `Placed` lists in order; once per link
    let mut values = values.to_vec();
    radix::sort(&mut values);
    Cow::Owned(values)
}

/// The segment `addr` is in, by index; an error when it is in none or dyld can't write it.
fn segment_of(spans: &[Span], addr: u64, what: &str) -> Result<usize, String> {
    let nth = (spans.iter())
        .position(|it| (it.start..it.end).contains(&addr))
        // alloc: error path
        .ok_or_else(|| format!("{what} at {addr:#x} is in no segment"))?;
    if !spans[nth].writable {
        // alloc: error path
        return Err(format!(
            "{what} at {addr:#x} is in {}, which the loader can't write: a pointer there would \
             need a text relocation",
            spans[nth].name
        ));
    }
    if nth > 15 {
        // alloc: error path
        return Err(format!("{what} at {addr:#x} is in segment {nth}, past the 16 opcodes name"));
    }
    Ok(nth)
}

/// Appends the rebase opcodes of `addrs`, each a pointer dyld adds the slide to.
///
/// Cut at gaps between pointers, where a run always starts: the cursor before one is the
/// segment of the pointer before and 8 past it.
pub(super) fn rebases(addrs: &[u64], spans: &[Span], out: &mut Vec<u8>) -> Result<usize, String> {
    let addrs = sorted(addrs);
    if let Some(twice) = par::find_map_first(par::windows(&addrs, 2), |it| (it[0] == it[1]).then_some(it)) {
        // alloc: error path
        return Err(format!("the address {:#x} is to be slid twice", twice[0]));
    }
    if addrs.is_empty() {
        return Ok(0);
    }
    let start = out.len();
    out.push(macho::REBASE_OPCODE_SET_TYPE_IMM.0 | macho::REBASE_TYPE_POINTER.0);
    let cuts = cuts(addrs.len(), |at| addrs[at] != addrs[at - 1].wrapping_add(8));
    if cuts.len() == 1 {
        // Runs of adjacent pointers take 2 to 3 bytes each; scattered ones about 3.
        out.reserve(addrs.len() * 3 + 16);
        rebase_runs(&addrs, None, spans, out)?;
    } else {
        let pieces = par::map(0..cuts.len(), |nth| {
            let (from, to) = (cuts[nth], cuts.get(nth + 1).copied().unwrap_or(addrs.len()));
            // A pointer in no segment before the cut is an error of the piece before.
            let cursor = from.checked_sub(1).and_then(|before| {
                let addr = addrs[before];
                spans.iter().position(|it| (it.start..it.end).contains(&addr)).map(|segment| (segment, addr + 8))
            });
            // alloc: once per piece of CHUNK pointers
            let mut piece = Vec::with_capacity((to - from) * 3 + 16);
            rebase_runs(&addrs[from..to], cursor, spans, &mut piece).map(|()| piece)
        }); // alloc: once per table, one result per piece
        join(out, pieces)?;
    }
    out.push(macho::REBASE_OPCODE_DONE.0);
    Ok(finish(out, start))
}

/// The opcodes of the runs of `addrs` (sorted, distinct, starting a run), from dyld's cursor
/// at `cursor` (its segment and address).
fn rebase_runs(addrs: &[u64], mut cursor: Option<(usize, u64)>, spans: &[Span], out: &mut Vec<u8>) -> Result<(), String> {
    let mut rest = addrs;
    while let Some(&first) = rest.first() {
        let segment = segment_of(spans, first, "a pointer to slide")?;
        let end = spans[segment].end;
        let run = (rest.iter().zip(0u64..))
            .take_while(|(it, nth)| **it == first + nth * 8 && **it + 8 <= end)
            .count()
            .max(1);
        match cursor {
            Some((at, address)) if at == segment && first >= address => {
                if first > address {
                    out.push(macho::REBASE_OPCODE_ADD_ADDR_ULEB.0);
                    uleb(out, first - address);
                }
            }
            _ => {
                out.push(macho::REBASE_OPCODE_SET_SEGMENT_AND_OFFSET_ULEB.0 | segment as u8);
                uleb(out, first - spans[segment].start);
            }
        }
        if run < 16 {
            out.push(macho::REBASE_OPCODE_DO_REBASE_IMM_TIMES.0 | run as u8);
        } else {
            out.push(macho::REBASE_OPCODE_DO_REBASE_ULEB_TIMES.0);
            uleb(out, run as u64);
        }
        cursor = Some((segment, first + run as u64 * 8));
        rest = &rest[run..];
    }
    Ok(())
}

/// Appends the bind opcodes of `binds`, sorted here by symbol so each symbol is named once.
pub(super) fn binds(binds: &mut [Bind<'_>], spans: &[Span], out: &mut Vec<u8>) -> Result<usize, String> {
    if binds.is_empty() {
        return Ok(0);
    }
    // In place: a radix sort by key of more than 64 binds would allocate its pairs. Equal
    // addresses are an error, so the order among them doesn't matter.
    binds.sort_unstable_by_key(|it| it.addr);
    if let Some(twice) = binds.windows(2).find(|it| it[0].addr == it[1].addr) {
        // alloc: error path
        return Err(format!("the address {:#x} is to be bound twice", twice[0].addr));
    }
    // The addresses are distinct, so with the address last this is a total order: the same
    // as a stable sort by symbol of the binds in address order, without its buffer.
    binds.sort_unstable_by(|a, b| {
        (a.ordinal, a.name, a.weak, a.addend, a.addr).cmp(&(b.ordinal, b.name, b.weak, b.addend, b.addr))
    });
    let start = out.len();
    // Each bind's address and opcode take about 3 bytes; each symbol its name and 2.
    out.reserve(binds.len() * 4 + 16);
    out.push(macho::BIND_OPCODE_SET_TYPE_IMM.0 | macho::BIND_TYPE_POINTER.0);
    let (mut ordinal, mut symbol, mut addend) = (None, None::<(&[u8], bool)>, 0i64);
    let mut cursor: Option<(usize, u64)> = None;
    for bind in binds.iter() {
        if ordinal != Some(bind.ordinal) {
            match bind.ordinal {
                ..=0 if bind.ordinal >= -15 => out.push(
                    macho::BIND_OPCODE_SET_DYLIB_SPECIAL_IMM.0
                        | (bind.ordinal as u8 & macho::BIND_IMMEDIATE_MASK),
                ),
                1..=15 => out.push(macho::BIND_OPCODE_SET_DYLIB_ORDINAL_IMM.0 | bind.ordinal as u8),
                16.. => {
                    out.push(macho::BIND_OPCODE_SET_DYLIB_ORDINAL_ULEB.0);
                    uleb(out, bind.ordinal as u64);
                }
                // alloc: error path
                other => return Err(format!("a bind to the library ordinal {other}")),
            }
            ordinal = Some(bind.ordinal);
        }
        if symbol != Some((bind.name, bind.weak)) {
            if bind.name.is_empty() || bind.name.contains(&0) {
                // alloc: error path
                return Err(format!("a bind at {:#x} to a symbol of no name", bind.addr));
            }
            let flags = if bind.weak { macho::BIND_SYMBOL_FLAGS_WEAK_IMPORT.0 } else { 0 };
            out.push(macho::BIND_OPCODE_SET_SYMBOL_TRAILING_FLAGS_IMM.0 | flags);
            out.extend_from_slice(bind.name);
            out.push(0);
            symbol = Some((bind.name, bind.weak));
        }
        if addend != bind.addend {
            out.push(macho::BIND_OPCODE_SET_ADDEND_SLEB.0);
            sleb(out, bind.addend);
            addend = bind.addend;
        }
        let segment = segment_of(spans, bind.addr, "a pointer to bind")?;
        match cursor {
            Some((at, address)) if at == segment && bind.addr >= address => {
                if bind.addr > address {
                    out.push(macho::BIND_OPCODE_ADD_ADDR_ULEB.0);
                    uleb(out, bind.addr - address);
                }
            }
            _ => {
                out.push(macho::BIND_OPCODE_SET_SEGMENT_AND_OFFSET_ULEB.0 | segment as u8);
                uleb(out, bind.addr - spans[segment].start);
            }
        }
        out.push(macho::BIND_OPCODE_DO_BIND.0);
        cursor = Some((segment, bind.addr + 8));
    }
    out.push(macho::BIND_OPCODE_DONE.0);
    Ok(finish(out, start))
}

/// A node of the export trie: 16 bytes; its edges a CSR range of [`Edge`]s.
#[derive(Clone, Copy)]
struct Node {
    /// The export it ends, or `u32::MAX`.
    terminal: u32,
    edges: u32,
    count: u32,
    offset: u32,
}

/// An edge of the export trie: its label is `name[from..to]` of the export `export`, so no
/// label is copied. 16 bytes.
#[derive(Clone, Copy)]
struct Edge {
    export: u32,
    from: u32,
    to: u32,
    child: u32,
}

const _: () = assert!(size_of::<Node>() == 16 && size_of::<Edge>() == 16);

/// Appends the export trie of `exports` (Mach-O name, `EXPORT_SYMBOL_FLAGS_*`, address from the
/// image's start), as dyld and `dlsym` walk it and ld64 writes it: nodes depth first, edges by
/// label, each offset a ULEB settled when no node moves.
///
/// Built from the names in order in one pass: a node's names are a range of them sharing its
/// prefix, its edges the runs of one next byte, each edge's label the run's common prefix (that
/// of its first and last name). Every node but the root ends a name or has two edges, so there
/// are at most `2n` nodes and edges for `n` names: four scratch columns sized so once (nodes,
/// edges, the work stack and the depth-first order), nothing allocated per node or edge.
pub(super) fn trie<N: AsRef<[u8]> + Ord>(exports: &mut [(N, u64, u64)], out: &mut Vec<u8>) -> Result<usize, String> {
    if exports.is_empty() {
        return Ok(0);
    }
    if !exports.is_sorted() {
        exports.sort_unstable();
    }
    let name = |nth: usize| exports[nth].0.as_ref();
    for nth in 0..exports.len() {
        if name(nth).is_empty() || name(nth).contains(&0) {
            // alloc: error path
            return Err("an export of no name".to_owned());
        }
        if nth > 0 && name(nth - 1) == name(nth) {
            // alloc: error path
            return Err(format!("`{}` is exported twice", String::from_utf8_lossy(name(nth))));
        }
    }
    if exports.len() >= (u32::MAX / 2) as usize {
        // alloc: error path
        return Err(format!("{} exports: an export trie names fewer than 2^31", exports.len()));
    }

    let most = 2 * exports.len();
    let empty = Node { terminal: u32::MAX, edges: 0, count: 0, offset: 0 };
    let mut nodes = Vec::with_capacity(most);
    let mut edges: Vec<Edge> = Vec::with_capacity(most);
    // A node, the range of names under it, and its prefix's length. A node's children are
    // pushed last edge first, so the nodes are popped depth first with edges by label: the
    // order they are written in.
    let mut work: Vec<(usize, usize, usize, usize)> = Vec::with_capacity(most);
    let mut order: Vec<u32> = Vec::with_capacity(most);
    nodes.push(empty);
    work.push((0, 0, exports.len(), 0));
    while let Some((node, mut lo, hi, depth)) = work.pop() {
        order.push(node as u32);
        if name(lo).len() == depth {
            nodes[node].terminal = lo as u32;
            lo += 1;
        }
        nodes[node].edges = edges.len() as u32;
        let children = work.len();
        while lo < hi {
            let byte = name(lo)[depth];
            let end = lo + exports[lo..hi].partition_point(|it| it.0.as_ref()[depth] == byte);
            let (first, last) = (name(lo), name(end - 1));
            let common = depth
                + 1
                + (first[depth + 1..].iter().zip(&last[depth + 1..]))
                    .take_while(|(a, b)| a == b)
                    .count();
            let child = nodes.len();
            nodes.push(empty);
            let (from, to) = (depth as u32, common as u32);
            edges.push(Edge { export: lo as u32, from, to, child: child as u32 });
            work.push((child, lo, end, common));
            lo = end;
        }
        work[children..].reverse();
        nodes[node].count = edges.len() as u32 - nodes[node].edges;
        if nodes[node].count > 255 {
            // alloc: error path
            return Err("an export trie node of more than 255 edges".to_owned());
        }
    }

    let info_len = |node: &Node| match node.terminal {
        u32::MAX => 0,
        export => {
            let (_, flags, addr) = &exports[export as usize];
            uleb_len(*flags) + uleb_len(*addr)
        }
    };
    let label = |edge: &Edge| &exports[edge.export as usize].0.as_ref()[edge.from as usize..edge.to as usize];
    let edges_of = |node: &Node| &edges[node.edges as usize..(node.edges + node.count) as usize];

    let mut len;
    loop {
        len = 0u64;
        let mut moved = false;
        for &node in &order {
            // alloc: error path
            let offset = u32::try_from(len).map_err(|_| "an export trie past 4 GiB".to_owned())?;
            moved |= nodes[node as usize].offset != offset;
            nodes[node as usize].offset = offset;
            let it = &nodes[node as usize];
            let info = info_len(it);
            len += uleb_len(info) + info + 1;
            for edge in edges_of(it) {
                len += u64::from(edge.to - edge.from) + 1
                    + uleb_len(u64::from(nodes[edge.child as usize].offset));
            }
        }
        if !moved {
            break;
        }
    }

    let start = out.len();
    out.reserve((len as usize).next_multiple_of(8));
    for &node in &order {
        let node = &nodes[node as usize];
        debug_assert_eq!((out.len() - start) as u64, u64::from(node.offset));
        uleb(out, info_len(node));
        if node.terminal != u32::MAX {
            let (_, flags, addr) = &exports[node.terminal as usize];
            uleb(out, *flags);
            uleb(out, *addr);
        }
        out.push(node.count as u8);
        for edge in edges_of(node) {
            out.extend_from_slice(label(edge));
            out.push(0);
            uleb(out, u64::from(nodes[edge.child as usize].offset));
        }
    }
    Ok(finish(out, start))
}

/// Appends `LC_FUNCTION_STARTS`: the functions' addresses as ULEB distances, the first from
/// `text`.
///
/// Cut anywhere: the distance after a cut is from the address before it, or from `text` when
/// that is not past it.
pub(super) fn function_starts(text: u64, addrs: &[u64], out: &mut Vec<u8>) -> usize {
    let addrs = sorted(addrs);
    let start = out.len();
    let cuts = cuts(addrs.len(), |_| true);
    if cuts.len() == 1 {
        // Most functions are less than 16 KiB apart: two bytes each.
        out.reserve(addrs.len() * 2 + 8);
        starts_from(text, text, &addrs, out);
    } else {
        let pieces = par::map(0..cuts.len(), |nth| {
            let (from, to) = (cuts[nth], cuts.get(nth + 1).copied().unwrap_or(addrs.len()));
            let last = from.checked_sub(1).map(|before| addrs[before]).filter(|it| *it > text).unwrap_or(text);
            // alloc: once per piece of CHUNK functions
            let mut piece = Vec::with_capacity((to - from) * 2 + 8);
            starts_from(text, last, &addrs[from..to], &mut piece);
            Ok(piece)
        }); // alloc: once per table, one result per piece
        join(out, pieces).expect("function starts have no error");
    }
    if out.len() == start {
        return 0;
    }
    out.push(0);
    finish(out, start)
}

/// The distances of `addrs` (sorted) past `text`, the first from `last`, the one before.
fn starts_from(text: u64, mut last: u64, addrs: &[u64], out: &mut Vec<u8>) {
    // A distance of 0 ends the list: the headers are at `text`, never a function.
    for &addr in addrs.iter().filter(|it| **it > text) {
        if addr == last {
            continue;
        }
        uleb(out, addr - last);
        last = addr;
    }
}

#[cfg(test)]
mod tests {
    use blaze_ids::rng::SplitMix64;

    use super::*;

    /// The rebase opcodes as one stream, as they were written before the pieces.
    fn rebases_serial(addrs: &[u64], spans: &[Span], out: &mut Vec<u8>) -> Result<usize, String> {
        let addrs = sorted(addrs);
        if let Some(twice) = addrs.windows(2).find(|it| it[0] == it[1]) {
            return Err(format!("the address {:#x} is to be slid twice", twice[0]));
        }
        if addrs.is_empty() {
            return Ok(0);
        }
        let start = out.len();
        out.push(macho::REBASE_OPCODE_SET_TYPE_IMM.0 | macho::REBASE_TYPE_POINTER.0);
        let mut cursor: Option<(usize, u64)> = None;
        let mut rest = &addrs[..];
        while let Some(&first) = rest.first() {
            let segment = segment_of(spans, first, "a pointer to slide")?;
            let end = spans[segment].end;
            let run = (rest.iter().zip(0u64..))
                .take_while(|(it, nth)| **it == first + nth * 8 && **it + 8 <= end)
                .count()
                .max(1);
            match cursor {
                Some((at, address)) if at == segment && first >= address => {
                    if first > address {
                        out.push(macho::REBASE_OPCODE_ADD_ADDR_ULEB.0);
                        uleb(out, first - address);
                    }
                }
                _ => {
                    out.push(macho::REBASE_OPCODE_SET_SEGMENT_AND_OFFSET_ULEB.0 | segment as u8);
                    uleb(out, first - spans[segment].start);
                }
            }
            if run < 16 {
                out.push(macho::REBASE_OPCODE_DO_REBASE_IMM_TIMES.0 | run as u8);
            } else {
                out.push(macho::REBASE_OPCODE_DO_REBASE_ULEB_TIMES.0);
                uleb(out, run as u64);
            }
            cursor = Some((segment, first + run as u64 * 8));
            rest = &rest[run..];
        }
        out.push(macho::REBASE_OPCODE_DONE.0);
        Ok(finish(out, start))
    }

    /// The function starts as one stream, as they were written before the pieces.
    fn starts_serial(text: u64, addrs: &[u64], out: &mut Vec<u8>) -> usize {
        let addrs = sorted(addrs);
        let start = out.len();
        let mut last = text;
        for &addr in addrs.iter().filter(|it| **it > text) {
            if addr == last {
                continue;
            }
            uleb(out, addr - last);
            last = addr;
        }
        if out.len() == start {
            return 0;
        }
        out.push(0);
        finish(out, start)
    }

    const TEXT: u64 = 0x1_0000_0000;
    const CONST: u64 = 0x1_0100_0000;
    const DATA: u64 = 0x1_0200_0000;
    const END: u64 = 0x1_0400_0000;

    fn spans() -> [Span; 4] {
        [
            Span { name: "__PAGEZERO", start: 0, end: TEXT, writable: false },
            Span { name: "__TEXT", start: TEXT, end: CONST, writable: false },
            Span { name: "__DATA_CONST", start: CONST, end: DATA, writable: true },
            Span { name: "__DATA", start: DATA, end: END, writable: true },
        ]
    }

    /// About `n` sorted, distinct pointers from `from`: runs of 1 to 40, gaps of 4 (a pointer
    /// off 8-byte alignment) to 4 KiB, and with `jump` a run up to the end of `__DATA_CONST`.
    fn pointers(rng: &mut SplitMix64, n: usize, from: u64, jump: bool) -> Vec<u64> {
        let mut addrs = Vec::with_capacity(n + 64);
        let mut at = from;
        while addrs.len() < n {
            for _ in 0..rng.range(1..41) {
                addrs.push(at);
                at += 8;
            }
            at += if rng.one_in(16) { 4 } else { 8 * rng.range(1..512) };
            if jump && at < DATA && rng.one_in(2000) {
                at = at.max(DATA - 8 * rng.range(1..20));
            }
        }
        addrs
    }

    fn both(addrs: &[u64], spans: &[Span]) -> (Result<Vec<u8>, String>, Result<Vec<u8>, String>) {
        let (mut pieces, mut one) = (vec![0xaa; 5], vec![0xaa; 5]);
        let pieces = rebases(addrs, spans, &mut pieces).map(|len| {
            assert_eq!(pieces.len(), 5 + len);
            pieces.split_off(5)
        });
        let one = rebases_serial(addrs, spans, &mut one).map(|len| {
            assert_eq!(one.len(), 5 + len);
            one.split_off(5)
        });
        (pieces, one)
    }

    fn sixteen() -> blaze_graph::pool::Pool {
        blaze_graph::pool::Pool::new(&blaze_graph::topology::Topology::flat(16), false).unwrap()
    }

    #[test]
    fn rebases_in_pieces_are_the_one_streams_bytes() {
        // Sixteen threads, so the inputs are cut into pieces on any machine.
        sixteen().install(|| {
            let spans = spans();
            for seed in 0..12u64 {
                let mut rng = SplitMix64::new(0x7265_6261_7365 ^ seed);
                let n = [10, CHUNK - 1, 2 * CHUNK, 3 * CHUNK + 77, 5 * CHUNK][seed as usize % 5];
                let start = CONST + 8 * rng.range(0..64);
                let mut addrs = pointers(&mut rng, n, start, true);
                if seed % 3 == 0 {
                    rng.shuffle(&mut addrs);
                }
                let (pieces, one) = both(&addrs, &spans);
                assert_eq!(pieces.unwrap(), one.unwrap(), "seed {seed}");
            }
            // One run longer than every piece: no gap to cut at.
            let addrs: Vec<u64> = (0..3 * CHUNK as u64).map(|it| DATA + 8 * it).collect();
            let (pieces, one) = both(&addrs, &spans);
            assert_eq!(pieces.unwrap(), one.unwrap());
        });
    }

    #[test]
    fn a_rebase_error_is_the_first_in_order() {
        // Sixteen threads, so the inputs are cut into pieces on any machine.
        sixteen().install(|| {
            // A segment dyld can't write amid the pointers, and a hole in none past it: the
            // pointers reach both in later pieces.
            const LOCKED: u64 = CONST + 0x40_0000;
            const HOLE: u64 = CONST + 0x58_0000;
            let spans = [
                Span { name: "__TEXT", start: TEXT, end: CONST, writable: false },
                Span { name: "__DATA_CONST", start: CONST, end: LOCKED, writable: true },
                Span { name: "__LOCKED", start: LOCKED, end: LOCKED + 0x8_0000, writable: false },
                Span { name: "__DATA", start: LOCKED + 0x8_0000, end: HOLE, writable: true },
            ];
            let mut rng = SplitMix64::new(11);
            let mut addrs = pointers(&mut rng, 4 * CHUNK, CONST, false);
            assert!(addrs[CHUNK] < LOCKED && *addrs.last().unwrap() > HOLE, "the pointers span the bad segments");
            let (pieces, one) = both(&addrs, &spans);
            let error = one.unwrap_err();
            assert!(error.contains("__LOCKED"), "{error}");
            assert_eq!(pieces.unwrap_err(), error);
            // Without the locked segment, the first error is the hole's.
            let mut open = spans.clone();
            open[2].writable = true;
            let (pieces, one) = both(&addrs, &open);
            let error = one.unwrap_err();
            assert!(error.contains("in no segment"), "{error}");
            assert_eq!(pieces.unwrap_err(), error);
            // A pointer twice is an error before any segment's.
            addrs[3 * CHUNK] = addrs[3 * CHUNK - 1];
            let (pieces, one) = both(&addrs, &spans);
            let error = one.unwrap_err();
            assert!(error.contains("slid twice"), "{error}");
            assert_eq!(pieces.unwrap_err(), error);
        });
    }

    #[test]
    fn function_starts_in_pieces_are_the_one_streams_bytes() {
        // Sixteen threads, so the inputs are cut into pieces on any machine.
        sixteen().install(|| {
            for seed in 0..10u64 {
                let mut rng = SplitMix64::new(0x7374_6172_7473 ^ seed);
                let n = [0, 7, CHUNK, 2 * CHUNK + 1, 4 * CHUNK + 999][seed as usize % 5];
                let mut addrs: Vec<u64> = Vec::with_capacity(n);
                // Some at or below `text`, which add nothing, and repeats, which add nothing either.
                let mut at = TEXT - 64;
                for _ in 0..n {
                    addrs.push(at);
                    if !rng.one_in(50) {
                        at += 4 * rng.range(1..2048);
                    }
                }
                if seed % 2 == 1 {
                    rng.shuffle(&mut addrs);
                }
                let (mut pieces, mut one) = (vec![0xaa; 3], vec![0xaa; 3]);
                let len = function_starts(TEXT, &addrs, &mut pieces);
                assert_eq!(len, starts_serial(TEXT, &addrs, &mut one), "seed {seed}");
                assert_eq!(pieces, one, "seed {seed}");
            }
        });
    }
}
