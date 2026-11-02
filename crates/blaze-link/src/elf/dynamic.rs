//! The regions only the ELF writer fills, from the placed image: `.interp`, the build ID,
//! the dynamic symbol table with its strings, GNU hash table and versions, `.rela.dyn`,
//! `.dynamic` and `.eh_frame_hdr`. A pure function of [`Placed`]: an incremental link asks
//! again and gets the same bytes for the same image, on any number of cores. An incremental
//! output keeps its last `.rela.dyn` and `.eh_frame_hdr` and patches them where
//! [`Placed::delta`] says what changed; a patched table is the one a build makes. A kept table
//! is a [`CowBytes`] the writer and the core's states share: a patch copies only the pages it
//! changes, and the table handed to the core is a clone, never a copy.

use std::collections::hash_map::Entry;

use blaze_codegen::Arch;
use blaze_format::cursor::Put;
use blaze_graph::par;
use blaze_ids::radix;
use object::elf;
use blaze_hash::HashMap;

use super::{
    BUILD_ID, DYNAMIC, DYNSTR, DYNSYM, EH_FRAME_HDR, Elf, GNU_HASH, INTERP, RELA_DYN, Segment, VERNEED,
    VERSYM, error,
};
use crate::cow::CowBytes;
use crate::model::{Class, Counts, Delta, Holds, Placed, Region, SymbolKind, Tls, View};
use crate::unwind::Frame;
use crate::{LinkError, Output, OutputKind};

/// `.dynamic` entries besides `DT_NEEDED`: at most one of each tag [`Tables::of`] writes.
const DYNAMIC_FIXED: u64 = 25;
const SYM: u64 = 24;
const RELA: u64 = 24;

/// Items per pool task: below this a split costs more than the work it shares.
pub(crate) const CHUNK: usize = 4096;

/// The first error in input order, so a failed link names the same place on any core count.
pub(crate) fn first_error(results: Vec<Result<(), LinkError>>) -> Result<(), LinkError> {
    results.into_iter().find_map(Result::err).map_or(Ok(()), Err)
}

/// Sorts relocations in their derived order: by addend, then stably by the fields before it.
/// Free when they are sorted already (the core hands over addresses in order), which it
/// returns.
fn sort_rela(items: &mut [Rela]) -> bool {
    if items.is_sorted() {
        return true;
    }
    radix::sort_copy_by_key(items, |it| radix::signed_64(it.addend));
    radix::sort_copy_by_key(items, |it| radix::pack_64(it.at, radix::pack_32(it.r_type, it.sym)));
    false
}

/// Every synthetic region, `.interp` first.
const SYNTHETIC: [&str; 10] =
    [INTERP, BUILD_ID, DYNSYM, DYNSTR, GNU_HASH, VERSYM, VERNEED, RELA_DYN, DYNAMIC, EH_FRAME_HDR];

/// The synthetic regions of an output, in no particular order ([`super::Elf::order`] sorts):
/// `.interp` only with an interpreter.
fn names(output: &Output) -> &'static [&'static str] {
    match output.interpreter {
        Some(_) => &SYNTHETIC,
        None => &SYNTHETIC[1..],
    }
}

fn soname(output: &Output) -> Option<&str> {
    match &output.kind {
        OutputKind::Shared { name, .. } => Some(name),
        OutputKind::Executable { .. } => None,
    }
}

/// How big each synthetic region may get, from the counts known before placement.
pub(crate) fn sizes(output: &Output, counts: &Counts) -> Vec<(Class, u64)> {
    let symbols = 1 + counts.imports + counts.exports;
    let soname = soname(output).map_or(0, |it| it.len() as u64 + 1);
    let runpath = output.rpaths.iter().map(|it| it.len() as u64 + 1).sum::<u64>();
    names(output)
        .iter()
        .map(|&name| {
            let size = match name {
                INTERP => output.interpreter.as_ref().map_or(0, |it| it.len() as u64 + 1),
                BUILD_ID => NOTE_HEADER + 16,
                DYNSYM => SYM * symbols,
                DYNSTR => {
                    1 + counts.import_names
                        + 2 * counts.imports
                        + counts.export_names
                        + counts.exports
                        + counts.library_names
                        + counts.libraries
                        + soname
                        + runpath
                }
                GNU_HASH => gnu_hash_len(counts.exports),
                VERSYM => 2 * symbols,
                VERNEED => 16 * (counts.libraries + counts.imports),
                RELA_DYN => RELA * (counts.absolute + counts.got),
                DYNAMIC => 16 * (counts.libraries + DYNAMIC_FIXED + u64::from(!output.rpaths.is_empty())),
                EH_FRAME_HDR => 12 + 8 * counts.fdes,
                _ => unreachable!("a synthetic region `names` doesn't list"),
            };
            (Class::Synthetic(name), size)
        })
        .collect() // alloc: once per image, what `Writer::synthetic` returns
}

const NOTE_HEADER: u64 = 12 + 4;

/// A string table: `\0` first, each string once. Keys borrow the image's names: one hash
/// probe (entry API) per string and no allocation per string.
pub(crate) struct Strings<'a> {
    pub(crate) bytes: Vec<u8>,
    at: HashMap<&'a str, u32>,
}

impl<'a> Strings<'a> {
    /// Room for `strings` strings of `bytes` bytes with their terminators.
    pub(crate) fn with_capacity(bytes: usize, strings: usize) -> Strings<'a> {
        let mut table = Vec::with_capacity(1 + bytes);
        table.push(0);
        Strings { bytes: table, at: HashMap::with_capacity_and_hasher(strings, Default::default()) }
    }

    pub(crate) fn add(&mut self, string: &'a str) -> u32 {
        if string.is_empty() {
            return 0;
        }
        let next = self.bytes.len() as u32;
        let at = *self.at.entry(string).or_insert(next);
        if at == next {
            self.bytes.extend_from_slice(string.as_bytes());
            self.bytes.push(0);
        }
        at
    }
}

/// One `Elf64_Sym`.
#[derive(Clone, Copy, Default)]
pub(crate) struct Sym {
    pub(crate) name: u32,
    pub(crate) info: u8,
    pub(crate) shndx: u16,
    pub(crate) value: u64,
    pub(crate) size: u64,
}

impl Sym {
    pub(crate) fn bytes(&self) -> [u8; SYM as usize] {
        let mut out = [0; SYM as usize];
        out[0..4].copy_from_slice(&self.name.to_le_bytes());
        out[4] = self.info;
        out[5] = elf::STV_DEFAULT.0;
        out[6..8].copy_from_slice(&self.shndx.to_le_bytes());
        out[8..16].copy_from_slice(&self.value.to_le_bytes());
        out[16..24].copy_from_slice(&self.size.to_le_bytes());
        out
    }

    pub(crate) fn put(&self, out: &mut Vec<u8>) {
        out.extend_from_slice(&self.bytes());
    }
}

pub(crate) fn sym_type(kind: SymbolKind) -> u8 {
    match kind {
        SymbolKind::Function => elf::STT_FUNC.0,
        SymbolKind::Data => elf::STT_OBJECT.0,
        SymbolKind::Tls => elf::STT_TLS.0,
        SymbolKind::Section | SymbolKind::Other => elf::STT_NOTYPE.0,
    }
}

pub(crate) fn info(bind: elf::SymbolBind, kind: SymbolKind) -> u8 {
    (bind.0 << 4) | sym_type(kind)
}

/// Whether every region's section header has an ordinary ELF number, as [`shndx`] needs.
pub(crate) fn numbered(regions: &[Region]) -> Result<(), LinkError> {
    if regions.len() + 1 >= usize::from(elf::SHN_LORESERVE) {
        return Err(error(format!("{} regions: more sections than ELF numbers", regions.len()))); // alloc: error path
    }
    Ok(())
}

/// The section header index of the region holding `addr`, once [`numbered`] held: one more
/// than its index in [`Placed::regions`], which [`super::image`] writes one section header
/// each, in order. A binary search over the regions.
pub(crate) fn shndx(regions: &[Region], addr: u64) -> u16 {
    let at = regions.partition_point(|it| it.addr <= addr);
    match at.checked_sub(1).filter(|&it| addr <= regions[it].addr + regions[it].capacity) {
        Some(index) => (index + 1) as u16,
        None => elf::SHN_ABS.0,
    }
}

/// The value an exported or listed symbol has: thread-locals by their offset in the TLS
/// template, as the loader reads `STT_TLS` symbols.
pub(crate) fn value(placed: &Placed<'_>, name: &str, addr: u64, kind: SymbolKind) -> Result<u64, LinkError> {
    if kind != SymbolKind::Tls {
        return Ok(addr);
    }
    let tls = placed.tls.ok_or_else(|| error(format!("thread-local `{name}` in an image with no TLS")))?; // alloc: error path
    addr.checked_sub(tls.addr)
        .filter(|&it| it <= tls.size)
        .ok_or_else(|| error(format!("thread-local `{name}` at {addr:#x} is outside the TLS template"))) // alloc: error path
}

/// The dynamic relocation types of an architecture.
struct Types {
    relative: u32,
    glob_dat: u32,
    abs64: u32,
    tpoff64: u32,
    dtpmod64: u32,
    dtpoff64: u32,
    tlsdesc: u32,
}

fn types(arch: Arch) -> Types {
    match arch {
        Arch::Aarch64 => Types {
            relative: elf::R_AARCH64_RELATIVE.0,
            glob_dat: elf::R_AARCH64_GLOB_DAT.0,
            abs64: elf::R_AARCH64_ABS64.0,
            tpoff64: elf::R_AARCH64_TLS_TPREL.0,
            dtpmod64: elf::R_AARCH64_TLS_DTPMOD.0,
            dtpoff64: elf::R_AARCH64_TLS_DTPREL.0,
            tlsdesc: elf::R_AARCH64_TLSDESC.0,
        },
        Arch::X86_64 | Arch::Wasm32 => Types {
            relative: elf::R_X86_64_RELATIVE.0,
            glob_dat: elf::R_X86_64_GLOB_DAT.0,
            abs64: elf::R_X86_64_64.0,
            tpoff64: elf::R_X86_64_TPOFF64.0,
            dtpmod64: elf::R_X86_64_DTPMOD64.0,
            dtpoff64: elf::R_X86_64_DTPOFF64.0,
            tlsdesc: elf::R_X86_64_TLSDESC.0,
        },
    }
}

/// One `Elf64_Rela`.
#[derive(Clone, Copy, Default, PartialEq, Eq, PartialOrd, Ord, Debug)]
pub(crate) struct Rela {
    pub(crate) at: u64,
    pub(crate) r_type: u32,
    pub(crate) sym: u32,
    pub(crate) addend: i64,
}

const _: () = assert!(size_of::<Rela>() == RELA as usize);

impl Rela {
    fn bytes(&self) -> [u8; RELA as usize] {
        let mut out = [0; RELA as usize];
        out[0..8].copy_from_slice(&self.at.to_le_bytes());
        out[8..16].copy_from_slice(&((u64::from(self.sym) << 32) | u64::from(self.r_type)).to_le_bytes());
        out[16..24].copy_from_slice(&self.addend.to_le_bytes());
        out
    }
}

/// Every synthetic region's bytes.
pub(crate) struct Tables {
    pub(crate) interp: Option<Vec<u8>>,
    pub(crate) note: Vec<u8>,
    pub(crate) dynsym: Vec<u8>,
    pub(crate) dynstr: Vec<u8>,
    pub(crate) gnu_hash: Vec<u8>,
    pub(crate) versym: Vec<u8>,
    pub(crate) verneed: Vec<u8>,
    /// The kept tables, taken once each by [`Tables::regions`].
    pub(crate) rela: Option<CowBytes>,
    pub(crate) dynamic: Vec<u8>,
    pub(crate) eh_frame_hdr: Option<CowBytes>,
}

impl Tables {
    pub(crate) fn of(writer: &Elf, placed: &Placed<'_>) -> Result<Tables, LinkError> {
        let output = &writer.output;
        let shared = matches!(output.kind, OutputKind::Shared { .. });
        let libraries = placed.libraries;
        let imports = placed.imports;
        let exports = placed.exports;

        // `--as-needed`: a library none of whose symbols are imported is left out. By library,
        // its place among the needed ones in load order, or NOT_NEEDED; an import marks its
        // library 0 until they are numbered.
        const NOT_NEEDED: u32 = u32::MAX;
        // alloc: once per image, 4 bytes per library
        let mut slot = vec![NOT_NEEDED; libraries.len()];
        for (at, import) in imports.iter().enumerate() {
            let Some(library) = import.library else { continue };
            let Some(flag) = slot.get_mut(library as usize) else {
                // alloc: error path
                return Err(error(format!("import {at} (`{}`) of library {library}, of no library", import.name)));
            };
            *flag = 0;
        }

        // alloc: once per image with an rpath, `DT_RUNPATH`'s one string
        let runpath = (!output.rpaths.is_empty()).then(|| output.rpaths.join(":"));
        let string_bytes = libraries.iter().map(|it| it.name.len() + 1).sum::<usize>()
            + soname(output).map_or(0, |it| it.len() + 1)
            + runpath.as_ref().map_or(0, |it| it.len() + 1)
            + imports.iter().map(|it| it.name.len() + 1 + it.version.as_ref().map_or(0, |it| it.len() + 1)).sum::<usize>()
            + exports.iter().map(|it| it.name.len() + 1).sum::<usize>();
        let mut strings = Strings::with_capacity(string_bytes, libraries.len() + 1 + 2 * imports.len() + exports.len());
        // By needed library: its name's offset in `.dynstr`.
        let mut needed_names = Vec::with_capacity(libraries.len());
        for (at, library) in libraries.iter().enumerate() {
            if library.as_needed && slot[at] == NOT_NEEDED {
                continue;
            }
            slot[at] = needed_names.len() as u32;
            needed_names.push(strings.add(&library.name));
        }
        let soname = soname(output).map(|it| strings.add(it));
        let runpath = runpath.as_deref().map(|it| strings.add(it));

        // Versions of imports, numbered from 2 in order of first use, one probe per import;
        // `.gnu.version` takes each import's as it is found. `needs` holds each version once
        // with its library's slot, then is sorted stably by slot: each library's versions are
        // a run of it, in the order they were numbered.
        let symbols = 1 + imports.len() + exports.len();
        let symoffset = 1 + imports.len() as u32;
        let mut versym = Vec::with_capacity(symbols * 2);
        versym.put_u16(elf::VER_NDX_LOCAL.0);
        let versioned = imports.iter().filter(|it| it.library.is_some() && it.version.is_some()).count();
        let mut needs: Vec<(u32, &str, u16)> = Vec::with_capacity(versioned);
        let mut version_index: HashMap<(u32, &str), u16> =
            HashMap::with_capacity_and_hasher(versioned, Default::default());
        let mut next = 2u16;
        for import in imports {
            let version = match (import.library, &import.version) {
                (Some(library), Some(version)) if slot[library as usize] != NOT_NEEDED => {
                    let slot = slot[library as usize];
                    match version_index.entry((slot, version.as_str())) {
                        Entry::Occupied(it) => *it.get(),
                        Entry::Vacant(it) => {
                            let index = next;
                            next = next
                                .checked_add(1)
                                .filter(|&it| it < elf::VERSYM_HIDDEN.0)
                                .ok_or_else(|| error("more symbol versions than ELF numbers".into()))?;
                            needs.push((slot, version.as_str(), index));
                            *it.insert(index)
                        }
                    }
                }
                _ => elf::VER_NDX_GLOBAL.0,
            };
            versym.put_u16(version);
        }
        versym.resize(symbols * 2, 0);
        for pair in versym[2 * symoffset as usize..].chunks_exact_mut(2) {
            pair.copy_from_slice(&elf::VER_NDX_GLOBAL.0.to_le_bytes());
        }
        radix::sort_copy_by_key(&mut needs, |it| it.0);

        // `.gnu.hash` as mold builds it: every hash once (in parallel when there are many),
        // then the exports in bucket order by a stable counting sort, the order a stable sort
        // by bucket gives, in two passes.
        let (buckets, mask_words) = gnu_hash_shape(exports.len() as u64);
        let mut hashes = Vec::with_capacity(exports.len());
        if exports.len() >= CHUNK {
            par::extend(&mut hashes, exports, |it| gnu_hash(it.name.as_bytes()));
        } else {
            hashes.extend(exports.iter().map(|it| gnu_hash(it.name.as_bytes())));
        }
        let bucket_of = |hash: u32| (hash % buckets as u32) as usize;
        // alloc: once per image, 4 bytes per bucket and per export
        let order = radix::Csr::build(
            buckets as usize,
            hashes.iter().enumerate().map(|(at, &hash)| (bucket_of(hash) as u32, at as u32)),
        );

        // `.dynsym`: the null symbol, the imports, then the exports in bucket order.
        let mut dynsym = Vec::with_capacity(symbols * SYM as usize);
        Sym::default().put(&mut dynsym);
        for import in imports {
            let bind = if import.weak { elf::STB_WEAK } else { elf::STB_GLOBAL };
            Sym { name: strings.add(&import.name), info: info(bind, import.kind), ..Sym::default() }.put(&mut dynsym);
        }
        numbered(placed.regions)?;
        for &at in order.items() {
            let export = &exports[at as usize];
            Sym {
                name: strings.add(&export.name),
                info: info(elf::STB_GLOBAL, export.kind),
                shndx: shndx(placed.regions, export.addr),
                value: value(placed, &export.name, export.addr, export.kind)?,
                size: export.size,
            }
            .put(&mut dynsym);
        }

        let versioned_libraries = needs.chunk_by(|a, b| a.0 == b.0).count();
        let mut verneed = Vec::with_capacity(16 * (versioned_libraries + needs.len()));
        for (at, list) in needs.chunk_by(|a, b| a.0 == b.0).enumerate() {
            let last = at + 1 == versioned_libraries;
            verneed.put_u16(1);
            verneed.put_u16(list.len() as u16);
            verneed.put_u32(needed_names[list[0].0 as usize]);
            verneed.put_u32(16);
            verneed.put_u32(if last { 0 } else { 16 + 16 * list.len() as u32 });
            for (at, &(_, name, index)) in list.iter().enumerate() {
                verneed.put_u32(elf_hash(name.as_bytes()));
                verneed.put_u16(0);
                verneed.put_u16(index);
                verneed.put_u32(strings.add(name));
                verneed.put_u32(if at + 1 == list.len() { 0 } else { 16 });
            }
        }

        let mut gnu_hash = Vec::with_capacity(gnu_hash_len(exports.len() as u64) as usize);
        gnu_hash.put_u32(buckets as u32);
        gnu_hash.put_u32(symoffset);
        gnu_hash.put_u32(mask_words as u32);
        gnu_hash.put_u32(BLOOM_SHIFT);
        // The bloom filter, set in place.
        let bloom = gnu_hash.len();
        gnu_hash.resize(bloom + 8 * mask_words as usize, 0);
        for &hash in &hashes {
            let word = bloom + 8 * ((hash as u64 / 64) % mask_words) as usize;
            let bits = (1u64 << (hash % 64)) | (1u64 << ((hash >> BLOOM_SHIFT) % 64));
            let cell: &mut [u8; 8] = (&mut gnu_hash[word..word + 8]).try_into().expect("8 bytes");
            *cell = (u64::from_le_bytes(*cell) | bits).to_le_bytes();
        }
        for bucket in 0..buckets as usize {
            let range = order.range(bucket);
            gnu_hash.put_u32(if range.is_empty() { 0 } else { symoffset + range.start as u32 });
        }
        for (at, &export) in order.items().iter().enumerate() {
            let hash = hashes[export as usize];
            let last = at + 1 == order.range(bucket_of(hash)).end;
            gnu_hash.put_u32((hash & !1) | last as u32);
        }

        let mut last = writer.last();
        let (rela, relative, tpoff) = rela_dyn(writer, placed, shared, last.as_deref_mut().map(|it| &mut it.rela))?;
        let eh_frame_hdr = eh_frame_hdr(placed, last.as_deref_mut().map(|it| &mut it.eh_frame_hdr))?;
        drop(last);

        let address = |name: &'static str| {
            placed
                .region(&Class::Synthetic(name))
                .map(|(_, it)| it.addr)
                .ok_or_else(|| error(format!("the image has no `{name}` region"))) // alloc: error path
        };
        let mut dynamic = Vec::with_capacity(16 * (needed_names.len() + DYNAMIC_FIXED as usize));
        let mut entry = |tag: elf::DynamicTag, value: u64| {
            dynamic.put_u64(tag.0 as u64);
            dynamic.put_u64(value);
        };
        for &name in &needed_names {
            entry(elf::DT_NEEDED, u64::from(name));
        }
        if let Some(soname) = soname {
            entry(elf::DT_SONAME, u64::from(soname));
        }
        if let Some(runpath) = runpath {
            entry(elf::DT_RUNPATH, u64::from(runpath));
        }
        let static_tls = shared && tpoff;
        let flags = elf::DF_BIND_NOW
            | if shared { elf::DF_SYMBOLIC } else { elf::DynamicFlags(0) }
            | if static_tls { elf::DF_STATIC_TLS } else { elf::DynamicFlags(0) };
        entry(elf::DT_FLAGS, flags.0);
        let flags_1 = elf::DF_1_NOW | if shared { elf::DynamicFlags1(0) } else { elf::DF_1_PIE };
        entry(elf::DT_FLAGS_1, flags_1.0);
        if let Some(init) = placed.init {
            entry(elf::DT_INIT, init);
        }
        if let Some(fini) = placed.fini {
            entry(elf::DT_FINI, fini);
        }
        for (class, tag, size) in [
            (Class::PreinitArray, elf::DT_PREINIT_ARRAY, elf::DT_PREINIT_ARRAYSZ),
            (Class::InitArray, elf::DT_INIT_ARRAY, elf::DT_INIT_ARRAYSZ),
            (Class::FiniArray, elf::DT_FINI_ARRAY, elf::DT_FINI_ARRAYSZ),
        ] {
            if let Some((_, region)) = placed.region(&class) {
                entry(tag, region.addr);
                entry(size, region.size);
            }
        }
        entry(elf::DT_GNU_HASH, address(GNU_HASH)?);
        entry(elf::DT_STRTAB, address(DYNSTR)?);
        entry(elf::DT_SYMTAB, address(DYNSYM)?);
        entry(elf::DT_STRSZ, strings.bytes.len() as u64);
        entry(elf::DT_SYMENT, SYM);
        if !shared {
            entry(elf::DT_DEBUG, 0);
        }
        if !rela.is_empty() {
            entry(elf::DT_RELA, address(RELA_DYN)?);
            entry(elf::DT_RELASZ, rela.len() as u64);
            entry(elf::DT_RELAENT, RELA);
            entry(elf::DT_RELACOUNT, relative as u64);
        }
        if versioned_libraries != 0 {
            entry(elf::DT_VERSYM, address(VERSYM)?);
            entry(elf::DT_VERNEED, address(VERNEED)?);
            entry(elf::DT_VERNEEDNUM, versioned_libraries as u64);
        }
        entry(elf::DT_NULL, 0);

        let mut note = Vec::with_capacity((NOTE_HEADER + 16) as usize);
        note.put_u32(4);
        note.put_u32(16);
        note.put_u32(elf::NT_GNU_BUILD_ID.0);
        note.extend_from_slice(b"GNU\0");
        note.extend_from_slice(&placed.id);

        Ok(Tables {
            interp: output.interpreter.as_ref().map(|it| [it.as_bytes(), &[0]].concat()),
            note,
            dynsym,
            dynstr: strings.bytes,
            gnu_hash,
            versym,
            verneed,
            rela: Some(rela),
            dynamic,
            eh_frame_hdr: Some(eh_frame_hdr),
        })
    }

    /// The bytes of the synthetic region `name`, moved out: a kept table as the writer keeps
    /// it, any other once wrapped.
    fn take(&mut self, name: &str) -> CowBytes {
        let kept = match name {
            RELA_DYN => self.rela.take(),
            EH_FRAME_HDR => self.eh_frame_hdr.take(),
            _ => None,
        };
        if let Some(bytes) = kept {
            return bytes;
        }
        // alloc: once per region per link, the `Arc` of a table no link keeps
        CowBytes::from_vec(std::mem::take(match name {
            INTERP => return CowBytes::from_vec(self.interp.take().unwrap_or_default()),
            BUILD_ID => &mut self.note,
            DYNSYM => &mut self.dynsym,
            DYNSTR => &mut self.dynstr,
            GNU_HASH => &mut self.gnu_hash,
            VERSYM => &mut self.versym,
            VERNEED => &mut self.verneed,
            DYNAMIC => &mut self.dynamic,
            RELA_DYN | EH_FRAME_HDR => unreachable!("`names` lists the kept table {name} once"),
            _ => unreachable!("a synthetic region `names` doesn't list"),
        }))
    }

    pub(crate) fn regions(mut self, output: &Output) -> Vec<(Class, CowBytes)> {
        // alloc: once per image, what `Writer::synthesize` returns
        names(output).iter().map(|&name| (Class::Synthetic(name), self.take(name))).collect()
    }
}

/// The region of `regions` (in address order) holding `addr`, by its placed size.
fn region_at(regions: &[Region], addr: u64) -> Option<usize> {
    let at = regions.partition_point(|it| it.addr <= addr).checked_sub(1)?;
    (addr < regions[at].addr + regions[at].size.max(regions[at].capacity)).then_some(at)
}

/// Whether the loader may write `at`: the region holding it, or an error naming a text
/// relocation. Code is never one: a dev run's code memory holds the file's bytes at every
/// load address, and no image has the loader write its code.
fn writable(placed: &Placed<'_>, at: u64, what: &str, symbol: Option<&str>) -> Result<usize, LinkError> {
    let regions = placed.regions;
    let region = region_at(regions, at).ok_or_else(|| error(format!("{what} at {at:#x}, in no region of the image")))?; // alloc: error path
    let class = &regions[region].class;
    if !Segment::of(class).loader_writes() {
        return Err(text_relocation(placed, at, what, symbol, class));
    }
    Ok(region)
}

/// The error of a dynamic relocation at `at` in `class`, which the loader can't write: the
/// section, and the symbol it is for (`symbol`, else the function holding `at`).
#[cold]
fn text_relocation(placed: &Placed<'_>, at: u64, what: &str, symbol: Option<&str>, class: &Class) -> LinkError {
    let section = super::image::section_name(class);
    let functions = placed.functions;
    let holding = functions
        .partition_point(|it| it.addr <= at)
        .checked_sub(1)
        .map(|it| &functions[it])
        .filter(|it| at < it.addr + it.size)
        .and_then(|it| it.name.as_deref());
    // alloc: error path
    let of = symbol.or(holding).map_or_else(String::new, |it| format!(" for `{it}`"));
    // alloc: error path
    error(format!("{what}{of} at {at:#x} is in section `{section}`, which the loader can't write (a text relocation)"))
}

/// `.rela.dyn` as [`relocations`] makes it, a kept table that patches the next.
#[derive(Default)]
pub(crate) struct KeptRela {
    synthesis: u64,
    /// The table's bytes, its first `relative` entries the `RELATIVE` run in the order of
    /// [`Placed::rebase`] (kept only when the slid addresses came sorted, so entry `i` is
    /// `rebase[i]`'s): what the last link's state holds too.
    bytes: CowBytes,
    relative: usize,
    /// The binds' and GOT cells' entries a patch builds, reused.
    scratch: Vec<u8>,
    /// How many builds were patches.
    #[cfg(test)]
    pub(crate) patched: u32,
}

/// `.rela.dyn`'s bytes, how many of its entries are `RELATIVE`, and whether another is a
/// `TPOFF64`. With `kept` (an incremental output's) of the synthesis `placed.delta` is from,
/// and every splice of the slid addresses of equal length, the `RELATIVE` run is patched and
/// the binds and GOT cells built; otherwise all of it is built.
fn rela_dyn(writer: &Elf, placed: &Placed<'_>, shared: bool, kept: Option<&mut KeptRela>) -> Result<(CowBytes, usize, bool), LinkError> {
    let tpoff64 = types(writer.output.arch).tpoff64;
    let Some(kept) = kept else {
        let (relocations, relative, _) = relocations(writer, placed, shared)?;
        let tpoff = relocations[relative..].iter().any(|it| it.r_type == tpoff64);
        return Ok((CowBytes::from_vec(rela_bytes(&relocations)), relative, tpoff));
    };
    let last = std::mem::replace(&mut kept.synthesis, 0);
    let patches = placed.delta.filter(|delta| {
        last != 0 && kept.relative == placed.rebase.len() && delta.patches(last, &[delta.rebase])
    });
    if let Some(delta) = patches
        && patch_relative(writer, placed, delta, kept)
    {
        // alloc: once per relink, the binds and GOT cells (a few hundred)
        let mut others = Vec::with_capacity(placed.binds.len() + 2 * placed.got.len());
        push_others(writer, placed, shared, &mut others)?;
        sort_rela(&mut others);
        let head = RELA as usize * kept.relative;
        let scratch = &mut kept.scratch;
        scratch.clear();
        scratch.resize(RELA as usize * others.len(), 0);
        put_relas(&others, scratch);
        kept.bytes.resize(head + scratch.len());
        kept.bytes.update(head, scratch);
        kept.synthesis = placed.synthesis;
        #[cfg(test)]
        {
            kept.patched += 1;
        }
        // clone: two reference counts, the table shared with the state that holds it
        return Ok((kept.bytes.clone(), kept.relative, others.iter().any(|it| it.r_type == tpoff64)));
    }
    let (relocations, relative, in_order) = relocations(writer, placed, shared)?;
    let tpoff = relocations[relative..].iter().any(|it| it.r_type == tpoff64);
    let bytes = CowBytes::from_vec(rela_bytes(&relocations));
    if in_order {
        // clone: two reference counts, the table shared with the state that holds it
        kept.bytes = bytes.clone();
        kept.relative = relative;
        kept.synthesis = placed.synthesis;
    }
    Ok((bytes, relative, tpoff))
}

/// Rewrites the kept `RELATIVE` run for `placed`: the entries of each splice of the slid
/// addresses, and those whose 8 bytes the core wrote (a range of `delta.written`, a local GOT
/// cell spliced), each read as [`relocations`] reads it. False, with the run left half
/// patched, when that is not the table a build makes: a spliced address out of order, or one
/// a build refuses (it gives the error).
fn patch_relative(writer: &Elf, placed: &Placed<'_>, delta: &Delta<'_>, kept: &mut KeptRela) -> bool {
    let rebase = placed.rebase;
    if kept.bytes.len() < RELA as usize * rebase.len() {
        return false;
    }
    // The run was sorted and every entry outside the splices is as it was: it stays sorted
    // (so a build sorts nothing) when each splice is with its neighbours.
    for splice in delta.rebase {
        let (start, end) = (splice.new.start as usize, splice.new.end as usize);
        if end > rebase.len() || !rebase[start.saturating_sub(1)..(end + 1).min(rebase.len())].is_sorted() {
            return false;
        }
    }
    let relative = types(writer.output.arch).relative;
    let bytes = &mut kept.bytes;
    let mut put = |at: usize| match slid(placed, rebase[at], relative) {
        Ok(rela) => {
            bytes.update(RELA as usize * at, &rela.bytes());
            true
        }
        Err(_) => false,
    };
    // The slid addresses whose 8 bytes overlap `addr..addr + len`.
    let overlapping = |addr: u64, len: u64| {
        rebase.partition_point(|&it| it.saturating_add(8) <= addr)..rebase.partition_point(|&it| it < addr.saturating_add(len))
    };
    for splice in delta.rebase {
        if !(splice.new.start as usize..splice.new.end as usize).all(&mut put) {
            return false;
        }
    }
    for &(addr, len) in delta.written {
        if !overlapping(addr, len).all(&mut put) {
            return false;
        }
    }
    for splice in delta.got {
        let cells = placed.got.get(splice.new.start as usize..splice.new.end as usize).unwrap_or_default();
        for cell in cells.iter().filter(|it| matches!(it.holds, Holds::Local(_))) {
            if !overlapping(cell.addr, 8).all(&mut put) {
                return false;
            }
        }
    }
    true
}

/// Each relocation's `Elf64_Rela`, in parallel chunks.
fn put_relas(relocations: &[Rela], out: &mut [u8]) {
    par::for_each(par::zip(par::chunks_mut(out, RELA as usize * CHUNK), par::chunks(relocations, CHUNK)), |(out, chunk)| {
        for (out, it) in out.chunks_exact_mut(RELA as usize).zip(chunk) {
            out.copy_from_slice(&it.bytes());
        }
    });
}

fn rela_bytes(relocations: &[Rela]) -> Vec<u8> {
    // alloc: the output buffer, `.rela.dyn`'s bytes
    let mut rela = vec![0; relocations.len() * RELA as usize];
    put_relas(relocations, &mut rela);
    rela
}

/// The `RELATIVE` relocation of the slid address `at`: its addend the 8 bytes placed there.
fn slid(placed: &Placed<'_>, at: u64, r_type: u32) -> Result<Rela, LinkError> {
    let regions = placed.regions;
    let region = writable(placed, at, "a slid address", None)?;
    let offset = (at - regions[region].addr) as usize;
    let value = placed
        .view(region)
        .u64(offset)
        .ok_or_else(|| error(format!("a slid address at {at:#x}, outside the bytes of its region")))?; // alloc: error path
    Ok(Rela { at, r_type, sym: 0, addend: value as i64 })
}

/// `.rela.dyn`: `RELATIVE` for every slid address (sorted, first, counted by
/// `DT_RELACOUNT`), then what binds to imports and thread-locals, by address; how many are
/// `RELATIVE`; and whether the slid addresses came sorted. One buffer of room for both: the
/// slid addresses, most of the table, are read into it in parallel, and the rest pushed
/// after them and sorted in place.
fn relocations(writer: &Elf, placed: &Placed<'_>, shared: bool) -> Result<(Vec<Rela>, usize, bool), LinkError> {
    let relative_type = types(writer.output.arch).relative;
    let other_cells = placed.binds.len() + 2 * placed.got.len();
    let mut relative = Vec::with_capacity(placed.rebase.len() + other_cells);
    relative.resize(placed.rebase.len(), Rela::default());
    let results: Vec<Result<(), LinkError>> = par::map(
        par::zip(par::chunks_mut(&mut relative, CHUNK), par::chunks(placed.rebase, CHUNK)),
        |(out, ats)| {
            for (out, &at) in out.iter_mut().zip(ats) {
                *out = slid(placed, at, relative_type)?;
            }
            Ok(())
        },
    ); // alloc: once per image, one result per chunk of slid addresses
    first_error(results)?;
    let in_order = sort_rela(&mut relative);
    let count = relative.len();
    push_others(writer, placed, shared, &mut relative)?;
    sort_rela(&mut relative[count..]);
    Ok((relative, count, in_order))
}

/// Onto `other`, unsorted: what binds to imports and thread-locals, the binds then the GOT
/// cells.
fn push_others(writer: &Elf, placed: &Placed<'_>, shared: bool, other: &mut Vec<Rela>) -> Result<(), LinkError> {
    let types = types(writer.output.arch);
    let import = |index: u32| -> Result<u32, LinkError> {
        if index as usize >= placed.imports.len() {
            return Err(error(format!("a dynamic relocation to import {index}, of no import"))); // alloc: error path
        }
        Ok(1 + index)
    };
    for bind in placed.binds {
        let name = placed.imports.get(bind.import as usize).map(|it| it.name.as_str());
        writable(placed, bind.addr, "a bound address", name)?;
        other.push(Rela { at: bind.addr, r_type: types.abs64, sym: import(bind.import)?, addend: bind.addend });
    }
    for cell in placed.got {
        let at = cell.addr;
        writable(placed, at, "a GOT cell", None)?;
        match &cell.holds {
            Holds::Local(_) => {}
            // A shared library's TLS block is placed when it is loaded: the loader adds the
            // block's offset from the thread pointer to the variable's offset in it.
            &Holds::TpOff(tp) if shared => {
                let tls = placed.tls.ok_or_else(|| {
                    // alloc: error path
                    error(format!("the GOT cell at {at:#x} holds a thread pointer offset in an image with no TLS"))
                })?;
                let addend = block_offset(writer.output.arch, tls, tp);
                if addend < 0 || addend as u64 > tls.size {
                    // alloc: error path
                    return Err(error(format!(
                        "the GOT cell at {at:#x} holds thread pointer offset {tp}, outside the TLS template"
                    )));
                }
                other.push(Rela { at, r_type: types.tpoff64, sym: 0, addend });
            }
            Holds::TpOff(_) => {}
            Holds::Import(index) => {
                other.push(Rela { at, r_type: types.glob_dat, sym: import(*index)?, addend: 0 })
            }
            Holds::ImportTpOff(index) => {
                other.push(Rela { at, r_type: types.tpoff64, sym: import(*index)?, addend: 0 })
            }
            Holds::TlsGd { import: index, offset } => {
                let sym = index.map(import).transpose()?.unwrap_or(0);
                other.push(Rela { at, r_type: types.dtpmod64, sym, addend: 0 });
                other.push(Rela { at: at + 8, r_type: types.dtpoff64, sym, addend: *offset as i64 });
            }
            Holds::TlsDesc { import: index, offset } => {
                let sym = index.map(import).transpose()?.unwrap_or(0);
                other.push(Rela { at, r_type: types.tlsdesc, sym, addend: *offset as i64 });
            }
        }
    }
    Ok(())
}

/// The offset in the TLS template of the thread-local at `tp` from the thread pointer: the
/// inverse of [`crate::reloc::tp_offset`].
fn block_offset(arch: Arch, tls: Tls, tp: i64) -> i64 {
    let align = tls.align.max(1);
    match arch {
        Arch::Aarch64 => tp - 16u64.next_multiple_of(align) as i64,
        Arch::X86_64 | Arch::Wasm32 => tp + tls.size.next_multiple_of(align) as i64,
    }
}

const BLOOM_SHIFT: u32 = 26;

pub(super) fn gnu_hash(name: &[u8]) -> u32 {
    name.iter().fold(5381u32, |hash, &it| hash.wrapping_mul(33).wrapping_add(u32::from(it)))
}

/// The SysV ELF hash, of version names in `.gnu.version_r`.
pub(super) fn elf_hash(name: &[u8]) -> u32 {
    let mut hash = 0u32;
    for &it in name {
        hash = (hash << 4).wrapping_add(u32::from(it));
        let high = hash & 0xf000_0000;
        if high != 0 {
            hash ^= high >> 24;
        }
        hash &= !high;
    }
    hash
}

/// lld's shape for `n` exports: a bucket per 4, a bloom filter of 12 bits per symbol in
/// a power of two of 64-bit words.
fn gnu_hash_shape(n: u64) -> (u64, u64) {
    ((n / 4).max(1), (n * 12 / 64 + 1).next_power_of_two())
}

fn gnu_hash_len(n: u64) -> u64 {
    let (buckets, mask_words) = gnu_hash_shape(n);
    16 + 8 * mask_words + 4 * buckets + 4 * n
}

fn rel32(to: u64, from: u64, what: &str) -> Result<u32, LinkError> {
    i32::try_from(to.wrapping_sub(from) as i64)
        .map(|it| it as u32)
        .map_err(|_| error(format!("{what}: {to:#x} is out of 32-bit reach of {from:#x}"))) // alloc: error path
}

/// `.eh_frame_hdr`: the binary search table of every FDE in [`Class::EhFrame`] (the
/// functions' and the native objects') and of an ELF patch's in [`Class::Patch`] ([`outside`]),
/// sorted by the address each covers. Encodings as
/// lld writes them: `pcrel|sdata4` to `.eh_frame`, a `udata4` count, `datarel|sdata4` pairs.
///
/// One pass over the records' headers ([`fdes`]), in parallel pieces split where the core
/// placed the functions' FDEs. With `kept` (an incremental output's) of the synthesis
/// `placed.delta` is from, the table is patched where that is exact ([`patch_hdr`]).
fn eh_frame_hdr(placed: &Placed<'_>, kept: Option<&mut KeptHdr>) -> Result<CowBytes, LinkError> {
    let hdr = placed
        .region(&Class::Synthetic(EH_FRAME_HDR))
        .map(|(_, it)| it.addr)
        .ok_or_else(|| error(format!("the image has no `{EH_FRAME_HDR}` region")))?; // alloc: error path
    let Some(kept) = kept else {
        return Ok(CowBytes::from_vec(build_hdr(placed, hdr)?.0));
    };
    let last = std::mem::replace(&mut kept.synthesis, 0);
    let patches = placed.delta.filter(|delta| last != 0 && delta.patches(last, &[delta.functions]));
    if let Some(delta) = patches
        && patch_hdr(placed, delta, hdr, kept)
    {
        kept.synthesis = placed.synthesis;
        #[cfg(test)]
        {
            kept.patched += 1;
        }
        // clone: two reference counts, the table shared with the state that holds it
        return Ok(kept.bytes.clone());
    }
    let (bytes, table, cies, in_order) = build_hdr(placed, hdr)?;
    let bytes = CowBytes::from_vec(bytes);
    if in_order && fdes_start_records(placed, &table) {
        kept.table = table;
        kept.cies = cies;
        // clone: two reference counts, the table shared with the state that holds it
        kept.bytes = bytes.clone();
        kept.synthesis = placed.synthesis;
    }
    Ok(bytes)
}

/// `.eh_frame_hdr` as [`eh_frame_hdr`] made it, kept to patch the next.
#[derive(Default)]
pub(crate) struct KeptHdr {
    synthesis: u64,
    /// The table as (address covered, FDE), in section order, which was its address order
    /// (kept only then), so sorted both ways.
    table: Vec<(u64, u64)>,
    /// `.eh_frame`'s CIEs as the build's scan met them: a patch walks from a kept FDE.
    cies: Vec<(usize, u8)>,
    /// What the last link's state holds too.
    bytes: CowBytes,
    /// The FDEs of the records walked again, reused.
    scratch: Vec<(u64, u64)>,
    /// How many builds were patches.
    #[cfg(test)]
    pub(crate) patched: u32,
}

/// Whether every FDE a function names in `.eh_frame` is one of `table` (sorted by FDE): then
/// whichever of them [`crate::unwind::fde_splits`] picks starts a record, and a patch that keeps
/// the FDEs where they were keeps the splits a build would refuse none of. Only checked where
/// the build splits at all.
fn fdes_start_records(placed: &Placed<'_>, table: &[(u64, u64)]) -> bool {
    if placed.functions.len() < 2 * CHUNK {
        return true;
    }
    let Some((index, region)) = placed.region(&Class::EhFrame) else { return true };
    let (section, end) = (region.addr, region.addr + placed.len(index) as u64);
    par::all(placed.functions, |it| match it.fde {
        Some(fde) if fde > section && fde < end => table.binary_search_by_key(&fde, |it| it.1).is_ok(),
        _ => true,
    })
}

/// Patches the kept table for `placed`. Each range of `delta.written` in `.eh_frame` has its
/// records walked again, from the last kept FDE at or before it to the first record start
/// past it where a kept FDE starts (or to the section's end or its zero length), and the FDEs
/// found replace those the span had. That is the walk a build makes from there, as the bytes
/// before were as before. False, the kept table left half patched, unless the result is
/// exactly the table a build makes: no CIE in a written range (its FDEs would read
/// differently), the FDEs where they were (so the build's splits stay record starts), the
/// table still sorted (so the build sorts nothing), every entry in reach, and each FDE a
/// spliced function names one of the table.
fn patch_hdr(placed: &Placed<'_>, delta: &Delta<'_>, hdr: u64, kept: &mut KeptHdr) -> bool {
    // An FDE outside `.eh_frame` (an ELF patch's) is listed only by a build.
    let changed = |splice: &crate::model::Splice| {
        placed.functions.get(splice.new.start as usize..splice.new.end as usize).unwrap_or_default()
    };
    if delta.functions.iter().any(|it| changed(it).iter().any(|f| f.fde.is_some_and(|fde| outside(placed, fde)))) {
        return false;
    }
    let Some((index, region)) = placed.region(&Class::EhFrame) else { return true };
    let (bytes, section) = (placed.view(index), region.addr);
    let end = section + bytes.len() as u64;
    let KeptHdr { table, cies, bytes: out, scratch, .. } = kept;
    if out.len() != 12 + 8 * table.len() {
        return false;
    }
    for &(addr, len) in delta.written {
        let (from, to) = (addr.max(section), addr.saturating_add(len).min(end));
        if from >= to {
            continue;
        }
        let (s, e) = ((from - section) as usize, (to - section) as usize);
        let Some(k) = table.partition_point(|it| it.1 <= from).checked_sub(1) else { return false };
        let first = (table[k].1 - section) as usize;
        let mut at = first;
        let j = loop {
            if at >= e
                && let Ok(j) = table.binary_search_by_key(&(section + at as u64), |it| it.1)
            {
                break j;
            }
            if at == bytes.len() {
                break table.len();
            }
            match crate::unwind::entry(bytes, at) {
                Ok(Some((body, next))) => {
                    let cie = bytes.u32(body).is_none_or(|id| id == 0);
                    if at < e && next > s && cie {
                        return false;
                    }
                    at = next;
                }
                Ok(None) if table.last().is_none_or(|it| it.1 < section + at as u64) => break table.len(),
                _ => return false,
            }
        };
        scratch.clear();
        if crate::unwind::rewalk(bytes, section, (first, at), &cies[..], fde_of(section), scratch).is_err() || scratch.len() != j - k {
            return false;
        }
        if scratch.iter().zip(&table[k..j]).any(|(new, old)| new.1 != old.1) {
            return false;
        }
        table[k..j].copy_from_slice(scratch);
        if !table[k.saturating_sub(1)..(j + 1).min(table.len())].is_sorted() {
            return false;
        }
        for (nth, &(pc, fde)) in table[k..j].iter().enumerate() {
            let (Ok(pc), Ok(fde)) = (rel32(pc, hdr, "a function"), rel32(fde, hdr, "an FDE")) else { return false };
            let mut cell = [0u8; 8];
            cell[..4].copy_from_slice(&pc.to_le_bytes());
            cell[4..].copy_from_slice(&fde.to_le_bytes());
            out.update(12 + 8 * (k + nth), &cell);
        }
    }
    delta.functions.iter().all(|splice| {
        let functions = placed.functions.get(splice.new.start as usize..splice.new.end as usize).unwrap_or_default();
        functions.iter().all(|it| match it.fde {
            Some(fde) if fde > section && fde < end => table.binary_search_by_key(&fde, |it| it.1).is_ok(),
            _ => true,
        })
    })
}

/// `.eh_frame_hdr` built from `.eh_frame` and the FDEs outside it ([`outside`]): its bytes,
/// its table, `.eh_frame`'s CIEs, and whether the table came in address order and holds only
/// `.eh_frame`'s FDEs (what [`patch_hdr`] can patch).
#[allow(clippy::type_complexity)]
fn build_hdr(placed: &Placed<'_>, hdr: u64) -> Result<(Vec<u8>, Vec<(u64, u64)>, Vec<(usize, u8)>, bool), LinkError> {
    let eh_frame = placed.region(&Class::EhFrame);
    let (mut table, cies) = match eh_frame {
        Some((index, region)) => {
            let bytes = placed.view(index);
            let starts = crate::unwind::fde_splits(placed, region.addr, bytes.len(), CHUNK);
            let scan = scan_fdes(bytes, region.addr, &starts)
                // alloc: error path
                .map_err(|it| error(format!("the image's .eh_frame at {:#x}: {it}", region.addr)))?;
            (scan.items, scan.cies)
        }
        // alloc: empty, which allocates nothing
        None => (Vec::new(), Vec::new()),
    };
    let inside = table.len();
    table.extend(placed.functions.iter().filter_map(|f| f.fde.filter(|&fde| outside(placed, fde)).map(|fde| (f.addr, fde))));
    let in_order = table.is_sorted();
    if !in_order {
        radix::sort_copy_by_key(&mut table, |&(pc, fde): &(u64, u64)| radix::pack_64(pc, fde));
    }
    let mut bytes = Vec::with_capacity(12 + 8 * table.len());
    bytes.put_u8(1);
    bytes.put_u8(gimli::DW_EH_PE_pcrel.0 | gimli::DW_EH_PE_sdata4.0);
    bytes.put_u8(gimli::DW_EH_PE_udata4.0);
    bytes.put_u8(gimli::DW_EH_PE_datarel.0 | gimli::DW_EH_PE_sdata4.0);
    let eh_frame_addr = eh_frame.map_or(hdr + 4, |(_, it)| it.addr);
    bytes.put_u32(rel32(eh_frame_addr, hdr + 4, ".eh_frame from .eh_frame_hdr")?);
    bytes.put_u32(u32::try_from(table.len()).map_err(|_| error("more FDEs than .eh_frame_hdr counts".into()))?);
    bytes.resize(12 + 8 * table.len(), 0);
    let results: Vec<Result<(), LinkError>> = par::map(
        par::zip(par::chunks_mut(&mut bytes[12..], 8 * CHUNK), par::chunks(&table, CHUNK)),
        |(out, chunk)| {
            for (out, &(pc, fde)) in out.chunks_exact_mut(8).zip(chunk) {
                out[..4].copy_from_slice(&rel32(pc, hdr, "a function from .eh_frame_hdr")?.to_le_bytes());
                out[4..].copy_from_slice(&rel32(fde, hdr, "an FDE from .eh_frame_hdr")?.to_le_bytes());
            }
            Ok(())
        },
    ); // alloc: once per image, one result per chunk of FDEs
    first_error(results)?;
    let unchanged = in_order && table.len() == inside;
    Ok((bytes, table, cies, unchanged))
}

/// Whether `fde` is outside `.eh_frame`: an incremental ELF patch's, placed in
/// [`Class::Patch`] after the patch's code.
fn outside(placed: &Placed<'_>, fde: u64) -> bool {
    placed.region(&Class::EhFrame).is_none_or(|(_, it)| !(it.addr..it.addr + it.size).contains(&fde))
}

/// Every FDE of `.eh_frame` (`bytes`, at address `section`) as (the address it covers from,
/// the address of the FDE), in section order, and its CIEs: [`crate::unwind::scan`], in
/// parallel pieces split at `splits`. Of an FDE only the length, CIE pointer, `pc_begin` and
/// `pc_range` are read, of a CIE only what leads to its pointer encoding; nothing else is
/// checked, so a record malformed past those fields is read where gimli refused it.
fn scan_fdes(bytes: View<'_>, section: u64, splits: &[usize]) -> Result<crate::unwind::Scan<(u64, u64)>, String> {
    crate::unwind::scan(bytes, section, splits, fde_of(section))
}

/// [`scan_fdes`]' table alone.
#[cfg(test)]
fn fdes(bytes: View<'_>, section: u64, splits: &[usize]) -> Result<Vec<(u64, u64)>, String> {
    scan_fdes(bytes, section, splits).map(|it| it.items)
}

/// What the table keeps of a record of `.eh_frame` at `section`: of an FDE, (the address it
/// covers from, its own address).
fn fde_of(section: u64) -> impl Fn(usize, Frame, &mut Vec<(u64, u64)>) -> Result<(), String> + Sync {
    move |at: usize, frame: Frame, table: &mut Vec<(u64, u64)>| {
        if let Frame::Fde { pc, .. } = frame {
            table.push((pc, section + at as u64));
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use blaze_ids::rng::SplitMix64;
    use gimli::UnwindSection;
    use blaze_hash::HashMap;

    use super::fdes;
    use crate::cow::CowBytes;
    use crate::model::{Pages, View};

    /// The walk `eh_frame_hdr` made before: gimli's parse of every record.
    fn reference(bytes: &[u8], section: u64) -> Result<Vec<(u64, u64)>, String> {
        let mut eh_frame = gimli::EhFrame::new(bytes, gimli::LittleEndian);
        eh_frame.set_address_size(8);
        let bases = gimli::BaseAddresses::default().set_eh_frame(section);
        let mut entries = eh_frame.entries(&bases);
        let mut cies = HashMap::default();
        let mut table = Vec::new();
        while let Some(entry) = entries.next().map_err(|it| it.to_string())? {
            match entry {
                gimli::CieOrFde::Cie(cie) => {
                    cies.insert(cie.offset(), cie);
                }
                gimli::CieOrFde::Fde(partial) => {
                    let fde = partial
                        .parse(|section, bases, offset| match cies.get(&offset.0) {
                            Some(cie) => Ok(cie.clone()),
                            None => section.cie_from_offset(bases, offset),
                        })
                        .map_err(|it| it.to_string())?;
                    table.push((fde.initial_address(), section + fde.offset() as u64));
                }
            }
        }
        Ok(table)
    }

    /// The kinds of CIE compilers write: no augmentation (absolute 8-byte pointers), `zR`
    /// with `pcrel|sdata4`, `zPLR` with an indirect personality, `zR` with `udata8`, and
    /// version 3 with `S`.
    #[derive(Clone, Copy)]
    enum Cie {
        Plain,
        Pcrel,
        Personality,
        Udata8,
        Signal,
    }

    impl Cie {
        fn encoding(self) -> Option<u8> {
            match self {
                Cie::Plain => None,
                Cie::Pcrel | Cie::Personality | Cie::Signal => Some(0x1b),
                Cie::Udata8 => Some(0x04),
            }
        }
    }

    fn push_cie(out: &mut Vec<u8>, kind: Cie) -> usize {
        let at = out.len();
        let mut body = vec![0, 0, 0, 0];
        let version = if matches!(kind, Cie::Signal) { 3 } else { 1 };
        body.push(version);
        body.extend_from_slice(match kind {
            Cie::Plain => b"\0".as_slice(),
            Cie::Pcrel | Cie::Udata8 => b"zR\0".as_slice(),
            Cie::Personality => b"zPLR\0".as_slice(),
            Cie::Signal => b"zRS\0".as_slice(),
        });
        blaze_format::leb::write_u64(&mut body, 1);
        body.push(0x78);
        if version == 1 {
            body.push(16);
        } else {
            blaze_format::leb::write_u64(&mut body, 16);
        }
        match kind {
            Cie::Plain => {}
            Cie::Personality => {
                body.push(7);
                body.push(0x9b);
                body.extend_from_slice(&0x1234i32.to_le_bytes());
                body.push(0x1b);
                body.push(0x1b);
            }
            _ => {
                body.push(1);
                body.push(kind.encoding().unwrap());
            }
        }
        while (body.len() + 4) % 8 != 0 {
            body.push(0);
        }
        out.extend_from_slice(&(body.len() as u32).to_le_bytes());
        out.extend_from_slice(&body);
        at
    }

    fn push_fde(out: &mut Vec<u8>, section: u64, cie: usize, kind: Cie, pc: u64, rng: &mut SplitMix64) -> usize {
        let at = out.len();
        let mut body = Vec::new();
        body.extend_from_slice(&((at + 4 - cie) as u32).to_le_bytes());
        match kind.encoding() {
            None => {
                body.extend_from_slice(&pc.to_le_bytes());
                body.extend_from_slice(&0x40u64.to_le_bytes());
            }
            Some(0x04) => {
                body.extend_from_slice(&pc.to_le_bytes());
                body.extend_from_slice(&0x40u64.to_le_bytes());
            }
            Some(_) => {
                let place = section + (at + 8) as u64;
                body.extend_from_slice(&(pc.wrapping_sub(place) as u32).to_le_bytes());
                body.extend_from_slice(&0x40u32.to_le_bytes());
            }
        }
        match kind {
            Cie::Plain => {}
            Cie::Personality => {
                body.push(4);
                body.extend_from_slice(&0x88i32.to_le_bytes());
            }
            _ => body.push(0),
        }
        for _ in 0..rng.next_u64() % 12 {
            body.push(0);
        }
        while (body.len() + 4) % 4 != 0 {
            body.push(0);
        }
        out.extend_from_slice(&(body.len() as u32).to_le_bytes());
        out.extend_from_slice(&body);
        at
    }

    /// A section of `n` FDEs under a few CIEs (some placed after their first FDE's CIE use
    /// moves on), out of address order; and every FDE's start.
    fn section(rng: &mut SplitMix64, n: usize, section: u64) -> (Vec<u8>, Vec<usize>) {
        let kinds = [Cie::Plain, Cie::Pcrel, Cie::Personality, Cie::Udata8, Cie::Signal];
        let mut out = Vec::new();
        let mut cies: Vec<(usize, Cie)> = Vec::new();
        let mut starts = Vec::new();
        for nth in 0..n {
            if cies.is_empty() || rng.next_u64() % 64 == 0 {
                let kind = kinds[(rng.next_u64() % kinds.len() as u64) as usize];
                cies.push((push_cie(&mut out, kind), kind));
            }
            let (cie, kind) = cies[(rng.next_u64() % cies.len() as u64) as usize];
            let pc = 0x10_0000 + (rng.next_u64() % (4 * n as u64 + 1)) * 16 + if nth % 97 == 0 { 0 } else { 1 };
            starts.push(push_fde(&mut out, section, cie, kind, pc, rng));
        }
        (out, starts)
    }

    fn split_at(starts: &[usize], every: usize) -> Vec<usize> {
        starts.iter().copied().filter(|&it| it > 0).step_by(every.max(1)).collect()
    }

    #[test]
    fn the_scan_reads_what_gimli_reads() {
        let mut rng = SplitMix64::new(0x2545_f491_4f6c_dd1d);
        for round in 0..64 {
            let n = 1 + (rng.next_u64() % 600) as usize;
            let base = 0x40_0000 + 0x1000 * round as u64;
            let (mut bytes, starts) = section(&mut rng, n, base);
            if round % 3 == 0 {
                bytes.extend_from_slice(&[0; 4]);
            }
            let want = reference(&bytes, base).unwrap();
            assert_eq!(want.len(), n);
            for every in [0, 1, 7, 64, n] {
                let splits = if every == 0 { Vec::new() } else { split_at(&starts, every) };
                assert_eq!(fdes(View::Slice(&bytes), base, &splits).unwrap(), want, "round {round}, a split every {every}");
            }
        }
    }

    /// The section held in pages, some written since its buffer (so records cross from the
    /// buffer into a page and from page to page), reads as the one slice does.
    #[test]
    fn a_section_in_pages_reads_as_one_slice() {
        let mut rng = SplitMix64::new(0x9e37_79b9_7f4a_7c15);
        for round in 0..8 {
            let n = 1500 + (rng.next_u64() % 1500) as usize;
            let base = 0x50_0000 + 0x1000 * round as u64;
            let (bytes, starts) = section(&mut rng, n, base);
            let mut paged = CowBytes::from_vec(bytes.clone());
            for page in (0..bytes.len().div_ceil(CowBytes::PAGE)).filter(|it| (it + round) % 2 == 0) {
                let at = page * CowBytes::PAGE;
                paged.write(at, &bytes[at..at + 1]);
            }
            assert!(paged.slice(0, bytes.len()).is_none(), "round {round}: the section is in pages");
            let view = View::Pages(Pages::all(&paged));
            let want = fdes(View::Slice(&bytes), base, &[]).unwrap();
            assert_eq!(want.len(), n);
            for every in [0, 1, 7, 64] {
                let splits = if every == 0 { Vec::new() } else { split_at(&starts, every) };
                assert_eq!(fdes(view, base, &splits).unwrap(), want, "round {round}, a split every {every}");
            }
            let short = CowBytes::from_vec(bytes[..bytes.len() - 3].to_vec());
            assert_eq!(
                fdes(View::Pages(Pages::all(&short)), base, &[]).unwrap_err(),
                fdes(View::Slice(&bytes[..bytes.len() - 3]), base, &[]).unwrap_err(),
            );
        }
    }

    #[test]
    fn a_terminator_ends_the_section_in_any_piece() {
        let mut rng = SplitMix64::new(77);
        let base = 0x20_0000;
        let (mut bytes, mut starts) = section(&mut rng, 300, base);
        let cut = starts[150];
        // Everything from FDE 150 on is past a zero length, and starts with a bad record.
        bytes.splice(cut..cut, [0, 0, 0, 0, 0xf0, 0xff, 0xff, 0xff]);
        for it in &mut starts[150..] {
            *it += 8;
        }
        let want = reference(&bytes, base).unwrap();
        assert_eq!(want.len(), 150);
        for every in [1, 10, 149, 151] {
            assert_eq!(fdes(View::Slice(&bytes), base, &split_at(&starts, every)).unwrap(), want, "every {every}");
        }
    }

    #[test]
    fn errors_are_the_first_in_section_order() {
        let mut rng = SplitMix64::new(5);
        let base = 0x30_0000;
        let (bytes, starts) = section(&mut rng, 200, base);
        // A bad CIE version in the first CIE, and a reserved length late in the section.
        let mut bad = bytes.clone();
        bad[8] = 2;
        let late = starts[180];
        bad[late..late + 4].copy_from_slice(&0xffff_fff3u32.to_le_bytes());
        assert!(reference(&bad, base).is_err());
        let serial = fdes(View::Slice(&bad), base, &[]).unwrap_err();
        assert!(serial.contains("offset 0x0"), "{serial}");
        for every in [1, 13, 50] {
            assert_eq!(fdes(View::Slice(&bad), base, &split_at(&starts, every)).unwrap_err(), serial, "every {every}");
        }
        // A split the walk doesn't land on is an error naming where the core placed an FDE.
        let wrong = vec![starts[100] + 4];
        assert!(fdes(View::Slice(&bytes), base, &wrong).unwrap_err().contains("where the core placed an FDE"));
        // A record cut short.
        assert!(fdes(View::Slice(&bytes[..bytes.len() - 3]), base, &[]).is_err());
        assert!(reference(&bytes[..bytes.len() - 3], base).is_err());
    }

    /// An FDE whose CIE pointer names an FDE, or the middle of a record, of an earlier piece
    /// names no CIE: the one walk's error, whichever piece reads it.
    #[test]
    fn a_cie_pointer_to_no_cie_is_one_error_in_any_piece() {
        let mut rng = SplitMix64::new(91);
        let base = 0x60_0000;
        let (bytes, starts) = section(&mut rng, 300, base);
        let at = starts[250];
        for target in [starts[10], starts[10] + 4] {
            let mut bad = bytes.clone();
            bad[at + 4..at + 8].copy_from_slice(&((at + 4 - target) as u32).to_le_bytes());
            let serial = fdes(View::Slice(&bad), base, &[]).unwrap_err();
            assert!(serial.contains(&format!("offset {at:#x}: its CIE pointer names no CIE")), "{serial}");
            for every in [1, 7, 64, 200] {
                assert_eq!(fdes(View::Slice(&bad), base, &split_at(&starts, every)).unwrap_err(), serial, "every {every}");
            }
        }
    }
}
