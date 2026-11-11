//! blaze's error unions (`#![blaze::error_unions]`): the type `A | B` is core's
//! `#[lang = "error_union"]` struct over the tuple of its members, `ErrorUnion<(A, B)>`, the
//! members in one canonical order, so that `A | B`, `B | A` and `A | A | B` are one type.
//!
//! The order is by each member's identity, a hash of what names it the same in every database
//! and every build: the crate's content ID, the names of the modules from the crate root to the
//! item, the item's name, and its arguments' identities in turn. Never an interned ID, an
//! address or a map's order (`deterministic-output`). A member that is itself a union gives its
//! members (`A | (B | C)` is `A | B | C`), a member written twice is one, and a union left with
//! one member is that member.
//!
//! Members are known types: a type parameter, `impl Trait`, `_` and the like are refused
//! ([`ErrorUnionRefusal`]), so a union's members and their order are known where it is written.
//! The library (core's `error_union` module) gives the unions their traits.

use blaze_ids::{ContentId, Hasher};
use hir_def::{
    AdtId, HasModule,
    signatures::{EnumSignature, StructSignature, UnionSignature},
    type_ref::{TypeRef, TypeRefId},
};
use rustc_type_ir::{
    TyKind, TypeVisitableExt,
    inherent::{GenericArgs as _, IntoKind as _, Ty as _},
};

use super::TyLoweringContext;
use crate::{
    ERROR_UNION_MAX_MEMBERS, ErrorUnionRefusal, TyLoweringDiagnostic,
    next_solver::{GenericArg, GenericArgKind, GenericArgs, Ty},
};

/// The domain of a member's identity.
const DOMAIN: &str = "blaze error union member 1";

impl<'db> TyLoweringContext<'db, '_> {
    /// The type `members[0] | members[1] | ..`, written at `source`: core's error union of
    /// the members in canonical order (see the module), or the error type once a refusal is
    /// reported.
    pub(super) fn lower_error_union(
        &mut self,
        source: TypeRefId,
        members: &[TypeRefId],
    ) -> Ty<'db> {
        let error = self.types.types.error;
        let refuse = |this: &mut Self, source, reason| {
            this.push_diagnostic(TyLoweringDiagnostic::ErrorUnionRefused { source, reason });
            error
        };
        if !base_db::crate_extensions(self.db, self.resolver.krate()).error_unions() {
            return refuse(self, source, ErrorUnionRefusal::NotEnabled);
        }
        let Some(lang) = self.lang_items.ErrorUnion else {
            return refuse(self, source, ErrorUnionRefusal::NoLangItem);
        };
        let mut keyed = Vec::new_in(self.frame);
        for &member in members {
            if let TypeRef::ImplTrait(_) | TypeRef::DynTrait(_) | TypeRef::Placeholder =
                self.store.type_ref(member)
            {
                return refuse(self, member, ErrorUnionRefusal::NotNamed);
            }
            let ty = self.lower_ty(member);
            if ty.references_error() {
                // Reported where it was lowered.
                return error;
            }
            let flattened = match ty.kind() {
                TyKind::Adt(def, args) if def.def_id() == AdtId::StructId(lang) => {
                    match args.types().next().map(|it| it.kind()) {
                        Some(TyKind::Tuple(inner)) => inner,
                        _ => return refuse(self, member, ErrorUnionRefusal::NotNamed),
                    }
                }
                _ => {
                    if let Err(reason) = self.member_identity(ty, true).map(|key| {
                        keyed.push((key, ty));
                    }) {
                        return refuse(self, member, reason);
                    }
                    continue;
                }
            };
            for inner in flattened.iter() {
                match self.member_identity(inner, true) {
                    Ok(key) => keyed.push((key, inner)),
                    Err(reason) => return refuse(self, member, reason),
                }
            }
        }
        keyed.sort_by(|a, b| a.0.cmp(&b.0));
        let mut canonical = Vec::with_capacity_in(keyed.len(), self.frame);
        for (at, &(key, ty)) in keyed.iter().enumerate() {
            match at.checked_sub(1).map(|before| keyed[before]) {
                Some((same, written)) if same == key && written == ty => {}
                Some((same, _)) if same == key => {
                    return refuse(self, source, ErrorUnionRefusal::Ambiguous);
                }
                _ => canonical.push(ty),
            }
        }
        match canonical[..] {
            [] => error,
            [one] => one,
            _ if canonical.len() > ERROR_UNION_MAX_MEMBERS => {
                refuse(self, source, ErrorUnionRefusal::TooMany)
            }
            _ => {
                let interner = self.interner;
                let tuple = Ty::new_tup(interner, &canonical);
                let args = GenericArgs::new_from_slice(&[GenericArg::from(tuple)]);
                Ty::new_adt(interner, AdtId::StructId(lang), args)
            }
        }
    }

    /// The identity of `ty`, a member (`top`) or an argument of one, by which a union's
    /// members are ordered: see the module. A member is a struct, enum or union, or a scalar;
    /// an argument may also be a reference, a raw pointer, a slice, a tuple, `str` or `!`.
    /// Lifetimes are left out, so two members that differ only in them are
    /// [`ErrorUnionRefusal::Ambiguous`].
    fn member_identity(&self, ty: Ty<'db>, top: bool) -> Result<ContentId, ErrorUnionRefusal> {
        let mut hasher = Hasher::new(DOMAIN);
        self.hash_member(&mut hasher, ty, top)?;
        Ok(hasher.finish())
    }

    fn hash_member(
        &self,
        hasher: &mut Hasher,
        ty: Ty<'db>,
        top: bool,
    ) -> Result<(), ErrorUnionRefusal> {
        if ty.has_param() {
            return Err(ErrorUnionRefusal::TypeParam);
        }
        match ty.kind() {
            TyKind::Bool => _ = hasher.update(&[0]),
            TyKind::Char => _ = hasher.update(&[1]),
            TyKind::Int(it) => _ = hasher.update(&[2, it.code()]),
            TyKind::Uint(it) => _ = hasher.update(&[3, it.code()]),
            TyKind::Float(it) => _ = hasher.update(&[4, it as u8]),
            TyKind::Adt(def, args) => {
                hasher.update(&[5]);
                self.hash_item(hasher, def.def_id());
                hasher.update(&(args.len() as u64).to_le_bytes());
                for arg in args.iter() {
                    match arg.kind() {
                        GenericArgKind::Type(it) => self.hash_member(hasher, it, false)?,
                        GenericArgKind::Lifetime(_) => _ = hasher.update(&[0]),
                        GenericArgKind::Const(_) => return Err(ErrorUnionRefusal::NotNamed),
                    }
                }
            }
            _ if top => return Err(ErrorUnionRefusal::NotNamed),
            TyKind::Str => _ = hasher.update(&[6]),
            TyKind::Never => _ = hasher.update(&[7]),
            TyKind::Ref(_, it, mutability) => {
                hasher.update(&[8, mutability as u8]);
                self.hash_member(hasher, it, false)?;
            }
            TyKind::RawPtr(it, mutability) => {
                hasher.update(&[9, mutability as u8]);
                self.hash_member(hasher, it, false)?;
            }
            TyKind::Slice(it) => {
                hasher.update(&[10]);
                self.hash_member(hasher, it, false)?;
            }
            TyKind::Tuple(items) => {
                hasher.update(&[11]).update(&(items.len() as u64).to_le_bytes());
                for it in items.iter() {
                    self.hash_member(hasher, it, false)?;
                }
            }
            _ => return Err(ErrorUnionRefusal::NotNamed),
        }
        Ok(())
    }

    /// An ADT's part of an identity: its kind, its crate's content ID, the names of the modules
    /// from its crate's root to it, and its name.
    fn hash_item(&self, hasher: &mut Hasher, adt: AdtId) {
        let db = self.db;
        let (tag, name) = match adt {
            AdtId::StructId(it) => (b"struct".as_slice(), &StructSignature::of(db, it).name),
            AdtId::EnumId(it) => (b"enum".as_slice(), &EnumSignature::of(db, it).name),
            AdtId::UnionId(it) => (b"union".as_slice(), &UnionSignature::of(db, it).name),
        };
        let container = adt.module(db);
        hasher.part(tag).id(&container.krate(db).content(db));
        let mut module = Some(container);
        while let Some(at) = module {
            match at.name(db) {
                Some(name) => hasher.part(name.as_str().as_bytes()),
                None if at.block(db).is_some() => hasher.part(b"{block}"),
                None => hasher.part(b"{root}"),
            };
            module = at.containing_module(db);
        }
        hasher.part(name.as_str().as_bytes());
    }
}
