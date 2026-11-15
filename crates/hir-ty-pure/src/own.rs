//! A body's own effects ([`own_effects`]): what the effect walk (`crate::walk`) works out of a
//! function's body before it folds in any callee, the fact the body makes for its readers
//! (`blaze_keyed::record::FactKind::Effects`, made by blaze-front's body task).
//!
//! The walk's summary of an instance is transitive: its kinds, sets and witnesses are "it or a
//! callee", a fixpoint over the call graph, so it changes when any callee's body does. What a
//! body makes is its own part alone, cut where `walk::summarize` reads its first callee
//! summary: `walk::own_part`. `walk::summary_in` is `walk::fold` over that part and its sites'
//! callees' summaries, so the cut and the walk are one definition.
//!
//! The invariant: the transitive summary of an instance is a function of the own part of each
//! instance it reaches (the closure of its sites' callees) and of nothing else of their
//! bodies; an instance's own part is a function of its function's body (whose text
//! [`OwnEffects::code`] covers), its type arguments, and the signatures, impls and attributes
//! its calls resolve through. [`own_effects`] is the own part of the function's instance in
//! its own where-clauses (the definition's, as `fn_purity_generic` checks it).
//!
//! What is not yet at that cut: `walk::fold` still reads the body for the three passes the
//! part does not reduce to body-free inputs, the locals' values, the memory and the locks
//! (each a fixpoint over the statements whose inputs are the callees' returned values, memory
//! facts and lock summaries). So the digest covers the body's text ([`OwnEffects::code`]), and
//! a body edit no reader can observe (a statement that changes no effect) still moves it. A
//! callee's body never does: no part of it is read here.
//!
//! Floor: one walk of the body's own monomorphized MIR (its sites, its reach proof, its
//! `unsafe` scan), no callee's summary and no other body, and one hash of the part. This costs
//! that plus one build of the body's runtime MIR in its own where-clauses, made for the call
//! and dropped with it (`dispatch::body_once`).

#![blaze::sans_io]

use std::alloc::Global;

use base_db::Db;
use blaze_ids::{ContentId, Hasher};
use blaze_item_hash::identity::early_identity;
use blaze_items::{ItemId, ItemKey, key};
use blaze_memo::Id as _;
use hir_def::{DefWithBodyId, FunctionId, signatures::FunctionSignature};
use hir_ty::{
    db::{InternedClosureId, InternedCoroutineId},
    facts::Proofs,
};
use hir_ty_mir::mir::dispatch::{self, Body};
use hir_ty_share::identity as share;

use crate::{
    effects::{self, EffectSet, Rule},
    models::tag,
    walk::{Cx, Def, How, OwnPart, Witness, own_part},
};

/// 1: the first form.
const DOMAIN: &str = "blaze v2 hir-ty-pure own effects 1";
/// The domain of the part's shape: what the fold reads of the body past its sites' callees.
const SHAPE_DOMAIN: &str = "blaze v2 hir-ty-pure own effects shape 1";

/// Why a function has no own effects: it has no body to walk.
pub const NO_BODY: &str =
    "a body's own effects (`hir_ty_pure::own_effects`): the function has no body (a declaration in a trait or an `extern` block)";
/// Why a function has no own effects: its body has no key made from its text.
pub const NO_KEY: &str =
    "a body's own effects (`hir_ty_pure::own_effects`): the body has no key (`blaze_item_hash::identity::early_identity` refused it)";
/// Why a function has no own effects: its body does not lower to MIR in its own where-clauses.
pub const UNLOWERED: &str =
    "a body's own effects (`hir_ty_pure::own_effects`): the body does not lower to MIR in its own where-clauses";
/// Why a function has no own effects: a callee's type arguments have no identity, so the
/// callee cannot be named the same in two databases.
pub const UNSHARED_ARGS: &str =
    "a body's own effects (`hir_ty_pure::own_effects`): a callee's type arguments have no identity (`hir_ty_share::identity::args`)";

/// Which kind of body a walked callee is.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum BodyKind {
    Fn,
    Closure,
    Coroutine,
    /// A coroutine's body run by move (an `async` closure's).
    CoroutineByMove,
}

/// What a site of the body reaches, named the same in every database.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum Callee {
    /// A body the walk goes into: its item key and its type arguments' identity
    /// (`hir_ty_share::identity::args`, in terms of the caller's parameters).
    Body { kind: BodyKind, item: ItemKey, args: ContentId },
    /// A function the models stop at, with the leaf's tag (`crate::models::tag`).
    Leaf { func: ItemKey, leaf: u8 },
}

impl Callee {
    /// The item it names.
    pub fn item(&self) -> ItemKey {
        match *self {
            Callee::Body { item, .. } | Callee::Leaf { func: item, .. } => item,
        }
    }
}

/// One site of the body, as the fold reads it of the caller's side.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub struct OwnSite {
    pub callee: Callee,
    /// A closure or function given to a trusted function, walked for its effects alone.
    pub given: bool,
    /// Its call is proved to have no panic here (a block no path reaches, a modeled call
    /// proved not to panic with these arguments).
    pub discharged: bool,
}

/// A function's own effects: the part of its summary its body gives by itself, in its own
/// where-clauses (see the module doc).
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct OwnEffects {
    /// The body's key made from its own text (`early_identity`): what covers the passes the
    /// fold runs over the body again.
    code: ContentId,
    /// Every kind the body has itself, no trust applied: what its walk observes and what its
    /// calls of the models' leaves do (a leaf's body is not walked).
    kinds: EffectSet,
    /// Per effect of `EFFECTS`, the kinds it refuses that the body has itself.
    sets: Box<[EffectSet]>,
    /// How the body has each kind of `sets`, by effect and then kind, each at depth 0.
    witnesses: Box<[Witness]>,
    /// The first thing the body does itself that a build script may not.
    script: Option<Box<str>>,
    /// Its sites in the order the walk meets them (block order, which the fold's witnesses
    /// read: the first site of the fewest calls is a kind's witness).
    sites: Box<[OwnSite]>,
    /// Every item its sites name, sorted, each once: what a reader asks the own effects of.
    callees: Box<[ItemKey]>,
    /// The functions trusted for some effect it calls with no site, by key, sorted.
    trusted: Box<[ItemKey]>,
    /// The link names of its calls of foreign functions that may block, in body order.
    foreign: Box<[Box<str>]>,
    /// What the fold reads of the body past its callees: each block's flow and call kind,
    /// which locals hold no pointer, each site's spread, block, origin and place, and whether
    /// the body is a workspace crate's.
    shape: ContentId,
}

impl OwnEffects {
    /// The digest of everything a reader's fold reads of it: deterministic, no `Gid` and no
    /// position an edit outside the body moves (sites are in the body's own block order, a
    /// place is an index into the body's own arenas); the callees by item key and argument
    /// identity.
    pub fn digest(&self) -> ContentId {
        let mut h = Hasher::new(DOMAIN);
        h.id(&self.code).update(&self.kinds.0.to_le_bytes());
        h.update(&(self.sets.len() as u64).to_le_bytes());
        for set in self.sets.iter() {
            h.update(&set.0.to_le_bytes());
        }
        h.update(&(self.witnesses.len() as u64).to_le_bytes());
        for witness in self.witnesses.iter() {
            h.update(&[witness.effect, witness.rule as u8]).update(&witness.depth.to_le_bytes());
            match &witness.how {
                How::Own { what, span } => {
                    h.update(&[0]).part(what.as_bytes());
                    feed(&mut h, span);
                }
                How::Site { site, said } => {
                    h.update(&[1]).update(&site.to_le_bytes()).opt_part(said.as_deref());
                }
            }
        }
        h.opt_part(self.script.as_deref());
        h.update(&(self.sites.len() as u64).to_le_bytes());
        for site in self.sites.iter() {
            match site.callee {
                Callee::Body { kind, item, args } => {
                    h.update(&[0, kind as u8]).id(&item.0).id(&args);
                }
                Callee::Leaf { func, leaf } => {
                    h.update(&[1, leaf]).id(&func.0);
                }
            }
            h.update(&[u8::from(site.given), u8::from(site.discharged)]);
        }
        h.update(&(self.trusted.len() as u64).to_le_bytes());
        for it in self.trusted.iter() {
            h.id(&it.0);
        }
        h.update(&(self.foreign.len() as u64).to_le_bytes());
        for link in self.foreign.iter() {
            h.part(link.as_bytes());
        }
        h.id(&self.shape);
        h.finish()
    }

    /// The body's key made from its own text.
    pub fn code(&self) -> ContentId {
        self.code
    }

    /// Every kind the body has itself, no trust applied.
    pub fn kinds(&self) -> EffectSet {
        self.kinds
    }

    /// Its sites in body order.
    pub fn sites(&self) -> &[OwnSite] {
        &self.sites
    }

    /// Every item its sites name, sorted, each once.
    pub fn callees(&self) -> &[ItemKey] {
        &self.callees
    }

    /// `part`, the own part of the body whose key is `code`, named the same in every database.
    fn of(db: &Db, code: ContentId, part: OwnPart) -> Result<OwnEffects, &'static str> {
        let OwnPart {
            sites,
            flows,
            script,
            kinds,
            calls,
            workspace,
            trusted_calls,
            foreign,
            discharged,
            pointerless,
        } = part;
        let mut shape = Hasher::new(SHAPE_DOMAIN);
        shape.update(&[u8::from(workspace)]);
        shape.update(&(flows.len() as u64).to_le_bytes());
        for flow in &flows {
            feed(&mut shape, flow);
        }
        shape.update(&(calls.len() as u64).to_le_bytes());
        for call in &calls {
            feed(&mut shape, call);
        }
        shape.update(&(pointerless.len() as u64).to_le_bytes());
        for &it in &pointerless {
            shape.update(&[u8::from(it)]);
        }
        let mut own_sites = Vec::with_capacity_in(sites.len(), Global);
        // A leaf of the models is not walked: its kinds are the body's own, as the fold counts
        // them (`walk::fold`'s row for a leaf), its panic dropped where its call is discharged.
        let mut leaf_kinds = EffectSet::EMPTY;
        for (site, &discharged) in sites.iter().zip(&discharged) {
            let (instance, spread) = site.reached.instance();
            feed(&mut shape, &(spread, site.call, site.origin, site.span));
            let callee = match instance.def {
                Def::Body(body) => {
                    let (kind, item) = body_key(db, body);
                    let args = share::args(db, instance.args.as_ref()).map_err(|_| UNSHARED_ARGS)?;
                    Callee::Body { kind, item, args }
                }
                Def::Leaf(func, leaf) => {
                    let (kinds, _) = effects::leaf_kinds(leaf);
                    leaf_kinds = leaf_kinds.union(match discharged {
                        true => kinds.without(EffectSet::one(Rule::Panic)),
                        false => kinds,
                    });
                    Callee::Leaf { func: key::item_key(db, ItemId::Function(func)), leaf: tag(leaf) }
                }
            };
            own_sites.push(OwnSite { callee, given: site.given, discharged });
        }
        let mut callees = Vec::with_capacity_in(own_sites.len(), Global);
        callees.extend(own_sites.iter().map(|it| it.callee.item()));
        callees.sort_unstable();
        callees.dedup();
        let mut trusted = Vec::with_capacity_in(trusted_calls.len(), Global);
        trusted.extend(trusted_calls.iter().map(|&it| key::item_key(db, ItemId::Function(it))));
        trusted.sort_unstable();
        trusted.dedup();
        let mut links = Vec::with_capacity_in(foreign.len(), Global);
        links.extend(foreign.into_iter().map(|it| it.link));
        let own_kinds = kinds.kinds.union(leaf_kinds);
        let (sets, witnesses) = kinds.into_sets();
        let mut set_list = Vec::with_capacity_in(sets.len(), Global);
        set_list.extend(sets.iter().copied());
        Ok(OwnEffects {
            code,
            kinds: own_kinds,
            sets: set_list.into_boxed_slice(),
            witnesses,
            script: script.map(String::into_boxed_str),
            sites: own_sites.into_boxed_slice(),
            callees: callees.into_boxed_slice(),
            trusted: trusted.into_boxed_slice(),
            foreign: links.into_boxed_slice(),
            shape: shape.finish(),
        })
    }
}

/// The own effects of `func` (see the module doc): its body walked in its own where-clauses,
/// before any callee is folded in. `Err` names why it has none: no body ([`NO_BODY`]), no key
/// of its text ([`NO_KEY`]), a body that does not lower ([`UNLOWERED`]), a callee that cannot
/// be named the same in two databases ([`UNSHARED_ARGS`]). Never an empty part for a body
/// that could not be walked.
pub fn own_effects(db: &Db, func: FunctionId) -> Result<OwnEffects, &'static str> {
    if !FunctionSignature::of(db, func).has_body() {
        return Err(NO_BODY);
    }
    let code = early_identity(db, DefWithBodyId::FunctionId(func)).map_err(|_| NO_KEY)?;
    let (env, args) = crate::function::generic(db, func);
    // The checks' part: proofs spent, as every effect and role lint asks (`walk::summary`).
    let cx = Cx::new(db, env, Proofs::Spent);
    let body = dispatch::body_once(db, &cx.stored, Body::Fn(func), &args).map_err(|_| UNLOWERED)?;
    let part = own_part(&cx, &body, Def::Body(Body::Fn(func)));
    OwnEffects::of(db, code, part)
}

/// A walked body's kind and item key: a function's own, a closure's or coroutine's by its
/// place in the body it is written in (`blaze_items::key`).
fn body_key(db: &Db, body: Body) -> (BodyKind, ItemKey) {
    match body {
        Body::Fn(func) => (BodyKind::Fn, key::item_key(db, ItemId::Function(func))),
        Body::Closure(id) => (BodyKind::Closure, key::closure_key(db, InternedClosureId::from_gid(id))),
        Body::Coroutine(id) => {
            (BodyKind::Coroutine, key::coroutine_key(db, InternedCoroutineId::from_gid(id)))
        }
        Body::CoroutineByMove(id) => {
            (BodyKind::CoroutineByMove, key::coroutine_key(db, InternedCoroutineId::from_gid(id)))
        }
    }
}

/// Hashes `value`'s `Debug` text into `h` as it is written, with no buffer, then an end mark no
/// text holds (`0xff` is no UTF-8 byte). Used only for values made of the body's own indices
/// and of enums with no id in them (a flow, a call kind, a span), whose text is the same in
/// every process.
fn feed(h: &mut Hasher, value: &impl std::fmt::Debug) {
    struct Fed<'a>(&'a mut Hasher);
    impl std::fmt::Write for Fed<'_> {
        fn write_str(&mut self, text: &str) -> std::fmt::Result {
            self.0.update(text.as_bytes());
            Ok(())
        }
    }
    std::fmt::Write::write_fmt(&mut Fed(&mut *h), format_args!("{value:?}"))
        .expect("a `Debug` into a hasher does not fail");
    h.update(&[0xff]);
}
