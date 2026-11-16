//! The body suite's world and body builder: items written out by hand as the item layer would
//! record them (one shared portable table), and bodies built node by node, so a case states its
//! program in a comment, builds exactly that, and names every expression it expects a type for.
//!
//! The world answers every question (nothing is `Missing`); impl buckets follow the item layer's
//! contract (`World::impls_of`): the impls whose self type has the shape, then the blanket ones,
//! in key order, which is declaration order here.

use std::collections::HashMap;
use std::format;
use std::string::String;
use std::vec::Vec;

use blaze_own::Alloc;

use crate::body::{
    Arm, BindAnn, BodyPath, BodyView, CoroutineKind, Expr, FmtTrait, FmtUse, IntSuffix, Lit, Pat, Segment, Stmt,
};
use crate::ids::{
    Edition, ExprId, FloatTy, FnPtrHeader, IntTy, ItemKey, LangItem, List, Mutbl, Name, PatId, PathIdx, Range,
    ScopeId, StmtId, UintTy,
};
use crate::portable::{PNode, PTy, PTys, PortableTys};
use crate::world::{
    Adt, AdtKind, AssocItem, AssocKind, AssocParent, AssocTyDef, ConstHolds, ConstParams, CtorKind, Def, Fetch, Field, FnSig,
    ArgsAfterSelf, Hidden, ImplHeader, Missing, PClause, PathRes, Predicates, Seal, Shape, TraitDef, TraitFlags, TypeAttrs, Variant,
    World,
};

struct FnRow {
    key: ItemKey,
    name: Name,
    container: Option<ItemKey>,
    parent: u32,
    own: u32,
    /// The last `anon` of the `own` parameters are anonymous (`impl Trait` arguments).
    anon: u32,
    self_param: bool,
    header: FnPtrHeader,
    inputs: Vec<PTy>,
    output: PTy,
    clauses: Vec<PClause>,
}

struct AdtRow {
    key: ItemKey,
    kind: AdtKind,
    generics: u32,
    variants: Vec<Variant>,
    fields: Vec<Field>,
}

struct TraitRow {
    key: ItemKey,
    generics: u32,
    lang: Option<LangItem>,
    /// `auto trait`: implemented structurally where no impl could apply.
    auto: bool,
    supers: Vec<PClause>,
    assoc: Vec<AssocTyDef>,
    /// Its members' dyn-compatibility facts, known: each associated type and fn as made,
    /// consts and the rest through [`Fx::trait_member`].
    dyn_members: Vec<crate::dyn_compat::MemberFacts>,
}

struct ImplRow {
    key: ItemKey,
    generics: u32,
    self_ty: PTy,
    trait_: Option<ItemKey>,
    trait_args: ArgsAfterSelf,
    assoc: Vec<(Name, PTy)>,
    clauses: Vec<PClause>,
    polarity: crate::world::ImplPolarity,
}

/// A hand-written world (see the module).
pub struct Fx {
    pub t: PortableTys<'static>,
    next: u128,
    names: Vec<(ItemKey, String)>,
    /// Return-position `impl Trait` occurrences: each one's bounds, `Self` the occurrence's
    /// `Opaque(k, generics in scope)`, as the front end answers `Predicates(k)`.
    opaques: Vec<(ItemKey, Vec<PClause>)>,
    /// Closures of other bodies (`PNode::Closure`): owner, ordinal, kind and signature (a
    /// `FnPtr` of `t` in the owner's parameters), as the owner's check records them.
    pub closures: Vec<(ItemKey, u32, crate::ty::ClosureKind, PTy)>,
    fns: Vec<FnRow>,
    adts: Vec<AdtRow>,
    traits: Vec<TraitRow>,
    impls: Vec<ImplRow>,
    /// The fn rows that are associated consts ([`Fx::impl_const`]): no inputs, the output their
    /// type.
    consts: Vec<ItemKey>,
    paths: Vec<(Vec<Name>, Def)>,
    lang: Vec<(LangItem, ItemKey)>,
    lang_paths: Vec<(LangItem, Def)>,
    in_scope: Vec<ItemKey>,
    buckets: HashMap<(ItemKey, Shape), Vec<ItemKey>>,
    blankets: HashMap<ItemKey, Vec<ItemKey>>,
    inherent: HashMap<(Shape, Name), Vec<AssocItem>>,
    /// An item's row in its kind's vector (keys are unique across kinds), and a container's
    /// fns by name: built by `finish`, so a question costs a lookup, not a scan.
    ix: HashMap<ItemKey, usize>,
    members: HashMap<(ItemKey, Name), usize>,
    path_ix: HashMap<Vec<Name>, Def>,
    /// `where const` conditions: each anonymous constant's value as a function of its ground
    /// arguments' nodes (the evaluator's stand-in).
    conditions: Vec<(ItemKey, fn(&[PNode]) -> ConstHolds)>,
    /// Each item's const generic parameters (index, declared type), sorted by index.
    const_params: Vec<(ItemKey, Vec<(u32, PTy)>)>,
    /// Typing attributes set by [`Fx::set_attrs`]; an enum not named here has the default
    /// representation (discriminant `isize`), any other item none.
    attrs: Vec<(ItemKey, TypeAttrs)>,
}

/// The shape an impl's self type is bucketed by; `None` for a blanket impl.
fn shape_of(t: PTys<'_>, p: PTy) -> Option<Shape> {
    Some(match t.nodes[p.0 as usize] {
        PNode::Bool => Shape::Bool,
        PNode::Char => Shape::Char,
        PNode::Int(i) => Shape::Int(i),
        PNode::Uint(u) => Shape::Uint(u),
        PNode::Float(f) => Shape::Float(f),
        PNode::Str => Shape::Str,
        PNode::Never => Shape::Never,
        PNode::Tuple(l) => Shape::Tuple(l.len),
        PNode::Ref(m, _) => Shape::Ref(m),
        PNode::Ptr(m, _) => Shape::Ptr(m),
        PNode::Array(..) => Shape::Array,
        PNode::Slice(_) => Shape::Slice,
        PNode::Pat(..) => Shape::Pat,
        PNode::Adt(k, _) => Shape::Adt(k),
        PNode::FnDef(k, _) => Shape::FnDef(k),
        PNode::FnPtr(l, _) => Shape::FnPtr(l.len.saturating_sub(1)),
        PNode::Opaque(k, _) => Shape::Opaque(k),
        PNode::Closure { .. } | PNode::Future(..) => Shape::Closure,
        PNode::Param(_) | PNode::Alias { .. } | PNode::Infer | PNode::Error => return None,
        PNode::ConstValue { .. } | PNode::ConstParam(_) => return None,
        PNode::Dynamic(l) => Shape::Dynamic(match t.lists.get(l.range()).and_then(|r| r.first()) {
            Some(&f) => match t.nodes[f.0 as usize] {
                PNode::ExTrait(k, _) => Some(k),
                _ => None,
            },
            None => None,
        }),
        PNode::ExTrait(..) | PNode::ExProj { .. } | PNode::ExAuto(_) => return None,
    })
}

impl Fx {
    pub fn new() -> Fx {
        Fx {
            t: PortableTys::new_in(Alloc::global()),
            next: 0x5157_0000,
            names: Vec::new(),
            opaques: Vec::new(),
            closures: Vec::new(),
            fns: Vec::new(),
            adts: Vec::new(),
            traits: Vec::new(),
            impls: Vec::new(),
            consts: Vec::new(),
            paths: Vec::new(),
            lang: Vec::new(),
            lang_paths: Vec::new(),
            in_scope: Vec::new(),
            buckets: HashMap::new(),
            blankets: HashMap::new(),
            inherent: HashMap::new(),
            ix: HashMap::new(),
            members: HashMap::new(),
            path_ix: HashMap::new(),
            conditions: Vec::new(),
            const_params: Vec::new(),
            attrs: Vec::new(),
        }
    }

    /// Sets `item`'s typing attributes (`#[repr(u8)]` is
    /// `TypeAttrs(0).with_discr(DiscrRepr::Uint(UintTy::U8))`).
    pub fn set_attrs(&mut self, item: ItemKey, a: TypeAttrs) {
        self.attrs.retain(|(k, _)| *k != item);
        self.attrs.push((item, a));
    }

    /// Declares generic parameter `index` of `item` (over all its generics, the parent's
    /// first) a const parameter of type `ty`.
    pub fn const_param(&mut self, item: ItemKey, index: u32, ty: PTy) {
        let row = match self.const_params.iter().position(|(k, _)| *k == item) {
            Some(i) => i,
            None => {
                self.const_params.push((item, Vec::new()));
                self.const_params.len() - 1
            }
        };
        let params = &mut self.const_params[row].1;
        params.push((index, ty));
        params.sort_unstable_by_key(|&(i, _)| i);
    }

    /// A `where const` condition (its anonymous constant) whose value at ground arguments is
    /// `value` of their nodes.
    pub fn condition(&mut self, label: &str, value: fn(&[PNode]) -> ConstHolds) -> ItemKey {
        let key = self.key(label);
        self.conditions.push((key, value));
        key
    }

    /// The clause `where const { cond }` at `args`.
    pub fn holds(&mut self, cond: ItemKey, args: &[PTy]) -> PClause {
        PClause::Const { cond, args: self.list(args) }
    }

    /// A return-position `impl Trait` occurrence's key (the front end's: the function's key and
    /// the occurrence's index), its bounds set by [`Fx::set_opaque_bounds`].
    pub fn opaque_key(&mut self, name: &str) -> ItemKey {
        let k = self.key(name);
        self.opaques.push((k, Vec::new()));
        k
    }

    /// The bounds of opaque `k`, each with `Self` the opaque type over the generics in scope.
    pub fn set_opaque_bounds(&mut self, k: ItemKey, bounds: Vec<PClause>) {
        if let Some(row) = self.opaques.iter_mut().find(|(o, _)| *o == k) {
            row.1 = bounds;
        }
    }

    fn key(&mut self, name: &str) -> ItemKey {
        self.next += 1;
        let k = ItemKey(self.next);
        self.names.push((k, String::from(name)));
        k
    }

    /// The name an item was declared with.
    pub fn name_of(&self, k: ItemKey) -> &str {
        self.names.iter().find(|(x, _)| *x == k).map_or("?", |(_, n)| n.as_str())
    }

    // Types in the world's table.

    pub fn ty(&mut self, n: PNode) -> PTy {
        self.t.intern(n).expect("intern")
    }

    pub fn list(&mut self, ts: &[PTy]) -> List {
        self.t.intern_list(ts).expect("intern")
    }

    pub fn param(&mut self, i: u32) -> PTy {
        self.ty(PNode::Param(i))
    }

    pub fn unit(&mut self) -> PTy {
        self.ty(PNode::Tuple(List::EMPTY))
    }

    pub fn never(&mut self) -> PTy {
        self.ty(PNode::Never)
    }

    pub fn u8(&mut self) -> PTy {
        self.ty(PNode::Uint(UintTy::U8))
    }

    /// `[elem; n]`.
    pub fn array(&mut self, elem: PTy, n: u64) -> PTy {
        self.t.array_n(elem, n).expect("intern")
    }

    pub fn u16(&mut self) -> PTy {
        self.ty(PNode::Uint(UintTy::U16))
    }

    pub fn u32(&mut self) -> PTy {
        self.ty(PNode::Uint(UintTy::U32))
    }

    pub fn usize(&mut self) -> PTy {
        self.ty(PNode::Uint(UintTy::Usize))
    }

    pub fn r(&mut self, m: Mutbl, t: PTy) -> PTy {
        self.ty(PNode::Ref(m, t))
    }

    pub fn adt(&mut self, k: ItemKey, args: &[PTy]) -> PTy {
        let l = self.list(args);
        self.ty(PNode::Adt(k, l))
    }

    pub fn tuple(&mut self, ts: &[PTy]) -> PTy {
        let l = self.list(ts);
        self.ty(PNode::Tuple(l))
    }

    /// `<args[0] as trait_<args[1..]>>::name`.
    pub fn alias(&mut self, trait_: ItemKey, name: &str, args: &[PTy]) -> PTy {
        let args = self.list(args);
        self.ty(PNode::Alias { trait_, name: Name::of(name), args })
    }

    pub fn bound(&mut self, trait_: ItemKey, args: &[PTy]) -> PClause {
        PClause::Trait { trait_, args: self.list(args) }
    }

    pub fn proj(&mut self, trait_: ItemKey, args: &[PTy], name: &str, term: PTy) -> PClause {
        PClause::Projection { trait_, args: self.list(args), name: Name::of(name), term }
    }

    // Items.

    /// A struct with named fields (generics `generics`), its name a path in scope.
    pub fn strukt(&mut self, name: &str, generics: u32, fields: &[(&str, PTy)]) -> ItemKey {
        let key = self.key(name);
        let fields: Vec<Field> = fields.iter().map(|&(n, ty)| Field { name: Name::of(n), ty }).collect();
        let v = Variant { key, name: Name::of(name), ctor: CtorKind::Braces, fields: List { start: 0, len: fields.len() as u32 } };
        self.adts.push(AdtRow { key, kind: AdtKind::Struct, generics, variants: std::vec![v], fields });
        self.paths.push((std::vec![Name::of(name)], Def::Adt(key)));
        key
    }

    /// An enum of tuple and unit variants (`(name, field types)`; no fields is a unit
    /// variant), its name a path in scope and each variant `Enum::Variant`. Each tuple
    /// variant's ctor is a fn (`fn_sig`: the enum's generics as its own, the fields as inputs,
    /// the enum as output). Returns the enum and its variants' keys.
    pub fn enum_(&mut self, name: &str, generics: u32, variants: &[(&str, &[PTy])]) -> (ItemKey, Vec<ItemKey>) {
        let key = self.key(name);
        let params: Vec<PTy> = (0..generics).map(|i| self.param(i)).collect();
        let self_ty = self.adt(key, &params);
        let mut vs = Vec::new();
        let mut fields = Vec::new();
        let mut keys = Vec::new();
        for (i, &(vname, tys)) in variants.iter().enumerate() {
            let vkey = self.key(&format!("{name}::{vname}"));
            let start = fields.len() as u32;
            for (j, &ty) in tys.iter().enumerate() {
                fields.push(Field { name: Name::of_index(j as u32), ty });
            }
            let ctor = if tys.is_empty() { CtorKind::Unit } else { CtorKind::Tuple };
            vs.push(Variant { key: vkey, name: Name::of(vname), ctor, fields: List { start, len: tys.len() as u32 } });
            if !tys.is_empty() {
                let mut clauses = Vec::new();
                self.implied_sized(0, generics, &mut clauses);
                self.fns.push(FnRow { key: vkey, name: Name::of(vname), container: None, parent: 0, own: generics, anon: 0, self_param: false,
                header: FnPtrHeader::RUST,
                inputs: tys.to_vec(),
                output: self_ty,
                clauses, });
            }
            let def = Def::Ctor { adt: key, variant: i as u32, kind: ctor };
            self.paths.push((std::vec![Name::of(name), Name::of(vname)], def));
            keys.push(vkey);
        }
        self.adts.push(AdtRow { key, kind: AdtKind::Enum, generics, variants: vs, fields });
        self.paths.push((std::vec![Name::of(name)], Def::Adt(key)));
        (key, keys)
    }

    /// `use path::name;`: `name` alone resolves to `def`.
    pub fn use_path(&mut self, name: &str, def: Def) {
        self.paths.push((std::vec![Name::of(name)], def));
    }

    /// Makes `def` the definition a desugaring's path to `l` names.
    pub fn lang_def(&mut self, l: LangItem, def: Def) {
        self.lang_paths.push((l, def));
    }

    /// A trait with `generics` parameters (`Self` included), its supertraits (over `Self` as
    /// `Param(0)`) and its associated types.
    pub fn tr(&mut self, name: &str, generics: u32, supers: Vec<PClause>, assoc: &[&str]) -> ItemKey {
        let key = self.key(name);
        let dyn_members = assoc
            .iter()
            .map(|&n| crate::dyn_compat::MemberFacts { name: Name::of(n), shape: crate::dyn_compat::MemberShape::Type { generic: false } })
            .collect();
        let assoc = assoc.iter().map(|&n| AssocTyDef { name: Name::of(n), item: ItemKey(crate::infer::assoc_def(key, Name::of(n))), default: None }).collect();
        self.traits.push(TraitRow { key, generics, lang: None, auto: false, supers, assoc, dyn_members });
        self.paths.push((std::vec![Name::of(name)], Def::Trait(key)));
        key
    }

    /// Sets `trait_`'s supertraits: for a trait whose supertrait names its own projection
    /// (`trait Try: FromResidual<<Self as Try>::Residual>`), made after the trait.
    pub fn set_supers(&mut self, trait_: ItemKey, supers: Vec<PClause>) {
        if let Some(t) = self.traits.iter_mut().find(|t| t.key == trait_) {
            t.supers = supers;
        }
    }

    /// Makes `trait_` an `auto trait`.
    pub fn auto(&mut self, k: ItemKey) {
        if let Some(t) = self.traits.iter_mut().find(|t| t.key == k) {
            t.auto = true;
        }
    }

    /// The item declared as `name` (the first one).
    pub fn key_of(&self, name: &str) -> ItemKey {
        self.names.iter().find(|(_, n)| n == name).map(|&(k, _)| k).expect("no item of that name")
    }

    /// Marks `trait_` as the lang item `l`.
    pub fn lang(&mut self, l: LangItem, k: ItemKey) {
        if let Some(t) = self.traits.iter_mut().find(|t| t.key == k) {
            t.lang = Some(l);
        }
        self.lang.push((l, k));
    }

    /// Brings `trait_` into scope for method calls.
    pub fn use_trait(&mut self, k: ItemKey) {
        self.in_scope.push(k);
        self.in_scope.sort();
    }

    /// A free fn: `own` generics, inputs, output, where-clauses (each own parameter's implied
    /// `Sized` is added when the world has `Sized`).
    pub fn func(&mut self, name: &str, own: u32, inputs: &[PTy], output: PTy, mut clauses: Vec<PClause>) -> ItemKey {
        let key = self.key(name);
        self.implied_sized(0, own, &mut clauses);
        self.fns.push(FnRow { key, name: Name::of(name), container: None, parent: 0, own, anon: 0, self_param: false,
        header: FnPtrHeader::RUST,
        inputs: inputs.to_vec(),
        output,
        clauses, });
        self.paths.push((std::vec![Name::of(name)], Def::Fn(key)));
        key
    }

    fn implied_sized(&mut self, from: u32, to: u32, clauses: &mut Vec<PClause>) {
        if let Some(&(_, sized)) = self.lang.iter().find(|(l, _)| *l == LangItem::Sized) {
            for i in from..from + to {
                let p = self.param(i);
                let c = self.bound(sized, &[p]);
                clauses.push(c);
            }
        }
    }

    /// A method of trait `tr` (its parameters first, `Self` as `Param(0)`); its predicates are
    /// `Self: Trait<..>` and its own.
    pub fn trait_fn(&mut self, tr: ItemKey, name: &str, self_param: bool, inputs: &[PTy], output: PTy) -> ItemKey {
        self.trait_fn_own(tr, name, self_param, 0, inputs, output, Vec::new())
    }

    /// [`Fx::trait_fn`] with `own` generics of its own (after the trait's) and their
    /// where-clauses as given: no implied `Sized` is added, so a parameter without a `Sized`
    /// clause is `?Sized`.
    pub fn trait_fn_own(
        &mut self,
        tr: ItemKey,
        name: &str,
        self_param: bool,
        own: u32,
        inputs: &[PTy],
        output: PTy,
        own_clauses: Vec<PClause>,
    ) -> ItemKey {
        let key = self.key(name);
        let generics = self.traits.iter().find(|t| t.key == tr).map_or(1, |t| t.generics);
        let ps: Vec<PTy> = (0..generics).map(|i| self.param(i)).collect();
        let me = self.bound(tr, &ps);
        let facts = self.fn_facts(self_param, own, inputs, output, &own_clauses);
        if let Some(t) = self.traits.iter_mut().find(|t| t.key == tr) {
            t.dyn_members.push(crate::dyn_compat::MemberFacts { name: Name::of(name), shape: crate::dyn_compat::MemberShape::Fn(facts) });
        }
        let mut clauses = std::vec![me];
        clauses.extend(own_clauses);
        self.fns.push(FnRow { key, name: Name::of(name), container: Some(tr), parent: generics, own, anon: 0, self_param,
        header: FnPtrHeader::RUST,
        inputs: inputs.to_vec(),
        output,
        clauses, });
        key
    }

    /// A trait fn's dyn-compatibility facts from how it was made: the receiver from `inputs[0]`
    /// (`Self`, `&Self`, `&mut Self`, `Box<Self>` once `Box` is the `owned_box` lang item;
    /// anything else is `Other`), `Self` anywhere after it (a
    /// projection's self type counts too: no case here writes one), and an own `Self: Sized`.
    fn fn_facts(&mut self, self_param: bool, own: u32, inputs: &[PTy], output: PTy, own_clauses: &[PClause]) -> crate::dyn_compat::FnFacts {
        use crate::dyn_compat::Receiver;
        let view = self.t.view();
        let is_self = |t: PTy| view.node(t).is_ok_and(|n| n == PNode::Param(0));
        let owned_box = self.lang.iter().find(|(l, _)| *l == LangItem::OwnedBox).map(|&(_, k)| k);
        let receiver = match (self_param, inputs.first().map(|&t| view.node(t))) {
            (false, _) => Receiver::Absent,
            (true, Some(Ok(PNode::Param(0)))) => Receiver::Value,
            (true, Some(Ok(PNode::Ref(Mutbl::Not, p)))) if is_self(p) => Receiver::Ref,
            (true, Some(Ok(PNode::Ref(Mutbl::Mut, p)))) if is_self(p) => Receiver::RefMut,
            // `self: Box<Self>`, with `Box` the `owned_box` lang ADT.
            (true, Some(Ok(PNode::Adt(k, l))))
                if Some(k) == owned_box && view.list(l).is_ok_and(|a| a.first().is_some_and(|&t| is_self(t))) =>
            {
                Receiver::Box
            }
            (true, _) => Receiver::Other,
        };
        let mut rest: Vec<PTy> = inputs.get(usize::from(self_param)..).unwrap_or(&[]).to_vec();
        rest.push(output);
        // `Self::X` (an alias whose self type is `Self`) is a projection, not `Self`: rustc's
        // `contains_illegal_self_type_reference` passes it, as front2's dyn_ty.rs does. The walk
        // stops at such an alias and tests every other node.
        let mut self_outside_receiver = false;
        let mut kids: blaze_own::Vec<PTy, Alloc<'_>> = blaze_own::Vec::new_in(Alloc::global());
        while let Some(t) = rest.pop() {
            let n = view.node(t).expect("the fixture's types");
            if n == PNode::Param(0) {
                self_outside_receiver = true;
                break;
            }
            if let PNode::Alias { args, .. } = n
                && view.list(args).expect("the fixture's types").first().is_some_and(|&s| is_self(s))
            {
                continue;
            }
            kids.clear();
            crate::machine::children(n, view, &mut kids).expect("the fixture's types");
            rest.extend_from_slice(&kids);
        }
        let where_self_sized = match self.lang.iter().find(|(l, _)| *l == LangItem::Sized) {
            Some(&(_, sized)) => {
                let p0 = self.param(0);
                let c = self.bound(sized, &[p0]);
                own_clauses.contains(&c)
            }
            None => false,
        };
        crate::dyn_compat::FnFacts {
            receiver,
            own_non_lifetime_params: own,
            where_self_sized,
            is_async: false,
            opaque_return: false,
            self_outside_receiver,
        }
    }

    /// Adds a member's facts to trait `tr` (an associated const, an `async fn`, ..).
    pub fn trait_member(&mut self, tr: ItemKey, m: crate::dyn_compat::MemberFacts) {
        let row = self.traits.iter_mut().find(|t| t.key == tr).expect("the trait");
        row.dyn_members.push(m);
    }

    /// Gives fn `f` the header `h`: `unsafe fn`, `extern "C" fn`.
    pub fn set_header(&mut self, f: ItemKey, h: FnPtrHeader) {
        let row = self.fns.iter_mut().find(|r| r.key == f).expect("the fn");
        row.header = h;
    }

    /// Makes the last `n` of fn `f`'s own parameters anonymous: `fn f<T>(a: T, b: impl Show)`
    /// has one declared and one anonymous parameter.
    pub fn anon_params(&mut self, f: ItemKey, n: u32) {
        let row = self.fns.iter_mut().find(|r| r.key == f).expect("the fn");
        assert!(n <= row.own, "more anonymous parameters than own ones");
        row.anon = n;
    }

    /// Drops the implied `Param(i): Sized` of free fn `f`: `fn f<T: ?Sized>`.
    pub fn unsized_fn_param(&mut self, f: ItemKey, i: u32) {
        let Some(&(_, sized)) = self.lang.iter().find(|(l, _)| *l == LangItem::Sized) else { return };
        let p = self.param(i);
        let c = self.bound(sized, &[p]);
        let row = self.fns.iter_mut().find(|r| r.key == f).expect("the fn");
        row.clauses.retain(|x| *x != c);
    }

    /// The world's answers to `adt` and `fn_sig`, for a consumer outside the checker that
    /// builds checked bodies (blaze-build's trait-object tests). Call after `finish`.
    pub fn adt_of(&self, item: ItemKey) -> Option<Adt<'_>> {
        self.adt_row(item).map(|a| Adt { kind: a.kind, generics: a.generics, variants: &a.variants, fields: &a.fields, types: self.t.view() })
    }

    pub fn sig_of(&self, item: ItemKey) -> Option<FnSig<'_>> {
        self.fn_row(item).map(|f| FnSig { parent_count: f.parent, own_count: f.own, declared_count: f.own - f.anon, self_param: f.self_param,
        header: f.header,
        inputs: &f.inputs,
        output: f.output,
        types: self.t.view(), })
    }

    /// Drops the implied `Param(i): Sized` of impl `imp`: `impl<T: ?Sized>`. Call before the
    /// impl's fns, which copy its clauses.
    pub fn unsized_param(&mut self, imp: ItemKey, i: u32) {
        let Some(&(_, sized)) = self.lang.iter().find(|(l, _)| *l == LangItem::Sized) else { return };
        let p = self.param(i);
        let c = self.bound(sized, &[p]);
        let row = self.impls.iter_mut().find(|r| r.key == imp).expect("the impl");
        row.clauses.retain(|x| *x != c);
    }

    /// An impl: `generics`, self type, the trait and its arguments after `Self` (or an inherent
    /// impl), associated types, where-clauses (with the implied `Sized` of its parameters).
    pub fn imp(
        &mut self,
        label: &str,
        generics: u32,
        self_ty: PTy,
        trait_: Option<(ItemKey, &[PTy])>,
        assoc: &[(&str, PTy)],
        mut clauses: Vec<PClause>,
    ) -> ItemKey {
        let key = self.key(label);
        self.implied_sized(0, generics, &mut clauses);
        let (tr, trait_args) = match trait_ {
            Some((t, a)) => {
                let mut with_self = Vec::with_capacity(a.len() + 1);
                with_self.push(self_ty);
                with_self.extend_from_slice(a);
                let with_self = self.list(&with_self);
                (Some(t), ArgsAfterSelf::split(self.t.view(), with_self, self_ty).expect("a trait reference with `Self` first"))
            }
            None => (None, ArgsAfterSelf::NONE),
        };
        let assoc = assoc.iter().map(|&(n, t)| (Name::of(n), t)).collect();
        self.impls.push(ImplRow { key, generics, self_ty, trait_: tr, trait_args, assoc, clauses, polarity: crate::world::ImplPolarity::Positive });
        key
    }

    /// Makes impl `imp` negative (`impl !Trait for T`) or a reservation, as its `!` or
    /// `#[rustc_reservation_impl]` says in the source.
    pub fn set_polarity(&mut self, imp: ItemKey, polarity: crate::world::ImplPolarity) {
        let row = self.impls.iter_mut().find(|r| r.key == imp).expect("an impl of this world");
        row.polarity = polarity;
    }

    /// Makes impl `imp`'s header answer its trait's arguments with `Self` first: the front
    /// end's old mistake, for the test that the checker refuses it by name.
    pub fn answer_self_first(&mut self, imp: ItemKey) {
        let row = self.impls.iter().find(|r| r.key == imp).expect("the impl");
        let (self_ty, rest) = (row.self_ty, row.trait_args.list());
        let mut with_self = Vec::with_capacity(rest.len as usize + 1);
        with_self.push(self_ty);
        with_self.extend_from_slice(self.t.view().list(rest).expect("in the table"));
        let wrong = ArgsAfterSelf::unchecked(self.list(&with_self));
        self.impls.iter_mut().find(|r| r.key == imp).expect("the impl").trait_args = wrong;
    }

    /// A method of impl `imp` (the impl's parameters first); its predicates are the impl's.
    pub fn impl_fn(&mut self, imp: ItemKey, name: &str, self_param: bool, inputs: &[PTy], output: PTy) -> ItemKey {
        self.impl_fn_own(imp, name, self_param, 0, inputs, output, Vec::new())
    }

    /// `const name: ty;` in the trait `tr` (core's `SizedTypeProperties::IS_ZST`): the world's
    /// `assoc_item(tr, name)` answers it as `AssocKind::Const`, its type the row's output.
    pub fn trait_const(&mut self, tr: ItemKey, name: &str, ty: PTy) -> ItemKey {
        let key = self.trait_fn_own(tr, name, false, 0, &[], ty, Vec::new());
        self.consts.push(key);
        key
    }

    /// `const name: ty;` in the impl `imp` (core's `impl u32 { pub const MAX: Self = ..; }`).
    pub fn impl_const(&mut self, imp: ItemKey, name: &str, ty: PTy) -> ItemKey {
        let key = self.impl_fn_own(imp, name, false, 0, &[], ty, Vec::new());
        self.consts.push(key);
        key
    }

    /// A free `const name: ty;` (bstd's `const HELD: usize = 1024;`): a path to it is
    /// `Def::Const`, its type the signature's output.
    pub fn konst(&mut self, name: &str, ty: PTy) -> ItemKey {
        let key = self.func(name, 0, &[], ty, Vec::new());
        if let Some(row) = self.paths.iter_mut().rev().find(|(_, d)| *d == Def::Fn(key)) {
            row.1 = Def::Const(key);
        }
        self.consts.push(key);
        key
    }

    /// The associated-item kind of the fn row `f`.
    fn assoc_kind(&self, f: &FnRow) -> AssocKind {
        if self.consts.contains(&f.key) { AssocKind::Const } else { AssocKind::Fn { self_param: f.self_param } }
    }

    /// [`Fx::impl_fn`] with `own` generics of its own (after the impl's) and their
    /// where-clauses as given (no implied `Sized`, as [`Fx::trait_fn_own`]).
    pub fn impl_fn_own(
        &mut self,
        imp: ItemKey,
        name: &str,
        self_param: bool,
        own: u32,
        inputs: &[PTy],
        output: PTy,
        own_clauses: Vec<PClause>,
    ) -> ItemKey {
        let key = self.key(name);
        let row = self.impls.iter().find(|i| i.key == imp).expect("the impl");
        let (parent, mut clauses) = (row.generics, row.clauses.clone());
        clauses.extend(own_clauses);
        self.fns.push(FnRow { key, name: Name::of(name), container: Some(imp), parent, own, anon: 0, self_param,
        header: FnPtrHeader::RUST,
        inputs: inputs.to_vec(),
        output,
        clauses, });
        key
    }

    /// Builds the impl buckets and the inherent index; call after the last item.
    pub fn finish(&mut self) {
        self.buckets.clear();
        self.blankets.clear();
        self.inherent.clear();
        self.ix.clear();
        self.members.clear();
        self.path_ix.clear();
        for (p, d) in &self.paths {
            self.path_ix.entry(p.clone()).or_insert(*d);
        }
        for (i, f) in self.fns.iter().enumerate() {
            self.ix.insert(f.key, i);
            if let Some(c) = f.container {
                self.members.entry((c, f.name)).or_insert(i);
            }
        }
        for (i, a) in self.adts.iter().enumerate() {
            self.ix.insert(a.key, i);
        }
        for (i, t) in self.traits.iter().enumerate() {
            self.ix.insert(t.key, i);
        }
        for (i, r) in self.impls.iter().enumerate() {
            self.ix.insert(r.key, i);
        }
        let view = PTys { nodes: self.t.view().nodes, lists: self.t.view().lists };
        let mut rows: Vec<(ItemKey, Option<ItemKey>, Option<Shape>)> = Vec::new();
        for i in &self.impls {
            rows.push((i.key, i.trait_, shape_of(view, i.self_ty)));
        }
        rows.sort_by_key(|r| r.0);
        for &(k, tr, shape) in &rows {
            match (tr, shape) {
                (Some(tr), None) => self.blankets.entry(tr).or_default().push(k),
                (Some(tr), Some(s)) => {
                    self.buckets.entry((tr, s)).or_default().push(k);
                    let any = match s {
                        Shape::Int(_) | Shape::Uint(_) => Some(Shape::AnyInt),
                        Shape::Float(_) => Some(Shape::AnyFloat),
                        _ => None,
                    };
                    if let Some(a) = any {
                        self.buckets.entry((tr, a)).or_default().push(k);
                    }
                }
                (None, Some(s)) => {
                    for f in self.fns.iter().filter(|f| f.container == Some(k)) {
                        let kind = if self.consts.contains(&f.key) { AssocKind::Const } else { AssocKind::Fn { self_param: f.self_param } };
                        self.inherent.entry((s, f.name)).or_default().push(AssocItem { container: k, item: f.key, kind });
                    }
                }
                (None, None) => panic!("an inherent impl on a parameter"),
            }
        }
        let blankets = self.blankets.clone();
        for ((tr, _), v) in self.buckets.iter_mut() {
            if let Some(b) = blankets.get(tr) {
                v.extend_from_slice(b);
            }
        }
    }

    fn fn_row(&self, k: ItemKey) -> Option<&FnRow> {
        self.ix.get(&k).and_then(|&i| self.fns.get(i)).filter(|f| f.key == k)
    }

    fn impl_row(&self, k: ItemKey) -> Option<&ImplRow> {
        self.ix.get(&k).and_then(|&i| self.impls.get(i)).filter(|r| r.key == k)
    }

    fn adt_row(&self, k: ItemKey) -> Option<&AdtRow> {
        self.ix.get(&k).and_then(|&i| self.adts.get(i)).filter(|r| r.key == k)
    }

    fn trait_row(&self, k: ItemKey) -> Option<&TraitRow> {
        self.ix.get(&k).and_then(|&i| self.traits.get(i)).filter(|r| r.key == k)
    }
}

impl World for Fx {
    fn fn_sig(&self, _: Seal, item: ItemKey) -> Fetch<Option<FnSig<'_>>> {
        Ok(self.fn_row(item).map(|f| FnSig { parent_count: f.parent, own_count: f.own, declared_count: f.own - f.anon, self_param: f.self_param,
        header: f.header,
        inputs: &f.inputs,
        output: f.output,
        types: self.t.view(), }))
    }

    fn predicates(&self, _: Seal, item: ItemKey) -> Fetch<Predicates<'_>> {
        let clauses: &[PClause] = if let Some(f) = self.fn_row(item) {
            &f.clauses
        } else if let Some(i) = self.impl_row(item) {
            &i.clauses
        } else if let Some((_, o)) = self.opaques.iter().find(|(k, _)| *k == item) {
            o
        } else {
            &[]
        };
        Ok(Predicates { clauses, types: self.t.view() })
    }

    fn adt(&self, _: Seal, item: ItemKey) -> Fetch<Option<Adt<'_>>> {
        Ok(self.adt_row(item).map(|a| Adt {
            kind: a.kind,
            generics: a.generics,
            variants: &a.variants,
            fields: &a.fields,
            types: self.t.view(),
        }))
    }

    fn impls_of(&self, _: Seal, trait_: ItemKey, shape: Shape) -> Fetch<&[ItemKey]> {
        if let Some(v) = self.buckets.get(&(trait_, shape)) {
            return Ok(v);
        }
        Ok(self.blankets.get(&trait_).map_or(&[] as &[ItemKey], |v| v.as_slice()))
    }

    fn impl_header(&self, _: Seal, item: ItemKey) -> Fetch<Option<ImplHeader<'_>>> {
        Ok(self.impl_row(item).map(|i| ImplHeader {
                    generics: i.generics,
                    self_ty: i.self_ty,
                    trait_: i.trait_,
                    trait_args: i.trait_args,
                    assoc_types: &i.assoc,
                    types: self.t.view(),
                    polarity: i.polarity,
                }))
    }

    fn inherent(&self, _: Seal, shape: Shape, name: Name) -> Fetch<&[AssocItem]> {
        Ok(self.inherent.get(&(shape, name)).map_or(&[] as &[AssocItem], |v| v.as_slice()))
    }

    fn assoc_item(&self, _: Seal, container: ItemKey, name: Name) -> Fetch<Option<AssocItem>> {
        if let Some(f) = self.members.get(&(container, name)).and_then(|&i| self.fns.get(i)) {
            return Ok(Some(AssocItem { container, item: f.key, kind: self.assoc_kind(f) }));
        }
        if let Some(t) = self.trait_row(container) {
            if let Some(a) = t.assoc.iter().find(|a| a.name == name) {
                return Ok(Some(AssocItem { container, item: a.item, kind: AssocKind::Type }));
            }
        }
        Ok(None)
    }

    fn traits_in_scope(&self, _: Seal, _: ScopeId) -> Fetch<&[ItemKey]> {
        Ok(&self.in_scope)
    }

    fn resolve_path(&self, _: Seal, _: ScopeId, _: PathIdx, segments: &[Name]) -> Fetch<Option<PathRes>> {
        // The longest prefix of `segments` that names an item.
        for n in (1..=segments.len()).rev() {
            if let Some(&d) = self.path_ix.get(&segments[..n]) {
                return Ok(Some(PathRes { def: d, consumed: n as u32 }));
            }
        }
        Ok(None)
    }

    fn lang_item(&self, _: Seal, item: LangItem) -> Fetch<Option<ItemKey>> {
        Ok(self.lang.iter().find(|(l, _)| *l == item).map(|&(_, k)| k))
    }

    fn lang_path(&self, _: Seal, item: LangItem) -> Fetch<Option<Def>> {
        Ok(self.lang_paths.iter().find(|(l, _)| *l == item).map(|&(_, d)| d))
    }

    fn const_value(&self, _: Seal, _: ItemKey) -> Fetch<Option<u64>> {
        Ok(None)
    }

    fn opaque_hidden(&self, _: Seal, _: ItemKey) -> Fetch<Option<Hidden<'_>>> {
        Ok(None)
    }
    /// The [`Fx::closures`] row: the facts another body's check recorded.
    fn closure(&self, _: Seal, owner: ItemKey, ordinal: u32) -> Fetch<Option<crate::world::ClosureFacts<'_>>> {
        Ok(self.closures.iter().find(|r| r.0 == owner && r.1 == ordinal).map(|r| crate::world::ClosureFacts {
            kind: r.2,
            sig: r.3,
            types: self.t.view(),
        }))
    }
    fn attrs(&self, _: Seal, item: ItemKey) -> Fetch<TypeAttrs> {
        if let Some(&(_, a)) = self.attrs.iter().find(|(k, _)| *k == item) {
            return Ok(a);
        }
        Ok(match self.adt_row(item) {
            Some(a) if a.kind == AdtKind::Enum => TypeAttrs(0).with_discr(crate::world::DiscrRepr::Int(IntTy::Isize)),
            _ => TypeAttrs(0),
        })
    }

    fn trait_def(&self, _: Seal, trait_: ItemKey) -> Fetch<Option<TraitDef<'_>>> {
        Ok(self.trait_row(trait_).map(|t| TraitDef {
            generics: t.generics,
            flags: TraitFlags(if t.auto { TraitFlags::AUTO } else { 0 }),
            lang: t.lang,
            supertraits: &t.supers,
            assoc_types: &t.assoc,
            types: self.t.view(),
            members: crate::world::TraitMembers::Known(&t.dyn_members),
        }))
    }

    fn assoc_parent(&self, _: Seal, item: ItemKey) -> Fetch<Option<AssocParent>> {
        Ok(self.fn_row(item).and_then(|f| {
            f.container.map(|c| AssocParent { container: c, name: f.name, kind: self.assoc_kind(f) })
        }))
    }

    fn const_holds(&self, _: Seal, cond: ItemKey, args: &[PTy], types: PTys<'_>) -> Fetch<ConstHolds> {
        let &(_, value) = self.conditions.iter().find(|(k, _)| *k == cond).ok_or(Missing)?;
        let nodes: Vec<PNode> = args.iter().map(|a| types.nodes[a.0 as usize]).collect();
        Ok(value(&nodes))
    }

    fn const_params(&self, _: Seal, item: ItemKey) -> Fetch<ConstParams<'_>> {
        let params = self.const_params.iter().find(|(k, _)| *k == item).map_or(&[] as &[(u32, PTy)], |(_, p)| p.as_slice());
        Ok(ConstParams { params, types: self.t.view() })
    }
}

/// A body under construction (see the module): its columns and its own table of written types.
pub struct Bb {
    pub exprs: Vec<Expr>,
    ranges: Vec<Range>,
    pats: Vec<Pat>,
    pat_ranges: Vec<Range>,
    stmts: Vec<Stmt>,
    expr_lists: Vec<ExprId>,
    pat_lists: Vec<PatId>,
    stmt_lists: Vec<StmtId>,
    paths: Vec<BodyPath>,
    segments: Vec<Segment>,
    opt_types: Vec<Option<PTy>>,
    /// Segments' explicit generic arguments (`BodyView::type_lists`).
    type_lists: Vec<PTy>,
    params: Vec<PatId>,
    arms: Vec<Arm>,
    fmt_uses: Vec<FmtUse>,
    coroutine: Option<CoroutineKind>,
    pub t: PortableTys<'static>,
}

impl Bb {
    pub fn new() -> Bb {
        Bb {
            exprs: Vec::new(),
            ranges: Vec::new(),
            pats: Vec::new(),
            pat_ranges: Vec::new(),
            stmts: Vec::new(),
            expr_lists: Vec::new(),
            pat_lists: Vec::new(),
            stmt_lists: Vec::new(),
            paths: Vec::new(),
            segments: Vec::new(),
            opt_types: Vec::new(),
            type_lists: Vec::new(),
            params: Vec::new(),
            arms: Vec::new(),
            fmt_uses: Vec::new(),
            coroutine: None,
            t: PortableTys::new_in(Alloc::global()),
        }
    }

    pub(crate) fn e(&mut self, x: Expr) -> ExprId {
        let at = self.exprs.len() as u32;
        self.exprs.push(x);
        self.ranges.push(Range { start: at, end: at + 1 });
        ExprId(at)
    }

    /// A binding pattern `name`.
    pub fn bind(&mut self, name: &str) -> PatId {
        let at = self.pats.len() as u32;
        self.pats.push(Pat::Bind { ann: BindAnn { by_ref: None, mutable: false }, name: Name::of(name), sub: None });
        self.pat_ranges.push(Range { start: 1000 + at, end: 1001 + at });
        PatId(at)
    }

    /// A parameter of the owner, bound by `name`.
    pub fn param(&mut self, name: &str) -> PatId {
        let p = self.bind(name);
        self.params.push(p);
        p
    }

    /// A written type in the body's table.
    pub fn ty(&mut self, n: PNode) -> PTy {
        self.t.intern(n).expect("intern")
    }

    /// A const argument naming variant `variant` of the enum `adt`, as the front end makes it
    /// ([`crate::PortableTys::variant_value`]).
    pub fn variant_value(&mut self, adt: ItemKey, variant: u32) -> PTy {
        self.t.variant_value(adt, variant).expect("intern")
    }

    pub fn int(&mut self, suffix: Option<IntSuffix>) -> ExprId {
        self.e(Expr::Lit(Lit::Int(suffix)))
    }

    pub fn float(&mut self, suffix: Option<FloatTy>) -> ExprId {
        self.e(Expr::Lit(Lit::Float(suffix)))
    }

    pub fn bool(&mut self) -> ExprId {
        self.e(Expr::Lit(Lit::Bool(true)))
    }

    pub fn local(&mut self, p: PatId) -> ExprId {
        self.e(Expr::Local(p))
    }

    /// A one-segment value path with explicit generic arguments: `name::<args..>`.
    pub fn path_args(&mut self, name: &str, args: &[PTy]) -> ExprId {
        let generic_args = List { start: self.type_lists.len() as u32, len: args.len() as u32 };
        self.type_lists.extend_from_slice(args);
        let start = self.segments.len() as u32;
        self.segments.push(Segment { name: Name::of(name), generic_args, has_args: true });
        let at = self.paths.len() as u32;
        self.paths.push(BodyPath { scope: ScopeId(0), segments: List { start, len: 1 }, lang: None, anchor: None });
        self.e(Expr::Path(PathIdx(at)))
    }

    /// A value path `segments[0]::segments[1]..`.
    pub fn path(&mut self, segments: &[&str]) -> ExprId {
        let start = self.segments.len() as u32;
        for s in segments {
            self.segments.push(Segment { name: Name::of(s), generic_args: List::EMPTY, has_args: false });
        }
        let at = self.paths.len() as u32;
        self.paths.push(BodyPath { scope: ScopeId(0), segments: List { start, len: segments.len() as u32 }, lang: None, anchor: None });
        self.e(Expr::Path(PathIdx(at)))
    }

    /// A qualified value path `<ty>::segments[0]..` (no trait).
    pub fn qpath(&mut self, ty: PTy, segments: &[&str]) -> ExprId {
        let start = self.segments.len() as u32;
        for s in segments {
            self.segments.push(Segment { name: Name::of(s), generic_args: List::EMPTY, has_args: false });
        }
        let at = self.paths.len() as u32;
        let anchor = Some(crate::body::Anchor { ty, trait_: None });
        self.paths.push(BodyPath { scope: ScopeId(0), segments: List { start, len: segments.len() as u32 }, lang: None, anchor });
        self.e(Expr::Path(PathIdx(at)))
    }

    /// A desugaring's path to the lang item `l` (no segments).
    fn lang_path_idx(&mut self, l: LangItem) -> PathIdx {
        let at = self.paths.len() as u32;
        self.paths.push(BodyPath { scope: ScopeId(0), segments: List::EMPTY, lang: Some(l), anchor: None });
        PathIdx(at)
    }

    /// A value path to the lang item `l`, as a desugaring writes it.
    pub fn lang(&mut self, l: LangItem) -> ExprId {
        let p = self.lang_path_idx(l);
        self.e(Expr::Path(p))
    }

    /// `L(elems..)` against the lang item `l`'s variant (`ControlFlow::Continue(v)`).
    pub fn lang_variant_pat(&mut self, l: LangItem, elems: &[PatId]) -> PatId {
        let path = self.lang_path_idx(l);
        let start = self.pat_lists.len() as u32;
        self.pat_lists.extend_from_slice(elems);
        let at = self.pats.len() as u32;
        self.pats.push(Pat::TupleStruct { path, elems: List { start, len: elems.len() as u32 }, rest: None });
        self.pat_ranges.push(Range { start: 1000 + at, end: 1001 + at });
        PatId(at)
    }

    /// `match scrutinee { pat => body, .. }`, no guards.
    pub fn match_(&mut self, scrutinee: ExprId, arms: &[(PatId, ExprId)]) -> ExprId {
        let start = self.arms.len() as u32;
        for &(pat, body) in arms {
            self.arms.push(Arm { pat, guard: None, body });
        }
        self.e(Expr::Match { scrutinee, arms: List { start, len: arms.len() as u32 } })
    }

    pub fn ret(&mut self, value: Option<ExprId>) -> ExprId {
        self.e(Expr::Return { value })
    }

    /// `e?` as rustc lowers it (blaze-lower's `try_op`):
    /// `match Try::branch(e) { ControlFlow::Break(r) => return FromResidual::from_residual(r),
    /// ControlFlow::Continue(v) => v }`. Returns the match.
    pub fn try_op(&mut self, e: ExprId) -> ExprId {
        let branch = self.lang(LangItem::TryTraitBranch);
        let call = self.call(branch, &[e]);
        let r = self.bind("residual");
        let brk = self.lang_variant_pat(LangItem::ControlFlowBreak, &[r]);
        let from = self.lang(LangItem::TryTraitFromResidual);
        let r_use = self.local(r);
        let conv = self.call(from, &[r_use]);
        let ret = self.ret(Some(conv));
        let v = self.bind("val");
        let cont = self.lang_variant_pat(LangItem::ControlFlowContinue, &[v]);
        let v_use = self.local(v);
        self.match_(call, &[(brk, ret), (cont, v_use)])
    }

    /// `(elems..)`; `()` with none.
    pub fn tuple(&mut self, elems: &[ExprId]) -> ExprId {
        let elems = self.exprs_list(elems);
        self.e(Expr::Tuple { elems })
    }

    /// `[elem; len]`.
    pub fn repeat(&mut self, elem: ExprId, len: u64) -> ExprId {
        let len = self.t.usize_value(len).expect("intern");
        self.e(Expr::Repeat { elem, len, count: None })
    }

    /// `[elem; count]` with the length's own expression `count` (a literal or a path to a
    /// constant) and its term `len` as the front end evaluated it.
    pub fn repeat_counted(&mut self, elem: ExprId, len: PTy, count: ExprId) -> ExprId {
        self.e(Expr::Repeat { elem, len, count: Some(count) })
    }

    /// A string literal (`&'static str`).
    pub fn str_(&mut self) -> ExprId {
        self.e(Expr::Lit(Lit::Str))
    }

    fn exprs_list(&mut self, xs: &[ExprId]) -> List {
        let start = self.expr_lists.len() as u32;
        self.expr_lists.extend_from_slice(xs);
        List { start, len: xs.len() as u32 }
    }

    pub fn call(&mut self, callee: ExprId, args: &[ExprId]) -> ExprId {
        let args = self.exprs_list(args);
        self.e(Expr::Call { callee, args })
    }

    pub fn method(&mut self, receiver: ExprId, name: &str, args: &[ExprId]) -> ExprId {
        let args = self.exprs_list(args);
        self.e(Expr::MethodCall { receiver, name: Name::of(name), generic_args: List::EMPTY, args, scope: ScopeId(0) })
    }

    pub fn borrow(&mut self, mutbl: Mutbl, operand: ExprId) -> ExprId {
        self.e(Expr::Ref { mutbl, operand })
    }

    /// `|params| body`, parameters unannotated.
    pub fn closure(&mut self, params: &[PatId], body: ExprId) -> ExprId {
        let start = self.pat_lists.len() as u32;
        self.pat_lists.extend_from_slice(params);
        let tstart = self.opt_types.len() as u32;
        for _ in params {
            self.opt_types.push(None);
        }
        let n = params.len() as u32;
        self.e(Expr::Closure { params: List { start, len: n }, param_tys: List { start: tstart, len: n }, ret: None, body, is_async: false, is_move: false })
    }

    pub fn let_(&mut self, pat: PatId, ty: Option<PTy>, init: ExprId) -> StmtId {
        let at = self.stmts.len() as u32;
        self.stmts.push(Stmt::Let { pat, ty, init: Some(init), else_: None });
        StmtId(at)
    }

    pub fn semi(&mut self, expr: ExprId) -> StmtId {
        let at = self.stmts.len() as u32;
        self.stmts.push(Stmt::Expr { expr, semi: true });
        StmtId(at)
    }

    pub fn block(&mut self, stmts: &[StmtId], tail: Option<ExprId>) -> ExprId {
        let start = self.stmt_lists.len() as u32;
        self.stmt_lists.extend_from_slice(stmts);
        self.e(Expr::Block { stmts: List { start, len: stmts.len() as u32 }, tail, scope: ScopeId(0) })
    }

    pub fn view(&self, owner: ItemKey, root: ExprId) -> BodyView<'_> {
        BodyView {
            owner,
            edition: Edition::E2021,
            params: &self.params,
            root,
            exprs: &self.exprs,
            expr_ranges: &self.ranges,
            pats: &self.pats,
            pat_ranges: &self.pat_ranges,
            stmts: &self.stmts,
            expr_lists: &self.expr_lists,
            pat_lists: &self.pat_lists,
            stmt_lists: &self.stmt_lists,
            arms: &self.arms,
            field_inits: &[],
            field_pats: &[],
            paths: &self.paths,
            segments: &self.segments,
            types: self.t.view(),
            type_lists: &self.type_lists,
            opt_types: &self.opt_types,
            fmt_uses: &self.fmt_uses,
            coroutine: self.coroutine,
        }
    }

    /// `format_args!` over `args`, with `uses` as (argument index, trait).
    pub fn format_args(&mut self, args: &[ExprId], uses: &[(u32, FmtTrait)]) -> ExprId {
        let start = self.expr_lists.len() as u32;
        self.expr_lists.extend_from_slice(args);
        let ustart = self.fmt_uses.len() as u32;
        self.fmt_uses.extend(uses.iter().map(|&(arg, trait_)| FmtUse { arg, trait_ }));
        self.e(Expr::FormatArgs {
            args: List { start, len: args.len() as u32 },
            uses: List { start: ustart, len: uses.len() as u32 },
        })
    }

    /// `async { body }`, or `async move { body }` with `Capture::Move`.
    pub fn async_block(&mut self, body: ExprId, capture: crate::body::Capture) -> ExprId {
        self.e(Expr::AsyncBlock { body, capture })
    }

    /// `move |params| body`, parameters unannotated.
    pub fn move_closure(&mut self, params: &[PatId], body: ExprId) -> ExprId {
        let c = self.closure(params, body);
        if let Some(Expr::Closure { is_move, .. }) = self.exprs.get_mut(c.0 as usize) {
            *is_move = true;
        }
        c
    }

    /// `async |params| body`, parameters unannotated.
    pub fn async_closure(&mut self, params: &[PatId], body: ExprId) -> ExprId {
        let c = self.closure(params, body);
        if let Some(Expr::Closure { is_async, .. }) = self.exprs.get_mut(c.0 as usize) {
            *is_async = true;
        }
        c
    }

    pub fn await_(&mut self, operand: ExprId) -> ExprId {
        self.e(Expr::Await(operand))
    }

    /// Makes the body an `async fn`'s, returning `output` (a type of the body's table).
    pub fn async_fn(&mut self, output: PTy) {
        self.coroutine = Some(CoroutineKind::Async { output });
    }
}

/// A record's type, printed as Rust writes it (items by their declared names; a parameter of
/// the owner as `P<i>`).
pub fn show(fx: &Fx, t: PTys<'_>, p: PTy) -> String {
    let list = |l: List| -> String {
        let parts: Vec<String> = t.lists[l.range()].iter().map(|&x| show(fx, t, x)).collect();
        parts.join(", ")
    };
    match t.nodes[p.0 as usize] {
        PNode::Bool => String::from("bool"),
        PNode::Char => String::from("char"),
        PNode::Int(i) => String::from(match i {
            IntTy::Isize => "isize",
            IntTy::I8 => "i8",
            IntTy::I16 => "i16",
            IntTy::I32 => "i32",
            IntTy::I64 => "i64",
            IntTy::I128 => "i128",
        }),
        PNode::Uint(u) => String::from(match u {
            UintTy::Usize => "usize",
            UintTy::U8 => "u8",
            UintTy::U16 => "u16",
            UintTy::U32 => "u32",
            UintTy::U64 => "u64",
            UintTy::U128 => "u128",
        }),
        PNode::Float(f) => String::from(match f {
            FloatTy::F16 => "f16",
            FloatTy::F32 => "f32",
            FloatTy::F64 => "f64",
            FloatTy::F128 => "f128",
        }),
        PNode::Str => String::from("str"),
        PNode::Never => String::from("!"),
        PNode::Tuple(l) if l.len == 1 => format!("({},)", list(l)),
        PNode::Tuple(l) => format!("({})", list(l)),
        PNode::Ref(Mutbl::Not, x) => format!("&{}", show(fx, t, x)),
        PNode::Ref(Mutbl::Mut, x) => format!("&mut {}", show(fx, t, x)),
        PNode::Ptr(Mutbl::Not, x) => format!("*const {}", show(fx, t, x)),
        PNode::Ptr(Mutbl::Mut, x) => format!("*mut {}", show(fx, t, x)),
        PNode::Array(x, n) => format!("[{}; {}]", show(fx, t, x), show(fx, t, n)),
        PNode::Slice(x) => format!("[{}]", show(fx, t, x)),
        PNode::Pat(x, l) => format!("({} is {})", show(fx, t, x), list(l)),
        PNode::Adt(k, l) if l.len == 0 => String::from(fx.name_of(k)),
        PNode::Adt(k, l) => format!("{}<{}>", fx.name_of(k), list(l)),
        PNode::FnDef(k, l) if l.len == 0 => format!("fn {}", fx.name_of(k)),
        PNode::FnDef(k, l) => format!("fn {}<{}>", fx.name_of(k), list(l)),
        PNode::FnPtr(l, h) => {
            let unsafe_ = if h.safety == crate::ids::Safety::Unsafe { "unsafe " } else { "" };
            let abi = match h.abi {
                crate::ids::Abi::Rust => "",
                crate::ids::Abi::C => "extern \"C\" ",
                crate::ids::Abi::CUnwind => "extern \"C-unwind\" ",
                crate::ids::Abi::System => "extern \"system\" ",
                crate::ids::Abi::SystemUnwind => "extern \"system-unwind\" ",
                crate::ids::Abi::RustCall => "extern \"rust-call\" ",
            };
            // The list is the inputs, then the output.
            let all = &t.lists[l.range()];
            let (ins, out) = all.split_at(all.len().saturating_sub(1));
            let ins: Vec<String> = ins.iter().map(|&x| show(fx, t, x)).collect();
            let out = match out.first() {
                Some(&o) => format!(" -> {}", show(fx, t, o)),
                None => String::new(),
            };
            format!("{unsafe_}{abi}fn({}){out}", ins.join(", "))
        }
        PNode::Closure { owner, ordinal, args } => format!("closure#{ordinal} of {}<{}>", fx.name_of(owner), list(args)),
        PNode::Future(e, o, i) => format!("future#{}<{}; {}>", e.0, show(fx, t, o), show(fx, t, i)),
        PNode::Param(i) => format!("P{i}"),
        PNode::Alias { trait_, name, args } => format!("<{} as {}>::{:x}", list(args), fx.name_of(trait_), name.0),
        PNode::Opaque(k, _) => format!("impl {}", fx.name_of(k)),
        PNode::Infer => String::from("_"),
        PNode::Error => String::from("{error}"),
        PNode::ConstParam(i) => format!("C{i}"),
        PNode::ConstValue { ty, bits } => match t.nodes.get(ty.0 as usize) {
            Some(PNode::Bool) => format!("{}", bits != 0),
            Some(PNode::Char) => match u32::try_from(bits).ok().and_then(char::from_u32) {
                Some(c) => format!("{c:?}"),
                None => format!("{{char {bits:#x}}}"),
            },
            Some(PNode::Uint(crate::ids::UintTy::Usize)) => format!("{bits}"),
            _ => format!("{bits}{}", show(fx, t, ty)),
        },
        PNode::Dynamic(l) => {
            let rows: Vec<String> = t.lists[l.range()].iter().map(|&x| show(fx, t, x)).collect();
            format!("dyn {}", rows.join(" + "))
        }
        PNode::ExTrait(k, l) if l.len == 0 => String::from(fx.name_of(k)),
        PNode::ExTrait(k, l) => format!("{}<{}>", fx.name_of(k), list(l)),
        // An object's binding: its args follow the erased `Self`, so it prints as the trait.
        PNode::ExProj { trait_, name, args, term } if args.len == 0 => {
            format!("{}::{:x} = {}", fx.name_of(trait_), name.0, show(fx, t, term))
        }
        PNode::ExProj { trait_, name, args, term } => {
            format!("{}<{}>::{:x} = {}", fx.name_of(trait_), list(args), name.0, show(fx, t, term))
        }
        PNode::ExAuto(k) => String::from(fx.name_of(k)),
    }
}

/// The items of [`loops`] the cases name.
pub struct Loops {
    /// `fn sum(xs: &[u32]) -> u32`.
    pub sum: ItemKey,
    pub option: ItemKey,
}

/// What `for` over a range and over a slice reads, as `core` declares it (cut to `usize`
/// ranges and `&[T]`):
///
/// ```text
/// #[lang = "sized"] trait Sized {}
/// enum Option<T> { #[lang = "None"] None, #[lang = "Some"] Some(T) }
/// #[lang = "Range"] struct Range<Idx> { start: Idx, end: Idx }
/// trait Iterator { type Item; #[lang = "next"] fn next(&mut self) -> Option<Self::Item>; }
/// trait IntoIterator { type Item; type IntoIter: Iterator<Item = Self::Item>;
///     #[lang = "into_iter"] fn into_iter(self) -> Self::IntoIter; }
/// impl<I: Iterator> IntoIterator for I { type Item = I::Item; type IntoIter = I; }
/// impl Iterator for Range<usize> { type Item = usize; }
/// struct Iter<T> { at: *const T }
/// impl<T> Iterator for Iter<T> { type Item = &T; }          // lifetimes erased
/// impl<T> IntoIterator for &[T] { type Item = &T; type IntoIter = Iter<T>; }
/// impl<T> [T] { fn len(&self) -> usize; }
/// fn sum(xs: &[u32]) -> u32;
/// ```
pub fn loops() -> (Fx, Loops) {
    let mut world = Fx::new();
    let fx = &mut world;
    let sized = fx.tr("Sized", 1, Vec::new(), &[]);
    fx.lang(LangItem::Sized, sized);
    let p0 = fx.param(0);
    let (option, _) = fx.enum_("Option", 1, &[("None", &[]), ("Some", &[p0])]);
    let range = fx.strukt("Range", 1, &[("start", p0), ("end", p0)]);
    let iterator = fx.tr("Iterator", 1, Vec::new(), &["Item"]);
    let item = fx.alias(iterator, "Item", &[p0]);
    let opt_item = fx.adt(option, &[item]);
    let m_self = fx.r(Mutbl::Mut, p0);
    let next = fx.trait_fn(iterator, "next", true, &[m_self], opt_item);
    let into_iterator = fx.tr("IntoIterator", 1, Vec::new(), &["Item", "IntoIter"]);
    let into_iter_t = fx.alias(into_iterator, "IntoIter", &[p0]);
    let into_iter = fx.trait_fn(into_iterator, "into_iter", true, &[p0], into_iter_t);
    let i_iter = fx.bound(iterator, &[p0]);
    fx.imp(
        "impl<I: Iterator> IntoIterator for I",
        1,
        p0,
        Some((into_iterator, &[])),
        &[("Item", item), ("IntoIter", p0)],
        std::vec![i_iter],
    );
    let usize_ = fx.usize();
    let range_usize = fx.adt(range, &[usize_]);
    fx.imp("impl Iterator for Range<usize>", 0, range_usize, Some((iterator, &[])), &[("Item", usize_)], Vec::new());
    let ptr = fx.ty(PNode::Ptr(Mutbl::Not, p0));
    let iter = fx.strukt("Iter", 1, &[("at", ptr)]);
    let iter_t = fx.adt(iter, &[p0]);
    let r_t = fx.r(Mutbl::Not, p0);
    fx.imp("impl<T> Iterator for Iter<T>", 1, iter_t, Some((iterator, &[])), &[("Item", r_t)], Vec::new());
    let slice_t = fx.ty(PNode::Slice(p0));
    let r_slice = fx.r(Mutbl::Not, slice_t);
    fx.imp(
        "impl<T> IntoIterator for &[T]",
        1,
        r_slice,
        Some((into_iterator, &[])),
        &[("Item", r_t), ("IntoIter", iter_t)],
        Vec::new(),
    );
    let slice_impl = fx.imp("impl<T> [T]", 1, slice_t, None, &[], Vec::new());
    fx.impl_fn(slice_impl, "len", true, &[r_slice], usize_);

    fx.lang_def(LangItem::IntoIterIntoIter, Def::Fn(into_iter));
    fx.lang_def(LangItem::IteratorNext, Def::Fn(next));
    fx.lang_def(LangItem::OptionNone, Def::Ctor { adt: option, variant: 0, kind: CtorKind::Unit });
    fx.lang_def(LangItem::OptionSome, Def::Ctor { adt: option, variant: 1, kind: CtorKind::Tuple });
    fx.lang_def(LangItem::Range, Def::Adt(range));

    let u32_ = fx.u32();
    let slice_u32 = fx.ty(PNode::Slice(u32_));
    let r_slice_u32 = fx.r(Mutbl::Not, slice_u32);
    let sum = fx.func("sum", 0, &[r_slice_u32], u32_, Vec::new());
    fx.finish();
    (world, Loops { sum, option })
}

/// The items of [`show_put`] the cases name.
pub struct ShowPut {
    /// `fn line<W: Show>(&self, cx: &Cx, what: W) -> Result<(), OutError>` of `impl Out`.
    pub line: ItemKey,
    /// `fn show<P: Put + ?Sized>(&self, out: &mut P)` of `impl<T: Show + ?Sized> Show for &T`.
    pub ref_show: ItemKey,
    /// `fn hello(o: &Out, cx: &Cx) -> Result<(), OutError>`.
    pub hello: ItemKey,
    /// The trait fn `Show::show`.
    pub show: ItemKey,
}

/// The hosted hello's output path (crates/blaze-text/src/{show,put}.rs, crates/blaze-std/src/
/// out.rs), cut to what `Out::line` reads; lifetimes erased:
///
/// ```text
/// #[lang = "sized"] trait Sized {}
/// enum Result<T, E> { Ok(T), Err(E) }
/// trait Put { fn put_str(&mut self, text: &str); }
/// trait Show { fn show<P: Put + ?Sized>(&self, out: &mut P); }
/// impl Show for str {}
/// impl<T: Show + ?Sized> Show for &T { fn show<P: Put + ?Sized>(&self, out: &mut P); }
/// struct Cx {}  struct Out {}  struct OutError {}  struct Held {}
/// impl Held { fn new(out: &Out, cx: &Cx) -> Held; fn take(&mut self, bytes: &[u8]) -> bool;
///             fn end(self) -> Result<(), OutError>; }
/// impl Put for Held {}
/// impl Out { fn line(&self, cx: &Cx, what: impl Show) -> Result<(), OutError>; }
/// fn hello(o: &Out, cx: &Cx) -> Result<(), OutError>;
/// ```
/// `impl Show` in argument position is the anonymous parameter `W` with `W: Show` (and its
/// implied `Sized`).
pub fn show_put() -> (Fx, ShowPut) {
    show_put_with(true)
}

/// [`show_put`], the `&T` impl's `T: ?Sized` kept when `t_unsized`, else `T` implied `Sized`
/// (the control: `&str: Show` then fails on `str: Sized`).
pub fn show_put_with(t_unsized: bool) -> (Fx, ShowPut) {
    let mut world = Fx::new();
    let fx = &mut world;
    let sized = fx.tr("Sized", 1, Vec::new(), &[]);
    fx.lang(LangItem::Sized, sized);
    let (p0, p1) = (fx.param(0), fx.param(1));
    let (result, _) = fx.enum_("Result", 2, &[("Ok", &[p0]), ("Err", &[p1])]);
    let unit = fx.unit();
    let str_ = fx.ty(PNode::Str);
    let r_str = fx.r(Mutbl::Not, str_);

    let put = fx.tr("Put", 1, Vec::new(), &[]);
    let m_self = fx.r(Mutbl::Mut, p0);
    fx.trait_fn(put, "put_str", true, &[m_self, r_str], unit);

    let show_tr = fx.tr("Show", 1, Vec::new(), &[]);
    let r_self = fx.r(Mutbl::Not, p0);
    let m_p = fx.r(Mutbl::Mut, p1);
    let p_put = fx.bound(put, &[p1]);
    let show = fx.trait_fn_own(show_tr, "show", true, 1, &[r_self, m_p], unit, std::vec![p_put]);
    fx.imp("impl Show for str", 0, str_, Some((show_tr, &[])), &[], Vec::new());
    let r_t = fx.r(Mutbl::Not, p0);
    let t_show = fx.bound(show_tr, &[p0]);
    let ref_impl = fx.imp("impl<T: Show + ?Sized> Show for &T", 1, r_t, Some((show_tr, &[])), &[], std::vec![t_show]);
    if t_unsized {
        fx.unsized_param(ref_impl, 0);
    }
    let r_r_t = fx.r(Mutbl::Not, r_t);
    let ref_show = fx.impl_fn_own(ref_impl, "show", true, 1, &[r_r_t, m_p], unit, std::vec![p_put]);

    let cx = fx.strukt("Cx", 0, &[]);
    let out = fx.strukt("Out", 0, &[]);
    let out_error = fx.strukt("OutError", 0, &[]);
    let held = fx.strukt("Held", 0, &[]);
    let (cx_t, out_t, out_error_t, held_t) = (fx.adt(cx, &[]), fx.adt(out, &[]), fx.adt(out_error, &[]), fx.adt(held, &[]));
    let (r_cx, r_out) = (fx.r(Mutbl::Not, cx_t), fx.r(Mutbl::Not, out_t));
    let res = fx.adt(result, &[unit, out_error_t]);
    let held_impl = fx.imp("impl Held", 0, held_t, None, &[], Vec::new());
    fx.impl_fn(held_impl, "new", false, &[r_out, r_cx], held_t);
    let m_held = fx.r(Mutbl::Mut, held_t);
    let u8_ = fx.u8();
    let bytes = fx.ty(PNode::Slice(u8_));
    let r_bytes = fx.r(Mutbl::Not, bytes);
    let bool_ = fx.ty(PNode::Bool);
    fx.impl_fn(held_impl, "take", true, &[m_held, r_bytes], bool_);
    fx.impl_fn(held_impl, "end", true, &[held_t], res);
    fx.imp("impl Put for Held", 0, held_t, Some((put, &[])), &[], Vec::new());

    let out_impl = fx.imp("impl Out", 0, out_t, None, &[], Vec::new());
    let w_show = fx.bound(show_tr, &[p0]);
    let w_sized = fx.bound(sized, &[p0]);
    let line = fx.impl_fn_own(out_impl, "line", true, 1, &[r_out, r_cx, p0], res, std::vec![w_show, w_sized]);
    let hello = fx.func("hello", 0, &[r_out, r_cx], res, Vec::new());
    fx.use_trait(show_tr);
    fx.use_trait(put);
    fx.finish();
    (world, ShowPut { line, ref_show, hello, show })
}

/// The items of [`rung_7c`] the cases name.
pub struct Rung7c {
    pub main: ItemKey,
    /// `fn bad(io: Io) -> u32`: `?` in a fn whose return type is no `FromResidual`.
    pub bad: ItemKey,
    pub result: ItemKey,
    pub control_flow: ItemKey,
    pub option: ItemKey,
    /// `fn same(io: Io) -> Result<u8, Error>`: `?` on a `Result` with the same error type.
    pub same: ItemKey,
    /// `fn opt(io: Io) -> Option<u8>`: `?` on an `Option`.
    pub opt: ItemKey,
    pub from_residual_tr: ItemKey,
    /// `impl<T, E, F: From<E>> FromResidual<Result<Infallible, E>> for Result<T, F>`.
    pub result_from_residual: ItemKey,
    /// `impl<T> FromResidual<Option<Infallible>> for Option<T>`.
    pub option_from_residual: ItemKey,
}

/// The world of the ladder's rung 7c (crates/blaze-cli/tests/ladder/7c-main-io: the owner's
/// hello world on `core`'s own lang items, cut to what it uses):
///
/// ```text
/// #[lang = "sized"] trait Sized {}
/// enum Infallible {}
/// enum ControlFlow<B, C> { #[lang = "Continue"] Continue(C), #[lang = "Break"] Break(B) }
/// trait Try: FromResidual<<Self as Try>::Residual> {
///     type Output; type Residual;
///     #[lang = "from_output"] fn from_output(output: Self::Output) -> Self;
///     #[lang = "branch"] fn branch(self) -> ControlFlow<Self::Residual, Self::Output>;
/// }
/// trait FromResidual<R = <Self as Try>::Residual> {
///     #[lang = "from_residual"] fn from_residual(residual: R) -> Self;
/// }
/// trait From<T>: Sized { fn from(value: T) -> Self; }
/// enum Result<T, E> { Ok(T), Err(E) }   use Result::{Err, Ok};
/// impl<T, E> Try for Result<T, E> { type Output = T; type Residual = Result<Infallible, E>; }
/// impl<T, E, F: From<E>> FromResidual<Result<Infallible, E>> for Result<T, F> {}
/// struct Cx {}  struct Io {}  struct Out {}  struct Missing;  struct OutError;  struct Error {}
/// impl From<Missing> for Error {}  impl From<OutError> for Error {}
/// impl Io { fn need_out(&self) -> Result<Out, Missing>; }
/// impl Out { fn line(&self, cx: &Cx, text: &str) -> Result<(), OutError>; }
/// fn main(cx: &Cx, io: Io) -> Result<(), Error>;
/// fn bad(io: Io) -> u32;
/// impl<T> From<T> for T {}
/// enum Option<T> { Some(T), None }   struct Other;
/// impl<T> Try for Option<T> { type Output = T; type Residual = Option<Infallible>; }
/// impl<T> FromResidual<Option<Infallible>> for Option<T> {}
/// impl Io { fn same(&self) -> Result<u8, Error>; fn maybe(&self) -> Option<u8>;
///           fn other(&self) -> Result<u8, Other>; }
/// fn same(io: Io) -> Result<u8, Error>;   fn opt(io: Io) -> Option<u8>;
/// ```
pub fn rung_7c() -> (Fx, Rung7c) {
    rung_7c_without(None)
}

/// [`rung_7c`] with the desugaring path to `drop` left unanswered (the world lacks it, as
/// front2's lang index once lacked `#[lang]` on enum variants).
pub fn rung_7c_without(drop: Option<LangItem>) -> (Fx, Rung7c) {
    let mut world = Fx::new();
    // Built through `&mut Fx`: on `Fx` itself, `fx.adt(..)` would pick `World::adt` (`&self`,
    // probed before the builder's `&mut self`).
    let fx = &mut world;
    let sized = fx.tr("Sized", 1, Vec::new(), &[]);
    fx.lang(LangItem::Sized, sized);
    let (p0, p1, p2) = (fx.param(0), fx.param(1), fx.param(2));
    let (infallible, _) = fx.enum_("Infallible", 0, &[]);
    let infallible_t = fx.adt(infallible, &[]);
    let (control_flow, _) = fx.enum_("ControlFlow", 2, &[("Continue", &[p1]), ("Break", &[p0])]);
    let (result, _) = fx.enum_("Result", 2, &[("Ok", &[p0]), ("Err", &[p1])]);
    fx.use_path("Ok", Def::Ctor { adt: result, variant: 0, kind: CtorKind::Tuple });
    fx.use_path("Err", Def::Ctor { adt: result, variant: 1, kind: CtorKind::Tuple });

    // `FromResidual` first: `Try`'s supertrait names it.
    let from_residual_tr = fx.tr("FromResidual", 2, Vec::new(), &[]);
    let from_residual = fx.trait_fn(from_residual_tr, "from_residual", false, &[p1], p0);
    // `Try`'s supertrait names `Try`'s own projection: the trait first, then its supertraits.
    let try_tr = fx.tr("Try", 1, Vec::new(), &["Output", "Residual"]);
    let residual = fx.alias(try_tr, "Residual", &[p0]);
    let output = fx.alias(try_tr, "Output", &[p0]);
    let sup = fx.bound(from_residual_tr, &[p0, residual]);
    fx.set_supers(try_tr, std::vec![sup]);
    let cf_ret = fx.adt(control_flow, &[residual, output]);
    let branch = fx.trait_fn(try_tr, "branch", true, &[p0], cf_ret);
    let from_output = fx.trait_fn(try_tr, "from_output", false, &[output], p0);
    let sized_self = fx.bound(sized, &[p0]);
    let from_tr = fx.tr("From", 2, std::vec![sized_self], &[]);
    fx.trait_fn(from_tr, "from", false, &[p1], p0);

    let res_p0_p1 = fx.adt(result, &[p0, p1]);
    let res_inf_p1 = fx.adt(result, &[infallible_t, p1]);
    fx.imp(
        "impl<T, E> Try for Result<T, E>",
        2,
        res_p0_p1,
        Some((try_tr, &[])),
        &[("Output", p0), ("Residual", res_inf_p1)],
        Vec::new(),
    );
    let res_p0_p2 = fx.adt(result, &[p0, p2]);
    let from_e = fx.bound(from_tr, &[p2, p1]);
    let result_from_residual = fx.imp(
        "impl<T, E, F: From<E>> FromResidual<Result<Infallible, E>> for Result<T, F>",
        3,
        res_p0_p2,
        Some((from_residual_tr, &[res_inf_p1])),
        &[],
        std::vec![from_e],
    );

    let cx = fx.strukt("Cx", 0, &[]);
    let io = fx.strukt("Io", 0, &[]);
    let out = fx.strukt("Out", 0, &[]);
    let missing = fx.strukt("Missing", 0, &[]);
    let out_error = fx.strukt("OutError", 0, &[]);
    let error = fx.strukt("Error", 0, &[]);
    let (cx_t, io_t, out_t, missing_t, out_error_t, error_t) = (
        fx.adt(cx, &[]),
        fx.adt(io, &[]),
        fx.adt(out, &[]),
        fx.adt(missing, &[]),
        fx.adt(out_error, &[]),
        fx.adt(error, &[]),
    );
    fx.imp("impl<T> From<T> for T", 1, p0, Some((from_tr, &[p0])), &[], Vec::new());
    fx.imp("impl From<Missing> for Error", 0, error_t, Some((from_tr, &[missing_t])), &[], Vec::new());
    fx.imp("impl From<OutError> for Error", 0, error_t, Some((from_tr, &[out_error_t])), &[], Vec::new());
    let io_impl = fx.imp("impl Io", 0, io_t, None, &[], Vec::new());
    let r_io = fx.r(Mutbl::Not, io_t);
    let res_out_missing = fx.adt(result, &[out_t, missing_t]);
    fx.impl_fn(io_impl, "need_out", true, &[r_io], res_out_missing);
    let out_impl = fx.imp("impl Out", 0, out_t, None, &[], Vec::new());
    let r_out = fx.r(Mutbl::Not, out_t);
    let r_cx = fx.r(Mutbl::Not, cx_t);
    let str_ = fx.ty(PNode::Str);
    let r_str = fx.r(Mutbl::Not, str_);
    let unit = fx.unit();
    let res_unit_out_error = fx.adt(result, &[unit, out_error_t]);
    fx.impl_fn(out_impl, "line", true, &[r_out, r_cx, r_str], res_unit_out_error);

    let (option, _) = fx.enum_("Option", 1, &[("Some", &[p0]), ("None", &[])]);
    let opt_p0 = fx.adt(option, &[p0]);
    let opt_inf = fx.adt(option, &[infallible_t]);
    fx.imp("impl<T> Try for Option<T>", 1, opt_p0, Some((try_tr, &[])), &[("Output", p0), ("Residual", opt_inf)], Vec::new());
    let option_from_residual = fx.imp(
        "impl<T> FromResidual<Option<Infallible>> for Option<T>",
        1,
        opt_p0,
        Some((from_residual_tr, &[opt_inf])),
        &[],
        Vec::new(),
    );
    let other = fx.strukt("Other", 0, &[]);
    let other_t = fx.adt(other, &[]);
    let u8_ = fx.ty(PNode::Uint(crate::ids::UintTy::U8));
    let res_u8_error = fx.adt(result, &[u8_, error_t]);
    let opt_u8 = fx.adt(option, &[u8_]);
    let res_u8_other = fx.adt(result, &[u8_, other_t]);
    fx.impl_fn(io_impl, "same", true, &[r_io], res_u8_error);
    fx.impl_fn(io_impl, "maybe", true, &[r_io], opt_u8);
    fx.impl_fn(io_impl, "other", true, &[r_io], res_u8_other);

    let paths = [
        (LangItem::TryTraitBranch, Def::Fn(branch)),
        (LangItem::TryTraitFromResidual, Def::Fn(from_residual)),
        (LangItem::TryTraitFromOutput, Def::Fn(from_output)),
        (LangItem::ControlFlowContinue, Def::Ctor { adt: control_flow, variant: 0, kind: CtorKind::Tuple }),
        (LangItem::ControlFlowBreak, Def::Ctor { adt: control_flow, variant: 1, kind: CtorKind::Tuple }),
    ];
    for (l, def) in paths {
        if Some(l) != drop {
            fx.lang_def(l, def);
        }
    }

    let res_unit_error = fx.adt(result, &[unit, error_t]);
    let main = fx.func("main", 0, &[r_cx, io_t], res_unit_error, Vec::new());
    let u32_ = fx.u32();
    let bad = fx.func("bad", 0, &[io_t], u32_, Vec::new());
    let same = fx.func("same", 0, &[io_t], res_u8_error, Vec::new());
    let opt = fx.func("opt", 0, &[io_t], opt_u8, Vec::new());
    fx.finish();
    (world, Rung7c { main, bad, result, control_flow, option, same, opt, from_residual_tr, result_from_residual, option_from_residual })
}
