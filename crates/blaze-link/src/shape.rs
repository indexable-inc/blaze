//! The shape of a laid-out image: each region's section, class, address, bytes placed and
//! room, in address order, and its code summed by backend and by crate
//! ([`blaze_codegen::Made`]).
//!
//! A stage says it once, after its last placement round and before anything is checked or
//! written ([`Stage::record_shape`]), as `tracing` events of target `blaze`, which the
//! profile's `events` table keeps (docs/profiling.md):
//! - `link region`: one a region, its fields `image generation full index addr size capacity
//!   section class` (`blaze explain link_regions`, with the gap before each);
//! - `link code`: the pieces of the code regions ([`code`]: the code, the patch region, the
//!   jump table's and PLT stubs, executable named sections) summed `by=backend` (stencil,
//!   Cranelift, LLVM at its level, `native` objects, the `linker`'s thunks and stubs, and
//!   `unsaid` for a function the change said nothing of) and `by=crate` (the [`TOP_CRATES`]
//!   largest, then the rest as one row), each with `pieces` and `bytes`, and `what` it is
//!   last (`blaze explain link_code`). A full link sums every live piece of its code; an
//!   incremental stage only those it placed.
//!
//! Every refusal about size or reach shows the same regions inline ([`render`]): an
//! [`crate::OutOfReach`] with its two ends marked, and a [`Refusal::NoRoom`] or
//! [`Refusal::FieldRange`] ([`explained`]), so one failed link says what fills the image.
//!
//! The cost is one event a region, and a few a stage for the code, from the regions the layout
//! has and the sizes of the code's pieces: no byte is read. A full link's walk of its live
//! pieces is a few milliseconds a million pieces, two map updates a piece of code. Every list is in address order, or
//! sorted by bytes then name, so one image always says the same (rule 25).

use std::alloc::Global;
use std::borrow::Cow;
use std::fmt::Write as _;

use blaze_codegen::Backend;
use blaze_codegen::plan::Origin;
use blaze_hash::HashMap;

use crate::model::{Class, Writer};
use crate::stage::Stage;
use crate::state::{PieceRef, What};
use crate::{LinkError, Refusal};

/// The crates a stage's `link code` events name one by one, the largest first: the rest are
/// one row.
const TOP_CRATES: usize = 16;

/// A placed region as the image's shape has it: what a refusal about size or reach shows
/// ([`crate::OutOfReach::layout`]), and a `link region` event says.
#[derive(Clone, PartialEq, Eq, Debug)]
pub struct RegionShape {
    /// The section, as the format names it (`__TEXT,__text`, `.eh_frame`).
    pub section: String,
    /// The linker's class of it (`Text`, `Patch`, `EhFrame`).
    pub class: String,
    pub addr: u64,
    /// The bytes placed in it, from `addr`.
    pub size: u64,
    /// The address range it keeps from `addr`: what was placed and its room to grow.
    pub capacity: u64,
}

impl RegionShape {
    /// Where its range ends: past its room, or past what was placed when that is more.
    pub fn end(&self) -> u64 {
        self.addr.saturating_add(self.size.max(self.capacity))
    }
}

/// `layout` as lines of text into `out`, by address: each region's range, bytes placed and
/// room, section and class, a line for each gap between two, and each of `marks` (an address
/// and what it is) after the region it is in, or on a line of its own when it is in none.
pub(crate) fn render(layout: &[RegionShape], base: u64, marks: &[(u64, &str)], out: &mut String) {
    // Writing to a `String` does not fail.
    _ = writeln!(
        out,
        "the image's regions by address, from its base {base:#x} (size: bytes placed; room: the range kept):"
    );
    // The headers before the first region are no gap.
    let mut end = layout.first().map_or(base, |it| it.addr);
    for region in layout {
        if region.addr > end {
            _ = writeln!(out, "    {:#x} bytes unused", region.addr - end);
        }
        _ = write!(
            out,
            "  {:#014x}..{:#014x}  size {:#x}  room {:#x}  {} ({})",
            region.addr,
            region.end(),
            region.size,
            region.capacity,
            region.section,
            region.class,
        );
        for (addr, what) in marks {
            if (region.addr..region.end()).contains(addr) {
                _ = write!(out, "  <- {what} {addr:#x}");
            }
        }
        out.push('\n');
        end = end.max(region.end());
    }
    for (addr, what) in marks {
        if !layout.iter().any(|it| (it.addr..it.end()).contains(addr)) {
            _ = writeln!(out, "  {what} {addr:#x} is in no region (the headers, or a gap)");
        }
    }
}

/// `error` with the image's regions after its message, when it is about size or reach: a
/// [`Refusal::NoRoom`] or a [`Refusal::FieldRange`]. Any other is as it was; an
/// [`crate::OutOfReach`] shows them already.
pub(crate) fn explained(mut error: LinkError, layout: &[RegionShape], base: u64) -> LinkError {
    if matches!(error.refusal, Some(Refusal::NoRoom { .. } | Refusal::FieldRange)) {
        error.what.push('\n');
        render(layout, base, &[], &mut error.what);
    }
    error
}

/// What made a piece of code, as `link code by=backend` names it.
#[derive(Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord, Debug)]
enum Kind {
    Stencil,
    Cranelift {
        speed: bool,
    },
    Llvm {
        opt: u8,
    },
    /// A native object's code (C, assembly, `global_asm!`).
    Native,
    /// The linker's own among the code: thunks, PLT and jump-table stubs, and the call frame
    /// records an ELF patch region holds.
    Linker,
    /// A function the change said nothing of ([`crate::Change::made`]).
    Unsaid,
}

impl Kind {
    fn of(backend: Backend) -> Kind {
        match backend {
            Backend::Stencil => Kind::Stencil,
            Backend::Cranelift { speed } => Kind::Cranelift { speed },
            Backend::Llvm { opt } => Kind::Llvm { opt },
        }
    }

    fn name(self) -> Cow<'static, str> {
        Cow::Borrowed(match self {
            Kind::Stencil => "stencil",
            Kind::Cranelift { speed: true } => "cranelift speed",
            Kind::Cranelift { speed: false } => "cranelift none",
            // alloc: once a stage, a backend's name
            Kind::Llvm { opt } => return Cow::Owned(format!("llvm O{opt}")),
            Kind::Native => "native",
            Kind::Linker => "linker",
            Kind::Unsaid => "unsaid",
        })
    }
}

/// Whose code a piece is, as `link code by=crate` names it.
#[derive(Clone, Copy, PartialEq, Eq, Hash, Debug)]
enum Owner {
    Origin(Origin),
    Native,
    Linker,
    Unsaid,
}

impl Owner {
    fn name(self) -> Cow<'static, str> {
        Cow::Borrowed(match self {
            Owner::Origin(Origin::Member(it) | Origin::Dependency(it)) => it.as_str(),
            // alloc: once a stage, a crate's name
            Owner::Origin(Origin::BuildTime(it)) => {
                return Cow::Owned(format!("{} (build time)", it.as_str()));
            }
            Owner::Origin(Origin::Std) => "std",
            Owner::Native => "(native objects)",
            Owner::Linker => "(linker)",
            Owner::Unsaid => "(unsaid)",
        })
    }
}

/// Pieces of code summed: how many, and their bytes.
#[derive(Clone, Copy, Default, Debug)]
struct Sum {
    pieces: u64,
    bytes: u64,
}

impl Sum {
    fn add(&mut self, bytes: u64) {
        self.pieces += 1;
        self.bytes += bytes;
    }
}

/// Whether a class holds code: what `link code` sums.
fn code(class: &Class) -> bool {
    matches!(
        class,
        Class::Text | Class::Patch | Class::Stubs | Class::Plt | Class::Named { exec: true, .. }
    )
}

/// `sums` as rows by name, the most bytes first, then by name: one order whatever the map's.
fn ranked<K>(
    sums: HashMap<K, Sum, Global>,
    name: impl Fn(K) -> Cow<'static, str>,
) -> Vec<(Cow<'static, str>, Sum)> {
    // alloc: once a stage, a row a backend or crate
    let mut rows: Vec<(Cow<'static, str>, Sum)> =
        sums.into_iter().map(|(key, sum)| (name(key), sum)).collect();
    rows.sort_unstable_by(|a, b| {
        b.1.bytes.cmp(&a.1.bytes).then_with(|| a.0.cmp(&b.0)).then(a.1.pieces.cmp(&b.1.pieces))
    });
    rows
}

impl Stage<'_> {
    /// The image's regions as laid out now, by address ([`RegionShape`]).
    pub(crate) fn shape(&self) -> Vec<RegionShape> {
        // alloc: once a stage, a row a region (a few dozen), which a refusal keeps
        let mut out: Vec<RegionShape> = Vec::with_capacity(self.new.spaces.len());
        for space in self.new.spaces.iter() {
            let region = &space.region;
            out.push(RegionShape {
                section: self.writer.section_name(&region.class),
                // alloc: as above, the class's name
                class: format!("{:?}", region.class),
                addr: region.addr,
                size: region.size,
                capacity: region.capacity,
            });
        }
        // Stable: two regions at one address (an empty one) stay in the writer's order.
        out.sort_by_key(|it| it.addr);
        out
    }

    /// `error` with the image's regions as laid out now, when it is about size or reach
    /// ([`explained`]): a placement round's refusal for want of room.
    pub(crate) fn explained(&self, error: LinkError) -> LinkError {
        if !matches!(error.refusal, Some(Refusal::NoRoom { .. } | Refusal::FieldRange)) {
            return error;
        }
        explained(error, &self.shape(), self.plan.base)
    }

    /// Says the image's shape as laid out: a `link region` event a region, and its code by
    /// backend and by crate (`link code`), when a subscriber keeps `blaze` events. The regions
    /// are returned whether said or not, for the refusals that show them.
    pub(crate) fn record_shape(&self) -> Vec<RegionShape> {
        let layout = self.shape();
        // An event's: the recorder keeps events and spans, and refuses a bare `enabled!` hint.
        if !tracing::event_enabled!(target: "blaze", tracing::Level::INFO) {
            return layout;
        }
        let (image, generation, full) = (self.name, self.new.generation, self.full);
        for (index, it) in layout.iter().enumerate() {
            tracing::info!(
                target: "blaze",
                image,
                generation,
                full,
                index,
                addr = it.addr,
                size = it.size,
                capacity = it.capacity,
                section = it.section.as_str(),
                class = it.class.as_str(),
                "link region",
            );
        }
        self.record_code();
        layout
    }

    /// The `link code` events: the code regions' pieces summed by backend and by crate, from
    /// each piece's size and its function's [`blaze_codegen::Made`]. A full link's every
    /// piece of code; an incremental stage's, only those it made.
    fn record_code(&self) {
        // alloc: once a stage, a few backends
        let mut backends: HashMap<Kind, Sum, Global> = HashMap::new_in(Global);
        // alloc: once a stage, an entry a crate
        let mut crates: HashMap<Owner, Sum, Global> = HashMap::new_in(Global);
        let mut add = |piece: PieceRef<'_>| {
            if !code(piece.class()) {
                return;
            }
            let (kind, owner) = match piece.what() {
                What::Function(id) => match self.new.made(self.new.function(id)) {
                    Some(made) => (Kind::of(made.backend), Owner::Origin(made.origin)),
                    None => (Kind::Unsaid, Owner::Unsaid),
                },
                What::Native { .. } | What::NativeFrame { .. } | What::Common => {
                    (Kind::Native, Owner::Native)
                }
                _ => (Kind::Linker, Owner::Linker),
            };
            backends.entry(kind).or_default().add(piece.size);
            crates.entry(owner).or_default().add(piece.size);
        };
        if self.full {
            self.new.live_pieces().for_each(&mut add);
        } else {
            self.fresh.iter().filter_map(|id| self.new.piece(id)).for_each(&mut add);
        }
        let (image, generation, full) = (self.name, self.new.generation, self.full);
        for (name, sum) in ranked(backends, Kind::name) {
            let Sum { pieces, bytes } = sum;
            tracing::info!(target: "blaze", image, generation, full, by = "backend", pieces, bytes, what = &*name, "link code");
        }
        let crates = ranked(crates, Owner::name);
        let (top, rest) = crates.split_at(crates.len().min(TOP_CRATES));
        for (rank, (name, sum)) in top.iter().enumerate() {
            let Sum { pieces, bytes } = *sum;
            tracing::info!(target: "blaze", image, generation, full, by = "crate", rank, pieces, bytes, what = &**name, "link code");
        }
        if !rest.is_empty() {
            let pieces = rest.iter().map(|it| it.1.pieces).sum::<u64>();
            let bytes = rest.iter().map(|it| it.1.bytes).sum::<u64>();
            // alloc: once a stage, the rest's name
            let name = format!("({} more crates)", rest.len());
            tracing::info!(target: "blaze", image, generation, full, by = "crate", rank = TOP_CRATES, pieces, bytes, what = name.as_str(), "link code");
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn region(section: &str, addr: u64, size: u64, capacity: u64) -> RegionShape {
        RegionShape { section: section.into(), class: "Text".into(), addr, size, capacity }
    }

    /// The rendered layout names each region in address order, the gap between two, and marks
    /// the ends of a reach in the regions they are in.
    #[test]
    fn render_marks_the_ends_and_the_gaps() {
        let layout = [
            region("__TEXT,__blaze_patch", 0x1_0000_4000, 0, 0x4000_0000),
            region("__TEXT,__text", 0x1_4000_4000, 0x9000_0000, 0x9000_0000),
            region("__TEXT,__eh_frame", 0x1_e000_0000, 0x100, 0x100),
        ];
        let mut out = String::new();
        render(
            &layout,
            0x1_0000_0000,
            &[(0x1_e000_0008, "from"), (0x1_0000_5000, "to"), (0x5, "nowhere")],
            &mut out,
        );
        let lines: Vec<&str> = out.lines().collect();
        assert!(
            lines[1].contains("__TEXT,__blaze_patch") && lines[1].ends_with("<- to 0x100005000"),
            "{out}"
        );
        assert!(lines[2].contains("__TEXT,__text") && !lines[2].contains("<-"), "{out}");
        assert_eq!(lines[3].trim(), "0xfffc000 bytes unused", "{out}");
        assert!(
            lines[4].contains("size 0x100  room 0x100")
                && lines[4].ends_with("<- from 0x1e0000008"),
            "{out}"
        );
        assert_eq!(lines[5].trim(), "nowhere 0x5 is in no region (the headers, or a gap)", "{out}");
    }

    /// Only a refusal about size or reach gets the layout.
    #[test]
    fn explained_adds_the_layout_to_size_and_reach_refusals_only() {
        let layout = [region("__TEXT,__text", 0x1000, 0x10, 0x10)];
        let error = |refusal| LinkError { function: None, what: "x".into(), refusal };
        let field = explained(error(Some(Refusal::FieldRange)), &layout, 0);
        assert!(
            field.what.starts_with("x\nthe image's regions")
                && field.what.contains("__TEXT,__text"),
            "{}",
            field.what
        );
        let room = explained(error(Some(Refusal::NoRoom { need: 2, room: 1 })), &layout, 0);
        assert!(room.what.contains("__TEXT,__text"));
        assert_eq!(explained(error(None), &layout, 0).what, "x");
        assert_eq!(explained(error(Some(Refusal::NoRegion)), &layout, 0).what, "x");
    }
}
