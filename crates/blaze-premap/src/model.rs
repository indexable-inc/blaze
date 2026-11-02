//! A model linker for the tests: it places pieces as blaze-link's layout would (code, then
//! read-only data and the thread-local template, then data and the open cells, each on its
//! own page), and links them three ways: as the image ([`Model`], the [`Prelink`] hook), as
//! the workspace executable against an image, and whole. Placement is shared, so what the
//! tests compare is what premap changes: how references resolve, through the image's table,
//! its binds and the run's start, against the whole link's one symbol space. The workspace
//! executable carries the image's init pieces and thread-local part for the system loader, as
//! blaze-link's link beside an image must: [`split`] runs them as that loader does.

use std::collections::BTreeMap;

use blaze_codegen::{CompiledFunction, Init, InstanceKey, RelocKind, Static, Target};
use blaze_hash::HashMap;
use blaze_ids::ContentId;
use blaze_items::ItemKey;
use blaze_link::NativeKind;

use crate::PremapError;
use crate::image::View;
use crate::prelink::{
    Bind, BindKind, InitEntry, Library, PlacedName, PlacedStatic, Prelink, Prelinked, Prot,
    Request, Segment, StaticAt, Tls,
};
use crate::split::WorkspacePart;
use crate::start::{self, Start};
use crate::table::{Def, Kind, Strength, SymbolId};

/// Every host's mapping granule divides it, so the tests' images map on every host.
pub(crate) const PAGE: u64 = 64 << 10;
/// Where the model's executable keeps the image's thread-locals from the thread pointer.
pub(crate) const TLS_BIAS: i64 = -(64 << 10);

/// Where the workspace's thread-locals start in the executable's block: after the image's,
/// at their alignment, as blaze-link's link beside an image puts them.
fn tls_from(image: Option<&Tls>, align: u64) -> u64 {
    image.map_or(0, |it| it.size.next_multiple_of(align.max(1)))
}

/// The shared libraries of the model's run: name and the address it finds.
pub(crate) type Libraries<'a> = &'a [(&'a str, u64)];

struct Site {
    at: u64,
    kind: RelocKind,
    target: Target,
    addend: i64,
}

pub(crate) struct Layout {
    pub functions: Vec<(InstanceKey, u64, u64)>,
    pub statics: Vec<PlacedStatic>,
    data: HashMap<ContentId, u64>,
    /// The open cells: name, address, referred to weakly only.
    pub cells: Vec<(String, u64, bool)>,
    sites: Vec<Site>,
    pub segments: Vec<Segment>,
    pub tls: Option<Tls>,
    pub init: Vec<InitEntry>,
    pub end: u64,
    /// Where its thread-locals start in the run's area.
    pub tls_from: u64,
}

fn pad(bytes: &mut Vec<u8>, align: u64) {
    bytes.resize((bytes.len() as u64).next_multiple_of(align.max(1)) as usize, 0);
}

fn tls_align(statics: &[&Static]) -> u64 {
    statics.iter().filter(|it| it.thread_local).map(|it| u64::from(it.align)).max().unwrap_or(1)
}

/// Places `functions` and `statics` from `from`, their thread-locals from `tls_from` in the
/// run's area.
pub(crate) fn place(from: u64, tls_from: u64, functions: &[&CompiledFunction], statics: &[&Static]) -> Layout {
    let mut sites = Vec::new();
    let mut placed = Vec::new();
    let mut code = Vec::new();
    for function in functions {
        pad(&mut code, u64::from(function.align));
        let addr = from + code.len() as u64;
        code.extend_from_slice(&function.code);
        placed.push((function.key, addr, function.code.len() as u64));
        for reloc in &function.relocs {
            sites.push(Site { at: addr + u64::from(reloc.at), kind: reloc.kind, target: reloc.target.clone(), addend: reloc.addend });
        }
    }

    let ro_at = (from + code.len() as u64).next_multiple_of(PAGE);
    let mut ro = Vec::new();
    let mut data = HashMap::default();
    let blobs = functions.iter().flat_map(|it| &it.data).chain(statics.iter().flat_map(|it| &it.data));
    for blob in blobs {
        if data.contains_key(&blob.id) {
            continue;
        }
        pad(&mut ro, u64::from(blob.align));
        let addr = ro_at + ro.len() as u64;
        ro.extend_from_slice(&blob.bytes);
        data.insert(blob.id, addr);
        for reloc in &blob.relocs {
            sites.push(Site { at: addr + u64::from(reloc.at), kind: reloc.kind, target: reloc.target.clone(), addend: reloc.addend });
        }
    }

    let mut out_statics = Vec::new();
    let align = tls_align(statics);
    let mut template = Vec::new();
    for value in statics.iter().filter(|it| it.thread_local) {
        pad(&mut template, u64::from(value.align));
        let offset = tls_from + template.len() as u64;
        let size = match &value.init {
            Init::Bytes { bytes, relocs } => {
                assert!(relocs.is_empty(), "the model places no thread-local holding an address");
                template.extend_from_slice(bytes);
                bytes.len() as u64
            }
            Init::Zeroed { size } => {
                template.resize(template.len() + *size as usize, 0);
                *size
            }
        };
        let at = StaticAt::ThreadLocal(offset.wrapping_add_signed(TLS_BIAS));
        out_statics.push(PlacedStatic { item: value.item, instance: value.instance, at, size });
    }
    let tls = (!template.is_empty()).then(|| {
        pad(&mut ro, align);
        let at = ro_at + ro.len() as u64;
        ro.extend_from_slice(&template);
        Tls { template: at, init: template.len() as u64, size: template.len() as u64, align }
    });

    let rw_at = (ro_at + ro.len() as u64).next_multiple_of(PAGE);
    let mut rw = Vec::new();
    let mut init = Vec::new();
    for value in statics.iter().filter(|it| !it.thread_local) {
        pad(&mut rw, u64::from(value.align));
        let addr = rw_at + rw.len() as u64;
        let size = match &value.init {
            Init::Bytes { bytes, relocs } => {
                rw.extend_from_slice(bytes);
                for reloc in relocs {
                    sites.push(Site { at: addr + u64::from(reloc.at), kind: reloc.kind, target: reloc.target.clone(), addend: reloc.addend });
                }
                bytes.len() as u64
            }
            Init::Zeroed { size } => {
                rw.resize(rw.len() + *size as usize, 0);
                *size
            }
        };
        if value.section.as_deref() == Some(".init_array") {
            let tie = InitEntry::tie_of(&value.item, &value.instance);
            init.push(InitEntry { rank: 65536, order: [u32::MAX; 3], tie, at: addr, count: (size / 8) as u32 });
        }
        out_statics.push(PlacedStatic { item: value.item, instance: value.instance, at: StaticAt::Addr(addr), size });
    }
    let mut cells: Vec<(String, u64, bool)> = Vec::new();
    for site in &sites {
        let (name, weak) = match &site.target {
            Target::Extern(name) => (name, false),
            Target::Weak(name) => (name, true),
            _ => continue,
        };
        match cells.iter_mut().find(|it| it.0 == *name) {
            Some(cell) => cell.2 &= weak,
            None => {
                pad(&mut rw, 8);
                cells.push((name.clone(), rw_at + rw.len() as u64, weak));
                rw.extend_from_slice(&[0; 8]);
            }
        }
    }

    let mut segments = Vec::new();
    let mut end = from;
    for (addr, bytes, prot) in [(from, code, Prot::Code), (ro_at, ro, Prot::ReadOnly), (rw_at, rw, Prot::Data)] {
        if !bytes.is_empty() {
            end = addr + bytes.len() as u64;
            segments.push(Segment { addr, size: bytes.len() as u64, prot, bytes });
        }
    }
    Layout { functions: placed, statics: out_statics, data, cells, sites, segments, tls, init, end, tls_from }
}

impl Layout {
    fn local(&self, target: &Target) -> Option<(u64, Kind)> {
        match target {
            Target::Function(key) | Target::Direct(key) => {
                self.functions.iter().find(|it| it.0 == *key).map(|it| (it.1, Kind::Function))
            }
            Target::Static { item, instance } => {
                self.statics.iter().find(|it| it.item == *item && it.instance == *instance).map(|it| match it.at {
                    StaticAt::Addr(addr) => (addr, Kind::Data),
                    StaticAt::ThreadLocal(offset) => (offset, Kind::ThreadLocal),
                })
            }
            Target::Data(id) => self.data.get(id).map(|it| (*it, Kind::Data)),
            Target::Extern(name) | Target::Weak(name) => {
                self.cells.iter().find(|it| it.0 == *name).map(|it| (it.1, Kind::Data))
            }
            // The model places no SanitizerCoverage arrays or tier counter cells: a site reaching
            // one is `apply`'s "reaches no" error.
            Target::Position(_) | Target::Coverage(_) | Target::Counter(_) | Target::ProfileCounters => None,
        }
    }

    /// Writes every site with what `resolve` gives its target; open names reach their cell.
    fn apply(&mut self, resolve: &impl Fn(&Target) -> Option<u64>) -> Result<(), PremapError> {
        for site in &self.sites {
            let value = match &site.target {
                Target::Extern(_) | Target::Weak(_) => self.local(&site.target).map(|it| it.0),
                target => resolve(target),
            }
            .ok_or_else(|| PremapError::new(format!("the model link reaches no {:?}", site.target)))?;
            let value = value.wrapping_add_signed(site.addend);
            let field: Vec<u8> = match site.kind {
                RelocKind::Abs64 => value.to_le_bytes().to_vec(),
                RelocKind::PcRel32 => (value.wrapping_sub(site.at + 4) as i32).to_le_bytes().to_vec(),
                RelocKind::TlsOffset => (value as i32).to_le_bytes().to_vec(),
                kind => panic!("the model writes no {kind:?}"),
            };
            write(&mut self.segments, site.at, &field);
        }
        Ok(())
    }

    fn names(&self, names: &[&(String, Target)]) -> Result<Vec<(String, Def)>, PremapError> {
        let mut out = Vec::with_capacity(names.len());
        for (name, target) in names {
            let (value, kind) = self
                .local(target)
                .ok_or_else(|| PremapError::new(format!("the model link defines `{name}` as {target:?}, not placed")))?;
            out.push((name.clone(), Def { value, size: 0, kind, strength: Strength::Strong, order: u32::MAX }));
        }
        out.sort_by(|a, b| a.0.cmp(&b.0));
        Ok(out)
    }
}

fn write(segments: &mut [Segment], at: u64, bytes: &[u8]) {
    let segment = segments
        .iter_mut()
        .find(|it| at >= it.addr && at + bytes.len() as u64 <= it.addr + it.size)
        .expect("a site inside its segments");
    let offset = (at - segment.addr) as usize;
    segment.bytes[offset..offset + bytes.len()].copy_from_slice(bytes);
}

fn libraries(natives: &[blaze_link::Native<'_>]) -> Vec<Library> {
    natives
        .iter()
        .filter_map(|it| match it.kind {
            NativeKind::Shared { as_needed } => Some(Library { name: it.name.to_owned(), order: it.order, as_needed }),
            _ => None,
        })
        .collect()
}

/// The model as blaze-link's prelink.
pub(crate) struct Model;

impl Prelink for Model {
    type Placed = Prelinked;

    fn prelink(&self, request: &Request<'_>) -> Result<Prelinked, PremapError> {
        assert_eq!(request.link.tls_bias, TLS_BIAS, "the model's executable keeps the image's thread-locals at its bias");
        let mut layout = place(request.link.base, 0, request.link.functions, request.link.statics);
        let names = layout.names(request.link.names)?;
        let local = |target: &Target| layout.local(target).map(|it| it.0);
        let resolved: Vec<_> = layout.sites.iter().map(|it| (it.target.clone(), local(&it.target))).collect();
        layout.apply(&|target| resolved.iter().find(|it| it.0 == *target).and_then(|it| it.1))?;
        let mut binds = Vec::new();
        for (name, cell, weak) in layout.cells.clone() {
            match names.iter().find(|it| it.0 == name) {
                Some((_, def)) => write(&mut layout.segments, cell, &def.value.to_le_bytes()),
                None => binds.push(Bind { place: cell, name, addend: 0, kind: BindKind::Addr, weak }),
            }
        }
        Ok(Prelinked {
            segments: layout.segments,
            functions: layout.functions,
            statics: layout.statics,
            names: names.into_iter().map(|(name, def)| PlacedName { name, def }).collect(),
            binds,
            libraries: libraries(request.link.natives),
            init: layout.init,
            fini: Vec::new(),
            tls: layout.tls,
            unwind: Vec::new(),
            thread_vars: Vec::new(),
            reached_directly: Vec::new(),
        })
    }
}

/// A run's memory: segments by address, with the start's writes made.
pub(crate) type Memory = BTreeMap<u64, Vec<u8>>;

/// What the tests compare of a link: where every symbol is, the bytes of every segment as
/// the program's entry sees them, the init functions in the order they run, the
/// thread-local area's parts.
#[derive(PartialEq, Eq, Debug)]
pub(crate) struct Run {
    pub functions: BTreeMap<InstanceKey, u64>,
    pub statics: BTreeMap<(ItemKey, ContentId), u64>,
    pub memory: Memory,
    pub init: Vec<u64>,
    pub tls: Vec<(u64, u64, u64)>,
}

fn read(memory: &Memory, at: u64) -> u64 {
    let (addr, bytes) = memory.range(..=at).next_back().expect("an address inside memory");
    let offset = (at - addr) as usize;
    u64::from_le_bytes(bytes[offset..offset + 8].try_into().expect("8 bytes"))
}

fn poke(memory: &mut Memory, at: u64, value: u64) {
    let (addr, bytes) = memory.range_mut(..=at).next_back().expect("an address inside memory");
    let offset = (at - *addr) as usize;
    bytes[offset..offset + 8].copy_from_slice(&value.to_le_bytes());
}

fn library(libraries: Libraries<'_>, name: &str, weak: bool) -> Result<u64, PremapError> {
    match libraries.iter().find(|it| it.0 == name) {
        Some(it) => Ok(it.1),
        None if weak => Ok(0),
        None => Err(PremapError::new(format!("no library of the model's run defines `{name}`"))),
    }
}

/// The whole link of the image's pieces then the workspace's, in one symbol space.
pub(crate) fn whole(
    image: (&[&CompiledFunction], &[&Static], &[&(String, Target)]),
    workspace: (&[&CompiledFunction], &[&Static], &[&(String, Target)]),
    base: u64,
    libraries: Libraries<'_>,
) -> Result<Run, PremapError> {
    let mut first = place(base, 0, image.0, image.1);
    let tls_from = tls_from(first.tls.as_ref(), tls_align(workspace.1));
    let mut second = place(first.end.next_multiple_of(PAGE), tls_from, workspace.0, workspace.1);
    let mut names = first.names(image.2)?;
    names.extend(second.names(workspace.2)?);
    let resolved: Vec<(Target, Option<u64>)> = first
        .sites
        .iter()
        .chain(&second.sites)
        .map(|it| (it.target.clone(), first.local(&it.target).or_else(|| second.local(&it.target)).map(|it| it.0)))
        .collect();
    let resolve = |target: &Target| resolved.iter().find(|it| it.0 == *target).and_then(|it| it.1);
    first.apply(&resolve)?;
    second.apply(&resolve)?;
    let mut memory = Memory::new();
    for layout in [&first, &second] {
        memory.extend(layout.segments.iter().map(|it| (it.addr, it.bytes.clone())));
    }
    for (name, cell, weak) in first.cells.iter().chain(&second.cells) {
        let value = match names.iter().find(|it| it.0 == *name) {
            Some((_, def)) => def.value,
            None => library(libraries, name, *weak)?,
        };
        poke(&mut memory, *cell, value);
    }
    let mut init: Vec<&InitEntry> = first.init.iter().chain(&second.init).collect();
    init.sort_by_key(|it| it.sort_key());
    let init = init.iter().flat_map(|it| (0..u64::from(it.count)).map(|n| read(&memory, it.at + n * 8))).collect();
    let tls = [&first, &second]
        .iter()
        .zip([0, tls_from])
        .filter_map(|(layout, offset)| layout.tls.map(|it| (it.template, it.size, offset)))
        .collect();
    Ok(run(&[&first, &second], memory, init, tls))
}

fn run(layouts: &[&Layout], memory: Memory, init: Vec<u64>, tls: Vec<(u64, u64, u64)>) -> Run {
    let mut functions = BTreeMap::new();
    let mut statics = BTreeMap::new();
    for layout in layouts {
        functions.extend(layout.functions.iter().map(|it| (it.0, it.1)));
        statics.extend(layout.statics.iter().map(|it| {
            let at = match it.at {
                StaticAt::Addr(addr) | StaticAt::ThreadLocal(addr) => addr,
            };
            ((it.item, it.instance), at)
        }));
    }
    Run { functions, statics, memory, init, tls }
}

/// The workspace link against `image`, and the run's start for it.
pub(crate) fn link_workspace(part: &WorkspacePart<'_>, image: &View<'_>) -> Result<Layout, PremapError> {
    let tls_from = tls_from(image.tls().as_ref(), tls_align(&part.statics));
    let mut layout = place(image.end().next_multiple_of(PAGE), tls_from, &part.functions, &part.statics);
    let table = image.table();
    let resolved: Vec<(Target, Option<u64>)> = layout
        .sites
        .iter()
        .map(|it| {
            let outside = match &it.target {
                Target::Function(key) | Target::Direct(key) => table.get(&SymbolId::function(key)),
                Target::Static { item, instance } => table.get(&SymbolId::of_static(item, instance)),
                _ => None,
            };
            (it.target.clone(), layout.local(&it.target).map(|it| it.0).or(outside.map(|it| it.value)))
        })
        .collect();
    layout.apply(&|target| resolved.iter().find(|it| it.0 == *target).and_then(|it| it.1))?;
    Ok(layout)
}

/// The run's start for `image` beside the workspace executable whose names are `names`.
pub(crate) fn start_of<'v>(image: &View<'v>, names: &[(String, Def)]) -> Result<Start<'v>, PremapError> {
    start::plan(image, names)
}

/// The names the workspace link defines, sorted, as its layout placed them.
pub(crate) fn workspace_names(layout: &Layout, part: &WorkspacePart<'_>) -> Result<Vec<(String, Def)>, PremapError> {
    layout.names(&part.names)
}

/// The split run: the image's segments and the workspace's, the workspace link's open cells
/// bound against its names, then the image's, then the libraries, the start's writes and
/// lookups made. The executable carries both sides' init pieces, which the system loader runs
/// in the whole link's order, and its thread-local block holds the image's part, then the
/// workspace's. Symbols are found where a run finds them: the workspace link's, then the
/// image's table, for every function and static of `keys`.
pub(crate) fn split(
    image: &View<'_>,
    layout: &Layout,
    names: &[(String, Def)],
    start: &Start<'_>,
    libraries: Libraries<'_>,
    keys: (&[InstanceKey], &[(ItemKey, ContentId)]),
) -> Result<Run, PremapError> {
    let mut memory = Memory::new();
    memory.extend(image.segments().map(|it| {
        let mut bytes = it.bytes.to_vec();
        bytes.resize(it.size as usize, 0);
        (it.addr, bytes)
    }));
    memory.extend(layout.segments.iter().map(|it| (it.addr, it.bytes.clone())));
    for (name, cell, weak) in &layout.cells {
        let value = match names.iter().find(|it| it.0 == *name) {
            Some((_, def)) => def.value,
            None => match image.get(&SymbolId::name(name)) {
                Some(def) => def.value,
                None => library(libraries, name, *weak)?,
            },
        };
        poke(&mut memory, *cell, value);
    }
    for write in &start.writes {
        poke(&mut memory, write.place, write.value);
    }
    for lookup in &start.lookups {
        let value = library(libraries, lookup.name, lookup.weak)?;
        poke(&mut memory, lookup.place, if value == 0 { 0 } else { value.wrapping_add_signed(lookup.addend) });
    }
    let mut pieces: Vec<InitEntry> = image.init().chain(layout.init.iter().copied()).collect();
    pieces.sort_by_key(|it| it.sort_key());
    let init = pieces.iter().flat_map(|it| (0..u64::from(it.count)).map(|n| read(&memory, it.at + n * 8))).collect();
    let tls = [(image.tls(), 0), (layout.tls, layout.tls_from)]
        .into_iter()
        .filter_map(|(tls, offset)| tls.map(|it| (it.template, it.size, offset)))
        .collect();

    let table = image.table();
    let mut functions = BTreeMap::new();
    for key in keys.0 {
        let at = match layout.local(&Target::Function(*key)) {
            Some((at, _)) => at,
            None => table
                .get(&SymbolId::function(key))
                .ok_or_else(|| PremapError::new(format!("the split run has no {key:?}")))?
                .value,
        };
        functions.insert(*key, at);
    }
    let mut statics = BTreeMap::new();
    for &(item, instance) in keys.1 {
        let at = match layout.local(&Target::Static { item, instance }) {
            Some((at, _)) => at,
            None => table
                .get(&SymbolId::of_static(&item, &instance))
                .ok_or_else(|| PremapError::new(format!("the split run has no static {item:?}")))?
                .value,
        };
        statics.insert((item, instance), at);
    }
    Ok(Run { functions, statics, memory, init, tls })
}
