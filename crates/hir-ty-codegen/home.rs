//! The crate an instance is compiled from the point of view of.
//!
//! An instance is asked for by a function of some crate, but what it compiles to depends only
//! on what its own name mentions: the function it instantiates and the types it is
//! instantiated with. Compiling it as the crate that asked would make `Vec<u8>::push` depend
//! on every file of the workspace crate that happened to use it, and give it one cache entry
//! per crate that does.

#![forbid(unsafe_code)]

use std::sync::LazyLock;

use base_db::{Crate, Db, FileId};
use blaze_hash::HashSet;
use blaze_query::db::FromId as _;
use hir_def::{CallableDefId, DefWithBodyId, FunctionId, HasModule, Lookup};
use rustc_type_ir::{
    AliasTy, AliasTyKind, TypeSuperVisitable, TypeVisitable, TypeVisitor,
    inherent::{BoundExistentialPredicates as _, IntoKind, SliceLike as _},
};
use smallvec::SmallVec;

use hir_ty::{
    ImplTraitId, InferBodyId,
    db::{HirDatabase, InternedClosureId, InternedCoroutineId},
    method_resolution::loaded_crates,
    next_solver::{DbInterner, Ty, TyKind},
    traits::{ParamEnvAndCrate, StoredParamEnvAndCrate},
};

use super::{Instance, InstanceDef};

/// The crates the name of an instance mentions.
struct Mentions<'db> {
    db: &'db Db,
    crates: HashSet<Crate>,
    /// The bodies it has the code of or mentions a type of.
    bodies: SmallVec<[DefWithBodyId; 4]>,
    /// Of those, the ones it mentions a coroutine of, whose layout is of the whole body.
    coroutines: SmallVec<[DefWithBodyId; 4]>,
    /// Whether an `impl Trait` is looked through to the type its body gives it, and a
    /// projection left as it is written, rather than either told apart as `opaque`.
    reveal: bool,
    /// How many `impl Trait`s the type being looked at is inside of.
    revealing: u32,
    /// The bodies that give the `impl Trait`s it mentions their types, and those of the
    /// closures in those types: its name says none of what they wrote.
    hidden: SmallVec<[DefWithBodyId; 4]>,
    /// It mentions something whose crate we can't tell.
    opaque: bool,
    /// The types looked into so far. The type of a future holds those of the futures it
    /// awaits, each of which holds its own, so the same few are reached along very many ways.
    seen: HashSet<Ty<'db>>,
}

impl<'db> Mentions<'db> {
    fn of(db: &'db Db, instance: &Instance) -> Mentions<'db> {
        Mentions::looking(db, instance, false)
    }

    fn looking(db: &'db Db, instance: &Instance, reveal: bool) -> Mentions<'db> {
        let mut mentions = Mentions {
            db,
            crates: HashSet::default(),
            bodies: SmallVec::new(),
            coroutines: SmallVec::new(),
            reveal,
            revealing: 0,
            hidden: SmallVec::new(),
            opaque: false,
            seen: HashSet::default(),
        };
        match instance.def {
            InstanceDef::Fn(func) | InstanceDef::VtableShim(func) => {
                mentions.crates.insert(func.krate(db));
                mentions.bodies.push(func.into());
            }
            InstanceDef::Closure(id) => {
                mentions.owner(InternedClosureId::from_id(id).loc(db).owner)
            }
            InstanceDef::Coroutine(id) | InstanceDef::CoroutineByMove(id) => {
                mentions.owner(InternedCoroutineId::from_id(id).loc(db).owner)
            }
            InstanceDef::DropGlue
            | InstanceDef::CallOnceShim
            | InstanceDef::FnPointerShim
            | InstanceDef::ReifyShim { .. } => {}
        }
        for arg in instance.args.as_ref().iter() {
            arg.visit_with(&mut mentions);
        }
        mentions
    }

    fn owner(&mut self, owner: InferBodyId<'_>) {
        let InferBodyId::DefWithBodyId(owner) = owner else {
            self.opaque = true;
            return;
        };
        let krate = match owner {
            DefWithBodyId::FunctionId(it) => it.krate(self.db),
            DefWithBodyId::ConstId(it) => it.krate(self.db),
            DefWithBodyId::StaticId(it) => it.krate(self.db),
            DefWithBodyId::VariantId(it) => it.krate(self.db),
        };
        self.crates.insert(krate);
        self.bodies.push(owner);
    }
}

/// The function the code of an instance is written in: its own, or the one a closure or an
/// `async` block is in. The symbol of every copy of the instance has its name.
pub(super) fn written_in(db: &Db, instance: &Instance) -> Option<FunctionId> {
    let owner = match instance.def {
        InstanceDef::Fn(func) => return Some(func),
        InstanceDef::Closure(id) => InternedClosureId::from_id(id).loc(db).owner,
        InstanceDef::Coroutine(id) | InstanceDef::CoroutineByMove(id) => {
            InternedCoroutineId::from_id(id).loc(db).owner
        }
        InstanceDef::DropGlue
        | InstanceDef::CallOnceShim
        | InstanceDef::VtableShim(_)
        | InstanceDef::FnPointerShim
        | InstanceDef::ReifyShim { .. } => return None,
    };
    match owner {
        InferBodyId::DefWithBodyId(DefWithBodyId::FunctionId(func)) => Some(func),
        _ => None,
    }
}

/// The file a body is written in, which for one a macro makes is where the macro is called.
pub fn body_file(db: &Db, body: DefWithBodyId) -> FileId {
    let file = match body {
        DefWithBodyId::FunctionId(it) => it.lookup(db).id.file_id,
        DefWithBodyId::ConstId(it) => it.lookup(db).id.file_id,
        DefWithBodyId::StaticId(it) => it.lookup(db).id.file_id,
        DefWithBodyId::VariantId(it) => it.lookup(db).parent.lookup(db).id.file_id,
    };
    file.original_file_respecting_includes(db).file_id(db)
}

impl<'db> TypeVisitor<DbInterner<'db>> for Mentions<'db> {
    type Result = ();

    fn visit_ty(&mut self, ty: Ty<'db>) {
        if !self.seen.insert(ty) {
            return;
        }
        match ty.kind() {
            TyKind::Adt(adt, _) => {
                self.crates.insert(adt.def_id().module(self.db).krate(self.db));
            }
            TyKind::FnDef(def, _) => {
                let krate = match def.0 {
                    CallableDefId::FunctionId(it) => {
                        self.bodies.push(it.into());
                        it.krate(self.db)
                    }
                    CallableDefId::StructId(it) => it.krate(self.db),
                    CallableDefId::EnumVariantId(it) => it.krate(self.db),
                };
                self.crates.insert(krate);
            }
            TyKind::Closure(id, _) => {
                let known = self.bodies.len();
                self.owner(id.0.loc(self.db).owner);
                if self.revealing > 0 {
                    self.hidden.extend(self.bodies.get(known).copied());
                }
            }
            TyKind::CoroutineClosure(id, _) => {
                self.owner(id.0.loc(self.db).owner);
                self.coroutines.extend(self.bodies.last().copied());
            }
            TyKind::Coroutine(id, _) => {
                self.owner(id.0.loc(self.db).owner);
                self.coroutines.extend(self.bodies.last().copied());
            }
            TyKind::Dynamic(bounds, _) => {
                if let Some(principal) = bounds.principal() {
                    self.crates.insert(principal.skip_binder().def_id.0.krate(self.db));
                }
            }
            TyKind::Alias(_, AliasTy { kind: AliasTyKind::Opaque { def_id }, args, .. })
                if self.reveal =>
            {
                let ImplTraitId::ReturnTypeImplTrait(of, _) = def_id.0.loc(self.db) else {
                    self.opaque = true;
                    return;
                };
                self.hidden.push(of.into());
                let interner = DbInterner::new_no_crate(self.db);
                let hidden = rustc_type_ir::Interner::type_of(interner, def_id.into())
                    .instantiate(interner, args)
                    .skip_norm_wip();
                if matches!(hidden.kind(), TyKind::Error(_)) {
                    self.opaque = true;
                    return;
                }
                self.revealing += 1;
                self.visit_ty(hidden);
                self.revealing -= 1;
            }
            // What no `impl` gave a type is the type its name spells.
            TyKind::Alias(_, AliasTy { kind: AliasTyKind::Projection { .. }, .. }) if self.reveal => {}
            TyKind::Alias(..) | TyKind::Foreign(..) => self.opaque = true,
            _ => {}
        }
        ty.super_visit_with(self)
    }
}

/// `BLAZE_NO_HOME`: every instance is compiled in the environment of the function that asks
/// for it. Read once, as [`Homes::home`] is asked for each instance a function calls.
static NO_HOME: LazyLock<bool> = LazyLock::new(|| std::env::var_os("BLAZE_NO_HOME").is_some());

/// What `krate` can see: itself and the crates whose impls it knows ([`loaded_crates`]),
/// sorted to be probed by binary search. One memo for every task that asks.
#[blaze_query::tracked(returns(deref))]
fn sees(db: &Db, krate: Crate) -> Box<[Crate]> {
    // alloc: the memo's result, one exact block
    let mut sees: Box<[Crate]> = loaded_crates(db, krate).into();
    sees.sort_unstable();
    sees
}

/// Picks the crate each instance is compiled as. It holds nothing: what a crate sees is the
/// database's memo ([`sees`]).
#[derive(Default)]
pub(super) struct Homes;

impl Homes {
    /// The environment to compile `instance` in when a function compiled in `asked` asks for
    /// it: that of the one crate it mentions that sees all the others, which every crate
    /// asking for the instance agrees on. Without such a crate, the asking one's.
    pub(super) fn home(
        &mut self,
        db: &Db,
        instance: &Instance,
        asked: &StoredParamEnvAndCrate,
    ) -> StoredParamEnvAndCrate {
        if *NO_HOME || !asked.param_env(db).clauses().is_empty()
        {
            // clone: a refcount of the interned clauses; callers own the environment they get
            return asked.clone();
        }
        let mentions = Mentions::of(db, instance);
        if mentions.opaque {
            // clone: a refcount of the interned clauses; callers own the environment they get
            return asked.clone();
        }
        let home = mentions.crates.iter().copied().find(|&krate| {
            let sees = sees(db, krate);
            mentions.crates.iter().all(|it| sees.binary_search(it).is_ok())
        });
        match home {
            Some(krate) => ParamEnvAndCrate { krate, ..asked.as_ref(db) }.store(),
            // clone: a refcount of the interned clauses; callers own the environment they get
            None => asked.clone(),
        }
    }
}
