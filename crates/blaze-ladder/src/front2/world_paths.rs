//! A body path's resolution for [`BodyWorld`]: `Self` and the body's generic parameters
//! first (the body's own scope), then the names library ([`Resolver::path`]) at the body's
//! module. The library's kind of the item it found picks the checker's [`Def`]; a struct or
//! variant named as a value needs its ADT's record (its constructor kind).

use blaze_own::{Alloc, Vec as OwnVec};
use blaze_typeck::{Def, Fetch, MissWhy, Name, PNode, PTy, PathAnswer, PathIdx, PathMiss, PathRes, Prim, ScopeId};

use super::locals::{LocalGap, LocalKind, LocalNs};

use super::heads::Want;
use super::keys::checker_of;
use super::types::SelfTy;
use super::world_items::{BodyWorld, Fill, Gap};
use crate::resolve::{Kind, Ns, ResolveError};

/// Stack-backed buffer for path segments (<= 8 segments hit zero heap allocations; longer
/// paths spill into the scratch `Alloc`).
enum SmallPathBuf<'a, 'b> {
    Inline { buf: [&'a str; 8], len: usize },
    Spill(OwnVec<&'a str, Alloc<'b>>),
}

impl<'a, 'b> SmallPathBuf<'a, 'b> {
    #[inline]
    fn with_capacity(cap: usize, alloc: Alloc<'b>) -> Self {
        if cap <= 8 {
            Self::Inline { buf: [""; 8], len: 0 }
        } else {
            Self::Spill(OwnVec::with_capacity_in(cap, alloc))
        }
    }

    #[inline]
    fn push(&mut self, value: &'a str, alloc: Alloc<'b>) {
        match self {
            Self::Inline { buf, len } => {
                if let Some(slot) = buf.get_mut(*len) {
                    *slot = value;
                    *len += 1;
                } else {
                    let mut v = OwnVec::with_capacity_in(16, alloc);
                    v.extend_from_slice(buf.get(..*len).unwrap_or(&[]));
                    v.push(value);
                    *self = Self::Spill(v);
                }
            }
            Self::Spill(v) => v.push(value),
        }
    }
}

impl<'a, 'b> core::ops::Deref for SmallPathBuf<'a, 'b> {
    type Target = [&'a str];

    #[inline]
    fn deref(&self) -> &[&'a str] {
        match self {
            Self::Inline { buf, len } => buf.get(..*len).unwrap_or(&[]),
            Self::Spill(v) => v.as_slice(),
        }
    }
}

impl<'n, S: super::source::NamesSource<'n>> BodyWorld<'_, 'n, S> {
    /// What `segments`, written in the body, name; when nothing, which segment failed, why,
    /// and what the segments before it named ([`Self::path_miss`]).
    pub(super) fn path(&self, scope: ScopeId, p: PathIdx, segments: &[Name]) -> Fetch<PathAnswer> {
        let alloc = self.store.work_alloc();
        let mut written = SmallPathBuf::with_capacity(segments.len(), alloc);
        for &s in segments {
            match self.spell.get(s) {
                Ok(t) => written.push(t, alloc),
                Err(u) => return self.gap(Gap::Unspelled(u)),
            }
        }
        // A path with no segment names nothing and has no segment to name.
        let Some(&first) = written.first() else { return Ok(Err(PathMiss::Whole)) };
        if first == "Self" {
            return Ok(match self.self_ {
                SelfTy::Trait(_) => Ok(PathRes { def: Def::TyParam(0), consumed: 1 }),
                SelfTy::Ty(t, _) => match self.store.tys.view().get(*t) {
                    Some(PNode::Adt(k, _)) => Ok(PathRes { def: Def::Adt(*k), consumed: 1 }),
                    // Any other implementing type (a parameter, an array, a primitive, a
                    // tuple, a reference): the Reference, "Paths", "`Self`": "In an
                    // implementation, it refers to the type being implemented." The checker
                    // reads the type from the impl's header (blaze-typeck rules/self_path.rs).
                    // A body that is no member of an impl yet has `Self` (an anonymous constant
                    // of a head filed with a ground `Self`, anon.rs `Ask::of_self`: itoa's
                    // `type Buffer = [MaybeUninit<u8>; Self::MAX_STR_LEN];` in `impl Sealed for
                    // i128`, checked as `const _: usize = Self::MAX_STR_LEN;`) has no header to
                    // read: there a built-in `Self` is the built-in type itself, as
                    // `i128::MAX_STR_LEN` names it.
                    Some(n) => {
                        let no_impl = self.store.fns.get(&self.owner).is_some_and(|f| f.container.is_none());
                        match prim_of(*n) {
                            Some(p) if no_impl => Ok(PathRes { def: Def::Prim(p), consumed: 1 }),
                            _ => Ok(PathRes { def: Def::SelfTy, consumed: 1 }),
                        }
                    }
                    None => Err(PathMiss::FIRST),
                },
                // `Self` outside an impl or trait: the first segment names nothing here.
                SelfTy::None => Err(PathMiss::FIRST),
            });
        }
        // The items of the body's blocks, innermost first, shadow generic parameters and the
        // module's items ([`super::locals`]); a local `use` names its path, resolved at the module.
        let mut via_use = None;
        // The module the path is resolved at: the body's, or the block's module of the local
        // struct, enum, union or alias its first segment names.
        // Where the module's names are read, by the path's first word's mark (drive.md, "A
        // token keeps its mark"; the one rule, blaze-lower `mark_site::site_of`): a `macro`
        // item's name at its definition's module, `$crate` as its mark's crate, every other
        // path (a `macro_rules!` item name too) at the call, the body's module.
        let Some(&mark) = self.path_marks.get(p.0 as usize) else { return self.gap(Gap::Site { path: p.0, why: None }) };
        // The namespace the path's site resolves it in, parallel to its mark (blaze-lower
        // `TypeckBody::path_ns`): a struct expression's or pattern's path is a type's
        // (rustc `PathSource::Struct`), so `libc::flock { .. }` is `struct flock` beside
        // `fn flock`.
        let Some(&ns) = self.path_ns.get(p.0 as usize) else { return self.gap(Gap::Site { path: p.0, why: None }) };
        let ns = match ns {
            blaze_lower::typeck::PathNs::Type => Ns::Type,
            blaze_lower::typeck::PathNs::Value => Ns::Value,
        };
        let site = match super::item_site::site_in(self.names, mark, first, first == "{{root}}", self.module) {
            Ok(site) => site,
            Err(why) => return self.gap(Gap::Site { path: p.0, why: Some(why) }),
        };
        let mut at_module = site.module;
        let mut local_type = None;
        let (found, via_glob) = match super::scope_table::value_prefix(
            &self.resolver(), &self.store.locals, self.owner, self.module, Some(scope.0), first, written.len() == 1, ns, mark, self.store.work_alloc(),
        ) {
            Ok(prefix) => prefix,
            Err(gap) => return self.gap(Gap::Local(gap)),
        };
        if let Some(row) = found {
            let k = checker_of(row.key);
            let def = match row.kind {
                LocalKind::Use => {
                    via_use = Some(row);
                    None
                }
                // A struct, enum, union or alias of a block is an item of the block's module
                // in the names library (blaze-scope blocks.rs): the path is resolved there by
                // the one name solver, whose answer carries the item's key, and the code
                // below treats it as any module's item (its head by `Store::step`). The
                // Reference, "Statements", "Item declarations": "There is no implicit capture
                // of the containing function's generic parameters, parameters, and local
                // variables": a module's item sees none of them. A trait of a block is the same
                // (`SpecWriteFmt::spec_write_fmt(x)`, `<T as SpecWriteFmt>::..` in core's
                // `Write::write_fmt`): the path to it is read at its block's module. A block's
                // `mod` and `extern crate` the same: `util::Mask`, `c::cmp::max` continue into
                // the module's items or the crate's root, which the solver reads there.
                LocalKind::Struct | LocalKind::Enum | LocalKind::Union | LocalKind::TypeAlias | LocalKind::Trait | LocalKind::Mod | LocalKind::ExternCrate => match row.module {
                    Some(m) => {
                        at_module = m;
                        local_type = Some(row);
                        None
                    }
                    None => return self.gap(Gap::Local(LocalGap { item: row.key, form: row.kind.unplaced(), range: row.range })),
                },
                LocalKind::Fn | LocalKind::Const if self.store.fns.contains_key(&k) => Some(if row.kind == LocalKind::Fn { Def::Fn(k) } else { Def::Const(k) }),
                LocalKind::Fn | LocalKind::Const | LocalKind::Static if !self.store.fns.contains_key(&k) => return self.fill(Fill::Store(Want::Fn(row.key))),
                LocalKind::Static => match self.store.statics.get(&k) {
                    Some(&m) => Some(Def::Static(k, m)),
                    None => return self.want(k),
                },
                _ => return self.gap(Gap::Local(LocalGap { item: row.key, form: row.kind.form(), range: row.range })),
            };
            if let Some(def) = def {
                return Ok(Ok(PathRes { def, consumed: 1 }));
            }
        }
        let mut shadowed_ty_param = None;
        if via_use.is_none()
            && via_glob.is_none()
            && local_type.is_none()
            && let Some(&(_, i)) = self.params.iter().find(|(n, _)| *n == segments[0])
        {
            // Type parameters live only in the type namespace, while const parameters live in the
            // value namespace (the Reference, "Namespaces"). For a single-segment value path
            // (`Err(x)` inside `fn f<Err>()`), a type parameter must not shadow value-namespace
            // items like prelude `Result::Err`; only fall back to `Def::TyParam(i)` if module
            // lookup finds nothing, so `let _ = T;` still reports E0423 (`NotAValue`).
            let is_type_only_in_value = written.len() == 1
                && ns == Ns::Value
                && self.const_params_once(self.owner)?.type_of(i).is_none();
            if is_type_only_in_value {
                shadowed_ty_param = Some(i);
            } else {
                return Ok(Ok(PathRes { def: Def::TyParam(i), consumed: 1 }));
            }
        }
        // A local `use`'s binding stands for its path: `max` of `use core::cmp::max` is
        // `core::cmp::max`, the rest of the written path after it.
        // Whether the path is read at the body's module exactly as written (no item of a
        // block, no local `use`, no `$crate`): the one case the body's conversion expands an
        // alias in ([`AliasPaths::expand`], which asks the same way).
        let plain = via_use.is_none() && via_glob.is_none() && local_type.is_none() && site.module == self.module && !written.iter().any(|t| *t == "$crate");
        let mut shift = 0u32;
        if let Some(hit) = &via_glob {
            let mut through = SmallPathBuf::with_capacity(hit.path.len() + written.len() + 1, alloc);
            if hit.abs {
                through.push("{{root}}", alloc);
            }
            for &seg in hit.path.iter() {
                through.push(seg, alloc);
            }
            for &seg in written[1..].iter() {
                through.push(seg, alloc);
            }
            shift = (through.len() - written.len()) as u32;
            written = through;
            at_module = hit.at;
        }
        let mut use_dollar = None;
        if let Some(row) = via_use {
            let expanded = match expand_local_use(&self.store.locals, self.owner, at_module, row, written.get(1..).unwrap_or(&[]), alloc) {
                Ok(e) => e,
                Err(gap) => return self.gap(Gap::Local(gap)),
            };
            let mut through = SmallPathBuf::with_capacity(expanded.segments.len() + usize::from(expanded.abs), alloc);
            if expanded.abs {
                through.push("{{root}}", alloc);
            }
            for &seg in expanded.segments.iter() {
                through.push(seg, alloc);
            }
            shift = u32::try_from(through.len().saturating_sub(written.len())).unwrap_or(u32::MAX);
            written = through;
            at_module = expanded.module;
            use_dollar = expanded.dollar;
            if expanded.local_type.is_some() {
                local_type = expanded.local_type;
            }
        }
        // `::x`: blaze-lower writes the global root as a first segment `{{root}}`.
        let (abs, rest) = match written.split_first() {
            Some((&"{{root}}", rest)) => (true, rest),
            _ => (false, &written[..]),
        };
        let lead = (written.len() - rest.len()) as u32;
        // The crate a first segment `$crate` names: once the path stands for a block import's,
        // that import's (its first segment is first now, with the mark it was written with:
        // `use $crate::builder::impl_prelude::*;` of clap_builder's `value_parser!`), else the
        // written path's own.
        let dollar = match (&via_glob, via_use) {
            (Some(hit), _) => hit.dollar,
            (None, Some(_)) => use_dollar,
            (None, None) => site.dollar_crate,
        };
        let target = match self.resolver().path_with(at_module, dollar, abs, rest, ns) {
            Ok(t) => t,
            // A block's item the body's lowering listed, which the name solver does not find
            // under its block's module (a later segment it cannot read): never "the program
            // names nothing" for a name that is declared.
            Err(ResolveError::Path(e)) => {
                if let Some(i) = shadowed_ty_param {
                    return Ok(Ok(PathRes { def: Def::TyParam(i), consumed: 1 }));
                }
                // A single-segment value path that skipped a block type-only `use` (e.g.
                // `use foo::bar::{self}; bar()`) and found no value outward still reports the
                // type-only `use` as `NotAValue` rather than an unresolved name.
                if segments.len() == 1
                    && ns == Ns::Value
                    && via_use.is_none()
                    && via_glob.is_none()
                    && local_type.is_none()
                    && let Some(row) = self.store.locals.find(self.owner, Some(scope.0), first, LocalNs::Value)
                    && row.kind == LocalKind::Use
                {
                    let mut use_module = self.module;
                    if let Some(&head) = row.path.first()
                        && !row.abs
                        && let Some(item) = self.store.locals.find(self.owner, Some(row.scope), head, LocalNs::Type).filter(|r| r.kind.in_block_module())
                        && let Some(m) = item.module
                    {
                        use_module = m;
                    }
                    let use_dollar = if row.abs { None } else { super::glob_uses::dollar_of(row.path.first().copied(), row.first_mark) };
                    if let Ok(t) = self.resolver().path_with(use_module, use_dollar, row.abs, row.path, Ns::Type) {
                        return Ok(Err(PathMiss::At { segment: 0, after: None, why: MissWhy::NotAValue(checker_of(t.key)) }));
                    }
                }
                // rustc_resolve late.rs `resolve_qpath`: a path of two or more segments that
                // fails, whose first segment is a builtin type's name, is that builtin type
                // with the rest unresolved ("The same fallback is used when `a` resolves to
                // nothing"). Applies both when `use std::str;` is at module level (`shift == 0`,
                // turso's `value.map(str::to_owned)`) and when `use std::u8;` is in a block
                // (`shift > 0`, termcolor's `fn parse_number { use std::u8; u8::from_str_radix }`),
                // as long as the written path itself did not start with `::` and had >= 2 segments.
                if first != "{{root}}"
                    && segments.len() > 1
                    && let Some(p) = builtin_first(&[first])
                {
                    return Ok(Ok(PathRes { def: Def::Prim(p), consumed: 1 }));
                }
                match local_type {
                    Some(row) if written.len() == 1 => return self.gap(Gap::Local(LocalGap { item: row.key, form: row.kind.unplaced(), range: row.range })),
                    // The program names nothing: the checker's own diagnostic, naming the segment
                    // the library stopped at (world_path_miss.rs).
                    _ => return self.path_miss(at_module, dollar, abs, rest, lead, shift, e),
                }
            }
            Err(e @ ResolveError::NoText { .. }) => return self.gap(Gap::Resolve(e)),
        };
        // Counted in the segments as written (a `use`'s path stands for its one binding). A
        // resolution that stops inside the `use`'s or glob's own path: its binding names no item.
        let Some(consumed) = (target.consumed + lead).checked_sub(shift) else {
            return Ok(Err(PathMiss::At { segment: 0, after: None, why: MissWhy::ThroughUse }));
        };
        let written = &written[shift as usize..];
        let all = consumed as usize == written.len();
        let _ = self.keys.item(target.key);
        if let Kind::Variant { adt, .. } = target.kind {
            let _ = self.keys.item(adt);
        }
        let k = checker_of(target.key);
        let def = match target.kind {
            Kind::Fn => Def::Fn(k),
            Kind::Const => Def::Const(k),
            Kind::Static => match self.store.statics.get(&k) {
                Some(&m) => Def::Static(k, m),
                None => return self.want(k),
            },
            Kind::Struct if all && super::source::NamesSource::is_ctor(self.names, target.key) => self.ctor(target.key, 0)?,
            Kind::Struct | Kind::Enum | Kind::Union => Def::Adt(k),
            Kind::Variant { adt, index } => self.ctor(adt, index)?,
            Kind::Trait => {
                // `Trait::f`: the trait's fn.
                if let Some(&next) = segments.get(consumed as usize) {
                    let Some(t) = self.store.maps.traits.get(&target.key) else {
                        return self.store_want(Want::Head(target.key));
                    };
                    if let Some(&(_, f)) = t.fns.iter().find(|(n, _)| *n == next) {
                        return Ok(Ok(PathRes { def: Def::Fn(f), consumed: consumed + 1 }));
                    }
                }
                Def::Trait(k)
            }
            // The Reference, "Type aliases": "A type alias defines a new name for an existing
            // type"; "A type alias to a tuple-struct or unit-struct cannot be used to qualify
            // that type's constructor". A body path's alias is its aliased type when that type
            // is a path the checker names (`Def`): an ADT whose arguments are the alias's own
            // parameters in order (the segment's arguments then go to the ADT unchanged), or
            // a built-in type. Any other aliased type is `Def::Alias`: the checker reads the
            // alias's definition (`World::type_alias`) and the segment's arguments as the
            // alias's own (blaze-typeck rules/alias_path.rs).
            Kind::TypeAlias => match self.store.maps.aliases.get(&target.key) {
                None => return self.store_want(Want::Head(target.key)),
                Some(None) => {
                    let why = self.store.maps.alias_gaps.get(&target.key).map_or_else(|| "(no why recorded)".into(), |w| format!("{w:?}").into_boxed_str());
                    return self.gap(Gap::AliasHead(target.key, why));
                }
                Some(&Some((generics, ty))) => match self.alias_def(generics, ty) {
                    Some(d) => d,
                    // `type Call<P, C> = Raw<P, C, u8>;`: the aliased ADT, whose arguments the
                    // body's conversion wrote on this segment ([`AliasPaths::expand`]: the
                    // alias's parameters substituted into `Raw`'s arguments, the fixed `u8`
                    // kept), so the checker reads them as `Raw`'s own. Only where that
                    // conversion expanded it (`plain`, and [`fixed_alias`]'s shape): anywhere
                    // else the segment's arguments are still the alias's own, so the path is
                    // the alias itself.
                    None => match fixed_alias(self.store.tys.view(), generics, ty, alloc) {
                        Some((adt, _)) if plain => Def::Adt(adt),
                        _ => Def::Alias(k),
                    },
                },
            },
            // A built-in type in a body path (`u8::MAX`, `str::len`): the names library's
            // answer mapped through the one table (`PRIMS`); the checker looks the next segment
            // up on it. blaze's wide ints have no checker type yet.
            Kind::Builtin(name) => match super::types::PRIMS.iter().find(|(p, _)| *p == name).and_then(|(_, n)| prim_of(*n)) {
                Some(p) => Def::Prim(p),
                None => return self.gap(Gap::BuiltinTypePath(written.join("::").into_boxed_str())),
            },
            Kind::WideInt { .. } => return self.gap(Gap::BuiltinTypePath(written.join("::").into_boxed_str())),
            // The whole path names an item that is no value: the segment that named it.
            Kind::Module | Kind::Macro | Kind::TraitAlias | Kind::Other(_) => {
                return Ok(Err(PathMiss::At { segment: consumed.saturating_sub(1), after: None, why: MissWhy::NotAValue(k) }));
            }
        };
        Ok(Ok(PathRes { def, consumed }))
    }

    /// The path `Def` of an alias with `generics` parameters whose aliased type is `ty`: the
    /// ADT when its arguments are exactly the alias's parameters in order (`type A<T> = B<T>`),
    /// a built-in type when the alias has none; `None` for any other type (`B<T, X>`: the
    /// `X` a path to `B` cannot carry).
    fn alias_def(&self, generics: u32, ty: PTy) -> Option<Def> {
        let view = self.store.tys.view();
        match *view.get(ty)? {
            PNode::Adt(k, args) => {
                let args = view.list(args).ok()?;
                let own = |(i, a): (usize, &PTy)| matches!(view.get(*a), Some(PNode::Param(p) | PNode::ConstParam(p)) if *p as usize == i);
                (args.len() == generics as usize && args.iter().enumerate().all(own)).then_some(Def::Adt(k))
            }
            n if generics == 0 => prim_of(n).map(Def::Prim),
            _ => None,
        }
    }
}

/// The most type nodes [`fixed_alias`] walks in one aliased type; a larger one is not expanded
/// (the path is then `Def::Alias`, which the checker expands itself).
const MOST_ALIAS_NODES: u32 = 4096;

/// The ADT and its arguments of an alias with `generics` parameters whose aliased type `ty`
/// is `Adt<args>` where the arguments FIX something (`type Call<P, C> = Raw<P, C, u8>`,
/// `type Sol<P, C, N> = Raw<P, PhantomData<C>, N>`): the shape a body path through the alias
/// is expanded for. Each parameter of the alias is used at most once, and a const parameter
/// only as a whole argument or an array's length (where the checker's hole is a const
/// variable): then, where the path writes no arguments, one hole (`_`) per parameter is the
/// whole of what the path says (a parameter used twice would need the two places held equal;
/// the checker lowers each written hole to its own variable, blaze-typeck machine.rs
/// `lower_holes`, so several holes in one argument stay apart). An alias of an alias is already its aliased type here: the head
/// of an alias is made with every alias in it substituted (types.rs `path_ty`). `None`: any
/// other aliased type (a tuple, a reference, a projection), or one past [`MOST_ALIAS_NODES`].
pub(super) fn fixed_alias<'a>(view: blaze_typeck::PTys<'_>, generics: u32, ty: PTy, alloc: Alloc<'a>) -> Option<(blaze_typeck::ItemKey, OwnVec<PTy, Alloc<'a>>)> {
    let PNode::Adt(adt, args) = *view.get(ty)? else { return None };
    let args = view.list(args).ok()?;
    let mut used = OwnVec::with_capacity_in(generics as usize, alloc);
    used.resize(generics as usize, false);
    let mut work: OwnVec<PTy, Alloc<'a>> = OwnVec::new_in(alloc);
    let mut steps = 0u32;
    for &arg in args {
        work.clear();
        work.push(arg);
        while let Some(t) = work.pop() {
            steps += 1;
            if steps > MOST_ALIAS_NODES {
                return None;
            }
            // A const parameter's hole is a const variable only as a whole argument
            // (blaze-typeck rules/lit_path.rs) or as an array's length (`Machine::lower_holes`,
            // the `Array` arm of `lower_node`); anywhere else the checker would make it a
            // type variable, so such an alias is not expanded.
            let param = match *view.get(t)? {
                PNode::Param(i) => Some(i),
                PNode::ConstParam(i) if t == arg => Some(i),
                PNode::ConstParam(_) => return None,
                PNode::Array(elem, len) => {
                    work.push(elem);
                    match *view.get(len)? {
                        PNode::ConstParam(i) => Some(i),
                        _ => {
                            work.push(len);
                            None
                        }
                    }
                }
                _ => {
                    super::types::children_of(view, t, &mut work)?;
                    None
                }
            };
            if let Some(i) = param {
                let slot = used.get_mut(i as usize)?;
                if *slot {
                    return None;
                }
                *slot = true;
            }
        }
    }
    let mut out = OwnVec::with_capacity_in(args.len(), alloc);
    out.extend_from_slice(args);
    Some((adt, out))
}

/// Result of expanding a block-local `use` binding (following chained local `use` rows and
/// block-module items).
struct ExpandedLocalUse<'a, 'b, 'l> {
    abs: bool,
    segments: SmallPathBuf<'a, 'b>,
    module: blaze_id::ItemKey,
    dollar: Option<blaze_id::CrateItemKey>,
    local_type: Option<super::locals::Row<'l>>,
}

/// Rewrites a path starting at a block-local `use` row `row` (followed by `tail`), following
/// chained block-local `use` rows (`use a::b as c; use c::d;`) and placing the final prefix at
/// its block module when it names a block-local item (`LocalKind::in_block_module`).
fn expand_local_use<'a, 'b, 'l: 'a>(
    locals: &super::locals::Locals<'l>,
    owner: blaze_typeck::ItemKey,
    default_module: blaze_id::ItemKey,
    row: super::locals::Row<'l>,
    tail: &[&'a str],
    alloc: Alloc<'b>,
) -> Result<ExpandedLocalUse<'a, 'b, 'l>, LocalGap> {
    let mut abs = row.abs;
    let mut cur_scope = row.scope;
    let mut cur_range = row.range;
    let mut cur_name = row.name;
    let mut first_mark = row.first_mark;
    let mut prefix = SmallPathBuf::with_capacity(row.path.len() + tail.len(), alloc);
    for &seg in row.path.iter() {
        prefix.push(seg, alloc);
    }
    let mut module = default_module;
    let mut local_type = None;
    for _ in 0..16 {
        if abs {
            break;
        }
        let Some(&head) = prefix.first() else { break };
        if matches!(head, "self" | "super" | "crate" | "$crate" | "Self" | "{{root}}") {
            break;
        }
        let Some(item) = locals.find(owner, Some(cur_scope), head, LocalNs::Type) else { break };
        if item.kind.in_block_module() {
            match item.module {
                Some(m) => {
                    module = m;
                    local_type = Some(item);
                    break;
                }
                None => return Err(LocalGap { item: item.key, form: item.kind.unplaced(), range: item.range }),
            }
        } else if item.kind == LocalKind::Use
            && !(item.scope == cur_scope && item.range == cur_range && item.name == cur_name)
            && !item.path.is_empty()
        {
            let mut next = SmallPathBuf::with_capacity(item.path.len() + prefix.len(), alloc);
            for &seg in item.path.iter() {
                next.push(seg, alloc);
            }
            for &seg in prefix.get(1..).unwrap_or(&[]) {
                next.push(seg, alloc);
            }
            prefix = next;
            abs = item.abs;
            cur_scope = item.scope;
            cur_range = item.range;
            cur_name = item.name;
            first_mark = item.first_mark;
        } else {
            break;
        }
    }
    for &seg in tail.iter() {
        prefix.push(seg, alloc);
    }
    let dollar = if abs { None } else { super::glob_uses::dollar_of(prefix.first().copied(), first_mark) };
    Ok(ExpandedLocalUse { abs, segments: prefix, module, dollar, local_type })
}

/// Resolution of a body path through block-local items, block-local `use` / glob `use`, hygiene
/// marks, and the module resolver (mirroring [`BodyWorld::path`]).
pub(super) struct ResolvedBodyPath {
    pub target: crate::resolve::Target,
    /// Segments consumed in the WRITTEN path (after subtracting any local-`use` / glob prefix).
    pub consumed: u32,
    /// Matches [`BodyWorld::path`]'s `plain`: read directly at the body's module with no block
    /// item, block `use`, glob `use`, foreign mark, or `$crate`.
    pub plain: bool,
}

/// Resolves a written body path `texts` (starting at `::` when `abs`) in `ns`, honoring block-local
/// items, named and glob `use` declarations, chained `use`s, generic parameters, and macro marks
/// identically to [`BodyWorld::path`]. Returns `None` when the path names `Self`, a local
/// value-only item (`fn`/`const`/`static`), a shadowing generic parameter, or fails to resolve.
#[expect(clippy::too_many_arguments, reason = "body path resolution mirrors BodyWorld::path inputs")]
pub(super) fn resolve_body_path<'n, 'l, 'a, S: super::source::NamesSource<'n>>(
    resolver: &crate::resolve::Paths<S>,
    locals: &super::locals::Locals<'l>,
    owner: blaze_typeck::ItemKey,
    module: blaze_id::ItemKey,
    params: &[(Name, u32)],
    consts: &[(u32, PTy)],
    scope: Option<u32>,
    abs: bool,
    texts: &[&'a str],
    ns: Ns,
    mark: Option<blaze_lower::mark_site::TextMark>,
    alloc: Alloc<'a>,
) -> Option<ResolvedBodyPath>
where
    'l: 'a,
{
    let &first = texts.first()?;
    let site = super::item_site::site_in(resolver.names, mark, first, abs, module).ok()?;
    let mut at_module = site.module;
    let mut dollar = site.dollar_crate;
    let mut resolved_abs = abs;
    let mut via_use = None;
    let mut via_glob = None;
    let mut local_type = None;
    if !abs {
        if first == "Self" {
            return None;
        }
        if let Some(scope) = scope {
            let (found, glob) = super::scope_table::value_prefix(
                resolver, locals, owner, module, Some(scope), first, texts.len() == 1, ns, mark, alloc,
            )
            .ok()?;
            via_glob = glob;
            if let Some(row) = found {
                match row.kind {
                    LocalKind::Use => via_use = Some(row),
                    LocalKind::Struct
                    | LocalKind::Enum
                    | LocalKind::Union
                    | LocalKind::TypeAlias
                    | LocalKind::Trait
                    | LocalKind::Mod
                    | LocalKind::ExternCrate => {
                        at_module = row.module?;
                        local_type = Some(row);
                    }
                    _ => return None,
                }
            }
        }
        if via_use.is_none()
            && via_glob.is_none()
            && local_type.is_none()
            && let Some(&(_, i)) = params.iter().find(|(n, _)| *n == Name::of(first))
        {
            let is_type_only_in_value = texts.len() == 1 && ns == Ns::Value && !consts.iter().any(|&(c, _)| c == i);
            if !is_type_only_in_value {
                return None;
            }
        }
    }
    let plain = via_use.is_none()
        && via_glob.is_none()
        && local_type.is_none()
        && site.module == module
        && site.dollar_crate.is_none()
        && !texts.iter().any(|t| *t == "$crate");
    let mut shift = 0u32;
    let target = if let Some(hit) = &via_glob {
        let mut through = SmallPathBuf::with_capacity(hit.path.len() + texts.len(), alloc);
        for &seg in hit.path.iter() {
            through.push(seg, alloc);
        }
        for &seg in texts.get(1..).unwrap_or(&[]) {
            through.push(seg, alloc);
        }
        shift = u32::try_from(through.len().saturating_sub(texts.len())).ok()?;
        at_module = hit.at;
        resolved_abs = hit.abs;
        dollar = hit.dollar;
        resolver.path_with(at_module, dollar, resolved_abs, &through, ns).ok()?
    } else if let Some(row) = via_use {
        let expanded = expand_local_use(locals, owner, at_module, row, texts.get(1..).unwrap_or(&[]), alloc).ok()?;
        shift = u32::try_from(expanded.segments.len().saturating_sub(texts.len())).ok()?;
        at_module = expanded.module;
        resolved_abs = expanded.abs;
        dollar = expanded.dollar;
        resolver.path_with(at_module, dollar, resolved_abs, &expanded.segments, ns).ok()?
    } else {
        resolver.path_with(at_module, dollar, resolved_abs, texts, ns).ok()?
    };
    let consumed = target.consumed.checked_sub(shift)?;
    if consumed == 0 {
        return None;
    }
    Some(ResolvedBodyPath { target, consumed, plain })
}

/// The body's conversion's side of a path through a type alias ([`fixed_alias`]): what
/// `blaze_lower::typeck::Types::alias_args` answers for front2.
pub(super) struct AliasPaths<'a, 'b, 'l, H: super::types::Heads, S> {
    pub builder: &'a super::types::Builder<'b, H, S>,
    pub env: &'a super::types::Env<'b>,
    /// The body's own items, and whose body it is: a first segment one of them names is
    /// not the module's alias.
    pub locals: &'a super::locals::Locals<'l>,
    pub owner: blaze_typeck::ItemKey,
}

impl<'n, H: super::types::Heads, S: super::source::NamesSource<'n>> AliasPaths<'_, '_, '_, H, S> {
    /// The Reference, "Type aliases": "A type alias defines a new name for an existing
    /// type". So a body path `p` of `view` is given:
    /// - for a struct, enum, union, or variant segment written short of parameters with defaults
    ///   (`super::adt_paths::adt_segment_defaults`), its defaulted arguments — including through
    ///   block-local `use`, glob `use`, or block-local type items;
    /// - for a type alias whose aliased type is `Adt<own params>` (`same`) written short of
    ///   alias defaults, its defaulted arguments;
    /// - for a plain path through an alias of [`fixed_alias`]'s shape (`plain`, matching
    ///   [`BodyWorld::path`]), the aliased type's arguments on the alias's segment.
    /// `None` for any other path, which is converted as written.
    pub(super) fn expand(
        &self,
        view: &blaze_lower::View<'_>,
        p: blaze_typeck::PathIdx,
        path_ns: blaze_lower::typeck::PathNs,
        written: &[&[PTy]],
        tys: &mut blaze_typeck::PortableTys<'_>,
        out: &mut dyn FnMut(PTy),
    ) -> Result<Option<u32>, super::types::Why> {
        use super::types::Why;
        let Some(scope) = view.path(p)?.scope else { return Ok(None) };
        let (abs, texts, _, r) = match super::types::path_texts(view, p) {
            Ok(read) => read,
            // A parenthesized segment: converted as written.
            Err(Why::Unsupported(..)) => return Ok(None),
            Err(why) => return Err(why),
        };
        let ns = match path_ns {
            blaze_lower::typeck::PathNs::Type => Ns::Type,
            blaze_lower::typeck::PathNs::Value => Ns::Value,
        };
        let mark = match super::item_site::path_mark(self.builder.marks, view, p, r) {
            Ok(m) => m,
            Err(_) => return Ok(None),
        };
        let Some(resolved) = resolve_body_path(
            &self.builder.resolver,
            self.locals,
            self.owner,
            self.env.module,
            self.env.params,
            self.env.consts,
            Some(scope.0),
            abs,
            &texts,
            ns,
            mark,
            tys.alloc(),
        ) else {
            return Ok(None);
        };
        let target = resolved.target;
        // `Vec::<u8>::with_capacity(n)`: the struct's segment written short of parameters
        // that have defaults takes them (super::adt_paths, where the rule is quoted).
        if matches!(target.kind, Kind::Struct | Kind::Enum | Kind::Union) {
            let Some(at) = resolved.consumed.checked_sub(1) else { return Ok(None) };
            let Ok(at_ix) = usize::try_from(at) else { return Ok(None) };
            let Some(&shown) = texts.get(at_ix) else { return Ok(None) };
            let own = written.get(at_ix).copied().unwrap_or(&[]);
            let filled = super::adt_paths::adt_segment_defaults(self.builder.heads, target.key, own, tys, shown, r, out)?;
            return Ok(filled.then_some(at));
        }
        // `FilterChanges::<T>::Logs(x)`, `E::V::<T>`: a variant's path, the enum's arguments
        // written on the enum's segment or on the variant's (the Reference, "Paths", "Paths in
        // expressions"; rustc takes them from either, never both: E0109), filled as the
        // enum's own segment is (super::adt_paths). The enum's segment is the one before the
        // variant's only where the variant was reached through it; a module's segment there
        // writes no arguments, so it is never the one taken.
        if let Kind::Variant { adt, .. } = target.kind {
            let Some(v_at) = resolved.consumed.checked_sub(1) else { return Ok(None) };
            let on = |i: u32| usize::try_from(i).ok().and_then(|ix| written.get(ix)).copied().unwrap_or(&[]);
            let on_enum = v_at.checked_sub(1).filter(|&e| !on(e).is_empty());
            let on_variant = Some(v_at).filter(|&v| !on(v).is_empty());
            let at = match (on_enum, on_variant) {
                (Some(e), None) => e,
                (None, Some(v)) => v,
                // Neither (inferred), or both (the language's E0109): as written.
                _ => return Ok(None),
            };
            let Ok(at_ix) = usize::try_from(at) else { return Ok(None) };
            let Some(&shown) = texts.get(at_ix) else { return Ok(None) };
            let filled = super::adt_paths::adt_segment_defaults(self.builder.heads, adt, on(at), tys, shown, r, out)?;
            return Ok(filled.then_some(at));
        }
        if !matches!(target.kind, Kind::TypeAlias) {
            return Ok(None);
        }
        let Some(at) = resolved.consumed.checked_sub(1) else { return Ok(None) };
        let Ok(at_ix) = usize::try_from(at) else { return Ok(None) };
        let Some(&shown) = texts.get(at_ix) else { return Ok(None) };
        let made = super::types::Heads::alias(self.builder.heads, target.key).ok_or(Why::Need(target.key))?;
        // A refused alias head is the world's gap naming it (`Gap::AliasHead`).
        let Some((generics, ty)) = made else { return Ok(None) };
        let alloc = tys.alloc();
        let Some((_, of)) = fixed_alias(tys.view(), generics, ty, alloc) else { return Ok(None) };
        let Ok(generics_len) = usize::try_from(generics) else { return Ok(None) };
        let same = of.len() == generics_len && {
            let v = tys.view();
            of.iter().enumerate().all(|(i, a)| matches!(v.get(*a), Some(PNode::Param(p) | PNode::ConstParam(p)) if usize::try_from(*p).ok() == Some(i)))
        };
        let own = written.get(at_ix).copied().unwrap_or(&[]);
        if same {
            // `BodyWorld::alias_def` maps a pass-through alias to `Def::Adt` regardless of `plain`.
            // When written short of defaulted alias parameters, fill those defaults here.
            if own.is_empty() || own.len() >= generics_len {
                return Ok(None);
            }
            let defaults = super::types::Heads::alias_defaults(self.builder.heads, target.key);
            if !(own.len()..generics_len).all(|i| defaults.get(i).copied().flatten().is_some()) {
                return Ok(None);
            }
        } else if !resolved.plain {
            // Non-plain paths through a `fixed_alias` resolve as `Def::Alias` in `BodyWorld::path`
            // and are expanded (including defaults) by `blaze-typeck` (`rules/alias_path.rs`).
            return Ok(None);
        }
        let consts = super::types::Heads::const_params(self.builder.heads, target.key).ok_or(Why::Need(target.key))?;
        let mut stamped = OwnVec::new_in(alloc);
        let own_in: &[PTy] = if own.is_empty() || consts.is_empty() {
            own
        } else {
            stamped.reserve(own.len());
            for (idx, &a) in own.iter().enumerate() {
                let mut a = a;
                if let Some(&(_, want_ty)) = consts.iter().find(|&&(ci, _)| usize::try_from(ci).ok() == Some(idx))
                    && let Some(&PNode::ConstValue { ty, bits }) = tys.view().get(a)
                    && matches!(tys.view().get(ty), Some(PNode::Infer))
                {
                    a = super::types::intern(tys, PNode::ConstValue { ty: want_ty, bits })?;
                }
                stamped.push(a);
            }
            &stamped
        };
        let mut args = if own_in.is_empty() {
            let hole = super::types::intern(tys, PNode::Infer)?;
            let mut holes = OwnVec::with_capacity_in(generics_len, alloc);
            holes.resize(generics_len, hole);
            holes
        } else {
            super::types::with_defaults(tys, own_in, generics, super::types::Heads::alias_defaults(self.builder.heads, target.key), shown, r)?
        };
        if !consts.is_empty() {
            for &(ci, want_ty) in consts {
                if let Ok(idx) = usize::try_from(ci)
                    && let Some(slot) = args.get_mut(idx)
                    && let Some(&PNode::ConstValue { ty, bits }) = tys.view().get(*slot)
                    && matches!(tys.view().get(ty), Some(PNode::Infer))
                {
                    *slot = super::types::intern(tys, PNode::ConstValue { ty: want_ty, bits })?;
                }
            }
        }
        for a in of {
            out(super::types::subst(tys, a, &args)?);
        }
        Ok(Some(at))
    }
}

/// The built-in type a path can name for a type node (the Reference, "Primitive types").
fn prim_of(n: PNode) -> Option<Prim> {
    match n {
        PNode::Bool => Some(Prim::Bool),
        PNode::Char => Some(Prim::Char),
        PNode::Str => Some(Prim::Str),
        PNode::Int(i) => Some(Prim::Int(i)),
        PNode::Uint(u) => Some(Prim::Uint(u)),
        PNode::Float(f) => Some(Prim::Float(f)),
        _ => None,
    }
}

/// The builtin type `rest`'s first segment names (`super::types::PRIMS`), if any.
fn builtin_first(rest: &[&str]) -> Option<Prim> {
    let first = *rest.first()?;
    super::types::PRIMS.iter().find(|(p, _)| *p == first).and_then(|(_, n)| prim_of(*n))
}
