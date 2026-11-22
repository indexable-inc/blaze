//! A body path's resolution for [`BodyWorld`]: `Self` and the body's generic parameters
//! first (the body's own scope), then the names library ([`Resolver::path`]) at the body's
//! module. The library's kind of the item it found picks the checker's [`Def`]; a struct or
//! variant named as a value needs its ADT's record (its constructor kind).

use std::alloc::Global;
use std::vec::Vec;

use blaze_typeck::{Def, Fetch, MissWhy, Name, PNode, PTy, PathAnswer, PathIdx, PathMiss, PathRes, Prim, ScopeId};

use super::locals::{LocalGap, LocalKind, LocalNs};

use super::heads::Want;
use super::keys::checker_of;
use super::types::SelfTy;
use super::world_items::{BodyWorld, Fill, Gap};
use crate::resolve::{Kind, Ns, ResolveError};

impl<'n, S: super::source::NamesSource<'n>> BodyWorld<'_, 'n, S> {
    /// What `segments`, written in the body, name; when nothing, which segment failed, why,
    /// and what the segments before it named ([`Self::path_miss`]).
    pub(super) fn path(&self, scope: ScopeId, p: PathIdx, segments: &[Name]) -> Fetch<PathAnswer> {
        let mut written = Vec::with_capacity_in(segments.len(), Global);
        for &s in segments {
            match self.spell.get(s) {
                Ok(t) => written.push(t),
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
        if let Some(&(_, i)) = self.params.iter().find(|(n, _)| *n == segments[0]) {
            return Ok(Ok(PathRes { def: Def::TyParam(i), consumed: 1 }));
        }
        // The items of the body's blocks, innermost first, before the module's
        // ([`super::locals`]); a local `use` names its path, resolved at the module.
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
            &self.resolver(), &self.store.locals, self.owner, self.module, Some(scope.0), first, written.len() == 1, mark, self.store.tys.alloc(),
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
        // A local `use`'s binding stands for its path: `max` of `use core::cmp::max` is
        // `core::cmp::max`, the rest of the written path after it.
        // Whether the path is read at the body's module exactly as written (no item of a
        // block, no local `use`, no `$crate`): the one case the body's conversion expands an
        // alias in ([`AliasPaths::expand`], which asks the same way).
        let plain = via_use.is_none() && via_glob.is_none() && local_type.is_none() && site.module == self.module && !written.iter().any(|t| *t == "$crate");
        let mut shift = 0u32;
        if let Some(hit) = &via_glob {
            let mut through = Vec::with_capacity_in(hit.path.len() + written.len() + 1, Global);
            if hit.abs {
                through.push("{{root}}");
            }
            through.extend(hit.path.iter().copied());
            through.extend(written[1..].iter().copied());
            shift = (through.len() - written.len()) as u32;
            written = through;
            at_module = hit.at;
        }
        if let Some(row) = via_use {
            let mut through = Vec::with_capacity_in(row.path.len() + written.len(), Global);
            if row.abs {
                through.push("{{root}}");
            }
            through.extend(row.path.iter().copied());
            through.extend(written[1..].iter().copied());
            shift = (through.len() - written.len()) as u32;
            written = through;
            // The `use`'s path is read where the `use` is written (super::scope_table states the
            // order): a first segment that is an item of a block around the `use` (`use
            // __select_util::Out;` beside `mod __select_util { .. }` in the block an expansion
            // wrote) is read at that item's block module, the scope table's for the item's
            // scope (`Row::module`). Any other first segment is no block's: the body's module.
            if let Some(&head) = row.path.first()
                && !row.abs
                && let Some(item) = self.store.locals.find(self.owner, Some(row.scope), head, LocalNs::Type).filter(|r| r.kind.in_block_module())
            {
                match item.module {
                    Some(m) => {
                        at_module = m;
                        local_type = Some(item);
                    }
                    None => return self.gap(Gap::Local(LocalGap { item: item.key, form: item.kind.unplaced(), range: item.range })),
                }
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
            (None, Some(row)) if !row.abs => super::glob_uses::dollar_of(row.path.first().copied(), row.first_mark),
            (None, Some(_)) => None,
            (None, None) => site.dollar_crate,
        };
        let target = match self.resolver().path_with(at_module, dollar, abs, rest, ns) {
            Ok(t) => t,
            // A block's item the body's lowering listed, which the name solver does not find
            // under its block's module (a later segment it cannot read): never "the program
            // names nothing" for a name that is declared.
            Err(ResolveError::Path(e)) => {
                // rustc_resolve late.rs `resolve_qpath`: a path of two or more segments that
                // fails, whose first segment is a builtin type's name, is that builtin type
                // with the rest unresolved ("The same fallback is used when `a` resolves to
                // nothing"). turso's `value.map(str::to_owned)` beside `use std::str;` is
                // `<str>::to_owned`. Only for the path as written (no block `use` or glob
                // standing for its first segment, no `::` root).
                if shift == 0
                    && !abs
                    && rest.len() > 1
                    && let Some(p) = builtin_first(rest)
                {
                    return Ok(Ok(PathRes { def: Def::Prim(p), consumed: lead + 1 }));
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
                    None => match fixed_alias(self.store.tys.view(), generics, ty) {
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
pub(super) fn fixed_alias(view: blaze_typeck::PTys<'_>, generics: u32, ty: PTy) -> Option<(blaze_typeck::ItemKey, Vec<PTy, Global>)> {
    let PNode::Adt(adt, args) = *view.get(ty)? else { return None };
    let args = view.list(args).ok()?;
    let mut used = Vec::with_capacity_in(generics as usize, Global);
    used.resize(generics as usize, false);
    let mut work: Vec<PTy, Global> = Vec::new_in(Global);
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
    let mut out = Vec::with_capacity_in(args.len(), Global);
    out.extend_from_slice(args);
    Some((adt, out))
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
    /// type". So the plain path `p` of `view`, where its first segments name an alias of
    /// [`fixed_alias`]'s shape, is given the ALIASED type's arguments on the alias's segment
    /// (into `out`; the segment's index returned): the alias's parameters replaced by the
    /// arguments written on that segment (`written[index]`; a parameter not written takes
    /// its default, by the one home of that, `with_defaults`), or each by a hole when none
    /// are written (an expression's or a pattern's path leaves them to inference). The path
    /// is read as `BodyWorld::path` reads it (`Self` and the body's parameters first, then
    /// the body's own items, then the names library at the body's module, in the value
    /// namespace), and expanded only where that reading is the module's (`plain` there):
    /// `None` for any other path, which is converted as written. An alias whose arguments
    /// are its own parameters in order needs no expansion (`BodyWorld::alias_def`).
    pub(super) fn expand(&self, view: &blaze_lower::View<'_>, p: blaze_typeck::PathIdx, written: &[&[PTy]], tys: &mut blaze_typeck::PortableTys<'_>, out: &mut dyn FnMut(PTy)) -> Result<Option<u32>, super::types::Why> {
        use super::types::Why;
        let Some(scope) = view.path(p)?.scope else { return Ok(None) };
        let (abs, texts, _, r) = match super::types::path_texts(view, p) {
            Ok(read) => read,
            // `$crate`, a parenthesized segment: no plain path (the world's `plain` is false
            // for `$crate`; the conversion refuses the others itself).
            Err(Why::Unsupported(..)) => return Ok(None),
            Err(why) => return Err(why),
        };
        let Some(&first) = texts.first() else { return Ok(None) };
        // Only a path the world reads at the body's module as written (`plain` there): one an
        // expansion's mark sends elsewhere (a `macro` item's name, `$crate`), or refuses, is
        // converted as written and answered by `BodyWorld::path`.
        match super::item_site::site_in(self.builder.resolver.names, view.mark_of_path(p)?, first, abs, self.env.module) {
            Ok(site) if site.module == self.env.module && site.dollar_crate.is_none() => {}
            _ => return Ok(None),
        }
        if !abs {
            if first == "Self" || self.env.params.iter().any(|(n, _)| *n == Name::of(first)) {
                return Ok(None);
            }
            if !matches!(super::scope_table::value_prefix(
                &self.builder.resolver, self.locals, self.owner, self.env.module, Some(scope.0), first, texts.len() == 1, view.mark_of_path(p)?, tys.alloc(),
            ), Ok((None, None))) {
                return Ok(None);
            }
        }
        // A path the names library does not resolve is the world's to answer, as before.
        let Ok(target) = self.builder.resolver.path(self.env.module, abs, &texts, Ns::Value) else { return Ok(None) };
        // `Vec::<u8>::with_capacity(n)`: the struct's segment written short of parameters
        // that have defaults takes them (super::adt_paths, where the rule is quoted).
        if matches!(target.kind, Kind::Struct | Kind::Enum | Kind::Union) {
            let Some(at) = target.consumed.checked_sub(1) else { return Ok(None) };
            let Some(&shown) = texts.get(at as usize) else { return Ok(None) };
            let own = written.get(at as usize).copied().unwrap_or(&[]);
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
            let Some(v_at) = target.consumed.checked_sub(1) else { return Ok(None) };
            let on = |i: u32| written.get(i as usize).copied().unwrap_or(&[]);
            let on_enum = v_at.checked_sub(1).filter(|&e| !on(e).is_empty());
            let on_variant = Some(v_at).filter(|&v| !on(v).is_empty());
            let at = match (on_enum, on_variant) {
                (Some(e), None) => e,
                (None, Some(v)) => v,
                // Neither (inferred), or both (the language's E0109): as written.
                _ => return Ok(None),
            };
            let Some(&shown) = texts.get(at as usize) else { return Ok(None) };
            let filled = super::adt_paths::adt_segment_defaults(self.builder.heads, adt, on(at), tys, shown, r, out)?;
            return Ok(filled.then_some(at));
        }
        if !matches!(target.kind, Kind::TypeAlias) {
            return Ok(None);
        }
        let Some(at) = target.consumed.checked_sub(1) else { return Ok(None) };
        let Some(&shown) = texts.get(at as usize) else { return Ok(None) };
        let made = super::types::Heads::alias(self.builder.heads, target.key).ok_or(Why::Need(target.key))?;
        // A refused alias head is the world's gap naming it (`Gap::AliasHead`).
        let Some((generics, ty)) = made else { return Ok(None) };
        let Some((_, of)) = fixed_alias(tys.view(), generics, ty) else { return Ok(None) };
        let same = of.len() == generics as usize && {
            let v = tys.view();
            of.iter().enumerate().all(|(i, a)| matches!(v.get(*a), Some(PNode::Param(p) | PNode::ConstParam(p)) if *p as usize == i))
        };
        if same {
            return Ok(None);
        }
        let own = written.get(at as usize).copied().unwrap_or(&[]);
        let args = if own.is_empty() {
            let hole = super::types::intern(tys, PNode::Infer)?;
            let mut holes = Vec::with_capacity_in(generics as usize, Global);
            holes.resize(generics as usize, hole);
            holes
        } else {
            super::types::with_defaults(tys, own, generics, super::types::Heads::alias_defaults(self.builder.heads, target.key), shown, r)?
        };
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
