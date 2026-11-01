//! What a debugger is told of the type of a variable: [`DebugType`]s from the types of a
//! function's locals.
//!
//! A number, a `bool`, a `char`, a pointer, an array, a tuple, a struct, a union and an enum
//! are told in full, an enum the way rustc tells of one, so that a debugger's printers for
//! `Option`, `Vec` and `String` find what they look for. A closure is told as a struct of
//! what it captured by the names of that, an `async` block as an enum of where it may have
//! left off with what it keeps there, and a pointer to a `dyn` as the pointer and the
//! vtable. Whatever else there is is told by name and size alone, which a debugger shows as
//! a value without fields.

use base_db::Db;
use hir_def::{
    AdtId, EnumId, GenericDefId, VariantId, expr_store::ExpressionStore,
    hir::generics::GenericParamDataRef, item_tree::FieldsShape as Written,
};

use blaze_hash::{Equivalent, HashMap, SharedMap};
use rustc_abi::{TagEncoding, Variants};
use rustc_type_ir::{
    UintTy,
    inherent::{BoundExistentialPredicates as _, GenericArgs as _, IntoKind},
};

use std::fmt::Write as _;
use std::sync::Arc;

use intern::{Interned, Symbol};

use super::super::vtable::{HEADER, VtableLayout};
use super::set::{self, Made, Own};
use super::{Aggregate, DebugType, Field, Tag, TypeIndex, TypeSet, Variant};
use hir_ty::{
    InferenceResult, ParamEnvAndCrate,
    closure_analysis::ProjectionKind,
    consteval::try_const_usize,
    db::HirDatabase,
    db::InternedClosure,
    generics::generics,
    layout::CoroutineFrame,
    layout::struct_tail_erasing_lifetimes,
    layout::{Layout, RustcFieldIdx},
    mir::{MirOwner, MirStage},
    next_solver::{
        DbInterner, GenericArgs, StoredTy, Ty, TyKind, TypingMode,
        infer::{DbInternerInferExt, InferCtxt},
    },
    traits::StoredParamEnvAndCrate,
};
use hir_ty_mir::mir::{normalize_or_keep, type_name::rustc_debug_name};

/// A type of the program, and how many more pointers are followed from it.
pub(crate) type Key = (StoredTy, u32);

/// [`Types::own`] of one type, kept by the database: every build after an edit asks it again
/// of the types of the variables of each function it compiles, and what it is found from
/// (layouts, names, captures, discriminants) mostly stays as it was. It is of the type alone,
/// with the types it is made of by their keys, so that it is kept once for every type that is
/// made of it, and depends on nothing of theirs.
#[blaze_query::tracked(returns(clone))]
pub(crate) fn told_type(
    db: &Db,
    env: StoredParamEnvAndCrate,
    ty: StoredTy,
    pointers: u32,
) -> Option<Arc<Own<Key>>> {
    Types::new(db, env.as_ref(db)).own(ty.as_ref(), pointers).map(Arc::new)
}

/// The sets put together so far, by their keys, for every thread: a set with a type shares
/// its set.
#[derive(Default)]
pub(crate) struct Sets(ByEnv<Key, Option<TypeSet>>);

/// What is known of types by the environment they are in, which every thread that compiles
/// asks of every variable it tells of: one lock-free table, looked up with no lock and without
/// a copy of the environment or the type, whose counts every thread would write to.
pub(crate) struct ByEnv<K, V>(SharedMap<(StoredParamEnvAndCrate, K), V>);

impl<K, V> Default for ByEnv<K, V> {
    fn default() -> Self {
        ByEnv(SharedMap::default())
    }
}

/// A key of [`ByEnv`], borrowed: hashed as the pair is, so a lookup counts nothing.
struct Of<'a, K>(&'a StoredParamEnvAndCrate, &'a K);

impl<K: std::hash::Hash> std::hash::Hash for Of<'_, K> {
    fn hash<H: std::hash::Hasher>(&self, state: &mut H) {
        self.0.hash(state);
        self.1.hash(state);
    }
}

impl<K: Eq> Equivalent<(StoredParamEnvAndCrate, K)> for Of<'_, K> {
    fn equivalent(&self, (env, key): &(StoredParamEnvAndCrate, K)) -> bool {
        self.0 == env && self.1 == key
    }
}

impl<K: std::hash::Hash + Eq + Clone, V: Clone> ByEnv<K, V> {
    pub(crate) fn get(&self, env: &StoredParamEnvAndCrate, key: &K) -> Option<V> {
        self.0.pin().get(&Of(env, key)).cloned()
    }

    pub(crate) fn insert(&self, env: &StoredParamEnvAndCrate, key: &K, value: V) {
        // clone: the environment and the key kept, counts; once per type kept
        self.0.pin().insert((env.clone(), key.clone()), value);
    }

    /// What was kept of `key` before, or else `value`, kept now.
    pub(crate) fn or_insert(&self, env: &StoredParamEnvAndCrate, key: &K, value: V) -> V {
        let types = self.0.pin();
        if let Some(kept) = types.get(&Of(env, key)) {
            // clone: a count of what was kept, which the caller takes owned
            return kept.clone();
        }
        // clone: the environment and the key kept, counts; once per type kept. Of two threads
        // that keep one key at once, the first's value is what both return.
        types.get_or_insert((env.clone(), key.clone()), value).clone()
    }
}

/// What a debugger is told of a variable of type `ty`: the type and every type it is made of,
/// from [`told_type`] of each. `None` is a type that has no layout or no name rustc would
/// spell.
pub(crate) fn described(
    db: &Db,
    env: &StoredParamEnvAndCrate,
    ty: &StoredTy,
    sets: &Sets,
) -> Option<TypeSet> {
    let key = Types::new(db, env.as_ref(db)).key(ty.as_ref());
    set::resolve(&mut InDatabase { db, env, sets }, &key)
}

struct InDatabase<'a> {
    db: &'a Db,
    env: &'a StoredParamEnvAndCrate,
    sets: &'a Sets,
}

impl set::Found for InDatabase<'_> {
    type Key = Key;

    fn own(&mut self, (ty, pointers): &Key) -> Option<Arc<Own<Key>>> {
        // clone: the query takes its keys owned, counts; once per type no set was kept of
        told_type(self.db, self.env.clone(), ty.clone(), *pointers)
    }

    fn set(&mut self, key: &Key) -> Option<Option<TypeSet>> {
        self.sets.0.get(self.env, key)
    }

    fn keep(&mut self, key: &Key, set: Option<TypeSet>) {
        self.sets.0.insert(self.env, key, set);
    }
}

/// `DW_ATE_*`: how the bytes of a [`DebugType::Base`] are read.
mod encoding {
    pub(super) const BOOLEAN: u8 = 0x02;
    pub(super) const FLOAT: u8 = 0x04;
    pub(super) const SIGNED: u8 = 0x05;
    pub(super) const UNSIGNED: u8 = 0x07;
    pub(super) const UTF: u8 = 0x10;
}

/// How many pointers are followed from a variable before what one points to is left untold,
/// unless it is told of anyway: a list is as long as it likes.
const POINTERS: u32 = 2;

/// What one type of an environment is told as. What a variable's type is told as depends on
/// nothing but the type, so it is found once for every function with a variable of it: the
/// layouts and the names are most of what telling of variables costs.
struct Types<'db> {
    db: &'db Db,
    env: ParamEnvAndCrate<'db>,
    interner: DbInterner<'db>,
    infcx: InferCtxt<'db>,
    /// The types the one being told of is made of so far, by the places the [`DebugType`] it
    /// is told as names them by.
    parts: Vec<Made<Key>>,
    names: HashMap<Ty<'db>, Option<String>>,
    normalized: HashMap<Ty<'db>, Ty<'db>>,
}

impl<'db> Types<'db> {
    fn new(db: &'db Db, env: ParamEnvAndCrate<'db>) -> Self {
        let interner = DbInterner::new_with(db, env.krate);
        let infcx = interner.infer_ctxt().build(TypingMode::Codegen);
        Types {
            db,
            env,
            interner,
            infcx,
            // alloc: empty until a type is told of; its parts end in the `Own` kept
            parts: Vec::new(),
            names: HashMap::default(),
            normalized: HashMap::default(),
        }
    }

    /// The key of a variable of type `ty`, as a type is made of it.
    fn key(&mut self, ty: Ty<'db>) -> Key {
        let ty = self.normalize(ty);
        let ty = match ty.kind() {
            TyKind::Pat(of, _) => self.normalize(of),
            _ => ty,
        };
        (ty.store(), POINTERS)
    }

    /// What `ty` is told as, followed `pointers` more pointers into. `None` is a type that has
    /// no layout or no name rustc would spell.
    fn own(&mut self, ty: Ty<'db>, pointers: u32) -> Option<Own<Key>> {
        let layout = self.db.layout_of_ty(ty.store(), self.env.store()).ok()?;
        // alloc: the type's name, kept in what it is told as
        let name = self.name(ty)?.to_owned();
        let told = self.told_as(ty, name, &layout, pointers)?;
        Some(self.owned(&told))
    }

    /// `told`, made of the types it names among [`Self::parts`].
    fn owned(&self, told: &DebugType) -> Own<Key> {
        // alloc: the places of the types it is made of, once per type told of on a miss
        let mut of = Vec::new();
        let told = Interned::new(set::shape(told, &mut of));
        // clone: a part's key is counts; alloc: the `Own`'s parts, kept; once per type on a miss
        Own { told, of: of.into_iter().map(|at| self.parts[at as usize].clone()).collect() }
    }

    /// The place of `ty` among the types the one being told of is made of.
    fn ty(&mut self, ty: Ty<'db>, pointers: u32) -> Option<TypeIndex> {
        // A type narrowed to some of its values, as what a `NonNull` keeps is, is told as
        // the type itself.
        let ty = self.normalize(ty);
        let ty = match ty.kind() {
            TyKind::Pat(of, _) => self.normalize(of),
            _ => ty,
        };
        Some(self.part(Made::Key((ty.store(), pointers))))
    }

    fn part(&mut self, part: Made<Key>) -> TypeIndex {
        self.parts.push(part);
        (self.parts.len() - 1) as TypeIndex
    }

    /// What rustc calls the type, which is asked of every variable the type is part of.
    fn name(&mut self, ty: Ty<'db>) -> Option<&str> {
        let db = self.db;
        self.names.entry(ty).or_insert_with(|| rustc_debug_name(db, ty)).as_deref()
    }

    fn told_as(
        &mut self,
        ty: Ty<'db>,
        name: String,
        layout: &Layout,
        pointers: u32,
    ) -> Option<DebugType> {
        let size = layout.size.bytes();
        let base = |name, encoding| Some(DebugType::Base { name, size, encoding });
        let mut of = Aggregate::opaque(name, size);
        // A scalable vector's lanes are as many as the machine's vector length makes them,
        // which a fixed description has no word for: it is told of without its fields.
        if let rustc_abi::BackendRepr::SimdScalableVector { .. } = layout.backend_repr {
            return Some(DebugType::Struct(of));
        }
        match ty.kind() {
            TyKind::Bool => return base(of.name, encoding::BOOLEAN),
            TyKind::Char => return base(of.name, encoding::UTF),
            TyKind::Int(_) => return base(of.name, encoding::SIGNED),
            TyKind::Uint(_) => return base(of.name, encoding::UNSIGNED),
            TyKind::Float(_) => return base(of.name, encoding::FLOAT),
            TyKind::Ref(_, to, _) | TyKind::RawPtr(to, _) => {
                return self.pointer(of, to, pointers);
            }
            TyKind::Array(element, len) => {
                let len = try_const_usize(self.db, len)? as u64;
                return Some(DebugType::Array { of: self.ty(element, pointers)?, len });
            }
            TyKind::Tuple(tys) => {
                of.fields.reserve(tys.len());
                for (index, ty) in tys.iter().enumerate() {
                    let offset = layout.fields.offset(index).bytes();
                    // A field of a type that is not told of is left out.
                    if let Some(ty) = self.ty(ty, pointers) {
                        // alloc: `Field`'s name is a `String`, interned by `set::shape`
                        of.fields.push(Field { name: format!("__{index}"), offset, ty });
                    }
                }
            }
            TyKind::Closure(..) | TyKind::CoroutineClosure(..) => {
                let closure = match ty.kind() {
                    TyKind::Closure(id, _) => id.0.loc(self.db),
                    TyKind::CoroutineClosure(id, _) => id.0.loc(self.db),
                    _ => return None,
                };
                let upvars = ty.closure_upvars_ty()?;
                let offset_of = |it| layout.fields.offset(it).bytes();
                of.fields = self.captures(closure, upvars, offset_of, pointers);
            }
            TyKind::Coroutine(id, args) => {
                return self.coroutine(of, id, args, layout, pointers);
            }
            TyKind::Adt(_, _) if self.is_pointer(ty, size) => {
                return self.pointer(of, ty.boxed_ty()?, pointers);
            }
            TyKind::Adt(def, args) => {
                of.parameters = self.parameters(def.def_id().into(), args, pointers);
                match def.def_id() {
                    AdtId::StructId(id) => {
                        of.fields = self.fields(
                            id.into(),
                            args,
                            |it| layout.fields.offset(it).bytes(),
                            pointers,
                        )?;
                    }
                    AdtId::UnionId(id) => {
                        of.fields = self.fields(
                            id.into(),
                            args,
                            |it| layout.fields.offset(it).bytes(),
                            pointers,
                        )?;
                        return Some(DebugType::Union(of));
                    }
                    AdtId::EnumId(id) => return self.enumeration(of, id, args, layout, pointers),
                }
            }
            _ => {}
        }
        Some(DebugType::Struct(of))
    }

    /// The fields of a struct, a union or a variant of an enum, which `layout` is of.
    fn fields(
        &mut self,
        variant: VariantId,
        args: GenericArgs<'db>,
        offset_of: impl Fn(usize) -> u64,
        pointers: u32,
    ) -> Option<Vec<Field>> {
        let written = variant.fields(self.db);
        let tuple = written.shape == Written::Tuple;
        let types = self.db.field_types(variant);
        let mut fields = Vec::with_capacity(types.len());
        for (id, ty) in types.iter() {
            let index = u32::from(id.into_raw()) as usize;
            let ty = ty.ty().instantiate(self.interner, args).skip_norm_wip();
            let offset = offset_of(index);
            if let Some(ty) = self.ty(ty, pointers) {
                let name = written.fields()[id].name.as_str();
                // alloc: `Field`'s name is a `String`, interned by `set::shape`
                let name = if tuple { format!("__{name}") } else { name.to_owned() };
                fields.push(Field { name, offset, ty });
            }
        }
        Some(fields)
    }

    /// The types the generic parameters of `def` stand for in `args`, by the names the
    /// parameters are written with.
    fn parameters(
        &mut self,
        def: GenericDefId,
        args: GenericArgs<'db>,
        pointers: u32,
    ) -> Vec<(String, TypeIndex)> {
        let generics = generics(self.db, def);
        // alloc: empty until a parameter is told of; the parameters are kept in the type
        let mut parameters = Vec::new();
        for ((_, parameter), arg) in generics.iter_self().zip(args.iter()) {
            if let GenericParamDataRef::TypeParamData(it) = parameter
                && let Some(name) = &it.name
                && let Some(ty) = arg.ty()
                && let Some(ty) = self.ty(ty, pointers)
            {
                // alloc: the parameter's name, a `String` of `Aggregate`'s
                parameters.push((name.as_str().to_owned(), ty));
            }
        }
        parameters
    }

    /// An enum: the names of its values when no variant has a field, and otherwise which
    /// variant a value is of by its tag and the fields of each.
    fn enumeration(
        &mut self,
        of: Aggregate,
        id: EnumId,
        args: GenericArgs<'db>,
        layout: &Layout,
        pointers: u32,
    ) -> Option<DebugType> {
        let written = &id.enum_variants(self.db).variants;
        let (tag, encoding, layouts) = match &layout.variants {
            Variants::Empty => return Some(DebugType::Struct(of)),
            Variants::Single { index } => {
                let (name, (variant, _)) = written.get_index(index.0)?;
                let offset_of = |it| layout.fields.offset(it).bytes();
                let fields = self.fields((*variant).into(), args, offset_of, pointers)?;
                // alloc: the variant's name, a `String` of `Variant`'s
                let variant = Variant { name: name.as_str().to_owned(), tag: None, fields };
                // alloc: the one variant, kept in the type
                return Some(DebugType::Enum { of, tag: None, variants: vec![variant] });
            }
            Variants::Multiple { tag, tag_encoding, tag_field, variants } => {
                let size = tag.size(self.target()?).bytes();
                let offset = layout.fields.offset(rustc_index::Idx::index(*tag_field)).bytes();
                (Tag { offset, size, states: false }, tag_encoding, variants)
            }
        };
        // A tag is told as a number of a machine word at most.
        let mask = match tag.size {
            8 => u64::MAX,
            size @ 1..8 => (1 << (size * 8)) - 1,
            _ => return Some(DebugType::Struct(of)),
        };
        let mut variants = Vec::with_capacity(layouts.len());
        let mut fieldless = true;
        for (index, layout) in layouts.iter_enumerated() {
            let (name, (variant, _)) = written.get_index(index.0)?;
            let value = match encoding {
                TagEncoding::Direct => {
                    Some(self.db.const_eval_discriminant(*variant).ok()? as u64 & mask)
                }
                TagEncoding::Niche { untagged_variant, .. } if *untagged_variant == index => None,
                TagEncoding::Niche { niche_variants, niche_start, .. } => {
                    let nth = (index.0 - niche_variants.start.0) as u128;
                    Some(nth.wrapping_add(*niche_start) as u64 & mask)
                }
            };
            let offset_of = |it| layout.field_offsets[RustcFieldIdx::new(it)].bytes();
            let fields = self.fields((*variant).into(), args, offset_of, pointers)?;
            fieldless &= fields.is_empty();
            // alloc: the variant's name, a `String` of `Variant`'s
            variants.push(Variant { name: name.as_str().to_owned(), tag: value, fields });
        }
        if fieldless && matches!(encoding, TagEncoding::Direct) {
            // alloc: the names of the values, kept in the type
            let names = variants.into_iter().filter_map(|it| Some((it.name, it.tag?))).collect();
            return Some(DebugType::Names { name: of.name, size: of.size, names });
        }
        Some(DebugType::Enum { of, tag: Some(tag), variants })
    }

    /// Whether a `Box` of `size` is nothing but the pointer, which it is told as then.
    fn is_pointer(&mut self, ty: Ty<'db>, size: u64) -> bool {
        let Some(to) = ty.boxed_ty() else { return false };
        let Some(word) = self.target().map(|it| it.pointer_size().bytes()) else { return false };
        let to = self.normalize(to);
        let wide = matches!(to.kind(), TyKind::Str | TyKind::Slice(_) | TyKind::Dynamic(..));
        size == if wide { 2 * word } else { word }
    }

    /// A pointer of one word, or of two as the struct rustc has for it: to a slice or a
    /// `str` where the first of them is and how many there are, to a `dyn` where the value is
    /// and the vtable of its type.
    fn pointer(&mut self, of: Aggregate, to: Ty<'db>, pointers: u32) -> Option<DebugType> {
        let to = self.normalize(to);
        let word = self.target()?.pointer_size().bytes();
        if of.size == word {
            return Some(DebugType::Pointer { name: of.name, to: self.followed(to, pointers) });
        }
        let usize = Ty::new_uint(self.interner, UintTy::Usize);
        let tail = self.tail(to);
        let (names, first, second) = match tail.kind() {
            TyKind::Str | TyKind::Slice(_) => {
                let first = match to.kind() {
                    TyKind::Slice(element) => element,
                    TyKind::Str => Ty::new_uint(self.interner, UintTy::U8),
                    _ => to,
                };
                let first = match first == to {
                    true => {
                        // alloc: the pointer's name, a `String` of `DebugType`'s
                        let name = format!("*const {}", self.name(to)?);
                        let whole = DebugType::Pointer { name, to: self.followed(to, pointers) };
                        self.unnamed(whole)
                    }
                    false => self.ty(Ty::new_imm_ptr(self.interner, first), pointers)?,
                };
                (["data_ptr", "length"], first, self.ty(usize, pointers)?)
            }
            TyKind::Dynamic(bounds, _) => {
                let value = DebugType::Pointer {
                    // alloc: the pointer's name, a `String` of `DebugType`'s
                    name: format!("*mut {}", self.name(to)?),
                    to: self.followed(to, pointers),
                };
                let slots = match bounds.principal() {
                    Some(principal) => {
                        let principal = principal.with_self_ty(self.interner, tail);
                        let principal =
                            self.interner.instantiate_bound_regions_with_erased(principal);
                        let interner = self.interner;
                        VtableLayout::of(interner, principal, &|it| {
                            super::super::vtable::without_lifetimes(interner, it)
                        })
                        .slots
                        .len()
                    }
                    None => 0,
                };
                let len = (HEADER + slots) as u64;
                let table = DebugType::Array { of: self.ty(usize, pointers)?, len };
                let table = self.unnamed(table);
                // alloc: the pointer's name, a `String` of `DebugType`'s
                let name = format!("&[usize; {len}]");
                let table = DebugType::Pointer { name, to: Some(table) };
                (["pointer", "vtable"], self.unnamed(value), self.unnamed(table))
            }
            _ => return Some(DebugType::Struct(of)),
        };
        // alloc: a field's name, a `String` of `Field`'s
        let field = |nth: usize, offset, ty| Field { name: names[nth].to_owned(), offset, ty };
        // alloc: the two fields of a wide pointer, kept in the type
        let fields = vec![field(0, 0, first), field(1, word, second)];
        Some(DebugType::Struct(Aggregate { fields, ..of }))
    }

    /// The part of a type whose size is not known until the program runs: the type, or the
    /// last field of a struct or a tuple, down to one that is neither.
    fn tail(&self, of: Ty<'db>) -> Ty<'db> {
        let normalize = |ty| normalize_or_keep(&self.infcx, self.env.param_env, ty);
        struct_tail_erasing_lifetimes(self.db, of, &normalize)
    }

    /// A type that is part of the one being told of and is no type of the program's.
    fn unnamed(&mut self, ty: DebugType) -> TypeIndex {
        let own = self.owned(&ty);
        // alloc: kept in the `Own` of the type it is part of; a wide pointer's halves only
        self.part(Made::Inline(Box::new(own)))
    }

    /// What a closure or an `async` block captured, each by the name rustc has for it: the
    /// variable, the fields of it down to what is captured, and `_ref__` before one that is
    /// borrowed. `upvars` is the tuple of them, and one whose name is not known is told by
    /// its place there.
    fn captures(
        &mut self,
        closure: InternedClosure<'db>,
        upvars: Ty<'db>,
        offset_of: impl Fn(usize) -> u64,
        pointers: u32,
    ) -> Vec<Field> {
        // alloc: empty, allocates nothing
        let TyKind::Tuple(upvars) = upvars.kind() else { return Vec::new() };
        let names = self.capture_names(closure).filter(|names| names.len() == upvars.len());
        let mut names = names.map(Vec::into_iter);
        let mut fields = Vec::with_capacity(upvars.len());
        for (index, ty) in upvars.iter().enumerate() {
            let name = names.as_mut().and_then(Iterator::next);
            if let Some(ty) = self.ty(ty, pointers) {
                // alloc: `Field`'s name is a `String`, interned by `set::shape`
                let name = name.unwrap_or_else(|| format!("__{index}"));
                fields.push(Field { name, offset: offset_of(index), ty });
            }
        }
        fields
    }

    fn capture_names(&self, closure: InternedClosure<'db>) -> Option<Vec<String>> {
        let store = ExpressionStore::of(self.db, closure.owner.expression_store_owner(self.db));
        let infer = InferenceResult::of(self.db, closure.owner);
        let captured = infer.closures_data.get(&closure.expr)?.min_captures.values().flatten();
        captured
            .map(|capture| {
                // alloc: the capture's name, a `String` of `Field`'s
                let mut name = String::new();
                if capture.is_by_ref() {
                    name.push_str("_ref__");
                }
                name.push_str(store[capture.captured_local()].name.as_str());
                let mut base = capture.place.base_ty.as_ref();
                for projection in &capture.place.projections {
                    if let ProjectionKind::Field { field_idx, variant_idx } = projection.kind {
                        name.push_str("__");
                        self.field_name(base, variant_idx, field_idx, &mut name)?;
                    }
                    base = projection.ty.as_ref();
                }
                Some(name)
            })
            // alloc: the names of a closure's captures, once per closure type on a miss
            .collect()
    }

    /// Writes the name of a field of a tuple, a struct or a variant of an enum to `name`.
    fn field_name(&self, of: Ty<'db>, variant: u32, field: u32, name: &mut String) -> Option<()> {
        let TyKind::Adt(def, _) = of.kind() else {
            return write!(name, "{field}").ok();
        };
        let variant: VariantId = match def.def_id() {
            AdtId::StructId(id) => id.into(),
            AdtId::UnionId(id) => id.into(),
            AdtId::EnumId(id) => {
                let variants = &id.enum_variants(self.db).variants;
                variants.get_index(variant as usize)?.1.0.into()
            }
        };
        let fields = variant.fields(self.db);
        let (_, field) = fields.fields().iter().nth(field as usize)?;
        name.push_str(field.name.as_str());
        Some(())
    }

    /// An `async` block, as rustc tells of one: an enum of not started, returned, panicked
    /// and each place it waits at, where it has the variables it keeps there, by their names.
    /// What it captured is in every one of them.
    fn coroutine(
        &mut self,
        of: Aggregate,
        id: hir_ty::next_solver::CoroutineIdWrapper<'db>,
        args: GenericArgs<'db>,
        layout: &Layout,
        pointers: u32,
    ) -> Option<DebugType> {
        let Variants::Multiple { tag, tag_field, variants, .. } = &layout.variants else {
            return Some(DebugType::Struct(of));
        };
        let size = tag.size(self.target()?).bytes();
        let offset = layout.fields.offset(rustc_index::Idx::index(*tag_field)).bytes();
        let args = args.as_coroutine();
        let offset_of = |it| layout.fields.offset(it).bytes();
        let captured =
            self.captures(id.0.loc(self.db), args.tupled_upvars_ty(), offset_of, pointers);
        let parent_args = GenericArgs::new_from_slice(args.parent_args());
        let owner = MirOwner::coroutine(self.db, id.0, args);
        let body = self
            .db
            .monomorphized_mir(owner, MirStage::Runtime, parent_args.store(), self.env.store())
            .ok()?;
        let held = body.locals_held_across_yields();
        let closure = id.0.loc(self.db);
        let store = ExpressionStore::of(self.db, closure.owner.expression_store_owner(self.db));
        let bindings = body.local_to_binding_map();
        let mut told = Vec::with_capacity(CoroutineFrame::SUSPENDED + held.yields.len());
        for (state, name) in ["Unresumed", "Returned", "Panicked"].into_iter().enumerate() {
            // clone: every state keeps what the block captured, as its fields
            let fields = captured.clone();
            // alloc: the state's name, a `String` of `Variant`'s
            told.push(Variant { name: name.to_owned(), tag: Some(state as u64), fields });
        }
        let waiting = variants.iter().skip(CoroutineFrame::SUSPENDED);
        for (nth, (point, kept)) in held.yields.iter().zip(waiting).enumerate() {
            let mut fields = Vec::with_capacity(point.kept.len() + captured.len());
            let awaited = body.awaited(point);
            for (field, &local) in point.kept.iter().enumerate() {
                let local = held.locals[local];
                // A value the body keeps without a name for it is not told of, but for the
                // future it waits for there, which rustc has a name for.
                let name = match bindings.get(local) {
                    Some(&binding) => store[binding].name.as_str(),
                    None if awaited == Some(local) => "__awaitee",
                    None => continue,
                };
                let offset = kept.field_offsets[RustcFieldIdx::new(field)].bytes();
                if let Some(ty) = self.ty(body.locals[local].ty.as_ref(), pointers) {
                    // alloc: `Field`'s name is a `String`, interned by `set::shape`
                    fields.push(Field { name: name.to_owned(), offset, ty });
                }
            }
            fields.extend(captured.iter().cloned());
            let tag = Some((CoroutineFrame::SUSPENDED + nth) as u64);
            // alloc: the state's name, a `String` of `Variant`'s
            told.push(Variant { name: format!("Suspend{nth}"), tag, fields });
        }
        Some(DebugType::Enum { of, tag: Some(Tag { offset, size, states: true }), variants: told })
    }

    /// What a pointer points to: past the last pointer followed, the type of its name where
    /// a debugger is told of one.
    fn followed(&mut self, to: Ty<'db>, pointers: u32) -> Option<TypeIndex> {
        match pointers.checked_sub(1) {
            Some(pointers) => self.ty(to, pointers),
            None => {
                let to = self.normalize(to);
                let name = Symbol::intern(self.name(to)?);
                Some(self.part(Made::Named(name)))
            }
        }
    }

    fn target(&self) -> Option<&'db rustc_abi::TargetDataLayout> {
        self.db.target_data_layout(self.env.krate).ok()
    }

    fn normalize(&mut self, ty: Ty<'db>) -> Ty<'db> {
        if let Some(&normalized) = self.normalized.get(&ty) {
            return normalized;
        }
        let normalized = normalize_or_keep(&self.infcx, self.env.param_env, ty);
        self.normalized.insert(ty, normalized);
        normalized
    }
}

impl Aggregate {
    /// A type nothing is told of but its name and its size.
    fn opaque(name: String, size: u64) -> Aggregate {
        // alloc: empty, allocates nothing until a field or a parameter is told of
        Aggregate { name, size, fields: Vec::new(), parameters: Vec::new() }
    }
}
