//! The layout questions the evaluator asks (consteval.md, "Layout"), answered by the one home
//! of layout, blaze-build's (`blaze_build::layout`, the build-support lane's), so compile time
//! and run time lay a value out the same way. [`Layout`] is the seam the machine reads through:
//! blaze-build's [`Layouts`] implements it here; nothing in this crate computes a size or an
//! offset itself.
//!
//! One [`Layouts`] lives as long as its unit and target, beside the ground table its ids are
//! of: the driver passes the same pair to every evaluation of the unit. An enum's explicit
//! discriminants are values the layout reads from their records (`LayoutFacts::discriminant`);
//! while one is not made, blaze-build's layout refuses with `Missing` and caches nothing, and
//! the evaluation returns it as a need ([`Stop::Need`]): nothing here checks it again.

use core::alloc::Allocator;

pub use blaze_build::layout::{IntRepr, LayoutCx, LayoutFacts, Layouts, Missing, ReprAttrs, ValidRange};
use blaze_own::{Alloc, Vec};
use blaze_typeck::{Adt, IntTy, ItemKey, List, PNode, PTy, PortableTys, UintTy};

use crate::error::Refusal;

/// How a variant's tag is written: `value` in `size` bytes at `offset`.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct TagWrite {
    pub offset: u64,
    pub size: u64,
    pub value: u128,
}

/// Why a layout question has no answer yet or ever.
#[derive(Debug)]
pub enum Stop {
    /// The type has no layout: refused by name.
    Refused(Refusal<'static>),
    /// The value of this item (an explicit discriminant) is read and its record is not made.
    Need(ItemKey),
}

impl From<Refusal<'static>> for Stop {
    fn from(r: Refusal<'static>) -> Stop {
        Stop::Refused(r)
    }
}

/// The layout of ground types, as the machine asks it.
pub trait Layout {
    /// Size and alignment (log 2) of a sized ground type.
    fn size_align(&mut self, ty: PTy, cx: &mut LayoutCx<'_, '_>) -> Result<(u64, u8), Stop>;
    /// The offset of field `field` (declaration order) of variant `variant` (0 for a struct,
    /// a union or a tuple).
    fn field_offset(&mut self, ty: PTy, variant: u32, field: u32, cx: &mut LayoutCx<'_, '_>) -> Result<u64, Stop>;
    /// How variant `variant` of enum `ty` is marked; `None`: nothing to write.
    fn tag_write(&mut self, ty: PTy, variant: u32, cx: &mut LayoutCx<'_, '_>) -> Result<Option<TagWrite>, Stop>;
    /// Where `ty`'s tag is read and how it decodes ([`Tag`]).
    fn tag(&mut self, ty: PTy, cx: &mut LayoutCx<'_, '_>) -> Result<Tag, Stop>;
    /// Whether `ty` is not `Freeze`: an `UnsafeCell` held by value somewhere in it
    /// (blaze-build's `Layouts::interior_mut`).
    fn interior_mut(&mut self, ty: PTy, cx: &mut LayoutCx<'_, '_>) -> Result<bool, Stop>;
}

fn stop(e: &blaze_build::layout::LayoutError) -> Stop {
    use blaze_build::layout::Refusal as L;
    Stop::Refused(match e.why {
        L::Missing(m) => return Stop::Need(m.item),
        L::Unsupported(what) => Refusal::Unsupported { what },
        L::Malformed(what) => Refusal::Malformed { what },
        L::Recursive => Refusal::Malformed { what: "a type that holds itself by value" },
        L::TooLarge => Refusal::Unsupported { what: "a type larger than `isize::MAX` bytes" },
        L::NoAdt(item) => Refusal::Missing { what: "ADT definition", item },
        L::Ground(ref error) => match projection_of(error) {
            Some((trait_, name)) => Refusal::Projection { trait_, name },
            None => Refusal::Unsupported { what: "a field type with no ground form" },
        },
        L::FieldGround { adt, field, name, ref error } => Refusal::FieldGround { adt, field, name, projection: projection_of(error) },
        L::NoSize(item) => Refusal::NoSize { item },
    })
}

const NO_ROW: Refusal<'static> = Refusal::Malformed { what: "a layout row its type does not have" };

impl<A: Allocator + Clone> Layout for Layouts<A> {
    fn size_align(&mut self, ty: PTy, cx: &mut LayoutCx<'_, '_>) -> Result<(u64, u8), Stop> {
        let id = self.of(ty, cx).map_err(|e| stop(&e))?;
        let l = self.get(id);
        if l.tail != blaze_build::layout::Tail::Sized {
            return Err(Stop::Refused(Refusal::Unsupported { what: "a value of an unsized type" }));
        }
        Ok((l.size, l.align))
    }

    fn field_offset(&mut self, ty: PTy, variant: u32, field: u32, cx: &mut LayoutCx<'_, '_>) -> Result<u64, Stop> {
        let id = self.of(ty, cx).map_err(|e| stop(&e))?;
        let row = match self.get(id).variants {
            blaze_build::layout::Variants::Multiple { .. } => self.variant(id, variant).ok_or(NO_ROW)?,
            blaze_build::layout::Variants::Single { .. } => id,
        };
        Ok(Layouts::field_offset(self, row, field).ok_or(NO_ROW)?)
    }

    fn tag_write(&mut self, ty: PTy, variant: u32, cx: &mut LayoutCx<'_, '_>) -> Result<Option<TagWrite>, Stop> {
        let id = self.of(ty, cx).map_err(|e| stop(&e))?;
        Ok(Layouts::tag_write(self, id, variant).map(|w| TagWrite { offset: w.offset, size: w.scalar.prim.size(), value: w.value }))
    }

    fn interior_mut(&mut self, ty: PTy, cx: &mut LayoutCx<'_, '_>) -> Result<bool, Stop> {
        Layouts::interior_mut(self, ty, cx).map_err(|e| stop(&e))
    }

    fn tag(&mut self, ty: PTy, cx: &mut LayoutCx<'_, '_>) -> Result<Tag, Stop> {
        use blaze_build::layout::{TagEncoding, Variants};
        let id = self.of(ty, cx).map_err(|e| stop(&e))?;
        Ok(match self.get(id).variants {
            Variants::Single { index } => Tag { at: None, decode: Decode::Single(index) },
            Variants::Multiple { tag, tag_offset, encoding, .. } => {
                let mask = blaze_build::layout::mask(tag.prim.bits());
                let decode = match encoding {
                    TagEncoding::Direct => Decode::Direct { mask },
                    TagEncoding::Niche { untagged, first, last, start } => Decode::Niche { untagged, first, last, start, mask },
                };
                Tag { at: Some((tag_offset, tag.prim.size())), decode }
            }
        })
    }
}

/// Where a type's tag is and how its bits name a variant, read from the layout once: a run
/// decodes with no layout lookup. The code compiler joins it with the enum's checked
/// discriminants into its proof, `code::EnumFacts`.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Tag {
    /// Offset and size; `None`: one variant, nothing to read.
    pub at: Option<(u64, u64)>,
    decode: Decode,
}

impl Tag {
    /// The variant whose tag reads `bits` (blaze-build's `Layouts::variant_of_tag`); a direct
    /// tag is matched against `discrs`, the enum's discriminants by variant.
    pub fn variant(&self, bits: u128, discrs: &[i128]) -> Result<u32, Refusal<'static>> {
        match self.decode {
            Decode::Single(v) => Ok(v),
            Decode::Direct { mask } => {
                discrs.iter().position(|&d| (d as u128) & mask == bits & mask).map(|v| v as u32).ok_or(Refusal::InvalidTag { bits })
            }
            Decode::Niche { untagged, first, last, start, mask } => {
                let rel = bits.wrapping_sub(start) & mask;
                Ok(if rel <= u128::from(last - first) { first + rel as u32 } else { untagged })
            }
        }
    }

    /// The variant when the tag slot holds a non-null pointer with provenance (only a single
    /// variant or a null-pointer niche `start == 0 && first == last` is certain without knowing
    /// the address).
    pub fn ptr_variant(&self) -> Option<u32> {
        match self.decode {
            Decode::Single(v) => Some(v),
            Decode::Niche { untagged, first, last, start: 0, .. } if first == last => Some(untagged),
            _ => None,
        }
    }
}

/// How a tag's bits name a variant (blaze-build's `TagEncoding`, its mask resolved).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Decode {
    /// No tag: always this variant.
    Single(u32),
    /// The tag is the discriminant at the tag's width.
    Direct { mask: u128 },
    /// Variants `first..=last` other than `untagged` are `start + (v - first)`; any other
    /// value is `untagged`.
    Niche { untagged: u32, first: u32, last: u32, start: u128, mask: u128 },
}

/// The size in bytes of an integer type (`isize`/`usize`: the target's pointers, 8).
pub(crate) fn int_size(n: PNode) -> Option<u64> {
    Some(match n {
        PNode::Int(IntTy::I8) | PNode::Uint(UintTy::U8) | PNode::Bool => 1,
        PNode::Int(IntTy::I16) | PNode::Uint(UintTy::U16) => 2,
        PNode::Int(IntTy::I32) | PNode::Uint(UintTy::U32) | PNode::Char => 4,
        PNode::Int(IntTy::I64) | PNode::Uint(UintTy::U64) | PNode::Int(IntTy::Isize) | PNode::Uint(UintTy::Usize) => 8,
        PNode::Int(IntTy::I128) | PNode::Uint(UintTy::U128) => 16,
        _ => return None,
    })
}

/// How many struct tails [`is_unsized`] follows before it refuses (rule 18).
const TAIL_DEPTH: u32 = 64;

/// Whether a pointer to `pointee` is wide (pointer and length): a slice, `str`, or a struct
/// whose last field is, followed through each struct's last field (the Reference, "Dynamically
/// sized types": "a struct may contain a DST as the last field ... making the struct itself a
/// DST"; core's `CStr { inner: [c_char] }`).
pub fn is_unsized<W: LayoutFacts>(world: &W, g: &mut PortableTys<'_>, pointee: PTy, scratch: Alloc<'_>) -> Result<bool, Refusal<'static>> {
    let mut t = pointee;
    for _ in 0..TAIL_DEPTH {
        match g.view().node(t).map_err(|_| bad())? {
            PNode::Slice(_) | PNode::Str => return Ok(true),
            PNode::Adt(key, args) => {
                let Some(adt) = world.adt(key) else { return Err(Refusal::Missing { what: "ADT definition", item: key }) };
                if adt.kind != blaze_typeck::AdtKind::Struct {
                    return Ok(false);
                }
                let Some(v) = adt.variants.first() else { return Ok(false) };
                if v.fields.len == 0 {
                    return Ok(false);
                }
                t = field_ty(key, &adt, v.fields.start + v.fields.len - 1, args, g, scratch, world)?;
            }
            _ => return Ok(false),
        }
    }
    Err(Refusal::Unsupported { what: "a struct tail nested past the evaluator's depth bound" })
}

fn bad() -> Refusal<'static> {
    Refusal::Malformed { what: "a type past the ground table" }
}

/// The `(trait, associated type)` of a grounding refusal that is a projection no impl
/// answered; `None` for any other.
fn projection_of(error: &blaze_build::BuildError) -> Option<(ItemKey, blaze_typeck::Name)> {
    match *error {
        blaze_build::BuildError::Projection { trait_, name } => Some((trait_, name)),
        _ => None,
    }
}

/// The ground type of field `field` (its row in `adt.fields`) of ADT `key` (`adt`) at `args`;
/// a type with no ground form is refused naming the field ([`Refusal::FieldGround`]).
pub(crate) fn field_ty(key: ItemKey, adt: &Adt<'_>, field: u32, args: List, ground: &mut PortableTys<'_>, scratch: Alloc<'_>, facts: &dyn LayoutFacts) -> Result<PTy, Refusal<'static>> {
    let f = adt.fields.get(field as usize).ok_or(Refusal::Malformed { what: "a field past its ADT" })?;
    let mut a: Vec<PTy, Alloc<'_>> = Vec::new_in(scratch);
    a.extend_from_slice(ground.view().list(args).map_err(|_| bad())?);
    blaze_build::ground(adt.types, f.ty, &a, ground, scratch, facts)
        .map_err(|e| Refusal::FieldGround { adt: key, field, name: f.name, projection: projection_of(&e) })
}

#[cfg(test)]
mod field_ground_tests;
