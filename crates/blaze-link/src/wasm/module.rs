//! Writes the module of a placed image, as wasm-ld lays one out: types, imports, functions,
//! the table, memory, globals, exports, the element segment, code, data, the custom sections,
//! and the `name` section.
//!
//! Written as mold and wild write an output: every size is known before a byte is written, so
//! the module is laid out as runs ([`Run`]) in one zeroed buffer of its exact size, and the
//! function, element, code, data, custom and name sections' runs are written there in
//! parallel. A code piece's header is read in place ([`Header`], 48 bytes, nothing copied). The
//! functions are handled in pieces of 4096 on every thread: each piece interns its types by
//! their encoded bytes (one hash of a few bytes each) and sizes its entries, and the pieces'
//! types are then interned in order, a few per piece, so the indices are those of one pass.
//! Per function the writer keeps only `u32` columns (its type, the CSR range of its fixups'
//! types); the checks of the functions' placing are parallel too, the first error in order the
//! one given. Nothing is allocated per function but the output itself; a set of columns per
//! piece.

use std::collections::hash_map::Entry;

use blaze_graph::par;
use blaze_ids::radix;
use blaze_hash::HashMap;

use super::code::{Header, NOTHING, Sig};
use super::leb::{self, PADDED, padded_uleb};
use super::reader::{MAGIC, Reader, Wasm as _};
use super::{FUNCTIONS, HEAP, PAGE, STACK_POINTER, Wasm, heap_base, in_memory};
use crate::model::{Class, Placed, Region, SymbolKind, View};
use crate::output::OutputKind;

mod id {
    pub(super) const TYPE: u8 = 1;
    pub(super) const IMPORT: u8 = 2;
    pub(super) const FUNCTION: u8 = 3;
    pub(super) const TABLE: u8 = 4;
    pub(super) const MEMORY: u8 = 5;
    pub(super) const GLOBAL: u8 = 6;
    pub(super) const EXPORT: u8 = 7;
    pub(super) const ELEMENT: u8 = 9;
    pub(super) const CODE: u8 = 10;
    pub(super) const DATA: u8 = 11;
}

const FUNCREF: u8 = 0x70;
const I32: u8 = 0x7f;
const EXPORT_FUNCTION: u8 = 0;
const EXPORT_MEMORY: u8 = 2;
const EXPORT_GLOBAL: u8 = 3;
/// The bytes one parallel copy takes at most: a large data segment is copied by many threads.
const CHUNK: usize = 1 << 18;

/// A function the module defines, in the code space: 72 bytes on a 64-bit host. `header` is
/// `None` for `__wasm_call_ctors`.
#[derive(Clone, Copy)]
struct Unit<'a> {
    addr: u64,
    name: Option<&'a str>,
    header: Option<Header<'a>>,
}

const _: () = assert!(size_of::<Unit<'static>>() <= 72);

/// The functions `__wasm_call_ctors` calls, in order: the slots in the init array, each a
/// function's index + 1.
pub(super) fn init_functions(placed: &Placed<'_>) -> Result<Vec<u32>, String> {
    let arrays = || {
        let regions = placed.regions.iter().enumerate();
        regions.filter(|(_, it)| it.class == Class::InitArray).map(|(index, _)| placed.view(index))
    };
    let mut functions = Vec::with_capacity(arrays().map(|it| it.len() / 4).sum());
    for bytes in arrays() {
        if bytes.len() % 4 != 0 {
            // alloc: error path
            return Err(format!("an init array of {} bytes, not of 4-byte slots", bytes.len()));
        }
        for slot in bytes.arrays::<4>(0, bytes.len()) {
            let slot = u32::from_le_bytes(slot);
            let function = slot
                .checked_sub(1)
                .ok_or("an init function that is an undefined weak function")?;
            functions.push(function);
        }
    }
    Ok(functions)
}

fn ctors_body(functions: &[u32]) -> Vec<u8> {
    let mut body = Vec::with_capacity(2 + 6 * functions.len());
    body.push(0);
    for &function in functions {
        body.push(0x10);
        leb::uleb(&mut body, function.into());
    }
    body.push(0x0b);
    body
}

/// That `bytes[from..to]`, at `addr`, holds only zeros: read in place, or a few hundred bytes
/// at a time where it crosses a page.
fn stray(bytes: View<'_>, from: usize, to: usize, addr: u64, class: &Class) -> Result<(), String> {
    let mut buffer = [0u8; 256];
    let mut at = from;
    while at < to {
        let held = match bytes.slice(at, to - at) {
            Some(held) => held,
            None => {
                let len = (to - at).min(buffer.len());
                bytes.copy_to(at, &mut buffer[..len]);
                &buffer[..len]
            }
        };
        if let Some(found) = held.iter().position(|&it| it != 0) {
            // alloc: error path
            return Err(format!(
                "code at {} of the {class:?} region that is no function of the image's",
                addr + (at - from + found) as u64
            ));
        }
        at += held.len();
    }
    Ok(())
}

/// `bytes` onto `out`, run by run, the bytes in no run zeros.
fn extend_with(out: &mut Vec<u8>, bytes: View<'_>) {
    let start = out.len();
    out.resize(start + bytes.len(), 0);
    bytes.runs(|at, run| out[start + at..start + at + run.len()].copy_from_slice(run));
}

/// The functions the module defines, by address in the code space, which is their order:
/// the core numbers them so, after the imports. Headers are read in parallel, in place; a
/// function across two pages of a region written since the full link is copied into `across`
/// first (one per page boundary at most), and its header read there.
fn functions<'a>(placed: &'a Placed<'a>, across: &'a mut Vec<u8>) -> Result<Vec<Unit<'a>>, String> {
    let is_code = |it: &&Region| matches!(it.class, Class::Text | Class::Patch);
    let mut code: Vec<(usize, &Region)> =
        Vec::with_capacity(placed.regions.iter().filter(is_code).count());
    code.extend(placed.regions.iter().enumerate().filter(|(_, it)| is_code(it)));
    let is_ctors = |it: &&Region| it.class == Class::Synthetic(FUNCTIONS);
    let ctors = placed.regions.iter().filter(is_ctors).count();
    // The code space's order without a sort when the core's functions are in it, strictly,
    // and every `__wasm_call_ctors` comes before them (its region is placed first): those
    // first, then the functions. Else sorted below, which makes the same order.
    let ordered = {
        let mut addrs = placed.regions.iter().filter(is_ctors).map(|it| it.addr);
        let mut last = None;
        let ctors_first = addrs.all(|addr| {
            let before = last.is_none_or(|it| it < addr);
            last = Some(addr);
            before
        });
        let functions = placed.functions;
        ctors_first
            && last.zip(functions.first()).is_none_or(|(ctors, first)| ctors < first.addr)
            && par::all(par::windows(functions, 2), |it| it[0].addr < it[1].addr)
    };
    let lead = if ordered { ctors } else { 0 };
    // Each function's slot made first, filled in parallel: one exact allocation.
    let mut units: Vec<Unit<'a>> = Vec::with_capacity(placed.functions.len() + ctors);
    units.resize(lead, Unit { addr: 0, name: None, header: None });
    par::extend(&mut units, placed.functions, |_| Unit { addr: 0, name: None, header: None });
    // `Placed` is not `Sync` (its `symbol` lookup isn't): the closure takes its contents.
    let (contents, paged) = (placed.contents, placed.paged);
    // Where a function's bytes are: its region's view and its offset and length there.
    let piece = |function: &crate::model::Function| -> Result<(View<'a>, usize, usize), String> {
        let name = function.name.as_deref();
        let &(index, region) = code
            .iter()
            .find(|(_, it)| (it.addr..it.addr + it.size).contains(&function.addr))
            .ok_or_else(|| {
                // alloc: error path
                format!(
                    "the function {} at {} is in no region of code",
                    name.unwrap_or("?"),
                    function.addr
                )
            })?;
        let bytes = View::of(contents, paged, index);
        let at = (function.addr - region.addr) as usize;
        let size = usize::try_from(function.size)
            .ok()
            .filter(|size| at.checked_add(*size).is_some_and(|end| end <= bytes.len()))
            .ok_or_else(|| {
                // alloc: error path
                format!("the function at {} reaches past its region's bytes", function.addr)
            })?;
        Ok((bytes, at, size))
    };
    let parse = |piece: &'a [u8], name: Option<&str>| {
        Header::parse(piece)
            // alloc: error path
            .map_err(|e| format!("the function {}: {e}", name.unwrap_or("?")))
    };
    let failure = par::find_map_first(par::zip(&mut units[lead..], placed.functions), |(unit, function)| {
        let result = (|| {
            let name = function.name.as_deref();
            let (bytes, at, size) = piece(function)?;
            *unit = Unit { addr: function.addr, name, header: None };
            if let Some(held) = bytes.slice(at, size) {
                unit.header = Some(parse(held, name)?);
            }
            Ok::<(), String>(())
        })();
        result.err()
    });
    if let Some(error) = failure {
        return Err(error);
    }
    // The functions across pages: their bytes one after another in `across`, then their headers.
    if par::any(&units[lead..], |it| it.header.is_none()) {
        across.clear();
        for (_, function) in units[lead..].iter().zip(placed.functions).filter(|it| it.0.header.is_none()) {
            let (bytes, at, size) = piece(function)?;
            let start = across.len();
            // alloc: once per link with code in pages, the functions across their page boundaries
            across.resize(start + size, 0);
            bytes.copy_to(at, &mut across[start..]);
        }
        let across: &'a [u8] = across;
        let mut start = 0;
        for (unit, function) in units[lead..].iter_mut().zip(placed.functions).filter(|it| it.0.header.is_none()) {
            let size = function.size as usize;
            unit.header = Some(parse(&across[start..start + size], unit.name)?);
            start += size;
        }
    }
    let ctors_units = placed.regions.iter().filter(is_ctors);
    let ctors_units = ctors_units.map(|region| Unit { addr: region.addr, name: Some("__wasm_call_ctors"), header: None });
    if ordered {
        for (slot, unit) in units.iter_mut().zip(ctors_units) {
            *slot = unit;
        }
    } else {
        units.extend(ctors_units);
        radix::sort_copy_by_key(&mut units, |it| it.addr);
        if let Some(pair) = par::find_map_first(par::windows(&units, 2), |it| (it[0].addr == it[1].addr).then_some(it)) {
            // alloc: error path
            return Err(format!("two functions at {} in the code space", pair[0].addr));
        }
    }
    // Every byte of code is some function's: a piece the core did not list would shift every
    // index after it. Checked in parallel; the first error in address order is the one given.
    for &(index, region) in &code {
        let bytes = placed.view(index);
        let first = units.partition_point(|it| it.addr < region.addr);
        let len = units[first..].partition_point(|it| it.addr < region.addr + region.size);
        let units = &units[first..first + len];
        // Where the function before `at` (the last with a header) ends in the region.
        let end_before = |at: usize| {
            let mut before = units[..at].iter().rev().filter_map(|it| Some((it.addr, it.header?)));
            before.next().map_or(0, |(addr, header)| (addr - region.addr) as usize + header.len())
        };
        let failed = par::find_map_first(0..units.len(), |at| {
            let (unit, end) = (&units[at], end_before(at));
            unit.header?;
            let start = (unit.addr - region.addr) as usize;
            if start < end {
                // alloc: error path
                return Some(format!("the function at {} overlaps the one before it", unit.addr));
            }
            stray(bytes, end, start, region.addr + end as u64, &region.class).err()
        });
        if let Some(failed) = failed {
            return Err(failed);
        }
        let end = end_before(units.len());
        stray(bytes, end.min(bytes.len()), bytes.len(), region.addr + end as u64, &region.class)?;
    }
    Ok(units)
}

/// What a placed image can't hold and still be a wasm module.
fn check(placed: &Placed<'_>) -> Result<(), String> {
    if let Some((name, _)) = placed.debug.first() {
        // alloc: error path
        return Err(format!(
            "the debug section {name}: a wasm module's debug info is its name section, and \
             DWARF of function indices would be wrong"
        ));
    }
    if let Some(cell) = placed.got.first() {
        // alloc: error path
        return Err(format!("a GOT cell at {:#x}: a wasm32 module has no GOT", cell.addr));
    }
    if let Some(bind) = placed.binds.first() {
        // alloc: error path
        return Err(format!("a bind at {:#x}: a wasm module is never bound", bind.addr));
    }
    if let Some(addr) = placed.rebase.first() {
        // alloc: error path
        return Err(format!("a rebase at {addr:#x}: a wasm32 module is never slid"));
    }
    if let Some(tls) = &placed.tls {
        // alloc: error path
        return Err(format!(
            "thread-local storage at {:#x}: wasm32 without shared memory has no TLS",
            tls.addr
        ));
    }
    for region in placed.regions {
        let unplaceable = match &region.class {
            Class::Plt
            | Class::Stubs
            | Class::Veneers
            | Class::EhFrame
            | Class::Table
            | Class::Got
            | Class::FiniArray
            | Class::ThreadVars
            | Class::Tdata
            | Class::Tbss => true,
            Class::Named { exec, .. } => *exec,
            Class::Synthetic(name) => *name != FUNCTIONS && *name != HEAP,
            _ => false,
        };
        if unplaceable && region.size > 0 {
            // alloc: error path
            return Err(format!(
                "a {:?} region of {} at {:#x}, which a wasm module has no place for",
                region.class, region.size, region.addr
            ));
        }
    }
    Ok(())
}

/// The module's types, each interned once by its encoding: a hash of a few bytes per
/// lookup, through the entry API.
struct Types<'a> {
    list: Vec<Sig<'a>>,
    index: HashMap<Sig<'a>, u32>,
}

impl<'a> Types<'a> {
    fn with_capacity(capacity: usize) -> Types<'a> {
        Types {
            list: Vec::with_capacity(capacity),
            index: HashMap::with_capacity_and_hasher(capacity, Default::default()),
        }
    }

    fn of(&mut self, signature: Sig<'a>) -> u32 {
        match self.index.entry(signature) {
            Entry::Occupied(it) => *it.get(),
            Entry::Vacant(it) => {
                let index = self.list.len() as u32;
                self.list.push(signature);
                *it.insert(index)
            }
        }
    }
}

/// What an import is imported as, borrowed.
#[derive(Clone, Copy)]
struct Imported<'a> {
    module: &'a str,
    field: &'a str,
    ty: Sig<'a>,
}

/// What each import is imported as, as the core says (its type encoded into `arena`).
fn imports<'a>(placed: &'a Placed<'a>, arena: &'a [u8], spans: &[(usize, usize)]) -> Vec<Imported<'a>> {
    let mut imports = Vec::with_capacity(spans.len());
    imports.extend(placed.imports.iter().zip(spans).map(|(import, &(start, end))| {
        let wasm = import.wasm.as_ref().expect("checked when the spans were made");
        Imported { module: &wasm.module, field: &wasm.field, ty: &arena[start..end] }
    }));
    imports
}

/// Checks that a 5-byte place at `at` is inside a body of `len` bytes.
fn fits(at: u32, len: usize, name: Option<&str>) -> Result<(), String> {
    match (at as usize).checked_add(PADDED) {
        Some(end) if end <= len => Ok(()),
        // alloc: error path
        _ => Err(format!(
            "the function {}: a fixup at {at} of a body of {len} bytes",
            name.unwrap_or("?")
        )),
    }
}

/// The producers sections of the objects, one after the other, as one: each field's values
/// by the first version of each name, the fields in the tool conventions' order.
fn producers(payloads: &[u8]) -> Result<Vec<u8>, String> {
    // Two flat columns, each sized by what the payloads can hold (a field or a value is at
    // least 2 bytes): the fields as first met, and each value with its field's place.
    let mut fields: Vec<&str> = Vec::with_capacity(payloads.len() / 2);
    let mut values: Vec<(u32, &str, &str)> = Vec::with_capacity(payloads.len() / 2);
    let mut r = Reader::new(payloads);
    while !r.is_done() {
        for _ in 0..r.count()? {
            let field = r.name()?;
            let at = match fields.iter().position(|&it| it == field) {
                Some(at) => at as u32,
                None => {
                    fields.push(field);
                    fields.len() as u32 - 1
                }
            };
            for _ in 0..r.count()? {
                let name = r.name()?;
                let version = r.name()?;
                if !values.iter().any(|&(it, known, _)| it == at && known == name) {
                    values.push((at, name, version));
                }
            }
        }
    }
    const ORDER: [&str; 3] = ["language", "processed-by", "sdk"];
    let rank = |field: &str| ORDER.iter().position(|&it| it == field).unwrap_or(ORDER.len());
    let mut out = Vec::with_capacity(payloads.len());
    leb::uleb(&mut out, fields.len() as u64);
    // A stable order by rank, then as first met: a few fields, each walked once per rank.
    for want in 0..=ORDER.len() {
        for (at, &field) in fields.iter().enumerate().filter(|&(_, &it)| rank(it) == want) {
            let of_field = || values.iter().filter(move |it| it.0 == at as u32);
            leb::name(&mut out, field);
            leb::uleb(&mut out, of_field().count() as u64);
            for &(_, name, version) in of_field() {
                leb::name(&mut out, name);
                leb::name(&mut out, version);
            }
        }
    }
    Ok(out)
}

/// The target_features sections of the objects as one: every feature one uses, an error when
/// another disallows it.
fn target_features(payloads: &[u8]) -> Result<Vec<u8>, String> {
    // Every object repeats the same few features: collected, then one sort and dedup each. A
    // feature is at least 2 bytes of the payloads.
    let mut used: Vec<&str> = Vec::with_capacity(payloads.len() / 2);
    let mut disallowed: Vec<&str> = Vec::with_capacity(payloads.len() / 2);
    let mut r = Reader::new(payloads);
    while !r.is_done() {
        for _ in 0..r.count()? {
            let prefix = r.u8()?;
            let feature = r.name()?;
            match prefix {
                b'+' | b'=' => used.push(feature),
                b'-' => disallowed.push(feature),
                // alloc: error path
                other => return Err(format!("the target feature prefix {other:#04x}")),
            };
        }
    }
    for list in [&mut used, &mut disallowed] {
        list.sort_unstable();
        list.dedup();
    }
    if let Some(feature) = used.iter().find(|it| disallowed.binary_search(it).is_ok()) {
        // alloc: error path
        return Err(format!("the target feature `{feature}` is used by one object and disallowed by another"));
    }
    let mut out = Vec::with_capacity(payloads.len());
    leb::uleb(&mut out, used.len() as u64);
    for feature in used {
        out.push(b'+');
        leb::name(&mut out, feature);
    }
    Ok(out)
}

/// A name map of the `name` section, from names in index order: its id, size, count, then an
/// index and a name each. `scratch` is reused between maps.
fn name_map<'n>(
    out: &mut Vec<u8>,
    scratch: &mut Vec<u8>,
    id: u8,
    names: impl Iterator<Item = (u32, &'n str)>,
) {
    scratch.clear();
    let mut count = 0u64;
    for (index, name) in names {
        leb::uleb(scratch, index.into());
        leb::name(scratch, name);
        count += 1;
    }
    if count == 0 {
        return;
    }
    out.push(id);
    leb::uleb(out, (leb::uleb_len(count) + scratch.len()) as u64);
    leb::uleb(out, count);
    out.extend_from_slice(scratch);
}

fn segment_name(class: &Class) -> &str {
    match class {
        Class::Rodata => ".rodata",
        Class::RelRo => ".data.rel.ro",
        Class::Tdata => ".tdata",
        Class::PatchData => ".data.patch",
        Class::Named { name, .. } => name.as_str(),
        _ => ".data",
    }
}

/// Adds an export, or checks that one of that name exports the same thing when `again` may.
fn export<'n>(
    named: &mut HashMap<&'n str, (u8, u32)>,
    list: &mut Vec<(&'n str, u8, u32)>,
    name: &'n str,
    kind: u8,
    index: u32,
    again: bool,
) -> Result<(), String> {
    match named.entry(name) {
        Entry::Occupied(it) if again && *it.get() == (kind, index) => Ok(()),
        // alloc: error path
        Entry::Occupied(_) if again => Err(format!("two different things exported as `{name}`")),
        // alloc: error path
        Entry::Occupied(_) => Err(format!("`{name}` exported twice")),
        Entry::Vacant(it) => {
            it.insert((kind, index));
            list.push((name, kind, index));
            Ok(())
        }
    }
}

/// Defined functions per piece the writer handles on one thread: their types, their entries
/// of the function, code and name sections.
const PIECE: usize = 1 << 12;
/// Table slots per run of the element segment written on one thread.
const ELEMENTS: u64 = 1 << 16;

/// One piece of the defined functions as the module holds them: the types it meets, in the
/// order it first meets them; each function's type and its fixups' types (indices into
/// `types`, then, once [`write`] merged the pieces, the module's); and the bytes its entries
/// take in the function, code and name sections.
struct Piece<'a> {
    types: Vec<Sig<'a>>,
    function_types: Vec<u32>,
    /// By function, where its run of `fixup_types` starts; one more at the end.
    fixup_starts: Vec<u32>,
    fixup_types: Vec<u32>,
    function_bytes: usize,
    code_bytes: usize,
    name_bytes: usize,
    names: u64,
}

/// A piece of `units` whose first is function `first` of the module, `ctors` the length of
/// `__wasm_call_ctors`' body: what [`Piece`] holds, its types interned on its own. The checks
/// and their order are those of one pass over the functions in order, so the first error of
/// the first piece that has one is that pass's.
fn piece<'a>(units: &[Unit<'a>], first: u32, ctors: usize) -> Result<Piece<'a>, String> {
    let (mut most, mut fixups) = (0, 0);
    for header in units.iter().filter_map(|it| it.header) {
        let (types, count) = header.counts();
        most = most.max(types);
        fixups += count;
    }
    // alloc: one set of columns per piece of 4096 functions
    let mut types = Types::with_capacity(64);
    let mut scratch: Vec<Sig<'a>> = Vec::with_capacity(most);
    let mut piece = Piece {
        // alloc: none: an empty `Vec`, the piece's list of types replacing it at the end
        types: Vec::new(),
        function_types: Vec::with_capacity(units.len()),
        fixup_starts: Vec::with_capacity(units.len() + 1),
        fixup_types: Vec::with_capacity(fixups),
        function_bytes: 0,
        code_bytes: 0,
        name_bytes: 0,
        names: 0,
    };
    for (at, unit) in units.iter().enumerate() {
        piece.fixup_starts.push(piece.fixup_types.len() as u32);
        if let Some(name) = unit.name {
            let index = u64::from(first + at as u32);
            piece.name_bytes += leb::uleb_len(index) + leb::uleb_len(name.len() as u64) + name.len();
            piece.names += 1;
        }
        let Some(header) = unit.header else {
            piece.function_types.push(types.of(NOTHING));
            piece.code_bytes += leb::uleb_len(ctors as u64) + ctors;
            continue;
        };
        piece.function_types.push(types.of(header.ty()));
        header.types(&mut scratch);
        let len = header.body().len();
        piece.code_bytes += leb::uleb_len(len as u64) + len;
        for (at, which) in header.type_fixups() {
            let ty = *scratch.get(which as usize).ok_or_else(|| {
                // alloc: error path
                format!("the function {}: no type {which}", unit.name.unwrap_or("?"))
            })?;
            fits(at, len, unit.name)?;
            piece.fixup_types.push(types.of(ty));
        }
        for at in header.stack_pointer() {
            fits(at, len, unit.name)?;
        }
    }
    piece.fixup_starts.push(piece.fixup_types.len() as u32);
    piece.types = types.list;
    Ok(piece)
}

/// The bytes of the ULEBs of `0..n`: `k` bytes each from `2^(7(k-1))` to `2^(7k)`.
fn ulebs_below(n: u64) -> usize {
    let (mut total, mut from, mut bytes) = (0usize, 0u64, 1u32);
    while from < n {
        let to = 1u64.checked_shl(7 * bytes).unwrap_or(u64::MAX).min(n);
        total += (to - from) as usize * bytes as usize;
        (from, bytes) = (to, bytes + 1);
    }
    total
}

/// A run of the module's bytes, in order. Every run's size is known before the module is
/// allocated; then each is written on its own thread.
enum Run<'a> {
    /// `head[range]`: the small sections and the big ones' headers, written in order.
    Head(std::ops::Range<usize>),
    /// At most [`CHUNK`] bytes of a data segment or a custom section.
    Copy(&'a [u8]),
    /// Bytes of a data segment or a custom section in no run of its pages, which are zeros.
    Zeros(usize),
    /// A piece's entries in the function section: its functions' type indices.
    Functions(usize),
    /// The element segment's table slots of these functions: each function's index.
    Elements(std::ops::Range<u64>),
    /// A piece's bodies, each patched.
    Code(usize),
    /// A piece's entries in the `name` section's function names.
    Names(usize),
}

impl Run<'_> {
    fn len(&self, pieces: &[Piece<'_>]) -> usize {
        match self {
            Run::Head(range) => range.len(),
            Run::Copy(bytes) => bytes.len(),
            Run::Zeros(len) => *len,
            Run::Functions(at) => pieces[*at].function_bytes,
            Run::Elements(range) => ulebs_below(range.end) - ulebs_below(range.start),
            Run::Code(at) => pieces[*at].code_bytes,
            Run::Names(at) => pieces[*at].name_bytes,
        }
    }
}

/// The runs of the module, made in order: the bytes written serially go to `head`, each big
/// run between them is a [`Run`] of its own.
struct Layout<'a> {
    head: Vec<u8>,
    runs: Vec<Run<'a>>,
    /// Where the bytes of `head` not yet in a run start.
    mark: usize,
}

impl<'a> Layout<'a> {
    /// The bytes of `head` not yet in a run, as one.
    fn flush(&mut self) {
        if self.mark < self.head.len() {
            self.runs.push(Run::Head(self.mark..self.head.len()));
            self.mark = self.head.len();
        }
    }

    fn run(&mut self, run: Run<'a>) {
        self.flush();
        self.runs.push(run);
    }

    /// `bytes` run by run, in chunks of at most [`CHUNK`], the bytes in no run zeros.
    fn view(&mut self, bytes: View<'a>) {
        let mut done = 0;
        bytes.runs(|at, run| {
            if at > done {
                self.run(Run::Zeros(at - done));
            }
            for chunk in run.chunks(CHUNK) {
                self.run(Run::Copy(chunk));
            }
            done = at + run.len();
        });
        if bytes.len() > done {
            self.run(Run::Zeros(bytes.len() - done));
        }
    }

    fn finish(mut self) -> (Vec<u8>, Vec<Run<'a>>) {
        self.flush();
        (self.head, self.runs)
    }
}

/// A piece's bodies into `place`, its exact size: each one's size, then its bytes with every
/// type fixup and stack pointer fixup written.
fn code_piece(place: &mut [u8], units: &[Unit<'_>], piece: &Piece<'_>, ctors: &[u8]) {
    let mut at = 0;
    for (index, unit) in units.iter().enumerate() {
        let len = unit.header.map_or(ctors.len(), |it| it.body().len());
        at += leb::uleb_into(&mut place[at..], len as u64);
        let body = &mut place[at..at + len];
        at += len;
        let Some(header) = unit.header else {
            body.copy_from_slice(ctors);
            continue;
        };
        body.copy_from_slice(header.body());
        let types = &piece.fixup_types[piece.fixup_starts[index] as usize..piece.fixup_starts[index + 1] as usize];
        for ((fixup, _), &ty) in header.type_fixups().zip(types) {
            body[fixup as usize..][..PADDED].copy_from_slice(&padded_uleb(ty));
        }
        for fixup in header.stack_pointer() {
            body[fixup as usize..][..PADDED].copy_from_slice(&padded_uleb(STACK_POINTER));
        }
    }
}

/// A piece's function names into `place`, its exact size: index and name of each named one,
/// the first function `first` of the module.
fn names_piece(place: &mut [u8], units: &[Unit<'_>], first: u32) {
    let mut at = 0;
    for (index, unit) in units.iter().enumerate() {
        let Some(name) = unit.name else { continue };
        at += leb::uleb_into(&mut place[at..], u64::from(first + index as u32));
        at += leb::uleb_into(&mut place[at..], name.len() as u64);
        place[at..at + name.len()].copy_from_slice(name.as_bytes());
        at += name.len();
    }
}

/// ULEBs of `values` one after another into `place`, its exact size.
fn ulebs_into(place: &mut [u8], values: impl Iterator<Item = u64>) {
    let mut at = 0;
    for value in values {
        at += leb::uleb_into(&mut place[at..], value);
    }
}

/// `__wasm_call_ctors`' body, when the image has one: a call of each init function, each of
/// which must be `fn()`.
fn ctors(placed: &Placed<'_>, units: &[Unit<'_>], imports: &[Imported<'_>]) -> Result<Vec<u8>, String> {
    if !placed.regions.iter().any(|it| it.class == Class::Synthetic(FUNCTIONS)) {
        return Ok(ctors_body(&[]));
    }
    let ctors = init_functions(placed)?;
    for &function in &ctors {
        let ty = match (function as usize).checked_sub(imports.len()) {
            None => imports.get(function as usize).map(|it| it.ty),
            Some(at) => units.get(at).and_then(|it| it.header).map(|it| it.ty()),
        };
        if ty != Some(NOTHING) {
            // alloc: error path
            return Err(format!(
                "the init function {function} takes or returns values (it must be `fn()`), or \
                 is no function of the program's"
            ));
        }
    }
    Ok(ctors_body(&ctors))
}

/// The module of a placed image.
pub(super) fn write(wasm: &Wasm, placed: &Placed<'_>) -> Result<Vec<u8>, String> {
    check(placed)?;
    let imported = u32::try_from(placed.imports.len()).map_err(|_| "more than 2^32 imports")?;
    // alloc: none unless code is in pages, then the functions across page boundaries, once per link
    let mut across = Vec::new();
    let units = functions(placed, &mut across)?;
    let defined = u32::try_from(units.len()).map_err(|_| "more than 2^32 functions")?;
    let count = u64::from(imported) + u64::from(defined);
    if count >= u64::from(u32::MAX) {
        // alloc: error path
        return Err(format!("{count} functions, more than a table of 2^32 slots holds"));
    }
    let index_at = |addr: u64| match units.binary_search_by_key(&addr, |it| it.addr) {
        Ok(at) => Some(imported + at as u32),
        Err(_) => None,
    };
    fn nowhere(what: std::fmt::Arguments<'_>, addr: u64) -> String {
        // alloc: error path
        format!("{what} at {addr} of the code space, where no function of the module starts")
    }

    // Imports: the core's types encoded once into one arena, the rest borrowed. A type is
    // 0x60, then two counts of at most 5 bytes and their value types.
    let encoded = |it: &crate::model::Import| {
        it.wasm.as_ref().map_or(0, |it| 11 + it.ty.params.len() + it.ty.results.len())
    };
    let mut arena = Vec::with_capacity(placed.imports.iter().map(encoded).sum());
    let mut spans = Vec::with_capacity(placed.imports.len());
    for import in placed.imports {
        if import.kind != SymbolKind::Function {
            // alloc: error path
            return Err(format!(
                "the import `{}` is {:?}: a wasm32 module imports only functions",
                import.name, import.kind
            ));
        }
        // The core makes every wasm import from what an input says it is imported as.
        let wasm = import.wasm.as_ref().ok_or_else(|| {
            // alloc: error path
            format!("the import `{}`: no module, field and type for it", import.name)
        })?;
        let start = arena.len();
        wasm.ty.encode(&mut arena);
        spans.push((start, arena.len()));
    }
    let imports = imports(placed, &arena, &spans);
    // The core keeps one import per name: a function that says another must not call it.
    if par::any(&units, |it| it.header.is_some_and(|it| it.imports().next().is_some())) {
        let names = placed.imports.iter().map(|it| it.name.as_str());
        let mut by_name: HashMap<&str, Imported<'_>> =
            HashMap::with_capacity_and_hasher(imports.len(), Default::default());
        by_name.extend(names.zip(imports.iter().copied()));
        let conflict = par::find_map_first(&units, |unit| {
            unit.header?.imports().find_map(|(name, module, field, ty)| {
                let it = by_name.get(name)?;
                ((it.module, it.field, it.ty) != (module, field, ty)).then(|| {
                    // alloc: error path
                    format!(
                        "`{name}` is imported as `{}.{}`, and the function {} says `{module}.{field}` \
                         or another type",
                        it.module,
                        it.field,
                        unit.name.unwrap_or("?")
                    )
                })
            })
        });
        if let Some(conflict) = conflict {
            return Err(conflict);
        }
    }

    // `__wasm_call_ctors`: what it calls must be `fn()`. Its error is the bodies' after.
    let ctors = ctors(placed, &units, &imports);

    // Types and the columns of the function bodies: each piece's on its own thread, then the
    // pieces' types interned in order after the imports', so each type's index is where one
    // pass over the imports and then the functions in order first meets it.
    let ctors_len = ctors.as_ref().map_or(0, Vec::len);
    let mut pieces: Vec<Result<Piece<'_>, String>> = Vec::with_capacity(units.len().div_ceil(PIECE));
    par::extend(&mut pieces, par::enumerate(par::chunks(&units, PIECE)), |(at, units)| {
        piece(units, imported + (at * PIECE) as u32, ctors_len)
    });
    let mut pieces = pieces.into_iter().collect::<Result<Vec<Piece<'_>>, String>>()?;
    let ctors = ctors?;
    let mut types = Types::with_capacity(imports.len() + 64);
    let mut import_types: Vec<u32> = Vec::with_capacity(imports.len());
    import_types.extend(imports.iter().map(|it| types.of(it.ty)));
    let mut remaps: Vec<Vec<u32>> = Vec::with_capacity(pieces.len());
    for piece in &pieces {
        // alloc: one per piece of 4096 functions, its distinct types' indices
        remaps.push(piece.types.iter().map(|&it| types.of(it)).collect());
    }
    par::for_each(par::zip(&mut pieces, &remaps), |(piece, remap)| {
        for it in piece.function_types.iter_mut().chain(&mut piece.fixup_types) {
            *it = remap[*it as usize];
        }
        piece.function_bytes = piece.function_types.iter().map(|&it| leb::uleb_len(it.into())).sum();
    });

    // Memory: the stack, the data, then the heap from `__heap_base`.
    if placed.region(&Class::Synthetic(HEAP)).is_none() {
        // alloc: error path
        return Err("no heap region: the writer's synthetic regions were not placed".to_owned());
    }
    let heap = heap_base(placed.regions);
    if heap > 1 << 32 {
        // alloc: error path
        return Err(format!("data up to {heap:#x}, past 32-bit memory"));
    }
    let pages = heap.div_ceil(PAGE).max(1);
    let mut segments: Vec<(&str, u32, View<'_>)> = Vec::with_capacity(placed.regions.len());
    for (index, region) in placed.regions.iter().enumerate() {
        if !in_memory(&region.class) {
            continue;
        }
        if region.addr < wasm.stack {
            // alloc: error path
            return Err(format!(
                "the {:?} region at {:#x}, in the stack below {:#x}",
                region.class, region.addr, wasm.stack
            ));
        }
        if region.addr + region.size > heap {
            // alloc: error path
            return Err(format!(
                "the {:?} region at {:#x} reaches past the heap's start {heap:#x}",
                region.class, region.addr
            ));
        }
        let bytes = placed.view(index);
        if bytes.len() as u64 > region.size {
            // alloc: error path
            return Err(format!("the {:?} region has more bytes than its size", region.class));
        }
        if !bytes.is_empty() {
            segments.push((segment_name(&region.class), region.addr as u32, bytes));
        }
    }

    // Globals and exports.
    let mut globals: Vec<(&str, bool, u32)> = Vec::with_capacity(1 + placed.exports.len());
    globals.push(("__stack_pointer", true, wasm.stack as u32));
    let mut exports: Vec<(&str, u8, u32)> = Vec::with_capacity(2 + placed.exports.len());
    let mut named = HashMap::with_capacity_and_hasher(2 + placed.exports.len(), Default::default());
    export(&mut named, &mut exports, "memory", EXPORT_MEMORY, 0, false)?;
    if let OutputKind::Executable { entry } = &wasm.kind {
        // alloc: error path
        let addr = placed.entry.ok_or_else(|| format!("the entry `{entry}` is not defined"))?;
        let function =
            index_at(addr).ok_or_else(|| nowhere(format_args!("the entry `{entry}`"), addr))?;
        export(&mut named, &mut exports, entry, EXPORT_FUNCTION, function, true)?;
    }
    for it in placed.exports {
        match it.kind {
            SymbolKind::Function => {
                let function = index_at(it.addr)
                    .ok_or_else(|| nowhere(format_args!("the export `{}`", it.name), it.addr))?;
                export(&mut named, &mut exports, &it.name, EXPORT_FUNCTION, function, true)?;
            }
            SymbolKind::Data | SymbolKind::Other => {
                let value = u32::try_from(it.addr)
                    // alloc: error path
                    .map_err(|_| format!("`{}` at {:#x}, past 32-bit memory", it.name, it.addr))?;
                let global = globals.len() as u32;
                export(&mut named, &mut exports, &it.name, EXPORT_GLOBAL, global, false)?;
                globals.push((it.name.as_str(), false, value));
            }
            SymbolKind::Tls | SymbolKind::Section => {
                // alloc: error path
                return Err(format!("`{}` is exported and is {:?}", it.name, it.kind));
            }
        }
    }
    // alloc: once per link, the functions whose headers export them, found in parallel
    let exported: Vec<(usize, &str)> = par::filter_map(par::enumerate(&units), |(at, unit)| Some((at, unit.header?.export()?)));
    for (at, name) in exported {
        export(&mut named, &mut exports, name, EXPORT_FUNCTION, imported + at as u32, true)?;
    }

    // Custom sections: each name a group, numbered as first met in address order, and its
    // regions one run of `parts` (a stable sort by group keeps the address order within one).
    // `producers` and `target_features` are merged, and written last.
    let is_custom =
        |it: &&Region| matches!(it.class, Class::Named { write: false, exec: false, .. });
    let customs = placed.regions.iter().filter(is_custom).count();
    let mut by_addr: Vec<(u64, usize)> = Vec::with_capacity(customs);
    let regions = placed.regions.iter().enumerate();
    by_addr.extend(regions.filter(|(_, it)| is_custom(it)).map(|(index, it)| (it.addr, index)));
    radix::sort_copy_by_key(&mut by_addr, |&(addr, index)| radix::pack_64(addr, index as u64));
    let mut groups: Vec<(&str, bool)> = Vec::with_capacity(customs);
    let mut group_of: HashMap<&str, u32> =
        HashMap::with_capacity_and_hasher(customs, Default::default());
    let mut parts: Vec<(u32, usize)> = Vec::with_capacity(customs);
    for &(_, index) in &by_addr {
        let Class::Named { name, .. } = &placed.regions[index].class else { continue };
        let name = name.as_str();
        let group = *group_of.entry(name).or_insert_with(|| {
            groups.push((name, matches!(name, "producers" | "target_features")));
            groups.len() as u32 - 1
        });
        parts.push((group, index));
    }
    radix::sort_copy_by_key(&mut parts, |it| it.0);
    let runs = || parts.chunk_by(|a, b| a.0 == b.0).map(|run| (groups[run[0].0 as usize], run));

    // The sizes of the big sections, known before anything is written.
    let code_bytes: usize = pieces.iter().map(|it| it.code_bytes).sum();
    let function_bytes: usize = pieces.iter().map(|it| it.function_bytes).sum();
    // Each segment's header: flags 0, `i32.const addr; end`, the size.
    let mut segment_headers: Vec<([u8; 16], usize)> = Vec::with_capacity(segments.len());
    segment_headers.extend(segments.iter().map(|&(_, addr, bytes)| {
        let mut header = [0; 16];
        header[1] = 0x41;
        let mut len = 2 + leb::sleb_into(&mut header[2..], (addr as i32).into());
        header[len] = 0x0b;
        len += 1 + leb::uleb_into(&mut header[len + 1..], bytes.len() as u64);
        (header, len)
    }));
    let data_bytes: usize =
        segments.iter().zip(&segment_headers).map(|(it, header)| header.1 + it.2.len()).sum();
    let part_bytes =
        |run: &[(u32, usize)]| run.iter().map(|it| placed.len(it.1)).sum::<usize>();

    // The merged custom sections' payloads, each made from its parts one after the other in
    // one reused buffer, `producers` first.
    let merged = || runs().filter(|((_, merged), _)| *merged);
    let most = merged().map(|(_, run)| part_bytes(run)).max().unwrap_or(0);
    let mut payloads = Vec::with_capacity(most);
    let mut merged_sections: Vec<(&str, Vec<u8>)> = Vec::with_capacity(2);
    for want in ["producers", "target_features"] {
        for ((name, _), run) in merged().filter(|((name, _), _)| *name == want) {
            payloads.clear();
            for &(_, index) in run {
                extend_with(&mut payloads, placed.view(index));
            }
            let payload = match name {
                "producers" => producers(&payloads),
                _ => target_features(&payloads),
            };
            // alloc: error path
            let payload = payload.map_err(|e| format!("the {name} sections: {e}"))?;
            merged_sections.push((name, payload));
        }
    }

    // The module as runs: the small sections and every header written here, in order; the
    // function, element, code and data sections, the custom sections' parts and the function
    // names as runs written in parallel once one zeroed buffer holds them all.
    let mut layout = Layout {
        head: Vec::with_capacity(
            MAGIC.len()
                + 256
                + types.list.iter().map(|it| it.len()).sum::<usize>()
                + arena.len()
                + 64 * imports.len()
                + 64 * exports.len()
                + 16 * (globals.len() + segments.len() + groups.len())
                + 32 * pieces.len()
                + 64 * placed.imports.len()
                + merged_sections.iter().map(|it| 32 + it.1.len()).sum::<usize>(),
        ),
        runs: Vec::with_capacity(16 + 4 * pieces.len() + 4 * segments.len() + 2 * parts.len()),
        mark: 0,
    };
    let section = |out: &mut Vec<u8>, id: u8, len: usize| {
        out.push(id);
        leb::uleb(out, len as u64);
    };
    let out = &mut layout.head;
    out.extend_from_slice(MAGIC);

    let list = types.list.len() as u64;
    section(out, id::TYPE, leb::uleb_len(list) + types.list.iter().map(|it| it.len()).sum::<usize>());
    leb::uleb(out, list);
    for ty in &types.list {
        out.extend_from_slice(ty);
    }

    // One buffer for the small sections' contents, sized for the largest of them.
    let mut contents =
        Vec::with_capacity(16 + 64 * imports.len().max(exports.len()) + 8 * globals.len());
    if !imports.is_empty() {
        leb::uleb(&mut contents, imports.len() as u64);
        for (import, &ty) in imports.iter().zip(&import_types) {
            leb::name(&mut contents, import.module);
            leb::name(&mut contents, import.field);
            contents.push(0);
            leb::uleb(&mut contents, ty.into());
        }
        leb::section(out, id::IMPORT, &contents);
    }

    section(out, id::FUNCTION, leb::uleb_len(defined.into()) + function_bytes);
    leb::uleb(out, defined.into());
    for at in 0..pieces.len() {
        layout.run(Run::Functions(at));
    }
    let out = &mut layout.head;

    // One table of funcrefs, its minimum and maximum the slots; one memory of `pages`.
    section(out, id::TABLE, 3 + 2 * leb::uleb_len(count + 1));
    out.extend([1, FUNCREF, 1]);
    leb::uleb(out, count + 1);
    leb::uleb(out, count + 1);
    section(out, id::MEMORY, 2 + leb::uleb_len(pages));
    out.extend([1, 0]);
    leb::uleb(out, pages);
    contents.clear();
    leb::uleb(&mut contents, globals.len() as u64);
    for &(_, mutable, value) in &globals {
        contents.extend([I32, u8::from(mutable)]);
        leb::i32_const(&mut contents, value as i32);
    }
    leb::section(out, id::GLOBAL, &contents);
    contents.clear();
    leb::uleb(&mut contents, exports.len() as u64);
    for &(name, kind, index) in &exports {
        leb::name(&mut contents, name);
        contents.push(kind);
        leb::uleb(&mut contents, index.into());
    }
    leb::section(out, id::EXPORT, &contents);

    if count > 0 {
        // One active segment of table 0 (flags 0) putting every function at its slot, its
        // index + 1: what a function pointer is.
        section(out, id::ELEMENT, 2 + 3 + leb::uleb_len(count) + ulebs_below(count));
        out.extend([1, 0]);
        leb::i32_const(out, 1);
        leb::uleb(out, count);
        for start in (0..count).step_by(ELEMENTS as usize) {
            layout.run(Run::Elements(start..(start + ELEMENTS).min(count)));
        }
    }

    // Code: each piece's bodies copied to their place and patched there.
    let out = &mut layout.head;
    section(out, id::CODE, leb::uleb_len(defined.into()) + code_bytes);
    leb::uleb(out, defined.into());
    for at in 0..pieces.len() {
        layout.run(Run::Code(at));
    }

    // Data: each segment's header, then its bytes copied in chunks.
    if !segments.is_empty() {
        let count = segments.len() as u64;
        let out = &mut layout.head;
        section(out, id::DATA, leb::uleb_len(count) + data_bytes);
        leb::uleb(out, count);
        for (&(_, _, bytes), (header, len)) in segments.iter().zip(&segment_headers) {
            layout.head.extend_from_slice(&header[..*len]);
            layout.view(bytes);
        }
    }

    for ((name, _), run) in runs().filter(|((_, merged), _)| !merged) {
        let out = &mut layout.head;
        section(out, 0, leb::uleb_len(name.len() as u64) + name.len() + part_bytes(run));
        leb::name(out, name);
        for &(_, index) in run {
            layout.view(placed.view(index));
        }
    }

    // The name section: the functions' names (the defined ones' by piece), then the globals'
    // and the segments'.
    let mut small = Vec::with_capacity(16 * (globals.len() + segments.len()) + 64 * placed.imports.len());
    let mut scratch = Vec::with_capacity(small.capacity());
    let import_names = placed.imports.iter().enumerate().map(|(index, it)| (index as u32, it.name.as_str()));
    for (index, name) in import_names {
        leb::uleb(&mut scratch, index.into());
        leb::name(&mut scratch, name);
    }
    let names = placed.imports.len() as u64 + pieces.iter().map(|it| it.names).sum::<u64>();
    let entries = scratch.len() + pieces.iter().map(|it| it.name_bytes).sum::<usize>();
    let names_payload = leb::uleb_len(names) + entries;
    let function_names = if names == 0 { 0 } else { 1 + leb::uleb_len(names_payload as u64) + names_payload };
    let global_names = globals.iter().enumerate().map(|(at, it)| (at as u32, it.0));
    let mut map_scratch = Vec::with_capacity(small.capacity());
    name_map(&mut small, &mut map_scratch, 7, global_names);
    let segment_names = segments.iter().enumerate().map(|(at, it)| (at as u32, it.0));
    name_map(&mut small, &mut map_scratch, 9, segment_names);
    let out = &mut layout.head;
    let payload = function_names + small.len();
    out.push(0);
    leb::uleb(out, (leb::uleb_len(4) + 4 + payload) as u64);
    leb::name(out, "name");
    if names > 0 {
        out.push(1);
        leb::uleb(out, names_payload as u64);
        leb::uleb(out, names);
        out.extend_from_slice(&scratch);
        for (at, piece) in pieces.iter().enumerate() {
            if piece.names > 0 {
                layout.run(Run::Names(at));
            }
        }
    }
    let out = &mut layout.head;
    out.extend_from_slice(&small);
    for (name, payload) in &merged_sections {
        leb::custom(out, name, payload);
    }

    let (head, runs) = layout.finish();
    let total: usize = runs.iter().map(|it| it.len(&pieces)).sum();
    // alloc: the module, zeroed: its pages are first touched by the threads writing its runs
    let mut out = vec![0u8; total];
    let mut rest = &mut out[..];
    let mut jobs = Vec::with_capacity(runs.len());
    for run in runs {
        let (place, tail) = std::mem::take(&mut rest).split_at_mut(run.len(&pieces));
        rest = tail;
        jobs.push((place, run));
    }
    let piece_units = |at: usize| &units[at * PIECE..((at + 1) * PIECE).min(units.len())];
    par::for_each(jobs, |(place, run)| match run {
        Run::Head(range) => place.copy_from_slice(&head[range]),
        Run::Copy(bytes) => place.copy_from_slice(bytes),
        Run::Zeros(_) => {}
        Run::Functions(at) => ulebs_into(place, pieces[at].function_types.iter().map(|&it| it.into())),
        Run::Elements(range) => ulebs_into(place, range),
        Run::Code(at) => code_piece(place, piece_units(at), &pieces[at], &ctors),
        Run::Names(at) => names_piece(place, piece_units(at), imported + (at * PIECE) as u32),
    });
    Ok(out)
}

#[cfg(test)]
mod tests {
    use std::collections::BTreeMap;

    use blaze_codegen::wasm::{Code, Import, Signature, ValType};

    use super::*;
    use crate::model::{self, Export, Function, GotEntry, Holds, Plan, Prot, Region, Writer};
    use crate::output::{Debug, Format, Output};
    use crate::wasm::leb::padded_sleb;
    use crate::wasm::reader;
    use crate::wasm::{STACK, custom};

    const DATA: u64 = STACK;

    fn sig(params: &[ValType], results: &[ValType]) -> Signature {
        Signature { params: params.to_vec(), results: results.to_vec() }
    }

    fn fd_write() -> Import {
        Import {
            module: "wasi_snapshot_preview1".to_owned(),
            field: "fd_write".to_owned(),
            ty: sig(&[ValType::I32; 4], &[ValType::I32]),
        }
    }

    /// `_start`'s body: the stack pointer at 2, a call of `__wasm_call_ctors` (index 1, as the
    /// core wrote it), the address of the data and `init`'s slot (3), and an indirect call
    /// whose type is at 27.
    fn start_body() -> Vec<u8> {
        let mut body = vec![0x00, 0x23];
        body.extend(padded_uleb(0));
        body.extend([0x1a, 0x10]);
        body.extend(padded_uleb(1));
        body.push(0x41);
        body.extend(padded_sleb(DATA as i32));
        body.push(0x41);
        body.extend(padded_sleb(3));
        body.push(0x11);
        body.extend(padded_uleb(0));
        body.extend([0x00, 0x0b]);
        assert_eq!(body.len(), 34);
        body
    }

    fn piece(code: &Code, body: &[u8]) -> Vec<u8> {
        code.encode(body).0
    }

    /// A placed image as the core hands it to the writer.
    struct Image {
        output: Output,
        plan: Plan,
        regions: Vec<Region>,
        contents: Vec<Vec<u8>>,
        imports: Vec<model::Import>,
        exports: Vec<Export>,
        functions: Vec<Function>,
        entry: Option<u64>,
        got: Vec<GotEntry>,
        debug: Vec<(&'static str, Vec<u8>)>,
    }

    impl Image {
        fn with<R>(&self, then: impl FnOnce(&Placed<'_>) -> R) -> R {
            let contents: Vec<&[u8]> = self.contents.iter().map(Vec::as_slice).collect();
            then(&Placed {
                output: &self.output,
                name: "app.wasm",
                plan: &self.plan,
                regions: &self.regions,
                contents: &contents,
                paged: &[],
                libraries: &[],
                imports: &self.imports,
                got: &self.got,
                rebase: &[],
                binds: &[],
                exports: &self.exports,
                functions: &self.functions,
                entry: self.entry,
                init: None,
                fini: None,
                tls: None,
                id: [0; 16],
                debug: &self.debug,
                symbol: &|_| None,
                synthesis: 0,
                delta: None,
                encoded: &[],
            })
        }

        fn writer(&self) -> Wasm {
            super::super::writer(&self.output).unwrap()
        }

        /// The writer's synthetic regions filled, as the core fills them before the file.
        fn synthesize(&mut self, writer: &Wasm) -> Result<(), String> {
            let synthetic = self.with(|placed| writer.synthesize(placed)).map_err(|e| e.what)?;
            for (class, bytes) in synthetic {
                let Some(at) = self.regions.iter().position(|it| it.class == class) else {
                    return Err(format!("the writer filled {class:?}, which has no region"));
                };
                assert!(bytes.len() as u64 <= self.regions[at].capacity, "{class:?} outgrew its room");
                self.regions[at].size = bytes.len() as u64;
                self.contents[at] = bytes.contiguous().into_owned();
            }
            Ok(())
        }

        /// The module, written as the core asks for it: synthesized, then the file.
        fn module(&mut self) -> Result<Vec<u8>, String> {
            let writer = self.writer();
            self.synthesize(&writer)?;
            self.with(|placed| {
                for region in placed.regions {
                    assert_eq!(writer.offset(placed, region), None, "{:?}", region.class);
                }
            });
            let file = self.with(|placed| writer.file(placed)).map_err(|e| e.what)?;
            assert_eq!(file.parts.len(), 1);
            assert_eq!(file.parts[0].0, 0);
            assert_eq!(file.len, file.parts[0].1.len() as u64);
            Ok(file.parts[0].1.contiguous().into_owned())
        }
    }

    fn region(writer: &Wasm, class: Class, addr: u64, size: u64) -> Region {
        let prot = writer.prot(&class);
        Region { class, addr, capacity: size, size, prot, align: 1 }
    }

    /// A command: `fd_write` imported; `__wasm_call_ctors`, `init` (an init function) and
    /// `_start` in the code space from 0; `.rodata`, `.data` (`init`'s slot) and `.bss` after
    /// the stack; the heap; the init array and two custom sections after it.
    fn command() -> Image {
        let output = Output {
            format: Format::Wasm,
            arch: blaze_codegen::Arch::Wasm32,
            kind: OutputKind::Executable { entry: "_start".to_owned() },
            interpreter: None,
            platform: None,
            incremental: false,
            debug: Debug::None,
            live: None,
            tags: 0,
            stack: None,
            windows_subsystem: false,
            dev_run: None,
            exec_log: false,
            sanitize: blaze_codegen::settings::Sanitize::default(),
            rpaths: Vec::new(),
            triple: "wasm32-unknown-unknown".into(),
        };
        let writer = super::super::writer(&output).unwrap();
        let init = piece(&Code { ty: sig(&[], &[]), ..Code::default() }, &[0x00, 0x0b]);
        let start = piece(
            &Code {
                ty: sig(&[], &[]),
                types: vec![sig(&[ValType::I32], &[])],
                type_fixups: vec![(27, 0)],
                stack_pointer: vec![2],
                imports: vec![("fd_write".to_owned(), fd_write())],
                export: Some("_start".to_owned()),
            },
            &start_body(),
        );
        let (init_at, start_at) = (1, 1 + init.len() as u64);
        let text = [init.clone(), start.clone()].concat();
        let mut producers = Vec::new();
        for (field, name, version) in [("processed-by", "clang", "19"), ("language", "C11", "")] {
            leb::uleb(&mut producers, 1);
            leb::name(&mut producers, field);
            leb::uleb(&mut producers, 1);
            leb::name(&mut producers, name);
            leb::name(&mut producers, version);
        }
        let regions = vec![
            region(&writer, Class::Synthetic(FUNCTIONS), 0, 1),
            region(&writer, Class::Text, 1, text.len() as u64),
            region(&writer, Class::Rodata, DATA, 5),
            region(&writer, Class::Data, DATA + 8, 4),
            region(&writer, Class::Bss, DATA + 12, 4),
            region(&writer, Class::Synthetic(HEAP), DATA + 16, 0),
            region(&writer, Class::InitArray, DATA + 16, 4),
            region(&writer, custom("my_section"), DATA + 20, 3),
            region(&writer, custom("producers"), DATA + 23, producers.len() as u64),
        ];
        let contents = vec![
            vec![0],
            text,
            b"hello".to_vec(),
            vec![3, 0, 0, 0],
            Vec::new(),
            Vec::new(),
            vec![3, 0, 0, 0],
            vec![1, 2, 3],
            producers,
        ];
        let function = |addr, piece: &Vec<u8>, name: &str| Function {
            addr,
            size: piece.len() as u64,
            fde: None,
            windows: None,
            name: Some(name.into()),
        };
        Image {
            plan: writer.plan(),
            output,
            regions,
            contents,
            imports: vec![model::Import {
                name: "fd_write".to_owned(),
                library: None,
                version: None,
                kind: SymbolKind::Function,
                weak: false,
                wasm: Some(fd_write()),
            }],
            exports: Vec::new(),
            functions: vec![function(init_at, &init, "init"), function(start_at, &start, "_start")],
            entry: Some(start_at),
            got: Vec::new(),
            debug: Vec::new(),
        }
    }

    /// The module's sections: each id, a custom one's name, and its contents.
    fn sections(module: &[u8]) -> Vec<(u8, String, Vec<u8>)> {
        reader::sections(module)
            .unwrap()
            .into_iter()
            .map(|section| {
                let mut r = Reader::new(section.contents);
                match section.id {
                    0 => {
                        let name = r.name().unwrap().to_owned();
                        (0, name, r.take_rest().to_vec())
                    }
                    id => (id, String::new(), section.contents.to_vec()),
                }
            })
            .collect()
    }

    fn find<'s>(sections: &'s [(u8, String, Vec<u8>)], id: u8, name: &str) -> &'s [u8] {
        let found = sections.iter().find(|it| it.0 == id && it.1 == name);
        &found.unwrap_or_else(|| panic!("no section {id} `{name}`")).2
    }

    fn names(r: &mut Reader<'_>) -> Vec<(u32, String)> {
        (0..r.count().unwrap()).map(|_| (r.uleb32().unwrap(), r.name().unwrap().to_owned())).collect()
    }

    #[test]
    fn a_command() {
        let module = command().module().unwrap();
        assert_eq!(module[..8], *b"\0asm\x01\0\0\0", "a wasm module, version 1");

        let sections = sections(&module);
        let ids: Vec<u8> = sections.iter().map(|it| it.0).collect();
        assert_eq!(ids, [1, 2, 3, 4, 5, 6, 7, 9, 10, 11, 0, 0, 0]);
        let customs: Vec<&str> = sections.iter().filter(|it| it.0 == 0).map(|it| it.1.as_str()).collect();
        assert_eq!(customs, ["my_section", "name", "producers"]);

        // Types, deduplicated: the import's, `fn()`, then the indirect call's.
        let types = find(&sections, 1, "");
        assert_eq!(types[0], 3);
        let mut at = 1;
        let decoded: Vec<Signature> =
            (0..3).map(|_| Signature::decode(types, &mut at).unwrap()).collect();
        assert_eq!(at, types.len());
        assert_eq!(decoded, [fd_write().ty, sig(&[], &[]), sig(&[ValType::I32], &[])]);

        let mut r = Reader::new(find(&sections, 2, ""));
        assert_eq!(r.count().unwrap(), 1);
        assert_eq!((r.name().unwrap(), r.name().unwrap()), ("wasi_snapshot_preview1", "fd_write"));
        assert_eq!((r.u8().unwrap(), r.uleb32().unwrap()), (0, 0));
        assert!(r.is_done());

        // `__wasm_call_ctors`, `init`, `_start`: all `fn()`.
        assert_eq!(find(&sections, 3, ""), [3, 1, 1, 1]);
        // A table of the 4 functions at slots 1 to 4, slot 0 null.
        assert_eq!(find(&sections, 4, ""), [1, FUNCREF, 1, 5, 5]);
        // Memory to the heap's start: 0x10_0010 is 17 pages.
        assert_eq!(find(&sections, 5, ""), [1, 0, 17]);
        let mut global = vec![1, I32, 1];
        leb::i32_const(&mut global, STACK as i32);
        assert_eq!(find(&sections, 6, ""), global);

        let mut r = Reader::new(find(&sections, 7, ""));
        assert_eq!(r.count().unwrap(), 2);
        assert_eq!((r.name().unwrap(), r.u8().unwrap(), r.uleb32().unwrap()), ("memory", EXPORT_MEMORY, 0));
        assert_eq!((r.name().unwrap(), r.u8().unwrap(), r.uleb32().unwrap()), ("_start", EXPORT_FUNCTION, 3));
        assert!(r.is_done());

        let mut element = vec![1, 0];
        leb::i32_const(&mut element, 1);
        element.extend([4, 0, 1, 2, 3]);
        assert_eq!(find(&sections, 9, ""), element);

        let mut r = Reader::new(find(&sections, 10, ""));
        assert_eq!(r.count().unwrap(), 3);
        let mut bodies = Vec::new();
        for _ in 0..3 {
            let len = r.uleb32().unwrap() as usize;
            bodies.push(r.take(len).unwrap().to_vec());
        }
        assert!(r.is_done());
        // `__wasm_call_ctors` calls `init` (slot 3 in the init array: index 2).
        assert_eq!(bodies[0], [0x00, 0x10, 2, 0x0b]);
        assert_eq!(bodies[1], [0x00, 0x0b]);
        let mut start = start_body();
        start[2..7].copy_from_slice(&padded_uleb(STACK_POINTER));
        start[27..32].copy_from_slice(&padded_uleb(2));
        assert_eq!(bodies[2], start);

        // `.rodata` and `.data` after the stack; `.bss` and the init array are not written.
        let mut r = Reader::new(find(&sections, 11, ""));
        assert_eq!(r.count().unwrap(), 2);
        for (addr, bytes) in [(DATA, &b"hello"[..]), (DATA + 8, &[3, 0, 0, 0][..])] {
            assert_eq!(r.uleb32().unwrap(), 0);
            assert_eq!(r.i32_const().unwrap(), addr as i32);
            let len = r.uleb32().unwrap() as usize;
            assert_eq!(r.take(len).unwrap(), bytes);
        }
        assert!(r.is_done());
        assert_eq!(find(&sections, 0, "my_section"), [1, 2, 3]);

        let mut r = Reader::new(find(&sections, 0, "name"));
        let mut subsections = BTreeMap::new();
        while !r.is_done() {
            let id = r.u8().unwrap();
            let len = r.uleb32().unwrap() as usize;
            subsections.insert(id, r.take(len).unwrap());
        }
        let owned = |list: &[(u32, &str)]| -> Vec<(u32, String)> {
            list.iter().map(|&(at, name)| (at, name.to_owned())).collect()
        };
        assert_eq!(
            names(&mut Reader::new(subsections[&1])),
            owned(&[(0, "fd_write"), (1, "__wasm_call_ctors"), (2, "init"), (3, "_start")])
        );
        assert_eq!(names(&mut Reader::new(subsections[&7])), owned(&[(0, "__stack_pointer")]));
        assert_eq!(names(&mut Reader::new(subsections[&9])), owned(&[(0, ".rodata"), (1, ".data")]));
        assert_eq!(subsections.keys().copied().collect::<Vec<_>>(), [1, 7, 9]);

        // The producers sections as one, `language` first.
        let mut r = Reader::new(find(&sections, 0, "producers"));
        assert_eq!(r.count().unwrap(), 2);
        assert_eq!(r.name().unwrap(), "language");
        assert_eq!(r.count().unwrap(), 1);
        assert_eq!((r.name().unwrap(), r.name().unwrap()), ("C11", ""));
        assert_eq!(r.name().unwrap(), "processed-by");
        assert_eq!(r.count().unwrap(), 1);
        assert_eq!((r.name().unwrap(), r.name().unwrap()), ("clang", "19"));
        assert!(r.is_done());
    }

    #[test]
    fn the_same_image_is_the_same_module() {
        assert_eq!(command().module().unwrap(), command().module().unwrap());
    }

    fn on_threads<R: Send>(threads: usize, run: impl FnOnce() -> R + Send) -> R {
        blaze_graph::pool::Pool::new(&blaze_graph::topology::Topology::flat(threads), false).unwrap().install(run)
    }

    #[test]
    fn one_thread_and_many_write_the_same() {
        let one = on_threads(1, || command().module().unwrap());
        assert_eq!(on_threads(8, || command().module().unwrap()), one);

        // Segments of several chunks, and one that ends in a partial chunk.
        let sources: Vec<Vec<u8>> = [3 * CHUNK + 7, CHUNK, 1]
            .iter()
            .enumerate()
            .map(|(at, &len)| (0..len).map(|it| (it * 7 + at) as u8).collect())
            .collect();
        for source in &sources {
            let mut layout = Layout { head: Vec::new(), runs: Vec::new(), mark: 0 };
            layout.view(View::Slice(source));
            let (_, runs) = layout.finish();
            let mut joined = Vec::new();
            for run in &runs {
                let Run::Copy(bytes) = run else { panic!("a slice is one run of bytes") };
                assert!(bytes.len() <= CHUNK);
                joined.extend_from_slice(bytes);
            }
            assert_eq!(&joined, source);
        }
    }

    #[test]
    fn uleb_runs_are_sized_exactly() {
        for n in [0u64, 1, 127, 128, 129, 16_383, 16_384, 70_000, 2_097_152 + 3] {
            assert_eq!(ulebs_below(n), (0..n).map(leb::uleb_len).sum::<usize>(), "{n}");
        }
        let mut place = vec![0; ulebs_below(300) - ulebs_below(100)];
        ulebs_into(&mut place, 100..300);
        let mut expected = Vec::new();
        for it in 100..300 {
            leb::uleb(&mut expected, it);
        }
        assert_eq!(place, expected);
    }

    /// Functions in several pieces: the command's module with each extra function's entries
    /// after its own, as one pass over the functions in order writes them, on any number of
    /// threads.
    #[test]
    fn many_pieces_write_what_one_pass_would() {
        let extra = 3 * PIECE + 5;
        let module = |threads| on_threads(threads, || with_functions(extra).module().unwrap());
        let one = module(1);
        assert_eq!(module(8), one);
        let (sections, base) = (sections(&one), sections(&command().module().unwrap()));
        let count = |bytes: &[u8]| Reader::new(bytes).count().unwrap() as usize;
        let rest = |bytes: &[u8]| {
            let mut r = Reader::new(bytes);
            r.count().unwrap();
            r.take_rest().to_vec()
        };
        let (imported, defined) = (count(find(&base, 2, "")), count(find(&base, 3, "")));

        // Each `fn()` is type 1, as in the command: the import's type, then `fn()`.
        let mut expected = Vec::new();
        leb::uleb(&mut expected, (defined + extra) as u64);
        expected.extend(rest(find(&base, 3, "")));
        expected.extend(std::iter::repeat_n(1, extra));
        assert_eq!(find(&sections, 3, ""), expected);

        let mut expected = Vec::new();
        leb::uleb(&mut expected, (defined + extra) as u64);
        expected.extend(rest(find(&base, 10, "")));
        for _ in 0..extra {
            expected.extend([2, 0x00, 0x0b]);
        }
        assert_eq!(find(&sections, 10, ""), expected);

        let slots = (imported + defined + extra) as u64;
        let mut expected = vec![1, 0];
        leb::i32_const(&mut expected, 1);
        leb::uleb(&mut expected, slots);
        for it in 0..slots {
            leb::uleb(&mut expected, it);
        }
        assert_eq!(find(&sections, 9, ""), expected);

        let function_names = |name_section: &[u8]| {
            let mut r = Reader::new(name_section);
            assert_eq!(r.u8().unwrap(), 1);
            let len = r.uleb32().unwrap() as usize;
            names(&mut Reader::new(r.take(len).unwrap()))
        };
        let mut expected = function_names(find(&base, 0, "name"));
        expected.extend((0..extra).map(|at| ((imported + defined + at) as u32, format!("f{at}"))));
        assert_eq!(function_names(find(&sections, 0, "name")), expected);
    }

    /// [`command`] with `extra` more `fn()`s after `_start`, each named.
    fn with_functions(extra: usize) -> Image {
        let mut image = command();
        let piece = piece(&Code { ty: sig(&[], &[]), ..Code::default() }, &[0x00, 0x0b]);
        let text = image.regions.iter().position(|it| it.class == Class::Text).unwrap();
        for at in 0..extra {
            let addr = image.regions[text].addr + image.contents[text].len() as u64;
            image.contents[text].extend_from_slice(&piece);
            image.functions.push(Function {
                addr,
                size: piece.len() as u64,
                fde: None,
                windows: None,
                name: Some(format!("f{at}").into()),
            });
        }
        let size = image.contents[text].len() as u64;
        (image.regions[text].size, image.regions[text].capacity) = (size, size);
        image
    }

    #[test]
    fn a_module_is_written_in_a_constant_number_of_buffers() {
        // On a pool of one thread the parallel parts run on the measured thread too.
        let used = |extra| {
            let mut image = with_functions(extra);
            let writer = image.writer();
            image.synthesize(&writer).unwrap();
            on_threads(1, || {
                image.with(|placed| blaze_alloc_count::measure(|| writer.file(placed).is_ok()))
            })
        };
        let ((small_ok, small), (large_ok, large)) = (used(100), used(2000));
        assert!(small_ok && large_ok);
        // About 40: the columns `write` keeps (a set per piece of 4096 functions), its maps of
        // types, exports, imports and custom sections, the merged producers section, the
        // layout and the output. None per function, segment or section.
        assert!(small.allocations <= 64, "{small:?}");
        assert_eq!(large.allocations, small.allocations, "20 times the functions");
    }

    #[test]
    fn a_reactor_exports_functions_and_data() {
        let mut image = command();
        image.output.kind = OutputKind::Shared {
            name: "lib.wasm".to_owned(),
            exports: vec!["init".to_owned(), "__heap_base".to_owned()],
        };
        image.entry = None;
        let init = image.functions[0].addr;
        image.exports = vec![
            Export { name: "init".to_owned(), addr: init, size: 4, kind: SymbolKind::Function },
            Export { name: "__heap_base".to_owned(), addr: DATA + 16, size: 0, kind: SymbolKind::Other },
        ];
        let module = image.module().unwrap();
        let sections = sections(&module);
        let mut r = Reader::new(find(&sections, 7, ""));
        let mut exports = Vec::new();
        for _ in 0..r.count().unwrap() {
            exports.push((r.name().unwrap().to_owned(), r.u8().unwrap(), r.uleb32().unwrap()));
        }
        // `_start` is exported by its object's `export_name`, as a reactor's `_initialize` is.
        assert_eq!(
            exports,
            [
                ("memory".to_owned(), EXPORT_MEMORY, 0),
                ("init".to_owned(), EXPORT_FUNCTION, 2),
                ("__heap_base".to_owned(), EXPORT_GLOBAL, 1),
                ("_start".to_owned(), EXPORT_FUNCTION, 3),
            ]
        );
        let mut global = vec![2, I32, 1];
        leb::i32_const(&mut global, STACK as i32);
        global.extend([I32, 0]);
        leb::i32_const(&mut global, (DATA + 16) as i32);
        assert_eq!(find(&sections, 6, ""), global);
    }

    #[test]
    fn functions_are_numbered_as_the_core_numbers_them() {
        // Two imports, then the code by its address, whatever order the core lists it in.
        let mut image = command();
        let mut second = image.imports[0].clone();
        second.name = "proc_exit".to_owned();
        second.wasm = Some(Import {
            module: "wasi_snapshot_preview1".to_owned(),
            field: "proc_exit".to_owned(),
            ty: sig(&[ValType::I32], &[]),
        });
        image.imports.push(second);
        image.functions.reverse();
        image.contents[3] = vec![4, 0, 0, 0];
        image.contents[6] = vec![4, 0, 0, 0];
        let module = image.module().unwrap();
        let sections = sections(&module);
        let mut r = Reader::new(find(&sections, 7, ""));
        assert_eq!(r.count().unwrap(), 2);
        assert_eq!((r.name().unwrap(), r.u8().unwrap(), r.uleb32().unwrap()), ("memory", EXPORT_MEMORY, 0));
        assert_eq!((r.name().unwrap(), r.u8().unwrap(), r.uleb32().unwrap()), ("_start", EXPORT_FUNCTION, 4));
        let mut r = Reader::new(find(&sections, 10, ""));
        r.count().unwrap();
        let len = r.uleb32().unwrap() as usize;
        assert_eq!(r.take(len).unwrap(), [0x00, 0x10, 3, 0x0b]);
        assert_eq!(find(&sections, 4, ""), [1, FUNCREF, 1, 6, 6]);
    }

    #[test]
    fn what_a_module_cannot_hold() {
        let refused = |change: fn(&mut Image), says: &str| {
            let mut image = command();
            change(&mut image);
            let what = image.module().unwrap_err();
            assert!(what.contains(says), "`{what}` does not say `{says}`");
        };
        refused(|it| it.debug.push((".debug_line", vec![1])), ".debug_line");
        refused(|it| it.got.push(GotEntry { addr: DATA, holds: Holds::Local(DATA) }), "GOT");
        refused(|it| it.imports[0].kind = SymbolKind::Data, "only functions");
        refused(|it| it.entry = None, "`_start` is not defined");
        refused(|it| it.entry = Some(2), "no function of the module starts");
        refused(
            |it| {
                it.functions.remove(0);
            },
            "no function of the image's",
        );
        refused(|it| it.contents[6] = vec![0, 0, 0, 0], "undefined weak");
        refused(|it| it.contents[6] = vec![9, 0, 0, 0], "init function 8");
        refused(|it| it.contents[6] = vec![1, 0, 0, 0], "init function 0");
        refused(|it| it.regions[2].addr = 16, "in the stack");
        refused(
            |it| {
                let fini = Region {
                    class: Class::FiniArray,
                    addr: DATA,
                    capacity: 4,
                    size: 4,
                    prot: Prot::RW,
                    align: 4,
                };
                it.regions.push(fini);
                it.contents.push(vec![0; 4]);
            },
            "FiniArray",
        );
        refused(
            |it| {
                let tdata = Region {
                    class: Class::Tdata,
                    addr: DATA,
                    capacity: 4,
                    size: 4,
                    prot: Prot::RW,
                    align: 4,
                };
                it.regions.push(tdata);
                it.contents.push(vec![0; 4]);
            },
            "Tdata",
        );
        refused(
            |it| {
                let at = it.regions.iter().position(|r| r.class == Class::Synthetic(HEAP)).unwrap();
                it.regions.remove(at);
                it.contents.remove(at);
            },
            "heap",
        );
    }

    #[test]
    fn imports_need_a_module() {
        let mut image = command();
        image.imports[0].wasm = None;
        let what = image.module().unwrap_err();
        assert!(what.contains("`fd_write`"), "{what}");

        // `_start`'s header imports `fd_write` from `wasi_snapshot_preview1`.
        let mut image = command();
        image.imports[0].wasm.as_mut().unwrap().module = "env".to_owned();
        let what = image.module().unwrap_err();
        assert!(what.contains("`env.fd_write`"), "{what}");
    }

    #[test]
    fn merged_target_features() {
        let mut payload = Vec::new();
        for (prefix, feature) in [(b'+', "mutable-globals"), (b'+', "sign-ext")] {
            leb::uleb(&mut payload, 1);
            payload.push(prefix);
            leb::name(&mut payload, feature);
        }
        let merged = target_features(&payload).unwrap();
        let mut r = Reader::new(&merged);
        assert_eq!(r.count().unwrap(), 2);
        assert_eq!((r.u8().unwrap(), r.name().unwrap()), (b'+', "mutable-globals"));
        assert_eq!((r.u8().unwrap(), r.name().unwrap()), (b'+', "sign-ext"));
        leb::uleb(&mut payload, 1);
        payload.push(b'-');
        leb::name(&mut payload, "sign-ext");
        assert!(target_features(&payload).unwrap_err().contains("sign-ext"));
    }
}
