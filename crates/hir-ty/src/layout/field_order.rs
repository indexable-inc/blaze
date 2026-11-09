//! Field order by heat (docs/v2/plan/design/field-order.md): the fields of a
//! `repr(Rust)` struct or enum variant that a profile shows hot code touching laid out first,
//! so they share the type's first cache line.
//!
//! What allows it, the Reference's "Type layout" chapter: "The only data layout guarantees made
//! by this representation are those required for soundness. These are: 1. The offset of a field
//! is divisible by that field's alignment. 2. The alignment of the type is at least the maximum
//! alignment of its fields." and, of structs, "The ordering does not have to be the same as the
//! order in which the fields are specified in the declaration of the type." "Type layout can be
//! changed with each compilation." rustc itself sorts fields by alignment and shuffles them
//! under `-Zrandomize-layout`.
//!
//! What it keeps, so nothing a program may rely on changes: a type with `#[repr(C)]`,
//! `#[repr(<int>)]`, `packed`, `align`, `transparent`, `simd`, `#[blaze::bits]`, a lang item, an
//! unsizable tail or one that fits one line is never reordered; and a reordered type keeps
//! rustc's size, alignment, ABI, niche and (for an enum) tag, so every type around it
//! (`Option<T>`, an enum holding it) is laid out exactly as before. A variant of one field is
//! never moved, so the niche guarantees `std::option` documents hold.
//!
//! Without a plan ([`FieldOrder`] absent, or no ranks for the type) the layout is rustc's,
//! byte for byte, and [`order_id`] adds nothing to the type's layout identity.

use std::{alloc::Global, cmp, fmt, sync::Arc};

use base_db::Db;
use blaze_hash::HashMap;
use blaze_ids::{ContentId, Domain, Hasher};
use blaze_mem::Frame;
use blaze_query::db::{Durability, Setter as _};
use hir_def::{
    AdtId, EnumVariantId, LocalFieldId, VariantId,
    attrs::AttrFlags,
    signatures::{EnumSignature, StructFlags, StructSignature, UnionSignature},
};
use hir_expand::name::Name;
use rustc_abi::{FieldsShape, NicheOptimizations, ReprFlags, ReprOptions, Size, TargetDataLayout};
use rustc_index::{IndexSlice, IndexVec};

use crate::layout::{
    Layout, LayoutError, RustcEnumVariantIdx, RustcFieldIdx, TagEncoding, Variants,
    enum_variant_ids,
};

pub mod heat;
#[cfg(test)]
mod tests;

/// The calculator's input, as `layout_of_adt` makes it: per variant, its fields' layouts in
/// declaration order.
pub(crate) type FieldLayouts<'a, 'b> =
    IndexVec<RustcEnumVariantIdx, IndexVec<RustcFieldIdx, &'a &'b Layout>>;

/// A type at most this big is left alone: in rustc's order it already fits one cache line.
pub const LINE: u64 = 64;

/// The most hot fields of a variant tried: [`hot_first`] lays a type out at most this many times.
pub const MAX_HOT: usize = 16;

// ---------------------------------------------------------------------------------------------
// The plan, as the driver installs it.

/// One ADT's hot fields as a plan names them: per variant in declaration order, its hot fields
/// hottest first. A field not named is cold.
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub struct AdtPlan {
    pub variants: Box<[Box<[Name]>]>,
}

/// A profile's field order for one tree: the ADTs it ranks, by their ids in this database (the
/// driver resolves the plan file's item keys, `blaze_items::field_order`), and the plan's
/// content ID. Equal plans are plans with equal IDs.
#[derive(Debug)]
pub struct FieldOrderPlan {
    id: ContentId,
    by_adt: HashMap<AdtId, AdtPlan, Global>,
}

impl FieldOrderPlan {
    pub fn new(id: ContentId, by_adt: HashMap<AdtId, AdtPlan, Global>) -> FieldOrderPlan {
        FieldOrderPlan { id, by_adt }
    }

    pub fn id(&self) -> ContentId {
        self.id
    }

    pub fn get(&self, adt: AdtId) -> Option<&AdtPlan> {
        self.by_adt.get(&adt)
    }

    pub fn len(&self) -> usize {
        self.by_adt.len()
    }

    pub fn is_empty(&self) -> bool {
        self.by_adt.is_empty()
    }
}

impl PartialEq for FieldOrderPlan {
    fn eq(&self, other: &FieldOrderPlan) -> bool {
        self.id == other.id
    }
}

impl Eq for FieldOrderPlan {}

/// The field order plan every layout reads: absent, or holding `None`, means no plan, and every
/// layout is rustc's. Set once per job, at high durability: a plan changes with the profile, not
/// with an edit.
#[blaze_query::input(singleton, debug)]
pub struct FieldOrder {
    #[returns(ref)]
    pub plan: Option<Arc<FieldOrderPlan>>,
}

/// Makes `plan` the plan of every query from now on (`None`: no plan). Sets the input only when
/// the plan changed, so an unchanged plan invalidates nothing.
pub fn install(db: &mut Db, plan: Option<Arc<FieldOrderPlan>>) {
    match FieldOrder::try_get(db) {
        None => {
            _ = FieldOrder::new(db, Durability::HIGH, plan);
        }
        Some(input) => {
            if *input.plan(db) != plan {
                input.set_plan(db).with_durability(Durability::HIGH).to(plan);
            }
        }
    }
}

// ---------------------------------------------------------------------------------------------
// One ADT's ranks, resolved against its fields.

/// What a layout reads of the plan for one ADT.
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub enum Ranks {
    Hot(HotFields),
    /// The plan does not fit the ADT: it was made of another tree.
    Stale(Stale),
}

/// Each variant's hot fields, hottest first, by their ids.
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub struct HotFields {
    /// What the type's layout identity adds (`blaze_items::hash`, through [`order_id`]).
    pub id: ContentId,
    /// Per variant, in declaration order.
    pub variants: Box<[Box<[LocalFieldId]>]>,
}

/// Why a plan's ranks don't fit an ADT.
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub enum Stale {
    Variants { planned: usize, has: usize },
    Field { variant: usize, name: Name },
    Repeated { variant: usize, name: Name },
}

impl fmt::Display for Stale {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Stale::Variants { planned, has } => {
                write!(f, "the plan ranks {planned} variants, the type has {has}")
            }
            Stale::Field { variant, name } => {
                write!(f, "variant {variant} has no field `{}`", name.as_str())
            }
            Stale::Repeated { variant, name } => {
                write!(f, "the plan names field `{}` of variant {variant} twice", name.as_str())
            }
        }
    }
}

static ORDER_DOMAIN: Domain = Domain::new("blaze v2 field order 1");

/// The plan's ranks for `adt`, its field names resolved to ids: `None` without a plan or when
/// the plan does not rank it. A field's rename that the plan does not name leaves this equal,
/// so the layout is not asked again.
#[blaze_query::tracked(returns(ref), mem_tag = Infer)]
pub fn field_ranks(db: &Db, adt: AdtId) -> Option<Ranks> {
    let input = FieldOrder::try_get(db)?;
    let planned = input.plan(db).as_ref()?.get(adt)?;
    Some(resolve(db, adt, planned))
}

/// What `adt`'s layout identity adds for its field order: the ranks' content ID, or nothing
/// (`None`) when no plan ranks it, so a build without a plan keeps every layout hash.
pub fn order_id(db: &Db, adt: AdtId) -> Option<ContentId> {
    match field_ranks(db, adt) {
        Some(Ranks::Hot(hot)) => Some(hot.id),
        Some(Ranks::Stale(_)) | None => None,
    }
}

/// Why `planned` does not fit `adt`, as its layout would find it ([`Ranks::Stale`]), or `None`
/// when it fits. The driver checks each ADT of a plan with this as it reads the plan
/// (`blaze_items::field_order::decode`), so a plan made of another tree is reported once,
/// naming each type that keeps rustc's order for it, not only on the type's `field_order` span.
pub fn stale(db: &Db, adt: AdtId, planned: &AdtPlan) -> Option<Stale> {
    match resolve(db, adt, planned) {
        Ranks::Hot(_) => None,
        Ranks::Stale(why) => Some(why),
    }
}

fn resolve(db: &Db, adt: AdtId, planned: &AdtPlan) -> Ranks {
    let enum_variants: &[EnumVariantId] = match adt {
        AdtId::EnumId(e) => enum_variant_ids(db, e),
        AdtId::StructId(_) | AdtId::UnionId(_) => &[],
    };
    let count = match adt {
        AdtId::EnumId(_) => enum_variants.len(),
        AdtId::StructId(_) | AdtId::UnionId(_) => 1,
    };
    if planned.variants.len() != count {
        return Ranks::Stale(Stale::Variants { planned: planned.variants.len(), has: count });
    }
    let variant = |v: usize| -> VariantId {
        match adt {
            AdtId::StructId(s) => s.into(),
            AdtId::UnionId(u) => u.into(),
            AdtId::EnumId(_) => enum_variants[v].into(),
        }
    };
    let mut h = Hasher::of(&ORDER_DOMAIN);
    h.update(&(count as u32).to_le_bytes());
    // alloc: kept in the memo, one slice per variant
    let mut variants = Vec::with_capacity(count);
    for (v, names) in planned.variants.iter().enumerate() {
        let fields = variant(v).fields(db);
        // alloc: kept in the memo
        let mut ids = Vec::with_capacity(names.len());
        for &name in names.iter() {
            let Some(id) = fields.field(&name) else {
                return Ranks::Stale(Stale::Field { variant: v, name });
            };
            if ids.contains(&id) {
                return Ranks::Stale(Stale::Repeated { variant: v, name });
            }
            ids.push(id);
        }
        h.update(&(ids.len() as u32).to_le_bytes());
        for &id in &ids {
            h.update(&u32::from(id.into_raw()).to_le_bytes());
        }
        variants.push(ids.into_boxed_slice());
    }
    Ranks::Hot(HotFields { id: h.finish(), variants: variants.into_boxed_slice() })
}

// ---------------------------------------------------------------------------------------------
// Applying the ranks to a layout.

/// What decides whether a type may be laid out hot first, besides its ranks and rustc's layout.
pub(crate) struct Shape {
    pub(crate) repr: ReprOptions,
    pub(crate) flags: Option<StructFlags>,
    pub(crate) maybe_unsized: bool,
    pub(crate) niches: NicheOptimizations,
}

/// `rustc`, `def`'s layout in rustc's order, or the same type with its hot fields first when the
/// plan ranks it and it may be reordered. The outcome is recorded on a `field_order` span naming
/// the type: applied (how many hot fields, how many layouts tried) or why not.
pub(crate) fn reorder<'a, 'b>(
    db: &Db,
    def: AdtId,
    shape: &Shape,
    rustc: Layout,
    fields: &FieldLayouts<'a, 'b>,
    dl: &TargetDataLayout,
    frame: &Frame,
    layout_with: impl Fn(&ReprOptions, &FieldLayouts<'a, 'b>) -> Result<Layout, LayoutError>,
) -> Layout {
    let Some(ranks) = field_ranks(db, def) else { return rustc };
    let span = tracing::info_span!(
        target: "blaze",
        "field_order",
        adt = %AdtName(db, def),
        outcome = tracing::field::Empty,
    )
    .entered();
    let hot = match ranks {
        Ranks::Hot(hot) => hot,
        Ranks::Stale(why) => {
            span.record("outcome", tracing::field::display(format_args!("not applied: {why}")));
            return rustc;
        }
    };
    if let Some(why) = refusal(db, def, shape, &rustc) {
        span.record("outcome", tracing::field::display(format_args!("not applied: {why}")));
        return rustc;
    }
    match hot_first(hot, &rustc, fields, &shape.repr, dl, frame, layout_with) {
        HotFirst::Applied { layout, hot, tried } => {
            span.record(
                "outcome",
                tracing::field::display(format_args!(
                    "applied: {hot} hot fields first, {tried} layouts tried"
                )),
            );
            layout
        }
        HotFirst::AlreadyFirst => {
            span.record("outcome", "not applied: its hot fields are first in rustc's order");
            rustc
        }
        HotFirst::NoneFits { tried } => {
            span.record(
                "outcome",
                tracing::field::display(format_args!(
                    "not applied: each of {tried} hot-first layouts grows the type or moves its \
                     niche, tag or ABI"
                )),
            );
            rustc
        }
        HotFirst::Shape(why) => {
            span.record("outcome", tracing::field::display(format_args!("not applied: {why}")));
            rustc
        }
    }
}

/// Why `def` keeps rustc's order whatever its ranks, if it does.
fn refusal(db: &Db, def: AdtId, shape: &Shape, rustc: &Layout) -> Option<&'static str> {
    let repr = &shape.repr;
    let flags = shape.flags.unwrap_or(StructFlags::empty());
    let compiler_laid_out = StructFlags::IS_PHANTOM_DATA
        | StructFlags::IS_BOX
        | StructFlags::IS_MANUALLY_DROP
        | StructFlags::IS_UNSAFE_CELL
        | StructFlags::IS_UNSAFE_PINNED
        | StructFlags::IS_COVARIANT_UNSAFE_CELL
        | StructFlags::IS_F16B
        | StructFlags::IS_PPC_F128
        | StructFlags::IS_FIELD_REPRESENTING_TYPE;
    Some(if matches!(def, AdtId::UnionId(_)) {
        "a union: its fields share offset 0"
    } else if repr.c() {
        "`#[repr(C)]` fixes its order"
    } else if repr.int.is_some() {
        "a primitive `#[repr]` specifies its layout"
    } else if repr.simd() || repr.scalable() {
        "a SIMD type"
    } else if repr.linear() {
        "its representation is linear"
    } else if repr.transparent() {
        "`#[repr(transparent)]` is laid out as its field"
    } else if repr.packed() {
        "`#[repr(packed)]`"
    } else if repr.align.is_some() {
        "`#[repr(align)]`"
    } else if repr.flags.contains(ReprFlags::RANDOMIZE_LAYOUT) {
        "`-Zrandomize-layout` orders it"
    } else if flags.contains(StructFlags::IS_BLAZE_BITS) {
        "`#[blaze::bits]` lays it out by bits"
    } else if flags.intersects(compiler_laid_out)
        || AttrFlags::query(db, def.into()).contains(AttrFlags::LANG_ITEM)
    {
        "a lang item: the compiler reads its fields by itself"
    } else if shape.maybe_unsized {
        "its last field may be unsized, and `T<dyn Trait>` must keep `T<U>`'s order"
    } else if shape.niches == NicheOptimizations::Disabled {
        "its niches are hidden"
    } else if rustc.uninhabited || matches!(rustc.variants, Variants::Empty) {
        "it is uninhabited"
    } else if rustc.size.bytes() <= LINE {
        "it fits one cache line in rustc's order"
    } else {
        return None;
    })
}

/// What [`hot_first`] made.
#[derive(Debug)]
pub(crate) enum HotFirst {
    /// The layout with the `hot` hottest fields of each variant first, after `tried` layouts.
    Applied { layout: Layout, hot: usize, tried: u32 },
    /// rustc's order has the hot fields first already.
    AlreadyFirst,
    /// Each hot-first layout tried differs from rustc's where other types see it.
    NoneFits { tried: u32 },
    /// rustc's layout is not one this reorders, and why.
    Shape(&'static str),
}

/// Lays the type out again with each variant's `h` hottest fields first, `h` from the most that
/// share a line (the hottest, and those after it while their sizes add up to at most
/// [`LINE`]; at most [`MAX_HOT`]) down to 1, skipping an `h` whose fields are first in rustc's
/// order already, and keeps the first layout that equals `rustc`'s wherever another type or a
/// caller sees it ([`same_outside`]): its size, alignment, ABI, niche, tag and inhabitedness.
/// So the type never grows and every type around it keeps its layout; only the offsets of its
/// own fields move.
///
/// Each variant is fed to the calculator in the chosen order with `#[repr]` made linear
/// (`ReprFlags::IS_LINEAR`, which only stops the calculator sorting), then its fields renamed
/// back. A variant's order is rustc's own with three changes: the fields up to and including
/// the one holding the type's niche, if it is first, stay first, and the fields from it on, if
/// it is not, stay last, so the niche stays where it was; the hot fields come next, in rustc's
/// relative order; and a gap an alignment leaves before one is filled with cold fields that
/// fit. A variant with no hot fields keeps rustc's order exactly.
pub(crate) fn hot_first<'a, 'b>(
    hot: &HotFields,
    rustc: &Layout,
    fields: &FieldLayouts<'a, 'b>,
    repr: &ReprOptions,
    dl: &TargetDataLayout,
    frame: &Frame,
    layout_with: impl Fn(&ReprOptions, &FieldLayouts<'a, 'b>) -> Result<Layout, LayoutError>,
) -> HotFirst {
    if hot.variants.len() != fields.len() {
        return HotFirst::Shape("its variants are not the plan's");
    }
    // Each variant's fields as rustc placed them, `None` for one it did not lay out.
    let mut placed: Vec<Option<(&[RustcFieldIdx], &IndexSlice<RustcFieldIdx, Size>)>, &Frame> =
        Vec::with_capacity_in(fields.len(), frame);
    placed.resize(fields.len(), None);
    // Where the niche other types read is (its variant and offset), and where a variant's
    // fields start (after a direct tag).
    let mut niche = None;
    let mut start = Size::ZERO;
    match &rustc.variants {
        Variants::Empty => return HotFirst::Shape("it is uninhabited"),
        Variants::Single { index } => {
            let FieldsShape::Arbitrary { offsets, in_memory_order } = &rustc.fields else {
                return HotFirst::Shape("its fields are not laid out one by one");
            };
            placed[index.0] = Some((in_memory_order.raw.as_slice(), offsets.as_slice()));
            niche = rustc.largest_niche.map(|it| (index.0, it.offset));
        }
        Variants::Multiple { tag, tag_encoding, variants, .. } => {
            for (v, variant) in variants.iter_enumerated() {
                placed[v.0] =
                    Some((&variant.fields_in_memory_order().raw, variant.field_offsets.as_slice()));
            }
            match tag_encoding {
                TagEncoding::Niche { untagged_variant, .. } => {
                    niche = Some((untagged_variant.0, rustc.fields.offset(0)));
                }
                TagEncoding::Direct => start = rustc.fields.offset(0) + tag.size(dl),
            }
        }
    }
    // Per variant, how many of its hottest fields share a line: the hottest, and those after it
    // while their sizes add up to at most a line. A field ranked below one that does not fit is
    // left cold, so a big lukewarm field is never put before a small hot one.
    let mut sharing: Vec<usize, &Frame> = Vec::with_capacity_in(fields.len(), frame);
    for (v, own) in fields.iter_enumerated() {
        let mut size = 0;
        let mut count = 0;
        for &f in hot.variants[v.0].iter() {
            size += own[RustcFieldIdx(f)].size.bytes();
            if count > 0 && size > LINE {
                break;
            }
            count += 1;
        }
        sharing.push(count);
    }
    let most = sharing.iter().copied().max().unwrap_or(0).min(MAX_HOT);
    if most == 0 {
        return HotFirst::Shape("the plan ranks none of its fields");
    }
    let linear = ReprOptions { flags: repr.flags | ReprFlags::IS_LINEAR, ..*repr };
    let mut tried = 0;
    for h in (1..=most).rev() {
        // alloc: the calculator takes an owned `IndexVec` per variant, as `variants_of` says
        let mut orders: IndexVec<RustcEnumVariantIdx, IndexVec<RustcFieldIdx, RustcFieldIdx>> =
            IndexVec::with_capacity(fields.len());
        let mut moved = false;
        for (v, own) in fields.iter_enumerated() {
            let ranked = &hot.variants[v.0][..cmp::min(h, sharing[v.0])];
            let order = match placed[v.0] {
                Some((order, offsets)) if !ranked.is_empty() => {
                    let variant_start =
                        if niche.is_some_and(|(at, _)| at == v.0) { Size::ZERO } else { start };
                    let niche_at = niche.and_then(|(at, offset)| (at == v.0).then_some(offset));
                    let chosen =
                        variant_order(order, offsets, own, ranked, niche_at, variant_start, frame);
                    moved |= chosen.raw != order;
                    chosen
                }
                // alloc: as above
                Some((order, _)) => IndexVec::from_raw(order.to_vec()),
                None => own.indices().collect(),
            };
            orders.push(order);
        }
        // These `h` are first in rustc's order already; fewer may not be.
        if !moved {
            continue;
        }
        let permuted: FieldLayouts<'a, 'b> = fields
            .iter_enumerated()
            // alloc: as above
            .map(|(v, own)| orders[v].iter().map(|&f| own[f]).collect())
            .collect();
        tried += 1;
        // A layout the calculator refuses in this order is one more that does not fit.
        let Ok(mut candidate) = layout_with(&linear, &permuted) else { continue };
        if !same_outside(rustc, &candidate) {
            continue;
        }
        unpermute(&mut candidate, &orders);
        return HotFirst::Applied { layout: candidate, hot: h, tried };
    }
    match tried {
        0 => HotFirst::AlreadyFirst,
        _ => HotFirst::NoneFits { tried },
    }
}

/// One variant's order, as [`hot_first`] says: `placed` is rustc's (by increasing offset),
/// `ranked` the hot fields to put first, `niche` the offset of the niche other types read when
/// this variant holds it, `start` where the variant's first field may go.
fn variant_order(
    placed: &[RustcFieldIdx],
    offsets: &IndexSlice<RustcFieldIdx, Size>,
    fields: &IndexSlice<RustcFieldIdx, &&Layout>,
    ranked: &[LocalFieldId],
    niche: Option<Size>,
    start: Size,
    frame: &Frame,
) -> IndexVec<RustcFieldIdx, RustcFieldIdx> {
    let n = placed.len();
    let holds = |f: RustcFieldIdx, at: Size| {
        let size = fields[f].size;
        size > Size::ZERO && offsets[f] <= at && at < offsets[f] + size
    };
    // `placed[..head]` and `placed[tail..]` keep their places.
    let (head, tail) = match niche {
        None => (0, n),
        Some(at) => match placed.iter().position(|&f| holds(f, at)) {
            Some(pos) if offsets[placed[pos]] == start => (pos + 1, n),
            Some(pos) => (0, pos),
            None => (n, n),
        },
    };
    let is_hot = |f: RustcFieldIdx| ranked.contains(&f.0);
    let middle = &placed[head..tail];
    let mut cold = Vec::with_capacity_in(middle.len(), frame);
    cold.extend(middle.iter().copied().filter(|&f| !is_hot(f)));
    // alloc: the calculator's owned order, as `variants_of` says
    let mut out = Vec::with_capacity(n);
    out.extend_from_slice(&placed[..head]);
    let mut at = start;
    for &f in &placed[..head] {
        at = at.align_to(fields[f].align.abi) + fields[f].size;
    }
    for &f in middle.iter().filter(|&&f| is_hot(f)) {
        at = fill(at, fields[f].align.abi.bytes(), &mut cold, fields, &mut out);
        out.push(f);
        at = at.align_to(fields[f].align.abi) + fields[f].size;
    }
    if let Some(&next) = cold.first() {
        fill(at, fields[next].align.abi.bytes(), &mut cold, fields, &mut out);
    }
    out.extend_from_slice(&cold);
    out.extend_from_slice(&placed[tail..]);
    IndexVec::from_raw(out)
}

/// Fills the gap from `at` up to the next multiple of `align` (bytes) with cold fields that fit
/// it, taken in rustc's order, and gives where the next field may go.
fn fill(
    mut at: Size,
    align: u64,
    cold: &mut Vec<RustcFieldIdx, &Frame>,
    fields: &IndexSlice<RustcFieldIdx, &&Layout>,
    out: &mut Vec<RustcFieldIdx>,
) -> Size {
    // Each pass moves one field, so `cold.len()` passes is the bound.
    for _ in 0..cold.len() {
        let gap = Size::from_bytes(at.bytes().next_multiple_of(align)) - at;
        if gap == Size::ZERO {
            break;
        }
        let fits = |&f: &RustcFieldIdx| {
            let layout = fields[f];
            layout.size > Size::ZERO && layout.size <= gap && at.align_to(layout.align.abi) == at
        };
        let Some(i) = cold.iter().position(fits) else { break };
        let f = cold.remove(i);
        out.push(f);
        at += fields[f].size;
    }
    at
}

/// Whether `a` and `b` are the same layout wherever anything but the type's own field accesses
/// reads it: what an enclosing type's layout, a call's ABI, a niche or a tag test use.
pub(crate) fn same_outside(a: &Layout, b: &Layout) -> bool {
    let variants = match (&a.variants, &b.variants) {
        (Variants::Empty, Variants::Empty) => true,
        (Variants::Single { index: x }, Variants::Single { index: y }) => x == y,
        (
            Variants::Multiple { tag: t1, tag_encoding: e1, tag_field: f1, .. },
            Variants::Multiple { tag: t2, tag_encoding: e2, tag_field: f2, .. },
        ) => t1 == t2 && e1 == e2 && f1 == f2 && a.fields == b.fields,
        _ => false,
    };
    variants
        && a.size == b.size
        && a.align == b.align
        && a.backend_repr == b.backend_repr
        && a.largest_niche == b.largest_niche
        && a.uninhabited == b.uninhabited
        && a.max_repr_align == b.max_repr_align
        && a.unadjusted_abi_align == b.unadjusted_abi_align
        && a.repr_c == b.repr_c
        && a.randomization_seed == b.randomization_seed
}

/// Names the fields of a layout made from `orders` (variant `v`'s field `k` being its field
/// `orders[v][k]`) by their own indices again.
fn unpermute(
    layout: &mut Layout,
    orders: &IndexSlice<RustcEnumVariantIdx, IndexVec<RustcFieldIdx, RustcFieldIdx>>,
) {
    match &mut layout.variants {
        Variants::Empty => {}
        Variants::Single { index } => {
            let index = *index;
            if let FieldsShape::Arbitrary { offsets, in_memory_order } = &mut layout.fields {
                rustc_abi::unpermute_fields(offsets, in_memory_order, &orders[index]);
            }
        }
        Variants::Multiple { variants, .. } => {
            for (v, variant) in variants.iter_enumerated_mut() {
                variant.unpermute_fields(&orders[v]);
            }
        }
    }
}

/// An ADT's name, for a span.
struct AdtName<'a>(&'a Db, AdtId);

impl fmt::Display for AdtName<'_> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        let AdtName(db, adt) = *self;
        let name = match adt {
            AdtId::StructId(it) => &StructSignature::of(db, it).name,
            AdtId::UnionId(it) => &UnionSignature::of(db, it).name,
            AdtId::EnumId(it) => &EnumSignature::of(db, it).name,
        };
        write!(f, "{} ({adt:?})", name.as_str())
    }
}
