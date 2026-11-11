//! `#[blaze::program_lifetime]` (docs/v2/plan/design/facts.md "ProgramLifetime"): a type
//! whose instances live until the program exits. The fact has two halves, and this is the
//! half a type's shape decides:
//!
//! - [`shape`], here: nothing in a value of the type is ever dropped or outlived. The type
//!   has no parameter, no `Drop`, and every field is plain data, a `&'static` reference, or a
//!   type that passes the same check (one declared `#[blaze::program_lifetime]` is taken at
//!   its own check). This is a function of the type's definition alone: what
//!   `core::mem::type_info` says of it. It is written over the front end's types only until
//!   reflection's phase 2 is on main (`lang-int-1110`), when it becomes a `const fn` over
//!   `type_info` behind a trait with a blanket impl (rule `reflection-first`), and this
//!   function is deleted.
//! - where instances are made, the lint's (`blaze::program_lifetime` in blaze-lints): every
//!   value of the type is built as the argument of the program arena's `alloc`, and the type
//!   is not another crate's to name unless its crate opts in. No shape says that: it is a
//!   fact about allocation, so it stays a declared, checked attribute.
//!
//! A type that passes [`shape`] has no drop glue by construction: nothing in it has a
//! `Drop`, so `needs_drop` is false of it with no special case in the compiler.

use base_db::Db;
use blaze_hash::HashSet;
use hir_def::{
    AdtId, HasModule, VariantId,
    attrs::AttrFlags,
    hir::generics::GenericParams,
    signatures::{EnumSignature, StructSignature, UnionSignature},
};
use hir_ty::{
    db::HirDatabase,
    drop::destructor,
    next_solver::{DbInterner, GenericArgs, RegionKind, Ty, TyKind},
};
use hir_ty_mir::mir::dispatch::variants;
use rustc_type_ir::inherent::{GenericArgs as _, IntoKind as _};
use std::alloc::Global;

use crate::walk::Shown;

/// The rule a type broke.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum Why {
    /// The type has a type, const or lifetime parameter: what an instance holds is not the
    /// definition's to say.
    Generic,
    /// The type has a `Drop` impl: its instances are never dropped, so it would never run.
    Drops,
    /// A field's type has a `Drop` impl (a `Box`, a `Vec`, an `Arc`): the same.
    FieldDrops,
    /// A field borrows for less than `'static`: the instance would outlive what it borrows.
    Borrows,
    /// A field's type is one the check can't see into (a `dyn`, a slice, a closure, an
    /// opaque or projected type).
    Unseen,
}

impl Why {
    /// The rule's name, for JSON and tests.
    pub const fn name(self) -> &'static str {
        match self {
            Why::Generic => "generic",
            Why::Drops => "drops",
            Why::FieldDrops => "field_drops",
            Why::Borrows => "borrows",
            Why::Unseen => "unseen",
        }
    }

    const fn said(self) -> &'static str {
        match self {
            Why::Generic => "has a generic parameter",
            Why::Drops => "has a `Drop` impl, which would never run",
            Why::FieldDrops => "holds a value with a `Drop` impl, which would never run",
            Why::Borrows => "borrows for less than `'static`",
            Why::Unseen => "holds a type the check can't see into",
        }
    }
}

/// Why a type's shape cannot be program-lifetime.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Refused {
    /// The declared type, by name.
    pub ty: Box<str>,
    pub why: Why,
    /// The fields from the declared type down to the one that breaks the rule, by name
    /// (`names.table`); empty when the type itself does.
    pub path: Box<str>,
    /// The type of that field, as written; empty when the type itself breaks the rule.
    pub of: Box<str>,
}

impl Refused {
    /// "`Tree` is `#[blaze::program_lifetime]`, but its field `nodes.buf` (`Vec<Node>`) holds
    /// a value with a `Drop` impl, which would never run".
    pub fn said(&self) -> String {
        // alloc: said once, when the lint reports it
        if self.path.is_empty() {
            return format!(
                "`{}` is `#[blaze::program_lifetime]`, but {}",
                self.ty,
                self.why.said()
            );
        }
        format!(
            "`{}` is `#[blaze::program_lifetime]`, but its field `{}` (`{}`) {}",
            self.ty,
            self.path,
            self.of,
            self.why.said()
        )
    }
}

/// The name of `adt` as its definition writes it.
pub fn adt_name(db: &Db, adt: AdtId) -> Box<str> {
    let name = match adt {
        AdtId::StructId(it) => &StructSignature::of(db, it).name,
        AdtId::EnumId(it) => &EnumSignature::of(db, it).name,
        AdtId::UnionId(it) => &UnionSignature::of(db, it).name,
    };
    name.symbol().as_str().into()
}

/// Whether `adt` is declared `#[blaze::program_lifetime]`.
pub fn declared(db: &Db, adt: AdtId) -> bool {
    AttrFlags::query(db, adt.into()).contains(AttrFlags::BLAZE_PROGRAM_LIFETIME)
}

/// Why the shape of `adt` cannot be program-lifetime, or `None` when it can: the half of the
/// fact reflection states (the module docs say which).
#[blaze_query::tracked(returns(ref))]
pub fn shape(db: &Db, adt: AdtId) -> Option<Refused> {
    let refuse = |why, path: &str, of: String| {
        Some(Refused { ty: adt_name(db, adt), why, path: path.into(), of: of.into() })
    };
    if !GenericParams::of(db, adt.into()).is_empty() {
        return refuse(Why::Generic, "", String::new());
    }
    if destructor(db, adt).is_some() {
        return refuse(Why::Drops, "", String::new());
    }
    let krate = HasModule::krate(&adt, db);
    let interner = DbInterner::new_with(db, krate);
    let mut walk = Walk {
        db,
        interner,
        seen: HashSet::new_in(Global),
        path: String::new(), // alloc: the path of a refusal, a few names
        found: None,
    };
    walk.fields(adt, GenericArgs::identity_for_item(interner, adt.into()));
    let (why, path, of) = walk.found?;
    refuse(why, &path, Shown::new(db, krate, of).to_string())
}

struct Walk<'db> {
    db: &'db Db,
    interner: DbInterner<'db>,
    /// The types looked into, each once: a type that holds itself behind a `&'static` is
    /// never walked through the reference, and one that holds itself by value has no layout.
    seen: HashSet<Ty<'db>, Global>,
    /// The fields down to the one being looked at, by name.
    path: String,
    /// The first field that breaks a rule: the rule, its path and its type.
    found: Option<(Why, String, Ty<'db>)>,
}

impl<'db> Walk<'db> {
    fn refuse(&mut self, why: Why, ty: Ty<'db>) {
        if self.found.is_none() {
            // clone: the path kept in the refusal, once
            self.found = Some((why, self.path.clone(), ty));
        }
    }

    fn fields(&mut self, adt: AdtId, args: GenericArgs<'db>) {
        for variant in variants(self.db, adt) {
            let names = variant.fields(self.db).fields();
            for (id, field) in self.db.field_types(variant).iter() {
                if self.found.is_some() {
                    return;
                }
                let ty = field.ty().instantiate(self.interner, args).skip_norm_wip();
                let before = self.path.len();
                if before != 0 {
                    self.path.push('.');
                }
                self.path.push_str(names[id].name.symbol().as_str());
                self.ty(ty);
                self.path.truncate(before);
            }
        }
    }

    /// `ty`, held by value in the type.
    fn ty(&mut self, ty: Ty<'db>) {
        if self.found.is_some() || !self.seen.insert(ty) {
            return;
        }
        match ty.kind() {
            TyKind::Bool
            | TyKind::Char
            | TyKind::Int(_)
            | TyKind::Uint(_)
            | TyKind::Float(_)
            | TyKind::Never
            // Not owned: a raw pointer or a function pointer drops nothing and borrows
            // nothing the type system holds it to.
            | TyKind::RawPtr(..)
            | TyKind::FnPtr(..)
            | TyKind::FnDef(..) => {}
            // What it points to is not part of the value: it lives as long as the program
            // whatever it is.
            TyKind::Ref(region, _, _) => {
                if !matches!(region.kind(), RegionKind::ReStatic) {
                    self.refuse(Why::Borrows, ty);
                }
            }
            TyKind::Tuple(fields) => {
                for field in fields.iter() {
                    self.ty(field);
                }
            }
            TyKind::Array(element, _) | TyKind::Pat(element, _) => self.ty(element),
            TyKind::Adt(def, args) => {
                let id = def.def_id();
                // Declared itself: its own check holds it, parameters and all.
                if declared(self.db, id) {
                    return;
                }
                if destructor(self.db, id).is_some() {
                    return self.refuse(Why::FieldDrops, ty);
                }
                self.fields(id, args);
            }
            _ => self.refuse(Why::Unseen, ty),
        }
    }
}
