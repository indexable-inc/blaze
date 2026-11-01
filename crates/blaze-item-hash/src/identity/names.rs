//! Section 3.5: what every path of a body resolves to, and the traits in scope where it is
//! written.
//!
//! A path's scope is the innermost one of the expression it is in: its own for a path
//! expression, a record literal's path and an `asm!` `sym` operand, that of the expression
//! whose patterns and types it is in otherwise (a `let`'s are its block's, a closure's
//! parameters' the closure's). The body's parameters are at the body's own scope. Inference
//! and MIR building resolve each path at that scope or at one inside the same blocks, which
//! differs only in the locals it sees (a function of the body, which is content) and so in
//! what a name that is a local in one is in the other: for a path that resolves to a local,
//! what it names in the items of its scope is hashed too. A path of an anonymous constant is
//! resolved by the constant at the body's own scope, so it is hashed there as well.
//!
//! Each path is hashed as what the type namespace, then the value namespace, give it there (an
//! item by key and signature, a local by its index, a generic parameter by its owner's key and
//! index, a module by key, a builtin by name, the visibility the lookup gave), and for a path a
//! macro wrote what its definition site gives it in the type namespace: the fallback type
//! lowering takes. Once a path's prefix is a type the rest is type-relative: inference
//! resolves it through impls and traits, which are facts (section 5).
//!
//! A lifetime written by name is hashed as the generic parameter it resolves to, or by its
//! text where it resolves to none, so its name in the HIR's bytes can be left out.
//!
//! The traits in scope are hashed once per module a scope is in (the owner's, each block's
//! with items): the impl's trait, the blocks', the prelude's and the module's, as a sorted set
//! of (key, signature), which is what method calls and type-relative paths search.
//!
//! The body's own item (and a generic parameter of it) is hashed as "self", not by its key, so
//! that a rename keeps the identity. Each path's segments that every resolution hashed here
//! resolved are hashed by that alone: [`Names::blank`] tells the HIR's bytes to leave their
//! text out, so a rename of a local and a `use .. as` alias keep the identity too. The rest of
//! a path (type-relative segments, and all of one that resolves to nothing) stays text.

use base_db::Db;
use blaze_format::cursor::Put;
use blaze_hash::HashMap;
use blaze_items::{ItemId, key};
use blaze_mem::scratch;
use blaze_query::db::AsId;
use hir_def::{
    AdtId, DefWithBodyId, EnumVariantId, GenericDefId, ModuleDefId, ModuleId,
    builtin_type::BuiltinType,
    expr_store::{
        Body, ExpressionStore, HygieneId, StoreVisitor, StoreVisitorExt as _, path::Path,
    },
    hir::{
        AsmOperand, Expr, ExprId, InlineAsm, Pat, PatId,
        type_ref::{LifetimeRef, LifetimeRefId, TypeRef, TypeRefId},
    },
    item_hash::{mod_path_index, path_count},
    item_tree::VisibilityExplicitness,
    nameres::ResolvePathResultPrefixInfo,
    per_ns::PerNs,
    resolver::{HasResolver, LifetimeNs, ResolveValueResult, Resolver, TypeNs, ValueNs},
    visibility::Visibility,
};
use hir_expand::{mod_path::ModPath, name::AsName};
use la_arena::RawIdx;

use super::item::{Part, SELF, item_part, own_item, self_part, variant_part};

type R<T = ()> = Result<T, &'static str>;

/// What `def`'s paths resolve to, computed once for its identity and for its records' keys
/// ([`crate::keys`]).
#[derive(PartialEq, Eq, Debug)]
pub(crate) struct Names {
    /// What each path resolves to where it is written.
    pub(crate) names: Box<[u8]>,
    /// The traits in scope of each module a scope of the body is in.
    pub(crate) traits: Box<[u8]>,
    /// For each path of the body's store (`hir_def::item_hash::mod_path_index`), how many of
    /// its leading segments `names` hashes by what they resolve to: the HIR's bytes leave
    /// their text out.
    pub(crate) blank: Box<[u16]>,
    /// Each item or variant a path resolves to, in the order `names` first writes it: what
    /// "the target of path `i`" of a record key is.
    pub(crate) targets: Box<[Target]>,
}

/// What a path of a body resolves to that a record may name.
#[derive(Clone, Copy, PartialEq, Eq, Hash, Debug)]
pub(crate) enum Target {
    Item(ItemId),
    Variant(EnumVariantId),
}

/// [`Names`] of `def`'s body.
#[blaze_query::tracked(returns(ref))]
pub(crate) fn names_of(db: &Db, def: DefWithBodyId) -> Result<Names, &'static str> {
    let body = Body::of(db, def);
    let mut names = scratch::lend_bytes();
    let mut traits = scratch::lend_bytes();
    // alloc: one count per path of the body and one entry per target, once per identity
    let mut blank = vec![u16::MAX; path_count(body)];
    // alloc: the targets, at most one per path, and the set that keeps each once
    let mut targets = Vec::with_capacity(blank.len());
    let mut seen = HashMap::default();
    write(db, def, body, &mut names, &mut traits, &mut blank, &mut targets, &mut seen)?;
    for it in &mut blank {
        if *it == u16::MAX {
            *it = 0;
        }
    }
    Ok(Names {
        // alloc: the query's value, once per identity
        names: names.as_slice().into(),
        traits: traits.as_slice().into(),
        blank: blank.into_boxed_slice(),
        targets: targets.into_boxed_slice(),
    })
}

/// The names of `body` into `names`, its traits in scope into `traits`, the segments of each
/// path hashed by resolution into `blank` (`u16::MAX` for a path not walked) and the items
/// its paths name into `targets`.
#[expect(clippy::too_many_arguments, reason = "the parts of `Names`, each its own buffer")]
fn write(
    db: &Db,
    def: DefWithBodyId,
    body: &Body,
    names: &mut Vec<u8>,
    traits: &mut Vec<u8>,
    blank: &mut [u16],
    targets: &mut Vec<Target>,
    seen: &mut HashMap<Target, ()>,
) -> R {
    let store: &ExpressionStore = body;
    let root = body.root_expr();
    let base = def.resolver(db);
    let mut resolver = def.resolver(db);
    let mut walk = Walk {
        out: Out {
            db,
            bytes: names,
            own: own_item(def),
            targets,
            seen,
            resolved: usize::MAX,
        },
        traits: Traits { db, out: traits, seen: scratch::lend_u32s() },
        pats: scratch::lend_u32s(),
        types: scratch::lend_u32s(),
        lifetimes: scratch::lend_u32s(),
        blank,
    };
    walk.traits.scope(&base)?;
    for param in body.params.iter() {
        walk.pats.push(param.formal.into_raw().into_u32());
        walk.pats.push(param.user_written.into_raw().into_u32());
    }
    walk.drain(store, &base, None)?;
    for (expr, it) in store.exprs() {
        let guard = resolver.update_to_inner_scope(db, def, expr);
        walk.traits.scope(&resolver)?;
        let also = (store.find_root_for_expr(expr) != root).then_some(&base);
        walk.expr(store, &resolver, also, expr, it)?;
        resolver.reset_to_guard(guard);
    }
    Ok(())
}

struct Walk<'a, 'db> {
    out: Out<'a, 'db>,
    traits: Traits<'a, 'db>,
    /// The patterns, types and lifetimes of the expression being walked, still to walk.
    pats: scratch::Lent<Vec<u32>>,
    types: scratch::Lent<Vec<u32>>,
    lifetimes: scratch::Lent<Vec<u32>>,
    blank: &'a mut [u16],
}

impl Walk<'_, '_> {
    /// The paths of `expr` itself, then of the patterns and types it holds (not of the
    /// expressions it holds, which are walked at their own scopes).
    fn expr(
        &mut self,
        store: &ExpressionStore,
        at: &Resolver<'_>,
        also: Option<&Resolver<'_>>,
        id: ExprId,
        expr: Expr<'_>,
    ) -> R {
        match expr {
            Expr::Path(path) => self.path(store, at, also, path, store.expr_path_hygiene(id))?,
            Expr::RecordLit { path, .. } => {
                self.path(store, at, also, path, store.expr_path_hygiene(id))?
            }
            Expr::InlineAsm(InlineAsm { operands, .. }) => {
                for (_, operand) in operands.iter() {
                    if let AsmOperand::Sym(path) = *operand {
                        let path = store.path_at(path);
                        self.path(store, at, also, path, store.expr_path_hygiene(id))?;
                        self.gather().on_path(path);
                    }
                }
            }
            _ => {}
        }
        store.visit_expr_children(id, self.gather());
        self.drain(store, at, also)
    }

    /// Walks the patterns and types gathered so far, and those they hold.
    fn drain(
        &mut self,
        store: &ExpressionStore,
        at: &Resolver<'_>,
        also: Option<&Resolver<'_>>,
    ) -> R {
        loop {
            if let Some(pat) = self.pats.pop() {
                let pat = PatId::from_raw(RawIdx::from_u32(pat));
                match store.pat(pat) {
                    Pat::Path(path)
                    | Pat::TupleStruct { path, .. }
                    | Pat::Record { path, .. } => {
                        self.path(store, at, also, path, store.pat_path_hygiene(pat))?
                    }
                    _ => {}
                }
                let collect = self.gather();
                store.visit_pat_children(pat, collect);
            } else if let Some(ty) = self.types.pop() {
                let ty = TypeRefId::from_raw(RawIdx::from_u32(ty));
                if let TypeRef::Path(path) = store.type_ref(ty) {
                    self.path(store, at, also, path, store.type_path_hygiene(ty))?;
                }
                let collect = self.gather();
                store.visit_type_ref_children(ty, collect);
            } else if let Some(lifetime) = self.lifetimes.pop() {
                self.lifetime(at, &store[LifetimeRefId::from_raw(RawIdx::from_u32(lifetime))]);
            } else {
                return Ok(());
            }
        }
    }

    fn gather(&mut self) -> Collect<'_> {
        Collect { pats: &mut self.pats, types: &mut self.types, lifetimes: &mut self.lifetimes }
    }

    /// A lifetime written by name, as what it resolves to at `at`: a generic parameter as
    /// [`Out::param`], so that a rename keeps the identity; one that resolves to nothing (a
    /// `for<..>` binder's) by its text. The others are the HIR's bytes alone.
    fn lifetime(&mut self, at: &Resolver<'_>, lifetime: &LifetimeRef) {
        let LifetimeRef::Named(name) = lifetime else { return };
        self.out.tag(0xfe);
        match at.resolve_lifetime(lifetime) {
            Some(LifetimeNs::LifetimeParam(it)) => {
                self.out.tag(1);
                self.out.param(it.parent, it.local_id.into_raw().into_u32());
            }
            Some(LifetimeNs::Static) => self.out.tag(2),
            None => {
                self.out.tag(0);
                self.out.bytes(name.as_str().as_bytes());
            }
        }
    }

    fn path(
        &mut self,
        store: &ExpressionStore,
        at: &Resolver<'_>,
        also: Option<&Resolver<'_>>,
        path: Path<'_>,
        hygiene: HygieneId,
    ) -> R {
        self.out.resolved = usize::MAX;
        self.out.resolutions(at, path, hygiene, true)?;
        if let Some(base) = also {
            self.out.tag(0xff);
            self.out.resolutions(base, path, hygiene, false)?;
        }
        if let Some(index) = mod_path_of(path).and_then(|it| mod_path_index(store, it)) {
            let resolved = match self.out.resolved {
                usize::MAX => 0,
                n => u16::try_from(n).unwrap_or(u16::MAX - 1),
            };
            let blank = &mut self.blank[index];
            *blank = (*blank).min(resolved);
        }
        Ok(())
    }
}

/// Gathers the patterns, types and lifetimes an expression, pattern or type holds.
struct Collect<'a> {
    pats: &'a mut Vec<u32>,
    types: &'a mut Vec<u32>,
    lifetimes: &'a mut Vec<u32>,
}

impl StoreVisitor for Collect<'_> {
    fn on_expr(&mut self, _: ExprId) {}

    fn on_anon_const_expr(&mut self, _: ExprId) {}

    fn on_pat(&mut self, pat: PatId) {
        self.pats.push(pat.into_raw().into_u32());
    }

    fn on_type(&mut self, ty: TypeRefId) {
        self.types.push(ty.into_raw().into_u32());
    }

    fn on_lifetime(&mut self, lifetime: LifetimeRefId) {
        self.lifetimes.push(lifetime.into_raw().into_u32());
    }
}

/// The traits in scope of each module a scope of the body is in, each module once.
struct Traits<'a, 'db> {
    db: &'db Db,
    out: &'a mut Vec<u8>,
    seen: scratch::Lent<Vec<u32>>,
}

impl Traits<'_, '_> {
    fn scope(&mut self, resolver: &Resolver<'_>) -> R {
        let module = resolver.module();
        let bits = module.as_id().as_u32();
        if self.seen.contains(&bits) {
            return Ok(());
        }
        self.seen.push(bits);
        let db = self.db;
        let mut parts = scratch::lend_bytes();
        let mut failed = None;
        resolver.for_each_trait_in_scope(db, |it| match item_part(db, ItemId::Trait(it)) {
            Ok(part) => parts.extend_from_slice(&part),
            Err(why) => failed = failed.or(Some(why)),
        });
        if let Some(why) = failed {
            return Err(why);
        }
        let (parts, _) = parts.as_chunks_mut::<{ size_of::<Part>() }>();
        parts.sort_unstable();
        let mut kept = 0;
        for at in 0..parts.len() {
            if kept == 0 || parts[kept - 1] != parts[at] {
                parts[kept] = parts[at];
                kept += 1;
            }
        }
        self.out.extend_from_slice(&key::module_key(db, module).0.0);
        self.out.extend_from_slice(&(kept as u64).to_le_bytes());
        for part in &parts[..kept] {
            self.out.extend_from_slice(part);
        }
        Ok(())
    }
}

fn mod_path_of(path: Path<'_>) -> Option<&ModPath> {
    match path {
        Path::BarePath(it) => Some(it),
        Path::Normal(it) => Some(it.mod_path),
        Path::LangItem(..) => None,
    }
}

/// The bytes resolutions are written as.
struct Out<'a, 'db> {
    db: &'db Db,
    bytes: &'a mut Vec<u8>,
    /// The body's own item, written as "self".
    own: Option<ItemId>,
    targets: &'a mut Vec<Target>,
    seen: &'a mut HashMap<Target, ()>,
    /// The fewest leading segments any resolution of the path being written resolved,
    /// `usize::MAX` while none has.
    resolved: usize,
}

impl Out<'_, '_> {
    fn tag(&mut self, tag: u8) {
        self.bytes.push(tag);
    }

    fn u64(&mut self, n: u64) {
        self.bytes.put_u64(n);
    }

    fn bytes(&mut self, bytes: &[u8]) {
        self.bytes.put_bytes_u64(bytes);
    }

    fn item(&mut self, item: ItemId) -> R {
        let part = match Some(item) == self.own {
            true => self_part(self.db, item)?,
            false => item_part(self.db, item)?,
        };
        self.bytes.extend_from_slice(&part);
        self.target(Target::Item(item));
        Ok(())
    }

    fn target(&mut self, target: Target) {
        if self.seen.insert(target, ()).is_none() {
            self.targets.push(target);
        }
    }

    /// A resolution of the path being written that resolved its first `segments`.
    fn resolved(&mut self, segments: usize) {
        self.resolved = self.resolved.min(segments);
    }

    fn adt(&mut self, adt: AdtId) -> R {
        self.item(ItemId::Adt(adt))
    }

    fn variant(&mut self, variant: EnumVariantId) -> R {
        let part = variant_part(self.db, variant)?;
        self.bytes.extend_from_slice(&part);
        self.target(Target::Variant(variant));
        Ok(())
    }

    fn module(&mut self, module: ModuleId) {
        let module = key::module_key(self.db, module);
        self.bytes.extend_from_slice(&module.0.0);
    }

    fn builtin(&mut self, builtin: BuiltinType) {
        self.bytes(builtin.as_name().as_str().as_bytes());
    }

    /// A generic parameter: its owner's key, "self" for the body's own item (the owner is the
    /// body's or its parent's, whose signatures section 3.2 hashes) and its index there.
    fn param(&mut self, owner: GenericDefId, index: u32) {
        let owner = match owner {
            GenericDefId::AdtId(it) => ItemId::Adt(it),
            GenericDefId::ConstId(it) => ItemId::Const(it),
            GenericDefId::FunctionId(it) => ItemId::Function(it),
            GenericDefId::ImplId(it) => ItemId::Impl(it),
            GenericDefId::StaticId(it) => ItemId::Static(it),
            GenericDefId::TraitId(it) => ItemId::Trait(it),
            GenericDefId::TypeAliasId(it) => ItemId::Alias(it),
        };
        match Some(owner) == self.own {
            true => self.bytes.extend_from_slice(&SELF),
            false => self.bytes.extend_from_slice(&key::item_key(self.db, owner).0.0),
        }
        self.u64(u64::from(index));
    }

    fn visibility(&mut self, visibility: Visibility) {
        match visibility {
            Visibility::Module(module, explicitness) => {
                self.tag(0);
                self.module(module);
                self.tag((explicitness == VisibilityExplicitness::Explicit) as u8);
            }
            Visibility::PubCrate(krate) => {
                self.tag(1);
                self.bytes.extend_from_slice(&key::crate_key(self.db, krate).0);
            }
            Visibility::Public => self.tag(2),
        }
    }

    /// What `path` resolves to at `at`; with `def_site`, also at the definition site of the
    /// macro that wrote it.
    fn resolutions(
        &mut self,
        at: &Resolver<'_>,
        path: Path<'_>,
        hygiene: HygieneId,
        def_site: bool,
    ) -> R {
        let db = self.db;
        let segments = mod_path_of(path).map_or(0, |it| it.segments().len());
        match at.resolve_path_in_type_ns_with_prefix_info(db, path) {
            Some((ty, remaining, _, prefix, visibility)) => {
                self.resolved(remaining.unwrap_or(segments));
                self.tag(1);
                self.type_ns(ty)?;
                self.u64(remaining.map_or(u64::MAX, |it| it as u64));
                self.prefix(prefix);
                self.visibility(visibility);
            }
            None => self.tag(0),
        }
        if def_site && hygiene != HygieneId::ROOT {
            match Resolver::def_site(db, hygiene) {
                Some(site) => match site.resolve_path_in_type_ns_with_prefix_info(db, path) {
                    Some((ty, remaining, _, prefix, visibility)) => {
                        self.resolved(remaining.unwrap_or(segments));
                        self.tag(2);
                        self.type_ns(ty)?;
                        self.u64(remaining.map_or(u64::MAX, |it| it as u64));
                        self.prefix(prefix);
                        self.visibility(visibility);
                    }
                    None => self.tag(1),
                },
                None => self.tag(0),
            }
        }
        match at.resolve_path_in_value_ns_with_prefix_info(db, path, hygiene) {
            Some((value, prefix, visibility)) => {
                self.tag(1);
                let local = matches!(value, ResolveValueResult::ValueNs(ValueNs::LocalBinding(_)));
                match value {
                    ResolveValueResult::ValueNs(value) => {
                        self.resolved(segments);
                        self.tag(0);
                        self.value_ns(value)?;
                    }
                    ResolveValueResult::Partial(ty, remaining) => {
                        self.resolved(remaining);
                        self.tag(1);
                        self.type_ns(ty)?;
                        self.u64(remaining as u64);
                    }
                }
                self.prefix(prefix);
                self.visibility(visibility);
                if local {
                    if let Some(mod_path) = mod_path_of(path) {
                        self.per_ns(at.resolve_module_path_in_items(db, mod_path))?;
                    }
                }
            }
            None => self.tag(0),
        }
        Ok(())
    }

    fn prefix(&mut self, prefix: ResolvePathResultPrefixInfo) {
        self.tag(prefix.enum_variant as u8);
    }

    fn type_ns(&mut self, ty: TypeNs) -> R {
        match ty {
            TypeNs::SelfType(it) => {
                self.tag(0);
                self.item(ItemId::Impl(it))?;
            }
            TypeNs::GenericParam(it) => {
                self.tag(1);
                self.param(it.parent(), it.local_id().into_raw().into_u32());
            }
            TypeNs::AdtId(it) => {
                self.tag(2);
                self.adt(it)?;
            }
            TypeNs::AdtSelfType(it) => {
                self.tag(3);
                self.adt(it)?;
            }
            TypeNs::EnumVariantId(it) => {
                self.tag(4);
                self.variant(it)?;
            }
            TypeNs::TypeAliasId(it) => {
                self.tag(5);
                self.item(ItemId::Alias(it))?;
            }
            TypeNs::BuiltinType(it) => {
                self.tag(6);
                self.builtin(it);
            }
            TypeNs::TraitId(it) => {
                self.tag(7);
                self.item(ItemId::Trait(it))?;
            }
            TypeNs::ModuleId(it) => {
                self.tag(8);
                self.module(it);
            }
        }
        Ok(())
    }

    fn value_ns(&mut self, value: ValueNs) -> R {
        match value {
            ValueNs::ImplSelf(it) => {
                self.tag(0);
                self.item(ItemId::Impl(it))?;
            }
            ValueNs::LocalBinding(it) => {
                self.tag(1);
                self.u64(u64::from(it.into_raw().into_u32()));
            }
            ValueNs::FunctionId(it) => {
                self.tag(2);
                self.item(ItemId::Function(it))?;
            }
            ValueNs::ConstId(it) => {
                self.tag(3);
                self.item(ItemId::Const(it))?;
            }
            ValueNs::StaticId(it) => {
                self.tag(4);
                self.item(ItemId::Static(it))?;
            }
            ValueNs::StructId(it) => {
                self.tag(5);
                self.adt(it.into())?;
            }
            ValueNs::EnumVariantId(it) => {
                self.tag(6);
                self.variant(it)?;
            }
            ValueNs::GenericParam(it) => {
                self.tag(7);
                self.param(it.parent(), it.local_id().into_raw().into_u32());
            }
        }
        Ok(())
    }

    /// What a name is in the items of a scope, in each namespace.
    fn per_ns(&mut self, per_ns: PerNs) -> R {
        for def in [per_ns.take_types(), per_ns.take_values()] {
            match def {
                Some(def) => {
                    self.tag(1);
                    self.module_def(def)?;
                }
                None => self.tag(0),
            }
        }
        match per_ns.take_macros() {
            Some(it) => {
                self.tag(1);
                self.item(ItemId::Macro(it))
            }
            None => {
                self.tag(0);
                Ok(())
            }
        }
    }

    fn module_def(&mut self, def: ModuleDefId) -> R {
        match def {
            ModuleDefId::ModuleId(it) => {
                self.tag(0);
                self.module(it);
                Ok(())
            }
            ModuleDefId::FunctionId(it) => {
                self.tag(1);
                self.item(ItemId::Function(it))
            }
            ModuleDefId::AdtId(it) => {
                self.tag(2);
                self.adt(it)
            }
            ModuleDefId::EnumVariantId(it) => {
                self.tag(3);
                self.variant(it)
            }
            ModuleDefId::ConstId(it) => {
                self.tag(4);
                self.item(ItemId::Const(it))
            }
            ModuleDefId::StaticId(it) => {
                self.tag(5);
                self.item(ItemId::Static(it))
            }
            ModuleDefId::TraitId(it) => {
                self.tag(6);
                self.item(ItemId::Trait(it))
            }
            ModuleDefId::TypeAliasId(it) => {
                self.tag(7);
                self.item(ItemId::Alias(it))
            }
            ModuleDefId::BuiltinType(it) => {
                self.tag(8);
                self.builtin(it);
                Ok(())
            }
            ModuleDefId::MacroId(it) => {
                self.tag(9);
                self.item(ItemId::Macro(it))
            }
        }
    }
}
