//! Detecting whether a type is infinitely-sized.

use base_db::Db;
use blaze_ids::bits::Bits;
use blaze_mem::Frame;
use hir_def::{AdtId, VariantId};
use rustc_type_ir::inherent::IntoKind;
use smallvec::SmallVec;

use crate::{
    db::HirDatabase,
    next_solver::{GenericArgKind, GenericArgs, Ty, TyKind},
};

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Representability {
    Representable,
    Infinite,
}

macro_rules! rtry {
    ($e:expr) => {
        match $e {
            e @ Representability::Infinite => return e,
            Representability::Representable => {}
        }
    };
}

#[blaze_query::tracked(cycle_result = representability_cycle, returns(copy), mem_tag = Infer)]
pub(crate) fn representability(db: &Db, id: AdtId) -> Representability {
    match id {
        AdtId::StructId(id) => variant_representability(db, id.into()),
        AdtId::UnionId(id) => variant_representability(db, id.into()),
        AdtId::EnumId(id) => {
            for &(variant, ..) in id.enum_variants(db).variants.values() {
                rtry!(variant_representability(db, variant.into()));
            }
            Representability::Representable
        }
    }
}

pub(crate) fn representability_cycle(
    _db: &Db,
    _: blaze_query::db::Id,
    _id: AdtId,
) -> Representability {
    Representability::Infinite
}

fn variant_representability(db: &Db, id: VariantId) -> Representability {
    for ty in db.field_types(id).values() {
        rtry!(representability_ty(db, ty.ty().instantiate_identity().skip_norm_wip()));
    }
    Representability::Representable
}

fn representability_ty<'db>(db: &'db Db, ty: Ty<'db>) -> Representability {
    match ty.kind() {
        TyKind::Adt(adt_id, args) => representability_adt_ty(db, adt_id.def_id(), args),
        // FIXME(#11924) allow zero-length arrays?
        TyKind::Array(ty, _) => representability_ty(db, ty),
        TyKind::Tuple(tys) => {
            for ty in tys {
                rtry!(representability_ty(db, ty));
            }
            Representability::Representable
        }
        _ => Representability::Representable,
    }
}

fn representability_adt_ty<'db>(
    db: &'db Db,
    def_id: AdtId,
    args: GenericArgs<'db>,
) -> Representability {
    rtry!(representability(db, def_id));

    // At this point, we know that the item of the ADT type is representable;
    // but the type parameters may cause a cycle with an upstream type
    let params_in_repr = params_in_repr(db, def_id);
    for (i, arg) in args.iter().enumerate() {
        if let GenericArgKind::Type(ty) = arg.kind()
            && params_in_repr.contains(i)
        {
            rtry!(representability_ty(db, ty));
        }
    }
    Representability::Representable
}

/// Which of an ADT's generic parameters, by index, appear in its representation: a bitset, the
/// first 64 parameters in its one inline word, which allocates nothing for nearly every ADT.
type ParamsInRepr = Bits<SmallVec<[u64; 1]>>;

const _: () = assert!(size_of::<ParamsInRepr>() == 24);

/// A [`ParamsInRepr`] while its ADT's fields are walked, its words in a frame.
struct ParamsInReprBuilder<'f> {
    words: Bits<Vec<u64, &'f Frame>>,
}

impl<'f> ParamsInReprBuilder<'f> {
    fn new(frame: &'f Frame) -> Self {
        ParamsInReprBuilder { words: Bits::from_words(Vec::new_in(frame)) }
    }

    fn insert(&mut self, index: usize) {
        self.words.insert_growing(index);
    }

    /// The set, inline up to 64 parameters, else its words in one allocation of their number.
    fn finish(self) -> ParamsInRepr {
        Bits::from_words(SmallVec::from_slice(self.words.words()))
    }
}

fn params_in_repr_cycle(_db: &Db, _: blaze_query::db::Id, _def_id: AdtId) -> ParamsInRepr {
    ParamsInRepr::default()
}

// This is a query because a chain of types that each hold the one before several times would
// otherwise be walked once per path through it.
#[blaze_query::tracked(cycle_result = params_in_repr_cycle, returns(ref), mem_tag = Infer)]
fn params_in_repr(db: &Db, def_id: AdtId) -> ParamsInRepr {
    blaze_mem::frame::current(|frame| {
        let mut params_in_repr = ParamsInReprBuilder::new(frame);
        let mut handle_variant = |variant| {
            for field in db.field_types(variant).values() {
                params_in_repr_ty(
                    db,
                    field.ty().instantiate_identity().skip_norm_wip(),
                    &mut params_in_repr,
                );
            }
        };
        match def_id {
            AdtId::StructId(def_id) => handle_variant(def_id.into()),
            AdtId::UnionId(def_id) => handle_variant(def_id.into()),
            AdtId::EnumId(def_id) => {
                for &(variant, ..) in def_id.enum_variants(db).variants.values() {
                    handle_variant(variant.into());
                }
            }
        }
        params_in_repr.finish()
    })
}

fn params_in_repr_ty<'db>(
    db: &'db Db,
    ty: Ty<'db>,
    params_in_repr: &mut ParamsInReprBuilder<'_>,
) {
    match ty.kind() {
        TyKind::Adt(adt, args) => {
            let inner_params_in_repr = self::params_in_repr(db, adt.def_id());
            for (i, arg) in args.iter().enumerate() {
                if let GenericArgKind::Type(ty) = arg.kind()
                    && inner_params_in_repr.contains(i)
                {
                    params_in_repr_ty(db, ty, params_in_repr);
                }
            }
        }
        TyKind::Array(ty, _) => params_in_repr_ty(db, ty, params_in_repr),
        TyKind::Tuple(tys) => tys.iter().for_each(|ty| params_in_repr_ty(db, ty, params_in_repr)),
        TyKind::Param(param) => params_in_repr.insert(param.index as usize),
        _ => {}
    }
}

#[cfg(test)]
mod tests {
    use super::{ParamsInRepr, ParamsInReprBuilder};

    #[test]
    fn params_in_repr_is_a_bitset_past_one_word() {
        let set: ParamsInRepr = blaze_mem::frame::task(blaze_mem::Tag::Infer, |frame| {
            let mut set = ParamsInReprBuilder::new(frame);
            for index in [0, 5, 63, 64, 130] {
                set.insert(index);
            }
            set.finish()
        });
        let set_bits: Vec<usize> = (0..200).filter(|&it| set.contains(it)).collect();
        assert_eq!(set_bits, [0, 5, 63, 64, 130]);
        assert_eq!(set.words().len(), 3);
    }

    #[test]
    fn params_in_repr_of_few_params_allocates_nothing() {
        let set = blaze_mem::frame::task(blaze_mem::Tag::Infer, |frame| {
            let mut set = ParamsInReprBuilder::new(frame);
            set.insert(3);
            set.insert(40);
            set.finish()
        });
        assert!(set.contains(3) && set.contains(40) && !set.contains(4) && !set.contains(64));
        assert!(!set.into_words().spilled());
    }
}
